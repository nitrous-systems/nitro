//! A pseudoterminal, and the child shell on the far end of it.
//!
//! The sibling argument to `nitro-launcher`'s `spawn` module: that had to
//! start a program and *detach* it, this one has to start a program and
//! *attach* it — to a terminal, as its session leader — and both have to
//! do it in a tree where `unsafe_code = "deny"`.
//!
//! # Attaching a child to a terminal without `unsafe`
//!
//! The textbook shell spawn is `fork`, then in the child `setsid()`,
//! `ioctl(TIOCSCTTY)`, `dup2` the slave onto 0/1/2, `execvp`. The hook
//! that would run those three calls between fork and exec —
//! `CommandExt::pre_exec` — is `unsafe`, and for a real reason: after a
//! fork only async-signal-safe functions may be called and the compiler
//! cannot check that a closure obeys. `nitro-launcher` got out of this
//! by needing only the safe subset (`process_group(0)`) because it has
//! no controlling terminal to pass on. A terminal emulator's whole job
//! *is* the controlling terminal, so that door is shut.
//!
//! The way through is that someone has already written the three calls,
//! in C, with the `unsafe` audited once: util-linux's `setsid(1)`. Run
//! `setsid --ctty <program> <args…>` with the pty slave as stdin and it
//! calls `setsid(2)`, then `TIOCSCTTY` on stdin, then execs — exactly the
//! child half of the textbook spawn, in a process we start with a plain
//! [`std::process::Command`]. The cost is a dependency on a binary
//! rather than on a crate, which is why [`Pty::has_job_control`] exists:
//! the fallback is real and the user is told about it instead of being
//! handed a terminal where `Ctrl-C` silently does nothing.
//!
//! # The fallback, and how it is detected
//!
//! Without `setsid` the child is spawned plainly, still with the slave
//! as its three standard descriptors — so it reads and writes the
//! terminal and `stty size` is right — but with no session and no
//! controlling terminal. What is then missing is *job control*: the
//! kernel sends `SIGINT`/`SIGWINCH` to the foreground process group of a
//! terminal's session, and this pty has no session, so `Ctrl-C` reaches
//! nobody and a resize raises no `SIGWINCH`. `process_group(0)` is set
//! in that path for the launcher's reason, so a signal aimed at *our*
//! group does not fall through to the child.
//!
//! The presence of `setsid` is probed rather than assumed, and the probe
//! runs the real thing: the obvious `setsid --ctty true` fails with
//! `ENOTTY` whenever our own stdin is not a terminal (a service, a test
//! harness, a pipeline), which would report "no job control" on a
//! machine that has it. So [`setsid_with_ctty`] opens a throwaway pty,
//! runs `setsid --ctty /bin/true` against it, and believes the exit
//! status. It costs one `fork`+`exec` per process and is cached.

use std::ffi::OsString;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal};
use rustix::termios::Winsize;

/// How long [`Pty::write`] will keep retrying a master that is full
/// before giving up and returning `WouldBlock`.
///
/// A pty's buffer only stays full while the child is not reading, and a
/// child that has stopped reading for a whole second is wedged; better
/// to hand the caller an error it can report than to stall the app loop
/// on a program that may never come back.
const WRITE_DEADLINE: Duration = Duration::from_secs(1);

/// How long [`Drop`] will spend reaping the child it just killed.
///
/// After `SIGKILL` the exit is immediate and the first `try_wait`
/// almost always succeeds; the bound exists so that a child stopped in
/// an uninterruptible state cannot stall a window closing. Giving up
/// leaves a zombie until the process exits, which is a task-table entry
/// and nothing else — the same trade [`nitro_launcher`] documents.
const REAP_DEADLINE: Duration = Duration::from_millis(50);

