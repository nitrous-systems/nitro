//! Non-blocking stream transport: [`Socket`], plus the listener side.
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
//!
//! # Local and remote are the same socket
//!
//! Since M4-E1 the same type also drives a **TCP** connection
//! ([`Socket::connect_tcp`], [`listen_tcp`]). The framing never depended
//! on the socket being local (`docs/wire.md` §Transport), so the only
//! difference is one flag: a remote socket refuses to send a frame
//! carrying descriptors ([`Error::RemoteNoFds`]) and passes no ancillary
//! buffer to `recvmsg`, because there is nothing that could arrive in one.
//! Everything else — the partial-write bookkeeping, the framer, the
//! budgets — is shared, which is the point: a second transport with its
//! own state machine is a second place for the protocol to drift.

use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use rustix::io::{Errno, IoSlice, IoSliceMut};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType, sockopt,
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

/// How long a TCP connection may sit idle before the kernel probes it.
///
/// The numbers below are the whole answer to "a cable was yanked": a
/// remote client's windows are the server's to clean up, and without
/// keepalive a half-open connection leaves them on screen until the
/// server is restarted — TCP itself will not notice, because neither end
/// sends anything to an idle app. With `10 s` idle, `5 s` between probes
/// and `3` probes the server gives up about **25 s** after the last byte,
/// which is slow enough not to kill a link that hiccups and fast enough
/// that a person watching the screen sees the window go rather than
/// wondering. The harness tests only the RST/close case, which is
/// instant; the keepalive window is what `docs/remote.md` reports from
/// the box run.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
/// Seconds between keepalive probes once the idle timer has fired.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Unanswered probes before the connection is declared dead.
const KEEPALIVE_COUNT: u32 = 3;

