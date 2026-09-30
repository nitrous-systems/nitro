//! The operations a file manager does to files: rename, new folder,
//! copy, move.
//!
//! Four functions and an error type, and most of the thinking is in the
//! error type: these run from a status bar with no modal dialogs, so
//! every way they can fail has to be a value with a short sentence in it
//! rather than a panic, an `unwrap`, or a silence.
//!
//! What is *not* here is delete: that is [`crate::trash::Trash::send`],
//! because a file manager that deleted rather than trashed would be one
//! mis-keypress from a bad day. Moving ([`move_into`], a cut-and-paste)
//! is a `rename` when it can be, and across filesystems a copy followed
//! by a delete of the source — only once the copy has completed, so a
//! failure half way leaves the original where it was.

use std::path::{Path, PathBuf};

/// Everything the operations here can fail at, as one value.
///
/// One enum rather than `std::io::Error` throughout, because two of the
/// four cases are not I/O errors at all: a name with a `/` in it and a
/// copy into its own subtree are refused *before* any syscall, and
/// dressing them up as `InvalidInput` would lose the sentence the status
/// bar wants to show.
#[derive(Debug)]
pub enum Error {
    /// The name is not a usable single path component; the string is the
    /// name as typed.
    BadName(String),
    /// A syscall failed.
    Io(std::io::Error),
    /// The target is already there.
    Exists(PathBuf),
    /// A directory was to be copied into itself.
    IntoSelf,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // One line each, no trailing period, no capital: these are
        // appended to a status line, not printed as sentences of their
        // own.
        match self {
            Self::BadName(n) => write!(f, "not a usable name: {n:?}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Exists(p) => write!(f, "already exists: {}", p.display()),
            Self::IntoSelf => write!(f, "cannot copy a directory into itself"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

/// Rename `path` to `new_name`, in the directory it is already in.
///
/// `new_name` must be a **single path component**: no `/`, not `.` or
/// `..`, not empty, and no interior NUL. That restriction is the whole
/// design of this function. An inline rename box that accepted a path
/// would be a *move* — typing `../elsewhere/notes.txt` into it would take
/// the file out of the directory the user is looking at, with no
/// confirmation and no visible destination — and moving files is a
/// different feature with a different interaction. Refusing is a
/// sentence in the status bar; allowing it is a file that vanished.
///
/// An existing target is [`Error::Exists`] rather than a silent
/// overwrite: `rename(2)` would replace it, and losing a file to a typo
/// in a rename box is not recoverable. (The check is a `try_exists`
/// before the `rename`, so it races a second process creating the target
/// in between; [`move_into`] closes it with `renameat2(RENAME_NOREPLACE)`,
/// which a rename box could use too but does not need to. The race needs two programs writing the same directory in the
/// same millisecond, and the window is not new — every file manager has
/// it.)
///
/// # Errors
/// [`Error::BadName`] for a name that is not a single component,
/// [`Error::Exists`] for a target that is there, [`Error::Io`] for the
/// `rename` itself.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf, Error> {
    check_name(new_name)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let target = parent.join(new_name);
    // Renaming a file to the name it already has is a no-op the user
    // means as "never mind", not a collision with itself.
    if target == path {
        return Ok(target);
    }
    if target.try_exists().unwrap_or(false) {
        return Err(Error::Exists(target));
    }
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Create the directory `name` under `parent`.
///
/// The same single-component rule as [`rename`], for the same reason: a
/// "new folder" box that accepted `a/b/c` would create a tree somewhere
/// the user is not looking at, and `mkdir -p` semantics hide a typo
/// (`/home/u` instead of `home u`) as a successful creation.
///
/// # Errors
/// [`Error::BadName`], [`Error::Exists`] when something of that name is
/// already there, [`Error::Io`] for the `mkdir`.
pub fn create_dir(parent: &Path, name: &str) -> Result<PathBuf, Error> {
    check_name(name)?;
    let target = parent.join(name);
    // `create_dir` reports `EEXIST` itself, but as an `io::Error` whose
    // message does not say *what* exists; the explicit check gives the
    // status bar the path.
    if target.try_exists().unwrap_or(false) {
        return Err(Error::Exists(target));
    }
    std::fs::create_dir(&target)?;
    Ok(target)
}

/// Whether a string is a single, usable path component.
///
/// Rejects the empty string, `.`, `..`, anything containing `/`, and
/// anything containing a NUL byte — the last because a NUL truncates the
/// name at the syscall boundary, so `"a\0b"` would create a file called
/// `a` and the user would be looking for one called `a\0b`.
fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(Error::BadName(name.to_owned()));
    }
    Ok(())
}

/// Copy `src` into the directory `dst_dir`, choosing a name that is free.
///
/// Recursive for a directory. The name is `src`'s own if nothing of that
/// name is there, and otherwise `foo copy`, `foo copy 2`, `foo copy 3` …
/// — words rather than `foo.1`, because the result is a file name a user
/// reads, and the suffix goes before the extension so `notes copy.txt`
/// still opens in the right program.
///
/// # Symlinks are followed
///
/// A symlink is copied as **its target's contents**, not as a link. That
/// is the paste a user means when they copy a link to a document
/// (`std::fs::copy` follows, and so does `cp` without `-d`), and the
/// alternative — recreating the link — produces a copy that points at the
/// original and breaks when the original moves. What it costs is a
/// recursive copy of a directory containing a link to something large:
/// the large thing is copied. A link whose target is missing fails with
/// the `ENOENT` of the target, which is reported and does not stop the
/// rest of the copy from having happened.
///
/// # Copying a directory into itself is refused
///
/// `cp -r a a/b` is an infinite tree, and it is a plausible mis-drop in a
/// file list. [`Error::IntoSelf`] is checked before anything is written,
/// by path prefix — lexical, so it cannot be fooled by the ordinary case
/// and *can* be fooled by a symlink pointing back into the source. The
/// recursion depth is bounded anyway ([`MAX_DEPTH`]) so the worst a
/// devious link does is fill a disk with a bounded amount of data rather
/// than loop forever.
///
/// # Errors
/// [`Error::IntoSelf`], [`Error::Exists`] when no free name could be
/// found, and [`Error::Io`] for a failing read, write or `mkdir`.
pub fn copy_into(src: &Path, dst_dir: &Path) -> Result<PathBuf, Error> {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::BadName(src.to_string_lossy().into_owned()))?;
    // Lexical containment: `a` into `a/b` is a tree with no end, and so
    // is `a` into `a` itself.
    if dst_dir == src || dst_dir.starts_with(src) {
        return Err(Error::IntoSelf);
    }
    let target = free_name(dst_dir, &name)?;
    copy_tree(src, &target, 0)?;
    Ok(target)
}

/// Move `src` into the directory `dst_dir`, choosing a name that is free.
///
/// The paste half of a cut. `Ok(None)` when `src` is already in
/// `dst_dir`: cutting a file and pasting it where it is means "leave it",
/// not "make a `foo copy`". A name collision picks the same `foo copy`
/// name as [`copy_into`] rather than replacing what is there.
///
/// The rename is `renameat2(RENAME_NOREPLACE)`, so a file that appears
/// under the chosen name between the check and the rename is never
/// clobbered (a fresh name is picked instead). Across filesystems
/// (`EXDEV`) the move is a [`copy_into`]-style copy of the tree followed
/// by removal of the source — and the source is removed **only** if the
/// copy finished; a failed copy has its partial target removed and the
/// original left alone.
///
/// # Errors
/// [`Error::IntoSelf`] for a directory into its own subtree,
/// [`Error::Exists`] when no free name could be found, [`Error::Io`] for
/// the rename, the copy or the removal.
pub fn move_into(src: &Path, dst_dir: &Path) -> Result<Option<PathBuf>, Error> {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| Error::BadName(src.to_string_lossy().into_owned()))?;
    if src.parent() == Some(dst_dir) {
        return Ok(None);
    }
    if dst_dir == src || dst_dir.starts_with(src) {
        return Err(Error::IntoSelf);
    }
    let mut attempts = 0;
    loop {
        let target = free_name(dst_dir, &name)?;
        match rustix::fs::renameat_with(
            rustix::fs::CWD,
            src,
            rustix::fs::CWD,
            &target,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => return Ok(Some(target)),
            // Someone took the name since `free_name` looked: look again.
            Err(rustix::io::Errno::EXIST) if attempts < 3 => attempts += 1,
            Err(rustix::io::Errno::XDEV) => {
                move_by_copy(src, &target)?;
                return Ok(Some(target));
            }
            // A filesystem or kernel without `RENAME_NOREPLACE`: the
            // `free_name` check is all the protection there is, as for
            // [`rename`].
            Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS) => {
                match std::fs::rename(src, &target) {
                    Ok(()) => return Ok(Some(target)),
                    Err(e) if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                        move_by_copy(src, &target)?;
                        return Ok(Some(target));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(Error::Io(e.into())),
        }
    }
}

/// The cross-filesystem move: copy `src` to `target`, and only if that
/// completed, remove `src`.
///
/// A failed copy removes its partial `target` (best effort) and leaves
/// `src` untouched. The removal looks at the link itself
/// (`symlink_metadata`), so a symlink to a directory is unlinked, never
/// recursed into.
fn move_by_copy(src: &Path, target: &Path) -> Result<(), Error> {
    if let Err(e) = copy_tree(src, target, 0) {
        let _ = if std::fs::symlink_metadata(target).is_ok_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(target)
        } else {
            std::fs::remove_file(target)
        };
        return Err(e);
    }
    remove(src)
}

