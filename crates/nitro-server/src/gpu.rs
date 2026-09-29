//! The GPU helper in the server (#3922): composite mode 2.
//!
//! `nitro-gpu-vulkan` is a separate process (design: `docs/surfaces.md`
//! § GPU helper). This module is the server's half of it, everything
//! that does not touch KMS or the scene:
//!
//! - the **supervisor**: spawn (socket on the child's fd 0), `Hello`
//!   with a deadline, EOF/HUP as death, restart with doubling backoff
//!   on a timerfd, give up after [`GIVE_UP_CRASHES`] in
//!   [`GIVE_UP_WINDOW`] (until a VT resume or SIGHUP), and the on-demand
//!   mode whose clean idle exit is not a crash;
//! - the **protocol pump**: a non-blocking [`nitro_gpu::Conn`] in the
//!   server's epoll, replies decoded into [`Reply`]s for `lib.rs`;
//! - the **output ring** of the one output in mode 2 ([`Ring`]) and its
//!   slot states;
//! - **textures**: one per client buffer, imported lazily on first use
//!   and released when the buffer goes;
//! - **borrowed buffers**: every frame's client buffers are held against
//!   `BufferReleased` until its completion fence signals ([`Borrows`]).
//!
//! The server never waits on the GPU: the completion `sync_file` goes to
//! KMS as `IN_FENCE_FD`, and a dup of it sits in epoll only to learn when
//! the borrowed buffers may go back.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nitro_gpu::Message as _;
use nitro_gpu::proto::{self, DeviceInfo, DmabufDesc, FromHelper, ToHelper};
use nitro_kms::{BufferId, OutputId as KmsOutputId};
use nitro_scene::{BufferKey, ClientId};

use crate::config::GpuHelper as Mode;
use crate::{debug, info, warn};

/// First restart delay.
pub const BACKOFF_MIN: Duration = Duration::from_millis(100);
/// Longest restart delay.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Running this long resets the backoff.
pub const HEALTHY: Duration = Duration::from_secs(60);
/// Crashes within [`GIVE_UP_WINDOW`] after which the server stops trying.
pub const GIVE_UP_CRASHES: usize = 5;
/// See [`GIVE_UP_CRASHES`].
pub const GIVE_UP_WINDOW: Duration = Duration::from_secs(300);
/// A helper that has not answered `Hello` by then is killed.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// A `Composite` unanswered for this long means the helper hung.
pub const HANG_TIMEOUT: Duration = Duration::from_millis(250);
/// Ring slots.
pub const RING_SLOTS: u32 = 3;

/// Tests run the helper in-process: the server hands them its socket end
/// instead of `exec`ing a binary ([`crate::Config::gpu_spawner`]).
#[derive(Clone)]
pub struct Spawner(pub Arc<dyn Fn(OwnedFd) + Send + Sync>);

impl std::fmt::Debug for Spawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Spawner")
    }
}

/// The supervisor's state, as `stats` reports it (`gpu_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not running and not wanted (off, or on-demand and idle).
    Off,
    /// Spawned, `Hello` not answered yet.
    Starting,
    /// Answered `Hello`: usable.
    Ready,
    /// Dead; a restart is scheduled.
    Backoff,
    /// Crashed too often; nothing until a VT resume or SIGHUP.
    GaveUp,
}

impl State {
    /// `gpu_state`: 0 off, 1 starting, 2 ready, 3 backoff, 4 gave up.
    #[must_use]
    pub const fn number(self) -> u64 {
        match self {
            State::Off => 0,
            State::Starting => 1,
            State::Ready => 2,
            State::Backoff => 3,
            State::GaveUp => 4,
        }
    }
}

/// Restart pacing: doubling from [`BACKOFF_MIN`] to [`BACKOFF_MAX`], and
/// the give-up rule.
#[derive(Debug, Default)]
pub struct Backoff {
    next: Option<Duration>,
    crashes: Vec<Instant>,
}

impl Backoff {
    /// A crash at `now`: the delay before the next start, or `None` to
    /// give up. `up_for` is how long the dead helper had been running.
    pub fn crashed(&mut self, now: Instant, up_for: Duration) -> Option<Duration> {
        if up_for >= HEALTHY {
            self.next = None;
        }
        self.crashes
            .retain(|t| now.saturating_duration_since(*t) < GIVE_UP_WINDOW);
        self.crashes.push(now);
        if self.crashes.len() >= GIVE_UP_CRASHES {
            return None;
        }
        let d = self.next.unwrap_or(BACKOFF_MIN);
        self.next = Some((d * 2).min(BACKOFF_MAX));
        Some(d)
    }

