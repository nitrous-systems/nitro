//! The player: decode thread, NV12 ring, pacing, and the Surface present path.
//!
//! # Threads
//!
//! Decoding runs on its own thread, which owns the [`Decoder`] and maps
//! the ring's memfds itself; the UI thread never touches a pixel. The two
//! talk over channels ([`Cmd`] down, [`Msg`] up) plus a wake pipe the UI
//! loop watches with [`Ui::add_fd`], so a finished frame wakes the app
//! the way a key press does.
//!
//! # The ring
//!
//! [`RING`] sealed memfds of one NV12 frame each: one on screen, one
//! latched, two decoded ahead. A slot is **with the decoder** until it is
//! filled, then **ready** (`pts`), then **queued** to the server by
//! `PresentSurface`, and back to the decoder at `BufferReleased`. A full
//! ring is the back-pressure: the decode thread blocks on its command
//! channel until a slot comes back.
//!
//! # Pacing
//!
//! While playing, every `Frame{deadline}` callback shows the newest ready
//! frame due by the deadline ([`crate::pacing::pick`]). Older due frames are
//! skipped, and at most one `PresentSurface` goes out per callback. A
//! callback that shows nothing does not ask again at once, because the
//! server answers an idle request immediately. It sets a timer for when
//! the next frame is due instead.

use std::os::fd::{AsFd as _, OwnedFd};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;

use nitro_core::IRect;
use nitro_shm::MappingMut;
use nitro_ui::event::{Handled, KeyEvent, key};
use nitro_ui::surface::{SurfaceEvent, SurfacePointer, SurfaceView};
use nitro_ui::widgets::{Button, Label, Slider};
use nitro_ui::{Frame, TimerId, Ui, WidgetId};
use nitro_wire::msg::{CreateSurfaceBuffer, PresentSurface};
use nitro_wire::types::{BufferId, ColorMatrix, ColorRange, NodeId, WindowState, format};

use crate::controls::{self, ICON_PAUSE, ICON_PLAY, Ids};
use crate::decode::{Decoder, Matrix, Nv12Layout, StreamInfo};
use crate::pacing::{self, Clock};

/// Buffers in the ring.
pub const RING: usize = 4;
/// Controls hide after this long without pointer motion while playing.
pub const HIDE_MS: u64 = 3000;
/// Two presses on the video this close together toggle fullscreen.
pub const DOUBLE_CLICK_NS: u64 = 400_000_000;
/// The seek step of the arrow keys, seconds.
pub const STEP_SECS: f64 = 5.0;
/// A presented frame this far behind its due time counts as late.
const LATE_NS: u64 = 20_000_000;

/// evdev keycodes the player binds beyond [`key`]'s.
mod keys {
    pub const F: u32 = 33;
    pub const F11: u32 = 87;
}

/// Commands to the decode thread.
#[derive(Debug)]
enum Cmd {
    /// Slot `n` is free again.
    Free(usize),
    /// Restart from the keyframe before `secs`, drop frames before
    /// `secs`, and tag what follows with `generation`.
    Seek { generation: u32, secs: f64 },
}

/// News from the decode thread.
#[derive(Debug)]
enum Msg {
    Frame { generation: u32, slot: usize, pts_us: i64 },
    Eof { generation: u32 },
    Error(String),
}

/// Where a ring slot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// With the decode thread (free or being filled).
    Decoder,
    /// Decoded and waiting for its moment.
    Ready,
    /// Sent to the server.
    Queued { serial: u32, pts_us: i64, shown: bool },
}

/// Playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The clock runs and frames are presented.
    Playing,
    /// Stopped by the user; the last frame stays up.
    Paused,
    /// The stream ran out.
    Ended,
}

/// Command-line options the player acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Opts {
    /// Quit after this many presented frames (0 = never).
    pub frames: u64,
    /// Start fullscreen.
    pub fullscreen: bool,
}

