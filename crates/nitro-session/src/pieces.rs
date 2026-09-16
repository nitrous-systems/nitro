//! What the session starts, in what order, and where it finds it.
//!
//! # The order is start order, and teardown is its reverse
//!
//! `[server, wallpaper, bar, launcher]`. The server first because nothing
//! else has anything to connect *to* — every shell piece opens
//! `shell.sock` in its first hundred microseconds and exits if it is not
//! there. Then the wallpaper, the bar, the launcher, which is
//! back-to-front on screen: the backdrop exists before anything can be
//! seen in front of it, so the first frame the user sees is never a bar
//! floating on the compositor's fallback grey.
//!
//! Teardown reverses it, and the reason is the same fact read backwards:
//! **a piece must not outlive the thing it talks to**. Stopping the
//! server first would leave three toolkit clients blocking on a socket
//! that has gone, each of which would then log a connection error on its
//! way out — three fatal-looking messages in the journal for what is in
//! fact a clean shutdown. Reverse order means every piece is stopped
//! while the server it is connected to is still answering, and the
//! server sees three clean disconnects and then its own SIGTERM.
//!
//! # Where the binaries come from
//!
//! **Next to `nitro-session` itself, then `$PATH`.** The test box's
//! `~/nitro-bin` is a flat directory of rsynced binaries that is not on
//! `$PATH`, and `just deploy` replaces all of them at once; a session
//! that searched `$PATH` first would start yesterday's `/usr/local/bin`
//! bar next to today's server. `current_exe`'s directory is the same
//! answer [`nitro-launcher`'s `exe_dir`] gives for the same reason, and
//! it makes a `target/release` run of the session pick up the rest of a
//! `target/release` build.
//!
//! `$PATH` remains the fallback, so an installed session in
//! `/usr/bin/nitro-session` finds an installed bar, and a sibling that is
//! not executable falls through to it rather than becoming a start
//! failure.
//!
//! # …and the same directory goes *onto* `PATH` for the children
//!
//! [`path_with_bin_dir`] prepends that directory to the `PATH` every
//! child inherits. The sibling lookup above answers "where is the bar?"
//! for the session; this answers the same question for everything the
//! session's children go on to start, and the case that forced it is the
//! launcher's.
//!
//! A `.desktop` file's `Exec=` is a **bare program name** — that is what
//! the freedesktop spec says to write and what a packager ships, because
//! on an ordinary system the binary is in `/usr/bin` and `/usr/bin` is on
//! `PATH`. On the box the binaries are in `~/nitro-bin`, which is on
//! nobody's `PATH`, so `Exec=nitro-term` was an `execvp` that could only
//! fail — and because a `.desktop` file *shadows* the launcher's built-in
//! entry for the same program, installing `deploy/nitro-term.desktop`
//! replaced a working launcher entry with `spawn: No such file or
//! directory`. That is why `just deploy-bins` refused to install the
//! files at all, and it is the root cause this fixes rather than works
//! around: the session is the process that knows where the desktop's
//! binaries are, so it is the process that should say so to its
//! children.
//!
//! **Prepended, not appended.** The same argument as the sibling lookup,
//! one level down: a box with a stale `/usr/local/bin/nitro-term` must
//! start the one that was deployed beside the running session, not
//! yesterday's. And it is the *session's* directory rather than a
//! configured one, so a `/usr/bin/nitro-session` contributes `/usr/bin`
//! — already on `PATH`, and therefore a no-op rather than a surprise.
//!
//! Nothing else about the environment is touched: the session passes its
//! environment on unchanged, which is how `NITRO_BACKEND` reaches the
//! server without this crate knowing the name.
//!
//! [`nitro-launcher`'s `exe_dir`]: https://example.invalid

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// What a piece is, for the parts of the supervisor that treat the
/// server differently from the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `nitro-server`. Its exit ends the session; it is never restarted.
    Server,
    /// A shell client: wallpaper, bar, launcher. Restarted with backoff.
    Shell,
}

