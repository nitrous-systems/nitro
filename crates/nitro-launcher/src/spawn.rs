//! Starting a program, and detaching it from the launcher.
//!
//! One function's worth of code, and all of its difficulty is in what it
//! has to break: a launched application must survive the launcher, must
//! not inherit the launcher's signals or terminal, and must not become a
//! zombie the launcher has to reap.
//!
//! # Detaching without `unsafe`
//!
//! The spec asks for `fork` + `setsid` + `execvp`, which is the textbook
//! answer and needs a raw `fork`. This tree denies `unsafe`, and the hook
//! that would run `setsid` between fork and exec —
//! `CommandExt::pre_exec` — is `unsafe` for a real reason: the child of a
//! fork may only call async-signal-safe functions, and the compiler
//! cannot check that the closure does.
//!
//! `Command::process_group(0)` is the **safe subset that covers what a
//! launcher actually needs**: the child gets a process group of its own,
//! so a signal sent to the launcher's group (a Ctrl-C in the terminal it
//! was started from, a `kill -- -PID` of the session) does not reach the
//! application. What it does not do is detach a *controlling terminal*,
//! and the launcher has none to pass on — it is started by the session,
//! its own stdio is the compositor's journal, and the child's is
//! `/dev/null`. So the difference between this and `setsid` is a thing
//! neither process has. Recorded in the README under *Limitations*.
//!
//! # Reaping
//!
//! The child is not orphaned while the launcher lives, so the launcher
//! reaps it — and it does so **when the child exits**, not when the user
//! next launches something. [`Children::spawn`] opens a pidfd
//! ([`rustix::process::pidfd_open`]) for every child, and
//! [`Children::watch`] registers that descriptor with the app loop
//! through [`Ui::add_fd`]: a pidfd becomes readable when the process it
//! names exits, so "the program the user started has finished" arrives
//! the same way a key does, as a wakeup on a descriptor the loop already
//! had. The launcher is an ordinary toolkit app sitting in `epoll_wait`,
//! and that is the only kind of event it can be told about.
//!
//! The alternative — `SIGCHLD`, a self-pipe and a `waitpid(-1)` loop —
//! would cost a signal-handling crate and would answer a question we did
//! not ask ("*some* child changed state") where the pidfd answers the one
//! we did ("*this* child exited"). `nitro-session`'s `child.rs` argues
//! the same choice at length for a supervisor; the launcher is the
//! smaller case of it.
//!
//! [`Children::reap`] — a non-blocking `try_wait` over the outstanding
//! children before every spawn — stays as the fallback for a child whose
//! pidfd could not be opened at all, and the launcher's own exit hands
//! the rest to init. The cost of getting this wrong is a zombie — a
//! task-table entry and nothing else — and the cost of *not* tracking the
//! children at all would be one per launch for the whole session. The
//! double-fork that avoids both needs the `fork` above.
//!
//! # Environment and stdio
//!
//! The child inherits the launcher's environment, which is the point:
//! `XDG_RUNTIME_DIR` is how it finds the compositor's socket and
//! `NITRO_SOCKET` is how it finds a non-default one. Passing them
//! explicitly rather than inheriting would mean a launcher that had to
//! know every variable an application might want.
//!
//! **`NITRO_SHELL_SOCKET` is removed**, and that is the one deliberate
//! subtraction. It names the *privileged* socket, and an application
//! started from the launcher is an ordinary application: leaving it in
//! the environment would hand every program the user launches the path
//! to the socket that grants hotkeys and keyboard grabs. The directory is
//! `0700` and a determined program can still construct the default path,
//! so this is defence in depth rather than a boundary — `docs/shell.md`
//! §"What this model is worth" is honest about the limit — but a
//! launcher should not be the thing that spreads it.
//!
//! All three stdio streams go to `/dev/null`. An application's stray
//! `println!` would otherwise land in the launcher's own stdout, which on
//! the test box is the compositor unit's journal.

use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use nitro_ui::{FdToken, Ui};

