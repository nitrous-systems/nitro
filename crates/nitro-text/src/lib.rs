//! `nitro-text` — server-side text: font discovery, shaping and layout,
//! measurement, and an A8 glyph atlas.
//!
//! Clients send **strings and a style** over the wire; the server shapes them
//! and rasterizes the glyphs. That is what keeps the remote link thin — no
//! pixels and no font files cross it — and it is why this crate lives on the
//! server side of `nitro-wire`.
//!
//! Four types do the work:
//!
//! * [`FontDb`] — one recursive scan of the font directories at startup, then
//!   an immutable index. [`FontDb::select`] resolves a [`TextStyle`] to a
//!   [`FontId`]; [`FontDb::fallbacks`] gives the chain to try per run.
//! * [`Layout`] — owns swash's shaping caches. [`Layout::shape`] produces a
//!   [`ShapedText`] (lines of positioned [`Glyph`]s), [`Layout::measure`] the
//!   same box plus cursor positions.
//! * [`Atlas`] — renders each (face, glyph, size, subpixel phase) once into a
//!   shelf-packed 1024×1024 A8 page and hands back a [`MaskInfo`] for the
//!   rasterizer to blit.
//! * [`TextStore`] — keeps shaped text per client id so a disconnect drops it
//!   all in one call.
//!
//! ```no_run
//! use nitro_text::{Atlas, FontDb, GlyphKey, Layout, TextStyle};
//!
//! let db = FontDb::scan();
//! let mut layout = Layout::new();
//! let mut atlas = Atlas::new();
//!
//! let style = TextStyle::default(); // sans, 14 px, weight 400, upright
//! let text = layout.shape(&db, "Hello, nitro", &style, Some(300.0), true);
//!
//! for line in &text.lines {
//!     for glyph in &line.glyphs {
//!         let key = GlyphKey::new(glyph.font, glyph.id, text.size_px, glyph.x);
//!         if let Some(mask) = atlas.get(&db, key) {
//!             // Blit `mask.w × mask.h` bytes from `atlas.page(mask.page)` at
//!             // (glyph.x.floor() + mask.left, line.baseline - mask.top).
//!             let _ = (mask, atlas.page(mask.page));
//!         }
//!     }
//! }
//! atlas.next_frame();
//! ```
//!
//! # Contract
//!
//! - **Coordinate space.** Device pixels, y down. A [`Glyph`]'s `x`/`y` are
//!   relative to its line's start and baseline; a [`Line`]'s `baseline` is
//!   relative to the block's top. So a glyph's pen position inside the block is
//!   `(glyph.x, line.baseline + glyph.y)`, and the mask's top-left corner is
//!   `(pen.x.floor() + mask.left, pen.y - mask.top)`.
//! - **Quantization.** The atlas key rounds the size to 1/64 px and the pen's
//!   fractional x to quarter pixels, so sliding text reuses four masks per
//!   glyph. Blit at `x.floor()`; the phase is baked into the mask.
//! - **Determinism.** Same db, same style, same string ⇒ same layout. Nothing
//!   here reads the clock or the locale.
//! - **Dependencies.** `swash` only. No `nitro-core`, no `nitro-scene`, no
//!   `nitro-wire`, no fontconfig, no logger — the server logs
//!   [`FontDb::len`] and [`FontDb::scan_time`] itself.
//!
//! # Limitations (M2)
//!
//! - **LTR only, no bidi.** The whole string is shaped left-to-right with one
//!   script, detected from the first strong character.
//! - **No rich text.** One [`TextStyle`] per call; a run of mixed styles is
//!   several calls, laid out by the caller.
//! - **Fallback is per run, not per cluster.** A contiguous stretch of
//!   characters is shaped with the first face in the chain that maps its
//!   characters; shaping state is not carried across a fallback boundary.
//! - **No colour glyphs in the returned mask.** Emoji render through the
//!   alpha path; see the README.
//! - **The atlas does not evict.** It records an LRU frame stamp and adds
//!   pages; see the README for the bound.

#![forbid(unsafe_code)]

mod atlas;
mod db;
mod layout;
mod store;

pub use atlas::{Atlas, GlyphKey, MaskInfo};
pub use db::{Family, FontDb, FontId, TextStyle};
pub use layout::{Glyph, Layout, Line, Metrics, ShapedText};
pub use store::{TextKey, TextStore};
