//! Font discovery: a flat index of the faces found on disk, and a bounded
//! cache of the file bytes behind them.
//!
//! No fontconfig, no D-Bus. [`FontDb::scan`] walks a list of directories and
//! records one [`Face`] per face in each `.ttf`/`.otf`/`.ttc`/`.otc` it finds —
//! **the family name, the attributes and the (path, index) pair, not the
//! bytes**. The bytes are read on first use by [`FontDb::face`], kept in an
//! LRU capped by `NITRO_FONT_CACHE_MB` (default 8), and evicted when the cap
//! is exceeded. A desktop uses two or three faces of the forty-odd installed,
//! so the resident cost is the two or three, not the forty-odd; see the README
//! for the measurement that motivated this.
//!
//! The index itself is cached on disk (see [`index`](crate::index)) so a
//! second boot does not have to re-read every font file to rebuild it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use swash::{FontDataRef, FontRef, StringId};

use crate::index::{self, FaceRecord};

/// Index of a font face in a [`FontDb`].
///
/// Stable for the lifetime of the db: the index is built once at startup and
/// never mutated (loading a face's bytes does not change it), so an id can be
/// put on the wire and handed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FontId(pub u32);

/// A font family request: one of the three generic aliases, or a name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Family {
    /// The generic sans-serif alias.
    #[default]
    Sans,
    /// The generic serif alias.
    Serif,
    /// The generic monospace alias.
    Mono,
    /// A concrete family name, matched case-insensitively.
    Named(String),
}

impl Family {
    /// Parse a CSS-ish family string.
    ///
    /// `"sans"`/`"sans-serif"` → [`Family::Sans`], `"mono"`/`"monospace"` →
    /// [`Family::Mono`], `"serif"` → [`Family::Serif`]; anything else is a
    /// [`Family::Named`] holding the string verbatim (matching lowercases it).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "sans" | "sans-serif" => Self::Sans,
            "mono" | "monospace" => Self::Mono,
            "serif" => Self::Serif,
            _ => Self::Named(s.trim().to_string()),
        }
    }
}

/// What to shape with: family, size and the two attributes we select on.
#[derive(Debug, Clone, PartialEq)]
pub struct TextStyle {
    /// Requested family.
    pub family: Family,
    /// Em size in device pixels.
    pub size_px: f32,
    /// CSS weight, 100..=900 (400 normal, 700 bold).
    pub weight: u16,
    /// Italic or oblique requested.
    pub italic: bool,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self {
            family: Family::Sans,
            size_px: 14.0,
            weight: 400,
            italic: false,
        }
    }
}

/// One face: a (file, index) pair plus the attributes we select on.
#[derive(Debug)]
struct Face {
    /// Index into `FontDb::files`.
    file: u32,
    /// Face index within the file (0 for a plain `.ttf`, 0..n for a `.ttc`).
    index: u32,
    /// Family name as found in the `name` table (original case).
    family: String,
    /// Lowercased family name, the index key.
    family_lower: String,
    weight: u16,
    /// swash's raw stretch value: 100 is normal, 50 ultra-condensed, 300
    /// ultra-expanded (one unit = half a percent of the normal aspect ratio).
    stretch: u16,
    italic: bool,
}

/// What the index knows about one family: its faces and its class.
///
/// The class is computed once at scan time because it needs the faces: a
/// family is monospace if its *name* says so **or** any of its faces sets
/// `post.isFixedPitch`, which catches the ones named "Terminus" or "Fixed".
#[derive(Debug, Default)]
struct FamilyInfo {
    /// Face ids, in discovery order.
    faces: Vec<u32>,
    mono: bool,
    serif: bool,
}

