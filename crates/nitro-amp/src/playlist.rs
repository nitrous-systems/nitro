//! The playlist: an ordered list of files, which one is current, and
//! what "next" means under shuffle and repeat.
//!
//! No I/O happens in [`Playlist`] itself — it is a state machine over
//! paths. [`expand`] is the one function that touches the disk: it turns
//! what the user named (a file, a folder, an `.m3u`, a `.pls`) into the
//! files to add, and the playlist files are parsed by the pure
//! [`parse_m3u`] and [`parse_pls`] beside it.

use std::path::{Path, PathBuf};

/// The file extensions treated as audio when a folder is added.
///
/// Anything `ffmpeg` can decode will *play* if it is added by name; this
/// list only decides what a folder scan picks up, and is deliberately
/// the formats people keep music in rather than everything `ffmpeg`
/// knows (which includes, say, `.txt` as a subtitle format).
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "ape", "flac", "it", "m4a", "mod", "mp2", "mp3", "mpc", "oga", "ogg",
    "opus", "s3m", "wav", "wave", "webm", "wma", "wv", "xm",
];

/// Whether `path` looks like an audio file by its extension.
#[must_use]
pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Whether `path` is a playlist file this module reads.
#[must_use]
pub fn is_playlist(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "m3u" | "m3u8" | "pls"))
}

/// One track in the list.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Where it is.
    pub path: PathBuf,
    /// What to call it: the tags once the track has been played (or a
    /// playlist file said), the file name until then.
    pub title: String,
    /// Length in seconds, once known.
    pub duration: Option<f64>,
}

impl Entry {
    /// An entry titled from its file name.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let title = title_from_path(&path);
        Self {
            path,
            title,
            duration: None,
        }
    }
}

/// A title from a path: the file stem, with underscores as spaces —
/// `02_Some_Song.mp3` reads as `02 Some Song`, which is what a tagless
/// file deserves.
#[must_use]
pub fn title_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        path.display().to_string()
    } else {
        stem.replace('_', " ")
    }
}

/// The playlist.
#[derive(Debug, Clone, Default)]
pub struct Playlist {
    entries: Vec<Entry>,
    /// Index of the current track, if any.
    current: Option<usize>,
    /// Whether "next" follows [`Playlist::order`] rather than the list.
    shuffle: bool,
    /// Whether "next" wraps from the end back to the start.
    repeat: bool,
    /// The shuffled play order: a permutation of the indices, drawn
    /// once when shuffle is switched on or the list changes, so that
    /// shuffle plays every track once before repeating any — what a
    /// listener means by it, and what a fresh random pick per track is
    /// not.
    order: Vec<usize>,
    /// xorshift state for the permutation.
    seed: u64,
}

impl Playlist {
    /// An empty list whose shuffles are drawn from `seed`. A fixed seed
    /// makes the tests' shuffles reproducible; the app seeds from the
    /// clock.
    #[must_use]
    pub fn with_seed(seed: u64) -> Self {
        Self {
            seed: seed | 1,
            ..Self::default()
        }
    }

