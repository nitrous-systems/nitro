//! SIGTERM / SIGINT → a readable fd, via the self-pipe trick. `signal-hook`
//! does the async-signal-safe part: an empty `send` from the handler. That
//! is why the "pipe" is a `UnixDatagram` pair — an empty datagram is
//! readable, an empty write to a stream socket is not.

use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixDatagram;

use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::low_level::{pipe, unregister};

/// Installed handlers plus the read end they write to.
#[derive(Debug)]
pub struct Signals {
    read: UnixDatagram,
    ids: Vec<SigId>,
}

impl Signals {
    /// Register SIGTERM and SIGINT.
    ///
    /// # Errors
    /// Socket-pair creation or handler registration failure.
    pub fn install() -> io::Result<Self> {
        let (read, write) = UnixDatagram::pair()?;
        read.set_nonblocking(true)?;
        let term = pipe::register(SIGTERM, write.try_clone()?)?;
        let int = pipe::register(SIGINT, write)?;
        let mut this = Self {
            read,
            ids: vec![term, int],
        };
        // `register` probes the fd with an empty `send`, which lands in
        // our datagram queue like a real signal would.
        this.drain();
        Ok(this)
    }

    /// Consume every queued (empty) datagram. Returns whether a signal
    /// arrived.
    pub fn drain(&mut self) -> bool {
        let mut got = false;
        let mut buf = [0u8; 64];
        loop {
            match self.read.recv(&mut buf) {
                Ok(_) => got = true,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return got,
            }
        }
    }
}

impl AsFd for Signals {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
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

    /// True when SIGTERM is blocked for this process (some sandboxes do
    /// that), in which case delivery cannot be observed.
    fn sigterm_blocked() -> bool {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        status
            .lines()
            .find_map(|l| l.strip_prefix("SigBlk:"))
            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
            .is_some_and(|mask| mask & (1 << (SIGTERM - 1)) != 0)
    }

    #[test]
    fn install_drain_and_unregister() {
        let mut s = Signals::install().unwrap();
        assert!(!s.drain(), "nothing queued yet");
        if sigterm_blocked() {
            eprintln!("SIGTERM is blocked in this environment; skipping delivery check");
            return;
        }
        // Raise SIGTERM at ourselves: the handler writes to the pipe.
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !s.drain() {
            assert!(std::time::Instant::now() < deadline, "signal never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(s);
    }
}
