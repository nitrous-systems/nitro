//! Analytic coverage for axis-aligned rounded rectangles.
//!
//! One device pixel row of a shape is reduced to a small set of *sub-scanline
//! spans*: horizontal intervals `[l, r]` each carrying the same vertical
//! weight. The coverage of a pixel is then
//!
//! ```text
//! cov(px) = w * Σ_i clamp(min(r_i, px + 1) - max(l_i, px), 0, 1)
//! ```
//!
//! which is the **exact** area of the pixel square inside the shape whenever
//! the shape's horizontal extent is constant over the row — i.e. for every
//! straight edge, which is the overwhelming majority of what a compositor
//! paints. Only rows that cross a corner arc need more than one sub-scanline,
//! and there the x-extent is still integrated exactly; only y is sampled
//! ([`SUB`] times), so the approximation error is confined to the `r × r`
//! corner boxes.

use nitro_core::Rect;

/// Maximum sub-scanlines used for a row that crosses a corner arc.
const SUB: usize = 8;

/// Radius above which a corner row uses all [`SUB`] sub-scanlines.
///
/// The sampling error of `n` sub-scanlines grows with the radius (a bigger
/// arc sweeps more x per row), so a small radius needs fewer. Measured
/// against a 256x-supersampled reference, 4 sub-scanlines stay within
/// ~7/255 up to r = 10 while costing half the work of 8; above that the
/// error would reach ~15/255, so the full 8 are used. Both are in the same
/// accuracy class as the distance-field approximation this replaces.
const SUB_FULL_RADIUS: f32 = 10.0;

/// The sub-scanline decomposition of one pixel row of a shape.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RowSpans {
    /// Left edge of each sub-scanline span.
    l: [f32; SUB],
    /// Right edge of each sub-scanline span.
    r: [f32; SUB],
    /// Number of valid sub-scanlines (0 = the row is empty).
    n: u8,
    /// Vertical weight of each sub-scanline; `n * w` is the row's height
    /// inside the shape.
    w: f32,
}

impl RowSpans {
    /// A row that the shape does not touch.
    pub(crate) const EMPTY: Self = Self {
        l: [0.0; SUB],
        r: [0.0; SUB],
        n: 0,
        w: 0.0,
    };

    /// Whether the row contributes nothing.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.n == 0 || self.w <= 0.0
    }

    /// Exact area of pixel `[px, px+1] × row` inside the shape, in `0..=1`.
    ///
    /// The `n == 1` case — every row of a sharp rect, and every non-corner
    /// row of a rounded one — is split out so it compiles to straight-line
    /// code instead of a loop with a dynamic trip count.
    #[inline]
    pub(crate) fn cov(&self, px: i32) -> f32 {
        let a = px as f32;
        let b = a + 1.0;
        if self.n == 1 {
            return (self.r[0].min(b) - self.l[0].max(a)).clamp(0.0, 1.0) * self.w;
        }
        let mut acc = 0.0;
        for i in 0..self.n as usize {
            let hit = self.r[i].min(b) - self.l[i].max(a);
            acc += hit.clamp(0.0, 1.0);
        }
        acc * self.w
    }

    /// Leftmost device column the row can touch.
    #[inline]
    pub(crate) fn first_px(&self) -> i32 {
        let mut m = f32::INFINITY;
        for i in 0..self.n as usize {
            m = m.min(self.l[i]);
        }
        m.floor() as i32
    }

    /// One past the rightmost device column the row can touch.
    #[inline]
    pub(crate) fn last_px(&self) -> i32 {
        let mut m = f32::NEG_INFINITY;
        for i in 0..self.n as usize {
            m = m.max(self.r[i]);
        }
        m.ceil() as i32
    }

    /// First device column that every sub-scanline covers completely.
    #[inline]
    pub(crate) fn full_start(&self) -> i32 {
        let mut m = f32::NEG_INFINITY;
        for i in 0..self.n as usize {
            m = m.max(self.l[i]);
        }
        m.ceil() as i32
    }

    /// One past the last device column that every sub-scanline covers
    /// completely.
    #[inline]
    pub(crate) fn full_end(&self) -> i32 {
        let mut m = f32::INFINITY;
        for i in 0..self.n as usize {
            m = m.min(self.r[i]);
        }
        m.floor() as i32
    }

    /// Whether the row is vertically covered from top to bottom, so a pixel in
    /// `[full_start, full_end)` has coverage exactly 1.
    #[inline]
    pub(crate) fn is_full_height(&self) -> bool {
        f32::from(self.n) * self.w >= 1.0
    }
}

