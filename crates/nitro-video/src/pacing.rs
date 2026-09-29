//! Frame pacing: which decoded frame goes on screen at a given flip.
//!
//! Pure logic, no I/O. A [`Clock`] maps `CLOCK_MONOTONIC` nanoseconds to
//! media time (track ticks), anchored at one `(mono_ns, pts)` pair. It is
//! re-anchored when playback starts, resumes or lands after a seek. For
//! each flip the player asks [`pick`] for the newest ready frame whose pts
//! is due by the flip's target time. The older ready frames lose the race
//! and are skipped rather than shown late: a video that falls behind
//! drops frames, it does not slow down.

/// Media time as a function of the monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clock {
    /// Ticks per second.
    pub timescale: u32,
    anchor: Option<(u64, i64)>,
}

impl Clock {
    /// An unanchored clock for a track of `timescale` ticks per second.
    #[must_use]
    pub fn new(timescale: u32) -> Self {
        Self {
            timescale: timescale.max(1),
            anchor: None,
        }
    }

    /// Whether the clock has been anchored (it runs only then).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.anchor.is_some()
    }

    /// Start (or restart) the clock so that `pts` is shown at `mono_ns`.
    pub fn anchor(&mut self, mono_ns: u64, pts: i64) {
        self.anchor = Some((mono_ns, pts));
    }

    /// Stop the clock (pause, seek). The next [`Clock::anchor`] restarts it.
    pub fn stop(&mut self) {
        self.anchor = None;
    }

    /// The media time due at `mono_ns`; `None` while stopped.
    #[must_use]
    pub fn media_at(&self, mono_ns: u64) -> Option<i64> {
        let (t0, p0) = self.anchor?;
        let dt = i128::from(mono_ns) - i128::from(t0);
        let ticks = dt * i128::from(self.timescale) / 1_000_000_000;
        Some(p0.saturating_add(i64::try_from(ticks).unwrap_or(i64::MAX)))
    }

    /// When `pts` is due on the monotonic clock; `None` while stopped.
    #[must_use]
    pub fn mono_at(&self, pts: i64) -> Option<u64> {
        let (t0, p0) = self.anchor?;
        let dp = i128::from(pts) - i128::from(p0);
        let ns = i128::from(t0) + dp * 1_000_000_000 / i128::from(self.timescale);
        Some(u64::try_from(ns.max(0)).unwrap_or(u64::MAX))
    }
}

/// What [`pick`] decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pick {
    /// Index (into the `ready` slice) of the frame to show, if any is due.
    pub show: Option<usize>,
    /// Indices of older frames that were due too and are skipped.
    pub skip: Vec<usize>,
}

/// Choose among `ready` frames (their pts, in any order) for a flip at
/// media time `target`: the newest one with `pts <= target` is shown, and
/// every other due one is skipped. Frames still in the future are left
/// alone.
#[must_use]
pub fn pick(ready: &[i64], target: i64) -> Pick {
    let mut best: Option<usize> = None;
    for (i, &p) in ready.iter().enumerate() {
        if p <= target && best.is_none_or(|b| p > ready[b]) {
            best = Some(i);
        }
    }
    let skip = match best {
        Some(b) => (0..ready.len())
            .filter(|&i| i != b && ready[i] <= target)
            .collect(),
        None => Vec::new(),
    };
    Pick { show: best, skip }
}

/// `m:ss`, or `h:mm:ss` from an hour up, for the time label.
#[must_use]
pub fn format_time(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    };
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_runs_from_its_anchor() {
        let mut c = Clock::new(90_000);
        assert_eq!(c.media_at(5), None);
        c.anchor(1_000_000_000, 9_000);
        assert_eq!(c.media_at(1_000_000_000), Some(9_000));
        assert_eq!(c.media_at(2_000_000_000), Some(99_000));
        assert_eq!(c.media_at(500_000_000), Some(-36_000));
        assert_eq!(c.mono_at(99_000), Some(2_000_000_000));
        c.stop();
        assert!(!c.is_running());
    }

    #[test]
    fn pause_and_resume_reanchor() {
        let mut c = Clock::new(1000);
        c.anchor(0, 0);
        let at_pause = c.media_at(3_000_000_000).unwrap();
        c.stop();
        // Ten seconds later, resume at the paused position.
        c.anchor(13_000_000_000, at_pause);
        assert_eq!(c.media_at(14_000_000_000), Some(4000));
    }

    #[test]
    fn pick_shows_the_newest_due_frame_and_skips_the_rest() {
        let p = pick(&[30, 10, 20, 40], 35);
        assert_eq!(p.show, Some(0));
        assert_eq!(p.skip, vec![1, 2]);
        assert_eq!(pick(&[50, 60], 35), Pick::default());
        assert_eq!(pick(&[], 35), Pick::default());
        assert_eq!(pick(&[35], 35).show, Some(0));
    }

    #[test]
    fn times_format_like_a_player() {
        assert_eq!(format_time(0.0), "0:00");
        assert_eq!(format_time(65.9), "1:05");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(f64::NAN), "0:00");
        assert_eq!(format_time(-3.0), "0:00");
    }
}
