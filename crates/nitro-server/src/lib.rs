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
pub mod defer;
pub mod frame;
pub mod input;
pub mod keyboard;
pub mod logging;
pub mod protocol;
pub mod render;
pub mod signals;
pub mod stats;
/// In-process server for another crate's tests; see the module docs.
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod text;
pub mod wm;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use nitro_core::{Damage, Point, Rect, Size};
use nitro_kms::{
    Backend, DrmBackend, DrmOptions, Error as KmsError, Event, FakeBackend,
    OutputId as KmsOutputId, OutputInfo, Rect as KmsRect,
};
use nitro_scene::{ClientId, DamageSink, OutputId as SceneOutputId, Scene, WindowKey, WindowState};
use nitro_seat::{Device, Seat, SeatEvent};
use nitro_wire::msg::{self, ClientMsg, ServerMsg};
use nitro_wire::server::Listener as WireListener;
use nitro_wire::types::{ButtonState, ErrorCode, NodeId};
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::clients::{ApplyError, BufferSource, Pending, WireClient};
use crate::control::{Client, ReadOutcome};
use crate::cursor::Cursor;
use crate::defer::DeferredFlip;
use crate::frame::{CursorState, OutputState};
use crate::input::{InputEvent, InputSource, LibinputSource, Pointer};
use crate::keyboard::{Hotkey, Keyboard, Mods};
use crate::protocol::Request;
use crate::stats::FrameStats;
use crate::text::{StyleRequest, TextEngine};
use crate::wm::{Drag, Edges, FrameNodes, Region, WindowManager};

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
    /// Per-output scale overrides by connector name. `main.rs` fills this
    /// from `NITRO_SCALE`; a test sets it directly, because the
    /// environment is process-global and the tests run in threads of one
    /// process.
    pub scales: HashMap<String, f32>,
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
            scales: HashMap::new(),
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

/// Parse per-output scale overrides, `<name>=<f32>,…`.
///
/// A stop-gap: the persistent, user-editable output layout (position,
/// rotation and scale per connector) belongs to the settings app, which is
/// M4. Until then an environment variable is the honest way to say "this
/// panel is `HiDPI`" without inventing a config format that will be thrown
/// away. `main.rs` reads `NITRO_SCALE` and passes it here; a test fills
/// [`Config::scales`] directly, because the environment is process-global
/// and the tests run in threads of one process.
#[must_use]
pub fn parse_scales(spec: &str) -> HashMap<String, f32> {
    let mut out = HashMap::new();
    for entry in spec.split(',').filter(|e| !e.trim().is_empty()) {
        let Some((name, value)) = entry.split_once('=') else {
            warn!("NITRO_SCALE: {entry:?} is not name=scale");
            continue;
        };
        match value.trim().parse::<f32>() {
            Ok(s) if s.is_finite() && s > 0.0 => {
                out.insert(name.trim().to_owned(), s);
            }
            _ => warn!("NITRO_SCALE: {value:?} is not a positive scale"),
        }
    }
    out
}

/// The scale an output gets when nothing overrides it.
///
/// Derived from the EDID physical size: a panel at 192 dpi or more is a
/// `HiDPI` panel and gets 2×, everything else 1×. Deliberately a step
/// function rather than a continuous ratio — fractional scaling makes
/// every rectangle in the tree land between pixels, and the whole damage
/// contract is built on exact device rects.
#[must_use]
fn default_scale(info: &OutputInfo) -> f32 {
    /// Millimetres per inch, times ten, so the arithmetic stays integral.
    const MM_PER_INCH_10: u32 = 254;
    /// The dpi at which a panel is treated as `HiDPI`.
    const HIDPI: u32 = 192;
    let (mm_w, mm_h) = info.phys_mm;
    if mm_w == 0 || mm_h == 0 {
        return 1.0;
    }
    // dpi = pixels / (mm / 25.4); computed as an integer to avoid a float
    // comparison deciding a discrete question.
    let dpi = info.width * MM_PER_INCH_10 / (mm_w * 10);
    if dpi >= HIDPI { 2.0 } else { 1.0 }
}

