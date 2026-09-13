//! `nitro-server` — the display server.
//!
//! One thread, one epoll, a seat, a KMS backend, a [`nitro_scene`] scene
//! graph, [`nitro_raster`] for pixels, [`nitro_wire`] for clients and
//! libinput/xkbcommon for input. [`run`] takes a [`Config`] and returns
//! when asked to quit, so tests drive the whole loop in-process against
//! [`nitro_kms::FakeBackend`] with a [`input::FakeSource`]; `main.rs` only
//! turns environment variables into a `Config`.
//!
//! Event loop (level-triggered epoll, no timers — an idle server never
//! wakes):
//!
//! | fd                 | on readable                                         |
//! |--------------------|-----------------------------------------------------|
//! | seat               | `Seat::dispatch`: `Disable` → suspend input, pause, ack; `Enable` → resume, full repaint |
//! | backend `poll_fds` | `Backend::dispatch`: `Flipped` → present + paint the next frame, `Hotplug` → rescan |
//! | libinput           | dispatch, convert, route, paint if anything moved   |
//! | signal self-pipe   | SIGTERM/SIGINT → orderly shutdown                    |
//! | control listener   | accept, register client (v0 line protocol)           |
//! | wire listener      | accept, register client (`nitro-wire` v1)            |
//! | wire client        | read, buffer mutations, apply on `Commit`            |
//!
//! # Why one thread is still enough
//!
//! Every wakeup does a bounded amount of work: a client read is capped by
//! `nitro-wire`'s read budget, a frame's paint is proportional to its
//! damage, and nothing blocks. The only unbounded thing would be a client
//! that never stops sending, and the read budget is exactly the bound on
//! that. Fanning rasterization out to a worker pool is a decision for the
//! day a frame's damage stops fitting in a vblank; it is not one yet.
//!
//! Drop order on shutdown is clients → sockets → input → backend → DRM
//! device → seat; the `Server` fields are declared in exactly that order.

pub mod clients;
pub mod control;
pub mod cursor;
pub mod frame;
pub mod input;
pub mod keyboard;
pub mod logging;
pub mod protocol;
pub mod render;
pub mod signals;
pub mod stats;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use nitro_core::{Damage, Point, Size};
use nitro_kms::{
    Backend, DrmBackend, DrmOptions, Error as KmsError, Event, FakeBackend,
    OutputId as KmsOutputId, OutputInfo, Rect as KmsRect,
};
use nitro_scene::{ClientId, DamageSink, OutputId as SceneOutputId, Scene, WindowKey};
use nitro_seat::{Device, Seat, SeatEvent};
use nitro_wire::msg::{self, ClientMsg, ServerMsg};
use nitro_wire::server::Listener as WireListener;
use nitro_wire::types::{ButtonState, ErrorCode, NodeId};
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::clients::{ApplyError, BufferSource, Pending, WireClient};
use crate::control::{Client, ReadOutcome};
use crate::cursor::Cursor;
use crate::frame::{CursorState, OutputState};
use crate::input::{InputEvent, InputSource, LibinputSource, Pointer};
use crate::keyboard::{Hotkey, Keyboard};
use crate::protocol::Request;
use crate::stats::FrameStats;

/// Server name reported in `Welcome`.
pub const SERVER_NAME: &str = "nitro";

/// Which display backend to run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// Real KMS through the seat. `card` overrides device selection
    /// (otherwise the first `/dev/dri/card*` with a connected output).
    Drm {
        /// Explicit device path.
        card: Option<PathBuf>,
    },
    /// Headless [`FakeBackend`] with one output; no seat at all.
    Fake {
        /// Output width.
        width: u32,
        /// Output height.
        height: u32,
    },
    /// Headless [`FakeBackend`] with **no** output, which a test plugs one
    /// into later. The state a real server is in between "started" and
    /// "the connector reported a mode", and the only way to exercise a
    /// window created before there is anywhere to put it.
    FakeHeadless,
}

/// Everything [`run`] needs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Display backend.
    pub backend: BackendKind,
    /// Control socket path (see [`control::resolve`]).
    pub control_path: PathBuf,
    /// Wire socket path; clients find it through `NITRO_SOCKET`.
    pub wire_path: PathBuf,
    /// Install SIGTERM/SIGINT handlers. Tests turn this off.
    pub handle_signals: bool,
    /// Directory scanned for `event*` devices. `None` disables input
    /// altogether, which is what the fake backend does.
    pub input_dir: Option<PathBuf>,
    /// A test's injection point, used instead of libinput when set.
    pub fake_input: Option<input::FakeInput>,
}

impl Config {
    /// The fake backend at `width × height`, sockets next to `path`, no
    /// signal handlers and no input devices. What tests want.
    #[must_use]
    pub fn fake(width: u32, height: u32, path: impl Into<PathBuf>) -> Self {
        let control_path: PathBuf = path.into();
        let wire_path = control_path.with_file_name("wire.sock");
        Self {
            backend: BackendKind::Fake { width, height },
            control_path,
            wire_path,
            handle_signals: false,
            input_dir: None,
            fake_input: None,
        }
    }
}

/// Anything that stops the server.
#[derive(Debug)]
pub enum Error {
    /// libseat failed.
    Seat(nitro_seat::Error),
    /// The display backend failed.
    Kms(KmsError),
    /// A system call failed; `op` says what we were doing.
    Io {
        /// What we were doing.
        op: &'static str,
        /// The underlying error.
        source: io::Error,
    },
    /// No usable DRM device was found.
    NoDevice(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Seat(e) => write!(f, "seat: {e}"),
            Error::Kms(e) => write!(f, "kms: {e}"),
            Error::Io { op, source } => write!(f, "{op}: {source}"),
            Error::NoDevice(why) => write!(f, "no usable DRM device: {why}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Seat(e) => Some(e),
            Error::Kms(e) => Some(e),
            Error::Io { source, .. } => Some(source),
            Error::NoDevice(_) => None,
        }
    }
}

impl From<nitro_seat::Error> for Error {
    fn from(e: nitro_seat::Error) -> Self {
        Error::Seat(e)
    }
}

impl From<KmsError> for Error {
    fn from(e: KmsError) -> Self {
        Error::Kms(e)
    }
}

fn io_err(op: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::Io { op, source }
}

fn errno(op: &'static str) -> impl FnOnce(rustix::io::Errno) -> Error {
    move |e| Error::Io {
        op,
        source: e.into(),
    }
}

// epoll tokens
const TOK_SEAT: u64 = 0;
const TOK_SIGNALS: u64 = 1;
const TOK_LISTENER: u64 = 2;
const TOK_BACKEND: u64 = 3;
const TOK_WIRE_LISTENER: u64 = 4;
const TOK_INPUT: u64 = 5;
/// How long an unanswered input keeps waiting for a frame to claim it.
/// Beyond this the number would not be a latency any more: nothing
/// responded to the event, and attributing the next unrelated frame to it
/// is how the histogram once reported 33 seconds.
const INPUT_STAMP_MAX_AGE_NS: u64 = 200_000_000;

const TOK_CLIENT_BASE: u64 = 1 << 32;
const TOK_WIRE_BASE: u64 = 1 << 33;

/// Flip-interval statistics for `stats` and the log.
#[derive(Debug, Default)]
struct FlipStats {
    last: Option<Duration>,
    count: u64,
    sum: Duration,
    min: Option<Duration>,
    max: Duration,
}

/// Intervals longer than this many refresh periods are not counted: the
/// server deliberately stops flipping when nothing changes, so the gap
/// across an idle stretch is the *absence* of frames, not a slow one.
/// Including it would make the figure measure how long the desktop sat
/// still rather than how evenly it paces when it is painting.
const FLIP_INTERVAL_MAX_PERIODS: u32 = 4;

impl FlipStats {
    fn record(&mut self, t: Duration, refresh_ns: u32) {
        if let Some(prev) = self.last {
            let iv = t.saturating_sub(prev);
            let cap =
                Duration::from_nanos(u64::from(refresh_ns) * u64::from(FLIP_INTERVAL_MAX_PERIODS));
            if iv <= cap {
                self.count += 1;
                self.sum += iv;
                self.min = Some(self.min.map_or(iv, |m| m.min(iv)));
                self.max = self.max.max(iv);
            }
        }
        self.last = Some(t);
    }

    fn mean_us(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            (self.sum.as_micros() / u128::from(self.count)) as u64
        }
    }
}

