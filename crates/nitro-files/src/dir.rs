//! Reading a directory: the rows, their order, how they are written out,
//! and how a big directory is read without stopping the event loop.
//!
//! This module is the file manager's model and contains no widget code,
//! which is the point: the interesting parts — "directories come first,
//! whatever you sorted by", "912 B but 4.2 kB", "a `stat` that failed is
//! still a row" — are properties of a directory listing rather than of a
//! list widget, and they are tested here without a display server.
//!
//! # Times are UTC, and that is a limitation
//!
//! [`format_mtime`] renders the civil date **in UTC**, computed
//! arithmetically from the Unix timestamp with Howard Hinnant's
//! days-from-civil algorithm. The tree links no libc time functions and
//! carries no timezone database, so there is nothing here that knows what
//! `Europe/Berlin` is; `localtime_r` would mean a libc dependency and
//! parsing `/etc/localtime` would mean implementing the `TZif` format,
//! which is a project rather than a line. A user east of Greenwich
//! therefore sees a timestamp that may be a few hours off their clock —
//! wrong, but wrong by a constant, which keeps the column's *ordering*
//! honest and is the failure mode that still lets you find the file you
//! saved this morning. Recorded in `docs/files.md` under *Limitations*.

use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;

/// What a directory entry is, as far as a file list cares.
///
/// The four cases are what `symlink_metadata` can tell us without
/// following anything: a symlink is [`Kind::Symlink`] even when it points
/// at a directory, because resolving it would mean a `stat` per link on
/// every listing and a hang on the one that points into a dead NFS
/// mount. The visible consequence is that a symlinked directory sorts
/// among the files rather than with the directories; entering it still
/// works, because the kernel follows the link when we `read_dir` it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symbolic link, followed by nobody here.
    Symlink,
    /// A fifo, socket, device node, or an entry whose `stat` failed.
    Other,
}

/// One row of a directory listing.
///
/// The name is the file name only, never a path: the directory it is in
/// is the one the app is showing, and carrying it per row would be a
/// thousand copies of the same string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name, lossily decoded.
    ///
    /// A name that is not UTF-8 (which Unix allows: a name is a byte
    /// string with no encoding) comes through with replacement
    /// characters, so it is visible in the list but cannot be opened or
    /// renamed by that text. Showing it is still better than dropping
    /// the row, which would make a file invisible rather than awkward.
    pub name: String,
    /// What it is.
    pub kind: Kind,
    /// Whether this is a symlink whose target is a directory.
    ///
    /// Resolved **once per listing**, with one `metadata` call per symlink
    /// and none at all for anything else — see [`read_dir`]. It is a field
    /// rather than a method because the answer costs a syscall: a
    /// `rows()` that asked it per repaint would `stat` every link on every
    /// selection change, and `rows()` is re-run for one.
    pub symlink_dir: bool,
    /// The **name** of the symbolic icon this row shows, resolved once
    /// when the listing was read.
    ///
    /// A name and not a glyph: the server owns the artwork
    /// (`docs/icons.md`). Resolved here rather than in
    /// [`crate::Files::rows`] because resolving it means a MIME lookup —
    /// a glob match per file — and `rows()` runs again whenever the
    /// selection moves. See [`icon_of`].
    pub icon: &'static str,
    /// Size in bytes; `0` for a directory, and `0` when the `stat`
    /// failed.
    pub size: u64,
    /// Modification time, in seconds since the Unix epoch, and negative
    /// before it.
    pub mtime: i64,
}

impl Entry {
    /// Whether activating this row enters a directory.
    ///
    /// A directory, or a symlink that points at one — the follow happened
    /// when the listing was read, so asking costs nothing here.
    #[must_use]
    pub fn opens_a_directory(&self) -> bool {
        self.kind == Kind::Dir || (self.kind == Kind::Symlink && self.symlink_dir)
    }
}

/// The icon a directory gets, and a symlink pointing at one.
pub const FOLDER_ICON: &str = "folder-fill";
/// The icon a file with no more specific type gets, and a symlink that
/// does not point at a directory.
pub const FILE_ICON: &str = "file-earmark";
/// The icon a fifo, socket, device node or failed `stat` gets.
///
/// `hdd` rather than a file icon because each of those is a *device or
/// channel* rather than a document, and the one thing a user must not
/// conclude from the column is "this is a file I can open".
pub const OTHER_ICON: &str = "hdd";

/// The symbolic icon name for one entry, given the MIME table.
///
/// The whole type → icon decision for a listing in one function, so that
/// [`read_dir`] and the background [`Scan`] cannot disagree about it. The
/// MIME half is [`crate::mime::icon_for`]; everything else is the kind.
#[must_use]
pub fn icon_of(
    kind: Kind,
    name: &str,
    symlink_dir: bool,
    globs: &[crate::mime::Glob],
) -> &'static str {
    match kind {
        Kind::Dir => FOLDER_ICON,
        // A symlink shows what it *points at*, because that is what
        // activating it does. The link itself is still marked, in the
        // detail column — see `Files::rows`.
        Kind::Symlink if symlink_dir => FOLDER_ICON,
        Kind::Symlink => FILE_ICON,
        Kind::Other => OTHER_ICON,
        Kind::File => crate::mime::type_of(Path::new(name), globs)
            .map_or(FILE_ICON, |m| crate::mime::icon_for(&m)),
    }
}

