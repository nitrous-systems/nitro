//! The [`Canvas`] surface and its drawing operations.

use nitro_core::{Color, IRect, Point, Rect};

use crate::blend::{div255, effective_alpha, over_premul, over_straight, unit_u8};
use crate::paint::{
    RowPaint, blend_mask_row, blend_mask_row_opaque, blend_pixel, blend_solid, lerp_color, mix,
    paint_cov, paint_full, store_solid,
};
use crate::shape::{RRect, RowSpans};

/// A source pixel: `b`/`g`/`r` plus straight or premultiplied alpha,
/// depending on who produced it (see each function's docs).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Texel {
    pub(crate) b: u32,
    pub(crate) g: u32,
    pub(crate) r: u32,
    pub(crate) a: u32,
}

/// The already-clipped device bounds of one blit.
#[derive(Debug, Clone, Copy)]
struct BlitBounds {
    y0: i32,
    y1: i32,
    cx0: i32,
    cx1: i32,
}

/// The loop-invariant part of a mask blit: the tint and its source alpha.
#[derive(Debug, Clone, Copy)]
struct MaskSetup {
    /// The tint, straight alpha.
    color: Color,
    /// `color.a * opacity` in `0..=65_025`, the hoisted half of
    /// [`effective_alpha`](crate::blend::effective_alpha).
    ca: u32,
    /// Coverage 255 means "replace the pixel": store instead of blend.
    opaque: bool,
}

impl MaskSetup {
    /// `None` when the blit would paint nothing at all.
    fn new(color: Color, opacity: f32) -> Option<Self> {
        let opacity = unit_u8(opacity);
        if opacity == 0 || color.is_transparent() {
            return None;
        }
        Some(Self {
            color,
            ca: u32::from(color.a) * u32::from(opacity),
            opaque: color.is_opaque() && opacity == 255,
        })
    }
}

/// Maximum columns of one stroke band the pre-resolved fast path handles.
///
/// A band is the vertical (or, at the top and bottom, horizontal) side of a
/// border, so its width is the stroke width plus at most one partial pixel at
/// each end. 16 covers every border a UI draws; a thicker one falls back to
/// the general two-coverage walk, which is not the case this exists for.
const BAND_MAX: usize = 16;

/// One vertical band of a stroke, pre-resolved for every corner-free row.
///
/// Away from the corner arcs a stroke row is the same two runs of columns
/// with the same per-column coverage, row after row: the geometry does not
/// depend on `y` at all. So the blend is resolved all the way down to its
/// operands once — not just the coverage, but the *premultiplied source* the
/// blend adds and the `255 - a` it scales the destination by. Each row then
/// costs three multiply-adds per column and nothing else.
#[derive(Debug, Clone, Copy)]
struct Band {
    /// First device column.
    x0: i32,
    /// Number of columns; 0 means the band is clipped away entirely.
    n: usize,
    /// Per column: `[b, g, r] * a + 128` (the rounding term folded in) and
    /// `255 - a`, exactly the four values [`blend_solid`] hoists out of its
    /// row loop — here hoisted out of the whole band.
    premul: [[u32; 4]; BAND_MAX],
}

impl Band {
    /// A band that paints nothing.
    const EMPTY: Self = Self {
        x0: 0,
        n: 0,
        premul: [[0; 4]; BAND_MAX],
    };

    /// The columns of the interval `[a, b]` visible in `[cx0, cx1)`, each with
    /// its blend resolved from `round(color.a * coverage * opacity / 255²)`.
    ///
    /// `None` means "no fast path": the band spans more than [`BAND_MAX`]
    /// columns, so the table would not fit.
    fn new(a: f32, b: f32, cx0: i32, cx1: i32, color: Color, opacity: u8) -> Option<Self> {
        if b <= a || !(b - a).is_finite() {
            return Some(Self::EMPTY);
        }
        let (fa, cb) = (a.floor() as i32, b.ceil() as i32);
        if (cb - fa) as usize > BAND_MAX {
            return None;
        }
        let x0 = fa.max(cx0);
        let x1 = cb.min(cx1);
        if x0 >= x1 {
            return Some(Self::EMPTY);
        }
        let mut band = Self {
            x0,
            n: (x1 - x0) as usize,
            premul: [[0; 4]; BAND_MAX],
        };
        for (slot, px) in band.premul.iter_mut().zip(x0..x1) {
            let cov = (b.min(px as f32 + 1.0) - a.max(px as f32)).clamp(0.0, 1.0);
            let alpha = u32::from(effective_alpha(color.a, unit_u8(cov), opacity));
            *slot = [
                u32::from(color.b) * alpha + 128,
                u32::from(color.g) * alpha + 128,
                u32::from(color.r) * alpha + 128,
                255 - alpha,
            ];
        }
        Some(band)
    }

    /// One past the last column, for the overlap check.
    fn end(&self) -> i32 {
        self.x0 + i32::try_from(self.n).unwrap_or(i32::MAX)
    }
}

/// The corner-free rows of a stroke and the bands they paint.
#[derive(Debug, Clone, Copy)]
struct Straight {
    /// First row free of every corner arc, and vertically inside both shapes.
    y0: i32,
    /// One past the last such row.
    y1: i32,
    /// The left and right bands; either may be empty.
    bands: [Band; 2],
}

/// `u32 -> i32` for pixel dimensions, which are far below `i32::MAX`.
trait CastI32 {
    fn cast_i32(self) -> i32;
}

impl CastI32 for u32 {
    #[inline]
    fn cast_i32(self) -> i32 {
        self.min(i32::MAX.unsigned_abs()).cast_signed()
    }
}

/// Bytes per pixel of the only destination format: XRGB8888.
pub const BYTES_PER_PIXEL: usize = 4;

/// What a fill is painted with.
///
/// Colours are straight-alpha sRGB bytes; see the crate docs for the blending
/// and colour-space contract.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fill {
    /// One colour everywhere.
    Solid(Color),
    /// Linear gradient between two device-space points.
    ///
    /// The gradient parameter is the projection of the pixel centre onto
    /// `start -> end`, clamped to `[0, 1]` (pad). A degenerate gradient
    /// (`start == end`) paints `c1`.
    ///
    /// M1 restriction: the gradient axis must be horizontal or vertical.
    /// A diagonal axis is projected onto its dominant component; see the
    /// crate docs.
    Linear {
        /// Device-space point where the gradient is `c0`.
        start: Point,
        /// Device-space point where the gradient is `c1`.
        end: Point,
        /// Colour at `start`.
        c0: Color,
        /// Colour at `end`.
        c1: Color,
    },
}

impl Fill {
    /// Whether every pixel this fill produces is fully opaque.
    #[must_use]
    pub fn is_opaque(&self) -> bool {
        match self {
            Self::Solid(c) => c.is_opaque(),
            Self::Linear { c0, c1, .. } => c0.is_opaque() && c1.is_opaque(),
        }
    }

    /// Whether the fill paints nothing at all.
    #[must_use]
    pub fn is_transparent(&self) -> bool {
        match self {
            Self::Solid(c) => c.is_transparent(),
            Self::Linear { c0, c1, .. } => c0.is_transparent() && c1.is_transparent(),
        }
    }