/// Unlinks a socket file on drop.
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("removing {}: {e}", self.0.display());
        }
    }
}

/// The running server. Field order is drop order: clients first, then the
/// sockets, then input (whose device fds belong to the seat), then the
/// backend (which holds a dup of the DRM fd), then the seat's `Device`, and
/// the seat last.
struct Server {
    wire_clients: HashMap<u64, WireClient>,
    clients: HashMap<u64, Client>,
    wire_listener: WireListener,
    listener: UnixListener,
    // Held for its drop side effect only; the wire listener unlinks itself.
    _socket_file: SocketFile,
    signals: Option<signals::Signals>,
    /// Input, which owns the seat `Device`s behind libinput's interface;
    /// dropping it closes them through the seat, so it must come before
    /// the seat in this struct's field order.
    input: Box<dyn InputSource>,
    backend: Box<dyn Backend>,
    _device: Option<Device>,
    seat: Option<Rc<RefCell<Seat>>>,
    epoll: OwnedFd,

    scene: Scene,
    outputs: Vec<OutputState>,
    keyboard: Option<Keyboard>,
    cursor: Cursor,
    pointer: Pointer,
    /// The window with keyboard focus, if any.
    focus: Option<WindowKey>,
    /// Which window each live touch point started on, and where it is in
    /// that window's coordinates.
    touch_targets: HashMap<i32, (WindowKey, Point)>,
    /// Buffers' descriptors and fds, so `BufferDamage` can re-read rows.
    buffer_sources: HashMap<(ClientId, nitro_scene::BufferKey), BufferSource>,
    /// Descriptors from `CreateBuffer`s in a transaction that has not
    /// committed yet; they move into `buffer_sources` when it does.
    pending_fds: Vec<(
        u64,
        nitro_wire::types::BufferId,
        OwnedFd,
        nitro_scene::BufferDesc,
    )>,
    /// Windows created so far, for the cascade.
    windows_created: u32,
    /// Windows created while no output existed, waiting for one.
    unplaced: Vec<(ClientId, WindowKey)>,
    /// Newest input timestamp not yet consumed by a frame; see
    /// [`Server::note_input`].
    pending_input_ns: u64,

    next_client: u64,
    next_wire: u64,
    next_client_id: u32,
    active: bool,
    quit: bool,
    frames: u64,
    started: Instant,
    flips: FlipStats,
    stats: FrameStats,
    events: Vec<Event>,
    input_events: Vec<InputEvent>,
    paint_items: Vec<nitro_scene::PaintItem>,
}

/// Run the server until `quit`, SIGTERM or SIGINT.
///
/// # Errors
/// Anything fatal at startup (no seat, no device, socket in use) or in
/// the loop (I/O on the epoll or DRM fd).
#[allow(clippy::too_many_lines)] // Startup is a sequence, not a structure: splitting it would only scatter the ordering rules it enforces.
pub fn run(mut config: Config) -> Result<(), Error> {
    let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(errno("epoll_create"))?;
    let signals = if config.handle_signals {
        let s = signals::Signals::install().map_err(io_err("install signal handlers"))?;
        add(&epoll, &s, TOK_SIGNALS)?;
        Some(s)
    } else {
        None
    };

    // Declared before `device`/`backend` so an early `?` drops it last.
    let mut seat: Option<Rc<RefCell<Seat>>> = None;
    let mut device: Option<Device> = None;
    let backend: Box<dyn Backend> = match &config.backend {
        BackendKind::Fake { width, height } => {
            info!("fake backend {width}x{height}");
            Box::new(FakeBackend::single(*width, *height).map_err(io_err("create fake backend"))?)
        }
        BackendKind::FakeHeadless => {
            info!("fake backend with no outputs");
            Box::new(FakeBackend::new(&[]).map_err(io_err("create fake backend"))?)
        }
        BackendKind::Drm { card } => {
            let mut s = Seat::open()?;
            info!("seat {:?} opened, active={}", s.name(), s.is_active());
            add(&epoll, &s, TOK_SEAT)?;
            if !wait_active(&epoll, &mut s, signals.is_some())? {
                info!("interrupted while waiting for the seat; exiting");
                return Ok(());
            }
            let (dev, be) = open_card(&mut s, card.as_deref())?;
            seat = Some(Rc::new(RefCell::new(s)));
            device = Some(dev);
            be
        }
    };

    let input: Box<dyn InputSource> = match (&seat, config.input_dir.as_deref()) {
        (_, _) if config.fake_input.is_some() => {
            let Some(handle) = config.fake_input.take() else {
                unreachable!("guarded by the match arm")
            };
            info!("fake input source");
            Box::new(input::FakeSource::new(handle))
        }
        (Some(seat), Some(dir)) => {
            let src = LibinputSource::open(Rc::clone(seat), dir);
            if src.is_empty() {
                warn!("no input devices in {}", dir.display());
            }
            info!("{}", src.describe());
            Box::new(src)
        }
        _ => {
            info!("no input devices (fake backend or input disabled)");
            Box::new(input::FakeSource::idle().map_err(|e| Error::Io {
                op: "create the idle input source",
                source: e.into(),
            })?)
        }
    };
    for fd in input.poll_fds() {
        add(&epoll, &fd, TOK_INPUT)?;
    }

    let keyboard = Keyboard::new();
    match &keyboard {
        Some(kb) => info!("xkb keymap: {}", kb.layout_names().join(", ")),
        None => warn!("no xkb keymap compiled; keys carry no keysym or text"),
    }

    let listener = control::bind(&config.control_path).map_err(io_err("bind control socket"))?;
    add(&epoll, &listener, TOK_LISTENER)?;
    info!("control socket at {}", config.control_path.display());

    let wire_listener = WireListener::bind(&config.wire_path).map_err(|e| Error::Io {
        op: "bind wire socket",
        source: io::Error::other(e.to_string()),
    })?;
    add(&epoll, &wire_listener.as_fd(), TOK_WIRE_LISTENER)?;
    info!("wire socket at {}", config.wire_path.display());

    let mut server = Server {
        wire_clients: HashMap::new(),
        clients: HashMap::new(),
        wire_listener,
        listener,
        _socket_file: SocketFile(config.control_path.clone()),
        signals,
        input,
        backend,
        _device: device,
        seat,
        epoll,
        scene: Scene::new(),
        outputs: Vec::new(),
        keyboard,
        cursor: Cursor::new(),
        pointer: Pointer::default(),
        focus: None,
        touch_targets: HashMap::new(),
        buffer_sources: HashMap::new(),
        pending_fds: Vec::new(),
        windows_created: 0,
        unplaced: Vec::new(),
        pending_input_ns: 0,
        next_client: 0,
        next_wire: 0,
        next_client_id: 1,
        active: true,
        quit: false,
        frames: 0,
        started: Instant::now(),
        flips: FlipStats::default(),
        stats: FrameStats::new(),
        events: Vec::new(),
        input_events: Vec::new(),
        paint_items: Vec::new(),
    };
    server.register_backend()?;
    server.sync_outputs();
    server.paint_all();
    let result = server.event_loop();
    info!(
        "shutting down after {} frames ({} wire client(s), {} control client(s))",
        server.frames,
        server.wire_clients.len(),
        server.clients.len()
    );
    drop(server);
    result
}

/// An epoll event buffer. `epoll::Event` has no `Default`, so build one.
fn event_buffer<const N: usize>() -> [epoll::Event; N] {
    [epoll::Event {
        flags: EventFlags::empty(),
        data: EventData::new_u64(0),
    }; N]
}

/// `epoll_wait` that retries on `EINTR`: the signal handler interrupts it
/// (the kernel never restarts `epoll_wait`), and the self-pipe is what
/// should end the loop, not the interruption. Returns the ready count.
fn wait(epoll: &OwnedFd, buf: &mut [epoll::Event]) -> Result<usize, Error> {
    loop {
        match epoll::wait(epoll, &mut *buf, None) {
            Ok(n) => return Ok(n),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(errno("epoll_wait")(e)),
        }
    }
}

/// `CLOCK_MONOTONIC` now, in nanoseconds — the same clock libinput stamps
/// its events with and KMS reports vblanks on, so the three are directly
/// comparable and the latency figures mean what they say.
fn monotonic_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(t.tv_nsec).unwrap_or(0)
}

fn add(epoll: &OwnedFd, fd: &impl AsFd, token: u64) -> Result<(), Error> {
    epoll::add(epoll, fd, EventData::new_u64(token), EventFlags::IN).map_err(errno("epoll_ctl add"))
}