// epoll tokens
const TOK_SEAT: u64 = 0;
const TOK_SIGNALS: u64 = 1;
const TOK_LISTENER: u64 = 2;
const TOK_BACKEND: u64 = 3;
const TOK_WIRE_LISTENER: u64 = 4;
const TOK_INPUT: u64 = 5;
/// The one timerfd that bounds a deferred flip; see [`defer`].
const TOK_DEFER: u64 = 6;
/// The uevent socket watching for input devices being plugged in and out.
const TOK_INPUT_HOTPLUG: u64 = 7;
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
    /// Fonts, the shaper, the glyph atlas and every shaped run on screen.
    text: TextEngine,
    outputs: Vec<OutputState>,
    keyboard: Option<Keyboard>,
    cursor: Cursor,
    pointer: Pointer,
    /// Window-management policy: MRU, focus, drags, placement.
    wm: WindowManager,
    /// The decoration nodes of each framed window.
    decorations: HashMap<WindowKey, FrameNodes>,
    /// The shaped title run of each framed window, so a retitle can release
    /// the old one.
    frame_titles: HashMap<WindowKey, nitro_text::TextKey>,
    /// Per-output scale overrides from `NITRO_SCALE`, by connector name.
    scale_overrides: HashMap<String, f32>,
    /// Watches `/sys` for input devices appearing and disappearing.
    input_hotplug: Option<nitro_kms::uevent::UeventSocket>,
    /// Where `event*` devices live, for the hotplug rescan.
    input_dir: Option<PathBuf>,
    /// The window with keyboard focus, if any.
    focus: Option<WindowKey>,
    /// A window placed this wakeup that should take focus once its
    /// client is back in `wire_clients`; see `place_new_window`.
    pending_focus: Option<WindowKey>,
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
    /// The clients whose answer a cursor-only flip is waiting for, and the
    /// timer that bounds the wait. See [`defer`].
    defer: DeferredFlip,

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

    // Input-device hotplug, deferred from M1: the same kernel uevent
    // socket the DRM backend uses, on the `input` subsystem. It is opened
    // only when input is real — a fake source has no devices to add — and
    // a failure is not fatal: a sandbox with no netlink loses hotplug, not
    // the keyboard it already has.
    let input_hotplug = if config.input_dir.is_some() && seat.is_some() {
        match nitro_kms::uevent::UeventSocket::open() {
            Ok(s) => {
                add(&epoll, &s, TOK_INPUT_HOTPLUG)?;
                info!("input hotplug via the kernel uevent socket");
                Some(s)
            }
            Err(e) => {
                warn!("input hotplug disabled: {e}");
                None
            }
        }
    } else {
        None
    };

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
        text: TextEngine::new(),
        outputs: Vec::new(),
        keyboard,
        cursor: Cursor::new(),
        pointer: Pointer::default(),
        wm: WindowManager::new(),
        decorations: HashMap::new(),
        frame_titles: HashMap::new(),
        scale_overrides: std::mem::take(&mut config.scales),
        input_hotplug,
        input_dir: config.input_dir.clone(),
        focus: None,
        pending_focus: None,
        touch_targets: HashMap::new(),
        buffer_sources: HashMap::new(),
        pending_fds: Vec::new(),
        windows_created: 0,
        unplaced: Vec::new(),
        pending_input_ns: 0,
        defer: defer::DeferredFlip::new().map_err(errno("create the deferred-flip timer"))?,
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
    // The deferral timer stays in the epoll set for the whole run. It is
    // disarmed unless a flip is actually being held, so a registered fd
    // that never fires costs an idle server nothing.
    add(&server.epoll, &server.defer.as_fd(), TOK_DEFER)?;
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
    /// with the backend's, laid out **left to right in connector order**.
    ///
    /// A row is the arrangement that needs no policy, and connector order
    /// is the only ordering the kernel gives us; a persistent layout the
    /// user can rearrange belongs to the settings app (M4). The scale of
    /// each output is `NITRO_SCALE` if it names that connector, else the
    /// EDID-derived default.
    ///
    /// Removing an output orphans its windows — the scene unplaces them —
    /// so they are migrated onto the primary output afterwards rather than
    /// left invisible with no way back.
    fn sync_outputs(&mut self) {
        let infos: Vec<OutputInfo> = self.backend.outputs().to_vec();
        let mut lost = false;
        self.outputs.retain(|o| {
            let keep = infos.iter().any(|i| i.id == o.kms_id);
            if !keep {
                info!("{} gone", o.kms_id);
                self.scene.remove_output(o.scene_id);
                lost = true;
            }
            keep
        });
        let mut x = 0;
        for info in &infos {
            let scene_id = SceneOutputId(info.id.0);
            let scale = self
                .scale_overrides
                .get(&info.name)
                .copied()
                .unwrap_or_else(|| default_scale(info));
            let rect =
                nitro_core::IRect::new(x, 0, info.width.cast_signed(), info.height.cast_signed());
            x += info.width.cast_signed();
            self.scene.add_output(scene_id, rect, scale);
            if let Some(existing) = self.outputs.iter_mut().find(|o| o.kms_id == info.id) {
                existing.width = info.width;
                existing.height = info.height;
                existing.refresh_ns = frame::refresh_ns(info.refresh_mhz);
                existing.invalidate();
                continue;
            }
            info!(
                "{} {}: {}x{}@{}.{:03} Hz, scale {scale}",
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
        if lost {
            self.migrate_orphans();
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
        // The pointer may be standing where an output used to be.
        if lost && let Some(bounds) = input::output_union(&self.scene) {
            let (x, y) = (self.pointer.x, self.pointer.y);
            self.pointer.move_to(x, y, Some(bounds));
            self.pointer.output = input::output_at(&self.scene, self.pointer.position());
        }
    }

    /// Move every window the scene unplaced (its output went away) onto the
    /// primary output, clamped into its work area.
    ///
    /// A window that is simply left unplaced is invisible and unreachable:
    /// it is not in any z-order, so no click and no `Alt+Tab` raise can get
    /// it back. Migrating is the only behaviour that does not lose work.
    fn migrate_orphans(&mut self) {
        let Some(primary) = self.outputs.first().map(|o| o.scene_id) else {
            // Every output is gone; the windows wait, exactly as they do
            // between startup and the first connector.
            return;
        };
        let area = wm::work_area(&self.scene, primary);
        let orphans: Vec<WindowKey> = self
            .wire_clients
            .values()
            .flat_map(|c| c.windows.values().copied())
            .filter(|w| {
                self.scene
                    .window_info(*w)
                    .is_ok_and(|i| i.output().is_none())
            })
            .collect();
        if orphans.is_empty() {
            return;
        }
        info!(
            "migrating {} window(s) to the primary output",
            orphans.len()
        );
        for win in orphans {
            let Ok(info) = self.scene.window_info(win) else {
                continue;
            };
            let (size, position, state) = (info.frame_size(), info.position(), info.state());
            // The orphan's position is in its departed output's space, so
            // it is only a hint; clamping it into the primary's work area
            // is what keeps a window that was near an edge near an edge.
            let origin = self.desktop_origin(primary);
            let pos = wm::clamp_into(position, size, area);
            let local = Point::new(pos.x - origin.x, pos.y - origin.y);
            if let Err(e) = self.scene.place_window(win, Some(primary), local) {
                warn!("migrating a window: {e}");
                continue;
            }
            // A maximized or fullscreen window's geometry is the old
            // output's; re-derive it for the new one.
            if state != WindowState::Normal && state != WindowState::Minimized {
                self.apply_state_geometry(win, state);
            }
            self.configure(win);
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
                output.damage_content(r.translate(-origin.0, -origin.1));
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
        let Some((position, scale, output)) = self.scene.window_info(win).ok().and_then(|w| {
            let id = w.output()?;
            let (_, s) = self.scene.output_info(id)?;
            // A client is told where its *content* is, not where its frame
            // is: `position` plus `size` must crop a screenshot down to
            // exactly the pixels the client drew.
            Some((w.content_position(), s, id.0))
        }) else {
            return;
        };
        for client in self.wire_clients.values_mut() {
            if let Some(window) = client.window_id(win) {
                client.send(&ServerMsg::Configure(msg::Configure {
                    window,
                    size,
                    position,
                    scale,
                    output,
                }));
            }
        }
    }

    /// Send the current `Configure` for a window: what the server just did
    /// to its geometry, whether or not its size changed.
    ///
    /// The scene's own `Configure` stream only fires on a *size* change, so
    /// a pure move — a drag, a migration, a tile that happens to preserve
    /// the size — would otherwise leave the client believing it is still
    /// where it was, and `position` is what it crops screenshots with.
    fn configure(&mut self, win: WindowKey) {
        let Ok(size) = self.scene.window_info(win).map(nitro_scene::Window::size) else {
            return;
        };
        self.send_configure(win, size);
    }

    /// Paint and commit one output if it is writable and has anything new.
    ///
    /// Returns whether a commit went in, which
    /// [`Server::paint_all`] uses to tell "nothing to do" from "held".
    fn paint(&mut self, id: KmsOutputId) -> bool {
        if !self.active || self.backend.flip_pending(id) {
            return false;
        }
        let Some(index) = self.outputs.iter().position(|o| o.kms_id == id) else {
            return false;
        };
        if !self.outputs[index].needs_paint() {
            return false;
        }
        let region = self.outputs[index].repaint_region();
        if region.is_empty() {
            return false;
        }
        let scene_id = self.outputs[index].scene_id;
        let cursor_state = self.cursor_state(scene_id);
        let paint_us = {
            let mut buf = match self.backend.back_buffer(id) {
                Ok(b) => b,
                Err(e) => {
                    warn!("{id}: back buffer: {e}");
                    return false;
                }
            };
            frame::paint_region(
                &mut buf,
                &self.scene,
                &mut self.text,
                scene_id,
                &region,
                (&self.cursor, cursor_state),
                &mut self.paint_items,
            )
        };
        // One frame stamp per painted frame: the atlas's LRU counts frames,
        // not glyphs.
        self.text.next_frame();
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
                true
            }
            Err(e) => {
                warn!("{id}: commit: {e}");
                // Keep the damage so the next event retries; otherwise the
                // output stalls until the next resume or hotplug.
                self.outputs[index].commit_failed(&region);
                false
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

    /// Paint every output that wants it, unconditionally, disarming any
    /// deferral first.
    ///
    /// Every caller that is *not* the ordinary settle pass (a resume, a
    /// hotplug, startup) goes through here rather than
    /// [`Server::paint_or_defer`], because none of those frames is a
    /// cursor a client is answering — and disarming even on those paths is
    /// what keeps "idle is zero wakeups" unconditionally true.
    fn paint_all(&mut self) {
        // Disarm first: the frame is going in now, and the timer would
        // otherwise wake a server with nothing left to do.
        if let Err(e) = self.defer.disarm() {
            warn!("disarming the deferred-flip timer: {e}");
        }
        let ids: Vec<KmsOutputId> = self.outputs.iter().map(|o| o.kms_id).collect();
        let mut painted = false;
        for id in ids {
            painted |= self.paint(id);
        }
        // The frame those clients would have ridden has gone. A wakeup
        // that wanted to paint and could not (a flip still in flight)
        // deliberately keeps the wait alive for the wakeup that can, or
        // the saturating-input case — every motion arriving mid-flip —
        // would lose the answer it was holding for.
        if painted {
            self.defer.forget_all();
        }
    }

    /// Paint, unless the only thing this frame would show is a cursor that
    /// a client is about to answer for.
    ///
    /// The whole latency fix is here, and it is a scheduling decision, not
    /// a faster anything: a pointer move damages the *cursor* immediately,
    /// so painting at once puts a flip in flight that the client's own
    /// commit — 0.12 ms behind it — then has to wait out, and the content
    /// lands one whole refresh after the arrow. Holding that flip for the
    /// few hundred microseconds it takes the answer to arrive lets both
    /// ride the same vblank.
    ///
    /// It is deferred only when every one of these holds:
    ///
    /// * a client was just told about an input and has not answered
    ///   ([`Server::note_client_input`]) — cursor movement over the bare
    ///   desktop has nobody to wait for and stays on the fast path;
    /// * something is paintable *now*. An output whose flip is still in
    ///   flight is not deferred, it is simply not being painted; that
    ///   wakeup comes back through [`Server::on_flip`], which asks again;
    /// * every output that wants a frame wants it for the cursor alone
    ///   ([`frame::OutputState::cursor_only`]) — content damage or a
    ///   commit retry both mean somebody is already waiting for those
    ///   pixels;
    /// * the timer arms. If it will not, paint: a frame nothing would ever
    ///   wake us for is far worse than a frame one refresh early.
    ///
    /// The bound is [`frame::OutputState::frame_deadline_ns`], the same
    /// next-vblank-minus-margin a client asking for a frame callback is
    /// given, so a client that never answers costs exactly nothing: the
    /// cursor still reaches that vblank, with a whole margin (ten paint
    /// passes on the test box) left to rasterize in.
    fn paint_or_defer(&mut self) {
        if self.should_defer() {
            let now_ns = monotonic_ns();
            let deadline_ns = self
                .outputs
                .iter()
                .filter(|o| o.needs_paint())
                .map(|o| o.frame_deadline_ns(now_ns))
                .min()
                .unwrap_or(now_ns);
            match self.defer.hold_until(deadline_ns) {
                Ok(()) => {
                    debug!(
                        "cursor-only flip held {} us for a client's answer",
                        deadline_ns.saturating_sub(now_ns) / 1_000
                    );
                    return;
                }
                // A timer that will not arm is the one failure that must
                // not be absorbed silently: nothing else would ever wake
                // the loop for this frame.
                Err(e) => warn!("arming the deferred-flip timer: {e}"),
            }
        }
        self.paint_all();
    }

    /// Whether this wakeup's frame should wait for a client's answer. See
    /// [`Server::paint_or_defer`] for what each clause protects.
    fn should_defer(&self) -> bool {
        if !self.active || !self.defer.awaiting() {
            return false;
        }
        let mut paintable = false;
        for output in &self.outputs {
            // An output with nothing to paint has nothing to hold back,
            // and one whose flip is still in flight is not being painted
            // either — that wakeup comes back through `on_flip`.
            if !output.needs_paint() || self.backend.flip_pending(output.kms_id) {
                continue;
            }
            if !output.cursor_only() {
                return false;
            }
            paintable = true;
        }
        paintable
    }

    /// The deferral deadline passed: the client did not answer in time, so
    /// paint the cursor on its own after all. `defer_timeouts` counts it.
    fn on_defer_deadline(&mut self) {
        self.defer.expired();
        debug!("deferred flip timed out");
        self.settle();
    }

    /// The update-then-paint pass every event that could have changed the
    /// scene ends with. Idle means both are no-ops and nothing is
    /// committed, which is what keeps a quiet server at zero wakeups.
    fn settle(&mut self) {
        // A window placed during this wakeup wants the focus — which is
        // what makes a launched app typable without a click — but could
        // not take it while its own client was lifted out of the map.
        if let Some(win) = self.pending_focus.take() {
            self.focus_window(Some(win));
        }
        self.update_scene();
        self.claim_input_stamp();
        self.paint_or_defer();
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
            // About to block: hand back any font file nothing needed this
            // turn. This is the state `VmRSS` is measured in — a desktop with
            // its labels drawn and nothing to do — and the font bytes are the
            // largest thing the server can give back there. The atlas keeps
            // the masks, so no glyph is re-rendered and nothing on screen
            // moves; a new glyph costs one re-read. See `docs/budget.md`.
            self.text.release_idle_fonts();
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
                    TOK_INPUT_HOTPLUG => self.on_input_hotplug(),
                    TOK_DEFER => self.on_defer_deadline(),
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
        // Through the deferral, not straight to `paint`: this is the
        // wakeup a saturated input rate arrives on — the motion landed
        // while this very flip was in flight, so its cursor damage is
        // sitting here waiting and the client's answer may still be a
        // wakeup away. Same rule, same deadline.
        self.paint_or_defer();
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

    /// A kernel uevent arrived on the `input` subsystem: a keyboard or
    /// mouse was plugged in or pulled out.
    ///
    /// Deferred from M1, and the same socket the DRM backend already uses.
    /// libinput's path backend does not watch `/dev/input` on its own —
    /// that is what the udev backend is for, and the udev backend is a
    /// dependency this tree does not want — so the server rescans the
    /// directory itself and tells libinput which paths appeared and
    /// disappeared.
    fn on_input_hotplug(&mut self) {
        let changed = match self.input_hotplug.as_mut() {
            Some(socket) => match socket.drain_subsystem(b"input") {
                Ok(changed) => changed,
                Err(e) => {
                    warn!("reading the input uevent socket: {e}");
                    false
                }
            },
            None => false,
        };
        if !changed {
            return;
        }
        let Some(dir) = self.input_dir.clone() else {
            return;
        };
        let (added, removed) = self.input.rescan(&dir);
        if added == 0 && removed == 0 {
            return;
        }
        info!("input hotplug: +{added} -{removed} device(s)");
        // A new device brings a new fd, which has to join the epoll set;
        // re-adding one already there is `EEXIST`, which is not an error
        // worth reporting.
        for fd in self.input.poll_fds() {
            if let Err(e) = epoll::add(
                &self.epoll,
                fd,
                EventData::new_u64(TOK_INPUT),
                EventFlags::IN,
            ) && e != rustix::io::Errno::EXIST
            {
                warn!("epoll_ctl add input fd: {e}");
            }
        }
        // A keyboard that went away may have been holding a modifier we
        // will never see released.
        if removed > 0
            && let Some(kb) = self.keyboard.as_mut()
        {
            kb.reset();
        }
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
                let sent_to = self.send_to_window(window, |id| {
                    ServerMsg::PointerAxis(msg::PointerAxis {
                        window: id,
                        dx,
                        dy,
                        source,
                        time_ns,
                    })
                });
                self.note_client_input(sent_to);
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
        // A drag in flight owns every motion: the window follows the
        // pointer and the client hears nothing at all, which is what makes
        // a drag zero round trips. A resize does send one `Configure` per
        // motion, and the frame scheduler already throttles those to one
        // per frame.
        if let Some(drag) = self.wm.drag()
            && self.drive_drag(drag)
        {
            self.note_input(time_ns);
            return;
        }
        let target = output.and_then(|id| input::hit(&self.scene, id, point));
        let now_over = target.map(|t| t.window);
        if now_over != self.pointer.over {
            if let Some(left) = self.pointer.over {
                let sent_to = self.send_to_window(left, |id| {
                    ServerMsg::PointerLeave(msg::PointerLeave {
                        window: id,
                        time_ns,
                    })
                });
                // A leave is an input the client will very plausibly
                // answer — an un-highlight — so it is worth the same wait
                // as the motion that caused it.
                self.note_client_input(sent_to);
            }
            self.pointer.over = now_over;
            if let Some(t) = target {
                let node = self.node_id_for(t.window, t.hit.node);
                let sent_to = self.send_to_window(t.window, |id| {
                    ServerMsg::PointerEnter(msg::PointerEnter {
                        window: id,
                        node,
                        pos: t.local,
                        time_ns,
                    })
                });
                self.note_client_input(sent_to);
            }
        } else if let Some(t) = target {
            let node = self.node_id_for(t.window, t.hit.node);
            let sent_to = self.send_to_window(t.window, |id| {
                ServerMsg::PointerMotion(msg::PointerMotion {
                    window: id,
                    node,
                    pos: t.local,
                    time_ns,
                })
            });
            self.note_client_input(sent_to);
        }
        self.note_input(time_ns);
    }

    fn pointer_button(&mut self, button: u32, state: ButtonState, time_ns: u64) {
        /// Linux evdev `BTN_RIGHT`.
        const BTN_RIGHT: u32 = 0x111;

        // A release always ends whatever drag was in flight, whether or not
        // the pointer is still over the window it started on: a drag that
        // survived the button coming up would follow the pointer for ever.
        if state == ButtonState::Released
            && let Some(drag) = self.wm.end_drag()
        {
            if let Drag::Button { window, region } = drag {
                // A button fires on release *inside itself*, which is what
                // lets a user change their mind by sliding off it.
                let still_on = self
                    .pointer_desktop()
                    .and_then(|p| self.frame_hit(p))
                    .is_some_and(|(w, r)| w == window && r == region);
                if still_on {
                    match region {
                        Region::Close => self.close_window(window),
                        Region::Maximize => self.toggle_maximize(window),
                        _ => {}
                    }
                }
            }
            self.note_input(time_ns);
            return;
        }

        let mods = self
            .keyboard
            .as_ref()
            .map_or_else(Mods::default, Keyboard::named_mods);
        if state == ButtonState::Pressed
            && let Some(point) = self.pointer_desktop()
        {
            // Super + drag: the server moves and resizes any window,
            // decorated or not. Checked before the frame regions so it
            // works over a client's own content too.
            if mods.logo
                && !mods.ctrl
                && !mods.alt
                && let Some((win, _)) = self.frame_hit(point)
            {
                self.raise_and_focus(win);
                if button == input::BTN_LEFT {
                    self.begin_move(win, point);
                    self.note_input(time_ns);
                    return;
                }
                if button == BTN_RIGHT && self.resizable(win) {
                    self.begin_corner_resize(win, point);
                    self.note_input(time_ns);
                    return;
                }
            }
            if button == input::BTN_LEFT
                && let Some((win, region)) = self.frame_hit(point)
                && region != Region::Content
            {
                self.raise_and_focus(win);
                match region {
                    Region::TitleBar => {
                        if self.wm.title_click(win, time_ns) {
                            self.toggle_maximize(win);
                        } else {
                            self.begin_move(win, point);
                        }
                    }
                    Region::Resize(edges) => {
                        self.begin_resize(win, edges, point);
                    }
                    Region::Close | Region::Maximize => {
                        self.wm.begin_drag(Drag::Button {
                            window: win,
                            region,
                        });
                    }
                    Region::Content => unreachable!("guarded above"),
                }
                self.note_input(time_ns);
                return;
            }
        }

        let Some(window) = self.pointer.over else {
            // A click on the desktop drops focus, which is what lets a
            // client know it stopped receiving keys.
            if state == ButtonState::Pressed {
                self.set_focus(None);
            }
            return;
        };
        if state == ButtonState::Pressed && button == input::BTN_LEFT {
            self.raise_and_focus(window);
        }
        let sent_to = self.send_to_window(window, |id| {
            ServerMsg::PointerButton(msg::PointerButton {
                window: id,
                button,
                state,
                time_ns,
            })
        });
        self.note_client_input(sent_to);
        self.note_input(time_ns);
    }

    /// Whether a window may be resized by a drag.
    fn resizable(&self, win: WindowKey) -> bool {
        self.scene.window_info(win).is_ok_and(|i| {
            !i.flags().fixed_size
                && matches!(i.state(), WindowState::Normal | WindowState::Maximized)
        })
    }

    /// Start a move drag, holding the pointer's offset into the frame.
    fn begin_move(&mut self, win: WindowKey, point: Point) {
        // Dragging a maximized window restores it first, under the cursor,
        // which is what every desktop does and the only behaviour that does
        // not silently discard the restore rectangle.
        if self
            .scene
            .window_info(win)
            .is_ok_and(|i| i.state() == WindowState::Maximized)
        {
            self.set_state(win, WindowState::Normal);
        }
        let rect = self.desktop_rect(win);
        // Keep the grab inside the frame even after a restore shrank it,
        // so the window does not jump out from under the pointer.
        let grab = Point::new(
            (point.x - rect.x).clamp(0.0, rect.w.max(0.0)),
            (point.y - rect.y).clamp(0.0, rect.h.max(0.0)),
        );
        self.wm.begin_drag(Drag::Move { window: win, grab });
    }

    /// Start a resize drag on the named edges.
    fn begin_resize(&mut self, win: WindowKey, edges: Edges, point: Point) {
        if !self.resizable(win) {
            return;
        }
        self.wm.begin_drag(Drag::Resize {
            window: win,
            edges,
            start: self.desktop_rect(win),
            origin: point,
        });
    }

    /// Start a resize drag from whichever corner the pointer is nearest,
    /// which is what `Super`+right-drag does anywhere in a window.
    fn begin_corner_resize(&mut self, win: WindowKey, point: Point) {
        let rect = self.desktop_rect(win);
        let edges = Edges::corner(
            point.x >= rect.x + rect.w / 2.0,
            point.y >= rect.y + rect.h / 2.0,
        );
        self.begin_resize(win, edges, point);
    }

    /// Toggle a window between `Maximized` and `Normal`.
    fn toggle_maximize(&mut self, win: WindowKey) {
        let Ok(state) = self.scene.window_info(win).map(nitro_scene::Window::state) else {
            return;
        };
        let next = if state == WindowState::Maximized {
            WindowState::Normal
        } else {
            WindowState::Maximized
        };
        self.set_state(win, next);
    }

    /// Toggle a window between `Fullscreen` and `Normal`.
    fn toggle_fullscreen(&mut self, win: WindowKey) {
        let Ok(state) = self.scene.window_info(win).map(nitro_scene::Window::state) else {
            return;
        };
        let next = if state == WindowState::Fullscreen {
            WindowState::Normal
        } else {
            WindowState::Fullscreen
        };
        self.set_state(win, next);
    }

    /// Tile a window to the left or right half of its work area.
    fn tile(&mut self, win: WindowKey, left: bool) {
        if !self.resizable(win) {
            return;
        }
        // Tiling is a `Normal` geometry, so a maximized window leaves that
        // state first rather than ending up half-maximized.
        if self
            .scene
            .window_info(win)
            .is_ok_and(|i| i.state() != WindowState::Normal)
        {
            self.set_state(win, WindowState::Normal);
        }
        let area = self.window_area(win);
        self.set_frame_rect(win, wm::tile_rect(area, left));
    }

    fn key(&mut self, keycode: u32, pressed: bool, time_ns: u64) {
        // Without a keymap the key still reaches the focused client, with
        // no keysym and no text: the evdev code is the part that never
        // depends on xkb, and a client that only wants raw keys still works.
        let resolved = self
            .keyboard
            .as_mut()
            .map_or_else(keyboard::KeyResolution::none, |kb| kb.key(keycode, pressed));
        if pressed && let Some(hotkey) = keyboard::hotkey(resolved.keysym, resolved.named) {
            self.hotkey(hotkey);
            // A hotkey is the compositor's, not the client's.
            self.note_input(time_ns);
            return;
        }
        // Releasing Alt ends an `Alt+Tab` cycle: the window it landed on
        // is raised and becomes the most recently used, so the *next*
        // Alt+Tab starts from there.
        if !pressed && keyboard::is_alt(resolved.keysym) && self.wm.cycling() {
            self.wm.end_cycle();
            if let Some(win) = self.focus {
                self.wm.touch(win);
                if let Err(e) = self.scene.raise(win) {
                    warn!("raise: {e}");
                }
            }
            self.note_input(time_ns);
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
        let sent_to = self.send_to_window(window, |id| {
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
        self.note_client_input(sent_to);
        self.note_input(time_ns);
    }

    /// Act on one compositor hotkey.
    ///
    /// Every window-management chord acts on the *focused* window, which
    /// is the one thing the user can always see; a chord with nothing
    /// focused is a no-op rather than a guess.
    fn hotkey(&mut self, hotkey: Hotkey) {
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
            Hotkey::CycleFocus(forward) => self.cycle_focus(forward),
            Hotkey::Close => {
                if let Some(win) = self.focus {
                    self.close_window(win);
                }
            }
            Hotkey::ToggleMaximize => {
                if let Some(win) = self.focus {
                    self.toggle_maximize(win);
                }
            }
            Hotkey::ToggleFullscreen => {
                if let Some(win) = self.focus {
                    self.toggle_fullscreen(win);
                }
            }
            Hotkey::Minimize => {
                if let Some(win) = self.focus {
                    self.set_state(win, WindowState::Minimized);
                }
            }
            Hotkey::Tile(left) => {
                if let Some(win) = self.focus {
                    self.tile(win, left);
                }
            }
            // The launcher and the terminal are M3-B; the chord is
            // reserved here so no client can claim it in the meantime.
            Hotkey::Launch => debug!("Super+Enter is reserved for the launcher"),
        }
    }

    /// Walk the MRU order one step. The focus moves at once — so the user
    /// sees where they are — but the MRU list is only reordered when Alt
    /// comes up, which is what makes repeated Tabs walk further back
    /// instead of bouncing between two windows.
    fn cycle_focus(&mut self, forward: bool) {
        let candidates = wm::cycle_candidates(&self.scene, self.wm.mru());
        let Some(win) = self.wm.cycle_next(&candidates, forward) else {
            return;
        };
        // Cycling onto a minimized window brings it back: it stayed in the
        // MRU list precisely so this would work.
        if self
            .scene
            .window_info(win)
            .is_ok_and(|i| i.state() == WindowState::Minimized)
        {
            self.set_state(win, WindowState::Normal);
        }
        self.set_focus(Some(win));
    }

    /// Drop every scrap of window-management state a closed window left
    /// behind: its frame's shaped title, its decoration node ids, its
    /// place in the MRU order and any drag holding it.
    ///
    /// The decoration *nodes* go with the window's subtree, which the
    /// scene destroys; the shaped run does not — it lives in the text
    /// store, keyed by owner, and this is the only place it can be freed.
    fn forget_window(&mut self, win: WindowKey) {
        self.decorations.remove(&win);
        let title = self.frame_titles.remove(&win);
        self.text.release(title);
        self.wm.remove(win);
        if self.focus == Some(win) {
            self.focus = None;
            // The focus goes to the next window in the MRU order rather
            // than nowhere: closing the top window should hand the
            // keyboard on, not drop it on the floor.
            let next = self.wm.mru().iter().copied().find(|w| self.focusable(*w));
            if next.is_some() {
                self.focus_window(next);
            }
        }
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
        let sent_to = self.send_to_window(win, |window| {
            ServerMsg::Touch(msg::Touch {
                window,
                id: touch_id,
                phase,
                pos,
                time_ns,
            })
        });
        self.note_client_input(sent_to);
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
    /// outputs it touches — the software cursor's rect, and nothing else.
    /// Scene damage goes through [`Server::update_scene`], which is what
    /// keeps [`frame::OutputState::cursor_only`] able to tell them apart.
    fn damage_global(&mut self, rect: nitro_core::IRect) {
        for output in &mut self.outputs {
            let Some((origin, _)) = self.scene.output_info(output.scene_id) else {
                continue;
            };
            let local = rect
                .translate(-origin.x, -origin.y)
                .intersect(&output.bounds());
            if !local.is_empty() {
                output.damage_cursor(local);
            }
        }
    }

    /// Send a message to whichever client owns `win`, naming the window
    /// with that client's own node id. Returns the client's epoll token
    /// when one owned it, so an input event can note whose answer it is
    /// now worth waiting for.
    fn send_to_window<F>(&mut self, win: WindowKey, build: F) -> Option<u64>
    where
        F: Fn(NodeId) -> ServerMsg,
    {
        let mut sent_to = None;
        for (token, client) in &mut self.wire_clients {
            if let Some(id) = client.window_id(win) {
                let msg = build(id);
                client.send(&msg);
                sent_to = Some(*token);
            }
        }
        sent_to
    }

    /// An input event has just been sent to a client: its answer is worth
    /// holding a cursor-only flip for. See [`Server::paint_or_defer`].
    ///
    /// A `None` token — the pointer is over the desktop, or over a window
    /// whose client has gone — records nothing, which is exactly how
    /// cursor movement over an empty desktop stays on the fast path.
    fn note_client_input(&mut self, token: Option<u64>) {
        if let Some(token) = token {
            self.defer.expect(token);
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
            self.restyle(old, false);
        }
        self.focus = window;
        self.wm.set_focus(window);
        if let Some(new) = window {
            self.send_to_window(new, |id| {
                ServerMsg::Focus(msg::Focus {
                    window: id,
                    focused: true,
                })
            });
            self.restyle(new, true);
        }
    }

    // ------------------------------------------------------ window management

    /// Whether a window may take keyboard focus: a `NO_FOCUS` window never
    /// does (that is what the launcher and the bar are for), and neither
    /// does a minimized one.
    fn focusable(&self, win: WindowKey) -> bool {
        self.scene.window_info(win).is_ok_and(|i| {
            i.flags().focusable && i.state() != WindowState::Minimized && i.output().is_some()
        })
    }

    /// Focus a window and mark it most-recently-used.
    fn focus_window(&mut self, window: Option<WindowKey>) {
        if let Some(win) = window {
            self.wm.touch(win);
        }
        self.set_focus(window);
    }

    /// Wrap a window in a server-owned frame group, unless it opted out.
    ///
    /// Called once, when the window is first placed: a frame is structural,
    /// so adding or removing one later would move every node under it.
    /// Fullscreen hides the decorations instead (see
    /// [`Server::apply_state_geometry`]).
    fn decorate(&mut self, win: WindowKey) {
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        if !info.flags().decorated || info.is_framed() {
            return;
        }
        let fixed = info.flags().fixed_size;
        if let Err(e) = self.scene.frame_window(win, wm::frame_insets()) {
            warn!("framing a window: {e}");
            return;
        }
        let nodes = match wm::build_frame(&mut self.scene, win, fixed) {
            Ok(n) => n,
            Err(e) => {
                warn!("building a frame: {e}");
                return;
            }
        };
        self.decorations.insert(win, nodes);
        self.restyle(win, self.focus == Some(win));
    }

    /// Restyle a window's frame for a focus change, and re-shape its title
    /// in the matching colour.
    fn restyle(&mut self, win: WindowKey, focused: bool) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        if let Err(e) = wm::style_frame(&mut self.scene, &nodes, focused) {
            warn!("styling a frame: {e}");
        }
        self.retitle(win);
    }

    /// (Re)shape a framed window's title text, eliding it to the space
    /// between the left border and the buttons.
    fn retitle(&mut self, win: WindowKey) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let title = info.title().to_owned();
        let width = self.scene.node(nodes.title).map_or(0.0, |n| n.bounds().w);
        let focused = self.focus == Some(win);
        let request = StyleRequest::new(
            "sans",
            wm::theme::TITLE_SIZE_PX,
            600,
            false,
            // No wrapping: a title bar is one line, and a title too long
            // for it is elided, not folded.
            0.0,
            false,
        );
        let elided = self.text.elide(&request, &title, width);
        let (key, shaped) = self.text.shape(ClientId::SERVER.0, &request, &elided);
        let reference = nitro_scene::TextRef {
            key: key.0,
            size: Size::new(shaped.width, shaped.height),
            ascent: shaped.ascent,
            color: wm::title_color(focused),
            align: nitro_scene::TextAlign::Left,
        };
        match self
            .scene
            .set_text(ClientId::SERVER, nodes.title, Some(reference))
        {
            Ok(()) => {
                // The node's previous run is unreachable now.
                let old = self.frame_titles.insert(win, key);
                self.text.release(old);
            }
            Err(e) => {
                warn!("setting a title: {e}");
                self.text.release(Some(key));
            }
        }
    }

    /// The work area of the output a window is on (or the primary one),
    /// in **desktop** coordinates — see [`Server::desktop_area`].
    fn window_area(&self, win: WindowKey) -> Rect {
        let output = self
            .scene
            .window_info(win)
            .ok()
            .and_then(nitro_scene::Window::output)
            .or_else(|| self.outputs.first().map(|o| o.scene_id));
        output.map_or(Rect::EMPTY, |id| self.desktop_area(id))
    }

    /// An output's work area in **desktop** coordinates.
    ///
    /// The scene positions a window in its *own output's* logical space,
    /// with the origin at that output's top-left corner — which is right
    /// for the scene, and useless for a window manager the moment there
    /// are two outputs: a drag crosses between them, and two windows on
    /// different screens can have the same position and not be in the same
    /// place. So every rectangle in the window manager is in one desktop
    /// space: each output's logical area, offset by
    /// [`Server::desktop_origin`]. The conversion happens at exactly two
    /// boundaries — here on the way out, and `place_window` on the way in.
    fn desktop_area(&self, id: SceneOutputId) -> Rect {
        let area = wm::work_area(&self.scene, id);
        let origin = self.desktop_origin(id);
        Rect::new(origin.x, origin.y, area.w, area.h)
    }

    /// Where an output's logical space starts in the desktop space.
    ///
    /// Outputs are laid out left to right in *device* pixels; in desktop
    /// logical units each one starts where the previous ended, at its own
    /// scale, so a 2x output takes half as much desktop width as its
    /// device width.
    fn desktop_origin(&self, id: SceneOutputId) -> Point {
        let mut x = 0.0;
        for (out, rect, scale) in self.scene.outputs() {
            if out == id {
                return Point::new(x, 0.0);
            }
            let s = if scale > 0.0 { scale } else { 1.0 };
            x += rect.w as f32 / s;
        }
        Point::ZERO
    }

    /// A window's frame rectangle in desktop coordinates.
    fn desktop_rect(&self, win: WindowKey) -> Rect {
        let Ok(info) = self.scene.window_info(win) else {
            return Rect::EMPTY;
        };
        let origin = info
            .output()
            .map_or(Point::ZERO, |id| self.desktop_origin(id));
        let r = info.frame_rect();
        Rect::new(r.x + origin.x, r.y + origin.y, r.w, r.h)
    }

    /// The output a desktop-space point falls on, and that point in the
    /// output's own logical space.
    fn output_for(&self, point: Point) -> Option<(SceneOutputId, Point)> {
        for (id, rect, scale) in self.scene.outputs() {
            let s = if scale > 0.0 { scale } else { 1.0 };
            let origin = self.desktop_origin(id);
            let (w, h) = (rect.w as f32 / s, rect.h as f32 / s);
            if point.x >= origin.x
                && point.y >= origin.y
                && point.x < origin.x + w
                && point.y < origin.y + h
            {
                return Some((id, Point::new(point.x - origin.x, point.y - origin.y)));
            }
        }
        None
    }

    /// Move a window's frame to a **desktop**-space position, without
    /// changing its size, re-homing it onto whichever output now contains
    /// its centre.
    ///
    /// One scene mutation and one `Configure`; no round trip, which is the
    /// whole point of server-side moves.
    fn move_window(&mut self, win: WindowKey, position: Point) {
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let size = info.frame_size();
        let current = info.output();
        // "The output containing its centre" is the rule a user can
        // predict: a window is on the screen it mostly is on, and dragging
        // it more than halfway across hands it over.
        let centre = Point::new(position.x + size.w / 2.0, position.y + size.h / 2.0);
        // Off every screen — only reachable while a hotplug is in flight.
        // Keep the window where it is rather than unplacing it.
        let Some((output, _)) = self
            .output_for(centre)
            .or_else(|| self.output_for(position))
        else {
            return;
        };
        let origin = self.desktop_origin(output);
        let local = Point::new(position.x - origin.x, position.y - origin.y);
        if current == Some(output) && info.position() == local {
            return;
        }
        if let Err(e) = self.scene.place_window(win, Some(output), local) {
            warn!("moving a window: {e}");
            return;
        }
        self.configure(win);
    }

    /// Set a window's **frame** rectangle: position and content size at
    /// once, with the content size clamped to the client's limits.
    fn set_frame_rect(&mut self, win: WindowKey, rect: Rect) {
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let inset = info.inset();
        // A resize never changes which output a window is on: it is the
        // opposite edge that moves, and handing a window over mid-resize
        // would be a surprise. So it keeps its current output and only its
        // position within it is recomputed.
        let output = info
            .output()
            .or_else(|| self.outputs.first().map(|o| o.scene_id));
        let origin = output.map_or(Point::ZERO, |id| self.desktop_origin(id));
        let content = Size::new(
            (rect.w - inset.width()).max(0.0),
            (rect.h - inset.height()).max(0.0),
        );
        let content = self.scene.clamp_to_limits(win, content);
        if let Err(e) = self.scene.set_window_size(ClientId::SERVER, win, content) {
            warn!("resizing a window: {e}");
            return;
        }
        if let Err(e) = self.scene.place_window(
            win,
            output,
            Point::new(rect.x - origin.x, rect.y - origin.y),
        ) {
            warn!("placing a resized window: {e}");
        }
        self.relayout_frame(win);
        self.configure(win);
    }

    /// Re-lay a window's decorations for its current size.
    fn relayout_frame(&mut self, win: WindowKey) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        if let Err(e) = wm::layout_frame(&mut self.scene, win, &nodes) {
            warn!("laying out a frame: {e}");
            return;
        }
        // The title's box changed width, so its elision did too.
        self.retitle(win);
    }

    /// Put a window into a state and give it the geometry that state
    /// implies, remembering where it was so `Normal` can put it back.
    fn set_state(&mut self, win: WindowKey, state: WindowState) {
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        if info.state() == state {
            return;
        }
        // A window that cannot be resized cannot be maximized or made
        // fullscreen either; refusing is silent, exactly as `docs/wire.md`
        // says (there is no per-request error in this protocol).
        if info.flags().fixed_size
            && matches!(state, WindowState::Maximized | WindowState::Fullscreen)
        {
            return;
        }
        let was_normal = info.state() == WindowState::Normal;
        if was_normal && matches!(state, WindowState::Maximized | WindowState::Fullscreen) {
            // Remembered in *desktop* coordinates, like every other
            // window-manager rectangle: the window may come back out of
            // maximize on a different output than it went in on.
            let rect = self.desktop_rect(win);
            let restore = (Point::new(rect.x, rect.y), info.size());
            let _ = self.scene.set_window_restore(win, Some(restore));
        }
        if let Err(e) = self.scene.set_window_state(win, state) {
            warn!("setting a window state: {e}");
            return;
        }
        self.apply_state_geometry(win, state);
        self.announce_state(win, state);
        if state == WindowState::Minimized {
            // Putting a window away makes it the *least* recently used, not
            // the most: leaving it at the front is what makes the first
            // `Alt+Tab` after a minimize land on the window that already
            // has focus and appear to do nothing at all.
            self.wm.demote(win);
            if self.focus == Some(win) {
                // The focus has to go somewhere reachable, or the keyboard
                // is lost until the user clicks.
                let next = self
                    .wm
                    .mru()
                    .iter()
                    .copied()
                    .find(|w| *w != win && self.focusable(*w));
                self.set_focus(next);
            }
        }
    }

    /// Give a window the geometry its state implies, on its current output.
    ///
    /// `Fullscreen` covers the whole output and hides the decorations;
    /// `Maximized` fills the work area and keeps them; `Normal` goes back
    /// to the remembered rectangle; `Minimized` does not move anything, so
    /// un-minimizing lands where it was.
    fn apply_state_geometry(&mut self, win: WindowKey, state: WindowState) {
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let output = info.output();
        let framed = info.is_framed();
        let area = self.window_area(win);
        // Fullscreen covers the whole output, work area or not — in
        // desktop coordinates, like everything else here.
        let full = output
            .and_then(|id| self.scene.output_info(id).map(|i| (id, i)))
            .map_or(area, |(id, (rect, scale))| {
                let s = if scale > 0.0 { scale } else { 1.0 };
                let origin = self.desktop_origin(id);
                Rect::new(origin.x, origin.y, rect.w as f32 / s, rect.h as f32 / s)
            });
        match state {
            WindowState::Minimized => {}
            WindowState::Maximized => {
                if framed {
                    let _ = self.scene.set_window_inset(win, wm::frame_insets());
                }
                self.set_frame_rect(win, area);
            }
            WindowState::Fullscreen => {
                // Decorations are hidden rather than destroyed: the frame
                // group stays, its insets go to zero, and leaving
                // fullscreen simply puts them back.
                if framed {
                    let _ = self.scene.set_window_inset(win, nitro_scene::Insets::NONE);
                    self.set_frame_visible(win, false);
                }
                self.set_frame_rect(win, full);
            }
            WindowState::Normal => {
                if framed {
                    let _ = self.scene.set_window_inset(win, wm::frame_insets());
                    self.set_frame_visible(win, true);
                }
                let inset = wm::frame_insets();
                let restore = self
                    .scene
                    .window_info(win)
                    .ok()
                    .and_then(nitro_scene::Window::restore);
                if let Some((pos, size)) = restore {
                    let frame = if framed {
                        Size::new(size.w + inset.width(), size.h + inset.height())
                    } else {
                        size
                    };
                    let pos = wm::clamp_into(pos, frame, area);
                    self.set_frame_rect(win, Rect::new(pos.x, pos.y, frame.w, frame.h));
                }
                let _ = self.scene.set_window_restore(win, None);
            }
        }
    }

    /// Show or hide a framed window's decorations (fullscreen).
    fn set_frame_visible(&mut self, win: WindowKey, visible: bool) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        let s = ClientId::SERVER;
        for key in [
            Some(nodes.background),
            Some(nodes.bar),
            Some(nodes.title),
            Some(nodes.close),
            nodes.maximize,
        ]
        .into_iter()
        .flatten()
        {
            if let Err(e) = self.scene.set_visible(s, key, visible) {
                warn!("hiding a decoration: {e}");
            }
        }
    }

    /// Tell the owning client its window changed state.
    fn announce_state(&mut self, win: WindowKey, state: WindowState) {
        let wire = clients::wire_state(state);
        self.send_to_window(win, |window| {
            ServerMsg::WindowState(msg::WindowState {
                window,
                state: wire,
            })
        });
    }

    /// Close a window the way a user asks for it: the client is told, and
    /// the window goes when the client destroys its root.
    ///
    /// The server does not tear the window down itself — a `Closed` the
    /// client has not acted on is a chance to save, and a client that
    /// ignores it keeps a window that is still on screen and still honest.
    fn close_window(&mut self, win: WindowKey) {
        self.send_to_window(win, |window| ServerMsg::Closed(msg::Closed { window }));
    }

    /// Raise a window within its layer and focus it, if it may be focused.
    fn raise_and_focus(&mut self, win: WindowKey) {
        // Raise within the Normal layer only: a click must not pull a
        // panel out from under a menu, or a menu below its panel.
        if self
            .scene
            .window_info(win)
            .is_ok_and(|w| w.layer() == nitro_scene::Layer::Normal)
            && let Err(e) = self.scene.raise(win)
        {
            warn!("raise: {e}");
        }
        if self.focusable(win) {
            self.focus_window(Some(win));
        }
    }

    /// The pointer in **desktop** coordinates — the space every window
    /// rectangle in the window manager lives in. See
    /// [`Server::desktop_area`].
    fn pointer_desktop(&self) -> Option<Point> {
        let point = self.pointer.position();
        let id = input::output_at(&self.scene, point)?;
        let (rect, scale) = self.scene.output_info(id)?;
        let s = if scale > 0.0 { scale } else { 1.0 };
        let origin = self.desktop_origin(id);
        Some(Point::new(
            (point.x - rect.x as f32) / s + origin.x,
            (point.y - rect.y as f32) / s + origin.y,
        ))
    }

    /// Which window's frame the pointer is over, and where in it.
    ///
    /// Front to back through the z-order, skipping minimized windows: a
    /// hidden window must not swallow a click, which is also why this is a
    /// separate walk from the scene's own hit test (that one only knows
    /// about *painted* nodes and would never see a resize band outside the
    /// window at all).
    fn frame_hit(&self, point: Point) -> Option<(WindowKey, Region)> {
        // The pointer decides which output's stack to walk, but the
        // *resize bands* reach outside a window, so a grab just past a
        // screen edge has to find the window on the other side of it: walk
        // every output, frontmost stack first.
        let outputs: Vec<SceneOutputId> = self.outputs.iter().map(|o| o.scene_id).collect();
        for id in outputs {
            let origin = self.desktop_origin(id);
            let local = Point::new(point.x - origin.x, point.y - origin.y);
            for win in self.scene.windows_front_to_back(id) {
                let Ok(info) = self.scene.window_info(win) else {
                    continue;
                };
                if info.state() == WindowState::Minimized {
                    continue;
                }
                if !info.is_framed() {
                    // Undecorated: it is all content, and the scene's own
                    // hit test decides whether the click lands in it.
                    if wm::contains(info.frame_rect(), local) {
                        return Some((win, Region::Content));
                    }
                    continue;
                }
                if let Some(region) = wm::hit_frame(
                    info.frame_rect(),
                    info.inset(),
                    local,
                    info.flags().fixed_size,
                ) {
                    return Some((win, region));
                }
            }
        }
        None
    }

    /// Advance whatever drag is in flight to the pointer's position.
    /// Returns whether anything moved (so the caller can skip the client
    /// event it would otherwise send).
    fn drive_drag(&mut self, drag: Drag) -> bool {
        let Some(point) = self.pointer_desktop() else {
            return true;
        };
        match drag {
            Drag::Button { .. } => false,
            Drag::Move { window, grab } => {
                let area = self.window_area(window);
                let Ok(info) = self.scene.window_info(window) else {
                    return true;
                };
                let size = info.frame_size();
                let want = Point::new(point.x - grab.x, point.y - grab.y);
                // A dragged window may cross onto another output; the
                // clamp is to the *union* of the outputs, not to one of
                // them, or a window could never leave its own screen.
                // `move_window` re-homes it once its centre lands there.
                let pos = self.clamp_to_desktop(want, size, area);
                self.move_window(window, pos);
                true
            }
            Drag::Resize {
                window,
                edges,
                start,
                origin,
            } => {
                let Ok(info) = self.scene.window_info(window) else {
                    return true;
                };
                let (inset, min, max) = (info.inset(), info.min_size(), info.max_size());
                let rect = wm::resize_rect(start, origin, point, edges, inset, min, max);
                self.set_frame_rect(window, rect);
                true
            }
        }
    }

    /// Clamp a frame origin so the window stays reachable: inside the
    /// union of every output's logical area, with at least a title bar's
    /// worth of it on screen.
    fn clamp_to_desktop(&self, pos: Point, size: Size, fallback: Rect) -> Point {
        let mut union: Option<Rect> = None;
        for (id, _, _) in self.scene.outputs() {
            let area = self.desktop_area(id);
            union = Some(match union {
                Some(u) => Rect::from_corners(
                    Point::new(u.x.min(area.x), u.y.min(area.y)),
                    Point::new(
                        (u.x + u.w).max(area.x + area.w),
                        (u.y + u.h).max(area.y + area.h),
                    ),
                ),
                None => area,
            });
        }
        let area = union.unwrap_or(fallback);
        // The whole title bar must stay grabbable: the window may hang off
        // the right and bottom edges, but never so far up or left that
        // there is nothing left to grab.
        let min_visible = wm::TITLE_H.min(size.w).max(1.0);
        Point::new(
            pos.x.clamp(
                area.x - (size.w - min_visible).max(0.0),
                area.x + area.w - min_visible,
            ),
            pos.y.clamp(area.y, area.y + area.h - min_visible),
        )
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
            Ok(Request::Unplug) => self.unplug(),
            Ok(Request::Focus) => self.focus_topmost(),
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
            // The deferral counters sit next to the flip statistics
            // because that is what they describe: how often a flip was
            // held for a client's answer, and how often that answer never
            // came. A `defer_timeouts` that tracks `flips_deferred` is a
            // client that is not responding — the cursor is fine, it is
            // reaching every vblank, but nothing is riding with it.
            ("flips_deferred", self.defer.deferred),
            ("defer_timeouts", self.defer.timeouts),
        ];
        self.stats.write_pairs(&mut pairs);
        self.text.write_pairs(&mut pairs);
        pairs.push(("clients", self.wire_clients.len() as u64));
        pairs.push(("windows", self.scene.window_count() as u64));
        pairs.push(("nodes", self.scene.node_count() as u64));
        pairs.push(("outputs", self.outputs.len() as u64));
        // The window-management view: how many windows carry a
        // server-drawn frame, how many are hidden, and whether a drag is
        // in flight. `decorated` under `windows` is the opt-out count.
        pairs.push(("decorated", self.decorations.len() as u64));
        let minimized = self
            .wm
            .mru()
            .iter()
            .filter(|w| {
                self.scene
                    .window_info(**w)
                    .is_ok_and(|i| i.state() == WindowState::Minimized)
            })
            .count();
        pairs.push(("minimized", minimized as u64));
        pairs.push(("dragging", u64::from(self.wm.drag().is_some())));
        pairs.push(("focused", u64::from(self.focus.is_some())));
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
                let caps = self.caps();
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                info!("wire client {} is {:?}", client.id.0, hello.name);
                if let Err(e) = client.stream.welcome(SERVER_NAME, caps) {
                    warn!("welcome: {e}");
                    return false;
                }
                true
            }
            ClientMsg::MeasureText(m) => {
                // Answered on receipt, not at the commit. Every other
                // message is a mutation and waits its turn; this one is a
                // *question*, and a text field that had to commit before it
                // could learn how wide its own content is would need a
                // frame per keystroke.
                let request = StyleRequest::new(
                    &m.family,
                    m.size_px,
                    m.weight,
                    m.italic,
                    m.max_width,
                    m.wrap,
                );
                let metrics = self.text.measure(&request, &m.text);
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                client.send(&ServerMsg::TextMeasured(msg::TextMeasured {
                    request: m.request,
                    width: metrics.width,
                    height: metrics.height,
                    ascent: metrics.ascent,
                    descent: metrics.descent,
                    line_count: metrics.line_count,
                    cursor_x: metrics
                        .cursor_x
                        .into_iter()
                        .map(|(offset, x)| nitro_wire::types::CursorPos { offset, x })
                        .collect(),
                }));
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

    /// Capability bits reported in `Welcome`.
    ///
    /// `WM` is unconditional: the server always manages windows, so a
    /// client may always send the M3 window ops. `TEXT` is set only when a
    /// font was actually found: the bit means "you may send `Text` nodes",
    /// and on a box with no fonts at all that would be a promise the server
    /// cannot keep. `DIRECT_SCANOUT` and `DMABUF` remain later milestones,
    /// and a zero bit is the protocol's way of saying "do not use this".
    fn caps(&self) -> u32 {
        let mut caps = nitro_wire::types::caps::WM;
        if self.text.has_fonts() {
            caps |= nitro_wire::types::caps::TEXT;
        }
        caps
    }

    /// Apply a client's transaction. Returns whether the client survives.
    fn commit(&mut self, token: u64, serial: u32) -> bool {
        // This is the answer a deferred flip was waiting for. Noted before
        // the transaction is applied rather than after: the client has
        // spoken either way, and a transaction that turns out to be fatal
        // must not leave the cursor held hostage to a dead connection.
        self.defer.forget(token);
        let Some(mut client) = self.wire_clients.remove(&token) else {
            return false;
        };
        // The buffer descriptors arrived with their messages; hand them to
        // the client's map once the scene has minted the keys.
        let result = clients::apply(&mut client, &mut self.scene, &mut self.text, serial);
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
        // Every node this transaction (re)shaped gets its measured size
        // back. Sent after the batch was applied, so a client that set the
        // text of several nodes in one commit sees one message per node and
        // in the order it asked for them.
        for (node, metrics) in outcome.text_metrics {
            debug_assert_eq!(metrics.node, node);
            client.send(&ServerMsg::TextMetrics(metrics));
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
            self.forget_window(win);
        }
        // The title bar is the server's, so a retitle is a repaint the
        // client never asks for and never sees.
        for win in outcome.retitled {
            self.retitle(win);
        }
        // State requests are applied last, after every geometry mutation
        // in the batch: `Maximized` has to win over the client's own
        // `SetBounds`, not race it. `set_state` reaches the owning client
        // by token, so the client goes back in the map first and the rest
        // of this function works through it.
        let has_states = !outcome.state_requests.is_empty();
        self.wire_clients.insert(token, client);
        for (win, state) in outcome.state_requests {
            self.set_state(win, state);
        }
        let Some(mut client) = self.wire_clients.remove(&token) else {
            // Only reachable if a state change disconnected the client,
            // which nothing in `set_state` does; be total rather than
            // clever about it.
            debug_assert!(has_states, "the client vanished without a state request");
            return false;
        };
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

    /// Place a newly created window: decorate it, centred-cascade it into
    /// the primary output's work area and tell the client the size, scale
    /// and output it got.
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
        // Decorate before placing: the frame changes the window's outer
        // size, and the placement has to know it to centre the thing the
        // user actually sees.
        self.decorate(win);
        let area = wm::work_area(&self.scene, scene_id);
        let scale = self.scene.output_info(scene_id).map_or(1.0, |(_, s)| s);
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let (size, frame) = (info.size(), info.frame_size());
        let position = wm::place(self.wm.next_placement(), frame, area);
        self.windows_created += 1;
        if let Err(e) = self.scene.place_window(win, Some(scene_id), position) {
            warn!("place window: {e}");
            return;
        }
        self.wm.add(win);
        // Focus is *deferred*, not taken here: this runs with the owning
        // client lifted out of `wire_clients` (a commit holds it), so a
        // `Focus` sent now would find no client to send it to. `settle`
        // applies it on the way out, with every client back in the map.
        if self.focusable(win) {
            self.pending_focus = Some(win);
        }
        let content = self
            .scene
            .window_info(win)
            .map_or(position, nitro_scene::Window::content_position);
        client.send(&ServerMsg::Configure(msg::Configure {
            window: node_id,
            size,
            position: content,
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
        // A client that is gone will never answer, and a flip held for it
        // would sit out its whole deadline for nothing.
        self.defer.forget(token);
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
            self.forget_window(win);
        }
        for key in client.buffers.values().copied().collect::<Vec<_>>() {
            let _ = self.scene.destroy_buffer(id, key);
            self.buffer_sources.remove(&(id, key));
        }
        self.pending_fds.retain(|(t, _, _, _)| *t != token);
        // Every shaped run the client's nodes held. The scene's destroy
        // walk drops the nodes, but the runs live in the text store, which
        // knows them only by owner — so this is the one place they are
        // freed, and a shell that restarts its clients would otherwise leak
        // a glyph vector per label per restart.
        self.text.release_owner(id.0);
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

    /// Unplug the last output from the fake backend.
    ///
    /// Test-only, like [`Server::plug`], and the half that matters to the
    /// window manager: removing an output orphans its windows, and
    /// migrating them is the behaviour under test.
    fn unplug(&mut self) -> Vec<u8> {
        if !self.backend.simulate_unplug() {
            return protocol::err_reply("`unplug` needs the fake backend and an output to remove");
        }
        info!("unplug requested");
        protocol::ok_reply()
    }

    /// Focus the topmost window on the first output, for a test.
    ///
    /// Focus normally follows a click, which is the right policy for a
    /// desktop and the wrong one for a toolkit test: synthesising a
    /// click to get focus would move the focus to whatever widget was
    /// under the pointer, which is exactly the state a focus test is
    /// about to assert on.
    fn focus_topmost(&mut self) -> Vec<u8> {
        let Some(output) = self.backend.outputs().first().map(|o| o.id) else {
            return protocol::err_reply("no outputs");
        };
        // Scene output ids mirror the backend's, one for one; see where
        // outputs are registered in `rescan`.
        let scene_output = SceneOutputId(output.0);
        let Some(window) = self.scene.windows_front_to_back(scene_output).next() else {
            return protocol::err_reply("no windows");
        };
        self.set_focus(Some(window));
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