    /// The entries, in list order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// How many entries there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `i`.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&Entry> {
        self.entries.get(i)
    }

    /// The entry at `i`, to fill in a title or a duration.
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Entry> {
        self.entries.get_mut(i)
    }

    /// The current track's index.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Make `i` current. Out of range clears it.
    pub fn set_current(&mut self, i: usize) {
        self.current = (i < self.entries.len()).then_some(i);
    }

    /// Whether shuffle is on.
    #[must_use]
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// Whether repeat is on.
    #[must_use]
    pub fn repeat(&self) -> bool {
        self.repeat
    }

    /// Switch repeat.
    pub fn set_repeat(&mut self, on: bool) {
        self.repeat = on;
    }

    /// Switch shuffle; switching it on draws a fresh order.
    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        if on {
            self.reshuffle();
        }
    }

    /// Append entries.
    pub fn extend(&mut self, entries: impl IntoIterator<Item = Entry>) {
        self.entries.extend(entries);
        if self.shuffle {
            self.reshuffle();
        }
    }

    /// Remove the entry at `i`, keeping `current` on the same track (or
    /// clearing it, if that was the one removed).
    pub fn remove(&mut self, i: usize) -> Option<Entry> {
        if i >= self.entries.len() {
            return None;
        }
        let e = self.entries.remove(i);
        self.current = match self.current {
            Some(c) if c == i => None,
            Some(c) if c > i => Some(c - 1),
            c => c,
        };
        if self.shuffle {
            self.reshuffle();
        }
        Some(e)
    }

    /// Empty the list.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.current = None;
    }

    /// The track after the current one, or `None` at the end of a list
    /// that does not repeat.
    ///
    /// With nothing current, it is the first track (in play order).
    #[must_use]
    pub fn next(&self) -> Option<usize> {
        self.step(1)
    }

    /// The track before the current one; wraps under repeat.
    #[must_use]
    pub fn prev(&self) -> Option<usize> {
        self.step(-1)
    }

    /// The first track in play order: where Play starts on a list with
    /// nothing current.
    #[must_use]
    pub fn first(&self) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        Some(if self.shuffle && !self.order.is_empty() {
            self.order[0]
        } else {
            0
        })
    }

    fn step(&self, by: isize) -> Option<usize> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        let Some(cur) = self.current else {
            return self.first();
        };
        let (seq, pos): (&[usize], usize) = if self.shuffle && self.order.len() == n {
            let pos = self.order.iter().position(|&i| i == cur).unwrap_or(0);
            (&self.order, pos)
        } else {
            (&[], cur)
        };
        // Both fit an `isize` by a mile: `n` is a `Vec`'s length.
        let (pos, len) = (pos.cast_signed(), n.cast_signed());
        let next = pos + by;
        let wrapped = if (0..len).contains(&next) {
            next.cast_unsigned()
        } else if self.repeat {
            next.rem_euclid(len).cast_unsigned()
        } else {
            return None;
        };
        Some(if seq.is_empty() {
            wrapped
        } else {
            seq[wrapped]
        })
    }

    /// Draw a new play order (Fisher–Yates over xorshift64).
    fn reshuffle(&mut self) {
        let n = self.entries.len();
        self.order = (0..n).collect();
        for i in (1..n).rev() {
            let j = (self.rand() % (i as u64 + 1)) as usize;
            self.order.swap(i, j);
        }
        // Start the new order from the track already playing, so that
        // switching shuffle on does not replay it later in the cycle.
        if let Some(cur) = self.current
            && let Some(p) = self.order.iter().position(|&i| i == cur)
        {
            self.order.swap(0, p);
        }
    }

    fn rand(&mut self) -> u64 {
        if self.seed == 0 {
            self.seed = 0x9E37_79B9_7F4A_7C15;
        }
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed = x;
        x
    }

    /// The list as an extended M3U, with titles and durations for the
    /// entries that have them.
    #[must_use]
    pub fn to_m3u(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::from("#EXTM3U\n");
        for e in &self.entries {
            let secs = e.duration.map_or(-1, |d| d.round() as i64);
            // Writing to a `String` cannot fail.
            let _ = writeln!(out, "#EXTINF:{secs},{}", e.title);
            let _ = writeln!(out, "{}", e.path.to_string_lossy());
        }
        out
    }
}

/// Resolve a playlist line against the playlist's own directory.
///
/// A `file://` URL is unwrapped (with `%20`-style escapes decoded); a
/// relative path is joined to `base`; any other URL scheme is kept as
/// written, because `ffmpeg` streams `http://` itself.
fn resolve(line: &str, base: &Path) -> PathBuf {
    if let Some(rest) = line.strip_prefix("file://") {
        return PathBuf::from(percent_decode(rest));
    }
    if line.contains("://") {
        return PathBuf::from(line);
    }
    let p = Path::new(line);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Decode `%xx` escapes; a malformed escape is kept literally.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Byte-wise, not `&s[i + 1..i + 3]`: a `%` before a multi-byte
        // character would make that slice split a code point and panic.
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One hex digit's value.
fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// Parse an M3U or extended M3U. `#EXTINF:<secs>,<title>` lines name
/// the entry after them; other `#` lines are comments.
#[must_use]
pub fn parse_m3u(text: &str, base: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut pending: Option<(Option<f64>, String)> = None;
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() {
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            let (secs, title) = info.split_once(',').unwrap_or((info, ""));
            let secs = secs
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|s| *s >= 0.0);
            pending = Some((secs, title.trim().to_owned()));
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let mut e = Entry::new(resolve(line, base));
        if let Some((secs, title)) = pending.take() {
            e.duration = secs;
            if !title.is_empty() {
                e.title = title;
            }
        }
        out.push(e);
    }
    out
}

/// Parse a PLS (`[playlist]`, `FileN=`, `TitleN=`, `LengthN=`).
#[must_use]
pub fn parse_pls(text: &str, base: &Path) -> Vec<Entry> {
    /// One `N` of `FileN`/`TitleN`/`LengthN`, gathered in any order.
    #[derive(Default)]
    struct Slot {
        n: u32,
        file: Option<Entry>,
        title: Option<String>,
        length: Option<f64>,
    }
    let mut slots: Vec<Slot> = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        for key in ["file", "title", "length"] {
            let Some(n) = k.strip_prefix(key).and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let i = if let Some(i) = slots.iter().position(|s| s.n == n) {
                i
            } else {
                slots.push(Slot {
                    n,
                    ..Slot::default()
                });
                slots.len() - 1
            };
            let slot = &mut slots[i];
            match key {
                "file" => slot.file = Some(Entry::new(resolve(v, base))),
                "title" => slot.title = Some(v.to_owned()),
                _ => slot.length = v.parse::<f64>().ok().filter(|s| *s >= 0.0),
            }
        }
    }
    slots.sort_by_key(|s| s.n);
    slots
        .into_iter()
        .filter_map(|s| {
            let mut e = s.file?;
            if let Some(t) = s.title.filter(|t| !t.is_empty()) {
                e.title = t;
            }
            e.duration = s.length;
            Some(e)
        })
        .collect()
}