/// Generic alias preference lists, first match wins.
const SANS_PREFS: &[&str] = &[
    "noto sans",
    "dejavu sans",
    "cantarell",
    "liberation sans",
    "droid sans",
];
const MONO_PREFS: &[&str] = &[
    "noto sans mono",
    "dejavu sans mono",
    "liberation mono",
    "droid sans mono",
];
const SERIF_PREFS: &[&str] = &[
    "noto serif",
    "dejavu serif",
    "liberation serif",
    "droid serif",
];

/// Default byte cap of the face cache, overridden by `NITRO_FONT_CACHE_MB`.
const DEFAULT_CACHE_MB: f64 = 8.0;

/// How many frame stamps a face may go untouched before
/// [`release_idle`](FontDb::release_idle) drops it, whatever the cap says.
///
/// One. A face is needed to *shape* a run and to *rasterize* a glyph the atlas
/// has not seen; once a label is on screen neither happens again, so holding
/// the file is holding megabytes against a re-read that costs tens of
/// microseconds (measured: 20 µs to shape a warm line, 53 µs when the face has
/// to be read back first — 0.2 % of a 16 ms frame, and only when a *new* glyph
/// appears). The cap still governs the working set *within* a frame, which is
/// what stops a run alternating between faces thrashing.
const IDLE_FRAMES: u64 = 1;

/// One font file's bytes plus the face index inside them.
///
/// Cloning is an `Arc` bump: several faces of one `.ttc` share the bytes, and
/// a face that is evicted while a caller still holds a `FaceData` stays valid
/// until that caller drops it — eviction can never pull the rug out from under
/// a shaper mid-call.
#[derive(Debug, Clone)]
pub struct FaceData {
    bytes: Arc<Vec<u8>>,
    index: u32,
}

impl FaceData {
    /// The file's bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Face index within the file, for `FontRef::from_index`.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.index
    }

    /// A `swash::FontRef` over these bytes, or `None` if the file has changed
    /// under us since the index was built.
    #[must_use]
    pub fn font_ref(&self) -> Option<FontRef<'_>> {
        FontRef::from_index(self.bytes.as_slice(), self.index as usize)
    }
}

/// One cached file: its bytes and the frame it was last handed out in.
#[derive(Debug)]
struct CacheEntry {
    bytes: Arc<Vec<u8>>,
    last_used: u64,
}

/// The bounded LRU of font file bytes.
#[derive(Debug)]
struct Cache {
    entries: HashMap<u32, CacheEntry>,
    /// Sum of the cached files' sizes.
    bytes: usize,
    /// Byte cap. Soft in exactly one way: the file that triggered the trim is
    /// never the victim, so a cap smaller than a single font file degrades to
    /// "cache nothing" rather than to "read the file and immediately drop it".
    limit: usize,
    /// Frame stamp, bumped by [`FontDb::next_frame`].
    frame: u64,
    /// Files read from disk since startup (the miss counter).
    loads: u64,
    /// Files dropped by the cap.
    evictions: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            limit: env_cache_bytes(),
            frame: 0,
            loads: 0,
            evictions: 0,
        }
    }
}

