//! Font discovery: a flat index of the faces found on disk.
//!
//! No fontconfig, no D-Bus, no cache file. [`FontDb::scan`] walks a list of
//! directories, reads every `.ttf`/`.otf`/`.ttc`/`.otc` it finds, and records
//! one [`Face`] per face in each file. The file bytes are kept in memory for
//! the lifetime of the db because the shaper and the scaler both need them on
//! every call; see the README for the cost.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use swash::{FontDataRef, FontRef, StringId};

/// Index of a font face in a [`FontDb`].
///
/// Stable for the lifetime of the db: the db is built once at startup and
/// never mutated, so an id can be put on the wire and handed back.
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
    italic: bool,
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

/// The font index.
///
/// Built once at startup, never mutated. Holds the bytes of every font file it
/// indexed, so [`face_data`](FontDb::face_data) can hand a `&[u8]` to swash
/// without re-reading the file.
#[derive(Debug, Default)]
pub struct FontDb {
    /// Raw file contents, indexed by `Face::file`.
    files: Vec<Vec<u8>>,
    faces: Vec<Face>,
    /// Lowercased family name → face indices, in discovery order.
    by_family: HashMap<String, Vec<u32>>,
    scan_time: Duration,
}

impl FontDb {
    /// Scan the directories in `NITRO_FONT_DIRS` (colon-separated), or the
    /// default `/usr/share/fonts:/usr/local/share/fonts:~/.local/share/fonts`.
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
        Self::scan_dirs(&dirs)
    }

    /// Scan an explicit list of directories, recursively.
    ///
    /// Unreadable directories and files that swash rejects are skipped
    /// silently: the crate has no logger, and a broken font on the box must
    /// not stop the server booting. [`len`](Self::len) and
    /// [`scan_time`](Self::scan_time) are what the server logs instead.
    #[must_use]
    pub fn scan_dirs<P: AsRef<Path>>(dirs: &[P]) -> Self {
        let start = Instant::now();
        let mut db = Self::default();
        let mut paths = Vec::new();
        for dir in dirs {
            collect_font_files(dir.as_ref(), 0, &mut paths);
        }
        paths.sort();
        for path in paths {
            db.add_file(&path);
        }
        db.scan_time = start.elapsed();
        db
    }

    /// Read one font file and index every face in it.
    fn add_file(&mut self, path: &Path) {
        let Ok(data) = std::fs::read(path) else {
            return;
        };
        let Some(font_data) = FontDataRef::new(&data) else {
            return;
        };
        let file = self.files.len() as u32;
        let mut added = false;
        for index in 0..font_data.len() {
            let Some(font) = font_data.get(index) else {
                continue;
            };
            let Some(family) = family_name(&font) else {
                continue;
            };
            let attrs = font.attributes();
            let (_, weight, style) = attrs.parts();
            let family_lower = family.to_ascii_lowercase();
            let id = self.faces.len() as u32;
            self.faces.push(Face {
                file,
                index: index as u32,
                family,
                family_lower: family_lower.clone(),
                weight: weight.0,
                italic: !matches!(style, swash::Style::Normal),
            });
            self.by_family.entry(family_lower).or_default().push(id);
            added = true;
        }
        if added {
            self.files.push(data);
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

    /// Best face for a style, or `None` when the db is empty.
    ///
    /// The family is resolved first (see [`Self::fallbacks`] for the alias
    /// tables), then the face within it whose `(weight, italic)` is closest to
    /// the request: italic matches are preferred over upright ones when italic
    /// was asked for (and vice versa), and within that group the CSS-ish
    /// nearest-weight rule applies — at or above the request first when the
    /// request is >= 400, below it first when it is < 400, ties going to the
    /// smaller distance.
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

    /// Raw font bytes plus the face index inside them, so a caller can build a
    /// `swash::FontRef` with `FontRef::from_index(data, index as usize)`.
    #[must_use]
    pub fn face_data(&self, font: FontId) -> Option<(&[u8], u32)> {
        let face = self.faces.get(font.0 as usize)?;
        let data = self.files.get(face.file as usize)?;
        Some((data.as_slice(), face.index))
    }

    /// Build a `FontRef` for a face.
    pub(crate) fn font_ref(&self, font: FontId) -> Option<FontRef<'_>> {
        let (data, index) = self.face_data(font)?;
        FontRef::from_index(data, index as usize)
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

    /// First family matching a generic alias.
    fn alias_first(&self, generic: &Family) -> Option<&str> {
        let (prefs, matches): (&[&str], fn(&str) -> bool) = match generic {
            Family::Mono => (MONO_PREFS, is_mono_name),
            Family::Serif => (SERIF_PREFS, is_serif_name),
            Family::Sans | Family::Named(_) => (SANS_PREFS, is_sans_name),
        };
        for pref in prefs {
            if let Some((key, _)) = self.by_family.get_key_value(*pref) {
                return Some(key.as_str());
            }
        }
        // No preferred family present: any family whose name fits the class,
        // then — so the db is never useless — any family at all.
        let mut any: Option<&str> = None;
        let mut best: Option<&str> = None;
        for face in &self.faces {
            let name = face.family_lower.as_str();
            if matches(name) && best.is_none_or(|b| name < b) {
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
        let (prefs, matches): (&[&str], fn(&str) -> bool) = match generic {
            Family::Mono => (MONO_PREFS, is_mono_name),
            Family::Serif => (SERIF_PREFS, is_serif_name),
            Family::Sans | Family::Named(_) => (SANS_PREFS, is_sans_name),
        };
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
            .filter(|name| matches(name) && !out.iter().any(|o| o == name))
            .collect();
        rest.sort_unstable();
        out.extend(rest.into_iter().map(ToString::to_string));
        out
    }

    /// Nearest `(weight, italic)` face within one family.
    fn pick_in_family(&self, family_lower: &str, style: &TextStyle) -> Option<FontId> {
        let ids = self.by_family.get(family_lower)?;
        let mut best: Option<(u32, FontId)> = None;
        for id in ids {
            let face = &self.faces[*id as usize];
            let score = attr_distance(face.weight, face.italic, style.weight, style.italic);
            if best.is_none_or(|(b, _)| score < b) {
                best = Some((score, FontId(*id)));
            }
        }
        best.map(|(_, id)| id)
    }
}

/// Distance between a face's attributes and a request.
///
/// Slant dominates (a wrong slant costs more than any weight mismatch), then
/// the CSS font-matching weight rule: when the request is >= 400 prefer heavier
/// faces, when it is < 400 prefer lighter ones, and the closer of the preferred
/// side always beats the other side.
fn attr_distance(face_weight: u16, face_italic: bool, want_weight: u16, want_italic: bool) -> u32 {
    let slant = u32::from(face_italic != want_italic) * 1_000_000;
    let delta = u32::from(face_weight.abs_diff(want_weight));
    let wrong_side = if want_weight >= 400 {
        face_weight < want_weight
    } else {
        face_weight > want_weight
    };
    slant + u32::from(wrong_side) * 1000 + delta
}

fn is_mono_name(name: &str) -> bool {
    name.contains("mono")
}

fn is_serif_name(name: &str) -> bool {
    name.contains("serif") && !name.contains("sans")
}

fn is_sans_name(name: &str) -> bool {
    !name.contains("mono") && !name.contains("serif")
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
    fn weight_distance_prefers_correct_slant_then_nearest_weight() {
        // Wrong slant loses to any weight mismatch.
        assert!(attr_distance(400, true, 400, false) > attr_distance(900, false, 400, false));
        // At or above the request wins when the request is >= 400.
        assert!(attr_distance(700, false, 600, false) < attr_distance(500, false, 600, false));
        // Below the request wins when the request is < 400.
        assert!(attr_distance(200, false, 300, false) < attr_distance(400, false, 300, false));
    }

    #[test]
    fn font_extensions_are_case_insensitive() {
        assert!(is_font_path(Path::new("/x/A.TTF")));
        assert!(is_font_path(Path::new("/x/a.otc")));
        assert!(!is_font_path(Path::new("/x/a.pfb")));
    }
}
