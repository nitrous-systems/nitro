//! Finding a PNG file on disk for an application icon **name**, per a
//! deliberately small subset of the freedesktop Icon Theme Specification.
//!
//! # Why the server needs this at all
//!
//! [`crate::icons`] owns the *symbolic* set: a closed list of shapes
//! compiled into the binary, named by a client and rasterised here. That
//! covers everything the desktop draws for itself. It does not cover the
//! one case where the artwork is not ours — a launcher listing the
//! applications installed on the box. A `.desktop` file says
//! `Icon=firefox`, and the only thing on the machine that knows what
//! `firefox` looks like is the icon theme the distribution installed
//! under `/usr/share/icons`. Resolving that name to a file is this
//! module's whole job.
//!
//! It is filesystem and parsing, nothing else: it returns a
//! [`PathBuf`](std::path::PathBuf) and decodes no bytes. The caller reads
//! the file and hands it to `nitro-png`, then caches the decoded tile —
//! which is why there is no result cache here (see *Cost* below).
//!
//! # What is implemented
//!
//! * The standard search path (`$XDG_DATA_HOME/icons`, `$HOME/.icons`,
//!   `$XDG_DATA_DIRS/icons`) plus the flat, unthemed `/usr/share/pixmaps`
//!   last, and the `NITRO_ICON_PATH` override documented below.
//! * `index.theme`: the `[Icon Theme]` group's `Inherits`, `Directories`
//!   and `ScaledDirectories`, and each directory group's `Size`, `Scale`,
//!   `Type`, `MinSize`, `MaxSize` and `Threshold`.
//! * The theme chain: the requested theme, its inherited themes
//!   breadth-first, and `hicolor` last — the spec's guaranteed fallback
//!   theme, which every icon set on earth ends up leaning on.
//! * The spec's size matching: an exact match at the requested scale
//!   first, then the smallest `directory_size_distance`.
//! * A bounded fallback scan, so an icon in a theme with no usable
//!   `index.theme`, or a lone file in `/usr/share/pixmaps`, is still
//!   found.
//!
//! # What is deliberately left out, and why
//!
//! * **SVG.** Only `.png` candidates are considered. A theme's SVGs are
//!   not the flat two-path shapes `nitro-raster` draws: they use
//!   gradients, clip paths and filters, and rasterising them properly is
//!   a renderer, not a feature. Shipping half of one would show up as
//!   silently wrong artwork on a user's launcher, which is worse than a
//!   missing icon — the caller already has a placeholder for `None`. The
//!   day the raster grows gradients this becomes a one-line change to
//!   [`EXTENSION`].
//! * **XPM.** A dead format kept alive by a handful of pre-2005 packages.
//!   Every one of them also ships a PNG.
//! * **Per-icon user overrides.** The spec's `~/.icons/<theme>` *is*
//!   honoured, because it is just a base directory; what is not honoured
//!   is any notion of pinning one icon name to one file. That is a
//!   desktop-settings feature and it belongs in `server.conf`, not in a
//!   path resolver.
//! * **`Context=`** (`Applications`, `MimeTypes`, `Devices`, …). It
//!   exists so an icon *chooser* can present categories. We are given a
//!   name and asked for a file; the context cannot change the answer.
//! * **Localized theme names** (`Name[de]=`). We key on the theme's
//!   *directory* name and never display a theme to anybody, so the
//!   localized keys are dead weight. They are still parsed carefully
//!   enough to be ignored: a `[locale]` suffix never overwrites the plain
//!   key, because `Size[de]` in a hostile file must not become `Size`.
//! * **`Hidden=`, `Example=`, `.icon` metadata files.** Chooser and
//!   authoring metadata; no effect on lookup.
//!
//! # `NITRO_ICON_PATH`
//!
//! If the environment variable `NITRO_ICON_PATH` is set, its
//! colon-separated list of absolute directories **replaces the entire
//! search path** — not just entries 1–3, but `/usr/share/pixmaps` too.
//! The override is total on purpose: a partial override is not a fixture,
//! because whatever the developer's box happens to have installed still
//! leaks into the answer. With this variable set, the only icons that
//! exist are the ones under the paths it names. It is documented for
//! users in `docs/settings.md`.
//!
//! # Cost
//!
//! [`IconTheme::load`] does all the parsing — every `index.theme` in the
//! chain, once — and that is the entire reason this is a struct rather
//! than a function. [`IconTheme::lookup`] then does nothing but join
//! paths and `stat` them.
//!
//! There is no result cache here, and that is not an oversight: a lookup
//! happens once per *distinct* icon name (a launcher's list, built once),
//! and the expensive artefact is the decoded tile, which the caller
//! caches keyed by name and size. A second cache in front of `stat`
//! would save microseconds and cost a staleness bug — this one would have
//! to be invalidated when a package is installed, and nothing here
//! watches the filesystem.
//!
//! # Hostile input
//!
//! Both the icon name and the `index.theme` contents come from outside
//! the server: a name arrives from a client or a `.desktop` file, and an
//! `index.theme` is a file any package may drop. So: a name containing
//! `/`, `..` or a NUL is refused outright (the one exception is an
//! absolute path, which the desktop-entry spec explicitly allows in
//! `Icon=`, and which must still end in `.png` and name a real file);
//! directory entries from an `index.theme` are validated the same way
//! before they are joined onto a base directory; the inheritance chain is
//! cycle-guarded and capped at [`MAX_CHAIN`]; and the fallback scan is
//! bounded in depth, in entries per directory, and does not follow
//! symlinks. Nothing in here can panic, loop forever, or build a path
//! that escapes a base directory.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// The one file extension a lookup will return, including the dot.
///
/// Named rather than inlined because it is the single point the SVG
/// decision is recorded at: see the module docs.
const EXTENSION: &str = ".png";

