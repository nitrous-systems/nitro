//! Headless backend: memory buffers and a timerfd standing in for vblank.
//!
//! Guarantees (relied on by server tests):
//!
//! - Same contract as the DRM backend: `back_buffer` fails with
//!   `FlipPending` between `commit` and the matching `Flipped`;
//!   `read_front` returns the most recently committed buffer.
//! - Buffer strides are padded to 64 bytes so stride bugs surface.
//! - The timer is one-shot and armed only by `commit`: an idle fake makes
//!   no wakeups. Every output committed before the timer fires flips at
//!   the same tick, `period` after the first commit.
//! - `tick()` flips synchronously without the timer, for tests that don't
//!   want to poll. `Flipped.time` is `CLOCK_MONOTONIC` either way.
//! - `resume()` abandons any flip in flight — no output is flip-pending
//!   afterwards — and disarms the timer, so a paused-then-resumed fake is
//!   as idle as a fresh one.
//! - Damage passed to `commit` is recorded verbatim in `damage_log()`.
//! - Hotplug is simulated with `plug()` / `unplug()`: they queue an
//!   `Event::Hotplug` **and make the poll fd readable**, so an idle
//!   server wakes for it; the change takes effect on `rescan`.
//! - Planes: each output has a configurable inventory
//!   ([`FakeOutputSpec::planes`], default one non-scaling XRGB/ARGB
//!   primary). `test_layout` is a rule-based acceptor (the rules are
//!   listed on `FakeBackend::check_layout`), overridable with
//!   `set_test_hook`, and every question is recorded in `test_log()`.
//! - ARGB scanout: [`FakeOutputSpec::alpha`] (default `true`) is the
//!   capability; `scanout_alpha_on()` / `scanout_alpha_sets()` expose the
//!   state and the call count.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

use crate::drm::select::{ModeCandidate, ModeRequest, select_mode};
use crate::planes::{
    BufferId, ColorEncoding, ColorRange, Fourcc, MOD_LINEAR, PlaneAssignment, PlaneId, PlaneInfo,
    PlaneKind, PlaneSource, Verdict, Zpos, rotation,
};
use crate::{BYTES_PER_PIXEL, Backend, BufferMut, Error, Event, Image, OutputId, OutputInfo, Rect};

/// Description of one virtual plane. Build with [`FakePlaneSpec::primary`],
/// [`FakePlaneSpec::overlay`] or [`FakePlaneSpec::cursor`] and the
/// builder methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakePlaneSpec {
    /// Primary, overlay or cursor.
    pub kind: PlaneKind,
    /// Formats with their modifiers.
    pub formats: Vec<(Fourcc, Vec<u64>)>,
    /// Stacking; `None` for no `zpos` property.
    pub zpos: Option<Zpos>,
    /// Whether source and destination sizes may differ.
    pub scaling: bool,
    /// Allowed `dst / src` ratio per axis in percent, inclusive, when
    /// `scaling` (e.g. `(50, 800)`: down to half, up to 8×).
    pub scale_pct: (u32, u32),
    /// Supported `rotation` bits; `ROTATE_0` is always accepted.
    pub rotations: u32,
    /// `COLOR_ENCODING` values offered.
    pub color_encodings: Vec<ColorEncoding>,
    /// `COLOR_RANGE` values offered.
    pub color_ranges: Vec<ColorRange>,
    /// Has `IN_FENCE_FD`.
    pub in_fence: bool,
}

impl FakePlaneSpec {
    fn new(kind: PlaneKind) -> Self {
        Self {
            kind,
            formats: Vec::new(),
            zpos: None,
            scaling: false,
            scale_pct: (0, u32::MAX),
            rotations: rotation::ROTATE_0,
            color_encodings: Vec::new(),
            color_ranges: Vec::new(),
            in_fence: true,
        }
    }

    /// A primary plane with no formats yet.
    #[must_use]
    pub fn primary() -> Self {
        Self::new(PlaneKind::Primary)
    }

    /// An overlay plane with no formats yet.
    #[must_use]
    pub fn overlay() -> Self {
        Self::new(PlaneKind::Overlay)
    }

    /// A cursor plane with no formats yet.
    #[must_use]
    pub fn cursor() -> Self {
        Self::new(PlaneKind::Cursor)
    }

    /// The default inventory's plane: a primary taking linear
    /// `XRGB8888` and `ARGB8888`, no scaling.
    #[must_use]
    pub fn default_primary() -> Self {
        Self::primary().formats(&[Fourcc::XRGB8888, Fourcc::ARGB8888])
    }

    /// Add formats, each with the linear modifier only.
    #[must_use]
    pub fn formats(mut self, formats: &[Fourcc]) -> Self {
        for &f in formats {
            self = self.format_mods(f, &[MOD_LINEAR]);
        }
        self
    }

    /// Add one format with the given modifiers.
    #[must_use]
    pub fn format_mods(mut self, format: Fourcc, mods: &[u64]) -> Self {
        self.formats.push((format, mods.to_vec()));
        self
    }

    /// Give it a `zpos` property.
    #[must_use]
    pub fn zpos(mut self, current: u64, min: u64, max: u64, immutable: bool) -> Self {
        self.zpos = Some(Zpos {
            current,
            min,
            max,
            immutable,
        });
        self
    }

    /// Let it scale, by any ratio.
    #[must_use]
    pub fn scaling(mut self) -> Self {
        self.scaling = true;
        self
    }

    /// Let it scale, with `dst / src` between `min_pct` and `max_pct`
    /// percent on each axis.
    #[must_use]
    pub fn scale_limits(mut self, min_pct: u32, max_pct: u32) -> Self {
        self.scaling = true;
        self.scale_pct = (min_pct, max_pct);
        self
    }

    /// Supported `rotation` bits.
    #[must_use]
    pub fn rotations(mut self, mask: u32) -> Self {
        self.rotations = mask | rotation::ROTATE_0;
        self
    }

    /// `COLOR_ENCODING` / `COLOR_RANGE` values.
    #[must_use]
    pub fn color(mut self, encodings: &[ColorEncoding], ranges: &[ColorRange]) -> Self {
        self.color_encodings = encodings.to_vec();
        self.color_ranges = ranges.to_vec();
        self
    }

    /// Whether it has `IN_FENCE_FD` (default yes).
    #[must_use]
    pub fn in_fence(mut self, yes: bool) -> Self {
        self.in_fence = yes;
        self
    }

    fn info(&self, id: PlaneId, crtc_mask: u32) -> PlaneInfo {
        PlaneInfo {
            id,
            kind: self.kind,
            crtc_mask,
            formats: self.formats.clone(),
            zpos: self.zpos,
            rotations: self.rotations,
            color_encodings: self
                .color_encodings
                .iter()
                .map(|e| e.kernel_name().to_owned())
                .collect(),
            color_ranges: self
                .color_ranges
                .iter()
                .map(|r| r.kernel_name().to_owned())
                .collect(),
            blend_modes: Vec::new(),
            alpha: false,
            damage_clips: self.kind == PlaneKind::Primary,
            in_fence: self.in_fence,
            scaling: Some(self.scaling),
        }
    }
}

/// One `test_layout` question and its answer, for tests to assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRecord {
    /// The output asked about.
    pub output: OutputId,
    /// The planes in the layout, in the order given.
    pub planes: Vec<PlaneId>,
    /// The answer.
    pub verdict: Verdict,
}