/// Counters for `--stats`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// `PresentSurface`s sent.
    pub sent: u64,
    /// Frames that got `Presented`.
    pub presented: u64,
    /// Frames released without `Presented` (superseded at the latch).
    pub dropped: u64,
    /// Presented more than 20 ms after they were due.
    pub late: u64,
    /// Decoded frames never sent, because a newer one was already due.
    pub skipped: u64,
    /// The pts (µs) of the first presented frames, in order (tests).
    pub shown: Vec<i64>,
}

/// The whole app state: `S` for the nitro-ui tree.
pub struct Player {
    opts: Opts,
    info: StreamInfo,
    layout: Nv12Layout,
    tx: Sender<Cmd>,
    rx: Receiver<Msg>,
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
    /// The memfds, until [`Player::start`] registers them.
    fds: Vec<Option<OwnedFd>>,
    buffers: Vec<BufferId>,
    slots: Vec<Slot>,
    /// Ready frames: `(slot, pts_us)`.
    ready: Vec<(usize, i64)>,
    clock: Clock,
    state: State,
    generation: u32,
    seek_in_flight: Option<u32>,
    seek_pending: Option<f64>,
    decoder_eof: bool,
    position_us: i64,
    ids: Option<Ids>,
    controls_visible: bool,
    hide_timer: Option<TimerId>,
    frame_timer: Option<TimerId>,
    last_down_ns: u64,
    started: bool,
    /// Re-anchor the clock at the next `Presented` (see `on_surface`).
    calibrate: bool,
    /// Counters for `--stats`.
    pub stats: Stats,
    /// A fatal error, reported when the loop ends.
    pub error: Option<String>,
}

/// `CLOCK_MONOTONIC`, nanoseconds.
#[must_use]
pub fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec.cast_unsigned() * 1_000_000_000 + t.tv_nsec.cast_unsigned()
}

fn wire_matrix(m: Matrix) -> ColorMatrix {
    match m {
        Matrix::Bt601 => ColorMatrix::Bt601,
        Matrix::Bt709 => ColorMatrix::Bt709,
        Matrix::Bt2020 => ColorMatrix::Bt2020,
    }
}

/// The decode thread's body. See the module docs.
fn decode_loop(
    mut dec: Box<dyn Decoder>,
    layout: Nv12Layout,
    fds: Vec<OwnedFd>,
    rx: &Receiver<Cmd>,
    tx: &Sender<Msg>,
    wake: &OwnedFd,
) {
    let send = |m: Msg| {
        let _ = tx.send(m);
        // Non-blocking; a full pipe already has a wakeup in it.
        let _ = rustix::io::write(wake, &[1]);
    };
    let mut maps = Vec::with_capacity(fds.len());
    for fd in &fds {
        match MappingMut::map_mut(fd.as_fd(), layout.frame_len()) {
            Ok(m) => maps.push(m),
            Err(e) => {
                send(Msg::Error(format!("mapping a frame buffer: {e}")));
                return;
            }
        }
    }
    drop(fds);
    let mut free: Vec<usize> = (0..maps.len()).rev().collect();
    let mut generation = 0;
    let mut skip_before: Option<i64> = None;
    let mut eof = false;
    loop {
        let cmd = if eof || free.is_empty() {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        } else {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            }
        };
        match cmd {
            Some(Cmd::Free(i)) => free.push(i),
            Some(Cmd::Seek {
                generation: g,
                secs,
            }) => {
                generation = g;
                eof = false;
                skip_before = Some((secs * 1e6) as i64);
                if let Err(e) = dec.seek(secs) {
                    send(Msg::Error(e));
                    return;
                }
            }
            None => {
                let slot = free[free.len() - 1];
                match dec.next_frame(maps[slot].as_bytes_mut(), layout) {
                    Ok(Some(pts_us)) => {
                        if skip_before.is_some_and(|t| pts_us < t) {
                            continue;
                        }
                        skip_before = None;
                        free.pop();
                        send(Msg::Frame {
                            generation,
                            slot,
                            pts_us,
                        });
                    }
                    Ok(None) => {
                        eof = true;
                        send(Msg::Eof { generation });
                    }
                    Err(e) => {
                        send(Msg::Error(e));
                        return;
                    }
                }
            }
        }
    }
}