/// The theme every icon set is required to fall back to, and which this
/// module appends to every chain whether or not it was asked for.
const FALLBACK_THEME: &str = "hicolor";

/// The most themes a chain may contain, cycles aside.
///
/// Real chains are two or three long (`Adwaita` → `hicolor`). Sixteen is
/// far past any honest theme and short enough that a pathological
/// `Inherits` web costs a few `stat`s rather than a stalled compositor.
pub const MAX_CHAIN: usize = 16;

/// How deep the fallback scan descends below `<basedir>/<theme>`.
///
/// Themes lay their icons out as `<size>/<context>/<name>.png`, which is
/// two levels; four leaves room for the odd `scalable/apps/extra/` while
/// keeping the worst case a handful of `readdir`s.
const MAX_SCAN_DEPTH: u32 = 4;

/// How many entries of any one directory the fallback scan will look at.
///
/// A theme directory holds tens of entries. A directory holding more than
/// this is not a theme, it is somebody's downloads folder mounted in the
/// wrong place, and walking all of it would turn a missing icon into a
/// visible stall.
const MAX_SCAN_ENTRIES: usize = 512;

/// The default value of `$XDG_DATA_DIRS`, from the Base Directory spec.
const DEFAULT_DATA_DIRS: &str = "/usr/local/share:/usr/share";

/// The flat, unthemed directory every distribution still puts a few
/// application icons in. Searched last, after every real theme.
const PIXMAPS: &str = "/usr/share/pixmaps";

/// How an `index.theme` directory group says its size should be matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeType {
    /// The directory holds exactly one size and matches nothing else.
    Fixed,
    /// The directory holds art usable anywhere in `MinSize..=MaxSize`.
    Scalable,
    /// The directory holds one size but tolerates `Threshold` either way.
    Threshold,
}

/// One directory listed in a theme's `Directories`/`ScaledDirectories`,
/// with the size rules its group declared.
#[derive(Debug, Clone)]
struct SubDir {
    /// The path relative to `<basedir>/<theme>`, e.g. `48x48/apps`.
    path: String,
    /// The group's `Size`, in logical pixels.
    size: u32,
    /// The group's `Scale`; 1 when the key is absent.
    scale: u32,
    /// How `size` is to be compared against a request.
    kind: SizeType,
    /// `MinSize`, defaulting to `size`. Only read for [`SizeType::Scalable`].
    min: u32,
    /// `MaxSize`, defaulting to `size`. Only read for [`SizeType::Scalable`].
    max: u32,
    /// `Threshold`, defaulting to 2. Only read for [`SizeType::Threshold`].
    threshold: u32,
}

impl SubDir {
    /// Whether this directory matches `size` at `scale` *exactly*, in the
    /// spec's sense of `directory_matches_size`.
    ///
    /// A different `Scale` is never an exact match: a 32 px icon drawn
    /// for a 2× output is a 64 px file, and handing it to a 1× output
    /// would be a half-size icon, not a crisp one.
    fn matches(&self, size: u32, scale: u32) -> bool {
        if self.scale != scale {
            return false;
        }
        match self.kind {
            SizeType::Fixed => self.size == size,
            SizeType::Scalable => self.min <= size && size <= self.max,
            SizeType::Threshold => {
                self.size.saturating_sub(self.threshold) <= size
                    && size <= self.size.saturating_add(self.threshold)
            }
        }
    }

    /// The spec's `directory_size_distance`, in device pixels.
    ///
    /// Everything is compared at `size * scale` — device pixels — so a
    /// 32 px directory at `Scale=2` and a 64 px directory at `Scale=1`
    /// are the same distance from a 64 device-pixel request, and the tie
    /// is broken by the caller preferring the matching scale.
    fn distance(&self, size: u32, scale: u32) -> u64 {
        let want = u64::from(size) * u64::from(scale);
        let s = u64::from(self.size) * u64::from(self.scale);
        match self.kind {
            SizeType::Fixed => s.abs_diff(want),
            SizeType::Scalable => Self::span_distance(
                want,
                u64::from(self.min) * u64::from(self.scale),
                u64::from(self.max) * u64::from(self.scale),
            ),
            SizeType::Threshold => {
                let t = u64::from(self.threshold) * u64::from(self.scale);
                Self::span_distance(want, s.saturating_sub(t), s.saturating_add(t))
            }
        }
    }

    /// Distance from `want` to the closed interval `lo..=hi`; zero inside.
    ///
    /// Saturating rather than checked because `lo > hi` cannot reach here
    /// (`parse_index` normalises an inverted range) and, if a future edit
    /// ever let it, a zero distance is a harmless answer where an
    /// underflow panic would take the compositor down for a typo in
    /// somebody's `index.theme`.
    fn span_distance(want: u64, lo: u64, hi: u64) -> u64 {
        lo.saturating_sub(want) + want.saturating_sub(hi)
    }

    /// The device size of the art in this directory, used to prefer a
    /// downscale over an upscale when two candidates tie on distance.
    fn device_size(&self) -> u64 {
        u64::from(self.size) * u64::from(self.scale)
    }
}

