//! Video: an NV12 or packed 4:2:2 (YUYV / UYVY) source blitted into the
//! XRGB canvas, with fused YUV → RGB conversion and scaling.
//!
//! This is the CPU path for video surfaces, the one that has to work
//! everywhere (simpledrm, VMs, no Mesa). It is a *store*: a video surface is
//! opaque, so there is no blend and no read of the destination.
//!
//! # Geometry
//!
//! The destination is an [`IRect`], not a [`Rect`](nitro_core::Rect): a video
//! surface is placed on whole pixels, so there are no anti-aliased edges and
//! no coverage maths. `src_rect` crops the source (it is intersected with the
//! source bounds first).
//!
//! - **1:1** (destination size == crop size): nearest sampling. Luma is
//!   exact; chroma is the sample covering the pixel, `(lx >> 1, ly >> 1)`.
//! - **Scaled**: bilinear on luma and bilinear on chroma at half resolution,
//!   16.16 fixed-point positions and 8-bit weights, both edge-clamped to the
//!   crop.
//!
//! Source positions are an affine function of the *absolute* destination
//! column and row (`base + (x - dst.x) * step`), so a clipped blit is
//! byte-identical to the same region of an unclipped one — painting a video
//! frame in damage-rect pieces leaves no seams.
//!
//! # Chroma siting
//!
//! The H.264/HEVC default (`chroma_sample_loc_type` 0, "left"): chroma sample
//! `(j, k)` is **co-sited with luma column `2j`** horizontally and **centred
//! between luma rows `2k` and `2k + 1`** vertically. In continuous luma
//! coordinates (texel centres on integers) that is chroma
//! `x = lx / 2`, `y = (ly − 0.5) / 2`. The scaled path samples exactly
//! there; the 1:1 path takes the nearest sample, which for the vertical axis
//! is the chroma row both luma rows share.
//!
//! Chroma columns are clamped to the samples that cover the crop,
//! `sr.x >> 1 ..= (sr.right() − 1) >> 1`, so an odd crop offset resolves to
//! the right chroma column.
//!
//! ## Packed 4:2:2 ([`Packed422`])
//!
//! One plane; each 4-byte group carries two pixels, `Y0 U Y1 V`
//! ([`Packed422Order::Yuyv`]) or `U Y0 V Y1` ([`Packed422Order::Uyvy`]).
//! Chroma is subsampled horizontally only: group `j` of row `ly` holds
//! chroma sample `(j, ly)`, **co-sited with luma column `2j`** (the same
//! horizontal siting as NV12) and on the luma row itself vertically. In luma
//! coordinates chroma `x = lx / 2`, `y = ly`. The 1:1 path takes the group
//! covering the pixel, `lx >> 1`; the scaled path samples bilinearly at
//! exactly that position, with chroma sharing the luma rows and vertical
//! weight. Chroma columns clamp to `sr.x >> 1 ..= (sr.right() − 1) >> 1`
//! as for NV12.
//!
//! # Conversion
//!
//! Integer fixed point with 12 fraction bits; see [`Coeffs`]. The 1:1 path is
//! within ±1 of a float reference for every input (in practice it rounds
//! identically almost everywhere).

// Y/U/V/R/G/B and lx/cx, ly/cy are the domain's names; spelling them out
// would make the colour maths harder to check against the standards.
#![allow(clippy::many_single_char_names, clippy::similar_names)]

use nitro_core::IRect;

use crate::canvas::{BYTES_PER_PIXEL, Canvas, Image, PixelFormat, texels_in_range};

/// The YUV → RGB matrix of a video source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum YuvMatrix {
    /// ITU-R BT.601 (SD video, JPEG): `Kr = 0.299`, `Kb = 0.114`.
    Bt601,
    /// ITU-R BT.709 (HD video): `Kr = 0.2126`, `Kb = 0.0722`. The default.
    #[default]
    Bt709,
    /// ITU-R BT.2020 (UHD), non-constant luminance: `Kr = 0.2627`,
    /// `Kb = 0.0593`.
    Bt2020,
}

impl YuvMatrix {
    /// `(Kr, Kb)`.
    pub(crate) fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt601 => (0.299, 0.114),
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt2020 => (0.2627, 0.0593),
        }
    }
}

/// The quantization range of a video source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum YuvRange {
    /// "TV" / "studio" range: Y in `16..=235`, chroma in `16..=240`. The
    /// default — it is what almost every video stream is. Values outside the
    /// range (super-white, super-black) clamp.
    #[default]
    Limited,
    /// "PC" / JPEG range: Y and chroma use all of `0..=255`.
    Full,
}

/// How a video source's YUV bytes are to be interpreted.
///
/// The default is BT.709, limited range: the right guess for an untagged HD
/// stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct YuvEncoding {
    /// Colour matrix.
    pub matrix: YuvMatrix,
    /// Quantization range.
    pub range: YuvRange,
}

impl YuvEncoding {
    /// Construct from a matrix and a range.
    pub const fn new(matrix: YuvMatrix, range: YuvRange) -> Self {
        Self { matrix, range }
    }
}

/// A borrowed NV12 image: a full-resolution Y plane and a half-resolution
/// (both axes) plane of interleaved `[U, V]` pairs.
///
/// Odd widths and heights are allowed: the chroma plane is then
/// `ceil(width / 2) × ceil(height / 2)` pairs. Like [`Mask`](crate::Mask),
/// the last row of each plane only needs its payload bytes, not a whole
/// stride, so a plane can be a sub-slice of a bigger buffer.
#[derive(Debug, Clone, Copy)]
pub struct Nv12<'a> {
    /// Luma bytes, `width` per row, rows `y_stride` apart.
    pub y: &'a [u8],
    /// Bytes per luma row; `>= width`.
    pub y_stride: u32,
    /// Interleaved chroma bytes, `2 * ceil(width / 2)` per row, rows
    /// `uv_stride` apart, `ceil(height / 2)` rows.
    pub uv: &'a [u8],
    /// Bytes per chroma row; `>= 2 * ceil(width / 2)`.
    pub uv_stride: u32,
    /// Width in (luma) pixels.
    pub width: u32,
    /// Height in (luma) pixels.
    pub height: u32,
}