impl Player {
    /// Allocate the ring and start the decode thread on `dec`.
    ///
    /// # Errors
    /// A memfd, pipe or thread failure.
    pub fn new(dec: Box<dyn Decoder>, opts: Opts) -> Result<Self, String> {
        let info = dec.info().clone();
        let layout = Nv12Layout::for_video(info.width, info.height);
        let len = layout.frame_len();
        let mut mine = Vec::with_capacity(RING);
        let mut theirs = Vec::with_capacity(RING);
        for _ in 0..RING {
            let fd = nitro_shm::create_sealed("nitro-video", len as u64)
                .map_err(|e| format!("memfd: {e}"))?;
            theirs.push(rustix::io::dup(&fd).map_err(|e| format!("dup: {e}"))?);
            mine.push(Some(fd));
        }
        let (wake_r, wake_w) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .map_err(|e| format!("pipe: {e}"))?;
        let (ctx, crx) = mpsc::channel();
        let (mtx, mrx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("nitro-video-decode".to_owned())
            .spawn(move || decode_loop(dec, layout, theirs, &crx, &mtx, &wake_w))
            .map_err(|e| format!("decode thread: {e}"))?;
        Ok(Self {
            opts,
            info,
            layout,
            tx: ctx,
            rx: mrx,
            wake: wake_r,
            thread: Some(thread),
            fds: mine,
            buffers: Vec::new(),
            slots: vec![Slot::Decoder; RING],
            ready: Vec::new(),
            clock: Clock::new(1_000_000),
            state: State::Playing,
            generation: 0,
            seek_in_flight: None,
            seek_pending: None,
            decoder_eof: false,
            position_us: 0,
            ids: None,
            controls_visible: true,
            hide_timer: None,
            frame_timer: None,
            last_down_ns: 0,
            started: false,
            calibrate: false,
            stats: Stats::default(),
            error: None,
        })
    }

    /// The stream's description.
    #[must_use]
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Playback state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// The pts of the frame last sent to the screen, seconds.
    #[must_use]
    pub fn position(&self) -> f64 {
        self.position_us as f64 / 1e6
    }

    /// Whether the controls bar is shown.
    #[must_use]
    pub fn controls_visible(&self) -> bool {
        self.controls_visible
    }

    /// The widget ids, once the tree exists.
    #[must_use]
    pub fn ids(&self) -> Option<Ids> {
        self.ids
    }

    /// The `--stats` line.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let s = &self.stats;
        format!(
            "video: {}x{} {} sent={} presented={} dropped={} late={} skipped={}",
            self.info.width,
            self.info.height,
            self.info.codec,
            s.sent,
            s.presented,
            s.dropped,
            s.late,
            s.skipped
        )
    }

    // -- setup --------------------------------------------------------

    /// Register the ring with the server, once the window is open. Run
    /// deferred from [`install`].
    fn start(&mut self, ui: &mut Ui<Self>) {
        if self.started {
            return;
        }
        self.started = true;
        if !ui.has_surfaces() {
            self.fail(
                ui,
                "the server cannot show video (no caps::SURFACE/RELEASE, or a remote link)".into(),
            );
            return;
        }
        let l = self.layout;
        for fd in &mut self.fds {
            let Some(fd) = fd.take() else { continue };
            let id = ui.alloc_buffer_id();
            let r = ui.create_surface_buffer(CreateSurfaceBuffer {
                id,
                width: l.width,
                height: l.height,
                format: format::NV12,
                size: l.frame_len() as u32,
                offset0: 0,
                stride0: l.width,
                offset1: l.luma_len() as u32,
                stride1: l.width,
                fd,
            });
            if let Err(e) = r {
                self.fail(ui, format!("registering a frame buffer: {e}"));
                return;
            }
            self.buffers.push(id);
        }
        if self.opts.fullscreen {
            let _ = ui.set_window_state(WindowState::Fullscreen);
        }
        let _ = ui.flush();
        self.ids = Ids::resolve(ui);
        self.arm_hide(ui);
        self.tick(ui);
        let _ = ui.request_frame();
    }

