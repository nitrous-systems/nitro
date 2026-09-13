//! The server's text engine: fonts, shaping, the glyph atlas and the store
//! that owns every shaped run a client has asked for.
//!
//! # Why the server shapes
//!
//! Clients send *strings and a style*, never glyph pixels and never glyph
//! ids. That is the whole reason the wire stays thin: a label is a few dozen
//! bytes over the link whatever the font, an app binary carries no font
//! library, and the remote case (`caps::REMOTE`) costs the same as the local
//! one. The price is that the server owns a font database, a shaper and a
//! glyph cache — all of which live in [`nitro_text`] and are assembled here
//! into the one [`TextEngine`] the event loop holds.
//!
//! # The three pieces
//!
//! * **`FontDb`** is scanned once at startup and never again. Fonts are not
//!   hot-reloaded: a font installed while the server runs is picked up at the
//!   next restart, which is what every other display server does too.
//! * **`TextStore`** owns the shaped runs. The scene deliberately does not:
//!   it holds a `TextRef` carrying an opaque `u32` key plus the measured
//!   size, so `nitro-scene` never sees a glyph or a font. Runs are keyed by
//!   their owning client so a disconnect drops all of them in one call.
//! * **`Atlas`** holds A8 coverage masks, rendered on first use and reused
//!   for ever after. It is shared across every client — a glyph is a glyph,
//!   and two apps at the same size and subpixel offset get the same bytes.
//!
//! # Coordinates
//!
//! Shaping happens in *logical* units at the style's `size_px`; painting
//! happens in *device* pixels. The bridge is [`TextEngine::paint`], which
//! reads the output scale out of the paint item's transform and rasterizes
//! the glyphs at `size_px * scale` — so a 2× output gets real 2× glyphs, not
//! a scaled-up 1× bitmap, without the scene or the rasterizer knowing that
//! text has a resolution at all.

use nitro_core::{Color, IRect, Point, Transform};
use nitro_raster::{Canvas, Mask};
use nitro_text::{Atlas, FontDb, GlyphKey, Layout, Metrics, ShapedText, TextKey, TextStore};

use crate::stats::Window;
use crate::{info, warn};

/// How many shaping calls the `shape_us_mean` statistic averages over.
pub const SHAPE_WINDOW: usize = 120;

/// The style of one text node or measurement request, as it arrives on the
/// wire, translated into what [`nitro_text`] wants.
///
/// A separate struct rather than two conversions because `SetText` and
/// `MeasureText` carry the same six style fields and must be interpreted
/// identically — a measurement that disagreed with the paint by one pixel
/// would be worse than no measurement at all.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleRequest {
    /// Font style: family, size, weight, italic.
    pub style: nitro_text::TextStyle,
    /// Wrap width in logical units, or `None` for "no limit" (wire 0.0).
    pub max_width: Option<f32>,
    /// Whether lines are broken at whitespace; only meaningful with a
    /// `max_width`.
    pub wrap: bool,
}

impl StyleRequest {
    /// Build a request from the wire's six style fields.
    ///
    /// `max_width` is clamped: a non-finite or non-positive width is "no
    /// limit", which is what the wire's 0.0 sentinel means, and keeps a NaN
    /// out of the line breaker. `size_px` is clamped into a sane range for
    /// the same reason — a client asking for a 10 000 px glyph would ask the
    /// atlas for a page of its own, and one asking for 0 would divide by it.
    #[must_use]
    pub fn new(
        family: &str,
        size_px: f32,
        weight: u16,
        italic: bool,
        max_width: f32,
        wrap: bool,
    ) -> Self {
        let size_px = if size_px.is_finite() {
            size_px.clamp(MIN_SIZE_PX, MAX_SIZE_PX)
        } else {
            DEFAULT_SIZE_PX
        };
        Self {
            style: nitro_text::TextStyle {
                family: nitro_text::Family::parse(family),
                size_px,
                weight,
                italic,
            },
            max_width: (max_width.is_finite() && max_width > 0.0).then_some(max_width),
            wrap,
        }
    }
}

/// Smallest font size the server will shape at. Below this a glyph is a
/// smudge, and the atlas key quantization stops being meaningful.
pub const MIN_SIZE_PX: f32 = 1.0;
/// Largest font size the server will shape at: one glyph must still fit in
/// an atlas page with room to spare.
pub const MAX_SIZE_PX: f32 = 256.0;
/// Size substituted for a non-finite one.
pub const DEFAULT_SIZE_PX: f32 = 14.0;

/// Longest string the server will shape, in bytes.
///
/// Shaping is O(n) but the glyph vector is not free, and a client is not
/// prevented from sending one `SetText` per frame. 64 KiB is a hundred pages
/// of prose: far past any label, far short of a memory problem.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// Everything the server needs to turn a string into pixels.
///
/// One instance lives in the event loop. It is not `Sync` and does not want
/// to be: shaping is on the commit path, which is single-threaded like
/// everything else here.
pub struct TextEngine {
    db: FontDb,
    layout: Layout,
    atlas: Atlas,
    store: TextStore,
    /// Microseconds per `shape` call, for the `shape_us_mean` statistic.
    shape_us: Window,
    /// Device-space glyph positions, reused across paint calls so a run of
    /// glyphs costs no allocation. See [`TextEngine::paint`].
    batch: Vec<(i32, i32, GlyphKey)>,
}

