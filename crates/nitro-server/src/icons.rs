//! The server's icon engine: the symbolic icon set, the rasteriser, and
//! the cache of coverage masks the painter blits.
//!
//! # Why the server owns the artwork
//!
//! A client sends a **name** — `"gear"`, `"list"`, `"cpu"` — and nothing
//! else. That is the same bargain [`crate::text`] makes for strings, and
//! it is made for the same three reasons, spelled out in `docs/icons.md`:
//!
//! * **Remote.** An icon by name costs four bytes and a string over TCP;
//!   an icon as pixels costs a file descriptor, which cannot cross a
//!   remote link at all (`caps::REMOTE`). A `SetIcon` works unchanged
//!   over `tcp://`, which is the whole point.
//! * **Theme.** The node carries a palette *role*, not a colour, and the
//!   role is resolved here, at paint time, against whatever palette the
//!   server is holding. A `theme.scheme = dark` therefore recolours every
//!   icon on screen in the same frame as the text, with no client message.
//! * **Scale.** The mask is rasterised at `round(size_logical × scale)`
//!   device pixels. The same icon on a 1× and a 2× output rasterises
//!   twice and is crisp on both, exactly as a glyph is — a client-side
//!   bitmap could only be a blurry 2× blit of a 16 px tile.
//!
//! # The cache
//!
//! Keyed by `(icon index, device px)` and holding an **A8 coverage
//! mask**, not a tinted BGRA tile. That is deliberate: the raster's glyph
//! path already blends a coverage mask with a colour
//! ([`Canvas::blit_mask`](nitro_raster::Canvas::blit_mask)), so a
//! symbolic icon really *is* a big glyph and gets the same code, the same
//! gamma and the same clipping. Tinting at blit time also means the
//! colour is not part of the key: a scheme flip costs zero re-rasters,
//! which is the property the theme test asserts.
//!
//! **No eviction**, matching the glyph atlas, and for a sharper reason:
//! the set is closed. There are [`nitro_icons::all()`] icons and the
//! sizes in use are the handful a desktop lays out at, so the cache has a
//! hard bound rather than a policy — see [`IconEngine::MAX_BYTES`], which
//! is what the server refuses to grow past. The `stats` keys
//! `icons_cached`, `icon_renders` and `icon_bytes` are how that bound is
//! watched from outside the process.
//!
//! # Application icons, which are the other half
//!
//! Everything above is about artwork the server *owns*. An application
//! icon is artwork it does not: `firefox` is a PNG the distribution
//! installed under `/usr/share/icons`, found through the XDG icon theme
//! by [`crate::icon_theme`] and decoded by `nitro-png`.
//!
//! Three things are different about those, and each has its own reason.
//!
//! **The role byte picks the set, not a search order.** A `SetIcon` whose
//! `role` is a palette role means the symbolic set and never looks at the
//! theme; `role == AS_COLOURED` (0xff) means the theme and never looks at
//! the symbolic set. The alternative — one namespace searched
//! symbolic-first — was rejected because it makes the meaning of
//! `icon("list")` depend on what the box happens to have installed:
//! shadowing is invisible, and the day a theme ships a `list` the
//! shadowing is the only thing standing between the desktop and somebody
//! else's artwork in its menu button. With the role deciding, the call
//! site says which it wants — `icon("list")` is ours, `icon("firefox")
//! .coloured()` is the theme's — and there is no collision to reason
//! about at all.
//!
//! **The cache holds pixels, not coverage.** A coloured icon has no tint
//! to resolve, so there is nothing to keep out of the key; the entry is
//! the BGRA tile the blitter takes, already resampled to the device size.
//! That also means a scheme flip does *not* touch it, which is correct:
//! a Firefox logo is not part of the palette.
//!
//! **The cache is bounded and evicts.** The symbolic set is closed, so it
//! can refuse rather than evict; the set of applications installed on a
//! machine is not. [`IconEngine::APP_MAX_BYTES`] is a byte cap with LRU
//! eviction behind it, and `app_icon_bytes` / `app_icons_cached` /
//! `app_icon_evictions` in `stats` are how it is watched.
//!
//! Decoding is **lazy**: the commit-time resolution is a `stat`, and the
//! file is read and decoded on the first paint that needs it, once per
//! `(name, device px)`. A 256 px PNG costs about 2 ms on the test box,
//! which is a frame — paying it per frame would be a bug and paying it
//! per commit would put it on the client's first-paint latency.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use nitro_core::{Color, IRect, Palette, Point, Rect, Role, Transform};
use nitro_raster::{Canvas, Image as RasterImage, Mask, PixelFormat};

use crate::icon_theme::IconTheme;
use crate::{debug, warn};

/// Smallest device size an icon is rasterised at. Below this the mask is
/// a smudge and the coverage is meaningless.
pub const MIN_PX: u32 = 4;
/// Largest device size an icon is rasterised at: 256 logical pixels on a
/// 2× output, which is far past any desktop use and short of a memory
/// problem (512² = 256 KiB for one mask).
pub const MAX_PX: u32 = 512;

/// The size an application icon name is *probed* at when a client names
/// it, before anything knows what size the node will be laid out at.
///
/// 48, because it is the size every theme ships and the size a launcher
/// row wants; the answer only decides whether the name exists at all and
/// which file is remembered as its source of last resort. The paint path
/// looks the name up again at the real device size.
const PROBE_PX: u32 = 48;

/// One rasterised icon: an A8 coverage mask and its square side.
#[derive(Debug)]
struct Entry {
    px: u32,
    coverage: Vec<u8>,
}

/// One decoded application icon: a straight-alpha BGRA tile, square, at
/// the device size it will be blitted at.
///
/// Pixels rather than coverage, unlike [`Entry`], because a coloured icon
/// has no tint to resolve — see the module docs. `used` is the frame-ish
/// clock the LRU evicts on: it is bumped on every hit, so the entry
/// evicted is genuinely the one nothing has drawn for longest.
#[derive(Debug)]
struct AppEntry {
    px: u32,
    /// `px * px * 4` bytes, `[b, g, r, a]` per pixel.
    data: Vec<u8>,
    used: u64,
}

/// What an application icon name resolved to, cached so a name that is
/// not on the box is not `stat`ed again every frame.
#[derive(Debug, Clone)]
enum Resolved {
    /// The file the theme lookup found.
    File(PathBuf),
    /// The theme has no such icon, or its file would not decode. Held so
    /// the failure costs one lookup rather than one per paint — a missing
    /// icon is the *common* case on a box with a thin theme, and a
    /// launcher redrawing forty rows must not walk the search path forty
    /// times a frame.
    Missing,
}