/// One theme of the chain: its directory name and its parsed directories.
///
/// A theme with an absent or malformed `index.theme` is kept with an
/// empty `subdirs`, because it is still worth the fallback scan — a
/// hand-made theme directory with no index at all is a real thing users
/// create, and refusing to look inside it would be pedantry.
#[derive(Debug, Clone)]
struct Theme {
    /// The directory name, e.g. `hicolor`. Never contains a separator.
    name: String,
    /// The usable entries of `Directories` + `ScaledDirectories`, in the
    /// order the index listed them.
    subdirs: Vec<SubDir>,
}

/// A parsed icon-theme search path: the directories, the theme chain and
/// each theme's `index.theme`.
///
/// Built once with [`IconTheme::load`] (or [`IconTheme::with_dirs`]) and
/// then asked for paths with [`IconTheme::lookup`]. The default value is
/// an empty search path that finds nothing, which is exactly the right
/// behaviour on a box with no icon theme installed.
#[derive(Debug, Default)]
pub struct IconTheme {
    /// The base directories, in search order, as handed to us — including
    /// the ones that do not exist, so [`IconTheme::dirs`] can be logged
    /// and understood.
    dirs: Vec<PathBuf>,
    /// The theme names in effect, in search order, `hicolor` last. Held
    /// separately from `themes` only so [`IconTheme::chain`] can hand out
    /// a contiguous slice.
    chain: Vec<String>,
    /// The same themes, with their parsed directories, in the same order.
    themes: Vec<Theme>,
    /// Whether any base directory existed at load time.
    any_dir: bool,
}

impl IconTheme {
    /// Load the theme called `theme` (e.g. "hicolor", "Adwaita") plus its
    /// inheritance chain, from the standard search directories.
    ///
    /// The directories are, in order:
    ///
    /// 1. `$XDG_DATA_HOME/icons`, defaulting to `$HOME/.local/share/icons`
    /// 2. `$HOME/.icons`, the spec's legacy per-user location
    /// 3. each `$XDG_DATA_DIRS` entry with `/icons` appended, defaulting
    ///    to `/usr/local/share:/usr/share`
    /// 4. `/usr/share/pixmaps`, last, flat and unthemed
    ///
    /// unless `NITRO_ICON_PATH` is set, in which case its entries are the
    /// whole list and none of the four applies — see the module docs.
    ///
    /// Nothing here fails: a directory that is not there contributes
    /// nothing, and a box with no icons at all loads to an
    /// [`IconTheme::is_empty`] value.
    #[must_use]
    pub fn load(theme: &str) -> Self {
        Self::with_dirs(default_dirs(), theme)
    }

    /// Load from an explicit list of icon base directories (the tests use
    /// this, and so does `NITRO_ICON_PATH`).
    ///
    /// The list is taken verbatim: no defaults are appended, `/usr/share/pixmaps`
    /// included. That is what makes it usable as a fixture — the answer
    /// depends only on the trees passed in.
    #[must_use]
    pub fn with_dirs(dirs: Vec<PathBuf>, theme: &str) -> Self {
        let any_dir = dirs.iter().any(|d| d.is_dir());
        let themes = build_chain(&dirs, theme);
        let chain = themes.iter().map(|t| t.name.clone()).collect();
        Self {
            dirs,
            chain,
            themes,
            any_dir,
        }
    }

    /// The base directories searched, in order.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The theme chain actually in effect, in search order, `hicolor` last.
    ///
    /// Themes named by an `Inherits` that exist nowhere on the box are
    /// absent from it, so this is what will genuinely be searched rather
    /// than what the files asked for.
    #[must_use]
    pub fn chain(&self) -> &[String] {
        &self.chain
    }