    /// Specialise for device row `py`.
    fn row_paint(&self, py: i32) -> RowPaint {
        match *self {
            Self::Solid(c) => RowPaint::Solid(c),
            Self::Linear { start, end, c0, c1 } => {
                let dx = end.x - start.x;
                let dy = end.y - start.y;
                // Axis-aligned in M1: take the dominant component.
                if dx.abs() >= dy.abs() {
                    if dx.abs() < 1e-6 {
                        return RowPaint::Solid(c1);
                    }
                    // t at pixel centre x + 0.5.
                    RowPaint::Linear {
                        t0: (0.5 - start.x) / dx,
                        dt: 1.0 / dx,
                        c0,
                        c1,
                    }
                } else {
                    let t = ((py as f32 + 0.5) - start.y) / dy;
                    RowPaint::Solid(lerp_color(c0, c1, t.clamp(0.0, 1.0)))
                }
            }
        }
    }
}

/// Byte layout of a source [`Image`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// `[B, G, R, X]` — the unused byte is ignored, the pixel is opaque.
    Xrgb8888,
    /// `[B, G, R, A]` with **straight** (non-premultiplied) alpha.
    Argb8888,
}

impl PixelFormat {
    /// Whether every pixel of this format is opaque by construction.
    #[must_use]
    pub fn is_opaque(self) -> bool {
        matches!(self, Self::Xrgb8888)
    }
}

/// A borrowed source image for [`Canvas::blit`].
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    /// Pixel bytes; at least `(height - 1) * stride + width * 4` long.
    pub data: &'a [u8],
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes; `>= width * 4`.
    pub stride: u32,
    /// Byte layout of each pixel.
    pub format: PixelFormat,
}

impl Image<'_> {
    /// The whole image as a rect.
    #[must_use]
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width.cast_i32(), self.height.cast_i32())
    }

    /// Whether the declared geometry fits in `data`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let need = (self.height as usize - 1) * self.stride as usize
            + self.width as usize * BYTES_PER_PIXEL;
        self.stride as usize >= self.width as usize * BYTES_PER_PIXEL && self.data.len() >= need
    }

    /// Pixel at `(x, y)` as a [`Texel`] with straight alpha. Coordinates are
    /// clamped to the image (edge extend). Used by the tests as the reference
    /// addressing path; the blit loops index hoisted row slices instead.
    #[inline]
    #[cfg(test)]
    pub(crate) fn texel(&self, x: i32, y: i32) -> Texel {
        let cx = x.clamp(0, self.width.cast_i32() - 1) as usize;
        let cy = y.clamp(0, self.height.cast_i32() - 1) as usize;
        let off = cy * self.stride as usize + cx * BYTES_PER_PIXEL;
        let px = &self.data[off..off + 4];
        let alpha = match self.format {
            PixelFormat::Xrgb8888 => 255,
            PixelFormat::Argb8888 => u32::from(px[3]),
        };
        Texel {
            b: u32::from(px[0]),
            g: u32::from(px[1]),
            r: u32::from(px[2]),
            a: alpha,
        }
    }
}

/// An 8-bit coverage mask: one byte of alpha per pixel.
///
/// Rows are `stride` bytes apart, so a sub-rectangle of a bigger atlas page
/// can be blitted without a copy — which is exactly how the glyph painter
/// uses it: `data` starts at the sub-rect's first byte and runs to the end
/// of the page, so the *last* row is `w` bytes, not `stride`.
#[derive(Debug, Clone, Copy)]
pub struct Mask<'a> {
    /// Coverage bytes. At least `(h - 1) * stride + w` of them; see
    /// [`Mask::is_valid`].
    pub data: &'a [u8],
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// Bytes per row.
    pub stride: u32,
}

impl Mask<'_> {
    /// Whether the mask is usable: non-zero extent, a stride covering the
    /// width, and enough data to reach the last pixel.
    ///
    /// The bound is `(h - 1) * stride + w`, **not** `stride * h`, and the
    /// difference is the whole point of the type. A mask is a strided view
    /// into somebody else's buffer, and when it sits at the bottom of an
    /// atlas page the bytes after its last pixel simply do not exist: a
    /// glyph packed at `y + h == PAGE` with `x > 0` leaves only
    /// `PAGE * h - x` bytes behind it. Requiring a full final row rejected
    /// exactly those glyphs, and rejected them *silently* — they stopped
    /// being drawn with no counter moving and no error anywhere. The blit
    /// loop never reads past `(h - 1) * stride + w`, so that is the honest
    /// requirement.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.w == 0 || self.h == 0 || self.stride < self.w {
            return false;
        }
        let need = u64::from(self.h - 1) * u64::from(self.stride) + u64::from(self.w);
        self.data.len() as u64 >= need
    }
}

/// A mutable XRGB8888 destination buffer.
///
/// Byte order per pixel is `[B, G, R, X]`, matching the dumb buffers
/// `nitro-kms` hands out. The unused byte is written as 0.
#[derive(Debug)]
pub struct Canvas<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    stride: u32,
}