/// The icon set, the rasteriser and the mask cache.
///
/// One instance lives in the event loop, beside the [`TextEngine`]. It is
/// not `Sync` and does not want to be: rasterising happens on the paint
/// path, which is single-threaded like everything else here.
#[derive(Debug, Default)]
pub struct IconEngine {
    cache: HashMap<(u32, u32), Entry>,
    renders: u64,
    bytes: usize,
    /// Rasters refused because the cache was already at [`Self::MAX_BYTES`].
    refused: u64,
    // -- application icons (see the module docs) ----------------------
    /// The parsed icon theme: the search path and every `index.theme` in
    /// the chain, read once at start and again on `reload`.
    theme: IconTheme,
    /// Application icon **names** a client has asked for, in the order
    /// they were first seen. The index into this is what the scene
    /// stores, exactly as [`nitro_icons::index_of`] is for the symbolic
    /// set — so no string crosses into `nitro-scene` either way.
    app_names: Vec<String>,
    /// `name -> index into app_names`, so a repeated `SetIcon` for the
    /// same application is a hash lookup and not a new entry.
    app_index: HashMap<String, u32>,
    /// `name index -> what the theme lookup answered`, resolved lazily on
    /// the first paint and never re-walked.
    app_paths: HashMap<u32, Resolved>,
    /// `(name index, device px) -> decoded tile`.
    app_cache: HashMap<(u32, u32), AppEntry>,
    app_bytes: usize,
    app_loads: u64,
    app_misses: u64,
    app_evictions: u64,
    /// Longest single decode-and-scale, in microseconds. The number the
    /// "decode lazily, never per frame" claim is settled on.
    app_decode_us_max: u64,
    /// Monotonic tick for the LRU, bumped on every cache touch.
    app_clock: u64,
}

impl IconEngine {
    /// The cache's hard ceiling in bytes.
    ///
    /// 2 MiB is about 8 000 16-px masks, or every icon in the set at four
    /// different sizes with two orders of magnitude of room left. Past it
    /// the engine stops caching and rasterises nothing further rather
    /// than evicting: an eviction policy for a *closed* set with a
    /// handful of sizes would be machinery answering a question that
    /// cannot be asked. A server that ever reports `icon_refusals` above
    /// zero has found a case this reasoning missed, which is why the
    /// counter exists.
    pub const MAX_BYTES: usize = 2 * 1024 * 1024;

    /// The **application** icon cache's ceiling in bytes.
    ///
    /// 4 MiB, and unlike [`Self::MAX_BYTES`] it is a cap with an LRU
    /// behind it rather than a refusal, because the set it bounds is open:
    /// a machine has as many application icons as it has applications, and
    /// "refuse the next one" would mean a launcher whose last rows are
    /// blank for the rest of the session. 4 MiB is 64 tiles of 128² or
    /// about a thousand at 32² — a launcher showing twenty 24 px rows and
    /// a bar showing ten 16 px buttons costs 41 KB of it.
    pub const APP_MAX_BYTES: usize = 4 * 1024 * 1024;

    /// An empty engine. Rasterises nothing until something is painted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An engine whose application icons come from the XDG theme called
    /// `theme` (`theme.icons` in `server.conf`).
    ///
    /// The parsing happens here, once. A server that finds no icon
    /// directories at all is not an error — it is the test box before
    /// anything was installed — and simply answers `BadIcon` for every
    /// application name.
    #[must_use]
    pub fn with_theme(theme: &str) -> Self {
        let mut e = Self::default();
        e.set_theme(theme);
        e
    }

    /// Re-read the icon theme, on `reload`.
    ///
    /// Everything derived from the old theme goes with it: the resolved
    /// paths and the decoded tiles, because the whole point of changing
    /// `theme.icons` is that `firefox` should now be a different file.
    /// The **names** survive, because the scene is holding their indices
    /// — a node whose icon index changed meaning would be a far worse bug
    /// than a re-decode, and the indices are what make the scene
    /// string-free.
    pub fn set_theme(&mut self, theme: &str) {
        let started = Instant::now();
        self.theme = IconTheme::load(theme);
        self.app_paths.clear();
        self.app_cache.clear();
        self.app_bytes = 0;
        debug!(
            "icon theme {theme:?}: chain [{}] over {} director{} in {:.1} ms",
            self.theme.chain().join(", "),
            self.theme.dirs().len(),
            if self.theme.dirs().len() == 1 {
                "y"
            } else {
                "ies"
            },
            started.elapsed().as_secs_f32() * 1e3
        );
    }

    /// The icon theme in force, for the tests and for diagnostics.
    #[must_use]
    pub fn theme(&self) -> &IconTheme {
        &self.theme
    }

    /// Whether the server has an icon set at all — the `caps::ICONS` bit.
    ///
    /// Always true in this build: the set is compiled in, so unlike fonts
    /// there is nothing to find on the box. It is still a method and
    /// still a capability bit, because a future icon-less server (a test
    /// fixture, a stripped remote view) is a thing a client must be able
    /// to ask about, and because a client that checks the bit is a client
    /// that lays out correctly against one.
    #[must_use]
    pub fn has_icons(&self) -> bool {
        !nitro_icons::all().is_empty()
    }