impl Nv12<'_> {
    /// The whole image as a rect at the origin.
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, cast_i32(self.width), cast_i32(self.height))
    }

    /// Whether the declared geometry fits in the two planes.
    pub fn is_valid(&self) -> bool {
        let (w, h) = (u64::from(self.width), u64::from(self.height));
        if w == 0 || h == 0 || w > i32::MAX as u64 || h > i32::MAX as u64 {
            return false;
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (ys, uvs) = (u64::from(self.y_stride), u64::from(self.uv_stride));
        ys >= w
            && uvs >= 2 * cw
            && self.y.len() as u64 >= (h - 1) * ys + w
            && self.uv.len() as u64 >= (ch - 1) * uvs + 2 * cw
    }
}

/// The byte order of a [`Packed422`] image's 4-byte groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Packed422Order {
    /// `Y0 U Y1 V` (V4L2 `YUYV`, DRM `YUYV`, a.k.a. YUY2). The default.
    #[default]
    Yuyv,
    /// `U Y0 V Y1` (V4L2 `UYVY`, DRM `UYVY`).
    Uyvy,
}

/// A borrowed packed 4:2:2 image: one plane of 4-byte groups, each holding
/// two horizontally adjacent pixels and the chroma pair they share (byte
/// order per [`Packed422Order`]).
///
/// The width must be even (every group is whole). As for [`Nv12`], the last
/// row only needs its `2 * width` payload bytes, not a whole stride.
#[derive(Debug, Clone, Copy)]
pub struct Packed422<'a> {
    /// The packed bytes, `2 * width` per row, rows `stride` apart.
    pub data: &'a [u8],
    /// Bytes per row; `>= 2 * width`.
    pub stride: u32,
    /// Width in (luma) pixels; must be even.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Byte order of the groups.
    pub order: Packed422Order,
}

impl Packed422<'_> {
    /// The whole image as a rect at the origin.
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, cast_i32(self.width), cast_i32(self.height))
    }

    /// Whether the declared geometry fits in the plane: a non-zero, even
    /// width, `stride >= 2 * width`, and `stride * (height − 1) + 2 * width`
    /// bytes.
    pub fn is_valid(&self) -> bool {
        let (w, h) = (u64::from(self.width), u64::from(self.height));
        if w == 0 || h == 0 || w > i32::MAX as u64 || h > i32::MAX as u64 || w % 2 != 0 {
            return false;
        }
        let s = u64::from(self.stride);
        s >= 2 * w && self.data.len() as u64 >= (h - 1) * s + 2 * w
    }
}

/// `(luma, u, v)` byte offsets inside a packed 4:2:2 group: pixel `2j + i`'s
/// luma is byte `4j + luma + 2i`, its chroma bytes `4j + u` and `4j + v`.
#[inline]
const fn packed_offsets<const UYVY: bool>() -> (usize, usize, usize) {
    if UYVY { (1, 0, 2) } else { (0, 1, 3) }
}

fn cast_i32(v: u32) -> i32 {
    v.min(i32::MAX.unsigned_abs()).cast_signed()
}

/// Fraction bits of the conversion coefficients.
const FRAC: u32 = 12;

/// The per-call fixed-point conversion constants.
///
/// With `Y`, `U`, `V` scaled by `2^e` (`e = 0` for the 1:1 path, `e = 8` for
/// bilinear samples, which carry 8 fraction bits):
///
/// ```text
/// R = ((Y − yoff)·ys + (V − 128)·crv                + round) >> (12 + e)
/// G = ((Y − yoff)·ys − (U − 128)·cgu − (V − 128)·cgv + round) >> (12 + e)
/// B = ((Y − yoff)·ys + (U − 128)·cbu                + round) >> (12 + e)
/// ```
///
/// clamped to `0..=255`. The coefficients are the float matrix times 4096,
/// rounded; the largest is `cbu` for BT.2020 limited, 2.142 → 8773.
///
/// **Headroom** (the worst case, `e = 8`, limited range): the luma term is at
/// most `(255 − 16)·256 · 4769 ≈ 2.92e8` in magnitude, a chroma term at most
/// `128·256 · 8773 ≈ 2.87e8`, and `cgu + cgv < 1.3·4096` keeps G smaller.
/// Sum `< 6e8`, well inside `i32` (`2.1e9`).
///
/// **Accuracy**: each coefficient is off by at most `0.5 / 4096`; over three
/// terms of at most 255 that is `< 0.1` of an output step, so the result is
/// the correctly rounded float value or one step from it.
#[derive(Debug, Clone, Copy)]
struct Coeffs {
    ys: i32,
    yoff: i32,
    coff: i32,
    crv: i32,
    cgu: i32,
    cgv: i32,
    cbu: i32,
    round: i32,
    shift: u32,
}

impl Coeffs {
    fn new(enc: YuvEncoding, e: u32) -> Self {
        let (kr, kb) = enc.matrix.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, yoff) = match enc.range {
            YuvRange::Limited => (255.0 / 219.0, 255.0 / 224.0, 16),
            YuvRange::Full => (1.0, 1.0, 0),
        };
        let q = |v: f64| (v * f64::from(1u32 << FRAC)).round() as i32;
        Self {
            ys: q(ys),
            yoff: yoff << e,
            coff: 128 << e,
            crv: q(2.0 * (1.0 - kr) * cs),
            cgu: q(2.0 * kb * (1.0 - kb) / kg * cs),
            cgv: q(2.0 * kr * (1.0 - kr) / kg * cs),
            cbu: q(2.0 * (1.0 - kb) * cs),
            round: 1 << (FRAC + e - 1),
            shift: FRAC + e,
        }
    }

    /// The luma term, rounding constant included.
    #[inline]
    fn luma(&self, y: i32) -> i32 {
        (y - self.yoff) * self.ys + self.round
    }

    /// The three chroma terms `[R, G, B]`.
    #[inline]
    fn chroma(&self, u: i32, v: i32) -> [i32; 3] {
        let (u, v) = (u - self.coff, v - self.coff);
        [v * self.crv, -(u * self.cgu + v * self.cgv), u * self.cbu]
    }

    /// One opaque pixel as a little-endian `u32` (`[B, G, R, 255]`).
    #[inline]
    fn pack(&self, l: i32, c: [i32; 3]) -> u32 {
        let r = ((l + c[0]) >> self.shift).clamp(0, 255) as u32;
        let g = ((l + c[1]) >> self.shift).clamp(0, 255) as u32;
        let b = ((l + c[2]) >> self.shift).clamp(0, 255) as u32;
        b | (g << 8) | (r << 16) | 0xFF00_0000
    }
}