/// An axis-aligned rounded rectangle in device coordinates.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RRect {
    /// Left edge.
    pub(crate) x0: f32,
    /// Top edge.
    pub(crate) y0: f32,
    /// Right edge.
    pub(crate) x1: f32,
    /// Bottom edge.
    pub(crate) y1: f32,
    /// Corner radius, clamped to `min(w, h) / 2`; 0 means a sharp rect.
    pub(crate) r: f32,
}

impl RRect {
    /// Build from a rect and a requested corner radius.
    ///
    /// The radius is clamped to half the shorter side and to `>= 0`;
    /// non-finite input yields an empty shape.
    pub(crate) fn new(rect: &Rect, radius: f32) -> Self {
        let w = rect.w.max(0.0);
        let h = rect.h.max(0.0);
        let r = if radius.is_finite() {
            radius.max(0.0).min(w * 0.5).min(h * 0.5)
        } else {
            0.0
        };
        Self {
            x0: rect.x,
            y0: rect.y,
            x1: rect.x + w,
            y1: rect.y + h,
            r,
        }
    }

    /// Whether the shape covers no area.
    pub(crate) fn is_empty(&self) -> bool {
        !(self.x1 > self.x0 && self.y1 > self.y0)
    }

    /// Device rows the shape can touch.
    pub(crate) fn row_range(&self) -> (i32, i32) {
        (self.y0.floor() as i32, self.y1.ceil() as i32)
    }

    /// Device columns the shape can touch.
    pub(crate) fn col_range(&self) -> (i32, i32) {
        (self.x0.floor() as i32, self.x1.ceil() as i32)
    }

    /// Horizontal inset of the shape at height `y` (0 away from the corners).
    #[inline]
    fn inset_at(&self, y: f32) -> f32 {
        if self.r <= 0.0 {
            return 0.0;
        }
        let dy = if y < self.y0 + self.r {
            self.y0 + self.r - y
        } else if y > self.y1 - self.r {
            y - (self.y1 - self.r)
        } else {
            return 0.0;
        };
        let k = self.r * self.r - dy * dy;
        self.r - if k > 0.0 { k.sqrt() } else { 0.0 }
    }

    /// Whether pixel row `py` crosses one of the corner arcs.
    #[inline]
    fn is_corner_row(&self, py: i32) -> bool {
        if self.r <= 0.0 {
            return false;
        }
        let top = py as f32;
        let bot = top + 1.0;
        top < self.y0 + self.r || bot > self.y1 - self.r
    }

