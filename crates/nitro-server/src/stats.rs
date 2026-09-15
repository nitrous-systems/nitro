//! Rolling frame statistics: what the `stats` control request reports.
//!
//! The server's performance claims are only worth what they can be
//! measured at, and the cheapest place to measure is the frame loop
//! itself: it already knows when it started painting, how many pixels the
//! damage covered, and when the input event that caused the frame arrived.
//! This module keeps those three numbers in fixed-size windows so that
//! `nitroctl stats` answers "how is it doing *now*", not "how has it done
//! since boot" — an average over an hour hides the stutter a test is
//! looking for.
//!
//! The windows are short on purpose: [`PAINT_WINDOW`] frames is two
//! seconds at 60 Hz (one at 120), long enough to average out scheduler
//! noise and short enough that a regression shows up while the tester is
//! still watching. A count rather than a duration, deliberately: the
//! quantity being averaged is per-frame work, so "the last N frames" is
//! the honest window at any refresh rate — what changes with the rate is
//! how many seconds of history that is, not what the number means.
//! [`I2P_WINDOW`] input-to-photon samples is the same idea for a quantity
//! that is only sampled when there is input to sample.
//!
//! Everything here is deliberately dumb. A [`Window`] is a ring of `u64`
//! with a write cursor, allocated once; `push` is a store and an index
//! bump, with no allocation, no timestamps and no locking, because it runs
//! on the frame path. Min, mean and max are computed by scanning the
//! window when they are asked for — see [`Window::min`] for why that is the
//! right trade — so a frame pays for nothing it does not use.
//!
//! There is no histogram and no percentile. Percentiles over 120 samples
//! are mostly noise; when the server needs a real latency distribution it
//! will need a real sampling story (and probably a trace buffer), which is
//! a later decision.

/// A fixed-capacity ring of `u64` samples with min/mean/max over the
/// window.
///
/// Holds the most recent `capacity` samples; the oldest is dropped when a
/// new one arrives at a full window. The backing `Vec` is allocated once,
/// in [`Window::new`], and never grows.
#[derive(Debug, Clone)]
pub struct Window {
    /// The samples, at most `capacity` of them; a ring once full.
    buf: Vec<u64>,
    /// Where the next sample goes once the ring is full.
    next: usize,
    /// Maximum number of samples retained; at least 1.
    capacity: usize,
}

impl Window {
    /// A window keeping the last `capacity` samples (capacity >= 1).
    ///
    /// A capacity of 0 would make every statistic meaningless, so it is
    /// clamped to 1 rather than rejected: statistics must never be the
    /// reason the server fails to start.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: Vec::with_capacity(capacity),
            next: 0,
            capacity,
        }
    }

    /// Record one sample, evicting the oldest when full.
    pub fn push(&mut self, value: u64) {
        if self.buf.len() < self.capacity {
            self.buf.push(value);
            // Keep `next` pointing at the oldest slot for the moment the
            // window fills up, which is slot 0 while it is still filling.
            self.next = 0;
        } else {
            self.buf[self.next] = value;
            self.next = (self.next + 1) % self.capacity;
        }
    }

    /// Number of samples currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether no samples have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Smallest sample in the window, 0 when empty.
    ///
    /// Scanning beats maintaining incremental extrema: eviction can drop
    /// the current minimum, so an incremental version needs a heap or a
    /// monotonic deque to stay correct, and pays for it on every frame.
    /// This scan touches at most [`PAINT_WINDOW`] elements and runs once
    /// per `stats` request — a rate set by a human typing — so the
    /// per-frame cost is zero, which is the cost that matters.
    #[must_use]
    pub fn min(&self) -> u64 {
        self.buf.iter().copied().min().unwrap_or(0)
    }

    /// Largest sample in the window, 0 when empty.
    #[must_use]
    pub fn max(&self) -> u64 {
        self.buf.iter().copied().max().unwrap_or(0)
    }

    /// Arithmetic mean over the window, rounded down; 0 when empty.
    ///
    /// Summed in `u128` so that a window full of implausibly large samples
    /// still cannot overflow; the sum is bounded anyway by
    /// `capacity * u64::MAX`.
    #[must_use]
    pub fn mean(&self) -> u64 {
        if self.buf.is_empty() {
            return 0;
        }
        let sum: u128 = self.buf.iter().map(|&v| u128::from(v)).sum();
        let mean = sum / self.buf.len() as u128;
        u64::try_from(mean).unwrap_or(u64::MAX)
    }

    /// Forget every sample.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.next = 0;
    }
}

