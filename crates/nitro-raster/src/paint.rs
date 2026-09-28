//! Row-level painting — the innermost loops.
//!
//! Everything here works on one contiguous slice of an XRGB8888 row that has
//! already been clipped, so there is no per-pixel bounds check and the loops
//! are shaped for autovectorization (`chunks_exact_mut(4)`, one 4-byte store
//! per pixel).

use nitro_core::Color;

use crate::blend::{effective_alpha, effective_alpha_cov, over_straight};

/// A [`Fill`](crate::Fill) specialised for one device row.
///
/// Built once per row; the painting loops match on it once, outside the pixel
/// loop.
pub(crate) enum RowPaint {
    /// One colour for the whole row.
    Solid(Color),
    /// Linear gradient: `t` at device x = 0 (pixel centre 0.5), stepping by
    /// `dt` per pixel, clamped to `[0, 1]` (pad).
    Linear {
        /// Gradient parameter at device x = 0.
        t0: f32,
        /// Change of `t` per device pixel.
        dt: f32,
        /// Colour at `t == 0`.
        c0: Color,
        /// Colour at `t == 1`.
        c1: Color,
    },
}

impl RowPaint {
    /// Whether every pixel this paint produces is fully opaque.
    #[inline]
    pub(crate) fn is_opaque(&self) -> bool {
        match self {
            Self::Solid(c) => c.is_opaque(),
            Self::Linear { c0, c1, .. } => c0.is_opaque() && c1.is_opaque(),
        }
    }

    /// Colour at device column `x`.
    #[inline]
    pub(crate) fn color_at(&self, x: i32) -> Color {
        match self {
            Self::Solid(c) => *c,
            Self::Linear { t0, dt, c0, c1 } => {
                lerp_color(*c0, *c1, (t0 + dt * x as f32).clamp(0.0, 1.0))
            }
        }
    }
}