/// The clipped destination region of one blit: rows `[y0, y1)`, columns
/// `[x0, x1)`.
#[derive(Debug, Clone, Copy)]
struct Region {
    y0: i32,
    y1: i32,
    x0: i32,
    x1: i32,
}

impl Canvas<'_> {
    /// Blit (a crop of) an NV12 video frame into `dst`, converting to RGB
    /// with `enc` and scaling to fit. Writes only inside
    /// `clip ∩ dst ∩ surface`, and stores (never blends): the result is
    /// opaque.
    ///
    /// `src_rect` is intersected with `src.bounds()`. An invalid source, an
    /// empty crop, an empty `dst` or an empty clip is a no-op. When `dst` and
    /// the crop have the same size, sampling is nearest (exact); otherwise
    /// bilinear. See the [module docs](crate::yuv) for the chroma siting.
    pub fn blit_nv12(
        &mut self,
        clip: &IRect,
        dst: &IRect,
        src: &Nv12<'_>,
        src_rect: &IRect,
        enc: YuvEncoding,
    ) {
        if !src.is_valid() || dst.is_empty() {
            return;
        }
        let sr = src_rect.intersect(&src.bounds());
        if sr.is_empty() {
            return;
        }
        let r = clip.intersect(&self.bounds()).intersect(dst);
        if r.is_empty() {
            return;
        }
        let region = Region {
            y0: r.y,
            y1: r.bottom(),
            x0: r.x,
            x1: r.right(),
        };
        if dst.w == sr.w && dst.h == sr.h {
            self.nv12_1to1(region, dst, src, &sr, &Coeffs::new(enc, 0));
        } else {
            self.nv12_scaled(region, dst, src, &sr, &Coeffs::new(enc, 8));
        }
    }

    /// Unscaled: nearest (exact) luma, the covering chroma sample.
    fn nv12_1to1(&mut self, r: Region, dst: &IRect, src: &Nv12<'_>, sr: &IRect, c: &Coeffs) {
        let stride = self.stride() as usize;
        let (ys, uvs) = (src.y_stride as usize, src.uv_stride as usize);
        // The source column of the region's first pixel. `lx` is odd when the
        // crop starts on an odd column or the clip cuts at one: that pixel
        // shares its chroma pair with the column before it.
        let lx0 = sr.x + (r.x0 - dst.x);
        let n = (r.x1 - r.x0) as usize;
        let data = self.data_mut();
        for y in r.y0..r.y1 {
            let ly = (sr.y + (y - dst.y)) as usize;
            let yrow = &src.y[ly * ys + lx0 as usize..][..n];
            let uvrow = &src.uv[(ly >> 1) * uvs + (lx0 as usize >> 1) * 2..];
            let start = y as usize * stride + r.x0 as usize * BYTES_PER_PIXEL;
            let drow = &mut data[start..start + n * BYTES_PER_PIXEL];
            nv12_row_1to1(drow, yrow, uvrow, lx0 & 1 == 1, c);
        }
    }

    /// Scaled: bilinear luma and chroma, one destination row at a time.
    fn nv12_scaled(&mut self, r: Region, dst: &IRect, src: &Nv12<'_>, sr: &IRect, c: &Coeffs) {
        let stride = self.stride() as usize;
        let (ys, uvs) = (src.y_stride as usize, src.uv_stride as usize);
        let map_x = Axis::new(sr.x, sr.w, dst.x, dst.w);
        let map_y = Axis::new(sr.y, sr.h, dst.y, dst.h);
        // Luma texel pairs `[ix, ix + 1]` in `[lx_first, lx_last]` need no
        // clamp; neither do chroma pairs in `[cx_first, cx_last]`.
        let (lx_first, lx_last) = (sr.x, sr.right() - 1);
        let (cx_first, cx_last) = (sr.x >> 1, (sr.right() - 1) >> 1);
        let (ly_first, ly_last) = (sr.y, sr.bottom() - 1);
        let (cy_first, cy_last) = (sr.y >> 1, (sr.bottom() - 1) >> 1);

        let (lo, hi) = (r.x0, r.x1);
        let base = map_x.at(lo);
        let step = map_x.step;
        // The chroma pair index is `fixed >> 17`, so "chroma pair in range"
        // is "luma index in `[2 * cx_first, 2 * cx_last)`" — the same shape
        // `texels_in_range` solves. The interior is the intersection.
        let (a0, a1) = texels_in_range(base, step, lo, hi, lx_first, lx_last);
        let (b0, b1) = texels_in_range(base, step, lo, hi, 2 * cx_first, 2 * cx_last);
        let (in_lo, in_hi) = if a0.max(b0) < a1.min(b1) {
            (a0.max(b0), a1.min(b1))
        } else {
            (lo, lo)
        };
        let edge = EdgeClamp {
            lx: (lx_first, lx_last),
            cx: (cx_first, cx_last),
        };

        let data = self.data_mut();
        for y in r.y0..r.y1 {
            let sy = map_y.at(y);
            let iy = clamp_shift(sy, ly_first, ly_last);
            let ly0 = iy.0 as usize;
            let ly1 = iy.1 as usize;
            // Chroma y = (ly − 0.5) / 2: centred between two luma rows.
            let cy = (sy - 32768) >> 1;
            let icy = clamp_shift(cy, cy_first, cy_last);
            let row = RowSrc {
                y0: &src.y[ly0 * ys..],
                y1: &src.y[ly1 * ys..],
                uv0: &src.uv[icy.0 as usize * uvs..],
                uv1: &src.uv[icy.1 as usize * uvs..],
                ty: ((sy >> 8) & 0xFF) as i32,
                tcy: ((cy >> 8) & 0xFF) as i32,
                step,
            };
            let start = y as usize * stride + lo as usize * BYTES_PER_PIXEL;
            let drow = &mut data[start..start + (hi - lo) as usize * BYTES_PER_PIXEL];
            let (head, rest) = drow.split_at_mut((in_lo - lo) as usize * BYTES_PER_PIXEL);
            let (mid, tail) = rest.split_at_mut((in_hi - in_lo) as usize * BYTES_PER_PIXEL);
            nv12_run_edge(head, &row, base, &edge, c);
            nv12_run_inner(mid, &row, map_x.at(in_lo), c);
            nv12_run_edge(tail, &row, map_x.at(in_hi), &edge, c);
        }
    }
}

