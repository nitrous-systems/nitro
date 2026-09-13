//! Latency bookkeeping: the serial ledger and the percentile summary.
//!
//! # The ledger
//!
//! Every `PointerMotion` the client answers produces a commit, and the
//! commit's serial is the only thing the server will hand back. So the
//! client remembers the serial against the input's `time_ns` when it
//! commits, and subtracts on `Presented{serial, time_ns}`. Both
//! timestamps are the server's `CLOCK_MONOTONIC`, so the difference is
//! free of any skew between the two processes.
//!
//! The ledger is bounded ([`Ledger::CAPACITY`]) and evicts oldest-first.
//! A serial can go unanswered — a commit that damaged nothing is
//! acknowledged from the last vblank, a window can be destroyed — and an
//! unbounded map of those would be a slow leak in a process that is
//! supposed to prove the system is small. Eviction is by insertion order
//! because serials are allocated in order, which is also what makes
//! "lookup then drop" correct: a serial is reported exactly once.
//!
//! # The summary
//!
//! [`Histogram`] keeps every sample (a 5-second window at 100 Hz is 500
//! numbers; an hour of pointer abuse is a few hundred thousand `u64`s,
//! which is nothing) and sorts a scratch copy when asked. Percentiles over
//! a *sorted copy* rather than incrementally-maintained buckets because
//! the sample count here is small and exactness is worth more than the
//! cleverness: the whole point is a number a human will quote.
//!
//! The percentile rule is **nearest-rank**: p95 of N samples is the
//! sample at index `ceil(0.95 * N) - 1` of the sorted vector. It is the
//! definition that always names a real observation, never an
//! interpolation between two, which is what you want when you are going
//! to say "the 95th percentile frame took X µs" about a real frame.

use std::collections::VecDeque;

/// A latency sample, in microseconds.
pub type Micros = u64;

/// `serial → input time_ns`, bounded and insertion-ordered.
///
/// A `VecDeque` rather than a `HashMap`: serials arrive in order and are
/// answered in order, so the answer is almost always at the front, and the
/// eviction policy falls out for free. At [`Ledger::CAPACITY`] entries a
/// linear scan is cheaper than hashing anyway.
#[derive(Debug, Default)]
pub struct Ledger {
    entries: VecDeque<(u32, u64)>,
}

impl Ledger {
    /// How many unanswered serials to remember. At 60 Hz this is four
    /// seconds of commits — far more than the server's 200 ms input-stamp
    /// carry, so nothing that *can* still be answered is ever dropped.
    pub const CAPACITY: usize = 256;

    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember that `serial` answers the input at `input_ns`.
    ///
    /// Re-recording a serial replaces the entry: a client that batches two
    /// motions into one commit should report the *latest* input the frame
    /// answers, because that is the one whose photons the user is waiting
    /// for. Reporting the earlier one would flatter nothing — it would
    /// inflate the number — but it would also measure a different thing.
    pub fn record(&mut self, serial: u32, input_ns: u64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.0 == serial) {
            e.1 = input_ns;
            return;
        }
        if self.entries.len() >= Self::CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((serial, input_ns));
    }

    /// Take the input time for `serial`, if it is still known.
    ///
    /// Every serial older than the one being answered is dropped with it:
    /// the server reports in order, so an older serial still in the ledger
    /// was one no frame ever carried (a commit that damaged nothing, a
    /// window that went away). Keeping them would let a stale entry be
    /// matched by a much later `Presented` after serial wraparound.
    pub fn take(&mut self, serial: u32) -> Option<u64> {
        let pos = self.entries.iter().position(|e| e.0 == serial)?;
        let (_, ns) = self.entries.remove(pos)?;
        self.entries.drain(..pos);
        Some(ns)
    }

    /// Serials waiting for a `Presented`.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.entries.len()
    }
}

/// Latency samples plus the percentile maths over them.
#[derive(Debug, Default, Clone)]
pub struct Histogram {
    samples: Vec<Micros>,
    /// Samples at the last [`Histogram::mark`], so the periodic summary
    /// can report the interval as well as the run.
    mark: usize,
}