/// A non-blocking `SOCK_STREAM` connection with `SCM_RIGHTS` fd passing.
///
/// The same type serves both directions: the client wraps the socket it
/// connected, the server the one it accepted. It also serves both
/// transports — a Unix socket and a TCP one differ here by the `remote`
/// flag and nothing else.
#[derive(Debug)]
pub struct Socket {
    fd: OwnedFd,
    /// Reusable receive buffer.
    scratch: Vec<u8>,
    /// Whether this link can carry file descriptors.
    ///
    /// A `bool` rather than a transport enum: the only thing any caller
    /// asks is "may I pass an fd here?", and a second copy of "which
    /// kind of socket is this" would be a fact that can drift from the
    /// descriptor it describes.
    remote: bool,
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
            remote: false,
        })
    }

    /// Wrap an already-connected **TCP** descriptor, switching it to
    /// non-blocking and marking the link remote.
    ///
    /// `TCP_NODELAY` is set here, on both ends of every link this crate
    /// makes. This is a latency protocol: a commit is a small write and
    /// the answer to it is a small write, which is exactly the traffic
    /// Nagle's algorithm holds back waiting for more to coalesce.
    /// `docs/remote.md` has the measured difference.
    ///
    /// # Errors
    /// If the descriptor cannot be made non-blocking, or the socket
    /// option cannot be set.
    pub fn from_tcp(fd: OwnedFd) -> Result<Self, Error> {
        rustix::io::ioctl_fionbio(&fd, true)?;
        sockopt::set_tcp_nodelay(&fd, true)?;
        Ok(Self {
            fd,
            scratch: vec![0; RECV_CHUNK],
            remote: true,
        })
    }

    /// Whether this link is remote, i.e. cannot carry descriptors.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.remote
    }

    /// Connect to a TCP address, trying each candidate in order.
    ///
    /// The connect itself is **blocking**, unlike the Unix one: a remote
    /// connect legitimately takes a round trip, and "try the next
    /// address" only means anything if this one is known to have failed.
    /// The socket is switched to non-blocking immediately afterwards, so
    /// the event loop sees exactly what it sees for a Unix socket.
    ///
    /// # Errors
    /// The last candidate's error if none connected, or
    /// [`Error::BadEndpoint`] if the list is empty.
    pub fn connect_tcp(addrs: &[SocketAddr]) -> Result<Self, Error> {
        let mut last: Option<Error> = None;
        for addr in addrs {
            match Self::connect_one_tcp(*addr) {
                Ok(s) => return Ok(s),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::BadEndpoint("no address to connect to".to_owned())))
    }

    /// Connect one address, blocking, then go non-blocking.
    fn connect_one_tcp(addr: SocketAddr) -> Result<Self, Error> {
        let fd = rustix::net::socket_with(
            family_of(addr),
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            Some(rustix::net::ipproto::TCP),
        )?;
        loop {
            match rustix::net::connect(&fd, &addr) {
                Ok(()) => break,
                Err(Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Self::from_tcp(fd)
    }

    /// Turn on `SO_KEEPALIVE` with the timings documented at
    /// [`KEEPALIVE_IDLE`].
    ///
    /// The server calls this on every socket it accepts on the remote
    /// listener. A client does not need to: it notices a dead server the
    /// moment it writes, and an app with nothing to draw is the normal
    /// state rather than a fault. The server is the side holding
    /// resources — windows on a screen — for a peer that may never speak
    /// again.
    ///
    /// # Errors
    /// Any `setsockopt` failure.
    pub fn set_keepalive(&self) -> Result<(), Error> {
        sockopt::set_socket_keepalive(&self.fd, true)?;
        sockopt::set_tcp_keepidle(&self.fd, KEEPALIVE_IDLE)?;
        sockopt::set_tcp_keepintvl(&self.fd, KEEPALIVE_INTERVAL)?;
        sockopt::set_tcp_keepcnt(&self.fd, KEEPALIVE_COUNT)?;
        Ok(())
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
            remote: false,
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
    /// [`Error::Closed`] if the peer hung up (`EPIPE`/`ECONNRESET`),
    /// [`Error::RemoteNoFds`] if a queued frame carries descriptors and
    /// this link is remote, or the underlying errno.
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
    ///
    /// On a remote socket a frame with descriptors is refused **here**,
    /// with nothing written. It cannot be allowed through: the header
    /// would declare descriptors the far end can never receive, and a
    /// receiver that waits for them has a desynchronised stream. The
    /// caller's frame is still queued, so the caller must drop it (the
    /// toolkit does, by never building one) — but no half-frame is on the
    /// wire, which is the invariant that matters.
    fn send_once(&mut self, w: &mut Writer) -> Result<(), Error> {
        let (chunk_len, fd_count) = w.send_chunk();
        if self.remote && fd_count > 0 {
            return Err(Error::RemoteNoFds);
        }
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
        if self.remote {
            return self.recv_plain(framer);
        }
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

    /// The remote receive path: one `recvmsg` with **no** ancillary
    /// buffer.
    ///
    /// Not a micro-optimisation — it is the statement that a remote link
    /// has no descriptor channel at all. A TCP socket cannot produce an
    /// `SCM_RIGHTS` cmsg, so asking for one would be asking a question
    /// with one possible answer; and if a future transport ever could,
    /// this is the one place that would have to decide what to do about
    /// it, rather than silently accepting descriptors on a link the rest
    /// of the stack believes has none.
    fn recv_plain(&mut self, framer: &mut Framer) -> Result<Option<usize>, Error> {
        let mut iov = [IoSliceMut::new(&mut self.scratch)];
        let n = match rustix::io::readv(&self.fd, &mut iov) {
            Ok(n) => n,
            Err(Errno::AGAIN | Errno::INTR) => return Ok(None),
            Err(Errno::CONNRESET) => return Err(Error::Closed),
            Err(e) => return Err(e.into()),
        };
        if n == 0 {
            return Err(Error::Closed);
        }
        framer.feed(&self.scratch[..n], std::iter::empty());
        Ok(Some(n))
    }
}

/// Bind a listening Unix socket at `path`, non-blocking and close-on-exec.
///
/// The parent directory is created `0700` if missing and a stale socket
/// file is unlinked first.
///
/// **The path appears only once the socket accepts.** `bind` creates the
/// file immediately, but a socket does not queue connections until `listen`
/// has run, so a peer that lands in that window gets `ECONNREFUSED` from a
/// path that plainly exists. Every test harness in this tree waits for the
/// socket *file* and then connects, which is exactly how to land in it. Not
/// hypothetical: it was **observed** failing on `main` at `3efa468`. It is
/// rare, and deliberately not quantified here — no defensible rate was
/// measured, and the mechanism is the whole argument.
///
/// So the bind happens on a temporary name in the same directory and is
/// `rename`d into place after `listen` — `rename(2)` within one directory is
/// atomic, so the path either does not exist or names a socket that is
/// already accepting. The temporary is unlinked on any failure rather than
/// left behind. Two servers racing one path still resolve last-writer-wins,
/// exactly as the previous unlink-then-bind did.
///
/// One consequence worth knowing: the *staged* name is the length-limiting
/// one, since it adds about 17 bytes (`.` plus `.<pid>.staging`) to the file
/// name. A path within ~17 bytes of `sun_path`'s 108-byte limit therefore
/// fails here where a direct bind would have fitted. It fails cleanly, as an
/// error from [`SocketAddrUnix::new`] at startup, and the runtime-directory
/// paths this crate resolves are nowhere near the limit.
///
/// # Errors
/// Any `mkdir`/`bind`/`listen`/`rename` failure, including a socket path so
/// long that the staged name exceeds `sun_path`.
pub fn listen(path: &Path) -> Result<OwnedFd, Error> {
    use rustix::fs::{Mode, unlinkat};
    if let Some(dir) = path.parent() {
        match rustix::fs::mkdir(dir, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // Same directory, so the rename below is a rename and not a copy. The
    // pid keeps two servers racing to the same path from colliding on the
    // temporary itself.
    let staging = staging_path(path);
    for p in [path, staging.as_path()] {
        match unlinkat(rustix::fs::CWD, p, rustix::fs::AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let addr = SocketAddrUnix::new(&staging)?;
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )?;
    // From here on a failure must not leave the staging file behind, so each
    // step cleans up before returning.
    let publish = || -> Result<(), Errno> {
        rustix::net::bind(&fd, &addr)?;
        rustix::net::listen(&fd, 64)?;
        rustix::fs::renameat(rustix::fs::CWD, &staging, rustix::fs::CWD, path)
    };
    match publish() {
        Ok(()) => Ok(fd),
        Err(e) => {
            let _ = unlinkat(rustix::fs::CWD, &staging, rustix::fs::AtFlags::empty());
            Err(e.into())
        }
    }
}

/// The temporary name [`listen`] binds before publishing, next to `path`.
fn staging_path(path: &Path) -> std::path::PathBuf {
    let pid = rustix::process::getpid().as_raw_nonzero().get();
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("socket");
    let staged = format!(".{name}.{pid}.staging");
    match path.parent() {
        Some(dir) => dir.join(staged),
        None => std::path::PathBuf::from(staged),
    }
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

/// The address family a socket address needs.
fn family_of(addr: SocketAddr) -> AddressFamily {
    match addr {
        SocketAddr::V4(_) => AddressFamily::INET,
        SocketAddr::V6(_) => AddressFamily::INET6,
    }
}

/// Bind a listening **TCP** socket at `addr`, non-blocking and
/// close-on-exec, and report the address it actually bound.
///
/// A port of 0 asks the kernel for one, which is what the tests use and
/// why the bound address is returned rather than assumed: `remote.listen
/// = 127.0.0.1:0` is the only way to run the suite without picking a
/// fixed port and racing every other developer on the box.
///
/// `SO_REUSEADDR` is set, so a server restarted while a previous
/// connection sits in `TIME_WAIT` comes back up instead of failing with
/// `EADDRINUSE` — the failure mode of every daemon that omits it, and
/// one a person restarting the desktop would meet immediately. It does
/// **not** let two servers share a port: that needs `SO_REUSEPORT`,
/// which is deliberately not set.
///
/// # Errors
/// Any `socket`/`bind`/`listen`/`getsockname` failure — in particular
/// `EADDRINUSE` for a port already taken and `EACCES` for a privileged
/// one. The caller warns and runs without a remote listener rather than
/// refusing to start: a bad `remote.listen` must not cost the user their
/// desktop.
pub fn listen_tcp(addr: SocketAddr) -> Result<(OwnedFd, SocketAddr), Error> {
    let fd = rustix::net::socket_with(
        family_of(addr),
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        Some(rustix::net::ipproto::TCP),
    )?;
    sockopt::set_socket_reuseaddr(&fd, true)?;
    rustix::net::bind(&fd, &addr)?;
    rustix::net::listen(&fd, 16)?;
    let bound = rustix::net::getsockname(&fd)?;
    let bound = SocketAddr::try_from(bound)
        .map_err(|_| Error::BadEndpoint("the bound address is not IP".to_owned()))?;
    Ok((fd, bound))
}

/// Accept one pending connection from a listener created by
/// [`listen_tcp`].
///
/// The accepted socket is marked remote and gets `TCP_NODELAY` (in
/// [`Socket::from_tcp`]) plus the keepalive timings, which is the server
/// side of the "a yanked cable frees the windows" promise.
///
/// # Errors
/// Any `accept` failure other than `EAGAIN`. A keepalive option that
/// will not set is **not** fatal: the connection works, it merely takes
/// the kernel's default to notice a dead peer.
pub fn accept_tcp(listener: impl AsFd) -> Result<Option<Socket>, Error> {
    match rustix::net::accept_with(listener, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK) {
        Ok(fd) => {
            let socket = Socket::from_tcp(fd)?;
            let _ = socket.set_keepalive();
            Ok(Some(socket))
        }
        Err(Errno::AGAIN | Errno::INTR) => Ok(None),
        Err(e) => Err(e.into()),
    }
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