/// A pseudoterminal with a child on the far end.
///
/// Owns the master descriptor and the child; dropping it kills the
/// child's process group, so a closed terminal window does not leave a
/// shell running against a pty nobody reads.
#[derive(Debug)]
pub struct Pty {
    /// The master side. Non-blocking, so the app loop can read it from
    /// an `epoll` callback without ever stalling.
    master: OwnedFd,
    /// The child, kept for its non-blocking `try_wait`: reaping through
    /// `Child` rather than a bare `waitpid` means std and this module
    /// cannot both claim the same exit status.
    child: Child,
    /// The child's pid, cached because `Child::id` is unavailable once
    /// the child has been reaped by `try_wait`.
    pid: u32,
    /// Whether the `setsid --ctty` path was taken; see the module docs.
    job_control: bool,
}

impl Pty {
    /// Open a pty, spawn `$SHELL` (else `/bin/sh`) on the slave side
    /// with its own session and the pty as controlling terminal, and
    /// return the master.
    ///
    /// `$SHELL` is the user's answer to "which shell", set by login;
    /// falling back to `/bin/sh` rather than to a guess at `bash` keeps
    /// the failure mode "a plainer shell" instead of "no terminal".
    ///
    /// # Errors
    /// Any of `openpt`/`grantpt`/`unlockpt`/`ptsname`, opening the
    /// slave, setting the window size, or spawning the child.
    pub fn spawn(cols: u16, rows: u16) -> std::io::Result<Self> {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"));
        let shell = shell.to_string_lossy().into_owned();
        Self::spawn_command(&[&shell], cols, rows)
    }

