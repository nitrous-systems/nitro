//! The per-stage input latency timeline (#3974): measurement only.
//!
//! `samples i2p` says how long input took to reach the screen, not where
//! the time went. This records, per frame that carried an input, a
//! `CLOCK_MONOTONIC` stamp at every step the server sees:
//!
//! | column       | stamped when                                                  |
//! |--------------|---------------------------------------------------------------|
//! | `input`      | the event's own time (evdev, or an injected event's due time); the **earliest** one this frame answers |
//! | `input_last` | the newest such event (what `samples i2p` measures from)       |
//! | `rx`         | the server routed the earliest one                             |
//! | `sent`       | the first input message written to a client for it            |
//! | `present`    | the first `PresentSurface(Fenced)` or `Commit` after `sent`    |
//! | `fence`      | that frame's acquire fence signalled (= `present` when none, or already signalled) |
//! | `latch`      | a queued Surface frame latched onto this output                |
//! | `paint`      | `paint` started on the output                                  |
//! | `commit`     | the atomic commit returned                                     |
//! | `vblank`     | the flip's vblank timestamp                                    |
//! | `srv_input`  | the input stamp the server's own i2p used for this flip (0: none) |
//! | `deferred`   | 1 when a cursor-only flip was held for the client's answer     |
//! | `unpresented`| the latching client's frames not yet `Presented`, after the latch |
//!
//! 0 means the stage was not seen. Which client frame answered which input
//! is a heuristic: the first one received after the input was sent, on
//! whatever output it latches to. With inputs further apart than a frame
//! that is exact; with denser input a frame answers every input since the
//! previous one, and `input` is the oldest of them.
//!
//! Off by default and then free: [`Timeline`] holds `None` and every hook
//! is one branch. `NITRO_TIMELINE=1` turns it on at start; the control
//! request `timeline on|off|clear` at run time, and `timeline` dumps it.
//! On, it is a ring of [`CAPACITY`] records (about 224 KiB).

use std::collections::VecDeque;

/// Records kept; older ones are dropped.
pub const CAPACITY: usize = 2048;

/// An input older than this with no frame to show it is abandoned, like
/// the server's own i2p carry.
const MAX_AGE_NS: u64 = 200_000_000;

/// The dump's column names, in [`FrameRecord::columns`] order.
pub const HEADER: &str = "output seq input input_last rx sent present fence latch paint commit vblank srv_input deferred unpresented";

/// The stages of one frame's life, filled in as they happen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stages {
    /// Earliest input event time answered.
    pub input: u64,
    /// Newest input event time answered.
    pub input_last: u64,
    /// The server routed the earliest input.
    pub rx: u64,
    /// The first input message was written to a client.
    pub sent: u64,
    /// The client's answering frame arrived.
    pub present: u64,
    /// That frame's acquire fence signalled.
    pub fence: u64,
    /// A Surface frame latched onto the output.
    pub latch: u64,
    /// `paint` started.
    pub paint: u64,
    /// The commit returned.
    pub commit: u64,
    /// A cursor-only flip was held for the answer.
    pub deferred: bool,
    /// The latching client's frames not yet `Presented`.
    pub unpresented: u32,
    /// The answer is a `PresentSurface`: only its latch claims it.
    pub surface: bool,
}

impl Stages {
    fn is_empty(&self) -> bool {
        self.input == 0 && self.present == 0
    }

    /// Fold `other` (an earlier stage set) into this one: earliest stamps
    /// win, except `input_last`.
    fn merge(&mut self, other: &Self) {
        let min = |a: u64, b: u64| match (a, b) {
            (0, x) | (x, 0) => x,
            (a, b) => a.min(b),
        };
        self.input = min(self.input, other.input);
        self.input_last = self.input_last.max(other.input_last);
        self.rx = min(self.rx, other.rx);
        self.sent = min(self.sent, other.sent);
        self.present = min(self.present, other.present);
        self.fence = min(self.fence, other.fence);
        self.latch = min(self.latch, other.latch);
        self.paint = min(self.paint, other.paint);
        self.commit = min(self.commit, other.commit);
        self.deferred |= other.deferred;
        self.surface |= other.surface;
        self.unpresented = self.unpresented.max(other.unpresented);
    }
}

/// One frame that reached the screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameRecord {
    /// Scene output id.
    pub output: u32,
    /// Vblank sequence.
    pub seq: u64,
    /// The stages before the flip.
    pub stages: Stages,
    /// The vblank timestamp.
    pub vblank: u64,
    /// The stamp the server's i2p used (0: none).
    pub srv_input: u64,
}

impl FrameRecord {
    /// The values in [`HEADER`] order.
    #[must_use]
    pub fn columns(&self) -> [u64; 15] {
        let s = &self.stages;
        [
            u64::from(self.output),
            self.seq,
            s.input,
            s.input_last,
            s.rx,
            s.sent,
            s.present,
            s.fence,
            s.latch,
            s.paint,
            s.commit,
            self.vblank,
            self.srv_input,
            u64::from(s.deferred),
            u64::from(s.unpresented),
        ]
    }
}