impl<'a> Canvas<'a> {
    /// Wrap a back buffer.
    ///
    /// # Panics
    /// If `stride < width * 4` or `data` is shorter than
    /// `(height - 1) * stride + width * 4`.
    #[must_use]
    pub fn new(data: &'a mut [u8], width: u32, height: u32, stride: u32) -> Self {
        assert!(
            stride as usize >= width as usize * BYTES_PER_PIXEL,
            "stride {stride} too small for width {width}"
        );
        if height > 0 && width > 0 {
            let need = (height as usize - 1) * stride as usize + width as usize * BYTES_PER_PIXEL;
            assert!(
                data.len() >= need,
                "buffer of {} bytes is too small; need {need}",
                data.len()
            );
        }
        Self {
            data,
            width,
            height,
            stride,
        }
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Row stride in bytes.
    #[must_use]
    pub fn stride(&self) -> u32 {
        self.stride
    }

    /// The whole surface as a clip rect.
    #[must_use]
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width.cast_i32(), self.height.cast_i32())
    }

    /// The pixel bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.data
    }

    /// The pixel bytes, mutably.
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.data
    }

    /// Columns `[x0, x1)` of row `y`, already bounds-checked by the caller.
    #[inline]
    fn row(&mut self, y: i32, x0: i32, x1: i32) -> &mut [u8] {
        let start = y as usize * self.stride as usize + x0 as usize * BYTES_PER_PIXEL;
        let len = (x1 - x0) as usize * BYTES_PER_PIXEL;
        &mut self.data[start..start + len]
    }

    /// `clip` intersected with the surface.
    #[inline]
    fn clip_to_surface(&self, clip: &IRect) -> IRect {
        clip.intersect(&self.bounds())
    }

    /// Fill an integer rect with one colour — the fast, blend-free path.
    ///
    /// A translucent colour is blended; an opaque one is stored. Nothing
    /// outside `clip` is touched.
    pub fn fill_irect(&mut self, clip: &IRect, rect: &IRect, color: Color) {
        let r = self.clip_to_surface(clip).intersect(rect);
        if r.is_empty() || color.is_transparent() {
            return;
        }
        let (x0, x1) = (r.x, r.right());
        if color.is_opaque() {
            for y in r.y..r.bottom() {
                store_solid(self.row(y, x0, x1), color);
            }
        } else {
            for y in r.y..r.bottom() {
                blend_solid(self.row(y, x0, x1), color, color.a);
            }
        }
    }

    /// Fill a rounded rect with anti-aliased edges.
    ///
    /// `corner_radius` is clamped to half the shorter side; 0 gives a sharp
    /// rect (still anti-aliased on fractional edges). `opacity` is a global
    /// multiplier in `[0, 1]`, applied on top of the fill's own alpha.
    pub fn fill_rect(
        &mut self,
        clip: &IRect,
        rect: &Rect,
        fill: &Fill,
        corner_radius: f32,
        opacity: f32,
    ) {
        let opacity = unit_u8(opacity);
        if opacity == 0 || fill.is_transparent() {
            return;
        }
        let clip = self.clip_to_surface(clip);
        if clip.is_empty() {
            return;
        }
        let shape = RRect::new(rect, corner_radius);
        if shape.is_empty() {
            return;
        }
        let (ry0, ry1) = shape.row_range();
        let (rx0, rx1) = shape.col_range();
        let y0 = ry0.max(clip.y);
        let y1 = ry1.min(clip.bottom());
        let cx0 = rx0.max(clip.x);
        let cx1 = rx1.min(clip.right());
        if y0 >= y1 || cx0 >= cx1 {
            return;
        }

        let opaque_fill = fill.is_opaque() && opacity == 255;
        for y in y0..y1 {
            let spans = shape.row_spans(y);
            if spans.is_empty() {
                continue;
            }
            let paint = fill.row_paint(y);
            let lo = spans.first_px().max(cx0);
            let hi = spans.last_px().min(cx1);
            if lo >= hi {
                continue;
            }
            // Interior: every sub-scanline covers these columns completely.
            let (fs, fe) = if spans.is_full_height() {
                (spans.full_start().max(lo), spans.full_end().min(hi))
            } else {
                (hi, hi)
            };
            if fs < fe {
                if lo < fs {
                    let row = self.row(y, lo, fs);
                    paint_cov(row, lo, &paint, opacity, |x| unit_u8(spans.cov(x)));
                }
                let row = self.row(y, fs, fe);
                if opaque_fill {
                    paint_full(row, fs, &paint, 255);
                } else {
                    paint_full(row, fs, &paint, opacity);
                }
                if fe < hi {
                    let row = self.row(y, fe, hi);
                    paint_cov(row, fe, &paint, opacity, |x| unit_u8(spans.cov(x)));
                }
            } else {
                let row = self.row(y, lo, hi);
                paint_cov(row, lo, &paint, opacity, |x| unit_u8(spans.cov(x)));
            }
        }
    }

    /// Stroke a rounded rect with the stroke lying entirely **inside** `rect`.
    ///
    /// The painted region is `rrect(rect, corner_radius)` minus
    /// `rrect(rect.inset(width), corner_radius - width)`; coverage is the
    /// difference of the two analytic coverages, so the ring is anti-aliased
    /// on both sides and never doubles up.
    ///
    /// Away from the corner arcs the ring is two vertical bands whose column
    /// geometry does not depend on `y` at all, and those rows are almost the
    /// whole of a thin border in a large rect. They take a fast path that
    /// resolves the columns and their blend alphas *once* and then does one
    /// blend per column — see [`Canvas::straight_rows`]. Only the `r`-tall
    /// corner rows walk the two coverages.
    pub fn stroke_rect_inside(
        &mut self,
        clip: &IRect,
        rect: &Rect,
        width: f32,
        color: Color,
        corner_radius: f32,
        opacity: f32,
    ) {
        self.stroke_impl(clip, rect, width, color, corner_radius, opacity, true);
    }

    /// [`Canvas::stroke_rect_inside`] with the straight-row fast path forced
    /// off, so the tests can assert the two paths agree byte for byte.
    #[cfg(test)]
    pub(crate) fn stroke_rect_inside_general(
        &mut self,
        clip: &IRect,
        rect: &Rect,
        width: f32,
        color: Color,
        corner_radius: f32,
        opacity: f32,
    ) {
        self.stroke_impl(clip, rect, width, color, corner_radius, opacity, false);
    }

    #[allow(clippy::too_many_arguments)] // the public wrapper is the API; this
    // is it plus one test-only switch
    fn stroke_impl(
        &mut self,
        clip: &IRect,
        rect: &Rect,
        width: f32,
        color: Color,
        corner_radius: f32,
        opacity: f32,
        fast: bool,
    ) {
        let opacity = unit_u8(opacity);
        if opacity == 0 || color.is_transparent() || width <= 0.0 {
            return;
        }
        let clip = self.clip_to_surface(clip);
        if clip.is_empty() {
            return;
        }
        let outer = RRect::new(rect, corner_radius);
        if outer.is_empty() {
            return;
        }
        let inner_rect = rect.inflate(-width);
        let inner = if inner_rect.is_empty() {
            None
        } else {
            Some(RRect::new(&inner_rect, (outer.r - width).max(0.0)))
        };

        let (ry0, ry1) = outer.row_range();
        let (rx0, rx1) = outer.col_range();
        let y0 = ry0.max(clip.y);
        let y1 = ry1.min(clip.bottom());
        let cx0 = rx0.max(clip.x);
        let cx1 = rx1.min(clip.right());
        if y0 >= y1 || cx0 >= cx1 {
            return;
        }

        let paint = RowPaint::Solid(color);
        // The straight (corner-free) rows are almost all of a thin border in a
        // large rect, and their geometry is the same on every one of them:
        // resolve the two bands once and blend them straight in.
        let straight = if fast {
            Self::straight_rows(&outer, inner.as_ref(), cx0, cx1, color, opacity)
        } else {
            None
        };
        if let Some(st) = straight {
            let sy0 = st.y0.max(y0);
            let sy1 = st.y1.min(y1);
            if sy0 < sy1 {
                self.stroke_straight_rows(sy0, sy1, &st.bands);
            }
        }
        // Rows whose whole visible run falls inside the inner shape's
        // full-coverage core paint nothing. A thin border inside a large rect
        // is the common case (window chrome), and this early-out skips the
        // span computation for every row of the hollow middle.
        let core = inner.map(|i| {
            (
                i.x0.ceil() as i32,
                i.x1.floor() as i32,
                (i.y0 + i.r).ceil() as i32,
                (i.y1 - i.r).floor() as i32,
            )
        });
        for y in y0..y1 {
            // Already painted by the fast path above.
            if let Some(st) = straight
                && y >= st.y0
                && y < st.y1
            {
                continue;
            }
            if let Some((kx0, kx1, ky0, ky1)) = core
                && y >= ky0
                && y < ky1
                && cx0 >= kx0
                && cx1 <= kx1
            {
                continue;
            }
            self.stroke_row(y, &outer, inner.as_ref(), cx0, cx1, color, &paint, opacity);
        }
    }

    /// One row of the general (two-coverage) stroke path.
    ///
    /// The coverage is `outer - inner`, evaluated per column, except for the
    /// long constant-coverage run the top and bottom bands have.
    #[allow(clippy::too_many_arguments)] // the row's share of `stroke_impl`'s
    // state; bundling it into a struct would only move the list
    fn stroke_row(
        &mut self,
        y: i32,
        outer: &RRect,
        inner: Option<&RRect>,
        cx0: i32,
        cx1: i32,
        color: Color,
        paint: &RowPaint,
        opacity: u8,
    ) {
        let os = outer.row_spans(y);
        if os.is_empty() {
            return;
        }
        let is = inner.map(|i| i.row_spans(y));
        let lo = os.first_px().max(cx0);
        let hi = os.last_px().min(cx1);
        if lo >= hi {
            return;
        }
        // Columns fully inside the inner shape contribute nothing; split
        // the run so the hollow middle is skipped entirely.
        let hole = is
            .filter(|s| !s.is_empty() && s.is_full_height())
            .map_or((hi, hi), |s| {
                let a = s.full_start().max(lo);
                let b = s.full_end().min(hi);
                if a < b { (a, b) } else { (hi, hi) }
            });
        let cov = |x: i32| {
            let c = os.cov(x) - is.map_or(0.0, |s| s.cov(x));
            unit_u8(c)
        };
        if hole.0 < hole.1 {
            if lo < hole.0 {
                let row = self.row(y, lo, hole.0);
                paint_cov(row, lo, paint, opacity, cov);
            }
            if hole.1 < hi {
                let row = self.row(y, hole.1, hi);
                paint_cov(row, hole.1, paint, opacity, cov);
            }
            return;
        }
        // The inner shape does not reach this row at all, so the row is one
        // run of the *outer* shape: the top and bottom bands of the border.
        // Their middle columns — everything between the two corner arcs —
        // share one coverage, because every sub-scanline covers them
        // completely and the inner subtracts nothing.
        //
        // That run is long (a border is far wider than it is thick, so these
        // rows carry most of the painted pixels: 37 px per row against the
        // vertical bands' 1, on the benchmark's window chrome) and a constant
        // alpha over a contiguous run is exactly what `blend_solid` is for —
        // one hoisted multiply-add per channel in a loop the compiler
        // vectorizes, instead of a coverage evaluation and a `blend_pixel`
        // per column.
        let (cs, ce) = if is.is_none_or(|s| s.is_empty()) {
            (os.full_start().max(lo), os.full_end().min(hi))
        } else {
            (hi, hi)
        };
        if cs >= ce {
            let row = self.row(y, lo, hi);
            paint_cov(row, lo, paint, opacity, cov);
            return;
        }
        if lo < cs {
            let row = self.row(y, lo, cs);
            paint_cov(row, lo, paint, opacity, cov);
        }
        let a = effective_alpha(color.a, cov(cs), opacity);
        if a != 0 {
            blend_solid(self.row(y, cs, ce), color, a);
        }
        if ce < hi {
            let row = self.row(y, ce, hi);
            paint_cov(row, ce, paint, opacity, cov);
        }
    }

    /// The corner-free rows of a stroke, with their two bands pre-resolved.
    ///
    /// A row is "straight" when neither the outer nor the inner shape crosses
    /// a corner arc anywhere in it, so both reduce to a single sub-scanline
    /// spanning the full pixel height. The painted columns are then exactly
    /// `[outer.x0, inner.x0]` and `[inner.x1, outer.x1]`, with the same
    /// coverage on every such row — which is why one table serves them all.
    ///
    /// `None` when there is no worthwhile straight region: no inner shape (a
    /// stroke wider than the rect is a plain fill), fewer than two straight
    /// rows, or a band too wide for the [`BAND_MAX`] table.
    fn straight_rows(
        outer: &RRect,
        inner: Option<&RRect>,
        cx0: i32,
        cx1: i32,
        color: Color,
        opacity: u8,
    ) -> Option<Straight> {
        let inner = inner?;
        // A row is corner-free for a shape when `y >= shape.y0 + r` and
        // `y + 1 <= shape.y1 - r`; it is then also full-height, since the row
        // lies strictly inside `[y0, y1]`. Both shapes must qualify. The
        // inner bound is usually the tighter one, but not always: `RRect::new`
        // clamps the radius to half the shorter side, and the inner shape is
        // the shorter one, so take the max/min rather than assuming.
        let y0 = (outer.y0 + outer.r).max(inner.y0 + inner.r).ceil() as i32;
        let y1 = (outer.y1 - outer.r).min(inner.y1 - inner.r).floor() as i32;
        if y1 - y0 < 2 {
            return None;
        }
        let left = Band::new(outer.x0, inner.x0, cx0, cx1, color, opacity)?;
        let right = Band::new(inner.x1, outer.x1, cx0, cx1, color, opacity)?;
        // Overlapping bands would blend the shared column twice, breaking the
        // "exactly one blend per pixel" contract that makes a translucent
        // border look right. It takes an inner shape narrower than a pixel,
        // which a non-empty inner rect makes unlikely rather than impossible
        // — and the check is one comparison.
        if left.n > 0 && right.n > 0 && left.end() > right.x0 {
            return None;
        }
        Some(Straight {
            y0,
            y1,
            bands: [left, right],
        })
    }

    /// Blend the pre-resolved bands into rows `[y0, y1)`.
    ///
    /// Three multiply-adds per column, straight out of the band's table: no
    /// coverage evaluation, no sub-scanline spans, no premultiply, no per-row
    /// setup beyond slicing the row. Bands are the outer loop so an empty one
    /// is skipped once rather than per row.
    fn stroke_straight_rows(&mut self, y0: i32, y1: i32, bands: &[Band; 2]) {
        let stride = self.stride as usize;
        for band in bands {
            if band.n == 0 {
                continue;
            }
            let premul = &band.premul[..band.n];
            let len = band.n * BYTES_PER_PIXEL;
            let mut start = y0 as usize * stride + band.x0 as usize * BYTES_PER_PIXEL;
            for _ in y0..y1 {
                let row = &mut self.data[start..start + len];
                for (d, p) in row.chunks_exact_mut(4).zip(premul) {
                    let out = [
                        mix(p[0], d[0], p[3]),
                        mix(p[1], d[1], p[3]),
                        mix(p[2], d[2], p[3]),
                        0,
                    ];
                    d.copy_from_slice(&out);
                }
                start += stride;
            }
        }
    }

    /// Draw `src_rect` of `src` into `dst`.
    ///
    /// Nearest-neighbour when the mapping is 1:1 and integer-aligned,
    /// bilinear otherwise. `Argb8888` sources are straight-alpha and are
    /// composited source-over; `Xrgb8888` sources are opaque.
    pub fn blit(
        &mut self,
        clip: &IRect,
        dst: &Rect,
        src: &Image<'_>,
        src_rect: &IRect,
        opacity: f32,
    ) {
        self.blit_impl(clip, dst, src, src_rect, opacity, true);
    }

    /// [`Canvas::blit`] with the scaled blit's run split forced off, so the
    /// tests can assert the split and the general walk agree byte for byte.
    #[cfg(test)]
    pub(crate) fn blit_general(
        &mut self,
        clip: &IRect,
        dst: &Rect,
        src: &Image<'_>,
        src_rect: &IRect,
        opacity: f32,
    ) {
        self.blit_impl(clip, dst, src, src_rect, opacity, false);
    }

    /// [`Canvas::blit`] plus the test-only switch that disables the run split.
    #[allow(clippy::too_many_arguments)] // the public wrapper is the API; this
    // is it plus one test-only switch
    fn blit_impl(
        &mut self,
        clip: &IRect,
        dst: &Rect,
        src: &Image<'_>,
        src_rect: &IRect,
        opacity: f32,
        split: bool,
    ) {
        let opacity = unit_u8(opacity);
        if opacity == 0 || !src.is_valid() {
            return;
        }
        let sr = src_rect.intersect(&src.bounds());
        if sr.is_empty() {
            return;
        }
        let clip = self.clip_to_surface(clip);
        if clip.is_empty() {
            return;
        }
        let shape = RRect::new(dst, 0.0);
        if shape.is_empty() {
            return;
        }
        let (ry0, ry1) = shape.row_range();
        let (rx0, rx1) = shape.col_range();
        let y0 = ry0.max(clip.y);
        let y1 = ry1.min(clip.bottom());
        let cx0 = rx0.max(clip.x);
        let cx1 = rx1.min(clip.right());
        if y0 >= y1 || cx0 >= cx1 {
            return;
        }

        // Device -> source mapping, in source pixel coordinates.
        let scale_x = sr.w as f32 / dst.w;
        let scale_y = sr.h as f32 / dst.h;
        let one_to_one = (scale_x - 1.0).abs() < 1e-4
            && (scale_y - 1.0).abs() < 1e-4
            && (dst.x - dst.x.round()).abs() < 1e-4
            && (dst.y - dst.y.round()).abs() < 1e-4;

        if one_to_one {
            let ox = dst.x.round() as i32;
            let oy = dst.y.round() as i32;
            self.blit_1to1(y0, y1, cx0, cx1, ox, oy, src, &sr, opacity);
            return;
        }

        self.blit_scaled(
            BlitBounds { y0, y1, cx0, cx1 },
            dst,
            src,
            &sr,
            &shape,
            opacity,
            split,
        );
    }

    /// The general (scaled) blit: bilinear, one destination row at a time.
    ///
    /// Each row is split into up to three runs: an interior where the
    /// destination coverage is constant *and* every sampled texel pair lies
    /// inside the source rect, and the leading/trailing columns where one or
    /// both fail. The interior is the overwhelming majority of every blit and
    /// gets a branch-free loop ([`blit_run_inner`]); the edges keep the fully
    /// general one ([`blit_run_edge`]).
    ///
    /// `split == false` (tests only) routes every column through the edge run,
    /// which is the pre-split loop verbatim — that is what
    /// `blit_split_is_byte_identical_to_the_general_walk` compares against.
    #[allow(clippy::too_many_arguments)] // a private path already carrying a
    // bounds struct; the switch is test-only
    fn blit_scaled(
        &mut self,
        b: BlitBounds,
        dst: &Rect,
        src: &Image<'_>,
        sr: &IRect,
        shape: &RRect,
        opacity: u8,
        split: bool,
    ) {
        let BlitBounds { y0, y1, cx0, cx1 } = b;
        let scale_x = sr.w as f32 / dst.w;
        let scale_y = sr.h as f32 / dst.h;
        // Step the source x in 16.16 fixed point instead of recomputing a
        // float and a `floor()` per pixel: two integer adds per pixel.
        let step = (scale_x * 65536.0) as i64;
        let (src_first, src_last) = (sr.x, sr.right() - 1);
        let opaque_src = src.format.is_opaque();
        let src_pitch = src.stride as usize;
        let full_extra = u32::from(effective_alpha(255, 255, opacity));
        let stride = self.stride as usize;
        for y in y0..y1 {
            let spans = shape.row_spans(y);
            if spans.is_empty() {
                continue;
            }
            let lo = spans.first_px().max(cx0);
            let hi = spans.last_px().min(cx1);
            if lo >= hi {
                continue;
            }
            // Source y at this row's pixel centre, in continuous source
            // coordinates (texel centres at +0.5).
            let sy = (y as f32 + 0.5 - dst.y) * scale_y + sr.y as f32 - 0.5;
            let fy = sy.floor();
            let ty = u32::from(unit_u8(sy - fy));
            let iy = fy as i32;
            // Hoist the two source rows the whole destination row samples
            // from: the inner loop then indexes two slices instead of doing
            // four independent clamped address computations per pixel.
            let iy0 = iy.clamp(sr.y, sr.bottom() - 1) as usize;
            let iy1 = (iy + 1).clamp(sr.y, sr.bottom() - 1) as usize;
            let sx0 = (lo as f32 + 0.5 - dst.x) * scale_x + sr.x as f32 - 0.5;
            let base = (sx0 * 65536.0).floor() as i64;
            let sampler = RowSampler {
                srow0: &src.data[iy0 * src_pitch..],
                srow1: &src.data[iy1 * src_pitch..],
                ty,
                base,
                step,
                opaque: opaque_src,
            };
            // The interior run: constant coverage (only the first and last
            // column of the destination can be partially covered) intersected
            // with the columns whose texel pair needs no clamp.
            let (in_lo, in_hi) = texels_in_range(base, step, lo, hi, src_first, src_last);
            let (fast_lo, fast_hi) = if split && spans.is_full_height() {
                (
                    spans.full_start().max(lo).max(in_lo),
                    spans.full_end().min(hi).min(in_hi),
                )
            } else {
                (lo, lo)
            };
            let (fast_lo, fast_hi) = if fast_lo < fast_hi {
                (fast_lo, fast_hi)
            } else {
                (lo, lo)
            };
            let start = y as usize * stride + lo as usize * BYTES_PER_PIXEL;
            let len = (hi - lo) as usize * BYTES_PER_PIXEL;
            let row = &mut self.data[start..start + len];
            let (head, rest) = row.split_at_mut((fast_lo - lo) as usize * BYTES_PER_PIXEL);
            let (mid, tail) = rest.split_at_mut((fast_hi - fast_lo) as usize * BYTES_PER_PIXEL);
            let src_range = (src_first, src_last);
            if !head.is_empty() {
                blit_run_edge(head, &sampler, lo, &spans, opacity, src_range);
            }
            if !mid.is_empty() {
                blit_run_inner(mid, &sampler.at(lo, fast_lo), full_extra);
            }
            if !tail.is_empty() {
                blit_run_edge(
                    tail,
                    &sampler.at(lo, fast_hi),
                    fast_hi,
                    &spans,
                    opacity,
                    src_range,
                );
            }
        }
    }

    /// Unscaled, integer-aligned blit: nearest (== exact) sampling.
    #[allow(clippy::too_many_arguments)] // a private fast path; grouping the
    // already-clipped bounds into a struct would only move the noise
    fn blit_1to1(
        &mut self,
        y0: i32,
        y1: i32,
        cx0: i32,
        cx1: i32,
        ox: i32,
        oy: i32,
        src: &Image<'_>,
        sr: &IRect,
        opacity: u8,
    ) {
        let opaque = src.format.is_opaque() && opacity == 255;
        for y in y0..y1 {
            let sy = sr.y + (y - oy);
            if sy < sr.y || sy >= sr.bottom() {
                continue;
            }
            let lo = cx0.max(ox);
            let hi = cx1.min(ox + sr.w);
            if lo >= hi {
                continue;
            }
            let so =
                sy as usize * src.stride as usize + (sr.x + (lo - ox)) as usize * BYTES_PER_PIXEL;
            let slen = (hi - lo) as usize * BYTES_PER_PIXEL;
            let srow = &src.data[so..so + slen];
            let dstart = y as usize * self.stride as usize + lo as usize * BYTES_PER_PIXEL;
            let drow = &mut self.data[dstart..dstart + slen];
            if opaque {
                // Two pixels per store. The obvious per-pixel form —
                // `d.copy_from_slice(&[s[0], s[1], s[2], 0])` — does not
                // vectorize on a baseline `x86-64` target (LLVM emits a byte
                // shuffle), and measured 3.6x slower than this at 1920x1080.
                // Widening to a `u64` reaches memcpy speed with no SIMD, no
                // intrinsics and no `unsafe`.
                //
                // The mask is not optional: it zeroes both X bytes, which is
                // the crate-wide contract that byte 3 of a stored pixel is 0.
                // A plain row `copy_from_slice` would propagate the client's
                // byte 3 and is a behaviour change, not an optimisation.
                const KEEP: u64 = 0x00FF_FFFF_00FF_FFFF;
                let pairs = drow.len() & !7;
                let (dhead, dtail) = drow.split_at_mut(pairs);
                let (shead, stail) = srow.split_at(pairs);
                for (d, s) in dhead.chunks_exact_mut(8).zip(shead.chunks_exact(8)) {
                    let v = u64::from_le_bytes(s.try_into().unwrap_or([0; 8])) & KEEP;
                    d.copy_from_slice(&v.to_le_bytes());
                }
                // `drow.len()` is always a multiple of 4, so the tail is one
                // pixel at most.
                for (d, s) in dtail.chunks_exact_mut(4).zip(stail.chunks_exact(4)) {
                    d.copy_from_slice(&[s[0], s[1], s[2], 0]);
                }
            } else {
                for (d, s) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
                    let sa = if src.format.is_opaque() {
                        255
                    } else {
                        u32::from(s[3])
                    };
                    let a = effective_alpha(sa as u8, 255, opacity);
                    if a == 0 {
                        continue;
                    }
                    let au = u32::from(a);
                    let out = [
                        over_straight(u32::from(s[0]), u32::from(d[0]), au),
                        over_straight(u32::from(s[1]), u32::from(d[1]), au),
                        over_straight(u32::from(s[2]), u32::from(d[2]), au),
                        0,
                    ];
                    d.copy_from_slice(&out);
                }
            }
        }
    }

    /// Blit an A8 coverage mask at device pixel `(x, y)`, tinted `color`,
    /// source-over, never writing outside `clip`.
    ///
    /// `(x, y)` is the device pixel the mask's top-left corner lands on —
    /// the caller (the glyph painter) has already applied the glyph's
    /// placement offsets. No scaling and no filtering: a glyph mask is
    /// rendered at its final size by the atlas.
    ///
    /// An empty or invalid mask, a transparent colour, `opacity <= 0` and an
    /// empty clip are all no-ops.
    pub fn blit_mask(
        &mut self,
        clip: &IRect,
        x: i32,
        y: i32,
        mask: &Mask<'_>,
        color: Color,
        opacity: f32,
    ) {
        let Some(setup) = MaskSetup::new(color, opacity) else {
            return;
        };
        let clip = self.clip_to_surface(clip);
        if clip.is_empty() {
            return;
        }
        self.blit_mask_clipped(&clip, x, y, mask, &setup);
    }

    /// Blit several masks that share a colour and an opacity.
    ///
    /// Same result as calling [`Canvas::blit_mask`] once per entry; the
    /// batch hoists the per-call setup (clip intersection with the surface,
    /// the effective source alpha, the premultiplied colour) out of the
    /// loop, which is what a run of glyphs actually wants.
    pub fn blit_masks(
        &mut self,
        clip: &IRect,
        color: Color,
        opacity: f32,
        masks: &[(i32, i32, Mask<'_>)],
    ) {
        let Some(setup) = MaskSetup::new(color, opacity) else {
            return;
        };
        let clip = self.clip_to_surface(clip);
        if clip.is_empty() {
            return;
        }
        for (x, y, mask) in masks {
            self.blit_mask_clipped(&clip, *x, *y, mask, &setup);
        }
    }

    /// One mask blit with `clip` already intersected with the surface and the
    /// colour setup already computed.
    fn blit_mask_clipped(
        &mut self,
        clip: &IRect,
        x: i32,
        y: i32,
        mask: &Mask<'_>,
        setup: &MaskSetup,
    ) {
        if !mask.is_valid() {
            return;
        }
        // i64 throughout: the placement of a glyph inside a scrolled document
        // can be far outside the surface, and `x + mask.w` must not wrap.
        let (mx, my) = (i64::from(x), i64::from(y));
        let x0 = mx.max(i64::from(clip.x));
        let x1 = (mx + i64::from(mask.w)).min(i64::from(clip.right()));
        let y0 = my.max(i64::from(clip.y));
        let y1 = (my + i64::from(mask.h)).min(i64::from(clip.bottom()));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        // Every value below is inside the clip, which is inside the surface.
        let run = (x1 - x0) as usize;
        let mask_x = (x0 - mx) as usize;
        let mask_stride = mask.stride as usize;
        let (dx0, dx1) = (x0 as i32, x1 as i32);
        for dy in y0..y1 {
            let mo = (dy - my) as usize * mask_stride + mask_x;
            let cov = &mask.data[mo..mo + run];
            let row = self.row(dy as i32, dx0, dx1);
            if setup.opaque {
                blend_mask_row_opaque(row, cov, setup.color);
            } else {
                blend_mask_row(row, cov, setup.color, setup.ca);
            }
        }
    }

    /// Source-over one straight-alpha colour into a single pixel, if it is
    /// inside `clip` and the surface. Mostly useful for tests and debug
    /// markers.
    pub fn blend_pixel_at(&mut self, clip: &IRect, x: i32, y: i32, color: Color, opacity: f32) {
        let clip = self.clip_to_surface(clip);
        if !clip.contains(x, y) {
            return;
        }
        let a = effective_alpha(color.a, 255, unit_u8(opacity));
        let o = y as usize * self.stride as usize + x as usize * BYTES_PER_PIXEL;
        blend_pixel(&mut self.data[o..o + 4], color, a);
    }
}

/// The source-sampling state one destination row of a scaled blit needs.
///
/// The two source rows the row interpolates between, the vertical weight, and
/// the horizontal 16.16 stepping. Built once per destination row and shared
/// by the interior and edge runs.
#[derive(Debug, Clone, Copy)]
struct RowSampler<'a> {
    /// The upper source row, sliced to start at its first byte.
    srow0: &'a [u8],
    /// The lower source row; equal to `srow0` when the sample lands on an
    /// edge row.
    srow1: &'a [u8],
    /// Vertical interpolation weight in `0..=255`.
    ty: u32,
    /// Source x in 16.16 fixed point at the run's first column.
    base: i64,
    /// Source x increment per destination column, 16.16.
    step: i64,
    /// Whether the source format has no alpha channel to filter.
    opaque: bool,
}

