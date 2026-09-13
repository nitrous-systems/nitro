//! The server-side glyph atlas: A8 coverage masks, shelf-packed into 1024×1024
//! pages.
//!
//! A [`GlyphKey`] quantizes (face, glyph, size, subpixel x) so that the same
//! glyph at the same size and phase is rasterized once. [`Atlas::get`] renders
//! on a miss and hands back a [`MaskInfo`] naming the page and rect; the
//! rasterizer blits from [`Atlas::page`].

use std::collections::HashMap;

use swash::FontRef;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Format, Vector};

use crate::db::{FontDb, FontId};

/// Cache key of one rendered glyph mask.
///
/// Size is quantized to 1/64 px and the subpixel x offset to quarter pixels,
/// so a run that slides horizontally reuses four masks per glyph instead of
/// one per position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    /// Face the glyph belongs to.
    pub font: FontId,
    /// Glyph id.
    pub glyph: u16,
    /// `round(size_px * 64)`.
    pub size_q: u32,
    /// Subpixel x phase, 0..=3 (quarter pixels).
    pub subpx: u8,
}

impl GlyphKey {
    /// Quantize a glyph request.
    ///
    /// `x` is the glyph's pen x; only its fractional part matters.
    #[must_use]
    pub fn new(font: FontId, glyph: u16, size_px: f32, x: f32) -> Self {
        let size_q = (size_px.max(0.0) * 64.0).round() as u32;
        let frac = x - x.floor();
        let subpx = ((frac * 4.0).round() as u32 & 3) as u8;
        Self {
            font,
            glyph,
            size_q,
            subpx,
        }
    }

    /// The size this key rasterizes at.
    #[must_use]
    pub fn size_px(self) -> f32 {
        self.size_q as f32 / 64.0
    }

    /// The subpixel offset this key rasterizes at: 0.0, 0.25, 0.5 or 0.75.
    #[must_use]
    pub fn subpixel_offset(self) -> f32 {
        f32::from(self.subpx) * 0.25
    }
}

/// Where a rendered mask lives in the atlas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskInfo {
    /// Page index, for [`Atlas::page`].
    pub page: u32,
    /// X of the mask's left edge in the page.
    pub x: u32,
    /// Y of the mask's top edge in the page.
    pub y: u32,
    /// Mask width in pixels.
    pub w: u32,
    /// Mask height in pixels.
    pub h: u32,
    /// X offset from the glyph origin to the mask's left edge.
    pub left: i32,
    /// Y offset from the glyph origin *upward* to the mask's top edge; the
    /// mask's top in a y-down space is `baseline - top`.
    pub top: i32,
}

/// A shelf-packed page of A8 coverage.
#[derive(Debug)]
struct Page {
    pixels: Vec<u8>,
    /// Y of the next free shelf.
    shelf_y: u32,
    /// Height of the current shelf.
    shelf_h: u32,
    /// X cursor within the current shelf.
    cursor_x: u32,
}

impl Page {
    fn new(size: u32) -> Self {
        Self {
            pixels: vec![0; (size as usize) * (size as usize)],
            shelf_y: 0,
            shelf_h: 0,
            cursor_x: 0,
        }
    }

    /// Reserve a `w × h` rect, 1 px of padding on the right and bottom.
    fn alloc(&mut self, size: u32, w: u32, h: u32) -> Option<(u32, u32)> {
        const PAD: u32 = 1;
        if w > size || h > size {
            return None;
        }
        if self.cursor_x + w > size {
            // Next shelf.
            self.shelf_y = self.shelf_y.checked_add(self.shelf_h)?;
            self.shelf_h = 0;
            self.cursor_x = 0;
        }
        if self.shelf_y + h > size {
            return None;
        }
        let (x, y) = (self.cursor_x, self.shelf_y);
        self.cursor_x += w + PAD;
        self.shelf_h = self.shelf_h.max(h + PAD);
        Some((x, y))
    }

    /// Copy a mask into the page.
    fn write(&mut self, size: u32, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
        for row in 0..h as usize {
            let src = row * w as usize;
            let dst = (y as usize + row) * size as usize + x as usize;
            let Some(src_row) = data.get(src..src + w as usize) else {
                return;
            };
            let Some(dst_row) = self.pixels.get_mut(dst..dst + w as usize) else {
                return;
            };
            dst_row.copy_from_slice(src_row);
        }
    }
}

/// One cached entry: where the mask is, plus the frame it was last used in.
///
/// `info` is `None` for a glyph that rasterized to nothing (a space): the
/// negative result is cached too, so a page full of spaces is not re-scaled
/// once per glyph per frame.
#[derive(Debug, Clone, Copy)]
struct Entry {
    info: Option<MaskInfo>,
    last_used: u64,
}

/// The glyph atlas.
///
/// Holds a `ScaleContext` (swash's scaler caches), the pages, and the key →
/// mask map. Never evicts: see the README for why, and for the memory bound.
pub struct Atlas {
    scale_cx: ScaleContext,
    pages: Vec<Page>,
    entries: HashMap<GlyphKey, Entry>,
    frame: u64,
    renders: u64,
}

impl Default for Atlas {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Atlas {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Atlas")
            .field("pages", &self.pages.len())
            .field("glyphs", &self.entries.len())
            .field("renders", &self.renders)
            .field("frame", &self.frame)
            .finish_non_exhaustive()
    }
}

impl Atlas {
    /// Page edge, in pixels. A page is `PAGE * PAGE` bytes = 1 MiB.
    pub const PAGE: u32 = 1024;