    /// Whether any base directory exists at all — a box with no icon
    /// theme installed. The caller uses it to skip work and the tests to
    /// skip.
    ///
    /// Note that this asks about the *directories*, not about the icons:
    /// a populated `/usr/share/icons` with no theme matching the request
    /// is not empty, it just answers `None` a lot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.any_dir
    }

    /// The best `.png` file for `name` at `size` logical px on an output
    /// of integer `scale`, or `None`.
    ///
    /// `name` is either a bare icon name (`firefox`) or, as the
    /// desktop-entry spec permits, an absolute path — which is honoured
    /// only when it ends in `.png` and names a readable file. Anything
    /// else containing `/`, `..` or a NUL is refused; see the module docs
    /// on hostile input.
    ///
    /// The search is the spec's, run over the whole chain: first every
    /// theme's directories that match `size` at `scale` exactly, then the
    /// candidate with the smallest size distance, then the bounded
    /// fallback scan. A `scale` or `size` of zero is read as one, because
    /// there is no sensible zero-sized icon and a caller that computed one
    /// deserves an icon rather than a panic.
    #[must_use]
    pub fn lookup(&self, name: &str, size: u32, scale: u32) -> Option<PathBuf> {
        if name.starts_with('/') {
            return absolute(name);
        }
        if !valid_component(name) {
            return None;
        }
        let size = size.max(1);
        let scale = scale.max(1);
        let file = format!("{name}{EXTENSION}");

        // Pass one: an exact match anywhere in the chain. This runs over
        // the *whole* chain before the closest-size pass below, where the
        // spec recurses into the parent theme only after both passes have
        // failed for the child. The difference shows up when a child
        // theme ships one odd size of an icon and `hicolor` ships the
        // requested one: the spec would take the child's mis-sized file,
        // we take hicolor's exact one. A display server scales every icon
        // it draws, so a correctly sized file from the fallback theme is
        // visibly better than a resampled one from the preferred theme,
        // and the theme's identity is preserved for every icon it ships
        // at a normal size — which is all of them.
        if let Some(hit) = self.pass_exact(&file, size, scale) {
            return Some(hit);
        }
        if let Some(hit) = self.pass_closest(&file, size, scale) {
            return Some(hit);
        }
        self.pass_fallback(&file)
    }

    /// Every existing `<basedir>/<theme>/<subdir>/<file>` whose `subdir`
    /// matches `size` at `scale` exactly; the first one wins.
    fn pass_exact(&self, file: &str, size: u32, scale: u32) -> Option<PathBuf> {
        for theme in &self.themes {
            for sub in &theme.subdirs {
                if !sub.matches(size, scale) {
                    continue;
                }
                for dir in &self.dirs {
                    let path = dir.join(&theme.name).join(&sub.path).join(file);
                    if is_file(&path) {
                        return Some(path);
                    }
                }
            }
        }
        None
    }

    /// The existing candidate with the smallest [`SubDir::distance`].
    ///
    /// Candidates are ranked by one key in which *smaller is better* all
    /// the way across, so the comparison needs no special cases: the size
    /// distance first; then whether the directory's `Scale` differs from
    /// the requested one (a match ranks first, because that art was drawn
    /// for this output); then the device size reversed, so the larger
    /// source wins a tie — downscaling loses detail gracefully where
    /// upscaling is a blur, and the caller counts on that preference for
    /// its cache tiles.
    fn pass_closest(&self, file: &str, size: u32, scale: u32) -> Option<PathBuf> {
        type Key = (u64, bool, std::cmp::Reverse<u64>);
        let mut best: Option<(Key, PathBuf)> = None;
        for theme in &self.themes {
            for sub in &theme.subdirs {
                let key: Key = (
                    sub.distance(size, scale),
                    sub.scale != scale,
                    std::cmp::Reverse(sub.device_size()),
                );
                // Strictly better only, so that an equal candidate found
                // in an earlier theme or base directory keeps the win and
                // the search order stays meaningful. Checking this before
                // touching the filesystem is also what keeps the pass to
                // one `stat` per directory that could still win.
                if best.as_ref().is_some_and(|(b, _)| *b <= key) {
                    continue;
                }
                for dir in &self.dirs {
                    let path = dir.join(&theme.name).join(&sub.path).join(file);
                    if is_file(&path) {
                        best = Some((key, path));
                        break;
                    }
                }
            }
        }
        best.map(|(_, path)| path)
    }

    /// The last resort: a bounded walk of any theme directory that had no
    /// usable index, then the flat unthemed directories.
    ///
    /// The walk exists because a theme whose `index.theme` is missing or
    /// broken would otherwise contribute nothing at all, and the flat
    /// check is what makes `/usr/share/pixmaps/foo.png` — which belongs
    /// to no theme and has no size in its name — resolvable.
    fn pass_fallback(&self, file: &str) -> Option<PathBuf> {
        for theme in &self.themes {
            if !theme.subdirs.is_empty() {
                continue;
            }
            for dir in &self.dirs {
                let root = dir.join(&theme.name);
                if let Some(hit) = scan(&root, file, MAX_SCAN_DEPTH) {
                    return Some(hit);
                }
            }
        }
        for dir in &self.dirs {
            let path = dir.join(file);
            if is_file(&path) {
                return Some(path);
            }
        }
        None
    }
}

