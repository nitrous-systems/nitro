//! Row-level painting — the innermost loops.
//!
//! Everything here works on one contiguous slice of a row that has already
//! been clipped. The destination is premultiplied ARGB8888: byte 3 is alpha
//! and is composited like a colour channel whose source value is 255, so it
//! stays 255 over an opaque destination and a hole (all zero) becomes a
//! correct premultiplied translucent pixel. The rest of the row is so there is no per-pixel bounds check and the loops
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
    let px = u32::from_le_bytes([c.b, c.g, c.r, 255]);
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
/// (`B`, `R`) and odd (`G`, `A`) bytes, each widened into four 16-bit
/// lanes, and one scalar multiply-add then does four channels at once.
/// The per-pixel form this replaced did not vectorize and cost about eight
/// cycles a pixel, which made a full-screen translucent fill — the
/// overview's scrim — take ~20 ms at idle clock on the test box: longer
/// than a frame, on the path a Super tap waits for.
///
/// Bit-identical to [`mix`], lane by lane: `premul + dst * inv <=
/// 255 * (a + inv) + 128 = 65153`, and the rounding step adds at most 255,
/// so no lane ever carries into its neighbour. The `A` byte is composited
/// with a source value of 255 (`255 * a + d_a * inv`), which is 255 again
/// over an opaque destination.
#[inline]
pub(crate) fn blend_solid(row: &mut [u8], c: Color, alpha: u8) {
    /// The low byte of every 16-bit lane.
    const LANES: u64 = 0x00ff_00ff_00ff_00ff;
    let a = u32::from(alpha);
    let inv = 255 - a;
    // Premultiplied source, plus the `+128` of the rounding step folded in.
    let pb = u32::from(c.b) * a + 128;
    let pg = u32::from(c.g) * a + 128;
    let pr = u32::from(c.r) * a + 128;
    let even = u64::from(pb) | u64::from(pr) << 16;
    let even = even | even << 32;
    let pa = 255 * a + 128;
    let odd = u64::from(pg) | u64::from(pa) << 16;
    let odd = odd | odd << 32;
    let inv64 = u64::from(inv);
    let round = |t: u64| ((t + ((t >> 8) & LANES)) >> 8) & LANES;
    let mut pairs = row.chunks_exact_mut(8);
    for d in &mut pairs {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(d);
        let src = u64::from_le_bytes(bytes);
        let lo = round(even + (src & LANES) * inv64);
        let hi = round(odd + ((src >> 8) & LANES) * inv64);
        d.copy_from_slice(&(lo | hi << 8).to_le_bytes());
    }
    for d in pairs.into_remainder().chunks_exact_mut(4) {
        let out = [
            mix(pb, d[0], inv),
            mix(pg, d[1], inv),
            mix(pr, d[2], inv),
            mix(pa, d[3], inv),
        ];
        d.copy_from_slice(&out);
    }
}