impl RowSampler<'_> {
    /// The same sampler with its origin moved to destination column `x`.
    ///
    /// `x0` is the column `base` currently refers to.
    fn at(&self, x0: i32, x: i32) -> Self {
        Self {
            base: self.base + i64::from(x - x0) * self.step,
            ..*self
        }
    }
}

/// The destination columns of `[lo, hi)` whose bilinear texel pair
/// `[ix, ix+1]` lies wholly inside `[src_first, src_last]`.
///
/// The mapping is monotonic (`step >= 0` for any real scale), so the in-range
/// columns are one contiguous run and finding its ends is two divisions
/// rather than a per-pixel comparison. Outside it the sample edge-extends and
/// the pair has to be assembled by hand; inside, the row is two 8-byte slices.
///
/// The two subtractions from `base` saturate: a pathological `dst` (sub-pixel
/// width straddling a column boundary) can drive `base` to `i64::MIN`, and
/// `first - base` would then overflow. Unreachable from the scene, which
/// clamps rects long before this, and not a regression — the pre-split
/// `fixed += step` overflowed on the same input — but free to close here
/// (issue #553).
fn texels_in_range(
    base: i64,
    step: i64,
    lo: i32,
    hi: i32,
    src_first: i32,
    src_last: i32,
) -> (i32, i32) {
    if step <= 0 {
        // A degenerate or reversed mapping: no interior worth splitting out.
        return (lo, lo);
    }
    // `ix(x) = (base + (x - lo) * step) >> 16`, wanted in `[src_first, src_last)`.
    let first = i64::from(src_first) << 16;
    let last = i64::from(src_last) << 16;
    // Smallest `k >= 0` with `base + k * step >= first`, i.e. the ceiling
    // division `(first - base) / step` (`i64::div_ceil` is still unstable).
    let k0 = if first > base {
        let d = first.saturating_sub(base);
        d / step + i64::from(d % step != 0)
    } else {
        0
    };
    // Largest `k` with `base + k * step < last`, plus one.
    let k1 = if last > base {
        (last.saturating_sub(base) - 1).div_euclid(step) + 1
    } else {
        0
    };
    let in_lo = (i64::from(lo) + k0).clamp(i64::from(lo), i64::from(hi)) as i32;
    let in_hi = (i64::from(lo) + k1).clamp(i64::from(lo), i64::from(hi)) as i32;
    if in_lo < in_hi {
        (in_lo, in_hi)
    } else {
        (lo, lo)
    }
}

