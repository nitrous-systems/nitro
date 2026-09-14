//! The session socket's listener and per-client buffering.
//!
//! Path: `$XDG_RUNTIME_DIR/nitro/session.sock` (directory created
//! `0700`), or `/tmp/nitro-<uid>/session.sock` with a warning when the
//! variable is unset. `NITRO_SESSION_SOCKET` overrides the whole path.
//! Stale files are unlinked before binding and removed on shutdown.
//!
//! Three paths resolved by three crates that must agree
//! (`nitro-wire`'s two, `nitro-server`'s control socket and this one), so
//! the rule is written the same way in all of them: override, else
//! `$XDG_RUNTIME_DIR/nitro/`, else `/tmp/nitro-<uid>/`. The directory
//! `0700` is the whole access control — the session socket can power the
//! machine off, and "the user's own processes" is exactly the audience
//! that may (a user who can reach this socket can also run `systemctl
//! poweroff` directly).
//!
//! The code here is `nitro-server`'s `control.rs` in miniature and for
//! the same reasons; the one thing that differs is that this listener
//! also unlinks its path on drop, because the session owns its socket for
//! the whole login and a stale file is the failure mode that costs a
//! confusing hour.

use std::fs;
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Socket file name inside the runtime directory.
pub const SOCKET_NAME: &str = "session.sock";
/// Subdirectory of the runtime dir, shared with the other sockets.
pub const SUBDIR: &str = "nitro";
/// Environment variable that overrides the whole path.
pub const SOCKET_ENV: &str = "NITRO_SESSION_SOCKET";

/// Where the socket goes and, when a fallback was taken, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPath {
    /// Full path of the socket file.
    pub path: PathBuf,
    /// Set when `XDG_RUNTIME_DIR` was unusable and `/tmp` was used.
    pub warning: Option<String>,
}

/// Resolve the socket path from an explicit override, the runtime dir, or
/// the `/tmp` fallback.
#[must_use]
pub fn resolve(override_path: Option<&Path>, xdg_runtime_dir: Option<&Path>) -> SocketPath {
    if let Some(p) = override_path {
        return SocketPath {
            path: p.to_path_buf(),
            warning: None,
        };
    }
    match xdg_runtime_dir {
        Some(dir) if dir.is_absolute() => SocketPath {
            path: dir.join(SUBDIR).join(SOCKET_NAME),
            warning: None,
        },
        _ => {
            let uid = rustix::process::getuid().as_raw();
            let dir = PathBuf::from(format!("/tmp/nitro-{uid}"));
            SocketPath {
                path: dir.join(SOCKET_NAME),
                warning: Some(format!(
                    "XDG_RUNTIME_DIR unset or relative; session socket falls back to {}",
                    dir.display()
                )),
            }
        }
    }
}

/// A bound, non-blocking listener that unlinks its path on drop.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Create the parent directory (`0700`), unlink a stale socket and
    /// bind.
    ///
    /// # Errors
    /// Directory creation or bind failure.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            match fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
            {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    /// The path bound.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The descriptor, for `poll`.
    #[must_use]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.listener.as_fd()
    }

    /// Unlink the socket file now, without dropping the listener.
    ///
    /// Teardown calls this: the moment the session stops answering, a
    /// client must not be able to connect and sit there waiting for a
    /// reply that will never come. `Drop` then finds nothing to remove,
    /// which is why the removal is idempotent rather than tracked.
    pub fn unlink(&self) {
        let _ = fs::remove_file(&self.path);
    }

    /// Accept one pending connection, or `None`.
    ///
    /// # Errors
    /// Any accept failure other than "would block".
    pub fn accept(&self) -> io::Result<Option<Client>> {
        match self.listener.accept() {
            Ok((stream, _)) => Client::new(stream).map(Some),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// What a read pass on a client concluded.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Still connected; `input` may hold new lines.
    Open,
    /// Peer hung up (or errored): drop the client.
    Closed,
    /// More than [`crate::power::MAX_LINE`] bytes without a newline.
    Overflow,
}

/// One connected session client with its buffers.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    /// Bytes received, not yet split into lines.
    pub input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    /// Set when the reply queued is the last one: the client is dropped
    /// once it has been written. `logout` and a protocol error both use
    /// it, so a caller sees its answer before the socket goes away.
    pub close_after_flush: bool,
}

