//! The on-disk font index cache.
//!
//! Scanning means opening every font file on the box and parsing its `name`
//! table; on the test box that is 47 files and 12.6 MB of I/O for an index
//! that fits in a few kilobytes. This module writes that index to
//! `$XDG_CACHE_HOME/nitro/fonts.idx` so the next boot only has to *walk* the
//! directories, not read them.
//!
//! The format is hand-written — a length-prefixed byte soup, no serde, no
//! version negotiation beyond a magic and a version number. Anything that
//! does not parse, or that describes a different set of files than the walk
//! just found, is discarded and the scan runs for real: a stale cache must
//! never be able to make the server draw with a font that is not there.
//!
//! **Validation is by file list, not by mtime alone.** The walk is cheap
//! (`readdir` plus one `stat` per font file), it is going to happen anyway to
//! find the files, and comparing the exact paths **with their size and mtime**
//! catches everything a directory mtime misses — most importantly a font
//! replaced in place, which changes neither the file list nor the directory.
//! The directory mtimes and file counts are recorded and checked too, so a
//! same-named replacement of a *directory* also invalidates.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Magic, so a truncated or foreign file is rejected before anything else.
const MAGIC: &[u8; 8] = b"NITROFNT";
/// Bumped whenever the layout below changes; an older file is discarded.
const VERSION: u32 = 1;
/// Refuse to parse anything larger; a cache file this big is corruption.
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// One face as the cache stores it: everything the index needs and no bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FaceRecord {
    /// Index into the file list.
    pub(crate) file: u32,
    /// Face index within that file.
    pub(crate) index: u32,
    /// Family name, original case.
    pub(crate) family: String,
    /// CSS weight.
    pub(crate) weight: u16,
    /// Stretch as swash's raw percentage-ish value.
    pub(crate) stretch: u16,
    /// Italic or oblique.
    pub(crate) italic: bool,
    /// `post.isFixedPitch`.
    pub(crate) monospace: bool,
}

/// One font file as the walk found it: path, size and mtime. All three are
/// compared against the cache, so an in-place replacement invalidates.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FileRecord {
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
    pub(crate) mtime_secs: u64,
    pub(crate) mtime_nanos: u32,
}

/// A parsed cache file: the directories it was built from and the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Index {
    /// `(directory, mtime seconds, mtime nanos, font files found under it)`.
    pub(crate) dirs: Vec<(PathBuf, u64, u32, u32)>,
    /// Every font file, in the same sorted order the scan uses.
    pub(crate) files: Vec<FileRecord>,
    /// Every face, in discovery order.
    pub(crate) faces: Vec<FaceRecord>,
}

/// Where the cache lives: `$XDG_CACHE_HOME/nitro/fonts.idx`, else
/// `~/.cache/nitro/fonts.idx`.
///
/// `NITRO_FONT_INDEX_CACHE` overrides it: a path uses that file, and `0`,
/// `off` or `none` disables the cache entirely (what the timing comparison in
/// the README was measured with).
pub(crate) fn cache_path() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("NITRO_FONT_INDEX_CACHE") {
        if matches!(v.as_str(), "0" | "off" | "none" | "") {
            return None;
        }
        return Some(PathBuf::from(v));
    }
    let base = if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(x)
    } else {
        PathBuf::from(std::env::var("HOME").ok()?).join(".cache")
    };
    Some(base.join("nitro").join("fonts.idx"))
}

/// Directory stamp: mtime split into seconds and nanoseconds. `(0, 0)` for a
/// directory that cannot be stat'ed, which simply means such a directory never
/// validates a cache.
pub(crate) fn dir_stamp(dir: &Path) -> (u64, u32) {
    let Ok(meta) = std::fs::metadata(dir) else {
        return (0, 0);
    };
    mtime(&meta)
}

/// A file's stamp: its size and mtime. Zeroes when it cannot be stat'ed, so
/// an unreadable file never validates a cache either.
pub(crate) fn file_record(path: &Path) -> FileRecord {
    let (len, (mtime_secs, mtime_nanos)) = match std::fs::metadata(path) {
        Ok(meta) => (meta.len(), mtime(&meta)),
        Err(_) => (0, (0, 0)),
    };
    FileRecord {
        path: path.to_path_buf(),
        len,
        mtime_secs,
        mtime_nanos,
    }
}