/// Channel-wise linear interpolation with round-to-nearest.
///
/// Exact at the endpoints: `t == 0` yields `a`, `t == 1` yields `b`.
///
/// The single implementation: `canvas.rs` calls it for the vertical-gradient
/// case (one colour per row) and `RowPaint::color_at` for the horizontal one
/// (one colour per pixel). There used to be a byte-identical copy here and in
/// `canvas.rs` (`lerp_row_color`); see issue #524.
#[inline]
pub(crate) fn lerp_color(a: Color, b: Color, t: f32) -> Color {
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

/// Store one opaque colour over a whole row slice (no blending).
///
/// Writes 8 bytes (two pixels) at a time: the destination is typically
/// write-combining memory, where wider stores are strictly better, and the
/// remainder is at most one pixel.
#[inline]
pub(crate) fn store_solid(row: &mut [u8], c: Color) {
    let px = u32::from_le_bytes([c.b, c.g, c.r, 0]);
    let pair = (u64::from(px) | (u64::from(px) << 32)).to_le_bytes();
    let split = row.len() & !7;
    let (head, tail) = row.split_at_mut(split);
    for d in head.chunks_exact_mut(8) {
        d.copy_from_slice(&pair);
    }
    for d in tail.chunks_exact_mut(4) {
        d.copy_from_slice(&px.to_le_bytes());
    }
}

/// One channel of [`blend_solid`]: `round((premul + dst * inv) / 255)` with
/// the `+128` already folded into `premul`.
///
/// Also the whole of a stroke band's inner loop, which pre-resolves the same
/// `premul`/`inv` pair per column instead of per row.
#[inline]
pub(crate) fn mix(premul: u32, dst: u8, inv: u32) -> u8 {
    let t = premul + u32::from(dst) * inv;
    ((t + (t >> 8)) >> 8) as u8
}

/// Source-over one straight-alpha colour with a uniform alpha over a row.
///
/// `src * a` is loop-invariant, so it is hoisted, and the loop does **two
/// pixels per `u64`** (SWAR): the eight bytes are split into their even
/// (`B`, `R`) and odd (`G`, `X`) bytes, each widened into four 16-bit
/// lanes, and one scalar multiply-add then does four channels at once.
/// The per-pixel form this replaced did not vectorize and cost about eight
/// cycles a pixel, which made a full-screen translucent fill — the
/// overview's scrim — take ~20 ms at idle clock on the test box: longer
/// than a frame, on the path a Super tap waits for.
///
/// Bit-identical to [`mix`], lane by lane: `premul + dst * inv <=
/// 255 * (a + inv) + 128 = 65153`, and the rounding step adds at most 255,
/// so no lane ever carries into its neighbour. The `X` byte is cleared, as
/// before.
#[inline]
pub(crate) fn blend_solid(row: &mut [u8], c: Color, alpha: u8) {
    /// The low byte of every 16-bit lane.
    const LANES: u64 = 0x00ff_00ff_00ff_00ff;
    /// Everything but the two `X` bytes.
    const KEEP: u64 = 0x00ff_ffff_00ff_ffff;
    let a = u32::from(alpha);
    let inv = 255 - a;
    // Premultiplied source, plus the `+128` of the rounding step folded in.
    let pb = u32::from(c.b) * a + 128;
    let pg = u32::from(c.g) * a + 128;
    let pr = u32::from(c.r) * a + 128;
    let even = u64::from(pb) | u64::from(pr) << 16;
    let even = even | even << 32;
    let odd = u64::from(pg) | u64::from(pg) << 32;
    let inv64 = u64::from(inv);
    let round = |t: u64| ((t + ((t >> 8) & LANES)) >> 8) & LANES;
    let mut pairs = row.chunks_exact_mut(8);
    for d in &mut pairs {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(d);
        let src = u64::from_le_bytes(bytes);
        let lo = round(even + (src & LANES) * inv64);
        let hi = round(odd + ((src >> 8) & LANES) * inv64);
        d.copy_from_slice(&((lo | hi << 8) & KEEP).to_le_bytes());
    }
    for d in pairs.into_remainder().chunks_exact_mut(4) {
        let out = [
            mix(pb, d[0], inv),
            mix(pg, d[1], inv),
            mix(pr, d[2], inv),
            0,
        ];
        d.copy_from_slice(&out);
    }
}

/// Paint a row slice with full (255) coverage.
///
/// `x0` is the device column of `row[0]`. Takes the opaque store path whenever
/// the result would be opaque anyway.
pub(crate) fn paint_full(row: &mut [u8], x0: i32, paint: &RowPaint, opacity: u8) {
    match paint {
        RowPaint::Solid(c) => {
            let a = effective_alpha(c.a, 255, opacity);
            if a == 255 {
                store_solid(row, *c);
            } else if a != 0 {
                blend_solid(row, *c, a);
            }
        }
        RowPaint::Linear { .. } => {
            let opaque = paint.is_opaque() && opacity == 255;
            for (x, d) in (x0..).zip(row.chunks_exact_mut(4)) {
                let c = paint.color_at(x);
                if opaque {
                    d.copy_from_slice(&[c.b, c.g, c.r, 0]);
                } else {
                    blend_pixel(d, c, effective_alpha(c.a, 255, opacity));
                }
            }
        }
    }
}

/// Source-over one straight-alpha colour into one pixel.
#[inline]
pub(crate) fn blend_pixel(d: &mut [u8], c: Color, alpha: u8) {
    if alpha == 0 {
        return;
    }
    let a = u32::from(alpha);
    let out = [
        over_straight(u32::from(c.b), u32::from(d[0]), a),
        over_straight(u32::from(c.g), u32::from(d[1]), a),
        over_straight(u32::from(c.r), u32::from(d[2]), a),
        0,
    ];
    d.copy_from_slice(&out);
}

/// Source-over one straight-alpha colour through a run of A8 coverage.
///
/// `row` is a clipped XRGB8888 run and `cov` holds one coverage byte per
/// pixel of it (the caller guarantees `cov.len() * 4 == row.len()`); `ca` is
/// `color.a * opacity` in `0..=65_025`, hoisted out of the loop because a
/// glyph run shares both. Coverage 0 leaves the pixel alone.
#[inline]
pub(crate) fn blend_mask_row(row: &mut [u8], cov: &[u8], c: Color, ca: u32) {
    for (d, &m) in row.chunks_exact_mut(4).zip(cov) {
        blend_pixel(d, c, effective_alpha_cov(ca, m));
    }
}

/// [`blend_mask_row`] for an opaque colour at full opacity.
///
/// Full coverage then means "replace the pixel", so it is *stored* — no read
/// of (typically write-combined) buffer memory — and coverage 0 is skipped by
/// [`blend_pixel`].
///
/// The store is per pixel rather than per maximal run of 255s: detecting runs
/// needs a scan of `cov` and a `store_solid` call per run, and a glyph mask is
/// a few pixels wide with anti-aliased edges, so the runs are 1-3 px long and
/// the scan costs more than the wide store saves. Measured: the run-detecting
/// variant is 35 % slower on the bench's 50-glyph runs (0.312 vs 0.231 ms for
/// scene `f`), so this loop stayed.
#[inline]
pub(crate) fn blend_mask_row_opaque(row: &mut [u8], cov: &[u8], c: Color) {
    let px = [c.b, c.g, c.r, 0];
    for (d, &m) in row.chunks_exact_mut(4).zip(cov) {
        if m == 255 {
            d.copy_from_slice(&px);
        } else {
            blend_pixel(d, c, m);
        }
    }
}

/// Paint a row slice with per-pixel analytic coverage.
///
/// `cov` is called with the device column and returns coverage in `0..=255`.
pub(crate) fn paint_cov<F: Fn(i32) -> u8>(
    row: &mut [u8],
    x0: i32,
    paint: &RowPaint,
    opacity: u8,
    cov: F,
) {
    match paint {
        RowPaint::Solid(c) => {
            for (x, d) in (x0..).zip(row.chunks_exact_mut(4)) {
                let a = effective_alpha(c.a, cov(x), opacity);
                blend_pixel(d, *c, a);
            }
        }
        RowPaint::Linear { .. } => {
            for (x, d) in (x0..).zip(row.chunks_exact_mut(4)) {
                let c = paint.color_at(x);
                let a = effective_alpha(c.a, cov(x), opacity);
                blend_pixel(d, c, a);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RowPaint, blend_solid, lerp_color, mix};
    use nitro_core::Color;

    /// The SWAR path against the per-channel reference, exhaustively over
    /// alpha and destination byte, for a few source colours; odd row
    /// lengths exercise the one-pixel tail too.
    #[test]
    fn blend_solid_matches_mix_exactly() {
        for c in [
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
            Color::rgb(0x12, 0x80, 0xfe),
        ] {
            for alpha in 0..=255u8 {
                let a = u32::from(alpha);
                let inv = 255 - a;
                for len in [3usize, 64] {
                    let mut row: Vec<u8> = (0..len * 4).map(|i| (i * 37 + 11) as u8).collect();
                    let orig = row.clone();
                    blend_solid(&mut row, c, alpha);
                    for (px, (got, dst)) in row.chunks(4).zip(orig.chunks(4)).enumerate() {
                        let want = [
                            mix(u32::from(c.b) * a + 128, dst[0], inv),
                            mix(u32::from(c.g) * a + 128, dst[1], inv),
                            mix(u32::from(c.r) * a + 128, dst[2], inv),
                            0,
                        ];
                        assert_eq!(got, want, "alpha {alpha} px {px} dst {dst:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn lerp_endpoints_are_exact() {
        let a = Color::rgba(10, 20, 30, 40);
        let b = Color::rgba(200, 210, 220, 230);
        assert_eq!(lerp_color(a, b, 0.0), a);
        assert_eq!(lerp_color(a, b, 1.0), b);
        let mid = lerp_color(Color::rgb(0, 0, 0), Color::rgb(255, 254, 100), 0.5);
        assert_eq!(mid.r, 128);
        assert_eq!(mid.g, 127);
        assert_eq!(mid.b, 50);
    }

    #[test]
    fn gradient_row_clamps() {
        let p = RowPaint::Linear {
            t0: 0.0,
            dt: 0.5,
            c0: Color::BLACK,
            c1: Color::WHITE,
        };
        assert_eq!(p.color_at(0), Color::BLACK);
        assert_eq!(p.color_at(2), Color::WHITE);
        assert_eq!(p.color_at(9), Color::WHITE);
        assert!(p.is_opaque());
    }
}
