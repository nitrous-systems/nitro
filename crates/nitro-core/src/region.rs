//! Exact pixel regions: a y-banded set of disjoint rects.
//!
//! [`Damage`](crate::Damage) is deliberately lossy — it merges rects to stay
//! small, so it may only ever *over*-report. Some questions need the exact
//! answer instead: "which pixels may be copied rather than repainted" is
//! only sound if the set is neither larger nor smaller than it claims to
//! be. [`Region`] is that exact set.
//!
//! The representation is the classic one (pixman, X11): a list of
//! horizontal **bands**, sorted top to bottom and non-overlapping, each
//! holding a sorted list of disjoint, non-touching `[x0, x1)` spans.
//! Vertically adjacent bands with identical spans are coalesced, so the
//! representation of a given pixel set is canonical and two regions are
//! equal exactly when they hold the same pixels.
//!
//! Every boolean operation is one sweep over the union of both operands'
//! band edges. To keep the cost of a pathological input bounded, a region
//! holds at most [`Region::MAX_SPANS`] spans; an operation that would
//! exceed it returns an **overflowed** region, which carries no pixels and
//! poisons every region computed from it. A caller that needs the exact
//! answer checks [`Region::overflowed`] and falls back to whatever it would
//! have done without one.

use crate::IRect;

/// One horizontal band: rows `[y0, y1)`, covered exactly by `spans`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Band {
    y0: i32,
    y1: i32,
    /// Sorted, disjoint and non-touching `[x0, x1)` spans; never empty.
    spans: Vec<(i32, i32)>,
}

/// An exact set of pixels. See the [module docs](self).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Region {
    bands: Vec<Band>,
    overflowed: bool,
}

impl Region {
    /// The most spans (summed over bands) a region may hold before an
    /// operation gives up and returns an overflowed region.
    pub const MAX_SPANS: usize = 4096;

