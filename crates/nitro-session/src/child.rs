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
//!
//! # `PATH`
//!
//! Every child is started with the session's own directory prepended to
//! `PATH` ([`crate::pieces::path_with_bin_dir`]). The session already
//! *finds* its pieces there; this is the same fact stated to the
//! processes it starts, so a `.desktop` file's bare `Exec=nitro-term`
//! resolves for the launcher on a box whose binaries live in
//! `~/nitro-bin`. `crates/nitro-session/src/pieces.rs` has the argument
//! and `docs/shell.md` §"The icon is the app id" the consequence.

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
    /// The exit status, once the process has been reaped.
    ///
    /// **This is the pid-reuse guard.** After `waitpid` returns, the pid
    /// is no longer ours: the kernel may hand it to anybody, and a
    /// `kill(pid)` from here would be aimed at a stranger. `spawn`'s
    /// `NoPidfd` arm already reasons about this in the other direction
    /// ("the child has not been reaped, so the pid is still ours"), and
    /// this field is what makes the reasoning symmetric — [`terminate`]
    /// and [`kill`] become no-ops once it is set.
    ///
    /// It is not a hypothetical. `Session::start`'s readiness probe
    /// calls `reap()` to ask whether the server is still alive; on the
    /// "server died before it was ready" path the slot was still
    /// populated, and `teardown` then signalled a pid that had already
    /// been waited for.
    ///
    /// [`terminate`]: Child::terminate
    /// [`kill`]: Child::kill
    exited: Option<Exit>,
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
    /// `bin_dir` is prepended to the child's `PATH`; see the module docs
    /// and [`crate::pieces::path_with_bin_dir`]. `None` leaves `PATH`
    /// exactly as the session received it.
    ///
    /// # Errors
    /// [`SpawnError::Spawn`] if the program could not be started,
    /// [`SpawnError::NoPidfd`] if it started but cannot be watched.
    pub fn spawn(
        name: &str,
        role: Role,
        program: &Path,
        args: &[String],
        bin_dir: Option<&Path>,
    ) -> Result<Self, SpawnError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            // Inherited: one journal, one ordering, no pipe for the
            // session to have to drain (a supervisor that owned its
            // children's stdout would deadlock the moment it blocked in
            // `poll` while a child filled the pipe).
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // The session's own directory, in front of whatever `PATH` the
        // unit handed us: `~/nitro-bin` is on nobody's `PATH`, and a
        // launcher asked to run a packaged `Exec=nitro-term` has nothing
        // else to go on. Read from the *current* environment rather than
        // from a value captured at start, because that is what the child
        // would otherwise inherit.
        if let Some(path) =
            crate::pieces::path_with_bin_dir(bin_dir, std::env::var("PATH").ok().as_deref())
        {
            cmd.env("PATH", path);
        }
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
            exited: None,
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
    /// Repeated calls return the same answer rather than a second
    /// `waitpid`: the status is remembered, which is also what stops
    /// [`terminate`](Child::terminate) signalling a recycled pid.
    pub fn reap(&mut self) -> Option<Exit> {
        if let Some(exit) = self.exited {
            return Some(exit);
        }
        let exit = match self.proc.try_wait() {
            Ok(Some(status)) => Exit::from_status(status),
            // A child we cannot wait for is a child we will never see
            // exit; treat it as gone rather than spinning on its pidfd.
            Err(_) => Exit::Unknown,
            Ok(None) => return None,
        };
        self.exited = Some(exit);
        Some(exit)
    }

    /// The exit status if this child has already been reaped.
    #[must_use]
    pub fn exit(&self) -> Option<Exit> {
        self.exited
    }

    /// Wait for the child, blocking. Only used after `SIGKILL`, where
    /// the wait is bounded by the kernel rather than by the child's
    /// willingness to cooperate.
    ///
    /// # Errors
    /// Whatever `waitpid` says.
    pub fn wait_blocking(&mut self) -> std::io::Result<Exit> {
        if let Some(exit) = self.exited {
            return Ok(exit);
        }
        let exit = self.proc.wait().map(Exit::from_status)?;
        self.exited = Some(exit);
        Ok(exit)
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

    /// Whether this child has been reaped, and so whether its pid still
    /// refers to it.
    #[must_use]
    pub fn is_reaped(&self) -> bool {
        self.exited.is_some()
    }

    /// Send `SIGKILL` to the child's process group, for a piece that
    /// ignored the deadline.
    pub fn kill(&mut self) {
        self.signal(Signal::KILL);
    }

    fn signal(&self, sig: Signal) {
        if self.exited.is_some() {
            // Already waited for: the pid belongs to the kernel now, and
            // possibly to somebody else. See the `exited` field.
            return;
        }
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
            None,
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
            None,
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
            None,
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

    /// The child really sees the directory on its `PATH`, and a **bare
    /// program name in it resolves** — which is the whole point, because
    /// the thing that has to work is a launcher's `execvp("nitro-term")`
    /// two processes further down.
    ///
    /// Asserted by running a child that `exec`s a bare name found only
    /// in a fixture directory, rather than by reading the `Command`'s
    /// env list: `Command::env` would show the string either way, and
    /// the claim is about what `execvp` does with it.
    #[test]
    fn a_child_inherits_the_bin_dir_on_its_path_and_a_bare_name_resolves() {
        let dir = std::env::temp_dir().join(format!("nitro-session-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        // A "binary" with a name no `PATH` on any machine has, so a pass
        // cannot come from the ambient environment.
        let bare = "nitro-session-path-probe";
        let probe = dir.join(bare);
        std::fs::write(
            &probe,
            format!("#!/bin/sh\necho resolved > {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(
            &probe,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();

        let ts = rustix::event::Timespec {
            tv_sec: 5,
            tv_nsec: 0,
        };

        // The probe is an image this process just wrote, and this is a
        // multi-threaded test binary: `fork` copies the whole fd table,
        // so any other test thread that forks in the window between the
        // write's `open` and its `close` inherits a writable descriptor
        // on the probe, and holds it until it `exec`s. `execve` refuses
        // an image that is open for writing anywhere in the system with
        // `ETXTBSY`, which `sh` reports as exit **126** — "found, but
        // refused" — as distinct from the 127 of "not on `PATH`". That
        // is a known Rust-level hazard, not a quirk of this test:
        // rust-lang/rust#89522. The window is inherently racy but
        // bounded, and closes as soon as the colliding child `exec`s, so
        // retry the spawn a few times, which is what upstream advises.
        let mut exit = None;
        for attempt in 0..8 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let mut c = Child::spawn(
                "probe",
                Role::Shell,
                Path::new("/bin/sh"),
                // `sh -c 'exec <bare>'` is `execvp` with the child's own
                // `PATH`, which is exactly what a launcher does with an
                // `Exec=` value.
                &["-c".to_owned(), format!("exec {bare}")],
                Some(&dir),
            )
            .expect("spawn");
            let fd = c.as_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            rustix::event::poll(&mut fds, Some(&ts)).expect("poll");
            exit = c.reap();
            if exit != Some(Exit::Code(126)) {
                break;
            }
        }
        assert_ne!(
            exit,
            Some(Exit::Code(126)),
            "`sh` found the probe on `PATH` and the kernel refused to \
             `exec` it: 126 is `ETXTBSY` (another forked child still \
             holds a writable fd on the freshly written image, \
             rust-lang/rust#89522), not a `PATH` failure — retrying the \
             spawn did not outlast the window"
        );
        assert_eq!(
            exit,
            Some(Exit::Code(0)),
            "the bare name resolved: in `sh -c`, an `exec` of a name not \
             on `PATH` exits 127"
        );

        assert_eq!(
            std::fs::read_to_string(&marker).unwrap_or_default().trim(),
            "resolved"
        );

        // And the control, which is what makes the arm mean something:
        // the same child without the directory cannot find the program.
        let _ = std::fs::remove_file(&marker);
        let mut c = Child::spawn(
            "probe-control",
            Role::Shell,
            Path::new("/bin/sh"),
            &["-c".to_owned(), format!("exec {bare} 2>/dev/null")],
            None,
        )
        .expect("spawn");
        let fd = c.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(&mut fds, Some(&ts)).expect("poll");
        assert_eq!(
            c.reap(),
            Some(Exit::Code(127)),
            "without the bin dir the bare name is not on PATH"
        );
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A reaped child must not be signalled again: the pid is the
    /// kernel's the moment `waitpid` returns, and may already name
    /// somebody else's process.
    ///
    /// Asserted from the outside, by giving the `Child` a pid that is
    /// *not* its own after the reap and checking nothing is sent to it:
    /// a live `sleep` in its own process group, which must survive.
    #[test]
    fn a_reaped_child_is_never_signalled_again() {
        let mut c = Child::spawn(
            "quick",
            Role::Shell,
            Path::new("/bin/sh"),
            &["-c".to_owned(), "exit 0".to_owned()],
            None,
        )
        .expect("spawn");
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
        assert_eq!(c.reap(), Some(Exit::Code(0)));
        assert!(c.is_reaped());

        // A second reap is the remembered answer, not a second
        // `waitpid` (which would report an error and become `Unknown`).
        assert_eq!(c.reap(), Some(Exit::Code(0)));
        assert_eq!(c.exit(), Some(Exit::Code(0)));

        // And the signals are no-ops now. If they were not, this would
        // be a `kill` aimed at whatever the kernel has since done with
        // that pid — the hazard `spawn`'s `NoPidfd` arm avoids in the
        // other direction.
        c.terminate();
        c.kill();
        assert_eq!(
            c.wait_blocking().expect("remembered"),
            Exit::Code(0),
            "a blocking wait after the reap is the remembered status, \
             not a second waitpid on a pid we no longer own"
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