/// The sort order, which `Ctrl+S` cycles through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// By name, case-insensitively.
    Name,
    /// Largest first — the order you want when you are looking for what
    /// filled the disk.
    Size,
    /// Newest first, for the same reason.
    Mtime,
}

impl Sort {
    /// The next order in the cycle.
    ///
    /// A cycle rather than a menu because there are three of them and the
    /// keystroke is cheaper than a popup the toolkit does not have.
    #[must_use]
    pub fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::Size,
            Sort::Size => Sort::Mtime,
            Sort::Mtime => Sort::Name,
        }
    }
}

/// Read one directory into rows.
///
/// Uses `symlink_metadata`, so a symlink is reported as a symlink and its
/// target is never touched for its *kind* — see [`Kind`].
///
/// An entry whose `stat` fails (a symlink into a directory we may not
/// search, a file deleted between the `getdents` and the `stat`) is kept
/// with a size and time of zero rather than dropped: the row exists, the
/// user can see it and act on it, and a listing that silently omitted
/// files would be a file manager you could not trust. Only the
/// `read_dir` itself is an error, because a directory that cannot be
/// opened has no rows at all and the app must say so.
///
/// # What this costs, per row
///
/// One `symlink_metadata`, as before, plus two things the icon column
/// needs and pays for **here rather than per repaint**:
///
/// * a **`metadata` call per symlink**, and only per symlink, to learn
///   whether it points at a directory. That is the one place this module
///   follows a link, and it is bounded by the number of links in the
///   directory rather than by its size;
/// * a **glob match per regular file** ([`crate::mime::type_of`]), which
///   is a suffix comparison against the table and no I/O at all.
///
/// Both land in [`Entry`] fields, so [`crate::Files::rows`] — which is
/// re-run whenever the selection moves — is pure formatting.
///
/// # Errors
/// Whatever `read_dir` returns: the path is missing, is not a directory,
/// or may not be read.
pub fn read_dir(path: &Path, globs: &[crate::mime::Glob]) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        // An individual `DirEntry` error is a read that failed partway
        // through the directory; there is no name to show for it, so
        // there is no row to make.
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let md = std::fs::symlink_metadata(entry.path());
        // The kind comes from the directory read when it can, because
        // `getdents` already carried it and that is one syscall we do
        // not have to spend or have fail.
        let kind = entry
            .file_type()
            .ok()
            .or_else(|| md.as_ref().ok().map(std::fs::Metadata::file_type))
            .map_or(Kind::Other, kind_of);
        // One `stat` *through* the link, for links only. A link into a
        // dead NFS mount can block here — which is the cost the `Kind`
        // doc comment refuses to pay per row, and pays only for the rows
        // that are actually links, once per listing.
        let symlink_dir =
            kind == Kind::Symlink && std::fs::metadata(entry.path()).is_ok_and(|m| m.is_dir());
        let icon = icon_of(kind, &name, symlink_dir, globs);
        let (size, mtime) = md.map_or((0, 0), |m| (m.len(), m.mtime()));
        out.push(Entry {
            name,
            kind,
            symlink_dir,
            icon,
            size,
            mtime,
        });
    }
    Ok(out)
}

