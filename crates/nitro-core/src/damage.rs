//! Damage regions: a bounded list of integer rects.

use crate::IRect;

/// Accumulated damage as a small list of non-empty pixel rects.
///
/// Rects are merged when one contains the other or when the merged bounding
/// box would waste little area; once more than [`Damage::MAX_RECTS`] would be
/// needed, the pair whose merge wastes the least area is merged (repeatedly)
/// until the list fits again, so nearby rects pair up rather than the whole
/// region collapsing to one bounding box. This keeps the
/// list cheap to produce, cheap to hand to `FB_DAMAGE_CLIPS`, and cheap to
/// send over a wire, at the cost of occasionally repainting a few extra
/// pixels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    rects: Vec<IRect>,
}

impl Damage {
    /// Maximum number of rects kept; beyond it the least-wasteful pairs merge.
    pub const MAX_RECTS: usize = 16;

    /// Empty damage.
    #[must_use]
    pub const fn new() -> Self {
        Self { rects: Vec::new() }
    }

    /// Whether nothing is damaged.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// The rects. Never empty rects; never more than `MAX_RECTS`.
    #[must_use]
    pub fn rects(&self) -> &[IRect] {
        &self.rects
    }

    /// Bounding box of everything damaged (`EMPTY` if nothing).
    #[must_use]
    pub fn bounds(&self) -> IRect {
        self.rects.iter().fold(IRect::EMPTY, |acc, r| acc.union(r))
    }

    /// Forget all damage (keeps the allocation).
    pub fn clear(&mut self) {
        self.rects.clear();
    }

    /// Add a rect.
    pub fn add(&mut self, r: IRect) {
        if r.is_empty() {
            return;
        }
        // Absorb / be absorbed.
        for existing in &mut self.rects {
            if existing.contains_rect(&r) {
                return;
            }
            if r.contains_rect(existing) {
                *existing = r;
                self.coalesce();
                return;
            }
        }
        // Merge with a rect whose union wastes little area.
        for existing in &mut self.rects {
            let u = existing.union(&r);
            if u.area() <= (existing.area() + r.area()) * 5 / 4 {
                *existing = u;
                self.coalesce();
                return;
            }
        }
        self.rects.push(r);
        if self.rects.len() > Self::MAX_RECTS {
            self.reduce();
        }
    }

    /// Add every rect of another region.
    pub fn add_all(&mut self, other: &Self) {
        for r in &other.rects {
            self.add(*r);
        }
    }

    /// Clip every rect to `clip`, dropping what falls outside.
    pub fn clip(&mut self, clip: &IRect) {
        self.rects.retain_mut(|r| {
            *r = r.intersect(clip);
            !r.is_empty()
        });
    }

    /// Whether `r` overlaps any damaged rect.
    #[must_use]
    pub fn intersects(&self, r: &IRect) -> bool {
        self.rects.iter().any(|d| d.intersects(r))
    }

    /// Take the rects out, leaving the region empty.
    #[must_use]
    pub fn take(&mut self) -> Vec<IRect> {
        std::mem::take(&mut self.rects)
    }

    /// Merge the pair whose union wastes the least area (ties: smaller
    /// union) until at most `MAX_RECTS` rects remain. O(n²) per merge, and
    /// it only runs on overflow, where n is `MAX_RECTS + 1`.
    fn reduce(&mut self) {
        while self.rects.len() > Self::MAX_RECTS {
            let mut best: Option<(i64, i64, usize, usize)> = None;
            for i in 0..self.rects.len() {
                for j in (i + 1)..self.rects.len() {
                    let (a, b) = (self.rects[i], self.rects[j]);
                    let u = a.union(&b);
                    let key = (u.area() - a.area() - b.area(), u.area());
                    if best.is_none_or(|(w, ua, _, _)| key < (w, ua)) {
                        best = Some((key.0, key.1, i, j));
                    }
                }
            }
            let Some((_, _, i, j)) = best else { return };
            self.rects[i] = self.rects[i].union(&self.rects[j]);
            self.rects.swap_remove(j);
            self.coalesce();
        }
    }