/// One thing the session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// Binary name, as found next to us or on `$PATH`.
    pub program: &'static str,
    /// What it is.
    pub role: Role,
}

/// The server, the wallpaper, the bar, the launcher — in start order.
pub const PIECES: &[Piece] = &[
    Piece {
        program: "nitro-server",
        role: Role::Server,
    },
    Piece {
        program: "nitro-wallpaper",
        role: Role::Shell,
    },
    Piece {
        program: "nitro-bar",
        role: Role::Shell,
    },
    Piece {
        program: "nitro-launcher",
        role: Role::Shell,
    },
];

/// The directory the running executable is in, if it can be determined.
#[must_use]
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(Path::to_path_buf)
}

/// Resolve `program` to a path: a sibling of `dir` if there is an
/// executable one, else the bare name for `execvp` to find on `$PATH`.
///
/// Returning the bare name rather than failing is what makes `$PATH` a
/// real fallback: the kernel does the search, and a "not found" surfaces
/// as the spawn error it is, with the program name in it.
#[must_use]
pub fn resolve(program: &str, dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = dir {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return candidate;
        }
    }
    PathBuf::from(program)
}

/// `PATH` with `dir` prepended, for the environment a child inherits.
///
/// `None` when there is nothing to do, which is three cases: no
/// directory; a directory that is already the first entry (so a caller
/// can leave the variable alone rather than rewrite it to itself); and a
/// directory whose path is **not UTF-8**.
///
/// That third one is a deliberate give-up rather than an oversight. A
/// `PATH` is a `:`-joined byte string and `OsString` could carry it, but
/// the child of a session installed under a non-UTF-8 path would then get
/// a `PATH` this function cannot log, compare or explain — and the whole
/// value of the prepend is that `/proc/<pid>/environ` answers "why did
/// that resolve?". Leaving `PATH` untouched loses the launcher's bare
/// `Exec=` on such a box, which is a visible missing icon rather than a
/// silent misbehaviour, and no box we ship to is in that state.
///
/// A `PATH` that is unset or
/// empty becomes just `dir`, which is the same answer `execvp` would
/// give for an empty `PATH` on a system with a confused environment
/// (POSIX says an empty `PATH` means the current directory, and
/// inheriting *that* is not a thing a session should hand its children).
///
/// The directory is not checked for existence: the whole point is that
/// `execvp` searches, and a `PATH` entry that does not exist is skipped
/// by the kernel's search rather than being an error. Checking here
/// would also be a TOCTOU against an rsync in flight — exactly the box's
/// deploy.
#[must_use]
pub fn path_with_bin_dir(dir: Option<&Path>, path: Option<&str>) -> Option<String> {
    let dir = dir?;
    let dir = dir.to_str()?;
    if dir.is_empty() {
        return None;
    }
    let Some(path) = path.filter(|p| !p.is_empty()) else {
        return Some(dir.to_owned());
    };
    // Already first: rewriting `PATH` to itself would churn the
    // environment of every restart for nothing, and would make the
    // variable look edited in a `/proc/<pid>/environ` somebody is
    // reading to answer "who put that there?".
    if path.split(':').next() == Some(dir) {
        return None;
    }
    Some(format!("{dir}:{path}"))
}