impl Canvas<'_> {
    /// Blit (a crop of) a packed 4:2:2 video frame (YUYV or UYVY, per
    /// `src.order`) into `dst`, converting to RGB with `enc` and scaling to
    /// fit. Same contract as [`Canvas::blit_nv12`]: writes only inside
    /// `clip ∩ dst ∩ surface`, stores opaque pixels, nearest sampling when
    /// `dst` and the crop have the same size and bilinear otherwise,
    /// clip-invariant. `src_rect` is intersected with `src.bounds()`; an
    /// invalid source, an empty crop, an empty `dst` or an empty clip is a
    /// no-op. See the [module docs](crate::yuv) for the chroma siting.
    pub fn blit_yuyv(
        &mut self,
        clip: &IRect,
        dst: &IRect,
        src: &Packed422<'_>,
        src_rect: &IRect,
        enc: YuvEncoding,
    ) {
        if !src.is_valid() || dst.is_empty() {
            return;
        }
        let sr = src_rect.intersect(&src.bounds());
        if sr.is_empty() {
            return;
        }
        let r = clip.intersect(&self.bounds()).intersect(dst);
        if r.is_empty() {
            return;
        }
        let region = Region {
            y0: r.y,
            y1: r.bottom(),
            x0: r.x,
            x1: r.right(),
        };
        let one = dst.w == sr.w && dst.h == sr.h;
        let c = Coeffs::new(enc, if one { 0 } else { 8 });
        match (src.order, one) {
            (Packed422Order::Yuyv, true) => self.packed_1to1::<false>(region, dst, src, &sr, &c),
            (Packed422Order::Uyvy, true) => self.packed_1to1::<true>(region, dst, src, &sr, &c),
            (Packed422Order::Yuyv, false) => self.packed_scaled::<false>(region, dst, src, &sr, &c),
            (Packed422Order::Uyvy, false) => self.packed_scaled::<true>(region, dst, src, &sr, &c),
        }
    }

    /// Unscaled packed 4:2:2: exact luma, the covering group's chroma.
    fn packed_1to1<const UYVY: bool>(
        &mut self,
        r: Region,
        dst: &IRect,
        src: &Packed422<'_>,
        sr: &IRect,
        c: &Coeffs,
    ) {
        let stride = self.stride() as usize;
        let ps = src.stride as usize;
        // Odd `lx0`: the first pixel is the second of its group.
        let lx0 = sr.x + (r.x0 - dst.x);
        let n = (r.x1 - r.x0) as usize;
        let data = self.data_mut();
        for y in r.y0..r.y1 {
            let ly = (sr.y + (y - dst.y)) as usize;
            let srow = &src.data[ly * ps + (lx0 as usize >> 1) * 4..];
            let start = y as usize * stride + r.x0 as usize * BYTES_PER_PIXEL;
            let drow = &mut data[start..start + n * BYTES_PER_PIXEL];
            packed_row_1to1::<UYVY>(drow, srow, lx0 & 1 == 1, c);
        }
    }

    /// Scaled packed 4:2:2: bilinear luma, bilinear chroma at half
    /// horizontal resolution on the luma rows.
    fn packed_scaled<const UYVY: bool>(
        &mut self,
        r: Region,
        dst: &IRect,
        src: &Packed422<'_>,
        sr: &IRect,
        c: &Coeffs,
    ) {
        let stride = self.stride() as usize;
        let ps = src.stride as usize;
        let map_x = Axis::new(sr.x, sr.w, dst.x, dst.w);
        let map_y = Axis::new(sr.y, sr.h, dst.y, dst.h);
        let (lx_first, lx_last) = (sr.x, sr.right() - 1);
        let (cx_first, cx_last) = (sr.x >> 1, (sr.right() - 1) >> 1);
        let (lo, hi) = (r.x0, r.x1);
        let base = map_x.at(lo);
        let step = map_x.step;
        // The same interior split as `nv12_scaled`.
        let (a0, a1) = texels_in_range(base, step, lo, hi, lx_first, lx_last);
        let (b0, b1) = texels_in_range(base, step, lo, hi, 2 * cx_first, 2 * cx_last);
        let (in_lo, in_hi) = if a0.max(b0) < a1.min(b1) {
            (a0.max(b0), a1.min(b1))
        } else {
            (lo, lo)
        };
        let edge = EdgeClamp {
            lx: (lx_first, lx_last),
            cx: (cx_first, cx_last),
        };
        let data = self.data_mut();
        for y in r.y0..r.y1 {
            let sy = map_y.at(y);
            let (y0, y1) = clamp_shift(sy, sr.y, sr.bottom() - 1);
            let row = PackedRow {
                r0: &src.data[y0 as usize * ps..],
                r1: &src.data[y1 as usize * ps..],
                ty: ((sy >> 8) & 0xFF) as i32,
                step,
            };
            let start = y as usize * stride + lo as usize * BYTES_PER_PIXEL;
            let drow = &mut data[start..start + (hi - lo) as usize * BYTES_PER_PIXEL];
            let (head, rest) = drow.split_at_mut((in_lo - lo) as usize * BYTES_PER_PIXEL);
            let (mid, tail) = rest.split_at_mut((in_hi - in_lo) as usize * BYTES_PER_PIXEL);
            packed_run_edge::<UYVY>(head, &row, base, &edge, c);
            packed_run_inner::<UYVY>(mid, &row, map_x.at(in_lo), c);
            packed_run_edge::<UYVY>(tail, &row, map_x.at(in_hi), &edge, c);
        }
    }
}