    /// After a merge, remove rects now contained in another.
    fn coalesce(&mut self) {
        let mut i = 0;
        while i < self.rects.len() {
            let ri = self.rects[i];
            let contained = self
                .rects
                .iter()
                .enumerate()
                .any(|(j, rj)| j != i && rj.contains_rect(&ri));
            if contained {
                self.rects.swap_remove(i);
            } else {
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_contained_rects_are_absorbed() {
        let mut d = Damage::new();
        d.add(IRect::EMPTY);
        assert!(d.is_empty());
        d.add(IRect::new(0, 0, 100, 100));
        d.add(IRect::new(10, 10, 5, 5));
        assert_eq!(d.rects(), &[IRect::new(0, 0, 100, 100)]);
        d.add(IRect::new(-10, -10, 200, 200));
        assert_eq!(d.rects(), &[IRect::new(-10, -10, 200, 200)]);
    }

    #[test]
    fn disjoint_rects_stay_separate_until_cap() {
        let mut d = Damage::new();
        for i in 0..i32::try_from(Damage::MAX_RECTS).unwrap() {
            d.add(IRect::new(i * 1000, 0, 10, 10));
        }
        assert_eq!(d.rects().len(), Damage::MAX_RECTS);
        d.add(IRect::new(-5000, 0, 10, 10));
        // One adjacent pair merges; the far-away rect stays separate.
        assert_eq!(d.rects().len(), Damage::MAX_RECTS);
        assert_eq!(d.bounds(), IRect::from_edges(-5000, 0, 15010, 10));
        assert!(d.rects().contains(&IRect::new(-5000, 0, 10, 10)));
        assert_eq!(d.rects().iter().filter(|r| r.w == 1010).count(), 1);
        let total: i64 = d.rects().iter().map(IRect::area).sum();
        assert_eq!(total, 15 * 100 + 1010 * 10);
    }

    #[test]
    fn overflow_merges_nearest_pairs_not_everything() {
        // Overview badge layout: 16 thumbnails in 2 rows of 8, each with an
        // icon and a caption pill just beneath it (gap 4, vs ~100 between
        // thumbnails).
        let mut d = Damage::new();
        let mut input = Vec::new();
        for row in 0..2 {
            for col in 0..8 {
                let x = 20 + col * 180;
                let y = 100 + row * 300;
                input.push(IRect::new(x + 30, y, 32, 32)); // icon
                input.push(IRect::new(x, y + 36, 92, 20)); // pill
            }
        }
        for r in &input {
            d.add(*r);
        }
        assert!(d.rects().len() <= Damage::MAX_RECTS);
        for r in &input {
            assert!(d.rects().iter().any(|o| o.contains_rect(r)), "{r:?}");
        }
        let in_area: i64 = input.iter().map(IRect::area).sum();
        let out_area: i64 = d.rects().iter().map(IRect::area).sum();
        assert!(out_area <= 2 * in_area, "{out_area} vs {in_area}");
        assert!(out_area * 4 < d.bounds().area());
    }

    #[test]
    fn overflow_keeps_every_input_covered() {
        let mut d = Damage::new();
        let mut input = Vec::new();
        let mut s: u32 = 12345;
        let mut next = || {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
            i32::try_from((s >> 16) % 2000).unwrap()
        };
        for _ in 0..200 {
            let r = IRect::new(next(), next(), next() % 80 + 1, next() % 80 + 1);
            input.push(r);
            d.add(r);
            assert!(d.rects().len() <= Damage::MAX_RECTS);
            assert!(d.rects().iter().all(|o| !o.is_empty()));
        }
        for r in &input {
            assert!(d.rects().iter().any(|o| o.contains_rect(r)), "{r:?}");
        }
    }

    #[test]
    fn nearby_rects_merge() {
        let mut d = Damage::new();
        d.add(IRect::new(0, 0, 10, 10));
        d.add(IRect::new(10, 0, 10, 10));
        assert_eq!(d.rects(), &[IRect::new(0, 0, 20, 10)]);
    }

    #[test]
    fn clip_and_intersects() {
        let mut d = Damage::new();
        d.add(IRect::new(0, 0, 10, 10));
        d.add(IRect::new(100, 100, 10, 10));
        assert!(d.intersects(&IRect::new(5, 5, 1, 1)));
        assert!(!d.intersects(&IRect::new(50, 50, 1, 1)));
        d.clip(&IRect::new(0, 0, 50, 50));
        assert_eq!(d.rects(), &[IRect::new(0, 0, 10, 10)]);
        assert_eq!(d.take().len(), 1);
        assert!(d.is_empty());
    }
}