/// The interior run of a scaled blit row: constant coverage, every texel pair
/// in range.
///
/// Branch-free by construction — the caller has already established that the
/// coverage is the same for every column and that no sample needs clamping —
/// so the loop body is a straight sequence of loads, multiplies and one
/// 4-byte store, which is the shape the autovectorizer wants. The
/// early-`continue`s of the general path are deliberately *not* here: a fully
/// transparent texel composites to the destination unchanged, so skipping it
/// and blending it write the same bytes, and on a source that is mostly
/// non-transparent the branch costs more than the blend.
#[inline]
fn blit_run_inner(row: &mut [u8], s: &RowSampler<'_>, extra: u32) {
    // `extra` is constant across the whole run, so the `alpha == 0` guard is
    // resolved once here rather than per pixel.
    //
    // For `extra >= 128` the guard is provably dead: `div255(t.a * extra)` is
    // zero only when `t.a` is zero (128 is the exact threshold -- at 127,
    // `t.a = 1` still rounds to 0), and `bilinear` returns an all-zero texel
    // when its alpha is zero, so the blend is the identity anyway. That is the
    // case a UI frame is made of: full coverage at full opacity is
    // `extra = 255`.
    //
    // Worth the duplicated loop: a per-pixel select sits in the dependency
    // chain of every channel and costs 2.1 ms of the 3.1 ms this split saves
    // on scene (d).
    if extra < 128 {
        blit_run_inner_guarded(row, s, extra);
        return;
    }
    let mut fixed = s.base;
    for d in row.chunks_exact_mut(4) {
        let o = (fixed >> 16) as usize * BYTES_PER_PIXEL;
        let tx = ((fixed >> 8) & 0xFF) as u32;
        fixed += s.step;
        let t = bilinear(&s.srow0[o..o + 8], &s.srow1[o..o + 8], tx, s.ty, s.opaque);
        blend_texel_unguarded(d, &t, extra);
    }
}