    /// The handle for an icon name, or `None` if the set has no such icon.
    ///
    /// The handle is what goes in the scene: an index, so no string ever
    /// crosses into `nitro-scene` and a repaint compares two `u32`s.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<u32> {
        nitro_icons::index_of(name)
    }

    /// The handle for an **application** icon name, or `None` when the
    /// machine's icon theme has no such icon.
    ///
    /// Resolution — walking the theme's directories and `stat`ing — happens
    /// here, at commit time, because this is the call that decides whether
    /// the client gets a `BadIcon`: a name that cannot be answered must be
    /// answered *now*, so the client's `.fallback(…)` can re-send inside
    /// the same interaction rather than a frame later. What does **not**
    /// happen here is the decode: that is milliseconds, it depends on the
    /// device size the node has not been laid out at yet, and it belongs
    /// on the paint path where it is paid once and cached.
    ///
    /// A name resolved once keeps its index forever, including the
    /// resolution's answer, so a launcher rebuilding its rows costs one
    /// hash lookup per row and no filesystem at all.
    pub fn lookup_app(&mut self, name: &str) -> Option<u32> {
        if name.is_empty() {
            return None;
        }
        if let Some(index) = self.app_index.get(name).copied() {
            return match self.app_paths.get(&index) {
                Some(Resolved::Missing) => None,
                _ => Some(index),
            };
        }
        // A wanted size is needed to pick a directory, and there isn't one
        // yet: the node's device size is a layout-and-scale question the
        // commit cannot answer. `PROBE_PX` only decides *which* file is
        // remembered as the name's source; the paint path re-resolves at
        // the real device size the first time it decodes, so a 16 px bar
        // button and a 48 px launcher row do not share a source by
        // accident.
        let path = self.theme.lookup(name, PROBE_PX, 1)?;
        let index = u32::try_from(self.app_names.len()).ok()?;
        self.app_names.push(name.to_owned());
        self.app_index.insert(name.to_owned(), index);
        self.app_paths.insert(index, Resolved::File(path));
        Some(index)
    }

    /// The name application-icon handle `icon` stands for, for diagnostics.
    #[must_use]
    pub fn app_name(&self, icon: u32) -> Option<&str> {
        self.app_names.get(icon as usize).map(String::as_str)
    }

    /// Whether the box has an icon theme to look application icons up in.
    ///
    /// Not a capability bit: `caps::ICONS` says the server has artwork,
    /// and it does — the symbolic set is compiled in. This is the weaker
    /// statement that a *coloured* name has somewhere to come from, and it
    /// is what the corpus tests skip on.
    #[must_use]
    pub fn has_app_icons(&self) -> bool {
        !self.theme.is_empty()
    }

    /// Draw icon `icon` into `canvas`, clipped to `clip`.
    ///
    /// `origin` is the icon box's top-left corner in the node's local
    /// space (the scene centred it in the bounds); `transform` maps that
    /// space to device pixels. The mask is rasterised at the **device**
    /// size, so a 2× output gets a real 2× icon.
    ///
    /// `role` is resolved against `palette` here rather than stored, which
    /// is what makes a scheme switch free — unless it is
    /// [`IconRef::AS_COLOURED`](nitro_scene::IconRef::AS_COLOURED), which
    /// selects an **application** icon instead: `icon` is then a handle
    /// from [`Self::lookup_app`], the tile is decoded from the theme's PNG
    /// on first use, and the palette has nothing to say about it.
    #[allow(clippy::too_many_arguments)] // One icon blit's inputs; a struct here would be this list with a name.
    pub fn paint(
        &mut self,
        canvas: &mut Canvas<'_>,
        clip: &IRect,
        transform: &Transform,
        icon: u32,
        origin: Point,
        size: f32,
        role: u8,
        palette: &Palette,
        opacity: f32,
    ) {
        if opacity <= 0.0 || clip.is_empty() || !(size.is_finite() && size > 0.0) {
            return;
        }
        // The world transform is axis-aligned (the rasterizer says so), so
        // one factor describes the scale from logical units to pixels.
        let scale = transform.a.abs().max(transform.b.abs());
        if !(scale.is_finite() && scale > 0.0) {
            return;
        }
        let px = device_px(size, scale);
        let pen = transform.apply(origin);
        if !(pen.x.is_finite() && pen.y.is_finite()) {
            return;
        }
        let (x, y) = (pen.x.round() as i32, pen.y.round() as i32);
        if role == nitro_scene::IconRef::AS_COLOURED {
            self.paint_app(canvas, clip, icon, x, y, px, opacity);
            return;
        }
        let Some(color) = tint(role, palette) else {
            return;
        };
        if color.is_transparent() {
            return;
        }
        let Some(entry) = self.mask(icon, px) else {
            return;
        };
        let mask = Mask {
            data: &entry.coverage,
            w: entry.px,
            h: entry.px,
            stride: entry.px,
        };
        // `round`, not `floor`: an icon has no subpixel bucket the way a
        // glyph does, so the honest placement is the nearest whole pixel —
        // which is also what keeps a 16-unit grid's own pixel boundaries
        // landing on the screen's.
        canvas.blit_mask(clip, x, y, &mask, color, opacity);
    }

    /// Blit an application icon's decoded tile, already placed at device
    /// pixel `(x, y)` and `px` on a side.
    ///
    /// One-to-one: the tile was resampled to `px` when it was decoded, so
    /// the blitter takes its integer-aligned fast path and nothing is
    /// filtered per frame. That is the whole reason the cache is keyed by
    /// device px — a tile stored at its file size and scaled at paint
    /// time would be a bilinear pass per icon per frame, and the icons on
    /// a bar are redrawn whenever the clock ticks.
    #[allow(clippy::too_many_arguments)] // The blit's inputs, one deeper than `paint`'s.
    fn paint_app(
        &mut self,
        canvas: &mut Canvas<'_>,
        clip: &IRect,
        icon: u32,
        x: i32,
        y: i32,
        px: u32,
        opacity: f32,
    ) {
        let Some(entry) = self.app_tile(icon, px) else {
            return;
        };
        let image = RasterImage {
            data: &entry.data,
            width: entry.px,
            height: entry.px,
            stride: entry.px * 4,
            // Straight alpha, which is what `nitro-png` returns and what
            // `Argb8888` means here; the blitter composites source-over.
            format: PixelFormat::Argb8888,
        };
        let dst = Rect::new(x as f32, y as f32, entry.px as f32, entry.px as f32);
        let side = i32::try_from(entry.px).unwrap_or(i32::MAX);
        let src = IRect::new(0, 0, side, side);
        canvas.blit(clip, &dst, &image, &src, opacity);
    }

    /// The cached mask for `(icon, px)`, rasterising it on first use.
    fn mask(&mut self, icon: u32, px: u32) -> Option<&Entry> {
        if !self.cache.contains_key(&(icon, px)) {
            let def = nitro_icons::at(icon)?;
            if self.bytes + (px as usize * px as usize) > Self::MAX_BYTES {
                self.refused += 1;
                return None;
            }
            let m = nitro_icons::rasterise(def, px);
            if m.w == 0 || m.h == 0 {
                return None;
            }
            self.renders += 1;
            self.bytes += m.coverage.len();
            self.cache.insert(
                (icon, px),
                Entry {
                    px: m.w,
                    coverage: m.coverage,
                },
            );
        }
        self.cache.get(&(icon, px))
    }

    /// The cached tile for application icon `(icon, px)`, decoding it on
    /// first use.
    ///
    /// This is the only place a PNG is read, and it happens on the paint
    /// path, once per `(name, device px)`. Everything that can go wrong
    /// with a file on somebody else's disk — it moved, it is not a PNG, it
    /// is a 4 GB claim in an `IHDR` — ends here as a one-line warning and
    /// a `Missing` mark, so the failure costs one log line rather than one
    /// per frame forever.
    fn app_tile(&mut self, icon: u32, px: u32) -> Option<&AppEntry> {
        self.app_clock += 1;
        let clock = self.app_clock;
        if let Some(entry) = self.app_cache.get_mut(&(icon, px)) {
            entry.used = clock;
            return self.app_cache.get(&(icon, px));
        }
        // Re-resolve at the size actually being painted rather than reusing
        // the commit-time probe: a theme that ships 16, 32 and 48 px of an
        // icon should give the bar its 16 and the launcher its 48, and
        // downscaling the 48 into a 16 px button would be both slower and
        // softer than reading the file made for it.
        let name = self.app_names.get(icon as usize)?.clone();
        let path = match self.theme.lookup(&name, px, 1) {
            Some(p) => p,
            // Nothing at this size: fall back to whatever the name
            // resolved to at commit time, which is a real file.
            None => match self.app_paths.get(&icon) {
                Some(Resolved::File(p)) => p.clone(),
                _ => return None,
            },
        };
        let started = Instant::now();
        let data = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("app icon {name:?}: {}: {e}", path.display());
                self.app_paths.insert(icon, Resolved::Missing);
                self.app_misses += 1;
                return None;
            }
        };
        let decoded = match nitro_png::decode(&data) {
            Ok(img) => img,
            Err(e) => {
                warn!("app icon {name:?}: {}: {e}", path.display());
                self.app_paths.insert(icon, Resolved::Missing);
                self.app_misses += 1;
                return None;
            }
        };
        let tile = square_tile(&decoded, px)?;
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.app_decode_us_max = self.app_decode_us_max.max(elapsed);
        self.app_loads += 1;
        debug!(
            "app icon {name:?} at {px} px from {} ({}×{} source) in {elapsed} µs",
            path.display(),
            decoded.width,
            decoded.height
        );
        self.evict_app_to(Self::APP_MAX_BYTES.saturating_sub(tile.len()));
        self.app_bytes += tile.len();
        self.app_cache.insert(
            (icon, px),
            AppEntry {
                px,
                data: tile,
                used: clock,
            },
        );
        self.app_cache.get(&(icon, px))
    }

    /// Evict least-recently-used application tiles until the cache holds
    /// at most `target` bytes.
    ///
    /// A linear scan per eviction, because the cache holds tens of entries
    /// and evicting is the rare path: a heap keyed by a clock that every
    /// hit bumps would be a second data structure to keep in step for a
    /// saving measured in nanoseconds on a set this size.
    fn evict_app_to(&mut self, target: usize) {
        while self.app_bytes > target {
            let Some(victim) = self
                .app_cache
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| *k)
            else {
                return;
            };
            if let Some(e) = self.app_cache.remove(&victim) {
                self.app_bytes -= e.data.len();
                self.app_evictions += 1;
            }
        }
    }

    /// Drop every cached mask. Only the tests use it; the set is closed
    /// and a running desktop has no reason to.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.bytes = 0;
        self.app_cache.clear();
        self.app_bytes = 0;
    }

    /// Append the `key value` pairs for the `stats` reply.
    pub fn write_pairs(&self, out: &mut Vec<(&'static str, u64)>) {
        // `icons_cached` is distinct `(name, px)` pairs actually in use,
        // which is the number to compare against what is on screen;
        // `icon_renders` is how many times one had to be rasterised, and
        // it is the interesting one: after the first paint of a settled
        // desktop it must stop growing, or something is re-rasterising
        // per frame. `icon_bytes` is what the two cost.
        out.push(("icons", nitro_icons::all().len() as u64));
        out.push(("icons_cached", self.cache.len() as u64));
        out.push(("icon_renders", self.renders));
        out.push(("icon_bytes", self.bytes as u64));
        out.push(("icon_refusals", self.refused));
        // The application half, which answers a different question: not
        // "is something re-rasterising" but "how much of somebody else's
        // artwork is this process holding, and what did reading it cost".
        // `app_icon_loads` is the one that must stop growing on a settled
        // desktop; `app_icon_decode_us_max` is the honest answer to "how
        // slow is the decoder on this box", measured rather than quoted.
        out.push(("app_icons_cached", self.app_cache.len() as u64));
        out.push(("app_icon_bytes", self.app_bytes as u64));
        out.push(("app_icon_loads", self.app_loads));
        out.push(("app_icon_misses", self.app_misses));
        out.push(("app_icon_evictions", self.app_evictions));
        out.push(("app_icon_decode_us_max", self.app_decode_us_max));
    }
}

