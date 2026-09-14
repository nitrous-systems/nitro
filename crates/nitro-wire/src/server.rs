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
use crate::{MAX_PENDING_FDS, VERSION};

/// Bytes one [`ClientStream::read`] takes before yielding to the event
/// loop. Large enough that an ordinary burst of mutations arrives in one
/// wakeup, small enough that no single client can monopolise the loop.
pub const READ_BUDGET: usize = 1024 * 1024;

/// Where the server should bind its **shell** socket; see
/// [`crate::shell_socket_path`].
pub use crate::shell_socket_path;
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

    /// Bind at the path from [`shell_socket_path`].
    ///
    /// Same socket type, same framing, same handshake; the *only* thing
    /// that differs is which capability bits the server puts in its
    /// `Welcome`. Keeping the listener code identical is deliberate — the
    /// privilege must live in one place (which path a client reached), not
    /// in a second, subtly different transport.
    ///
    /// # Errors
    /// As [`Listener::bind`].
    pub fn bind_shell_default() -> Result<Self, Error> {
        Self::bind(&shell_socket_path())
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

/// A bound, non-blocking **TCP** listener: the remote half of the wire.
///
/// Separate from [`Listener`] because the two differ in exactly the
/// places a shared type would have to branch anyway — there is no path to
/// unlink, there *is* a bound address to report, and every socket it
/// accepts is marked remote. What is deliberately *not* different is
/// everything downstream: it yields the same [`ClientStream`], so the
/// server's read/decode/flush arms are the ones it already had.
#[derive(Debug)]
pub struct TcpListener {
    fd: OwnedFd,
    addr: std::net::SocketAddr,
}

impl TcpListener {
    /// Bind at `addr`. A port of 0 asks the kernel for one; the address
    /// actually bound is then [`TcpListener::addr`].
    ///
    /// # Errors
    /// Any `socket`/`bind`/`listen` failure.
    pub fn bind(addr: std::net::SocketAddr) -> Result<Self, Error> {
        let (fd, addr) = io::listen_tcp(addr)?;
        Ok(Self { fd, addr })
    }

    /// The address bound, with the port the kernel chose resolved.
    #[must_use]
    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
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
        Ok(io::accept_tcp(&self.fd)?.map(ClientStream::new))
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

    /// Whether the peer is on a remote link — no descriptors, in either
    /// direction.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.socket.is_remote()
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
    /// **Bounded on purpose, twice over.** The loop stops after roughly
    /// [`READ_BUDGET`] bytes, and *also* once the framer is holding
    /// [`MAX_PENDING_FDS`] unclaimed descriptors. Either way the socket
    /// stays readable, so the caller drains what arrived and the next
    /// epoll wakeup continues where this left off — which is the fairness
    /// the loop wants anyway.
    ///
    /// The fd check is the one that is easy to get wrong. Descriptors are
    /// claimed in [`ClientStream::next_msg`], not here, so they
    /// accumulate across every `recvmsg` in one call — and the *byte*
    /// budget does not bound them, because the kernel does not coalesce
    /// skbs carrying `SCM_RIGHTS`: each `recvmsg` returns one `sendmsg`'s
    /// worth, so 65 `CreateBuffer`s are 65 reads of ~32 bytes, nowhere
    /// near a megabyte. So at the cap this yields to the caller — whose
    /// drain is what claims them — instead of letting [`Framer::feed`]
    /// hit its own cap, which is fatal. That is what keeps a legitimate
    /// batch of buffers (a glyph atlas, a tiled surface) alive.
    ///
    /// Yielding is only correct while the caller can actually drain,
    /// though: at the cap with *nothing decodable left*, the pending
    /// descriptors belong to no frame and never will, and yielding
    /// forever would spin the event loop instead of killing the peer.
    /// That case is the flood and returns
    /// [`DecodeError::UnexpectedFd`]. The two together are the real
    /// test — do pending descriptors survive a drain? — and it is why
    /// the check sits *before* each `recvmsg`: once `feed` has seen the
    /// overflow the connection is already poisoned.
    ///
    /// A hangup is remembered rather than raised at once, so the bytes
    /// that arrived before it can still be decoded; [`Error::Closed`] is
    /// returned once the framer has nothing left either.
    ///
    /// # Errors
    /// [`Error::Closed`] on hangup with nothing left to decode, or
    /// [`DecodeError::UnexpectedFd`] when the peer sent descriptors that
    /// no frame claims.
    pub fn read(&mut self) -> Result<usize, Error> {
        let mut total = 0;
        while !self.closed && total < READ_BUDGET {
            if self.framer.pending_fds() >= MAX_PENDING_FDS {
                // At the cap. Yielding is only progress if the caller has
                // something to drain: decoding those frames is what claims
                // the descriptors. If nothing decodable is left, these fds
                // belong to no frame and never will — reading on would
                // accumulate more, and yielding forever would spin the
                // event loop at 100% without ever erroring. That is the
                // flood, and it is fatal.
                if self.framer.has_frame() {
                    break;
                }
                return Err(Error::Decode(DecodeError::UnexpectedFd));
            }
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
    ///
    /// The **one exception** is [`Error::RemoteNoFds`], which is not
    /// fatal: it means a remote peer sent an op that needs a descriptor
    /// (`CreateBuffer`) with none declared. The frame was consumed whole,
    /// so the stream is still synchronised and the caller may keep the
    /// client and carry on — which is what the server does, because a
    /// client that ignored `caps::REMOTE` deserves an explanation and not
    /// a dead socket. A frame that *declares* descriptors on a remote
    /// link is a different thing and stays fatal: the descriptors can
    /// never arrive, so the peer and the receiver disagree about the byte
    /// stream, and that is [`DecodeError::MissingFd`].
    pub fn next_msg(&mut self) -> Result<Option<ClientMsg>, Error> {
        let Some(frame) = self.framer.next_frame()? else {
            return Ok(None);
        };
        if self.socket.is_remote() && needs_fd(frame.op) {
            return Err(Error::RemoteNoFds);
        }
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

/// Whether an op can only be honoured with a file descriptor attached.
///
/// One op in v1. It is a function rather than a `matches!` at the call
/// site so that adding a second fd-carrying message is a change in one
/// place, and so the rule is greppable from the remote code that depends
/// on it.
#[must_use]
pub fn needs_fd(op: u16) -> bool {
    op == msg::CreateBuffer::OP
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
