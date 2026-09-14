//! Waiting for the server to be *ready*, which is not the same as
//! waiting for its socket file to appear.
//!
//! # What "ready" means
//!
//! The session must not start the wallpaper before the server can accept
//! it, and the tempting test — does `wire.sock` exist? — is wrong in a
//! way that is invisible until the box is slow. `bind(2)` creates the
//! file, and `listen(2)` is what makes a `connect(2)` succeed; between
//! them a client gets `ECONNREFUSED`. Worse, a *stale* socket file from a
//! session that was killed with `SIGKILL` exists and refuses connections
//! forever, so the file test would report "ready" for a server that is
//! not running at all and the whole shell would die one by one in the
//! first second.
//!
//! So the check is a **real client connection with a real handshake**:
//! [`nitro_wire::client::Connection::connect`] sends `Hello` and waits
//! for the `Welcome`. That is the same code path every shell piece is
//! about to take, which is the property worth having — the session
//! cannot conclude "ready" through a path the pieces do not use.
//!
//! Both sockets are checked, wire *and* shell, because they are bound at
//! different moments in the server's start-up and the shell pieces need
//! the second one. Checking only `wire.sock` would restore exactly the
//! race this module exists to remove.
//!
//! The connection is dropped immediately. It costs the server one accept,
//! one `Welcome` and one disconnect, and it is worth it: the alternative
//! to a probe is a retry loop in three separate programs.
//!
//! # The timeout is a failure, not a delay
//!
//! If the server has not answered within [`DEFAULT_TIMEOUT`], the session
//! gives up and tears down rather than starting the shell anyway. A
//! desktop whose compositor did not come up is not improved by three
//! clients failing to connect to it in a restart loop, and `systemctl`
//! showing the unit failed with "server never became ready" is the
//! shortest path from symptom to cause.

use std::path::Path;
use std::time::{Duration, Instant};

/// How long the session waits for the server's sockets.
///
/// Ten seconds is far more than the ~200 ms a cold KMS start takes on the
/// test box, and far less than a user's patience. It is generous on
/// purpose: the failure it must not produce is a *spurious* one on a box
/// under load, and the only cost of generosity is how long a genuinely
/// broken server takes to be declared broken.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait between connection attempts.
///
/// 50 ms: twenty wakeups a second during start-up only, which is the one
/// moment in a session's life when the CPU is not idle anyway.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long one probe attempt may take before it is abandoned.
///
/// The handshake is a `connect`, a `Hello` and a `Welcome` on a local
/// socket: microseconds when the server is well. This bound exists for
/// the case where it is **not** — a server that has `listen`ed but is
/// stuck before its accept loop leaves our `connect` succeeding (the
/// kernel completes it into the backlog) and the `Welcome` never coming.
/// [`nitro_wire::client::Connection::connect`] waits for that `Welcome`
/// with an unbounded `poll`, so without a deadline of our own a wedged
/// compositor would hang the *session* rather than time out — a
/// supervisor that cannot be started is at least visible, one that hangs
/// forever with no output is not.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Why the wait ended without the server being ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitError {
    /// The deadline passed. Carries the last connection error, because
    /// "timed out" alone never tells you whether the server was missing
    /// or refusing.
    TimedOut {
        /// How long the wait actually lasted.
        waited: Duration,
        /// The last probe's error, verbatim.
        last: String,
    },
    /// A child we were waiting for exited while we waited.
    Died,
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut { waited, last } => write!(
                f,
                "server not ready after {:.1}s (last: {last})",
                waited.as_secs_f32()
            ),
            Self::Died => write!(f, "server exited before it was ready"),
        }
    }
}

impl std::error::Error for WaitError {}

/// One connect-and-handshake attempt against `path`, bounded by
/// [`PROBE_TIMEOUT`].
///
/// `Ok(())` means a client would have been accepted.
///
/// The handshake runs on a **thread**, and that deserves a sentence,
/// because this tree has exactly one thread per process everywhere else.
/// `nitro-wire`'s blocking handshake is the code path every shell piece
/// takes, and probing through a *different* path — hand-rolling the
/// `Hello` frame here with a timeout on the `poll` — would mean the
/// session's readiness check could succeed where a real client's fails.
/// So the real client code runs, and the thread is how it is given a
/// deadline. The thread is detached if it is still stuck at the
/// deadline; it holds one socket and ends when the connection does, and
/// the alternative (a session that hangs on a wedged server) is worse.
///
/// # Errors
/// The text of whatever went wrong, for the timeout message.
pub fn probe(path: &Path) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_path_buf();
    std::thread::Builder::new()
        .name("nitro-session-probe".to_owned())
        .spawn(move || {
            let r = match nitro_wire::client::Connection::connect(&p, "nitro-session") {
                Ok(_conn) => Ok(()),
                Err(e) => Err(format!("{}: {e}", p.display())),
            };
            // The receiver is gone if we were abandoned; that is fine.
            let _ = tx.send(r);
        })
        .map_err(|e| format!("probe thread: {e}"))?;
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(r) => r,
        Err(_) => Err(format!(
            "{}: no Welcome within {} ms",
            path.display(),
            PROBE_TIMEOUT.as_millis()
        )),
    }
}