/// A test's replacement for the rule-based acceptor.
pub type TestHook = Box<dyn FnMut(OutputId, &[PlaneAssignment<'_>]) -> Verdict>;

/// Description of one virtual output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeOutputSpec {
    /// Connector-style name, e.g. `Virtual-1`.
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh in millihertz; also the tick period.
    pub refresh_mhz: u32,
    /// Reported physical size.
    pub phys_mm: (u32, u32),
    /// The modes this virtual connector claims to offer.
    ///
    /// Empty means "only the one above", which is what every fake output
    /// was before `output.<c>.mode` existed and what a test that does not
    /// care still gets. A test that *does* care ([`FakeOutputSpec::modes`])
    /// hands over a table and the backend picks from it the same way the
    /// DRM backend picks from a connector's list — same [`select_mode`],
    /// same fallback, same warning — which is what makes a mode test
    /// runnable without a monitor.
    pub modes: Vec<ModeCandidate>,
    /// The planes this output's CRTC has. Empty means the default: one
    /// [`FakePlaneSpec::default_primary`].
    pub planes: Vec<FakePlaneSpec>,
    /// Whether the primary can scan out `ARGB8888`
    /// ([`Backend::scanout_alpha`]). Default `true`.
    pub alpha: bool,
}

impl FakeOutputSpec {
    /// A 60 Hz output of the given size with a 96-dpi physical size.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            name: String::from("Virtual-1"),
            width,
            height,
            refresh_mhz: 60_000,
            phys_mm: (width * 254 / 960, height * 254 / 960),
            modes: Vec::new(),
            planes: Vec::new(),
            alpha: true,
        }
    }

    /// Set whether the primary can scan out `ARGB8888`.
    #[must_use]
    pub fn alpha(mut self, capable: bool) -> Self {
        self.alpha = capable;
        self
    }

    /// Set the plane inventory (replacing the default single primary).
    #[must_use]
    pub fn planes(mut self, planes: Vec<FakePlaneSpec>) -> Self {
        self.planes = planes;
        self
    }

    /// Set the name.
    #[must_use]
    pub fn named(mut self, name: &str) -> Self {
        name.clone_into(&mut self.name);
        self
    }

    /// Set the refresh rate in millihertz.
    #[must_use]
    pub fn refresh_mhz(mut self, mhz: u32) -> Self {
        self.refresh_mhz = mhz;
        self
    }

    /// Offer a mode table, so `output.<c>.mode` has something to choose
    /// from.
    ///
    /// Each entry is `(width, height, refresh_mhz)`; the **first** is the
    /// preferred one, which is what the output comes up on when nothing is
    /// configured. Sizes other than the spec's are allowed and are what
    /// make a size change testable headlessly.
    #[must_use]
    pub fn modes(mut self, modes: &[(u32, u32, u32)]) -> Self {
        self.modes = modes
            .iter()
            .enumerate()
            .map(|(i, &(width, height, refresh_mhz))| ModeCandidate {
                width,
                height,
                refresh_mhz,
                preferred: i == 0,
                interlaced: false,
            })
            .collect();
        if let Some(m) = self.modes.first() {
            self.width = m.width;
            self.height = m.height;
            self.refresh_mhz = m.refresh_mhz;
        }
        self
    }

    /// The mode table this spec offers, which is its own single mode when
    /// it was never given one.
    fn mode_table(&self) -> Vec<ModeCandidate> {
        if self.modes.is_empty() {
            vec![ModeCandidate {
                width: self.width,
                height: self.height,
                refresh_mhz: self.refresh_mhz,
                preferred: true,
                interlaced: false,
            }]
        } else {
            self.modes.clone()
        }
    }
}

struct FakeOutput {
    info: OutputInfo,
    stride: u32,
    bufs: [Vec<u8>; 2],
    front: usize,
    pending: bool,
    sequence: u64,
    /// Committed at least once (the DRM backend's "lit").
    lit: bool,
    planes: Vec<(PlaneId, FakePlaneSpec)>,
    /// `ARGB8888` scanout is possible.
    alpha_capable: bool,
    /// What the primary scans out from the next commit: `XRGB8888`, or
    /// `ARGB8888` once `set_scanout_alpha(true)`.
    scanout_format: Fourcc,
    /// Successful `set_scanout_alpha` calls.
    alpha_sets: usize,
}

impl FakeOutput {
    fn crtc_mask(&self) -> u32 {
        1 << ((self.info.id.0 - 1) % 32)
    }

    fn new(id: OutputId, spec: &FakeOutputSpec, planes: Vec<(PlaneId, FakePlaneSpec)>) -> Self {
        let stride = (spec.width * BYTES_PER_PIXEL).div_ceil(64) * 64;
        let len = (stride * spec.height) as usize;
        Self {
            info: OutputInfo {
                id,
                name: spec.name.clone(),
                width: spec.width,
                height: spec.height,
                refresh_mhz: spec.refresh_mhz,
                phys_mm: spec.phys_mm,
                // Nothing on the fake backend is a modeline: it has no
                // EDID to bypass, so every mode it offers is its own.
                custom_mode: false,
            },
            stride,
            bufs: [vec![0; len], vec![0; len]],
            front: 0,
            pending: false,
            sequence: 0,
            lit: false,
            planes,
            alpha_capable: spec.alpha,
            scanout_format: Fourcc::XRGB8888,
            alpha_sets: 0,
        }
    }
}

/// The headless backend. See the [module docs](self) for guarantees.
pub struct FakeBackend {
    outputs: Vec<FakeOutput>,
    infos: Vec<OutputInfo>,
    next_id: u32,
    timer: OwnedFd,
    armed: bool,
    paused: bool,
    damage_log: Vec<(OutputId, Vec<Rect>)>,
    pending_specs: Vec<FakeOutputSpec>,
    pending_removals: Vec<OutputId>,
    hotplug_queued: bool,
    /// The spec each live output was built from, so a mode change can be
    /// resolved against its table without re-deriving it.
    specs: Vec<FakeOutputSpec>,
    /// The mode requests in force, by connector name.
    modes: HashMap<String, ModeRequest>,
    warnings: Vec<String>,
    next_plane: u32,
    next_buffer: u32,
    /// `(format, width, height)` by buffer id.
    buffers: HashMap<u32, (Fourcc, u32, u32)>,
    max_active_planes: Option<usize>,
    test_hook: Option<TestHook>,
    test_log: Vec<TestRecord>,
}