/// Source-over a row of **straight-alpha** source pixels onto `drow`, one
/// alpha per pixel (the non-opaque 1:1 image blit, #3877).
///
/// `src_opaque`: the source is `XRGB8888`, so every pixel's alpha is just
/// `opacity`. Otherwise the alpha is `effective_alpha(s[3], 255, opacity)`.
///
/// Two pixels per `u64`, as [`blend_solid`] does, with two fast paths that
/// dominate real client content (Chromium's AR24 window is opaque apart from
/// its rounded corners and shadow):
///
/// - both alphas 255: the opaque copy with alpha forced (`s | A_MASK`);
/// - both alphas 0: nothing to do.
///
/// Otherwise the even (`B`, `R`) and odd (`G`, `A`) bytes are widened into
/// 16-bit lanes. The two pixels' alphas differ, so a scalar multiply cannot
/// cover all four lanes at once; masking the lanes of each pixel apart and
/// multiplying each half by its own alpha can, because every lane product is
/// `<= 255 * 255` and so stays inside its 16 bits. `s*a + d*(255-a) + 128`
/// is at most 65153 and the rounding step adds at most 255, so no lane
/// carries: the result is bit-identical to `over_straight` per channel.
///
/// The source's alpha *channel* is taken as 255 (`sv | A_MASK`) before
/// widening, so the destination alpha is composited exactly as
/// [`blend_pixel`] does it: `255 * a + d_a * (255 - a)`. A zero-alpha pixel
/// paired with a visible one is therefore rewritten unchanged.
pub(crate) fn blend_straight_row(drow: &mut [u8], srow: &[u8], src_opaque: bool, opacity: u8) {
    /// The low byte of every 16-bit lane.
    const LANES: u64 = 0x00ff_00ff_00ff_00ff;
    /// The lanes of the first (low) pixel, and of the second.
    const LO: u64 = 0x0000_0000_00ff_00ff;
    const HI: u64 = 0x00ff_00ff_0000_0000;
    /// The `+128` of the rounding step, per lane.
    const HALF: u64 = 0x0080_0080_0080_0080;
    /// Byte 3 of both pixels.
    const A_MASK: u64 = 0xff00_0000_ff00_0000;
    let alpha = |sa: u64| -> u64 {
        if src_opaque {
            u64::from(opacity)
        } else if opacity == 255 {
            sa & 0xff
        } else {
            u64::from(effective_alpha((sa & 0xff) as u8, 255, opacity))
        }
    };
    let round = |t: u64| ((t + ((t >> 8) & LANES)) >> 8) & LANES;
    let len = drow.len().min(srow.len()) & !3;
    let pairs = len & !7;
    let (dhead, dtail) = drow[..len].split_at_mut(pairs);
    let (shead, stail) = srow[..len].split_at(pairs);
    for (d, s) in dhead.chunks_exact_mut(8).zip(shead.chunks_exact(8)) {
        let sv = u64::from_le_bytes(s.try_into().unwrap_or([0; 8]));
        let a0 = alpha(sv >> 24);
        let a1 = alpha(sv >> 56);
        let sw = sv | A_MASK;
        if a0 & a1 == 255 {
            d.copy_from_slice(&sw.to_le_bytes());
            continue;
        }
        if a0 | a1 == 0 {
            continue;
        }
        let dv = u64::from_le_bytes((&*d).try_into().unwrap_or([0; 8]));
        let (i0, i1) = (255 - a0, 255 - a1);
        let lanes = |v: u64, a0: u64, a1: u64| (v & LO) * a0 + (v & HI) * a1;
        let even = lanes(sw, a0, a1) + lanes(dv, i0, i1) + HALF;
        let odd = lanes(sw >> 8, a0, a1) + lanes(dv >> 8, i0, i1) + HALF;
        d.copy_from_slice(&(round(even) | round(odd) << 8).to_le_bytes());
    }
    // `len` is a multiple of 4, so the tail is one pixel at most.
    for (d, s) in dtail.chunks_exact_mut(4).zip(stail.chunks_exact(4)) {
        let a = alpha(u64::from(s[3])) as u32;
        if a == 0 {
            continue;
        }
        let out = [
            over_straight(u32::from(s[0]), u32::from(d[0]), a),
            over_straight(u32::from(s[1]), u32::from(d[1]), a),
            over_straight(u32::from(s[2]), u32::from(d[2]), a),
            over_straight(255, u32::from(d[3]), a),
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
                    d.copy_from_slice(&[c.b, c.g, c.r, 255]);
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
        over_straight(255, u32::from(d[3]), a),
    ];
    d.copy_from_slice(&out);
}

/// Source-over one straight-alpha colour through a run of A8 coverage.
///
/// `row` is a clipped ARGB8888 run and `cov` holds one coverage byte per
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
    let px = [c.b, c.g, c.r, 255];
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
    use super::{RowPaint, blend_solid, blend_straight_row, lerp_color, mix};
    use crate::blend::{effective_alpha, over_straight};

    /// The per-pixel loop `blend_straight_row` replaced, kept as the
    /// reference.
    fn straight_ref(drow: &mut [u8], srow: &[u8], src_opaque: bool, opacity: u8) {
        for (d, s) in drow.chunks_exact_mut(4).zip(srow.chunks_exact(4)) {
            let sa = if src_opaque { 255 } else { s[3] };
            let a = effective_alpha(sa, 255, opacity);
            if a == 0 {
                continue;
            }
            let au = u32::from(a);
            let out = [
                over_straight(u32::from(s[0]), u32::from(d[0]), au),
                over_straight(u32::from(s[1]), u32::from(d[1]), au),
                over_straight(u32::from(s[2]), u32::from(d[2]), au),
                over_straight(255, u32::from(d[3]), au),
            ];
            d.copy_from_slice(&out);
        }
    }

    /// Exhaustive over (alpha of pixel 0, alpha of pixel 1) for several
    /// colour patterns and opacities, both source formats, and row lengths
    /// that exercise the one-pixel tail. Destination alpha is opaque, a
    /// hole, or a sweep of partial values.
    #[test]
    fn blend_straight_row_matches_reference_exactly() {
        let pats: [fn(usize) -> u8; 3] = [|i| (i * 37 + 11) as u8, |_| 0, |_| 255];
        for opacity in [255u8, 128, 1] {
            for src_opaque in [false, true] {
                for (pi, sp) in pats.iter().enumerate() {
                    let dp = pats[(pi + 1) % pats.len()];
                    for a0 in 0..=255u8 {
                        // All of a1 for the common opacity; a sample otherwise.
                        let step = if opacity == 255 && !src_opaque { 1 } else { 17 };
                        for a1 in (0..=255u8).step_by(step) {
                            for len in [1usize, 2, 3] {
                                let mut src: Vec<u8> = (0..len * 4).map(sp).collect();
                                for (k, px) in src.chunks_mut(4).enumerate() {
                                    px[3] = if k % 2 == 0 { a0 } else { a1 };
                                }
                                let mut dst: Vec<u8> = (0..len * 4).map(|i| dp(i + 5)).collect();
                                for (k, px) in dst.chunks_mut(4).enumerate() {
                                    px[3] = match pi {
                                        0 => 255,
                                        1 => 0,
                                        _ => (k * 97 + usize::from(a0)) as u8,
                                    };
                                }
                                let mut want = dst.clone();
                                straight_ref(&mut want, &src, src_opaque, opacity);
                                blend_straight_row(&mut dst, &src, src_opaque, opacity);
                                assert_eq!(
                                    dst, want,
                                    "a0 {a0} a1 {a1} len {len} opacity {opacity} opaque {src_opaque}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Mixed long rows — runs of 255, runs of 0, partials, odd length.
    #[test]
    fn blend_straight_row_mixed_runs() {
        for len in [63usize, 64, 1001] {
            let src: Vec<u8> = (0..len * 4)
                .map(|i| {
                    if i % 4 == 3 {
                        match (i / 4) % 11 {
                            0..=4 => 255,
                            5 | 6 => 0,
                            k => (k * 40) as u8,
                        }
                    } else {
                        (i * 13 + 7) as u8
                    }
                })
                .collect();
            let base: Vec<u8> = (0..len * 4)
                .map(|i| if i % 4 == 3 { 255 } else { (i * 29) as u8 })
                .collect();
            let mut want = base.clone();
            straight_ref(&mut want, &src, false, 255);
            let mut got = base.clone();
            blend_straight_row(&mut got, &src, false, 255);
            assert_eq!(got, want, "len {len}");
            assert!(got.chunks(4).all(|p| p[3] == 255));
        }
    }
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
                            mix(255 * a + 128, dst[3], inv),
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
