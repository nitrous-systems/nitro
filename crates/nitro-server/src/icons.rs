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

use std::collections::HashMap;

use nitro_core::{Color, IRect, Palette, Point, Role, Transform};
use nitro_raster::{Canvas, Mask};

/// Smallest device size an icon is rasterised at. Below this the mask is
/// a smudge and the coverage is meaningless.
pub const MIN_PX: u32 = 4;
/// Largest device size an icon is rasterised at: 256 logical pixels on a
/// 2× output, which is far past any desktop use and short of a memory
/// problem (512² = 256 KiB for one mask).
pub const MAX_PX: u32 = 512;

/// One rasterised icon: an A8 coverage mask and its square side.
#[derive(Debug)]
struct Entry {
    px: u32,
    coverage: Vec<u8>,
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

    /// An empty engine. Rasterises nothing until something is painted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    /// Draw icon `icon` into `canvas`, clipped to `clip`.
    ///
    /// `origin` is the icon box's top-left corner in the node's local
    /// space (the scene centred it in the bounds); `transform` maps that
    /// space to device pixels. The mask is rasterised at the **device**
    /// size, so a 2× output gets a real 2× icon.
    ///
    /// `role` is resolved against `palette` here rather than stored, which
    /// is what makes a scheme switch free.
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
        let Some(color) = tint(role, palette) else {
            return;
        };
        if color.is_transparent() {
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
        canvas.blit_mask(
            clip,
            pen.x.round() as i32,
            pen.y.round() as i32,
            &mask,
            color,
            opacity,
        );
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

    /// Drop every cached mask. Only the tests use it; the set is closed
    /// and a running desktop has no reason to.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.bytes = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> IconEngine {
        IconEngine::new()
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
    fn an_as_coloured_icon_paints_nothing_yet() {
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
            "full-colour icons are icons-B; until then the node is empty"
        );
        assert_eq!(e.renders, 0, "and nothing was rasterised for it either");
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