    fn fail(&mut self, ui: &mut Ui<Self>, e: String) {
        if self.error.is_none() {
            self.error = Some(e);
        }
        ui.quit();
    }

    fn node(&self, ui: &Ui<Self>) -> NodeId {
        self.ids
            .and_then(|ids| ui.widget::<SurfaceView<Self>>(ids.view).ok().map(SurfaceView::node))
            .unwrap_or(NodeId::NONE)
    }

    fn free(&mut self, slot: usize) {
        self.slots[slot] = Slot::Decoder;
        let _ = self.tx.send(Cmd::Free(slot));
    }

    // -- the decode thread's news ----------------------------------------

    fn on_wake(&mut self, ui: &mut Ui<Self>) {
        let mut buf = [0u8; 64];
        while matches!(rustix::io::read(&self.wake, &mut buf), Ok(n) if n > 0) {}
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Frame {
                    generation,
                    slot,
                    pts_us,
                }) => self.on_decoded(ui, generation, slot, pts_us),
                Ok(Msg::Eof { generation }) if generation == self.generation => {
                    self.decoder_eof = true;
                    if self.seek_in_flight == Some(generation) {
                        self.seek_done(ui);
                    }
                    self.check_ended(ui);
                }
                Ok(Msg::Eof { .. }) => {}
                Ok(Msg::Error(e)) => {
                    self.fail(ui, e);
                    return;
                }
                Err(_) => break,
            }
        }
    }

    fn on_decoded(&mut self, ui: &mut Ui<Self>, generation: u32, slot: usize, pts_us: i64) {
        if generation != self.generation {
            self.free(slot);
            return;
        }
        self.slots[slot] = Slot::Ready;
        self.ready.push((slot, pts_us));
        let first_after_seek = self.seek_in_flight == Some(generation);
        if first_after_seek {
            self.seek_done(ui);
        }
        match self.state {
            State::Playing => {
                if self.frame_timer.is_none() {
                    let _ = ui.request_frame();
                }
            }
            // A seek while paused shows where it landed.
            State::Paused | State::Ended if first_after_seek => {
                if let Some(i) = self.ready.iter().position(|r| r.0 == slot) {
                    self.ready.remove(i);
                    self.present(ui, slot, pts_us);
                }
                self.tick(ui);
            }
            State::Paused | State::Ended => {}
        }
    }

    fn seek_done(&mut self, ui: &mut Ui<Self>) {
        self.seek_in_flight = None;
        if let Some(s) = self.seek_pending.take() {
            self.send_seek(ui, s);
        }
    }

    fn check_ended(&mut self, ui: &mut Ui<Self>) {
        if self.decoder_eof
            && self.ready.is_empty()
            && self.seek_in_flight.is_none()
            && self.state == State::Playing
        {
            self.state = State::Ended;
            self.clock.stop();
            self.set_play_icon(ui);
            self.show_controls(ui);
            self.tick(ui);
        }
    }

    // -- presenting ------------------------------------------------------

    fn present(&mut self, ui: &mut Ui<Self>, slot: usize, pts_us: i64) -> bool {
        let node = self.node(ui);
        if node.is_none() || slot >= self.buffers.len() {
            self.slots[slot] = Slot::Ready;
            self.ready.push((slot, pts_us));
            return false;
        }
        let serial = ui.next_serial();
        let l = self.layout;
        let r = ui.present_surface(PresentSurface {
            id: node,
            buffer: self.buffers[slot],
            serial,
            src: IRect::new(0, 0, l.width.cast_signed(), l.height.cast_signed()),
            matrix: wire_matrix(self.info.matrix),
            range: if self.info.full_range {
                ColorRange::Full
            } else {
                ColorRange::Limited
            },
            damage: Vec::new(),
        });
        if let Err(e) = r {
            self.fail(ui, format!("presenting: {e}"));
            return false;
        }
        self.slots[slot] = Slot::Queued {
            serial,
            pts_us,
            shown: false,
        };
        self.position_us = pts_us;
        self.stats.sent += 1;
        true
    }

    /// A `Frame` callback: show whatever is due by its deadline.
    fn on_frame(&mut self, ui: &mut Ui<Self>, f: Frame) {
        if self.state != State::Playing || self.ready.is_empty() {
            self.check_ended(ui);
            return;
        }
        let now = now_ns();
        // A deadline far from now is not a real vblank (a test's
        // synthetic callback); aim at now instead.
        let t = if f.deadline_ns.abs_diff(now) < 1_000_000_000 {
            f.deadline_ns
        } else {
            now
        };
        if !self.clock.is_running() {
            let first = self.ready.iter().map(|r| r.1).min().unwrap_or(0);
            self.clock.anchor(t, first);
            self.calibrate = true;
        }
        let target = self.clock.media_at(t).unwrap_or(0);
        let pts: Vec<i64> = self.ready.iter().map(|r| r.1).collect();
        let pick = pacing::pick(&pts, target);
        let chosen = pick.show.map(|i| self.ready[i]);
        let skipped: Vec<usize> = pick.skip.iter().map(|&i| self.ready[i].0).collect();
        let gone: Vec<usize> = chosen.iter().map(|c| c.0).chain(skipped.iter().copied()).collect();
        self.ready.retain(|r| !gone.contains(&r.0));
        for s in skipped {
            self.stats.skipped += 1;
            self.free(s);
        }
        if let Some((slot, pts_us)) = chosen
            && self.present(ui, slot, pts_us)
        {
            // The flip carrying it answers the next request.
            let _ = ui.request_frame();
            return;
        }
        // Nothing due yet: wake when the earliest ready frame is, a little
        // before its vblank, rather than spinning on idle callbacks.
        if let Some(next) = self.ready.iter().map(|r| r.1).min()
            && self.frame_timer.is_none()
        {
            let due = self.clock.mono_at(next).unwrap_or(now);
            let lead = u64::from(f.refresh_ns).max(1_000_000);
            let ms = (due.saturating_sub(now).saturating_sub(lead) / 1_000_000).max(1);
            self.frame_timer = Some(ui.set_timer(ms, |p: &mut Self, ui: &mut Ui<Self>| {
                p.frame_timer = None;
                let _ = ui.request_frame();
            }));
        }
    }

    fn on_surface(&mut self, ui: &mut Ui<Self>, ev: &SurfaceEvent) {
        match *ev {
            SurfaceEvent::Presented { serial, time_ns, .. } => {
                let Some(i) = self.slots.iter().position(
                    |s| matches!(s, Slot::Queued { serial: q, shown: false, .. } if *q == serial),
                ) else {
                    return;
                };
                let Slot::Queued { pts_us, .. } = self.slots[i] else {
                    return;
                };
                self.slots[i] = Slot::Queued {
                    serial,
                    pts_us,
                    shown: true,
                };
                self.stats.presented += 1;
                if self.stats.shown.len() < 4096 {
                    self.stats.shown.push(pts_us);
                }
                // The first frame of a run calibrates the clock to when
                // frames really reach the screen: the frame callback's
                // deadline is the server's estimate, and anchoring on it
                // would call every frame late by the estimate's error.
                if std::mem::take(&mut self.calibrate) && self.clock.is_running() {
                    self.clock.anchor(time_ns, pts_us);
                } else if let Some(due) = self.clock.mono_at(pts_us)
                    && time_ns > due + LATE_NS
                {
                    self.stats.late += 1;
                }
                if self.opts.frames > 0 && self.stats.presented >= self.opts.frames {
                    ui.quit();
                }
            }
            SurfaceEvent::Released(id) => {
                if let Some(i) = self.buffers.iter().position(|b| *b == id)
                    && let Slot::Queued { shown, .. } = self.slots[i]
                {
                    if !shown {
                        self.stats.dropped += 1;
                    }
                    self.free(i);
                }
            }
            SurfaceEvent::Hint { .. } => {}
        }
    }

    // -- user actions ----------------------------------------------------

    /// Play ↔ pause; from the end, play again from the start.
    pub fn toggle_play(&mut self, ui: &mut Ui<Self>) {
        match self.state {
            State::Playing => {
                self.state = State::Paused;
                self.clock.stop();
                self.show_controls(ui);
            }
            State::Paused => {
                self.state = State::Playing;
                self.clock.stop();
                let _ = ui.request_frame();
                self.arm_hide(ui);
            }
            State::Ended => {
                self.state = State::Playing;
                self.send_seek(ui, 0.0);
                self.arm_hide(ui);
            }
        }
        self.set_play_icon(ui);
        self.tick(ui);
    }

    /// Seek to `secs`. Drags coalesce: one restart is in flight at a
    /// time, and only the newest target waits behind it.
    pub fn request_seek(&mut self, ui: &mut Ui<Self>, secs: f64) {
        let secs = if self.info.duration > 0.0 {
            secs.clamp(0.0, self.info.duration)
        } else {
            secs.max(0.0)
        };
        self.position_us = (secs * 1e6) as i64;
        if self.state == State::Ended {
            self.state = State::Paused;
            self.set_play_icon(ui);
        }
        if self.seek_in_flight.is_some() {
            self.seek_pending = Some(secs);
            return;
        }
        self.send_seek(ui, secs);
    }

    fn send_seek(&mut self, ui: &mut Ui<Self>, secs: f64) {
        self.generation = self.generation.wrapping_add(1);
        for (slot, _) in std::mem::take(&mut self.ready) {
            self.free(slot);
        }
        self.decoder_eof = false;
        self.clock.stop();
        self.seek_in_flight = Some(self.generation);
        let _ = self.tx.send(Cmd::Seek {
            generation: self.generation,
            secs,
        });
        self.tick(ui);
    }

    /// Seek by `delta` seconds from where the picture is.
    pub fn step(&mut self, ui: &mut Ui<Self>, delta: f64) {
        let at = self.seek_pending.unwrap_or_else(|| self.position());
        self.request_seek(ui, at + delta);
    }

    /// Fullscreen ↔ windowed.
    pub fn toggle_fullscreen(&mut self, ui: &mut Ui<Self>) {
        let want = if ui.window_state() == WindowState::Fullscreen {
            WindowState::Normal
        } else {
            WindowState::Fullscreen
        };
        let _ = ui.set_window_state(want);
    }

    /// Pointer news from the video view.
    pub fn on_pointer(&mut self, ui: &mut Ui<Self>, ev: SurfacePointer) {
        match ev {
            SurfacePointer::Move(_) => {
                self.show_controls(ui);
                self.arm_hide(ui);
            }
            SurfacePointer::Down(_, _) => {
                let now = now_ns();
                if now.saturating_sub(self.last_down_ns) < DOUBLE_CLICK_NS {
                    self.last_down_ns = 0;
                    self.toggle_fullscreen(ui);
                } else {
                    self.last_down_ns = now;
                }
            }
            SurfacePointer::Up(..) | SurfacePointer::Leave => {}
        }
    }

    fn on_key(&mut self, ui: &mut Ui<Self>, k: &KeyEvent) -> Handled {
        self.show_controls(ui);
        self.arm_hide(ui);
        match k.keycode {
            key::SPACE => self.toggle_play(ui),
            keys::F | keys::F11 => self.toggle_fullscreen(ui),
            key::ESC if ui.window_state() == WindowState::Fullscreen => {
                let _ = ui.set_window_state(WindowState::Normal);
            }
            key::LEFT => self.step(ui, -STEP_SECS),
            key::RIGHT => self.step(ui, STEP_SECS),
            key::Q => ui.quit(),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    // -- the overlay -------------------------------------------------------

    fn show_controls(&mut self, ui: &mut Ui<Self>) {
        if !self.controls_visible
            && let Some(ids) = self.ids
        {
            self.controls_visible = true;
            ui.set_node_visible(ids.bar, true);
            self.tick(ui);
        }
    }

    fn arm_hide(&mut self, ui: &mut Ui<Self>) {
        if let Some(t) = self.hide_timer.take() {
            ui.cancel_timer(&t);
        }
        self.hide_timer = Some(ui.set_timer(HIDE_MS, |p: &mut Self, ui: &mut Ui<Self>| {
            p.hide_timer = None;
            let dragging = p.ids.is_some_and(|ids| ui.is_captured(ids.seek));
            if p.state == State::Playing
                && !dragging
                && let Some(ids) = p.ids
            {
                p.controls_visible = false;
                ui.set_node_visible(ids.bar, false);
            }
        }));
    }

    fn set_play_icon(&self, ui: &mut Ui<Self>) {
        let Some(ids) = self.ids else { return };
        let (icon, text) = if self.state == State::Playing {
            (ICON_PAUSE, "Pause")
        } else {
            (ICON_PLAY, "Play")
        };
        if let Ok(mut b) = ui.widget_mut::<Button<Self>>(ids.play) {
            if b.icon() != Some(icon) {
                b.set_icon(icon);
            }
            if b.text() != text {
                b.set_text(text);
            }
        }
    }

    /// Refresh the time and the slider. Called once a second (and on
    /// state changes), never per video frame.
    fn tick(&mut self, ui: &mut Ui<Self>) {
        let Some(ids) = self.ids else { return };
        if !self.controls_visible {
            return;
        }
        let pos = self.position();
        let text = controls::time_text(pos, self.info.duration);
        if let Ok(mut l) = ui.widget_mut::<Label>(ids.time)
            && l.text() != text
        {
            l.set_text(text);
        }
        if !ui.is_captured(ids.seek)
            && let Ok(mut s) = ui.widget_mut::<Slider<Self>>(ids.seek)
        {
            s.set_value(pos as f32);
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // Closing the command channel ends the decode thread at its next
        // look; a decode in progress finishes first.
        let (tx, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Refresh the overlay once a second, re-arming itself.
fn every_second(ui: &mut Ui<Player>) {
    ui.set_timer(1000, |p: &mut Player, ui: &mut Ui<Player>| {
        p.tick(ui);
        every_second(ui);
    });
}

/// Build the tree and wire the player into `ui`: what both the binary
/// and the tests call. Returns the root.
///
/// # Panics
/// If the wake pipe cannot be duplicated (out of descriptors at start).
pub fn install(ui: &mut Ui<Player>, info: &StreamInfo, wake: std::os::fd::BorrowedFd<'_>) -> WidgetId {
    ui.enable_surfaces();
    ui.on_surface(|p: &mut Player, ui: &mut Ui<Player>, ev: &SurfaceEvent| p.on_surface(ui, ev));
    ui.on_frame(|p: &mut Player, ui: &mut Ui<Player>, f| p.on_frame(ui, f));
    ui.on_key(|p: &mut Player, ui: &mut Ui<Player>, k: &KeyEvent| p.on_key(ui, k));
    ui.on_window_state(|p: &mut Player, ui: &mut Ui<Player>, _| p.tick(ui));
    ui.add_fd(wake, |p: &mut Player, ui: &mut Ui<Player>| p.on_wake(ui))
        .expect("watch the decode thread's wake pipe");
    ui.defer(|p: &mut Player, ui: &mut Ui<Player>| p.start(ui));
    // The once-a-second overlay refresh, re-armed by itself.
    every_second(ui);
    controls::build(ui, info.aspect(), info.duration)
}

impl Player {
    /// The read end of the wake pipe, for [`install`].
    #[must_use]
    pub fn wake_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.wake.as_fd()
    }
}

/// The window's first size: the video's, scaled down to fit 1280×720
/// logical pixels.
#[must_use]
pub fn initial_size(info: &StreamInfo) -> (f32, f32) {
    let (w, h) = (info.width.max(1) as f32, info.height.max(1) as f32);
    let s = (1280.0 / w).min(720.0 / h).min(1.0);
    ((w * s).round().max(160.0), (h * s).round().max(90.0))
}