    /// Decompose pixel row `py` into sub-scanline spans.
    pub(crate) fn row_spans(&self, py: i32) -> RowSpans {
        if self.is_empty() {
            return RowSpans::EMPTY;
        }
        let ylo = self.y0.max(py as f32);
        let yhi = self.y1.min(py as f32 + 1.0);
        let h = yhi - ylo;
        if h <= 0.0 {
            return RowSpans::EMPTY;
        }
        let mut s = RowSpans::EMPTY;
        if self.is_corner_row(py) {
            let sub = if self.r > SUB_FULL_RADIUS {
                SUB
            } else {
                SUB / 2
            };
            s.n = sub as u8;
            s.w = h / sub as f32;
            let step = h / sub as f32;
            for i in 0..sub {
                let y = ylo + (i as f32 + 0.5) * step;
                let inset = self.inset_at(y);
                s.l[i] = self.x0 + inset;
                s.r[i] = self.x1 - inset;
            }
        } else {
            s.n = 1;
            s.w = h;
            s.l[0] = self.x0;
            s.r[0] = self.x1;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::RRect;
    use nitro_core::Rect;

    #[test]
    fn sharp_rect_edge_is_exact() {
        let rr = RRect::new(&Rect::new(0.5, 0.0, 10.0, 4.0), 0.0);
        let s = rr.row_spans(1);
        assert!(s.is_full_height());
        assert!((s.cov(0) - 0.5).abs() < 1e-6);
        assert!((s.cov(1) - 1.0).abs() < 1e-6);
        assert!((s.cov(10) - 0.5).abs() < 1e-6);
        assert!((s.cov(11) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn partial_row_height_scales_coverage() {
        let rr = RRect::new(&Rect::new(0.0, 0.25, 4.0, 0.5), 0.0);
        let s = rr.row_spans(0);
        assert!(!s.is_full_height());
        assert!((s.cov(1) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn rounded_corner_is_empty_at_the_tip_and_full_inside() {
        let rr = RRect::new(&Rect::new(0.0, 0.0, 40.0, 20.0), 6.0);
        // The extreme corner pixel is outside the arc.
        assert!(rr.row_spans(0).cov(0) < 0.02);
        // Deep inside the inscribed area everything is covered.
        assert!((rr.row_spans(10).cov(20) - 1.0).abs() < 1e-6);
        assert!((rr.row_spans(0).cov(20) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn corners_are_symmetric() {
        let rr = RRect::new(&Rect::new(0.0, 0.0, 40.0, 20.0), 6.0);
        for i in 0..6 {
            let top = rr.row_spans(i);
            let bot = rr.row_spans(19 - i);
            for x in 0..6 {
                let a = top.cov(x);
                let b = top.cov(39 - x);
                let c = bot.cov(x);
                let d = bot.cov(39 - x);
                assert!((a - b).abs() < 1e-5, "h mirror {i} {x}: {a} {b}");
                assert!((a - c).abs() < 1e-5, "v mirror {i} {x}: {a} {c}");
                assert!((a - d).abs() < 1e-5, "d mirror {i} {x}: {a} {d}");
            }
        }
    }

    #[test]
    fn quarter_disc_area_is_close_to_analytic() {
        // Summed coverage over a corner box must approach r^2 - pi r^2 / 4
        // missing (i.e. the box minus the quarter disc is cut away).
        let r = 8.0_f32;
        let rr = RRect::new(&Rect::new(0.0, 0.0, 64.0, 64.0), r);
        let mut sum = 0.0;
        for y in 0..8 {
            let s = rr.row_spans(y);
            for x in 0..8 {
                sum += s.cov(x);
            }
        }
        let want = std::f32::consts::PI * r * r / 4.0;
        assert!((sum - want).abs() < 0.2, "sum {sum} want {want}");
    }

    #[test]
    fn corner_coverage_is_close_to_a_supersampled_reference() {
        // The sub-scanline count adapts to the radius; check both branches
        // against a 256x-supersampled reference over the whole corner box.
        for r in [3.0_f32, 6.0, 10.0, 16.0, 24.0] {
            let rr = RRect::new(&Rect::new(0.0, 0.0, 96.0, 96.0), r);
            let n = r.ceil() as i32 + 1;
            let mut worst = 0.0_f32;
            for py in 0..n {
                let spans = rr.row_spans(py);
                for px in 0..n {
                    // Reference: 256 sub-scanlines, x integrated exactly.
                    let mut acc = 0.0;
                    for i in 0..256 {
                        let y = py as f32 + (i as f32 + 0.5) / 256.0;
                        let dy = (r - y).max(0.0);
                        let k = r * r - dy * dy;
                        let inset = r - if k > 0.0 { k.sqrt() } else { 0.0 };
                        let hit = 96.0_f32.min(px as f32 + 1.0) - inset.max(px as f32);
                        acc += hit.clamp(0.0, 1.0);
                    }
                    let want = acc / 256.0;
                    worst = worst.max((spans.cov(px) - want).abs());
                }
            }
            assert!(
                worst < 0.05,
                "r = {r}: worst coverage error {worst} ({}/255)",
                worst * 255.0
            );
        }
    }

    #[test]
    fn radius_is_clamped_to_half_the_short_side() {
        let rr = RRect::new(&Rect::new(0.0, 0.0, 10.0, 4.0), 50.0);
        assert!((rr.r - 2.0).abs() < 1e-6);
        let rr = RRect::new(&Rect::new(0.0, 0.0, 10.0, 4.0), -3.0);
        assert!((rr.r - 0.0).abs() < 1e-6);
        let rr = RRect::new(&Rect::new(0.0, 0.0, 10.0, 4.0), f32::NAN);
        assert!((rr.r - 0.0).abs() < 1e-6);
    }
}