/// What a name the user gave expands to: a folder's audio files
/// (recursively, sorted), a playlist file's entries, or the file itself.
///
/// A path that does not exist is still returned as an entry — it may be
/// a URL, and if it is simply missing, trying to play it reports that
/// more usefully than silently dropping it here would.
#[must_use]
pub fn expand(path: &Path) -> Vec<Entry> {
    if path.is_dir() {
        let mut files = Vec::new();
        walk(path, &mut files, 0);
        files.sort();
        return files.into_iter().map(Entry::new).collect();
    }
    if is_playlist(path)
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let base = path.parent().unwrap_or(Path::new("."));
        let pls = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pls"));
        return if pls {
            parse_pls(&text, base)
        } else {
            parse_m3u(&text, base)
        };
    }
    vec![Entry::new(path.to_path_buf())]
}

/// Collect the audio files under `dir`. Depth-bounded, and symlinked
/// directories are not followed, so a link back up the tree cannot make
/// "add folder" run forever.
fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 16 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            walk(&p, out, depth + 1);
        } else if is_audio(&p) {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: usize) -> Playlist {
        let mut p = Playlist::with_seed(42);
        p.extend((0..n).map(|i| Entry::new(PathBuf::from(format!("/m/{i}.mp3")))));
        p
    }

    #[test]
    fn next_and_prev_walk_the_list_and_stop_at_the_ends() {
        let mut p = list(3);
        assert_eq!(p.next(), Some(0), "nothing current starts at the top");
        p.set_current(0);
        assert_eq!(p.prev(), None);
        assert_eq!(p.next(), Some(1));
        p.set_current(2);
        assert_eq!(p.next(), None);
        p.set_repeat(true);
        assert_eq!(p.next(), Some(0));
        p.set_current(0);
        assert_eq!(p.prev(), Some(2));
    }

    #[test]
    fn shuffle_plays_every_track_once_per_cycle() {
        let mut p = list(20);
        p.set_current(7);
        p.set_shuffle(true);
        let mut seen = vec![7];
        while let Some(n) = p.next() {
            p.set_current(n);
            seen.push(n);
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn removing_keeps_the_current_track_current() {
        let mut p = list(4);
        p.set_current(2);
        p.remove(0);
        assert_eq!(p.current(), Some(1));
        assert_eq!(p.get(1).unwrap().title, "2");
        p.remove(1);
        assert_eq!(p.current(), None);
        assert!(p.remove(9).is_none());
    }

    #[test]
    fn titles_come_from_file_names() {
        assert_eq!(
            title_from_path(Path::new("/a/02_Some_Song.mp3")),
            "02 Some Song"
        );
    }

    #[test]
    fn m3u_round_trips() {
        let mut p = list(2);
        p.get_mut(0).unwrap().title = "Artist - Song".into();
        p.get_mut(0).unwrap().duration = Some(185.4);
        let text = p.to_m3u();
        let back = parse_m3u(&text, Path::new("/"));
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].title, "Artist - Song");
        assert_eq!(back[0].duration, Some(185.0));
        assert_eq!(back[1].duration, None);
        assert_eq!(back[1].path, PathBuf::from("/m/1.mp3"));
    }

    #[test]
    fn m3u_resolves_relative_paths_and_file_urls() {
        let text =
            "\u{feff}#EXTM3U\n\n# a comment\nsub/a.mp3\nfile:///x/b%20c.ogg\nhttp://radio/stream\n";
        let e = parse_m3u(text, Path::new("/lists"));
        assert_eq!(e[0].path, PathBuf::from("/lists/sub/a.mp3"));
        assert_eq!(e[1].path, PathBuf::from("/x/b c.ogg"));
        assert_eq!(e[2].path, PathBuf::from("http://radio/stream"));
    }

    #[test]
    fn pls_is_read_in_number_order() {
        let text = "[playlist]\nFile2=b.mp3\nTitle2=Bee\nFile1=/a.mp3\nLength1=61\nNumberOfEntries=2\nVersion=2\n";
        let e = parse_pls(text, Path::new("/d"));
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].path, PathBuf::from("/a.mp3"));
        assert_eq!(e[0].duration, Some(61.0));
        assert_eq!(e[1].title, "Bee");
        assert_eq!(e[1].path, PathBuf::from("/d/b.mp3"));
    }

    #[test]
    fn a_torn_escape_is_kept_literally() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("%41"), "A");
        assert_eq!(percent_decode("%é1"), "%é1");
    }
}