/// Modification time as `(seconds, nanos)` since the epoch, `(0, 0)` when the
/// platform or the file system cannot say.
fn mtime(meta: &std::fs::Metadata) -> (u64, u32) {
    let Ok(mtime) = meta.modified() else {
        return (0, 0);
    };
    let Ok(delta) = mtime.duration_since(SystemTime::UNIX_EPOCH) else {
        return (0, 0);
    };
    (delta.as_secs(), delta.subsec_nanos())
}

/// Serialize an index.
pub(crate) fn encode(index: &Index) -> Vec<u8> {
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, VERSION);
    put_u32(&mut out, index.dirs.len() as u32);
    for (path, secs, nanos, files) in &index.dirs {
        put_path(&mut out, path);
        put_u64(&mut out, *secs);
        put_u32(&mut out, *nanos);
        put_u32(&mut out, *files);
    }
    put_u32(&mut out, index.files.len() as u32);
    for file in &index.files {
        put_path(&mut out, &file.path);
        put_u64(&mut out, file.len);
        put_u64(&mut out, file.mtime_secs);
        put_u32(&mut out, file.mtime_nanos);
    }
    put_u32(&mut out, index.faces.len() as u32);
    for face in &index.faces {
        put_u32(&mut out, face.file);
        put_u32(&mut out, face.index);
        put_u32(&mut out, u32::from(face.weight));
        put_u32(&mut out, u32::from(face.stretch));
        out.push(u8::from(face.italic));
        out.push(u8::from(face.monospace));
        put_str(&mut out, &face.family);
    }
    out
}

/// Parse an index. `None` for anything that does not parse exactly.
pub(crate) fn decode(bytes: &[u8]) -> Option<Index> {
    if bytes.len() < MAGIC.len() + 4 || bytes.len() > MAX_BYTES {
        return None;
    }
    let mut cur = Cursor { bytes, at: 0 };
    if cur.take(MAGIC.len())? != MAGIC {
        return None;
    }
    if cur.u32()? != VERSION {
        return None;
    }
    let dir_count = cur.u32()? as usize;
    let mut dirs = Vec::with_capacity(dir_count.min(64));
    for _ in 0..dir_count {
        let path = cur.path()?;
        let secs = cur.u64()?;
        let nanos = cur.u32()?;
        let files = cur.u32()?;
        dirs.push((path, secs, nanos, files));
    }
    let file_count = cur.u32()? as usize;
    let mut files = Vec::with_capacity(file_count.min(4096));
    for _ in 0..file_count {
        let path = cur.path()?;
        let len = cur.u64()?;
        let mtime_secs = cur.u64()?;
        let mtime_nanos = cur.u32()?;
        files.push(FileRecord {
            path,
            len,
            mtime_secs,
            mtime_nanos,
        });
    }
    let face_count = cur.u32()? as usize;
    let mut faces = Vec::with_capacity(face_count.min(4096));
    for _ in 0..face_count {
        let file = cur.u32()?;
        let index = cur.u32()?;
        let weight = u16::try_from(cur.u32()?).ok()?;
        let stretch = u16::try_from(cur.u32()?).ok()?;
        let italic = cur.u8()? != 0;
        let monospace = cur.u8()? != 0;
        let family = cur.string()?;
        if file as usize >= files.len() {
            return None;
        }
        faces.push(FaceRecord {
            file,
            index,
            family,
            weight,
            stretch,
            italic,
            monospace,
        });
    }
    if cur.at != bytes.len() {
        return None;
    }
    Some(Index { dirs, files, faces })
}