    /// As [`Pty::spawn`], but run `argv` instead of the login shell.
    ///
    /// The tests use it to run `sh -c '…'`, which is also how a terminal
    /// would honour a `-e`/`--command` flag.
    ///
    /// # Errors
    /// As [`Pty::spawn`], plus [`std::io::ErrorKind::InvalidInput`] for
    /// an empty `argv`.
    pub fn spawn_command(argv: &[&str], cols: u16, rows: u16) -> std::io::Result<Self> {
        let (program, rest) = argv.split_first().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "nothing to run in the pty",
            )
        })?;

        let (master, slave) = open_pair()?;
        // The size is set on the master *before* the child starts, so
        // the child's very first `stty size` — or `tput lines` in a
        // shell rc file — sees the real geometry rather than the 0x0
        // a fresh pty starts at.
        rustix::termios::tcsetwinsize(&master, winsize(cols, rows))?;

        // The `setsid` path first, the plain one if it is unavailable or
        // if it fails to start (a `setsid` that exists but cannot be
        // executed is a machine we should still give a terminal on).
        let mut job_control = false;
        let mut child = None;
        if let Some(setsid) = setsid_with_ctty() {
            let mut cmd = Command::new(setsid);
            cmd.arg("--ctty").arg(program).args(rest);
            if let Ok(c) = spawn_on(cmd, &slave, false) {
                job_control = true;
                child = Some(c);
            }
        }
        let child = if let Some(c) = child {
            c
        } else {
            let mut cmd = Command::new(program);
            cmd.args(rest);
            spawn_on(cmd, &slave, true)?
        };
        // The parent's copy of the slave must go, and go here: as long
        // as any descriptor keeps the slave open the master never reads
        // EOF, so a child that exited would look like a child that is
        // simply quiet, forever.
        drop(slave);

        // Non-blocking last, so the spawn above could not have been
        // tripped by it. The app loop polls this fd; a blocking read
        // there would freeze the compositor's client.
        let flags = rustix::fs::fcntl_getfl(&master)?;
        rustix::fs::fcntl_setfl(&master, flags | OFlags::NONBLOCK)?;

        let pid = child.id();
        Ok(Self {
            master,
            child,
            pid,
            job_control,
        })
    }

    /// The master descriptor, for `epoll`/`add_fd`.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    /// Read whatever is available; `Ok(0)` means the child closed it.
    ///
    /// `WouldBlock` is the caller's to expect — the master is
    /// non-blocking, and "nothing right now" is the normal answer for a
    /// terminal nobody is typing at.
    ///
    /// Linux reports the far end being gone as `EIO` rather than as end
    /// of file; that is translated to `Ok(0)`, because the distinction
    /// is a pty implementation detail and every caller wants "the child
    /// is done".
    ///
    /// # Errors
    /// `WouldBlock` when no bytes are ready, and any other read error.
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match rustix::io::read(&self.master, buf) {
            Ok(n) => Ok(n),
            Err(rustix::io::Errno::IO) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    /// Write to the child's stdin. Partial writes are retried
    /// internally.
    ///
    /// A pty's buffer is a few kilobytes, so a paste is routinely bigger
    /// than one write; the loop is the difference between pasting a file
    /// and pasting its first 4 KiB. A full buffer is waited on in 1 ms
    /// steps up to [`WRITE_DEADLINE`].
    ///
    /// # Errors
    /// `WouldBlock` if the child stopped reading for a whole
    /// [`WRITE_DEADLINE`], or any other write error.
    /// Write `bytes` to `fd`, retrying partial writes.
    ///
    /// Free-standing because two things write to the master: this type,
    /// and the widget, which holds a `dup` of it so that a key or a
    /// scripted `send` reaches the child *immediately* rather than
    /// queueing for someone to notice. Sharing the loop is what keeps
    /// the retry rules — and the deadline — in one place.
    ///
    /// # Errors
    /// `WouldBlock` if the far end stopped reading for a whole
    /// [`WRITE_DEADLINE`], or any other write error.
    pub fn write_all(fd: BorrowedFd<'_>, bytes: &[u8]) -> std::io::Result<()> {
        let deadline = Instant::now() + WRITE_DEADLINE;
        let mut rest = bytes;
        while !rest.is_empty() {
            match rustix::io::write(fd, rest) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "the pty accepted no bytes",
                    ));
                }
                Ok(n) => rest = &rest[n..],
                Err(rustix::io::Errno::INTR) => {}
                Err(rustix::io::Errno::AGAIN) => {
                    if Instant::now() >= deadline {
                        return Err(rustix::io::Errno::AGAIN.into());
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Write to the child's stdin, retrying partial writes.
    ///
    /// The instance form of [`Pty::write_all`], for a caller that has
    /// the `Pty` rather than a duplicate of its descriptor.
    ///
    /// # Errors
    /// As [`Pty::write_all`].
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        Self::write_all(self.master.as_fd(), bytes)
    }

    /// A `dup` of the master, for a second writer.
    ///
    /// The widget takes one so that a keystroke or a scripted `send`
    /// reaches the child in the same turn it happened, without the app
    /// having to remember to drain a queue after every possible entry
    /// point — which is exactly the bug the box run found: `hey set grid
    /// value` filled a queue that only the descriptor hook emptied, so a
    /// scripted command sat there until the child happened to say
    /// something.
    ///
    /// # Errors
    /// If the descriptor cannot be duplicated.
    pub fn dup_master(&self) -> std::io::Result<OwnedFd> {
        Ok(rustix::io::dup(&self.master)?)
    }

    /// `TIOCSWINSZ`, so the child gets `SIGWINCH` and `$COLUMNS` is
    /// right.
    ///
    /// The kernel's winsize is the single truth about the geometry —
    /// which is why the environment this module builds has no `LINES`
    /// or `COLUMNS` in it to disagree with. The `SIGWINCH` only reaches
    /// the child if it has a session on this terminal; see
    /// [`Pty::has_job_control`].
    ///
    /// # Errors
    /// If the ioctl fails, which for a live master it does not.
    pub fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        rustix::termios::tcsetwinsize(&self.master, winsize(cols, rows))?;
        Ok(())
    }

    /// The child's pid, for the tests and for teardown.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the child has exited (non-blocking `waitpid`).
    ///
    /// The app loop asks this after a read returns `Ok(0)`, to tell a
    /// shell that exited from a shell that closed its own stdout.
    pub fn child_exited(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => false,
            // `ECHILD`: someone already reaped it, so it is certainly
            // gone. Any other error is not a reason to claim it lives.
            Ok(Some(_)) | Err(_) => true,
        }
    }

    /// Whether the child got a controlling terminal, i.e. whether
    /// `setsid --ctty` was available. Job control does not work without
    /// it.
    ///
    /// The caller logs a warning when this is false: the terminal still
    /// works for typing and for output, but `Ctrl-C`, `Ctrl-Z`, `fg`
    /// and the `SIGWINCH` on resize all silently do nothing, and a user
    /// deserves to be told which of those they are missing rather than
    /// discovering it during a runaway build.
    pub fn has_job_control(&self) -> bool {
        self.job_control
    }
}