/// Delete `path` permanently — no trash: a file or symlink is unlinked,
/// a directory is removed with everything under it.
///
/// The check looks at the link itself (`symlink_metadata`), so a symlink
/// to a directory is unlinked and its target is never touched: a
/// permanent delete of a link must not reach through it.
///
/// # Errors
/// [`Error::Io`] when the path cannot be read or removed.
pub fn remove(path: &Path) -> Result<(), Error> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// How deep a recursive copy will go.
///
/// A guard, not a limit anyone should reach: real directory trees are
/// tens deep, and a hundred is where "this is a symlink loop" becomes the
/// likelier explanation than "this is somebody's source tree".
pub const MAX_DEPTH: usize = 100;

/// Copy one file or tree to an exact destination path.
fn copy_tree(src: &Path, dst: &Path, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory nested too deeply to copy",
        )));
    }
    // `metadata`, not `symlink_metadata`: a link is followed, as the
    // documentation above says.
    let md = std::fs::metadata(src)?;
    if md.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()), depth + 1)?;
        }
    } else {
        // `std::fs::copy` copies the permission bits too, which is what
        // makes a copied script still executable.
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// A path in `dir` that nothing occupies: `name`, else `name copy`, else
/// `name copy 2`, …
fn free_name(dir: &Path, name: &str) -> Result<PathBuf, Error> {
    let plain = dir.join(name);
    if !plain.try_exists().unwrap_or(false) {
        return Ok(plain);
    }
    for n in 1..10_000 {
        let candidate = dir.join(copy_name(name, n));
        if !candidate.try_exists().unwrap_or(false) {
            return Ok(candidate);
        }
    }
    Err(Error::Exists(plain))
}

/// The `n`th copy's name: `foo copy`, `foo copy 2`, `foo copy 3`, …
///
/// The suffix goes before the extension so the copy keeps its type, and
/// the first one has no number because "notes copy.txt" is what a person
/// would have typed.
fn copy_name(name: &str, n: u32) -> String {
    let suffix = if n == 1 {
        "copy".to_owned()
    } else {
        format!("copy {n}")
    };
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} {suffix}.{ext}"),
        _ => format!("{name} {suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own; see `dir::tests::scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitro-files-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read the file")
    }

    #[test]
    fn a_rename_stays_in_the_directory() {
        let dir = scratch("rename");
        let file = dir.join("old.txt");
        std::fs::write(&file, b"body").expect("write");
        let moved = rename(&file, "new.txt").expect("rename");
        assert_eq!(moved, dir.join("new.txt"));
        assert!(!file.exists());
        assert_eq!(read(&moved), "body");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_to_a_path_is_refused_rather_than_moving_the_file() {
        // The bug this prevents: a rename box that accepted `../x` is an
        // unconfirmed move to somewhere the user cannot see.
        let dir = scratch("rename-path");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"x").expect("write");
        for bad in [
            "../elsewhere.txt",
            "sub/notes.txt",
            "/etc/passwd",
            "",
            ".",
            "..",
        ] {
            let err = rename(&file, bad).expect_err("refused");
            assert!(matches!(err, Error::BadName(_)), "{bad}: {err}");
        }
        assert!(file.exists(), "and the file did not move");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_onto_an_existing_file_does_not_overwrite_it() {
        let dir = scratch("rename-exists");
        std::fs::write(dir.join("a"), b"a").expect("write");
        std::fs::write(dir.join("b"), b"b").expect("write");
        let err = rename(&dir.join("a"), "b").expect_err("refused");
        assert!(matches!(err, Error::Exists(_)), "{err}");
        // `rename(2)` would have replaced it; both are still here.
        assert_eq!(read(&dir.join("a")), "a");
        assert_eq!(read(&dir.join("b")), "b");
        // Renaming to the name it already has is a no-op, not a
        // collision with itself.
        assert!(rename(&dir.join("a"), "a").is_ok());
        assert_eq!(read(&dir.join("a")), "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_something_that_is_not_there_is_an_io_error() {
        let dir = scratch("rename-missing");
        let err = rename(&dir.join("ghost"), "still-a-ghost").expect_err("an error");
        assert!(matches!(err, Error::Io(_)), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_folder_is_created_and_a_second_one_is_refused() {
        let dir = scratch("mkdir");
        let made = create_dir(&dir, "Projects").expect("mkdir");
        assert!(made.is_dir());
        let err = create_dir(&dir, "Projects").expect_err("refused");
        assert!(matches!(err, Error::Exists(_)), "{err}");
        // A path is not a name here either: `mkdir -p` semantics would
        // hide a typo as a success.
        assert!(matches!(
            create_dir(&dir, "a/b").expect_err("refused"),
            Error::BadName(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_with_a_nul_is_refused() {
        // A NUL truncates the name at the syscall, so the file created
        // would not be the file the user named.
        let dir = scratch("nul");
        assert!(matches!(
            create_dir(&dir, "a\0b").expect_err("refused"),
            Error::BadName(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copying_a_file_into_the_same_directory_picks_a_copy_name() {
        let dir = scratch("copy-name");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"body").expect("write");
        let first = copy_into(&file, &dir).expect("copy");
        assert_eq!(first, dir.join("notes copy.txt"));
        assert_eq!(read(&first), "body");
        let second = copy_into(&file, &dir).expect("copy");
        assert_eq!(second, dir.join("notes copy 2.txt"));
        // A name with no extension numbers just as readably.
        let plain = dir.join("README");
        std::fs::write(&plain, b"x").expect("write");
        assert_eq!(
            copy_into(&plain, &dir).expect("copy"),
            dir.join("README copy")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copying_into_another_directory_keeps_the_name() {
        let dir = scratch("copy-elsewhere");
        let other = dir.join("other");
        std::fs::create_dir(&other).expect("mkdir");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"body").expect("write");
        let landed = copy_into(&file, &other).expect("copy");
        assert_eq!(landed, other.join("notes.txt"));
        assert!(file.exists(), "a copy, not a move");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_is_copied_recursively_with_its_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("copy-tree");
        let src = dir.join("project");
        std::fs::create_dir_all(src.join("src/deep")).expect("mkdir");
        std::fs::write(src.join("src/lib.rs"), b"fn main() {}").expect("write");
        std::fs::write(src.join("src/deep/x"), b"deep").expect("write");
        let script = src.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let dst = dir.join("elsewhere");
        std::fs::create_dir(&dst).expect("mkdir");
        let landed = copy_into(&src, &dst).expect("copy");
        assert_eq!(read(&landed.join("src/lib.rs")), "fn main() {}");
        assert_eq!(read(&landed.join("src/deep/x")), "deep");
        // A copied script is still executable, which is `std::fs::copy`
        // carrying the mode across.
        let mode = std::fs::metadata(landed.join("run.sh"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "the executable bits survived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_is_copied_as_its_targets_contents() {
        let dir = scratch("copy-link");
        let target = dir.join("target.txt");
        std::fs::write(&target, b"the real thing").expect("write");
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let other = dir.join("other");
        std::fs::create_dir(&other).expect("mkdir");

        let landed = copy_into(&link, &other).expect("copy");
        assert_eq!(read(&landed), "the real thing");
        assert!(
            !std::fs::symlink_metadata(&landed)
                .expect("stat")
                .file_type()
                .is_symlink(),
            "a copy, not a second link to the original"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dangling_symlink_copies_as_an_error_not_a_panic() {
        let dir = scratch("copy-dangling");
        let link = dir.join("link");
        std::os::unix::fs::symlink(dir.join("nowhere"), &link).expect("symlink");
        let other = dir.join("other");
        std::fs::create_dir(&other).expect("mkdir");
        let err = copy_into(&link, &other).expect_err("an error");
        assert!(matches!(err, Error::Io(_)), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copying_a_directory_into_itself_is_refused() {
        let dir = scratch("copy-self");
        let src = dir.join("project");
        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        assert!(matches!(
            copy_into(&src, &src).expect_err("refused"),
            Error::IntoSelf
        ));
        assert!(matches!(
            copy_into(&src, &src.join("sub")).expect_err("refused"),
            Error::IntoSelf
        ));
        // A sibling of the same prefix is not inside it, and is allowed:
        // `project2` must not be mistaken for a child of `project`.
        let sibling = dir.join("project2");
        std::fs::create_dir(&sibling).expect("mkdir");
        assert!(copy_into(&src, &sibling).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copying_something_that_is_not_there_is_an_io_error() {
        let dir = scratch("copy-missing");
        let err = copy_into(&dir.join("ghost"), &dir).expect_err("an error");
        assert!(matches!(err, Error::Io(_)), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_into_another_directory_renames_the_file() {
        let dir = scratch("move");
        let other = dir.join("other");
        std::fs::create_dir(&other).expect("mkdir");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"body").expect("write");
        let landed = move_into(&file, &other).expect("move");
        assert_eq!(landed, Some(other.join("notes.txt")));
        assert!(!file.exists(), "a move, not a copy");
        assert_eq!(read(&other.join("notes.txt")), "body");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_move_onto_an_existing_name_takes_a_copy_name() {
        let dir = scratch("move-collide");
        let other = dir.join("other");
        std::fs::create_dir(&other).expect("mkdir");
        std::fs::write(other.join("x.txt"), b"old").expect("write");
        let file = dir.join("x.txt");
        std::fs::write(&file, b"new").expect("write");
        let landed = move_into(&file, &other).expect("move");
        assert_eq!(landed, Some(other.join("x copy.txt")));
        assert_eq!(read(&other.join("x.txt")), "old", "not overwritten");
        assert_eq!(read(&other.join("x copy.txt")), "new");
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_into_its_own_directory_does_nothing() {
        let dir = scratch("move-here");
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"body").expect("write");
        assert_eq!(move_into(&file, &dir).expect("no-op"), None);
        assert_eq!(read(&file), "body");
        assert!(!dir.join("notes copy.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_a_directory_into_itself_is_refused() {
        let dir = scratch("move-self");
        let src = dir.join("project");
        std::fs::create_dir_all(src.join("sub")).expect("mkdir");
        assert!(matches!(
            move_into(&src, &src.join("sub")).expect_err("refused"),
            Error::IntoSelf
        ));
        assert!(matches!(
            move_into(&src, &src).expect_err("refused"),
            Error::IntoSelf
        ));
        assert!(src.join("sub").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_tree_moves() {
        let dir = scratch("move-tree");
        let src = dir.join("project");
        std::fs::create_dir_all(src.join("deep")).expect("mkdir");
        std::fs::write(src.join("deep/x"), b"deep").expect("write");
        let dst = dir.join("elsewhere");
        std::fs::create_dir(&dst).expect("mkdir");
        let landed = move_into(&src, &dst).expect("move").expect("moved");
        assert_eq!(read(&landed.join("deep/x")), "deep");
        assert!(!src.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cross_filesystem_fallback_copies_then_removes() {
        let dir = scratch("move-copy");
        let src = dir.join("project");
        std::fs::create_dir_all(src.join("deep")).expect("mkdir");
        std::fs::write(src.join("deep/x"), b"deep").expect("write");
        let target = dir.join("landed");
        move_by_copy(&src, &target).expect("move");
        assert_eq!(read(&target.join("deep/x")), "deep");
        assert!(!src.exists(), "the source goes once the copy is complete");

        // A symlink to a directory is unlinked, not recursed into.
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        move_by_copy(&link, &dir.join("link-copy")).expect("move");
        assert!(!link.exists());
        assert_eq!(read(&target.join("deep/x")), "deep", "the target survived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_deletes_files_and_trees_and_only_unlinks_symlinks() {
        let dir = scratch("remove");
        let file = dir.join("file");
        std::fs::write(&file, b"x").expect("write");
        remove(&file).expect("remove a file");
        assert!(!file.exists());

        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("deep/er")).expect("mkdir");
        std::fs::write(tree.join("deep/er/x"), b"deep").expect("write");
        let link = dir.join("link");
        std::os::unix::fs::symlink(&tree, &link).expect("symlink");
        remove(&link).expect("remove a link");
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the link is gone"
        );
        assert_eq!(read(&tree.join("deep/er/x")), "deep", "its target survived");

        remove(&tree).expect("remove a tree");
        assert!(!tree.exists());
        assert!(
            matches!(remove(&tree), Err(Error::Io(_))),
            "a missing path is an error"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_cross_filesystem_move_keeps_the_source() {
        let dir = scratch("move-fail");
        let link = dir.join("dangling");
        std::os::unix::fs::symlink(dir.join("nowhere"), &link).expect("symlink");
        let target = dir.join("landed");
        assert!(matches!(
            move_by_copy(&link, &target).expect_err("an error"),
            Error::Io(_)
        ));
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "the source stayed"
        );
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "no partial target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_error_prints_one_short_line_for_the_status_bar() {
        // The status bar has one line and no scrollbar, so a `Display`
        // with a newline in it would truncate the useful half.
        let errors = [
            Error::BadName("a/b".to_owned()),
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied",
            )),
            Error::Exists(PathBuf::from("/tmp/x")),
            Error::IntoSelf,
        ];
        for e in &errors {
            let text = e.to_string();
            assert!(!text.is_empty());
            assert!(!text.contains('\n'), "{text:?} is one line");
            assert!(text.len() < 120, "{text:?} fits a status bar");
        }
        // And the type is a real error, so `?` and `source` work.
        let io: &dyn std::error::Error = &errors[1];
        assert!(io.source().is_some());
    }

    #[test]
    fn the_copy_name_puts_the_suffix_before_the_extension() {
        assert_eq!(copy_name("notes.txt", 1), "notes copy.txt");
        assert_eq!(copy_name("notes.txt", 2), "notes copy 2.txt");
        assert_eq!(copy_name("README", 1), "README copy");
        // A dotfile has no stem, so the suffix goes on the end.
        assert_eq!(copy_name(".bashrc", 1), ".bashrc copy");
    }
}