/// The colour a `role` byte names, or `None` when the icon is to be drawn
/// in its own colours — which no symbolic icon has, so it draws nothing.
///
/// Out-of-range role indices fall back to [`Role::Text`] rather than
/// vanishing: a client one release ahead, naming a role this server does
/// not have, gets a visible icon in the default text colour instead of a
/// silent gap.
fn tint(role: u8, palette: &Palette) -> Option<Color> {
    if role == nitro_scene::IconRef::AS_COLOURED {
        return None;
    }
    let role = Role::from_index(role as usize).unwrap_or(Role::Text);
    Some(palette.get(role))
}

/// The device size an icon of `size` logical pixels is rasterised at on an
/// output of `scale`.
///
/// Rounded to a whole pixel and clamped: the icon grid is 16 units, so a
/// fractional device size would put the grid's own edges between pixels
/// and blur exactly the horizontal and vertical strokes the artwork is
/// made of.
#[must_use]
pub fn device_px(size: f32, scale: f32) -> u32 {
    let px = (size * scale).round();
    if !px.is_finite() {
        return MIN_PX;
    }
    (px as u32).clamp(MIN_PX, MAX_PX)
}

/// Resample a decoded PNG into a square `px × px` straight-alpha BGRA
/// tile, or `None` when there is nothing to resample.
///
/// Three things happen here, and each is a decision rather than a detail.
///
/// **The source is letterboxed, not stretched.** Theme icons are square
/// by convention but not by rule, and a 64×48 banner stretched into a
/// square is a logo nobody recognises. The aspect ratio is kept and the
/// result is centred in the square the layout reserved, which is the same
/// bargain the scene already makes when it centres an icon in its bounds.
///
/// **The resampling is written here rather than taken from
/// `nitro-raster`,** and that is not duplication: `Canvas` is a *screen*,
/// its pixels are `XRGB8888`, and its blitter writes a zero into the
/// fourth byte of every pixel it touches because a framebuffer has no
/// alpha to keep. Running an icon through it would produce a fully
/// transparent tile — which is exactly what the first version of this
/// function did, and what `an_application_icon_is_decoded_once_and_
/// blitted_in_its_own_colours` now catches. An icon tile is not a screen:
/// it is a source image that has to survive with its alpha intact until
/// it is composited.
///
/// **It is done once, at cache-fill time.** The tile the painter blits is
/// already at the device size, so the per-frame path is an
/// integer-aligned one-to-one copy through the real blitter.
fn square_tile(img: &nitro_png::Image, px: u32) -> Option<Vec<u8>> {
    if img.width == 0 || img.height == 0 || px == 0 {
        return None;
    }
    let need = (px as usize).checked_mul(px as usize)?.checked_mul(4)?;
    if img.data.len() < (img.width as usize * img.height as usize * 4) {
        return None;
    }
    // Fit inside the square, keeping the aspect ratio, and centre what is
    // left over. Both axes take the *same* factor, which is what makes
    // "is this a downscale" one question rather than two.
    let scale = (f64::from(px) / f64::from(img.width)).min(f64::from(px) / f64::from(img.height));
    let w = ((f64::from(img.width) * scale).floor() as u32).clamp(1, px);
    let h = ((f64::from(img.height) * scale).floor() as u32).clamp(1, px);
    let ox = (px - w) / 2;
    let oy = (px - h) / 2;

    let mut out = vec![0u8; need];
    let inner = resample(&img.data, img.width, img.height, w, h);
    for y in 0..h as usize {
        let src = y * w as usize * 4;
        let dst = (y + oy as usize) * px as usize * 4 + ox as usize * 4;
        out[dst..dst + w as usize * 4].copy_from_slice(&inner[src..src + w as usize * 4]);
    }
    Some(out)
}

