//! Locking at the server, before the lock screen exists.
//!
//! A `lock` request must leave the screen locked by the time the session
//! answers `ok`. The lock screen (`nitro-greeter --lock`) needs about
//! 100–300 ms to start, connect and send its own `Lock`, and during that
//! time the desktop would still be visible and would still take keys. So
//! the session locks first, over the shell socket: it connects, sends
//! `Lock`, waits until the server has handled it, and disconnects.
//!
//! Disconnecting as the owner leaves the lock **ownerless** and still
//! locked. The server draws nothing and routes no input. The greeter's
//! own `Lock` then takes it over, the same way a restarted greeter takes
//! over after a crash. The lock belongs to the server (docs/greeter.md,
//! decision 6); this module only asks for it early.
//!
//! # Knowing the `Lock` was handled
//!
//! The wire protocol has no roundtrip message. `Lock` is handled on
//! receipt, and the server handles one connection's messages in order.
//! So the session sends `Lock` and then `WindowList`, which is also
//! answered on receipt, and waits for the `WindowListEnd`. When it
//! arrives, the `Lock` before it has been applied. An `Error` in its
//! place means the `Lock` was refused: another connection owns the lock.
//!
//! The whole exchange runs on a thread with a deadline, like
//! [`crate::wait::probe`], so a wedged server cannot hang the session's
//! loop.

use std::path::Path;
use std::time::{Duration, Instant};

use nitro_wire::msg::ServerMsg;

/// How long the lock exchange may take before the session gives up.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(1);

/// Lock the session at the server on `shell_path`, and leave the lock
/// ownerless.
///
/// # Errors
/// The text of whatever went wrong: no server, a refusal (the lock is
/// held by another connection), or no answer within `timeout`.
pub fn lock_at_server(shell_path: &Path, timeout: Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = shell_path.to_path_buf();
    std::thread::Builder::new()
        .name("nitro-session-lock".to_owned())
        .spawn(move || {
            let _ = tx.send(exchange(&p, timeout));
        })
        .map_err(|e| format!("lock thread: {e}"))?;
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err(format!("no answer within {} ms", timeout.as_millis())))
}

fn exchange(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut conn = nitro_wire::client::Connection::connect(path, "nitro-session")
        .map_err(|e| format!("{}: {e}", path.display()))?;
    conn.lock().map_err(|e| e.to_string())?;
    conn.window_list().map_err(|e| e.to_string())?;
    let mut msgs = Vec::new();
    loop {
        match conn.flush() {
            Ok(_) => {}
            Err(e) => return Err(format!("send: {e}")),
        }
        match conn.poll(&mut msgs) {
            Ok(_) => {}
            Err(nitro_wire::Error::Closed) => {
                return Err(first_error(&msgs).unwrap_or_else(|| "server hung up".to_owned()));
            }
            Err(e) => return Err(e.to_string()),
        }
        if let Some(e) = first_error(&msgs) {
            return Err(e);
        }
        if msgs
            .iter()
            .any(|m| matches!(m, ServerMsg::WindowListEnd(_)))
        {
            return Ok(());
        }
        msgs.clear();
        let now = Instant::now();
        if now >= deadline {
            return Err("no answer".to_owned());
        }
        let left = (deadline - now).min(Duration::from_millis(50));
        let fd = conn.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let ts = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: i64::from(left.subsec_nanos()),
        };
        let _ = rustix::event::poll(&mut fds, Some(&ts));
    }
}

fn first_error(msgs: &[ServerMsg]) -> Option<String> {
    msgs.iter().find_map(|m| match m {
        ServerMsg::Error(e) => Some(format!("refused: {}", e.msg)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_server_is_an_error_not_a_hang() {
        let dir = std::env::temp_dir().join(format!("nitro-session-lock-{}", std::process::id()));
        let start = Instant::now();
        let e = lock_at_server(&dir.join("shell.sock"), Duration::from_millis(300))
            .expect_err("nothing listens there");
        assert!(e.contains("shell.sock"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