/// Per-output stages: the frame being prepared and the one in flight.
#[derive(Debug, Default)]
struct OutputStages {
    output: u32,
    painting: Stages,
    in_flight: Stages,
}

#[derive(Debug, Default)]
struct Ring {
    records: VecDeque<FrameRecord>,
    total: u64,
    /// Input routed, not yet on any output.
    pending: Stages,
    outputs: Vec<OutputStages>,
}

impl Ring {
    fn output(&mut self, output: u32) -> &mut OutputStages {
        let i = match self.outputs.iter().position(|o| o.output == output) {
            Some(i) => i,
            None => {
                self.outputs.push(OutputStages {
                    output,
                    ..OutputStages::default()
                });
                self.outputs.len() - 1
            }
        };
        &mut self.outputs[i]
    }

    /// Hand the pending stages to `output`'s frame being prepared.
    fn claim(&mut self, output: u32) {
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        self.output(output).painting.merge(&pending);
    }
}

/// `CLOCK_MONOTONIC` now, in nanoseconds.
fn monotonic_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(t.tv_nsec).unwrap_or(0)
}

/// The timeline: `None` (and free) unless enabled. Every hook reads the
/// clock itself, and only when enabled.
#[derive(Debug)]
pub struct Timeline {
    ring: Option<Box<Ring>>,
    clock: fn() -> u64,
}

impl Default for Timeline {
    fn default() -> Self {
        Self {
            ring: None,
            clock: monotonic_ns,
        }
    }
}

impl Timeline {
    /// Off.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// On when `NITRO_TIMELINE=1`.
    #[must_use]
    pub fn from_env() -> Self {
        let mut t = Self::new();
        t.set_enabled(std::env::var("NITRO_TIMELINE").as_deref() == Ok("1"));
        t
    }