impl Drop for Pty {
    /// Kill the child's process group and reap what can be reaped
    /// without blocking.
    ///
    /// The *group*, not the pid: the child is a session and group
    /// leader, and what the user closed the window on is usually the
    /// shell's foreground job rather than the shell. `SIGHUP` first,
    /// because that is what a hangup on a terminal means and a shell
    /// treats it as "save and exit"; `SIGKILL` immediately after,
    /// because a terminal being destroyed is not a negotiation.
    fn drop(&mut self) {
        if let Some(pid) = Pid::from_raw(self.pid.cast_signed()) {
            let _ = rustix::process::kill_process_group(pid, Signal::HUP);
            let _ = rustix::process::kill_process_group(pid, Signal::KILL);
        }
        let deadline = Instant::now() + REAP_DEADLINE;
        loop {
            match self.child.try_wait() {
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                _ => break,
            }
        }
    }
}

/// A `winsize` with the pixel fields left at zero.
///
/// Nothing on Linux derives anything from `ws_xpixel`/`ws_ypixel` — a
/// terminal that reports them is reporting its font's metrics, which a
/// child has no business acting on — and `sixel`-style users of the
/// fields ask via an escape sequence instead.
fn winsize(cols: u16, rows: u16) -> Winsize {
    Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Open a master/slave pair, unlocked and ready to be handed out.
///
/// `NOCTTY` on both: the *parent* must never acquire this pty as its
/// controlling terminal, which is precisely the thing the child is
/// supposed to do. `CLOEXEC` on the master so it does not leak into the
/// child — the child has the slave, and a child holding the master open
/// would keep the terminal alive after we let go of it.
fn open_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let master = rustix::pty::openpt(
        rustix::pty::OpenptFlags::RDWR
            | rustix::pty::OpenptFlags::NOCTTY
            | rustix::pty::OpenptFlags::CLOEXEC,
    )?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        Path::new(std::str::from_utf8(name.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "pty name is not UTF-8")
        })?),
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )?;
    Ok((master, slave))
}

/// Spawn `cmd` with `slave` as all three standard descriptors and the
/// environment a terminal owes its child.
///
/// `own_group` is the fallback path's [`nitro_launcher::spawn`] trick:
/// with no session to separate us, a process group of our own is what
/// keeps a `Ctrl-C` typed at *our* terminal from reaching the child (or
/// a signal aimed at the child's group from reaching us).
fn spawn_on(mut cmd: Command, slave: &OwnedFd, own_group: bool) -> std::io::Result<Child> {
    // Three separate duplicates rather than one fd three times: each
    // `Stdio` owns what it is given, and the child's `exec` wants 0, 1
    // and 2 to be independent descriptors on the same pty, as a login
    // would have left them.
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // What we promise to be. `xterm-256color` because that is the
    // terminfo entry every distribution ships and the one our escape
    // handling is written against; `COLORTERM=truecolor` because the
    // terminfo format has no way to say "24-bit" and this de-facto
    // variable is how every emulator says it.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // Inherited `LINES`/`COLUMNS` would outrank the kernel's winsize in
    // every program that reads them, and ours came from a window that
    // no longer exists. The kernel's winsize is the truth; see
    // [`Pty::resize`].
    cmd.env_remove("LINES");
    cmd.env_remove("COLUMNS");
    if own_group {
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// The `setsid` binary, if this machine has one that supports `--ctty`.
///
/// Cached: the probe runs a real `fork`+`exec` and the answer cannot
/// change under a running process.
fn setsid_with_ctty() -> Option<&'static Path> {
    static PROBE: OnceLock<Option<PathBuf>> = OnceLock::new();
    PROBE
        .get_or_init(|| {
            let path = setsid_on_path()?;
            probe_ctty(&path).then_some(path)
        })
        .as_deref()
}

/// Find `setsid` on `$PATH`, then in the two places it lives when
/// `$PATH` is the empty one a service inherits.
///
/// Executability is checked with `access(X_OK)` rather than by reading
/// the mode bits, because the answer depends on who we are.
fn setsid_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join("setsid"))
        .chain([
            PathBuf::from("/usr/bin/setsid"),
            PathBuf::from("/bin/setsid"),
        ])
        .find(|p| rustix::fs::access(p, rustix::fs::Access::EXEC_OK).is_ok())
}