/// Whether `path` is a regular file with an execute bit set.
///
/// The check is deliberately "could this be executed" and not "does this
/// exist": a stale directory named `nitro-bar`, or a half-rsynced file
/// with mode `0600`, must fall through to `$PATH` rather than turn every
/// start attempt into `EACCES`.
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order is a promise two other modules depend on, so it is
    /// asserted here rather than read off the constant at review time.
    #[test]
    fn the_server_is_first_and_the_shell_follows_back_to_front() {
        let names: Vec<_> = PIECES.iter().map(|p| p.program).collect();
        assert_eq!(
            names,
            vec![
                "nitro-server",
                "nitro-wallpaper",
                "nitro-bar",
                "nitro-launcher"
            ]
        );
        assert_eq!(PIECES[0].role, Role::Server);
        assert!(PIECES[1..].iter().all(|p| p.role == Role::Shell));
        assert_eq!(
            PIECES.iter().filter(|p| p.role == Role::Server).count(),
            1,
            "exactly one piece ends the session by exiting"
        );
    }

    /// Teardown is the reverse of start, and the server is last out.
    #[test]
    fn teardown_order_is_the_reverse_and_ends_at_the_server() {
        let down: Vec<_> = PIECES.iter().rev().map(|p| p.program).collect();
        assert_eq!(
            down,
            vec![
                "nitro-launcher",
                "nitro-bar",
                "nitro-wallpaper",
                "nitro-server"
            ]
        );
    }

    #[test]
    fn a_sibling_binary_wins_over_the_path() {
        let dir = std::env::temp_dir().join(format!("nitro-session-pieces-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("nitro-bar");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();

        // Not executable yet: it must fall through to `$PATH` rather
        // than become an `EACCES` at spawn time.
        assert_eq!(resolve("nitro-bar", Some(&dir)), PathBuf::from("nitro-bar"));

        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve("nitro-bar", Some(&dir)), exe);

        // A directory of that name is not a program either.
        let subdir = dir.join("nitro-launcher");
        std::fs::create_dir_all(&subdir).unwrap();
        assert_eq!(
            resolve("nitro-launcher", Some(&dir)),
            PathBuf::from("nitro-launcher")
        );

        assert_eq!(resolve("nitro-bar", None), PathBuf::from("nitro-bar"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The children's `PATH` gets the session's own directory in front,
    /// which is what makes a packaged `Exec=nitro-term` resolve on a box
    /// whose binaries are in `~/nitro-bin`.
    #[test]
    fn the_bin_dir_goes_in_front_of_the_inherited_path() {
        let dir = Path::new("/home/kaspar/nitro-bin");
        assert_eq!(
            path_with_bin_dir(Some(dir), Some("/usr/bin:/bin")).as_deref(),
            Some("/home/kaspar/nitro-bin:/usr/bin:/bin"),
            "prepended, so a deployed binary wins over a stale installed one"
        );
    }

    #[test]
    fn an_absent_or_empty_path_becomes_the_bin_dir_alone() {
        let dir = Path::new("/opt/nitro");
        assert_eq!(
            path_with_bin_dir(Some(dir), None).as_deref(),
            Some("/opt/nitro")
        );
        assert_eq!(
            path_with_bin_dir(Some(dir), Some("")).as_deref(),
            Some("/opt/nitro"),
            "an empty PATH means the current directory to execvp; the \
             children get the session's directory instead"
        );
    }

    /// Two no-ops, and both matter: a session with no directory to offer
    /// must not touch the variable, and a session already first in the
    /// list must not rewrite `PATH` to itself on every restart.
    #[test]
    fn there_is_nothing_to_do_without_a_dir_or_when_it_is_already_first() {
        assert_eq!(path_with_bin_dir(None, Some("/usr/bin")), None);
        // A non-UTF-8 directory is the third `None`: documented as a
        // give-up rather than silently absent, and pinned so a future
        // `OsString` rewrite has to change the test that states the rule.
        {
            use std::os::unix::ffi::OsStrExt as _;
            let raw = std::ffi::OsStr::from_bytes(b"/opt/\xff\xfenitro");
            assert_eq!(
                path_with_bin_dir(Some(Path::new(raw)), Some("/usr/bin")),
                None,
                "a path that is not UTF-8 leaves PATH alone"
            );
        }
        assert_eq!(
            path_with_bin_dir(Some(Path::new("/usr/bin")), Some("/usr/bin:/bin")),
            None,
            "an installed session contributes a directory that is already there"
        );
        // Present but *not* first is still a change: the whole point is
        // that the session's own directory outranks the rest.
        assert_eq!(
            path_with_bin_dir(Some(Path::new("/usr/bin")), Some("/bin:/usr/bin")).as_deref(),
            Some("/usr/bin:/bin:/usr/bin")
        );
    }
}