impl Canvas<'_> {
    /// Scale (a crop of) an opaque [`PixelFormat::Xrgb8888`] image into the
    /// integer rect `dst`: a *store*, bilinear (nearest when `dst` and the
    /// crop have the same size), byte 3 (alpha) written as 255.
    ///
    /// The same row machinery as [`Canvas::blit_nv12`] — separable bilinear,
    /// source positions affine in the absolute destination pixel, so it is
    /// clip-invariant — minus the colour conversion. It exists because it is
    /// about twice as fast as the general [`Canvas::blit`] on this case (see
    /// the crate README); it agrees with it to ±1. An `Argb8888` source is
    /// straight alpha and cannot be stored, so it is a no-op here: use
    /// [`Canvas::blit`].
    ///
    /// `src_rect` is intersected with the image bounds; an invalid source,
    /// an empty crop, an empty `dst` or an empty clip is a no-op.
    pub fn blit_xrgb_scaled(
        &mut self,
        clip: &IRect,
        dst: &IRect,
        src: &Image<'_>,
        src_rect: &IRect,
    ) {
        if src.format != PixelFormat::Xrgb8888 || !src.is_valid() || dst.is_empty() {
            return;
        }
        let sr = src_rect.intersect(&src.bounds());
        if sr.is_empty() {
            return;
        }
        let r = clip.intersect(&self.bounds()).intersect(dst);
        if r.is_empty() {
            return;
        }
        let stride = self.stride() as usize;
        let pitch = src.stride as usize;
        let map_x = Axis::new(sr.x, sr.w, dst.x, dst.w);
        let map_y = Axis::new(sr.y, sr.h, dst.y, dst.h);
        let one_to_one = dst.w == sr.w && dst.h == sr.h;
        let (x_first, x_last) = (sr.x, sr.right() - 1);
        let (lo, hi) = (r.x, r.right());
        let base = map_x.at(lo);
        let (in_lo, in_hi) = texels_in_range(base, map_x.step, lo, hi, x_first, x_last);
        let data = self.data_mut();
        for y in r.y..r.bottom() {
            let start = y as usize * stride + lo as usize * BYTES_PER_PIXEL;
            let drow = &mut data[start..start + (hi - lo) as usize * BYTES_PER_PIXEL];
            if one_to_one {
                let sy = (sr.y + (y - dst.y)) as usize;
                let sx = (sr.x + (lo - dst.x)) as usize;
                let srow = &src.data[sy * pitch + sx * BYTES_PER_PIXEL..][..drow.len()];
                for (d, p) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
                    d.copy_from_slice(&[p[0], p[1], p[2], 255]);
                }
                continue;
            }
            let sy = map_y.at(y);
            let (y0, y1) = clamp_shift(sy, sr.y, sr.bottom() - 1);
            let row = XrgbRow {
                r0: &src.data[y0 as usize * pitch..],
                r1: &src.data[y1 as usize * pitch..],
                ty: ((sy >> 8) & 0xFF) as i32,
                step: map_x.step,
            };
            let (head, rest) = drow.split_at_mut((in_lo - lo) as usize * BYTES_PER_PIXEL);
            let (mid, tail) = rest.split_at_mut((in_hi - in_lo) as usize * BYTES_PER_PIXEL);
            xrgb_run_edge(head, &row, base, (x_first, x_last));
            xrgb_run_inner(mid, &row, map_x.at(in_lo));
            xrgb_run_edge(tail, &row, map_x.at(in_hi), (x_first, x_last));
        }
    }
}

/// The two source rows and vertical weight of one XRGB destination row.
#[derive(Debug, Clone, Copy)]
struct XrgbRow<'a> {
    r0: &'a [u8],
    r1: &'a [u8],
    ty: i32,
    step: i64,
}

/// The interior of an XRGB scaled row: vertical pass over the contiguous
/// source span into `u16` stack bytes, then a horizontal lerp per pixel.
/// Bit-identical to the direct four-tap form, as in [`nv12_run_inner`].
fn xrgb_run_inner(row: &mut [u8], s: &XrgbRow<'_>, base: i64) {
    let mut vb = [0u16; SPAN * BYTES_PER_PIXEL];
    let max_n = chunk_cols(s.step);
    let (w1, w0) = (s.ty as u16, 256 - s.ty as u16);
    let mut fixed = base;
    let mut rest = row;
    while !rest.is_empty() {
        let n = (rest.len() / BYTES_PER_PIXEL).min(max_n);
        let (dchunk, tail) = rest.split_at_mut(n * BYTES_PER_PIXEL);
        rest = tail;
        let last = fixed + i64::from((n - 1) as u32) * s.step;
        let b0 = (fixed >> 16) as usize * BYTES_PER_PIXEL;
        let b1 = ((last >> 16) as usize + 2) * BYTES_PER_PIXEL;
        for ((d, &a), &b) in vb[..b1 - b0]
            .iter_mut()
            .zip(&s.r0[b0..b1])
            .zip(&s.r1[b0..b1])
        {
            *d = u16::from(a) * w0 + u16::from(b) * w1;
        }
        for d in dchunk.chunks_exact_mut(4) {
            let o = (fixed >> 16) as usize * BYTES_PER_PIXEL - b0;
            let tx = ((fixed >> 8) & 0xFF) as u32;
            fixed += s.step;
            let p = &vb[o..o + 8];
            let h =
                |i: usize| ((u32::from(p[i]) * (256 - tx) + u32::from(p[i + 4]) * tx) >> 16) as u8;
            d.copy_from_slice(&[h(0), h(1), h(2), 255]);
        }
    }
}

/// The edge columns of an XRGB scaled row: clamped four-tap sampling.
fn xrgb_run_edge(row: &mut [u8], s: &XrgbRow<'_>, base: i64, range: (i32, i32)) {
    let mut fixed = base;
    for d in row.chunks_exact_mut(4) {
        let (a, b) = clamp_shift(fixed, range.0, range.1);
        let tx = ((fixed >> 8) & 0xFF) as i32;
        fixed = fixed.saturating_add(s.step);
        let (a, b) = (a as usize * BYTES_PER_PIXEL, b as usize * BYTES_PER_PIXEL);
        let h = |i: usize| {
            (bilerp(s.r0[a + i], s.r0[b + i], s.r1[a + i], s.r1[b + i], tx, s.ty) >> 8) as u8
        };
        d.copy_from_slice(&[h(0), h(1), h(2), 255]);
    }
}

