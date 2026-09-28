//! A client for `nitro-session`'s power socket.
//!
//! `nitro-session` answers one request per line on
//! `$XDG_RUNTIME_DIR/nitro/session.sock` — `logout`, `suspend`, `reboot`,
//! `poweroff`, `lock`, `status` — with `ok` or `err <reason>`. The
//! protocol is two lines, so this is a `UnixStream` and a `read_line`
//! rather than a shared crate: `nitro-session` does **not** depend on
//! this module, and the rules it states (path resolution, the reply
//! shape) are its own; see `crates/nitro-session/README.md`, "The
//! protocol". A test here pins the client side against a fake listener.
//!
//! # Blocking, briefly
//!
//! [`request`] blocks the caller for at most [`TIMEOUT`] waiting for the
//! answer. The session answers `ok` *before* it runs `systemctl`, so the
//! wait is a socket round trip in practice; the bound is for a session
//! that is wedged, which must cost a menu a stall rather than a hang.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Socket file name inside the runtime directory; `nitro-session`'s
/// `socket::SOCKET_NAME`.
pub const SOCKET_NAME: &str = "session.sock";
/// Subdirectory of the runtime dir; `nitro-session`'s `socket::SUBDIR`.
pub const SUBDIR: &str = "nitro";
/// Environment variable that overrides the whole path; the one
/// `nitro-session` binds.
pub const SOCKET_ENV: &str = "NITRO_SESSION_SOCKET";
/// How long [`request`] waits to send and for the reply.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// Where the session socket is, given the environment.
///
/// The override wins; else `<runtime>/nitro/session.sock`. A relative
/// runtime directory counts as none — the same rule as
/// `nitro-session`'s `socket::resolve`. Unlike the session, which falls
/// back to `/tmp/nitro-<uid>/` with a warning, a client with no runtime
/// directory gets `None`: guessing a `/tmp` path a session may not have
/// used would turn "no session here" into a confusing connect error.
///
/// Taken as arguments so the rule is testable without mutating the
/// process environment; [`default_socket_path`] reads it.
#[must_use]
pub fn socket_path(override_path: Option<&Path>, runtime: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return Some(p.to_path_buf());
    }
    let dir = runtime.filter(|d| d.is_absolute())?;
    Some(dir.join(SUBDIR).join(SOCKET_NAME))
}

/// [`socket_path`] from `$NITRO_SESSION_SOCKET` and `$XDG_RUNTIME_DIR`.
#[must_use]
pub fn default_socket_path() -> Option<PathBuf> {
    let over = std::env::var_os(SOCKET_ENV).map(PathBuf::from);
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    socket_path(over.as_deref(), runtime.as_deref())
}

/// Something the session can be asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// End the session: the desktop is torn down in order.
    Logout,
    /// `systemctl suspend`.
    Suspend,
    /// `systemctl reboot`.
    Reboot,
    /// `systemctl poweroff`.
    Poweroff,
    /// Lock the screen. Not implemented by `nitro-session` yet (M4); see
    /// [`Action::available`].
    Lock,
}

impl Action {
    /// Every action, in menu order.
    pub const ALL: [Self; 5] = [
        Self::Lock,
        Self::Suspend,
        Self::Reboot,
        Self::Poweroff,
        Self::Logout,
    ];

    /// The request word on the wire.
    #[must_use]
    pub fn verb(self) -> &'static str {
        match self {
            Self::Logout => "logout",
            Self::Suspend => "suspend",
            Self::Reboot => "reboot",
            Self::Poweroff => "poweroff",
            Self::Lock => "lock",
        }
    }

    /// Whether the session implements it today.
    ///
    /// `lock` answers `err not implemented` until M4 brings a lock
    /// screen, so a menu should show it disabled rather than offer a
    /// button that can only fail. Flip this when `nitro-session` does.
    #[must_use]
    pub fn available(self) -> bool {
        !matches!(self, Self::Lock)
    }
}

