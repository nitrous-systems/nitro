//! Buffer-age damage for the output ring.
//!
//! The server reports what changed since the **previous frame**. A ring
//! slot was last drawn some frames ago, so its pixels are stale by every
//! frame's damage since then: the clip for drawing into it is this
//! frame's damage ∪ the damage of every frame after the slot's last one.
//! With two slots that is "age 2" (this frame + the previous one); the
//! bookkeeping here works for any `n`.
//!
//! A slot that was never drawn, or whose history has been pruned (more
//! than [`HISTORY`] frames ago), gets a full clip. Reallocating the ring
//! (new size or modifier) starts over with every slot never drawn.
//!
//! The accumulation uses [`nitro_core::Damage`], which bounds the rect
//! count by merging the pairs that waste the least area.

use std::collections::VecDeque;

use nitro_core::{Damage, IRect};

/// Frames of damage history kept. Enough for any ring of up to
/// [`crate::proto::MAX_RING`] slots driven round-robin, with room for a
/// server that skips a slot now and then.
pub const HISTORY: usize = 16;

/// Per-output ring state.
#[derive(Debug, Clone, Default)]
pub struct DamageRing {
    w: i32,
    h: i32,
    /// Frame number each slot last received, if any.
    last: Vec<Option<u64>>,
    /// `(frame number, damage)` of the newest frames, oldest first.
    history: VecDeque<(u64, Damage)>,
    /// Frames started so far.
    frame: u64,
}

impl DamageRing {
    /// A ring of `n` slots of `w × h` pixels, none drawn yet.
    #[must_use]
    pub fn new(n: usize, w: u32, h: u32) -> Self {
        Self {
            w: crate::px(w),
            h: crate::px(h),
            last: vec![None; n],
            history: VecDeque::with_capacity(HISTORY),
            frame: 0,
        }
    }

    /// Number of slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.last.len()
    }

    /// Whether the ring has no slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.last.is_empty()
    }

    /// The whole output.
    #[must_use]
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.w, self.h)
    }

    /// The clip for drawing `damage` (this frame's change, any rects) into
    /// `slot` — **without** recording the frame. Use [`DamageRing::commit`]
    /// once the frame was actually submitted.
    ///
    /// # Panics
    /// Never; an out-of-range `slot` gets a full clip.
    #[must_use]
    pub fn clip_for(&self, slot: usize, damage: &[IRect]) -> Vec<IRect> {
        let full = vec![self.bounds()];
        let Some(Some(last)) = self.last.get(slot).copied() else {
            return full;
        };
        // The history must reach back to the frame right after `last`.
        match self.history.front() {
            Some((oldest, _)) if *oldest <= last + 1 => {}
            None if last + 1 > self.frame => {}
            _ => return full,
        }
        let mut acc = Damage::new();
        for (f, d) in &self.history {
            if *f > last {
                acc.add_all(d);
            }
        }
        for r in damage {
            acc.add(*r);
        }
        acc.clip(&self.bounds());
        acc.take()
    }

    /// Record that a frame with `damage` was drawn into `slot`.
    pub fn commit(&mut self, slot: usize, damage: &[IRect]) {
        self.frame += 1;
        let mut d = Damage::new();
        for r in damage {
            d.add(*r);
        }
        d.clip(&self.bounds());
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back((self.frame, d));
        if let Some(s) = self.last.get_mut(slot) {
            *s = Some(self.frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> IRect {
        IRect::new(x, y, w, h)
    }

    #[test]
    fn never_drawn_slots_get_the_full_output() {
        let ring = DamageRing::new(2, 100, 50);
        assert_eq!(ring.clip_for(0, &[r(1, 1, 2, 2)]), vec![r(0, 0, 100, 50)]);
        assert_eq!(ring.clip_for(7, &[]), vec![r(0, 0, 100, 50)]);
    }

    #[test]
    fn age_two_adds_the_previous_frame() {
        let mut ring = DamageRing::new(2, 100, 100);
        ring.commit(0, &[r(0, 0, 10, 10)]); // frame 1 → slot 0 (full clip really)
        ring.commit(1, &[r(50, 50, 10, 10)]); // frame 2 → slot 1
        // Frame 3 into slot 0: its own damage + frame 2's.
        let clip = ring.clip_for(0, &[r(20, 20, 5, 5)]);
        assert!(clip.contains(&r(50, 50, 10, 10)), "{clip:?}");
        assert!(clip.contains(&r(20, 20, 5, 5)), "{clip:?}");
        assert!(
            !clip.iter().any(|c| c.contains(1, 1)),
            "frame 1 already in slot 0: {clip:?}"
        );
    }

    #[test]
    fn age_n_accumulates_every_frame_since_the_slot() {
        let mut ring = DamageRing::new(3, 1000, 1000);
        for (slot, x) in [(0, 0), (1, 100), (2, 200)] {
            ring.commit(slot, &[r(x, 0, 10, 10)]);
        }
        let clip = ring.clip_for(0, &[]);
        assert!(clip.iter().any(|c| c.contains(100, 0)));
        assert!(clip.iter().any(|c| c.contains(200, 0)));
        assert!(!clip.iter().any(|c| c.contains(0, 0)));
    }

    #[test]
    fn empty_damage_after_catching_up_is_empty() {
        let mut ring = DamageRing::new(1, 10, 10);
        ring.commit(0, &[]);
        assert!(ring.clip_for(0, &[]).is_empty());
    }

    #[test]
    fn damage_is_clipped_to_the_output() {
        let mut ring = DamageRing::new(1, 10, 10);
        ring.commit(0, &[]);
        assert_eq!(ring.clip_for(0, &[r(5, 5, 100, 100)]), vec![r(5, 5, 5, 5)]);
        assert!(ring.clip_for(0, &[r(-50, -50, 10, 10)]).is_empty());
    }

    #[test]
    fn pruned_history_falls_back_to_full() {
        let mut ring = DamageRing::new(2, 10, 10);
        ring.commit(0, &[]);
        for _ in 0..=HISTORY {
            ring.commit(1, &[r(0, 0, 1, 1)]);
        }
        assert_eq!(ring.clip_for(0, &[]), vec![r(0, 0, 10, 10)]);
        // Slot 1 is current: only the new damage.
        assert!(ring.clip_for(1, &[]).is_empty());
    }

    #[test]
    fn many_rects_stay_bounded() {
        let mut ring = DamageRing::new(1, 1000, 1000);
        ring.commit(0, &[]);
        let dmg: Vec<_> = (0..64).map(|i| r(i * 15, i * 15, 3, 3)).collect();
        let clip = ring.clip_for(0, &dmg);
        assert!(clip.len() <= Damage::MAX_RECTS);
        for d in &dmg {
            assert!(clip.iter().any(|c| c.contains_rect(d)));
        }
    }
}