/// Block until the seat reports active. Returns `false` if a signal
/// arrived first (only possible when handlers are installed).
///
/// libseat queues the initial `Enable` inside `open_seat` without making
/// the fd readable, so dispatch once before waiting on epoll.
fn wait_active(epoll: &OwnedFd, seat: &mut Seat, signals: bool) -> Result<bool, Error> {
    let mut buf = event_buffer::<8>();
    drain_seat(seat)?;
    while !seat.is_active() {
        info!("waiting for the seat to become active");
        let n = wait(epoll, &mut buf)?;
        for ev in &buf[..n] {
            match ev.data.u64() {
                TOK_SIGNALS if signals => return Ok(false),
                TOK_SEAT => drain_seat(seat)?,
                _ => {}
            }
        }
    }
    Ok(true)
}

fn drain_seat(seat: &mut Seat) -> Result<(), Error> {
    for e in seat.dispatch()? {
        info!("seat event {e:?} (before device open)");
        if e == SeatEvent::Disable {
            seat.ack_disable()?;
        }
    }
    Ok(())
}

/// DRM device candidates: the override or `/dev/dri/card*` sorted.
fn card_candidates(card: Option<&Path>) -> Vec<PathBuf> {
    if let Some(c) = card {
        return vec![c.to_path_buf()];
    }
    let mut cards: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("card"))
                })
                .collect()
        })
        .unwrap_or_default();
    cards.sort();
    cards
}

/// Open the first candidate with a connected output; fall back to the
/// first that opens at all (hotplug may bring an output later).
fn open_card(seat: &mut Seat, card: Option<&Path>) -> Result<(Device, Box<dyn Backend>), Error> {
    let mut fallback: Option<PathBuf> = None;
    let mut last_err = String::from("no /dev/dri/card* found");
    for path in card_candidates(card) {
        match try_open(seat, &path) {
            Ok((dev, be)) if !be.outputs().is_empty() || card.is_some() => {
                return Ok((dev, be));
            }
            Ok((dev, be)) => {
                info!("{}: opens but has no connected output", path.display());
                drop(be);
                seat.close_device(dev)?;
                fallback.get_or_insert(path);
            }
            Err(e) => {
                warn!("{}: {e}", path.display());
                last_err = e.to_string();
            }
        }
    }
    if let Some(path) = fallback {
        warn!("no card has a connected output; using {}", path.display());
        return try_open(seat, &path);
    }
    Err(Error::NoDevice(last_err))
}

fn try_open(seat: &mut Seat, path: &Path) -> Result<(Device, Box<dyn Backend>), Error> {
    let dev = seat.open_device(path)?;
    // The backend gets its own fd (a dup sharing the open file
    // description, hence DRM master) so it can be `'static`; the seat's
    // fd is closed through `close_device` after the backend is gone.
    let fd = dev
        .as_fd()
        .try_clone_to_owned()
        .map_err(io_err("dup DRM fd"))?;
    match DrmBackend::open(fd, &DrmOptions::default()) {
        Ok(be) => {
            if let Some(e) = be.hotplug_error() {
                warn!("hotplug disabled: {e}");
            }
            info!(
                "opened {} with {} output(s)",
                path.display(),
                be.outputs().len()
            );
            Ok((dev, Box::new(be)))
        }
        Err(e) => {
            seat.close_device(dev)?;
            Err(e.into())
        }
    }
}

impl Server {
    fn register_backend(&mut self) -> Result<(), Error> {
        for fd in self.backend.poll_fds() {
            add(&self.epoll, &fd, TOK_BACKEND)?;
        }
        Ok(())
    }

    fn unregister_backend(&mut self) {
        for fd in self.backend.poll_fds() {
            if let Err(e) = epoll::delete(&self.epoll, fd) {
                warn!("epoll_ctl del backend fd: {e}");
            }
        }
    }

    /// Bring the scene's outputs and the per-output frame state in line
    /// with the backend's, side by side along the x axis. Multi-output
    /// layout is M3; a row is the arrangement that needs no policy.
    fn sync_outputs(&mut self) {
        let infos: Vec<OutputInfo> = self.backend.outputs().to_vec();
        self.outputs.retain(|o| {
            let keep = infos.iter().any(|i| i.id == o.kms_id);
            if !keep {
                info!("{} gone", o.kms_id);
                self.scene.remove_output(o.scene_id);
            }
            keep
        });
        let mut x = 0;
        for info in &infos {
            let scene_id = SceneOutputId(info.id.0);
            let rect =
                nitro_core::IRect::new(x, 0, info.width.cast_signed(), info.height.cast_signed());
            x += info.width.cast_signed();
            self.scene.add_output(scene_id, rect, 1.0);
            if let Some(existing) = self.outputs.iter_mut().find(|o| o.kms_id == info.id) {
                existing.width = info.width;
                existing.height = info.height;
                existing.refresh_ns = frame::refresh_ns(info.refresh_mhz);
                existing.invalidate();
                continue;
            }
            info!(
                "{} {}: {}x{}@{}.{:03} Hz",
                info.id,
                info.name,
                info.width,
                info.height,
                info.refresh_mhz / 1000,
                info.refresh_mhz % 1000
            );
            self.outputs.push(OutputState::new(
                info.id,
                scene_id,
                info.width,
                info.height,
                info.refresh_mhz,
            ));
        }
        // Windows created while there was no output can be placed now.
        // `place_new_window` re-queues any that still cannot be, so an
        // output list that went empty again leaves them waiting rather
        // than dropping them.
        if !self.outputs.is_empty() && !self.unplaced.is_empty() {
            let waiting = std::mem::take(&mut self.unplaced);
            info!("placing {} window(s) held for an output", waiting.len());
            for (client_id, win) in waiting {
                let Some(token) = self
                    .wire_clients
                    .iter()
                    .find(|(_, c)| c.id == client_id)
                    .map(|(t, _)| *t)
                else {
                    continue;
                };
                let Some(mut client) = self.wire_clients.remove(&token) else {
                    continue;
                };
                if let Some(node_id) = client.window_id(win) {
                    self.place_new_window(&mut client, node_id, win);
                }
                self.wire_clients.insert(token, client);
            }
            self.flush_wire_clients();
        }
        // Park the pointer in the middle of the first output, so its first
        // motion starts somewhere sensible rather than at (0, 0) under the
        // desktop frame. It is not *drawn* until a device actually reports
        // something — see `Pointer::present`.
        if !self.pointer.placed && !self.outputs.is_empty() {
            self.pointer.placed = true;
            let o = &self.outputs[0];
            self.pointer.x = f64::from(o.width / 2);
            self.pointer.y = f64::from(o.height / 2);
            self.pointer.output = Some(o.scene_id);
        }
    }

    fn output_mut(&mut self, id: KmsOutputId) -> Option<&mut OutputState> {
        self.outputs.iter_mut().find(|o| o.kms_id == id)
    }

    /// Run the scene's update pass and fold the damage into every output.
    fn update_scene(&mut self) {
        let mut regions: Vec<(SceneOutputId, Damage)> = self
            .outputs
            .iter()
            .map(|o| (o.scene_id, Damage::new()))
            .collect();
        let result = {
            let mut pairs: Vec<(SceneOutputId, &mut Damage)> =
                regions.iter_mut().map(|(id, d)| (*id, d)).collect();
            self.scene.update(&mut DamageSink::new(&mut pairs))
        };
        for (id, damage) in regions {
            let Some(output) = self.outputs.iter_mut().find(|o| o.scene_id == id) else {
                continue;
            };
            // Scene damage is global device pixels; an output's buffer
            // starts at its own origin, so shift it back.
            let origin = self
                .scene
                .output_info(id)
                .map_or((0, 0), |(rect, _)| (rect.x, rect.y));
            for r in damage.rects() {
                output.damage.add(r.translate(-origin.0, -origin.1));
            }
        }
        // A resize the server decided on (or a client's own `SetBounds` on
        // a window root) is told to the client, which lays out for it.
        for configure in result.configures {
            self.send_configure(configure.window, configure.size);
        }
    }

