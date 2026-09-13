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
//!   `Event::Hotplug`; the change takes effect on `rescan`.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::time::Duration;

use rustix::time::{
    Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
    timerfd_settime,
};

use crate::{BYTES_PER_PIXEL, Backend, BufferMut, Error, Event, Image, OutputId, OutputInfo, Rect};

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
        }
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
}

struct FakeOutput {
    info: OutputInfo,
    stride: u32,
    bufs: [Vec<u8>; 2],
    front: usize,
    pending: bool,
    sequence: u64,
}

impl FakeOutput {
    fn new(id: OutputId, spec: &FakeOutputSpec) -> Self {
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
            },
            stride,
            bufs: [vec![0; len], vec![0; len]],
            front: 0,
            pending: false,
            sequence: 0,
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
        self.outputs.push(FakeOutput::new(id, spec));
        self.infos.push(self.outputs.last().unwrap().info.clone());
    }

    /// Simulate plugging in a new output: queues `Event::Hotplug`; the
    /// output appears after `rescan`.
    pub fn plug(&mut self, spec: FakeOutputSpec) {
        self.pending_specs.push(spec);
        self.hotplug_queued = true;
    }

    /// Simulate unplugging: queues `Event::Hotplug`; the output vanishes
    /// after `rescan`.
    pub fn unplug(&mut self, output: OutputId) {
        self.pending_removals.push(output);
        self.hotplug_queued = true;
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

    fn output_mut(&mut self, id: OutputId) -> Result<&mut FakeOutput, Error> {
        self.outputs
            .iter_mut()
            .find(|o| o.info.id == id)
            .ok_or(Error::NoSuchOutput(id))
    }
}

impl Backend for FakeBackend {
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
        self.disarm()
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

    #[test]
    fn works_as_trait_object() {
        let b: Box<dyn Backend> = Box::new(FakeBackend::single(2, 2).unwrap());
        assert_eq!(b.outputs().len(), 1);
    }
}