impl FakeBackend {
    /// Create with the given outputs (at least one is typical, zero is
    /// allowed).
    ///
    /// # Errors
    /// If the timerfd cannot be created.
    pub fn new(specs: &[FakeOutputSpec]) -> io::Result<Self> {
        let timer = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )?;
        let mut this = Self {
            outputs: Vec::new(),
            infos: Vec::new(),
            next_id: 1,
            timer,
            armed: false,
            paused: false,
            damage_log: Vec::new(),
            pending_specs: Vec::new(),
            pending_removals: Vec::new(),
            hotplug_queued: false,
            specs: Vec::new(),
            modes: HashMap::new(),
            warnings: Vec::new(),
            next_plane: 1,
            next_buffer: 1,
            buffers: HashMap::new(),
            max_active_planes: None,
            test_hook: None,
            test_log: Vec::new(),
        };
        for s in specs {
            this.add_output(s);
        }
        Ok(this)
    }

    /// One `width × height` output at 60 Hz.
    ///
    /// # Errors
    /// If the timerfd cannot be created.
    pub fn single(width: u32, height: u32) -> io::Result<Self> {
        Self::new(&[FakeOutputSpec::new(width, height)])
    }

    fn add_output(&mut self, spec: &FakeOutputSpec) {
        let id = OutputId(self.next_id);
        self.next_id += 1;
        let specs = if spec.planes.is_empty() {
            vec![FakePlaneSpec::default_primary()]
        } else {
            spec.planes.clone()
        };
        let planes = specs
            .into_iter()
            .map(|p| {
                let pid = PlaneId(self.next_plane);
                self.next_plane += 1;
                (pid, p)
            })
            .collect();
        self.outputs.push(FakeOutput::new(id, spec, planes));
        self.specs.push(spec.clone());
        self.apply_mode(self.outputs.len() - 1);
        self.infos.push(self.outputs.last().unwrap().info.clone());
    }

    /// Resolve output `i`'s configured mode against its table and resize
    /// it if the answer moved. Returns whether anything changed.
    ///
    /// An unmatched request is a warning naming what the connector *does*
    /// list, plus the default mode — the same shape the DRM backend has.
    /// Buffers are reallocated when the size changes, because on a real
    /// backend that is a fresh pair of dumb buffers too.
    ///
    /// **Where this backend cannot stand in for the real one.** A fake
    /// output is *edited*, so it keeps its [`OutputId`] across any mode
    /// change by construction — there is no code path here that could
    /// replace one. The DRM backend has to actively decide not to
    /// (`select::reconcile_one` returning `Retime` rather than
    /// `Replace`), and that decision is the whole of what makes
    /// `output.<c>.mode` a live key. So a test on this backend asserting
    /// the id survives a retime is **vacuous** and must not be read as
    /// evidence for the DRM path; `a_retime_keeps_the_output_and_a_resize_replaces_it`
    /// in `drm/select.rs` tests the rule itself.
    fn apply_mode(&mut self, i: usize) -> bool {
        let table = self.specs[i].mode_table();
        let name = self.specs[i].name.clone();
        let wanted = self.modes.get(&name).copied();
        if let Some(req) = wanted.as_ref() {
            if matches!(req, ModeRequest::Custom(_)) {
                self.warnings.push(format!(
                    "{name}: a modeline needs real hardware; the fake backend has no timings to set"
                ));
            } else if crate::drm::select::request_match(&table, req).is_none() {
                self.warnings.push(format!(
                    "{name}: no mode matches `{req}`; this connector lists {}. Using the default mode.",
                    crate::drm::select::describe_modes(&table)
                ));
            }
        }
        let Some(mi) = select_mode(&table, wanted.as_ref()) else {
            return false;
        };
        let m = table[mi];
        let o = &mut self.outputs[i];
        if (o.info.width, o.info.height, o.info.refresh_mhz) == (m.width, m.height, m.refresh_mhz) {
            return false;
        }
        let resized = (o.info.width, o.info.height) != (m.width, m.height);
        o.info.width = m.width;
        o.info.height = m.height;
        o.info.refresh_mhz = m.refresh_mhz;
        if resized {
            o.stride = (m.width * BYTES_PER_PIXEL).div_ceil(64) * 64;
            let len = (o.stride * m.height) as usize;
            o.bufs = [vec![0; len], vec![0; len]];
            o.front = 0;
            // A mode set is a modeset: nothing is in flight across it, the
            // same promise `resume` makes.
            o.pending = false;
        }
        true
    }

    /// Simulate plugging in a new output: queues `Event::Hotplug`; the
    /// output appears after `rescan`.
    pub fn plug(&mut self, spec: FakeOutputSpec) {
        self.pending_specs.push(spec);
        self.queue_hotplug();
    }

    /// Simulate unplugging: queues `Event::Hotplug`; the output vanishes
    /// after `rescan`.
    pub fn unplug(&mut self, output: OutputId) {
        self.pending_removals.push(output);
        self.queue_hotplug();
    }

    /// Queue a hotplug **and make the poll fd readable**, so a server
    /// sitting in `epoll_wait` actually wakes up for it.
    ///
    /// Without the second half the event is only delivered if something
    /// else happens to fire the timer, which on an idle desktop — or one
    /// with no output at all, which is exactly when a hotplug matters most
    /// — is never. The real backend gets this for free: its uevent socket
    /// becomes readable on its own.
    fn queue_hotplug(&mut self) {
        self.hotplug_queued = true;
        if let Err(e) = self.arm() {
            // The only failure mode is a broken timerfd, which would have
            // shown up long before this; there is nothing useful to do
            // with it here and the caller is a test.
            debug_assert!(false, "arming the fake timer for a hotplug: {e}");
        }
    }

    /// Every `(output, damage)` passed to `commit`, oldest first.
    #[must_use]
    pub fn damage_log(&self) -> &[(OutputId, Vec<Rect>)] {
        &self.damage_log
    }

    /// Forget the damage log.
    pub fn clear_damage_log(&mut self) {
        self.damage_log.clear();
    }

    /// Shortest tick period across outputs (the timer period).
    fn period(&self) -> Duration {
        let mhz = self
            .outputs
            .iter()
            .map(|o| o.info.refresh_mhz)
            .filter(|&m| m > 0)
            .min()
            .unwrap_or(60_000);
        Duration::from_nanos(1_000_000_000_000 / u64::from(mhz))
    }

    fn arm(&mut self) -> Result<(), Error> {
        if self.armed {
            return Ok(());
        }
        let p = self.period();
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: p.as_secs().cast_signed(),
                tv_nsec: i64::from(p.subsec_nanos()),
            },
        };
        timerfd_settime(&self.timer, TimerfdTimerFlags::empty(), &spec).map_err(|e| Error::Io {
            op: "arm fake vblank timer",
            source: e.into(),
        })?;
        self.armed = true;
        Ok(())
    }

    /// Stop the vblank timer and swallow an expiration it has already
    /// queued, so a disarmed fake really makes no wakeups.
    fn disarm(&mut self) -> Result<(), Error> {
        let spec = Itimerspec {
            it_interval: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        };
        timerfd_settime(&self.timer, TimerfdTimerFlags::empty(), &spec).map_err(|e| Error::Io {
            op: "disarm fake vblank timer",
            source: e.into(),
        })?;
        // Disarming does not clear an expiration that already happened, and
        // the fd would stay readable and wake every poll of an idle backend.
        // The fd is non-blocking, so this read just drains that count.
        let mut buf = [0u8; 8];
        match rustix::io::read(&self.timer, &mut buf[..]) {
            Ok(_) | Err(rustix::io::Errno::AGAIN) => {}
            Err(e) => {
                return Err(Error::Io {
                    op: "read fake vblank timer",
                    source: e.into(),
                });
            }
        }
        self.armed = false;
        Ok(())
    }

    /// Complete every in-flight commit now, as if a vblank happened,
    /// appending `Flipped` events. Also delivers a queued `Hotplug`.
    pub fn tick(&mut self, events: &mut Vec<Event>) {
        let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let time = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
        for o in &mut self.outputs {
            if o.pending {
                o.pending = false;
                o.sequence += 1;
                events.push(Event::Flipped {
                    output: o.info.id,
                    sequence: o.sequence,
                    time,
                });
            }
        }
        self.armed = false;
        if self.hotplug_queued {
            self.hotplug_queued = false;
            events.push(Event::Hotplug);
        }
    }

    /// Write the front buffer of `output` as a binary PPM.
    ///
    /// # Errors
    /// Unknown output or I/O failure.
    pub fn write_ppm(
        &mut self,
        output: OutputId,
        path: impl AsRef<std::path::Path>,
    ) -> io::Result<()> {
        let img = self
            .read_front(output)
            .map_err(|e| io::Error::new(io::ErrorKind::NotFound, e.to_string()))?;
        img.write_ppm(path)
    }

    /// A layout limit standing in for shared scalers and memory
    /// bandwidth: a layout with more than `n` planes is rejected with
    /// `ENOSPC`. `None` (the default) is no limit.
    pub fn set_max_active_planes(&mut self, n: Option<usize>) {
        self.max_active_planes = n;
    }

    /// Replace the rule-based acceptor. The hook is asked only after the
    /// ids have been checked (unknown planes and buffers are still `Err`).
    pub fn set_test_hook(&mut self, hook: Option<TestHook>) {
        self.test_hook = hook;
    }

    /// Every `test_layout` question that got a verdict, oldest first.
    #[must_use]
    pub fn test_log(&self) -> &[TestRecord] {
        &self.test_log
    }

    /// Forget the test log.
    pub fn clear_test_log(&mut self) {
        self.test_log.clear();
    }

    /// The `(format, width, height)` of each live scanout buffer.
    #[must_use]
    pub fn buffer(&self, id: BufferId) -> Option<(Fourcc, u32, u32)> {
        self.buffers.get(&id.0).copied()
    }

    /// Whether `output` is set to scan out `ARGB8888` (`false` for an
    /// unknown id).
    #[must_use]
    pub fn scanout_alpha_on(&self, output: OutputId) -> bool {
        self.outputs
            .iter()
            .any(|o| o.info.id == output && o.scanout_format == Fourcc::ARGB8888)
    }

    /// How many `set_scanout_alpha` calls on `output` succeeded, including
    /// ones that did not change the state (`0` for an unknown id).
    #[must_use]
    pub fn scanout_alpha_sets(&self, output: OutputId) -> usize {
        self.outputs
            .iter()
            .find(|o| o.info.id == output)
            .map_or(0, |o| o.alpha_sets)
    }

    /// The rules of the fake's `TEST_ONLY`. `EINVAL` when:
    ///
    /// - a plane appears twice;
    /// - the source's format is not in the plane's list with the linear
    ///   modifier (the output front is linear `XRGB8888`, buffers are
    ///   linear);
    /// - `src` is empty or leaves the buffer, or `dst` is empty or misses
    ///   the output entirely;
    /// - the plane must scale and cannot, or the ratio is outside its
    ///   `scale_pct`;
    /// - a requested `zpos` is outside the range or differs from an
    ///   immutable one, or two planes end up with the same mutable zpos;
    /// - a rotation, `COLOR_ENCODING`, `COLOR_RANGE` or `IN_FENCE_FD` the
    ///   plane does not offer is requested.
    ///
    /// `ENOSPC` when the layout has more planes than
    /// [`FakeBackend::set_max_active_planes`] allows.
    fn check_layout(
        o: &FakeOutput,
        buffers: &HashMap<u32, (Fourcc, u32, u32)>,
        max_active: Option<usize>,
        layout: &[(&FakePlaneSpec, &PlaneAssignment<'_>)],
    ) -> Verdict {
        let no = Verdict::einval();
        if let Some(n) = max_active
            && layout.len() > n
        {
            return Verdict::Rejected(rustix::io::Errno::NOSPC.raw_os_error());
        }
        let mut zs: Vec<(u64, bool)> = Vec::new();
        for (i, (spec, a)) in layout.iter().enumerate() {
            if layout[..i].iter().any(|(_, b)| b.plane == a.plane) {
                return no;
            }
            let (format, bw, bh) = match a.source {
                PlaneSource::OutputFront => (Fourcc::XRGB8888, o.info.width, o.info.height),
                PlaneSource::Buffer(id) => buffers[&id.0],
            };
            if !spec
                .formats
                .iter()
                .any(|(f, m)| *f == format && m.contains(&MOD_LINEAR))
            {
                return no;
            }
            let (sx, sy, sw, sh) = (
                u64::from(a.src.x),
                u64::from(a.src.y),
                u64::from(a.src.w),
                u64::from(a.src.h),
            );
            if sw == 0 || sh == 0 || sx + sw > u64::from(bw) << 16 || sy + sh > u64::from(bh) << 16
            {
                return no;
            }
            if a.dst.clipped_to(o.info.width, o.info.height).is_none() {
                return no;
            }
            if a.scales() {
                if !spec.scaling {
                    return no;
                }
                let pct = |d: u32, s: u64| u64::from(d) * 100 * 65536 / s;
                let (lo, hi) = (u64::from(spec.scale_pct.0), u64::from(spec.scale_pct.1));
                for r in [pct(a.dst.w, sw), pct(a.dst.h, sh)] {
                    if r < lo || r > hi {
                        return no;
                    }
                }
            }
            match (a.zpos, spec.zpos) {
                (Some(_), None) => return no,
                (Some(z), Some(zp)) if z < zp.min || z > zp.max => return no,
                (Some(z), Some(zp)) if zp.immutable && z != zp.current => return no,
                _ => {}
            }
            if let Some(zp) = spec.zpos {
                let z = a.zpos.unwrap_or(zp.current);
                if zs
                    .iter()
                    .any(|&(other, imm)| other == z && !(imm && zp.immutable))
                {
                    return no;
                }
                zs.push((z, zp.immutable));
            }
            if let Some(r) = a.rotation
                && r & !(spec.rotations | rotation::ROTATE_0) != 0
            {
                return no;
            }
            if a.color_encoding
                .is_some_and(|e| !spec.color_encodings.contains(&e))
                || a.color_range
                    .is_some_and(|r| !spec.color_ranges.contains(&r))
                || (a.in_fence.is_some() && !spec.in_fence)
            {
                return no;
            }
        }
        Verdict::Accepted
    }

    fn output_mut(&mut self, id: OutputId) -> Result<&mut FakeOutput, Error> {
        self.outputs
            .iter_mut()
            .find(|o| o.info.id == id)
            .ok_or(Error::NoSuchOutput(id))
    }
}

impl Backend for FakeBackend {
    fn simulate_plug(&mut self, width: u32, height: u32) -> bool {
        // Distinct connector names, so a test can ask for a scale
        // override by name and so the log is readable with two outputs.
        let name = format!("Virtual-{}", self.next_id);
        self.plug(FakeOutputSpec::new(width, height).named(&name));
        true
    }

    fn simulate_unplug(&mut self) -> bool {
        let Some(last) = self.outputs.last().map(|o| o.info.id) else {
            return false;
        };
        self.unplug(last);
        true
    }

    fn outputs(&self) -> &[OutputInfo] {
        &self.infos
    }

    fn back_buffer(&mut self, output: OutputId) -> Result<BufferMut<'_>, Error> {
        let o = self.output_mut(output)?;
        if o.pending {
            return Err(Error::FlipPending(output));
        }
        let back = 1 - o.front;
        Ok(BufferMut {
            width: o.info.width,
            height: o.info.height,
            stride: o.stride,
            data: &mut o.bufs[back],
        })
    }

    fn commit(&mut self, output: OutputId, damage: &[Rect]) -> Result<(), Error> {
        if self.paused {
            return Err(Error::Paused);
        }
        let o = self.output_mut(output)?;
        if o.pending {
            return Err(Error::FlipPending(output));
        }
        o.front = 1 - o.front;
        o.pending = true;
        o.lit = true;
        self.damage_log.push((output, damage.to_vec()));
        self.arm()
    }

    fn flip_pending(&self, output: OutputId) -> bool {
        self.outputs
            .iter()
            .any(|o| o.info.id == output && o.pending)
    }

    fn poll_fds(&self) -> Vec<BorrowedFd<'_>> {
        vec![self.timer.as_fd()]
    }

    fn dispatch(&mut self, events: &mut Vec<Event>) -> Result<(), Error> {
        let mut buf = [0u8; 8];
        match rustix::io::read(&self.timer, &mut buf[..]) {
            Ok(_) => self.tick(events),
            Err(rustix::io::Errno::AGAIN) => {
                // Not fired yet; still deliver a queued hotplug.
                if self.hotplug_queued {
                    self.hotplug_queued = false;
                    events.push(Event::Hotplug);
                }
            }
            Err(e) => {
                return Err(Error::Io {
                    op: "read fake vblank timer",
                    source: e.into(),
                });
            }
        }
        Ok(())
    }

    fn rescan(&mut self) -> Result<bool, Error> {
        let mut changed = false;
        for id in std::mem::take(&mut self.pending_removals) {
            if let Some(i) = self.outputs.iter().position(|o| o.info.id == id) {
                self.outputs.remove(i);
                self.specs.remove(i);
                changed = true;
            }
        }
        for spec in std::mem::take(&mut self.pending_specs) {
            self.add_output(&spec);
            changed = true;
        }
        if changed {
            self.infos = self.outputs.iter().map(|o| o.info.clone()).collect();
        }
        Ok(changed)
    }

    fn set_modes(&mut self, modes: &HashMap<String, ModeRequest>) -> Result<bool, Error> {
        if *modes == self.modes {
            return Ok(false);
        }
        self.modes.clone_from(modes);
        let mut changed = false;
        for i in 0..self.outputs.len() {
            changed |= self.apply_mode(i);
        }
        if changed {
            self.infos = self.outputs.iter().map(|o| o.info.clone()).collect();
        }
        Ok(changed)
    }

    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    fn available_modes(&self, output: OutputId) -> Vec<ModeCandidate> {
        self.outputs
            .iter()
            .position(|o| o.info.id == output)
            .map(|i| self.specs[i].mode_table())
            .unwrap_or_default()
    }

    fn planes(&self, output: OutputId) -> Vec<PlaneInfo> {
        let Some(o) = self.outputs.iter().find(|o| o.info.id == output) else {
            return Vec::new();
        };
        let mask = o.crtc_mask();
        let mut v: Vec<PlaneInfo> = o.planes.iter().map(|(id, s)| s.info(*id, mask)).collect();
        v.sort_by_key(|p| {
            let k = match p.kind {
                PlaneKind::Primary => 0,
                PlaneKind::Overlay => 1,
                PlaneKind::Cursor => 2,
            };
            (k, p.id)
        });
        v
    }

    fn alloc_buffer(&mut self, format: Fourcc, width: u32, height: u32) -> Result<BufferId, Error> {
        if width == 0 || height == 0 {
            return Err(Error::Unsupported("empty scanout buffers"));
        }
        if format == Fourcc::NV12 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
            return Err(Error::Unsupported("odd NV12 sizes"));
        }
        let id = self.next_buffer;
        self.next_buffer += 1;
        self.buffers.insert(id, (format, width, height));
        Ok(BufferId(id))
    }

    fn free_buffer(&mut self, id: BufferId) {
        self.buffers.remove(&id.0);
    }

    fn test_layout(
        &mut self,
        output: OutputId,
        layout: &[PlaneAssignment<'_>],
    ) -> Result<Verdict, Error> {
        if self.paused {
            return Err(Error::Paused);
        }
        let o = self
            .outputs
            .iter()
            .find(|o| o.info.id == output)
            .ok_or(Error::NoSuchOutput(output))?;
        if !o.lit {
            return Err(Error::NotLit(output));
        }
        let mut pairs = Vec::with_capacity(layout.len());
        for a in layout {
            let spec = o
                .planes
                .iter()
                .find(|(id, _)| *id == a.plane)
                .map(|(_, s)| s)
                .ok_or(Error::NoSuchObject("plane on this output", a.plane.0))?;
            if let PlaneSource::Buffer(b) = a.source
                && !self.buffers.contains_key(&b.0)
            {
                return Err(Error::NoSuchObject("buffer", b.0));
            }
            pairs.push((spec, a));
        }
        let verdict = if let Some(hook) = self.test_hook.as_mut() {
            hook(output, layout)
        } else {
            Self::check_layout(o, &self.buffers, self.max_active_planes, &pairs)
        };
        self.test_log.push(TestRecord {
            output,
            planes: layout.iter().map(|a| a.plane).collect(),
            verdict,
        });
        Ok(verdict)
    }

    fn scanout_alpha(&self, output: OutputId) -> bool {
        self.outputs
            .iter()
            .any(|o| o.info.id == output && o.alpha_capable)
    }

    fn set_scanout_alpha(&mut self, output: OutputId, on: bool) -> Result<(), Error> {
        let o = self.output_mut(output)?;
        if on && !o.alpha_capable {
            return Err(Error::Unsupported("ARGB8888 scanout"));
        }
        o.scanout_format = if on {
            Fourcc::ARGB8888
        } else {
            Fourcc::XRGB8888
        };
        o.alpha_sets += 1;
        Ok(())
    }

    fn pause(&mut self) {
        self.paused = true;
        // Like the DRM backend, pausing keeps every bit of state: a flip in
        // flight stays pending and `tick` or `dispatch` may still retire it.
        // Nothing has to be undone here, because `resume` clears the pending
        // flag unconditionally.
    }

    fn resume(&mut self) -> Result<(), Error> {
        self.paused = false;
        // Abandon any flip that was in flight, matching the DRM backend's
        // contract: after a resume no output has a pending flip and the
        // caller repaints fully. `commit` already moved `front` to the
        // committed buffer, so clearing the flag without swapping leaves
        // `front` as what is being scanned out; the back buffer's contents
        // are then stale, which the full repaint covers.
        for o in &mut self.outputs {
            o.pending = false;
        }
        // With nothing in flight there is no flip left to report, so the
        // timer must go too: an idle paused-then-resumed fake makes no
        // wakeups, as the module docs promise.
        let r = self.disarm();
        // The same post-condition the DRM backend asserts, checked here
        // too so a test backend cannot drift from the contract it stands
        // in for.
        debug_assert!(
            !self.outputs.iter().any(|o| o.pending),
            "resume must leave no output flip-pending"
        );
        r
    }

    fn read_front(&mut self, output: OutputId) -> Result<Image, Error> {
        let o = self.output_mut(output)?;
        let (w, h) = (o.info.width, o.info.height);
        let row = (w * BYTES_PER_PIXEL) as usize;
        let src = &o.bufs[o.front];
        let mut data = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            let start = y * o.stride as usize;
            data.extend_from_slice(&src[start..start + row]);
        }
        Ok(Image {
            width: w,
            height: h,
            stride: w * BYTES_PER_PIXEL,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> (FakeBackend, OutputId) {
        let b = FakeBackend::single(8, 4).unwrap();
        let id = b.outputs()[0].id;
        (b, id)
    }

    /// A fake connector with the test box's mode shape: a preferred
    /// 1080p60 plus a 120 and a smaller size, so a request can be right,
    /// wrong, or about a size change.
    fn multi_mode() -> FakeBackend {
        FakeBackend::new(&[FakeOutputSpec::new(1920, 1080).named("HDMI-A-1").modes(&[
            (1920, 1080, 60_000),
            (1920, 1080, 120_000),
            (1280, 720, 240_000),
        ])])
        .unwrap()
    }

    fn req(text: &str) -> ModeRequest {
        ModeRequest::parse(text).unwrap()
    }

    fn want(name: &str, spec: &str) -> HashMap<String, ModeRequest> {
        HashMap::from([(name.to_owned(), req(spec))])
    }

    #[test]
    fn a_mode_request_retimes_the_output() {
        let mut b = multi_mode();
        // Preferred first, which is the 60.
        assert_eq!(b.outputs()[0].refresh_mhz, 60_000);
        // Deliberately *not* asserting the `OutputId` survives: on this
        // backend an output is edited and could not get a new id however
        // the code were broken, so such an assertion would be vacuous.
        // The rule that matters is `select::reconcile_one`, tested there.
        assert!(b.set_modes(&want("HDMI-A-1", "1920x1080@120")).unwrap());
        assert_eq!(b.outputs()[0].refresh_mhz, 120_000);
        assert_eq!((b.outputs()[0].width, b.outputs()[0].height), (1920, 1080));
        assert!(b.take_warnings().is_empty());
        // Idempotent: the same map is not a modeset.
        assert!(!b.set_modes(&want("HDMI-A-1", "1920x1080@120")).unwrap());
        // And so is a *different* request that resolves to the same mode:
        // the map changed, the hardware did not.
        assert!(!b.set_modes(&want("HDMI-A-1", "fastest")).unwrap());
        assert_eq!(b.outputs()[0].refresh_mhz, 120_000);
    }

    #[test]
    fn a_size_change_reallocates_the_buffers() {
        let mut b = multi_mode();
        let id = b.outputs()[0].id;
        assert!(b.set_modes(&want("HDMI-A-1", "1280x720")).unwrap());
        let info = b.outputs()[0].clone();
        assert_eq!(
            (info.width, info.height, info.refresh_mhz),
            (1280, 720, 240_000)
        );
        let buf = b.back_buffer(id).unwrap();
        assert_eq!((buf.width, buf.height), (1280, 720));
        assert_eq!(buf.data.len() as u32, buf.stride * 720);
        // The shortest period across outputs is the tick period, so a
        // 240 Hz output really does tick four times as often as a 60.
        assert_eq!(
            b.period(),
            Duration::from_nanos(1_000_000_000_000 / 240_000)
        );
    }

    #[test]
    fn an_unmatched_request_warns_and_keeps_the_default() {
        let mut b = multi_mode();
        assert!(!b.set_modes(&want("HDMI-A-1", "2560x1440@144")).unwrap());
        assert_eq!(b.outputs()[0].refresh_mhz, 60_000);
        let warnings = b.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("no mode matches `2560x1440@144`"),
            "{warnings:?}"
        );
        // The evidence a user needs to write a line that works.
        assert!(
            warnings[0].contains("1920x1080@60, 1920x1080@120, 1280x720@240"),
            "{warnings:?}"
        );
        // Drained, so a reload that did not change the line is silent.
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_connector_the_map_does_not_name_is_left_alone() {
        let mut b = multi_mode();
        assert!(!b.set_modes(&want("DP-1", "1280x720")).unwrap());
        assert_eq!(
            (b.outputs()[0].width, b.outputs()[0].refresh_mhz),
            (1920, 60_000)
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn reports_output_with_padded_stride() {
        let (mut b, id) = fake();
        let info = &b.outputs()[0];
        assert_eq!((info.width, info.height, info.refresh_mhz), (8, 4, 60_000));
        assert_eq!(info.name, "Virtual-1");
        let buf = b.back_buffer(id).unwrap();
        assert_eq!(buf.stride, 64);
        assert_eq!(buf.data.len(), 64 * 4);
    }

    #[test]
    fn commit_flip_ordering_and_flip_pending() {
        let (mut b, id) = fake();
        let mut ev = Vec::new();
        assert!(!b.flip_pending(id));
        b.commit(id, &[]).unwrap();
        assert!(b.flip_pending(id));
        assert!(matches!(b.back_buffer(id), Err(Error::FlipPending(_))));
        assert!(matches!(b.commit(id, &[]), Err(Error::FlipPending(_))));
        // Timer has not fired: dispatch yields nothing.
        b.dispatch(&mut ev).unwrap();
        assert!(ev.is_empty());
        b.tick(&mut ev);
        assert_eq!(ev.len(), 1);
        assert!(matches!(
            ev[0],
            Event::Flipped {
                output,
                sequence: 1,
                ..
            } if output == id
        ));
        assert!(!b.flip_pending(id));
        b.commit(id, &[]).unwrap();
        ev.clear();
        b.tick(&mut ev);
        assert!(matches!(ev[0], Event::Flipped { sequence: 2, .. }));
    }

    #[test]
    fn timer_fires_and_dispatch_flips() {
        let (mut b, id) = fake();
        b.commit(id, &[]).unwrap();
        let fds = b.poll_fds();
        assert_eq!(fds.len(), 1);
        let mut pfd = [rustix::event::PollFd::new(
            &fds[0],
            rustix::event::PollFlags::IN,
        )];
        let n = rustix::event::poll(
            &mut pfd,
            Some(&Timespec {
                tv_sec: 1,
                tv_nsec: 0,
            }),
        )
        .unwrap();
        assert_eq!(n, 1);
        let mut ev = Vec::new();
        b.dispatch(&mut ev).unwrap();
        assert!(matches!(ev.as_slice(), [Event::Flipped { .. }]));
        assert!(!b.flip_pending(id));
    }

    #[test]
    fn read_front_sees_committed_writes_only() {
        let (mut b, id) = fake();
        let mut ev = Vec::new();
        {
            let mut buf = b.back_buffer(id).unwrap();
            buf.fill_rect(Rect::new(0, 0, 8, 4), 0x0011_2233);
        }
        // Not committed: front is still black.
        assert_eq!(b.read_front(id).unwrap().pixel(3, 2), 0);
        b.commit(id, &[Rect::new(0, 0, 8, 4)]).unwrap();
        // Committed (flip pending): front is the new buffer.
        let img = b.read_front(id).unwrap();
        assert_eq!(img.pixel(3, 2), 0x0011_2233);
        assert_eq!(img.stride, 32);
        assert_eq!(img.data.len(), 32 * 4);
        b.tick(&mut ev);
        // The back buffer is now the old front, still black.
        {
            let mut buf = b.back_buffer(id).unwrap();
            assert_eq!(buf.data[0..4], [0, 0, 0, 0]);
            buf.fill_rect(Rect::new(1, 1, 1, 1), 0x00FF_0000);
        }
        b.commit(id, &[Rect::new(1, 1, 1, 1)]).unwrap();
        let img = b.read_front(id).unwrap();
        assert_eq!(img.pixel(1, 1), 0x00FF_0000);
        assert_eq!(img.pixel(3, 2), 0);
    }

    #[test]
    fn damage_is_recorded() {
        let (mut b, id) = fake();
        let d = [Rect::new(1, 2, 3, 4), Rect::new(0, 0, 8, 1)];
        b.commit(id, &d).unwrap();
        assert_eq!(b.damage_log(), &[(id, d.to_vec())]);
        b.clear_damage_log();
        assert!(b.damage_log().is_empty());
    }

    #[test]
    fn pause_refuses_commits_and_hotplug_rescans() {
        let (mut b, id) = fake();
        b.pause();
        assert!(matches!(b.commit(id, &[]), Err(Error::Paused)));
        b.resume().unwrap();
        b.commit(id, &[]).unwrap();

        let mut ev = Vec::new();
        b.plug(FakeOutputSpec::new(4, 4).named("Virtual-2"));
        b.dispatch(&mut ev).unwrap();
        assert!(ev.contains(&Event::Hotplug));
        assert_eq!(b.outputs().len(), 1);
        assert!(b.rescan().unwrap());
        assert_eq!(b.outputs().len(), 2);
        assert_eq!(b.outputs()[1].name, "Virtual-2");
        assert!(!b.rescan().unwrap());
        b.unplug(id);
        assert!(b.rescan().unwrap());
        assert_eq!(b.outputs().len(), 1);
        assert!(matches!(b.commit(id, &[]), Err(Error::NoSuchOutput(_))));
    }

    #[test]
    fn resume_abandons_pending_flip() {
        let (mut b, id) = fake();
        b.commit(id, &[]).unwrap();
        assert!(b.flip_pending(id));
        b.pause();
        // Pausing on its own leaves the flip in flight, as documented.
        assert!(b.flip_pending(id));
        b.resume().unwrap();
        assert!(!b.flip_pending(id));
        // The buffers are usable again straight away, without waiting for a
        // flip completion that is never going to come.
        assert!(b.back_buffer(id).is_ok());
        b.commit(id, &[]).unwrap();
    }

    #[test]
    fn resume_disarms_the_timer() {
        let (mut b, id) = fake();
        b.commit(id, &[]).unwrap();
        b.pause();
        b.resume().unwrap();
        // The abandoned flip must not leave a wakeup behind: poll the timer
        // for well over one 60 Hz period and expect no readiness.
        let fds = b.poll_fds();
        let mut pfd = [rustix::event::PollFd::new(
            &fds[0],
            rustix::event::PollFlags::IN,
        )];
        let n = rustix::event::poll(
            &mut pfd,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 50_000_000,
            }),
        )
        .unwrap();
        assert_eq!(n, 0);
        let mut ev = Vec::new();
        b.dispatch(&mut ev).unwrap();
        assert!(ev.is_empty());
        // A later commit re-arms it normally.
        b.commit(id, &[]).unwrap();
        assert!(b.flip_pending(id));
        b.tick(&mut ev);
        assert!(matches!(ev.as_slice(), [Event::Flipped { .. }]));
    }

    #[test]
    fn ppm_round_trip() {
        let (mut b, id) = fake();
        {
            let mut buf = b.back_buffer(id).unwrap();
            buf.fill_rect(Rect::new(0, 0, 1, 1), 0x0012_3456);
        }
        b.commit(id, &[]).unwrap();
        let dir = std::env::temp_dir().join(format!("nitro-kms-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.ppm");
        b.write_ppm(id, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"P6\n8 4\n255\n"));
        assert_eq!(&bytes[11..14], &[0x12, 0x34, 0x56]);
        assert_eq!(bytes.len(), 11 + 8 * 4 * 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    // -- planes ----------------------------------------------------------------

    use crate::planes::SrcRect;

    /// A 1920×1080 output with an HSW-like inventory: XRGB primary, a
    /// scaling YUYV/NV12 overlay with fixed zpos above it, a cursor.
    fn with_planes() -> (FakeBackend, OutputId, Vec<PlaneInfo>) {
        let spec = FakeOutputSpec::new(1920, 1080).planes(vec![
            FakePlaneSpec::default_primary().zpos(0, 0, 0, true),
            FakePlaneSpec::overlay()
                .formats(&[Fourcc::XRGB8888, Fourcc::YUYV, Fourcc::NV12])
                .zpos(1, 1, 1, true)
                .scale_limits(50, 800)
                .color(
                    &[ColorEncoding::Bt601, ColorEncoding::Bt709],
                    &[ColorRange::Limited],
                ),
            FakePlaneSpec::cursor()
                .formats(&[Fourcc::ARGB8888])
                .zpos(2, 2, 2, true),
        ]);
        let mut b = FakeBackend::new(&[spec]).unwrap();
        let id = b.outputs()[0].id;
        b.commit(id, &[]).unwrap();
        let planes = b.planes(id);
        (b, id, planes)
    }

    fn full(plane: PlaneId, src: PlaneSource, w: u32, h: u32) -> PlaneAssignment<'static> {
        PlaneAssignment::new(plane, src, SrcRect::whole(w, h), Rect::new(0, 0, w, h))
    }

    #[test]
    fn default_inventory_is_one_linear_primary() {
        let (mut b, id) = fake();
        let p = b.planes(id);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, PlaneKind::Primary);
        assert!(p[0].supports(Fourcc::XRGB8888, MOD_LINEAR));
        assert!(!p[0].supports(Fourcc::NV12, MOD_LINEAR));
        assert_eq!(p[0].scaling, Some(false));
        assert!(b.planes(OutputId(99)).is_empty());
        // Not lit before its first commit, like the DRM backend.
        let a = full(p[0].id, PlaneSource::OutputFront, 8, 4);
        assert!(matches!(b.test_layout(id, &[a]), Err(Error::NotLit(_))));
        b.commit(id, &[]).unwrap();
        assert_eq!(b.test_layout(id, &[a]).unwrap(), Verdict::Accepted);
    }

    #[test]
    fn plane_ids_are_unique_across_outputs() {
        let b = FakeBackend::new(&[FakeOutputSpec::new(4, 4), FakeOutputSpec::new(4, 4)]).unwrap();
        let a = b.planes(b.outputs()[0].id);
        let c = b.planes(b.outputs()[1].id);
        assert_ne!(a[0].id, c[0].id);
        assert_ne!(a[0].crtc_mask, c[0].crtc_mask);
    }

    #[test]
    fn nv12_overlay_accepted_by_format_and_rejected_on_the_primary() {
        let (mut b, id, p) = with_planes();
        let (primary, overlay) = (p[0].id, p[1].id);
        let nv12 = b.alloc_buffer(Fourcc::NV12, 1920, 1080).unwrap();
        let ok = [
            full(primary, PlaneSource::OutputFront, 1920, 1080),
            PlaneAssignment {
                color_encoding: Some(ColorEncoding::Bt709),
                color_range: Some(ColorRange::Limited),
                ..full(overlay, PlaneSource::Buffer(nv12), 1920, 1080)
            },
        ];
        assert_eq!(b.test_layout(id, &ok).unwrap(), Verdict::Accepted);
        let bad = [full(primary, PlaneSource::Buffer(nv12), 1920, 1080)];
        assert_eq!(b.test_layout(id, &bad).unwrap(), Verdict::einval());
        // An encoding the plane does not offer.
        let bt2020 = [PlaneAssignment {
            color_encoding: Some(ColorEncoding::Bt2020),
            ..full(overlay, PlaneSource::Buffer(nv12), 1920, 1080)
        }];
        assert_eq!(b.test_layout(id, &bt2020).unwrap(), Verdict::einval());
        let log = b.test_log();
        assert_eq!(log.len(), 3);
        assert_eq!(log[0].planes, vec![primary, overlay]);
        assert!(log[0].verdict.accepted());
    }

    #[test]
    fn scaling_follows_the_plane() {
        let (mut b, id, p) = with_planes();
        let buf = b.alloc_buffer(Fourcc::XRGB8888, 960, 540).unwrap();
        let scaled = |plane| {
            PlaneAssignment::new(
                plane,
                PlaneSource::Buffer(buf),
                SrcRect::whole(960, 540),
                Rect::new(0, 0, 1920, 1080),
            )
        };
        assert_eq!(
            b.test_layout(id, &[scaled(p[0].id)]).unwrap(),
            Verdict::einval()
        );
        assert_eq!(
            b.test_layout(id, &[scaled(p[1].id)]).unwrap(),
            Verdict::Accepted
        );
        // Beyond 8× is outside the overlay's limits.
        let tiny = b.alloc_buffer(Fourcc::XRGB8888, 100, 100).unwrap();
        let too_far = PlaneAssignment::new(
            p[1].id,
            PlaneSource::Buffer(tiny),
            SrcRect::whole(100, 100),
            Rect::new(0, 0, 1000, 1000),
        );
        assert_eq!(b.test_layout(id, &[too_far]).unwrap(), Verdict::einval());
        // A source rectangle outside the buffer.
        let oob = PlaneAssignment::new(
            p[1].id,
            PlaneSource::Buffer(tiny),
            SrcRect::pixels(50, 0, 100, 100),
            Rect::new(0, 0, 100, 100),
        );
        assert_eq!(b.test_layout(id, &[oob]).unwrap(), Verdict::einval());
    }

    #[test]
    fn zpos_rules() {
        let (mut b, id, p) = with_planes();
        let front = |pl| full(pl, PlaneSource::OutputFront, 1920, 1080);
        // Immutable zpos: its own value is fine, moving it is not.
        assert!(
            b.test_layout(id, &[front(p[1].id).with_zpos(1)])
                .unwrap()
                .accepted()
        );
        assert_eq!(
            b.test_layout(id, &[front(p[1].id).with_zpos(0)]).unwrap(),
            Verdict::einval()
        );
        // Mutable zpos: two planes may not share a value.
        let spec = FakeOutputSpec::new(64, 64).planes(vec![
            FakePlaneSpec::default_primary().zpos(0, 0, 3, false),
            FakePlaneSpec::overlay()
                .formats(&[Fourcc::XRGB8888])
                .zpos(1, 0, 3, false),
        ]);
        let mut m = FakeBackend::new(&[spec]).unwrap();
        let mid = m.outputs()[0].id;
        m.commit(mid, &[]).unwrap();
        let mp = m.planes(mid);
        let f = |pl| full(pl, PlaneSource::OutputFront, 64, 64);
        let under = [f(mp[0].id).with_zpos(2), f(mp[1].id).with_zpos(1)];
        assert!(m.test_layout(mid, &under).unwrap().accepted());
        let clash = [f(mp[0].id).with_zpos(1), f(mp[1].id)];
        assert_eq!(m.test_layout(mid, &clash).unwrap(), Verdict::einval());
        let range = [f(mp[0].id).with_zpos(4)];
        assert_eq!(m.test_layout(mid, &range).unwrap(), Verdict::einval());
    }

    #[test]
    fn duplicate_plane_and_plane_budget() {
        let (mut b, id, p) = with_planes();
        let f = |pl| full(pl, PlaneSource::OutputFront, 1920, 1080);
        assert_eq!(
            b.test_layout(id, &[f(p[0].id), f(p[0].id)]).unwrap(),
            Verdict::einval()
        );
        let cur = b.alloc_buffer(Fourcc::ARGB8888, 64, 64).unwrap();
        let three = [
            f(p[0].id),
            f(p[1].id),
            full(p[2].id, PlaneSource::Buffer(cur), 64, 64),
        ];
        assert!(b.test_layout(id, &three).unwrap().accepted());
        b.set_max_active_planes(Some(2));
        assert_eq!(
            b.test_layout(id, &three).unwrap(),
            Verdict::Rejected(rustix::io::Errno::NOSPC.raw_os_error())
        );
    }

    #[test]
    fn unknown_ids_and_pause_are_errors_not_verdicts() {
        let (mut b, id, p) = with_planes();
        let buf = b.alloc_buffer(Fourcc::YUYV, 32, 32).unwrap();
        b.free_buffer(buf);
        let gone = [full(p[1].id, PlaneSource::Buffer(buf), 32, 32)];
        assert!(matches!(
            b.test_layout(id, &gone),
            Err(Error::NoSuchObject("buffer", _))
        ));
        let alien = [full(PlaneId(999), PlaneSource::OutputFront, 1920, 1080)];
        assert!(matches!(
            b.test_layout(id, &alien),
            Err(Error::NoSuchObject(..))
        ));
        assert!(matches!(
            b.test_layout(OutputId(9), &[]),
            Err(Error::NoSuchOutput(_))
        ));
        b.pause();
        assert!(matches!(b.test_layout(id, &[]), Err(Error::Paused)));
        assert!(b.test_log().is_empty(), "errors are not logged as verdicts");
        assert!(b.alloc_buffer(Fourcc::NV12, 3, 2).is_err());
    }

    #[test]
    fn a_test_hook_replaces_the_rules() {
        let (mut b, id, p) = with_planes();
        b.set_test_hook(Some(Box::new(|_, layout| {
            if layout.len() > 1 {
                Verdict::Rejected(28)
            } else {
                Verdict::Accepted
            }
        })));
        let f = |pl| full(pl, PlaneSource::OutputFront, 1920, 1080);
        assert!(b.test_layout(id, &[f(p[0].id)]).unwrap().accepted());
        assert_eq!(
            b.test_layout(id, &[f(p[0].id), f(p[1].id)]).unwrap(),
            Verdict::Rejected(28)
        );
        b.set_test_hook(None);
        assert!(
            b.test_layout(id, &[f(p[0].id), f(p[1].id)])
                .unwrap()
                .accepted()
        );
    }

    #[test]
    fn scanout_alpha_defaults_capable_and_off() {
        let (b, id) = fake();
        assert!(b.scanout_alpha(id));
        assert!(!b.scanout_alpha_on(id));
        assert_eq!(b.scanout_alpha_sets(id), 0);
        assert!(!b.scanout_alpha(OutputId(99)));
    }

    #[test]
    fn scanout_alpha_toggles_and_counts() {
        let (mut b, id) = fake();
        b.set_scanout_alpha(id, true).unwrap();
        assert!(b.scanout_alpha_on(id));
        b.set_scanout_alpha(id, true).unwrap();
        b.set_scanout_alpha(id, false).unwrap();
        assert!(!b.scanout_alpha_on(id));
        assert_eq!(b.scanout_alpha_sets(id), 3);
        assert!(matches!(
            b.set_scanout_alpha(OutputId(99), true),
            Err(Error::NoSuchOutput(_))
        ));
    }

    #[test]
    fn scanout_alpha_unsupported_on_a_non_alpha_output() {
        let mut b = FakeBackend::new(&[FakeOutputSpec::new(8, 4).alpha(false)]).unwrap();
        let id = b.outputs()[0].id;
        assert!(!b.scanout_alpha(id));
        assert!(matches!(
            b.set_scanout_alpha(id, true),
            Err(Error::Unsupported(_))
        ));
        assert!(!b.scanout_alpha_on(id));
        b.set_scanout_alpha(id, false).unwrap();
        assert_eq!(b.scanout_alpha_sets(id), 1);
    }

    #[test]
    fn read_front_keeps_alpha_byte() {
        let (mut b, id) = fake();
        b.set_scanout_alpha(id, true).unwrap();
        {
            let mut buf = b.back_buffer(id).unwrap();
            buf.fill_rect(Rect::new(0, 0, 8, 4), 0xFF11_2233);
            buf.fill_rect(Rect::new(1, 1, 1, 1), 0);
        }
        b.commit(id, &[]).unwrap();
        let img = b.read_front(id).unwrap();
        assert_eq!(img.pixel(0, 0), 0x0011_2233);
        assert_eq!(img.alpha(0, 0), 0xFF);
        assert_eq!(img.alpha(1, 1), 0);
    }

    #[test]
    fn works_as_trait_object() {
        let b: Box<dyn Backend> = Box::new(FakeBackend::single(2, 2).unwrap());
        assert_eq!(b.outputs().len(), 1);
    }
}