impl TextEngine {
    /// Scan the font directories and build an empty store and atlas.
    ///
    /// Logs the face count and how long the scan took: it is the one startup
    /// cost that depends on what is installed on the box rather than on
    /// anything nitro controls, so it is worth seeing in the journal. A box
    /// with no fonts at all is a warning, not a failure — the server still
    /// runs, text nodes simply draw nothing.
    #[must_use]
    pub fn new() -> Self {
        let db = FontDb::scan();
        let ms = db.scan_time().as_secs_f64() * 1000.0;
        if db.is_empty() {
            warn!("no fonts found ({ms:.1} ms scan); text nodes will draw nothing");
        } else {
            info!("fonts: {} faces in {ms:.1} ms", db.len());
        }
        Self {
            db,
            layout: Layout::new(),
            atlas: Atlas::new(),
            store: TextStore::new(),
            shape_us: Window::new(SHAPE_WINDOW),
            batch: Vec::new(),
        }
    }

    /// Whether any font was found at all.
    #[must_use]
    pub fn has_fonts(&self) -> bool {
        !self.db.is_empty()
    }

    /// Shape `text` and store the run under `owner`, returning its key and
    /// the shaped block.
    ///
    /// The caller puts the key in the scene's `TextRef` and hands the old
    /// key (if any) back to [`TextEngine::release`].
    ///
    /// # Panics
    /// Never in practice: the only `expect` is the lookup of the run that
    /// was inserted one line above, which the store cannot have lost.
    pub fn shape(
        &mut self,
        owner: u32,
        request: &StyleRequest,
        text: &str,
    ) -> (TextKey, &ShapedText) {
        let text = truncate(text);
        let start = std::time::Instant::now();
        let shaped = self.layout.shape(
            &self.db,
            text,
            &request.style,
            request.max_width,
            request.wrap,
        );
        self.shape_us
            .push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
        let key = self.store.insert(owner, shaped);
        // The store just took it, so the lookup cannot fail.
        let shaped = self
            .store
            .get(key)
            .expect("a run is present immediately after being inserted");
        (key, shaped)
    }

    /// Measure `text` without storing anything.
    ///
    /// This is the `MeasureText` path, answered the moment the message
    /// arrives rather than at the commit: a text field cannot lay itself out
    /// until it knows how wide its content is, and making it wait a frame for
    /// that would put a round trip in the middle of every keystroke.
    pub fn measure(&mut self, request: &StyleRequest, text: &str) -> Metrics {
        let text = truncate(text);
        let start = std::time::Instant::now();
        let metrics = self.layout.measure(
            &self.db,
            text,
            &request.style,
            request.max_width,
            request.wrap,
        );
        self.shape_us
            .push(u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX));
        metrics
    }

    /// Drop a stored run. A `None` key (a node that had no text) is a no-op.
    pub fn release(&mut self, key: Option<TextKey>) {
        if let Some(key) = key {
            self.store.remove(key);
        }
    }

    /// Drop every run a client owned: what a disconnect calls.
    pub fn release_owner(&mut self, owner: u32) {
        self.store.remove_owner(owner);
    }

    /// Advance the atlas's frame stamp; called once per painted frame.
    pub fn next_frame(&mut self) {
        self.atlas.next_frame();
    }

    /// Look up a stored run.
    #[must_use]
    pub fn run(&self, key: u32) -> Option<&ShapedText> {
        self.store.get(TextKey(key))
    }

    /// Draw one shaped run into `canvas`, clipped to `clip`.
    ///
    /// `origin` is the block's top-left corner in the node's local space
    /// (the scene applied the alignment); `transform` maps that space to
    /// device pixels. Glyphs are rasterized at the *device* size, so an
    /// output scale of 2 produces real 2× glyphs.
    ///
    /// Two passes on purpose. The first resolves every glyph to an atlas
    /// entry, which may rasterize and therefore needs `&mut self`; the
    /// second blits, which needs to borrow the atlas pages immutably. A
    /// single pass would have to drop and retake the borrow per glyph, and
    /// the split also gives the batch blit a run of glyphs sharing one
    /// colour, which is exactly the shape `blit_masks` wants.
    #[allow(clippy::too_many_arguments)] // Eight positional facts about one glyph run; a struct here would only be this argument list with a name.
    pub fn paint(
        &mut self,
        canvas: &mut Canvas<'_>,
        clip: &IRect,
        transform: &Transform,
        key: u32,
        origin: Point,
        color: Color,
        opacity: f32,
    ) {
        if color.is_transparent() || opacity <= 0.0 || clip.is_empty() {
            return;
        }
        let Some(run) = self.store.get(TextKey(key)) else {
            return;
        };
        // The world transform is axis-aligned (the rasterizer says so), so
        // one factor describes the scale from logical units to pixels.
        let scale = transform.a.abs().max(transform.b.abs());
        if !(scale.is_finite() && scale > 0.0) {
            return;
        }
        let device_size = (run.size_px * scale).clamp(MIN_SIZE_PX, MAX_SIZE_PX);

        let mut batch = std::mem::take(&mut self.batch);
        batch.clear();
        for line in &run.lines {
            for glyph in &line.glyphs {
                let pen = transform.apply(Point::new(
                    origin.x + glyph.x,
                    origin.y + line.baseline + glyph.y,
                ));
                if !(pen.x.is_finite() && pen.y.is_finite()) {
                    continue;
                }
                batch.push((
                    // `floor`, not `round`: the fractional part is what the
                    // key's subpixel bucket carries, so the two together
                    // reconstruct the exact pen position.
                    pen.x.floor() as i32,
                    pen.y.round() as i32,
                    GlyphKey::new(glyph.font, glyph.id, device_size, pen.x),
                ));
            }
        }

        for (pen_x, baseline_y, key) in &batch {
            let Some(info) = self.atlas.get(&self.db, *key) else {
                continue;
            };
            let Some(page) = self.atlas.page(info.page) else {
                continue;
            };
            let offset = (info.y * Atlas::PAGE + info.x) as usize;
            let Some(data) = page.get(offset..) else {
                continue;
            };
            let mask = Mask {
                data,
                w: info.w,
                h: info.h,
                stride: Atlas::PAGE,
            };
            canvas.blit_mask(
                clip,
                pen_x + info.left,
                baseline_y - info.top,
                &mask,
                color,
                opacity,
            );
        }

        batch.clear();
        self.batch = batch;
    }

    /// Append the `key value` pairs for the `stats` reply.
    pub fn write_pairs(&self, out: &mut Vec<(&'static str, u64)>) {
        out.push(("fonts", self.db.len() as u64));
        out.push(("glyphs_cached", self.atlas.glyph_count() as u64));
        out.push(("glyph_renders", self.atlas.renders()));
        out.push(("atlas_pages", self.atlas.page_count() as u64));
        out.push(("text_runs", self.store.len() as u64));
        out.push(("shape_us_mean", self.shape_us.mean()));
    }
}