/// The interior run for a faint `extra`, where the `alpha == 0` guard is load
/// bearing (see [`blend_texel`]).
///
/// Deliberately **not** inlined into [`blit_run_inner`]. It is the rare case --
/// `extra < 128` means coverage times opacity below ~50 %, which a UI frame
/// mostly does not do -- and letting it share a function with the hot loop
/// costs 1.6 ms on scene (d) even when this code never runs, purely through
/// the pressure two copies of the loop put on inlining and layout. Splitting
/// it out puts the hot path back at full speed.
#[cold]
#[inline(never)]
fn blit_run_inner_guarded(row: &mut [u8], s: &RowSampler<'_>, extra: u32) {
    let mut fixed = s.base;
    for d in row.chunks_exact_mut(4) {
        let o = (fixed >> 16) as usize * BYTES_PER_PIXEL;
        let tx = ((fixed >> 8) & 0xFF) as u32;
        fixed += s.step;
        let t = bilinear(&s.srow0[o..o + 8], &s.srow1[o..o + 8], tx, s.ty, s.opaque);
        blend_texel(d, &t, extra);
    }
}

/// The leading/trailing run of a scaled blit row: per-column coverage and
/// edge-extended sampling.
fn blit_run_edge(
    row: &mut [u8],
    s: &RowSampler<'_>,
    x0: i32,
    spans: &RowSpans,
    opacity: u8,
    src: (i32, i32),
) {
    let (src_first, src_last) = src;
    let mut fixed = s.base;
    for (x, d) in (x0..).zip(row.chunks_exact_mut(4)) {
        let ix = (fixed >> 16) as i32;
        let tx = ((fixed >> 8) & 0xFF) as u32;
        fixed += s.step;
        // Fetch each row's texel *pair* as one 8-byte slice when the pair is
        // in range: two bounds checks instead of four.
        let t = if ix >= src_first && ix < src_last {
            let o = ix as usize * BYTES_PER_PIXEL;
            bilinear(&s.srow0[o..o + 8], &s.srow1[o..o + 8], tx, s.ty, s.opaque)
        } else {
            // Edge-extend: build the pair by hand.
            let a = ix.clamp(src_first, src_last) as usize * BYTES_PER_PIXEL;
            let b = (ix + 1).clamp(src_first, src_last) as usize * BYTES_PER_PIXEL;
            let mut p0 = [0u8; 8];
            let mut p1 = [0u8; 8];
            p0[..4].copy_from_slice(&s.srow0[a..a + 4]);
            p0[4..].copy_from_slice(&s.srow0[b..b + 4]);
            p1[..4].copy_from_slice(&s.srow1[a..a + 4]);
            p1[4..].copy_from_slice(&s.srow1[b..b + 4]);
            bilinear(&p0, &p1, tx, s.ty, s.opaque)
        };
        if t.a == 0 {
            continue;
        }
        let extra = u32::from(effective_alpha(255, unit_u8(spans.cov(x)), opacity));
        if extra == 0 {
            continue;
        }
        blend_texel(d, &t, extra);
    }
}