impl Client {
    /// Wrap an accepted stream (made non-blocking here).
    ///
    /// # Errors
    /// If the socket cannot be made non-blocking.
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
            close_after_flush: false,
        })
    }

    /// The underlying socket, for `poll`.
    #[must_use]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.stream.as_fd()
    }

    /// Drain everything readable into `input`.
    pub fn read(&mut self) -> ReadOutcome {
        let mut buf = [0u8; 1024];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return ReadOutcome::Closed,
                Ok(n) => {
                    self.input.extend_from_slice(&buf[..n]);
                    if self.input.len() > crate::power::MAX_LINE && !self.input.contains(&b'\n') {
                        return ReadOutcome::Overflow;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return ReadOutcome::Open,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return ReadOutcome::Closed,
            }
        }
    }

    /// Take the next complete line from `input`, if there is one.
    pub fn next_line(&mut self) -> Option<String> {
        let idx = self.input.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = self.input.drain(..=idx).collect();
        Some(String::from_utf8_lossy(&line[..idx]).into_owned())
    }

    /// Queue a reply.
    pub fn send(&mut self, bytes: Vec<u8>) {
        if self.output.len() == self.written {
            self.output = bytes;
            self.written = 0;
        } else {
            self.output.extend_from_slice(&bytes);
        }
    }

    /// True while queued reply bytes remain.
    #[must_use]
    pub fn has_pending_output(&self) -> bool {
        self.written < self.output.len()
    }

    /// Write as much queued output as the socket takes. `Ok(true)` once
    /// everything has been written.
    ///
    /// # Errors
    /// A write error other than "would block" (the peer is gone).
    pub fn flush(&mut self) -> io::Result<bool> {
        while self.written < self.output.len() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => return Err(io::Error::from(ErrorKind::WriteZero)),
                Ok(n) => self.written += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.output.clear();
        self.written = 0;
        Ok(true)
    }

    /// Write whatever is queued, blocking briefly if the socket is full.
    ///
    /// Used on the teardown path, where "the reply was delivered" matters
    /// more than "the loop never blocks": a `logout` whose `ok` was still
    /// in the buffer when the process exited looks to the caller exactly
    /// like a session that ignored it. Bounded by one `poll` with a short
    /// timeout, so a client that has stopped reading cannot hold the
    /// shutdown.
    pub fn flush_blocking(&mut self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match self.flush() {
                Ok(true) | Err(_) => return,
                Ok(false) => {}
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return;
            }
            let left = deadline - now;
            let fd = self.as_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::OUT,
            )];
            let ts = rustix::event::Timespec {
                tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(left.subsec_nanos()),
            };
            if rustix::event::poll(&mut fds, Some(&ts)).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_override_then_xdg_then_tmp() {
        let o = resolve(Some(Path::new("/x/y.sock")), Some(Path::new("/run/user/1")));
        assert_eq!(o.path, Path::new("/x/y.sock"));
        assert!(o.warning.is_none());

        let x = resolve(None, Some(Path::new("/run/user/1")));
        assert_eq!(x.path, Path::new("/run/user/1/nitro/session.sock"));
        assert!(x.warning.is_none());

        let t = resolve(None, None);
        assert!(t.path.to_string_lossy().starts_with("/tmp/nitro-"));
        assert!(t.warning.is_some());
        // A relative runtime dir is not a runtime dir.
        assert_eq!(resolve(None, Some(Path::new("relative"))).path, t.path);
    }

    #[test]
    fn binding_creates_the_dir_replaces_a_stale_socket_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("nitro-session-sock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("session.sock");
        let first = Listener::bind(&path).unwrap();
        assert!(path.exists());
        // Drop unlinks, and a fresh bind over a stale file works.
        drop(first);
        assert!(!path.exists(), "the listener removes its own socket file");
        fs::write(&path, b"stale").unwrap();
        let second = Listener::bind(&path).unwrap();
        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_client_buffers_lines_and_replies() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Client::new(a).unwrap();
        let mut peer = b;
        peer.write_all(b"status\nsusp").unwrap();
        assert_eq!(client.read(), ReadOutcome::Open);
        assert_eq!(client.next_line().as_deref(), Some("status"));
        assert_eq!(client.next_line(), None, "a partial line is not a line");
        peer.write_all(b"end\n").unwrap();
        assert_eq!(client.read(), ReadOutcome::Open);
        assert_eq!(client.next_line().as_deref(), Some("suspend"));

        client.send(crate::power::ok());
        assert!(client.flush().unwrap());
        let mut got = [0u8; 3];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok\n");

        drop(peer);
        assert_eq!(client.read(), ReadOutcome::Closed);
    }

    #[test]
    fn a_line_that_never_ends_is_an_overflow_not_an_allocation() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Client::new(a).unwrap();
        let mut peer = b;
        peer.write_all(&vec![b'x'; crate::power::MAX_LINE + 1])
            .unwrap();
        assert_eq!(client.read(), ReadOutcome::Overflow);
    }
}