/// Cache cap from `NITRO_FONT_CACHE_MB`, in bytes.
///
/// Unset, unparseable or negative means [`DEFAULT_CACHE_MB`]; the value is
/// clamped to 1 GB so a typo cannot ask for an allocation the box cannot back.
fn env_cache_bytes() -> usize {
    let mb = std::env::var("NITRO_FONT_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(DEFAULT_CACHE_MB);
    (mb.clamp(0.0, 1024.0) * 1024.0 * 1024.0) as usize
}

/// The font index.
///
/// Built once at startup and never mutated. Holds **no font bytes** until a
/// face is actually used: [`face`](FontDb::face) reads the file on first use
/// and the cache keeps it until the cap says otherwise.
#[derive(Debug, Default)]
pub struct FontDb {
    /// Font file paths, indexed by `Face::file`.
    files: Vec<PathBuf>,
    faces: Vec<Face>,
    /// Lowercased family name → its faces and class.
    by_family: HashMap<String, FamilyInfo>,
    scan_time: Duration,
    /// Whether the index came off the on-disk cache rather than from reading
    /// every font file.
    from_cache: bool,
    cache: RefCell<Cache>,
}

impl FontDb {
    /// Scan the directories in `NITRO_FONT_DIRS` (colon-separated), or the
    /// default `/usr/share/fonts:/usr/local/share/fonts:~/.local/share/fonts`,
    /// using the on-disk index cache.
    #[must_use]
    pub fn scan() -> Self {
        let dirs: Vec<PathBuf> = if let Ok(v) = std::env::var("NITRO_FONT_DIRS") {
            v.split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect()
        } else {
            let mut dirs = vec![
                PathBuf::from("/usr/share/fonts"),
                PathBuf::from("/usr/local/share/fonts"),
            ];
            if let Ok(home) = std::env::var("HOME") {
                dirs.push(Path::new(&home).join(".local/share/fonts"));
            }
            dirs
        };
        let cache = index::cache_path();
        Self::scan_dirs_with_cache(&dirs, cache.as_deref())
    }

    /// Scan an explicit list of directories, recursively, **without** the
    /// on-disk index cache.
    ///
    /// That is what the tests want: a scan of a temp directory must not write
    /// over the index the user's own session built. Use
    /// [`scan_dirs_with_cache`](Self::scan_dirs_with_cache) to opt in.
    ///
    /// Unreadable directories and files that swash rejects are skipped
    /// silently: the crate has no logger, and a broken font on the box must
    /// not stop the server booting. [`len`](Self::len) and
    /// [`scan_time`](Self::scan_time) are what the server logs instead.
    #[must_use]
    pub fn scan_dirs<P: AsRef<Path>>(dirs: &[P]) -> Self {
        Self::scan_dirs_with_cache(dirs, None)
    }

    /// Scan an explicit list of directories, reading and refreshing the index
    /// cache at `cache` when one is given.
    ///
    /// The cache is validated against the directory walk that just happened:
    /// the same directories with the same mtimes and file counts, and the same
    /// font files in the same order **with the same sizes and mtimes**. Any
    /// mismatch discards it, so a font installed, removed or replaced in place
    /// between boots is picked up. A cache that does not parse is discarded
    /// too: the scan is a few milliseconds, and a wrong index would have the
    /// server shaping with a face that is not there.
    #[must_use]
    pub fn scan_dirs_with_cache<P: AsRef<Path>>(dirs: &[P], cache: Option<&Path>) -> Self {
        let start = Instant::now();
        let mut stamps = Vec::with_capacity(dirs.len());
        let mut paths: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            let dir = dir.as_ref();
            let mut found = Vec::new();
            collect_font_files(dir, 0, &mut found);
            let (secs, nanos) = index::dir_stamp(dir);
            stamps.push((dir.to_path_buf(), secs, nanos, found.len() as u32));
            paths.append(&mut found);
        }
        paths.sort();
        paths.dedup();
        let files: Vec<index::FileRecord> = paths.iter().map(|p| index::file_record(p)).collect();

        let cached = cache.and_then(|path| index::load(path, &stamps, &files));
        let from_cache = cached.is_some();
        let idx = cached.unwrap_or_else(|| build_index(stamps, files));
        if !from_cache && let Some(path) = cache {
            index::store(path, &idx);
        }

        let mut db = Self {
            from_cache,
            ..Self::default()
        };
        db.ingest(idx);
        db.scan_time = start.elapsed();
        db
    }

    /// Turn a parsed index into the lookup tables.
    fn ingest(&mut self, idx: index::Index) {
        self.files = idx.files.into_iter().map(|f| f.path).collect();
        for record in idx.faces {
            let FaceRecord {
                file,
                index,
                family,
                weight,
                stretch,
                italic,
                monospace,
            } = record;
            let family_lower = family.to_ascii_lowercase();
            let id = self.faces.len() as u32;
            let info = self.by_family.entry(family_lower.clone()).or_default();
            info.faces.push(id);
            info.mono |= monospace || family_lower.contains("mono");
            self.faces.push(Face {
                file,
                index,
                family,
                family_lower,
                weight,
                stretch,
                italic,
            });
        }
        for (name, info) in &mut self.by_family {
            info.serif = !info.mono && name.contains("serif") && !name.contains("sans");
        }
    }

    /// True when nothing was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// Number of faces indexed (a `.ttc` contributes several).
    #[must_use]
    pub fn len(&self) -> usize {
        self.faces.len()
    }

    /// How long the scan took, for the server's startup log.
    #[must_use]
    pub fn scan_time(&self) -> Duration {
        self.scan_time
    }

    /// Whether the index came off the on-disk cache instead of a full read of
    /// every font file. The server logs it; the README quotes both timings.
    #[must_use]
    pub fn used_index_cache(&self) -> bool {
        self.from_cache
    }

    /// Best face for a style, or `None` when the db is empty.
    ///
    /// The family is resolved first (see [`Self::fallbacks`] for the alias
    /// tables), then the face within it whose attributes are closest to the
    /// request: slant dominates, then width (a normal-width face beats a
    /// condensed one), then the CSS-ish nearest-weight rule — at or above the
    /// request first when the request is >= 400, below it first when it is
    /// < 400, ties going to the smaller distance.
    #[must_use]
    pub fn select(&self, style: &TextStyle) -> Option<FontId> {
        let family = self.resolve_family(&style.family)?;
        self.pick_in_family(family, style)
    }

    /// The fallback chain for a style.
    ///
    /// [`select`](Self::select) first, then one face from every other family in
    /// the same alias list, in alias order. Used per run when the selected face
    /// maps a character to `.notdef`.
    ///
    /// This is a list of *ids*: nothing is read off disk by building it, which
    /// is what lets the shaper load the primary face only and touch the rest
    /// of the chain solely when a character needs it.
    #[must_use]
    pub fn fallbacks(&self, style: &TextStyle) -> Vec<FontId> {
        let mut out = Vec::new();
        if let Some(id) = self.select(style) {
            out.push(id);
        }
        let generic = match &style.family {
            Family::Named(_) | Family::Sans => Family::Sans,
            Family::Mono => Family::Mono,
            Family::Serif => Family::Serif,
        };
        let mut seen: Vec<&str> = Vec::new();
        if let Some(first) = out.first() {
            seen.push(&self.faces[first.0 as usize].family_lower);
        }
        for family in self.alias_families(&generic) {
            if seen.contains(&family.as_str()) {
                continue;
            }
            if let Some(id) = self.pick_in_family(&family, style) {
                out.push(id);
                seen.push(&self.faces[id.0 as usize].family_lower);
            }
        }
        out
    }

    /// Family name of a face, as spelled in the font.
    #[must_use]
    pub fn family_name(&self, font: FontId) -> Option<&str> {
        self.faces.get(font.0 as usize).map(|f| f.family.as_str())
    }

    /// Path of the file a face lives in.
    #[must_use]
    pub fn face_path(&self, font: FontId) -> Option<&Path> {
        let face = self.faces.get(font.0 as usize)?;
        self.files.get(face.file as usize).map(PathBuf::as_path)
    }

    /// The bytes of a face, reading the file on first use.
    ///
    /// `None` when the id is unknown or the file cannot be read. The returned
    /// [`FaceData`] holds its own reference to the bytes, so it stays valid
    /// even if the cache evicts the file in the meantime.
    #[must_use]
    pub fn face(&self, font: FontId) -> Option<FaceData> {
        let face = self.faces.get(font.0 as usize)?;
        let bytes = self.load(face.file)?;
        Some(FaceData {
            bytes,
            index: face.index,
        })
    }

    /// Load one file, or hand back the cached bytes, refreshing its stamp.
    fn load(&self, file: u32) -> Option<Arc<Vec<u8>>> {
        let mut cache = self.cache.borrow_mut();
        let frame = cache.frame;
        if let Some(entry) = cache.entries.get_mut(&file) {
            entry.last_used = frame;
            return Some(Arc::clone(&entry.bytes));
        }
        let path = self.files.get(file as usize)?;
        let bytes = Arc::new(std::fs::read(path).ok()?);
        cache.loads += 1;
        cache.bytes += bytes.len();
        cache.entries.insert(
            file,
            CacheEntry {
                bytes: Arc::clone(&bytes),
                last_used: frame,
            },
        );
        cache.trim(Some(file));
        Some(bytes)
    }

    /// Bump the frame stamp the face LRU records.
    ///
    /// The server calls this once per painted frame, next to
    /// [`Atlas::next_frame`](crate::Atlas::next_frame). Bumping does not
    /// evict; [`release_idle`](Self::release_idle) does.
    pub fn next_frame(&self) {
        self.cache.borrow_mut().frame += 1;
    }

    /// Drop every face untouched for [`IDLE_FRAMES`] frames, **regardless of
    /// the cap**.
    ///
    /// This, not the cap, is what actually keeps the server's resident set
    /// small: on a box whose fonts fit inside `NITRO_FONT_CACHE_MB` the cap
    /// never fires at all, and a desktop that has finished drawing its labels
    /// would hold every font file it ever touched for the rest of the session.
    /// The atlas keeps the rendered masks, so the only cost of being wrong is
    /// one `read(2)` the next time a genuinely new glyph turns up, and a face
    /// a caller is still using is untouched by this — [`FaceData`] owns an
    /// `Arc` to the bytes.
    ///
    /// Called from the server's event loop at the end of a turn, because a
    /// settled desktop stops painting: hanging the release off
    /// [`next_frame`](Self::next_frame) alone would never fire in exactly the
    /// state the memory budget is measured in.
    pub fn release_idle(&self) {
        self.cache.borrow_mut().sweep_idle();
    }

    /// Font files resident in the cache right now.
    #[must_use]
    pub fn loaded_files(&self) -> usize {
        self.cache.borrow().entries.len()
    }

    /// Bytes of font data resident right now — the number the memory budget
    /// cares about. Zero immediately after a scan.
    #[must_use]
    pub fn loaded_bytes(&self) -> usize {
        self.cache.borrow().bytes
    }

    /// Files read off disk since startup, cache misses included: the load
    /// counter. It stops rising once the desktop's two or three faces are in.
    #[must_use]
    pub fn loads(&self) -> u64 {
        self.cache.borrow().loads
    }

    /// Files dropped by the cap since startup.
    #[must_use]
    pub fn evictions(&self) -> u64 {
        self.cache.borrow().evictions
    }

    /// The cache's byte cap.
    #[must_use]
    pub fn cache_limit(&self) -> usize {
        self.cache.borrow().limit
    }

    /// Set the cache's byte cap and trim to it immediately.
    ///
    /// The server does not call this — `NITRO_FONT_CACHE_MB` is the knob — but
    /// a test that wants to watch eviction happen needs a cap smaller than one
    /// font file, which no sane environment variable would express.
    pub fn set_cache_limit(&self, bytes: usize) {
        let mut cache = self.cache.borrow_mut();
        cache.limit = bytes;
        cache.trim(None);
    }

    /// Resolve a requested family to a lowercased family name present in the
    /// db, applying the generic alias tables.
    fn resolve_family(&self, family: &Family) -> Option<&str> {
        if let Family::Named(name) = family {
            let lower = name.to_ascii_lowercase();
            if let Some((key, _)) = self.by_family.get_key_value(&lower) {
                return Some(key.as_str());
            }
            // Unknown name: fall through to sans, like a browser would.
            return self.alias_first(&Family::Sans);
        }
        self.alias_first(family)
    }

    /// Whether a family present in the db fits a generic class.
    fn family_matches(&self, name: &str, generic: &Family) -> bool {
        let Some(info) = self.by_family.get(name) else {
            return false;
        };
        match generic {
            Family::Mono => info.mono,
            Family::Serif => info.serif,
            Family::Sans | Family::Named(_) => !info.mono && !info.serif,
        }
    }

    /// First family matching a generic alias.
    fn alias_first(&self, generic: &Family) -> Option<&str> {
        let prefs = prefs_for(generic);
        for pref in prefs {
            if let Some((key, _)) = self.by_family.get_key_value(*pref) {
                return Some(key.as_str());
            }
        }
        // No preferred family present: any family whose name fits the class,
        // then — so the db is never useless — any family at all.
        let mut any: Option<&str> = None;
        let mut best: Option<&str> = None;
        for name in self.by_family.keys().map(String::as_str) {
            if self.family_matches(name, generic) && best.is_none_or(|b| name < b) {
                best = Some(name);
            }
            if any.is_none_or(|a| name < a) {
                any = Some(name);
            }
        }
        best.or(any)
    }

    /// Every family in a generic alias list that is present, alias order first
    /// then the class-matching families in name order.
    fn alias_families(&self, generic: &Family) -> Vec<String> {
        let prefs = prefs_for(generic);
        let mut out: Vec<String> = Vec::new();
        for pref in prefs {
            if self.by_family.contains_key(*pref) {
                out.push((*pref).to_string());
            }
        }
        let mut rest: Vec<&str> = self
            .by_family
            .keys()
            .map(String::as_str)
            .filter(|name| self.family_matches(name, generic) && !out.iter().any(|o| o == name))
            .collect();
        rest.sort_unstable();
        out.extend(rest.into_iter().map(ToString::to_string));
        out
    }

    /// Nearest face within one family.
    fn pick_in_family(&self, family_lower: &str, style: &TextStyle) -> Option<FontId> {
        let ids = &self.by_family.get(family_lower)?.faces;
        let mut best: Option<(u32, FontId)> = None;
        for id in ids {
            let face = &self.faces[*id as usize];
            let score = attr_distance(face, style);
            if best.is_none_or(|(b, _)| score < b) {
                best = Some((score, FontId(*id)));
            }
        }
        best.map(|(_, id)| id)
    }
}