/// Source-over one already-premultiplied [`Texel`], scaled by `extra`.
///
/// `t` is premultiplied by `t.a`; scaling every channel *and* the alpha by
/// the same `extra / 255` (the coverage-times-opacity factor) keeps it
/// premultiplied by the effective alpha, so there is no per-pixel division by
/// a varying quantity.
///
/// The `alpha == 0` case is a **correctness** requirement, not an
/// optimization. The general loop this was split out of skipped such a pixel
/// with a `continue`. That looks redundant — zero alpha, nothing to blend —
/// but it is not: `over_premul(c, dst, 0)` is `c + dst`, so a non-zero
/// premultiplied channel is *added* to the destination. And `c` can be
/// non-zero while `alpha` is zero, because the two are rounded separately:
/// [`bilinear`] can return `t.b > t.a` by one step (e.g. `t.a = 1, t.b = 128,
/// extra = 1` — 637 such triples exist), and then `div255(t.a * extra)` is 0
/// while `div255(t.b * extra)` is 1. Dropping the check brightened exactly one
/// pixel in a 3600-case sweep against the pre-split code, which is precisely
/// the kind of once-in-a-frame artefact that never gets diagnosed.
///
/// The guard zeroes the *scale* rather than each scaled channel, so it is one
/// select outside the three channel expressions rather than three inside
/// them. `blit_run_inner` avoids paying even that on the hot path by proving
/// the guard dead for `extra >= 128`.
#[inline]
fn blend_texel(d: &mut [u8], t: &Texel, extra: u32) {
    let alpha = div255(t.a * extra);
    let extra = extra * u32::from(alpha != 0);
    blend_texel_unguarded(d, t, extra);
}