    /// The empty region.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bands: Vec::new(),
            overflowed: false,
        }
    }

    /// An overflowed region: no pixels, and poison for every operation.
    #[must_use]
    pub const fn overflow() -> Self {
        Self {
            bands: Vec::new(),
            overflowed: true,
        }
    }

    /// One rect (empty rects give the empty region).
    #[must_use]
    pub fn rect(r: IRect) -> Self {
        if r.is_empty() {
            return Self::new();
        }
        Self {
            bands: vec![Band {
                y0: r.y,
                y1: r.bottom(),
                spans: vec![(r.x, r.right())],
            }],
            overflowed: false,
        }
    }

    /// The union of `rects`, exactly.
    #[must_use]
    pub fn from_rects(rects: &[IRect]) -> Self {
        let rects: Vec<IRect> = rects.iter().copied().filter(|r| !r.is_empty()).collect();
        if rects.is_empty() {
            return Self::new();
        }
        let mut ys: Vec<i32> = rects.iter().flat_map(|r| [r.y, r.bottom()]).collect();
        ys.sort_unstable();
        ys.dedup();
        let mut out = Builder::default();
        let mut spans: Vec<(i32, i32)> = Vec::new();
        for w in ys.windows(2) {
            let (y0, y1) = (w[0], w[1]);
            spans.clear();
            spans.extend(
                rects
                    .iter()
                    .filter(|r| r.y <= y0 && r.bottom() >= y1)
                    .map(|r| (r.x, r.right())),
            );
            spans.sort_unstable();
            let mut merged: Vec<(i32, i32)> = Vec::with_capacity(spans.len());
            for &(a, b) in &spans {
                match merged.last_mut() {
                    Some(last) if a <= last.1 => last.1 = last.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            if !out.push(y0, y1, merged) {
                return Self::overflow();
            }
        }
        out.finish()
    }

    /// Whether an operation gave up (see [`Region::MAX_SPANS`]). An
    /// overflowed region holds no pixels and must not be trusted.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Whether no pixel is in the region. An overflowed region is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bands.is_empty()
    }

    /// Number of pixels in the region.
    #[must_use]
    pub fn area(&self) -> i64 {
        self.bands
            .iter()
            .map(|b| {
                let w: i64 = b.spans.iter().map(|&(a, c)| i64::from(c - a)).sum();
                w * i64::from(b.y1 - b.y0)
            })
            .sum()
    }

    /// Number of rects [`Region::rects`] would return.
    #[must_use]
    pub fn rect_count(&self) -> usize {
        self.bands.iter().map(|b| b.spans.len()).sum()
    }

    /// The region as disjoint rects, sorted by `y` and then by `x`: one
    /// rect per span of each band.
    #[must_use]
    pub fn rects(&self) -> Vec<IRect> {
        let mut out = Vec::with_capacity(self.rect_count());
        for b in &self.bands {
            for &(x0, x1) in &b.spans {
                out.push(IRect::from_edges(x0, b.y0, x1, b.y1));
            }
        }
        out
    }

    /// Bounding box (`EMPTY` if empty).
    #[must_use]
    pub fn bounds(&self) -> IRect {
        let (Some(first), Some(last)) = (self.bands.first(), self.bands.last()) else {
            return IRect::EMPTY;
        };
        let x0 = self.bands.iter().map(|b| b.spans[0].0).min().unwrap_or(0);
        let x1 = self
            .bands
            .iter()
            .map(|b| b.spans[b.spans.len() - 1].1)
            .max()
            .unwrap_or(0);
        IRect::from_edges(x0, first.y0, x1, last.y1)
    }

    /// Whether pixel `(x, y)` is in the region.
    #[must_use]
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.bands
            .iter()
            .find(|b| b.y0 <= y && y < b.y1)
            .is_some_and(|b| b.spans.iter().any(|&(a, c)| a <= x && x < c))
    }

    /// The region moved by `(dx, dy)`.
    #[must_use]
    pub fn translate(&self, dx: i32, dy: i32) -> Self {
        Self {
            bands: self
                .bands
                .iter()
                .map(|b| Band {
                    y0: b.y0 + dy,
                    y1: b.y1 + dy,
                    spans: b.spans.iter().map(|&(a, c)| (a + dx, c + dx)).collect(),
                })
                .collect(),
            overflowed: self.overflowed,
        }
    }

    /// Pixels in either region.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        self.op(other, |a, b| a || b)
    }

    /// Pixels in both regions.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        self.op(other, |a, b| a && b)
    }

    /// Pixels in `self` but not in `other`.
    #[must_use]
    pub fn subtract(&self, other: &Self) -> Self {
        self.op(other, |a, b| a && !b)
    }

    /// The one sweep every boolean operation is: walk the union of both
    /// operands' band edges, combine the two span lists of each slice with
    /// `keep`, and append.
    fn op(&self, other: &Self, keep: impl Fn(bool, bool) -> bool) -> Self {
        const NONE: &[(i32, i32)] = &[];
        if self.overflowed || other.overflowed {
            return Self::overflow();
        }
        let mut ys: Vec<i32> = self
            .bands
            .iter()
            .chain(&other.bands)
            .flat_map(|b| [b.y0, b.y1])
            .collect();
        ys.sort_unstable();
        ys.dedup();
        let mut out = Builder::default();
        let (mut i, mut j) = (0, 0);
        for w in ys.windows(2) {
            let (y0, y1) = (w[0], w[1]);
            while i < self.bands.len() && self.bands[i].y1 <= y0 {
                i += 1;
            }
            while j < other.bands.len() && other.bands[j].y1 <= y0 {
                j += 1;
            }
            let a = self
                .bands
                .get(i)
                .filter(|b| b.y0 <= y0)
                .map_or(NONE, |b| b.spans.as_slice());
            let b = other
                .bands
                .get(j)
                .filter(|b| b.y0 <= y0)
                .map_or(NONE, |b| b.spans.as_slice());
            let spans = combine(a, b, &keep);
            if !out.push(y0, y1, spans) {
                return Self::overflow();
            }
        }
        out.finish()
    }
}

/// Combine two sorted span lists pixel-wise with `keep`.
fn combine(
    a: &[(i32, i32)],
    b: &[(i32, i32)],
    keep: &impl Fn(bool, bool) -> bool,
) -> Vec<(i32, i32)> {
    let mut xs: Vec<i32> = a.iter().chain(b).flat_map(|&(p, q)| [p, q]).collect();
    xs.sort_unstable();
    xs.dedup();
    let mut out: Vec<(i32, i32)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    for w in xs.windows(2) {
        let (x0, x1) = (w[0], w[1]);
        while i < a.len() && a[i].1 <= x0 {
            i += 1;
        }
        while j < b.len() && b[j].1 <= x0 {
            j += 1;
        }
        let in_a = a.get(i).is_some_and(|s| s.0 <= x0);
        let in_b = b.get(j).is_some_and(|s| s.0 <= x0);
        if keep(in_a, in_b) {
            match out.last_mut() {
                Some(last) if last.1 == x0 => last.1 = x1,
                _ => out.push((x0, x1)),
            }
        }
    }
    out
}

/// Appends bands in order, dropping empty ones and coalescing vertically
/// adjacent bands with identical spans, and counts spans against the cap.
#[derive(Default)]
struct Builder {
    bands: Vec<Band>,
    spans: usize,
}

