//! Non-blocking Unix-socket transport: [`Socket`], plus the listener side.
//!
//! One `Socket` owns one connection. Sending queues into a [`Writer`] and
//! drains it with `sendmsg`; a short write leaves the remainder queued and
//! [`Socket::flush`] continues where it stopped. Receiving does one
//! `recvmsg` with an `SCM_RIGHTS` ancillary buffer sized for [`MAX_FDS`]
//! descriptors and pushes bytes plus fds straight into a [`Framer`].
//!
//! Nothing here allocates per message: the `Writer`'s byte buffer and the
//! `Framer`'s receive buffer are reused, and the ancillary buffer lives on
//! the stack.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::io::{Errno, IoSlice, IoSliceMut};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

use crate::codec::Writer;
use crate::framing::Framer;
use crate::{MAX_FDS, error::Error};

/// Ancillary buffer size: enough `cmsg` space for [`MAX_FDS`] descriptors.
const CMSG_SPACE: usize = rustix::cmsg_space!(ScmRights(16));

const _: () = assert!(MAX_FDS <= 16, "CMSG_SPACE must cover MAX_FDS");

/// Bytes pulled off the socket in one `recvmsg`. Big enough that a typical
/// burst of mutations arrives in one syscall, small enough to live in the
/// connection struct.
const RECV_CHUNK: usize = 64 * 1024;

/// A non-blocking `SOCK_STREAM` connection with `SCM_RIGHTS` fd passing.
///
/// The same type serves both directions: the client wraps the socket it
/// connected, the server the one it accepted.
#[derive(Debug)]
pub struct Socket {
    fd: OwnedFd,
    /// Reusable receive buffer.
    scratch: Vec<u8>,
}

impl Socket {
    /// Wrap an already-connected descriptor, switching it to non-blocking.
    ///
    /// # Errors
    /// If the descriptor cannot be made non-blocking.
    pub fn from_fd(fd: OwnedFd) -> Result<Self, Error> {
        rustix::io::ioctl_fionbio(&fd, true)?;
        Ok(Self {
            fd,
            scratch: vec![0; RECV_CHUNK],
        })
    }