    fn send_configure(&mut self, win: WindowKey, size: Size) {
        // An unplaced window has no size, scale or output to report, and
        // naming output 0 for it would be a lie the client has no way to
        // detect. It is configured by `place_new_window` the moment it
        // lands on a screen, which is the honest moment to say where.
        let Some((scale, output)) = self
            .scene
            .window_info(win)
            .ok()
            .and_then(nitro_scene::Window::output)
            .and_then(|id| self.scene.output_info(id).map(|(_, s)| (s, id.0)))
        else {
            return;
        };
        for client in self.wire_clients.values_mut() {
            if let Some(window) = client.window_id(win) {
                client.send(&ServerMsg::Configure(msg::Configure {
                    window,
                    size,
                    scale,
                    output,
                }));
            }
        }
    }

    /// Paint and commit one output if it is writable and has anything new.
    fn paint(&mut self, id: KmsOutputId) {
        if !self.active || self.backend.flip_pending(id) {
            return;
        }
        let Some(index) = self.outputs.iter().position(|o| o.kms_id == id) else {
            return;
        };
        if !self.outputs[index].needs_paint() {
            return;
        }
        let region = self.outputs[index].repaint_region();
        if region.is_empty() {
            return;
        }
        let scene_id = self.outputs[index].scene_id;
        let cursor_state = self.cursor_state(scene_id);
        let paint_us = {
            let mut buf = match self.backend.back_buffer(id) {
                Ok(b) => b,
                Err(e) => {
                    warn!("{id}: back buffer: {e}");
                    return;
                }
            };
            frame::paint_region(
                &mut buf,
                &self.scene,
                scene_id,
                &region,
                (&self.cursor, cursor_state),
                &mut self.paint_items,
            )
        };
        let damage_px = frame::region_area(&region);
        let kms_damage: Vec<KmsRect> = region
            .iter()
            .map(|r| KmsRect::new(r.x, r.y, r.w.cast_unsigned(), r.h.cast_unsigned()))
            .collect();
        match self.backend.commit(id, &kms_damage) {
            Ok(()) => {
                self.stats.paint_us.push(paint_us);
                self.stats.damage_px.push(damage_px);
                self.outputs[index].committed();
            }
            Err(e) => {
                warn!("{id}: commit: {e}");
                // Keep the damage so the next event retries; otherwise the
                // output stalls until the next resume or hotplug.
                self.outputs[index].commit_failed(&region);
            }
        }
    }

    /// Where the cursor is on `output`, in that output's buffer space.
    fn cursor_state(&self, output: SceneOutputId) -> CursorState {
        let origin = self
            .scene
            .output_info(output)
            .map_or((0, 0), |(rect, _)| (rect.x, rect.y));
        let (x, y) = self.pointer.device();
        CursorState {
            x: x - origin.0,
            y: y - origin.1,
            visible: self.pointer.present,
        }
    }

    fn paint_all(&mut self) {
        let ids: Vec<KmsOutputId> = self.outputs.iter().map(|o| o.kms_id).collect();
        for id in ids {
            self.paint(id);
        }
    }

    /// The update-then-paint pass every event that could have changed the
    /// scene ends with. Idle means both are no-ops and nothing is
    /// committed, which is what keeps a quiet server at zero wakeups.
    fn settle(&mut self) {
        self.update_scene();
        self.claim_input_stamp();
        self.paint_all();
        self.answer_idle_clients();
        self.flush_wire_clients();
    }

    /// Answer the clients whose commit or frame request will not be
    /// carried by any frame, because there is no frame to carry it.
    ///
    /// The server only flips when something changed, which is the whole
    /// design — but it means "wait for the next flip" is not a promise it
    /// can keep to a client that changed nothing. Two cases need closing:
    /// a commit whose output has no paint pending (nothing it did was
    /// visible), and a `RequestFrame` from a quiescent desktop, which is
    /// exactly how a client starts an animation. Both are answered from
    /// the extrapolated vblank clock, which `frame::frame_deadline` can
    /// compute with or without a flip ever having happened.
    fn answer_idle_clients(&mut self) {
        // An output with a paint pending will flip, and `on_flip` is the
        // right place to answer everything riding on that frame.
        if self.outputs.iter().any(frame::OutputState::needs_paint)
            || self
                .backend
                .outputs()
                .iter()
                .any(|o| self.backend.flip_pending(o.id))
        {
            return;
        }
        let now_ns = monotonic_ns();
        let (deadline_ns, refresh_ns, output, time_ns, seq) =
            self.outputs.first().map_or((0, 0, 0, 0, 0), |o| {
                (
                    o.frame_deadline_ns(now_ns),
                    o.refresh_ns,
                    o.scene_id.0,
                    o.last_vblank_ns,
                    o.last_sequence,
                )
            });
        // Serials stamped onto an output that then painted nothing: the
        // transaction was applied and is as visible as it is ever going to
        // be, so acknowledge it instead of growing the list for ever.
        let stale: Vec<(u32, u32)> = self
            .outputs
            .iter_mut()
            .flat_map(|o| o.painting.drain(..))
            .collect();
        for (client_key, serial) in stale {
            let Some(client) = self
                .wire_clients
                .values_mut()
                .find(|c| c.id.0 == client_key)
            else {
                continue;
            };
            client.unpresented.retain(|s| *s != serial);
            client.send(&ServerMsg::Presented(msg::Presented {
                serial,
                output,
                time_ns,
                seq,
            }));
        }
        for client in self.wire_clients.values_mut() {
            let requests = std::mem::take(&mut client.frame_requests);
            for window in requests {
                client.send(&ServerMsg::Frame(msg::Frame {
                    window,
                    deadline_ns,
                    refresh_ns,
                }));
            }
        }
    }

    fn event_loop(&mut self) -> Result<(), Error> {
        let mut buf = event_buffer::<32>();
        while !self.quit {
            let n = wait(&self.epoll, &mut buf)?;
            for ev in &buf[..n] {
                let (token, flags) = (ev.data.u64(), ev.flags);
                match token {
                    TOK_SEAT => self.on_seat()?,
                    TOK_SIGNALS => {
                        if self.signals.as_mut().is_some_and(signals::Signals::drain) {
                            info!("signal received; shutting down");
                            self.quit = true;
                        }
                    }
                    TOK_LISTENER => self.on_accept()?,
                    TOK_WIRE_LISTENER => self.on_wire_accept()?,
                    TOK_BACKEND => self.on_backend()?,
                    TOK_INPUT => self.on_input(),
                    t if t >= TOK_WIRE_BASE => self.on_wire_client(t, flags),
                    t if t >= TOK_CLIENT_BASE => self.on_client(t, flags),
                    t => warn!("unknown epoll token {t}"),
                }
            }
        }
        Ok(())
    }

    fn on_seat(&mut self) -> Result<(), Error> {
        let events = match self.seat.as_ref() {
            Some(seat) => seat.borrow_mut().dispatch()?,
            None => return Ok(()),
        };
        for ev in events {
            match ev {
                SeatEvent::Disable => {
                    info!("session inactive: pausing");
                    // Input first: libinput must have let go of its device
                    // fds before the ack, or the VT switch hangs.
                    self.input.suspend();
                    self.backend.pause();
                    self.active = false;
                    if let Some(seat) = self.seat.as_ref() {
                        seat.borrow_mut().ack_disable()?;
                    }
                }
                SeatEvent::Enable => {
                    info!("session active: resuming");
                    match self.backend.resume() {
                        Ok(()) => self.active = true,
                        Err(e) => {
                            error!("resume failed: {e}");
                            continue;
                        }
                    }
                    self.input.resume();
                    // Key releases that happened on the other VT were never
                    // seen, so the modifier state is a guess: drop it.
                    if let Some(kb) = self.keyboard.as_mut() {
                        kb.reset();
                    }
                    for output in &mut self.outputs {
                        output.invalidate();
                    }
                    self.paint_all();
                }
            }
        }
        Ok(())
    }

    fn on_backend(&mut self) -> Result<(), Error> {
        let mut events = std::mem::take(&mut self.events);
        events.clear();
        self.backend.dispatch(&mut events)?;
        let mut hotplug = false;
        for ev in events.drain(..) {
            match ev {
                Event::Flipped {
                    output,
                    sequence,
                    time,
                } => {
                    self.frames += 1;
                    let refresh_ns = self
                        .outputs
                        .iter()
                        .find(|o| o.kms_id == output)
                        .map_or(16_666_667, |o| o.refresh_ns);
                    self.flips.record(time, refresh_ns);
                    debug!("{output} flipped seq={sequence} t={time:?}");
                    self.on_flip(output, sequence, time);
                }
                Event::Hotplug => hotplug = true,
            }
        }
        self.events = events;
        if hotplug {
            info!("hotplug");
            self.unregister_backend();
            match self.backend.rescan() {
                Ok(changed) => {
                    if changed {
                        self.sync_outputs();
                    }
                }
                Err(e) => warn!("rescan: {e}"),
            }
            self.register_backend()?;
            self.settle();
        }
        Ok(())
    }