/// Everything a spawn can go wrong at, as one value.
#[derive(Debug)]
pub enum Error {
    /// The entry had no command to run.
    Empty,
    /// `Terminal=true` and there is no terminal emulator yet.
    NeedsTerminal,
    /// `posix_spawn`/`fork` or the `exec` failed.
    Spawn(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "nothing to run"),
            Self::NeedsTerminal => {
                write!(f, "needs a terminal, and nitro-term is not written yet")
            }
            Self::Spawn(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// One launched process, as this module has to hold it.
#[derive(Debug)]
struct Live {
    /// The process itself, for `try_wait`.
    proc: Child,
    /// Readable once the process exits, and `None` if `pidfd_open`
    /// failed — which puts this child back on the reap-before-spawn
    /// path rather than losing it.
    pidfd: Option<OwnedFd>,
    /// Names the loop hook watching `pidfd`, once [`Children::watch`]
    /// has registered one. It is what the hook is removed by, so it is
    /// kept next to the child it belongs to.
    token: Option<FdToken>,
}

/// Reaps the children a launcher has started.
///
/// Held by the launcher's state, which is also how the loop reaches it:
/// each child's pidfd is registered with [`Children::watch`], and the
/// callback finds this set again through the accessor it was given. See
/// the module docs for what this does and does not promise.
#[derive(Debug, Default)]
pub struct Children {
    live: Vec<Live>,
}

impl Children {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many launched processes have not been reaped yet. For the
    /// tests, and for `hey nitro-launcher get window value`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Whether nothing is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Reap whatever has exited and is not being watched by the loop.
    /// Called before every spawn, so the list is bounded by the number of
    /// launches that are still running rather than by the number ever
    /// made.
    ///
    /// A **watched** child is deliberately left alone here: reaping it
    /// would drop the entry, and with it the only record of the
    /// [`FdToken`] naming its loop hook — leaving a hook registered on a
    /// descriptor that is readable for ever, which is the spin
    /// [`Children::watch`] explains. Its exit belongs to the loop, which
    /// notices it long before the next launch. What is left for this is
    /// the child whose `pidfd_open` failed, which is the case it now
    /// exists for.
    pub fn reap(&mut self) {
        self.live
            .retain_mut(|c| c.token.is_some() || !matches!(c.proc.try_wait(), Ok(Some(_))));
    }

    /// Reap every child that has exited, and report the loop hooks of the
    /// ones that were being watched so the caller can unregister them.
    ///
    /// Private because the unregistering is not optional — see
    /// [`Children::watch`] — and the only caller that can do it is the
    /// callback that hook installs.
    fn reap_exited(&mut self) -> Vec<FdToken> {
        let mut done = Vec::new();
        self.live.retain_mut(|c| {
            if matches!(c.proc.try_wait(), Ok(Some(_))) {
                if let Some(token) = c.token {
                    done.push(token);
                }
                false
            } else {
                true
            }
        });
        done
    }

    /// Register every not-yet-watched child's pidfd with the loop, so its
    /// exit is reaped on the spot rather than at the next spawn.
    ///
    /// `get` is how the callback finds this set again inside the app's
    /// state (`|s: &mut Launcher| &mut s.children`). A closure that had
    /// captured `&mut self` could not be it: the state owns the set, and
    /// the loop hands the callback that state.
    ///
    /// The callback reaps whatever exited and then **removes the
    /// [`FdToken`] of every child it reaped**, with [`Ui::remove_fd`].
    /// That second half is load-bearing rather than tidy: the app loop's
    /// `epoll` is **level-triggered**, and an exited process's pidfd
    /// stays readable for as long as the descriptor exists. A hook left
    /// registered would therefore be dispatched on every turn of the
    /// loop, for ever — a launcher sitting at 100 % CPU with nothing on
    /// screen, which is exactly what the toolkit's "idle costs nothing"
    /// contract forbids.
    ///
    /// A child whose `pidfd_open` failed has no descriptor to watch and
    /// is skipped here; [`Children::reap`] still collects it before the
    /// next spawn, which is what this module did before the pidfd
    /// existed. So is a registration that fails: the child keeps no
    /// token, so a later `watch` tries again and `reap` still knows about
    /// it.
    pub fn watch<S: 'static>(&mut self, ui: &mut Ui<S>, get: fn(&mut S) -> &mut Children) {
        for child in &mut self.live {
            if child.token.is_some() {
                continue;
            }
            let Some(pidfd) = child.pidfd.as_ref() else {
                continue;
            };
            let hook = move |s: &mut S, ui: &mut Ui<S>| {
                for token in get(s).reap_exited() {
                    ui.remove_fd(token);
                }
            };
            if let Ok(token) = ui.add_fd(pidfd.as_fd(), hook) {
                child.token = Some(token);
            }
        }
    }

    /// Every watched child, as its pidfd and the [`FdToken`] naming the
    /// hook registered on it.
    ///
    /// For the tests, which have no real `epoll` under them: a test polls
    /// the descriptor itself and then calls [`Ui::run_fd`] with the
    /// token, which is exactly the pair of steps the app loop takes on a
    /// wakeup.
    #[must_use]
    pub fn watched(&self) -> Vec<(std::os::fd::BorrowedFd<'_>, FdToken)> {
        self.live
            .iter()
            .filter_map(|c| Some((c.pidfd.as_ref()?.as_fd(), c.token?)))
            .collect()
    }

    /// Start `argv` detached, and remember the child so it can be reaped.
    ///
    /// # Errors
    /// [`Error::Empty`] for an empty argv, or [`Error::Spawn`] if the
    /// process could not be started at all — which for `execvp` means the
    /// program was not found on `PATH`.
    pub fn spawn(&mut self, argv: &[String]) -> Result<u32, Error> {
        self.reap();
        let (program, rest) = argv.split_first().ok_or(Error::Empty)?;
        let child = command(program, rest).spawn().map_err(Error::Spawn)?;
        let pid = child.id();
        // The descriptor that will say "this one exited". It is opened
        // here rather than in [`Children::watch`] so it is taken while
        // the child is certainly still ours — before anything can have
        // reaped it and let the kernel hand the pid to somebody else.
        //
        // A failure is not fatal and must not lose the child: it is
        // remembered without a pidfd, and [`Children::reap`] collects it
        // before the next spawn, exactly as every child was collected
        // before this existed.
        let pidfd = rustix::process::pidfd_open(
            rustix::process::Pid::from_child(&child),
            rustix::process::PidfdFlags::NONBLOCK,
        )
        .ok();
        self.live.push(Live {
            proc: child,
            pidfd,
            token: None,
        });
        Ok(pid)
    }
}

/// The `Command` a launch runs, built but not spawned.
///
/// Separate from [`Children::spawn`] so a test can assert on what would
/// be run — the environment subtraction and the stdio redirection are
/// promises this module makes, and a test that had to actually start a
/// process to check them would be asserting on something else.
#[must_use]
pub fn command(program: &str, args: &[String]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // The privileged socket does not propagate to launched applications.
    cmd.env_remove("NITRO_SHELL_SOCKET");
    // A process group of its own, so a signal aimed at the launcher's
    // group does not reach the application. This is the safe half of
    // `setsid`; see the module docs for the half it is not and why that
    // half is a thing neither process has.
    cmd.process_group(0);
    cmd
}

/// Where a nitro binary that sits next to the launcher would be.
///
/// The launcher lists its siblings so a box with **no desktop files at
/// all** still has something to launch — which is the state a freshly
/// rsynced `~/nitro-bin` is in, and so the state the test box is in. The
/// directory is the one the running executable is in, found with
/// `current_exe`, because a launcher deployed to `~/nitro-bin` and one
/// run from `target/debug` should each find their own siblings rather
/// than a stale copy of the other's.
#[must_use]
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(std::path::Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_drops_the_shell_socket_from_the_environment() {
        // The one deliberate subtraction: an application launched from
        // the launcher must not inherit the path to the *privileged*
        // socket.
        //
        // Asserted on the `Command`'s own env list rather than by
        // running a child and reading its environment, because setting
        // the variable to observe it would mean `set_var` in a test
        // binary that runs its tests on threads — which is exactly the
        // race the removal is about.
        let cmd = command("/bin/true", &[]);
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            removed,
            vec!["NITRO_SHELL_SOCKET".to_owned()],
            "the shell socket is removed, and nothing else is"
        );
        // And the rest of the environment is inherited rather than
        // rebuilt: `XDG_RUNTIME_DIR` is how the child finds the server.
        assert!(
            !cmd.get_envs()
                .any(|(k, v)| k == "XDG_RUNTIME_DIR" && v.is_none()),
            "the runtime directory is not touched"
        );
    }