/// One axis of the destination → source mapping, in 16.16 fixed point:
/// the source coordinate (texel centres on integers) of destination pixel
/// `d` is `base + (d − origin) * step`.
///
/// Affine in the *absolute* destination coordinate, so where a row or run
/// starts does not change what it samples (clip invariance). All arithmetic
/// saturates: a pathological rect (a huge source squeezed into one pixel, a
/// destination far off-surface) cannot overflow, it only clamps.
#[derive(Debug, Clone, Copy)]
struct Axis {
    base: i64,
    step: i64,
    origin: i64,
}

impl Axis {
    fn new(src_pos: i32, src_len: i32, dst_pos: i32, dst_len: i32) -> Self {
        let step = (i64::from(src_len) << 16) / i64::from(dst_len);
        // Centre of destination pixel 0 → source `(0.5 · scale) + pos − 0.5`.
        let base = (i64::from(src_pos) << 16) + step / 2 - 32768;
        Self {
            base,
            step,
            origin: i64::from(dst_pos),
        }
    }

    fn at(&self, d: i32) -> i64 {
        (i64::from(d) - self.origin)
            .saturating_mul(self.step)
            .saturating_add(self.base)
    }
}

/// The two texel indices `[i, i + 1]` a 16.16 position interpolates
/// between, each clamped to `[first, last]`.
#[inline]
fn clamp_shift(fixed: i64, first: i32, last: i32) -> (i32, i32) {
    let i = fixed >> 16;
    let a = i.clamp(i64::from(first), i64::from(last)) as i32;
    let b = (i + 1).clamp(i64::from(first), i64::from(last)) as i32;
    (a, b)
}

/// The source rows and vertical weights one destination row samples.
#[derive(Debug, Clone, Copy)]
struct RowSrc<'a> {
    y0: &'a [u8],
    y1: &'a [u8],
    uv0: &'a [u8],
    uv1: &'a [u8],
    /// Luma vertical weight, `0..=255`.
    ty: i32,
    /// Chroma vertical weight, `0..=255`.
    tcy: i32,
    step: i64,
}

/// The clamp ranges of the edge runs: luma and chroma columns.
#[derive(Debug, Clone, Copy)]
struct EdgeClamp {
    lx: (i32, i32),
    cx: (i32, i32),
}

/// Bilinear blend of four bytes with 8-bit weights; the result carries 8
/// fraction bits (`value · 256`). At most `255 · 65536 >> 8`, so `i32` is
/// ample.
#[inline]
fn bilerp(a: u8, b: u8, c: u8, d: u8, tx: i32, ty: i32) -> i32 {
    let top = i32::from(a) * (256 - tx) + i32::from(b) * tx;
    let bot = i32::from(c) * (256 - tx) + i32::from(d) * tx;
    (top * (256 - ty) + bot * ty) >> 8
}

/// Destination columns per chunk whose source span fits [`SPAN`] at this
/// 16.16 step: at most [`CHUNK`], at least 1.
#[allow(clippy::cast_possible_wrap)] // SPAN and CHUNK are small constants
fn chunk_cols(step: i64) -> usize {
    const SPAN_FIXED: i64 = (SPAN as i64 - 2) << 16;
    (SPAN_FIXED / step.max(1)).clamp(1, CHUNK as i64) as usize
}

/// Destination columns per chunk of [`nv12_run_inner`].
const CHUNK: usize = 128;
/// Source luma columns one chunk may span (its vertically blended row lives
/// on the stack as `u16`s). Chroma needs half as many pairs, plus one.
const SPAN: usize = 512;

/// The interior of a scaled row: every luma and chroma pair in range, so the
/// loop is clamp-free.
///
/// Separable, per chunk of destination columns:
///
/// 1. **Vertical**: blend the two luma rows (and the two chroma rows) over
///    the contiguous source span the chunk touches into `u16` stack rows —
///    `a·(256 − ty) + b·ty`, at most `255·256`. Contiguous loads and
///    multiplies, which vectorize.
/// 2. **Horizontal**: per destination pixel, two loads from each blended
///    row and one lerp, then the conversion.
///
/// The result is **bit-identical** to the direct four-tap form
/// `((a·(256−tx) + b·tx)·(256−ty) + (c·(256−tx) + d·tx)·ty) >> 8`: both are
/// the same exact sum of `w_x·w_y·p` products before the one shift. This
/// halves the gathers, which are what a scaled row costs.
///
/// A chunk is shortened so its source span fits [`SPAN`]; at an extreme
/// downscale that is a single column, which always fits.
#[inline]
fn nv12_run_inner(row: &mut [u8], s: &RowSrc<'_>, base: i64, c: &Coeffs) {
    let mut vy = [0u16; SPAN];
    let mut vuv = [0u16; SPAN + 4];
    let (mut hy, mut hu, mut hv) = ([0i32; CHUNK], [0i32; CHUNK], [0i32; CHUNK]);
    let max_n = chunk_cols(s.step);
    let (wy1, wy0) = (s.ty as u16, 256 - s.ty as u16);
    let (wc1, wc0) = (s.tcy as u16, 256 - s.tcy as u16);
    let mut fixed = base;
    let mut rest = row;
    while !rest.is_empty() {
        let n = (rest.len() / BYTES_PER_PIXEL).min(max_n);
        let (dchunk, tail) = rest.split_at_mut(n * BYTES_PER_PIXEL);
        rest = tail;
        let last = fixed + i64::from((n - 1) as u32) * s.step;
        // Luma span `[l0, l1]` and chroma pair span `[c0, c1]` (inclusive
        // of the `+1` neighbour), all in range by the caller's contract.
        let l0 = (fixed >> 16) as usize;
        let l1 = (last >> 16) as usize + 1;
        let c0 = (fixed >> 17) as usize;
        let c1 = (last >> 17) as usize + 1;
        for ((d, &a), &b) in vy[..=l1 - l0]
            .iter_mut()
            .zip(&s.y0[l0..=l1])
            .zip(&s.y1[l0..=l1])
        {
            *d = u16::from(a) * wy0 + u16::from(b) * wy1;
        }
        let cb = 2 * (c1 - c0 + 1);
        for ((d, &a), &b) in vuv[..cb]
            .iter_mut()
            .zip(&s.uv0[2 * c0..2 * c0 + cb])
            .zip(&s.uv1[2 * c0..2 * c0 + cb])
        {
            *d = u16::from(a) * wc0 + u16::from(b) * wc1;
        }
        let (hy, hu, hv) = (&mut hy[..n], &mut hu[..n], &mut hv[..n]);
        for ((y, u), v) in hy.iter_mut().zip(hu.iter_mut()).zip(hv.iter_mut()) {
            let o = (fixed >> 16) as usize - l0;
            let tx = ((fixed >> 8) & 0xFF) as i32;
            let cf = fixed >> 1;
            let co = ((cf >> 16) as usize - c0) * 2;
            let tcx = ((cf >> 8) & 0xFF) as i32;
            fixed += s.step;
            let h = |a: u16, b: u16, t: i32| (i32::from(a) * (256 - t) + i32::from(b) * t) >> 8;
            *y = h(vy[o], vy[o + 1], tx);
            *u = h(vuv[co], vuv[co + 2], tcx);
            *v = h(vuv[co + 1], vuv[co + 3], tcx);
        }
        for (((d, &y), &u), &v) in dchunk.chunks_exact_mut(4).zip(&*hy).zip(&*hu).zip(&*hv) {
            d.copy_from_slice(&c.pack(c.luma(y), c.chroma(u, v)).to_le_bytes());
        }
    }
}