/// Resample straight-alpha BGRA `src` from `sw × sh` to `dw × dh`.
///
/// Premultiplied throughout, and that is the whole reason this is not
/// three lines of averaging: mixing straight-alpha samples multiplies a
/// transparent pixel's *colour* into its neighbour, so a theme icon drawn
/// on a transparent black background — which is most of them — comes back
/// with a dark halo round every edge. Premultiplying before the mix and
/// dividing back out afterwards is the only correct order.
///
/// Two filters, picked by direction, because one filter cannot do both
/// jobs. **Downscaling** takes the area average of every source pixel the
/// destination pixel covers: a 48 px icon into a 16 px button is 9
/// samples per output pixel, and a bilinear 4-tap would simply miss five
/// ninths of the artwork — which is how thin strokes disappear.
/// **Upscaling** is bilinear: there is nothing to average, and
/// nearest-neighbour would make the blocky doubled tile `docs/icons.md`
/// spends a section arguing against.
fn resample(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    let px = |x: u32, y: u32| -> [u32; 4] {
        let o = (y as usize * sw as usize + x as usize) * 4;
        let a = u32::from(src[o + 3]);
        // Premultiplied, rounded rather than truncated: an 8-bit channel
        // truncated twice (here and on the way out) loses a level per
        // resample, which shows up as a tile that is visibly darker than
        // its source.
        [
            (u32::from(src[o]) * a + 127) / 255,
            (u32::from(src[o + 1]) * a + 127) / 255,
            (u32::from(src[o + 2]) * a + 127) / 255,
            a,
        ]
    };
    let write = |out: &mut [u8], i: usize, acc: [u64; 4], weight: u64| {
        // Never zero on either path — the area box is at least one pixel
        // and the bilinear weights sum to 256² — but clamped rather than
        // asserted, because a divide by zero here would be a panic in the
        // paint path and a wrong pixel is not worth one.
        let weight = weight.max(1);
        let a = ((acc[3] + weight / 2) / weight) as u32;
        let unpremul = |c: u64| -> u8 {
            if a == 0 {
                return 0;
            }
            let v = ((c + weight / 2) / weight) as u32;
            ((v * 255 + a / 2) / a).min(255) as u8
        };
        out[i] = unpremul(acc[0]);
        out[i + 1] = unpremul(acc[1]);
        out[i + 2] = unpremul(acc[2]);
        out[i + 3] = a as u8;
    };

    let downscale = dw <= sw && dh <= sh;
    for y in 0..dh {
        for x in 0..dw {
            let i = (y as usize * dw as usize + x as usize) * 4;
            if downscale {
                // The half-open source box this destination pixel covers,
                // never empty: a 1:1 axis gives exactly one column.
                let x0 = (u64::from(x) * u64::from(sw) / u64::from(dw)) as u32;
                let x1 = (((u64::from(x) + 1) * u64::from(sw)).div_ceil(u64::from(dw)) as u32)
                    .clamp(x0 + 1, sw);
                let y0 = (u64::from(y) * u64::from(sh) / u64::from(dh)) as u32;
                let y1 = (((u64::from(y) + 1) * u64::from(sh)).div_ceil(u64::from(dh)) as u32)
                    .clamp(y0 + 1, sh);
                let mut acc = [0u64; 4];
                let mut n = 0u64;
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let p = px(sx, sy);
                        for (a, v) in acc.iter_mut().zip(p) {
                            *a += u64::from(v);
                        }
                        n += 1;
                    }
                }
                write(&mut out, i, acc, n);
            } else {
                // Sample positions are pixel *centres*, which is why the
                // half pixels are there: without them the filter is
                // shifted by half an output pixel and a symmetric icon
                // comes back lopsided.
                let fx = (f64::from(x) + 0.5) * f64::from(sw) / f64::from(dw) - 0.5;
                let fy = (f64::from(y) + 0.5) * f64::from(sh) / f64::from(dh) - 0.5;
                let x0 = fx.floor().clamp(0.0, f64::from(sw - 1)) as u32;
                let y0 = fy.floor().clamp(0.0, f64::from(sh - 1)) as u32;
                let x1 = (x0 + 1).min(sw - 1);
                let y1 = (y0 + 1).min(sh - 1);
                // 8-bit weights, so the whole mix is integer arithmetic
                // and the total weight is exactly 256 * 256.
                let tx = ((fx - f64::from(x0)).clamp(0.0, 1.0) * 256.0).round() as u64;
                let ty = ((fy - f64::from(y0)).clamp(0.0, 1.0) * 256.0).round() as u64;
                let corners = [
                    (px(x0, y0), (256 - tx) * (256 - ty)),
                    (px(x1, y0), tx * (256 - ty)),
                    (px(x0, y1), (256 - tx) * ty),
                    (px(x1, y1), tx * ty),
                ];
                let mut acc = [0u64; 4];
                for (p, w) in corners {
                    for (a, v) in acc.iter_mut().zip(p) {
                        *a += u64::from(v) * w;
                    }
                }
                write(&mut out, i, acc, 256 * 256);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> IconEngine {
        IconEngine::new()
    }

    /// An empty fixture tree under the temporary directory, wiped first so
    /// a previous run's files cannot make this one pass.
    ///
    /// It carries the minimum `index.theme` that makes `hicolor` a real
    /// theme — four fixed-size directories — so a lookup takes the spec's
    /// path rather than the fallback scan.
    fn fixture(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nitro-app-icons-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("hicolor")).expect("the fixture directory");
        std::fs::write(
            dir.join("hicolor").join("index.theme"),
            "[Icon Theme]\n\
             Directories=16x16/apps,24x24/apps,32x32/apps,48x48/apps\n\
             [16x16/apps]\nSize=16\nType=Fixed\n\
             [24x24/apps]\nSize=24\nType=Fixed\n\
             [32x32/apps]\nSize=32\nType=Fixed\n\
             [48x48/apps]\nSize=48\nType=Fixed\n",
        )
        .expect("the fixture index.theme");
        dir
    }

    /// An engine whose search path is exactly `dir` — nothing the box
    /// happens to have installed can reach these tests.
    fn themed(dir: &std::path::Path) -> IconEngine {
        IconEngine {
            theme: IconTheme::with_dirs(vec![dir.to_path_buf()], "hicolor"),
            ..IconEngine::default()
        }
    }

    /// Write `bytes` to `rel` under `dir`, creating the directories.
    fn install_bytes(dir: &std::path::Path, rel: &str, bytes: &[u8]) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the icon directory");
        }
        std::fs::write(&path, bytes).expect("the icon file");
    }

    /// Install a `side × side` PNG of one solid BGR colour at `rel`.
    fn install_png(dir: &std::path::Path, rel: &str, side: u32, bgr: (u8, u8, u8)) {
        install_bytes(dir, rel, &encode_rgba(side, side, bgr));
    }

    /// A `width × height` RGBA PNG of one opaque colour, given in the
    /// **BGR** order the decoder returns so a test can compare what it
    /// wrote with what it read without reversing anything in its head.
    ///
    /// Stored deflate blocks and unfiltered scanlines: this is a fixture
    /// writer, not a compressor, and every byte of it is checked by the
    /// decoder the moment it is used.
    fn encode_rgba(width: u32, height: u32, bgr: (u8, u8, u8)) -> Vec<u8> {
        let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
        for _ in 0..height {
            // Filter byte 0 ("none"), then the row.
            raw.extend_from_slice(&[0u8]);
            for _ in 0..width {
                raw.extend_from_slice(&[bgr.2, bgr.1, bgr.0, 0xff]);
            }
        }
        let mut zlib = vec![0x78u8, 0x01];
        for (i, block) in raw.chunks(65_535).enumerate() {
            let last = (i + 1) * 65_535 >= raw.len();
            zlib.push(u8::from(last));
            let len = block.len() as u16;
            zlib.extend_from_slice(&len.to_le_bytes());
            zlib.extend_from_slice(&(!len).to_le_bytes());
            zlib.extend_from_slice(block);
        }
        zlib.extend_from_slice(&nitro_png::adler32(&raw).to_be_bytes());

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut chunk = |kind: &[u8; 4], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        // depth 8, colour type 6 (RGBA), deflate, adaptive filtering, no
        // interlace — the shape every icon in every theme is written in.
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &zlib);
        chunk(b"IEND", &[]);
        out
    }

    /// CRC-32 as PNG defines it, for [`encode_rgba`]'s chunks.
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        c ^ 0xFFFF_FFFF
    }

    #[test]
    fn the_set_is_compiled_in_and_names_resolve() {
        let e = engine();
        assert!(e.has_icons());
        assert!(e.lookup("gear").is_some());
        assert_eq!(e.lookup("no-such-icon"), None);
    }

    #[test]
    fn a_device_size_is_the_logical_size_times_the_output_scale() {
        assert_eq!(device_px(16.0, 1.0), 16);
        assert_eq!(device_px(16.0, 2.0), 32);
        // A fractional scale still lands on a whole pixel.
        assert_eq!(device_px(16.0, 1.5), 24);
        // Hostile inputs are clamped rather than propagated.
        assert_eq!(device_px(f32::NAN, 1.0), MIN_PX);
        assert_eq!(device_px(1e9, 4.0), MAX_PX);
        assert_eq!(device_px(0.5, 1.0), MIN_PX);
    }

    #[test]
    fn the_same_icon_at_two_scales_is_rasterised_twice() {
        let mut e = engine();
        let icon = e.lookup("gear").expect("gear is in the set");
        assert!(e.mask(icon, 16).is_some());
        assert_eq!(e.renders, 1);
        assert!(e.mask(icon, 16).is_some());
        assert_eq!(e.renders, 1, "the second ask is a cache hit");
        assert!(e.mask(icon, 32).is_some());
        assert_eq!(e.renders, 2, "a different device size is a different mask");
        assert_eq!(e.cache.len(), 2);
    }

    #[test]
    fn a_role_is_resolved_against_the_palette_not_stored() {
        let light = Palette::light();
        let dark = Palette::dark();
        let a = tint(Role::Text.index() as u8, &light).expect("a role has a colour");
        let b = tint(Role::Text.index() as u8, &dark).expect("a role has a colour");
        assert_ne!(a, b, "the same role is a different colour in each scheme");
        // Out of range is the text colour, not nothing: a gap is worse
        // than a wrong-but-visible icon.
        assert_eq!(tint(250, &light), Some(light.get(Role::Text)));
        assert_eq!(tint(nitro_scene::IconRef::AS_COLOURED, &light), None);
    }

    #[test]
    fn a_tint_is_not_part_of_the_cache_key() {
        // The property the theme test depends on: recolouring costs no
        // raster at all, because the cache holds coverage, not pixels.
        let mut e = engine();
        let icon = e.lookup("gear").expect("gear is in the set");
        assert!(e.mask(icon, 16).is_some());
        let before = e.renders;
        let mut buf = vec![0u8; 64 * 64 * 4];
        let clip = IRect::new(0, 0, 64, 64);
        for palette in [Palette::light(), Palette::dark()] {
            let mut canvas = Canvas::new(&mut buf, 64, 64, 64 * 4);
            e.paint(
                &mut canvas,
                &clip,
                &Transform::IDENTITY,
                icon,
                Point::new(0.0, 0.0),
                16.0,
                Role::Text.index() as u8,
                &palette,
                1.0,
            );
        }
        assert_eq!(e.renders, before);
    }

    #[test]
    fn painting_puts_ink_on_the_canvas_and_a_flipped_scheme_changes_it() {
        let mut e = engine();
        let icon = e.lookup("gear").expect("gear is in the set");
        let clip = IRect::new(0, 0, 32, 32);
        let mut shot = |palette: &Palette| {
            let mut buf = vec![0u8; 32 * 32 * 4];
            let mut canvas = Canvas::new(&mut buf, 32, 32, 32 * 4);
            e.paint(
                &mut canvas,
                &clip,
                &Transform::IDENTITY,
                icon,
                Point::new(0.0, 0.0),
                16.0,
                Role::Text.index() as u8,
                palette,
                1.0,
            );
            buf
        };
        let light = shot(&Palette::light());
        let dark = shot(&Palette::dark());
        assert!(light.iter().any(|b| *b != 0), "the icon painted something");
        assert_ne!(light, dark, "a scheme flip moves the icon's pixels");
    }

    #[test]
    fn an_as_coloured_role_selects_the_application_set_and_never_the_symbolic_one() {
        // The selector decision, asserted from the engine's side: a
        // symbolic handle painted with `AS_COLOURED` finds nothing,
        // because `AS_COLOURED` does not mean "this index, untinted" — it
        // means "this index is an *application* handle", and the
        // application set is empty here.
        let mut e = engine();
        let icon = e.lookup("gear").expect("gear is in the set");
        let mut buf = vec![0u8; 32 * 32 * 4];
        let mut canvas = Canvas::new(&mut buf, 32, 32, 32 * 4);
        e.paint(
            &mut canvas,
            &IRect::new(0, 0, 32, 32),
            &Transform::IDENTITY,
            icon,
            Point::new(0.0, 0.0),
            16.0,
            nitro_scene::IconRef::AS_COLOURED,
            &Palette::light(),
            1.0,
        );
        assert!(
            buf.iter().all(|b| *b == 0),
            "a symbolic index is not an application handle"
        );
        assert_eq!(e.renders, 0, "and nothing was rasterised for it either");
        assert_eq!(e.app_loads, 0);
    }

    #[test]
    fn an_application_icon_is_decoded_once_and_blitted_in_its_own_colours() {
        let dir = fixture("paints");
        // Solid `#3050c0`, which no palette role is: if the tile were
        // tinted rather than blitted, this exact colour could not appear.
        install_png(&dir, "hicolor/48x48/apps/testapp.png", 48, (0xc0, 0x50, 0x30));
        let mut e = themed(&dir);
        let icon = e
            .lookup_app("testapp")
            .expect("the fixture theme has testapp");
        let mut buf = vec![0u8; 32 * 32 * 4];
        let shot = |e: &mut IconEngine, buf: &mut Vec<u8>| {
            buf.fill(0);
            let mut canvas = Canvas::new(buf, 32, 32, 32 * 4);
            e.paint(
                &mut canvas,
                &IRect::new(0, 0, 32, 32),
                &Transform::IDENTITY,
                icon,
                Point::new(0.0, 0.0),
                24.0,
                nitro_scene::IconRef::AS_COLOURED,
                &Palette::light(),
                1.0,
            );
        };
        shot(&mut e, &mut buf);
        assert_eq!(e.app_loads, 1, "one decode");
        assert_eq!(e.app_cache.len(), 1);
        assert_eq!(e.app_bytes, 24 * 24 * 4);
        // The pixel at the tile's centre is the source colour, straight
        // through: BGRA in, BGRX out.
        let centre = 12 * 32 * 4 + 12 * 4;
        assert_eq!(
            &buf[centre..centre + 3],
            &[0xc0, 0x50, 0x30],
            "the icon's own colours, not a tint"
        );
        // And the second paint is free — the thing the "decode lazily,
        // never per frame" claim rests on.
        let before = e.app_loads;
        for _ in 0..8 {
            shot(&mut e, &mut buf);
        }
        assert_eq!(e.app_loads, before, "a settled desktop decodes nothing");
        assert!(e.app_decode_us_max > 0, "the decode was timed");
    }

    #[test]
    fn each_device_size_is_its_own_tile_and_prefers_the_source_made_for_it() {
        // Two sources, one per size, which is how a real theme ships an
        // application. A 16 px button must read the 16 px file rather than
        // downscale the 48, and the two are separate cache entries for the
        // same reason two glyph sizes are.
        let dir = fixture("sizes");
        install_png(&dir, "hicolor/16x16/apps/testapp.png", 16, (0x10, 0x20, 0x30));
        install_png(&dir, "hicolor/48x48/apps/testapp.png", 48, (0x40, 0x50, 0x60));
        let mut e = themed(&dir);
        let icon = e.lookup_app("testapp").expect("testapp resolves");
        let small = e.app_tile(icon, 16).expect("16 px decodes").data.clone();
        let big = e.app_tile(icon, 48).expect("48 px decodes").data.clone();
        assert_eq!(e.app_loads, 2);
        assert_eq!(e.app_cache.len(), 2);
        assert_eq!(small.len(), 16 * 16 * 4);
        assert_eq!(big.len(), 48 * 48 * 4);
        assert_eq!(&small[..3], &[0x10, 0x20, 0x30], "the 16 px file");
        assert_eq!(&big[..3], &[0x40, 0x50, 0x60], "the 48 px file");
        assert_eq!(e.app_bytes, 16 * 16 * 4 + 48 * 48 * 4);
    }

    #[test]
    fn an_unknown_application_name_is_missing_and_is_not_looked_up_twice() {
        let dir = fixture("missing");
        install_png(&dir, "hicolor/48x48/apps/testapp.png", 48, (1, 2, 3));
        let mut e = themed(&dir);
        assert_eq!(e.lookup_app("no-such-application"), None);
        assert_eq!(e.lookup_app(""), None, "an empty name clears, never looks");
        // The name that *does* exist still works afterwards — a failed
        // lookup must not poison the index.
        assert!(e.lookup_app("testapp").is_some());
        // And a file that is not a PNG is a miss with a log line, not a
        // panic and not a retry on every frame.
        std::fs::write(dir.join("hicolor/48x48/apps/broken.png"), b"not a png")
            .expect("write the broken fixture");
        let broken = e.lookup_app("broken").expect("the file is there to find");
        assert!(e.app_tile(broken, 24).is_none());
        assert_eq!(e.app_misses, 1);
        assert!(e.app_tile(broken, 24).is_none());
        assert_eq!(e.app_misses, 2, "each size asks once");
        assert_eq!(e.app_loads, 0, "nothing decoded");
    }

    #[test]
    fn a_non_square_source_keeps_its_aspect_ratio() {
        // A stretched logo is a logo nobody recognises, so the tile is
        // letterboxed: the 32×16 source lands 32 wide and 16 tall inside a
        // 32 px square, with transparent bands above and below.
        let dir = fixture("aspect");
        let png = encode_rgba(32, 16, (0x11, 0x22, 0x33));
        install_bytes(&dir, "hicolor/32x32/apps/wide.png", &png);
        let mut e = themed(&dir);
        let icon = e.lookup_app("wide").expect("wide resolves");
        let tile = e.app_tile(icon, 32).expect("decodes").data.clone();
        let at = |x: usize, y: usize| tile[(y * 32 + x) * 4 + 3];
        assert_eq!(at(16, 16), 0xff, "the middle band is the artwork");
        assert_eq!(at(16, 0), 0, "and above it is transparent");
        assert_eq!(at(16, 31), 0, "as is below it");
    }

    #[test]
    fn the_application_cache_evicts_the_least_recently_used_tile() {
        // The bound the symbolic cache does not need: the set of
        // applications on a machine is open, so this one has an LRU rather
        // than a refusal. Driven past the cap with `evict_app_to` directly,
        // because filling 4 MiB with real PNGs would be a slow test that
        // asserted the same thing.
        let dir = fixture("evict");
        for n in 0..3 {
            install_png(
                &dir,
                &format!("hicolor/48x48/apps/app{n}.png"),
                48,
                (n as u8, 0x20, 0x30),
            );
        }
        let mut e = themed(&dir);
        let mut handles = Vec::new();
        for n in 0..3 {
            let h = e
                .lookup_app(&format!("app{n}"))
                .expect("the fixture has it");
            e.app_tile(h, 48).expect("decodes");
            handles.push(h);
        }
        assert_eq!(e.app_cache.len(), 3);
        // Touch the first one, so it is no longer the coldest.
        e.app_tile(handles[0], 48).expect("a cache hit");
        let one = 48 * 48 * 4;
        e.evict_app_to(2 * one);
        assert_eq!(e.app_evictions, 1);
        assert_eq!(e.app_bytes, 2 * one);
        assert!(
            e.app_cache.contains_key(&(handles[0], 48)),
            "the touched tile survived"
        );
        assert!(
            !e.app_cache.contains_key(&(handles[1], 48)),
            "the coldest went"
        );
        // Down to nothing, and the accounting still adds up.
        e.evict_app_to(0);
        assert_eq!(e.app_bytes, 0);
        assert!(e.app_cache.is_empty());
    }

    #[test]
    fn changing_the_theme_drops_the_tiles_and_keeps_the_handles() {
        // A node in the scene holds an *index*, so an index may never
        // change meaning — but the file behind it may, which is the whole
        // point of `theme.icons`.
        let dir = fixture("retheme");
        install_png(&dir, "hicolor/48x48/apps/testapp.png", 48, (1, 2, 3));
        let mut e = themed(&dir);
        let icon = e.lookup_app("testapp").expect("testapp resolves");
        e.app_tile(icon, 24).expect("decodes");
        assert_eq!(e.app_cache.len(), 1);
        assert!(e.app_bytes > 0);
        e.set_theme("hicolor");
        assert!(e.app_cache.is_empty(), "the tiles went");
        assert_eq!(e.app_bytes, 0);
        assert_eq!(
            e.lookup_app("testapp"),
            Some(icon),
            "and the handle did not move"
        );
    }

    #[test]
    fn the_application_stats_keys_are_present_and_move() {
        let dir = fixture("stats");
        install_png(&dir, "hicolor/32x32/apps/testapp.png", 32, (9, 9, 9));
        let mut e = themed(&dir);
        let mut pairs = Vec::new();
        e.write_pairs(&mut pairs);
        for key in [
            "app_icons_cached",
            "app_icon_bytes",
            "app_icon_loads",
            "app_icon_misses",
            "app_icon_evictions",
            "app_icon_decode_us_max",
        ] {
            assert_eq!(
                pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v),
                Some(0),
                "{key} is missing or not zero on a fresh engine: {pairs:?}"
            );
        }
        let icon = e.lookup_app("testapp").expect("testapp resolves");
        e.app_tile(icon, 32).expect("decodes");
        let mut pairs = Vec::new();
        e.write_pairs(&mut pairs);
        let get = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map_or(0, |(_, v)| *v)
        };
        assert_eq!(get("app_icons_cached"), 1);
        assert_eq!(get("app_icon_bytes"), 32 * 32 * 4);
        assert_eq!(get("app_icon_loads"), 1);
        assert_eq!(get("app_icon_misses"), 0);
    }

    #[test]
    fn a_two_times_output_really_rasterises_at_two_times() {
        // The crispness claim, in the engine rather than on the box: the
        // 32 px mask is its own raster, not a doubled 16 px one, so its
        // anti-aliased edge has a pixel count a 2× blit cannot produce.
        let mut e = engine();
        let icon = e.lookup("circle-fill").expect("circle-fill is in the set");
        let small = e.mask(icon, 16).expect("16 px rasterises").coverage.clone();
        let big = e.mask(icon, 32).expect("32 px rasterises").coverage.clone();
        let edge = |m: &[u8]| m.iter().filter(|b| (1..=254).contains(*b)).count();
        // A nearest-neighbour 2× of the small mask has exactly four times
        // its intermediate-alpha pixels; a real 32 px raster has fewer,
        // because the circle's edge is a curve of length ~2× and not ~4×.
        assert!(
            edge(&big) < 4 * edge(&small),
            "32 px edge {} vs a 2x blit's {}",
            edge(&big),
            4 * edge(&small)
        );
        assert!(edge(&big) > edge(&small));
    }

    #[test]
    fn the_stats_keys_are_present_and_move() {
        let mut e = engine();
        let mut pairs = Vec::new();
        e.write_pairs(&mut pairs);
        let get = |pairs: &[(&'static str, u64)], key: &str| {
            pairs.iter().find(|(k, _)| *k == key).map_or_else(
                || panic!("stats key {key} is missing: {pairs:?}"),
                |(_, v)| *v,
            )
        };
        assert_eq!(get(&pairs, "icons_cached"), 0);
        assert_eq!(get(&pairs, "icon_renders"), 0);
        assert_eq!(get(&pairs, "icon_bytes"), 0);
        assert!(get(&pairs, "icons") > 0);

        let icon = e.lookup("list").expect("list is in the set");
        e.mask(icon, 16).expect("16 px rasterises");
        let mut pairs = Vec::new();
        e.write_pairs(&mut pairs);
        assert_eq!(get(&pairs, "icons_cached"), 1);
        assert_eq!(get(&pairs, "icon_renders"), 1);
        assert_eq!(get(&pairs, "icon_bytes"), 16 * 16);
        assert_eq!(get(&pairs, "icon_refusals"), 0);
    }

    #[test]
    fn a_whole_desktops_worth_of_icons_is_far_inside_the_cap() {
        // The bound the no-eviction decision rests on: every icon in the
        // set, at every size a desktop lays out at, on a 2x output.
        let mut e = engine();
        for i in 0..nitro_icons::all().len() as u32 {
            for px in [16u32, 24, 32, 48, 64, 96] {
                e.mask(i, px);
            }
        }
        assert_eq!(e.refused, 0);
        assert!(
            e.bytes < IconEngine::MAX_BYTES,
            "the whole set at six sizes is {} bytes, cap {}",
            e.bytes,
            IconEngine::MAX_BYTES
        );
    }
}
