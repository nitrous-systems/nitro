//! The [`Canvas`] surface and its drawing operations.

use nitro_core::{Color, IRect, Point, Rect};

use crate::blend::{div255, effective_alpha, over_premul, over_straight, unit_u8};
use crate::paint::{RowPaint, blend_pixel, paint_cov, paint_full, store_solid};
use crate::shape::RRect;

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
                    RowPaint::Solid(lerp_row_color(c0, c1, t.clamp(0.0, 1.0)))
                }
            }
        }
    }
}

#[inline]
fn lerp_row_color(a: Color, b: Color, t: f32) -> Color {
    #[inline]
    fn ch(a: u8, b: u8, t: f32) -> u8 {
        let a = f32::from(a);
        (a + (f32::from(b) - a) * t + 0.5) as u8
    }
    Color::rgba(
        ch(a.r, b.r, t),
        ch(a.g, b.g, t),
        ch(a.b, b.b, t),
        ch(a.a, b.a, t),
    )
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
                crate::paint::blend_solid(self.row(y, x0, x1), color, color.a);
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
    pub fn stroke_rect_inside(
        &mut self,
        clip: &IRect,
        rect: &Rect,
        width: f32,
        color: Color,
        corner_radius: f32,
        opacity: f32,
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
            if let Some((kx0, kx1, ky0, ky1)) = core
                && y >= ky0
                && y < ky1
                && cx0 >= kx0
                && cx1 <= kx1
            {
                continue;
            }
            let os = outer.row_spans(y);
            if os.is_empty() {
                continue;
            }
            let is = inner.map(|i| i.row_spans(y));
            let lo = os.first_px().max(cx0);
            let hi = os.last_px().min(cx1);
            if lo >= hi {
                continue;
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
                    paint_cov(row, lo, &paint, opacity, cov);
                }
                if hole.1 < hi {
                    let row = self.row(y, hole.1, hi);
                    paint_cov(row, hole.1, &paint, opacity, cov);
                }
            } else {
                let row = self.row(y, lo, hi);
                paint_cov(row, lo, &paint, opacity, cov);
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
        );
    }

    /// The general (scaled) blit: bilinear, one destination row at a time.
    fn blit_scaled(
        &mut self,
        b: BlitBounds,
        dst: &Rect,
        src: &Image<'_>,
        sr: &IRect,
        shape: &RRect,
        opacity: u8,
    ) {
        let BlitBounds { y0, y1, cx0, cx1 } = b;
        let scale_x = sr.w as f32 / dst.w;
        let scale_y = sr.h as f32 / dst.h;
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
            let src_pitch = src.stride as usize;
            let iy0 = iy.clamp(sr.y, sr.bottom() - 1) as usize;
            let iy1 = (iy + 1).clamp(sr.y, sr.bottom() - 1) as usize;
            let srow0 = &src.data[iy0 * src_pitch..];
            let srow1 = &src.data[iy1 * src_pitch..];
            // Step the source x in 16.16 fixed point instead of recomputing a
            // float and a `floor()` per pixel: two integer adds per pixel.
            let sx0 = (lo as f32 + 0.5 - dst.x) * scale_x + sr.x as f32 - 0.5;
            let step = (scale_x * 65536.0) as i64;
            let base = (sx0 * 65536.0).floor() as i64;
            // Clamp the horizontal source range so the interior of the row
            // needs no per-pixel clamp at all: for `x` in `[in_lo, in_hi)` the
            // texel pair `[ix, ix+1]` is guaranteed inside `sr`.
            let (src_first, src_last) = (sr.x, sr.right() - 1);
            let opaque_src = src.format.is_opaque();
            let stride = self.stride as usize;
            let start = y as usize * stride + lo as usize * BYTES_PER_PIXEL;
            let len = (hi - lo) as usize * BYTES_PER_PIXEL;
            let row = &mut self.data[start..start + len];
            // Only the first and last column of the row can have fractional
            // destination-edge coverage; the interior skips the coverage call.
            let full_lo = spans.full_start().max(lo);
            let full_hi = spans.full_end().min(hi);
            let row_full = spans.is_full_height();
            let full_extra = u32::from(effective_alpha(255, 255, opacity));
            let mut fixed = base;
            for (x, d) in (lo..).zip(row.chunks_exact_mut(4)) {
                let ix = (fixed >> 16) as i32;
                let tx = ((fixed >> 8) & 0xFF) as u32;
                fixed += step;
                // Fetch each row's texel *pair* as one 8-byte slice when the
                // pair is in range (the overwhelmingly common case): two
                // bounds checks per pixel instead of four.
                let t = if ix >= src_first && ix < src_last {
                    let o = ix as usize * BYTES_PER_PIXEL;
                    bilinear(&srow0[o..o + 8], &srow1[o..o + 8], tx, ty, opaque_src)
                } else {
                    // Edge-extend: build the pair by hand.
                    let a = ix.clamp(src_first, src_last) as usize * BYTES_PER_PIXEL;
                    let b = (ix + 1).clamp(src_first, src_last) as usize * BYTES_PER_PIXEL;
                    let mut p0 = [0u8; 8];
                    let mut p1 = [0u8; 8];
                    p0[..4].copy_from_slice(&srow0[a..a + 4]);
                    p0[4..].copy_from_slice(&srow0[b..b + 4]);
                    p1[..4].copy_from_slice(&srow1[a..a + 4]);
                    p1[4..].copy_from_slice(&srow1[b..b + 4]);
                    bilinear(&p0, &p1, tx, ty, opaque_src)
                };
                if t.a == 0 {
                    continue;
                }
                // `t` is premultiplied by `t.a`; scaling it by the extra
                // `coverage * opacity` factor keeps it premultiplied by the
                // effective alpha, with no per-pixel division.
                let extra = if row_full && x >= full_lo && x < full_hi {
                    full_extra
                } else {
                    u32::from(effective_alpha(255, unit_u8(spans.cov(x)), opacity))
                };
                if extra == 0 {
                    continue;
                }
                let alpha = div255(t.a * extra);
                if alpha == 0 {
                    continue;
                }
                let out = [
                    over_premul(div255(t.b * extra), u32::from(d[0]), alpha),
                    over_premul(div255(t.g * extra), u32::from(d[1]), alpha),
                    over_premul(div255(t.r * extra), u32::from(d[2]), alpha),
                    0,
                ];
                d.copy_from_slice(&out);
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
                for (d, s) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
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