/// The leading/trailing columns of a scaled row: edge-clamped sampling.
fn nv12_run_edge(row: &mut [u8], s: &RowSrc<'_>, base: i64, e: &EdgeClamp, c: &Coeffs) {
    let mut fixed = base;
    for d in row.chunks_exact_mut(4) {
        let (xa, xb) = clamp_shift(fixed, e.lx.0, e.lx.1);
        let tx = ((fixed >> 8) & 0xFF) as i32;
        let cf = fixed >> 1;
        let (ca, cb) = clamp_shift(cf, e.cx.0, e.cx.1);
        let tcx = ((cf >> 8) & 0xFF) as i32;
        fixed = fixed.saturating_add(s.step);
        let (xa, xb) = (xa as usize, xb as usize);
        let (ca, cb) = (ca as usize * 2, cb as usize * 2);
        let l = bilerp(s.y0[xa], s.y0[xb], s.y1[xa], s.y1[xb], tx, s.ty);
        let u = bilerp(s.uv0[ca], s.uv0[cb], s.uv1[ca], s.uv1[cb], tcx, s.tcy);
        let v = bilerp(
            s.uv0[ca + 1],
            s.uv0[cb + 1],
            s.uv1[ca + 1],
            s.uv1[cb + 1],
            tcx,
            s.tcy,
        );
        d.copy_from_slice(&c.pack(c.luma(l), c.chroma(u, v)).to_le_bytes());
    }
}

/// One unscaled row. `yrow` is the row's luma, `uvrow` starts at the chroma
/// pair of its first pixel; `odd` says that pixel is the *second* of its
/// pair.
fn nv12_row_1to1(drow: &mut [u8], yrow: &[u8], uvrow: &[u8], odd: bool, c: &Coeffs) {
    let (drow, yrow, uvrow) = if odd && !yrow.is_empty() {
        let ch = c.chroma(i32::from(uvrow[0]), i32::from(uvrow[1]));
        let px = c.pack(c.luma(i32::from(yrow[0])), ch);
        let (first, rest) = drow.split_at_mut(BYTES_PER_PIXEL);
        first.copy_from_slice(&px.to_le_bytes());
        (rest, &yrow[1..], &uvrow[2..])
    } else {
        (drow, yrow, uvrow)
    };
    // Two pixels per chroma pair. Per chunk: widen luma and duplicate each
    // chroma pair into stack arrays, then one straight-line conversion pass
    // over them, which vectorizes (the fused per-pair loop did not).
    let pairs = yrow.len() / 2;
    let (dh, dt) = drow.split_at_mut(pairs * 8);
    let (yh, yt) = yrow.split_at(pairs * 2);
    let (mut ay, mut au, mut av) = ([0i32; CHUNK], [0i32; CHUNK], [0i32; CHUNK]);
    for ((dc, yc), uvc) in dh
        .chunks_mut(CHUNK * BYTES_PER_PIXEL)
        .zip(yh.chunks(CHUNK))
        .zip(uvrow.chunks(CHUNK))
    {
        let n = yc.len();
        for (a, &y) in ay[..n].iter_mut().zip(yc) {
            *a = i32::from(y);
        }
        for ((u, v), p) in au[..n]
            .chunks_exact_mut(2)
            .zip(av[..n].chunks_exact_mut(2))
            .zip(uvc.chunks_exact(2))
        {
            let (pu, pv) = (i32::from(p[0]), i32::from(p[1]));
            u[0] = pu;
            u[1] = pu;
            v[0] = pv;
            v[1] = pv;
        }
        for (((d, &y), &u), &v) in dc
            .chunks_exact_mut(4)
            .zip(&ay[..n])
            .zip(&au[..n])
            .zip(&av[..n])
        {
            d.copy_from_slice(&c.pack(c.luma(y), c.chroma(u, v)).to_le_bytes());
        }
    }
    // An odd tail: one pixel, the first of its pair.
    if let (Some(&y), Some(uv)) = (yt.first(), uvrow.get(pairs * 2..pairs * 2 + 2)) {
        let ch = c.chroma(i32::from(uv[0]), i32::from(uv[1]));
        dt.copy_from_slice(&c.pack(c.luma(i32::from(y)), ch).to_le_bytes());
    }
}

/// The two source rows and vertical weight of one packed 4:2:2 destination
/// row (luma and chroma share both).
#[derive(Debug, Clone, Copy)]
struct PackedRow<'a> {
    r0: &'a [u8],
    r1: &'a [u8],
    ty: i32,
    step: i64,
}

