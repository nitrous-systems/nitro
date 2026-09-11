//! The server side: [`Listener`] for accepting, and [`ClientStream`] —
//! one connected client's socket, framer and outgoing buffer — for the
//! server's epoll loop to drive.
//!
//! This crate does not run an event loop; it hands the server the two
//! operations an epoll loop needs: "readable → [`ClientStream::read`],
//! then drain [`ClientStream::next_msg`]" and "writable →
//! [`ClientStream::flush`]".

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::codec::{FdQueue, Writer};
use crate::error::{DecodeError, Error};
use crate::framing::Framer;
use crate::io::{self, Socket};
use crate::msg::{self, ClientMsg, Hello, ServerMsg, Welcome};
use crate::types::ErrorCode;
use crate::{DEFAULT_SOCKET_NAME, SOCKET_ENV, SOCKET_SUBDIR, VERSION};

/// Where the server should bind, honouring `NITRO_SOCKET`.
///
/// Same resolution as the client's [`socket_path`](crate::client::socket_path),
/// so a client started in the same environment finds the server.
#[must_use]
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os(SOCKET_ENV) {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join(SOCKET_SUBDIR).join(DEFAULT_SOCKET_NAME);
        }
    }
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/nitro-{uid}")).join(DEFAULT_SOCKET_NAME)
}

/// A bound, non-blocking listening socket that unlinks its path on drop.
#[derive(Debug)]
pub struct Listener {
    fd: OwnedFd,
    path: PathBuf,
}

impl Listener {
    /// Bind at `path`, creating the parent directory `0700` and removing a
    /// stale socket file.
    ///
    /// # Errors
    /// Any `mkdir`/`bind`/`listen` failure.
    pub fn bind(path: &Path) -> Result<Self, Error> {
        let fd = io::listen(path)?;
        Ok(Self {
            fd,
            path: path.to_path_buf(),
        })
    }

    /// Bind at the path from [`socket_path`].
    ///
    /// # Errors
    /// As [`Listener::bind`].
    pub fn bind_default() -> Result<Self, Error> {
        Self::bind(&socket_path())
    }

    /// The path bound.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The descriptor, for `epoll`.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Accept one pending connection, or `None`.
    ///
    /// # Errors
    /// Any `accept` failure other than `EAGAIN`.
    pub fn accept(&self) -> Result<Option<ClientStream>, Error> {
        Ok(io::accept(&self.fd)?.map(ClientStream::new))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(rustix::fs::CWD, &self.path, rustix::fs::AtFlags::empty());
    }
}

/// One connected client as the server sees it: socket, incoming framer,
/// outgoing buffer, and the handshake state machine.
#[derive(Debug)]
pub struct ClientStream {
    socket: Socket,
    framer: Framer,
    out: Writer,
    /// `None` until `Hello` arrived.
    name: Option<String>,
    /// Set once the peer hung up; reported after the last message.
    closed: bool,
}

impl ClientStream {
    /// Wrap an accepted socket.
    #[must_use]
    pub fn new(socket: Socket) -> Self {
        Self {
            socket,
            framer: Framer::new(),
            out: Writer::new(),
            name: None,
            closed: false,
        }
    }

    /// The socket descriptor, for `epoll`.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }

