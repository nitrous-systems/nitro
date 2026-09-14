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
//! reaps it: [`Children::reap`] runs a non-blocking `try_wait` over the
//! outstanding children before every spawn, and the launcher's own exit
//! hands the rest to init. The cost of getting this wrong is a zombie —
//! a task-table entry and nothing else — and the cost of *not* tracking
//! the children at all would be one per launch for the whole session.
//! The double-fork that avoids both needs the `fork` above.
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

use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

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

/// Reaps the children a launcher has started.
///
/// Held by the launcher's state so `try_wait` can be called on the next
/// launch; see the module docs for what this does and does not promise.
#[derive(Debug, Default)]
pub struct Children {
    live: Vec<Child>,
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

    /// Reap whatever has exited. Called before every spawn, so the list
    /// is bounded by the number of launches that are still running rather
    /// than by the number ever made.
    pub fn reap(&mut self) {
        self.live
            .retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
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
        self.live.push(child);
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