    /// Whether stamps are being taken.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.ring.is_some()
    }

    /// Turn recording on (keeping what is there) or off (freeing it).
    pub fn set_enabled(&mut self, on: bool) {
        if !on {
            self.ring = None;
        } else if self.ring.is_none() {
            self.ring = Some(Box::new(Ring {
                records: VecDeque::with_capacity(CAPACITY),
                ..Ring::default()
            }));
        }
    }

    /// Forget every record and every frame in progress.
    pub fn clear(&mut self) {
        if self.ring.is_some() {
            self.ring = None;
            self.set_enabled(true);
        }
    }

    /// An input event stamped `time_ns` was routed.
    pub fn input(&mut self, time_ns: u64) {
        let Some(r) = self.ring.as_deref_mut() else {
            return;
        };
        let now = (self.clock)();
        if r.pending.rx != 0 && now.saturating_sub(r.pending.rx) > MAX_AGE_NS {
            r.pending = Stages::default();
        }
        let p = &mut r.pending;
        if p.input == 0 || time_ns < p.input {
            p.input = time_ns;
        }
        p.input_last = p.input_last.max(time_ns);
        if p.rx == 0 {
            p.rx = now;
        }
    }

    /// An input message was written to a client.
    pub fn sent(&mut self) {
        if let Some(r) = self.ring.as_deref_mut()
            && r.pending.sent == 0
        {
            let now = (self.clock)();
            r.pending.sent = now;
            if r.pending.rx == 0 {
                r.pending.rx = now;
            }
        }
    }

    /// A client frame arrived: a `PresentSurface` (`surface`), or a
    /// `Commit`; `ready` when it has no acquire fence left to wait for.
    pub fn present(&mut self, surface: bool, ready: bool) {
        if let Some(r) = self.ring.as_deref_mut()
            && r.pending.sent != 0
            && r.pending.present == 0
        {
            let now = (self.clock)();
            r.pending.present = now;
            r.pending.surface = surface;
            if ready {
                r.pending.fence = now;
            }
        }
    }

    /// An acquire fence signalled.
    pub fn fence(&mut self) {
        if let Some(r) = self.ring.as_deref_mut()
            && r.pending.present != 0
            && r.pending.fence == 0
        {
            r.pending.fence = (self.clock)();
        }
    }

    /// A Surface frame latched onto `output`; its client has
    /// `unpresented` frames not yet `Presented`.
    pub fn latch(&mut self, output: u32, unpresented: usize) {
        let Some(r) = self.ring.as_deref_mut() else {
            return;
        };
        if r.pending.present == 0 {
            return;
        }
        r.pending.latch = (self.clock)();
        r.pending.unpresented = u32::try_from(unpresented).unwrap_or(u32::MAX);
        r.claim(output);
    }

    /// The server's own i2p stamp went to `output`, which is about to
    /// paint. Taken only when no client was sent the input (a cursor
    /// move over the desktop) or the client answered with a `Commit`;
    /// otherwise the input waits for the client's Surface frame to
    /// latch, so the stages name the frame that answered it, not
    /// whatever else was painted meanwhile.
    pub fn claim(&mut self, output: u32) {
        if let Some(r) = self.ring.as_deref_mut()
            && (r.pending.sent == 0 || (r.pending.present != 0 && !r.pending.surface))
        {
            r.claim(output);
        }
    }

    /// A cursor-only flip is being held for a client's answer.
    pub fn deferred(&mut self) {
        if let Some(r) = self.ring.as_deref_mut() {
            r.pending.deferred = true;
        }
    }

    /// `paint` started on `output`.
    pub fn paint(&mut self, output: u32) {
        if let Some(r) = self.ring.as_deref_mut() {
            let o = r.output(output);
            if !o.painting.is_empty() && o.painting.paint == 0 {
                o.painting.paint = (self.clock)();
            }
        }
    }

    /// `output`'s commit went in: the frame being prepared is in flight.
    pub fn commit(&mut self, output: u32) {
        if let Some(r) = self.ring.as_deref_mut() {
            let o = r.output(output);
            if o.painting.is_empty() {
                return;
            }
            let mut s = std::mem::take(&mut o.painting);
            s.commit = (self.clock)();
            o.in_flight.merge(&s);
        }
    }

    /// `output` flipped at `vblank`; `srv_input` is the stamp the
    /// server's i2p used (0 when it recorded none).
    pub fn flip(&mut self, output: u32, seq: u64, vblank: u64, srv_input: u64) {
        let Some(r) = self.ring.as_deref_mut() else {
            return;
        };
        let stages = std::mem::take(&mut r.output(output).in_flight);
        if stages.is_empty() && srv_input == 0 {
            return;
        }
        if r.records.len() == CAPACITY {
            r.records.pop_front();
        }
        r.records.push_back(FrameRecord {
            output,
            seq,
            stages,
            vblank,
            srv_input,
        });
        r.total += 1;
    }

    /// Records ever taken, and the retained ones oldest first.
    #[must_use]
    pub fn records(&self) -> (u64, Vec<FrameRecord>) {
        self.ring
            .as_deref()
            .map_or((0, Vec::new()), |r| (r.total, r.records.iter().copied().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static NOW: Cell<u64> = const { Cell::new(0) };
    }

    fn fake_now() -> u64 {
        NOW.with(Cell::get)
    }

    fn at(t: u64) {
        NOW.with(|n| n.set(t));
    }

    fn on() -> Timeline {
        let mut t = Timeline {
            ring: None,
            clock: fake_now,
        };
        t.set_enabled(true);
        t
    }

    #[test]
    fn off_records_nothing_and_allocates_nothing() {
        let mut t = Timeline::new();
        t.input(1);
        t.sent();
        t.present(true, true);
        t.latch(0, 1);
        t.commit(0);
        t.flip(0, 1, 7, 1);
        assert!(!t.enabled());
        assert!(t.ring.is_none());
        assert_eq!(t.records(), (0, Vec::new()));
    }

    #[test]
    fn a_surface_frame_carries_every_stage_in_order() {
        let mut t = on();
        at(110);
        t.sent();
        t.input(100);
        at(120);
        t.input(105);
        // Not the client's answer yet: the input stays pending.
        t.claim(3);
        at(130);
        t.present(true, false);
        at(140);
        t.fence();
        // Nor is another output's paint once it did.
        t.claim(3);
        at(150);
        t.latch(3, 2);
        at(160);
        t.paint(3);
        at(170);
        t.commit(3);
        t.flip(3, 9, 180, 105);
        let (total, recs) = t.records();
        assert_eq!(total, 1);
        assert_eq!(
            recs[0].columns(),
            [3, 9, 100, 105, 110, 110, 130, 140, 150, 160, 170, 180, 105, 0, 2]
        );
        assert_eq!(HEADER.split(' ').count(), recs[0].columns().len());
    }

    #[test]
    fn a_present_before_any_input_is_not_attributed() {
        let mut t = on();
        at(10);
        t.present(true, true);
        t.latch(0, 1);
        t.commit(0);
        t.flip(0, 1, 13, 0);
        assert_eq!(t.records().0, 0);
    }

    #[test]
    fn a_stale_input_is_abandoned() {
        let mut t = on();
        at(1);
        t.input(1);
        at(MAX_AGE_NS + 10);
        t.input(MAX_AGE_NS + 10);
        t.claim(0);
        t.commit(0);
        t.flip(0, 1, MAX_AGE_NS + 30, 0);
        assert_eq!(t.records().1[0].stages.input, MAX_AGE_NS + 10);
    }

    #[test]
    fn the_ring_wraps() {
        let mut t = on();
        for i in 1..=(CAPACITY as u64 + 5) {
            at(i);
            t.input(i);
            t.claim(0);
            t.commit(0);
            t.flip(0, i, i, i);
        }
        let (total, recs) = t.records();
        assert_eq!(total, CAPACITY as u64 + 5);
        assert_eq!(recs.len(), CAPACITY);
        assert_eq!(recs[0].seq, 6);
        t.clear();
        assert!(t.enabled());
        assert_eq!(t.records().0, 0);
        t.set_enabled(false);
        assert!(!t.enabled());
    }
}