    /// Start afresh (VT resume, SIGHUP).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// A Surface the helper could composite, as the planner saw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    /// The Surface node.
    pub node: nitro_scene::NodeKey,
    /// Its current buffer.
    pub key: BufferKey,
    /// Visible part, output-local pixels.
    pub dst: nitro_core::IRect,
    /// The texels that map onto `dst`: x, y, w, h.
    pub src: [f32; 4],
    /// YUV matrix.
    pub encoding: proto::ColorEncoding,
    /// YUV range.
    pub range: proto::ColorRange,
}

/// What one ring slot is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Nothing reads it: it may be drawn into.
    Free,
    /// A `Composite` into it is submitted (serial).
    Submitted(u64),
    /// Committed; the flip showing it is pending or done. It becomes
    /// free when KMS reports the flip that replaced it
    /// (`take_released_buffers`), which implies its fence signalled.
    Shown,
}

/// One ring slot: its KMS framebuffer and state.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    /// The framebuffer (`AddFB2` once, at ring allocation).
    pub fb: BufferId,
    /// What it is doing.
    pub state: SlotState,
    /// The serial last drawn into it: the helper refuses the slot
    /// (`Busy`) until that frame's fence signalled.
    pub last: Option<u64>,
}

/// The first slot nothing reads and whose last frame is done
/// (`pending(serial)` says a fence is still out), if any.
#[must_use]
pub fn pick_free(slots: &[Slot], pending: impl Fn(u64) -> bool) -> Option<usize> {
    slots
        .iter()
        .position(|s| s.state == SlotState::Free && s.last.is_none_or(|l| !pending(l)))
}

/// Client buffers sampled by submitted frames whose fence has not
/// signalled: a `BufferReleased` for any of them waits.
#[derive(Debug, Default)]
pub struct Borrows {
    frames: Vec<(u64, Vec<BufferKey>)>,
}

impl Borrows {
    /// Frame `serial` samples `keys`.
    pub fn add(&mut self, serial: u64, keys: Vec<BufferKey>) {
        self.frames.push((serial, keys));
    }

    /// Frame `serial` is done.
    pub fn done(&mut self, serial: u64) {
        self.frames.retain(|(s, _)| *s != serial);
    }

    /// Whether an unfinished frame samples `key`.
    #[must_use]
    pub fn holds(&self, key: BufferKey) -> bool {
        self.frames.iter().any(|(_, k)| k.contains(&key))
    }

    /// Frames outstanding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether none is outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Forget every frame (the helper died and its fences errored).
    pub fn clear(&mut self) {
        self.frames.clear();
    }
}

/// A client buffer's dma-buf, kept (dup'd fds) so it can be imported
/// into the helper on first use.
#[derive(Debug)]
pub struct Source {
    /// The layout; `id` is filled in at import.
    pub desc: DmabufDesc,
    /// One fd per plane.
    pub fds: Vec<OwnedFd>,
}

/// A texture the helper holds for a client buffer.
#[derive(Debug, Clone, Copy)]
struct Tex {
    id: u32,
    /// The import was refused: this buffer cannot be composited.
    refused: bool,
}

/// A frame whose `Composite` is out and whose `Composited` has not come.
#[derive(Debug, Clone)]
pub struct InFlight {
    /// Serial.
    pub serial: u64,
    /// Slot index.
    pub slot: usize,
    /// When it was sent.
    pub sent: Instant,
    /// Client buffers it samples.
    pub keys: Vec<BufferKey>,
}

/// A frame the helper submitted: waiting for the commit (a flip is
/// still pending on the output).
#[derive(Debug)]
pub struct Composited {
    /// Serial.
    pub serial: u64,
    /// Slot index.
    pub slot: usize,
    /// The completion `sync_file`, for `IN_FENCE_FD`.
    pub fence: OwnedFd,
}

/// The ring of the output in mode 2.
#[derive(Debug, Default)]
pub struct Ring {
    /// The output it was allocated for (its size).
    pub size: (u32, u32),
    /// Slots; empty until `OutputRing` arrived and was imported.
    pub slots: Vec<Slot>,
    /// `AllocOutputRing` is out.
    pub requested: bool,
    /// The shadow memfd was sent (`ImportShadow`) and not refused.
    pub shadow: Option<u32>,
    /// The frame out, if any.
    pub in_flight: Option<InFlight>,
    /// The frame submitted, waiting for its commit.
    pub composited: Option<Composited>,
    /// The slot last committed (the primary shows it).
    pub shown: Option<usize>,
}

impl Ring {
    /// Whether the helper can composite into this ring now.
    #[must_use]
    pub fn ready(&self) -> bool {
        !self.slots.is_empty() && self.shadow.is_some()
    }

