//! One supervised process: how it is started, how its exit is noticed,
//! and how it is stopped.
//!
//! # A child's exit is a descriptor, not a timer
//!
//! The supervisor sits in one `poll(2)` over the session socket, the
//! signal pipe and **one pidfd per running child**
//! ([`rustix::process::pidfd_open`]). A pidfd becomes readable when the
//! process it names exits, so "the bar died" arrives the same way "a
//! client connected" does: as a wakeup on a descriptor the loop already
//! had.
//!
//! The obvious alternative — `SIGCHLD` — is worse here for a reason that
//! is specific to this process. `signal-hook`'s self-pipe would tell us
//! *a* child changed state, and we would then `waitpid(-1)` in a loop to
//! find out which. That is fine, until you remember what else lives in
//! this process tree: the session is PID 1 of nothing, but it is the
//! parent of the server, and a `waitpid(-1)` cannot distinguish "the
//! piece I supervise exited" from "something reparented onto me exited".
//! With a pidfd per child, the loop asks about exactly the processes it
//! started, and the `waitpid` it then does is `waitpid(that_pid)`.
//!
//! The second alternative — polling with a 1 s timer — is what a
//! supervisor written in a hurry does, and it would cost the session a
//! wakeup per second forever, on a box whose whole idle-CPU claim is
//! `0.0 %` with **zero** context switches. The pidfd keeps the session
//! asleep between events, which is the only acceptable idle cost for a
//! process that runs for the whole login.
//!
//! # Stopping is SIGTERM, then SIGKILL, and the deadline is shared
//!
//! [`Child::terminate`] sends `SIGTERM` to the child's **process group**
//! and nothing else; the waiting is [`crate::session`]'s, because the
//! whole teardown shares one deadline rather than giving each piece its
//! own. The group, not the pid, because a shell piece that has itself
//! spawned something (the launcher's applications are in their own
//! groups, but a future piece may not be) should not leave the tree
//! holding the terminal.
//!
//! # Stdio
//!
//! stdout and stderr are inherited, so everything the pieces print lands
//! in the same journal, in one stream, in the order it happened. That is
//! the whole debugging story on the box: `journalctl -u nitro-dev -f`.
//!
//! stdin is inherited **only by the server**, and this is load-bearing on
//! real hardware: the unit gives the session `/dev/tty2` as stdin
//! (`StandardInput=tty-force`), and it is the compositor that needs a
//! terminal to be on a VT of. A shell client with a tty on stdin would be
//! a toolkit app that can be Ctrl-C'd from a keyboard the compositor also
//! owns; they get `/dev/null`.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child as StdChild, Command, Stdio};
use std::time::Instant;

use rustix::process::{Pid, Signal};

use crate::pieces::Role;

/// A running (or just-exited) supervised process.
#[derive(Debug)]
pub struct Child {
    /// Name of the piece, for logs.
    pub name: String,
    /// What it is.
    pub role: Role,
    /// The process itself, for `try_wait`.
    proc: StdChild,
    /// Readable once it exits; registered with the loop's `poll`.
    pidfd: std::os::fd::OwnedFd,
    /// When it was started, for the backoff's "did this run last?".
    started: Instant,
    /// Set once `SIGTERM` has been sent, so teardown is idempotent.
    termed: bool,
}

/// Why a child could not be started.
#[derive(Debug)]
pub enum SpawnError {
    /// `execvp` (or the fork) failed: the usual cause is a binary that is
    /// neither next to the session nor on `$PATH`.
    Spawn(std::io::Error),
    /// The process started but no pidfd could be opened for it. The child
    /// is killed rather than left unsupervised — an unwatched child of a
    /// supervisor is worse than no child, because nothing would ever
    /// restart it and nothing would reap it.
    NoPidfd(rustix::io::Errno),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "spawn: {e}"),
            Self::NoPidfd(e) => write!(f, "pidfd_open: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

impl Child {
    /// Start `program` (already resolved to a path or a bare name) with
    /// `args`, in its own process group, and open a pidfd for it.
    ///
    /// # Errors
    /// [`SpawnError::Spawn`] if the program could not be started,
    /// [`SpawnError::NoPidfd`] if it started but cannot be watched.
    pub fn spawn(
        name: &str,
        role: Role,
        program: &Path,
        args: &[String],
    ) -> Result<Self, SpawnError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            // Inherited: one journal, one ordering, no pipe for the
            // session to have to drain (a supervisor that owned its
            // children's stdout would deadlock the moment it blocked in
            // `poll` while a child filled the pipe).
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if role == Role::Server {
            // The compositor's tty; see the module docs.
            cmd.stdin(Stdio::inherit());
        } else {
            cmd.stdin(Stdio::null());
        }
        // A group of its own, so a stray Ctrl-C or a `kill -- -PID` of
        // the session's group does not race our orderly teardown — and so
        // `terminate` can signal a piece and its descendants together.
        cmd.process_group(0);
        let child = cmd.spawn().map_err(SpawnError::Spawn)?;
        let pid = Pid::from_child(&child);
        let pidfd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::NONBLOCK) {
            Ok(fd) => fd,
            Err(e) => {
                // Started but unwatchable: kill it and report. Racing a
                // pid here is not a concern — the child has not been
                // reaped, so the pid is still ours.
                let mut child = child;
                let _ = child.kill();
                let _ = child.wait();
                return Err(SpawnError::NoPidfd(e));
            }
        };
        Ok(Self {
            name: name.to_owned(),
            role,
            proc: child,
            pidfd,
            started: Instant::now(),
            termed: false,
        })
    }

    /// The process id, for logs and for `kill`.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.proc.id()
    }

    /// How long this run has lasted so far.
    #[must_use]
    pub fn uptime(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// The descriptor that becomes readable when the child exits.
    #[must_use]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.pidfd.as_fd()
    }

    /// Reap the child if it has exited; `None` while it runs.
    ///
    /// Returns the exit status as a small enum rather than
    /// `ExitStatus`, because everything downstream wants "what code do
    /// *we* exit with" and "what do we print", and both of those are
    /// awkward to derive from `ExitStatus` twice.
    pub fn reap(&mut self) -> Option<Exit> {
        match self.proc.try_wait() {
            Ok(Some(status)) => Some(Exit::from_status(status)),
            // A child we cannot wait for is a child we will never see
            // exit; treat it as gone rather than spinning on its pidfd.
            Err(_) => Some(Exit::Unknown),
            Ok(None) => None,
        }
    }

    /// Wait for the child, blocking. Only used after `SIGKILL`, where
    /// the wait is bounded by the kernel rather than by the child's
    /// willingness to cooperate.
    ///
    /// # Errors
    /// Whatever `waitpid` says.
    pub fn wait_blocking(&mut self) -> std::io::Result<Exit> {
        self.proc.wait().map(Exit::from_status)
    }

    /// Send `SIGTERM` to the child's process group. Idempotent.
    ///
    /// Failures are swallowed on purpose: the only interesting one is
    /// `ESRCH`, which means the child exited between the poll and here —
    /// exactly what we were asking for.
    pub fn terminate(&mut self) {
        if self.termed {
            return;
        }
        self.termed = true;
        self.signal(Signal::TERM);
    }

    /// Send `SIGKILL` to the child's process group, for a piece that
    /// ignored the deadline.
    pub fn kill(&mut self) {
        self.signal(Signal::KILL);
    }

    fn signal(&self, sig: Signal) {
        let raw = i32::try_from(self.proc.id()).unwrap_or(i32::MAX);
        if let Some(pid) = Pid::from_raw(raw) {
            // The group first — that is the whole tree the piece owns —
            // and the process itself as the fallback for the case where
            // the group no longer exists but the process does.
            if rustix::process::kill_process_group(pid, sig).is_err() {
                let _ = rustix::process::kill_process(pid, sig);
            }
        }
    }
}

