//! The server's end of the helper socket: send requests, receive replies.
//!
//! Deliberately small: nitro-server will drive the socket from its own
//! epoll loop with [`Conn::send`] / [`Conn::flush`] / [`Conn::read`] /
//! [`Conn::next_reply`]; tests and tools use the blocking [`Conn::recv`].

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::{Duration, Instant};

use nitro_wire::{Framer, Socket, Writer};
use rustix::event::{PollFd, PollFlags};

use crate::proto::{FromHelper, Message, ToHelper};

/// A connection to a helper.
#[derive(Debug)]
pub struct Conn {
    sock: Socket,
    framer: Framer,
    w: Writer,
}

impl AsFd for Conn {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.sock.as_fd()
    }
}

impl Conn {
    /// Wrap a connected socket.
    #[must_use]
    pub fn new(sock: Socket) -> Self {
        Self {
            sock,
            framer: Framer::new(),
            w: Writer::new(),
        }
    }

    /// Queue a request (with its fds) and try to send everything queued.
    ///
    /// # Errors
    /// The message is over a protocol limit or has the wrong fd count
    /// (nothing queued), or the socket failed.
    pub fn send(&mut self, msg: &ToHelper, fds: Vec<OwnedFd>) -> Result<(), nitro_wire::Error> {
        msg.encode(&mut self.w, fds)?;
        self.flush().map(|_| ())
    }

    /// The outgoing buffer, for writing frames the typed API refuses to
    /// build (hostile-input tests). Call [`Conn::flush`] afterwards.
    pub fn writer(&mut self) -> &mut Writer {
        &mut self.w
    }

    /// Continue a partial write. Returns whether everything went out.
    ///
    /// # Errors
    /// The socket failed.
    pub fn flush(&mut self) -> Result<bool, nitro_wire::Error> {
        self.sock.send_all(&mut self.w)
    }

    /// One non-blocking read into the frame buffer. Returns the bytes
    /// read, 0 when nothing was readable. Fewer than
    /// [`nitro_wire::io::RECV_CHUNK`] means the socket is drained (or the
    /// read stopped at a descriptor boundary; a level-triggered poll wakes
    /// again for the rest).
    ///
    /// # Errors
    /// [`nitro_wire::Error::Closed`] when the helper is gone, or an errno.
    pub fn read(&mut self) -> Result<usize, nitro_wire::Error> {
        self.sock
            .recv_into(&mut self.framer)
            .map(|n| n.unwrap_or(0))
    }

    /// The next buffered reply, if a whole one has arrived.
    ///
    /// # Errors
    /// The stream is corrupt or the reply does not decode.
    pub fn next_reply(&mut self) -> Result<Option<(FromHelper, Vec<OwnedFd>)>, nitro_wire::Error> {
        match self.framer.next_frame()? {
            Some(f) => Ok(Some(FromHelper::decode(f)?)),
            None => Ok(None),
        }
    }

    /// Block (up to `timeout`) for the next reply.
    ///
    /// # Errors
    /// As [`Conn::read`] / [`Conn::next_reply`]; `Io(TIMEDOUT)` on timeout.
    pub fn recv(
        &mut self,
        timeout: Duration,
    ) -> Result<(FromHelper, Vec<OwnedFd>), nitro_wire::Error> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(m) = self.next_reply()? {
                return Ok(m);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(nitro_wire::Error::Io(rustix::io::Errno::TIMEDOUT));
            }
            let mut flags = PollFlags::IN;
            if !self.w.is_empty() {
                flags |= PollFlags::OUT;
            }
            let mut fds = [PollFd::from_borrowed_fd(self.sock.as_fd(), flags)];
            let ts = crate::timespec(left);
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            let readable = fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP);
            if !self.w.is_empty() {
                self.flush()?;
            }
            if readable {
                self.read()?;
            }
        }
    }

    /// Send `msg` and block for the next reply.
    ///
    /// # Errors
    /// As [`Conn::send`] and [`Conn::recv`].
    pub fn call(
        &mut self,
        msg: &ToHelper,
        fds: Vec<OwnedFd>,
        timeout: Duration,
    ) -> Result<(FromHelper, Vec<OwnedFd>), nitro_wire::Error> {
        self.send(msg, fds)?;
        self.recv(timeout)
    }
}

/// Whether a fence (`sync_file`, pipe) is signalled, waiting up to
/// `timeout`.
#[must_use]
pub fn fence_signalled(fd: impl AsFd, timeout: Duration) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let ts = crate::timespec(timeout);
    matches!(rustix::event::poll(&mut fds, Some(&ts)), Ok(n) if n > 0)
}