impl Cache {
    /// Drop every face untouched for [`IDLE_FRAMES`] frames.
    fn sweep_idle(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let frame = self.frame;
        let mut freed = 0u64;
        self.entries.retain(|_, entry| {
            let idle = frame.saturating_sub(entry.last_used) > IDLE_FRAMES;
            if idle {
                freed += 1;
            }
            !idle
        });
        if freed > 0 {
            self.bytes = self.entries.values().map(|e| e.bytes.len()).sum();
            self.evictions += freed;
        }
    }

    /// Drop least-recently-used files until the cap is met.
    ///
    /// Evicting a face that a caller is *using* is safe and deliberately not
    /// guarded against: a [`FaceData`] owns an `Arc` to the bytes, so they stay
    /// alive exactly as long as somebody is shaping or scaling with them. The
    /// atlas keeps the rendered masks, so an eviction costs a re-read on the
    /// next *new* glyph of that face and never a redraw or a missing glyph.
    ///
    /// `protect` is the file that triggered this trim, if any: dropping it
    /// immediately would turn a too-small cap into one disk read per glyph.
    fn trim(&mut self, protect: Option<u32>) {
        if self.bytes <= self.limit {
            return;
        }
        let mut victims: Vec<(u64, u32)> = self
            .entries
            .iter()
            .filter(|(file, _)| Some(**file) != protect)
            .map(|(file, e)| (e.last_used, *file))
            .collect();
        // Oldest first; the file index breaks ties so eviction is deterministic.
        victims.sort_unstable();
        for (_, file) in victims {
            if self.bytes <= self.limit {
                break;
            }
            if let Some(entry) = self.entries.remove(&file) {
                self.bytes = self.bytes.saturating_sub(entry.bytes.len());
                self.evictions += 1;
            }
        }
    }
}