/// Write the index, creating the parent directory. Errors are swallowed: a
/// cache that cannot be written is a slower boot, not a failure.
pub(crate) fn store(path: &Path, index: &Index) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // Write-then-rename, so a crash or a second server mid-write cannot leave
    // a half-file that the next boot has to detect as corrupt.
    let tmp = path.with_extension(format!("idx.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, encode(index)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Read and validate the cache against what the directory walk just found.
///
/// `dirs` is the requested directory list with its current stamps, `files` the
/// sorted font files the walk produced. Both must match exactly.
pub(crate) fn load(
    path: &Path,
    dirs: &[(PathBuf, u64, u32, u32)],
    files: &[FileRecord],
) -> Option<Index> {
    let bytes = std::fs::read(path).ok()?;
    let index = decode(&bytes)?;
    if index.dirs != dirs || index.files != files {
        return None;
    }
    Some(index)
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// Paths go out as their lossy UTF-8 form. A font directory whose name is not
/// UTF-8 therefore never validates its cache and is rescanned every boot,
/// which is the right trade against carrying an OS-string encoding here.
fn put_path(out: &mut Vec<u8>, path: &Path) {
    put_str(out, &path.to_string_lossy());
}

/// Bounds-checked reader over the cache bytes.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(u32::from_le_bytes(b))
    }

    fn u64(&mut self) -> Option<u64> {
        let b: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(u64::from_le_bytes(b))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).ok()
    }

    fn path(&mut self) -> Option<PathBuf> {
        self.string().map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, len: u64) -> FileRecord {
        FileRecord {
            path: PathBuf::from(path),
            len,
            mtime_secs: 1_700_000_001,
            mtime_nanos: 7,
        }
    }

    fn sample() -> Index {
        Index {
            dirs: vec![(PathBuf::from("/usr/share/fonts"), 1_700_000_000, 42, 2)],
            files: vec![file("/a/x.ttf", 1024), file("/a/y.ttc", 2048)],
            faces: vec![
                FaceRecord {
                    file: 0,
                    index: 0,
                    family: "DejaVu Sans".into(),
                    weight: 400,
                    stretch: 100,
                    italic: false,
                    monospace: false,
                },
                FaceRecord {
                    file: 1,
                    index: 3,
                    family: "Noto Sans Mono".into(),
                    weight: 700,
                    stretch: 75,
                    italic: true,
                    monospace: true,
                },
            ],
        }
    }

    #[test]
    fn encode_decode_round_trips() {
        let index = sample();
        assert_eq!(decode(&encode(&index)), Some(index));
    }

    #[test]
    fn corruption_is_rejected_rather_than_guessed_at() {
        let good = encode(&sample());
        assert!(decode(&[]).is_none(), "empty");
        assert!(decode(&good[..good.len() - 1]).is_none(), "truncated");
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_none(), "trailing garbage");
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(decode(&bad_magic).is_none(), "magic");
        let mut bad_version = good.clone();
        bad_version[8] = 0xff;
        assert!(decode(&bad_version).is_none(), "version");
        // A face pointing past the end of the file list is rejected: trusting
        // it would index a face whose bytes live nowhere.
        let mut bad_file = Index::clone(&sample());
        bad_file.faces[0].file = 9;
        assert!(decode(&encode(&bad_file)).is_none(), "dangling file index");
        // And every truncation of a good file is a clean `None`, not a panic.
        let full = encode(&sample());
        for cut in 0..full.len() {
            assert!(decode(&full[..cut]).is_none(), "prefix of {cut} bytes");
        }
    }

    #[test]
    fn a_cache_only_loads_for_the_exact_same_walk() {
        let dir = std::env::temp_dir().join(format!("nitro-idx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("fonts.idx");
        let index = sample();
        store(&path, &index);

        assert_eq!(load(&path, &index.dirs, &index.files), Some(index.clone()));

        // A changed directory mtime invalidates.
        let mut dirs = index.dirs.clone();
        dirs[0].1 += 1;
        assert!(load(&path, &dirs, &index.files).is_none());

        // So does a changed file list, even at the same count.
        let files = vec![file("/a/x.ttf", 1024), file("/a/z.ttc", 2048)];
        assert!(load(&path, &index.dirs, &files).is_none());

        // And so does a font replaced in place: same path, different size.
        let resized = vec![file("/a/x.ttf", 1025), index.files[1].clone()];
        assert!(load(&path, &index.dirs, &resized).is_none());

        // And a missing file is simply a miss.
        std::fs::remove_file(&path).ok();
        assert!(load(&path, &index.dirs, &index.files).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