/// The numbers a summary prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    /// How many samples went into it.
    pub count: usize,
    /// Smallest sample, µs.
    pub min: Micros,
    /// Median (p50), µs.
    pub median: Micros,
    /// 95th percentile, µs.
    pub p95: Micros,
    /// Largest sample, µs.
    pub max: Micros,
    /// Arithmetic mean, µs — the figure the server's `i2p_mean_us`
    /// reports, kept so the two views can be compared directly.
    pub mean: Micros,
}

impl Histogram {
    /// An empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one sample, in microseconds.
    pub fn push(&mut self, us: Micros) {
        self.samples.push(us);
    }

    /// Add one sample given as a pair of `CLOCK_MONOTONIC` nanosecond
    /// stamps, ignoring a non-positive interval.
    ///
    /// A `Presented` that predates its input is not a negative latency, it
    /// is a serial that was answered from the *previous* vblank — the
    /// server does that for a commit that damaged nothing — and counting
    /// it as zero would quietly pull the median down.
    pub fn push_interval(&mut self, input_ns: u64, presented_ns: u64) -> Option<Micros> {
        let us = presented_ns.checked_sub(input_ns)? / 1_000;
        self.push(us);
        Some(us)
    }

    /// Number of samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Summary over every sample, or `None` when there are none.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        summarize(&self.samples)
    }

    /// Summary over the samples added since the last [`Histogram::mark`].
    #[must_use]
    pub fn since_mark(&self) -> Option<Summary> {
        summarize(&self.samples[self.mark.min(self.samples.len())..])
    }

    /// Start a new interval for [`Histogram::since_mark`].
    pub fn mark(&mut self) {
        self.mark = self.samples.len();
    }
}

/// Percentiles and extremes of `samples`, or `None` when it is empty.
///
/// Sorts a copy: the caller's order is the arrival order, which the
/// `--save-small`-style debugging wants to keep.
#[must_use]
pub fn summarize(samples: &[Micros]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let sum: u128 = sorted.iter().map(|&s| u128::from(s)).sum();
    Some(Summary {
        count: sorted.len(),
        min: sorted[0],
        median: percentile(&sorted, 50),
        p95: percentile(&sorted, 95),
        max: sorted[sorted.len() - 1],
        mean: u64::try_from(sum / sorted.len() as u128).unwrap_or(u64::MAX),
    })
}

/// Nearest-rank percentile of an **already sorted** slice.
///
/// `p` is a whole percent in `1..=100`; the rank is `ceil(p/100 * N)`,
/// clamped into the slice. Nearest-rank always returns a sample that was
/// actually observed — no interpolation — which is the honest thing to
/// quote about a frame.
///
/// # Panics
/// If `sorted` is empty.
#[must_use]
pub fn percentile(sorted: &[Micros], p: u32) -> Micros {
    assert!(!sorted.is_empty(), "percentile of no samples");
    let n = sorted.len() as u128;
    // ceil(p * n / 100) without floating point, so the rank is exact.
    let rank = (u128::from(p) * n).div_ceil(100).max(1);
    let index = usize::try_from(rank - 1).unwrap_or(sorted.len() - 1);
    sorted[index.min(sorted.len() - 1)]
}

impl Summary {
    /// One line: `count=N min=… median=… p95=… max=… mean=… µs`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "count={} min={} median={} p95={} max={} mean={} us",
            self.count, self.min, self.median, self.p95, self.max, self.mean
        )
    }
}

