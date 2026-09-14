//! SIGTERM / SIGINT / SIGHUP → readable fds, via the self-pipe trick.
//! `signal-hook` does the async-signal-safe part: an empty `send` from the
//! handler. That is why the "pipe" is a `UnixDatagram` pair — an empty
//! datagram is readable, an empty write to a stream socket is not.
//!
//! # Why two pairs and not one
//!
//! A self-pipe carries no payload: the handler writes nothing, and the
//! reader learns only that *a* signal arrived. That is enough while every
//! signal means the same thing, and stops being enough the moment SIGHUP
//! means "re-read `server.conf`" and SIGTERM means "shut down" — confusing
//! the two would either kill the desktop on a reload or silently ignore a
//! shutdown. Writing the signal number into the datagram would work too,
//! but then the *reader* has to parse a byte the handler wrote, and the
//! epoll set would still have to demultiplex it. Two pairs, two fds and two
//! epoll tokens keep the kernel doing the demultiplexing, and keep each
//! drain a plain `bool`.

use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixDatagram;

use signal_hook::SigId;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::low_level::{pipe, unregister};

/// Installed handlers plus the read ends they write to.
#[derive(Debug)]
pub struct Signals {
    quit: UnixDatagram,
    reload: UnixDatagram,
    ids: Vec<SigId>,
}

/// Consume every queued (empty) datagram on one socket. Returns whether
/// anything was there.
fn drain_socket(socket: &UnixDatagram) -> bool {
    let mut got = false;
    let mut buf = [0u8; 64];
    loop {
        match socket.recv(&mut buf) {
            Ok(_) => got = true,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return got,
        }
    }
}

impl Signals {
    /// Register SIGTERM and SIGINT on the quit fd, SIGHUP on the reload fd.
    ///
    /// # Errors
    /// Socket-pair creation or handler registration failure.
    pub fn install() -> io::Result<Self> {
        let (quit, quit_write) = UnixDatagram::pair()?;
        quit.set_nonblocking(true)?;
        let (reload, reload_write) = UnixDatagram::pair()?;
        reload.set_nonblocking(true)?;
        let term = pipe::register(SIGTERM, quit_write.try_clone()?)?;
        let int = pipe::register(SIGINT, quit_write)?;
        let hup = pipe::register(SIGHUP, reload_write)?;
        let mut this = Self {
            quit,
            reload,
            ids: vec![term, int, hup],
        };
        // `register` probes the fd with an empty `send`, which lands in
        // our datagram queue like a real signal would.
        this.drain();
        this.drain_reload();
        Ok(this)
    }

    /// The fd that becomes readable on SIGTERM or SIGINT.
    #[must_use]
    pub fn quit_fd(&self) -> BorrowedFd<'_> {
        self.quit.as_fd()
    }

    /// The fd that becomes readable on SIGHUP.
    #[must_use]
    pub fn reload_fd(&self) -> BorrowedFd<'_> {
        self.reload.as_fd()
    }

    /// Consume every queued (empty) datagram on the quit fd. Returns
    /// whether a SIGTERM or SIGINT arrived.
    pub fn drain(&mut self) -> bool {
        drain_socket(&self.quit)
    }

    /// Consume every queued datagram on the reload fd. Returns whether a
    /// SIGHUP arrived.
    pub fn drain_reload(&mut self) -> bool {
        drain_socket(&self.reload)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            unregister(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True when `signal` is blocked for this process (some sandboxes do
    /// that), in which case delivery cannot be observed.
    fn blocked(signal: i32) -> bool {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        status
            .lines()
            .find_map(|l| l.strip_prefix("SigBlk:"))
            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
            .is_some_and(|mask| mask & (1 << (signal - 1)) != 0)
    }

    /// Raise a signal at ourselves and wait for `f` to see it, with a
    /// deadline: a test that hangs tells you nothing.
    fn raise_and_wait(signal: rustix::process::Signal, mut f: impl FnMut() -> bool) {
        rustix::process::kill_process(rustix::process::getpid(), signal).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !f() {
            assert!(std::time::Instant::now() < deadline, "signal never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn install_drain_and_unregister() {
        let mut s = Signals::install().unwrap();
        assert!(!s.drain(), "nothing queued yet");
        if blocked(SIGTERM) {
            eprintln!("SIGTERM is blocked in this environment; skipping delivery check");
            return;
        }
        // Raise SIGTERM at ourselves: the handler writes to the pipe.
        raise_and_wait(rustix::process::Signal::TERM, || s.drain());
        drop(s);
    }

    #[test]
    fn a_raised_sighup_is_reported_as_reload_and_not_as_quit() {
        let mut s = Signals::install().unwrap();
        assert!(!s.drain_reload(), "nothing queued yet");
        if blocked(SIGHUP) {
            eprintln!("SIGHUP is blocked in this environment; skipping delivery check");
            return;
        }
        raise_and_wait(rustix::process::Signal::HUP, || s.drain_reload());
        // The whole point of the second pair: a reload must not read as a
        // request to shut the desktop down.
        assert!(!s.drain(), "SIGHUP must not look like SIGTERM");
        drop(s);
    }

    #[test]
    fn a_drained_reload_fd_stops_being_readable() {
        // The property a level-triggered epoll set depends on, and whose
        // absence is a 100 % CPU spin rather than a wrong answer.
        //
        // The reload fd is registered before `wait_active` runs, and that
        // loop waits for an inactive VT. An undrained datagram keeps the fd
        // readable forever, so a SIGHUP arriving there made every `wait`
        // return immediately — spinning, and logging a line per iteration,
        // until the VT happened to become active. `wait_active` therefore
        // drains this fd, and this pins the half that makes draining work.
        let mut s = Signals::install().unwrap();
        if blocked(SIGHUP) {
            eprintln!("SIGHUP is blocked in this environment; skipping delivery check");
            return;
        }
        raise_and_wait(rustix::process::Signal::HUP, || s.drain_reload());
        assert!(!s.drain_reload(), "a drained fd has nothing left to report");

        // And it is genuinely not readable any more, which is what epoll
        // asks and what `drain_reload`'s `bool` does not answer.
        let fd = s.reload_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let ready = rustix::event::poll(
            &mut fds,
            Some(&rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .unwrap();
        assert_eq!(ready, 0, "a drained reload fd must not wake epoll again");
        drop(s);
    }
}