/// The standard search path, or the `NITRO_ICON_PATH` override.
fn default_dirs() -> Vec<PathBuf> {
    if let Some(over) = std::env::var_os("NITRO_ICON_PATH") {
        // A total override: no defaults, not even `/usr/share/pixmaps`.
        // Relative entries are dropped rather than resolved against the
        // cwd, which for a compositor is `/` or wherever it was started.
        return std::env::split_paths(&over)
            .filter(|p| p.is_absolute())
            .collect();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        Some(d) if d.is_absolute() => dirs.push(d.join("icons")),
        _ => {
            if let Some(h) = &home {
                dirs.push(h.join(".local").join("share").join("icons"));
            }
        }
    }
    if let Some(h) = &home {
        dirs.push(h.join(".icons"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_DATA_DIRS.into());
    for d in std::env::split_paths(&data_dirs).filter(|p| p.is_absolute()) {
        dirs.push(d.join("icons"));
    }
    // Last, and outside the theme machinery entirely: see `pass_fallback`.
    dirs.push(PathBuf::from(PIXMAPS));
    dirs
}

/// Resolve an absolute `Icon=` path.
///
/// The extension test is case-insensitive because `.PNG` occurs in the
/// wild, and the file test is `is_file` so that a dangling symlink or a
/// directory named `foo.png` answers `None` rather than handing the
/// decoder something it will choke on.
fn absolute(name: &str) -> Option<PathBuf> {
    if name.contains('\0') {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(EXTENSION) {
        return None;
    }
    let path = PathBuf::from(name);
    is_file(&path).then_some(path)
}

/// Whether `s` is safe to join onto a base directory as one path
/// component, or as a `/`-joined run of them for a theme's `Directories`.
///
/// Refuses the empty string, anything containing a NUL, anything with a
/// `..` in it and — for the icon-name case — any separator at all. Note
/// that a name like `foo..bar` is refused too: the check is on the
/// substring rather than on parsed components, which costs us nothing
/// real (no icon is named that) and cannot be argued into allowing an
/// escape.
fn valid_component(s: &str) -> bool {
    !s.is_empty() && !s.contains('\0') && !s.contains('/') && !s.contains("..") && s != "."
}

/// The same rules for a theme directory entry, which may contain `/`
/// (`48x48/apps`) but must not start with one or climb.
fn valid_subpath(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('\0')
        && !s.contains("..")
        && !s.starts_with('/')
        && s.split('/').all(|c| !c.is_empty() && c != ".")
}

/// Whether `path` is an existing regular file (symlinks followed, which
/// is how themes alias one icon to another).
fn is_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Look for `file` under `root`, descending at most `depth` levels.
///
/// Checks `root/file` before descending, so the common layout is found
/// without a walk. Subdirectories are entered only when the directory
/// entry itself says "directory": a symlinked subdirectory is skipped,
/// which is a cheap way to make a symlink loop impossible without keeping
/// a set of visited inodes.
fn scan(root: &Path, file: &str, depth: u32) -> Option<PathBuf> {
    let direct = root.join(file);
    if is_file(&direct) {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.take(MAX_SCAN_ENTRIES).flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir())
            && let Some(hit) = scan(&entry.path(), file, depth - 1)
        {
            return Some(hit);
        }
    }
    None
}

/// Build the theme chain: `theme`, its `Inherits` breadth-first, then
/// `hicolor`.
///
/// Breadth-first because that is the order the spec's `Inherits` list
/// implies — a theme's own parents before its grandparents — and because
/// it makes the cap predictable. A theme that exists in no base directory
/// is dropped from the chain, but its index is never read either, so a
/// dangling `Inherits` costs one `stat` and nothing more.
fn build_chain(dirs: &[PathBuf], theme: &str) -> Vec<Theme> {
    let mut out: Vec<Theme> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    if valid_component(theme) {
        queue.push_back(theme.to_string());
    }
    while let Some(name) = queue.pop_front() {
        if out.len() >= MAX_CHAIN {
            break;
        }
        // `hicolor` is appended below whatever the chain says, so an
        // explicit mention of it here is dropped rather than promoted.
        if name == FALLBACK_THEME || !seen.insert(name.clone()) {
            continue;
        }
        let Some(index) = read_index(dirs, &name) else {
            continue;
        };
        for parent in index.inherits {
            if valid_component(&parent) {
                queue.push_back(parent);
            }
        }
        out.push(Theme {
            name,
            subdirs: index.subdirs,
        });
    }
    if let Some(index) = read_index(dirs, FALLBACK_THEME) {
        out.push(Theme {
            name: FALLBACK_THEME.to_string(),
            subdirs: index.subdirs,
        });
    }
    out
}

/// A theme's `index.theme`, reduced to the two things a lookup needs.
#[derive(Debug, Default)]
struct Index {
    /// The `Inherits` list, in order, unvalidated.
    inherits: Vec<String>,
    /// The usable directory groups, in the order `Directories` listed
    /// them followed by `ScaledDirectories`.
    subdirs: Vec<SubDir>,
}

/// Read and parse `<basedir>/<theme>/index.theme`, or `None` when the
/// theme's directory exists in no base directory at all.
///
/// The first base directory that has the theme *and* a readable index
/// wins; a theme split across base directories with two half-indexes is
/// not a thing that occurs, and merging them would mean deciding which
/// `Inherits` is authoritative for no gain. A theme directory that exists
/// with no readable index yields a default [`Index`] — empty, which is
/// the signal `pass_fallback` scans on.
fn read_index(dirs: &[PathBuf], theme: &str) -> Option<Index> {
    let mut exists = false;
    for dir in dirs {
        let root = dir.join(theme);
        if !root.is_dir() {
            continue;
        }
        exists = true;
        if let Ok(text) = std::fs::read_to_string(root.join("index.theme")) {
            let index = parse_index(&text);
            if !index.subdirs.is_empty() || !index.inherits.is_empty() {
                return Some(index);
            }
        }
    }
    exists.then(Index::default)
}

/// Split an `index.theme` into `group name -> key -> value`.
///
/// The grammar is the desktop-entry one, cut down: `#` comments, `[Group]`
/// headers, `Key=Value`. Anything that does not parse is skipped rather
/// than rejected, because half a usable theme beats none and because this
/// file is not ours to validate. Keys carrying a `[locale]` suffix are
/// dropped entirely — we read no localized key, and dropping them is what
/// guarantees a `Size[de]=999` cannot shadow `Size`. Within one group the
/// first spelling of a key wins, so a duplicate cannot overwrite either.
fn parse_groups(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut groups: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            // A `[` line that never closes is a broken header, and
            // everything after it belongs to no group until the next one.
            current = rest.strip_suffix(']').map(str::to_string);
            if let Some(name) = &current {
                groups.entry(name.clone()).or_default();
            }
            continue;
        }
        let (Some(group), Some((key, value))) = (current.as_ref(), line.split_once('=')) else {
            continue;
        };
        let key = key.trim();
        if key.contains('[') || key.is_empty() {
            continue;
        }
        if let Some(map) = groups.get_mut(group) {
            map.entry(key.to_string())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    groups
}

/// Parse the `[Icon Theme]` group and every directory group it names.
///
/// A directory listed without a group, or with a group whose `Size` is
/// absent, zero or unparseable, is dropped: there is no defensible guess
/// for the size of a directory that will not say, and a wrong guess wins
/// the size comparison against directories that were honest.
fn parse_index(text: &str) -> Index {
    let groups = parse_groups(text);
    let Some(head) = groups.get("Icon Theme") else {
        return Index::default();
    };
    let inherits = head.get("Inherits").map(|v| list(v)).unwrap_or_default();
    let mut names = head.get("Directories").map(|v| list(v)).unwrap_or_default();
    names.extend(
        head.get("ScaledDirectories")
            .map(|v| list(v))
            .unwrap_or_default(),
    );

    let mut subdirs = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for name in &names {
        if !valid_subpath(name) || !seen.insert(name.as_str()) {
            continue;
        }
        let Some(group) = groups.get(name) else {
            continue;
        };
        let Some(size) = group.get("Size").and_then(|v| num(v)).filter(|s| *s > 0) else {
            continue;
        };
        let scale = group.get("Scale").and_then(|v| num(v)).filter(|s| *s > 0);
        let kind = match group.get("Type").map(String::as_str) {
            Some("Fixed") => SizeType::Fixed,
            Some("Scalable" | "Scaled") => SizeType::Scalable,
            // `Threshold` is the spec's default, and is also what an
            // unrecognised spelling is read as: it is the forgiving rule,
            // so a typo costs a slightly loose match rather than a
            // directory that matches nothing.
            _ => SizeType::Threshold,
        };
        let min = group.get("MinSize").and_then(|v| num(v)).unwrap_or(size);
        let max = group.get("MaxSize").and_then(|v| num(v)).unwrap_or(size);
        // An inverted or degenerate range is a broken file; fall back to
        // the exact size rather than letting it match everything.
        let (min, max) = if min == 0 || max == 0 || min > max {
            (size, size)
        } else {
            (min, max)
        };
        subdirs.push(SubDir {
            path: name.clone(),
            size,
            scale: scale.unwrap_or(1),
            kind,
            min,
            max,
            threshold: group.get("Threshold").and_then(|v| num(v)).unwrap_or(2),
        });
    }
    Index { inherits, subdirs }
}

/// Split a comma-separated `index.theme` value, dropping empty entries.
fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse an `index.theme` integer, refusing anything a theme would not
/// write (a sign, a float, or a value past any plausible icon size).
fn num(value: &str) -> Option<u32> {
    let n: u32 = value.trim().parse().ok()?;
    (n <= 8192).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh fixture root under the system temp directory, named for
    /// the process and the test so two tests — or two runs — never share
    /// one, and wiped on entry so a crashed run leaves no booby trap.
    fn fixture(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitro-icon-theme-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture root");
        dir
    }

    /// Create `root/rel` and everything above it, with `body` inside.
    fn put(root: &Path, rel: &str, body: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture parent");
        }
        std::fs::write(&path, body).expect("fixture file");
        path
    }

    /// An empty file, which is all `is_file` and the tests care about:
    /// this module decodes nothing.
    fn touch(root: &Path, rel: &str) -> PathBuf {
        put(root, rel, "")
    }

    /// The tail of a path, for readable assertions.
    fn tail(path: &Path, n: usize) -> String {
        let parts: Vec<_> = path
            .components()
            .rev()
            .take(n)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        parts.into_iter().rev().collect::<Vec<_>>().join("/")
    }

    #[test]
    fn an_exact_size_match_beats_a_near_one() {
        let root = fixture("exact");
        put(
            &root,
            "t/index.theme",
            "[Icon Theme]\n\
             Directories=16x16/apps,22x22/apps\n\
             \n\
             [16x16/apps]\n\
             Size=16\n\
             Type=Fixed\n\
             \n\
             [22x22/apps]\n\
             Size=22\n\
             Type=Fixed\n",
        );
        touch(&root, "t/16x16/apps/gear.png");
        touch(&root, "t/22x22/apps/gear.png");
        let it = IconTheme::with_dirs(vec![root.clone()], "t");
        let hit = it.lookup("gear", 16, 1).expect("16 px icon");
        assert_eq!(tail(&hit, 4), "t/16x16/apps/gear.png");
        // And the other way round, so the test is about matching rather
        // than about listing order.
        let hit = it.lookup("gear", 22, 1).expect("22 px icon");
        assert_eq!(tail(&hit, 4), "t/22x22/apps/gear.png");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_scale_two_directory_wins_only_for_a_scale_two_output() {
        let root = fixture("scale");
        put(
            &root,
            "t/index.theme",
            "[Icon Theme]\n\
             Directories=32x32/apps\n\
             ScaledDirectories=32x32@2/apps\n\
             \n\
             [32x32/apps]\n\
             Size=32\n\
             Type=Fixed\n\
             \n\
             [32x32@2/apps]\n\
             Size=32\n\
             Scale=2\n\
             Type=Fixed\n",
        );
        touch(&root, "t/32x32/apps/gear.png");
        touch(&root, "t/32x32@2/apps/gear.png");
        let it = IconTheme::with_dirs(vec![root.clone()], "t");
        assert_eq!(
            tail(&it.lookup("gear", 32, 1).expect("1x"), 4),
            "t/32x32/apps/gear.png"
        );
        assert_eq!(
            tail(&it.lookup("gear", 32, 2).expect("2x"), 4),
            "t/32x32@2/apps/gear.png"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inheritance_is_followed_and_hicolor_is_always_last() {
        let root = fixture("inherit");
        let index = |dirs: &str| {
            format!(
                "[Icon Theme]\n{dirs}Directories=16x16/apps\n\n[16x16/apps]\nSize=16\nType=Fixed\n"
            )
        };
        put(&root, "kid/index.theme", &index("Inherits=mum\n"));
        put(&root, "mum/index.theme", &index(""));
        put(&root, "hicolor/index.theme", &index(""));
        touch(&root, "mum/16x16/apps/frommum.png");
        touch(&root, "hicolor/16x16/apps/fromhicolor.png");
        let it = IconTheme::with_dirs(vec![root.clone()], "kid");
        assert_eq!(it.chain(), ["kid", "mum", "hicolor"]);
        assert_eq!(
            tail(&it.lookup("frommum", 16, 1).expect("inherited"), 4),
            "mum/16x16/apps/frommum.png"
        );

        // A theme that inherits nothing at all still reaches hicolor,
        // which is the property every icon set on the box relies on.
        let lone = IconTheme::with_dirs(vec![root.clone()], "mum");
        assert_eq!(lone.chain(), ["mum", "hicolor"]);
        assert_eq!(
            tail(&lone.lookup("fromhicolor", 16, 1).expect("hicolor"), 4),
            "hicolor/16x16/apps/fromhicolor.png"
        );
        // A theme nobody installed is dropped from the chain rather than
        // searched, and hicolor still terminates it.
        let missing = IconTheme::with_dirs(vec![root.clone()], "nosuch");
        assert_eq!(missing.chain(), ["hicolor"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_svg_never_wins_and_an_svg_only_icon_is_absent() {
        let root = fixture("svg");
        put(
            &root,
            "t/index.theme",
            "[Icon Theme]\n\
             Directories=scalable/apps,16x16/apps\n\
             \n\
             [scalable/apps]\n\
             Size=16\n\
             Type=Scalable\n\
             MinSize=8\n\
             MaxSize=256\n\
             \n\
             [16x16/apps]\n\
             Size=16\n\
             Type=Fixed\n",
        );
        touch(&root, "t/scalable/apps/gear.svg");
        touch(&root, "t/16x16/apps/gear.png");
        touch(&root, "t/scalable/apps/vector.svg");
        touch(&root, "t/16x16/apps/vector.svg");
        let it = IconTheme::with_dirs(vec![root.clone()], "t");
        assert_eq!(
            tail(&it.lookup("gear", 16, 1).expect("png beside svg"), 4),
            "t/16x16/apps/gear.png"
        );
        assert_eq!(it.lookup("vector", 16, 1), None, "svg-only is not found");
        // Nor by asking for the SVG by name: the extension is appended,
        // so this looks for `vector.svg.png` and finds nothing.
        assert_eq!(it.lookup("vector.svg", 16, 1), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_closest_size_wins_and_a_bigger_source_beats_a_smaller_one() {
        let root = fixture("closest");
        put(
            &root,
            "t/index.theme",
            "[Icon Theme]\n\
             Directories=16x16/apps,48x48/apps,64x64/apps\n\
             \n\
             [16x16/apps]\n\
             Size=16\n\
             Type=Fixed\n\
             \n\
             [48x48/apps]\n\
             Size=48\n\
             Type=Fixed\n\
             \n\
             [64x64/apps]\n\
             Size=64\n\
             Type=Fixed\n",
        );
        // 32 px requested: 16 and 48 are both 16 away, so the downscale
        // from 48 must win; 64 is 32 away and must lose to both.
        touch(&root, "t/16x16/apps/tie.png");
        touch(&root, "t/48x48/apps/tie.png");
        touch(&root, "t/64x64/apps/tie.png");
        // And with only the far ones present, the nearer of them wins.
        touch(&root, "t/48x48/apps/far.png");
        touch(&root, "t/64x64/apps/far.png");
        let it = IconTheme::with_dirs(vec![root.clone()], "t");
        assert_eq!(
            tail(&it.lookup("tie", 32, 1).expect("tie"), 4),
            "t/48x48/apps/tie.png"
        );
        assert_eq!(
            tail(&it.lookup("far", 60, 1).expect("far"), 4),
            "t/64x64/apps/far.png"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_absolute_png_path_is_honoured_and_a_climbing_name_is_refused() {
        let root = fixture("absolute");
        let png = touch(&root, "stuff/logo.png");
        let svg = touch(&root, "stuff/logo.svg");
        touch(&root, "secret.png");
        let it = IconTheme::with_dirs(vec![root.join("stuff")], "t");
        assert_eq!(
            it.lookup(&png.to_string_lossy(), 32, 1).as_ref(),
            Some(&png),
            "an absolute .png is taken as given"
        );
        assert_eq!(
            it.lookup(&svg.to_string_lossy(), 32, 1),
            None,
            "an absolute .svg is still an svg"
        );
        assert_eq!(
            it.lookup(&root.join("stuff/nothere.png").to_string_lossy(), 32, 1),
            None,
            "an absolute path to nothing is None, not a path to nothing"
        );
        // A relative name may not climb, carry a separator or a NUL.
        assert_eq!(it.lookup("../secret", 32, 1), None);
        assert_eq!(it.lookup("apps/gear", 32, 1), None);
        assert_eq!(it.lookup("ge\0ar", 32, 1), None);
        assert_eq!(it.lookup("", 32, 1), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_flat_pixmaps_directory_is_the_last_resort() {
        let root = fixture("pixmaps");
        let themes = root.join("icons");
        let pixmaps = root.join("pixmaps");
        std::fs::create_dir_all(&pixmaps).expect("pixmaps");
        put(
            &themes,
            "hicolor/index.theme",
            "[Icon Theme]\nDirectories=16x16/apps\n\n[16x16/apps]\nSize=16\nType=Fixed\n",
        );
        touch(&themes, "hicolor/16x16/apps/themed.png");
        touch(&pixmaps, "legacy.png");
        let it = IconTheme::with_dirs(vec![themes.clone(), pixmaps.clone()], "hicolor");
        assert_eq!(
            it.lookup("legacy", 16, 1).expect("flat"),
            pixmaps.join("legacy.png")
        );
        // The themed directory still wins for an icon that is in both.
        touch(&pixmaps, "themed.png");
        assert_eq!(
            tail(&it.lookup("themed", 16, 1).expect("themed"), 4),
            "hicolor/16x16/apps/themed.png"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_index_theme_does_not_panic_and_the_scan_still_finds_it() {
        let root = fixture("broken");
        put(
            &root,
            "junk/index.theme",
            "[[[not an ini\n= = =\nSize\n[Icon Theme\nDirectories=../../etc,\n#\n",
        );
        touch(&root, "junk/48x48/apps/thing.png");
        // And a theme with no index at all, which users really do make.
        touch(&root, "noindex/64x64/apps/other.png");
        put(
            &root,
            "hicolor/index.theme",
            "[Icon Theme]\nDirectories=16x16/apps\n\n[16x16/apps]\nSize=16\n",
        );
        let it = IconTheme::with_dirs(vec![root.clone()], "junk");
        assert_eq!(
            tail(&it.lookup("thing", 32, 1).expect("scanned"), 4),
            "junk/48x48/apps/thing.png"
        );
        assert_eq!(it.lookup("absent", 32, 1), None);
        let other = IconTheme::with_dirs(vec![root.clone()], "noindex");
        assert_eq!(
            tail(&other.lookup("other", 32, 1).expect("scanned"), 4),
            "noindex/64x64/apps/other.png"
        );
        // Deeper than the scan's bound is deliberately not found.
        touch(&root, "noindex/a/b/c/d/e/deep.png");
        assert_eq!(other.lookup("deep", 32, 1), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cyclic_inherits_terminates() {
        let root = fixture("cycle");
        let index = |inherits: &str| {
            format!(
                "[Icon Theme]\nInherits={inherits}\nDirectories=16x16/apps\n\n\
                 [16x16/apps]\nSize=16\nType=Fixed\n"
            )
        };
        put(&root, "a/index.theme", &index("b"));
        put(&root, "b/index.theme", &index("a,b,a"));
        put(&root, "hicolor/index.theme", &index("a"));
        touch(&root, "b/16x16/apps/gear.png");
        let it = IconTheme::with_dirs(vec![root.clone()], "a");
        assert_eq!(it.chain(), ["a", "b", "hicolor"]);
        assert!(it.chain().len() <= MAX_CHAIN);
        assert_eq!(
            tail(&it.lookup("gear", 16, 1).expect("through the cycle"), 4),
            "b/16x16/apps/gear.png"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_box_with_no_icon_directories_is_empty_and_finds_nothing() {
        let root = fixture("empty");
        let it = IconTheme::with_dirs(vec![root.join("nosuch")], "hicolor");
        assert!(it.is_empty());
        assert_eq!(it.lookup("gear", 16, 1), None);
        assert!(it.chain().is_empty());
        assert_eq!(it.dirs().len(), 1);
        // A default value is the same thing with no directories at all.
        let none = IconTheme::default();
        assert!(none.is_empty());
        assert_eq!(none.lookup("gear", 16, 1), None);
        // An existing directory is not empty even with nothing in it.
        let it = IconTheme::with_dirs(vec![root.clone()], "hicolor");
        assert!(!it.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_localized_key_never_shadows_the_plain_one() {
        // The hostile case this guards: `Size[de]` parsed as `Size` would
        // let a translation decide an icon's size.
        let index = parse_index(
            "[Icon Theme]\n\
             Name[de]=Beispiel\n\
             Directories=16x16/apps\n\
             \n\
             [16x16/apps]\n\
             Size[de]=999\n\
             Size=16\n\
             Type=Fixed\n",
        );
        assert_eq!(index.subdirs.len(), 1);
        assert_eq!(index.subdirs[0].size, 16);
        // A group whose Size is only localized has no size at all.
        let index =
            parse_index("[Icon Theme]\nDirectories=16x16/apps\n\n[16x16/apps]\nSize[de]=16\n");
        assert!(index.subdirs.is_empty());
    }

    #[test]
    fn a_directory_entry_may_not_climb_out_of_the_theme() {
        let index = parse_index(
            "[Icon Theme]\n\
             Directories=../../../etc,/abs,ok/apps\n\
             \n\
             [../../../etc]\n\
             Size=16\n\
             \n\
             [/abs]\n\
             Size=16\n\
             \n\
             [ok/apps]\n\
             Size=16\n",
        );
        let paths: Vec<_> = index.subdirs.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, ["ok/apps"]);
    }
}