impl Default for TextEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary only: the font bytes, the atlas pages and every shaped run would
/// be megabytes of hex in a log line, and none of it is what a reader of
/// `{:?}` on the server state wants to see.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for TextEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextEngine")
            .field("fonts", &self.db.len())
            .field("runs", &self.store.len())
            .field("atlas_pages", &self.atlas.page_count())
            .finish()
    }
}

/// Cut a string to [`MAX_TEXT_BYTES`] at a char boundary.
///
/// Truncating rather than erroring is deliberate: an over-long string is a
/// client bug, not an attack the connection has to die for, and a label that
/// is merely cut off is a far more debuggable symptom than a disconnect.
fn truncate(text: &str) -> &str {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SIZE_PX, MAX_SIZE_PX, MAX_TEXT_BYTES, MIN_SIZE_PX, StyleRequest, truncate,
    };

    /// `a == b` for two `f32`s that are meant to be bit-identical: these
    /// are clamps of literals, not the result of arithmetic.
    fn same(a: f32, b: f32) -> bool {
        a.to_bits() == b.to_bits()
    }

    #[test]
    fn a_style_request_normalises_the_wire_fields() {
        let r = StyleRequest::new("sans", 14.0, 400, false, 0.0, true);
        assert_eq!(r.max_width, None, "0.0 means no limit");
        assert!(r.wrap);
        assert_eq!(r.style.family, nitro_text::Family::Sans);

        let r = StyleRequest::new("Fancy Face", 14.0, 700, true, 200.0, false);
        assert_eq!(r.max_width, Some(200.0));
        assert_eq!(
            r.style.family,
            nitro_text::Family::Named("Fancy Face".into())
        );
        assert_eq!(r.style.weight, 700);
        assert!(r.style.italic);
    }

    #[test]
    fn hostile_floats_never_reach_the_shaper() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let r = StyleRequest::new("sans", bad, 400, false, bad, true);
            assert!(same(r.style.size_px, DEFAULT_SIZE_PX));
            assert_eq!(r.max_width, None);
        }
        assert!(same(
            StyleRequest::new("sans", 0.0, 400, false, -5.0, true)
                .style
                .size_px,
            MIN_SIZE_PX
        ));
        assert!(same(
            StyleRequest::new("sans", 1e9, 400, false, 0.0, true)
                .style
                .size_px,
            MAX_SIZE_PX
        ));
        assert_eq!(
            StyleRequest::new("sans", 14.0, 400, false, -1.0, true).max_width,
            None
        );
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        let short = "hello";
        assert_eq!(truncate(short), short);
        // A multi-byte char straddling the cut must not be split.
        let long = "é".repeat(MAX_TEXT_BYTES);
        let cut = truncate(&long);
        assert!(cut.len() <= MAX_TEXT_BYTES);
        assert!(long.starts_with(cut));
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