    /// A frame reached the screen: tell the clients whose commits were in
    /// it, close the input-to-photon loop, answer frame callbacks, and
    /// paint the next frame if anything is still pending.
    fn on_flip(&mut self, id: KmsOutputId, sequence: u64, time: Duration) {
        let time_ns = u64::try_from(time.as_nanos()).unwrap_or(u64::MAX);
        let Some(output) = self.output_mut(id) else {
            return;
        };
        output.last_vblank_ns = time_ns;
        output.last_sequence = sequence;
        let presented = std::mem::take(&mut output.in_flight);
        let input_ns = std::mem::take(&mut output.in_flight_input_ns);
        let scene_id = output.scene_id;
        let refresh_ns = output.refresh_ns;
        let deadline_ns = output.frame_deadline_ns(time_ns);

        if input_ns > 0 && time_ns > input_ns {
            self.stats.i2p_us.push((time_ns - input_ns) / 1_000);
        }
        for (client_key, serial) in presented {
            let Some(client) = self
                .wire_clients
                .values_mut()
                .find(|c| c.id.0 == client_key)
            else {
                continue;
            };
            client.unpresented.retain(|s| *s != serial);
            client.send(&ServerMsg::Presented(msg::Presented {
                serial,
                output: scene_id.0,
                time_ns,
                seq: sequence,
            }));
        }
        // Frame callbacks are answered after the flip, not at commit time:
        // the deadline is only meaningful once we know when this vblank
        // actually happened. Only requests for windows on *this* output
        // are answered here — another output's vblank says nothing about
        // when this one will scan out.
        let mut answered: Vec<(u64, NodeId)> = Vec::new();
        for (token, client) in &mut self.wire_clients {
            client.frame_requests.retain(|window| {
                let on_output = client
                    .windows
                    .get(window)
                    .and_then(|win| self.scene.window_info(*win).ok())
                    .and_then(nitro_scene::Window::output)
                    == Some(scene_id);
                if on_output {
                    answered.push((*token, *window));
                }
                !on_output
            });
        }
        for (token, window) in answered {
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::Frame(msg::Frame {
                    window,
                    deadline_ns,
                    refresh_ns,
                }));
            }
        }
        self.flush_wire_clients();
        self.paint(id);
    }

    fn on_accept(&mut self) -> Result<(), Error> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let client = match Client::new(stream) {
                        Ok(c) => c,
                        Err(e) => {
                            warn!("client setup: {e}");
                            continue;
                        }
                    };
                    let token = TOK_CLIENT_BASE + self.next_client;
                    self.next_client += 1;
                    add(&self.epoll, client.stream(), token)?;
                    debug!("control client {} connected", token - TOK_CLIENT_BASE);
                    self.clients.insert(token, client);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(Error::Io {
                        op: "accept",
                        source: e,
                    });
                }
            }
        }
    }

    fn on_wire_accept(&mut self) -> Result<(), Error> {
        loop {
            let stream = match self.wire_listener.accept() {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(()),
                Err(e) => {
                    warn!("wire accept: {e}");
                    return Ok(());
                }
            };
            let id = ClientId(self.next_client_id);
            self.next_client_id += 1;
            let token = TOK_WIRE_BASE + self.next_wire;
            self.next_wire += 1;
            add(&self.epoll, &stream.as_fd(), token)?;
            debug!("wire client {} connected", id.0);
            self.wire_clients.insert(token, WireClient::new(stream, id));
        }
    }

    fn on_input(&mut self) {
        let mut events = std::mem::take(&mut self.input_events);
        events.clear();
        self.input.dispatch(&mut events);
        for event in events.drain(..) {
            self.route_input(&event);
        }
        self.input_events = events;
        self.flush_wire_clients();
        self.settle();
    }

    /// One input event: update the pointer or the keyboard, then send the
    /// protocol event to whoever owns what it landed on.
    fn route_input(&mut self, event: &InputEvent) {
        let time_ns = event.time_ns();
        match *event {
            InputEvent::PointerMotion { dx, dy, .. } => {
                let (x, y) = (self.pointer.x + dx, self.pointer.y + dy);
                self.move_pointer(x, y, time_ns);
            }
            InputEvent::PointerAbsolute { x, y, .. } => {
                let p = self.normalised_to_device(x, y);
                self.move_pointer(f64::from(p.x), f64::from(p.y), time_ns);
            }
            InputEvent::PointerButton {
                button,
                state,
                time_ns,
            } => self.pointer_button(button, state, time_ns),
            InputEvent::PointerAxis {
                dx,
                dy,
                source,
                time_ns,
            } => {
                let Some(window) = self.pointer.over else {
                    return;
                };
                self.send_to_window(window, |id| {
                    ServerMsg::PointerAxis(msg::PointerAxis {
                        window: id,
                        dx,
                        dy,
                        source,
                        time_ns,
                    })
                });
                self.note_input(time_ns);
            }
            InputEvent::Key {
                keycode,
                pressed,
                time_ns,
            } => self.key(keycode, pressed, time_ns),
            InputEvent::Touch {
                id,
                phase,
                x,
                y,
                time_ns,
            } => self.touch(id, phase, x, y, time_ns),
        }
    }

    /// Move the pointer, damage the cursor's old and new rectangles, and
    /// send enter/leave/motion.
    fn move_pointer(&mut self, x: f64, y: f64, time_ns: u64) {
        let bounds = input::output_union(&self.scene);
        let (old_x, old_y) = self.pointer.device();
        let appeared = self.pointer.seen();
        if appeared {
            self.damage_global(Cursor::rect(old_x, old_y));
        }
        if !self.pointer.move_to(x, y, bounds) && !appeared {
            return;
        }
        let (new_x, new_y) = self.pointer.device();
        if (old_x, old_y) != (new_x, new_y) {
            // Old ∪ new, exactly like the scene's own damage rule.
            self.damage_global(Cursor::rect(old_x, old_y));
            self.damage_global(Cursor::rect(new_x, new_y));
        }
        let point = self.pointer.position();
        let output = input::output_at(&self.scene, point);
        self.pointer.output = output;
        let target = output.and_then(|id| input::hit(&self.scene, id, point));
        let now_over = target.map(|t| t.window);
        if now_over != self.pointer.over {
            if let Some(left) = self.pointer.over {
                self.send_to_window(left, |id| {
                    ServerMsg::PointerLeave(msg::PointerLeave {
                        window: id,
                        time_ns,
                    })
                });
            }
            self.pointer.over = now_over;
            if let Some(t) = target {
                let node = self.node_id_for(t.window, t.hit.node);
                self.send_to_window(t.window, |id| {
                    ServerMsg::PointerEnter(msg::PointerEnter {
                        window: id,
                        node,
                        pos: t.local,
                        time_ns,
                    })
                });
            }
        } else if let Some(t) = target {
            let node = self.node_id_for(t.window, t.hit.node);
            self.send_to_window(t.window, |id| {
                ServerMsg::PointerMotion(msg::PointerMotion {
                    window: id,
                    node,
                    pos: t.local,
                    time_ns,
                })
            });
        }
        self.note_input(time_ns);
    }

    fn pointer_button(&mut self, button: u32, state: ButtonState, time_ns: u64) {
        let Some(window) = self.pointer.over else {
            // A click on the desktop drops focus, which is what lets a
            // client know it stopped receiving keys.
            if state == ButtonState::Pressed {
                self.set_focus(None);
            }
            return;
        };
        if state == ButtonState::Pressed && button == input::BTN_LEFT {
            // Raise within the Normal layer only: a click must not pull a
            // panel out from under a menu, or a menu below its panel.
            if self
                .scene
                .window_info(window)
                .is_ok_and(|w| w.layer() == nitro_scene::Layer::Normal)
                && let Err(e) = self.scene.raise(window)
            {
                warn!("raise: {e}");
            }
            self.set_focus(Some(window));
        }
        self.send_to_window(window, |id| {
            ServerMsg::PointerButton(msg::PointerButton {
                window: id,
                button,
                state,
                time_ns,
            })
        });
        self.note_input(time_ns);
    }

    fn key(&mut self, keycode: u32, pressed: bool, time_ns: u64) {
        // Without a keymap the key still reaches the focused client, with
        // no keysym and no text: the evdev code is the part that never
        // depends on xkb, and a client that only wants raw keys still works.
        let resolved = self
            .keyboard
            .as_mut()
            .map_or_else(keyboard::KeyResolution::none, |kb| kb.key(keycode, pressed));
        if pressed && let Some(hotkey) = keyboard::hotkey(resolved.keysym, resolved.ctrl_alt) {
            match hotkey {
                Hotkey::Quit => {
                    info!("Ctrl+Alt+Backspace: quitting");
                    self.quit = true;
                }
                Hotkey::SwitchVt(vt) => {
                    info!("Ctrl+Alt+F{vt}: switching session");
                    if let Some(seat) = self.seat.as_ref()
                        && let Err(e) = seat.borrow_mut().switch_session(vt)
                    {
                        warn!("switch to VT {vt}: {e}");
                    }
                }
            }
            // A hotkey is the compositor's, not the client's.
            return;
        }
        let Some(window) = self.focus else {
            return;
        };
        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        let utf8 = resolved.utf8.clone();
        let (keysym, mods) = (resolved.keysym, resolved.mods);
        self.send_to_window(window, |id| {
            ServerMsg::Key(msg::Key {
                window: id,
                keycode,
                state,
                mods,
                keysym,
                time_ns,
                utf8: utf8.clone(),
            })
        });
        self.note_input(time_ns);
    }

    fn touch(
        &mut self,
        touch_id: i32,
        phase: nitro_wire::types::TouchPhase,
        norm_x: f64,
        norm_y: f64,
        time_ns: u64,
    ) {
        use nitro_wire::types::TouchPhase;
        // A touch sequence belongs to the window it started on: the finger
        // may wander off the window while dragging, and the client still
        // owns the gesture until it lifts.
        let target = match phase {
            TouchPhase::Down => {
                let point = self.normalised_to_device(norm_x, norm_y);
                let output = input::output_at(&self.scene, point);
                let Some(t) = output.and_then(|out| input::hit(&self.scene, out, point)) else {
                    return;
                };
                self.touch_targets.insert(touch_id, (t.window, t.local));
                Some((t.window, t.local))
            }
            TouchPhase::Move => self.touch_targets.get(&touch_id).copied().map(|(win, _)| {
                let point = self.normalised_to_device(norm_x, norm_y);
                let local = input::window_local(&self.scene, win, point).unwrap_or(Point::ZERO);
                (win, local)
            }),
            TouchPhase::Up | TouchPhase::Cancel => self.touch_targets.remove(&touch_id),
        };
        let Some((win, pos)) = target else {
            return;
        };
        self.send_to_window(win, |window| {
            ServerMsg::Touch(msg::Touch {
                window,
                id: touch_id,
                phase,
                pos,
                time_ns,
            })
        });
        self.note_input(time_ns);
    }

    /// Scale a device's normalised (0..1) position onto the first output.
    /// Absolute devices report in their own unit square with no idea which
    /// screen they belong to; mapping a tablet or touchscreen to the right
    /// output is a settings question, and settings are M4.
    fn normalised_to_device(&self, x: f64, y: f64) -> Point {
        let (w, h) = self
            .outputs
            .first()
            .map_or((1.0, 1.0), |o| (f64::from(o.width), f64::from(o.height)));
        Point::new((x * w) as f32, (y * h) as f32)
    }

    /// Remember when this input arrived, so the frame that eventually
    /// shows its effect can be timed against it.
    ///
    /// The stamp is *not* applied to an output here, because at this point
    /// there is usually nothing to apply it to. A pointer move damages the
    /// cursor immediately, but a click or a key does not: the pixels that
    /// answer it are the client's, and they arrive in a later wakeup as a
    /// commit. Stamping only what is already dirty would quietly reduce
    /// the histogram to a cursor-motion histogram; carrying the timestamp
    /// until a frame actually consumes it measures the thing the metric is
    /// named after, click-to-photon included.
    ///
    /// The carry is bounded — see [`Server::claim_input_stamp`]. An input
    /// nothing ever responds to must not sit here waiting to be reported
    /// as a multi-second latency by some unrelated frame later on.
    fn note_input(&mut self, time_ns: u64) {
        self.pending_input_ns = self.pending_input_ns.max(time_ns);
    }

    /// Hand the pending input timestamp to whichever outputs are about to
    /// paint, or drop it once it is too old to be anyone's latency.
    ///
    /// Called after the scene update, which is the first moment the damage
    /// a client produced in response to the input is visible to us.
    fn claim_input_stamp(&mut self) {
        if self.pending_input_ns == 0 {
            return;
        }
        let mut claimed = false;
        for output in &mut self.outputs {
            if output.needs_paint() {
                output.painting_input_ns = output.painting_input_ns.max(self.pending_input_ns);
                claimed = true;
            }
        }
        if claimed || monotonic_ns().saturating_sub(self.pending_input_ns) > INPUT_STAMP_MAX_AGE_NS
        {
            self.pending_input_ns = 0;
        }
    }

    /// Damage a device-pixel rect in global coordinates on whichever
    /// outputs it touches.
    fn damage_global(&mut self, rect: nitro_core::IRect) {
        for output in &mut self.outputs {
            let Some((origin, _)) = self.scene.output_info(output.scene_id) else {
                continue;
            };
            let local = rect
                .translate(-origin.x, -origin.y)
                .intersect(&output.bounds());
            if !local.is_empty() {
                output.damage.add(local);
            }
        }
    }

    /// Send a message to whichever client owns `win`, naming the window
    /// with that client's own node id.
    fn send_to_window<F>(&mut self, win: WindowKey, build: F)
    where
        F: Fn(NodeId) -> ServerMsg,
    {
        for client in self.wire_clients.values_mut() {
            if let Some(id) = client.window_id(win) {
                let msg = build(id);
                client.send(&msg);
            }
        }
    }

    fn node_id_for(&self, win: WindowKey, node: nitro_scene::NodeKey) -> NodeId {
        self.wire_clients
            .values()
            .find(|c| c.owns_window(win))
            .map_or(NodeId::NONE, |c| c.node_id(node))
    }

    /// Move keyboard focus, telling both the old and the new window.
    fn set_focus(&mut self, window: Option<WindowKey>) {
        if self.focus == window {
            return;
        }
        if let Some(old) = self.focus {
            self.send_to_window(old, |id| {
                ServerMsg::Focus(msg::Focus {
                    window: id,
                    focused: false,
                })
            });
        }
        self.focus = window;
        if let Some(new) = window {
            self.send_to_window(new, |id| {
                ServerMsg::Focus(msg::Focus {
                    window: id,
                    focused: true,
                })
            });
        }
    }

    fn on_client(&mut self, token: u64, flags: EventFlags) {
        let Some(mut client) = self.clients.remove(&token) else {
            return;
        };
        let mut keep = true;
        if flags.intersects(EventFlags::IN | EventFlags::HUP | EventFlags::ERR) {
            match client.read() {
                ReadOutcome::Open => {}
                ReadOutcome::Closed => keep = false,
                ReadOutcome::Overflow => {
                    client.send(protocol::err_reply("request line too long"));
                    let _ = client.flush();
                    keep = false;
                }
            }
            while keep && let Some(line) = protocol::take_line(&mut client.input) {
                self.handle_request(&mut client, &line);
            }
        }
        if keep {
            match client.flush() {
                Ok(_) => {}
                Err(e) => {
                    debug!("control client {}: write: {e}", token - TOK_CLIENT_BASE);
                    keep = false;
                }
            }
        }
        if keep {
            let want = if client.has_pending_output() {
                EventFlags::IN | EventFlags::OUT
            } else {
                EventFlags::IN
            };
            if let Err(e) = epoll::modify(
                &self.epoll,
                client.stream(),
                EventData::new_u64(token),
                want,
            ) {
                warn!("epoll_ctl mod client: {e}");
                keep = false;
            }
        }
        if keep {
            self.clients.insert(token, client);
        } else {
            debug!("control client {} closed", token - TOK_CLIENT_BASE);
            // Dropping the stream removes it from the epoll set.
        }
    }

    fn handle_request(&mut self, client: &mut Client, line: &str) {
        debug!("request {line:?}");
        let reply = match protocol::parse(line) {
            Err(msg) => protocol::err_reply(&msg),
            Ok(Request::Outputs) => protocol::outputs_reply(self.backend.outputs()),
            Ok(Request::Stats) => self.stats_reply(),
            Ok(Request::Shot(name)) => self.shot(name.as_deref()),
            Ok(Request::Quit) => {
                info!("quit requested");
                self.quit = true;
                protocol::ok_reply()
            }
            Ok(Request::Plug(w, h)) => self.plug(w, h),
        };
        client.send(reply);
    }

    fn stats_reply(&self) -> Vec<u8> {
        let pending = self
            .backend
            .outputs()
            .iter()
            .filter(|o| self.backend.flip_pending(o.id))
            .count() as u64;
        let mut pairs: Vec<(&'static str, u64)> = vec![
            ("frames", self.frames),
            ("flips_pending", pending),
            ("uptime_ms", self.started.elapsed().as_millis() as u64),
            ("active", u64::from(self.active)),
            ("flip_interval_mean_us", self.flips.mean_us()),
            (
                "flip_interval_min_us",
                self.flips.min.map_or(0, |d| d.as_micros() as u64),
            ),
            ("flip_interval_max_us", self.flips.max.as_micros() as u64),
        ];
        self.stats.write_pairs(&mut pairs);
        pairs.push(("clients", self.wire_clients.len() as u64));
        pairs.push(("windows", self.scene.window_count() as u64));
        pairs.push(("nodes", self.scene.node_count() as u64));
        protocol::stats_reply(&pairs)
    }

    // -------------------------------------------------------- wire clients

    /// One wire client became readable (or writable, or hung up).
    fn on_wire_client(&mut self, token: u64, flags: EventFlags) {
        if !self.wire_clients.contains_key(&token) {
            return;
        }
        let mut keep = true;
        if flags.intersects(EventFlags::IN | EventFlags::HUP | EventFlags::ERR) {
            keep = self.read_wire_client(token);
        }
        if keep {
            keep = self.flush_wire_client(token);
        }
        if keep {
            self.arm_wire_client(token);
        } else {
            self.disconnect(token, None);
        }
        self.settle();
    }

    /// Read and decode everything available from one client. Returns
    /// whether to keep it.
    fn read_wire_client(&mut self, token: u64) -> bool {
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        match client.stream.read() {
            Ok(_) => {}
            Err(nitro_wire::error::Error::Closed) => return false,
            Err(e) => {
                let code = clients::wire_code(&e);
                let detail = e.to_string();
                self.disconnect(token, Some((0, code, detail)));
                return false;
            }
        }
        loop {
            let Some(client) = self.wire_clients.get_mut(&token) else {
                return false;
            };
            let msg = match client.stream.next_msg() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(nitro_wire::error::Error::Closed) => return false,
                Err(e) => {
                    let code = clients::wire_code(&e);
                    let detail = e.to_string();
                    self.disconnect(token, Some((0, code, detail)));
                    return false;
                }
            };
            if !self.handle_wire_msg(token, msg) {
                return false;
            }
        }
        // A peer that hung up after sending a final batch has had every
        // message of it decoded by now.
        self.wire_clients
            .get(&token)
            .is_some_and(|c| !c.stream.is_closed())
    }

    /// Buffer, or act on, one decoded client message. Returns whether the
    /// client survives it.
    fn handle_wire_msg(&mut self, token: u64, message: ClientMsg) -> bool {
        match message {
            ClientMsg::Hello(hello) => {
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                info!("wire client {} is {:?}", client.id.0, hello.name);
                // No capability bits in v1: direct scanout, text and
                // dma-buf are all later milestones, and a zero bit is the
                // protocol's way of saying "do not use this".
                if let Err(e) = client.stream.welcome(SERVER_NAME, 0) {
                    warn!("welcome: {e}");
                    return false;
                }
                true
            }
            ClientMsg::Commit(commit) => self.commit(token, commit.serial),
            ClientMsg::CreateBuffer(buffer) => {
                // The descriptor is read *now*, not at commit: the client
                // may legitimately reuse the memfd for the next frame as
                // soon as it has sent this message.
                let (desc, data) = match clients::read_buffer(&buffer) {
                    Ok(pair) => pair,
                    Err(e) => {
                        self.disconnect(token, Some((0, e.code, e.detail)));
                        return false;
                    }
                };
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                client.pending.push(Pending::Buffer(buffer.id, desc, data));
                self.pending_fds.push((token, buffer.id, buffer.fd, desc));
                true
            }
            other => {
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                client.pending.push(Pending::Msg(Box::new(other)));
                true
            }
        }
    }

    /// Apply a client's transaction. Returns whether the client survives.
    fn commit(&mut self, token: u64, serial: u32) -> bool {
        let Some(mut client) = self.wire_clients.remove(&token) else {
            return false;
        };
        // The buffer descriptors arrived with their messages; hand them to
        // the client's map once the scene has minted the keys.
        let result = clients::apply(&mut client, &mut self.scene, serial);
        let outcome = match result {
            Ok(o) => o,
            Err(ApplyError { code, detail }) => {
                warn!("wire client {}: {detail}", client.id.0);
                self.wire_clients.insert(token, client);
                self.disconnect(token, Some((serial, code, detail)));
                return false;
            }
        };
        self.adopt_buffer_fds(token, &client);
        for (key, rects) in outcome.buffer_damage {
            self.refresh_damaged_buffer(client.id, key, &rects);
        }
        // `DestroyBuffer` promises the server drops its mapping. The scene
        // forgets the pixels itself; the descriptor is ours, and this is
        // the only place it can be released before the client goes — a
        // client cycling one buffer per frame would otherwise leak an fd
        // per frame until it hit the process limit.
        for key in outcome.destroyed_buffers {
            self.buffer_sources.remove(&(client.id, key));
        }
        for (node_id, win) in outcome.new_windows {
            self.place_new_window(&mut client, node_id, win);
        }
        client.frame_requests.extend(outcome.frame_requests);
        for win in outcome.closed_windows {
            if self.focus == Some(win) {
                self.focus = None;
            }
            if self.pointer.over == Some(win) {
                self.pointer.over = None;
            }
            self.touch_targets.retain(|_, (w, _)| *w != win);
        }
        // Which outputs this commit can actually reach: the ones its
        // windows are on. Stamping every output would report the same
        // serial once per flip on a multi-output desktop, and `Presented`
        // means "this transaction is on screen", not "a screen flipped".
        let client_key = client.id.0;
        let mut touched: Vec<SceneOutputId> = Vec::new();
        for win in client.windows.values() {
            if let Some(id) = self
                .scene
                .window_info(*win)
                .ok()
                .and_then(nitro_scene::Window::output)
                && !touched.contains(&id)
            {
                touched.push(id);
            }
        }
        if touched.is_empty() {
            // A client with no placed window has nowhere for its commit to
            // appear. Answer at once rather than holding the serial for a
            // frame that will never carry it. Built against `client`
            // directly: it is out of the map for the length of this
            // function, so anything that looks the client up by token
            // would silently find nothing.
            let (output, time_ns, seq) = self.outputs.first().map_or((0, 0, 0), |o| {
                (o.scene_id.0, o.last_vblank_ns, o.last_sequence)
            });
            client.send(&ServerMsg::Presented(msg::Presented {
                serial,
                output,
                time_ns,
                seq,
            }));
        } else {
            client.unpresented.push(serial);
            for output in &mut self.outputs {
                if touched.contains(&output.scene_id) {
                    output.painting.push((client_key, serial));
                }
            }
        }
        self.wire_clients.insert(token, client);
        true
    }

    /// Move the fds that came with this transaction's `CreateBuffer`s into
    /// the server's map, now that the scene has keys for them.
    fn adopt_buffer_fds(&mut self, token: u64, client: &WireClient) {
        let mine: Vec<(
            u64,
            nitro_wire::types::BufferId,
            OwnedFd,
            nitro_scene::BufferDesc,
        )> = std::mem::take(&mut self.pending_fds);
        for (t, id, fd, desc) in mine {
            if t != token {
                self.pending_fds.push((t, id, fd, desc));
                continue;
            }
            let Some(key) = client.buffers.get(&id).copied() else {
                // The transaction that would have created it failed or the
                // id was destroyed in the same batch; the fd goes with it.
                continue;
            };
            self.buffer_sources
                .insert((client.id, key), BufferSource { fd, desc });
        }
    }

    /// Place a newly created window: cascade it onto the primary output
    /// and tell the client the size, scale and output it got.
    fn place_new_window(&mut self, client: &mut WireClient, node_id: NodeId, win: WindowKey) {
        let Some(output) = self.outputs.first() else {
            // No output yet (every connector unplugged, or a hotplug still
            // in flight). The window is real and owns its nodes; it simply
            // has nowhere to be. `sync_outputs` drains this list when an
            // output appears, so the client gets its `Configure` then.
            self.unplaced.push((client.id, win));
            return;
        };
        let scene_id = output.scene_id;
        let (scale, logical) = self
            .scene
            .output_info(scene_id)
            .map_or((1.0, Size::ZERO), |(r, s)| {
                (s, Size::new(r.w as f32 / s, r.h as f32 / s))
            });
        let size = self
            .scene
            .window_info(win)
            .map_or(Size::ZERO, nitro_scene::Window::size);
        let position = clients::cascade_position(self.windows_created, size, logical);
        self.windows_created += 1;
        if let Err(e) = self.scene.place_window(win, Some(scene_id), position) {
            warn!("place window: {e}");
            return;
        }
        client.send(&ServerMsg::Configure(msg::Configure {
            window: node_id,
            size,
            scale,
            output: scene_id.0,
        }));
    }

    /// Push queued bytes at every client, dropping the ones whose socket
    /// has gone.
    fn flush_wire_clients(&mut self) {
        let tokens: Vec<u64> = self.wire_clients.keys().copied().collect();
        for token in tokens {
            if self.flush_wire_client(token) {
                self.arm_wire_client(token);
            } else {
                self.disconnect(token, None);
            }
        }
    }

    /// Returns whether the client is still usable.
    fn flush_wire_client(&mut self, token: u64) -> bool {
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        match client.stream.flush() {
            Ok(_) => true,
            Err(e) => {
                debug!("wire client {}: write: {e}", client.id.0);
                false
            }
        }
    }

    /// Ask for `OUT` only while bytes are queued: an idle client that is
    /// merely connected must not make the loop spin.
    fn arm_wire_client(&mut self, token: u64) {
        let Some(client) = self.wire_clients.get(&token) else {
            return;
        };
        let want = if client.stream.has_pending_writes() {
            EventFlags::IN | EventFlags::OUT
        } else {
            EventFlags::IN
        };
        if let Err(e) = epoll::modify(
            &self.epoll,
            client.stream.as_fd(),
            EventData::new_u64(token),
            want,
        ) {
            warn!("epoll_ctl mod wire client: {e}");
        }
    }

    /// Drop a client and everything it owned.
    ///
    /// The scene has no "destroy everything of client X" call and does not
    /// need one: the client's own window map names every tree it owns, and
    /// destroying a window destroys its nodes. The pixels those windows
    /// covered are damaged by the scene as it removes them, so the area
    /// repaints without them on the next frame.
    fn disconnect(&mut self, token: u64, failure: Option<(u32, ErrorCode, String)>) {
        let Some(mut client) = self.wire_clients.remove(&token) else {
            return;
        };
        if let Some((serial, code, detail)) = failure {
            info!("wire client {}: {detail}", client.id.0);
            client.stream.fail(serial, code, &detail);
        }
        let id = client.id;
        for win in client.windows.values().copied().collect::<Vec<_>>() {
            if self.focus == Some(win) {
                self.focus = None;
            }
            if self.pointer.over == Some(win) {
                self.pointer.over = None;
            }
            self.touch_targets.retain(|_, (w, _)| *w != win);
            if let Err(e) = self.scene.destroy_window(id, win) {
                warn!("destroying window of client {}: {e}", id.0);
            }
        }
        for key in client.buffers.values().copied().collect::<Vec<_>>() {
            let _ = self.scene.destroy_buffer(id, key);
            self.buffer_sources.remove(&(id, key));
        }
        self.pending_fds.retain(|(t, _, _, _)| *t != token);
        for output in &mut self.outputs {
            output.painting.retain(|(c, _)| *c != id.0);
            output.in_flight.retain(|(c, _)| *c != id.0);
        }
        debug!("wire client {} disconnected", id.0);
        // Dropping the stream removes it from the epoll set.
    }

    /// Re-read the rows a client declared damaged in one of its buffers.
    /// Called after a transaction, because the scene only learns which
    /// image nodes to repaint at that point.
    fn refresh_damaged_buffer(
        &mut self,
        id: ClientId,
        key: nitro_scene::BufferKey,
        rects: &[nitro_core::IRect],
    ) {
        let Some(source) = self.buffer_sources.get(&(id, key)) else {
            return;
        };
        let desc = source.desc;
        let fd = source.fd.as_fd();
        let Ok(data) = self.scene.buffer_mut(id, key) else {
            return;
        };
        if let Err(e) = clients::reread_damage(fd, desc, rects, data) {
            warn!("re-reading buffer damage: {}", e.detail);
        }
    }

    /// Hotplug an output into the fake backend.
    ///
    /// Test-only, and refused on a real backend: on DRM an output exists
    /// because a connector reports a mode, and inventing one would mean
    /// lying to the modesetting code. It exists so a test can drive the
    /// "server with no output yet" state, which is otherwise unreachable
    /// in-process and is exactly where the deferred window placement
    /// lives.
    fn plug(&mut self, width: u32, height: u32) -> Vec<u8> {
        if !self.backend.simulate_plug(width, height) {
            return protocol::err_reply("`plug` is only available on the fake backend");
        }
        info!("plug {width}x{height} requested");
        protocol::ok_reply()
    }

    fn shot(&mut self, name: Option<&str>) -> Vec<u8> {
        let id = match name {
            Some(n) => self
                .backend
                .outputs()
                .iter()
                .find(|o| o.name == n)
                .map(|o| o.id),
            None => self.backend.outputs().first().map(|o| o.id),
        };
        let Some(id) = id else {
            return protocol::err_reply(&match name {
                Some(n) => format!("no output named {n}"),
                None => "no outputs".to_owned(),
            });
        };
        match self.backend.read_front(id) {
            Ok(img) => protocol::shot_reply(&img),
            Err(e) => protocol::err_reply(&e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 Hz, the rate every interval below is measured against.
    const HZ60: u32 = 16_666_667;

    #[test]
    fn flip_stats_track_intervals() {
        let mut s = FlipStats::default();
        s.record(Duration::from_millis(100), HZ60);
        assert_eq!(s.mean_us(), 0);
        s.record(Duration::from_millis(116), HZ60);
        s.record(Duration::from_millis(134), HZ60);
        assert_eq!(s.count, 2);
        assert_eq!(s.min, Some(Duration::from_millis(16)));
        assert_eq!(s.max, Duration::from_millis(18));
        assert_eq!(s.mean_us(), 17_000);
    }

    #[test]
    fn an_idle_gap_is_not_a_slow_frame() {
        let mut s = FlipStats::default();
        s.record(Duration::from_millis(100), HZ60);
        s.record(Duration::from_millis(116), HZ60);
        // The desktop sat still for a minute, then something moved. That
        // minute is not a flip interval, and counting it would swamp every
        // real sample.
        s.record(Duration::from_mins(1), HZ60);
        s.record(Duration::from_millis(60_016), HZ60);
        assert_eq!(s.count, 2, "the idle gap was skipped");
        assert_eq!(s.max, Duration::from_millis(16));
        assert_eq!(s.mean_us(), 16_000);
    }

    #[test]
    fn card_candidates_honour_override() {
        assert_eq!(
            card_candidates(Some(Path::new("/dev/dri/card9"))),
            [PathBuf::from("/dev/dri/card9")]
        );
        let all = card_candidates(None);
        assert!(all.windows(2).all(|w| w[0] <= w[1]), "sorted");
    }

    #[test]
    fn error_display() {
        let e = Error::NoDevice("nothing".into());
        assert_eq!(e.to_string(), "no usable DRM device: nothing");
        let e = Error::Io {
            op: "accept",
            source: io::Error::from(io::ErrorKind::Other),
        };
        assert!(e.to_string().starts_with("accept: "));
    }

    #[test]
    fn the_fake_config_puts_both_sockets_in_one_directory() {
        let c = Config::fake(320, 200, "/run/x/nitro/control.sock");
        assert_eq!(c.control_path, PathBuf::from("/run/x/nitro/control.sock"));
        assert_eq!(c.wire_path, PathBuf::from("/run/x/nitro/wire.sock"));
        assert!(!c.handle_signals);
        assert!(c.input_dir.is_none());
    }
}
