//! 8-bit source-over compositing.
//!
//! Everything here works on sRGB byte values *as if they were linear* — see
//! the crate docs for why that is a deliberate M1 simplification.
//!
//! The one division that matters is by 255, and it is exact:
//!
//! ```text
//! div255(x) = (t + (t >> 8)) >> 8   where t = x + 128
//! ```
//!
//! which equals `round(x / 255)` for every `x <= 65_535`. Every numerator we
//! produce is a convex combination scaled by 255 — `src * a + dst * (255 - a)`
//! with all terms `<= 255` — so the maximum is `255 * 255 = 65_025`, safely
//! inside the range. The unit test checks the whole range exhaustively.

/// `round(x / 255)`, exact for every `x <= 65_535`.
///
/// Callers must keep numerators at or below `255 * 255`; see the module docs.
#[inline]
pub(crate) const fn div255(x: u32) -> u32 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

/// Source-over for one channel with a **straight-alpha** source.
///
/// `round((src * alpha + dst * (255 - alpha)) / 255)` — one rounding step for
/// the whole expression, so the result is within 0.5 of the float reference
/// (and therefore within ±1 of a float implementation that rounds at the end).
#[inline]
pub(crate) const fn over_straight(src: u32, dst: u32, alpha: u32) -> u8 {
    div255(src * alpha + dst * (255 - alpha)) as u8
}

/// Source-over for one channel with an **already premultiplied** source.
///
/// `src_premul + round(dst * (255 - alpha) / 255)`, saturated at 255 so a
/// malformed premultiplied source cannot wrap.
#[inline]
pub(crate) const fn over_premul(src_premul: u32, dst: u32, alpha: u32) -> u8 {
    let v = src_premul + div255(dst * (255 - alpha));
    if v > 255 { 255 } else { v as u8 }
}

/// Fold a source alpha, an analytic coverage and a global opacity into the
/// single 0..=255 alpha the blend uses: `round(a * cov * opacity / 255^2)`.
#[inline]
pub(crate) fn effective_alpha(a: u8, cov: u8, opacity: u8) -> u8 {
    let v = u32::from(a) * u32::from(cov) * u32::from(opacity);
    ((v + 32_512) / 65_025) as u8
}

/// Quantise a value in `[0, 1]` (coverage or opacity) to `0..=255`.
#[inline]
pub(crate) fn unit_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::{div255, effective_alpha, over_premul, over_straight, unit_u8};

    #[test]
    fn div255_is_exact_rounding() {
        for x in 0..=65_535u32 {
            let want = (f64::from(x) / 255.0).round() as u32;
            assert_eq!(div255(x), want, "x = {x}");
        }
    }

    #[test]
    fn over_matches_float_reference() {
        for src in [0u32, 1, 17, 128, 200, 254, 255] {
            for dst in [0u32, 1, 17, 128, 200, 254, 255] {
                for alpha in [0u32, 1, 64, 128, 200, 255] {
                    let (sf, df, af) = (f64::from(src), f64::from(dst), f64::from(alpha));
                    let want = (sf * af + df * (255.0 - af)) / 255.0;
                    let got = f64::from(over_straight(src, dst, alpha));
                    assert!((got - want).abs() <= 0.5 + 1e-9, "{src} {dst} {alpha}");
                }
            }
        }
    }

    #[test]
    fn premul_and_straight_agree() {
        for c in [0u32, 3, 90, 255] {
            for a in [0u32, 5, 128, 255] {
                for d in [0u32, 7, 199, 255] {
                    let s = div255(c * a);
                    let p = i32::from(over_premul(s, d, a));
                    let t = i32::from(over_straight(c, d, a));
                    assert!(
                        (p - t).abs() <= 1,
                        "c={c} a={a} d={d} premul={p} straight={t}"
                    );
                }
            }
        }
    }

    #[test]
    fn alpha_and_unit_endpoints() {
        assert_eq!(effective_alpha(255, 255, 255), 255);
        assert_eq!(effective_alpha(255, 0, 255), 0);
        assert_eq!(effective_alpha(0, 255, 255), 0);
        assert_eq!(effective_alpha(128, 255, 255), 128);
        assert_eq!(effective_alpha(255, 128, 255), 128);
        assert_eq!(unit_u8(0.0), 0);
        assert_eq!(unit_u8(1.0), 255);
        assert_eq!(unit_u8(0.5), 128);
        assert_eq!(unit_u8(-3.0), 0);
        assert_eq!(unit_u8(3.0), 255);
    }
}