/// Run `setsid --ctty /bin/true` against a throwaway pty and report
/// whether it succeeded.
///
/// The throwaway pty is the whole point: `--ctty` calls `TIOCSCTTY` on
/// *stdin*, so probing with our own inherited stdin answers a question
/// about the harness that started us rather than about `setsid`. A
/// `setsid` too old for `--ctty` fails the argument parse and exits
/// non-zero, which is the other thing this catches.
///
/// Bounded at 500 ms and then killed: a probe that hangs is a probe
/// that has already told us not to rely on the thing.
fn probe_ctty(setsid: &Path) -> bool {
    let Ok((_master, slave)) = open_pair() else {
        return false;
    };
    let mut cmd = Command::new(setsid);
    cmd.arg("--ctty").arg("/bin/true");
    let Ok(mut child) = spawn_on(cmd, &slave, false) else {
        return false;
    };
    drop(slave);
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough that a loaded CI box still wins, short enough that a
    /// broken pty fails the run rather than hanging it.
    const PATIENCE: Duration = Duration::from_secs(3);

    /// Read until `needle` shows up, or give up at the deadline. Every
    /// test in this module goes through it, because a test that loops
    /// on a pty without a deadline is a test that hangs CI.
    fn read_until(pty: &mut Pty, needle: &str) -> String {
        let deadline = Instant::now() + PATIENCE;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let mut buf = [0u8; 4096];
            match pty.read(&mut buf) {
                Ok(0) => {
                    if String::from_utf8_lossy(&seen).contains(needle) {
                        break;
                    }
                    // The child is gone and the needle never came; give
                    // the kernel a moment for buffered output, then stop.
                    break;
                }
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&seen).contains(needle) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("reading the pty: {e}"),
            }
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    fn a_shell_echoes_what_it_is_told() {
        let mut pty = Pty::spawn_command(&["/bin/sh", "-c", "echo hello"], 80, 24).expect("spawn");
        let out = read_until(&mut pty, "hello");
        assert!(out.contains("hello"), "output was {out:?}");
    }

    #[test]
    fn the_child_sees_the_size_we_set() {
        // `stty size` reads the kernel's winsize through stdin, which is
        // the slave: the same number the child's `ncurses` would get.
        let mut pty = Pty::spawn_command(&["/bin/sh", "-c", "stty size"], 100, 37).expect("spawn");
        let out = read_until(&mut pty, "37 100");
        assert!(out.contains("37 100"), "stty said {out:?}");
    }

    #[test]
    fn the_child_is_told_it_is_an_xterm() {
        let mut pty = Pty::spawn_command(
            &[
                "/bin/sh",
                "-c",
                "echo \"[$TERM|$COLORTERM|${LINES-unset}]\"",
            ],
            80,
            24,
        )
        .expect("spawn");
        let out = read_until(&mut pty, "]");
        assert!(
            out.contains("[xterm-256color|truecolor|unset]"),
            "the child's environment was {out:?}"
        );
    }

    #[test]
    fn a_resize_reaches_the_child() {
        // The child waits for a line before asking, so the resize is
        // ordered before the question rather than racing it: what is
        // under test is that `TIOCSWINSZ` reached the kernel, and a
        // `SIGWINCH` race would be testing the scheduler.
        let mut pty =
            Pty::spawn_command(&["/bin/sh", "-c", "read line; stty size"], 80, 24).expect("spawn");
        pty.resize(132, 43).expect("resize");
        pty.write(b"\n").expect("write");
        let out = read_until(&mut pty, "43 132");
        assert!(out.contains("43 132"), "stty said {out:?} after a resize");
    }

    #[test]
    fn what_we_write_reaches_the_child() {
        let mut pty = Pty::spawn_command(&["/bin/sh", "-c", "read l; echo \"got:$l\""], 80, 24)
            .expect("spawn");
        pty.write(b"ping\n").expect("write");
        let out = read_until(&mut pty, "got:ping");
        assert!(out.contains("got:ping"), "output was {out:?}");
    }

    #[test]
    fn the_master_reads_would_block_when_nothing_is_there() {
        let mut pty = Pty::spawn_command(&["/bin/sh", "-c", "sleep 5"], 80, 24).expect("spawn");
        let mut buf = [0u8; 32];
        let e = pty.read(&mut buf).expect_err("a silent child has no bytes");
        assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{e}");
    }

    #[test]
    fn an_exited_child_is_noticed() {
        let mut pty = Pty::spawn_command(&["/bin/sh", "-c", "exit 3"], 80, 24).expect("spawn");
        let deadline = Instant::now() + PATIENCE;
        while !pty.child_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(pty.child_exited(), "the child ran `exit 3` and was reaped");
    }

    #[test]
    fn dropping_the_pty_kills_the_child() {
        let pty = Pty::spawn_command(&["/bin/sh", "-c", "sleep 30"], 80, 24).expect("spawn");
        let pid = pty.pid();
        assert!(pid > 1);
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "the child is running before the drop"
        );
        drop(pty);
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline && !is_gone(pid) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(is_gone(pid), "/proc/{pid} outlived the pty");
    }

    /// Whether a pid names no live process. A pid that has exited but
    /// not been reaped still has a `/proc` entry, so the zombie state is
    /// checked too — `Drop` bounds its reaping and is allowed to leave
    /// one behind.
    fn is_gone(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return true;
        };
        // `comm` may contain spaces and parentheses; the state is the
        // first field after the last `)`.
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state == "Z")
    }

    #[test]
    fn job_control_is_reported_honestly() {
        let pty = Pty::spawn_command(&["/bin/sh", "-c", "sleep 1"], 80, 24).expect("spawn");
        let on_path = setsid_on_path().is_some();
        assert_eq!(
            pty.has_job_control(),
            setsid_with_ctty().is_some(),
            "the flag says exactly what the probe found"
        );
        if !on_path {
            assert!(
                !pty.has_job_control(),
                "no `setsid` on this machine, so no controlling terminal"
            );
        }
    }

    #[test]
    fn the_child_gets_a_controlling_terminal_when_it_can() {
        // `/dev/tty` opens only for a process that *has* a controlling
        // terminal, which is the property `--ctty` is bought for — and
        // the one thing a flag set by a probe could otherwise lie about.
        let mut pty = Pty::spawn_command(
            &[
                "/bin/sh",
                "-c",
                "if : >/dev/tty; then echo CTTY; else echo NONE; fi",
            ],
            80,
            24,
        )
        .expect("spawn");
        let expected = if pty.has_job_control() {
            "CTTY"
        } else {
            "NONE"
        };
        let out = read_until(&mut pty, expected);
        assert!(
            out.contains(expected),
            "job control is {}, but the child said {out:?}",
            pty.has_job_control()
        );
    }

    #[test]
    fn spawning_nothing_is_an_error_not_a_panic() {
        let e = Pty::spawn_command(&[], 80, 24).expect_err("an empty argv");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
    }
}