/// How many frames of paint timing and damage area are kept.
pub const PAINT_WINDOW: usize = 120;

/// How many input-to-photon samples are kept.
pub const I2P_WINDOW: usize = 100;

/// The server's rolling frame statistics.
///
/// One value lives in the server state; the frame loop pushes into it and
/// the control socket reads it. The fields are public because there is
/// nothing to protect: they are windows of numbers, and a wrapper method
/// per field would only obscure which window a call site feeds.
#[derive(Debug)]
pub struct FrameStats {
    /// Microseconds spent rasterizing, per frame.
    pub paint_us: Window,
    /// Microseconds spent streaming the shadow into the scanout buffer,
    /// per frame; always 0 under `NITRO_SHADOW=0`, where there is no copy.
    pub copy_us: Window,
    /// Damaged device pixels repainted, per frame.
    pub damage_px: Window,
    /// Input-to-photon latency in microseconds.
    pub i2p_us: Window,
}

impl FrameStats {
    /// Empty statistics with the documented window sizes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            paint_us: Window::new(PAINT_WINDOW),
            copy_us: Window::new(PAINT_WINDOW),
            damage_px: Window::new(PAINT_WINDOW),
            i2p_us: Window::new(I2P_WINDOW),
        }
    }

    /// Append the `key value` pairs for the `stats` reply.
    ///
    /// The order is part of the reply format: clients that print the pairs
    /// verbatim get a stable layout, and the golden output of the control
    /// tests depends on it.
    pub fn write_pairs(&self, out: &mut Vec<(&'static str, u64)>) {
        // The key naming is inconsistent on purpose: the paint keys put
        // the unit before the statistic (`paint_us_min`) and the
        // input-to-photon keys after it (`i2p_min_us`). That is what the
        // protocol spec says, and the wire format outranks tidiness; do
        // not "fix" it here without changing the spec and the clients.
        out.push(("paint_us_min", self.paint_us.min()));
        out.push(("paint_us_mean", self.paint_us.mean()));
        out.push(("paint_us_max", self.paint_us.max()));
        // The copy keys sit next to the paint ones and are spelled the
        // same way, because they are the other half of the same frame:
        // `paint_us` is the rasterizer in cached heap memory,
        // `copy_us` the write-combined stream out to the scanout buffer.
        out.push(("copy_us_min", self.copy_us.min()));
        out.push(("copy_us_mean", self.copy_us.mean()));
        out.push(("copy_us_max", self.copy_us.max()));
        out.push(("damage_px_mean", self.damage_px.mean()));
        out.push(("i2p_min_us", self.i2p_us.min()));
        out.push(("i2p_mean_us", self.i2p_us.mean()));
        out.push(("i2p_max_us", self.i2p_us.max()));
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameStats, I2P_WINDOW, PAINT_WINDOW, Window};

    #[test]
    fn empty_window_is_all_zeros() {
        let w = Window::new(8);
        assert!(w.is_empty());
        assert_eq!(w.len(), 0);
        assert_eq!(w.min(), 0);
        assert_eq!(w.max(), 0);
        assert_eq!(w.mean(), 0);
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let mut w = Window::new(0);
        w.push(5);
        w.push(9);
        assert_eq!(w.len(), 1);
        assert_eq!(w.min(), 9);
        assert_eq!(w.max(), 9);
        assert_eq!(w.mean(), 9);
    }

    #[test]
    fn partially_filled_window_uses_only_real_samples() {
        let mut w = Window::new(4);
        w.push(10);
        w.push(30);
        assert_eq!(w.len(), 2);
        assert!(!w.is_empty());
        assert_eq!(w.min(), 10);
        assert_eq!(w.max(), 30);
        assert_eq!(w.mean(), 20);
    }

    #[test]
    fn full_window_evicts_the_oldest() {
        let mut w = Window::new(3);
        for v in [100, 1, 2, 3] {
            w.push(v);
        }
        // 100 has been evicted; only 1, 2, 3 remain.
        assert_eq!(w.len(), 3);
        assert_eq!(w.min(), 1);
        assert_eq!(w.max(), 3);
        assert_eq!(w.mean(), 2);

        w.push(4);
        assert_eq!(w.min(), 2);
        assert_eq!(w.max(), 4);
        assert_eq!(w.mean(), 3);
    }

    #[test]
    fn eviction_survives_many_wraps() {
        let mut w = Window::new(5);
        for v in 0..1000u64 {
            w.push(v);
        }
        // The last five samples are 995..=999.
        assert_eq!(w.len(), 5);
        assert_eq!(w.min(), 995);
        assert_eq!(w.max(), 999);
        assert_eq!(w.mean(), 997);
    }

    #[test]
    fn mean_rounds_down() {
        let mut w = Window::new(4);
        for v in [1, 2, 2, 2] {
            w.push(v);
        }
        // 7 / 4 = 1.75 -> 1
        assert_eq!(w.mean(), 1);
    }

    #[test]
    fn mean_does_not_overflow() {
        let mut w = Window::new(4);
        for _ in 0..4 {
            w.push(u64::MAX);
        }
        assert_eq!(w.mean(), u64::MAX);
    }

    #[test]
    fn clear_forgets_everything() {
        let mut w = Window::new(3);
        w.push(7);
        w.push(8);
        w.clear();
        assert!(w.is_empty());
        assert_eq!(w.max(), 0);
        // And the ring still behaves after being emptied.
        w.push(4);
        assert_eq!(w.len(), 1);
        assert_eq!(w.min(), 4);
    }

    #[test]
    fn fresh_stats_have_the_documented_capacities() {
        let mut s = FrameStats::new();
        for i in 0..(PAINT_WINDOW + I2P_WINDOW) as u64 {
            s.paint_us.push(i);
            s.copy_us.push(i);
            s.damage_px.push(i);
            s.i2p_us.push(i);
        }
        assert_eq!(s.paint_us.len(), PAINT_WINDOW);
        assert_eq!(s.copy_us.len(), PAINT_WINDOW);
        assert_eq!(s.damage_px.len(), PAINT_WINDOW);
        assert_eq!(s.i2p_us.len(), I2P_WINDOW);
    }

    #[test]
    fn write_pairs_emits_the_documented_keys_in_order() {
        let mut s = FrameStats::default();
        for v in [10, 20, 60] {
            s.paint_us.push(v);
        }
        for v in [1, 2, 3] {
            s.copy_us.push(v);
        }
        for v in [100, 300] {
            s.damage_px.push(v);
        }
        for v in [5, 7, 8] {
            s.i2p_us.push(v);
        }

        let mut out = Vec::new();
        s.write_pairs(&mut out);
        assert_eq!(
            out,
            vec![
                ("paint_us_min", 10),
                ("paint_us_mean", 30),
                ("paint_us_max", 60),
                ("copy_us_min", 1),
                ("copy_us_mean", 2),
                ("copy_us_max", 3),
                ("damage_px_mean", 200),
                ("i2p_min_us", 5),
                ("i2p_mean_us", 6),
                ("i2p_max_us", 8),
            ]
        );
    }

    #[test]
    fn write_pairs_on_empty_stats_is_all_zeros() {
        let mut out = Vec::new();
        FrameStats::new().write_pairs(&mut out);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|&(_, v)| v == 0));
    }

    #[test]
    fn write_pairs_appends_rather_than_replaces() {
        let mut out = vec![("frames", 42)];
        FrameStats::new().write_pairs(&mut out);
        assert_eq!(out.len(), 11);
        assert_eq!(out[0], ("frames", 42));
    }
}