/// Whether the client's and the server's mean latency agree to within
/// `tolerance_us` — one refresh period, in practice.
///
/// They measure overlapping but different intervals (the client's spans
/// the round trip, the server's does not), so they cannot be equal; what
/// matters is that neither has lost the plot. Called by the `--stats`
/// cross-check.
#[must_use]
pub fn agree(client_us: Micros, server_us: Micros, tolerance_us: Micros) -> bool {
    client_us.abs_diff(server_us) <= tolerance_us
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_serial_is_answered_once() {
        let mut l = Ledger::new();
        l.record(7, 1_000);
        assert_eq!(l.take(7), Some(1_000));
        assert_eq!(l.take(7), None);
        assert_eq!(l.pending(), 0);
    }

    #[test]
    fn re_recording_a_serial_keeps_the_latest_input() {
        let mut l = Ledger::new();
        l.record(3, 100);
        l.record(3, 250);
        assert_eq!(l.pending(), 1);
        assert_eq!(l.take(3), Some(250));
    }

    /// Answering serial 5 discards 3 and 4: no frame will ever carry them.
    #[test]
    fn answering_a_serial_drops_the_older_unanswered_ones() {
        let mut l = Ledger::new();
        for s in 3..=5 {
            l.record(s, u64::from(s) * 10);
        }
        assert_eq!(l.take(5), Some(50));
        assert_eq!(l.pending(), 0);
        assert_eq!(l.take(3), None);
    }

    #[test]
    fn the_ledger_is_bounded() {
        let mut l = Ledger::new();
        for s in 0..(Ledger::CAPACITY as u32 + 50) {
            l.record(s, u64::from(s));
        }
        assert_eq!(l.pending(), Ledger::CAPACITY);
        // The oldest went first.
        assert_eq!(l.take(0), None);
        assert_eq!(l.take(50), Some(50));
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let s: Vec<Micros> = (1..=100).collect();
        assert_eq!(percentile(&s, 50), 50);
        assert_eq!(percentile(&s, 95), 95);
        assert_eq!(percentile(&s, 100), 100);
        assert_eq!(percentile(&s, 1), 1);
    }

    /// The rank must round *up*, or p95 of 10 samples would be the 9th.
    #[test]
    fn the_rank_rounds_up() {
        let s: Vec<Micros> = (1..=10).collect();
        assert_eq!(percentile(&s, 95), 10);
        assert_eq!(percentile(&s, 91), 10);
        assert_eq!(percentile(&s, 90), 9);
    }

    #[test]
    fn one_sample_is_every_percentile() {
        let s = [42];
        for p in [1, 50, 95, 100] {
            assert_eq!(percentile(&s, p), 42);
        }
    }

    #[test]
    fn a_summary_reports_the_five_numbers() {
        let mut h = Histogram::new();
        for us in [5_000, 1_000, 9_000, 3_000] {
            h.push(us);
        }
        let s = h.summary().unwrap();
        assert_eq!(
            s,
            Summary {
                count: 4,
                min: 1_000,
                median: 3_000,
                p95: 9_000,
                max: 9_000,
                mean: 4_500,
            }
        );
        assert!(s.line().contains("median=3000"));
    }

    #[test]
    fn an_empty_histogram_has_no_summary() {
        assert!(Histogram::new().summary().is_none());
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn the_mark_splits_the_run_into_intervals() {
        let mut h = Histogram::new();
        h.push(1_000);
        h.mark();
        h.push(3_000);
        h.push(5_000);
        assert_eq!(h.since_mark().unwrap().count, 2);
        assert_eq!(h.since_mark().unwrap().min, 3_000);
        assert_eq!(h.summary().unwrap().count, 3);
    }

    #[test]
    fn an_interval_is_nanoseconds_in_microseconds_out() {
        let mut h = Histogram::new();
        assert_eq!(h.push_interval(1_000_000, 9_400_000), Some(8_400));
        assert_eq!(h.len(), 1);
    }

    /// A `Presented` older than its input is the server answering from the
    /// previous vblank, not a negative latency: it is not a sample.
    #[test]
    fn a_presented_before_its_input_is_not_a_sample() {
        let mut h = Histogram::new();
        assert_eq!(h.push_interval(9_000_000, 1_000_000), None);
        assert!(h.is_empty());
    }

    #[test]
    fn agreement_is_within_a_frame() {
        assert!(agree(8_700, 8_000, 16_667));
        assert!(agree(8_000, 8_700, 16_667));
        assert!(!agree(40_000, 8_000, 16_667));
    }
}