/// The preference list for a generic alias.
fn prefs_for(generic: &Family) -> &'static [&'static str] {
    match generic {
        Family::Mono => MONO_PREFS,
        Family::Serif => SERIF_PREFS,
        Family::Sans | Family::Named(_) => SANS_PREFS,
    }
}

/// Read every font file in `files` and build the index: family names and
/// attributes, no bytes.
///
/// This is the slow path the on-disk cache exists to skip — the only place
/// that reads every font on the box, and it holds no bytes past the loop.
fn build_index(dirs: Vec<(PathBuf, u64, u32, u32)>, files: Vec<index::FileRecord>) -> index::Index {
    let mut faces = Vec::new();
    for (file, record) in files.iter().enumerate() {
        let Ok(data) = std::fs::read(&record.path) else {
            continue;
        };
        let Some(font_data) = FontDataRef::new(&data) else {
            continue;
        };
        for i in 0..font_data.len() {
            let Some(font) = font_data.get(i) else {
                continue;
            };
            let Some(family) = family_name(&font) else {
                continue;
            };
            let (stretch, weight, style) = font.attributes().parts();
            faces.push(FaceRecord {
                file: file as u32,
                index: i as u32,
                family,
                weight: weight.0,
                stretch: stretch.raw(),
                italic: !matches!(style, swash::Style::Normal),
                monospace: font.metrics(&[]).is_monospace,
            });
        }
    }
    index::Index { dirs, files, faces }
}

