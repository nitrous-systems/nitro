//! Damage regions: a bounded list of integer rects.

use crate::IRect;

/// Accumulated damage as a small list of non-empty pixel rects.
///
/// Rects are merged when one contains the other or when the merged bounding
/// box would waste little area; once more than [`Damage::MAX_RECTS`] would be
/// needed the whole region collapses to its bounding box. This keeps the
/// list cheap to produce, cheap to hand to `FB_DAMAGE_CLIPS`, and cheap to
/// send over a wire, at the cost of occasionally repainting a few extra
/// pixels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    rects: Vec<IRect>,
}

impl Damage {
    /// Maximum number of rects kept before collapsing to a bounding box.
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
            let b = self.bounds();
            self.rects.clear();
            self.rects.push(b);
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
        assert_eq!(d.rects().len(), 1);
        assert_eq!(d.bounds(), IRect::from_edges(-5000, 0, 15010, 10));
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