    /// Whether `fb` is one of the slots, and which.
    #[must_use]
    pub fn slot_of(&self, fb: BufferId) -> Option<usize> {
        self.slots.iter().position(|s| s.fb == fb)
    }
}

/// A decoded reply, for `lib.rs` to act on.
#[derive(Debug)]
pub enum Reply {
    /// `HelloReply`: the helper is ready.
    Ready,
    /// `OutputRing`: import each slot as a framebuffer.
    Ring {
        /// Size.
        size: (u32, u32),
        /// Fourcc.
        fourcc: u32,
        /// Modifier the driver picked.
        modifier: u64,
        /// Per-slot layout and dma-buf.
        slots: Vec<(proto::SlotLayout, OwnedFd)>,
    },
    /// A frame was submitted.
    Composited {
        /// Serial.
        serial: u64,
        /// Completion fence.
        fence: OwnedFd,
    },
    /// A frame was refused (`what` is its serial).
    CompositeFailed {
        /// Serial.
        serial: u64,
        /// The code.
        code: proto::ErrorCode,
    },
    /// The ring could not be allocated.
    RingFailed,
    /// The shadow import was refused.
    ShadowRefused,
}

/// Counters for `stats`.
#[derive(Debug, Default, Clone, Copy)]
#[allow(clippy::struct_field_names)]
pub struct Counters {
    /// Helper processes started.
    pub spawns: u64,
    /// Deaths that were not a clean idle exit.
    pub crashes: u64,
    /// Outputs that fell back out of mode 2 because the helper died.
    pub fallbacks: u64,
    /// Frames composited (answered `Composited`).
    pub frames: u64,
    /// Frames not submitted for want of a free slot.
    pub busy_slots: u64,
    /// Frames the helper refused.
    pub refused_frames: u64,
    /// Texture imports the helper refused.
    pub import_refused: u64,
    /// Sum over frames of fence signalled − submit, µs.
    pub busy_us: u64,
}

/// The server's side of the GPU helper.
#[allow(clippy::struct_excessive_bools)] // independent flags
pub struct Helper {
    /// When it runs.
    pub mode: Mode,
    idle_exit: u32,
    path: Option<PathBuf>,
    spawner: Option<Spawner>,
    /// Supervisor state.
    pub state: State,
    conn: Option<nitro_gpu::Conn>,
    child: Option<Child>,
    started: Option<Instant>,
    /// The helper's device, once `Hello` was answered.
    pub info: Option<DeviceInfo>,
    backoff: Backoff,
    timer: OwnedFd,
    hello_at: Option<Instant>,
    respawn_at: Option<Instant>,
    /// The one output that may be in mode 2 (one ring).
    pub owner: Option<KmsOutputId>,
    /// Its ring and shadow.
    pub ring: Ring,
    texs: HashMap<BufferKey, Tex>,
    /// Dma-bufs of client buffers, for the lazy import.
    pub sources: HashMap<BufferKey, Source>,
    next_tex: u32,
    next_serial: u64,
    /// Frames whose fence has not signalled, by serial, with the fence
    /// dup in epoll and the submit time.
    fences: HashMap<u64, (OwnedFd, Instant)>,
    /// Client buffers borrowed by those frames.
    pub borrows: Borrows,
    /// `BufferReleased`s held back while a frame samples the buffer.
    pub held: Vec<(ClientId, BufferKey)>,
    /// Submit-to-`Composited` samples, µs.
    pub composite_us: crate::stats::Window,
    /// Counters.
    pub counters: Counters,
    /// The helper's last `Stats` answer.
    pub last_stats: proto::Stats,
    /// The socket wants `EPOLLOUT`.
    want_out: bool,
    /// Last activity towards the helper that holds something (on-demand).
    pub busy_since_idle: bool,
}

impl std::fmt::Debug for Helper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Helper")
            .field("mode", &self.mode)
            .field("state", &self.state)
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