/// The interior of a packed 4:2:2 scaled row: vertical pass over the
/// contiguous packed span (luma and chroma bytes together) into `u16` stack
/// bytes, then per pixel one horizontal lerp per channel, then the
/// conversion. Bit-identical to the four-tap edge form, as in
/// [`nv12_run_inner`].
#[inline]
fn packed_run_inner<const UYVY: bool>(row: &mut [u8], s: &PackedRow<'_>, base: i64, c: &Coeffs) {
    let (yo, uo, vo) = packed_offsets::<UYVY>();
    // A chunk's luma span is at most `SPAN − 1` columns, so its groups span
    // at most `2 * SPAN + 8` bytes (see `chunk_cols`).
    let mut vb = [0u16; 2 * SPAN + 8];
    let (mut hy, mut hu, mut hv) = ([0i32; CHUNK], [0i32; CHUNK], [0i32; CHUNK]);
    let max_n = chunk_cols(s.step);
    let (w1, w0) = (s.ty as u16, 256 - s.ty as u16);
    let mut fixed = base;
    let mut rest = row;
    while !rest.is_empty() {
        let n = (rest.len() / BYTES_PER_PIXEL).min(max_n);
        let (dchunk, tail) = rest.split_at_mut(n * BYTES_PER_PIXEL);
        rest = tail;
        let last = fixed + i64::from((n - 1) as u32) * s.step;
        // Groups `[g0, g1]` cover every luma column and chroma pair (with
        // their `+1` neighbours) the chunk reads; in range by contract.
        let b0 = (fixed >> 17) as usize * 4;
        let b1 = ((last >> 17) as usize + 2) * 4;
        for ((d, &a), &b) in vb[..b1 - b0]
            .iter_mut()
            .zip(&s.r0[b0..b1])
            .zip(&s.r1[b0..b1])
        {
            *d = u16::from(a) * w0 + u16::from(b) * w1;
        }
        let (hy, hu, hv) = (&mut hy[..n], &mut hu[..n], &mut hv[..n]);
        for ((y, u), v) in hy.iter_mut().zip(hu.iter_mut()).zip(hv.iter_mut()) {
            let o = (fixed >> 16) as usize * 2 + yo - b0;
            let tx = ((fixed >> 8) & 0xFF) as i32;
            let cf = fixed >> 1;
            let co = (cf >> 16) as usize * 4 - b0;
            let tcx = ((cf >> 8) & 0xFF) as i32;
            fixed += s.step;
            let h = |a: u16, b: u16, t: i32| (i32::from(a) * (256 - t) + i32::from(b) * t) >> 8;
            *y = h(vb[o], vb[o + 2], tx);
            *u = h(vb[co + uo], vb[co + uo + 4], tcx);
            *v = h(vb[co + vo], vb[co + vo + 4], tcx);
        }
        for (((d, &y), &u), &v) in dchunk.chunks_exact_mut(4).zip(&*hy).zip(&*hu).zip(&*hv) {
            d.copy_from_slice(&c.pack(c.luma(y), c.chroma(u, v)).to_le_bytes());
        }
    }
}

/// The leading/trailing columns of a packed 4:2:2 scaled row: edge-clamped
/// four-tap sampling.
fn packed_run_edge<const UYVY: bool>(
    row: &mut [u8],
    s: &PackedRow<'_>,
    base: i64,
    e: &EdgeClamp,
    c: &Coeffs,
) {
    let (yo, uo, vo) = packed_offsets::<UYVY>();
    let mut fixed = base;
    for d in row.chunks_exact_mut(4) {
        let (xa, xb) = clamp_shift(fixed, e.lx.0, e.lx.1);
        let tx = ((fixed >> 8) & 0xFF) as i32;
        let cf = fixed >> 1;
        let (ca, cb) = clamp_shift(cf, e.cx.0, e.cx.1);
        let tcx = ((cf >> 8) & 0xFF) as i32;
        fixed = fixed.saturating_add(s.step);
        let (xa, xb) = (xa as usize * 2 + yo, xb as usize * 2 + yo);
        let (ca, cb) = (ca as usize * 4, cb as usize * 4);
        let tap = |a: usize, b: usize, t: i32| bilerp(s.r0[a], s.r0[b], s.r1[a], s.r1[b], t, s.ty);
        let l = tap(xa, xb, tx);
        let u = tap(ca + uo, cb + uo, tcx);
        let v = tap(ca + vo, cb + vo, tcx);
        d.copy_from_slice(&c.pack(c.luma(l), c.chroma(u, v)).to_le_bytes());
    }
}

/// One unscaled packed 4:2:2 row. `srow` starts at the group of the first
/// pixel; `odd` says that pixel is the *second* of its group.
fn packed_row_1to1<const UYVY: bool>(drow: &mut [u8], srow: &[u8], odd: bool, c: &Coeffs) {
    let (yo, uo, vo) = packed_offsets::<UYVY>();
    let (drow, srow) = if odd && !drow.is_empty() {
        let g = &srow[..4];
        let ch = c.chroma(i32::from(g[uo]), i32::from(g[vo]));
        let px = c.pack(c.luma(i32::from(g[yo + 2])), ch);
        let (first, rest) = drow.split_at_mut(BYTES_PER_PIXEL);
        first.copy_from_slice(&px.to_le_bytes());
        (rest, &srow[4..])
    } else {
        (drow, srow)
    };
    // Per chunk: deinterleave into stack arrays (a trailing odd pixel reads
    // its whole group, which a valid image always has), then one
    // straight-line conversion pass, as in `nv12_row_1to1`.
    let (mut ay, mut au, mut av) = ([0i32; CHUNK], [0i32; CHUNK], [0i32; CHUNK]);
    for (dc, sc) in drow
        .chunks_mut(CHUNK * BYTES_PER_PIXEL)
        .zip(srow.chunks(CHUNK * 2))
    {
        let m = dc.len() / BYTES_PER_PIXEL;
        let groups = m.div_ceil(2);
        for (((g, y), u), v) in sc[..4 * groups]
            .chunks_exact(4)
            .zip(ay.chunks_exact_mut(2))
            .zip(au.chunks_exact_mut(2))
            .zip(av.chunks_exact_mut(2))
        {
            y[0] = i32::from(g[yo]);
            y[1] = i32::from(g[yo + 2]);
            u[0] = i32::from(g[uo]);
            u[1] = u[0];
            v[0] = i32::from(g[vo]);
            v[1] = v[0];
        }
        for (((d, &y), &u), &v) in dc
            .chunks_exact_mut(4)
            .zip(&ay[..m])
            .zip(&au[..m])
            .zip(&av[..m])
        {
            d.copy_from_slice(&c.pack(c.luma(y), c.chroma(u, v)).to_le_bytes());
        }
    }
}