    /// Connect to a Unix socket at `path`.
    ///
    /// # Errors
    /// Any `socket`/`connect` failure.
    pub fn connect(path: &Path) -> Result<Self, Error> {
        let addr = SocketAddrUnix::new(path)?;
        let fd = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )?;
        // A non-blocking connect to a Unix socket completes at once in
        // practice; tolerate the in-progress answer anyway.
        match rustix::net::connect(&fd, &addr) {
            Ok(()) | Err(Errno::INPROGRESS) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            fd,
            scratch: vec![0; RECV_CHUNK],
        })
    }

    /// The descriptor, for `epoll`/`poll` registration.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Consume the socket, returning its descriptor.
    #[must_use]
    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    /// Write as much of `w` as the kernel takes.
    ///
    /// On `EAGAIN` the unwritten bytes and their descriptors stay queued in
    /// `w`; call [`Socket::flush`] when the socket is writable again.
    /// Returns `true` when `w` is empty afterwards.
    ///
    /// # Errors
    /// [`Error::Closed`] if the peer hung up (`EPIPE`/`ECONNRESET`), or
    /// the underlying errno.
    pub fn send_all(&mut self, w: &mut Writer) -> Result<bool, Error> {
        while !w.bytes().is_empty() {
            match self.send_once(w) {
                Ok(()) | Err(Error::Io(Errno::INTR)) => {}
                Err(Error::Io(Errno::AGAIN)) => return Ok(false),
                Err(Error::Io(Errno::PIPE | Errno::CONNRESET)) => return Err(Error::Closed),
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Alias of [`Socket::send_all`], for the "continue a partial write"
    /// call site.
    ///
    /// # Errors
    /// As [`Socket::send_all`].
    pub fn flush(&mut self, w: &mut Writer) -> Result<bool, Error> {
        self.send_all(w)
    }

    /// One `sendmsg`: as many queued bytes as may go together, and the
    /// descriptors of the first fd-carrying frame among them.
    ///
    /// The split comes from the [`Writer`], which knows where each frame's
    /// header is; re-deriving it by re-parsing the buffer would be wrong
    /// after a partial write that stopped mid-header.
    fn send_once(&mut self, w: &mut Writer) -> Result<(), Error> {
        let (chunk_len, fd_count) = w.send_chunk();
        let mut space = [MaybeUninit::uninit(); CMSG_SPACE];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let borrowed = w.borrow_fds(fd_count);
        if !borrowed.is_empty() {
            control.push(SendAncillaryMessage::ScmRights(&borrowed));
        }
        let iov = [IoSlice::new(&w.bytes()[..chunk_len])];
        let sent = rustix::net::sendmsg(&self.fd, &iov, &mut control, SendFlags::NOSIGNAL)?;
        drop(borrowed);
        // The kernel attaches all ancillary data to the first byte, so the
        // descriptors are transferred as soon as anything was written.
        if sent > 0 {
            w.consume_fds(fd_count);
        }
        w.consume(sent);
        Ok(())
    }

    /// One `recvmsg` into `framer`.
    ///
    /// Returns the number of bytes received; `Ok(0)` never happens — a
    /// clean peer close is [`Error::Closed`], and "nothing readable" is
    /// `Ok(None)`.
    ///
    /// # Errors
    /// [`Error::Closed`] on peer hangup, otherwise the errno.
    pub fn recv_into(&mut self, framer: &mut Framer) -> Result<Option<usize>, Error> {
        let mut space = [MaybeUninit::uninit(); CMSG_SPACE];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut iov = [IoSliceMut::new(&mut self.scratch)];
        let msg =
            match rustix::net::recvmsg(&self.fd, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC) {
                Ok(m) => m,
                Err(Errno::AGAIN | Errno::INTR) => return Ok(None),
                Err(Errno::CONNRESET) => return Err(Error::Closed),
                Err(e) => return Err(e.into()),
            };
        let mut fds: Vec<OwnedFd> = Vec::new();
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = m {
                fds.extend(rights);
            }
        }
        if msg.bytes == 0 {
            // A clean EOF can still carry ancillary data; drop it with the
            // connection.
            return Err(Error::Closed);
        }
        framer.feed(&self.scratch[..msg.bytes], fds);
        Ok(Some(msg.bytes))
    }
}

/// Bind a listening Unix socket at `path`, non-blocking and close-on-exec.
///
/// The parent directory is created `0700` if missing and a stale socket
/// file is unlinked first.
///
/// # Errors
/// Any `mkdir`/`bind`/`listen` failure.
pub fn listen(path: &Path) -> Result<OwnedFd, Error> {
    use rustix::fs::{Mode, unlinkat};
    if let Some(dir) = path.parent() {
        match rustix::fs::mkdir(dir, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    match unlinkat(rustix::fs::CWD, path, rustix::fs::AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(e) => return Err(e.into()),
    }
    let addr = SocketAddrUnix::new(path)?;
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    rustix::net::bind(&fd, &addr)?;
    rustix::net::listen(&fd, 64)?;
    Ok(fd)
}

/// Accept one pending connection from a listener created by [`listen`].
///
/// Returns `Ok(None)` when nothing is pending.
///
/// # Errors
/// Any `accept` failure other than `EAGAIN`.
pub fn accept(listener: impl AsFd) -> Result<Option<Socket>, Error> {
    match rustix::net::accept_with(listener, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK) {
        Ok(fd) => Ok(Some(Socket::from_fd(fd)?)),
        Err(Errno::AGAIN | Errno::INTR) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A connected pair of sockets, for tests and in-process clients.
///
/// # Errors
/// Any `socketpair` failure.
pub fn pair() -> Result<(Socket, Socket), Error> {
    let (a, b) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    Ok((Socket::from_fd(a)?, Socket::from_fd(b)?))
}

#[cfg(test)]
mod tests {
    use crate::codec::Writer;
    use crate::header;

    /// A writer holding `specs` frames: `(payload_len, fd_count)`.
    fn writer(specs: &[(usize, usize)]) -> Writer {
        let mut w = Writer::new();
        for (i, &(len, fds)) in specs.iter().enumerate() {
            w.frame(i as u16 + 1, |w| {
                for _ in 0..fds {
                    w.put_fd(
                        rustix::fs::memfd_create("chunk-test", rustix::fs::MemfdFlags::CLOEXEC)
                            .expect("memfd"),
                    );
                }
                for _ in 0..len {
                    w.put_u8(0);
                }
                Ok(())
            })
            .expect("frame");
        }
        w
    }

    #[test]
    fn without_fds_the_whole_buffer_goes_at_once() {
        let w = writer(&[(4, 0), (8, 0)]);
        assert_eq!(w.send_chunk(), (w.len(), 0));
    }

    #[test]
    fn a_chunk_stops_before_the_next_fd_carrying_header() {
        // frame 0 has an fd, frame 1 has one too: they must not share a call.
        let w = writer(&[(4, 1), (8, 1)]);
        let first_frame = header::SIZE + 4;
        assert_eq!(w.send_chunk(), (first_frame, 1));
    }

    #[test]
    fn leading_fdless_frames_ride_with_the_first_fd_frame() {
        // The call must include the fd frame's header, so it may not stop
        // early; both frames go together.
        let w = writer(&[(4, 0), (8, 1)]);
        assert_eq!(w.send_chunk(), (w.len(), 1));
    }

    #[test]
    fn several_fds_on_one_frame_ride_together() {
        let w = writer(&[(4, 3)]);
        assert_eq!(w.send_chunk(), (w.len(), 3));
    }

    /// The regression this tracking exists for: a short write that stops
    /// *inside* a frame header must not make the next call mis-attribute
    /// descriptors. Re-parsing the buffer from a non-boundary offset reads
    /// a garbage fd count; tagged offsets stay correct.
    #[test]
    fn a_write_stopping_mid_header_keeps_fds_on_their_frame() {
        let mut w = writer(&[(4, 1), (8, 1)]);
        let (chunk, fds) = w.send_chunk();
        assert_eq!(fds, 1);

        // Pretend the kernel took the first frame and its fd, plus three
        // bytes of the next frame's header.
        w.consume_fds(fds);
        w.consume(chunk + 3);

        // The second frame's fd must still wait for a call that carries
        // the rest of its header: offset 0 now, since the header started
        // before the current front of the buffer.
        let (chunk2, fds2) = w.send_chunk();
        assert_eq!(fds2, 1, "the remaining fd belongs to the remaining frame");
        assert_eq!(chunk2, w.len(), "and the rest of the buffer may go now");
    }

    #[test]
    fn consuming_bytes_shifts_the_remaining_tags() {
        let mut w = writer(&[(4, 0), (8, 1), (4, 1)]);
        let (chunk, fds) = w.send_chunk();
        assert_eq!(fds, 1);
        w.consume_fds(fds);
        w.consume(chunk);
        // Only the last frame is left, with its own fd.
        let (chunk2, fds2) = w.send_chunk();
        assert_eq!((chunk2, fds2), (w.len(), 1));
    }
}