/// What a `run`-owned epoll needs: registration of the socket and fence
/// fds under tokens `lib.rs` chooses.
pub trait Poll {
    /// Add `fd` for reading (and writing with `out`) under `token`.
    fn add(&self, fd: BorrowedFd<'_>, token: u64, out: bool);
    /// Change the interest of `fd`.
    fn modify(&self, fd: BorrowedFd<'_>, token: u64, out: bool);
    /// Remove `fd`.
    fn remove(&self, fd: BorrowedFd<'_>);
}

impl Helper {
    /// A stopped helper.
    ///
    /// # Errors
    /// `timerfd_create`.
    pub fn new(
        mode: Mode,
        idle_exit: u32,
        path: Option<PathBuf>,
        spawner: Option<Spawner>,
    ) -> rustix::io::Result<Self> {
        let timer = rustix::time::timerfd_create(
            rustix::time::TimerfdClockId::Monotonic,
            rustix::time::TimerfdFlags::CLOEXEC | rustix::time::TimerfdFlags::NONBLOCK,
        )?;
        Ok(Self {
            mode,
            idle_exit,
            path,
            spawner,
            state: State::Off,
            conn: None,
            child: None,
            started: None,
            info: None,
            backoff: Backoff::default(),
            timer,
            hello_at: None,
            respawn_at: None,
            owner: None,
            ring: Ring::default(),
            texs: HashMap::new(),
            sources: HashMap::new(),
            next_tex: 1,
            next_serial: 1,
            fences: HashMap::new(),
            borrows: Borrows::default(),
            held: Vec::new(),
            composite_us: crate::stats::Window::new(crate::stats::PAINT_WINDOW),
            counters: Counters::default(),
            last_stats: proto::Stats::default(),
            want_out: false,
            busy_since_idle: false,
        })
    }

    /// The timerfd (backoff, `Hello` and hang deadlines).
    #[must_use]
    pub fn timer_fd(&self) -> BorrowedFd<'_> {
        self.timer.as_fd()
    }

    /// Whether mode 2 is configured at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.mode != Mode::Off
    }

    /// Whether it answered `Hello`.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.state == State::Ready
    }

    /// Whether the socket is connected (the helper is alive or starting).
    #[must_use]
    pub fn running(&self) -> bool {
        self.conn.is_some()
    }

    /// Change the configuration (a reload). Returns whether the mode
    /// changed.
    pub fn configure(&mut self, mode: Mode, idle_exit: u32) -> bool {
        self.idle_exit = idle_exit;
        std::mem::replace(&mut self.mode, mode) != mode
    }

