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
}
