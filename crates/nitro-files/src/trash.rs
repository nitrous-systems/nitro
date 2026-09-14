//! The `FreeDesktop` trash, or the part of it a file manager needs.
//!
//! Deleting in a file manager should be undoable, and the desktop's
//! answer to that is a directory — `$XDG_DATA_HOME/Trash`, holding
//! `files/` (the things) and `info/` (a `.trashinfo` per thing, saying
//! where it came from and when it left). Anything that implements the
//! same spec, including whatever the user's other desktop ships, can then
//! restore what this put there.
//!
//! # What is implemented, and what is not
//!
//! The **home trash** only. The spec also describes per-filesystem trash
//! directories (`.Trash/$uid` at the mount point, or `.Trash-$uid`),
//! which exist because a file cannot be `rename`d across filesystems: a
//! delete on a USB stick has to go to a trash *on* the stick. This does
//! not create them, so a delete of a file on another filesystem fails
//! with `EXDEV` and says so ([`Trash::send`] argues why that is better
//! than the alternative). Recorded in `docs/files.md` under
//! *Limitations*.
//!
//! Not implemented either: `DeletionDate` in local time (there is no
//! timezone database in this tree — see [`crate::dir`]), the `directorysizes`
//! cache (an optimisation for a trash browser we do not have), and
//! restoring (this crate deletes; the restore is a file manager feature
//! nobody has asked for yet, and the spec's own `trash-cli` can do it).

use std::path::{Path, PathBuf};

/// A trash directory, and the operations that put things in it.
pub struct Trash {
    /// The `Trash` directory itself; `files/` and `info/` live under it.
    root: PathBuf,
}

impl Trash {
    /// The user's home trash, from `$XDG_DATA_HOME` (default
    /// `~/.local/share`).
    ///
    /// With neither variable set — a session broken enough that nothing
    /// else would work either — the root comes out relative
    /// (`.local/share/Trash`), which will fail at the first `create_dir`
    /// with a message rather than deleting something into a path nobody
    /// meant.
    #[must_use]
    pub fn from_env() -> Trash {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".local/share"))
            })
            .unwrap_or_else(|| PathBuf::from(".local/share"));
        Trash::at(data_home.join("Trash"))
    }

    /// A trash rooted at an explicit directory.
    ///
    /// Injectable so the tests never touch the developer's real trash,
    /// and so a future "trash on this filesystem" can be the same code
    /// with a different root.
    pub fn at(root: impl Into<PathBuf>) -> Trash {
        Trash { root: root.into() }
    }

    /// Where this trash lives.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Move `path` into the trash, and return where it landed.
    ///
    /// # The order matters
    ///
    /// The `.trashinfo` is written **before** the file is moved, because
    /// the spec requires that a file in `files/` always have its info
    /// file: a trash containing a file nobody knows the origin of cannot
    /// be restored, and a crash between the two operations is exactly
    /// when that would happen. Written first, the failure window holds an
    /// *orphan info file* instead, which is the recoverable direction —
    /// and this cleans it up itself when the move then fails.
    ///
    /// The info file is created with `O_EXCL`, which is what makes the
    /// name reservation atomic: two file managers trashing `notes.txt` at
    /// the same moment cannot both decide the name is free.
    ///
    /// # Cross-filesystem deletes fail
    ///
    /// The move is a `rename`, so trashing a file that is not on the same
    /// filesystem as the trash fails with `EXDEV` (an "Invalid
    /// cross-device link" in the status line). The alternative — copy,
    /// then delete — is a *different operation* wearing the same name: it
    /// is not atomic, it can half-finish on a full disk leaving the user
    /// with neither the original nor a trashed copy, and on a large
    /// directory it takes minutes with no progress to show. The spec's
    /// answer is a trash on the other filesystem, which is the feature
    /// this does not have. Failing loudly is the honest version of not
    /// having it.
    ///
    /// # Errors
    /// If the trash directories cannot be created, if the info file
    /// cannot be written, or if the `rename` fails — including `EXDEV`
    /// above, and `ENOENT` for a file that is already gone.
    pub fn send(&self, path: &Path) -> std::io::Result<PathBuf> {
        let files = self.root.join("files");
        let info = self.root.join("info");
        std::fs::create_dir_all(&files)?;
        std::fs::create_dir_all(&info)?;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cannot trash a path with no file name",
                )
            })?;
        // An absolute original path, so the info file says something a
        // restore can act on from any working directory.
        let original = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().map_or_else(|_| path.to_path_buf(), |d| d.join(path))
        };

        let (name, mut info_file) = reserve(&info, &name)?;
        let body = trashinfo(&original, now_unix());
        // The write and the rename are separated so a failed write can
        // take the reservation back down with it.
        let result = std::io::Write::write_all(&mut info_file, body.as_bytes()).and_then(|()| {
            drop(info_file);
            std::fs::rename(path, files.join(&name))
        });
        if let Err(e) = result {
            // The orphan cleanup: an info file naming a file that is not
            // in the trash is litter, and litter that a restore would
            // trip over.
            let _ = std::fs::remove_file(info.join(format!("{name}.trashinfo")));
            return Err(e);
        }
        Ok(files.join(name))
    }
}