    /// The client's self-reported name, once it has said `Hello`.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Whether the handshake is complete.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.name.is_some()
    }

    /// Whether bytes are waiting to be written.
    #[must_use]
    pub fn has_pending_writes(&self) -> bool {
        !self.out.is_empty()
    }

    /// Whether the peer has closed its end. Messages already received are
    /// still delivered by [`ClientStream::next_msg`] first.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Read whatever is readable into the framer.
    ///
    /// Returns the number of bytes read (0 when the socket had nothing).
    /// A hangup is remembered rather than raised at once, so the bytes
    /// that arrived before it can still be decoded; [`Error::Closed`] is
    /// returned by the read *after* the buffer has been drained.
    ///
    /// # Errors
    /// [`Error::Closed`] on hangup with nothing left to decode.
    pub fn read(&mut self) -> Result<usize, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let mut total = 0;
        loop {
            match self.socket.recv_into(&mut self.framer) {
                Ok(Some(n)) => total += n,
                Ok(None) => break,
                Err(Error::Closed) => {
                    self.closed = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if total == 0 && self.closed {
            return Err(Error::Closed);
        }
        Ok(total)
    }

    /// Take the next decoded message, or `None` if more bytes are needed.
    ///
    /// Handles the handshake itself: the first message must be `Hello`
    /// with a matching version, and it is answered with `Welcome` (queued
    /// in the outgoing buffer, so the caller still needs [`flush`]). Both
    /// `Hello` and `Welcome` are returned to the caller, which is what the
    /// server logs and the tests assert on.
    ///
    /// [`flush`]: ClientStream::flush
    ///
    /// # Errors
    /// [`Error::Decode`] on a malformed frame, [`Error::Version`] on a
    /// version mismatch, [`Error::Unexpected`] when a message arrives
    /// before `Hello` or a second `Hello` arrives. Every one is fatal:
    /// answer with [`ClientStream::fail`] and drop the client.
    pub fn next_msg(&mut self) -> Result<Option<ClientMsg>, Error> {
        let Some(frame) = self.framer.next_frame()? else {
            return Ok(None);
        };
        let mut fds = FdQueue::from_vec(frame.fds);
        let msg = ClientMsg::decode(frame.op, &frame.payload, &mut fds)?;
        match (&msg, self.name.is_some()) {
            (ClientMsg::Hello(h), false) => {
                if h.version != VERSION {
                    return Err(Error::Version {
                        ours: VERSION,
                        theirs: h.version,
                    });
                }
                self.name = Some(h.name.clone());
            }
            (ClientMsg::Hello(_), true) => return Err(Error::Unexpected("second Hello")),
            (_, false) => return Err(Error::Unexpected("message before Hello")),
            (_, true) => {}
        }
        Ok(Some(msg))
    }

    /// Queue a `Welcome`. Call after a successful [`ClientStream::next_msg`]
    /// that returned [`Hello`].
    ///
    /// # Errors
    /// [`Error::Encode`] if `name` is absurdly long.
    pub fn welcome(&mut self, name: &str, caps: u32) -> Result<(), Error> {
        self.send(&ServerMsg::Welcome(Welcome {
            version: VERSION,
            caps,
            name: name.to_owned(),
        }))
    }

    /// Queue a message for this client.
    ///
    /// # Errors
    /// [`Error::Encode`] for a message that cannot be represented.
    pub fn send(&mut self, msg: &ServerMsg) -> Result<(), Error> {
        msg.encode(&mut self.out)?;
        Ok(())
    }

    /// Queue the fatal [`Error`](crate::msg::Error) for a failed request
    /// and try to push it out at once, best-effort. The caller drops the
    /// client afterwards.
    ///
    /// [`Error`]: crate::msg::Error
    pub fn fail(&mut self, serial: u32, code: ErrorCode, msg: &str) {
        let _ = self.send(&ServerMsg::Error(msg::Error {
            serial,
            code,
            msg: msg.to_owned(),
        }));
        let _ = self.flush();
    }

    /// Push queued bytes at the socket; `false` means more remain.
    ///
    /// # Errors
    /// [`Error::Closed`] or the underlying errno.
    pub fn flush(&mut self) -> Result<bool, Error> {
        self.socket.send_all(&mut self.out)
    }
}

/// The [`ErrorCode`] to report for a protocol-level failure.
///
/// Everything the decoder rejects is a `Protocol` error except an oversize
/// length, which is a `Limit`.
#[must_use]
pub fn code_for(err: &Error) -> ErrorCode {
    match err {
        Error::Decode(DecodeError::TooLarge) => ErrorCode::Limit,
        Error::Version { .. } => ErrorCode::Version,
        _ => ErrorCode::Protocol,
    }
}

/// Answer a `Hello` whose version we do not speak: queue `Error` with
/// [`ErrorCode::Version`]. The caller closes the connection.
pub fn reject_version(stream: &mut ClientStream, theirs: u32) {
    stream.fail(
        0,
        ErrorCode::Version,
        &format!("server speaks protocol v{VERSION}, client asked for v{theirs}"),
    );
}

/// Helper for the common "did the client say `Hello` correctly?" check in
/// tests and simple servers.
#[must_use]
pub fn is_hello(msg: &ClientMsg) -> Option<&Hello> {
    match msg {
        ClientMsg::Hello(h) => Some(h),
        _ => None,
    }
}