/// Ask the session at `path` to perform `action`.
///
/// Connects, writes `"<verb>\n"`, and reads one line, waiting at most
/// [`TIMEOUT`] for either.
///
/// # Errors
/// The session's own reason for `err <reason>`; otherwise a description
/// of what went wrong — no socket, a timeout, a connection closed
/// without a reply, or a reply that is neither `ok` nor `err`.
pub fn request(path: &Path, action: Action) -> Result<(), String> {
    let mut stream =
        UnixStream::connect(path).map_err(|e| format!("no session at {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|e| format!("session socket: {e}"))?;
    stream
        .write_all(format!("{}\n", action.verb()).as_bytes())
        .map_err(|e| format!("session socket: {e}"))?;
    let mut line = String::new();
    let n = BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| format!("session did not answer: {e}"))?;
    if n == 0 {
        return Err("session closed the connection without answering".to_owned());
    }
    parse_reply(&line)
}

/// Read a reply line: `ok` → `Ok`, `err <reason>` → `Err(reason)`.
fn parse_reply(line: &str) -> Result<(), String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line == "ok" {
        return Ok(());
    }
    if line == "err" {
        return Err("failed".to_owned());
    }
    if let Some(reason) = line.strip_prefix("err ") {
        return Err(reason.trim().to_owned());
    }
    Err(format!("unexpected answer from the session: {line:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;

    /// A fresh, empty directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nitro-system-session-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A one-shot fake session: accepts one client, reads one line,
    /// answers `reply` (or nothing, for `None`) and returns the request.
    fn fake_session(path: &Path, reply: Option<&'static str>) -> JoinHandle<String> {
        let listener = UnixListener::bind(path).expect("bind");
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).expect("read");
            if let Some(reply) = reply {
                (&stream).write_all(reply.as_bytes()).expect("reply");
            }
            line
        })
    }

    #[test]
    fn the_path_follows_the_sessions_rule() {
        let over = Path::new("/x/s.sock");
        let run = Path::new("/run/user/1000");
        assert_eq!(socket_path(Some(over), Some(run)), Some(over.to_path_buf()));
        assert_eq!(
            socket_path(None, Some(run)),
            Some(PathBuf::from("/run/user/1000/nitro/session.sock"))
        );
        assert_eq!(socket_path(None, Some(Path::new("relative"))), None);
        assert_eq!(socket_path(None, None), None);
    }

    #[test]
    fn verbs_are_the_wire_words_and_lock_is_not_available() {
        let verbs: Vec<_> = Action::ALL.iter().map(|a| a.verb()).collect();
        assert_eq!(verbs, ["lock", "suspend", "reboot", "poweroff", "logout"]);
        assert!(!Action::Lock.available());
        assert!(
            Action::ALL
                .iter()
                .filter(|a| **a != Action::Lock)
                .all(|a| a.available())
        );
    }

    #[test]
    fn ok_is_success_and_the_verb_is_sent_as_one_line() {
        let dir = temp_dir("ok");
        let path = dir.join(SOCKET_NAME);
        let server = fake_session(&path, Some("ok\n"));
        assert_eq!(request(&path, Action::Logout), Ok(()));
        assert_eq!(server.join().expect("server"), "logout\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn err_carries_the_sessions_reason() {
        let dir = temp_dir("err");
        let path = dir.join(SOCKET_NAME);
        let server = fake_session(&path, Some("err nope\n"));
        assert_eq!(request(&path, Action::Suspend), Err("nope".to_owned()));
        assert_eq!(server.join().expect("server"), "suspend\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_socket_is_an_error_not_a_hang() {
        let dir = temp_dir("none");
        let err = request(&dir.join(SOCKET_NAME), Action::Reboot).expect_err("no socket");
        assert!(err.starts_with("no session at"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_that_hangs_up_without_answering_is_an_error() {
        let dir = temp_dir("closed");
        let path = dir.join(SOCKET_NAME);
        let server = fake_session(&path, None);
        let err = request(&path, Action::Poweroff).expect_err("no reply");
        assert!(err.contains("without answering"), "{err}");
        assert_eq!(server.join().expect("server"), "poweroff\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbled_reply_is_reported() {
        assert_eq!(parse_reply("ok\r\n"), Ok(()));
        assert_eq!(parse_reply("err"), Err("failed".to_owned()));
        let err = parse_reply("maybe\n").expect_err("garbled");
        assert!(err.contains("unexpected"), "{err}");
    }
}