/// Claim a free name in `info`, returning it and the open info file.
///
/// `create_new` is `O_EXCL`: the file either did not exist and is now
/// ours, or somebody else has the name and we try the next one. The
/// suffix is inserted before the extension (`notes 2.txt`, not
/// `notes.txt 2`) so a restored duplicate still opens in the right
/// program.
///
/// A free function rather than a method, because it needs nothing from
/// the trash but the `info` directory it is handed.
fn reserve(info: &Path, name: &str) -> std::io::Result<(String, std::fs::File)> {
    for n in 0..10_000 {
        let candidate = if n == 0 {
            name.to_owned()
        } else {
            numbered(name, n)
        };
        match std::fs::File::create_new(info.join(format!("{candidate}.trashinfo"))) {
            Ok(f) => return Ok((candidate, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    // Ten thousand files of the same name in one trash is not a state to
    // keep looping over; the user needs to empty it.
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many files of that name in the trash",
    ))
}

/// `name` with ` N` inserted before its extension.
fn numbered(name: &str, n: u32) -> String {
    // `rsplit_once` on the *last* dot, and not when the name begins with
    // it: `.bashrc` is a hidden file, not an extension.
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} {n}.{ext}"),
        _ => format!("{name} {n}"),
    }
}

/// The body of a `.trashinfo` file.
///
/// ```text
/// [Trash Info]
/// Path=/home/u/notes.txt
/// DeletionDate=2024-03-01T09:15:00
/// ```
///
/// `Path` is percent-encoded per the spec, which points at RFC 2396: the
/// unreserved set (`A-Z a-z 0-9 - _ . ! ~ * ' ( )`) and `/` pass through,
/// everything else becomes `%XX` over the path's **bytes**, so a name
/// that is not UTF-8 survives the round trip. The separators are left
/// alone because the value is a path and a restore has to be able to read
/// it as one.
///
/// `DeletionDate` is `YYYY-MM-DDTHH:MM:SS` in **UTC**, for the reason
/// [`crate::dir::format_mtime`] gives: there is no timezone database
/// here to convert with. The spec asks for local time, so a trash
/// browser will show a deletion as having happened at an hour that is off
/// by the machine's offset. It is a display artefact — nothing restores
/// by date — and it is the same limitation in the same place as the
/// listing's time column.
#[must_use]
pub fn trashinfo(original: &Path, when_unix: i64) -> String {
    let (y, mo, d, h, mi, s) = crate::dir::civil_from_unix(when_unix);
    format!(
        "[Trash Info]\nPath={}\nDeletionDate={y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}\n",
        percent_encode(original)
    )
}