/// Normal width, in swash's raw stretch units.
const NORMAL_STRETCH: u16 = 100;
/// A wrong slant costs more than anything else.
const SLANT_PENALTY: u32 = 1_000_000;
/// One raw unit of width mismatch (half a percent) costs more than any
/// possible weight distance, so width is decided before weight.
const STRETCH_PENALTY: u32 = 2_000;
/// Preferring the wrong side of the requested weight costs more than any
/// weight delta (which is at most 900).
const WRONG_SIDE_PENALTY: u32 = 1_000;

/// Distance between a face's attributes and a request.
///
/// Slant dominates (a wrong slant costs more than any other mismatch), then
/// width — nothing asks for a condensed face, so the closest to normal wins —
/// then the CSS font-matching weight rule: when the request is >= 400 prefer
/// heavier faces, when it is < 400 prefer lighter ones, and the closer of the
/// preferred side always beats the other side.
fn attr_distance(face: &Face, want: &TextStyle) -> u32 {
    let slant = u32::from(face.italic != want.italic) * SLANT_PENALTY;
    let stretch = u32::from(face.stretch.abs_diff(NORMAL_STRETCH)) * STRETCH_PENALTY;
    let delta = u32::from(face.weight.abs_diff(want.weight));
    let wrong_side = if want.weight >= 400 {
        face.weight < want.weight
    } else {
        face.weight > want.weight
    };
    slant + stretch + u32::from(wrong_side) * WRONG_SIDE_PENALTY + delta
}