/// A `std::fs::FileType` as one of our four kinds.
fn kind_of(ft: std::fs::FileType) -> Kind {
    if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

/// Order the rows: **directories first**, then the chosen key, then the
/// name.
///
/// Directories first whatever the key, because a file manager's list is a
/// place you navigate as much as a place you read: the folders are the
/// part you click through, and having them scattered through a
/// size-ordered list of files makes moving around the filesystem a search
/// problem. Every file manager does this and it is worth saying why.
///
/// The name is always the last comparison, so the order is **total**: two
/// files of the same size in the same second would otherwise come out in
/// whatever order the filesystem handed them over, which changes between
/// listings of the same unchanged directory and makes the list jump under
/// a re-sort. Case-insensitively, because `Downloads` sorting before
/// `apps` is an ASCII artefact nobody means.
pub fn sort(entries: &mut [Entry], by: Sort) {
    entries.sort_by(|a, b| {
        let dirs_first = is_dir(b).cmp(&is_dir(a));
        let key = match by {
            Sort::Name => std::cmp::Ordering::Equal,
            // Biggest and newest first: the reason to sort by these is
            // to find the extremes, and the extreme you want is at the
            // top rather than a page-down away.
            Sort::Size => b.size.cmp(&a.size),
            Sort::Mtime => b.mtime.cmp(&a.mtime),
        };
        dirs_first
            .then(key)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            // The raw name breaks a tie between two names that differ
            // only in case, so `README` and `readme` keep a fixed order
            // rather than swapping between runs.
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Whether an entry sorts with the directories.
fn is_dir(e: &Entry) -> bool {
    e.kind == Kind::Dir
}

/// The rows to show: everything, or everything not starting with `.`.
///
/// Borrowed rather than cloned, because this runs on every keystroke of
/// the `Ctrl+H` toggle and on every repaint of a directory that may hold
/// fifty thousand rows.
#[must_use]
pub fn visible(entries: &[Entry], hidden: bool) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|e| hidden || !e.name.starts_with('.'))
        .collect()
}

/// The size column: `<dir>` for a directory, else a human-readable size.
///
/// Units are **1000-based** (`kB`, `MB`, `GB`), not 1024-based, because
/// that is what `k` means, what the disk on the box is sold in, and what
/// `ls -h --si` and every file manager written this decade shows. One
/// decimal above a kilobyte and none below it: `912 B` is exact and `4.2
/// kB` is the precision anyone reads at a glance.
#[must_use]
pub fn format_size(e: &Entry) -> String {
    if e.kind == Kind::Dir {
        return "<dir>".to_owned();
    }
    format_bytes(e.size)
}

/// The size of a plain byte count, without the directory case.
///
/// Split out from [`format_size`] so the unit arithmetic can be tested
/// against numbers rather than against constructed [`Entry`] values.
#[must_use]
pub fn format_bytes(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    if size < 1000 {
        return format!("{size} B");
    }
    let mut value = size as f64;
    let mut unit = 0;
    // `999.95` rather than `1000.0`: a value that would *print* as
    // `1000.0 kB` after rounding to one decimal belongs in the next unit
    // up, and comparing against 1000 would let 999 999 bytes come out as
    // `1000.0 kB`, which is a unit nobody writes.
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A modification time as `YYYY-MM-DD HH:MM`, in UTC.
///
/// UTC, and the module documentation says why: there is no timezone
/// database in this tree to convert with. Seconds are dropped because the
/// column is for telling files apart by day and hour, and a minute is
/// already finer than the question "is this the one I edited after
/// lunch?".
#[must_use]
pub fn format_mtime(secs: i64) -> String {
    let (y, mo, d, h, mi, _) = civil_from_unix(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}

/// A Unix timestamp as `(year, month, day, hour, minute, second)` in UTC.
///
/// Howard Hinnant's days-from-civil algorithm, run backwards: it shifts
/// the epoch to the 1st of March of year 0 so that the leap day lands at
/// the *end* of the shifted year, which is what makes the month-length
/// arithmetic a pair of divisions instead of a table plus a special case
/// for February. It is exact for every year this program can be handed,
/// including 1900 (not a leap year) and 2000 (one), and it is the reason
/// this file needs no `chrono` and no libc.
///
/// Shared with [`crate::trash`], whose `DeletionDate` is the same civil
/// time in a different punctuation.
pub(crate) fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    const SECS_PER_DAY: i64 = 86_400;
    // Euclidean division, so a timestamp before the epoch floors to the
    // day it is *in* rather than towards zero: `-1` is 23:59:59 on the
    // 31st of December 1969, not 00:00:01 on the 1st of January 1970.
    let days = secs.div_euclid(SECS_PER_DAY);
    let secs_of_day = secs.rem_euclid(SECS_PER_DAY);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day / 60) % 60) as u32;
    let second = (secs_of_day % 60) as u32;

    // Shift the epoch from 1970-01-01 to 0000-03-01.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097); // [0, 146096]
    // Years of era: subtract the leap days, of which there is one every
    // 4 years, minus one every 100, plus one every 400.
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let shifted_month = (5 * day_of_year + 2) / 153; // March is 0
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32; // [1, 31]
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32; // [1, 12]
    // January and February belong to the following civil year.
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

/// The directory above `path`; the root is its own parent.
///
/// The root being its own parent is what makes an "up" button safe to
/// hold down: there is no error state at the top of the tree, the button
/// simply stops changing anything.
#[must_use]
pub fn parent_of(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

/// What the user typed in the path bar, as a path.
///
/// Handles `~`, `~/sub`, absolute paths, paths relative to `cwd`, and the
/// `.`/`..` components in any of them. Purely textual: nothing here
/// touches the filesystem, so it cannot block on a dead mount and cannot
/// fail — whether the answer exists is the caller's next question, and
/// the answer to *that* is a status-line message.
///
/// Because it is textual, `..` is resolved lexically: `a/link/..` becomes
/// `a`, even when `link` points somewhere else entirely. That is what a
/// path bar should do (the user means "up one from what is written") and
/// the opposite of what `chdir` does; it is also why this does not
/// pretend to be `canonicalize`.
#[must_use]
pub fn resolve(input: &str, cwd: &Path) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_with(input, cwd, home.as_deref())
}

/// The pure half of [`resolve`]: the same work with `$HOME` handed in.
///
/// Split out for the same reason `nitro-launcher`'s `dirs_from` is: a
/// test that set `HOME` would race every other test in the binary, and a
/// test that avoided the variable by not testing `~` would be missing the
/// only interesting case.
///
/// With no home (`$HOME` unset, which is a broken session but not our
/// problem to fix), `~` is left to mean a file called `~`, relative to
/// `cwd`. It will not exist, the caller will say so, and nothing has
/// silently gone somewhere unexpected.
#[must_use]
pub fn resolve_with(input: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let input = input.trim();
    if input.is_empty() {
        return cwd.to_path_buf();
    }
    let joined = match (input, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (i, Some(h)) if i.starts_with("~/") => h.join(&i[2..]),
        (i, _) => {
            let p = Path::new(i);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
        }
    };
    lexically_normal(&joined)
}

/// Collapse `.` and `..` components without touching the filesystem.
fn lexically_normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Above the root there is nothing, so `/..` is `/`; above
                // a relative path's start there is a real `..`, which has
                // to be kept or `../x` would become `x`.
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => {
                out.push(other.as_os_str());
                if matches!(other, std::path::Component::Normal(_)) {
                    depth += 1;
                }
            }
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Directories bigger than this are read on a thread.
///
/// Two thousand entries is roughly where a `read_dir` plus a `stat` each
/// stops being free on a warm cache and starts being a visible pause on a
/// cold one — `/usr/bin` is about that size, and it is the directory
/// people open when they want to know whether a file manager is slow.
/// Below it the synchronous path is simpler and finishes inside one
/// frame; above it, a frame is the wrong unit.
pub const BIG_DIR: usize = 2_000;

/// How many entries a directory has, counted no further than `cap`.
///
/// Names only: no `stat`, no metadata, nothing but the `getdents` the
/// kernel is doing anyway, so the cost of asking is a fraction of the
/// cost of the listing it decides about. Capped because the answer is
/// only ever compared against [`BIG_DIR`] — "at least this many" is the
/// whole question, and counting a 200 000-entry directory to the end to
/// find out it is big would be the pause we are trying to avoid.
///
/// A directory that cannot be read counts as empty: the caller is about
/// to try to read it properly and report the error from there.
#[must_use]
pub fn count_at_most(path: &Path, cap: usize) -> usize {
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut n = 0;
    for _ in rd {
        n += 1;
        if n >= cap {
            break;
        }
    }
    n
}

/// A directory being read on a background thread.
///
/// # Why a pipe *and* a channel
///
/// The app loop sleeps in `epoll` ([`nitro_ui::Ui::add_fd`] is how an app
/// adds to that set), so a worker thread that only sent on a
/// `std::sync::mpsc` channel would have no way to wake it: the result
/// would sit in the channel until the user happened to move the mouse.
/// The pipe is therefore the **doorbell** — one byte, written after the
/// result is in the channel, which makes the descriptor readable and the
/// loop return.
///
/// The payload does not go through the pipe because it is a `Vec<Entry>`
/// of arbitrary size: pushing it through a descriptor means choosing a
/// serialisation, and a serialisation between two threads of the same
/// process is work done for nobody. So the channel carries the data and
/// the pipe carries the fact that there is data — which is also exactly
/// how the compositor's own event loop is woken, and is the pattern this
/// crate is expected to establish for "long work off the loop".
///
/// The read end is non-blocking, so [`Scan::take`] can be called on a
/// spurious wakeup without stalling the loop.
pub struct Scan {
    /// The directory being read, so the app can tell a result it still
    /// wants from one for a directory the user has already left.
    path: PathBuf,
    /// The read end of the doorbell pipe.
    wake: OwnedFd,
    /// The payload.
    rx: Receiver<std::io::Result<Vec<Entry>>>,
    /// Kept so the finished thread is joined rather than detached; see
    /// [`Scan::take`].
    join: Option<JoinHandle<()>>,
}

impl Scan {
    /// Start reading `path` on a thread, sorted by `by`, with `globs` as
    /// the MIME table its icon column is resolved against.
    ///
    /// The sort happens on the thread as well, because sorting fifty
    /// thousand rows is the same kind of work as reading them and doing
    /// it on the loop would give back the pause the thread exists to
    /// avoid. The MIME lookups go with them, for the same reason and with
    /// more force: a thousand glob matches on the loop is the cost this
    /// whole module exists to keep off it. The table is **cloned** onto
    /// the thread — a `globs2` on this box is ~2 000 small rules and the
    /// alternative is an `Arc` in the app state for a copy made once per
    /// big directory.
    ///
    /// # Errors
    /// If the pipe cannot be created or the thread cannot be spawned.
    /// Reading the directory itself fails *later*, as the value
    /// [`Scan::take`] hands back.
    pub fn start(path: PathBuf, by: Sort, globs: Vec<crate::mime::Glob>) -> std::io::Result<Scan> {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        let flags = rustix::fs::fcntl_getfl(&read)?;
        rustix::fs::fcntl_setfl(&read, flags | rustix::fs::OFlags::NONBLOCK)?;
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = path.clone();
        let join = std::thread::Builder::new()
            .name("nitro-files-scan".to_owned())
            .spawn(move || {
                let mut result = read_dir(&dir, &globs);
                if let Ok(entries) = &mut result {
                    sort(entries, by);
                }
                // Send *then* ring: a wakeup whose payload had not
                // arrived yet would make `take` return `None` and the
                // result would wait for the next unrelated wakeup.
                let _ = tx.send(result);
                // Retried on `EINTR` only. Anything else, including the
                // `EPIPE` of a `Scan` the app dropped because the user
                // moved on, is the end of this thread's business.
                // (Rust's runtime ignores `SIGPIPE`, so that write is an
                // error rather than a death.)
                while let Err(rustix::io::Errno::INTR) = rustix::io::write(&write, &[1u8]) {}
            })?;
        Ok(Scan {
            path,
            wake: read,
            rx,
            join: Some(join),
        })
    }

    /// The directory this scan is of.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The descriptor to hand to [`nitro_ui::Ui::add_fd`].
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    /// Drain the wakeup byte and take the result if it is ready.
    ///
    /// `None` means "not yet, and nothing is wrong": a descriptor
    /// callback may run for a wakeup that has already been consumed, and
    /// a caller that treated that as an error would report one. After the
    /// result has been taken once, every later call is `None` too — there
    /// is exactly one listing per scan.
    pub fn take(&mut self) -> Option<std::io::Result<Vec<Entry>>> {
        self.drain();
        match self.rx.try_recv() {
            Ok(result) => {
                // The thread has sent and is on its way out; joining it
                // here is immediate and keeps the process from
                // accumulating detached threads over a session's worth
                // of directory changes.
                if let Some(handle) = self.join.take() {
                    let _ = handle.join();
                }
                Some(result)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Swallow whatever the doorbell left in the pipe.
    ///
    /// Non-blocking, so an empty pipe is `EAGAIN` and not a stall.
    fn drain(&self) {
        let mut buf = [0u8; 8];
        loop {
            match rustix::io::read(&self.wake, &mut buf[..]) {
                // Bytes came out; go round, because the pipe may hold
                // more than one doorbell ring.
                Ok(n) if n > 0 => {}
                // Nothing left (`EAGAIN` on the non-blocking read),
                // end of file, or an error we cannot do anything
                // about: the doorbell is quiet either way. Draining is
                // best-effort — a byte left behind costs one extra
                // wakeup, and never a stall.
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own, named after the test and the
    /// process, so two tests in the threaded binary cannot collide and a
    /// leftover from a previous run cannot be mistaken for this one's.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nitro-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn entry(name: &str, kind: Kind, size: u64, mtime: i64) -> Entry {
        Entry {
            name: name.to_owned(),
            kind,
            symlink_dir: false,
            icon: icon_of(kind, name, false, &[]),
            size,
            mtime,
        }
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// Sleep on a scan's descriptor the way the app loop does, up to
    /// `secs`; `true` if it became readable.
    fn wait_readable(scan: &Scan, secs: i64) -> bool {
        let fd = scan.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        rustix::event::poll(&mut fds, Some(&timeout)).expect("poll") == 1
    }

    #[test]
    fn a_directory_reads_as_rows_with_kinds_and_sizes() {
        let dir = scratch("read");
        std::fs::write(dir.join("a.txt"), b"hello").expect("write");
        std::fs::create_dir(dir.join("sub")).expect("mkdir");
        std::os::unix::fs::symlink(dir.join("a.txt"), dir.join("link")).expect("symlink");

        let mut entries = read_dir(&dir, &[]).expect("read the directory");
        sort(&mut entries, Sort::Name);
        assert_eq!(names(&entries), vec!["sub", "a.txt", "link"]);
        let by = |n: &str| {
            entries
                .iter()
                .find(|e| e.name == n)
                .cloned()
                .expect("the entry")
        };
        assert_eq!(by("a.txt").kind, Kind::File);
        assert_eq!(by("a.txt").size, 5);
        assert_eq!(by("sub").kind, Kind::Dir);
        // A symlink is a symlink even though it points at a regular
        // file: nothing here follows it.
        assert_eq!(by("link").kind, Kind::Symlink);
        assert!(by("a.txt").mtime > 1_600_000_000, "a plausible mtime");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_symlink_is_a_row_rather_than_a_dropped_file() {
        // The `stat` of a dangling link fails; the row must survive it,
        // because a file manager that hides files it could not stat is
        // one you cannot use to find out why a file is broken.
        let dir = scratch("dangling");
        std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("dangling")).expect("symlink");
        let entries = read_dir(&dir, &[]).expect("read");
        assert_eq!(names(&entries), vec!["dangling"]);
        assert_eq!(entries[0].kind, Kind::Symlink);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_listing_resolves_each_rows_icon_once_and_follows_only_the_symlinks() {
        // The icon column, from the model's side: every row carries the
        // *name* of an icon, resolved while the directory was read.
        let dir = scratch("icons");
        std::fs::write(dir.join("notes.txt"), b"x").expect("write");
        std::fs::write(dir.join("main.rs"), b"x").expect("write");
        std::fs::write(dir.join("photo.png"), b"x").expect("write");
        std::fs::write(dir.join("song.mp3"), b"x").expect("write");
        std::fs::write(dir.join("clip.mp4"), b"x").expect("write");
        std::fs::write(dir.join("bundle.zip"), b"x").expect("write");
        std::fs::write(dir.join("mystery.qqq"), b"x").expect("write");
        std::fs::create_dir(dir.join("sub")).expect("mkdir");
        std::os::unix::fs::symlink(dir.join("sub"), dir.join("to-dir")).expect("symlink");
        std::os::unix::fs::symlink(dir.join("notes.txt"), dir.join("to-file")).expect("symlink");
        std::os::unix::fs::symlink(dir.join("nowhere"), dir.join("dangling")).expect("symlink");
        std::fs::write(dir.join("file-with-fifo-name"), b"x").expect("write");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.join("pipe"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_bits_truncate(0o600),
            0,
        )
        .expect("mkfifo");

        // An empty glob table: the built-in extension table answers, so
        // this test says the same thing on a box with
        // `shared-mime-info` and on one without.
        let entries = read_dir(&dir, &[]).expect("read");
        let icon = |n: &str| {
            entries
                .iter()
                .find(|e| e.name == n)
                .map_or_else(|| panic!("no entry {n}"), |e| e.icon)
        };
        assert_eq!(icon("sub"), FOLDER_ICON);
        assert_eq!(icon("notes.txt"), "file-earmark-text");
        assert_eq!(icon("main.rs"), "file-earmark-code");
        assert_eq!(icon("photo.png"), "file-earmark-image");
        assert_eq!(icon("song.mp3"), "file-earmark-music");
        assert_eq!(icon("clip.mp4"), "file-earmark-play");
        assert_eq!(icon("bundle.zip"), "file-earmark-zip");
        assert_eq!(icon("mystery.qqq"), FILE_ICON, "an unknown type is a file");
        // A fifo is not a document, and the icon must not invite the user
        // to open it as one.
        assert_eq!(icon("pipe"), OTHER_ICON);
        // A symlink shows what it points at, because that is what
        // activating it does — and a dangling one shows a file rather
        // than a folder, since its `metadata` fails.
        assert_eq!(icon("to-dir"), FOLDER_ICON);
        assert_eq!(icon("to-file"), FILE_ICON);
        assert_eq!(icon("dangling"), FILE_ICON);

        // The follow is recorded, so nothing downstream needs to `stat`
        // again: `opens_a_directory` is the one question activation asks.
        let by = |n: &str| {
            entries
                .iter()
                .find(|e| e.name == n)
                .cloned()
                .unwrap_or_else(|| panic!("no entry {n}"))
        };
        assert!(by("to-dir").symlink_dir && by("to-dir").opens_a_directory());
        assert!(!by("to-file").symlink_dir && !by("to-file").opens_a_directory());
        assert!(!by("dangling").symlink_dir);
        assert!(by("sub").opens_a_directory());
        assert!(!by("notes.txt").opens_a_directory());
        // And the *kind* is untouched by the follow: a symlink to a
        // directory is still a symlink, so it still sorts with the files
        // (`Kind`'s doc comment, and the reason the icon is a separate
        // field rather than a re-derived kind).
        assert_eq!(by("to-dir").kind, Kind::Symlink);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_system_glob_table_decides_the_icon_when_there_is_one() {
        // The half the built-in table cannot show: with a `globs2` the
        // machine's own answer wins, and the icon follows it. A `.ttf` has
        // no built-in type, so this is the same name drawing two
        // different icons with one variable.
        let dir = scratch("icons-globs");
        std::fs::write(dir.join("Vera.ttf"), b"x").expect("write");
        let without = read_dir(&dir, &[]).expect("read");
        assert_eq!(without[0].icon, FILE_ICON);
        let globs = crate::mime::parse_globs2("50:font/ttf:*.ttf\n");
        let with = read_dir(&dir, &globs).expect("read");
        assert_eq!(with[0].icon, "file-earmark-font");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_thousand_row_listing_stays_inside_a_frame() {
        // The cost of the icon column, measured rather than argued, with
        // the **added** work isolated rather than a before/after of the
        // whole function: the third arm below times a thousand `icon_of`
        // calls against the same table with no filesystem in the way, so
        // it is the icon column's own cost and nothing else's.
        //
        // The bound is deliberately loose — 100 ms for a thousand rows on
        // any machine this runs on — because a tight one is a flaky test
        // readers learn to re-run, which is worse than no test. What it
        // guards is a *shape* regression: a lookup that started opening
        // files, or one that moved from per-listing to per-repaint. The
        // numbers beside it are the result, and they are in
        // `docs/files.md`.
        let dir = scratch("thousand");
        for i in 0..1_000 {
            let ext = match i % 5 {
                0 => "txt",
                1 => "rs",
                2 => "png",
                3 => "mp3",
                _ => "zip",
            };
            std::fs::write(dir.join(format!("f{i:04}.{ext}")), b"x").expect("write");
        }
        // A synthetic table the size of a real `globs2` (~2 000 rules on
        // this box), so the measurement is of the worst case the app
        // actually meets rather than of an empty table.
        let text = (0..2_000).fold(String::new(), |mut acc, i| {
            use std::fmt::Write as _;
            let _ = writeln!(acc, "50:application/x-synthetic-{i}:*.ext{i}");
            acc
        });
        let globs = crate::mime::parse_globs2(&text);
        assert_eq!(globs.len(), 2_000);

        let start = std::time::Instant::now();
        let entries = read_dir(&dir, &globs).expect("read");
        let with = start.elapsed();
        assert_eq!(entries.len(), 1_000);
        // Every row really did get an icon, so the timing is of the work
        // and not of a lookup that was skipped.
        assert!(
            entries.iter().all(|e| e.icon.starts_with("file-earmark")),
            "a row came back without an icon"
        );
        assert!(
            entries.iter().any(|e| e.icon == "file-earmark-image"),
            "the types really were resolved"
        );

        let start = std::time::Instant::now();
        let _ = read_dir(&dir, &[]).expect("read");
        let without = start.elapsed();

        // The icon column's own cost: the same thousand names through the
        // same table, with no `getdents` and no `stat` at all.
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let start = std::time::Instant::now();
        let mut sink = 0usize;
        for n in &names {
            sink += icon_of(Kind::File, n, false, &globs).len();
        }
        let lookups = start.elapsed();
        assert!(sink > 0, "the lookups were optimised away");

        eprintln!(
            "1000 rows: {:.1} ms listing with a 2000-rule globs2, {:.1} ms with an \
             empty table, {:.1} ms for the 1000 MIME lookups alone",
            with.as_secs_f64() * 1e3,
            without.as_secs_f64() * 1e3,
            lookups.as_secs_f64() * 1e3
        );
        assert!(
            with.as_millis() < 100,
            "a thousand-row listing took {with:?}, which is past a frame by an order of magnitude"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_a_missing_directory_is_an_error_value() {
        let missing = std::env::temp_dir().join(format!("nitro-files-nope-{}", std::process::id()));
        assert!(read_dir(&missing, &[]).is_err());
    }

    #[test]
    fn directories_come_first_whatever_the_key() {
        let mut entries = vec![
            entry("zebra.txt", Kind::File, 10, 300),
            entry("apps", Kind::Dir, 0, 100),
            entry("big.bin", Kind::File, 9_000, 200),
            entry("work", Kind::Dir, 0, 400),
        ];
        for by in [Sort::Name, Sort::Size, Sort::Mtime] {
            sort(&mut entries, by);
            assert!(
                entries[0].kind == Kind::Dir && entries[1].kind == Kind::Dir,
                "directories first under {by:?}"
            );
        }
    }

    #[test]
    fn names_sort_case_insensitively() {
        let mut entries = vec![
            entry("banana", Kind::File, 1, 1),
            entry("Apple", Kind::File, 1, 1),
            entry("cherry", Kind::File, 1, 1),
        ];
        sort(&mut entries, Sort::Name);
        // The bug this pins: a plain `sort_by_key(|e| &e.name)` puts
        // every capitalised name before every lowercase one, so
        // `Downloads` lands above `apps` for no reason a user can see.
        assert_eq!(names(&entries), vec!["Apple", "banana", "cherry"]);
    }

    #[test]
    fn size_and_time_sort_biggest_and_newest_first_with_the_name_breaking_ties() {
        let mut entries = vec![
            entry("b", Kind::File, 100, 5),
            entry("a", Kind::File, 100, 5),
            entry("c", Kind::File, 900, 1),
        ];
        sort(&mut entries, Sort::Size);
        assert_eq!(names(&entries), vec!["c", "a", "b"]);
        sort(&mut entries, Sort::Mtime);
        assert_eq!(names(&entries), vec!["a", "b", "c"]);
    }

    #[test]
    fn the_order_is_total_so_a_re_sort_does_not_reshuffle() {
        // Two rows equal in every key but the name would otherwise come
        // out in filesystem order, which changes between listings and
        // makes the list jump under the user's cursor.
        let mut a = vec![
            entry("one", Kind::File, 7, 7),
            entry("two", Kind::File, 7, 7),
            entry("six", Kind::File, 7, 7),
        ];
        let mut b = vec![
            entry("two", Kind::File, 7, 7),
            entry("six", Kind::File, 7, 7),
            entry("one", Kind::File, 7, 7),
        ];
        sort(&mut a, Sort::Size);
        sort(&mut b, Sort::Size);
        assert_eq!(names(&a), names(&b));
        // And a name that differs only in case is still ordered.
        let mut c = vec![
            entry("readme", Kind::File, 1, 1),
            entry("README", Kind::File, 1, 1),
        ];
        let mut d = vec![
            entry("README", Kind::File, 1, 1),
            entry("readme", Kind::File, 1, 1),
        ];
        sort(&mut c, Sort::Name);
        sort(&mut d, Sort::Name);
        assert_eq!(names(&c), names(&d));
    }

    #[test]
    fn hidden_files_are_filtered_unless_asked_for() {
        let entries = vec![
            entry(".config", Kind::Dir, 0, 1),
            entry("notes", Kind::File, 1, 1),
            entry("..odd", Kind::File, 1, 1),
        ];
        assert_eq!(
            visible(&entries, false)
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["notes"]
        );
        assert_eq!(visible(&entries, true).len(), 3);
    }

    #[test]
    fn the_sort_cycles_through_all_three_and_returns() {
        assert_eq!(Sort::Name.next(), Sort::Size);
        assert_eq!(Sort::Size.next(), Sort::Mtime);
        assert_eq!(Sort::Mtime.next(), Sort::Name);
    }

    #[test]
    fn sizes_are_1000_based_with_one_decimal_above_a_kilobyte() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(912), "912 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(4_200), "4.2 kB");
        assert_eq!(format_bytes(1_100_000), "1.1 MB");
        assert_eq!(format_bytes(3_400_000_000), "3.4 GB");
        // The rollover a naive `>= 1000` test misses: 999 999 bytes is
        // not `1000.0 kB`, which is a unit nobody writes.
        assert_eq!(format_bytes(999_999), "1.0 MB");
        assert_eq!(format_bytes(1_000), "1.0 kB");
    }

    #[test]
    fn a_directory_shows_no_size() {
        assert_eq!(format_size(&entry("d", Kind::Dir, 4096, 0)), "<dir>");
        assert_eq!(format_size(&entry("f", Kind::File, 1, 0)), "1 B");
    }

    #[test]
    fn a_timestamp_is_the_utc_civil_time_to_the_minute() {
        assert_eq!(format_mtime(0), "1970-01-01 00:00");
        assert_eq!(format_mtime(1_700_000_000), "2023-11-14 22:13");
        // Before the epoch floors into the previous day rather than
        // truncating towards zero.
        assert_eq!(format_mtime(-1), "1969-12-31 23:59");
    }

    #[test]
    fn the_civil_calendar_gets_the_leap_years_right() {
        // 2000 is a leap year (divisible by 400) and 1900 is not
        // (divisible by 100 but not 400); a calendar that gets these
        // backwards is off by a day for decades either side.
        assert_eq!(format_mtime(951_782_400), "2000-02-29 00:00");
        assert_eq!(format_mtime(-2_203_891_200), "1900-03-01 00:00");
        // A December 31st, where the shifted-year arithmetic has to put
        // the day back into the right civil year.
        assert_eq!(format_mtime(1_704_067_199), "2023-12-31 23:59");
    }

    #[test]
    fn the_root_is_its_own_parent() {
        assert_eq!(parent_of(Path::new("/")), PathBuf::from("/"));
        assert_eq!(parent_of(Path::new("/usr/share")), PathBuf::from("/usr"));
        assert_eq!(parent_of(Path::new("/usr")), PathBuf::from("/"));
    }

    #[test]
    fn resolve_expands_a_tilde_and_collapses_dot_dot() {
        let home = Path::new("/home/u");
        let cwd = Path::new("/var/log");
        assert_eq!(resolve_with("~", cwd, Some(home)), PathBuf::from("/home/u"));
        assert_eq!(
            resolve_with("~/src", cwd, Some(home)),
            PathBuf::from("/home/u/src")
        );
        assert_eq!(resolve_with("/etc", cwd, Some(home)), PathBuf::from("/etc"));
        assert_eq!(
            resolve_with("nginx", cwd, Some(home)),
            PathBuf::from("/var/log/nginx")
        );
        assert_eq!(resolve_with("..", cwd, Some(home)), PathBuf::from("/var"));
        assert_eq!(
            resolve_with("../../etc/./ssl", cwd, Some(home)),
            PathBuf::from("/etc/ssl")
        );
        // `/..` is `/`, so holding the key down at the top does nothing.
        assert_eq!(resolve_with("/../..", cwd, Some(home)), PathBuf::from("/"));
        // Nothing typed means where we already are.
        assert_eq!(resolve_with("  ", cwd, Some(home)), cwd.to_path_buf());
    }

    #[test]
    fn without_a_home_a_tilde_is_just_a_name() {
        let cwd = Path::new("/var/log");
        assert_eq!(resolve_with("~", cwd, None), PathBuf::from("/var/log/~"));
        assert_eq!(
            resolve_with("~/x", cwd, None),
            PathBuf::from("/var/log/~/x")
        );
    }

    #[test]
    fn count_at_most_stops_at_the_cap() {
        let dir = scratch("count");
        for i in 0..20 {
            std::fs::write(dir.join(format!("f{i}")), b"").expect("write");
        }
        assert_eq!(count_at_most(&dir, 5), 5, "counting stops at the cap");
        assert_eq!(count_at_most(&dir, 1_000), 20);
        // A directory that cannot be read counts as empty rather than
        // failing: the caller is about to read it properly and report.
        assert_eq!(count_at_most(&dir.join("nope"), 10), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_background_scan_wakes_a_poll_and_hands_over_sorted_entries() {
        let dir = scratch("scan");
        for i in 0..300 {
            std::fs::write(dir.join(format!("f{i:04}")), b"x").expect("write");
        }
        std::fs::create_dir(dir.join("adir")).expect("mkdir");

        let mut scan = Scan::start(dir.clone(), Sort::Name, Vec::new()).expect("start the scan");
        assert_eq!(scan.path(), dir.as_path());

        // Exactly what the app loop does: sleep on the descriptor until
        // the worker rings it.
        assert!(
            wait_readable(&scan, 10),
            "the scan rang the doorbell within ten seconds"
        );

        let entries = scan.take().expect("a result is ready").expect("read ok");
        assert_eq!(entries.len(), 301);
        assert_eq!(entries[0].name, "adir", "sorted on the thread");
        assert_eq!(entries[1].name, "f0000");
        // One listing per scan: a second call is `None`, not a repeat.
        assert!(scan.take().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scan_of_a_missing_directory_delivers_the_error_not_a_panic() {
        let missing = std::env::temp_dir().join(format!("nitro-files-gone-{}", std::process::id()));
        let mut scan = Scan::start(missing, Sort::Name, Vec::new()).expect("start");
        assert!(wait_readable(&scan, 10), "the doorbell rang");
        let result = scan.take().expect("a result");
        assert!(result.is_err(), "the failure arrives as a value");
    }

    #[test]
    fn dropping_a_scan_does_not_take_the_process_with_it() {
        // The worker writes to a pipe whose read end has gone; that is
        // `EPIPE`, which the thread ignores, and not a `SIGPIPE`, which
        // would kill the app when a user left a directory mid-scan.
        let dir = scratch("abandon");
        for i in 0..50 {
            std::fs::write(dir.join(format!("f{i}")), b"").expect("write");
        }
        let scan = Scan::start(dir.clone(), Sort::Name, Vec::new()).expect("start");
        drop(scan);
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Still here.
        assert_eq!(count_at_most(&dir, 100), 50);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