/// A path, percent-encoded for a `.trashinfo` `Path=` value.
fn percent_encode(path: &Path) -> String {
    use std::fmt::Write as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::new();
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()/".contains(&b) {
            out.push(b as char);
        } else {
            // Writing into a `String` is infallible, so the `Result` is
            // the formatter's shape rather than a failure to handle.
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// The wall clock, in seconds since the epoch.
///
/// `0` if the clock is before the epoch, which is a machine with no RTC
/// and no network time rather than a case to propagate an error for: the
/// deletion still happens, and its recorded date is wrong in a way the
/// user can see.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own; see `dir::tests::scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitro-files-trash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).expect("read the file")
    }

    #[test]
    fn sending_a_file_moves_it_and_writes_its_info() {
        let dir = scratch("send");
        let trash = Trash::at(dir.join("Trash"));
        let file = dir.join("notes.txt");
        std::fs::write(&file, b"hello").expect("write");

        let landed = trash.send(&file).expect("trash the file");
        assert_eq!(landed, dir.join("Trash/files/notes.txt"));
        assert!(!file.exists(), "the original is gone");
        assert_eq!(read(&landed), "hello");

        let info = read(&dir.join("Trash/info/notes.txt.trashinfo"));
        assert!(info.starts_with("[Trash Info]\n"), "{info}");
        assert!(
            info.contains(&format!("Path={}\n", file.display())),
            "the original path is recorded: {info}"
        );
        assert!(info.contains("DeletionDate="), "{info}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_file_of_the_same_name_gets_a_number() {
        let dir = scratch("collide");
        let trash = Trash::at(dir.join("Trash"));
        for body in ["first", "second", "third"] {
            let file = dir.join("notes.txt");
            std::fs::write(&file, body).expect("write");
            trash.send(&file).expect("trash");
        }
        // The suffix goes before the extension, so a restored copy still
        // opens in the right program.
        assert_eq!(read(&dir.join("Trash/files/notes.txt")), "first");
        assert_eq!(read(&dir.join("Trash/files/notes 1.txt")), "second");
        assert_eq!(read(&dir.join("Trash/files/notes 2.txt")), "third");
        assert!(dir.join("Trash/info/notes 2.txt.trashinfo").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_goes_to_the_trash_whole() {
        let dir = scratch("dir");
        let trash = Trash::at(dir.join("Trash"));
        let sub = dir.join("project");
        std::fs::create_dir_all(sub.join("src")).expect("mkdir");
        std::fs::write(sub.join("src/lib.rs"), b"fn main() {}").expect("write");

        let landed = trash.send(&sub).expect("trash the directory");
        assert!(landed.join("src/lib.rs").exists(), "contents came along");
        assert!(!sub.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trashing_a_missing_file_is_an_error_and_leaves_no_orphan_info() {
        // The failure window the ordering creates: the info file exists
        // before the rename, so a rename that fails must take it away
        // again or the trash accumulates entries for files that are not
        // in it.
        let dir = scratch("orphan");
        let trash = Trash::at(dir.join("Trash"));
        let missing = dir.join("ghost.txt");
        assert!(trash.send(&missing).is_err());
        assert!(
            !dir.join("Trash/info/ghost.txt.trashinfo").exists(),
            "the info file was cleaned up"
        );
        // And the next attempt gets the un-numbered name back, which is
        // the visible proof the reservation was released.
        std::fs::write(&missing, b"now it exists").expect("write");
        let landed = trash.send(&missing).expect("trash");
        assert_eq!(landed.file_name().expect("a name"), "ghost.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_trash_directories_are_created_on_demand() {
        let dir = scratch("create");
        let trash = Trash::at(dir.join("nested/deep/Trash"));
        assert_eq!(trash.root(), dir.join("nested/deep/Trash"));
        let file = dir.join("a");
        std::fs::write(&file, b"x").expect("write");
        trash.send(&file).expect("trash");
        assert!(dir.join("nested/deep/Trash/files").is_dir());
        assert!(dir.join("nested/deep/Trash/info").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_info_body_is_the_specs_shape() {
        let body = trashinfo(Path::new("/home/u/notes.txt"), 1_700_000_000);
        assert_eq!(
            body,
            "[Trash Info]\nPath=/home/u/notes.txt\nDeletionDate=2023-11-14T22:13:20\n"
        );
    }

    #[test]
    fn the_path_is_percent_encoded_but_the_separators_are_not() {
        // A restore has to read the value as a path, so `/` stays; a
        // space and a `%` do not, or the value would be ambiguous.
        let body = trashinfo(Path::new("/home/u/my notes (1).txt"), 0);
        assert!(
            body.contains("Path=/home/u/my%20notes%20(1).txt\n"),
            "{body}"
        );
        let body = trashinfo(Path::new("/home/u/100%.txt"), 0);
        assert!(body.contains("Path=/home/u/100%25.txt\n"), "{body}");
        // The unreserved set passes through untouched.
        let body = trashinfo(Path::new("/a/-_.!~*'()"), 0);
        assert!(body.contains("Path=/a/-_.!~*'()\n"), "{body}");
    }

    #[test]
    fn a_non_utf8_name_is_encoded_byte_by_byte() {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::OsStr::from_bytes(b"/tmp/bad\xff\xfename");
        let body = trashinfo(Path::new(name), 0);
        assert!(body.contains("Path=/tmp/bad%FF%FEname\n"), "{body}");
    }

    #[test]
    fn numbering_puts_the_suffix_before_the_extension() {
        assert_eq!(numbered("notes.txt", 1), "notes 1.txt");
        assert_eq!(numbered("archive.tar.gz", 2), "archive.tar 2.gz");
        // A name with no extension, and a dotfile, which has no stem to
        // put the number after.
        assert_eq!(numbered("README", 3), "README 3");
        assert_eq!(numbered(".bashrc", 4), ".bashrc 4");
    }
}