impl Builder {
    /// Returns `false` once the cap is exceeded.
    fn push(&mut self, y0: i32, y1: i32, spans: Vec<(i32, i32)>) -> bool {
        if spans.is_empty() || y1 <= y0 {
            return true;
        }
        if let Some(last) = self.bands.last_mut()
            && last.y1 == y0
            && last.spans == spans
        {
            last.y1 = y1;
            return true;
        }
        self.spans += spans.len();
        if self.spans > Region::MAX_SPANS {
            return false;
        }
        self.bands.push(Band { y0, y1, spans });
        true
    }

    fn finish(self) -> Region {
        Region {
            bands: self.bands,
            overflowed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: i32 = 24;

    /// The oracle: an `N × N` bitmap over `[-4, N-4)²`.
    fn bitmap(rects: &[IRect]) -> Vec<bool> {
        let mut m = vec![false; (N * N) as usize];
        for r in rects {
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    let (bx, by) = (x + 4, y + 4);
                    if (0..N).contains(&bx) && (0..N).contains(&by) {
                        m[(by * N + bx) as usize] = true;
                    }
                }
            }
        }
        m
    }

    fn bits(r: &Region) -> Vec<bool> {
        let rects = r.rects();
        // Disjointness: the rects' areas add up to the region's area.
        let sum: i64 = rects.iter().map(IRect::area).sum();
        assert_eq!(sum, r.area());
        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                assert!(!a.intersects(b), "{a:?} overlaps {b:?}");
            }
        }
        bitmap(&rects)
    }

    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self, m: i32) -> i32 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
            i32::try_from((self.0 >> 16) % m.cast_unsigned()).unwrap()
        }
        fn rects(&mut self) -> Vec<IRect> {
            let n = self.next(6);
            (0..n)
                .map(|_| {
                    IRect::new(
                        self.next(N) - 4,
                        self.next(N) - 4,
                        self.next(10),
                        self.next(10),
                    )
                })
                .collect()
        }
    }

    #[test]
    fn every_operation_matches_a_bitmap_oracle() {
        let mut rng = Lcg(7);
        for _ in 0..3000 {
            let (ra, rb) = (rng.rects(), rng.rects());
            let (a, b) = (Region::from_rects(&ra), Region::from_rects(&rb));
            let (ma, mb) = (bitmap(&ra), bitmap(&rb));
            assert_eq!(bits(&a), ma);
            let zip = |f: fn(bool, bool) -> bool| -> Vec<bool> {
                ma.iter().zip(&mb).map(|(&x, &y)| f(x, y)).collect()
            };
            assert_eq!(bits(&a.union(&b)), zip(|x, y| x || y));
            assert_eq!(bits(&a.intersect(&b)), zip(|x, y| x && y));
            assert_eq!(bits(&a.subtract(&b)), zip(|x, y| x && !y));
            // Canonical: the same pixels are the same region.
            assert_eq!(a.union(&b), b.union(&a));
            assert_eq!(a.union(&a), a);
            let (dx, dy) = (rng.next(5) - 2, rng.next(5) - 2);
            let moved: Vec<IRect> = ra.iter().map(|r| r.translate(dx, dy)).collect();
            assert_eq!(a.translate(dx, dy), Region::from_rects(&moved));
            for y in -4..N - 4 {
                for x in -4..N - 4 {
                    assert_eq!(a.contains(x, y), ma[((y + 4) * N + x + 4) as usize]);
                }
            }
        }
    }

    #[test]
    fn rects_are_sorted_and_bands_coalesce() {
        let r = Region::from_rects(&[IRect::new(0, 0, 10, 5), IRect::new(0, 5, 10, 5)]);
        assert_eq!(r.rects(), [IRect::new(0, 0, 10, 10)]);
        let r = Region::from_rects(&[IRect::new(20, 0, 5, 5), IRect::new(0, 0, 5, 5)]);
        assert_eq!(r.rects(), [IRect::new(0, 0, 5, 5), IRect::new(20, 0, 5, 5)]);
        assert_eq!(r.bounds(), IRect::new(0, 0, 25, 5));
        assert!(Region::rect(IRect::EMPTY).is_empty());
    }

    #[test]
    fn a_region_past_the_cap_overflows_and_poisons() {
        let many: Vec<IRect> = (0..=i32::try_from(Region::MAX_SPANS).unwrap())
            .map(|i| IRect::new(i * 2, 0, 1, 1))
            .collect();
        let r = Region::from_rects(&many);
        assert!(r.overflowed());
        assert!(r.is_empty());
        let ok = Region::rect(IRect::new(0, 0, 4, 4));
        assert!(ok.union(&r).overflowed());
        assert!(r.subtract(&ok).overflowed());
        assert!(r.translate(1, 1).overflowed());
    }
}