/// Family name of a face: typographic family when present, else the legacy one.
fn family_name(font: &FontRef<'_>) -> Option<String> {
    let strings = font.localized_strings();
    for id in [StringId::TypographicFamily, StringId::Family] {
        if let Some(s) = strings.find_by_id(id, None) {
            let name: String = s.chars().collect();
            let name = name.trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Recursively collect font files under `dir`.
///
/// Depth is capped so a symlink loop cannot hang the scan.
fn collect_font_files(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    const MAX_DEPTH: u32 = 8;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_font_files(&path, depth + 1, out);
        } else if is_font_path(&path) {
            out.push(path);
        }
    }
}

/// `.ttf`/`.otf`/`.ttc`/`.otc`, case-insensitive.
fn is_font_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| {
        let ext = ext.to_string_lossy().to_ascii_lowercase();
        matches!(ext.as_str(), "ttf" | "otf" | "ttc" | "otc")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn face(weight: u16, italic: bool, stretch: u16) -> Face {
        Face {
            file: 0,
            index: 0,
            family: "X".into(),
            family_lower: "x".into(),
            weight,
            stretch,
            italic,
        }
    }

    fn want(weight: u16, italic: bool) -> TextStyle {
        TextStyle {
            weight,
            italic,
            ..TextStyle::default()
        }
    }

    #[test]
    fn family_parse_maps_generics() {
        assert_eq!(Family::parse("sans"), Family::Sans);
        assert_eq!(Family::parse("sans-serif"), Family::Sans);
        assert_eq!(Family::parse("SANS-SERIF"), Family::Sans);
        assert_eq!(Family::parse("mono"), Family::Mono);
        assert_eq!(Family::parse("monospace"), Family::Mono);
        assert_eq!(Family::parse("serif"), Family::Serif);
        assert_eq!(
            Family::parse("DejaVu Sans"),
            Family::Named("DejaVu Sans".to_string())
        );
    }

    #[test]
    fn default_style_is_sans_14() {
        let style = TextStyle::default();
        assert_eq!(style.family, Family::Sans);
        assert!((style.size_px - 14.0).abs() < f32::EPSILON);
        assert_eq!(style.weight, 400);
        assert!(!style.italic);
    }

    #[test]
    fn attribute_distance_orders_slant_then_width_then_weight() {
        // Wrong slant loses to any other mismatch.
        assert!(
            attr_distance(&face(400, true, 100), &want(400, false))
                > attr_distance(&face(900, false, 50), &want(400, false))
        );
        // A normal-width face beats a condensed one of the same weight, and
        // width outranks any weight difference.
        assert!(
            attr_distance(&face(900, false, 100), &want(400, false))
                < attr_distance(&face(400, false, 75), &want(400, false))
        );
        // At or above the request wins when the request is >= 400.
        assert!(
            attr_distance(&face(700, false, 100), &want(600, false))
                < attr_distance(&face(500, false, 100), &want(600, false))
        );
        // Below the request wins when the request is < 400.
        assert!(
            attr_distance(&face(200, false, 100), &want(300, false))
                < attr_distance(&face(400, false, 100), &want(300, false))
        );
    }

    #[test]
    fn font_extensions_are_case_insensitive() {
        assert!(is_font_path(Path::new("/x/A.TTF")));
        assert!(is_font_path(Path::new("/x/a.otc")));
        assert!(!is_font_path(Path::new("/x/a.pfb")));
    }

    #[test]
    fn the_cache_evicts_oldest_first_and_spares_the_newcomer() {
        let mut cache = Cache {
            limit: 100,
            ..Cache::default()
        };
        for (file, last_used) in [(0u32, 0u64), (1, 1), (2, 2)] {
            cache.entries.insert(
                file,
                CacheEntry {
                    bytes: Arc::new(vec![0; 50]),
                    last_used,
                },
            );
            cache.bytes += 50;
        }
        cache.frame = 2;
        cache.trim(Some(2));
        assert_eq!(cache.bytes, 100);
        assert!(!cache.entries.contains_key(&0), "oldest goes first");
        assert!(cache.entries.contains_key(&2), "the newcomer stays");
        assert_eq!(cache.evictions, 1);

        // A cap below one file's size keeps only the file just read, so the
        // caller that asked for it is not immediately made to read it again.
        cache.limit = 0;
        cache.trim(Some(2));
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key(&2));
        // Without a newcomer to protect, a zero cap empties the cache.
        cache.trim(None);
        assert!(cache.entries.is_empty());
        assert_eq!(cache.bytes, 0);
    }
}
