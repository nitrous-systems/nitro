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

use crate::VERSION;
use crate::codec::{FdQueue, Writer};
use crate::error::{DecodeError, Error};
use crate::framing::Framer;
use crate::io::{self, Socket};
use crate::msg::{self, ClientMsg, Hello, ServerMsg, Welcome};
use crate::types::ErrorCode;

/// Bytes one [`ClientStream::read`] takes before yielding to the event
/// loop. Large enough that an ordinary burst of mutations arrives in one
/// wakeup, small enough that no single client can monopolise the loop.
pub const READ_BUDGET: usize = 1024 * 1024;

/// Where the server should bind; see [`crate::socket_path`]. The client
/// resolves through the same function, so a client started in the same
/// environment finds the server.
pub use crate::socket_path;

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

    /// Read what is readable into the framer, up to a bounded budget.
    ///
    /// Returns the number of bytes read (0 when the socket had nothing).
    ///
    /// **Bounded on purpose.** This stops after roughly
    /// [`READ_BUDGET`] bytes even if the socket still has more, so one
    /// busy client cannot starve the server's event loop or grow the
    /// framer's buffer without limit. The socket stays readable, so the
    /// next epoll wakeup continues where this left off — which is the
    /// fairness the loop wants anyway.
    ///
    /// A hangup is remembered rather than raised at once, so the bytes
    /// that arrived before it can still be decoded; [`Error::Closed`] is
    /// returned once the framer has nothing left either.
    ///
    /// # Errors
    /// [`Error::Closed`] on hangup with nothing left to decode.
    pub fn read(&mut self) -> Result<usize, Error> {
        let mut total = 0;
        while !self.closed && total < READ_BUDGET {
            match self.socket.recv_into(&mut self.framer) {
                Ok(Some(n)) => total += n,
                Ok(None) => break,
                Err(Error::Closed) => self.closed = true,
                Err(e) => return Err(e),
            }
        }
        // Only report the hangup once nothing decodable is left: a caller
        // that drains one message per loop iteration must not lose what
        // already arrived.
        if total == 0 && self.closed && !self.has_frames() {
            return Err(Error::Closed);
        }
        Ok(total)
    }

    /// Whether the framer holds at least one complete, undecoded frame
    /// (or an error to report).
    fn has_frames(&self) -> bool {
        self.framer.has_frame()
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