/// How a supervised process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Exited with a status code.
    Code(i32),
    /// Killed by a signal.
    Signal(i32),
    /// `waitpid` failed; the process is gone but the status is not known.
    Unknown,
}

impl Exit {
    fn from_status(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(code) = status.code() {
            Self::Code(code)
        } else if let Some(sig) = status.signal() {
            Self::Signal(sig)
        } else {
            Self::Unknown
        }
    }

    /// The process exit code the session should use when the **server**
    /// ended this way.
    ///
    /// A signalled server becomes `128 + signo`, the shell convention,
    /// so `systemctl status` shows something a reader can decode instead
    /// of a flat `1`. A clean `0` stays `0`, which is what makes
    /// `systemctl stop nitro-dev` report success.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::Code(c) => u8::try_from(c).unwrap_or(1),
            Self::Signal(s) => u8::try_from(128 + s).unwrap_or(1),
            Self::Unknown => 1,
        }
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(c) => write!(f, "exit {c}"),
            Self::Signal(s) => write!(f, "signal {s}"),
            Self::Unknown => write!(f, "gone (status unknown)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_an_error_and_not_a_panic() {
        let e = Child::spawn(
            "ghost",
            Role::Shell,
            Path::new("nitro-no-such-binary-ever"),
            &[],
        )
        .expect_err("not on PATH");
        assert!(matches!(e, SpawnError::Spawn(_)), "{e}");
    }

    #[test]
    fn a_child_that_exits_is_noticed_through_its_pidfd() {
        let mut c = Child::spawn(
            "true",
            Role::Shell,
            Path::new("/bin/sh"),
            &["-c".to_owned(), "exit 7".to_owned()],
        )
        .expect("spawn");
        // The pidfd is the wakeup: poll it rather than sleeping.
        let fd = c.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let ts = rustix::event::Timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        let n = rustix::event::poll(&mut fds, Some(&ts)).expect("poll");
        assert_eq!(n, 1, "the pidfd became readable when the child exited");
        assert_eq!(c.reap(), Some(Exit::Code(7)));
    }

    #[test]
    fn terminate_is_idempotent_and_a_signalled_child_reports_its_signal() {
        let mut c = Child::spawn(
            "sleeper",
            Role::Shell,
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 60".to_owned()],
        )
        .expect("spawn");
        c.terminate();
        c.terminate(); // the second one must not send a second signal
        let fd = c.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let ts = rustix::event::Timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };
        rustix::event::poll(&mut fds, Some(&ts)).expect("poll");
        assert_eq!(c.reap(), Some(Exit::Signal(15)));
    }

    #[test]
    fn a_child_is_in_its_own_process_group() {
        // What makes `terminate` able to signal a piece's whole tree
        // without signalling the session's.
        let out = Command::new("/bin/sh")
            .args(["-c", "ps -o pgid= -p $$"])
            .process_group(0)
            .output()
            .expect("ps");
        let child_pgid: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("a pgid");
        assert_ne!(
            child_pgid,
            rustix::process::getpgrp().as_raw_nonzero().get()
        );
    }

    #[test]
    fn the_exit_code_follows_the_shell_convention() {
        assert_eq!(Exit::Code(0).code(), 0);
        assert_eq!(Exit::Code(3).code(), 3);
        assert_eq!(Exit::Signal(15).code(), 143);
        assert_eq!(Exit::Unknown.code(), 1);
    }
}