/// Wait until both sockets accept a handshaken client, or the deadline
/// passes.
///
/// `alive` is asked between attempts whether the server is still
/// running; a session must not sit out a ten-second timeout for a
/// compositor that died in 40 ms with "no such DRM device". Tests pass
/// `|| true`.
///
/// # Errors
/// [`WaitError::TimedOut`] or [`WaitError::Died`].
pub fn wait_for_sockets(
    wire: &Path,
    shell: &Path,
    timeout: Duration,
    mut alive: impl FnMut() -> bool,
) -> Result<Duration, WaitError> {
    let start = Instant::now();
    let deadline = start + timeout;
    // Seeded with what the caller sees if `alive` is false before any
    // probe has run; every other path overwrites it.
    let mut last = String::from("not attempted");
    loop {
        if !alive() {
            return Err(WaitError::Died);
        }
        match probe(wire).and_then(|()| probe(shell)) {
            Ok(()) => return Ok(start.elapsed()),
            Err(e) => last.clone_from(&e),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(WaitError::TimedOut {
                waited: start.elapsed(),
                last,
            });
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nitro-session-wait-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The timeout is bounded, is reported with the last error, and does
    /// not take appreciably longer than it was given.
    #[test]
    fn a_server_that_never_comes_up_times_out_with_a_reason() {
        let dir = tmpdir("timeout");
        let start = Instant::now();
        let e = wait_for_sockets(
            &dir.join("wire.sock"),
            &dir.join("shell.sock"),
            Duration::from_millis(200),
            || true,
        )
        .expect_err("nothing is listening there");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "the wait is bounded by its timeout, took {elapsed:?}"
        );
        match e {
            WaitError::TimedOut { last, .. } => {
                assert!(last.contains("wire.sock"), "{last}");
            }
            WaitError::Died => panic!("the probe was told the server was alive"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A **stale socket file** is the case a `path.exists()` check gets
    /// wrong: the file is there, and connecting to it is refused.
    #[test]
    fn a_stale_socket_file_is_not_mistaken_for_a_server() {
        let dir = tmpdir("stale");
        let wire = dir.join("wire.sock");
        {
            // Bind and drop: the file survives, the listener does not.
            let l = std::os::unix::net::UnixListener::bind(&wire).unwrap();
            drop(l);
        }
        assert!(wire.exists(), "the file is the trap this test is about");
        assert!(probe(&wire).is_err(), "a stale socket refuses connections");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A server that died is reported at once rather than after the
    /// whole timeout.
    #[test]
    fn a_dead_server_ends_the_wait_immediately() {
        let dir = tmpdir("died");
        let start = Instant::now();
        let e = wait_for_sockets(
            &dir.join("wire.sock"),
            &dir.join("shell.sock"),
            Duration::from_secs(30),
            || false,
        )
        .expect_err("the server is gone");
        assert_eq!(e, WaitError::Died);
        assert!(start.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And the positive case: both sockets have to answer, not one. A
    /// listener on `wire.sock` alone is not readiness.
    #[test]
    fn a_listener_that_never_accepts_is_not_a_ready_server() {
        let dir = tmpdir("both");
        let wire = dir.join("wire.sock");
        let shell = dir.join("shell.sock");
        // A listener that never accepts: `connect` succeeds into the
        // backlog and no `Welcome` ever comes, which is the wedged-server
        // case `PROBE_TIMEOUT` exists for. `wire.sock` therefore fails
        // *slowly* rather than at once, and the deadline still holds.
        let _listener = std::os::unix::net::UnixListener::bind(&wire).unwrap();
        let e = wait_for_sockets(&wire, &shell, Duration::from_millis(150), || true)
            .expect_err("the shell socket is missing");
        match e {
            WaitError::TimedOut { last, .. } => {
                assert!(
                    last.contains("wire.sock"),
                    "the wedged listener is what we hit: {last}"
                );
                assert!(last.contains("no Welcome"), "{last}");
            }
            WaitError::Died => panic!("alive said true"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