/// [`blend_texel`] without the `alpha == 0` guard.
///
/// Only correct where the caller has established that `alpha == 0` implies an
/// all-zero `t` -- either because `t` came straight from [`bilinear`] and
/// `extra >= 128` (see [`blit_run_inner`]), or because `extra` has already
/// been zeroed. Everywhere else, use [`blend_texel`].
///
/// # The X byte (issue #553)
///
/// This run stores `0` into byte 3 for **every** destination pixel it touches,
/// including fully transparent texels. The pre-split general loop `continue`d
/// on `t.a == 0` and so left the pixel — X byte included — entirely alone.
/// The store is the deliberate behaviour and the `continue` was the anomaly:
/// the crate's contract is that every write path stores 0 in the X byte of
/// XRGB8888 (`fill_irect`, `blit_1to1`, the stroke band loop and the mask
/// paths all do), the byte is never read by anything in the tree or by the
/// scanout hardware, and a destination painted by this crate therefore already
/// holds 0 there before a blit runs. Output is identical for every in-tree
/// caller. Preserving byte 3 instead would be a crate-wide decision about the
/// pixel-format contract, not a blit detail — do not "fix" the two runs into
/// agreement in that direction.
#[inline]
fn blend_texel_unguarded(d: &mut [u8], t: &Texel, extra: u32) {
    let alpha = div255(t.a * extra);
    let out = [
        over_premul(div255(t.b * extra), u32::from(d[0]), alpha),
        over_premul(div255(t.g * extra), u32::from(d[1]), alpha),
        over_premul(div255(t.r * extra), u32::from(d[2]), alpha),
        0,
    ];
    d.copy_from_slice(&out);
}

/// One channel of a [`bilinear`] blend.
///
/// `row0`/`row1` each hold the two horizontally adjacent texels of one source
/// row, as `[B,G,R,A, B,G,R,A]`; `w` are the four texel weights, summing to
/// 65536 so the normalisation is a shift.
#[inline]
fn ch(row0: &[u8], row1: &[u8], i: usize, w: [u32; 4]) -> u32 {
    (u32::from(row0[i]) * w[0]
        + u32::from(row0[i + 4]) * w[1]
        + u32::from(row1[i]) * w[2]
        + u32::from(row1[i + 4]) * w[3])
        >> 16
}

/// Bilinear blend of the four neighbouring texels of one destination pixel.
///
/// Each argument is a 4-byte `[B, G, R, A]` texel; `tx`/`ty` are the
/// fractional weights in `0..=255`. The returned colour channels are
/// **premultiplied** by the returned alpha — filtering straight alpha
/// directly produces halos around transparent texels.
///
/// The four weights are built to sum to `65536`, so the final normalisation
/// is a `>> 16` rather than a division; an opaque source needs no division
/// at all.
#[inline]
fn bilinear(row0: &[u8], row1: &[u8], tx: u32, ty: u32, opaque: bool) -> Texel {
    let (wx1, wx0) = (tx, 256 - tx);
    let (wy1, wy0) = (ty, 256 - ty);
    let w = [wx0 * wy0, wx1 * wy0, wx0 * wy1, wx1 * wy1];

    if opaque {
        return Texel {
            b: ch(row0, row1, 0, w),
            g: ch(row0, row1, 1, w),
            r: ch(row0, row1, 2, w),
            a: 255,
        };
    }

    let a = [
        u32::from(row0[3]),
        u32::from(row0[7]),
        u32::from(row1[3]),
        u32::from(row1[7]),
    ];
    let alpha = (a[0] * w[0] + a[1] * w[1] + a[2] * w[2] + a[3] * w[3]) >> 16;
    if alpha == 0 {
        return Texel {
            b: 0,
            g: 0,
            r: 0,
            a: 0,
        };
    }
    // Fold each texel's alpha into its weight so the weighted sum comes out
    // already premultiplied. The accumulator is `w * a * c`, at most
    // `65536 * 255 * 255 = 4_261_478_400`, which still fits a `u32` because
    // the weights sum to exactly 65536. `>> 16` leaves a 255-scaled value
    // that `div255` rounds exactly, so a flat opaque image filters to itself
    // bit for bit.
    let wa = [w[0] * a[0], w[1] * a[1], w[2] * a[2], w[3] * a[3]];
    Texel {
        b: div255(ch(row0, row1, 0, wa)),
        g: div255(ch(row0, row1, 1, wa)),
        r: div255(ch(row0, row1, 2, wa)),
        a: alpha,
    }
}

#[cfg(test)]
mod blend_texel_tests {
    use super::{Texel, blend_texel};
    use crate::blend::div255;

    #[test]
    fn the_unguarded_fast_path_threshold_is_exactly_128() {
        // `blit_run_inner` skips the `alpha == 0` guard when `extra >= 128`,
        // on the claim that above that threshold `alpha == 0` implies
        // `t.a == 0`. Both halves are checked here, because the whole
        // correctness of the hot loop rests on them.
        //
        // 1. At and above 128 there is no `t.a > 0` that rounds to zero.
        for extra in 128..=255u32 {
            for ta in 1..=255u32 {
                assert_ne!(
                    div255(ta * extra),
                    0,
                    "extra={extra} t.a={ta} would need the guard"
                );
            }
        }
        // 2. 127 is not good enough -- the threshold is tight, not arbitrary.
        assert_eq!(div255(127), 0, "127 must still need the guard");
    }

    #[test]
    fn bilinear_returns_an_all_zero_texel_when_its_alpha_is_zero() {
        // The other half of the unguarded path's premise: with `t.a == 0` the
        // channels must be zero too, or the blend would add them to the
        // destination. `bilinear` early-returns a zeroed texel in that case;
        // this pins it, since a future change to that early-out would silently
        // brighten pixels.
        let mut rows = [[0u8; 8]; 2];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % 256) as u8
        };
        for _ in 0..2000 {
            for r in &mut rows {
                for (i, b) in r.iter_mut().enumerate() {
                    // Colour channels random, alpha bytes (3 and 7) zero.
                    *b = if i % 4 == 3 { 0 } else { next() };
                }
            }
            let t = super::bilinear(
                &rows[0],
                &rows[1],
                u32::from(next()),
                u32::from(next()),
                false,
            );
            assert_eq!((t.a, t.b, t.g, t.r), (0, 0, 0, 0));
        }
    }

    #[test]
    fn a_zero_effective_alpha_leaves_the_destination_alone() {
        // The `alpha == 0` case of `blend_texel` is a correctness requirement,
        // not an optimization: `over_premul(c, dst, 0) == c + dst`, so a
        // non-zero premultiplied channel would be *added* to the destination.
        //
        // The trap is that `c` can be non-zero while `alpha` is zero. The
        // channel and the alpha are rounded separately, so `bilinear` can
        // return `t.b > t.a`, and then `div255(t.a * extra) == 0` while
        // `div255(t.b * extra) == 1`. Sweeping every such combination is
        // cheap, so sweep it rather than trusting the argument.
        let dst = [0x40u8, 0x80, 0xC0, 0];
        let mut found = 0;
        for ta in 0..=255u32 {
            for extra in 0..=255u32 {
                if div255(ta * extra) != 0 {
                    continue;
                }
                for tb in 0..=255u32 {
                    let t = Texel {
                        b: tb,
                        g: tb,
                        r: tb,
                        a: ta,
                    };
                    let mut d = dst;
                    blend_texel(&mut d, &t, extra);
                    assert_eq!(
                        [d[0], d[1], d[2]],
                        [dst[0], dst[1], dst[2]],
                        "alpha==0 must not write: t.a={ta} t.b={tb} extra={extra}"
                    );
                    if div255(tb * extra) != 0 {
                        found += 1;
                    }
                }
            }
        }
        // If this ever reaches zero the test has stopped covering the case it
        // exists for -- the bug it pins is only reachable through these.
        assert!(
            found > 0,
            "no (t.a, t.b, extra) with alpha==0 but a non-zero channel"
        );
    }
}
