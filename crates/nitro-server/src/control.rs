//! Control socket: where it lives, the listener, and per-client buffering.
//! The protocol itself is in [`crate::protocol`].
//!
//! Path: `$XDG_RUNTIME_DIR/nitro/control.sock` (directory created `0700`),
//! or `/tmp/nitro-<uid>/control.sock` with a warning when the variable is
//! unset. `NITRO_CONTROL` overrides the whole path. A stale socket file is
//! unlinked before binding and the file is removed on shutdown.

use std::fs;
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Where the control socket goes and, when a fallback was taken, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPath {
    /// Full path of the socket file.
    pub path: PathBuf,
    /// Set when `XDG_RUNTIME_DIR` was unusable and `/tmp` was used.
    pub warning: Option<String>,
}

/// Socket file name inside the runtime directory.
pub const SOCKET_NAME: &str = "control.sock";
/// Subdirectory of the runtime dir.
pub const SUBDIR: &str = "nitro";

/// Resolve the socket path from an explicit override, the runtime dir,
/// or the `/tmp` fallback.
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
                    "XDG_RUNTIME_DIR unset or relative; control socket falls back to {}",
                    dir.display()
                )),
            }
        }
    }
}

/// Create the parent directory (`0700`), unlink a stale socket and bind a
/// non-blocking listener.
///
/// # Errors
/// Directory creation or bind failure.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
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
    Ok(listener)
}

/// What a read pass on a client concluded.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Still connected; `input` may hold new lines.
    Open,
    /// Peer hung up (or errored): drop the client.
    Closed,
    /// The client sent more than [`crate::protocol::MAX_LINE`] bytes
    /// without a newline.
    Overflow,
}

/// One connected control client with its buffers.
#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    /// Bytes received, not yet split into lines.
    pub input: Vec<u8>,
    /// Reply bytes not yet written.
    output: Vec<u8>,
    /// How much of `output` has been written.
    written: usize,
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
        })
    }

    /// The underlying socket (for epoll registration).
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// Drain everything readable into `input`.
    pub fn read(&mut self) -> ReadOutcome {
        let mut buf = [0u8; 1024];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return ReadOutcome::Closed,
                Ok(n) => {
                    self.input.extend_from_slice(&buf[..n]);
                    if self.input.len() > crate::protocol::MAX_LINE && !self.input.contains(&b'\n')
                    {
                        return ReadOutcome::Overflow;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return ReadOutcome::Open,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return ReadOutcome::Closed,
            }
        }
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
    pub fn has_pending_output(&self) -> bool {
        self.written < self.output.len()
    }

    /// Write as much queued output as the socket takes. Returns `Ok(true)`
    /// when everything has been written.
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
        assert_eq!(x.path, Path::new("/run/user/1/nitro/control.sock"));
        assert!(x.warning.is_none());

        let t = resolve(None, None);
        assert!(
            t.path.to_string_lossy().starts_with("/tmp/nitro-"),
            "{}",
            t.path.display()
        );
        assert!(t.warning.is_some());
        let rel = resolve(None, Some(Path::new("relative")));
        assert_eq!(rel.path, t.path);
    }

    #[test]
    fn bind_creates_dir_and_replaces_stale_socket() {
        let dir = std::env::temp_dir().join(format!("nitro-control-test-{}", std::process::id()));
        let path = dir.join("sub").join("control.sock");
        let first = bind(&path).unwrap();
        drop(first);
        assert!(path.exists(), "socket file left behind for the stale case");
        let second = bind(&path).unwrap();
        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn client_buffers_reads_and_writes() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Client::new(a).unwrap();
        let mut peer = b;
        peer.write_all(b"outputs\nsta").unwrap();
        assert_eq!(client.read(), ReadOutcome::Open);
        assert_eq!(client.input, b"outputs\nsta");

        client.send(b"ok\n".to_vec());
        assert!(client.has_pending_output());
        assert!(client.flush().unwrap());
        assert!(!client.has_pending_output());
        let mut got = [0u8; 3];
        peer.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok\n");

        drop(peer);
        assert_eq!(client.read(), ReadOutcome::Closed);
    }

    #[test]
    fn client_overflow_without_newline() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client = Client::new(a).unwrap();
        let mut peer = b;
        peer.write_all(&vec![b'x'; crate::protocol::MAX_LINE + 1])
            .unwrap();
        assert_eq!(client.read(), ReadOutcome::Overflow);
    }
}