    #[test]
    fn spawning_an_empty_argv_is_an_error_not_a_panic() {
        let mut c = Children::new();
        assert!(matches!(c.spawn(&[]), Err(Error::Empty)));
    }

    #[test]
    fn a_missing_program_is_reported_rather_than_ignored() {
        let mut c = Children::new();
        let e = c
            .spawn(&["nitro-no-such-binary-ever".to_owned()])
            .expect_err("a program that is not on PATH");
        assert!(matches!(e, Error::Spawn(_)), "{e}");
        assert!(c.is_empty(), "nothing was recorded for a failed spawn");
    }

    #[test]
    fn a_launched_process_really_runs_and_is_reaped() {
        // The end-to-end shape of a launch, with a marker file standing
        // in for "the application started": a spawn that silently did
        // nothing would pass every other test in this module.
        let dir = std::env::temp_dir().join(format!("nitro-launcher-spawn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let mut c = Children::new();
        let pid = c
            .spawn(&[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("echo launched > {}", marker.display()),
            ])
            .expect("spawn");
        assert!(pid > 0);
        assert_eq!(c.len(), 1, "the child is remembered so it can be reaped");

        for _ in 0..200 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap_or_default().trim(),
            "launched"
        );

        // And the next launch reaps it, which is what keeps the list
        // bounded by what is running rather than by what ever ran.
        for _ in 0..200 {
            c.reap();
            if c.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(c.is_empty(), "the exited child was reaped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_launched_process_is_in_its_own_process_group() {
        // What makes a launched application survive the launcher: a
        // signal to the launcher's process group must not reach it.
        let out = command(
            "/bin/sh",
            &["-c".to_owned(), "ps -o pgid= -p $$".to_owned()],
        )
        .stdout(Stdio::piped())
        .output()
        .expect("run ps");
        let child_pgid: i32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("a process group id");
        let mine = rustix::process::getpgrp().as_raw_nonzero().get();
        assert_ne!(
            child_pgid, mine,
            "the child left the launcher's process group"
        );
    }
}