    /// Hinting is on at or below this size, where it earns its stem snapping.
    const HINT_MAX_PX: f32 = 24.0;

    /// An empty atlas with no pages allocated.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scale_cx: ScaleContext::new(),
            pages: Vec::new(),
            entries: HashMap::new(),
            frame: 0,
            renders: 0,
        }
    }

    /// The mask for `key`, rendering it on a miss.
    ///
    /// `None` when the glyph has no pixels (a space, a control character) or
    /// the face is not in `db`. An empty result is cached like any other, so
    /// asking again is a hit and does not re-rasterize; it still counts once
    /// in [`glyph_count`](Self::glyph_count).
    pub fn get(&mut self, db: &FontDb, key: GlyphKey) -> Option<MaskInfo> {
        let frame = self.frame;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = frame;
            return entry.info;
        }
        // A miss is the *only* thing that touches the font db's byte cache:
        // once a glyph is packed, redrawing it never needs the face again, so
        // an evicted face costs a re-read only when a new glyph turns up.
        let data = db.face(key.font)?;
        let font = data.font_ref()?;
        let info = self.render(&font, key);
        self.entries.insert(
            key,
            Entry {
                info,
                last_used: frame,
            },
        );
        info
    }

    /// Rasterize and pack one glyph. `None` for an empty mask.
    fn render(&mut self, font: &FontRef<'_>, key: GlyphKey) -> Option<MaskInfo> {
        let size = key.size_px();
        let mut scaler = self
            .scale_cx
            .builder(*font)
            .size(size)
            .hint(size <= Self::HINT_MAX_PX)
            .build();
        // Outlines only. `Source::ColorOutline` and `Source::ColorBitmap`
        // produce `Content::Color` — 4 bytes per pixel — which does not belong
        // in an A8 page; an emoji therefore renders as its alpha outline, or
        // as nothing when the face is bitmap-only. See the README (M3).
        let image = Render::new(&[Source::Bitmap(StrikeWith::BestFit), Source::Outline])
            .format(Format::Alpha)
            .offset(Vector::new(key.subpixel_offset(), 0.0))
            .render(&mut scaler, key.glyph)?;
        self.renders += 1;
        if image.content != Content::Mask {
            return None;
        }
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return None;
        }
        let (page, x, y) = self.alloc(w, h)?;
        self.pages[page as usize].write(Self::PAGE, x, y, w, h, &image.data);
        Some(MaskInfo {
            page,
            x,
            y,
            w,
            h,
            left: image.placement.left,
            top: image.placement.top,
        })
    }

    /// Find room for a `w × h` mask, opening a page when none fits.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32, u32)> {
        if let Some(last) = self.pages.len().checked_sub(1)
            && let Some((x, y)) = self.pages[last].alloc(Self::PAGE, w, h)
        {
            return Some((last as u32, x, y));
        }
        let mut page = Page::new(Self::PAGE);
        let (x, y) = page.alloc(Self::PAGE, w, h)?;
        self.pages.push(page);
        Some(((self.pages.len() - 1) as u32, x, y))
    }

    /// A page's pixels: `PAGE * PAGE` bytes, row stride `PAGE`.
    #[must_use]
    pub fn page(&self, index: u32) -> Option<&[u8]> {
        self.pages.get(index as usize).map(|p| p.pixels.as_slice())
    }

    /// Number of pages allocated.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Number of cached entries, including glyphs that rasterized to nothing.
    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.entries.len()
    }

    /// Total rasterizations performed — the miss counter.
    #[must_use]
    pub fn renders(&self) -> u64 {
        self.renders
    }

    /// Bump the frame stamp the LRU records. Call once per frame.
    pub fn next_frame(&mut self) {
        self.frame += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_quantizes_size_and_phase() {
        let key = GlyphKey::new(FontId(3), 42, 14.0, 10.5);
        assert_eq!(key.size_q, 896);
        assert!((key.size_px() - 14.0).abs() < f32::EPSILON);
        assert_eq!(key.subpx, 2);
        assert!((key.subpixel_offset() - 0.5).abs() < f32::EPSILON);
        // A whole-pixel x and the next whole pixel share a key.
        assert_eq!(
            GlyphKey::new(FontId(3), 42, 14.0, 10.0),
            GlyphKey::new(FontId(3), 42, 14.0, 11.0)
        );
        // 0.9 rounds to 4 quarters, which wraps to phase 0.
        assert_eq!(GlyphKey::new(FontId(0), 1, 14.0, 0.9).subpx, 0);
    }

    #[test]
    fn shelf_packing_fills_rows_then_moves_down() {
        let mut page = Page::new(64);
        let a = page.alloc(64, 10, 10).unwrap();
        let b = page.alloc(64, 10, 10).unwrap();
        assert_eq!(a, (0, 0));
        assert_eq!(b, (11, 0)); // 1 px padding
        // Fill the rest of the shelf: 0, 11, 22, 33, 44 fit; 55 + 10 > 64.
        for _ in 0..3 {
            page.alloc(64, 10, 10).unwrap();
        }
        let next = page.alloc(64, 10, 10).unwrap();
        assert_eq!(next, (0, 11));
    }

    #[test]
    fn a_page_reports_full_when_it_cannot_fit() {
        let mut page = Page::new(16);
        assert!(page.alloc(16, 20, 4).is_none());
        assert!(page.alloc(16, 16, 16).is_some());
        assert!(page.alloc(16, 16, 16).is_none());
    }
}
