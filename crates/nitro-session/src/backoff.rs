//! When to restart a shell piece that died.
//!
//! One number with one rule: **1 s, doubling, capped at 30 s, reset by a
//! run that outlived the cap.**
//!
//! A restart policy is where a supervisor turns a broken program into a
//! broken *machine*, so the two failure modes it has to avoid are worth
//! naming:
//!
//! * **No delay at all** turns `nitro-bar` with a missing font into a
//!   fork bomb: the piece dies in 20 ms, is restarted, dies again, and
//!   the session spends a core on it forever. On a 3.3 GB box with two
//!   cores and no swap that is the difference between "the bar is
//!   missing" and "the box is gone".
//! * **A delay that only grows** turns one bad afternoon into a desktop
//!   that takes half a minute to put its bar back a week later, because
//!   nothing ever forgave the crashes from Tuesday.
//!
//! So the delay resets, and the condition for resetting is the one fact
//! the supervisor actually has: **how long the last run lasted**. A run
//! that outlived [`MAX`] is evidence that the delay we imposed was
//! already enough — the piece came up, did its job, and something else
//! killed it later. Anything shorter is treated as the same crash
//! continuing.
//!
//! There is deliberately no "give up after N tries". A desktop with no
//! bar is a desktop the user cannot fix from inside, and 30 s of patience
//! costs one wakeup every 30 s — which is nothing, and is what lets a
//! `just deploy` of a fixed binary be picked up by the *running* session
//! without a restart.

use std::time::Duration;

/// The first delay after a crash.
pub const FIRST: Duration = Duration::from_secs(1);
/// The longest delay the policy will ever impose.
pub const MAX: Duration = Duration::from_secs(30);

/// Per-piece restart state: the delay the *next* crash will cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    first: Duration,
    max: Duration,
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(FIRST, MAX)
    }
}

impl Backoff {
    /// A policy with explicit bounds. Tests — and a developer who does
    /// not want to wait 30 s — use small ones; the shipped session uses
    /// [`FIRST`] and [`MAX`].
    ///
    /// `first` is clamped to `max`, so a mis-set pair cannot produce a
    /// delay longer than the cap it is capped by.
    #[must_use]
    pub fn new(first: Duration, max: Duration) -> Self {
        let first = first.min(max);
        Self {
            first,
            max,
            next: first,
        }
    }

    /// The delay to wait before restarting a piece whose run lasted
    /// `uptime`, and advance the policy.
    ///
    /// The reset is applied *before* the delay is taken, so the first
    /// crash after a long healthy run is answered in `first` rather than
    /// in whatever the previous crash storm had climbed to.
    pub fn after_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= self.max {
            self.next = self.first;
        }
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    /// The delay the next crash would cost, without advancing anything.
    #[must_use]
    pub fn peek(&self) -> Duration {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A crash loop: 1, 2, 4, 8, 16, 30, 30, … The cap is a cap, not a
    /// step the sequence overshoots on its way past.
    #[test]
    fn the_delay_doubles_and_stops_at_thirty_seconds() {
        let mut b = Backoff::default();
        let died_at_once = Duration::from_millis(20);
        let got: Vec<u64> = (0..8)
            .map(|_| b.after_exit(died_at_once).as_secs())
            .collect();
        assert_eq!(got, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    /// A run that outlived the cap is evidence the last delay was
    /// enough, so the next crash is answered in one second again.
    #[test]
    fn a_run_that_outlived_the_cap_forgives_the_history() {
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.after_exit(Duration::from_millis(20));
        }
        assert_eq!(b.peek(), Duration::from_secs(30));
        assert_eq!(b.after_exit(Duration::from_secs(31)), FIRST);
        // …and it really is a reset, not one cheap restart: the next
        // crash costs 2 s, the second step of the sequence.
        assert_eq!(
            b.after_exit(Duration::from_millis(20)),
            Duration::from_secs(2)
        );
    }

    /// The boundary is `>= max`, and a run just short of it is still the
    /// same crash continuing. Written as its own test because "the piece
    /// lasted 29.9 s" is exactly the case a `>` would get wrong.
    #[test]
    fn a_run_just_short_of_the_cap_does_not_reset() {
        let mut b = Backoff::default();
        assert_eq!(
            b.after_exit(Duration::from_millis(20)),
            Duration::from_secs(1)
        );
        assert_eq!(
            b.after_exit(MAX.checked_sub(Duration::from_millis(100)).unwrap()),
            Duration::from_secs(2)
        );
        assert_eq!(b.after_exit(MAX), Duration::from_secs(1));
    }

    #[test]
    fn a_first_longer_than_the_max_is_clamped_rather_than_honoured() {
        let mut b = Backoff::new(Duration::from_secs(10), Duration::from_secs(2));
        assert_eq!(b.after_exit(Duration::ZERO), Duration::from_secs(2));
    }
}