    /// Start the helper, unless running, backing off or given up.
    pub fn spawn(&mut self, poll: &dyn Poll, token: u64) {
        if self.conn.is_some() || matches!(self.state, State::Backoff | State::GaveUp) {
            return;
        }
        if self.mode == Mode::Off {
            return;
        }
        let (ours, theirs) = match rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!("gpu helper: socketpair: {e}");
                return;
            }
        };
        self.counters.spawns += 1;
        if let Some(s) = self.spawner.clone() {
            (s.0)(theirs);
        } else {
            let path = self.helper_path();
            let mut cmd = Command::new(&path);
            cmd.stdin(Stdio::from(theirs));
            if self.mode == Mode::OnDemand {
                cmd.env("NITRO_GPU_IDLE_EXIT", self.idle_exit.to_string());
            }
            match cmd.spawn() {
                Ok(c) => {
                    info!("gpu helper {} started (pid {})", path.display(), c.id());
                    self.child = Some(c);
                }
                Err(e) => {
                    warn!("gpu helper {}: {e}", path.display());
                    self.started = Some(Instant::now());
                    self.died(false);
                    return;
                }
            }
        }
        let sock = match nitro_wire::Socket::from_fd(ours) {
            Ok(s) => s,
            Err(e) => {
                warn!("gpu helper: socket: {e}");
                return;
            }
        };
        let mut conn = nitro_gpu::Conn::new(sock);
        poll.add(conn.as_fd(), token, false);
        let hello = ToHelper::Hello {
            version: proto::PROTO_VERSION,
        };
        if let Err(e) = conn.send(&hello, Vec::new()) {
            warn!("gpu helper: Hello: {e}");
        }
        self.conn = Some(conn);
        self.state = State::Starting;
        self.started = Some(Instant::now());
        self.hello_at = Some(Instant::now() + HELLO_TIMEOUT);
        self.arm();
    }

    /// `NITRO_GPU_HELPER` (already in `path`), else `nitro-gpu-vulkan`
    /// next to the server binary, else `$PATH`.
    fn helper_path(&self) -> PathBuf {
        if let Some(p) = &self.path {
            return p.clone();
        }
        const NAME: &str = "nitro-gpu-vulkan";
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let p = dir.join(NAME);
            if p.exists() {
                return p;
            }
        }
        PathBuf::from(NAME)
    }

    /// Send a request. Returns false if the helper is not connected (or
    /// the send failed; death is noticed on the socket).
    pub fn send(&mut self, poll: &dyn Poll, token: u64, msg: &ToHelper, fds: Vec<OwnedFd>) -> bool {
        let Some(conn) = self.conn.as_mut() else {
            return false;
        };
        match conn.send(msg, fds) {
            Ok(()) => {}
            Err(nitro_wire::Error::Io(rustix::io::Errno::AGAIN)) => {}
            Err(e) => {
                debug!("gpu helper: send {}: {e}", msg.op());
                return false;
            }
        }
        self.update_out(poll, token);
        true
    }

    fn update_out(&mut self, poll: &dyn Poll, token: u64) {
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let pending = !conn.writer().is_empty();
        if pending != self.want_out {
            self.want_out = pending;
            poll.modify(conn.as_fd(), token, pending);
        }
    }

    /// The socket is readable (or writable, or hung up). Returns the
    /// decoded replies, and whether the helper is gone.
    pub fn pump(&mut self, poll: &dyn Poll, token: u64) -> (Vec<Reply>, bool) {
        let mut out = Vec::new();
        let Some(conn) = self.conn.as_mut() else {
            return (out, false);
        };
        let mut gone = false;
        if let Err(e) = conn.flush() {
            debug!("gpu helper: write: {e}");
            gone = true;
        }
        loop {
            match conn.read() {
                Ok(()) => {}
                Err(nitro_wire::Error::Io(rustix::io::Errno::AGAIN)) => {}
                Err(nitro_wire::Error::Closed) => gone = true,
                Err(e) => {
                    debug!("gpu helper: read: {e}");
                    gone = true;
                }
            }
            let mut any = false;
            loop {
                match conn.next_reply() {
                    Ok(Some((msg, fds))) => {
                        any = true;
                        if let Some(r) = Self::decode(&mut self.state, &mut self.info, &mut self.ring, &mut self.texs, &mut self.counters, &mut self.last_stats, msg, fds) {
                            out.push(r);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("gpu helper: bad reply: {e}");
                        gone = true;
                        break;
                    }
                }
            }
            if gone || !any {
                break;
            }
        }
        if out.iter().any(|r| matches!(r, Reply::Ready)) {
            self.hello_at = None;
            self.arm();
        }
        if !gone {
            self.update_out(poll, token);
        }
        (out, gone)
    }

    #[allow(clippy::too_many_arguments)] // disjoint borrows of self
    fn decode(
        state: &mut State,
        info_slot: &mut Option<DeviceInfo>,
        ring: &mut Ring,
        texs: &mut HashMap<BufferKey, Tex>,
        counters: &mut Counters,
        last_stats: &mut proto::Stats,
        msg: FromHelper,
        fds: Vec<OwnedFd>,
    ) -> Option<Reply> {
        match msg {
            FromHelper::HelloReply { version, info } => {
                if version != proto::PROTO_VERSION {
                    warn!("gpu helper speaks {version}");
                    return None;
                }
                info!(
                    "gpu helper ready: {} ({}), {} sampleable, {} render formats",
                    info.device,
                    info.driver,
                    info.sampleable.len(),
                    info.render.len()
                );
                *info_slot = Some(info);
                *state = State::Ready;
                Some(Reply::Ready)
            }
            FromHelper::Imported { .. } => None,
            FromHelper::Released { id } => {
                texs.retain(|_, t| t.id != id);
                None
            }
            FromHelper::OutputRing {
                w,
                h,
                fourcc,
                modifier,
                slots,
            } => Some(Reply::Ring {
                size: (w, h),
                fourcc,
                modifier,
                slots: slots.into_iter().zip(fds).collect(),
            }),
            FromHelper::Composited { serial } => {
                let fence = fds.into_iter().next()?;
                Some(Reply::Composited { serial, fence })
            }
            FromHelper::Error {
                op,
                what,
                code,
                msg,
            } => {
                debug!("gpu helper refused op {op:#x} ({what}): {code:?} {msg}");
                match op {
                    proto::op::COMPOSITE => Some(Reply::CompositeFailed { serial: what, code }),
                    proto::op::ALLOC_OUTPUT_RING => Some(Reply::RingFailed),
                    proto::op::IMPORT_SHADOW => {
                        ring.shadow = None;
                        Some(Reply::ShadowRefused)
                    }
                    proto::op::IMPORT_DMABUF => {
                        counters.import_refused += 1;
                        let id = u32::try_from(what).unwrap_or(0);
                        for t in texs.values_mut() {
                            if t.id == id {
                                t.refused = true;
                            }
                        }
                        None
                    }
                    _ => None,
                }
            }
            FromHelper::Stats(s) => {
                *last_stats = s;
                None
            }
            FromHelper::ReadBackReply { .. } => None,
        }
    }

    /// The helper is gone (EOF, a failed spawn, a hang we killed). Returns
    /// whether it was a crash (anything but a clean on-demand idle exit
    /// while holding nothing).
    pub fn died(&mut self, idle_ok: bool) -> bool {
        let mut clean = false;
        if let Some(mut c) = self.child.take() {
            // Give it a moment to reap: EOF usually precedes the exit.
            let deadline = Instant::now() + Duration::from_millis(50);
            let status = loop {
                match c.try_wait() {
                    Ok(Some(s)) => break Some(s),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    _ => break None,
                }
            };
            if status.is_none() {
                let _ = c.kill();
                let _ = c.wait();
            }
            clean = status.is_some_and(|s| s.success());
        } else if self.spawner.is_some() {
            clean = true;
        }
        self.conn = None;
        self.want_out = false;
        self.info = None;
        self.hello_at = None;
        self.texs.clear();
        // Their fences error or signal as the driver lets go; the
        // borrows go when they do (`fence_signalled`), or now if no fence
        // was kept.
        self.ring = Ring::default();
        let up_for = self.started.take().map_or(Duration::ZERO, |t| t.elapsed());
        let crash = !(idle_ok && clean && self.mode == Mode::OnDemand);
        if !crash {
            info!("gpu helper exited idle");
            self.state = State::Off;
            return false;
        }
        self.counters.crashes += 1;
        if self.mode == Mode::Off {
            self.state = State::Off;
            return true;
        }
        match self.backoff.crashed(Instant::now(), up_for) {
            Some(d) => {
                warn!("gpu helper died; restarting in {} ms", d.as_millis());
                self.state = State::Backoff;
                self.respawn_at = Some(Instant::now() + d);
            }
            None => {
                warn!(
                    "gpu helper died {GIVE_UP_CRASHES} times in {} s; giving up until a VT switch or SIGHUP",
                    GIVE_UP_WINDOW.as_secs()
                );
                self.state = State::GaveUp;
                self.respawn_at = None;
            }
        }
        self.arm();
        true
    }

    /// Stop it deliberately (VT switch away, `gpu.helper = off`, shutdown):
    /// `Shutdown`, then drop the socket. Not a crash.
    pub fn stop(&mut self, poll: &dyn Poll, token: u64) {
        if let Some(mut conn) = self.conn.take() {
            let _ = conn.send(&ToHelper::Shutdown, Vec::new());
            poll.remove(conn.as_fd());
        }
        if let Some(mut c) = self.child.take() {
            // It exits on EOF; reap without blocking the compositor for
            // long, and make sure.
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                if matches!(c.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            if !matches!(c.try_wait(), Ok(Some(_))) {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
        let _ = token;
        self.want_out = false;
        self.info = None;
        self.hello_at = None;
        self.respawn_at = None;
        self.texs.clear();
        self.ring = Ring::default();
        self.started = None;
        self.state = State::Off;
        self.arm();
    }

    /// Forget the give-up (VT resume, SIGHUP).
    pub fn forgive(&mut self) {
        self.backoff.reset();
        if matches!(self.state, State::GaveUp | State::Backoff) {
            self.state = State::Off;
            self.respawn_at = None;
        }
    }

    /// The timer fired: returns `(respawn now, helper hung or silent)`.
    pub fn on_timer(&mut self) -> (bool, bool) {
        let mut buf = [0u8; 8];
        let _ = rustix::io::read(&self.timer, &mut buf);
        let now = Instant::now();
        let respawn = self.respawn_at.is_some_and(|t| t <= now);
        if respawn {
            self.respawn_at = None;
            self.state = State::Off;
        }
        let hung = self.hello_at.is_some_and(|t| t <= now)
            || self
                .ring
                .in_flight
                .as_ref()
                .is_some_and(|f| now.duration_since(f.sent) >= HANG_TIMEOUT);
        self.arm();
        (respawn, hung)
    }

    /// Arm the timer for the earliest deadline.
    pub fn arm(&self) {
        let hang = self.ring.in_flight.as_ref().map(|f| f.sent + HANG_TIMEOUT);
        let next = [self.hello_at, self.respawn_at, hang]
            .into_iter()
            .flatten()
            .min();
        let value = next.map_or(Duration::ZERO, |t| {
            t.saturating_duration_since(Instant::now())
                .max(Duration::from_micros(1))
        });
        let spec = rustix::time::Itimerspec {
            it_interval: rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: rustix::time::Timespec {
                tv_sec: i64::try_from(value.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(value.subsec_nanos()),
            },
        };
        if let Err(e) = rustix::time::timerfd_settime(
            &self.timer,
            rustix::time::TimerfdTimerFlags::empty(),
            &spec,
        ) {
            warn!("gpu helper timer: {e}");
        }
    }

    /// Kill a hung helper; the caller then treats it as dead.
    pub fn kill(&mut self, poll: &dyn Poll) {
        warn!("gpu helper not answering; killing it");
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
        }
        if let Some(conn) = self.conn.as_ref() {
            poll.remove(conn.as_fd());
        }
    }

    /// Whether the helper can sample `fourcc`/`modifier`.
    #[must_use]
    pub fn samples(&self, fourcc: u32, modifier: u64) -> bool {
        self.info.as_ref().is_some_and(|i| {
            i.sampleable
                .iter()
                .any(|f| f.fourcc == fourcc && f.modifier == modifier)
        })
    }

    /// The render modifiers for `fourcc` the primary also scans out.
    #[must_use]
    pub fn ring_modifiers(&self, fourcc: u32, primary: &[u64]) -> Vec<u64> {
        self.info.as_ref().map_or_else(Vec::new, |i| {
            i.render
                .iter()
                .filter(|f| f.fourcc == fourcc && primary.contains(&f.modifier))
                .map(|f| f.modifier)
                .take(proto::MAX_MODIFIERS)
                .collect()
        })
    }

    /// Whether `key`'s import was refused.
    #[must_use]
    pub fn refused(&self, key: BufferKey) -> bool {
        self.texs.get(&key).is_some_and(|t| t.refused)
    }

    /// The texture for `key`, importing it first (the import is ordered
    /// before any `Composite` on the socket, so it is usable at once).
    /// `None` if there is no source or it was refused.
    pub fn texture(
        &mut self,
        poll: &dyn Poll,
        token: u64,
        key: BufferKey,
        encoding: proto::ColorEncoding,
        range: proto::ColorRange,
    ) -> Option<u32> {
        if let Some(t) = self.texs.get(&key) {
            return (!t.refused).then_some(t.id);
        }
        let src = self.sources.get(&key)?;
        let mut fds = Vec::with_capacity(src.fds.len());
        for fd in &src.fds {
            fds.push(fd.try_clone().ok()?);
        }
        let base = src.desc.clone();
        let id = self.tex_id();
        let desc = DmabufDesc {
            id,
            encoding,
            range,
            ..base
        };
        if !self.send(poll, token, &ToHelper::ImportDmabuf(desc), fds) {
            return None;
        }
        self.texs.insert(key, Tex { id, refused: false });
        Some(id)
    }

    /// Release the textures of buffers `alive` says are gone, and drop
    /// their sources.
    pub fn prune(&mut self, poll: &dyn Poll, token: u64, alive: impl Fn(BufferKey) -> bool) {
        self.sources.retain(|k, _| alive(*k));
        let dead: Vec<(BufferKey, u32)> = self
            .texs
            .iter()
            .filter(|(k, _)| !alive(**k))
            .map(|(k, t)| (*k, t.id))
            .collect();
        for (k, id) in dead {
            self.texs.remove(&k);
            self.send(poll, token, &ToHelper::Release { id }, Vec::new());
        }
    }

    /// Release every texture (leaving mode 2 on demand: lets it idle-exit).
    pub fn release_all(&mut self, poll: &dyn Poll, token: u64) {
        let ids: Vec<u32> = self.texs.values().map(|t| t.id).collect();
        self.texs.clear();
        for id in ids {
            self.send(poll, token, &ToHelper::Release { id }, Vec::new());
        }
    }

    /// Textures held.
    #[must_use]
    pub fn textures(&self) -> usize {
        self.texs.len()
    }

    /// A fresh texture id.
    pub fn tex_id(&mut self) -> u32 {
        let id = self.next_tex;
        self.next_tex = self.next_tex.wrapping_add(1).max(1);
        id
    }

    /// Whether frame `serial`'s fence is still out.
    #[must_use]
    pub fn fence_pending(&self, serial: u64) -> bool {
        self.fences.contains_key(&serial)
    }

    /// A fresh frame serial.
    pub fn serial(&mut self) -> u64 {
        let s = self.next_serial;
        self.next_serial += 1;
        s
    }

    /// Frame `serial`'s fence: keep a dup (in epoll under `token`) until
    /// it signals.
    pub fn keep_fence(&mut self, poll: &dyn Poll, token: u64, serial: u64, fence: &OwnedFd, sent: Instant) {
        match fence.try_clone() {
            Ok(dup) => {
                poll.add(dup.as_fd(), token, false);
                self.fences.insert(serial, (dup, sent));
            }
            Err(_) => self.borrows.done(serial),
        }
    }

    /// Frame `serial`'s fence signalled (or errored).
    pub fn fence_signalled(&mut self, poll: &dyn Poll, serial: u64) {
        if let Some((fd, sent)) = self.fences.remove(&serial) {
            poll.remove(fd.as_fd());
            self.counters.busy_us += sent.elapsed().as_micros() as u64;
        }
        self.borrows.done(serial);
    }

    /// Frames whose fence is still pending.
    #[must_use]
    pub fn fences_pending(&self) -> usize {
        self.fences.len()
    }

    /// Ask for `Stats` (answered later; the helper is non-dumpable, so
    /// this is the only way to its RSS).
    pub fn query_stats(&mut self, poll: &dyn Poll, token: u64) {
        if self.ready() {
            self.send(poll, token, &ToHelper::GetStats, Vec::new());
        }
    }

    /// Frames whose borrows have not been returned (for the HUP path):
    /// every held fence is dropped from epoll and its borrows forgotten.
    pub fn drop_fences(&mut self, poll: &dyn Poll) {
        for (_, (fd, _)) in self.fences.drain() {
            poll.remove(fd.as_fd());
        }
        self.borrows.clear();
    }

    /// Serials whose fence dup is registered (for `lib.rs` to test
    /// without blocking after a death).
    #[must_use]
    pub fn fence_serials(&self) -> Vec<u64> {
        self.fences.keys().copied().collect()
    }

    /// Whether the helper holds nothing (on-demand idle).
    #[must_use]
    pub fn idle(&self) -> bool {
        self.texs.is_empty() && self.fences.is_empty() && self.owner.is_none()
    }

    /// The textures' buffer keys.
    #[must_use]
    pub fn texture_keys(&self) -> HashSet<BufferKey> {
        self.texs.keys().copied().collect()
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            if let Some(mut conn) = self.conn.take() {
                let _ = conn.send(&ToHelper::Shutdown, Vec::new());
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Every descriptor ≥ 3 of this process that is **not** close-on-exec,
/// read from `/proc/self/fdinfo` (no `unsafe`): a spawned helper would
/// inherit it, and refuses to start if it is a DRM primary node or an
/// input device.
#[must_use]
pub fn inheritable_fds() -> Vec<(i32, String)> {
    const O_CLOEXEC: u32 = 0o2_000_000;
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/fdinfo") else {
        return out;
    };
    for e in dir.flatten() {
        let Some(n) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if n < 3 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let flags = text
            .lines()
            .find_map(|l| l.strip_prefix("flags:"))
            .and_then(|v| u32::from_str_radix(v.trim(), 8).ok());
        if flags.is_some_and(|f| f & O_CLOEXEC == 0) {
            let target = std::fs::read_link(format!("/proc/self/fd/{n}"))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push((n, target));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_gives_up() {
        let mut b = Backoff::default();
        let t0 = Instant::now();
        let d: Vec<_> = (0..4)
            .map(|i| b.crashed(t0 + Duration::from_secs(i), Duration::ZERO))
            .collect();
        assert_eq!(
            d,
            vec![
                Some(BACKOFF_MIN),
                Some(BACKOFF_MIN * 2),
                Some(BACKOFF_MIN * 4),
                Some(BACKOFF_MIN * 8)
            ]
        );
        assert_eq!(b.crashed(t0 + Duration::from_secs(5), Duration::ZERO), None);
        // Spread out over more than the window: never gives up; and a
        // long healthy run resets the delay.
        let mut b = Backoff::default();
        for i in 0..20u64 {
            let d = b.crashed(t0 + Duration::from_secs(i * 100), HEALTHY);
            assert_eq!(d, Some(BACKOFF_MIN), "{i}");
        }
        let mut b = Backoff::default();
        for i in 0..12u64 {
            assert!(b.crashed(t0 + Duration::from_secs(i * 80), Duration::ZERO).is_some());
        }
        assert_eq!(b.next, Some(BACKOFF_MAX));
    }

    #[test]
    fn slots_pick_the_first_free() {
        let slot = |fb, state, last| Slot {
            fb: BufferId(fb),
            state,
            last,
        };
        let mut s = vec![
            slot(1, SlotState::Shown, Some(1)),
            slot(2, SlotState::Submitted(3), Some(3)),
            slot(3, SlotState::Free, Some(2)),
        ];
        assert_eq!(pick_free(&s, |_| false), Some(2));
        // Its last frame still running: not yet.
        assert_eq!(pick_free(&s, |l| l == 2), None);
        s[2].state = SlotState::Shown;
        assert_eq!(pick_free(&s, |_| false), None);
    }

    #[test]
    fn borrows_hold_until_their_frame_is_done() {
        let (a, b) = (BufferKey::from_parts(1, 1), BufferKey::from_parts(2, 1));
        let mut br = Borrows::default();
        br.add(1, vec![a]);
        br.add(2, vec![a, b]);
        assert!(br.holds(a) && br.holds(b));
        br.done(2);
        assert!(br.holds(a));
        br.done(1);
        assert!(!br.holds(a) && br.is_empty());
    }

    #[test]
    fn states_number_as_documented() {
        let n: Vec<u64> = [
            State::Off,
            State::Starting,
            State::Ready,
            State::Backoff,
            State::GaveUp,
        ]
        .iter()
        .map(|s| s.number())
        .collect();
        assert_eq!(n, vec![0, 1, 2, 3, 4]);
    }
}
