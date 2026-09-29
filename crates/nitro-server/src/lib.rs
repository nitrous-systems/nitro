//! `nitro-server` — the display server.
//!
//! One thread, one epoll, a seat, a KMS backend, a [`nitro_scene`] scene
//! graph, [`nitro_raster`] for pixels, [`nitro_wire`] for clients and
//! libinput/xkbcommon for input. [`run`] takes a [`Config`] and returns
//! when asked to quit, so tests drive the whole loop in-process against
//! [`nitro_kms::FakeBackend`] with a [`input::FakeSource`]; `main.rs` only
//! turns environment variables into a `Config`.
//!
//! Event loop (level-triggered epoll; two timerfds, the deferred-flip
//! deadline and key repeat, each armed only while it has something to do —
//! so an idle server still never wakes):
//!
//! | fd                 | on readable                                         |
//! |--------------------|-----------------------------------------------------|
//! | seat               | `Seat::dispatch`: `Disable` → suspend input, pause, ack; `Enable` → resume, full repaint |
//! | backend `poll_fds` | `Backend::dispatch`: `Flipped` → present + paint the next frame, `Hotplug` → rescan |
//! | libinput           | dispatch, convert, route, paint if anything moved   |
//! | repeat timerfd     | send the held key again ([`repeat`]); armed only while a key is held |
//! | signal self-pipe   | SIGTERM/SIGINT → orderly shutdown                    |
//! | SIGHUP self-pipe   | re-read `server.conf` and apply it                   |
//! | config inotify     | the configuration directory changed → the same reload |
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
pub mod config;
pub mod control;
pub mod cursor;
pub mod data;
pub mod defer;
pub mod desktop_index;
pub mod dmabuf;
pub mod frame;
pub mod gpu;
pub mod icon_theme;
pub mod icons;
pub mod inject;
pub mod input;
pub mod keyboard;
pub mod lock;
pub mod logging;
pub mod overview;
pub mod planes;
pub mod popup;
pub mod protocol;
pub mod remote;
pub mod render;
pub mod repeat;
pub mod share;
pub mod shell;
pub mod signals;
pub mod stats;
pub mod surface;
/// In-process server for another crate's tests; see the module docs.
#[cfg(feature = "test-support")]
pub mod test_support;
pub mod text;
pub mod wm;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use nitro_core::{Damage, Point, Rect, Size};
use nitro_kms::{
    Backend, DrmBackend, DrmOptions, Error as KmsError, Event, FakeBackend, ModeRequest, Modeline,
    OutputId as KmsOutputId, OutputInfo, Rect as KmsRect,
};
use nitro_raster::Canvas;
use nitro_scene::{
    BufferKey, ClientId, DamageSink, OutputId as SceneOutputId, Scene, WindowKey, WindowState,
};
use nitro_seat::{Device, Seat, SeatEvent};
use nitro_wire::msg::{self, ClientMsg, ServerMsg};
use nitro_wire::server::Listener as WireListener;
use nitro_wire::types::{ButtonState, ErrorCode, NodeId, ShareToken};
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::clients::{ApplyError, HeldBuffer, Pending, WireClient};
use crate::control::{Client, ReadOutcome};
use crate::cursor::Cursor;
use crate::defer::DeferredFlip;
use crate::frame::{CursorState, OutputState};
use crate::icons::IconEngine;
use crate::input::{InputEvent, InputSource, LibinputSource, Pointer};
use crate::keyboard::{Hotkey, Keyboard, Mods};
use crate::protocol::Request;
use crate::remote::RemoteListener;
use crate::stats::FrameStats;
use crate::text::{StyleRequest, TextEngine};
use crate::wm::{Drag, Edges, FrameNodes, Region, WindowManager};

/// Server name reported in `Welcome`.
pub const SERVER_NAME: &str = "nitro";

/// The uid of a local socket's peer (`SO_PEERCRED`), for the share
/// token's same-uid check (#3904).
fn peer_uid(fd: std::os::fd::BorrowedFd<'_>) -> Option<u32> {
    rustix::net::sockopt::socket_peercred(fd)
        .ok()
        .map(|c| c.uid.as_raw())
}

/// What a **remote** client is told when it sends a buffer op.
///
/// One constant because it is sent from two places — the decoder's
/// refusal of the fd-carrying buffer op (`CreateBuffer`; the decoder's
/// other fd ops are a fatal `Protocol`, not this), and the check on the
/// two ops that only *name* a buffer — and a client that meets both
/// should not get two different explanations of one rule.
const REMOTE_NO_BUFFERS: &str = "buffers are not available on a remote link: \
     file descriptors cannot be passed over TCP (caps::REMOTE, docs/remote.md)";

/// Largest number of outstanding selection requests one connection may
/// have made.
///
/// The clipboard's peer of `MAX_PENDING_FDS`. A request parks server state
/// until the owner answers it, and an owner is under no obligation to be
/// quick, so a client that fires requests and never reads the answers
/// would grow that state without bound. The 17th outstanding request is
/// **answered immediately with an EOF descriptor** rather than refused:
/// the requester already has exactly one code path for "I cannot serve
/// that", so the bound costs no new error, and the client that feels it is
/// the one misbehaving. See `docs/wire.md` § Receive-side limits.
pub const MAX_PENDING_SELECTIONS: usize = 16;

/// Most buffers one `AllocSurfaceBuffers` may ask for (#3914).
pub const MAX_SCANOUT_ALLOC: u8 = 4;

/// Largest width or height of a server-allocated scanout buffer (#3914).
pub const MAX_SCANOUT_DIM: u32 = 8192;

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
// Independent switches, each its own environment variable.
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// Display backend.
    pub backend: BackendKind,
    /// Control socket path (see [`control::resolve`]).
    pub control_path: PathBuf,
    /// Wire socket path; clients find it through `NITRO_SOCKET`.
    pub wire_path: PathBuf,
    /// **Shell** socket path; privileged clients find it through
    /// `NITRO_SHELL_SOCKET`. A connection accepted here is granted
    /// `caps::SHELL` — the socket *is* the privilege (`docs/shell.md`).
    pub shell_path: PathBuf,
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
    /// Per-output mode overrides by connector name. `main.rs` fills this
    /// from `NITRO_MODE` / `NITRO_MODELINE`; a test sets it directly, for
    /// the same reason `scales` is a field.
    pub modes: HashMap<String, ModeRequest>,
    /// The mode table the **fake** backend's output offers, as
    /// `(width, height, refresh_mhz)` with the first entry preferred.
    ///
    /// Empty is the historical shape: one output at exactly
    /// [`BackendKind::Fake`]'s size and 60 Hz, which is what every test
    /// that does not care about modes still gets. A test that *does* care
    /// fills this in, and `output.<c>.mode` then has something to choose
    /// from without a monitor — the same [`select_mode`](nitro_kms::drm::select::select_mode)
    /// the DRM backend runs, over a table a test wrote.
    pub fake_modes: Vec<(u32, u32, u32)>,
    /// The fake output's plane inventory; empty is the fake's default
    /// single XR24/AR24 primary. Tests of the scanout-buffer format default
    /// (#3914) list an NV12 or YUYV overlay here.
    pub fake_planes: Vec<nitro_kms::FakePlaneSpec>,
    /// Paint into a heap shadow buffer per output and stream the damage
    /// into the scanout buffer, rather than rasterizing straight into it.
    ///
    /// On by default — it is ~4× on real (write-combined) framebuffers,
    /// see `crates/nitro-server/src/frame.rs`. `NITRO_SHADOW=0` turns it
    /// off so the two can be measured against each other on hardware; a
    /// test sets it directly, for the same reason `scales` is a field.
    pub shadow: bool,
    /// Serve a pure-translation scroll by moving pixels already in the
    /// shadow and rasterizing only what that cannot provide
    /// (`frame::blit_region`). Needs `shadow`. On by default;
    /// `NITRO_SCROLL_BLIT=0` turns it off, so the two can be compared on
    /// hardware and a test can drive a reference server beside it.
    pub scroll_blit: bool,
    /// The environment's override of `overview.animate`
    /// (`NITRO_OVERVIEW_ATLAS=0|1`): `Some(true)` gives every output an
    /// overview thumbnail atlas (#3902) — one opaque buffer of the
    /// output's size, allocated when the output appears, so the overview
    /// animates out of 1:1 copies and pressing Super allocates nothing —
    /// and `Some(false)` never allocates one. `None` (the default) follows
    /// the file, whose default is **off**: overview snaps with a direct
    /// repaint, which is also what a failed allocation falls back to.
    pub overview_atlas: Option<bool>,
    /// Start with the session **locked** and no lock owner: nothing but the
    /// background is drawn and no window receives input until a shell
    /// client sends `Lock` (and so owns the lock), and then only its
    /// windows do. `NITRO_LOCKED=1`. See [`lock`].
    pub locked: bool,
    /// Where `server.conf` lives (see [`config`]). `None` means there is
    /// no file and no watch, which is what a test wants and what a service
    /// with neither `$XDG_CONFIG_HOME` nor `$HOME` gets. `main.rs` fills
    /// it from [`config::path`].
    pub config_path: Option<PathBuf>,
    /// Icon-theme base directories, replacing the XDG search path.
    ///
    /// `None` means the real one. A test sets it for the reason `scales`
    /// is a field rather than an environment variable: the environment is
    /// process-global and the tests run as threads of one process, so
    /// `NITRO_ICON_PATH` set by one test would decide another's answer.
    /// It is also the only way to make an application-icon test
    /// deterministic at all — otherwise it asserts about whatever theme
    /// the machine running it happens to have installed.
    pub icon_dirs: Option<Vec<PathBuf>>,
    /// `.desktop` search directories, replacing the XDG ones
    /// ([`crate::desktop_index`]).
    ///
    /// `None` means the real ones. A test sets it for exactly the reason
    /// `icon_dirs` exists: the `app_id → Icon=` hop reads files the
    /// distribution wrote, and a test that used the box's own
    /// `/usr/share/applications` would assert about whatever is
    /// installed there.
    pub desktop_dirs: Option<Vec<PathBuf>>,
    /// The environment's override of `gpu.helper` (`NITRO_GPU`), #3922.
    /// [`Config::fake`] sets `Some(Off)`, so a test that does not ask for
    /// the helper never starts one.
    pub gpu: Option<config::GpuHelper>,
    /// The helper binary (`NITRO_GPU_HELPER`); `None` finds
    /// `nitro-gpu-vulkan` next to the server, else on `$PATH`.
    pub gpu_helper: Option<PathBuf>,
    /// Tests: run the helper in-process on the socket end this is handed,
    /// instead of `exec`ing a binary.
    pub gpu_spawner: Option<gpu::Spawner>,
}

impl Config {
    /// The fake backend at `width × height`, sockets next to `path`, no
    /// signal handlers, no input devices and **no configuration file**.
    /// What tests want.
    ///
    /// `config_path: None` rather than the real one: a test must not read
    /// the developer's own `~/.config/nitro/server.conf`, which would make
    /// its result depend on the box it runs on. A configuration test sets
    /// the field to a file in its own temporary directory.
    ///
    /// `desktop_dirs: Some(vec![])` for exactly that reason one step
    /// further out: the `app_id → .desktop → Icon=` hop reads whatever
    /// `/usr/share/applications` holds, so a test left on the real path
    /// would resolve `nitro-calc` on a packager's box and not on anyone
    /// else's. `icon_dirs` stays `None` because the icon *theme* is
    /// already inert without one — `IconTheme` finds no files and answers
    /// `BadIcon` — whereas an application directory full of entries is
    /// the normal state of a developer machine.
    #[must_use]
    pub fn fake(width: u32, height: u32, path: impl Into<PathBuf>) -> Self {
        let control_path: PathBuf = path.into();
        let wire_path = control_path.with_file_name("wire.sock");
        let shell_path = control_path.with_file_name("shell.sock");
        Self {
            backend: BackendKind::Fake { width, height },
            control_path,
            wire_path,
            shell_path,
            handle_signals: false,
            input_dir: None,
            fake_input: None,
            scales: HashMap::new(),
            modes: HashMap::new(),
            fake_modes: Vec::new(),
            fake_planes: Vec::new(),
            shadow: true,
            scroll_blit: true,
            overview_atlas: None,
            locked: false,
            config_path: None,
            icon_dirs: None,
            desktop_dirs: Some(Vec::new()),
            gpu: Some(config::GpuHelper::Off),
            gpu_helper: None,
            gpu_spawner: None,
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

/// Parse per-output mode overrides, `<name>=<mode>,…`.
///
/// `NITRO_MODE=HDMI-A-1=1920x1080@120`, in exactly the style `NITRO_SCALE`
/// established and for the same reason: a one-off measurement or a test
/// must be able to say "run this connector at this rate" without editing
/// the box's configuration file, and the environment is the *development*
/// channel that beats the file.
///
/// A mode spec has no comma in it, so the split is unambiguous. A bad
/// entry is a warning and is skipped, like every other configuration
/// error in this tree: an unparseable `NITRO_MODE` must not stop a server
/// from starting.
#[must_use]
pub fn parse_modes(spec: &str) -> HashMap<String, ModeRequest> {
    let mut out = HashMap::new();
    for entry in spec.split(',').filter(|e| !e.trim().is_empty()) {
        let Some((name, value)) = entry.split_once('=') else {
            warn!("NITRO_MODE: {entry:?} is not name=mode");
            continue;
        };
        match ModeRequest::parse(value) {
            Ok(m) => {
                out.insert(name.trim().to_owned(), m);
            }
            Err(e) => warn!("NITRO_MODE: {value:?}: {e}"),
        }
    }
    out
}

/// Parse per-output modelines, `<name>=<clock> <hdisp> …`.
///
/// `NITRO_MODELINE=HDMI-A-1=249000 1280 1328 1360 1440 720 723 728 735
/// +hsync -vsync`. Separate from [`parse_modes`] because a modeline
/// contains spaces and could not share `NITRO_MODE`'s comma-separated
/// list without a quoting rule; one variable therefore holds **one**
/// connector's timings, which is what a one-off experiment wants anyway.
///
/// Merged on top of `NITRO_MODE`, so a variable naming the same connector
/// twice ends up with the modeline — the more specific of the two.
#[must_use]
pub fn parse_modelines(spec: &str) -> HashMap<String, ModeRequest> {
    let mut out = HashMap::new();
    let spec = spec.trim();
    if spec.is_empty() {
        return out;
    }
    let Some((name, value)) = spec.split_once('=') else {
        warn!("NITRO_MODELINE: {spec:?} is not name=<modeline>");
        return out;
    };
    match Modeline::parse(value) {
        Ok(m) => {
            out.insert(name.trim().to_owned(), ModeRequest::Custom(m));
        }
        Err(e) => warn!("NITRO_MODELINE: {value:?}: {e}"),
    }
    out
}

/// The modes every connector should run, after everything has had its say.
///
/// `NITRO_MODE`/`NITRO_MODELINE` beat `output.<c>.mode`, which beats the
/// connector's own preferred mode — the same precedence, and for the same
/// reasons, as [`resolve_scale`]. One function so startup, a reload and a
/// hotplug cannot drift apart: a replugged monitor has to come back at the
/// rate the user configured, not the one the EDID prefers.
#[must_use]
fn resolve_modes(
    overrides: &HashMap<String, ModeRequest>,
    settings: &config::Settings,
) -> HashMap<String, ModeRequest> {
    let mut out: HashMap<String, ModeRequest> = settings
        .outputs
        .iter()
        .filter_map(|(name, o)| o.mode.map(|m| (name.clone(), m)))
        .collect();
    for (name, m) in overrides {
        out.insert(name.clone(), *m);
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

/// The scale an output gets, after everything has had its say.
///
/// One function so the precedence cannot drift between startup, a reload
/// and a hotplug — all three go through [`Server::sync_outputs`], and all
/// three must agree or a replugged monitor comes back a different size
/// from the one the user configured.
///
/// `NITRO_SCALE` beats the file beats the EDID default, because the
/// environment is the *development* channel (`just fake` must not be
/// overridden by the box's own config) and the file is the user's explicit
/// answer to the EDID's guess. Argued in `crates/nitro-server/src/config.rs`.
#[must_use]
fn resolve_scale(
    info: &OutputInfo,
    overrides: &HashMap<String, f32>,
    settings: &config::Settings,
) -> f32 {
    if let Some(s) = overrides.get(&info.name) {
        return *s;
    }
    if let Some(s) = settings.output(&info.name).and_then(|o| o.scale) {
        return s;
    }
    default_scale(info)
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
/// The privileged shell socket's listener; see [`shell`].
const TOK_SHELL_LISTENER: u64 = 8;
/// The inotify fd watching the directory `server.conf` lives in, and the
/// SIGHUP self-pipe. Both mean exactly one thing — re-read the file — so
/// they sit next to each other and end in the same call.
const TOK_CONFIG: u64 = 9;
const TOK_SIGHUP: u64 = 10;
/// The **remote** listener, when `remote.listen` asked for one. Absent
/// from the epoll set entirely when it did not, which is why the feature
/// costs an ordinary desktop nothing (see [`remote`]).
const TOK_REMOTE_LISTENER: u64 = 11;
/// The key-repeat timerfd; see [`repeat`]. Armed only while a key is held.
const TOK_REPEAT: u64 = 12;
/// The injected-input timerfd; see [`inject`]. Armed only while a scripted
/// `input` sequence has events still to come.
const TOK_INJECT: u64 = 13;
/// The GPU helper's socket (#3922); see [`gpu`].
const TOK_GPU: u64 = 14;
/// The GPU helper's timerfd: restart backoff, `Hello` and hang deadlines.
const TOK_GPU_TIMER: u64 = 15;
/// How long an unanswered input keeps waiting for a frame to claim it.
/// Beyond this the number would not be a latency any more: nothing
/// responded to the event, and attributing the next unrelated frame to it
/// is how the histogram once reported 33 seconds.
const INPUT_STAMP_MAX_AGE_NS: u64 = 200_000_000;

/// How long the keyboard is held for a shell that has just been sent a
/// `HotKey`, before ordinary routing resumes.
///
/// The gap being covered is one client round trip — the write, the
/// shell's wakeup, the tree it builds and its commit — which measures at
/// a fraction of a millisecond on a warm idle launcher but is bounded by
/// scheduling, not by any server work. 50 ms is generous enough that a
/// shell under load still gets its turn, and short enough that a shell
/// which never answers costs the user at most one keystroke.
///
/// Wall clock, not the input event's `time_ns`: what is being bounded is
/// how long a client is given to answer, which is real elapsed time. An
/// input timestamp says when a key was *pressed*, and on a synthetic
/// source it need not advance with the world at all. Nothing waits on
/// this deadline either way — it is only read when the next key arrives,
/// so an idle desktop still takes zero wakeups.
const HOTKEY_ANSWER: Duration = Duration::from_millis(50);

const TOK_CLIENT_BASE: u64 = 1 << 32;
const TOK_WIRE_BASE: u64 = 1 << 33;
/// Shell clients get their own token range, so a token says which socket a
/// client arrived on even before its `WireClient` is looked up.
const TOK_SHELL_BASE: u64 = 1 << 34;
/// Remote clients get their own range too, for exactly the same reason:
/// the token *is* the answer to "did this client arrive over TCP?", so
/// `caps::REMOTE` and the buffer refusal are decided from one fact rather
/// than from a per-client flag that could drift from it.
const TOK_REMOTE_BASE: u64 = 1 << 35;
/// Pending acquire fences (#3918), one token each, above every client
/// range: `dmabuf::FenceSet` hands out `TOK_FENCE_BASE + key`.
const TOK_FENCE_BASE: u64 = 1 << 36;
/// Completion fences of GPU-helper frames (#3922), `base + serial`: the
/// buffers a frame samples are held against `BufferReleased` until it
/// signals. Above the acquire fences, so its match arm comes first.
const TOK_GPU_FENCE_BASE: u64 = 1 << 37;

/// The server's epoll as [`gpu::Poll`].
struct EpollPoll<'a>(&'a OwnedFd);

impl gpu::Poll for EpollPoll<'_> {
    fn add(&self, fd: std::os::fd::BorrowedFd<'_>, token: u64, out: bool) {
        let flags = if out {
            EventFlags::IN | EventFlags::OUT
        } else {
            EventFlags::IN
        };
        if let Err(e) = epoll::add(self.0, fd, EventData::new_u64(token), flags) {
            warn!("epoll add (gpu): {e}");
        }
    }

    fn modify(&self, fd: std::os::fd::BorrowedFd<'_>, token: u64, out: bool) {
        let flags = if out {
            EventFlags::IN | EventFlags::OUT
        } else {
            EventFlags::IN
        };
        if let Err(e) = epoll::modify(self.0, fd, EventData::new_u64(token), flags) {
            warn!("epoll modify (gpu): {e}");
        }
    }

    fn remove(&self, fd: std::os::fd::BorrowedFd<'_>) {
        let _ = epoll::delete(self.0, fd);
    }
}

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

/// An inotify watch on the directory `server.conf` lives in.
///
/// # Why the directory and not the file
///
/// A watch is on an *inode*, and the way a settings app writes a
/// configuration file safely is `write temp + rename` — which is what
/// `nitro-settings` does, and what `sed -i` does, and what every editor
/// with a crash-safe save does. That replaces the inode, so a watch on the
/// file follows the old one into oblivion and never fires again. Watching
/// the directory and filtering on the file's own name sees the rename, the
/// plain overwrite and the first creation of a file that did not exist.
///
/// # Why this costs an idle server nothing
///
/// An inotify fd with no queued event is simply not readable, so it never
/// wakes epoll. Registering it adds one fd to the set and zero wakeups to a
/// desktop nobody is configuring — the same bargain the defer timerfd and
/// the uevent socket make.
struct ConfigWatch {
    fd: OwnedFd,
    /// The file name to filter events on (`server.conf`), as bytes,
    /// because that is what inotify reports.
    file_name: std::ffi::OsString,
}

impl ConfigWatch {
    /// Watch the directory `path` sits in.
    ///
    /// # Errors
    /// The directory does not exist or inotify is unavailable. Not fatal:
    /// the caller warns and runs without a watch, exactly as it does for
    /// the input-hotplug uevent socket.
    fn open(path: &Path) -> Result<Self, rustix::io::Errno> {
        use rustix::fs::inotify;
        let dir = path.parent().unwrap_or(Path::new("."));
        let file_name = path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new(config::FILE_NAME))
            .to_owned();
        // Create the directory if it is not there, because otherwise the
        // watch cannot be placed and the very first write from a settings
        // app — the one that creates the file — is the one event that is
        // missed. That is not an edge case: it is the state **every fresh
        // installation is in**, and it is exactly how this was found, on a
        // box whose `~/.config/nitro` did not exist. `add_watch` needs an
        // existing inode, and there is no "watch this path when it appears"
        // short of walking up to the first extant ancestor and re-arming on
        // every intermediate `CREATE` — much more machinery than `mkdir -p`
        // of a directory the server already owns the name of.
        //
        // A failure here is not returned: it is almost always a read-only
        // or unwritable home, and the watch attempt below will fail with a
        // better message than the `mkdir` would give. The caller warns and
        // runs on, unwatched.
        if let Err(e) = std::fs::create_dir_all(dir) {
            debug!("creating {}: {e}", dir.display());
        }
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        // `CLOSE_WRITE` catches an in-place overwrite, `MOVED_TO` the
        // atomic rename, `CREATE` the first appearance of a file that was
        // not there when the server started, and `DELETE`/`MOVED_FROM`
        // its removal.
        //
        // Watching the removal is not obvious and was originally left
        // out, on the theory that a file that goes away should leave the
        // last configuration in force so a half-finished `mv` does not
        // flicker the desktop. That was wrong, and issue #558 is what it
        // cost: `rm ~/.config/nitro/server.conf` is the documented way to
        // get back to defaults, and without these flags a `theme.scheme`
        // or a `keyboard.layout` from a file that no longer exists stayed
        // in force until something else triggered a reload. A `mv`'s
        // intermediate state is answered by the reload path instead,
        // which reads whatever is on disk *now*: the `MOVED_TO` of the
        // replacement arrives in the same drain as the `MOVED_FROM` of
        // the original, so the pair costs one reload, not two.
        inotify::add_watch(
            &fd,
            dir,
            inotify::WatchFlags::CLOSE_WRITE
                | inotify::WatchFlags::MOVED_TO
                | inotify::WatchFlags::CREATE
                | inotify::WatchFlags::DELETE
                | inotify::WatchFlags::MOVED_FROM,
        )?;
        Ok(Self { fd, file_name })
    }

    /// Drain the queue and report whether *our* file was among the events.
    ///
    /// Every event is drained whatever it named: an undrained inotify fd
    /// stays readable, and a level-triggered epoll would then spin at
    /// 100 % on the first unrelated file written into the configuration
    /// directory.
    fn drain(&mut self) -> bool {
        use rustix::fs::inotify;
        use std::mem::MaybeUninit;
        use std::os::unix::ffi::OsStrExt as _;
        let mut buf = [MaybeUninit::uninit(); 4096];
        let mut reader = inotify::Reader::new(&self.fd, &mut buf);
        let mut ours = false;
        loop {
            match reader.next() {
                Ok(event) => {
                    ours |= event
                        .file_name()
                        .is_some_and(|n| n.to_bytes() == self.file_name.as_bytes());
                }
                // The end of the queue, which is where every healthy
                // drain finishes.
                Err(rustix::io::Errno::AGAIN) => return ours,
                // Anything else is a broken watch. Stopping the drain is
                // the only way not to spin on it, and it is worth saying
                // out loud: from here on the file is only reloaded when
                // something asks.
                Err(e) => {
                    warn!("reading the config inotify fd: {e}");
                    return ours;
                }
            }
        }
    }
}

impl AsFd for ConfigWatch {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
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

/// The popup grab's per-seat state, held together so the `Server` does not
/// grow a loose bool per rule.
#[derive(Debug, Default)]
struct PopupSeat {
    /// The outermost popup of the chain holding the **pointer grab**, if
    /// any: a press outside that chain dismisses it and is consumed.
    grab: Option<WindowKey>,
    /// A press was consumed by the grab, so its release must be too: a
    /// client must never see an unpaired `Released`.
    click_consumed: bool,
    /// Escape dismissed a grabbing chain, so its release is swallowed for
    /// the same reason.
    escape_consumed: bool,
}

/// The running server. Field order is drop order: clients first, then the
/// sockets, then input (whose device fds belong to the seat), then the
/// backend (which holds a dup of the DRM fd), then the seat's `Device`, and
/// the seat last.
// Independent flags about unrelated subsystems (the cursor's staleness
// among them), not a state machine a bool cluster is hiding; an enum would
// have to invent combinations nobody names.
#[allow(clippy::struct_excessive_bools)]
struct Server {
    wire_clients: HashMap<u64, WireClient>,
    clients: HashMap<u64, Client>,
    wire_listener: WireListener,
    /// The privileged listener. Held next to the wire one and dropped with
    /// it, so both socket files go at shutdown.
    shell_listener: WireListener,
    /// The **remote** listener, when `server.conf`'s `remote.listen` asked
    /// for one. `None` — the default — means no TCP socket exists at all.
    /// A reload may create it, replace it or drop it; see
    /// [`Server::apply_remote_listen`].
    remote_listener: Option<RemoteListener>,
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
    /// The symbolic icon set and its cache of rasterised coverage masks.
    icons: IconEngine,
    outputs: Vec<OutputState>,
    keyboard: Option<Keyboard>,
    /// The current keymap as a sealed memfd, for `Keymap` (M5-C). Rebuilt
    /// with the keymap; never cleared once set, so `caps::KEYMAP` does not
    /// retract mid-session (see `Server::caps`).
    keymap_fd: Option<keyboard::KeymapFd>,
    cursor: Cursor,
    pointer: Pointer,
    /// Window-management policy: MRU, focus, drags, placement.
    wm: WindowManager,
    /// The decoration nodes of each framed window.
    decorations: HashMap<WindowKey, FrameNodes>,
    /// The window whose frame edge is currently lit as a resize
    /// affordance, because the pointer is in its resize band.
    ///
    /// Cached rather than recomputed at paint time because it is a
    /// *change* that matters: the restyle has to put the previous window's
    /// border back, and nothing else remembers which that was. See
    /// [`Server::set_resize_hint`].
    resize_hint: Option<WindowKey>,
    /// The title-bar button the pointer is on, whose disc is painted.
    ///
    /// Cached for exactly [`Server::resize_hint`]'s reason: what matters
    /// is the *change*, and putting the previous button's disc back needs
    /// to know which it was. The window is part of the key because two
    /// frames' buttons are different buttons — sliding from one window's
    /// close to another's has to unlight the first.
    button_hover: Option<(WindowKey, Region)>,
    /// Which cursor shape the pointer is showing; `None` means **hidden** —
    /// a client asked for `CursorShape::None` and still holds pointer
    /// focus over its content.
    ///
    /// Cached for the same reason [`Server::resize_hint`] is, and with the
    /// same damage rule: a shape change damages the **old rect ∪ the new**
    /// (they differ, because the hotspots do) and nothing else — no scene
    /// node moved, so no scene damage and no restyle. Chosen from the same
    /// single `frame_hit` per motion that already drives the hint and the
    /// hover; see [`Server::set_cursor`] and [`Server::cursor_choice`].
    cursor_shown: Option<crate::cursor::Shape>,
    /// The shape a client asked for with `SetCursor` (M5-E), and the
    /// window it held pointer focus on when it asked. An inner `None` is
    /// "hide".
    ///
    /// One field, not a per-client map: only the client under the pointer
    /// can have a live request. It lasts one continuous period of pointer
    /// focus — cleared wherever `pointer.over` changes and when the owning
    /// client goes — and is read only through
    /// [`Server::requested_cursor`], which re-validates the window, so a
    /// stale entry can never be honoured.
    client_cursor: Option<(WindowKey, Option<crate::cursor::Shape>)>,
    /// Pointer focus changed without a motion to re-derive the cursor —
    /// a window closed or its client went under a still pointer, or a
    /// popup mapped or unmapped beneath it. `settle` consumes it, so a
    /// client that hid the cursor and then died cannot leave the pointer
    /// invisible until the next motion (#644). `move_pointer` clears it
    /// after its own choice, so the motion path pays no second hit test.
    cursor_stale: bool,
    /// Something changed what is under a *stationary* pointer this wakeup —
    /// a window (or popup) mapped, unmapped, closed, restacked, moved or
    /// resized, a grab ended — so `settle` re-runs enter/leave after the
    /// scene update. Without it a window mapped under the pointer never
    /// gets its first `PointerEnter` (so the wheel goes nowhere until the
    /// pointer moves, #3886), and a client whose window vanished from
    /// under it believes the pointer is still inside.
    ///
    /// Set at the specific paths rather than on every settle: the re-check
    /// is a z-order walk, and most wakeups (a frame commit that touched no
    /// window geometry) cannot change its answer.
    pointer_refresh: bool,
    /// The shaped title run of each framed window, so a retitle can release
    /// the old one.
    frame_titles: HashMap<WindowKey, nitro_text::TextKey>,
    /// Per-output scale overrides from `NITRO_SCALE`, by connector name.
    scale_overrides: HashMap<String, f32>,
    /// Per-output mode overrides from `NITRO_MODE` / `NITRO_MODELINE`, by
    /// connector name. Beats `output.<c>.mode`, like every other
    /// environment override here.
    mode_overrides: HashMap<String, ModeRequest>,
    /// Where each output's logical space starts in the desktop space, by
    /// scene id, in the order `sync_outputs` laid them out.
    ///
    /// A table rather than a computation: [`Server::desktop_origin`] is
    /// called from many hot paths (every hit test, every drag motion,
    /// every clamp) and the answer now depends on the configuration file,
    /// so re-deriving it per call would be both slower and a second place
    /// for the rule to live.
    origins: Vec<(SceneOutputId, Point)>,
    /// The parsed `server.conf`, re-read on every reload. Empty settings
    /// when there is no file, which is what makes every rule below read
    /// the same whether a file exists or not.
    settings: config::Settings,
    /// The desktop's colours, derived from `settings.theme`: the scheme
    /// it names with its per-role overrides on top.
    ///
    /// Held rather than re-derived per use because it is read on every
    /// decoration restyle and sent to every client that connects, and
    /// because the *identity* of the current palette is what decides
    /// whether a reload has anything to push.
    palette: nitro_core::Palette,
    /// Bumped on every palette change and carried in the `Theme`
    /// message, so a client can tell a re-send from a real change.
    theme_serial: u32,
    /// Where that file lives, and `None` when there is none.
    config_path: Option<PathBuf>,
    /// The inotify watch on its directory; `None` when there is no file or
    /// the watch could not be created (a warning, never fatal).
    config_watch: Option<ConfigWatch>,
    /// Completed configuration reloads, however triggered. `stats`.
    config_reloads: u64,
    /// Whether each output gets a heap shadow buffer to paint into
    /// (`NITRO_SHADOW`). Read when an output is added; see
    /// [`frame::Shadow`].
    shadow: bool,
    /// Whether a scroll hint may be served from the shadow
    /// (`NITRO_SCROLL_BLIT`); see [`Config::scroll_blit`].
    scroll_blit: bool,
    /// The environment's override of `overview.animate`; see
    /// [`Config::overview_atlas`] and [`Server::wants_atlas`].
    overview_atlas: Option<bool>,
    /// Thumbnails rendered into an atlas, cumulative, and the
    /// microseconds they took. `stats`.
    thumb_renders: u64,
    thumb_render_us: u64,
    /// Frames that took the scroll blit, for `stats`.
    blit_frames: u64,
    /// Watches `/sys` for input devices appearing and disappearing.
    input_hotplug: Option<nitro_kms::uevent::UeventSocket>,
    /// Where `event*` devices live, for the hotplug rescan.
    input_dir: Option<PathBuf>,
    /// The window with keyboard focus, if any.
    focus: Option<WindowKey>,
    /// A window placed this wakeup that should take focus once its
    /// client is back in `wire_clients`; see `place_new_window`.
    pending_focus: Option<WindowKey>,
    /// Scratch for `send_buffer_releases`, kept to avoid an allocation per
    /// settle.
    released: Vec<(ClientId, BufferKey)>,
    /// `PresentSurface` frames waiting for their output's next paint
    /// opportunity (#3897); see [`surface::Latch`].
    latch: surface::Latch,
    /// The `SurfaceHint` last sent per Surface node (#3897).
    surface_hints: surface::Hints,
    /// Exported Surface nodes and their imports (#3904).
    shares: share::Shares,
    /// Acquire fences waiting to signal (#3918).
    fences: dmabuf::FenceSet,
    /// Which `DmabufFeedback` each tracked Surface node was last sent
    /// (#3918).
    feedback: dmabuf::FeedbackTracker,
    /// Client dma-bufs the KMS import refused (#3918), cumulative.
    dmabuf_kms_refused: u64,
    /// KMS framebuffers a committed plane layout has shown and the
    /// backend has not yet reported released (#3899): a client buffer
    /// among them is still read by the display.
    on_kms: HashSet<nitro_kms::BufferId>,
    /// `BufferReleased`s held back until the display lets go of the
    /// framebuffer: `(framebuffer, owner, buffer)`.
    held_releases: Vec<(nitro_kms::BufferId, ClientId, BufferKey)>,
    /// Plane-only commits (`Backend::commit_planes`), cumulative.
    plane_flips: u64,
    /// The GPU helper: composite mode 2 (#3922).
    gpu: gpu::Helper,
    /// `NITRO_GPU`, which beats `gpu.helper` on every reload.
    gpu_env: Option<config::GpuHelper>,
    /// The mode-2 output's shadow was reallocated: import it again.
    gpu_reshadow: bool,
    /// Acquire fences that had to be waited for, cumulative.
    fence_waits: u64,
    /// Frames latched early onto a plane, their fence still pending
    /// (#3938), cumulative.
    plane_fence_latches: u64,
    /// Fences handed to the display as `IN_FENCE_FD`, cumulative.
    plane_fences: u64,
    /// CPU-readable dma-bufs latched early (#3938): their read bracket
    /// was never begun (`DMA_BUF_IOCTL_SYNC` would block on the fence),
    /// so it is not ended either.
    unbracketed: HashSet<BufferKey>,
    /// Implicit fences taken by polling the dma-buf itself because
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` is missing, cumulative.
    implicit_fence_fallbacks: u64,
    /// Which window each live touch point started on, and where it is in
    /// that window's coordinates.
    touch_targets: HashMap<i32, (WindowKey, Point)>,
    /// Windows created while no output existed, waiting for one.
    ///
    /// **Never a popup.** This list drains into `place_new_window`, which
    /// decorates and centre-cascades — wrong for a menu in every
    /// particular. A popup whose parent has nowhere to be is dismissed at
    /// once instead; see [`Server::map_popup`].
    unplaced: Vec<(ClientId, WindowKey)>,
    /// Every popup the server has mapped, live or dismissed-but-not-yet-
    /// destroyed, with the positioner it asked for. See [`popup`].
    ///
    /// The server's own index, not the scene's parent links, because
    /// `Scene::destroy_window` runs inside `clients::apply` — before
    /// `forget_closed` sees the closed window — so by then the scene's
    /// edge from a dead parent to its popups is already gone.
    popups: HashMap<WindowKey, popup::PopupInfo>,
    /// The pointer grab and the input it swallowed; see [`PopupSeat`].
    popup_seat: PopupSeat,
    /// Popups unmapped and owing their client a `PopupDone`, deepest
    /// first.
    ///
    /// Deferred for `pending_focus`'s reason and not a line further: a
    /// dismissal can run with the owning client lifted out of
    /// `wire_clients` by `Server::commit` (a parent destroyed by its own
    /// `DestroyNode`), where `send_to_window` would find nothing and drop
    /// the message. **Dismissal only ever enqueues; `settle` is the only
    /// sender.**
    pending_popup_done: Vec<WindowKey>,
    /// Scratch for the popup chain walks, reused so a title-bar drag with a
    /// menu open allocates nothing per motion event (`docs/budget.md`).
    popup_scratch: Vec<WindowKey>,
    /// Newest input timestamp not yet consumed by a frame; see
    /// [`Server::note_input`].
    pending_input_ns: u64,
    /// The clients whose answer a cursor-only flip is waiting for, and the
    /// timer that bounds the wait. See [`defer`].
    defer: DeferredFlip,
    /// The held key being auto-repeated, and its timer. See [`repeat`].
    key_repeat: repeat::KeyRepeat,
    /// Control-socket input still to come, and its timer. See [`inject`].
    injector: inject::Injector,

    /// Exclusive zones and anchors set by shell clients; see [`shell`].
    zones: shell::Zones,
    /// Server-global hotkey bindings.
    hotkeys: shell::HotKeys,
    /// Server-global window ids, minted for the shell's window list.
    window_refs: shell::WindowRefs,
    /// The clipboard: owner, offer and parked requests; see [`data`].
    data: data::Selections,
    /// The drag-and-drop gesture in flight, from `StartDrag` to the
    /// source's `FinishDrag` (M5-I); see [`data::Dnd`]. While it is
    /// [`grabbing`](data::Dnd::grabbing) it owns the pointer: every
    /// motion drives it and every button is consumed.
    dnd: Option<data::Dnd>,
    /// `StartDrag`s from this wakeup's commits, by token, waiting for
    /// `settle`: a commit holds its client out of `wire_clients`, and a
    /// drag start sends `PointerLeave` and `DragEnter` — possibly to that
    /// very client (the `pending_focus` trap).
    pending_drag_starts: Vec<(u64, clients::DragStart)>,
    /// `DragFinished`s for a source that was lifted out of `wire_clients`
    /// when the drag ended (a commit destroying the window dropped on):
    /// token, accepted, action. `settle` sends them.
    pending_drag_finished: Vec<(u64, bool, nitro_wire::types::DragAction)>,
    /// Windows adopted as drag icons. Not application windows any more:
    /// out of the window list, the MRU and every hit test, and never
    /// migrated as orphans (the drag re-places its icon itself).
    drag_icons: HashSet<WindowKey>,
    /// `SetDragIconOffset`s: an icon window's top-left relative to the
    /// pointer hotspot, logical pixels. Sticks to the window until changed
    /// or the window dies, so a reused icon keeps it; an icon without one
    /// is centred (`place_drag_icon`).
    drag_icon_offsets: HashMap<WindowKey, Point>,
    /// A drag ended with a button still held (Escape, a lock, the source
    /// gone): swallow every button event until all are up, so no client
    /// sees a `Released` whose press it never saw.
    dnd_swallow: bool,
    /// A button whose press overview mode consumed (a thumbnail
    /// selection): its release is dropped too, even though the overview
    /// has ended by then, so the window just selected never sees a
    /// `Released` whose press it never saw.
    overview_swallow: Option<u32>,
    /// Escape cancelled a drag; its release is swallowed.
    dnd_escape_consumed: bool,
    /// Drops delivered to an accepting target, cumulative. `stats`.
    dnd_drops: u64,
    /// Drags that ended rejected or cancelled, cumulative. `stats`.
    dnd_cancels: u64,
    /// `SendSelection` descriptors relayed to a requester, cumulative.
    /// Reported as `selection_transfers`.
    selection_transfers: u64,
    /// Requests answered with an EOF descriptor by the server itself,
    /// cumulative. Reported as `selection_eof`.
    selection_eof: u64,
    /// Shell clients subscribed to the window list, by token.
    window_watchers: Vec<u64>,
    /// Shell clients subscribed to output hotplug, by token.
    output_watchers: Vec<u64>,
    /// Shell clients subscribed to overview state, by token: every
    /// connection that has sent a `SetOverview`.
    overview_watchers: Vec<u64>,
    /// The overview state the watchers were last told: the output in
    /// overview, or `None`. `announce_overview` compares against it, which
    /// is what makes a relayout (leave + enter on one output) say nothing.
    overview_announced: Option<SceneOutputId>,
    /// `SetOverview`s received this wakeup, by token, waiting for
    /// `settle`: entering dismisses popups and sends to other clients,
    /// the `pending_drag_starts` trap.
    pending_overview: Vec<(u64, nitro_wire::types::OverviewRequest)>,
    /// `SetOverview`s applied, cumulative. `stats`.
    overview_requests: u64,
    /// The window holding an explicit keyboard grab: every key goes there
    /// instead of to the focused window. See
    /// [`GrabKeyboard`](nitro_wire::msg::GrabKeyboard).
    grab: Option<WindowKey>,
    /// The session lock: who owns it, if anyone. Applied through the
    /// scene's [`Admit`](nitro_scene::Admit) filter (`sync_admit`), which
    /// every input path then asks. See [`lock`].
    lock: lock::Lock,
    /// The window that had focus when the session was locked, given it back
    /// at the unlock if it still exists.
    focus_before_lock: Option<WindowKey>,
    /// A shell whose hotkey has just fired and whose answer we are
    /// holding the keyboard for: its epoll token, and the deadline past
    /// which we stop waiting. See [`Server::key`].
    hotkey_pending: Option<(u64, Instant)>,
    /// Keys dropped by that wait, cumulative. Reported as `keys_withheld`.
    keys_withheld: u64,

    next_client: u64,
    next_wire: u64,
    /// Shell tokens are allocated from their own counter, so a shell client
    /// and a wire client can never share a token.
    next_shell: u64,
    /// Remote tokens, from their own counter for the same reason.
    next_remote: u64,
    next_client_id: u32,
    active: bool,
    quit: bool,
    frames: u64,
    started: Instant,
    /// Milliseconds from `run()` to the first commit the backend accepted:
    /// how long the panel showed someone else's picture (fbcon's, or the
    /// previous compositor's) before ours. `None` until then.
    first_frame_ms: Option<u64>,
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
    // Taken first, so `uptime_ms` and `first_frame_ms` count the seat, the
    // card and the font setup rather than starting after them.
    let started = Instant::now();
    let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(errno("epoll_create"))?;
    let mut signals = if config.handle_signals {
        let s = signals::Signals::install().map_err(io_err("install signal handlers"))?;
        add(&epoll, &s.quit_fd(), TOK_SIGNALS)?;
        add(&epoll, &s.reload_fd(), TOK_SIGHUP)?;
        Some(s)
    } else {
        None
    };

    // Declared before `device`/`backend` so an early `?` drops it last.
    let mut seat: Option<Rc<RefCell<Seat>>> = None;
    let mut device: Option<Device> = None;
    // The configuration file is read **before** the backend, because
    // `output.<c>.mode` is an input to the very first modeset: reading it
    // afterwards would light the panel at its preferred rate and retime it
    // a moment later, which is a visible blank at every boot for no
    // reason. Its `keyboard.*` section is likewise an input to the keymap
    // compiled further down.
    let settings = match config.config_path.as_deref() {
        Some(path) => {
            let s = config::load(path);
            info!("configuration from {}", path.display());
            for w in &s.warnings {
                warn!("{}: {w}", path.display());
            }
            s
        }
        None => config::Settings::default(),
    };
    let modes = resolve_modes(&config.modes, &settings);
    let mut backend: Box<dyn Backend> = match &config.backend {
        BackendKind::Fake { width, height } => {
            info!("fake backend {width}x{height}");
            let spec = nitro_kms::FakeOutputSpec::new(*width, *height);
            let spec = if config.fake_modes.is_empty() {
                spec
            } else {
                spec.modes(&config.fake_modes)
            };
            let spec = if config.fake_planes.is_empty() {
                spec
            } else {
                spec.planes(config.fake_planes.clone())
            };
            Box::new(FakeBackend::new(&[spec]).map_err(io_err("create fake backend"))?)
        }
        BackendKind::FakeHeadless => {
            info!("fake backend with no outputs");
            Box::new(FakeBackend::new(&[]).map_err(io_err("create fake backend"))?)
        }
        BackendKind::Drm { card } => {
            let mut s = Seat::open()?;
            info!("seat {:?} opened, active={}", s.name(), s.is_active());
            add(&epoll, &s, TOK_SEAT)?;
            if !wait_active(&epoll, &mut s, signals.as_mut())? {
                info!("interrupted while waiting for the seat; exiting");
                return Ok(());
            }
            let (dev, be) = open_card(&mut s, card.as_deref(), &modes)?;
            seat = Some(Rc::new(RefCell::new(s)));
            device = Some(dev);
            be
        }
    };
    // The fake backend learns its modes here rather than at construction,
    // because `FakeBackend::single` is the shape every test already calls.
    // On the DRM backend the map went in through `DrmOptions` and this is
    // a no-op comparison against what is already in force.
    if let Err(e) = backend.set_modes(&modes) {
        warn!("applying the configured modes: {e}");
    }
    for w in backend.take_warnings() {
        warn!("{w}");
    }

    let mut input: Box<dyn InputSource> = match (&seat, config.input_dir.as_deref()) {
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
    // `pointer.speed` / `pointer.accel` before the first event: libinput
    // applies them to each device as its `DeviceAdded` is dispatched.
    input.configure_pointer(&settings.pointer);

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

    // The keyboard's section came out of the file read before the
    // backend; the watch is opened even when the file itself is absent —
    // the directory is what is watched, so a `server.conf` created after
    // the server started is picked up. A missing *directory* is the
    // ordinary state of a fresh install and is not worth more than a debug
    // line.
    let config_watch = match config.config_path.as_deref() {
        Some(path) => match ConfigWatch::open(path) {
            Ok(w) => {
                add(&epoll, &w, TOK_CONFIG)?;
                info!("watching {} for changes", path.display());
                Some(w)
            }
            Err(e) => {
                warn!("not watching {}: {e}", path.display());
                None
            }
        },
        None => None,
    };

    let keyboard = Keyboard::with_settings(&settings.keyboard);
    match &keyboard {
        Some(kb) => info!("xkb keymap: {}", kb.layout_names().join(", ")),
        None => warn!("no xkb keymap compiled; keys carry no keysym or text"),
    }
    let keymap_fd = keyboard.as_ref().and_then(|kb| {
        let fd = kb.export();
        if fd.is_none() {
            warn!("xkb keymap export failed; caps::KEYMAP not advertised");
        }
        fd
    });

    let listener = control::bind(&config.control_path).map_err(io_err("bind control socket"))?;
    add(&epoll, &listener, TOK_LISTENER)?;
    info!("control socket at {}", config.control_path.display());

    let wire_listener = WireListener::bind(&config.wire_path).map_err(|e| Error::Io {
        op: "bind wire socket",
        source: io::Error::other(e.to_string()),
    })?;
    add(&epoll, &wire_listener.as_fd(), TOK_WIRE_LISTENER)?;
    info!("wire socket at {}", config.wire_path.display());

    // The second socket. Same framing, same handshake; the difference is
    // that a `Welcome` sent here carries `caps::SHELL`. Both sockets live in
    // the same `0700` directory, so "can open it" means "is this user", which
    // is the whole of the privilege model in M3 (`docs/shell.md`).
    let shell_listener = WireListener::bind(&config.shell_path).map_err(|e| Error::Io {
        op: "bind shell socket",
        source: io::Error::other(e.to_string()),
    })?;
    add(&epoll, &shell_listener.as_fd(), TOK_SHELL_LISTENER)?;
    info!("shell socket at {}", config.shell_path.display());

    let gpu_helper = gpu::Helper::new(
        config.gpu.unwrap_or_else(|| settings.gpu.helper()),
        settings.gpu.idle_exit(),
        config.gpu_helper.clone(),
        config.gpu_spawner.clone(),
    )
    .map_err(errno("create the gpu helper timer"))?;
    let mut server = Server {
        gpu: gpu_helper,
        gpu_env: config.gpu,
        gpu_reshadow: false,
        wire_clients: HashMap::new(),
        clients: HashMap::new(),
        wire_listener,
        shell_listener,
        remote_listener: None,
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
        icons: {
            let mut icons = match config.icon_dirs.take() {
                Some(dirs) => IconEngine::with_dirs(dirs, settings.theme.icon_theme()),
                None => IconEngine::with_theme(settings.theme.icon_theme()),
            };
            if let Some(dirs) = config.desktop_dirs.take() {
                icons.set_desktop_dirs(dirs);
            }
            icons
        },
        outputs: Vec::new(),
        keyboard,
        keymap_fd,
        cursor: Cursor::new(),
        pointer: Pointer::default(),
        wm: WindowManager::new(),
        decorations: HashMap::new(),
        resize_hint: None,
        button_hover: None,
        cursor_shown: Some(crate::cursor::Shape::Arrow),
        client_cursor: None,
        cursor_stale: false,
        pointer_refresh: false,
        frame_titles: HashMap::new(),
        scale_overrides: std::mem::take(&mut config.scales),
        mode_overrides: std::mem::take(&mut config.modes),
        origins: Vec::new(),
        palette: settings.palette(),
        theme_serial: 1,
        settings,
        config_path: config.config_path.clone(),
        config_watch,
        config_reloads: 0,
        shadow: config.shadow,
        scroll_blit: config.scroll_blit,
        overview_atlas: config.overview_atlas,
        thumb_renders: 0,
        thumb_render_us: 0,
        blit_frames: 0,
        lock: if config.locked {
            lock::Lock::locked()
        } else {
            lock::Lock::Unlocked
        },
        focus_before_lock: None,
        input_hotplug,
        input_dir: config.input_dir.clone(),
        focus: None,
        pending_focus: None,
        released: Vec::new(),
        latch: surface::Latch::default(),
        surface_hints: surface::Hints::default(),
        shares: share::Shares::default(),
        fences: dmabuf::FenceSet::new(TOK_FENCE_BASE),
        feedback: dmabuf::FeedbackTracker::default(),
        dmabuf_kms_refused: 0,
        on_kms: HashSet::new(),
        held_releases: Vec::new(),
        plane_flips: 0,
        fence_waits: 0,
        plane_fence_latches: 0,
        plane_fences: 0,
        unbracketed: HashSet::new(),
        implicit_fence_fallbacks: 0,
        touch_targets: HashMap::new(),
        unplaced: Vec::new(),
        popups: HashMap::new(),
        popup_seat: PopupSeat::default(),
        pending_popup_done: Vec::new(),
        popup_scratch: Vec::new(),
        pending_input_ns: 0,
        defer: defer::DeferredFlip::new().map_err(errno("create the deferred-flip timer"))?,
        key_repeat: repeat::KeyRepeat::new().map_err(errno("create the key-repeat timer"))?,
        injector: inject::Injector::new().map_err(errno("create the input-injection timer"))?,
        zones: shell::Zones::new(),
        hotkeys: shell::HotKeys::new(),
        window_refs: shell::WindowRefs::new(),
        data: data::Selections::new(),
        selection_transfers: 0,
        selection_eof: 0,
        dnd: None,
        pending_drag_starts: Vec::new(),
        pending_drag_finished: Vec::new(),
        drag_icons: HashSet::new(),
        drag_icon_offsets: HashMap::new(),
        dnd_swallow: false,
        overview_swallow: None,
        dnd_escape_consumed: false,
        dnd_drops: 0,
        dnd_cancels: 0,
        window_watchers: Vec::new(),
        output_watchers: Vec::new(),
        overview_watchers: Vec::new(),
        overview_announced: None,
        pending_overview: Vec::new(),
        overview_requests: 0,
        grab: None,
        hotkey_pending: None,
        keys_withheld: 0,
        next_client: 0,
        next_wire: 0,
        next_shell: 0,
        next_remote: 0,
        next_client_id: 1,
        active: true,
        quit: false,
        frames: 0,
        started,
        first_frame_ms: None,
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
    // Likewise the repeat timer: armed only while a key is held down.
    add(&server.epoll, &server.key_repeat.as_fd(), TOK_REPEAT)?;
    // And the injection timer: armed only while an `input` sequence runs.
    add(&server.epoll, &server.injector.as_fd(), TOK_INJECT)?;
    // And the GPU helper's timer: armed only for a restart, a `Hello` or
    // a frame outstanding.
    add(&server.epoll, &server.gpu.timer_fd(), TOK_GPU_TIMER)?;
    if server.gpu.mode == config::GpuHelper::On {
        server.gpu_spawn();
    }
    // The third listener, and the only one that is optional. Applied here
    // through the same function the reload path uses, so "what
    // `remote.listen` means" has exactly one implementation.
    server.apply_remote_listen();
    // Before the first frame: a server started locked must never have
    // painted a window, so the filter is in force before anything is.
    server.sync_admit();
    if server.lock.is_locked() {
        info!("starting locked: nothing is drawn until a shell client sends Lock");
    }
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

/// Whether a message is one of the shell ops, i.e. needs `caps::SHELL`.
///
/// A `match` over the shell variants rather than an op-code range test: a
/// range would keep compiling after someone put a new op in the 0x4xx block,
/// which is exactly when a privilege check must not keep compiling.
fn is_shell_op(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::SetLayer(_)
            | ClientMsg::SetExclusiveZone(_)
            | ClientMsg::SetAnchor(_)
            | ClientMsg::BindKey(_)
            | ClientMsg::UnbindKey(_)
            | ClientMsg::GrabKeyboard(_)
            | ClientMsg::WindowList(_)
            | ClientMsg::FocusWindow(_)
            | ClientMsg::CloseWindow(_)
            | ClientMsg::SetWindowStateFor(_)
            | ClientMsg::Outputs(_)
            | ClientMsg::Lock(_)
            | ClientMsg::Unlock(_)
            | ClientMsg::SetOverview(_)
    )
}

/// Whether a message is one of the popup ops, i.e. needs the client to
/// have listed `caps::POPUP` in its `ClientCaps`.
fn is_popup_op(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::CreatePopup(_) | ClientMsg::RepositionPopup(_)
    )
}

fn add(epoll: &OwnedFd, fd: &impl AsFd, token: u64) -> Result<(), Error> {
    epoll::add(epoll, fd, EventData::new_u64(token), EventFlags::IN).map_err(errno("epoll_ctl add"))
}

/// Block until the seat reports active. Returns `false` if a signal
/// arrived first (only possible when handlers are installed).
///
/// libseat queues the initial `Enable` inside `open_seat` without making
/// the fd readable, so dispatch once before waiting on epoll.
///
/// # Why SIGHUP is drained here rather than ignored
///
/// The reload fd is in the epoll set before this function runs, and it is
/// **level-triggered**: an unread datagram keeps it readable forever. So a
/// SIGHUP delivered while the server is waiting for an inactive VT — which
/// is precisely the situation this function exists for — would make every
/// `wait` return immediately, spinning at 100 % CPU and logging a line per
/// iteration until the VT happened to become active. Draining it is what
/// makes the wait a wait again; it is the same rule `ConfigWatch::drain`
/// states for inotify, applied to the one fd that reaches this loop.
///
/// A reload asked for now is *answered by doing nothing*, deliberately.
/// There is no output, no window and no keymap to re-apply yet, and the
/// configuration is read in full a few lines further on — so the reload the
/// caller wanted happens anyway, and happens later than the signal.
///
/// # Not reachable on the test box, which is worth knowing before you try
///
/// The spin above is real by inspection and is pinned by
/// `signals::tests::a_drained_reload_fd_stops_being_readable`, but an
/// attempt to reproduce it on the M4-C test box failed four times in a
/// row: **libseat queues the initial `Enable` at open even for a session
/// logind reports `Active=no`** — under the logind backend and under
/// `LIBSEAT_BACKEND=seatd` alike — so `seat.is_active()` is already true
/// and this loop's body never iterates there. A hardware repro needs a
/// seat that really does stay inactive (a VT switch away *after* open, or
/// a backend that does not auto-enable); on that box the honest status is
/// "fixed and unit-tested, hardware path does not occur".
fn wait_active(
    epoll: &OwnedFd,
    seat: &mut Seat,
    mut signals: Option<&mut signals::Signals>,
) -> Result<bool, Error> {
    let mut buf = event_buffer::<8>();
    drain_seat(seat)?;
    while !seat.is_active() {
        info!("waiting for the seat to become active");
        let n = wait(epoll, &mut buf)?;
        for ev in &buf[..n] {
            match ev.data.u64() {
                TOK_SIGNALS if signals.is_some() => return Ok(false),
                TOK_SEAT => drain_seat(seat)?,
                TOK_SIGHUP => {
                    if let Some(s) = signals.as_mut()
                        && s.drain_reload()
                    {
                        info!(
                            "SIGHUP before the seat is active; the config is read at startup anyway"
                        );
                    }
                }
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
fn open_card(
    seat: &mut Seat,
    card: Option<&Path>,
    modes: &HashMap<String, ModeRequest>,
) -> Result<(Device, Box<dyn Backend>), Error> {
    let mut fallback: Option<PathBuf> = None;
    let mut last_err = String::from("no /dev/dri/card* found");
    for path in card_candidates(card) {
        match try_open(seat, &path, modes) {
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
        return try_open(seat, &path, modes);
    }
    Err(Error::NoDevice(last_err))
}

fn try_open(
    seat: &mut Seat,
    path: &Path,
    modes: &HashMap<String, ModeRequest>,
) -> Result<(Device, Box<dyn Backend>), Error> {
    let dev = seat.open_device(path)?;
    // The backend gets its own fd (a dup sharing the open file
    // description, hence DRM master) so it can be `'static`; the seat's
    // fd is closed through `close_device` after the backend is gone.
    let fd = dev
        .as_fd()
        .try_clone_to_owned()
        .map_err(io_err("dup DRM fd"))?;
    let opts = DrmOptions {
        hotplug: true,
        modes: modes.clone(),
    };
    match DrmBackend::open(fd, &opts) {
        Ok(mut be) => {
            if let Some(e) = be.hotplug_error() {
                warn!("hotplug disabled: {e}");
            }
            // What the mode configuration had to say about this card.
            // Emitted here rather than swallowed, because "the line you
            // wrote matched nothing" is the one thing a user needs to
            // hear: the desktop comes up either way and the only visible
            // symptom is a rate that did not change.
            for w in be.take_warnings() {
                warn!("{}: {w}", path.display());
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
    /// with the backend's.
    ///
    /// The layout is **left to right in connector order**, except where
    /// `server.conf` says otherwise: a connector with an explicit
    /// `output.<c>.position` is put there, and one without is placed after
    /// whatever came before it, which is the row the server laid out
    /// before the file existed. A row is the arrangement that needs no
    /// policy, and connector order is the only ordering the kernel gives
    /// us. The scale is [`resolve_scale`]: `NITRO_SCALE`, then the file,
    /// then the EDID.
    ///
    /// # Device rects and desktop origins are kept in step
    ///
    /// Every output has two origins: a **device** rect (in scanout pixels,
    /// which the pointer is clamped to and [`input::output_at`] hit-tests)
    /// and a **desktop** origin (in logical units, which every window
    /// rectangle in the window manager is relative to). The configured
    /// position is a *logical* one — that is the space a user thinks in,
    /// and the one the file documents — so it is multiplied back by the
    /// scale to produce the device rect.
    ///
    /// Doing only half of that would be worse than doing neither: with the
    /// desktop layout following the file and the device layout still in
    /// connector order, the pointer would cross from one screen to the
    /// next at a different place from where a dragged window does, and
    /// `output_at` would disagree with `output_for` about which output a
    /// point is on. So both spaces are computed here, in one pass, and
    /// [`Server::desktop_origin`] does nothing but read the table this
    /// leaves behind.
    ///
    /// # Where that correspondence stops being exact
    ///
    /// The device rect is `position × that output's own scale`, so the two
    /// layouts are a faithful image of each other **when the outputs share
    /// a scale** — which is every case the tests cover and every case a
    /// single-monitor or uniform-DPI desk is in. Mix scales *and* give
    /// explicit positions and they can come apart: a 1920-wide output at
    /// scale 2 is 960 logical units across, so a neighbour configured at
    /// `position = 960,0` with scale 1 lands its device rect at x = 960 —
    /// inside the first output's 0..1920 device span. Device space is
    /// global, so that is a real overlap, not a cosmetic gap: a pointer in
    /// the shared strip hit-tests to whichever output `output_at` finds
    /// first.
    ///
    /// It is recorded rather than fixed because there is no honest fix at
    /// this layer — a device layout that packs scaled outputs without gaps
    /// or overlaps is a different allocation pass (it has to *choose*
    /// device positions rather than derive them), and that belongs with
    /// drag-arrange, where the user can see what they are arranging. Until
    /// then the file's position is taken at face value, which is what a
    /// user typing coordinates expects. `docs/wm.md` says the same to a
    /// reader who is not in the source.
    ///
    /// Removing an output orphans its windows — the scene unplaces them —
    /// so they are migrated onto the primary output afterwards rather than
    /// left invisible with no way back.
    /// Re-apply `output.<c>.mode` after a reload.
    ///
    /// **A mode set is a full modeset**, not a property flip like scale or
    /// position: the CRTC is retimed and the panel blanks for the
    /// duration. So this is done only when the resolved map actually
    /// changed — which the backend enforces by comparing before touching
    /// anything — and a reload that moved a colour costs nothing.
    ///
    /// A refresh change **at the same size** (1080p60 → 1080p120) is done
    /// live and cheaply: the backend retimes the output in place, so the
    /// [`OutputId`](nitro_kms::OutputId) survives and the `sync_outputs`
    /// below finds the same output with a new `refresh_ns` — no scene
    /// removal, no window migration, no `OutputGone` on any socket, and
    /// no buffer reallocated. That is a property of
    /// `select::reconcile_one`, not an accident: without it a rate change
    /// would destroy the output and build a new one under a fresh id,
    /// which arrives here as an unplug followed by a plug.
    ///
    /// A **size** change is the expensive case and takes exactly that
    /// path, correctly: the output is replaced, its windows are migrated,
    /// the shadow is resized and the desktop repaints in full — the same
    /// sequence a hotplug already runs.
    fn apply_modes(&mut self) {
        let modes = resolve_modes(&self.mode_overrides, &self.settings);
        match self.backend.set_modes(&modes) {
            Ok(true) => {
                info!("output modes re-applied");
                self.planes_reset(None);
            }
            Ok(false) => {}
            Err(e) => warn!("applying the configured modes: {e}"),
        }
        for w in self.backend.take_warnings() {
            warn!("{w}");
        }
    }

    fn sync_outputs(&mut self) {
        let infos: Vec<OutputInfo> = self.backend.outputs().to_vec();
        // An overview on an output about to go is over: its thumbnails
        // are put back before they become orphans to migrate.
        if let Some(out) = self.overview_output()
            && !infos.iter().any(|i| SceneOutputId(i.id.0) == out)
        {
            self.leave_overview(None);
        }
        // The mode-2 output going away takes its ring with it (#3922).
        if let Some(owner) = self.gpu.owner
            && !infos.iter().any(|i| i.id == owner)
        {
            self.gpu_drop_owner(false);
        }
        let mut lost = false;
        let mut gone: Vec<u32> = Vec::new();
        self.outputs.retain_mut(|o| {
            let keep = infos.iter().any(|i| i.id == o.kms_id);
            if !keep {
                info!("{} gone", o.kms_id);
                if let Some(atlas) = o.atlas.take() {
                    atlas.free(&mut self.scene);
                }
                self.scene.remove_output(o.scene_id);
                gone.push(o.scene_id.0);
                lost = true;
            }
            keep
        });
        let rescaled = self.layout_outputs(&infos);
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
        // A bar has to keep spanning the edge it anchored to after a mode
        // change, a scale change or a hotplug, and the desktop width it
        // spans just changed. Re-applied unconditionally rather than only
        // when something differs: `set_frame_rect` is idempotent and an
        // anchor that silently stopped holding is the harder bug.
        self.reflow_anchors();
        self.reconfigure_rescaled(&rescaled);
        self.notify_outputs(&gone);
        // Output set or modes changed: the dma-buf feedback may have too
        // (#3918). Per-node feedback follows from `send_surface_hints`.
        self.send_default_feedback(None);
        self.feedback.invalidate();
    }

    /// Give every connected output its scale, its device rect and its
    /// desktop origin, creating the [`OutputState`] of any that is new.
    /// Returns the outputs whose scale *changed*, which is what
    /// [`Server::reconfigure_rescaled`] needs.
    ///
    /// The layout rule, and the reason the two spaces are computed
    /// together, are in [`Server::sync_outputs`]'s doc comment; this is
    /// only the loop.
    fn layout_outputs(&mut self, infos: &[OutputInfo]) -> Vec<SceneOutputId> {
        // The two cursors: where the next unpositioned output starts, in
        // each space. A *positioned* output moves them too, so "after the
        // last one" means after whatever was actually placed last.
        let mut device_x = 0;
        let mut desktop_x = 0.0f32;
        let mut origins: Vec<(SceneOutputId, Point)> = Vec::with_capacity(infos.len());
        let mut rescaled: Vec<SceneOutputId> = Vec::new();
        for info in infos {
            let scene_id = SceneOutputId(info.id.0);
            let scale = resolve_scale(info, &self.scale_overrides, &self.settings);
            let (w, h) = (info.width.cast_signed(), info.height.cast_signed());
            let (rect, origin) = match self.settings.output(&info.name).and_then(|o| o.position) {
                Some((px, py)) => {
                    let origin = Point::new(px as f32, py as f32);
                    let device = nitro_core::IRect::new(
                        (origin.x * scale).round() as i32,
                        (origin.y * scale).round() as i32,
                        w,
                        h,
                    );
                    (device, origin)
                }
                None => (
                    nitro_core::IRect::new(device_x, 0, w, h),
                    Point::new(desktop_x, 0.0),
                ),
            };
            device_x = rect.x + w;
            desktop_x = origin.x + info.width as f32 / if scale > 0.0 { scale } else { 1.0 };
            origins.push((scene_id, origin));
            let was = self.scene.output_info(scene_id).map(|(_, s)| s);
            self.scene.add_output(scene_id, rect, scale);
            #[allow(clippy::float_cmp)] // Exact: "did this number change", not "are these near".
            if was.is_some_and(|s| s != scale) {
                rescaled.push(scene_id);
            }
            // The atlas is the output's size too; a new one is paid here,
            // on the mode change, never on the overview path. Leaving
            // overview on this output first puts every thumbnail back
            // before its images lose their buffer.
            let resized_atlas = self
                .outputs
                .iter()
                .find(|o| o.kms_id == info.id)
                .and_then(|o| o.atlas)
                .filter(|a| a.size != (info.width, info.height));
            if let Some(stale) = resized_atlas {
                if self.wm.overview().is_some_and(|o| o.output == scene_id) {
                    self.leave_overview(None);
                }
                stale.free(&mut self.scene);
                let atlas = overview::Atlas::allocate(&mut self.scene, info.width, info.height);
                if let Some(existing) = self.outputs.iter_mut().find(|o| o.kms_id == info.id) {
                    existing.atlas = atlas;
                }
            }
            if let Some(existing) = self.outputs.iter_mut().find(|o| o.kms_id == info.id) {
                existing.width = info.width;
                existing.height = info.height;
                existing.refresh_ns = frame::refresh_ns(info.refresh_mhz);
                // A resized shadow is blank again; the `invalidate` below
                // is what repaints into it, so the two belong together.
                if let Some(shadow) = existing.shadow.as_mut()
                    && (shadow.width() != info.width || shadow.height() != info.height)
                {
                    *shadow = frame::Shadow::new(info.width, info.height);
                }

                // A mode change replaces the backend's buffers, which come
                // back as XRGB8888; the next paint re-selects the format.
                if existing.alpha && self.backend.set_scanout_alpha(info.id, false).is_err() {
                    warn!("{}: could not reset the scanout format", info.id);
                }
                existing.alpha = false;
                existing.plane_info = self.backend.planes(info.id);
                existing.hint_format = planes::hint_format(&existing.plane_info);
                existing.invalidate();
                continue;
            }
            info!(
                "{} {}: {}x{}@{}.{:03} Hz, scale {scale}, at {},{}",
                info.id,
                info.name,
                info.width,
                info.height,
                info.refresh_mhz / 1000,
                info.refresh_mhz % 1000,
                origin.x,
                origin.y
            );
            let mut state = OutputState::new(
                info.id,
                scene_id,
                info.width,
                info.height,
                info.refresh_mhz,
                self.shadow,
            );
            state.plane_info = self.backend.planes(info.id);
            state.hint_format = planes::hint_format(&state.plane_info);
            // The overview atlas is paid here, when the output appears,
            // and pre-faulted: nothing on the Super path allocates.
            if self.wants_atlas() {
                state.atlas = overview::Atlas::allocate(&mut self.scene, info.width, info.height);
            }
            self.outputs.push(state);
        }
        self.origins = origins;
        rescaled
    }

    /// Tell every window on a rescaled output about its new scale.
    ///
    /// A scale change reaches the clients only as a `Configure`: the scene
    /// marks the window roots `Dirty::TRANSFORM` so the *pixels* are
    /// re-transformed, but a client sizing its buffers from
    /// `Configure.scale` would keep drawing at the old one. The outputs
    /// themselves are `invalidate`d by [`Server::layout_outputs`], so the
    /// repaint is already queued; this is the half the clients need.
    fn reconfigure_rescaled(&mut self, rescaled: &[SceneOutputId]) {
        if rescaled.is_empty() {
            return;
        }
        let windows: Vec<WindowKey> = self
            .wire_clients
            .values()
            .flat_map(|c| c.windows.values().copied())
            .filter(|w| {
                self.scene
                    .window_info(*w)
                    .ok()
                    .and_then(nitro_scene::Window::output)
                    .is_some_and(|id| rescaled.contains(&id))
            })
            .collect();
        info!(
            "scale changed on {} output(s); reconfiguring {} window(s)",
            rescaled.len(),
            windows.len()
        );
        for win in windows {
            self.configure(win);
        }
    }

    /// Move every window the scene unplaced (its output went away) onto the
    /// primary output, clamped into its work area.
    ///
    /// A window that is simply left unplaced is invisible and unreachable:
    /// it is not in any z-order, so no click and no `Alt+Tab` raise can get
    /// it back. Migrating is the only behaviour that does not lose work.
    fn migrate_orphans(&mut self) {
        let orphans: Vec<WindowKey> = self
            .wire_clients
            .values()
            .flat_map(|c| c.windows.values().copied())
            .filter(|w| {
                self.scene
                    .window_info(*w)
                    .is_ok_and(|i| i.output().is_none() && !i.is_popup())
                    && !self.drag_icons.contains(w)
            })
            .collect();
        // An orphaned parent's popups are dismissed, not migrated — and
        // *before* the migration, so no popup is ever briefly a toplevel.
        // Also before the primary check: with every output gone the
        // windows wait, but a menu anchored to a screen that no longer
        // exists is over, and its client has to hear so.
        for win in &orphans {
            self.dismiss_popups_of(*win);
        }
        let Some(primary) = self.primary_output() else {
            // Every output is gone; the windows wait, exactly as they do
            // between startup and the first connector.
            return;
        };
        let area = self.local_work_area(primary);
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
            self.pointer_refresh = true;
            // A maximized or fullscreen window's geometry is the old
            // output's; re-derive it for the new one.
            if state != WindowState::Normal && state != WindowState::Minimized {
                self.apply_state_geometry(win, state);
            }
            self.configure(win);
            // `WindowInfo.output` changed: a shell following windows per
            // output has to hear which screen this one is on now.
            self.notify_window(win);
        }
    }

    fn output_mut(&mut self, id: KmsOutputId) -> Option<&mut OutputState> {
        self.outputs.iter_mut().find(|o| o.kms_id == id)
    }

    /// Run the scene's update pass and fold the damage into every output.
    fn update_scene(&mut self) {
        let (configures, mut offscreen) = self.update_scene_once();
        // A resize the server decided on has already re-laid its frame
        // (`set_frame_rect`). A client's own `SetBounds` on its window
        // root has not: the scene resized the content and the frame
        // group, but the decorations drawn inside the frame — border,
        // title bar, background — are the server's, and only this layer
        // knows them. Without this the frame kept its old size around a
        // client that had shrunk itself, and the gap showed the frame's
        // background.
        //
        // Re-laid *here*, then folded in with a second pass rather than
        // left for the next update: on an idle desktop there may not be
        // one, and the stale frame would stay on screen until something
        // unrelated moved.
        let mut relaid = false;
        for c in &configures {
            if self.decorations.contains_key(&c.window) {
                self.relayout_frame(c.window);
                relaid = true;
            }
        }
        if relaid {
            // Decorations never resize content, so this pass has no
            // configures of its own to send.
            let (again, more) = self.update_scene_once();
            debug_assert!(again.is_empty(), "a frame re-layout resized content");
            offscreen.extend(more);
        }
        // Overview thumbnails are offscreen in atlas mode: their changes
        // came back per window. Re-render those parts of the atlas, then
        // one more pass carries the atlas images' damage to the output.
        if self.render_thumbs(&offscreen) {
            let (again, more) = self.update_scene_once();
            debug_assert!(again.is_empty(), "rendering the atlas resized content");
            debug_assert!(more.is_empty(), "rendering the atlas dirtied a thumbnail");
        }
        // Every resize is told to the client, which lays out for it.
        for configure in configures {
            self.send_configure(configure.window, configure.size);
        }
    }

    /// One scene update: fold its damage into the outputs and return the
    /// windows whose size it changed, and the damage inside offscreen
    /// windows (the overview's thumbnails in atlas mode).
    fn update_scene_once(
        &mut self,
    ) -> (
        Vec<nitro_scene::Configure>,
        Vec<(WindowKey, nitro_core::IRect)>,
    ) {
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
            let local = |r: &nitro_core::IRect| r.translate(-origin.0, -origin.1);
            // A scroll hint rides along with the damage, never instead of
            // it: an output that ignores it is exactly as correct.
            let hint = result
                .translations
                .iter()
                .find(|t| t.output == id && self.scroll_blit && output.shadow.is_some());
            if let Some(t) = hint {
                let mut foreign = Damage::new();
                for r in t.foreign.rects() {
                    foreign.add(local(r));
                }
                let rects: Vec<nitro_core::IRect> = damage.rects().iter().map(local).collect();
                output.damage_scroll(
                    &rects,
                    frame::PendingScroll {
                        node: t.node,
                        moves_node: t.moves_node,
                        delta: t.delta,
                        clip: local(&t.clip),
                        foreign,
                        blocked: false,
                    },
                );
                continue;
            }
            for r in damage.rects() {
                output.damage_content(local(r));
            }
        }
        (result.configures, result.offscreen)
    }

    /// Re-render the overview thumbnails `damage` (global device pixels,
    /// per offscreen window) touches into the output's atlas — or, right
    /// after entry, every thumbnail whole. One render per window per call,
    /// over the bounding box of its damage inside its slot. Returns
    /// whether anything was rendered (the atlas images then need a scene
    /// pass to reach the output).
    fn render_thumbs(&mut self, damage: &[(WindowKey, nitro_core::IRect)]) -> bool {
        let Some(ov) = self.wm.overview().filter(|o| o.atlas) else {
            debug_assert!(damage.is_empty(), "offscreen damage outside atlas mode");
            return false;
        };
        let output = ov.output;
        let Some(atlas) = self
            .outputs
            .iter()
            .find(|o| o.scene_id == output)
            .and_then(|o| o.atlas)
        else {
            return false;
        };
        let Some((orect, scale)) = self.scene.output_info(output) else {
            return false;
        };
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let mut jobs: Vec<(WindowKey, nitro_core::IRect)> = Vec::new();
        if ov.rendered {
            for (win, rect) in damage {
                let Some(t) = ov.thumbs.iter().find(|t| t.window == *win) else {
                    debug_assert!(false, "offscreen damage for a window that is no thumbnail");
                    continue;
                };
                let slot = overview::slot_device_rect(&t.slot, scale);
                let local = rect.translate(-orect.x, -orect.y).intersect(&slot);
                if local.is_empty() {
                    continue;
                }
                match jobs.iter_mut().find(|(w, _)| w == win) {
                    Some((_, r)) => *r = r.union(&local),
                    None => jobs.push((*win, local)),
                }
            }
        } else {
            jobs.extend(
                ov.thumbs
                    .iter()
                    .map(|t| (t.window, overview::slot_device_rect(&t.slot, scale))),
            );
        }
        if let Some(ov) = self.wm.overview_mut() {
            ov.rendered = true;
        }
        let mut paint = overview::ThumbPaint {
            text: &mut self.text,
            icons: &mut self.icons,
            palette: &self.palette,
            items: &mut self.paint_items,
        };
        let mut any = false;
        for (win, rect) in jobs {
            match overview::render_thumb(
                &mut self.scene,
                &atlas,
                &mut paint,
                win,
                rect,
                (orect.x, orect.y),
            ) {
                Ok(us) => {
                    self.thumb_renders += 1;
                    self.thumb_render_us += us;
                    any = true;
                }
                Err(e) => warn!("rendering a thumbnail: {e}"),
            }
        }
        any
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
    /// With a shadow buffer this is two steps with two different cost
    /// models: rasterize **this frame's damage** into heap memory, then
    /// stream `damage(n) ∪ damage(n-1)` — the age-2 region the back buffer
    /// is behind by — out of the shadow into the write-combined scanout
    /// mapping with write-only row copies. Without one (`NITRO_SHADOW=0`)
    /// the rasterizer is given the union and writes it straight out, which
    /// is what the server did before #539.
    ///
    /// Returns whether a commit went in, which
    /// [`Server::paint_all`] uses to tell "nothing to do" from "held".
    #[allow(clippy::too_many_lines)] // One frame's steps in order; splitting it would scatter the age-2/shadow reasoning.
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
        // Which Surfaces go on planes this frame (#3899), before anything
        // is rasterized: a switch invalidates the output.
        self.plan_planes(index);
        if self.outputs[index].decision.mode == planes::Mode::Gpu {
            return self.paint_gpu(index);
        }
        let output = &mut self.outputs[index];
        if output.decision.mode == planes::Mode::Direct {
            // The output buffer is not on screen: nothing to paint into
            // it. Leaving direct scanout invalidates it again.
            output.discard_damage();
        }
        if !output.needs_raster() {
            return output.planes_dirty && self.flip_planes(index);
        }
        let region = self.outputs[index].repaint_region();
        if region.is_empty() {
            return false;
        }
        let scene_id = self.outputs[index].scene_id;
        let cursor_state = self.cursor_state(scene_id);
        // Spent whatever this paint does: see `OutputState::take_scroll`.
        let scroll = self.outputs[index].take_scroll();
        // A snap overview's scaled items are all thumbnails: they take the
        // fast XR24 blit (~4 ns/px against ~12), as an atlas render does.
        // Nowhere else, so no live pixel outside one changes.
        let fast_scaled = self
            .wm
            .overview()
            .is_some_and(|o| o.output == scene_id && !o.atlas);
        // (rasterized px, moved px, took the blit), for the statistics.
        let mut split = (frame::region_area(&region), 0, false);
        let (paint_us, copy_us) = {
            let mut buf = match self.backend.back_buffer(id) {
                Ok(b) => b,
                Err(e) => {
                    warn!("{id}: back buffer: {e}");
                    return false;
                }
            };
            let output = &mut self.outputs[index];
            let bounds = output.bounds();
            let mut rasterize = output.rasterize_region();
            if let Some(shadow) = output.shadow.as_mut() {
                // A stride or geometry the shadow was not built for means
                // its contents are gone, and a partial copy out of a blank
                // shadow would put black on screen. Repaint everything
                // instead — the same answer `invalidate` gives, for the
                // same reason. (The first frame of every output takes this
                // branch on a backend whose pitch is not `width * 4`, and
                // is a full repaint already.)
                let reset = shadow.ensure(buf.width, buf.height, buf.stride);
                if reset {
                    rasterize = vec![bounds];
                    // A shadow the helper imported was just replaced.
                    if self.gpu.owner == Some(id) {
                        self.gpu_reshadow = true;
                    }
                }
                let painted = paint_shadow(
                    shadow,
                    &mut ShadowPaint {
                        scene: &self.scene,
                        text: &mut self.text,
                        icons: &mut self.icons,
                        items: &mut self.paint_items,
                        palette: &self.palette,
                        output: scene_id,
                        bounds,
                        cursor: (&self.cursor, cursor_state),
                        fast_scaled,
                    },
                    &rasterize,
                    scroll.filter(|_| !reset),
                );
                let paint_us = painted.paint_us;
                split = (painted.raster_px, painted.moved_px, painted.blitted);
                shadow.note_painted(&rasterize);
                let copy_us = frame::copy_region(shadow, &mut buf, &region);
                (paint_us, copy_us)
            } else {
                let (width, height, stride) = (buf.width, buf.height, buf.stride);
                let mut canvas = Canvas::new(buf.data, width, height, stride);
                let paint_us = frame::paint_region(
                    &mut canvas,
                    &self.scene,
                    &mut self.text,
                    &mut self.icons,
                    scene_id,
                    &region,
                    (&self.cursor, cursor_state),
                    &mut self.paint_items,
                    &self.palette,
                    fast_scaled,
                );
                (paint_us, 0)
            }
        };
        // One frame stamp per painted frame: the atlas's LRU counts frames,
        // not glyphs.
        self.text.next_frame();
        let damage_px = frame::region_area(&region);
        let kms_damage: Vec<KmsRect> = region
            .iter()
            .map(|r| KmsRect::new(r.x, r.y, r.w.cast_unsigned(), r.h.cast_unsigned()))
            .collect();
        self.select_scanout_alpha(index);
        self.stage_plane_fences(index);
        let first = !self.outputs[index].lit;
        match self.backend.commit(id, &kms_damage) {
            Ok(()) => {
                if first {
                    // Lighting an output modesets the others, which drops
                    // their plane layouts.
                    self.planes_reset(Some(index));
                }
                self.note_on_kms(index);
                if self.first_frame_ms.is_none() {
                    let ms = self.started.elapsed().as_millis() as u64;
                    self.first_frame_ms = Some(ms);
                    info!("{id}: first frame {ms} ms after start");
                }
                self.stats.paint_us.push(paint_us);
                self.stats.copy_us.push(copy_us);
                self.stats.damage_px.push(damage_px);
                self.stats.raster_px.push(split.0);
                self.stats.blit_px.push(split.1);
                self.blit_frames += u64::from(split.2);
                self.stats.paint_log.push(paint_us);
                self.stats.damage_log.push(damage_px);
                self.outputs[index].committed();
                true
            }
            Err(e) => {
                warn!("{id}: commit: {e}");
                // Keep the damage so the next event retries; otherwise the
                // output stalls until the next resume or hotplug. The
                // shadow keeps what was painted into it — it is never
                // stale — so the retry only has to copy again.
                self.outputs[index].commit_failed(&region);
                // A layout the kernel refused at commit time after
                // accepting it in a test: composite from here on.
                if !self.outputs[index].decision.layout.is_empty() {
                    self.planes_fallback(index);
                }
                false
            }
        }
    }

    /// Scan the output out as `ARGB8888` exactly while a hole is on it and
    /// the plane can blend alpha, `XRGB8888` otherwise (#3898). The pixels
    /// are the same bytes either way, so this only swaps the framebuffer
    /// format for the coming commit. Without Surfaces on a plane,
    /// [`nitro_scene::Scene::has_holes`] is an O(1) `false` and nothing
    /// else happens.
    fn select_scanout_alpha(&mut self, index: usize) {
        let output = &mut self.outputs[index];
        // Only underlays need the holes to be transparent; an overlay
        // above covers its hole whatever its alpha (#3899).
        let holes = output.decision.need_alpha() && self.scene.has_holes(output.scene_id);
        frame::select_scanout_alpha(self.backend.as_mut(), output, holes);
    }

    /// Where the cursor is on `output`, in that output's buffer space,
    /// which shape it is showing and how far it is magnified.
    fn cursor_state(&self, output: SceneOutputId) -> CursorState {
        let (origin, scale) = self
            .scene
            .output_info(output)
            .map_or(((0, 0), 1.0), |(rect, scale)| ((rect.x, rect.y), scale));
        let (x, y) = self.pointer.device();
        CursorState {
            x: x - origin.0,
            y: y - origin.1,
            // A hidden cursor still reports a shape, for the damage
            // arithmetic's sake; `visible` is what stops it painting.
            shape: self.cursor_shown.unwrap_or_default(),
            // Per **output**: the cursor is painted in device pixels, so
            // a 2x output needs a 2x cursor to be the same physical size,
            // and the pointer can be on either screen of a mixed-scale
            // desk. Each output paints it at its own factor.
            scale: Cursor::paint_scale(scale),
            visible: self.pointer.present && self.cursor_shown.is_some(),
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
        for (token, accepted, action) in std::mem::take(&mut self.pending_drag_finished) {
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::DragFinished(msg::DragFinished {
                    accepted,
                    action,
                }));
            }
        }
        // Drag starts, for the same reason, and before `flush_popup_done`:
        // starting one dismisses a grabbing menu.
        for (token, start) in std::mem::take(&mut self.pending_drag_starts) {
            self.start_dnd(token, start);
        }
        // Overview requests, for the same reason: entering dismisses
        // popups (a `PopupDone` to their owner) and moves pointer focus.
        // Each one is answered as it is applied, so a requester always
        // hears what its own request did.
        for (token, request) in std::mem::take(&mut self.pending_overview) {
            self.apply_overview_request(request);
            self.announce_overview(Some(token));
        }
        // Whatever else changed overview state this wakeup — a scrim
        // click, the lock, a hotplug, a WM hotkey, the control request —
        // is announced here, once. After every leave and enter of the
        // wakeup, so a relayout's leave + enter nets out to nothing.
        self.announce_overview(None);
        // The same deferral for `PopupDone`: dismissal can run with the
        // owning client out of the map. Drained before the update, so the
        // unmap is already recorded when the client hears (unmap, then
        // notify).
        self.flush_popup_done();
        // Last thing before the update: every path that can change which
        // window is frontmost, its state or the focus has run by now.
        self.sync_fullscreen_cover();
        // Queued Surface frames whose output can take a paint now: in
        // steady state the flip is pending here and they wait for
        // `on_flip`; on an idle output they latch at once.
        self.latch_surfaces();
        self.update_scene();
        self.send_surface_hints();
        // A window mapped, unmapped or moved under a stationary pointer
        // changed what the pointer is over; hit testing needs the update
        // above.
        if std::mem::take(&mut self.pointer_refresh) {
            self.refresh_pointer_over();
        }
        // Pointer focus moved under a still pointer: the displayed cursor
        // reverts to the server's own choice now, not at the next motion.
        // After the scene update, so the hit test sees the window gone. A
        // drag in flight owns the shape and is left alone.
        if std::mem::take(&mut self.cursor_stale)
            && self.wm.drag().is_none()
            && !self.dnd_grabbing()
        {
            self.update_cursor_shape();
        }
        self.claim_input_stamp();
        self.paint_or_defer();
        self.answer_idle_clients();
        self.send_buffer_releases();
        self.flush_wire_clients();
    }

    /// Queue `BufferReleased` for every buffer no image node references
    /// any more (`Scene::take_released_buffers`), to owners that listed
    /// `caps::RELEASE`. Called just before the flush, so a release rides
    /// the same write as the commit's other replies and never costs a
    /// wakeup of its own. Releases for clients without the cap, or already
    /// gone, are drained and dropped.
    fn send_buffer_releases(&mut self) {
        let mut released = std::mem::take(&mut self.released);
        released.clear();
        self.scene.take_released_buffers(&mut released);
        for &(owner, key) in &released {
            let Some(client) = self.wire_clients.values_mut().find(|c| c.id == owner) else {
                continue;
            };
            if client.client_caps & nitro_wire::types::caps::RELEASE == 0 {
                continue;
            }
            // Still scanned out by a plane (#3899): released when the
            // flip that replaces it completes.
            if !self.on_kms.is_empty()
                && let Some(k) = client
                    .buffers
                    .values()
                    .find(|h| h.key == key)
                    .and_then(|h| h.scanout)
                && self.on_kms.contains(&k)
            {
                self.held_releases.push((k, owner, key));
                continue;
            }
            // Sampled by a GPU-helper frame still running (#3922).
            if self.gpu.borrows.holds(key) {
                self.gpu.held.push((owner, key));
                continue;
            }
            if let Some(id) = client.buffer_id(key) {
                client.send(&ServerMsg::BufferReleased(msg::BufferReleased { id }));
            }
        }
        released.clear();
        self.released = released;
        self.gpu_prune();
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
            || self.outputs.iter().any(|o| o.gpu_pending.is_some())
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
                    TOK_SIGHUP => {
                        if self
                            .signals
                            .as_mut()
                            .is_some_and(signals::Signals::drain_reload)
                        {
                            info!("SIGHUP");
                            self.reload_config();
                        }
                    }
                    TOK_CONFIG => self.on_config_event(),
                    TOK_WIRE_LISTENER => self.on_wire_accept()?,
                    TOK_SHELL_LISTENER => self.on_shell_accept()?,
                    TOK_REMOTE_LISTENER => self.on_remote_accept()?,
                    TOK_BACKEND => self.on_backend()?,
                    TOK_INPUT => self.on_input(),
                    TOK_INPUT_HOTPLUG => self.on_input_hotplug(),
                    TOK_DEFER => self.on_defer_deadline(),
                    TOK_REPEAT => self.on_key_repeat(),
                    TOK_INJECT => self.on_inject(),
                    TOK_GPU => self.on_gpu(),
                    TOK_GPU_TIMER => self.on_gpu_timer(),
                    // Shell tokens sort above wire tokens, so this arm has
                    // to come first; both end up in `on_wire_client`,
                    // because a shell client *is* a wire client with an
                    // extra capability bit. Remote tokens sort above both,
                    // for the same reason and with the same answer.
                    // Fences sort above every client range (#3918).
                    t if t >= TOK_GPU_FENCE_BASE => self.on_gpu_fence(t - TOK_GPU_FENCE_BASE),
                    t if t >= TOK_FENCE_BASE => self.on_fence(t),
                    t if t >= TOK_REMOTE_BASE => self.on_wire_client(t, flags),
                    t if t >= TOK_SHELL_BASE => self.on_wire_client(t, flags),
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
                    // The release of a held key will arrive on the other
                    // VT, if anywhere: stop repeating it now.
                    self.stop_key_repeat();
                    // Scripted input stops with the real kind: the
                    // rest of a sequence would land on the other VT's
                    // return with stale timestamps.
                    if let Err(e) = self.injector.clear() {
                        warn!("input injection: disarm: {e}");
                    }
                    // A drag cannot survive the pointer going to another
                    // session: the release will never arrive here.
                    self.dnd_step(data::Dnd::cancel);
                    // The helper goes with the VT (#3922): it is
                    // respawned on the way back.
                    self.gpu_pause();
                    self.backend.pause();
                    self.active = false;
                    if let Some(seat) = self.seat.as_ref() {
                        seat.borrow_mut().ack_disable()?;
                    }
                }
                SeatEvent::Enable => {
                    info!("session active: resuming");
                    let resumed = self.backend.resume();
                    self.planes_reset(None);
                    self.gpu.forgive();
                    if self.gpu.mode == config::GpuHelper::On {
                        self.gpu_spawn();
                    }
                    match resumed {
                        Ok(()) => self.active = true,
                        Err(e) => {
                            error!("resume failed: {e}");
                            continue;
                        }
                    }
                    self.input.resume();
                    // Key releases that happened on the other VT were never
                    // seen, so the modifier state is a guess: drop it. The
                    // shell's armed tap goes with it for the same reason —
                    // the release that would complete it never arrived.
                    if let Some(kb) = self.keyboard.as_mut() {
                        kb.reset();
                        self.stop_key_repeat();
                    }
                    self.sync_modifiers();
                    self.hotkeys.reset();
                    self.hotkey_pending = None;
                    // Held buttons, for the keyboard's reason: a release on
                    // the other VT was never seen, and a button believed
                    // down for ever would let any client start a drag.
                    self.pointer.buttons.clear();
                    self.end_pointer_grab();
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
        // Plane buffers a flip stopped reading (#3899), before the flip's
        // `Presented` goes out.
        self.drain_kms_releases();
        let mut hotplug = false;
        for ev in events.drain(..) {
            match ev {
                Event::Flipped {
                    output,
                    sequence,
                    time,
                } => {
                    self.frames += 1;
                    // The flip's own output's period, so the
                    // `FLIP_INTERVAL_MAX_PERIODS` cut-off below means four
                    // *actual* frames — 8.3 ms each at 120 Hz, not 16.7.
                    // The literal is the fallback for one racy case: a
                    // completion arriving for an output unplugged between
                    // the commit and the event, where there is no period
                    // to read and the sample is about to be discarded
                    // anyway.
                    let refresh_ns = self
                        .outputs
                        .iter()
                        .find(|o| o.kms_id == output)
                        .map_or(frame::refresh_ns(0), |o| o.refresh_ns);
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
            let rescan = self.backend.rescan();
            // A rescan modesets the lit outputs: every layout is back to
            // the default.
            self.planes_reset(None);
            match rescan {
                Ok(changed) => {
                    if changed {
                        self.sync_outputs();
                    }
                }
                Err(e) => warn!("rescan: {e}"),
            }
            // A rescan re-reads every connector, so a `mode` line that
            // matches nothing produces its warning again. Drained here
            // rather than left to the next reload: the warnings are a
            // `Vec` on the backend, so a flapping connector would
            // otherwise grow it without bound and then deliver the whole
            // pile at once, long after the event that caused it.
            for w in self.backend.take_warnings() {
                warn!("{w}");
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
        let prev_vblank_ns = output.last_vblank_ns;
        output.last_vblank_ns = time_ns;
        output.last_sequence = sequence;
        let presented = std::mem::take(&mut output.in_flight);
        let input_ns = std::mem::take(&mut output.in_flight_input_ns);
        let scene_id = output.scene_id;
        let refresh_ns = output.refresh_ns;
        let deadline_ns = output.frame_deadline_ns(time_ns);

        if prev_vblank_ns > 0 && time_ns > prev_vblank_ns {
            self.stats.flip_log.push((time_ns - prev_vblank_ns) / 1_000);
        }
        if input_ns > 0 && time_ns > input_ns {
            let us = (time_ns - input_ns) / 1_000;
            self.stats.i2p_us.push(us);
            self.stats.i2p_log.push(us);
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
        //
        // The overview badges' fade steps here, per vblank: see
        // `step_overview_fade` for why no timer is needed. On *now*, not
        // the flip's `time_ns`: the vblank of a frame already in flight
        // when overview was entered predates the fade's start stamp, and
        // a step at elapsed 0 would set the opacity it already has,
        // damage nothing, flip nothing — and stall the fade.
        // `paint` does not update the scene itself, so the step's damage
        // is folded into the outputs here.
        let faded =
            self.overview_output() == Some(scene_id) && self.step_overview_fade(monotonic_ns());
        // The latch point (#3897): this output has no flip pending any
        // more, so the newest queued Surface frame on it becomes current
        // and rides the paint below. The buffer it replaces is released
        // in the same write, well before this frame's `Presented`.
        // A helper frame that finished while this flip was pending (#3922).
        if let Some(i) = self.outputs.iter().position(|o| o.kms_id == id) {
            self.gpu_commit(i);
        }
        let latched = self.latch_surfaces();
        if faded || latched {
            self.update_scene();
        }
        if latched {
            self.send_surface_hints();
            self.send_buffer_releases();
        }
        self.paint_or_defer();
        // A commit stamped while this flip was in flight (#646) is in
        // `painting`; if nothing is left to paint, no frame will carry it.
        self.answer_idle_clients();
        self.flush_wire_clients();
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
            let mut client = WireClient::new(stream, id);
            client.peer_uid = peer_uid(client.as_fd());
            self.wire_clients.insert(token, client);
        }
    }

    /// A client connected to the **privileged** socket.
    ///
    /// Identical to [`Server::on_wire_accept`] but for the token range, and
    /// that is the point: the privilege is not a different transport or a
    /// different state machine, it is one bit in the `Welcome` decided by
    /// which listener accepted the socket. `docs/shell.md` argues why.
    fn on_shell_accept(&mut self) -> Result<(), Error> {
        loop {
            let stream = match self.shell_listener.accept() {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(()),
                Err(e) => {
                    warn!("shell accept: {e}");
                    return Ok(());
                }
            };
            let id = ClientId(self.next_client_id);
            self.next_client_id += 1;
            let token = TOK_SHELL_BASE + self.next_shell;
            self.next_shell += 1;
            add(&self.epoll, &stream.as_fd(), token)?;
            debug!("shell client {} connected", id.0);
            let mut client = WireClient::new(stream, id);
            client.peer_uid = peer_uid(client.as_fd());
            self.wire_clients.insert(token, client);
        }
    }

    /// A client connected over **TCP**.
    ///
    /// Identical to [`Server::on_wire_accept`] but for the token range,
    /// and again that is the point: a remote client is not a different
    /// kind of client, it is one whose token says its link cannot carry
    /// descriptors. `docs/remote.md` argues the model.
    fn on_remote_accept(&mut self) -> Result<(), Error> {
        loop {
            let Some(listener) = self.remote_listener.as_ref() else {
                return Ok(());
            };
            let stream = match listener.listener().accept() {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(()),
                Err(e) => {
                    warn!("remote accept: {e}");
                    return Ok(());
                }
            };
            let id = ClientId(self.next_client_id);
            self.next_client_id += 1;
            let token = TOK_REMOTE_BASE + self.next_remote;
            self.next_remote += 1;
            add(&self.epoll, &stream.as_fd(), token)?;
            info!("remote client {} connected", id.0);
            self.wire_clients.insert(token, WireClient::new(stream, id));
        }
    }

    /// Bring the remote listener in line with `remote.listen`.
    ///
    /// The one place the key is turned into a socket, called from startup
    /// and from every reload. Three cases, and the third is the one worth
    /// stating: a value that has **not changed** leaves the existing
    /// listener alone, so a reload that moved a monitor does not rebind
    /// the port — and every connected remote client keeps its connection,
    /// because a connection lives on the socket it was accepted on, not
    /// on the listener.
    ///
    /// A bind that fails is a warning and no listener, never a fatal
    /// error: a typo in `remote.listen` must not take the desktop down,
    /// and "the port is in use" is a thing a person fixes while looking
    /// at their running session.
    fn apply_remote_listen(&mut self) {
        let want = self.settings.remote.listen;
        match (want, self.remote_listener.as_ref()) {
            (Some(addr), Some(current)) if current.satisfies(addr) => {}
            (Some(addr), _) => {
                // Drop the old one *first*. Not for the same-address
                // case — `satisfies` caught that in the arm above and
                // there is nothing to rebind — but for the **overlapping**
                // one: `0.0.0.0:7700` → `127.0.0.1:7700` is a changed
                // value whose two sockets cannot be bound at once, and
                // binding the new one before releasing the old would fail
                // with `EADDRINUSE` and leave the user with neither.
                self.drop_remote_listener();
                match remote::RemoteListener::bind(addr) {
                    Ok(listener) => {
                        if let Err(e) = add(
                            &self.epoll,
                            &listener.listener().as_fd(),
                            TOK_REMOTE_LISTENER,
                        ) {
                            warn!("registering the remote listener: {e}");
                            return;
                        }
                        info!("remote wire socket at tcp://{}", listener.addr());
                        self.remote_listener = Some(listener);
                    }
                    Err(e) => warn!("remote.listen {addr}: {e}"),
                }
            }
            (None, Some(_)) => {
                self.drop_remote_listener();
                info!("remote listener closed (remote.listen removed)");
            }
            (None, None) => {}
        }
    }

    /// Close the remote listener, if any, and take it out of the epoll
    /// set.
    ///
    /// The `epoll_ctl(DEL)` is explicit rather than left to the close:
    /// closing a descriptor does remove it, but the listener is dropped
    /// after this returns, and a `DEL` on a live fd is the version that
    /// cannot race a wakeup already queued for it.
    ///
    /// Connected remote clients are deliberately **not** touched. They
    /// are on their own sockets; a listener going away means "no new
    /// connections", which is what removing the key asks for.
    fn drop_remote_listener(&mut self) {
        let Some(listener) = self.remote_listener.take() else {
            return;
        };
        let _ = epoll::delete(&self.epoll, listener.listener().as_fd());
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
            self.stop_key_repeat();
        }
        if removed > 0 {
            self.sync_modifiers();
            self.hotkeys.reset();
            self.hotkey_pending = None;
        }
    }

    /// The configuration directory changed. Reload if it was our file.
    ///
    /// The queue is drained whatever it named — an undrained inotify fd
    /// stays readable, and a level-triggered epoll would then spin — but
    /// only an event naming `server.conf` costs a reload, so an editor's
    /// swap file appearing next to it is one read and nothing else.
    fn on_config_event(&mut self) {
        let ours = self.config_watch.as_mut().is_some_and(ConfigWatch::drain);
        if ours {
            info!("server.conf changed on disk");
            self.reload_config();
        }
    }

    /// Adopt a new palette: restyle every decoration, tell every client,
    /// and mark the screen for repaint.
    ///
    /// Everything a palette change costs happens here, in one place and
    /// in one wakeup: the decorations are restyled in the scene (the
    /// damage is the title bars and borders, nothing else), and every
    /// connected client — wire and shell — is sent one `Theme`. The
    /// clients' own repaints arrive as their ordinary commits, so the
    /// whole desktop changes colour in a single frame without the server
    /// waiting for anybody.
    ///
    /// The caller is responsible for the diff: this always does the work.
    ///
    /// A client that has not finished its handshake is **skipped**, and
    /// that is load-bearing rather than tidy — see the comment on the
    /// loop below.
    fn set_palette(&mut self, palette: nitro_core::Palette) {
        self.palette = palette;
        self.theme_serial = self.theme_serial.wrapping_add(1);
        // The decorations are server-drawn, so nothing else will repaint
        // them. `restyle` also re-shapes the title in its new colour.
        let framed: Vec<WindowKey> = self.decorations.keys().copied().collect();
        for win in framed {
            self.restyle(win, self.focus == Some(win));
        }
        // The captions and pills were styled from the old palette.
        self.relayout_overview();
        let theme = msg::Theme::from_palette(self.theme_serial, &self.palette);
        for client in self.wire_clients.values_mut() {
            // Not before the handshake. `wire_clients` holds a client
            // from `accept` onward, which is *earlier* than its `Hello`
            // — the two land on different epoll wakeups, and a reload
            // (inotify, SIGHUP, or a control-socket `reload`) can be
            // dispatched in between. A `Theme` queued in that window
            // reaches the socket ahead of the `Welcome`, and
            // `Connection::with_socket` requires `Welcome` to be the
            // first message: the client dies with
            // `Unexpected("Theme")` before it has drawn anything.
            //
            // Not hypothetical in the case this feature exists for:
            // ticking "Dark" in nitro-settings rewrites `server.conf`
            // while the session is still launching clients.
            //
            // Nothing is lost by skipping: the handshake path sends the
            // current palette itself, right behind the `Welcome`, so a
            // client that connects during a reload gets the *new*
            // palette a moment later rather than the old one now. This
            // is the only unconditional broadcast in this file; every
            // other one is gated by ownership or a subscription, which
            // is why it was the only one exposed.
            if client.stream.is_ready() {
                client.send(&ServerMsg::Theme(theme.clone()));
            }
        }
        info!(
            "palette {} ({} scheme, {} override(s))",
            self.theme_serial,
            self.settings.theme.scheme.unwrap_or_default().name(),
            self.settings.theme.overrides.len()
        );
    }

    /// Re-read `server.conf` and apply it: the one place all three reload
    /// triggers — inotify, SIGHUP and the control socket's `reload` — end
    /// up.
    ///
    /// Everything is re-applied unconditionally rather than diffed — with
    /// two exceptions that earn it, the keyboard and the palette, both
    /// noted below. The work is one file read, one keymap compile and one
    /// `sync_outputs`, all of which the server already does at startup; a
    /// diff would be a second description of what the settings mean, and
    /// the failure mode of a wrong diff is a desktop that ignores the
    /// file until the next reboot.
    ///
    /// A file that will not parse cannot stop the server: [`config::load`]
    /// never fails, and a line it could not use is a warning and a skipped
    /// line, so everything the file still says stays in force and
    /// everything it no longer says falls back to the environment or the
    /// EDID exactly as it did at startup.
    fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else {
            // No file: `reload` is still a valid request, it simply has
            // nothing to read. Counted anyway, so the caller can tell the
            // request was handled rather than dropped.
            self.config_reloads += 1;
            return;
        };
        let settings = config::load(&path);
        for w in &settings.warnings {
            warn!("{}: {w}", path.display());
        }
        let keyboard_changed = !settings.keyboard.same_keymap(&self.settings.keyboard);
        let repeat_changed = settings.keyboard.repeat() != self.settings.keyboard.repeat();
        let pointer_changed = settings.pointer != self.settings.pointer;
        let icons_changed = settings.theme.icon_theme() != self.settings.theme.icon_theme();
        let palette = settings.palette();
        let gpu_mode = self.gpu_env.unwrap_or_else(|| settings.gpu.helper());
        let gpu_idle = settings.gpu.idle_exit();
        self.settings = settings;
        // The GPU helper (#3922): a reload (SIGHUP included) forgives a
        // give-up, and a changed mode starts or stops it.
        self.gpu.forgive();
        if self.gpu.configure(gpu_mode, gpu_idle) {
            info!("gpu.helper = {gpu_mode:?}");
            if self.gpu.running() {
                self.gpu_pause();
            }
            self.gpu.configure(gpu_mode, gpu_idle);
        }
        if gpu_mode == config::GpuHelper::On && !self.gpu.running() && self.active {
            self.gpu_spawn();
        }
        // The palette *is* diffed, unlike everything else here, and for a
        // reason the rest does not have: applying it is not idempotent
        // from the outside. It restyles every decoration, repaints every
        // client and puts a `Theme` on every socket, so a `reload` that
        // changed only `keyboard.layout` would otherwise cost a full
        // desktop repaint and a wire message per client. An equal palette
        // is therefore silence — which is exactly what the
        // `nothing_is_sent_when_the_palette_did_not_change` test asserts.
        if palette != self.palette {
            self.set_palette(palette);
        }

        // The keyboard, only when its section actually changed: compiling
        // a keymap costs tens of milliseconds and resetting the state
        // drops the modifiers the user is holding, neither of which a
        // reload that only moved a monitor should cost.
        if keyboard_changed {
            let mut recompiled = false;
            match Keyboard::with_settings(&self.settings.keyboard) {
                Some(kb) => {
                    info!("xkb keymap: {}", kb.layout_names().join(", "));
                    // A failed export keeps the previous file, exactly as a
                    // failed compile keeps the previous keymap: clearing
                    // `keymap_fd` would retract `caps::KEYMAP` from under
                    // connected clients, and one re-sending `ClientCaps`
                    // (rule 5) would then be disconnected for naming a bit
                    // it was legitimately granted.
                    match kb.export() {
                        Some(fd) => {
                            self.keymap_fd = Some(fd);
                            recompiled = true;
                        }
                        None => warn!("keymap export failed; clients keep the previous keymap"),
                    }
                    self.keyboard = Some(kb);
                }
                None => warn!("no xkb keymap compiled; keeping the previous one"),
            }
            // A keymap swap invalidates every held modifier — the releases
            // belong to keys that no longer mean what they did — which is
            // the same reasoning the VT-switch and input-hotplug paths
            // use. The shell's armed tap goes with it.
            if let Some(kb) = self.keyboard.as_mut() {
                kb.reset();
                self.stop_key_repeat();
            }
            self.hotkeys.reset();
            self.hotkey_pending = None;
            // The client now evaluates the keymap itself, so a new one is
            // news to every `KEYMAP` client; `send_keymap` follows it with
            // the (reset) masks. Without a fresh export there is nothing new
            // to ship, but the reset still moved the masks.
            if recompiled {
                let tokens: Vec<u64> = self.wire_clients.keys().copied().collect();
                for token in tokens {
                    self.send_keymap(token);
                }
            }
            self.sync_modifiers();
        } else if repeat_changed {
            // Only the repeat moved: no recompile, no reset, nothing the
            // user is holding is dropped. A `KEYMAP` client repeats on its
            // own from the figures `Keymap` carries, so it gets the same
            // keymap again with the new ones (the wire has no separate
            // message for them, by design — `docs/wire.md` § `Keymap`).
            let tokens: Vec<u64> = self.wire_clients.keys().copied().collect();
            for token in tokens {
                self.send_keymap(token);
            }
        }
        // A key repeating at the old rate stops; the next press starts at
        // the new one. Simpler than re-timing a live repeat, and a reload
        // with a key held is not a case worth a second code path.
        if repeat_changed {
            self.stop_key_repeat();
        }
        // Speed and acceleration, only when the section moved: libinput
        // re-applies to every live device, which is cheap but not free,
        // and a reload that touched a monitor should not touch the mouse.
        // Scroll direction needs nothing here — `route_input` reads it
        // from `self.settings` on every axis event.
        if pointer_changed {
            self.input.configure_pointer(&self.settings.pointer);
        }

        // The icon theme, only when it moved, and for the same reason the
        // keyboard is diffed: re-reading it walks the whole search path
        // and throws away every decoded application tile, so a `reload`
        // that only moved a monitor must not cost the launcher its icons.
        if icons_changed {
            self.icons.set_theme(self.settings.theme.icon_theme());
        }
        // The `.desktop` index, unconditionally — see
        // [`IconEngine::rescan_desktop`]. There is no setting to diff:
        // what changes is the filesystem, and `reload` is the user saying
        // "look again" after installing something.
        self.icons.rescan_desktop();
        // A frame's icon is its window's `app_id` re-resolved, so every
        // decoration asks again: the package that just arrived may be the
        // one whose window is on screen showing the generic fallback.
        for win in self.decorations.keys().copied().collect::<Vec<_>>() {
            self.reicon(win);
        }

        // Scale, position and primary all land in `sync_outputs`, which is
        // the one place that decides them; it re-`Configure`s the clients
        // of any output whose scale moved and `invalidate`s every output.
        //
        // The **mode** is applied first, because it decides what
        // `sync_outputs` is laying out: a retimed or resized output has to
        // be the one the scene is told about, not the one it was before.
        self.apply_modes();
        self.sync_outputs();
        // `overview.animate`: allocate or free every output's atlas now,
        // not on the next Super press.
        self.apply_overview_atlas();
        // The remote listener, which may appear, move or go away. Done
        // unconditionally like everything else here, and idempotent: an
        // unchanged `remote.listen` is a comparison and nothing more, so
        // a reload that touched only the keyboard never disturbs a
        // connected remote client.
        self.apply_remote_listen();
        // `invalidate` only marks. A scale change repaints the whole
        // screen, so drive it now rather than waiting for the next thing
        // that happens to damage something.
        self.paint_all();
        self.settle();
        self.config_reloads += 1;
        info!("configuration reloaded from {}", path.display());
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
                // `pointer.natural_scroll`: inverted here, for every axis
                // source (wheel, finger, continuous) and every device —
                // including ones libinput has no natural-scroll switch
                // for — rather than through libinput, so nothing is ever
                // inverted twice.
                let (dx, dy) = if self.settings.pointer.natural_scroll() {
                    (-dx, -dy)
                } else {
                    (dx, dy)
                };
                let sent_to = self.send_input(window, |id| {
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
        let old_shape = self.cursor_shown;
        let appeared = self.pointer.seen();
        if appeared && let Some(shape) = old_shape {
            self.damage_cursor_at(old_x, old_y, shape);
        }
        if !self.pointer.move_to(x, y, bounds) && !appeared {
            return;
        }
        let (new_x, new_y) = self.pointer.device();
        if (old_x, old_y) != (new_x, new_y)
            && let Some(shape) = old_shape
        {
            // Old ∪ new, exactly like the scene's own damage rule. A
            // hidden cursor draws nothing, so moving it damages nothing.
            self.damage_cursor_at(old_x, old_y, shape);
            self.damage_cursor_at(new_x, new_y, shape);
        }
        let point = self.pointer.position();
        let output = input::output_at(&self.scene, point);
        self.pointer.output = output;
        // A drag-and-drop in flight owns every motion, like a window drag.
        if self.dnd_grabbing() {
            self.drive_dnd(time_ns, true);
            self.note_input(time_ns);
            return;
        }
        // A drag in flight owns every motion: the window follows the
        // pointer and the client is never consulted, which is what makes a
        // drag zero round trips. Both a move and a resize do send one
        // one-way `Configure` per motion, and the frame scheduler already
        // throttles those to one per frame.
        if let Some(drag) = self.wm.drag()
            && self.drive_drag(drag)
        {
            // A drag in flight owns the shape too: a title drag shows the
            // move cross for as long as it lasts, and a resize drag keeps
            // the shape of the edges it grabbed even once the pointer has
            // run past them. Neither can be re-derived from the region
            // under the pointer, because during a drag the pointer is
            // routinely nowhere near the frame it is moving.
            self.set_cursor(Some(Self::drag_shape(drag)));
            self.note_input(time_ns);
            return;
        }
        // The implicit grab (`Pointer::grab`, `docs/wire.md`): a press was
        // delivered to a window and a button is still down, so every
        // motion is that window's whatever it is over now, in its own
        // coordinates — which may be negative or beyond its content, as
        // `input::window_local` allows. No enter/leave goes out: `over`
        // stays where the press left it, so the grabbing client's
        // `SetCursor` stays honoured and its released-outside gesture
        // works. The frame affordances go dark for the same reason a
        // drag's do: a band that would not act on a press must not light.
        if let Some(grab) = self.pointer.grab {
            if self.scene.window_info(grab).is_err() {
                // The window went away under a held button; fall back to
                // the ordinary path, which re-derives focus.
                self.end_pointer_grab();
            } else {
                self.grabbed_motion(grab, point, output, time_ns);
                return;
            }
        }
        // The resize affordance and the button hover both follow the
        // pointer, but not during a drag: the branch above has already
        // returned, so a drag in flight never repaints a frame it is not
        // over.
        //
        // This puts `frame_hit` on the **motion** path, where it used to run
        // only on a button press — a z-order walk per motion event. That is
        // why it no longer allocates, and why the restyle it may cause is
        // `style_only` rather than the full `restyle`: see both for the
        // costs that were taken back out.
        //
        // **One** walk answers both affordances. #3715 added the second
        // and took the obvious shape first — a `resize_hint_at` and a
        // `button_hover_at`, each doing its own hit test — which is two
        // z-order walks per motion event where #3713's review had just
        // finished getting it down to one.
        // In overview no frame affordance applies anywhere on that
        // output: every frame is hidden or scaled, and the pointer over a
        // thumbnail belongs to the window manager (`input::overview_hit`).
        let in_overview = output.is_some() && output == self.overview_output();
        let frame_hit = if in_overview {
            None
        } else {
            self.pointer_desktop().and_then(|p| self.frame_hit(p))
        };
        self.set_resize_hint(
            frame_hit.and_then(|(win, region)| matches!(region, Region::Resize(_)).then_some(win)),
        );
        self.set_button_hover(frame_hit.filter(|(_, region)| region.is_button()));
        let target = output.and_then(|id| self.pointer_target(id, point));
        let now_over = target.map(|t| t.window);
        if now_over != self.pointer.over {
            if let Some(left) = self.pointer.over {
                let sent_to = self.send_input(left, |id| {
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
            self.set_pointer_over(now_over);
            if let Some(t) = target {
                let node = self.node_id_for(t.window, t.hit.node);
                let sent_to = self.send_input(t.window, |id| {
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
            let sent_to = self.send_input(t.window, |id| {
                ServerMsg::PointerMotion(msg::PointerMotion {
                    window: id,
                    node,
                    pos: t.local,
                    time_ns,
                })
            });
            self.note_client_input(sent_to);
        }
        // And the cursor shape, off that same one walk — decided **after**
        // enter/leave, so a client's request is judged against the window
        // the pointer is on now, not the one it just left.
        // In overview the only client the pointer can be over is a
        // `Top`/`Overlay` one, whose content honours its own cursor.
        let cursor_hit = if in_overview {
            self.pointer.over.map(|w| (w, Region::Content))
        } else {
            frame_hit
        };
        self.set_cursor(self.cursor_choice(cursor_hit));
        self.cursor_stale = false;
        self.note_input(time_ns);
    }

    /// A motion while `grab` holds the implicit grab: see `move_pointer`.
    fn grabbed_motion(
        &mut self,
        grab: WindowKey,
        point: Point,
        output: Option<SceneOutputId>,
        time_ns: u64,
    ) {
        self.set_resize_hint(None);
        self.set_button_hover(None);
        let local = input::window_local(&self.scene, grab, point).unwrap_or(Point::ZERO);
        let node = output
            .and_then(|id| self.pointer_target(id, point))
            .filter(|t| t.window == grab)
            .map_or(NodeId::NONE, |t| self.node_id_for(grab, t.hit.node));
        let sent_to = self.send_input(grab, |id| {
            ServerMsg::PointerMotion(msg::PointerMotion {
                window: id,
                node,
                pos: local,
                time_ns,
            })
        });
        self.note_client_input(sent_to);
        self.set_cursor(self.cursor_choice(Some((grab, Region::Content))));
        self.cursor_stale = false;
        self.note_input(time_ns);
    }

    fn pointer_button(&mut self, button: u32, state: ButtonState, time_ns: u64) {
        /// Linux evdev `BTN_RIGHT`.
        const BTN_RIGHT: u32 = 0x111;

        // Button state is recorded before *anything* else, because every
        // branch below may return early — and the release that ends a drag,
        // the commonest release there is, is one of them. A release missed
        // here leaves the server believing a button is held for ever, which
        // is exactly the hole the `StartMove`/`StartResize` guard closes.
        match state {
            ButtonState::Pressed => self.pointer.press(button),
            ButtonState::Released => self.pointer.release(button),
        }

        // The grabs go **first** — above the release/drag branch: a
        // drag-and-drop (which holds the pointer, so no popup grab can
        // coexist with it), then the popup grab, then overview mode. See
        // `Server::button_grabs`.
        if self.button_grabs(button, state, time_ns) {
            return;
        }

        // A click while a modifier is held is not a bare-modifier tap. This
        // is what keeps `Super`-drag (`docs/wm.md`) and the launcher's
        // bare-Super trigger from being the same gesture: a drag ends with
        // Super released and no key in between, which is exactly a tap's
        // shape, and the button is the only thing that distinguishes them.
        if state == ButtonState::Pressed {
            self.hotkeys.cancel_tap();
        }

        // A release always ends whatever drag was in flight, whether or not
        // the pointer is still over the window it started on: a drag that
        // survived the button coming up would follow the pointer for ever.
        if state == ButtonState::Released
            && let Some(drag) = self.wm.end_drag()
        {
            // The window stopped following the pointer; whatever is under
            // it now gets the enter the drag held back.
            self.pointer_refresh = true;
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
                        // Not a soft close: the window keeps its
                        // geometry, its z-order and its place in the
                        // cycling order, and `Alt+Tab` brings it back.
                        // See `wm::WindowManager::demote`.
                        Region::Minimize => self.set_state(window, WindowState::Minimized),
                        _ => {}
                    }
                }
            }
            // The drag owned the shape while it lasted (a `move` cross, or
            // the grabbed edges'), and the pointer is very likely nowhere
            // near the frame any more. Re-derive it from what is actually
            // under the pointer *now*, or a release over the bare desktop
            // leaves the move cross sitting there until the next motion
            // event — which, if the user lets go and does not move, is
            // indefinitely.
            self.update_cursor_shape();

            self.note_input(time_ns);
            return;
        }

        let mods = self
            .keyboard
            .as_ref()
            .map_or_else(Mods::default, Keyboard::named_mods);
        // Neither server-side drag may start mid-grab: a second button
        // pressed over another window's title bar belongs to the grabbing
        // window, which is where the ordinary delivery below sends it.
        if state == ButtonState::Pressed
            && self.pointer.grab.is_none()
            && let Some(point) = self.frame_point()
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
                    Region::Close | Region::Maximize | Region::Minimize => {
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

        // The implicit grab owns the release (and any further press) even
        // once the pointer has left the window it began on.
        let Some(window) = self.pointer.grab.or(self.pointer.over) else {
            // A click on nothing changes nothing: the desktop is not a
            // focus target, so the keyboard stays where it was. Dropping
            // focus here would leave a screen full of windows and nowhere
            // for keys to go until the next Alt+Tab — `docs/wm.md` is
            // explicit that focus is only ever handed on, never dropped.
            return;
        };
        self.deliver_button(window, button, state, time_ns);
    }

    /// The ordinary delivery of a button event to `window`, and the
    /// implicit grab's bookkeeping around it: a delivered press with no
    /// button held begins one, the release of the last button ends it.
    fn deliver_button(&mut self, window: WindowKey, button: u32, state: ButtonState, time_ns: u64) {
        if state == ButtonState::Pressed && button == input::BTN_LEFT && self.pointer.grab.is_none()
        {
            self.raise_and_focus(window);
        }
        let sent_to = self.send_input(window, |id| {
            ServerMsg::PointerButton(msg::PointerButton {
                window: id,
                button,
                state,
                time_ns,
            })
        });
        self.note_client_input(sent_to);
        // A press that was actually delivered begins the grab — not one
        // a lock kept from an unadmitted window, which would otherwise
        // hold the pointer for a client that never saw it.
        if state == ButtonState::Pressed && self.pointer.grab.is_none() && sent_to.is_some() {
            self.pointer.grab = Some(window);
        }
        if state == ButtonState::Released && !self.pointer.any_button_down() {
            self.end_pointer_grab();
        }
        self.note_input(time_ns);
    }

    /// End the implicit pointer grab, if one is held.
    ///
    /// Focus is not re-derived here: the pointer may be over another
    /// window, or the desktop, and the leave/enter that says so goes out
    /// from [`Server::refresh_pointer_over`] on the next settle — the same
    /// stationary re-check a popup mapping under a still pointer uses,
    /// because a grab ending is exactly that: what the pointer is over
    /// changed without it moving. The cursor is re-derived the same way.
    fn end_pointer_grab(&mut self) {
        if self.pointer.grab.take().is_some() {
            self.pointer_refresh = true;
            self.cursor_stale = true;
        }
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

    /// The window a client-initiated drag (`StartMove`/`StartResize`)
    /// names, if the request is authorized. `None` means "ignore it" —
    /// silently, the client survives.
    ///
    /// Silent rather than `Error { Protocol }` because every error in this
    /// protocol is fatal, and each check below is one a well-behaved
    /// client can fail by losing a race it cannot see: pointer focus moves
    /// with no round trip, and the canonical sequence — `PointerButton`
    /// pressed, the client decides, `StartMove` — meets a user who let go
    /// in between. An id the client does not own is not a race, but it is
    /// ignored too rather than answered with `UnknownNode`: the op is an
    /// advisory hint about a gesture, with no serial to blame, and one rule
    /// ("if it cannot be honoured it is dropped") is smaller than two.
    ///
    /// The checks, cheapest and likeliest-to-fail first:
    ///
    /// 1. no drag is already in flight — one is never restarted;
    /// 2. a pointer button is actually down — what stops a client starting
    ///    an unprovoked drag that hijacks the pointer;
    /// 3. the window is one of this client's own;
    /// 4. the client holds pointer focus: the pointer is over **any** of
    ///    its windows, not necessarily the one named — a client may start
    ///    a move of its main window from a press on another of its own
    ///    surfaces, as Wayland allows.
    fn drag_request(&self, token: u64, id: NodeId, what: &str) -> Option<WindowKey> {
        if self.wm.drag().is_some() || self.dnd_grabbing() {
            debug!("{what}: a drag is already in flight: ignored");
            return None;
        }
        if !self.pointer.any_button_down() {
            debug!("{what}: no pointer button is down: ignored");
            return None;
        }
        let client = self.wire_clients.get(&token)?;
        let Some(win) = client.windows.get(&id).copied() else {
            debug!("{what}: {id:?} is not one of this client's windows: ignored");
            return None;
        };
        if !self.pointer.over.is_some_and(|w| client.owns_window(w)) {
            debug!("{what}: the client does not hold pointer focus: ignored");
            return None;
        }
        Some(win)
    }

    /// `StartMove` (M5-F): a client asking the server to begin a move drag
    /// of one of its windows, typically a client-side-decorated window
    /// whose title bar the user just grabbed. Always returns `true`: see
    /// [`Server::drag_request`] for why a refusal is silent.
    fn start_move(&mut self, token: u64, id: NodeId) -> bool {
        // The client has answered the press, whatever becomes of the
        // request: release a flip held for it, as `SetCursor` does.
        self.defer.forget(token);
        let Some(win) = self.drag_request(token, id, "StartMove") else {
            return true;
        };
        let Some(point) = self.pointer_desktop() else {
            return true;
        };
        self.raise_and_focus(win);
        self.begin_move(win, point);
        self.begin_client_drag();
        true
    }

    /// `StartResize` (M5-F): as [`Server::start_move`], for a resize from
    /// the named edges. `0` lets the server pick the corner nearest the
    /// pointer, exactly as `Super`+right-drag does.
    ///
    /// A mask with reserved bits drops the whole request — unlike
    /// `SetAnchor`, which answers them with `Protocol`: that is a commit
    /// mutation with a serial to blame, this is advisory, and a reserved
    /// bit is most plausibly a newer toolkit. Masking the bit off instead
    /// would hand a client that meant `TOP|<future>` a plain `TOP` drag
    /// it did not ask for. `LEFT|RIGHT` and `TOP|BOTTOM` are dropped for
    /// the same reason (see [`Edges::from_wire`]). A `FIXED_SIZE` window
    /// refuses inside `begin_resize`, silently, as it refuses `Maximized`.
    fn start_resize(&mut self, token: u64, id: NodeId, edges: u8) -> bool {
        self.defer.forget(token);
        let wanted = if edges == 0 {
            None
        } else if let Some(e) = Edges::from_wire(edges) {
            Some(e)
        } else {
            debug!("StartResize: edges {edges:#x} name no resize: ignored");
            return true;
        };
        let Some(win) = self.drag_request(token, id, "StartResize") else {
            return true;
        };
        let Some(point) = self.pointer_desktop() else {
            return true;
        };
        self.raise_and_focus(win);
        match wanted {
            Some(e) => self.begin_resize(win, e, point),
            None => self.begin_corner_resize(win, point),
        }
        self.begin_client_drag();
        true
    }

    /// The tail both client-initiated drags share: show the drag's shape
    /// on the press rather than on the first motion, and stamp the input.
    ///
    /// Reads the drag back rather than assuming one began: `begin_resize`
    /// refuses a window that is not resizable without saying so, and a
    /// shape for a drag that does not exist would stick until the next
    /// motion. The stamp uses the monotonic clock input events carry, so
    /// latency accounting attributes the drag's first frame to the request
    /// that caused it.
    ///
    /// It also takes pointer focus away from the client with a
    /// `PointerLeave`, as a Wayland move/resize grab does. Unlike a frame
    /// drag, the client *saw* the press — that is what it answered — but
    /// the drag swallows every motion and the release, so without a leave
    /// it would believe the button held for ever, and its next ordinary
    /// motion would look like a drag with no press behind it. Focus is
    /// dropped to `None`, so the first motion after the release re-derives
    /// it and sends a fresh `PointerEnter`. A live `SetCursor` request goes
    /// with it, by `set_pointer_over`'s rule: one period of focus ended.
    fn begin_client_drag(&mut self) {
        let Some(drag) = self.wm.drag() else {
            debug!("client drag request: the window refused it: ignored");
            return;
        };
        let now = monotonic_ns();
        if let Some(left) = self.pointer.over {
            let sent_to = self.send_input(left, |id| {
                ServerMsg::PointerLeave(msg::PointerLeave {
                    window: id,
                    time_ns: now,
                })
            });
            self.note_client_input(sent_to);
        }
        // The drag owns the pointer now, implicit grab included: the
        // release ends the drag and is never delivered.
        self.pointer.grab = None;
        self.set_pointer_over(None);
        self.set_cursor(Some(Self::drag_shape(drag)));
        self.note_input(now);
    }

    /// Super+Down: take a `Maximized` or `Fullscreen` window back to
    /// `Normal` — the rectangle `set_state` remembered when it grew, which
    /// for a tiled window is its half of the work area and not its original
    /// size. A `Normal` or `Minimized` window is left alone: Super+Down is
    /// the other half of the Super+Up cycle, never a minimize.
    fn unmaximize(&mut self, win: WindowKey) {
        if self
            .scene
            .window_info(win)
            .is_ok_and(|i| matches!(i.state(), WindowState::Maximized | WindowState::Fullscreen))
        {
            self.set_state(win, WindowState::Normal);
        }
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

    /// One key event: route it, then tell the keyboard recipient what the
    /// modifiers became.
    ///
    /// The sync is here rather than at the end of [`Server::route_key`]
    /// because that function returns early on every path that consumes
    /// the key (compositor hotkey, popup Escape, shell binding, the Alt
    /// release ending a cycle, no recipient, withheld) — and `kb.key()`
    /// has already moved the xkb state on each of them. No path skips it:
    /// the masks describe the physical keyboard, and a client whose
    /// `xkb_state` missed the Ctrl of a swallowed Ctrl+Alt+F1 is the
    /// stuck-modifier bug `Modifiers` exists to prevent.
    ///
    /// Ordering: a `Key`'s `keysym`/`utf8` are resolved against the state
    /// **before** the event, its `mods` (and the `Modifiers` that follow)
    /// describe the state **after** it (`Keyboard::key`). `Modifiers`
    /// follows the `Key` — Wayland's order — so a client evaluating the
    /// keycode through its own `xkb_state` does so before applying the
    /// change the key caused, and then converges on the same post-event
    /// masks the server holds. The recipient is computed *after* routing,
    /// because routing can move focus (Alt+Tab, a popup dismiss).
    fn key(&mut self, keycode: u32, pressed: bool, time_ns: u64) {
        self.route_key(keycode, pressed, time_ns);
        self.sync_modifiers();
    }

    fn route_key(&mut self, keycode: u32, pressed: bool, time_ns: u64) {
        // Without a keymap the key still reaches the focused client, with
        // no keysym and no text: the evdev code is the part that never
        // depends on xkb, and a client that only wants raw keys still works.
        let resolved = self
            .keyboard
            .as_mut()
            .map_or_else(keyboard::KeyResolution::none, |kb| kb.key(keycode, pressed));
        // Repeat bookkeeping, before any early return below. A release of
        // the repeating key ends it; so does the press of any other key
        // that is not a modifier (the new key takes over the repeat if it
        // is delivered, at the end of this function — and if it was a
        // hotkey or a shell binding, nothing repeats). A modifier press
        // leaves it running, so holding `a` and adding Shift repeats `A`.
        let repeating = self.key_repeat.held().map(|h| h.keycode);
        if (!pressed && repeating == Some(keycode))
            || (pressed && keyboard::mod_of_keysym(resolved.keysym).is_none())
        {
            self.stop_key_repeat();
        }
        if pressed && let Some(hotkey) = keyboard::hotkey(resolved.keysym, resolved.named) {
            // While locked the only compositor chord is a VT switch: it
            // leaves this session locked behind it, and it is how a user
            // reaches a text console when the lock screen is broken. The
            // rest (close, maximize, Alt+Tab, quit) act on windows nobody
            // may touch until the unlock, and are swallowed.
            if !self.lock.is_locked() || matches!(hotkey, keyboard::Hotkey::SwitchVt(_)) {
                self.hotkey(hotkey);
            }
            // A hotkey is the compositor's, not the client's.
            self.note_input(time_ns);
            return;
        }
        // The grabs own Escape (`Server::escape_grabs`): after the
        // compositor's own table (which is not negotiable), before the
        // shell's bindings (a live grab outranks a shell hotkey for the
        // reason it outranks focus).
        if keyboard::is_escape(resolved.keysym) && self.escape_grabs(pressed, time_ns) {
            return;
        }
        // A shell's own bindings come next: after the compositor's, which are
        // not negotiable, and before any client's, because a global hotkey
        // the focused application could also see would be both a keylogger
        // and an ambiguity. `HotKeys::key` is fed every key, hotkey or not,
        // because the bare-modifier tap is decided by what did *not* happen
        // while a modifier was held.
        // While locked, the shell's bindings are not consulted at all: a
        // launcher opened over a lock screen would be a way past it. The
        // tap state machine is reset on both edges of the lock instead of
        // being fed here.
        let fired = if self.lock.is_locked() {
            Vec::new()
        } else {
            self.hotkeys.key(resolved.keysym, pressed, resolved.named)
        };
        if !fired.is_empty() {
            let mut answered_by = None;
            for (binding, down) in fired {
                if let Some(client) = self.wire_clients.get_mut(&binding.token) {
                    client.send(&ServerMsg::HotKey(msg::HotKey {
                        id: binding.id,
                        pressed: down,
                        time_ns,
                    }));
                    answered_by = Some(binding.token);
                }
            }
            // The shell now owes us an answer, and it is a round trip away:
            // write, wake, build the tree, commit. Every key the user types
            // in that gap would otherwise be routed by focus — into
            // whatever application happened to be focused, which is how a
            // query beginning with `q` once quit the calculator. Hold the
            // keyboard until the shell has had its turn. See
            // `Server::withheld`.
            if let Some(token) = answered_by {
                self.hotkey_pending = Some((token, Instant::now() + HOTKEY_ANSWER));
            }
            self.note_input(time_ns);
            return;
        }
        // Releasing Alt ends an `Alt+Tab` cycle: the window it landed on
        // is already raised (each Tab raises), and now it becomes the most
        // recently used, so the *next* Alt+Tab starts from there. The raise
        // here is idempotent and kept for safety.
        if !pressed && keyboard::is_alt(resolved.keysym) && self.wm.cycling() {
            self.wm.end_cycle();
            if let Some(win) = self.focus {
                self.wm.touch(win);
                if let Err(e) = self.scene.raise(win) {
                    warn!("raise: {e}");
                }
                self.pointer_refresh = true;
            }
            self.note_input(time_ns);
            return;
        }
        // A keyboard grab wins over focus: it is how a `NO_FOCUS` overlay
        // reads the keyboard without taking focus away, so the window that
        // was focused stays focused and keeps its active frame.
        //
        // While locked, only a window the lock admits may be either: a grab
        // or a focus left on anyone else's window is skipped, not honoured.
        let grab = self.grab_target().filter(|w| self.scene.admits_window(*w));
        let focus = self.focus.filter(|w| self.scene.admits_window(*w));
        let Some(window) = grab.or(focus) else {
            return;
        };
        // ...unless a shell's hotkey just fired and it has not answered
        // yet: this key is not for whoever is merely still focused.
        if self.withheld(window) {
            self.keys_withheld += 1;
            self.note_input(time_ns);
            return;
        }
        let state = if pressed {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        };
        let utf8 = resolved.utf8.clone();
        let (keysym, mods) = (resolved.keysym, resolved.mods);
        let sent_to = self.send_input(window, |id| {
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
        if pressed && let Some(token) = sent_to {
            self.start_key_repeat(keycode, window, token);
        }
        self.note_client_input(sent_to);
        self.note_input(time_ns);
    }

    /// Start repeating a press that was just delivered to `window`'s
    /// client `token`, if it should repeat. See [`repeat`] for the rules.
    ///
    /// Not for a `KEYMAP` client: it evaluates keys itself and repeats on
    /// its own from `Keymap`'s `rate_hz`/`delay_ms`, exactly as a Wayland
    /// client does, so a server repeat would type every key twice. And
    /// not without a keymap: xkb is what says which keys repeat.
    fn start_key_repeat(&mut self, keycode: u32, window: WindowKey, token: u64) {
        let Some(kb) = self.keyboard.as_ref() else {
            return;
        };
        if !kb.repeats(keycode) {
            return;
        }
        if self
            .wire_clients
            .get(&token)
            .is_none_or(|c| c.client_caps & nitro_wire::types::caps::KEYMAP != 0)
        {
            return;
        }
        let repeat = self.settings.keyboard.repeat();
        if let Err(e) = self
            .key_repeat
            .start(keycode, window, monotonic_ns(), repeat)
        {
            warn!("key repeat: arm: {e}");
        }
    }

    /// Stop any key repeat. Cheap when nothing is repeating.
    fn stop_key_repeat(&mut self) {
        if let Err(e) = self.key_repeat.stop() {
            warn!("key repeat: disarm: {e}");
        }
    }

    /// The repeat timer fired: send the held key again.
    ///
    /// Re-checks that the window the press went to is still the one keys
    /// go to — grab, focus, lock, a shell's pending hotkey — and stops
    /// instead of sending if it is not. Every path that moves the
    /// recipient already stops the repeat; this is the backstop for the
    /// ones that do so lazily (a grab on a window that stopped showing),
    /// and it covers the session going inactive too.
    ///
    /// The key is re-resolved against the **current** modifier state
    /// ([`Keyboard::resolve_held`]), and its `time_ns` is now: a repeat
    /// is a new event, not a replay of the old one.
    fn on_key_repeat(&mut self) {
        let now = monotonic_ns();
        let held = match self.key_repeat.fire(now, self.settings.keyboard.repeat()) {
            Ok(Some(held)) => held,
            Ok(None) => return,
            Err(e) => {
                warn!("key repeat: re-arm: {e}");
                return;
            }
        };
        let grab = self.grab_target().filter(|w| self.scene.admits_window(*w));
        let focus = self.focus.filter(|w| self.scene.admits_window(*w));
        // Inactive (a VT switch): the `Disable` arm already stopped it;
        // this is the half that catches a missed cancel.
        if !self.active || grab.or(focus) != Some(held.window) || self.withheld(held.window) {
            self.stop_key_repeat();
            return;
        }
        let Some(resolved) = self
            .keyboard
            .as_ref()
            .map(|kb| kb.resolve_held(held.keycode))
        else {
            self.stop_key_repeat();
            return;
        };
        let sent_to = self.send_input(held.window, |id| {
            ServerMsg::Key(msg::Key {
                window: id,
                keycode: held.keycode,
                state: ButtonState::Pressed,
                mods: resolved.mods,
                keysym: resolved.keysym,
                time_ns: now,
                utf8: resolved.utf8.clone(),
            })
        });
        if sent_to.is_none() {
            // The client went away under the key.
            self.stop_key_repeat();
            return;
        }
        self.note_client_input(sent_to);
        self.note_input(now);
        self.flush_wire_clients();
        self.settle();
    }

    /// Act on one compositor hotkey.
    ///
    /// Every window-management chord acts on the *focused* window, which
    /// is the one thing the user can always see; a chord with nothing
    /// focused is a no-op rather than a guess.
    fn hotkey(&mut self, hotkey: Hotkey) {
        // Every window-management chord leaves the overview first: Super+M
        // on a scaled thumbnail would maximize a window drawn at a quarter
        // of its size, with no decorations. Quitting and VT switching do
        // not touch windows, and are left alone.
        if !matches!(hotkey, Hotkey::Quit | Hotkey::SwitchVt(_)) {
            self.leave_overview(None);
        }
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
            Hotkey::Maximize => {
                if let Some(win) = self.focus {
                    self.set_state(win, WindowState::Maximized);
                }
            }
            Hotkey::Restore => {
                if let Some(win) = self.focus {
                    self.unmaximize(win);
                }
            }
        }
    }

    /// Walk the MRU order one step. The focus moves and the window is
    /// raised at once — so the user sees where they are — but the MRU list
    /// is only reordered when Alt comes up, which is what makes repeated
    /// Tabs walk further back instead of bouncing between two windows.
    /// Windows walked past stay raised in walk order; the pre-cycle
    /// stacking is not restored.
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
        if let Err(e) = self.scene.raise(win) {
            warn!("raise: {e}");
        }
        self.pointer_refresh = true;
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
        // A thumbnail's nodes died with its subtree; its caption run did
        // not, and the grid has a hole. Drop it, then lay out again.
        if let Some(ov) = self.wm.overview_mut()
            && let Some(i) = ov.thumbs.iter().position(|t| t.window == win)
        {
            ov.thumbs.remove(i);
            self.relayout_overview();
        }
        self.drag_icons.remove(&win);
        self.drag_icon_offsets.remove(&win);
        self.dnd_step(|d| d.forget_window(win));
        self.decorations.remove(&win);
        if self.resize_hint == Some(win) {
            self.resize_hint = None;
        }
        if self.button_hover.is_some_and(|(w, _)| w == win) {
            // A dead window's button is not hovered. Cleared rather than
            // left to the next motion, because nothing guarantees there
            // is one: a window closed under a stationary pointer would
            // leave the hover pointing at a key the scene has destroyed,
            // and the next frame to gain that key would light up.
            self.button_hover = None;
        }
        let title = self.frame_titles.remove(&win);
        self.text.release(title);
        self.wm.remove(win);
        // Everything the shell knew about this window goes with it: its
        // exclusive zone (or the desktop would stay short of the strip a
        // dead bar reserved), its anchor, its grab and its server-global id.
        let had_zone = !self.zones.is_empty();
        self.zones.forget(win);
        if self.grab == Some(win) {
            self.grab = None;
        }
        self.notify_window_gone(win);
        if had_zone {
            self.work_area_changed();
        }
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

    /// Drop everything a *closed* window left behind: what
    /// [`Server::forget_window`] forgets, plus the pointer state that
    /// only a commit's `closed_windows` list can reach.
    fn forget_closed(&mut self, win: WindowKey) {
        if self.focus == Some(win) {
            self.focus = None;
        }
        if self.pointer.over == Some(win) {
            self.set_pointer_over(None);
        }
        if self.pointer.grab == Some(win) {
            self.end_pointer_grab();
        }
        self.touch_targets.retain(|_, (w, _)| *w != win);
        self.popup_window_gone(win);
        self.forget_window(win);
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
                if let Some(out) = output
                    && Some(out) == self.overview_output()
                    && input::overview_hit(&self.scene, out, point).is_none()
                {
                    // A touch on a thumbnail or the scrim is a click.
                    self.overview_click(out, point);
                    self.note_input(time_ns);
                    return;
                }
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
        let sent_to = self.send_input(win, |window| {
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

    /// Damage the rectangle a cursor with its hotspot at `(x, y)` global
    /// device pixels covers, on every output it touches.
    ///
    /// This is the server's *only* non-scene damage: the software cursor,
    /// and nothing else. Scene damage goes through
    /// [`Server::update_scene`], which is what keeps
    /// [`frame::OutputState::cursor_only`] able to tell them apart.
    ///
    /// The covered rectangle is computed **per output** rather than once
    /// globally, because the cursor is magnified by each output's own
    /// whole scale factor ([`Cursor::paint_scale`]): on a 2× screen it is
    /// 48 device pixels square and on its 1× neighbour 24. One global rect
    /// would have to be the larger of the two and would over-damage the
    /// smaller screen on every motion — on the motion path, which is the
    /// one `docs/budget.md` is strict about.
    fn damage_cursor_at(&mut self, x: i32, y: i32, shape: crate::cursor::Shape) {
        for output in &mut self.outputs {
            let Some((origin, scale)) = self.scene.output_info(output.scene_id) else {
                continue;
            };
            let rect = Cursor::rect_scaled(x, y, shape, Cursor::paint_scale(scale));
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

    /// [`Server::send_to_window`] for **input**: pointer, keys, scroll and
    /// touch. A window the session lock does not admit receives none.
    ///
    /// Separate from `send_to_window` on purpose. That one also carries
    /// `Configure`-like news a hidden client still needs (`WindowState`,
    /// `Closed`); input is the one kind that must stop at the lock.
    fn send_input<F>(&mut self, win: WindowKey, build: F) -> Option<u64>
    where
        F: Fn(NodeId) -> ServerMsg,
    {
        if !self.scene.admits_window(win) {
            return None;
        }
        self.send_to_window(win, build)
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
        // The one gate every focus path goes through: a click, Alt+Tab, a
        // new window, the MRU hand-off, the shell's `FocusWindow` and the
        // control socket's `focus`. While locked, a window the lock does
        // not admit cannot take the keyboard.
        if let Some(w) = window
            && !self.scene.admits_window(w)
        {
            return;
        }
        let old = self.focus;
        // Moved before the old window's restyle: `retitle` colours the
        // title by `self.focus`, so restyling first re-shaped the window
        // losing focus in the *focused* colour.
        self.focus = window;
        if let Some(old) = old {
            self.send_to_window(old, |id| {
                ServerMsg::Focus(msg::Focus {
                    window: id,
                    focused: false,
                })
            });
            self.restyle(old, false);
        }
        self.wm.set_focus(window);
        // A key held while focus moves must not keep typing into the
        // window that just got it — the classic stuck-key bug.
        self.stop_key_repeat();
        if let Some(new) = window {
            self.send_to_window(new, |id| {
                ServerMsg::Focus(msg::Focus {
                    window: id,
                    focused: true,
                })
            });
            self.restyle(new, true);
        }
        // The shell's window list carries `focused`, so both ends of the
        // change are announced — a bar highlighting the active window needs
        // to un-highlight the old one.
        if let Some(old) = old {
            self.notify_window(old);
        }
        if let Some(new) = window {
            self.notify_window(new);
        }
        // The new recipient needs the current masks before its first key;
        // the old one's cache is dropped so it is re-told when it returns.
        self.sync_modifiers();
    }

    /// The client that keyboard input currently goes to: the grab holder,
    /// else the focused window's owner, each only if the session lock
    /// admits it — exactly the recipient `route_key` picks.
    fn keyboard_recipient(&mut self) -> Option<u64> {
        let grab = self.grab_target().filter(|w| self.scene.admits_window(*w));
        let focus = self.focus.filter(|w| self.scene.admits_window(*w));
        let window = grab.or(focus)?;
        self.wire_clients
            .iter()
            .find(|(_, c)| c.owns_window(window))
            .map(|(t, _)| *t)
    }

    /// Send `Modifiers` to the keyboard recipient if its masks moved.
    ///
    /// **Only** the recipient: an unfocused client streaming which
    /// modifiers are held while the user types elsewhere would be half a
    /// keylogger (`wl_keyboard.modifiers` has the same rule). Every other
    /// client that was told something has its cache cleared, so it gets a
    /// fresh snapshot the moment it becomes the recipient again.
    fn sync_modifiers(&mut self) {
        let recipient = self.keyboard_recipient();
        for (token, client) in &mut self.wire_clients {
            if Some(*token) != recipient {
                client.last_mods = None;
            }
        }
        if let Some(token) = recipient {
            self.send_modifiers_to(token);
        }
    }

    /// Send `Modifiers` to one `KEYMAP` client, unless it already has
    /// exactly these masks.
    fn send_modifiers_to(&mut self, token: u64) {
        let masks = self
            .keyboard
            .as_ref()
            .map(Keyboard::mod_masks)
            .unwrap_or_default();
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        if !client.stream.is_ready()
            || client.client_caps & nitro_wire::types::caps::KEYMAP == 0
            || client.last_mods == Some(masks)
        {
            return;
        }
        client.send(&ServerMsg::Modifiers(msg::Modifiers {
            depressed: masks.depressed,
            latched: masks.latched,
            locked: masks.locked,
            group: masks.group,
        }));
        client.last_mods = Some(masks);
    }

    /// Send the current `Keymap` to one client, then a `Modifiers`
    /// snapshot ("after every `Keymap`", `docs/wire.md`) — the one
    /// exception to recipient-only masks, a single message rather than a
    /// stream.
    ///
    /// Skipped for a client that has not finished its handshake (the
    /// `set_palette` race) or did not list `KEYMAP` (capability opt-in
    /// rule 1).
    fn send_keymap(&mut self, token: u64) {
        let (rate_hz, delay_ms) = self.repeat_advice();
        let Some(km) = self.keymap_fd.as_ref() else {
            return;
        };
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        if !client.stream.is_ready() || client.client_caps & nitro_wire::types::caps::KEYMAP == 0 {
            return;
        }
        // `encode_body` dups the descriptor again for the socket; this dup
        // only lends the message an owned fd. The file is write-sealed,
        // so every client sharing it is safe (`Keyboard::export`).
        let fd = match rustix::io::dup(km.fd.as_fd()) {
            Ok(fd) => fd,
            Err(e) => {
                warn!("dup keymap fd: {e}");
                return;
            }
        };
        client.send(&ServerMsg::Keymap(msg::Keymap {
            format: nitro_wire::types::KeymapFormat::XkbV1,
            size: km.size,
            rate_hz,
            delay_ms,
            fd,
        }));
        client.last_mods = None;
        self.send_modifiers_to(token);
    }

    /// The key-repeat figures `Keymap` carries: `keyboard.repeat` (or its
    /// default), which a `KEYMAP` client uses for its *own* repeat because
    /// the server does not repeat into it (`docs/wire.md` § `Keymap`).
    /// `0, 0` when repeat is off.
    fn repeat_advice(&self) -> (u32, u32) {
        let r = self.settings.keyboard.repeat();
        if r.enabled() {
            (r.rate_hz, r.delay_ms)
        } else {
            (0, 0)
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
        let nodes = match wm::build_frame(&mut self.scene, win, fixed, &self.palette) {
            Ok(n) => n,
            Err(e) => {
                warn!("building a frame: {e}");
                return;
            }
        };
        self.decorations.insert(win, nodes);
        self.restyle(win, self.focus == Some(win));
        self.reicon(win);
    }

    /// Point a frame's application-icon node at whatever the window's
    /// `app_id` resolves to, or at the `window` fallback.
    ///
    /// The frame is the **server's own** tree, so there is no `SetIcon`
    /// and no `BadIcon`: the fallback happens here, synchronously, in the
    /// same call that failed to resolve. A client has to be told its name
    /// failed and send another message; the server is the resolver, so
    /// the node is never briefly blank.
    ///
    /// Called on `decorate` and again whenever `SetAppId` moves — the
    /// protocol allows a client to change its app id after mapping
    /// (`docs/wire.md`), and `nitro-term` does exactly that when it
    /// learns what it is running — so the icon has to be re-resolved
    /// rather than resolved once and remembered.
    fn reicon(&mut self, win: WindowKey) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        let Ok(app_id) = self.scene.window_info(win).map(|i| i.app_id().to_owned()) else {
            return;
        };
        // The full three-step resolution — theme, then `<app_id>.desktop`
        // `Icon=`, then that name symbolic-or-theme — which is what makes
        // `nitro-calc` a calculator rather than a generic window.
        //
        // **Both branches take the title's role**, and that is the whole
        // of the tint decision for this node. `AppIcon::role` ignores the
        // argument for a theme PNG (a picture has no tint) and takes it
        // for one of our coverage masks, so the one expression is correct
        // for both — and a symbolic icon reached through the hop recedes
        // with the title exactly as the fallback does.
        //
        // An earlier cut passed `Role::Text` here and only the fallback
        // branch the title role, which left a hop-resolved icon at full
        // strength on an *unfocused* frame until some later `style_only`
        // corrected it. `reicon` runs on `SetAppId` and on every
        // decoration at `reload`, neither of which is a focus change, so
        // nothing was guaranteed to follow.
        let tint = wm::title_role(self.focus == Some(win));
        let resolved = self
            .icons
            .lookup_app(&app_id)
            .map(|icon| (icon.handle(), icon.role(tint)))
            .or_else(|| {
                // Nothing anywhere: one of ours, tinted like the title.
                self.icons
                    .lookup(wm::icon_names::FALLBACK_APP)
                    .map(|handle| (handle, wm::role_byte(tint)))
            });
        if let Err(e) = wm::set_app_icon(&mut self.scene, &nodes, resolved) {
            warn!("setting a frame icon: {e}");
        }
    }

    /// Restyle a window's frame for a focus change, and re-shape its title
    /// in the matching colour.
    fn restyle(&mut self, win: WindowKey, focused: bool) {
        self.style_only(win, focused);
        self.retitle(win);
    }

    /// The colours of a frame, and **only** the colours: no title.
    ///
    /// Split out of [`Server::restyle`] because the resize hint runs on the
    /// *motion* path, and `retitle` is not cheap: it elides (a binary
    /// search of `measure` calls), shapes, inserts a fresh run in the text
    /// store and releases the old one — and because the `TextKey` is new
    /// every time, `set_text`'s "same run" early-out never fires and the
    /// title node is marked `Dirty::PAINT` on every call.
    ///
    /// Nothing about the hint changes the title: [`wm::title_color`]
    /// depends on `focused` alone. So paying all of that whenever a pointer
    /// crosses a window edge would be pure waste, and it would land in
    /// `shape_us` — polluting the statistic `docs/latency.md` points a
    /// reader at to find real shaping costs. Since #3715 the button
    /// hover rides the same path and inherits the same guarantee.
    fn style_only(&mut self, win: WindowKey, focused: bool) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        let hint = self.resize_hint == Some(win);
        let hover = self.button_hover.filter(|(w, _)| *w == win).map(|(_, r)| r);
        if let Err(e) =
            wm::style_frame(&mut self.scene, &nodes, focused, hint, hover, &self.palette)
        {
            warn!("styling a frame: {e}");
        }
        // A symbolic app-icon fallback is tinted like the title, so it
        // has to follow the focus with it. A resolved application icon is
        // a picture and is left alone — an unfocused Firefox logo is
        // still the Firefox logo.
        self.restyle_fallback_icon(win, focused);
    }

    /// Retint a frame's app icon **if** it is one of our coverage masks.
    ///
    /// The discriminator is the node's own stored role byte rather than a
    /// flag beside it: `AS_COLOURED` means the tile is somebody else's
    /// artwork and has no tint to change, and anything else is one of our
    /// coverage masks — whether it got there as the `window` fallback or
    /// through the `.desktop` hop, which are the same kind of thing and
    /// must recede with the title alike. One record, so nothing can
    /// disagree with it.
    ///
    /// [`Server::reicon`] already stores the right role, so on a freshly
    /// resolved icon this is a no-op that returns at the `icon.role ==
    /// want` line. What it exists for is the *later* focus change, where
    /// the node is correct for the old focus and nothing else would
    /// touch it.
    fn restyle_fallback_icon(&mut self, win: WindowKey, focused: bool) {
        let Some(nodes) = self.decorations.get(&win).copied() else {
            return;
        };
        let Some(icon) = self
            .scene
            .node(nodes.app_icon)
            .ok()
            .and_then(nitro_scene::Node::icon)
        else {
            return;
        };
        if icon.role == nitro_scene::IconRef::AS_COLOURED {
            return;
        }
        let want = wm::role_byte(wm::title_role(focused));
        if icon.role == want {
            return;
        }
        if let Err(e) = wm::set_app_icon(&mut self.scene, &nodes, Some((icon.icon, want))) {
            warn!("retinting a frame icon: {e}");
        }
    }

    /// Light the frame edge of whichever window the pointer could resize by
    /// pressing right now, and put the previous one back.
    ///
    /// The resize band is six logical pixels wide and the border it
    /// straddles is one, so a user aiming at the border they can see has
    /// nothing telling them whether they are in it — which is #3713's
    /// second half, reported as "resizing does not work (by grabbing a
    /// border)". Since #3724 the cursor takes the band's shape too, and
    /// the border keeps lighting up beside it: the shape says "a press
    /// here resizes", the lit edge says *which* edge, and a symmetric
    /// double arrow cannot say the second. `docs/wm.md` has the argument.
    ///
    /// Called from every motion, so it is written to do nothing in the
    /// common case: the hit test it needs has already been run by the
    /// caller, and an unchanged hint returns before touching the scene.
    /// Only a window that is actually [`Server::resizable`] lights up —
    /// offering a grab that would do nothing is worse than offering none.
    fn set_resize_hint(&mut self, hint: Option<WindowKey>) {
        let hint = hint.filter(|w| self.resizable(*w));
        if self.resize_hint == hint {
            return;
        }
        let old = self.resize_hint;
        self.resize_hint = hint;
        for win in [old, hint].into_iter().flatten() {
            // Colours only: a hover must not re-shape a title that cannot
            // have changed. See [`Server::style_only`], and
            // `the_frame_edge_lights_up_where_a_press_would_resize_it`,
            // which fails on `text_layouts` if this becomes `restyle`.
            self.style_only(win, self.focus == Some(win));
        }
    }

    /// Light the disc under whichever title-bar button the pointer is on,
    /// and put the previous one back.
    ///
    /// The twin of [`Server::set_resize_hint`], on the same motion path,
    /// written to the same rules and for the same reason: the frame's
    /// buttons are bare symbolic glyphs since #3715, and a symbol with no
    /// hover state gives no sign that it is a control at all.
    ///
    /// Three properties it shares, each of which was a review finding on
    /// #3713 before it was a rule here:
    ///
    /// * The hit test is the caller's, and it is the *same* one the
    ///   resize hint uses — one `frame_hit` per motion, not two.
    /// * An unchanged hover returns before touching the scene, so the
    ///   common case (the pointer is over content) is a comparison.
    /// * The restyle is `style_only`, never `restyle`: a hover must not
    ///   re-shape a title that cannot have changed.
    fn set_button_hover(&mut self, hover: Option<(WindowKey, Region)>) {
        if self.button_hover == hover {
            return;
        }
        let old = self.button_hover;
        self.button_hover = hover;
        for win in [old.map(|(w, _)| w), hover.map(|(w, _)| w)]
            .into_iter()
            .flatten()
        {
            self.style_only(win, self.focus == Some(win));
        }
    }

    /// Put the pointer into a cursor shape, damaging what it leaves and
    /// what it takes.
    ///
    /// The third affordance on the motion path, beside
    /// [`Server::set_resize_hint`] and [`Server::set_button_hover`], and
    /// written to the same rules: the hit test is the caller's (the *same*
    /// `frame_hit`), and an unchanged shape returns before touching
    /// anything.
    ///
    /// What it does **not** do is touch the scene. A shape change is
    /// cursor damage and only cursor damage — the old shape's rect ∪ the
    /// new one's, which differ because the hotspots do — so it is not a
    /// restyle, it shapes no text, it re-rasterises no icon, and
    /// [`frame::OutputState::cursor_only`] still calls the frame it causes
    /// a cursor-only flip. That is what
    /// `hovering_a_band_changes_the_cursor_and_nothing_else` pins, and
    /// what `a_client_shape_change_is_cursor_damage_and_nothing_else`
    /// pins for a client's `SetCursor`.
    ///
    /// `None` hides the cursor (a client's `CursorShape::None`). Hiding
    /// damages exactly the old rect and showing exactly the new one — the
    /// same rule, with one side empty.
    fn set_cursor(&mut self, want: Option<crate::cursor::Shape>) {
        if self.cursor_shown == want {
            return;
        }
        let old = self.cursor_shown;
        self.cursor_shown = want;
        if !self.pointer.present {
            // Nothing is drawn, so nothing changed on screen. The shape is
            // still recorded: the first motion that makes the pointer
            // visible paints whatever it should already have been.
            return;
        }
        let (x, y) = self.pointer.device();
        for shape in [old, want].into_iter().flatten() {
            self.damage_cursor_at(x, y, shape);
        }
    }

    /// Point `pointer.over` at a (possibly) different window, dropping a
    /// client's cursor request when it does.
    ///
    /// Every reassignment goes through here, because a `SetCursor` lasts
    /// exactly one continuous period of pointer focus: `wl_pointer`'s own
    /// rule, and the one Chromium already follows by re-applying its
    /// cursor on every enter (`wayland_window.cc`).
    fn set_pointer_over(&mut self, over: Option<WindowKey>) {
        if self.pointer.over != over {
            self.client_cursor = None;
            self.cursor_stale = true;
        }
        self.pointer.over = over;
    }

    /// A client's live cursor request, re-validated against the window
    /// the pointer is on now.
    ///
    /// The outer `Option` is "a live request exists"; the inner is the
    /// request itself, where `None` means "hide". A request recorded for
    /// any window but `pointer.over` answers the outer `None` — the field
    /// is cleared on every focus change anyway, so this is belt and
    /// braces rather than the mechanism.
    // Two distinct absences, deliberately: "no request" falls back to the
    // arrow, "a request to hide" must not. A three-case enum would be this
    // type with new names on it, and the doc comment names the cases.
    #[allow(clippy::option_option)]
    fn requested_cursor(&self) -> Option<Option<crate::cursor::Shape>> {
        let (win, shape) = self.client_cursor?;
        (self.pointer.over == Some(win)).then_some(shape)
    }

    /// `SetCursor` (M5-E): a client choosing the pointer's shape over its
    /// own content. Returns whether the client survives.
    ///
    /// Three steps, in this order:
    ///
    /// 1. **The capability**, fatal: a client that did not list `CURSOR`
    ///    in its `ClientCaps` is `Error { Protocol }` (`docs/wire.md`
    ///    rule 3) — confusion, not a race.
    /// 2. **Release the deferral.** The client has spoken: a `SetCursor`
    ///    with no commit is exactly how a client answers the motion that
    ///    put the pointer over a link, so a flip held for its answer is
    ///    released on `commit`'s reasoning. Before the focus test, because
    ///    a client that just lost focus was very plausibly answering the
    ///    `PointerLeave` that took it; `forget` is per-token, so this
    ///    never releases another client's wait.
    /// 3. **Pointer focus**, silent: a client whose window is not under
    ///    the pointer is ignored, no error — focus can legitimately leave
    ///    between the send and the receipt, and every error is fatal.
    ///
    /// Then the effective cursor is re-derived at once, so the shape shows
    /// without waiting for a motion — through [`Server::cursor_choice`],
    /// so the server's own chrome still wins.
    fn set_cursor_request(&mut self, token: u64, shape: nitro_wire::types::CursorShape) -> bool {
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        let listed = client.client_caps & nitro_wire::types::caps::CURSOR != 0;
        let focused = self.pointer.over.filter(|w| client.owns_window(*w));
        if !listed {
            self.disconnect(
                token,
                Some((
                    0,
                    ErrorCode::Protocol,
                    "SetCursor needs `CURSOR` listed in ClientCaps".to_owned(),
                )),
            );
            return false;
        }
        self.defer.forget(token);
        let Some(win) = focused else {
            debug!("SetCursor from a client without pointer focus: ignored");
            return true;
        };
        self.client_cursor = Some((win, crate::cursor::Shape::from_wire(shape)));
        self.update_cursor_shape();
        true
    }

    /// What the cursor should show, given the frame region under the
    /// pointer; `None` is hidden.
    ///
    /// The precedence, below a drag (which short-circuits before this in
    /// `move_pointer`):
    ///
    /// 1. A band the window can **actually** be resized by takes the shape
    ///    that points along it. `resizable` is the filter
    ///    [`Server::set_resize_hint`] applies, and for the same reason — a
    ///    diagonal cursor over a `FIXED_SIZE` window would promise a grab
    ///    that does nothing.
    /// 2. The client's own content honours the client's request, if it
    ///    has a live one.
    /// 3. Everything else takes the arrow.
    ///
    /// The server's chrome wins over a request because it is the server's
    /// affordance, not the client's: a client cannot know where nitro's
    /// bands are. Decorations are part of the window, so moving off them
    /// back onto the content brings the request back without a re-send.
    ///
    /// Two independent mechanisms make "leaving the window forgets the
    /// shape" true, and both are intended: this function never consults
    /// the request off the content, and [`Server::set_pointer_over`]
    /// drops the stored request outright on a focus change. The second is
    /// what makes re-entry need a re-send; the first is why the desktop
    /// shows an arrow even if the clearing were ever missed.
    ///
    /// One rule for both callers — the motion path (with the hit it
    /// already has, so no second z-order walk) and the release that ends
    /// a drag — because a second copy would be a second thing to get
    /// wrong.
    fn cursor_choice(&self, hit: Option<(WindowKey, Region)>) -> Option<crate::cursor::Shape> {
        use crate::cursor::Shape;
        match hit {
            // The server's own affordance: a band that would really resize.
            Some((win, Region::Resize(edges))) if self.resizable(win) => {
                Some(Shape::for_edges(edges))
            }
            // The client's own content: the one place a request is honoured.
            Some((_, Region::Content)) => self.requested_cursor().unwrap_or(Some(Shape::Arrow)),
            // Everything else takes the arrow: server-drawn chrome (title
            // bar, buttons, a FIXED_SIZE window's band) and the bare
            // desktop.
            //
            // The desktop is `None` here, and it is deliberately *not*
            // routed through `requested_cursor()`: off every window nobody
            // holds pointer focus, so it could only ever re-validate to
            // `None` and answer the arrow anyway.
            _ => Some(Shape::Arrow),
        }
    }

    /// Re-derive the cursor shape from whatever is under the pointer now.
    ///
    /// The motion path does this inline, off the `frame_hit` it already
    /// has. This is for the places that have no hit in hand: a client's
    /// `SetCursor`, and the **release** that ends a drag. A drag owns
    /// the shape while it lasts and the pointer is routinely nowhere near
    /// the frame by the time it ends, so without this a release over the
    /// bare desktop leaves the move cross there until the next motion —
    /// indefinitely, if the user lets go and does not move.
    fn update_cursor_shape(&mut self) {
        if self.dnd_grabbing() {
            self.set_cursor(Some(self.dnd_shape()));
            return;
        }
        let hit = if self.pointer_in_overview() {
            self.pointer.over.map(|w| (w, Region::Content))
        } else {
            self.pointer_desktop().and_then(|p| self.frame_hit(p))
        };
        self.set_cursor(self.cursor_choice(hit));
    }

    /// The shape a drag in flight shows.
    fn drag_shape(drag: Drag) -> crate::cursor::Shape {
        match drag {
            Drag::Move { .. } => crate::cursor::Shape::Move,
            Drag::Resize { edges, .. } => crate::cursor::Shape::for_edges(edges),
            // A button press is not a drag the pointer follows; the
            // pointer is parked on a title-bar button.
            Drag::Button { .. } => crate::cursor::Shape::Arrow,
        }
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
            self.title_size_px(win),
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
            color: wm::title_color(focused, &self.palette),
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
            .or_else(|| self.primary_output());
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
        let area = self.local_work_area(id);
        let origin = self.desktop_origin(id);
        Rect::new(origin.x + area.x, origin.y + area.y, area.w, area.h)
    }

    /// An output's work area in that **output's own** logical space: the
    /// scene's rectangle minus the shell's exclusive zones.
    ///
    /// The subtraction lives here, at the one place the window manager asks
    /// "what may a window use?". Putting it in `wm::work_area` would have
    /// made a pure geometry function need the shell's state and the scene's
    /// window table; putting it at every call site would have let one forget.
    ///
    /// A bar that is not **showing** reserves nothing — see
    /// [`Server::showing`]: hiding a panel has to give its strip back, or a
    /// shell would have to remember to release the zone first and a crashed
    /// one would leave the desktop permanently short.
    fn local_work_area(&self, id: SceneOutputId) -> Rect {
        let area = wm::work_area(&self.scene, id);
        if self.zones.is_empty() {
            return area;
        }
        self.zones.work_area(area, id, |win| {
            if !self.showing(win) {
                return None;
            }
            self.scene
                .window_info(win)
                .ok()
                .and_then(nitro_scene::Window::output)
        })
    }

    /// Whether a window is actually on screen: live, not `Minimized`, and
    /// its content subtree visible.
    ///
    /// The one predicate behind two shell promises that used to be tested
    /// differently, which is how they drifted: an exclusive zone is released
    /// when its bar stops showing, and a keyboard grab is dropped when its
    /// overlay does. Both matter for the same reason — the launcher hides
    /// itself with `SetVisible(false)` on Escape, and a shell that had to
    /// send an explicit release as well would swallow the keyboard, or keep
    /// a strip of the desktop, for the whole session on one forgotten
    /// message.
    ///
    /// `Minimized` is checked as well as node visibility because they are
    /// different things: the scene hides a minimized window's *root* without
    /// touching the client's own `visible` flag on the content group.
    fn showing(&self, win: WindowKey) -> bool {
        self.scene
            .window_info(win)
            .ok()
            .filter(|i| i.state() != WindowState::Minimized)
            .and_then(|i| self.scene.node(i.content()).ok())
            .is_some_and(nitro_scene::Node::visible)
    }

    /// Whether a window's **frame** is on screen, which is what decides
    /// whether it can be grabbed by [`Server::frame_hit`].
    ///
    /// The window's root node, and deliberately not [`Server::showing`]'s
    /// *content* node: for a decorated window the title bar, the border and
    /// the buttons are the content's siblings, so a client that hid its own
    /// content group still has a frame painted on screen and that frame must
    /// still drag, close and resize. `Minimized` needs no separate test —
    /// `Scene::set_window_state` hides the root for exactly that state — but
    /// it is checked anyway, because "is this window on screen" having one
    /// obvious answer is worth more than the branch it costs.
    fn on_screen(&self, win: WindowKey) -> bool {
        self.scene
            .window_info(win)
            .ok()
            .filter(|i| i.state() != WindowState::Minimized)
            .and_then(|i| self.scene.node(i.root()).ok())
            .is_some_and(nitro_scene::Node::visible)
            // A window the session lock hides is not on screen: no title bar
            // to drag, no button to press, no edge to resize.
            && self.scene.admits_window(win)
            // Nor is a panel a fullscreen window covers.
            && !self.scene.is_layer_hidden(win)
    }

    /// Hide the [`nitro_scene::Layer::Top`] of every output whose frontmost window is
    /// fullscreen, and show it again everywhere else: `docs/wm.md`
    /// §States, "Fullscreen covers the panels".
    ///
    /// The frontmost window is the first `Normal`-layer toplevel on screen
    /// (popups, the scrim and drag icons do not count). The panels stay
    /// hidden while it is fullscreen, unless the output is in overview or
    /// the focus is on some *other* window of the same output (focus on
    /// another output, or nowhere, leaves them hidden). Run once per
    /// wakeup, just before the scene update, so every trigger (state
    /// change, raise, focus, minimize, destroy, overview, hotplug) is
    /// covered from one place. Allocation-free unless something changes.
    fn sync_fullscreen_cover(&mut self) {
        for i in 0..self.outputs.len() {
            let id = self.outputs[i].scene_id;
            let hide = self.fullscreen_covers(id);
            if hide == self.scene.top_layer_hidden(id) {
                continue;
            }
            self.scene.set_top_layer_hidden(id, hide);
            // Bar ↔ fullscreen window under a still pointer.
            self.pointer_refresh = true;
            self.cursor_stale = true;
            if hide {
                // A panel's menu must not stay open (and grabbing) while
                // its panel is invisible.
                let panels: Vec<WindowKey> = self
                    .scene
                    .windows(id)
                    .filter(|w| {
                        self.scene
                            .window_info(*w)
                            .is_ok_and(|i| i.layer() == nitro_scene::Layer::Top && !i.is_popup())
                    })
                    .collect();
                for panel in panels {
                    self.dismiss_popups_of(panel);
                }
            }
        }
    }

    /// Whether a fullscreen window covers the panels on `output`; see
    /// [`Server::sync_fullscreen_cover`].
    fn fullscreen_covers(&self, output: SceneOutputId) -> bool {
        if self.overview_output() == Some(output) {
            return false;
        }
        let Some(front) = self.scene.windows_front_to_back(output).find(|w| {
            self.scene
                .window_info(*w)
                .is_ok_and(|i| i.layer() == nitro_scene::Layer::Normal && !i.is_popup())
                && !self.drag_icons.contains(w)
                && !self.is_scrim(*w)
                && self.on_screen(*w)
        }) else {
            return false;
        };
        if self
            .scene
            .window_info(front)
            .map_or(true, |i| i.state() != WindowState::Fullscreen)
        {
            return false;
        }
        match self.focus {
            Some(f)
                if self
                    .scene
                    .window_info(f)
                    .is_ok_and(|i| i.output() == Some(output)) =>
            {
                self.scene.chain_root(f) == front
            }
            _ => true,
        }
    }

    /// Where an output's logical space starts in the desktop space.
    ///
    /// A lookup in the table [`Server::sync_outputs`] built, which is what
    /// keeps this cheap and total: it is called from every hit test, every
    /// drag motion and every clamp, and an output it does not know about
    /// is the origin (the state between "a window exists" and "a connector
    /// reported a mode").
    ///
    /// The table says either what `output.<c>.position` asked for or, for
    /// a connector the file does not position, where the previous output
    /// ended — outputs laid out left to right in connector order, each
    /// taking its *logical* width, so a 2× output takes half as much
    /// desktop width as it has device pixels.
    fn desktop_origin(&self, id: SceneOutputId) -> Point {
        self.origins
            .iter()
            .find(|(out, _)| *out == id)
            .map_or(Point::ZERO, |(_, origin)| *origin)
    }

    /// The primary output: where orphaned windows migrate, and what a
    /// window with no output of its own is measured against.
    ///
    /// `output.<c>.primary = true` in `server.conf` names it; with nothing
    /// marked (or the marked connector unplugged) it is the first output,
    /// exactly as it was before the file existed. One function so the two
    /// dozen call sites cannot each pick a different answer.
    fn primary_output(&self) -> Option<SceneOutputId> {
        if let Some(name) = self.settings.primary() {
            let kms = self.backend.outputs();
            let by_name = self
                .outputs
                .iter()
                .find(|o| kms.iter().any(|i| i.id == o.kms_id && i.name == name));
            if let Some(o) = by_name {
                return Some(o.scene_id);
            }
        }
        self.outputs.first().map(|o| o.scene_id)
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
        self.pointer_refresh = true;
        self.configure(win);
        self.reflow_popups(win);
    }

    /// Set a window's **frame** rectangle: position and content size at
    /// once, with the content size clamped to the client's limits.
    fn set_frame_rect(&mut self, win: WindowKey, rect: Rect) {
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        self.pointer_refresh = true;
        let inset = info.inset();
        // A resize never changes which output a window is on: it is the
        // opposite edge that moves, and handing a window over mid-resize
        // would be a surprise. So it keeps its current output and only its
        // position within it is recomputed.
        let output = info.output().or_else(|| self.primary_output());
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
        self.reflow_popups(win);
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
        // The restore rectangle is taken whenever a window *enters* the
        // enlarged states from outside them — not just from `Normal`.
        // `Minimized` keeps a window's geometry (it is not a soft close),
        // so minimize → maximize must remember the pre-maximize rectangle
        // too, or `Normal` would have nowhere to put the window back and it
        // would be stuck at the maximized size for good. Going the other
        // way, `Maximized` → `Fullscreen` must *not* overwrite it: what is
        // remembered there is already the normal rectangle.
        let was_enlarged = matches!(
            info.state(),
            WindowState::Maximized | WindowState::Fullscreen
        );
        if !was_enlarged && matches!(state, WindowState::Maximized | WindowState::Fullscreen) {
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
        // Minimized, maximized, restored: the window grew, shrank or went
        // away under a pointer that may not move.
        self.pointer_refresh = true;
        // A window with an exclusive zone that became (or stopped being)
        // hidden changed the work area, and every maximized window has to be
        // re-sized for it. Guarded on the zone map being non-empty, so a
        // desktop with no shell running pays one `is_empty` per state change.
        if !self.zones.is_empty() {
            self.work_area_changed();
        }
        if state == WindowState::Minimized {
            // A window that stops showing takes its menus with it.
            self.dismiss_popups_of(win);
            // The pointer may be on one of this window's frame buttons —
            // in fact it *is*, in the case that matters: clicking
            // minimize hides the frame with its own disc still filled.
            // `button_hover` is otherwise cleared only by a motion, so
            // a restore before the pointer moves (`Alt+Tab`) would bring
            // the window back lit under a pointer that is elsewhere.
            //
            // `forget_window` already does this for a window that was
            // destroyed; this is the same rule for one that merely
            // stopped being on screen.
            if self.button_hover.is_some_and(|(w, _)| w == win) {
                self.button_hover = None;
                self.style_only(win, self.focus == Some(win));
            }
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
    /// `Maximized` fills the work area and shows them (re-showing them when
    /// coming out of fullscreen); `Normal` goes back to the remembered
    /// rectangle; `Minimized` does not move anything, so un-minimizing lands
    /// where it was.
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
                    self.set_frame_visible(win, true);
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
        for key in nodes.all() {
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
        // The shell's window list carries the state too, and a bar's
        // minimize/restore button has to reflect what actually happened
        // rather than what it asked for: `set_state` may have refused.
        self.notify_window(win);
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
        self.pointer_refresh = true;
        if self.focusable(win) {
            self.focus_window(Some(win));
        }
    }

    /// A shell's `FocusWindow`: restore the window if it is minimized,
    /// then raise and focus it.
    ///
    /// The **`NO_FOCUS` refusal is unchanged**: a window whose client
    /// opted out of focus, or a stale ref, is silently ignored on exactly
    /// the terms a click on it would be, because a bar's window list must
    /// not be able to wedge the keyboard by naming the wrong row.
    ///
    /// What changed in #3724 is the *minimized* case, which was refused
    /// with it: [`Server::focusable`] excludes `Minimized`, so clicking a
    /// minimized window in the bar did nothing at all, silently — the box
    /// reported it as "if a window is minimized, clicking it in the bar
    /// should re-open it". Restoring it first is what every taskbar does,
    /// and it is the same `set_state(Normal)` that `Alt+Tab` already uses
    /// to bring a minimized window back ([`Server::cycle_focus`]) rather
    /// than a second un-minimize path that could drift from it.
    fn focus_window_for_shell(&mut self, win: WindowKey) {
        if self
            .scene
            .window_info(win)
            .is_ok_and(|i| i.state() == WindowState::Minimized)
        {
            // Only for a window that could take focus once it is back. A
            // `NO_FOCUS` window is refused *before* it is restored, or the
            // bar would be able to un-minimize a panel it can never focus.
            if !self
                .scene
                .window_info(win)
                .is_ok_and(|i| i.flags().focusable)
            {
                return;
            }
            self.set_state(win, WindowState::Normal);
        }
        if self.focusable(win) {
            self.raise_and_focus(win);
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
    /// Front to back through the z-order, skipping windows that are not
    /// **showing**: a hidden window must not swallow a click, which is also
    /// why this is a separate walk from the scene's own hit test (that one
    /// only knows about *painted* nodes and would never see a resize band
    /// outside the window at all).
    ///
    /// "Not showing" is [`Server::showing`], not just `Minimized`, and that
    /// is #3713: the launcher is a centred 600x400 `Overlay` created
    /// visible and hidden on its loop's first turn with `SetVisible` — no
    /// state change, so its state stays `Normal`. A walk that only skipped
    /// `Minimized` therefore found that invisible rectangle in front of
    /// everything and returned its `Region::Content`, and every title-bar
    /// press, frame button and resize band under it did nothing while
    /// content clicks (which take the `pointer.over` path, filled in by the
    /// scene's visibility-honouring hit test) kept working. Reported from
    /// the box as "I cannot move windows" after a restart, and it "fixed
    /// itself" only once the window was dragged out from under the
    /// launcher's rectangle by some other means.
    ///
    /// The two hit tests must agree about who is on screen; this is the
    /// half that had its own answer.
    fn frame_hit(&self, point: Point) -> Option<(WindowKey, Region)> {
        // The pointer decides which output's stack to walk, but the
        // *resize bands* reach outside a window, so a grab just past a
        // screen edge has to find the window on the other side of it: walk
        // every output, frontmost stack first.
        //
        // By index, and deliberately: this used to collect the scene ids
        // into a `Vec` to end the borrow of `self.outputs`, which was
        // affordable when it ran once per button press. The resize hint put
        // it on the *motion* path, and an allocation per motion event is
        // exactly what `docs/budget.md` promises the input path does not do.
        for i in 0..self.outputs.len() {
            let id = self.outputs[i].scene_id;
            let origin = self.desktop_origin(id);
            let local = Point::new(point.x - origin.x, point.y - origin.y);
            for win in self.scene.windows_front_to_back(id) {
                let Ok(info) = self.scene.window_info(win) else {
                    continue;
                };
                if !self.on_screen(win) || self.drag_icons.contains(&win) || self.is_scrim(win) {
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
    ///
    /// Rounded to whole logical pixels, as [`wm::clamp_into`] is for a
    /// new window: libinput deltas are fractional, and a moved frame at
    /// a half pixel has every edge blended across two device pixels —
    /// which is what made the 1-px `resize_hint` stroke read at half
    /// strength on hardware (#565). The grab offset stays fractional;
    /// only the written origin is snapped.
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
            pos.x
                .clamp(
                    area.x - (size.w - min_visible).max(0.0),
                    area.x + area.w - min_visible,
                )
                .round(),
            pos.y.clamp(area.y, area.y + area.h - min_visible).round(),
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
            Ok(Request::Outputs) => self.outputs_reply(),
            Ok(Request::Modes) => self.modes_reply(),
            Ok(Request::Stats) => {
                // The helper is non-dumpable: its memory comes from its
                // own `Stats`, answered after this reply (the previous
                // answer is reported).
                self.gpu
                    .query_stats(&EpollPoll(&self.epoll), TOK_GPU);
                self.stats_reply()
            }
            Ok(Request::Shot(name)) => self.shot(name.as_deref()),
            Ok(Request::ShotFront(name)) => self.shot_front(name.as_deref()),
            Ok(Request::Quit) => {
                info!("quit requested");
                self.quit = true;
                protocol::ok_reply()
            }
            Ok(Request::Plug(w, h)) => self.plug(w, h),
            Ok(Request::Reload) => {
                self.reload_config();
                protocol::ok_reply()
            }
            Ok(Request::Unplug) => self.unplug(),
            Ok(Request::Focus) => self.focus_topmost(),
            Ok(Request::Overview { on, output }) => self.overview_request(on, output.as_deref()),
            Ok(Request::Input(spec)) => self.input_request(&spec),
            Ok(Request::Samples(kind)) => {
                let log = match kind {
                    protocol::SampleKind::I2p => &self.stats.i2p_log,
                    protocol::SampleKind::Flip => &self.stats.flip_log,
                    protocol::SampleKind::Paint => &self.stats.paint_log,
                    protocol::SampleKind::Damage => &self.stats.damage_log,
                };
                protocol::samples_reply(log.total, &log.values())
            }
            Ok(Request::Theme) => protocol::theme_reply(
                self.settings.theme.scheme.unwrap_or_default(),
                self.theme_serial,
                &self.palette,
            ),
        };
        client.send(reply);
    }

    /// The `outputs` reply: one line per output with its mode, the scale
    /// in force, its desktop-space origin and whether it is the primary.
    ///
    /// Assembled here rather than in [`protocol`] because everything past
    /// the mode is the *server's* view — the scene's scale, the origin
    /// table `sync_outputs` built and `server.conf`'s primary — and none
    /// of it is in an [`OutputInfo`].
    ///
    /// Sorted left to right in **desktop** space, which is the space the
    /// line's `pos` is in. (The wire's `OutputInfo` sorts by *device* x
    /// instead, because its `x`/`y` are the device rect; the two orders
    /// coincide for every layout anyone writes, and each reply is at
    /// least ordered in the space it reports.)
    fn outputs_reply(&self) -> Vec<u8> {
        let primary = self.primary_output();
        let mut lines: Vec<protocol::OutputLine> = self
            .backend
            .outputs()
            .iter()
            .map(|info| {
                let scene_id = SceneOutputId(info.id.0);
                let origin = self.desktop_origin(scene_id);
                protocol::OutputLine {
                    name: info.name.clone(),
                    width: info.width,
                    height: info.height,
                    refresh_mhz: info.refresh_mhz,
                    scale: self
                        .scene
                        .output_info(scene_id)
                        .map_or(1.0, |(_, scale)| scale),
                    position: (origin.x as i32, origin.y as i32),
                    primary: primary == Some(scene_id),
                    custom_mode: info.custom_mode,
                }
            })
            .collect();
        lines.sort_unstable_by_key(|o| o.position);
        protocol::outputs_reply(&lines)
    }

    /// `modes`: every mode every connected connector offers.
    ///
    /// The answer to "what may I write in `output.<c>.mode`", which is a
    /// question `outputs` cannot answer — it reports the one mode in
    /// force. Ordered by connector in the backend's own order and, within
    /// one, in the kernel's, because that order is itself information: the
    /// kernel lists a connector's modes best-first.
    fn modes_reply(&self) -> Vec<u8> {
        let mut lines: Vec<protocol::ModeLine> = Vec::new();
        for info in self.backend.outputs() {
            // `=` marks the mode in use, and **at most one line may carry
            // it**. A connector can list the same W/H/refresh twice with
            // different timings — the test box lists 1920x1080@60 twice,
            // at 148 500 kHz and another clock — and matching on those
            // three numbers alone would mark both, so the reply would say
            // two modes are in use. The first match wins, which is the
            // kernel's own order and therefore the one `select_mode`
            // would have picked.
            let mut marked = false;
            for m in self.backend.available_modes(info.id) {
                let current = !marked
                    && !info.custom_mode
                    && m.width == info.width
                    && m.height == info.height
                    && m.refresh_mhz == info.refresh_mhz;
                marked |= current;
                lines.push(protocol::ModeLine {
                    name: info.name.clone(),
                    mode: m.to_string(),
                    preferred: m.preferred,
                    current,
                });
            }
            // A modeline is not in the connector's list at all, so it
            // would otherwise be the one mode this reply does not mention
            // — and it is the one most worth seeing, because it is the one
            // nothing else validated.
            if info.custom_mode {
                lines.push(protocol::ModeLine {
                    name: info.name.clone(),
                    mode: format!(
                        "{}x{}@{} (custom)",
                        info.width,
                        info.height,
                        nitro_kms::drm::select::hz_text(info.refresh_mhz)
                    ),
                    preferred: false,
                    current: true,
                });
            }
        }
        protocol::modes_reply(&lines)
    }

    /// The overview lines of `stats`.
    fn overview_stats(&self, pairs: &mut Vec<(&'static str, u64)>) {
        // Overview mode: whether one is up, and how many thumbnails it
        // has. The scrim counts under `windows`, as the scene window it is.
        pairs.push(("overview", u64::from(self.wm.overview().is_some())));
        let ov = self.wm.overview();
        pairs.push(("overview_thumbs", ov.map_or(0, |o| o.thumbs.len()) as u64));
        pairs.push((
            "overview_fading",
            u64::from(ov.is_some_and(|o| o.fade_start_ns.is_some())),
        ));
        // Whether search results have replaced the grid (`Search`).
        pairs.push((
            "overview_grid_hidden",
            u64::from(ov.is_some_and(|o| o.grid_hidden)),
        ));
        self.atlas_stats(pairs);
    }

    /// The thumbnail-atlas lines of `stats`.
    fn atlas_stats(&self, pairs: &mut Vec<(&'static str, u64)>) {
        // The thumbnail atlas (#3902): whether the output in overview (or
        // else the first output) has one, the heap they hold, and the
        // damage-driven re-renders into them.
        let atlas_output = self
            .wm
            .overview()
            .map(|o| o.output)
            .or_else(|| self.outputs.first().map(|o| o.scene_id));
        pairs.push((
            "overview_atlas",
            u64::from(
                self.outputs
                    .iter()
                    .any(|o| Some(o.scene_id) == atlas_output && o.atlas.is_some()),
            ),
        ));
        pairs.push((
            "overview_atlas_bytes",
            self.outputs
                .iter()
                .filter_map(|o| o.atlas)
                .map(|a| a.bytes)
                .sum(),
        ));
        pairs.push(("thumb_renders", self.thumb_renders));
        pairs.push(("thumb_render_us", self.thumb_render_us));
        pairs.push(("buffers", self.scene.buffer_count() as u64));
        // Server-allocated scanout buffers (#3914): how many clients hold,
        // and their mapped bytes (dumb-buffer memory, not in RssAnon).
        let scanout = self
            .wire_clients
            .values()
            .flat_map(|c| c.buffers.values())
            .filter(|h| h.scanout.is_some() && !h.dmabuf);
        let (n, bytes) = scanout.fold((0u64, 0u64), |(n, b), h| (n + 1, b + h.bytes));
        pairs.push(("scanout_buffers", n));
        pairs.push(("scanout_buffer_bytes", bytes));
        // Client dma-bufs (#3918).
        let dma: Vec<&HeldBuffer> = self
            .wire_clients
            .values()
            .flat_map(|c| c.buffers.values())
            .filter(|h| h.dmabuf)
            .collect();
        pairs.push(("dmabuf_buffers", dma.len() as u64));
        pairs.push((
            "dmabuf_cpu_mapped",
            dma.iter().filter(|h| h.bytes > 0).count() as u64,
        ));
        pairs.push((
            "dmabuf_kms_imported",
            dma.iter().filter(|h| h.scanout.is_some()).count() as u64,
        ));
        pairs.push(("dmabuf_kms_refused", self.dmabuf_kms_refused));
        self.planes_stats(pairs);
        self.gpu_stats(pairs);
        pairs.push(("dmabuf_placeholder_paints", frame::placeholder_paints()));
        pairs.push(("fences_pending", self.fences.len() as u64));
        pairs.push(("fence_waits", self.fence_waits));
        pairs.push(("implicit_fence_fallbacks", self.implicit_fence_fallbacks));
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
            ("first_frame_ms", self.first_frame_ms.unwrap_or(0)),
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
        pairs.push(("blit_frames", self.blit_frames));
        self.text.write_pairs(&mut pairs);
        self.icons.write_pairs(&mut pairs);
        pairs.push(("clients", self.wire_clients.len() as u64));
        pairs.push(("windows", self.scene.window_count() as u64));
        pairs.push(("nodes", self.scene.node_count() as u64));
        pairs.push(("outputs", self.outputs.len() as u64));
        // Heap the shadow buffers hold, summed over the outputs: ~8 MB per
        // 1080p screen, 0 under `NITRO_SHADOW=0`. It is the one deliberate
        // memory-for-speed trade in the server (`docs/budget.md`), so it
        // is reported rather than left to be inferred from `outputs`.
        pairs.push((
            "shadow_bytes",
            self.outputs
                .iter()
                .filter_map(|o| o.shadow.as_ref())
                .map(frame::Shadow::bytes)
                .sum(),
        ));
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
        self.overview_stats(&mut pairs);
        pairs.push(("overview_requests", self.overview_requests));
        pairs.push(("overview_watchers", self.overview_watchers.len() as u64));
        pairs.push(("focused", u64::from(self.focus.is_some())));
        // The shell's view. `shell_clients` counts connections on the
        // privileged socket, which is the number to look at when a bar is
        // "not working": zero means it never got there.
        let shell_clients = self
            .wire_clients
            .keys()
            .filter(|t| Self::is_shell(**t))
            .count();
        pairs.push(("shell_clients", shell_clients as u64));
        // And the remote one. `remote_clients` counts connections that
        // arrived over TCP; `remote_listen` is reported separately,
        // below, because it is an address and not a number.
        let remote_clients = self
            .wire_clients
            .keys()
            .filter(|t| Self::is_remote(**t))
            .count();
        pairs.push(("remote_clients", remote_clients as u64));
        pairs.push(("hotkeys", self.hotkeys.len() as u64));
        pairs.push(("exclusive_zones", self.zones.zone_count() as u64));
        pairs.push(("grabbed", u64::from(self.grab.is_some())));
        // Keys dropped because a shell's hotkey had fired and the shell had
        // not answered yet (`Server::withheld`). Cumulative, and normally
        // zero: a non-zero value means someone types faster than the shell
        // wakes, which is exactly the race this counter exists to pin.
        pairs.push(("keys_withheld", self.keys_withheld));
        // Key repeats synthesised (`repeat`), cumulative, and whether a
        // key is repeating right now. The second is 0 on an idle desktop,
        // which is the check that nothing is left stuck.
        pairs.push(("key_repeats", self.key_repeat.repeats));
        pairs.push(("key_repeating", u64::from(self.key_repeat.held().is_some())));
        // The clipboard (M5-H). Relayed owner descriptors, the server's own
        // EOF answers, and requests parked waiting for an owner. The first
        // two are cumulative; the third returns to 0 whenever every owner
        // has answered, so a stuck non-zero value names a slow owner.
        pairs.push(("selection_transfers", self.selection_transfers));
        pairs.push(("selection_eof", self.selection_eof));
        pairs.push(("selections_pending", self.data.pending() as u64));
        // Drag and drop (M5-I). `dnd_active` is any drag state held (a drop
        // being read, a source yet to finish) and `dnd_grab` whether one
        // holds the pointer; both 0 on an idle desktop, which is the check
        // that nothing was left stuck. The other two are cumulative.
        pairs.push(("dnd_active", u64::from(self.dnd.is_some())));
        pairs.push(("dnd_grab", u64::from(self.dnd_grabbing())));
        // Whether the current target has accepted: what the cursor shows.
        pairs.push((
            "dnd_accepted",
            u64::from(self.dnd.as_ref().is_some_and(data::Dnd::accepted)),
        ));
        pairs.push(("dnd_drops", self.dnd_drops));
        pairs.push(("dnd_cancels", self.dnd_cancels));
        pairs.push(("locked", u64::from(self.lock.is_locked())));
        pairs.push(("lock_owned", u64::from(self.lock.owner().is_some())));
        // Completed reloads, however triggered: the control request,
        // SIGHUP and the inotify watch all land in one counter, because
        // what a caller wants to know is "did the server pick my edit up",
        // not which of the three doors it came through.
        pairs.push(("config_reloads", self.config_reloads));
        // Control-socket input injection (`input`): events routed so far,
        // cumulative, and events of a scripted sequence still to come. A
        // benchmark waits for the second to reach 0.
        pairs.push(("input_injected", self.injector.injected));
        pairs.push(("input_inject_pending", self.injector.pending() as u64));
        // The remote listener's address, with the port the kernel chose
        // for a configured `:0`, or `off`. Text rather than a number
        // because an address is not a count; see
        // [`protocol::stats_reply_with`].
        let remote_listen = remote::listen_text(self.remote_listener.as_ref());
        protocol::stats_reply_with(&pairs, &[("remote_listen", remote_listen)])
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
                // A remote client that sent a buffer op anyway. Not
                // fatal: the frame was consumed whole, so the stream is
                // still in step, and a client that missed `caps::REMOTE`
                // is better served by an explanation than by a dead
                // socket. It keeps its windows and its connection; only
                // the image is missing, which is exactly what "no pixels
                // cross the link" means (`docs/remote.md`).
                //
                // "Buffer op" is accurate by construction, not by
                // accident: `next_msg` splits its remote fd refusal on
                // `nitro_wire::server::is_buffer_op`, so `RemoteNoFds`
                // reaches this arm only for a buffer op — which is what
                // makes `BadBuffer` and the `REMOTE_NO_BUFFERS` sentence
                // right here. Any other fd-carrying op from a remote
                // client arrives as `Error::Unexpected` and falls into
                // the generic fatal arm below as `Protocol`, which is the
                // withheld-`DATA`/`KEYMAP` rule (`docs/wire.md`).
                Err(nitro_wire::error::Error::RemoteNoFds) => {
                    let Some(client) = self.wire_clients.get_mut(&token) else {
                        return false;
                    };
                    warn!(
                        "remote client {}: buffers are not available on a remote link",
                        client.id.0
                    );
                    client.send(&ServerMsg::Error(msg::Error {
                        serial: 0,
                        code: ErrorCode::BadBuffer,
                        msg: REMOTE_NO_BUFFERS.to_owned(),
                    }));
                    continue;
                }
                Err(e) => {
                    let code = clients::wire_code(&e);
                    let detail = e.to_string();
                    self.disconnect(token, Some((0, code, detail)));
                    return false;
                }
            };
            if Self::is_remote(token) && Self::refuse_remote_buffer_op(&msg) {
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                warn!(
                    "remote client {}: {} is not available on a remote link",
                    client.id.0,
                    msg.name()
                );
                client.send(&ServerMsg::Error(msg::Error {
                    serial: 0,
                    code: ErrorCode::BadBuffer,
                    msg: REMOTE_NO_BUFFERS.to_owned(),
                }));
                continue;
            }
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

    /// Record a `ClientCaps` (M5-A). Returns whether the client survives.
    ///
    /// Answered **on receipt** rather than buffered for the commit: it is
    /// a connection property, not a scene mutation — the `BindKey`
    /// precedent.
    ///
    /// **Stored**, because the rule that makes `ClientCaps` safe is "the
    /// server must not send a message belonging to a bit the client did
    /// not list": the popup ops and every clipboard push read it, and each
    /// later M5 task gains one `if` instead of re-litigating this. See
    /// `docs/wire.md` § Capability opt-in.
    fn record_client_caps(&mut self, token: u64, caps: u32) -> bool {
        let advertised = self.caps(Self::is_shell(token), Self::is_remote(token));
        let extra = caps & !advertised;
        if extra != 0 {
            // A client claiming to understand messages the server never
            // offered is confused, and this is the cheap place to say so.
            self.disconnect(
                token,
                Some((
                    0,
                    ErrorCode::Protocol,
                    format!("ClientCaps named bits {extra:#x} the server did not advertise"),
                )),
            );
            return false;
        }
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        let before = client.client_caps;
        client.client_caps = caps;
        // A client opting into `KEYMAP` is sent the keymap now. Not right
        // behind `Welcome` as `Theme` is: rule 1 of the opt-in forbids a
        // bit-8+ message before the client listed the bit, and `ClientCaps`
        // necessarily comes after `Welcome`. A client sends it straight
        // behind `Hello`, so it is the same round trip. Keyed on the
        // transition, so a repeated `ClientCaps` is not a keymap storm.
        let keymap = nitro_wire::types::caps::KEYMAP;
        let keymap_now = before & keymap == 0 && caps & keymap != 0;
        // A client opting into `DATA` is told about the selection that
        // already exists, the way `Theme` follows `Welcome`: without it a
        // browser launched after the copy would not know there was
        // anything to paste until the next `SetSelection`.
        let data = nitro_wire::types::caps::DATA;
        if before & data == 0 && caps & data != 0 && self.data.offer().is_some() {
            let mimes = self.data.mimes().to_vec();
            client.send(&ServerMsg::SelectionOffer(msg::SelectionOffer { mimes }));
        }
        if keymap_now {
            self.send_keymap(token);
        }
        // A client opting into `DMABUF` hears the default feedback now
        // (#3918): which dma-bufs it may create at all.
        let dma = nitro_wire::types::caps::DMABUF;
        if before & dma == 0 && caps & dma != 0 {
            self.send_default_feedback(Some(token));
        }
        true
    }

    /// Whether this client may speak `DATA`, disconnecting it if not.
    ///
    /// Two checks in one place, both fatal `Protocol` because a conformant
    /// client cannot fail them: the link must be able to carry descriptors
    /// (a remote one was never advertised the bit), and the client must
    /// have listed `caps::DATA` in its `ClientCaps` — `docs/wire.md`
    /// § Capability opt-in rule 3.
    fn data_allowed(&mut self, token: u64, name: &str) -> bool {
        let detail = if Self::is_remote(token) {
            format!("{name} needs caps::DATA, which a remote link does not have")
        } else if self
            .wire_clients
            .get(&token)
            .is_some_and(|c| c.client_caps & nitro_wire::types::caps::DATA == 0)
        {
            format!("{name} needs `DATA` listed in ClientCaps")
        } else {
            return self.wire_clients.contains_key(&token);
        };
        self.disconnect(token, Some((0, ErrorCode::Protocol, detail)));
        false
    }

    /// The token of the client whose window holds keyboard focus.
    ///
    /// The *client*, not the window: a client with two windows, one of
    /// them focused, may take the selection from either.
    fn focus_token(&self) -> Option<u64> {
        let focus = self.focus?;
        self.wire_clients
            .iter()
            .find(|(_, c)| c.owns_window(focus))
            .map(|(t, _)| *t)
    }

    /// `SetSelection`: take (or clear) the clipboard. Returns whether the
    /// client survives.
    ///
    /// Authorized by **keyboard focus**. Without it the request is dropped,
    /// not fatal: focus can leave between the client's send and our
    /// receive (the `SetCursor` race; Chromium hit it on a slow copy), and
    /// a background process still cannot replace the clipboard. Nothing is
    /// sent back: no echo arrives, which a client that saw its `Focus`
    /// go before the echo reads as "dropped" (docs/wire.md).
    fn set_selection(&mut self, token: u64, mimes: Vec<String>) -> bool {
        if !self.data_allowed(token, "SetSelection") {
            return false;
        }
        if self.focus_token() != Some(token) {
            info!("wire client {token}: SetSelection without keyboard focus: dropped");
            return true;
        }
        if let Err(e) = data::validate_mimes(&mimes) {
            let code = if e.is_limit() {
                ErrorCode::Limit
            } else {
                ErrorCode::Protocol
            };
            self.disconnect(token, Some((0, code, e.detail("SetSelection"))));
            return false;
        }
        // The old owner is not told directly; it sees the new
        // `SelectionOffer` like everyone else (docs/wire.md § What the
        // server does).
        for t in self.data.set_offer(token, mimes) {
            self.answer_eof(t.requester, t.reply_to);
        }
        self.broadcast_offer();
        true
    }

    /// `RequestSelection`: ask the owner for the selection in one MIME
    /// type. Returns whether the client survives. Every request that gets
    /// past the protocol checks is answered by exactly one `SelectionData`.
    fn request_selection(&mut self, token: u64, m: msg::RequestSelection) -> bool {
        if !self.data_allowed(token, "RequestSelection") {
            return false;
        }
        let drag = m.source == nitro_wire::types::DataSource::Drag;
        let fatal = if drag && !self.dnd.as_ref().is_some_and(|d| d.is_drop_target(token)) {
            // Valid only while this client is the drop target: between a
            // `DragEnter` and its `DragLeave`, or dropped on and reading.
            Some("RequestSelection { source: Drag } outside a drag")
        } else if self.data.has_reply_id(token, m.request) {
            Some("RequestSelection reuses an outstanding request id")
        } else {
            None
        };
        if let Some(detail) = fatal {
            self.disconnect(token, Some((0, ErrorCode::Protocol, detail.to_owned())));
            return false;
        }
        // The failures that are answered, not refused. A MIME type outside
        // the offer is answered here rather than relayed: the answer would
        // be byte-identical, and this saves waking the owner.
        let owner = if drag {
            self.dnd
                .as_ref()
                .filter(|d| d.mimes.contains(&m.mime))
                .map(|d| d.source)
        } else {
            self.data
                .offer()
                .filter(|o| o.mimes.contains(&m.mime))
                .map(|o| o.owner)
        }
        .filter(|o| self.wire_clients.contains_key(o));
        let Some(owner) = owner.filter(|_| self.data.outstanding(token) < MAX_PENDING_SELECTIONS)
        else {
            self.answer_eof(token, m.request);
            return true;
        };
        let id = self.data.start(token, m.request, owner, m.source);
        let Some(client) = self.wire_clients.get_mut(&owner) else {
            // Checked above; kept total rather than trusting it.
            self.data.take(id, owner);
            self.answer_eof(token, m.request);
            return true;
        };
        client.send(&ServerMsg::SelectionRequest(msg::SelectionRequest {
            request: id,
            source: m.source,
            mime: m.mime,
        }));
        true
    }

    /// `SendSelection`: relay the owner's descriptor to the requester.
    /// Returns whether the client survives.
    ///
    /// The server never reads, seeks or `fstat`s the descriptor. An
    /// unknown or stale id is **not** an error: the owner is racing a
    /// selection change it has not heard about yet, and its requester has
    /// already been answered at EOF.
    fn send_selection(&mut self, token: u64, request: u32, fd: OwnedFd) -> bool {
        if !self.data_allowed(token, "SendSelection") {
            return false;
        }
        let Some(t) = self.data.take(request, token) else {
            debug!("stale SendSelection {request} from token {token}; dropped");
            drop(fd);
            return true;
        };
        if let Some(client) = self.wire_clients.get_mut(&t.requester) {
            client.send(&ServerMsg::SelectionData(msg::SelectionData {
                request: t.reply_to,
                fd,
            }));
            self.selection_transfers += 1;
            self.arm_wire_client(t.requester);
        }
        true
    }

    /// Answer a request with a descriptor already at EOF: the single
    /// failure path, byte-identical to an owner's own "I cannot serve
    /// that".
    ///
    /// `SelectionData::encode_body` dups the read end into the writer's
    /// queue, so the server's copy closes at the end of this function and
    /// it holds a descriptor only until the next flush. The client is
    /// re-armed here because this can run from inside `disconnect` during
    /// `flush_wire_clients`, after the requester's own flush in that pass:
    /// without asking for `OUT` the answer would sit queued and the
    /// requester would wait for an EOF that had been produced but not sent.
    fn answer_eof(&mut self, token: u64, reply_to: u32) {
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        let (r, w) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
            Ok(pair) => pair,
            Err(e) => {
                // `EMFILE`/`ENFILE`: the server's own exhaustion. Killing
                // the client for it would be worse than the one case where
                // "exactly one answer" cannot be honoured.
                warn!("pipe for an EOF selection answer: {e}");
                return;
            }
        };
        drop(w);
        client.send(&ServerMsg::SelectionData(msg::SelectionData {
            request: reply_to,
            fd: r,
        }));
        self.selection_eof += 1;
        self.arm_wire_client(token);
    }

    /// Push the current offer to every client that listed `DATA` — and
    /// only those, `docs/wire.md` rule 1: a client that never sent
    /// `ClientCaps` (the bar, the terminal, every pre-M5 client) would die
    /// on an op it does not know. Remote clients are excluded by
    /// construction: they were never advertised the bit, so
    /// `record_client_caps` refused it.
    fn broadcast_offer(&mut self) {
        let mimes = self.data.mimes().to_vec();
        let mut touched = Vec::new();
        for (token, client) in &mut self.wire_clients {
            if client.stream.is_ready() && client.client_caps & nitro_wire::types::caps::DATA != 0 {
                client.send(&ServerMsg::SelectionOffer(msg::SelectionOffer {
                    mimes: mimes.clone(),
                }));
                touched.push(*token);
            }
        }
        for token in touched {
            self.arm_wire_client(token);
        }
    }

    /// What the client at `token` holds against the buffer caps, with
    /// every client's bytes filled in for the server-wide one. Counts the
    /// client's uncommitted batch (`WireClient::held_buffers`), because
    /// the mapping happens at arrival. `None` if the client is gone.
    fn buffer_budget(&self, token: u64) -> Option<clients::BufferBudget> {
        let all: u64 = self
            .wire_clients
            .values()
            .map(|c| c.held_buffers().bytes)
            .sum();
        let mut held = self.wire_clients.get(&token)?.held_buffers();
        held.all_clients_bytes = all;
        Some(held)
    }

    /// Check and map a `CreateBuffer` at receipt, parking the mapping in
    /// the client's batch. Returns whether the client survives it.
    ///
    /// The descriptor is checked and *mapped* now, not at commit: the
    /// client may legitimately close or reuse its own descriptor as soon
    /// as it has sent this message, and a buffer whose fd is not sealed
    /// must be refused before any of the batch is applied. From here on
    /// the scene holds the mapping, so there is no server-side copy to
    /// keep in step and `BufferDamage` only marks nodes for repaint. The
    /// buffer caps are checked here too, for the same reason: a cap
    /// checked at commit would come after the `mmap` it exists to bound.
    fn create_buffer(&mut self, token: u64, buffer: nitro_wire::msg::CreateBuffer) -> bool {
        let id = buffer.id;
        let Some(held) = self.buffer_budget(token) else {
            return false;
        };
        let (desc, pixels) = match clients::map_buffer(buffer, held) {
            Ok(pair) => pair,
            Err(e) => {
                self.disconnect(token, Some((0, e.code, e.detail)));
                return false;
            }
        };
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        client.pending.push(Pending::Buffer(id, desc, pixels));
        true
    }

    /// Buffer, or act on, one decoded client message. Returns whether the
    /// client survives it.
    #[allow(clippy::too_many_lines)] // one arm per op
    fn handle_wire_msg(&mut self, token: u64, message: ClientMsg) -> bool {
        match message {
            ClientMsg::Hello(hello) => {
                let shell = Self::is_shell(token);
                let remote = Self::is_remote(token);
                let caps = self.caps(shell, remote);
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                info!(
                    "{} client {} is {:?}",
                    if shell {
                        "shell"
                    } else if remote {
                        "remote"
                    } else {
                        "wire"
                    },
                    client.id.0,
                    hello.name
                );
                if let Err(e) = client.stream.welcome(SERVER_NAME, caps) {
                    warn!("welcome: {e}");
                    return false;
                }
                // The palette, immediately behind the `Welcome` and in
                // the same batch, so a client's very first paint already
                // has the user's colours: a client that had to wait for
                // a second round trip would paint one frame in its
                // built-in defaults and then flash.
                let theme = msg::Theme::from_palette(self.theme_serial, &self.palette);
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                client.send(&ServerMsg::Theme(theme));
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
            ClientMsg::ClientCaps(m) => self.record_client_caps(token, m.caps),
            // Acted on at receipt, never buffered for a commit: the cursor
            // is a property of the pointer, not a scene mutation a frame
            // must show atomically, and a client that had to commit before
            // its I-beam appeared would show it a frame late.
            ClientMsg::SetCursor(m) => self.set_cursor_request(token, m.shape),
            // Acted on at receipt too: an input gesture, not a scene
            // mutation. Parked in `pending` until the next commit, the drag
            // would start a round trip after the user's hand moved — and
            // the button might well be up by then, so the guard would judge
            // a stale world.
            ClientMsg::StartMove(m) => self.start_move(token, m.window),
            ClientMsg::StartResize(m) => self.start_resize(token, m.window, m.edges),
            // The one unprivileged op answered in the shell block. It has
            // its own arm, *before* the catch-all, so it never reaches
            // `is_shell_op` or `handle_shell_msg`: it shares no code path
            // with the eleven shell ops, and so grants nothing they do.
            // Answered on receipt, for `Outputs`' reason: a question.
            ClientMsg::ListOutputs(_) => self.list_outputs(token),
            // The clipboard ops are answered on receipt, never buffered for
            // the commit. For `SendSelection` that is load-bearing:
            // buffering it would park a descriptor in `pending` until a
            // commit that may never come. The other two are not scene
            // mutations either — a paste must not wait for a frame.
            ClientMsg::SetSelection(m) => self.set_selection(token, m.mimes),
            ClientMsg::RequestSelection(m) => self.request_selection(token, m),
            ClientMsg::SendSelection(m) => self.send_selection(token, m.request, m.fd),
            // The drag answers too: an acceptance answers the motion the
            // target just saw, and a finish must not wait for a commit.
            // `StartDrag` is buffered to the commit instead (its icon may
            // be created in the same batch); see `Server::start_dnd`.
            ClientMsg::AcceptDrop(m) => self.accept_drop(token, m.action, m.mime),
            ClientMsg::FinishDrag(_) => self.finish_drag(token),
            ClientMsg::CreateBuffer(buffer) => self.create_buffer(token, buffer),
            ClientMsg::CreateSurfaceBuffer(buffer) => {
                self.surface_allowed(token, "CreateSurfaceBuffer")
                    && self.create_surface_buffer(token, buffer)
            }
            // Never buffered: the latch path is outside the transactions.
            ClientMsg::PresentSurface(frame) => {
                self.surface_allowed(token, "PresentSurface")
                    && self.present_surface(token, &frame, None)
            }
            // Answered at receipt, like `PresentSurface` (#3914).
            ClientMsg::AllocSurfaceBuffers(m) => self.alloc_surface_buffers(token, &m),
            // Client dma-bufs (#3918): validated at receipt like
            // `CreateSurfaceBuffer`; the fenced present like `PresentSurface`.
            ClientMsg::CreateDmabufBuffer(m) => {
                self.dmabuf_allowed(token, "CreateDmabufBuffer")
                    && self.create_dmabuf_buffer(token, m)
            }
            ClientMsg::PresentSurfaceFenced(m) => {
                self.dmabuf_allowed(token, "PresentSurfaceFenced")
                    && self.present_surface(token, &m.frame, Some(m.fence))
            }
            // Answered at receipt, like `PresentSurface` (#3904).
            ClientMsg::ExportSurface(m) => {
                self.share_allowed(token, "ExportSurface") && self.export_surface(token, m.id)
            }
            ClientMsg::ImportSurface(m) => {
                self.share_allowed(token, "ImportSurface")
                    && self.import_surface(token, m.token, m.id)
            }
            other => {
                // Refused **at receipt**, not at the commit; see
                // `Server::refuse_at_receipt`.
                if self.refuse_at_receipt(token, &other) {
                    return false;
                }
                // The shell ops are answered on receipt rather than buffered
                // for the commit. They are not scene mutations a frame must
                // show atomically: `WindowList` is a *question*, `BindKey` a
                // registration, and a bar that had to commit to arm its
                // launcher key would be arming it a frame late for no gain.
                // `SetLayer`/`SetExclusiveZone`/`SetAnchor` do change what is
                // on screen, and go through the same `settle` pass every
                // other wakeup ends with.
                if is_shell_op(&other) {
                    return self.handle_shell_msg(token, other);
                }
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
    /// cannot keep. `DIRECT_SCANOUT` is set when some output has a
    /// non-cursor plane the planner could put a client dma-buf on
    /// (`dmabuf::direct_scanout`, #3938); the per-node feedback says which
    /// format/modifier pairs. A zero bit is the protocol's way of saying
    /// "do not use this".
    ///
    /// `SHELL` is set for, and only for, a connection accepted on the shell
    /// socket — which is what `shell` says. It is reported rather than
    /// negotiated: the grant already happened when the client managed to
    /// open that path.
    ///
    /// `THEME` is unconditional for the same reason `WM` is: the server
    /// always owns a palette and always pushes it, so every client may
    /// rely on the `Theme` that follows its `Welcome`.
    ///
    /// `REMOTE` is the same shape of fact from the other direction: a
    /// connection accepted on the TCP listener cannot carry descriptors,
    /// so the bit tells the client "buffers are expensive, text is cheap"
    /// *before* it tries to allocate one. A remote client never gets
    /// `SHELL`: the shell socket is a `0700` path and the privilege is
    /// having opened it, which a TCP port cannot prove.
    ///
    /// `ICONS` has the shape of `TEXT` rather than of `THEME`: it says
    /// the server has an icon set to draw with. This build compiles one
    /// in, so it is always set — but it is asked rather than asserted,
    /// because a stripped or fixture server honestly may not have one,
    /// and a client that checks the bit lays out identically either way.
    /// `POPUP` is unconditional: every client may create a menu for a
    /// window it owns, and nothing about the server's hardware or fonts
    /// can make that a promise it cannot keep.
    ///
    /// `DATA` (the clipboard, M5-H) is set on every **local** link and
    /// withheld on a remote one: every leg of a transfer carries a
    /// descriptor (`SendSelection` in, `SelectionData` out), and
    /// `ClientStream::send` refuses an fd-carrying message on TCP, so
    /// advertising it there would be a promise the transport cannot keep.
    /// It is the fact `REMOTE` states about buffers, expressed as the
    /// absence of a bit because `DATA` has one.
    ///
    /// `RELEASE` (M5-B) is set on every **local** link and withheld on a
    /// remote one, which cannot create buffers at all
    /// (`refuse_remote_buffer_op`). `BufferReleased` goes out once a buffer is
    /// referenced by no image node — painting reads the current scene, so
    /// such a buffer is never read again (`send_buffer_releases`).
    ///
    /// `OUTPUTS` (M5-D) is unconditional, for `WM`'s reason: the server
    /// always owns an output list, so `ListOutputs` is a promise it can
    /// always keep — on every link, a remote one included, since output
    /// geometry carries no descriptor.
    ///
    /// `CURSOR` (M5-E) is unconditional for the same reason: the server
    /// always draws a cursor, so `SetCursor` is always a request it can
    /// honour.
    ///
    /// `DRAG` (M5-F) is unconditional, like `WM`: the server always runs
    /// the drag state machine, so a client may always ask to enter it. The
    /// guard on *using* it is pointer focus plus a button down, not a
    /// capability — and it carries no server→client message (the drag is
    /// reported through the ordinary `Configure` stream), so nothing needs
    /// a `ClientCaps` gate either.
    ///
    /// `KEYMAP` (M5-C) has `DATA`'s shape plus `TEXT`'s: set only on a
    /// **local** link (`Keymap` carries a descriptor, which TCP cannot)
    /// *and* only when a keymap was exported — a box with no
    /// `xkeyboard-config` has nothing to ship. Once set it stays set for
    /// the session: a reload whose export fails keeps the previous file
    /// (`reload_config`), so a client re-listing the bit is never
    /// disconnected for naming one it was granted.
    ///
    /// `SURFACE` (#3897) has `RELEASE`'s shape: local links only, since
    /// every Surface buffer is a descriptor. `DMABUF` (#3918) likewise:
    /// a dma-buf is a descriptor, and every local backend (the fake
    /// included) takes linear ones on the CPU path. `SHARE` (#3904)
    /// likewise: its only check is the peer uid, which a TCP link cannot
    /// prove.
    fn caps(&self, shell: bool, remote: bool) -> u32 {
        let mut caps = nitro_wire::types::caps::WM
            | nitro_wire::types::caps::THEME
            | nitro_wire::types::caps::POPUP
            | nitro_wire::types::caps::OUTPUTS
            | nitro_wire::types::caps::CURSOR
            | nitro_wire::types::caps::DRAG
            | nitro_wire::types::caps::OPAQUE_REGION;
        if self.text.has_fonts() {
            caps |= nitro_wire::types::caps::TEXT;
        }
        if self.icons.has_icons() {
            caps |= nitro_wire::types::caps::ICONS;
        }
        if shell {
            caps |= nitro_wire::types::caps::SHELL;
        }
        if remote {
            caps |= nitro_wire::types::caps::REMOTE;
        } else {
            caps |= nitro_wire::types::caps::DATA
                | nitro_wire::types::caps::RELEASE
                | nitro_wire::types::caps::SURFACE
                | nitro_wire::types::caps::DMABUF
                | nitro_wire::types::caps::SHARE;
            if self
                .outputs
                .iter()
                .any(|o| dmabuf::direct_scanout(&o.plane_info))
            {
                caps |= nitro_wire::types::caps::DIRECT_SCANOUT;
            }
            if self.keymap_fd.is_some() {
                caps |= nitro_wire::types::caps::KEYMAP;
            }
        }
        caps
    }

    /// Whether a token names a client that arrived on the shell socket.
    ///
    /// The token range *is* the answer, which is why shell clients get their
    /// own: a per-client boolean would be a second copy of the same fact,
    /// and the two could drift.
    ///
    /// Remote tokens sort *above* the shell range, so the check is a
    /// window and not a threshold — a remote client is emphatically not a
    /// shell client.
    const fn is_shell(token: u64) -> bool {
        token >= TOK_SHELL_BASE && token < TOK_REMOTE_BASE
    }

    /// Whether a token names a client that arrived over TCP.
    const fn is_remote(token: u64) -> bool {
        token >= TOK_REMOTE_BASE
    }

    /// Whether this message from a **remote** client is a buffer op that
    /// cannot mean anything, and should be refused without killing the
    /// connection.
    ///
    /// All three buffer ops, not just the one carrying a descriptor.
    /// `BufferDamage` and `SetImage` merely *name* a buffer, but a remote
    /// client can never have registered one, so honouring them is
    /// impossible for the same reason. Refusing only `CreateBuffer` would
    /// hand a client that ignored `caps::REMOTE` a clear sentence and
    /// then disconnect it two messages later on the `SetImage` that
    /// follows, with `no buffer with id 1` — the worse error, arriving
    /// after the recoverable one, which is the shape of a bug report
    /// nobody can read.
    ///
    /// `SetImage` with [`BufferId::NONE`] is *allowed through*: that is
    /// how an image node is cleared, it names no buffer, and it is the
    /// one op in this group a remote client may legitimately send.
    fn refuse_remote_buffer_op(msg: &ClientMsg) -> bool {
        match msg {
            ClientMsg::SetImage(m) => !m.buffer.is_none(),
            ClientMsg::SetSurface(m) => !m.buffer.is_none(),
            // Answered `AllocSurfaceBuffersFailed { Unsupported }` by
            // `Server::alloc_surface_buffers` instead (#3914): the reply is
            // fd-free and sends the client down its memfd fallback.
            ClientMsg::AllocSurfaceBuffers(_) => false,
            other => nitro_wire::server::is_buffer_op(other.op()),
        }
    }

    // ---------------------------------------------------------- shell ops

    /// Handle one shell op, or kill the connection that had no business
    /// sending it. Returns whether the client survives.
    ///
    /// The privilege check is here and nowhere else: one `if` against the
    /// token range, before any of the ops is looked at. Spreading it over
    /// eleven message handlers is how a capability check gets forgotten in
    /// the twelfth.
    fn handle_shell_msg(&mut self, token: u64, msg: ClientMsg) -> bool {
        if !Self::is_shell(token) {
            let name = msg.name();
            self.disconnect(
                token,
                Some((
                    0,
                    ErrorCode::Protocol,
                    format!("{name} needs caps::SHELL: connect to the shell socket"),
                )),
            );
            return false;
        }
        match msg {
            ClientMsg::BindKey(m) => self.shell_bind_key(token, m),
            ClientMsg::UnbindKey(m) => {
                self.hotkeys.unbind(token, m.id);
                true
            }
            ClientMsg::WindowList(_) => {
                if !self.window_watchers.contains(&token) {
                    self.window_watchers.push(token);
                }
                self.send_window_list(token);
                true
            }
            ClientMsg::Outputs(_) => {
                if !self.output_watchers.contains(&token) {
                    self.output_watchers.push(token);
                }
                self.send_output_list(token);
                true
            }
            ClientMsg::FocusWindow(m) => {
                if let Some(win) = self.window_refs.key_for(m.window) {
                    self.focus_window_for_shell(win);
                }
                true
            }
            ClientMsg::CloseWindow(m) => {
                if let Some(win) = self.window_refs.key_for(m.window) {
                    self.close_window(win);
                }
                true
            }
            ClientMsg::SetWindowStateFor(m) => {
                if let Some(win) = self.window_refs.key_for(m.window) {
                    self.set_state(win, clients::scene_state(m.state));
                }
                true
            }
            ClientMsg::Lock(_) => self.shell_lock(token),
            ClientMsg::Unlock(_) => self.shell_unlock(token),
            // Subscribes on receipt; applied (and answered) at `settle`,
            // see `pending_overview`.
            ClientMsg::SetOverview(m) => {
                if !self.overview_watchers.contains(&token) {
                    self.overview_watchers.push(token);
                }
                self.pending_overview.push((token, m.request));
                true
            }
            // The four that name the sender's *own* window are buffered and
            // applied at its `Commit`, by `Server::apply_shell_op`: a bar
            // sends `CreateWindow` and `SetAnchor` in one transaction, so an
            // anchor applied here would be looking for a window that does not
            // exist yet. Only the privilege check belongs on receipt.
            other => {
                debug_assert!(
                    matches!(
                        other,
                        ClientMsg::SetLayer(_)
                            | ClientMsg::SetExclusiveZone(_)
                            | ClientMsg::SetAnchor(_)
                            | ClientMsg::GrabKeyboard(_)
                    ),
                    "{} is not a shell op",
                    other.name()
                );
                let Some(client) = self.wire_clients.get_mut(&token) else {
                    return false;
                };
                client.pending.push(Pending::Msg(Box::new(other)));
                true
            }
        }
    }

    /// Apply one buffered shell op at its client's commit.
    ///
    /// The window was resolved by `clients::apply_msg`, which is also where
    /// a bad `NodeId` or a reserved bit aborted the batch — so by the time
    /// this runs the op is known-good and cannot fail the transaction.
    fn apply_shell_op(&mut self, win: WindowKey, op: shell::WindowOp) {
        match op {
            shell::WindowOp::Layer(layer) => {
                if let Err(e) = self.scene.set_layer(win, layer) {
                    warn!("SetLayer: {e}");
                }
            }
            shell::WindowOp::Zone { edge, px } => {
                self.zones.set_zone(win, edge, px);
                // The work area just changed, so every window *sized by* it
                // has to be re-sized: a maximized window must give the bar
                // its strip immediately, not at the next maximize.
                self.work_area_changed();
            }
            shell::WindowOp::Anchor {
                edges,
                margin,
                output,
            } => {
                self.zones.set_anchor(win, edges, margin, output);
                self.apply_anchor(win);
            }
            shell::WindowOp::Grab(on) => {
                // Who the keyboard goes to is about to change (or be
                // re-asserted): a repeat started for the old recipient
                // ends here.
                self.stop_key_repeat();
                if on {
                    self.grab = Some(win);
                } else if self.grab == Some(win) {
                    self.grab = None;
                }
            }
        }
    }

    /// `Lock` from a shell client. See [`lock`] for the rules.
    fn shell_lock(&mut self, token: u64) -> bool {
        let was_locked = self.lock.is_locked();
        // The lock screen must not come up over a desktop of thumbnails.
        self.leave_overview(None);
        match self.lock.claim(token) {
            Ok(how) => {
                match how {
                    lock::Claimed::Locked => info!("session locked"),
                    lock::Claimed::TookOver => info!("lock taken over by a new owner"),
                    lock::Claimed::Already => {}
                }
                if !was_locked {
                    // The hovered window loses the pointer, as it would on a
                    // VT switch. `PointerLeave` is not input, so it still
                    // reaches a window the gate is about to hide.
                    self.release_pointer_for_lock();
                }
                self.sync_admit();
                if !was_locked {
                    // What the unlock hands the keyboard back to: the focused
                    // window, unless that is the lock screen's own (it made
                    // its window before sending `Lock`), in which case the
                    // application that had it before that.
                    self.focus_before_lock = match self.focus {
                        Some(w) if self.scene.admits_window(w) => self
                            .wm
                            .mru()
                            .iter()
                            .copied()
                            .find(|w| !self.scene.admits_window(*w) && self.focusable(*w)),
                        other => other,
                    };
                }
                // A focus the lock does not admit is taken away (the window
                // is told), and the lock screen gets the keyboard if it
                // already has a window: one made before `Lock`, or while
                // the lock had no owner, was refused the focus then and
                // would otherwise never get it.
                if let Some(w) = self.focus
                    && !self.scene.admits_window(w)
                {
                    self.set_focus(None);
                }
                if self.focus.is_none() {
                    let own = self
                        .wm
                        .mru()
                        .iter()
                        .copied()
                        .find(|w| self.scene.admits_window(*w) && self.focusable(*w));
                    if own.is_some() {
                        self.set_focus(own);
                    }
                }
                true
            }
            Err(e) => {
                self.disconnect(token, Some((0, ErrorCode::Protocol, e.to_string())));
                false
            }
        }
    }

    /// `Unlock` from a shell client: the owner's, or fatal.
    fn shell_unlock(&mut self, token: u64) -> bool {
        match self.lock.release(token) {
            Ok(()) => {
                info!("session unlocked");
                self.sync_admit();
                self.hotkeys.reset();
                // The window that had the keyboard gets it back, if it is
                // still there and still wants it.
                if let Some(w) = self.focus_before_lock.take()
                    && self.focusable(w)
                {
                    self.set_focus(Some(w));
                }
                true
            }
            Err(e) => {
                self.disconnect(token, Some((0, ErrorCode::Protocol, e.to_string())));
                false
            }
        }
    }

    /// Everything the pointer is in the middle of, ended for a lock: the
    /// hovered window is told the pointer left, a drag in flight is dropped
    /// and the shell's tap state is reset. The next motion re-hit-tests
    /// against the admitted windows only.
    fn release_pointer_for_lock(&mut self) {
        // The lock takes the pointer: the grab goes, and the release
        // that would have ended it is never delivered.
        self.pointer.grab = None;
        if let Some(left) = self.pointer.over.take() {
            let time_ns = monotonic_ns();
            self.send_to_window(left, |id| {
                ServerMsg::PointerLeave(msg::PointerLeave {
                    window: id,
                    time_ns,
                })
            });
        }
        let _ = self.wm.end_drag();
        self.dnd_step(data::Dnd::cancel);
        self.hotkeys.reset();
        self.hotkey_pending = None;
    }

    /// Point the scene's [`Admit`](nitro_scene::Admit) filter at the lock
    /// state: everyone when unlocked, the owner's windows when locked, and
    /// nobody's while the lock has no owner. The change damages every
    /// output, and the frame that follows is the one that shows it.
    fn sync_admit(&mut self) {
        use nitro_scene::Admit;
        let admit = match self.lock {
            lock::Lock::Unlocked => Admit::All,
            lock::Lock::Locked { owner: None } => Admit::Nobody,
            lock::Lock::Locked { owner: Some(t) } => self
                .wire_clients
                .get(&t)
                .map_or(Admit::Nobody, |c| Admit::Only(c.id)),
        };
        self.scene.set_admit(admit);
    }

    /// `BindKey`: claim a server-global chord.
    fn shell_bind_key(&mut self, token: u64, m: msg::BindKey) -> bool {
        match self.hotkeys.bind(token, m.id, m.mods, m.keysym) {
            Ok(()) => true,
            Err(e) => {
                let detail = match e {
                    shell::BindError::ReservedBits => {
                        format!("BindKey: reserved modifier bits in {:#x}", m.mods)
                    }
                    shell::BindError::BadTap => {
                        "BindKey: a bare-modifier tap must name exactly one modifier".to_owned()
                    }
                    shell::BindError::Reserved => format!(
                        "BindKey: keysym {:#x} with mods {:#x} is a compositor chord",
                        m.keysym, m.mods
                    ),
                    shell::BindError::Taken => format!(
                        "BindKey: keysym {:#x} with mods {:#x} is already bound",
                        m.keysym, m.mods
                    ),
                };
                self.disconnect(token, Some((0, ErrorCode::Protocol, detail)));
                false
            }
        }
    }

    /// The window a live keyboard grab points at, if any.
    ///
    /// A grab on a window that is no longer *showing* does not count, and is
    /// dropped here rather than tracked: see [`Server::showing`] for why
    /// both this and the exclusive zone hang on that one predicate. Checked
    /// lazily because the scene does not report visibility changes and
    /// polling one node on each key is cheaper than watching every commit.
    fn grab_target(&mut self) -> Option<WindowKey> {
        let win = self.grab?;
        if !self.showing(win) {
            self.grab = None;
            return None;
        }
        Some(win)
    }

    /// Whether this key must be withheld rather than delivered to `window`.
    ///
    /// A shell that binds a hotkey learns its binding fired over the wire,
    /// so between the server writing the `HotKey` and the shell's commit
    /// landing there is a round trip in which the shell holds no grab and
    /// keys are routed by focus — into whatever application the user was
    /// last in. Tapping Super and typing "quit" fast enough typed it into
    /// the calculator, which quit.
    ///
    /// The fix is stated without reference to grabs, so it holds for any
    /// shell and not just the launcher: **once a binding fires, no other
    /// client sees a key until its client has had a turn.** The wait ends
    /// at the shell's next commit (it answered, grab or no grab), at
    /// [`HOTKEY_ANSWER`] (it is not going to), or when the shell goes
    /// away or the modifier state is reset.
    ///
    /// Keys are *dropped*, not queued and replayed: a replay would arrive
    /// out of order with the `HotKey` the shell already has, would have to
    /// be re-resolved against a keymap that may have moved, and would mean
    /// deciding what to do when the shell declines to show. Losing the
    /// keystroke that raced a trigger is what a user expects of a trigger;
    /// delivering it to the previous window is the bug.
    ///
    /// Both presses and releases are withheld, or a client would see a
    /// release for a press it never got. The pending client itself is
    /// exempt — if it already holds a grab from an earlier show, its keys
    /// keep flowing.
    fn withheld(&mut self, window: WindowKey) -> bool {
        let Some((token, deadline)) = self.hotkey_pending else {
            return false;
        };
        if Instant::now() >= deadline {
            self.hotkey_pending = None;
            return false;
        }
        // Not withheld from the client we are waiting for.
        self.wire_clients
            .get(&token)
            .is_none_or(|c| c.window_id(window).is_none())
    }

    /// Put an anchored window where its anchor says, resizing it if the
    /// anchor spans an axis.
    ///
    /// Against the output's **full** logical rectangle, not its work area:
    /// a bar that anchored into the work area would be pushed off the screen
    /// by its own exclusive zone.
    ///
    /// An anchor that names an output moves the window there first. That
    /// move is deliberately *not* in `set_frame_rect` ("a resize never
    /// changes which output a window is on"), so it is explicit here. An
    /// output that is not connected — unplugged between the `OutputInfo`
    /// and the commit, or after the anchor was set — falls back to the
    /// window's current one, which after `migrate_orphans` is the primary:
    /// that fall-back *is* the re-homing on unplug, and the same anchor
    /// springs back if the named output returns (ids are never reused, so
    /// it cannot).
    fn apply_anchor(&mut self, win: WindowKey) {
        let Some(a) = self.zones.anchor(win) else {
            return;
        };
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let size = info.frame_size();
        let current = info.output();
        let overlay = info.layer() == nitro_scene::Layer::Overlay;
        let named = a.output.filter(|id| self.scene.output_info(*id).is_some());
        let output = named.or(current).or_else(|| self.primary_output());
        let Some(output) = output else {
            // No output yet; `sync_outputs` re-applies anchors when one
            // appears, so the window simply waits where it is.
            return;
        };
        let Some((rect, scale)) = self.scene.output_info(output) else {
            return;
        };
        let moved = current.is_some_and(|c| c != output);
        if moved && let Err(e) = self.scene.place_window(win, Some(output), Point::ZERO) {
            warn!("moving an anchored window to its output: {e}");
            return;
        }
        let s = if scale > 0.0 { scale } else { 1.0 };
        let origin = self.desktop_origin(output);
        let full = Rect::new(origin.x, origin.y, rect.w as f32 / s, rect.h as f32 / s);
        let target = if overlay {
            let work = self.local_work_area(output);
            let work = Rect::new(work.x + origin.x, work.y + origin.y, work.w, work.h);
            shell::overlay_anchor_rect(full, work, size, a)
        } else {
            shell::anchor_rect(full, size, a)
        };
        self.set_frame_rect(win, target);
        if !moved {
            return;
        }
        // The window's `WindowInfo.output` changed for the watchers, and if
        // it carries a zone, that strip left one output and arrived on the
        // other: maximized windows on both reflow and `OUTPUTS` watchers
        // get fresh work areas. Order: place → frame → notify → work area.
        self.notify_window(win);
        if self.zones.zone(win).is_some() {
            self.work_area_changed();
        }
        if let Some(ov) = self.overview_output()
            && (ov == output || Some(ov) == current)
            && overview::wants_thumb(&self.scene, win)
        {
            self.relayout_overview();
        }
    }

    /// Re-apply every anchor. Called when an output's geometry changes, so a
    /// bar keeps spanning across a mode change or a hotplug.
    fn reflow_anchors(&mut self) {
        let anchored: Vec<WindowKey> = self.zones.anchored().map(|(w, _)| w).collect();
        for win in anchored {
            self.apply_anchor(win);
        }
    }

    /// The work area may have changed (an exclusive zone moved, a bar was
    /// hidden or went away). Two duties, kept together so a fifth call
    /// site cannot do one and forget the other:
    ///
    /// 1. re-apply the geometry of every window whose rectangle is
    ///    *derived* from the work area. Only `Maximized` windows: a
    ///    floating window is where the user put it, and a fullscreen one
    ///    covers the output work area or not. This is also why a zone is
    ///    cheap — the reflow is proportional to the number of maximized
    ///    windows, not to the number of windows;
    /// 2. tell the output watchers that listed `OUTPUTS`: one
    ///    `OutputWorkArea` per output and nothing else — no `OutputInfo`,
    ///    no `OutputsEnd`, because this is an update, not a snapshot. That
    ///    cadence is the reason the work area is its own message.
    fn work_area_changed(&mut self) {
        self.reflow_maximized();
        self.reflow_overlay_anchors();
        // Reflowed windows moved and resized under the pointer.
        self.pointer_refresh = true;
        if self.output_watchers.is_empty() {
            return;
        }
        let areas: Vec<msg::OutputWorkArea> = self
            .output_snapshot()
            .into_iter()
            .filter_map(|(_, area)| area)
            .collect();
        for token in self.output_watchers.clone() {
            if !self.output_gate(token).1 {
                continue;
            }
            let Some(client) = self.wire_clients.get_mut(&token) else {
                continue;
            };
            for area in &areas {
                client.send(&ServerMsg::OutputWorkArea(*area));
            }
        }
    }

    /// Re-apply the anchor of every top-anchored `Overlay` window, the
    /// ones [`shell::overlay_anchor_rect`] places against the work area:
    /// the launcher's field follows a bar that appears, resizes or hides.
    /// Part of duty 1 of [`Server::work_area_changed`].
    fn reflow_overlay_anchors(&mut self) {
        use nitro_wire::types::anchor;
        let wins: Vec<WindowKey> = self
            .zones
            .anchored()
            .filter(|(_, a)| a.edges & (anchor::TOP | anchor::BOTTOM) == anchor::TOP)
            .map(|(w, _)| w)
            .filter(|w| {
                self.scene
                    .window_info(*w)
                    .is_ok_and(|i| i.layer() == nitro_scene::Layer::Overlay)
            })
            .collect();
        for win in wins {
            self.apply_anchor(win);
        }
    }

    /// Duty 1 of [`Server::work_area_changed`].
    fn reflow_maximized(&mut self) {
        let maximized: Vec<WindowKey> = self
            .wire_clients
            .values()
            .flat_map(|c| c.windows.values().copied())
            .filter(|w| {
                self.scene
                    .window_info(*w)
                    .is_ok_and(|i| i.state() == WindowState::Maximized)
            })
            .collect();
        for win in maximized {
            self.apply_state_geometry(win, WindowState::Maximized);
        }
    }

    // ------------------------------------------------ the window list

    /// Build one window's `WindowInfo`, minting its server-global id.
    fn window_info_msg(&mut self, win: WindowKey) -> Option<msg::WindowInfo> {
        if self.scene.window_info(win).ok()?.is_popup() || self.drag_icons.contains(&win) {
            return None;
        }
        let id = self.window_refs.id_for(win);
        let focused = self.focus == Some(win);
        let info = self.scene.window_info(win).ok()?;
        Some(msg::WindowInfo {
            window: id,
            state: clients::wire_state(info.state()),
            focused,
            // `u32::MAX` rather than 0 for "nowhere": output 0 is a real
            // output, and a shell must be able to tell an unplaced window
            // from one on the primary screen.
            output: info.output().map_or(u32::MAX, |o| o.0),
            // The layer a task list filters on: only `Normal` windows are
            // applications, and a bar that does not filter lists the
            // wallpaper and the launcher as windows.
            layer: clients::wire_layer(info.layer()),
            app_id: info.app_id().to_owned(),
            title: info.title().to_owned(),
        })
    }

    /// Every window the server knows about, in a stable order.
    ///
    /// Ordered by the clients' own window maps rather than by z-order: a
    /// bar's task list should not reshuffle itself every time the user
    /// raises a window, and a shell that wants stacking order can ask for
    /// it when there is a reason to.
    fn all_windows(&self) -> Vec<WindowKey> {
        let mut out: Vec<WindowKey> = self
            .wire_clients
            .values()
            .flat_map(|c| c.windows.values().copied())
            // A menu is not an application window: a bar's task list must
            // not sprout an entry per open menu.
            .filter(|w| self.scene.window_info(*w).is_ok_and(|i| !i.is_popup()))
            // Nor is a drag icon.
            .filter(|w| !self.drag_icons.contains(w))
            .collect();
        out.sort_unstable_by_key(|w| (w.index(), w.generation()));
        out
    }

    /// Answer a `WindowList`: a snapshot, then `WindowListEnd`.
    fn send_window_list(&mut self, token: u64) {
        let windows = self.all_windows();
        let infos: Vec<msg::WindowInfo> = windows
            .into_iter()
            .filter_map(|w| self.window_info_msg(w))
            .collect();
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        for info in infos {
            client.send(&ServerMsg::WindowInfo(info));
        }
        client.send(&ServerMsg::WindowListEnd(msg::WindowListEnd));
    }

    /// Tell every subscribed shell client that a window changed.
    ///
    /// Called from the places that change something in a `WindowInfo`:
    /// focus, state, title, app id, placement. The fast path is the first
    /// line — a desktop with no shell running pays one `is_empty`.
    fn notify_window(&mut self, win: WindowKey) {
        if self.window_watchers.is_empty() {
            return;
        }
        let Some(info) = self.window_info_msg(win) else {
            return;
        };
        for token in self.window_watchers.clone() {
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::WindowInfo(info.clone()));
            }
        }
    }

    /// Tell every subscribed shell client that a window is gone, and retire
    /// its id.
    fn notify_window_gone(&mut self, win: WindowKey) {
        let Some(id) = self.window_refs.forget(win) else {
            // Never named to any shell, so nothing to retract.
            return;
        };
        for token in self.window_watchers.clone() {
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::WindowGone(msg::WindowGone { window: id }));
            }
        }
    }

    // ---------------------------------------------------------- outputs

    /// Every connected output as an `OutputInfo`, in device-x order — the
    /// same left-to-right order `sync_outputs` laid them out in.
    fn output_infos(&self) -> Vec<msg::OutputInfo> {
        let kms = self.backend.outputs();
        let mut out: Vec<msg::OutputInfo> = self
            .outputs
            .iter()
            .filter_map(|o| {
                let (rect, scale) = self.scene.output_info(o.scene_id)?;
                // Name and refresh come from the backend's own description:
                // `OutputState` keeps the refresh as a *period*, and turning
                // it back into millihertz would round the number the client
                // is told away from the mode the kernel actually set.
                let info = kms.iter().find(|i| i.id == o.kms_id);
                Some(msg::OutputInfo {
                    id: o.scene_id.0,
                    w: o.width,
                    h: o.height,
                    scale,
                    x: rect.x,
                    y: rect.y,
                    refresh_mhz: info.map_or(0, |i| i.refresh_mhz),
                    name: info.map_or_else(String::new, |i| i.name.clone()),
                })
            })
            .collect();
        out.sort_unstable_by_key(|o| o.x);
        out
    }

    /// An output's work area as the wire carries it: **global device
    /// pixels**, like `OutputInfo.x/y/w/h`.
    ///
    /// The origin is the scene's device `rect`, *not* `desktop_origin`
    /// (which is the logical desktop space), so the work area and its
    /// `OutputInfo` share an origin by construction whatever the scale.
    /// The zone subtraction is `local_work_area`'s, not re-implemented
    /// here: a hidden bar reserves nothing on the wire either.
    fn work_area_msg(&self, id: SceneOutputId) -> Option<msg::OutputWorkArea> {
        let (rect, scale) = self.scene.output_info(id)?;
        let s = if scale > 0.0 { scale } else { 1.0 };
        let area = self.local_work_area(id);
        #[allow(clippy::cast_possible_truncation)]
        let px = |v: f32| (v * s).round() as i32;
        Some(msg::OutputWorkArea {
            id: id.0,
            area: nitro_core::IRect::new(
                rect.x + px(area.x),
                rect.y + px(area.y),
                px(area.w),
                px(area.h),
            ),
        })
    }

    /// Every output as the pair a snapshot sends for it, in
    /// `output_infos` order. One place decides the interleaving.
    fn output_snapshot(&self) -> Vec<(msg::OutputInfo, Option<msg::OutputWorkArea>)> {
        self.output_infos()
            .into_iter()
            .map(|info| {
                let area = self
                    .outputs
                    .iter()
                    .find(|o| o.scene_id.0 == info.id)
                    .and_then(|o| self.work_area_msg(o.scene_id));
                (info, area)
            })
            .collect()
    }

    /// What this watcher may be sent of the output messages: `(any, work
    /// areas)`.
    ///
    /// `docs/wire.md` § Capability opt-in rule 1: the four output
    /// messages belong to `SHELL` **or** `OUTPUTS`, and `OutputWorkArea`
    /// is new with `OUTPUTS`. A shell client (`SHELL`, grandfathered)
    /// gets `OutputInfo`/`OutputsEnd`/`OutputGone` regardless, but a work
    /// area only once it listed `OUTPUTS` — which keeps `nitro-bar`, a
    /// pre-`ClientCaps` client, byte-identical. An ordinary watcher that
    /// narrowed `OUTPUTS` away with a second `ClientCaps` (rule 5) stays
    /// subscribed but is sent nothing until it widens again.
    fn output_gate(&self, token: u64) -> (bool, bool) {
        let listed = self
            .wire_clients
            .get(&token)
            .is_some_and(|c| c.client_caps & nitro_wire::types::caps::OUTPUTS != 0);
        (Self::is_shell(token) || listed, listed)
    }

    /// Answer an `Outputs` or a `ListOutputs`: a snapshot, then
    /// `OutputsEnd`. The same messages for both, so one function.
    fn send_output_list(&mut self, token: u64) {
        let (any, areas) = self.output_gate(token);
        if !any {
            return;
        }
        let snapshot = self.output_snapshot();
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        for (info, area) in snapshot {
            client.send(&ServerMsg::OutputInfo(info));
            if let Some(area) = area.filter(|_| areas) {
                client.send(&ServerMsg::OutputWorkArea(area));
            }
        }
        client.send(&ServerMsg::OutputsEnd(msg::OutputsEnd));
    }

    /// `ListOutputs` (M5-D): the output snapshot for an unprivileged
    /// client, and a hotplug subscription. Returns whether the client
    /// survives.
    ///
    /// Grants output enumeration and nothing else: it shares the
    /// subscription set and the snapshot with the shell's `Outputs`, but
    /// no code path with `handle_shell_msg`.
    ///
    /// `OUTPUTS` must be listed in `ClientCaps` (`docs/wire.md` rule 3):
    /// a client asking for outputs while claiming not to understand the
    /// answer is confused. That holds for a **shell** client too — rule 3
    /// keys on `ClientCaps`, not on the socket; a shell that wants the
    /// list without opting in sends `Outputs`.
    fn list_outputs(&mut self, token: u64) -> bool {
        let listed = self
            .wire_clients
            .get(&token)
            .map(|c| c.client_caps & nitro_wire::types::caps::OUTPUTS != 0);
        match listed {
            None => return false,
            Some(false) => {
                self.disconnect(
                    token,
                    Some((
                        0,
                        ErrorCode::Protocol,
                        "ListOutputs needs `OUTPUTS` listed in ClientCaps".to_owned(),
                    )),
                );
                return false;
            }
            Some(true) => {}
        }
        if !self.output_watchers.contains(&token) {
            self.output_watchers.push(token);
        }
        self.send_output_list(token);
        true
    }

    /// Tell every subscribed shell client the output list changed.
    ///
    /// A hotplug sends the *whole* list rather than a diff: outputs are few,
    /// their positions are relative to each other (unplugging the left one
    /// moves every other), and a diff a shell had to reassemble would be a
    /// second source of truth about the layout.
    fn notify_outputs(&mut self, gone: &[u32]) {
        if self.output_watchers.is_empty() {
            return;
        }
        let snapshot = self.output_snapshot();
        for token in self.output_watchers.clone() {
            let (any, areas) = self.output_gate(token);
            if !any {
                continue;
            }
            let Some(client) = self.wire_clients.get_mut(&token) else {
                continue;
            };
            for id in gone {
                client.send(&ServerMsg::OutputGone(msg::OutputGone { id: *id }));
            }
            for (info, area) in &snapshot {
                client.send(&ServerMsg::OutputInfo(info.clone()));
                if let Some(area) = area.filter(|_| areas) {
                    client.send(&ServerMsg::OutputWorkArea(area));
                }
            }
            client.send(&ServerMsg::OutputsEnd(msg::OutputsEnd));
        }
    }

    /// Apply a client's transaction. Returns whether the client survives.
    #[allow(clippy::too_many_lines)] // One transaction, applied in order: the steps share the commit's state and ordering rules.
    fn commit(&mut self, token: u64, serial: u32) -> bool {
        // This is the answer a deferred flip was waiting for. Noted before
        // the transaction is applied rather than after: the client has
        // spoken either way, and a transaction that turns out to be fatal
        // must not leave the cursor held hostage to a dead connection.
        self.defer.forget(token);
        // And it is the turn a withheld key was waiting for: the shell has
        // answered its hotkey, so ordinary routing resumes from here
        // whether or not it took a grab. Cleared before the transaction is
        // applied, for the same reason the flip is.
        if self.hotkey_pending.is_some_and(|(t, _)| t == token) {
            self.hotkey_pending = None;
        }
        let Some(mut client) = self.wire_clients.remove(&token) else {
            return false;
        };
        // The buffer descriptors arrived with their messages; hand them to
        // the client's map once the scene has minted the keys.
        let (scene, text, icons) = (&mut self.scene, &mut self.text, &mut self.icons);
        let result = clients::apply(&mut client, scene, text, icons, serial);
        let outcome = match result {
            Ok(o) => o,
            Err(ApplyError { code, detail }) => {
                warn!("wire client {}: {detail}", client.id.0);
                self.wire_clients.insert(token, client);
                self.disconnect(token, Some((serial, code, detail)));
                return false;
            }
        };
        // No buffer bookkeeping here since #569: the scene holds each
        // buffer's mapping, so `BufferDamage` needs no re-read and
        // `DestroyBuffer` releases the pages by dropping the store (its
        // `Drop` is the `munmap`).
        // Every node this transaction (re)shaped gets its measured size
        // back. Sent after the batch was applied, so a client that set the
        // text of several nodes in one commit sees one message per node and
        // in the order it asked for them.
        for (node, metrics) in outcome.text_metrics {
            debug_assert_eq!(metrics.node, node);
            client.send(&ServerMsg::TextMetrics(metrics));
        }
        report_bad_icons(&mut client, serial, outcome.bad_icons);
        for (node_id, win) in outcome.new_windows {
            self.place_new_window(&mut client, node_id, win);
        }
        // Popups after `new_windows`, so a parent created in the same
        // batch is already placed, and before `state_requests`, so a
        // maximize in the same batch re-places the chain rather than
        // racing it.
        for (node_id, win, info) in outcome.new_popups {
            self.map_popup(&mut client, node_id, win, info);
        }
        let repositioned = outcome.repositioned_popups;
        self.pending_drag_starts
            .extend(outcome.start_drags.into_iter().map(|d| (token, d)));
        // Before `closed_windows`, so an icon destroyed in the same batch
        // drops its offset. A mid-drag change moves the icon at once; only
        // scene, pointer and drag state are touched, so the lifted-out
        // client does not matter.
        for (icon, offset) in outcome.drag_icon_offsets {
            self.drag_icon_offsets.insert(icon, offset);
            if self
                .dnd
                .as_ref()
                .is_some_and(|d| d.grabbing() && d.icon == Some(icon))
            {
                self.place_drag_icon();
            }
        }
        client.frame_requests.extend(outcome.frame_requests);
        // Scanout buffers the batch destroyed (#3914): the scene already
        // dropped the mapping; the backend defers the free while one is
        // still on screen.
        for k in outcome.freed_scanout {
            self.backend.free_buffer(k);
        }
        // Client dma-bufs this batch registered (#3918): imported as KMS
        // framebuffers when the output backend has planes to put them on,
        // the hook the planes module (#3899) reads. A refusal is not an
        // error: the buffer is shown on the CPU path or as a placeholder.
        for (id, import) in outcome.dmabuf_imports {
            if self.gpu.enabled()
                && let Some(h) = client.buffers.get(&id)
                && let Some(src) = gpu_source(&import)
            {
                self.gpu.sources.insert(h.key, src);
            }
            let has_planes = self
                .outputs
                .first()
                .is_some_and(|o| !self.backend.planes(o.kms_id).is_empty());
            if !has_planes {
                continue;
            }
            let fds: Vec<_> = import.fds.iter().map(AsFd::as_fd).collect();
            match self.backend.import_buffer(&import.desc, &fds) {
                Ok(k) => {
                    if let Some(h) = client.buffers.get_mut(&id) {
                        h.scanout = Some(k);
                    } else {
                        self.backend.free_buffer(k);
                    }
                }
                Err(e) => {
                    debug!("dma-buf {}: KMS import refused: {e}", id.raw());
                    self.dmabuf_kms_refused += 1;
                }
            }
        }
        let surfaces_set = outcome.surfaces_set;
        if client.client_caps & nitro_wire::types::caps::SURFACE != 0 {
            let dma = client.client_caps & nitro_wire::types::caps::DMABUF != 0;
            for (id, key) in outcome.new_surfaces {
                self.surface_hints.track(key, token, id);
                if dma {
                    self.feedback.track(key, token, id);
                }
            }
        }
        for win in outcome.closed_windows {
            self.forget_closed(win);
        }
        // The title bar is the server's, so a retitle is a repaint the
        // client never asks for and never sees. An app id is an icon
        // name (`docs/shell.md`), so a window that renamed itself
        // re-resolves its frame icon the same way — the server's own
        // node, so no message and no round trip in either direction.
        for win in outcome.retitled {
            self.retitle(win);
        }
        for win in outcome.reiconed {
            self.reicon(win);
        }
        let relisted = outcome.relisted;
        let shell_ops = outcome.shell_ops;
        let visibility_changed = outcome.visibility_changed;
        // State requests are applied last, after every geometry mutation
        // in the batch: `Maximized` has to win over the client's own
        // `SetBounds`, not race it. `set_state` reaches the owning client
        // by token, so the client goes back in the map first and the rest
        // of this function works through it.
        let has_states = !outcome.state_requests.is_empty();
        self.wire_clients.insert(token, client);
        // A committed `SetSurface` wins over a queued frame: the frame is
        // dropped and its buffer released now (unless something shows it).
        for node in surfaces_set {
            let dropped = self.latch.cancel(node);
            self.drop_frames(dropped);
        }
        // Imports this batch destroyed (#3904): a frame the client still
        // had queued through one is dropped and released.
        for id in outcome.dropped_imports {
            if let Some(node) = self.shares.drop_import(token, id) {
                self.end_import(token, node);
            }
        }
        // The shell ops go *before* the state requests and after everything
        // else, for the same reason `SetWindowState` is last: an anchor
        // decides a window's whole rectangle, so it has to win over the
        // client's own `SetBounds` in the same batch — and a `Maximized`
        // asked for in that batch has to win over the anchor, which is the
        // shell deliberately handing its window to the window manager.
        for (win, op) in shell_ops {
            self.apply_shell_op(win, op);
        }
        // Repositions with the client back in the map, so the `Configure`
        // for the popup and for any submenu it drags along reaches it.
        for (win, info) in repositioned {
            self.reposition_popup(win, info);
        }
        for (win, state) in outcome.state_requests {
            self.set_state(win, state);
        }
        // A window that showed or hid may have been a bar holding a strip of
        // the desktop, and a zone is released the moment its bar stops
        // showing (`Server::showing`). Guarded on the zone map so a desktop
        // with no shell running pays one `is_empty` per transaction that
        // touched visibility at all.
        self.visibility_changed(visibility_changed);
        // Announced with every client back in the map, because a watcher is
        // a *different* client than the one that committed: notifying while
        // this one was lifted out would be fine, but notifying after also
        // reports the state changes above, and a bar wants one message with
        // the final truth rather than two with a transient.
        for win in relisted {
            self.notify_window(win);
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
        // Whatever this transaction did may have changed what is under a
        // still pointer: a window's first content (a window is only
        // hit-testable once something in it paints, which for Chromium is
        // a later commit than its `CreateWindow`), a window closed or
        // hidden, a `SetBounds` on a window root, a raise. Keying this on
        // message kinds would miss the first of those, and it is the one
        // #3886 was about: the wheel went nowhere until the pointer moved.
        // One z-order walk per commit, and `refresh_pointer_over` sends
        // nothing unless the window under the pointer actually changed.
        self.pointer_refresh = true;
        true
    }

    /// Place a newly created window: decorate it, centred-cascade it into
    /// the primary output's work area and tell the client the size, scale
    /// and output it got.
    fn place_new_window(&mut self, client: &mut WireClient, node_id: NodeId, win: WindowKey) {
        let Some(scene_id) = self.primary_output() else {
            // No output yet (every connector unplugged, or a hotplug still
            // in flight). The window is real and owns its nodes; it simply
            // has nowhere to be. `sync_outputs` drains this list when an
            // output appears, so the client gets its `Configure` then.
            self.unplaced.push((client.id, win));
            return;
        };
        // Decorate before placing: the frame changes the window's outer
        // size, and the placement has to know it to centre the thing the
        // user actually sees.
        self.decorate(win);
        let area = self.local_work_area(scene_id);
        let scale = self.scene.output_info(scene_id).map_or(1.0, |(_, s)| s);
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let (size, frame) = (info.size(), info.frame_size());
        let position = wm::place(self.wm.next_placement(), frame, area);
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
        // A new window is a new entry in every shell's list. Announced here
        // rather than at creation because this is the first moment it has an
        // output and a place to report.
        self.notify_window(win);
        // A window mapped on the output in overview joins the grid. After
        // the `Configure` and the announcement: an undecorated thumbnail
        // is *moved* to its slot, and that position is not the client's.
        if self.overview_output() == Some(scene_id) && overview::wants_thumb(&self.scene, win) {
            self.relayout_overview();
        }
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
        // A shell that died mid-trigger is not going to answer it, and the
        // keyboard must not stay held for a token that is about to be
        // reused.
        if self.hotkey_pending.is_some_and(|(t, _)| t == token) {
            self.hotkey_pending = None;
        }
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
                self.set_pointer_over(None);
            }
            if self.pointer.grab == Some(win) {
                self.end_pointer_grab();
            }
            self.touch_targets.retain(|_, (w, _)| *w != win);
            self.popup_window_gone(win);
            if let Err(e) = self.scene.destroy_window(id, win) {
                warn!("destroying window of client {}: {e}", id.0);
            }
            self.forget_window(win);
            // The window below it, if any, is what the pointer is over now.
            self.pointer_refresh = true;
        }
        for held in client.buffers.values().copied().collect::<Vec<_>>() {
            // Dropping the scene's buffer drops its mapping, which is the
            // `munmap`. Since #569 there is no descriptor to release
            // alongside it: `Mapping::map` closed the client's fd the
            // moment the pages were mapped.
            let _ = self.scene.destroy_buffer(id, held.key);
            // A server-allocated scanout buffer (#3914) goes back to the
            // backend after its mapping is gone.
            if let Some(k) = held.scanout {
                self.backend.free_buffer(k);
            }
        }
        // Every shaped run the client's nodes held. The scene's destroy
        // walk drops the nodes, but the runs live in the text store, which
        // knows them only by owner — so this is the one place they are
        // freed, and a shell that restarts its clients would otherwise leak
        // a glyph vector per label per restart.
        self.text.release_owner(id.0);
        let _ = self.latch.forget_client(token);
        self.fences.forget_client(&self.epoll, token);
        self.surface_hints.forget_client(token);
        self.feedback.forget_client(token);
        // Its exports die with it; their importers hear at once. Its
        // imports simply go (the tokens stay valid for a restart).
        for r in self.shares.forget_client(token) {
            self.revoke_import(r);
        }
        for output in &mut self.outputs {
            output.painting.retain(|(c, _)| *c != id.0);
            output.in_flight.retain(|(c, _)| *c != id.0);
        }
        debug!("wire client {} disconnected", id.0);
        // Whatever this client held as a *shell*: its hotkey bindings (or the
        // launcher's Super would stay swallowed after the launcher died), its
        // subscriptions, and any grab it still had. Its windows' zones and
        // anchors went with `forget_window` above.
        self.hotkeys.forget_client(token);
        // A lock owner that goes away leaves the session locked with nobody
        // holding it: nothing is drawn but the background until a new lock
        // screen takes it over. A crash is never a way in.
        if self.lock.forget(token) {
            warn!("the lock owner went away; the session stays locked");
            self.sync_admit();
        }
        self.window_watchers.retain(|t| *t != token);
        self.output_watchers.retain(|t| *t != token);
        self.overview_watchers.retain(|t| *t != token);
        self.pending_overview.retain(|(t, _)| *t != token);
        // The clipboard: requests this client owed are answered at EOF so
        // their requesters see an end rather than a hang, and a selection
        // it owned is cleared for everyone. After `wire_clients.remove`, so
        // nothing is queued to the dying client.
        let (was_owner, owed) = self.data.forget_client(token);
        for t in owed {
            self.answer_eof(t.requester, t.reply_to);
        }
        if was_owner {
            self.broadcast_offer();
        }
        // Drag and drop: a source gone ends the drag (the target is told),
        // a target gone leaves it over nothing or fails the drop. After
        // the clipboard, which already answered what this client owed.
        self.pending_drag_starts.retain(|(t, _)| *t != token);
        self.dnd_step(|d| d.forget_client(token));
        // Dropping the stream removes it from the epoll set.
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
        let Some(window) = self
            .scene
            .windows_front_to_back(scene_output)
            .find(|w| !self.is_scrim(*w))
        else {
            return protocol::err_reply("no windows");
        };
        self.set_focus(Some(window));
        protocol::ok_reply()
    }

    /// Answer a `shot`: the pixels of one output, `XRGB8888`.
    ///
    /// Taken from the shadow when there is one. That is both cheaper (no
    /// uncached reads back out of the write-combined mapping) and *more*
    /// honest: the shadow is complete by construction, whereas the front
    /// buffer is only complete because the age-2 rule says so. The two are
    /// byte-identical once a frame has settled, which
    /// `tests/shadow.rs` pins.
    ///
    /// A shadow that has never been painted into would be black, so it is
    /// only trusted after the first commit — before that the front buffer
    /// is the one with real pixels in it.
    fn shot(&mut self, name: Option<&str>) -> Vec<u8> {
        let id = match self.shot_output(name) {
            Ok(id) => id,
            Err(reply) => return reply,
        };
        if let Some(shadow) = self
            .outputs
            .iter()
            .find(|o| o.kms_id == id)
            .and_then(|o| o.shadow.as_ref())
            .filter(|s| s.is_complete())
        {
            return protocol::shot_reply(&Self::honest(shadow.image()));
        }
        match self.backend.read_front(id) {
            Ok(img) => protocol::shot_reply(&Self::honest(img)),
            Err(e) => protocol::err_reply(&e.to_string()),
        }
    }

    /// A screenshot as the user sees it: the screen's premultiplied ARGB
    /// composited over whatever is behind its holes, alpha 255 everywhere.
    fn honest(mut image: nitro_kms::Image) -> nitro_kms::Image {
        // #3897: sample the Surface buffer here when CPU-readable; until
        // Surfaces carry buffers, every hole shows the placeholder.
        let underlay = |_x: u32, _y: u32| frame::HOLE_PLACEHOLDER;
        frame::fill_holes(&mut image, underlay);
        image
    }

    /// Answer a `shot-front`: the same pixels, read off the scanout buffer
    /// whatever the shadow says.
    ///
    /// Test-only, and raw: holes are *not* filled, so byte 3 is the
    /// premultiplied alpha the scanout holds (#3898). It is the only way to
    /// check that the copy out of the shadow put the right bytes in the
    /// buffer the display scans, which
    /// an ordinary `shot` would answer out of the shadow and therefore
    /// could not fail.
    fn shot_front(&mut self, name: Option<&str>) -> Vec<u8> {
        let id = match self.shot_output(name) {
            Ok(id) => id,
            Err(reply) => return reply,
        };
        match self.backend.read_front(id) {
            Ok(img) => protocol::shot_reply(&img),
            Err(e) => protocol::err_reply(&e.to_string()),
        }
    }

    /// Which output a `shot`-shaped request names, or the `err` reply to
    /// send instead.
    fn shot_output(&self, name: Option<&str>) -> Result<KmsOutputId, Vec<u8>> {
        let id = match name {
            Some(n) => self
                .backend
                .outputs()
                .iter()
                .find(|o| o.name == n)
                .map(|o| o.id),
            None => self.backend.outputs().first().map(|o| o.id),
        };
        id.ok_or_else(|| {
            protocol::err_reply(&match name {
                Some(n) => format!("no output named {n}"),
                None => "no outputs".to_owned(),
            })
        })
    }
}

// ------------------------------------------------------------------ popups
//
// A popup is a scene `Window` with a parent link (see
// `Scene::create_popup`); everything here is the server's half: placing it
// against the parent and the work area, the pointer grab, and dismissal.
// `docs/wm.md` § Popups has the rules and the reasons.
impl Server {
    /// Collect `from`'s live popup descendants, parents before children,
    /// into `out` (which is cleared first). `from` itself is not included.
    ///
    /// Walks the server's own index, not the scene's links: a destroyed
    /// parent's scene edge is already gone by the time the server hears.
    fn popups_below(&self, from: WindowKey, out: &mut Vec<WindowKey>) {
        out.clear();
        out.push(from);
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            i += 1;
            for (k, info) in &self.popups {
                if info.parent == p && !info.dismissed && !out.contains(k) {
                    out.push(*k);
                }
            }
        }
        out.remove(0);
    }

    /// Whether `win` is `root` or one of its popup descendants.
    fn in_popup_chain(&self, win: WindowKey, root: WindowKey) -> bool {
        let mut cur = win;
        for _ in 0..=popup::MAX_POPUP_DEPTH {
            if cur == root {
                return true;
            }
            match self.popups.get(&cur) {
                Some(info) => cur = info.parent,
                None => return false,
            }
        }
        false
    }

    /// Compute a popup's rectangle from its stored positioner and put it
    /// there. Returns whether it is placed.
    ///
    /// The anchor rectangle is in the parent's **content** space; the work
    /// area is the parent output's, shell zones subtracted, so a menu
    /// flips away from a bar rather than under it.
    fn place_popup(&mut self, win: WindowKey) -> bool {
        let Some(info) = self.popups.get(&win).copied() else {
            return false;
        };
        let Some((output, origin)) = self
            .scene
            .window_info(info.parent)
            .ok()
            .and_then(|p| Some((p.output()?, p.content_position())))
        else {
            return false;
        };
        let (rect, anchor, gravity, constraint) = popup::fallback(info.anchor_rect).unwrap_or((
            info.anchor_rect,
            info.anchor,
            info.gravity,
            info.constraint,
        ));
        let area = self.local_work_area(output);
        let r = popup::constrain(
            rect.translate(origin.x, origin.y),
            anchor,
            gravity,
            constraint,
            info.size,
            area,
        );
        let size = Size::new(r.w, r.h);
        if self.scene.window_info(win).is_ok_and(|i| i.size() != size)
            && let Err(e) = self.scene.set_window_size(ClientId::SERVER, win, size)
        {
            warn!("resizing a popup: {e}");
        }
        if let Err(e) = self
            .scene
            .place_window(win, Some(output), Point::new(r.x, r.y))
        {
            warn!("placing a popup: {e}");
            return false;
        }
        true
    }

    /// Map a popup a commit just created and tell its client where it
    /// went. `client` is the committing client, lifted out of the map, so
    /// the `Configure` is built against it directly (`place_new_window`'s
    /// reason).
    ///
    /// A parent with no output — every connector gone, or a hotplug in
    /// flight — is a **race**, not a lie: the user clicked, the client sent
    /// `CreatePopup`, and a monitor went away in between. So the popup is
    /// created and immediately dismissed with a `PopupDone`, never refused
    /// with a fatal error, and never parked on `unplaced` (which would
    /// decorate and cascade it like a toplevel).
    fn map_popup(
        &mut self,
        client: &mut WireClient,
        node_id: NodeId,
        win: WindowKey,
        info: popup::PopupInfo,
    ) {
        self.popups.insert(win, info);
        let parent_live = self.popups.get(&info.parent).is_none_or(|p| !p.dismissed);
        // A grabbing menu cannot open mid-drag: the drag holds the pointer.
        // The race path, like a parent with nowhere to be.
        let refused = info.grab && self.dnd_grabbing();
        if !parent_live || refused || !self.place_popup(win) {
            // A submenu of a menu that is already gone goes the same way.
            if let Some(i) = self.popups.get_mut(&win) {
                i.dismissed = true;
            }
            self.pending_popup_done.push(win);
            return;
        }
        if info.grab {
            match self.popup_seat.grab {
                None => self.popup_seat.grab = Some(win),
                Some(root) if self.in_popup_chain(win, root) => {}
                // A grabbing popup outside the chain that holds the grab:
                // there is only one pointer, so the older chain goes.
                Some(root) => {
                    self.dismiss_chain(root);
                    self.popup_seat.grab = Some(win);
                }
            }
            // A grabbing menu that opened on a press ends the press's
            // implicit grab: the press-drag-into-the-menu-release-on-an-item
            // gesture wants ordinary enter/leave from here on. A tooltip
            // (no `GRAB`) mapping mid-drag leaves the grab alone.
            self.end_pointer_grab();
        }
        if let Ok(i) = self.scene.window_info(win)
            && let Some(output) = i.output()
        {
            let scale = self.scene.output_info(output).map_or(1.0, |(_, s)| s);
            client.send(&ServerMsg::Configure(msg::Configure {
                window: node_id,
                size: i.size(),
                position: i.content_position(),
                scale,
                output: output.0,
            }));
        }
        self.pointer_refresh = true;
    }

    /// Apply a `RepositionPopup`: anchor first, then the bounds that follow
    /// from it, which is the order `submenu_view.cc:579-586` sends them in.
    /// A dismissed popup stays dismissed — a reposition must not
    /// resurrect a menu the user already closed.
    fn reposition_popup(&mut self, win: WindowKey, new: popup::PopupInfo) {
        let Some(info) = self.popups.get_mut(&win) else {
            return;
        };
        if info.dismissed {
            return;
        }
        info.anchor_rect = new.anchor_rect;
        info.anchor = new.anchor;
        info.gravity = new.gravity;
        info.constraint = new.constraint;
        if self.place_popup(win) {
            self.configure(win);
            self.reflow_popups(win);
        }
        self.pointer_refresh = true;
    }

    /// Re-place every popup below `parent` after the parent moved or
    /// resized. Re-derived from the stored positioner rather than
    /// dismissed: a tooltip that vanished because its window was nudged
    /// would be the worse answer.
    ///
    /// On the drag path, so it allocates nothing: the walk reuses
    /// `popup_scratch`, and a desktop with no popup pays one `is_empty`.
    fn reflow_popups(&mut self, parent: WindowKey) {
        if self.popups.is_empty() {
            return;
        }
        let mut chain = std::mem::take(&mut self.popup_scratch);
        self.popups_below(parent, &mut chain);
        for &win in &chain {
            if self.place_popup(win) {
                self.configure(win);
            }
        }
        if !chain.is_empty() {
            self.pointer_refresh = true;
        }
        chain.clear();
        self.popup_scratch = chain;
    }

    /// Dismiss `from` and every popup below it: **unmap, then notify**,
    /// deepest first, which is the order Chromium expects
    /// (`xdg_popup.cc:351-358`).
    ///
    /// Unmapping is unplacing — out of every z-order, neither painted nor
    /// hit — and it happens here, synchronously. The `PopupDone` does
    /// **not**: it is enqueued, and `settle` sends it. Never "simplify"
    /// this into a direct `send_to_window`: a dismissal caused by a
    /// client's own `DestroyNode` runs while `commit` holds that client
    /// out of the map, and the message would be silently dropped.
    fn dismiss_chain(&mut self, from: WindowKey) {
        let mut chain = Vec::new();
        self.popups_below(from, &mut chain);
        if self.popups.get(&from).is_some_and(|i| !i.dismissed) {
            chain.insert(0, from);
        }
        for &win in chain.iter().rev() {
            let Some(info) = self.popups.get_mut(&win) else {
                continue;
            };
            info.dismissed = true;
            let pos = self
                .scene
                .window_info(win)
                .map_or(Point::ZERO, nitro_scene::Window::position);
            if let Err(e) = self.scene.place_window(win, None, pos) {
                warn!("unmapping a popup: {e}");
            }
            if self.popup_seat.grab == Some(win) {
                self.popup_seat.grab = None;
            }
            self.pending_popup_done.push(win);
        }
        if !chain.is_empty() {
            self.pointer_refresh = true;
        }
    }

    /// Dismiss every popup chain hanging directly off `win`.
    fn dismiss_popups_of(&mut self, win: WindowKey) {
        if self.popups.is_empty() {
            return;
        }
        let children: Vec<WindowKey> = self
            .popups
            .iter()
            .filter(|(_, i)| i.parent == win && !i.dismissed)
            .map(|(k, _)| *k)
            .collect();
        for child in children {
            self.dismiss_chain(child);
        }
    }

    /// A window is gone (destroyed, or its client disconnected): its
    /// popups go with it, and if it was a popup it leaves the index.
    fn popup_window_gone(&mut self, win: WindowKey) {
        if self.popups.is_empty() {
            return;
        }
        self.dismiss_popups_of(win);
        if self.popups.remove(&win).is_some() {
            self.pointer_refresh = true;
        }
        if self.popup_seat.grab == Some(win) {
            self.popup_seat.grab = None;
        }
        // A client that destroyed its own popup already knows.
        self.pending_popup_done.retain(|w| *w != win);
    }

    /// The window under the pointer right now, computed fresh from the
    /// scene rather than read from `pointer.over`: a popup mapped under a
    /// stationary pointer has produced no motion event yet.
    fn window_under_pointer(&self) -> Option<WindowKey> {
        let point = self.pointer.position();
        let output = input::output_at(&self.scene, point)?;
        input::hit(&self.scene, output, point).map(|t| t.window)
    }

    /// The popup grab's share of a button event. Returns whether it
    /// consumed the event.
    ///
    /// Called at the very top of `pointer_button` — above the release/drag
    /// branch and the Super-drag and frame-region branches. Below them, a
    /// press on a title bar with a menu open would start a drag instead of
    /// dismissing, and the swallowed press's release would fall through
    /// to `pointer.over` as an unpaired `Released`.
    fn popup_grab_button(&mut self, state: ButtonState, time_ns: u64) -> bool {
        if state == ButtonState::Released && self.popup_seat.click_consumed {
            self.popup_seat.click_consumed = false;
            self.note_input(time_ns);
            return true;
        }
        if state == ButtonState::Pressed
            && let Some(root) = self.popup_seat.grab
        {
            let inside = self
                .window_under_pointer()
                .is_some_and(|w| self.in_popup_chain(w, root));
            if !inside {
                // Outside the chain: the whole chain goes and the click is
                // consumed — not delivered, not a raise, not a focus.
                self.hotkeys.cancel_tap();
                self.dismiss_chain(root);
                self.popup_seat.click_consumed = true;
                self.note_input(time_ns);
                return true;
            }
        }
        false
    }

    /// Refuse, on receipt, a client op this client may not send: an
    /// unimplemented M5-A op (no bit advertised), or a popup op without
    /// `POPUP` in its `ClientCaps`. Returns whether it was refused (and the
    /// client disconnected).
    ///
    /// At receipt rather than at the commit, which matters for
    /// `SendSelection`: buffering it would park a descriptor in `pending`
    /// until a commit that may never come.
    fn refuse_at_receipt(&mut self, token: u64, msg: &ClientMsg) -> bool {
        if self.refuse_popup_op(token, msg) {
            return true;
        }
        // `StartDrag` and `SetDragIconOffset` are buffered to the
        // commit, so their `DATA` gate is here; `AcceptDrop`/`FinishDrag` are answered at receipt and
        // check it themselves.
        match msg {
            ClientMsg::SetOpaqueRegion(_) => !self.opaque_region_allowed(token),
            ClientMsg::SetSurface(_) => !self.surface_allowed(token, "SetSurface"),
            ClientMsg::StartDrag(_) => !self.data_allowed(token, "StartDrag"),
            ClientMsg::SetDragIconOffset(_) => !self.data_allowed(token, "SetDragIconOffset"),
            _ => false,
        }
    }

    /// `SetOpaqueRegion` needs `OPAQUE_REGION` listed in `ClientCaps`
    /// (`docs/wire.md` rule 3, as for `SetCursor`). Returns whether the
    /// client may send it; if not, it has been disconnected.
    fn opaque_region_allowed(&mut self, token: u64) -> bool {
        let listed = self
            .wire_clients
            .get(&token)
            .map(|c| c.client_caps & nitro_wire::types::caps::OPAQUE_REGION != 0);
        match listed {
            Some(true) => true,
            Some(false) => {
                self.disconnect(
                    token,
                    Some((
                        0,
                        ErrorCode::Protocol,
                        "SetOpaqueRegion needs `OPAQUE_REGION` listed in ClientCaps".to_owned(),
                    )),
                );
                false
            }
            None => false,
        }
    }

    /// The Surface ops need `SURFACE` listed in `ClientCaps` (#3897,
    /// `docs/wire.md` rule 3). Returns whether the client may send `name`;
    /// if not, it has been disconnected.
    fn surface_allowed(&mut self, token: u64, name: &str) -> bool {
        let listed = self
            .wire_clients
            .get(&token)
            .map(|c| c.client_caps & nitro_wire::types::caps::SURFACE != 0);
        match listed {
            Some(true) => true,
            Some(false) => {
                self.disconnect(
                    token,
                    Some((
                        0,
                        ErrorCode::Protocol,
                        format!("{name} needs `SURFACE` listed in ClientCaps"),
                    )),
                );
                false
            }
            None => false,
        }
    }

    /// `CreateSurfaceBuffer`: checked and mapped at receipt exactly like
    /// [`Server::create_buffer`], sharing its caps and id space.
    fn create_surface_buffer(
        &mut self,
        token: u64,
        buffer: nitro_wire::msg::CreateSurfaceBuffer,
    ) -> bool {
        let id = buffer.id;
        let Some(held) = self.buffer_budget(token) else {
            return false;
        };
        let (desc, pixels) = match clients::map_surface_buffer(buffer, held) {
            Ok(pair) => pair,
            Err(e) => {
                self.disconnect(token, Some((0, e.code, e.detail)));
                return false;
            }
        };
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        client.pending.push(Pending::Buffer(id, desc, pixels));
        true
    }

    /// `AllocSurfaceBuffers` (#3914): allocate `count` linear scanout
    /// buffers through the backend, map each read-only for the CPU path,
    /// register them as the client's surface buffers and send each one's
    /// dma-buf. Acts at receipt against the committed scene. A malformed
    /// request is fatal; anything the server merely cannot do is one
    /// `AllocSurfaceBuffersFailed`, all or nothing. Returns whether the
    /// client survives.
    fn alloc_surface_buffers(&mut self, token: u64, req: &msg::AllocSurfaceBuffers) -> bool {
        use nitro_wire::types::BufferId;
        if !self.surface_allowed(token, "AllocSurfaceBuffers") {
            return false;
        }
        let node = match self.check_alloc_request(token, req) {
            Ok(Some(n)) => n,
            Ok(None) => return false,
            Err(ApplyError { code, detail }) => {
                self.disconnect(token, Some((0, code, detail)));
                return false;
            }
        };
        let plan = match self.plan_alloc(token, node, req) {
            Ok(p) => p,
            Err((reason, why)) => {
                self.refuse_alloc(token, req, reason, &why);
                return true;
            }
        };
        let (fmt, width, height) = plan;
        let mut made: Vec<(BufferId, HeldBuffer, msg::SurfaceBufferAllocated)> = Vec::new();
        let mut failure = None;
        for i in 0..u32::from(req.count) {
            let id = BufferId(req.first_id.raw() + i);
            match self.alloc_one_scanout(token, req.node, id, fmt, width, height) {
                Ok(one) => made.push(one),
                Err(refusal) => {
                    failure = Some(refusal);
                    break;
                }
            }
        }
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        if let Some((reason, why)) = failure {
            // All or nothing: undo what was made.
            for (_, held, _) in made {
                let _ = self.scene.destroy_buffer(client.id, held.key);
                if let Some(k) = held.scanout {
                    self.backend.free_buffer(k);
                }
            }
            self.refuse_alloc(token, req, reason, &why);
            return true;
        }
        for (id, held, reply) in made {
            client.buffers.insert(id, held);
            client.send(&ServerMsg::SurfaceBufferAllocated(reply));
        }
        true
    }

    /// The fatal half of `AllocSurfaceBuffers`: what a correct client
    /// never sends. `Ok(None)` when the client is gone.
    fn check_alloc_request(
        &self,
        token: u64,
        req: &msg::AllocSurfaceBuffers,
    ) -> Result<Option<nitro_scene::NodeKey>, ApplyError> {
        use nitro_wire::types::BufferId;
        let Some(client) = self.wire_clients.get(&token) else {
            return Ok(None);
        };
        if !(1..=MAX_SCANOUT_ALLOC).contains(&req.count) {
            return Err(ApplyError::new(
                ErrorCode::Protocol,
                format!(
                    "AllocSurfaceBuffers: count {} is not 1..={MAX_SCANOUT_ALLOC}",
                    req.count
                ),
            ));
        }
        let node = client.nodes.get(&req.node).copied().ok_or_else(|| {
            ApplyError::new(
                ErrorCode::UnknownNode,
                format!("AllocSurfaceBuffers: no node with id {}", req.node.raw()),
            )
        })?;
        if self.scene.node(node).map(nitro_scene::Node::kind) != Ok(nitro_scene::NodeKind::Surface)
        {
            return Err(ApplyError::new(
                ErrorCode::WrongKind,
                format!(
                    "AllocSurfaceBuffers: node {} is not a Surface",
                    req.node.raw()
                ),
            ));
        }
        for i in 0..u32::from(req.count) {
            let in_use = req
                .first_id
                .raw()
                .checked_add(i)
                .map(BufferId)
                .is_none_or(|b| {
                    b.is_none()
                        || client.buffers.contains_key(&b)
                        || client
                            .pending
                            .iter()
                            .any(|p| matches!(p, Pending::Buffer(q, _, _) | Pending::Dmabuf(q, _) if *q == b))
                });
            if in_use {
                return Err(ApplyError::new(
                    ErrorCode::BadBuffer,
                    format!(
                        "AllocSurfaceBuffers: buffer ids {}..+{} include zero or one in use",
                        req.first_id.raw(),
                        req.count
                    ),
                ));
            }
        }
        Ok(Some(node))
    }

    /// Resolve an `AllocSurfaceBuffers`' defaults and check it against the
    /// caps: `(format, width, height)`, or the refusal.
    fn plan_alloc(
        &self,
        token: u64,
        node: nitro_scene::NodeKey,
        req: &msg::AllocSurfaceBuffers,
    ) -> Result<(u32, u32, u32), (nitro_wire::types::AllocRefusal, String)> {
        use nitro_wire::types::{AllocRefusal, format};
        if Self::is_remote(token) {
            return Err((AllocRefusal::Unsupported, "remote link".to_owned()));
        }
        // Defaults: the node's output's planes pick the format, the hint
        // the size.
        let fmt = if req.format == 0 {
            let kms = self
                .scene
                .node(node)
                .ok()
                .and_then(|n| self.scene.window_info(n.window()).ok())
                .and_then(nitro_scene::Window::output)
                .and_then(|o| self.outputs.iter().find(|s| s.scene_id == o))
                .or_else(|| self.outputs.first())
                .map(|o| o.kms_id);
            planes::alloc_format(&kms.map(|k| self.backend.planes(k)).unwrap_or_default())
        } else {
            req.format
        };
        if ![format::NV12, format::YUYV, format::XR24, format::AR24].contains(&fmt) {
            return Err((
                AllocRefusal::Format,
                format!("format {fmt:#010x} is not allocatable"),
            ));
        }
        let (mut width, mut height) = (req.width, req.height);
        if width == 0 || height == 0 {
            let Some((hw, hh)) = surface::hinted_size(&self.scene, node) else {
                return Err((
                    AllocRefusal::TooBig,
                    "no size given and none hinted".to_owned(),
                ));
            };
            if width == 0 {
                width = hw;
            }
            if height == 0 {
                height = hh;
            }
        }
        // 4:2:0 and 4:2:2 want even sizes; round up.
        if fmt == format::NV12 || fmt == format::YUYV {
            width += width % 2;
        }
        if fmt == format::NV12 {
            height += height % 2;
        }
        if width > MAX_SCANOUT_DIM || height > MAX_SCANOUT_DIM {
            return Err((
                AllocRefusal::TooBig,
                format!("{width}x{height} is past 8192"),
            ));
        }
        // An estimate for the budget before anything is allocated: the
        // tight size of the format. The kernel's padded layout is checked
        // again per buffer.
        let half_bytes_per_px = match fmt {
            format::NV12 => 3,
            format::YUYV => 4,
            _ => 8,
        };
        let each = u64::from(width) * u64::from(height) * half_bytes_per_px / 2;
        if each > clients::MAX_BUFFER_BYTES {
            return Err((AllocRefusal::TooBig, "buffer past the byte cap".to_owned()));
        }
        let held = self.buffer_budget(token).unwrap_or_default();
        let count = usize::from(req.count);
        clients::check_budget(each * count as u64, count, held)
            .map_err(|why| (AllocRefusal::Limit, why))?;
        Ok((fmt, width, height))
    }

    /// Answer an `AllocSurfaceBuffers` with `AllocSurfaceBuffersFailed`.
    fn refuse_alloc(
        &mut self,
        token: u64,
        req: &msg::AllocSurfaceBuffers,
        reason: nitro_wire::types::AllocRefusal,
        why: &str,
    ) {
        info!("AllocSurfaceBuffers refused ({reason:?}): {why}");
        if let Some(c) = self.wire_clients.get_mut(&token) {
            c.send(&ServerMsg::AllocSurfaceBuffersFailed(
                msg::AllocSurfaceBuffersFailed {
                    node: req.node,
                    first_id: req.first_id,
                    reason,
                },
            ));
        }
    }

    /// One buffer of [`Server::alloc_surface_buffers`]: allocate, export,
    /// map, validate, register in the scene. On error nothing of it is
    /// left behind.
    fn alloc_one_scanout(
        &mut self,
        token: u64,
        node: NodeId,
        id: nitro_wire::types::BufferId,
        fmt: u32,
        w: u32,
        h: u32,
    ) -> Result<
        (
            nitro_wire::types::BufferId,
            HeldBuffer,
            msg::SurfaceBufferAllocated,
        ),
        (nitro_wire::types::AllocRefusal, String),
    > {
        use nitro_wire::types::AllocRefusal;
        let client = self
            .wire_clients
            .get(&token)
            .map(|c| c.id)
            .ok_or((AllocRefusal::Failed, "client gone".to_owned()))?;
        let kms = match self.backend.alloc_buffer(nitro_kms::Fourcc(fmt), w, h) {
            Ok(k) => k,
            Err(KmsError::Unsupported(what)) => {
                return Err((AllocRefusal::Unsupported, format!("backend: {what}")));
            }
            Err(e) => return Err((AllocRefusal::Failed, e.to_string())),
        };
        let result = (|| {
            let fail = |e: String| (AllocRefusal::Failed, e);
            let info = self
                .backend
                .buffer_info(kms)
                .ok_or_else(|| fail("no layout for the new buffer".to_owned()))?;
            let size =
                u32::try_from(info.size).map_err(|_| (AllocRefusal::TooBig, "size".to_owned()))?;
            let geo = clients::SurfaceGeometry {
                width: w,
                height: h,
                format: fmt,
                size,
                offset0: info.offsets[0],
                stride0: info.pitches[0],
                offset1: info.offsets[1],
                stride1: info.pitches[1],
            };
            let desc = clients::validate_surface_geometry(&geo).map_err(|e| {
                let r = if e.code == ErrorCode::Limit {
                    AllocRefusal::TooBig
                } else {
                    AllocRefusal::Failed
                };
                (r, e.detail)
            })?;
            // The whole export: with the kernel's padded pitch the scene's
            // `byte_len` (stride × rows) runs past the last row's payload,
            // and `size` is what the buffer really costs.
            let map_len = (info.size as usize).max(clients::surface_map_len(&geo));
            let fd = self
                .backend
                .export_buffer(kms)
                .map_err(|e| fail(e.to_string()))?;
            let mine = rustix::io::dup(&fd).map_err(|e| fail(format!("dup: {e}")))?;
            let mapping = nitro_shm::Mapping::map_dmabuf(mine, map_len)
                .map_err(|e| fail(format!("mapping the export: {e}")))?;
            let key = self
                .scene
                .create_buffer(client, desc, clients::ScanoutPixels(mapping))
                .map_err(|e| fail(e.to_string()))?;
            let held = HeldBuffer {
                key,
                bytes: map_len as u64,
                scanout: Some(kms),
                dmabuf: false,
            };
            let reply = msg::SurfaceBufferAllocated {
                node,
                id,
                format: fmt,
                width: w,
                height: h,
                size,
                offset0: geo.offset0,
                stride0: geo.stride0,
                offset1: geo.offset1,
                stride1: geo.stride1,
                fd,
            };
            Ok((id, held, reply))
        })();
        if result.is_err() {
            self.backend.free_buffer(kms);
        }
        result
    }

    /// `PresentSurface` (#3897): validate against the *committed* scene
    /// and queue the frame for the latch. A superseded frame's buffer is
    /// released at once unless something still shows it.
    fn present_surface(
        &mut self,
        token: u64,
        frame: &msg::PresentSurface,
        fence: Option<OwnedFd>,
    ) -> bool {
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        let checked = (|| {
            let node = match client.nodes.get(&frame.id) {
                Some(key) => Some(*key),
                // An import (#3904): live, or dead (`None`).
                None if client.imports.contains(&frame.id) => {
                    self.shares.live_import(token, frame.id)
                }
                None => {
                    return Err(ApplyError::new(
                        ErrorCode::UnknownNode,
                        format!("PresentSurface: no node with id {}", frame.id.raw()),
                    ));
                }
            };
            let buffer = client
                .buffers
                .get(&frame.buffer)
                .map(|h| h.key)
                .ok_or_else(|| {
                    ApplyError::new(
                        ErrorCode::BadBuffer,
                        format!("PresentSurface: no buffer with id {}", frame.buffer.raw()),
                    )
                })?;
            let fits = self.scene.buffer(buffer).is_ok_and(|b| {
                !frame.src.is_empty() && b.desc().full_rect().contains_rect(&frame.src)
            });
            if !fits {
                return Err(ApplyError::new(
                    ErrorCode::BadBuffer,
                    "PresentSurface: src is empty or leaves the buffer",
                ));
            }
            let Some(node) = node else {
                return Ok((None, buffer));
            };
            if self.scene.node(node).map(nitro_scene::Node::kind)
                != Ok(nitro_scene::NodeKind::Surface)
            {
                return Err(ApplyError::new(
                    ErrorCode::WrongKind,
                    format!("PresentSurface: node {} is not a Surface", frame.id.raw()),
                ));
            }

            Ok((Some(node), buffer))
        })();
        let (node, buffer) = match checked {
            Ok(pair) => pair,
            Err(ApplyError { code, detail }) => {
                self.disconnect(token, Some((frame.serial, code, detail)));
                return false;
            }
        };
        let Some(node) = node else {
            // A revoked import: not an error, but never shown (#3904).
            self.release_now(token, buffer);
            return true;
        };
        let client_id = client.id;
        // The acquire fence (#3918): the explicit one, or a snapshot of a
        // dma-buf's write fences. Never waited on here.
        let fence = match self.acquire_fence(token, buffer, fence) {
            Ok(f) => f,
            Err(e) => {
                self.disconnect(token, Some((frame.serial, e.code, e.detail)));
                return false;
            }
        };
        let queued = surface::Queued {
            token,
            client: client_id,
            buffer,
            serial: frame.serial,
            src: frame.src,
            color: clients::scene_color(frame.matrix, frame.range),
            damage: Damage::new(),
            whole: false,
            fence,
        };
        let lost = self.latch.queue(node, queued, &frame.damage);
        self.drop_frames(lost);
        true
    }

    /// The fence a frame on `buffer` waits for (#3918): `explicit` if the
    /// client sent one, else an implicit snapshot for a dma-buf, else
    /// none. A fence already signalled costs nothing more; a pending one
    /// is registered with epoll and named by its key.
    fn acquire_fence(
        &mut self,
        token: u64,
        buffer: BufferKey,
        explicit: Option<OwnedFd>,
    ) -> Result<Option<surface::FenceKey>, ApplyError> {
        let fd = if let Some(fd) = explicit {
            fd
        } else {
            let Some(dfd) = self
                .scene
                .buffer(buffer)
                .ok()
                .and_then(nitro_scene::Buffer::fence_fd)
            else {
                return Ok(None);
            };
            match nitro_shm::export_sync_file(dfd, nitro_shm::SyncAccess::Read) {
                Ok(Some(f)) => f,
                // Not a dma-buf (the fake's memfd): nothing to wait on.
                Ok(None) => return Ok(None),
                Err(e) => {
                    // A kernel before 6.0: the dma-buf itself polls
                    // readable once its writers are done.
                    self.implicit_fence_fallbacks += 1;
                    debug!("EXPORT_SYNC_FILE: {e}; polling the dma-buf instead");
                    rustix::io::dup(dfd)
                        .map_err(|e| ApplyError::new(ErrorCode::Limit, format!("dup: {e}")))?
                }
            }
        };
        if dmabuf::signalled(fd.as_fd()) {
            return Ok(None);
        }
        if self.fences.count_for(token) >= dmabuf::MAX_FENCES_PER_CLIENT {
            return Err(ApplyError::new(
                ErrorCode::Limit,
                format!(
                    "more than {} acquire fences pending",
                    dmabuf::MAX_FENCES_PER_CLIENT
                ),
            ));
        }
        self.fence_waits += 1;
        self.fences
            .add(&self.epoll, token, fd)
            .map(Some)
            .map_err(|e| ApplyError::new(ErrorCode::Limit, format!("epoll_ctl: {e}")))
    }

    /// An acquire fence signalled: its frame is ready, and latches at the
    /// next paint opportunity like any arriving frame.
    fn on_fence(&mut self, epoll_token: u64) {
        let Some(key) = self.fences.key_of(epoll_token) else {
            return;
        };
        self.fences.remove(&self.epoll, key);
        self.latch.fence_signalled(key);
        self.settle();
    }

    /// Frames the latch let go of without showing (superseded, cancelled,
    /// overflowed): release each buffer to its presenter unless still
    /// shown or queued, and drop its fence.
    fn drop_frames(&mut self, frames: Vec<surface::Queued>) {
        for q in frames {
            if let Some(k) = q.fence {
                self.fences.remove(&self.epoll, k);
            }
            if !self.latch.holds(q.token, q.buffer) {
                self.release_now(q.token, q.buffer);
            }
        }
    }

    /// The dma-buf ops need `DMABUF` and `SURFACE` listed in `ClientCaps`
    /// and a local link (#3918, `docs/wire.md` rule 3). `DMABUF` is bit 2,
    /// below the M5 mask, so it is checked here by name. Returns whether
    /// the client may send `name`; if not, it has been disconnected.
    fn dmabuf_allowed(&mut self, token: u64, name: &str) -> bool {
        use nitro_wire::types::caps::{DMABUF, SURFACE};
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        let why = if Self::is_remote(token) {
            format!("{name} is not available on a remote link")
        } else if client.client_caps & DMABUF == 0 {
            format!("{name} needs `DMABUF` listed in ClientCaps")
        } else if client.client_caps & SURFACE == 0 {
            format!("{name} needs `SURFACE` listed in ClientCaps")
        } else {
            return true;
        };
        self.disconnect(token, Some((0, ErrorCode::Protocol, why)));
        false
    }

    /// `CreateDmabufBuffer` (#3918): validated (and on the CPU path
    /// mapped) at receipt, parked in the batch like `CreateSurfaceBuffer`.
    fn create_dmabuf_buffer(&mut self, token: u64, m: msg::CreateDmabufBuffer) -> bool {
        let id = m.id;
        let Some(held) = self.buffer_budget(token) else {
            return false;
        };
        let importable = self.default_feedback();
        let v = match dmabuf::validate(m, &importable, held) {
            Ok(v) => v,
            Err(e) => {
                self.disconnect(token, Some((0, e.code, e.detail)));
                return false;
            }
        };
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return false;
        };
        client.pending.push(Pending::Dmabuf(id, Box::new(v)));
        true
    }

    /// The default feedback (#3918): the union over every output.
    fn default_feedback(&self) -> Vec<nitro_wire::types::DmabufFormat> {
        let mut all = dmabuf::feedback(&[]);
        for o in &self.outputs {
            all.extend(dmabuf::feedback(&self.backend.planes(o.kms_id)));
        }
        dmabuf::merge(all)
    }

    /// Send the default `DmabufFeedback` to `only`, or to every client that
    /// listed `DMABUF`.
    fn send_default_feedback(&mut self, only: Option<u64>) {
        let formats = self.default_feedback();
        let (w, h) = self
            .outputs
            .iter()
            .fold((0, 0), |(w, h), o| (w.max(o.width), h.max(o.height)));
        let main_device = self.backend.device_id().unwrap_or(0);
        for (t, client) in &mut self.wire_clients {
            if only.is_some_and(|o| o != *t)
                || client.client_caps & nitro_wire::types::caps::DMABUF == 0
            {
                continue;
            }
            client.send(&ServerMsg::DmabufFeedback(msg::DmabufFeedback {
                id: NodeId(0),
                main_device,
                max_width: w,
                max_height: h,
                formats: formats.clone(),
            }));
        }
    }

    /// Send the per-node `DmabufFeedback`s whose output or output feedback
    /// changed (#3918).
    fn send_node_feedback(&mut self) {
        if self.feedback.is_empty() {
            return;
        }
        let mut per_output: Vec<(
            SceneOutputId,
            u32,
            u32,
            Vec<nitro_wire::types::DmabufFormat>,
        )> = Vec::new();
        for o in &self.outputs {
            let f = dmabuf::feedback(&self.backend.planes(o.kms_id));
            per_output.push((o.scene_id, o.width, o.height, f));
        }
        let main_device = self.backend.device_id().unwrap_or(0);
        let changed = self.feedback.changed(&self.scene, &per_output);
        for (token, id, out) in changed {
            let Some((_, w, h, formats)) = per_output.iter().find(|p| p.0 == out) else {
                continue;
            };
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::DmabufFeedback(msg::DmabufFeedback {
                    id,
                    main_device,
                    max_width: *w,
                    max_height: *h,
                    formats: formats.clone(),
                }));
            }
        }
    }

    /// The sharing ops need `SHARE` listed in `ClientCaps` and a local
    /// link (#3904, `docs/wire.md` § Surface sharing). Returns whether the
    /// client may send `name`; if not, it has been disconnected.
    fn share_allowed(&mut self, token: u64, name: &str) -> bool {
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        let why = if Self::is_remote(token) {
            format!("{name} is not available on a remote link")
        } else if client.client_caps & nitro_wire::types::caps::SHARE == 0 {
            format!("{name} needs `SHARE` listed in ClientCaps")
        } else {
            return true;
        };
        self.disconnect(token, Some((0, ErrorCode::Protocol, why)));
        false
    }

    /// `ExportSurface` (#3904): mint a token for one of the client's
    /// committed Surface nodes and answer `SurfaceExported`. A previous
    /// token for the node dies, and its importer is told.
    fn export_surface(&mut self, token: u64, id: NodeId) -> bool {
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        let (uid, node) = (client.peer_uid, client.nodes.get(&id).copied());
        let failure = match node {
            None if client.imports.contains(&id) => Some((
                ErrorCode::WrongKind,
                format!("ExportSurface: node {} is an import", id.raw()),
            )),
            None => Some((
                ErrorCode::UnknownNode,
                format!("ExportSurface: no node with id {}", id.raw()),
            )),
            Some(key)
                if self.scene.node(key).map(nitro_scene::Node::kind)
                    != Ok(nitro_scene::NodeKind::Surface) =>
            {
                Some((
                    ErrorCode::WrongKind,
                    format!("ExportSurface: node {} is not a Surface", id.raw()),
                ))
            }
            Some(_) => None,
        };
        if let Some((code, detail)) = failure {
            self.disconnect(token, Some((0, code, detail)));
            return false;
        }
        let Some(node) = node else {
            return false;
        };
        let share = match share::mint() {
            Ok(t) => t,
            Err(e) => {
                warn!("ExportSurface: getrandom: {e}");
                self.disconnect(
                    token,
                    Some((
                        0,
                        ErrorCode::Limit,
                        format!("ExportSurface: no randomness: {e}"),
                    )),
                );
                return false;
            }
        };
        if let Some(r) = self.shares.export(token, uid, node, share) {
            self.revoke_import(r);
        }
        if let Some(client) = self.wire_clients.get_mut(&token) {
            client.send(&ServerMsg::SurfaceExported(msg::SurfaceExported {
                id,
                token: share,
            }));
        }
        true
    }

    /// `ImportSurface` (#3904): bind `id` in the client's id space to the
    /// node `share` names — or bind it dead and say so at once.
    fn import_surface(&mut self, token: u64, share: ShareToken, id: NodeId) -> bool {
        let Some(client) = self.wire_clients.get(&token) else {
            return false;
        };
        let failure = if clients::id_taken(client, id) {
            Some((
                ErrorCode::Protocol,
                format!("ImportSurface: id {} is zero or already in use", id.raw()),
            ))
        } else if client.imports.len() >= share::MAX_IMPORTS_PER_CLIENT {
            Some((
                ErrorCode::Limit,
                "ImportSurface: too many imports".to_owned(),
            ))
        } else {
            None
        };
        if let Some((code, detail)) = failure {
            self.disconnect(token, Some((0, code, detail)));
            return false;
        }
        let uid = client.peer_uid;
        let surface_caps = client.client_caps & nitro_wire::types::caps::SURFACE != 0;
        let dma_caps = client.client_caps & nitro_wire::types::caps::DMABUF != 0;
        let imported = match self.shares.import(share, token, uid, id) {
            Ok(i) => i,
            Err(share::ImportError::OwnToken) => {
                self.disconnect(
                    token,
                    Some((
                        0,
                        ErrorCode::Protocol,
                        "ImportSurface: the token is this client's own export".to_owned(),
                    )),
                );
                return false;
            }
        };
        if let Some(client) = self.wire_clients.get_mut(&token) {
            client.imports.insert(id);
        }
        if let Some(r) = imported.displaced {
            self.revoke_import(r);
        }
        if let Some(node) = imported.node {
            debug!("client token {token}: imported a surface as {}", id.raw());
            if surface_caps {
                self.surface_hints.track(node, token, id);
                if dma_caps {
                    self.feedback.track(node, token, id);
                }
            }
        } else {
            debug!("client token {token}: dead import {}", id.raw());
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::SurfaceRevoked(msg::SurfaceRevoked { id }));
            }
        }
        true
    }

    /// An import of `node` by `token` ended (revoked or dropped): stop
    /// hinting it and release whatever frame the importer had queued.
    fn end_import(&mut self, token: u64, node: nitro_scene::NodeKey) {
        self.surface_hints.untrack(node, token);
        self.feedback.untrack(node, token);
        let dropped = self.latch.cancel_from(node, token);
        self.drop_frames(dropped);
    }

    /// Tell an importer its import is dead (#3904).
    fn revoke_import(&mut self, r: share::Revoked) {
        self.end_import(r.importer, r.node);
        if let Some(client) = self.wire_clients.get_mut(&r.importer) {
            client.send(&ServerMsg::SurfaceRevoked(msg::SurfaceRevoked { id: r.id }));
        }
    }

    /// Revoke the imports of every exported node that died this wakeup.
    /// Before the latch, so a frame the importer queued on the dead node is
    /// released rather than dropped silently.
    fn sweep_shares(&mut self) {
        if self.shares.is_empty() {
            return;
        }
        for r in self.shares.sweep(&self.scene) {
            self.revoke_import(r);
        }
    }

    /// Send `BufferReleased` for a buffer that never reached the scene (a
    /// superseded or cancelled latch frame), unless a node shows it.
    fn release_now(&mut self, token: u64, key: BufferKey) {
        if self.scene.buffer_in_use(key) {
            return;
        }
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        if client.client_caps & nitro_wire::types::caps::RELEASE == 0 {
            return;
        }
        if let Some(k) = client
            .buffers
            .values()
            .find(|h| h.key == key)
            .and_then(|h| h.scanout)
            && self.on_kms.contains(&k)
        {
            self.held_releases.push((k, client.id, key));
            return;
        }
        if self.gpu.borrows.holds(key) {
            self.gpu.held.push((client.id, key));
            return;
        }
        if let Some(id) = client.buffer_id(key) {
            client.send(&ServerMsg::BufferReleased(msg::BufferReleased { id }));
        }
    }

    /// Latch every queued Surface frame whose output has no flip pending
    /// (#3897). Each latched serial joins its output's `painting` list,
    /// exactly as a commit's does, so `Presented` goes out from `on_flip`
    /// when the frame carrying it completes — or from
    /// `answer_idle_clients` if the latch painted nothing. Returns whether
    /// anything latched.
    fn latch_surfaces(&mut self) -> bool {
        // A frame an importer queued on a node that died must be released
        // to it, not dropped silently with the node (#3904).
        self.sweep_shares();
        if self.latch.is_empty() {
            return false;
        }
        let busy: Vec<SceneOutputId> = self
            .outputs
            .iter()
            .filter(|o| self.backend.flip_pending(o.kms_id))
            .map(|o| o.scene_id)
            .collect();
        let output_of = |scene: &Scene, node: nitro_scene::NodeKey| {
            scene
                .node(node)
                .ok()
                .and_then(|n| scene.window_info(n.window()).ok())
                .and_then(nitro_scene::Window::output)
        };
        let early = self.early_latch_nodes();
        if !self.unbracketed.is_empty() {
            let scene = &self.scene;
            self.unbracketed.retain(|k| scene.buffer(*k).is_ok());
        }
        let mut latch = std::mem::take(&mut self.latch);
        let outcome = latch.latch_ready(
            &mut self.scene,
            |scene, node| output_of(scene, node).is_none_or(|o| !busy.contains(&o)),
            |node, buffer| early.contains(&(node, buffer)),
        );
        self.latch = latch;
        for q in outcome.dropped {
            if let Some(k) = q.fence {
                self.fences.remove(&self.epoll, k);
            }
        }
        self.drop_frames(outcome.superseded);
        let latched = outcome.latched;
        for l in &latched {
            if let Some(prev) = l.previous.filter(|p| *p != l.buffer)
                && !self.unbracketed.remove(&prev)
            {
                self.dmabuf_sync(prev, false);
            }
            if let Some(k) = l.fence {
                // Latched early onto a plane (#3938): the display waits
                // on the fence (`stage_plane_fences`), never the server,
                // and there is no CPU read bracket to begin.
                self.plane_fence_latches += 1;
                self.unbracketed.insert(l.buffer);
                if let Some(fd) = self.fences.take(&self.epoll, k)
                    && let Some(o) = self.outputs.iter_mut().find(|o| o.decision.places(l.node))
                {
                    o.plane_fences.retain(|(n, _)| *n != l.node);
                    o.plane_fences.push((l.node, fd));
                }
            } else {
                // The CPU read bracket on a client dma-buf (#3918): begun
                // at the latch, when the fence has signalled so the ioctl
                // cannot block, and ended when the buffer stops being
                // shown.
                self.unbracketed.remove(&l.buffer);
                self.dmabuf_sync(l.buffer, true);
            }
            let output = output_of(&self.scene, l.node);
            // A new frame of a Surface on a plane is a plane-only flip
            // (#3899): the scene damages nothing for it.
            for o in &mut self.outputs {
                if o.decision.shows(l.node) {
                    o.planes_dirty = true;
                }
            }
            let Some(client) = self.wire_clients.get_mut(&l.token) else {
                continue;
            };
            if let Some(out) =
                output.and_then(|id| self.outputs.iter_mut().find(|o| o.scene_id == id))
            {
                client.unpresented.push(l.serial);
                out.painting.push((l.client.0, l.serial));
            } else {
                // Nowhere to appear: answer at once, the commit rule.
                let (output, time_ns, seq) = self.outputs.first().map_or((0, 0, 0), |o| {
                    (o.scene_id.0, o.last_vblank_ns, o.last_sequence)
                });
                client.send(&ServerMsg::Presented(msg::Presented {
                    serial: l.serial,
                    output,
                    time_ns,
                    seq,
                }));
            }
        }
        !latched.is_empty()
    }

    /// The `(node, buffer)` pairs whose frames may latch before their
    /// acquire fence signals (#3938): the node is on a plane with
    /// `IN_FENCE_FD` in the current decision, and the buffer is a KMS
    /// framebuffer that plane lists the format and modifier of — so the
    /// frame goes to the plane, and the display waits on the fence.
    fn early_latch_nodes(&self) -> HashSet<(nitro_scene::NodeKey, BufferKey)> {
        let mut out = HashSet::new();
        if self.outputs.iter().all(|o| o.decision.placed.is_empty()) {
            return out;
        }
        for c in self.wire_clients.values() {
            for h in c.buffers.values() {
                let Some(info) = h.scanout.and_then(|k| self.backend.buffer_info(k)) else {
                    continue;
                };
                for o in &self.outputs {
                    for (node, plane) in &o.decision.placed {
                        let fits = o.plane_info.iter().any(|p| {
                            p.id == *plane && p.in_fence && p.supports(info.format, info.modifier)
                        });
                        if fits {
                            out.insert((*node, h.key));
                        }
                    }
                }
            }
        }
        out
    }

    /// Begin (`start`) or end the CPU read bracket on a client dma-buf the
    /// CPU path maps. A no-op for anything else (`ENOTTY` on a memfd).
    fn dmabuf_sync(&self, key: BufferKey, start: bool) {
        let Some(b) = self.scene.buffer(key).ok().filter(|b| b.cpu_readable()) else {
            return;
        };
        let Some(fd) = b.fence_fd() else {
            return;
        };
        let r = if start {
            nitro_shm::sync_start(fd, nitro_shm::SyncAccess::Read)
        } else {
            nitro_shm::sync_end(fd, nitro_shm::SyncAccess::Read)
        };
        if let Err(e) = r {
            debug!("DMA_BUF_IOCTL_SYNC: {e}");
        }
    }

    /// Send the `SurfaceHint`s whose size changed since the last one. v1's
    /// CPU path always prefers NV12 at the node's device size; the planes
    /// module (#3899) is where another answer will come from.
    fn send_surface_hints(&mut self) {
        self.send_node_feedback();
        let outputs = &self.outputs;
        let hints = self.surface_hints.changed(&self.scene, |scene, node| {
            let output = scene
                .node(node)
                .ok()
                .and_then(|n| scene.window_info(n.window()).ok())
                .and_then(nitro_scene::Window::output);
            outputs
                .iter()
                .find(|o| Some(o.scene_id) == output)
                .or_else(|| outputs.first())
                .map_or(nitro_wire::types::format::NV12, |o| o.hint_format)
        });
        for (token, id, format, width, height) in hints {
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&ServerMsg::SurfaceHint(msg::SurfaceHint {
                    id,
                    format,
                    width,
                    height,
                }));
            }
        }
    }

    /// Refuse a popup op from a client that never listed `POPUP` in its
    /// `ClientCaps` (`docs/wire.md` rule 3). Returns whether it was
    /// refused (and the client disconnected).
    ///
    /// Checked once, at receipt, which is also what makes pushing
    /// `PopupDone` safe — no client that did not list the bit can own a
    /// popup to be told about.
    fn refuse_popup_op(&mut self, token: u64, msg: &ClientMsg) -> bool {
        if !is_popup_op(msg)
            || self
                .wire_clients
                .get(&token)
                .is_none_or(|c| c.client_caps & nitro_wire::types::caps::POPUP != 0)
        {
            return false;
        }
        let name = msg.name();
        self.disconnect(
            token,
            Some((
                0,
                ErrorCode::Protocol,
                format!("{name} needs `POPUP` listed in ClientCaps"),
            )),
        );
        true
    }

    /// Act on the windows a commit showed or hid: a bar's zone may have
    /// come or gone, and a parent that stopped showing takes its menus
    /// down with it (ungated on zones: that part is about popups).
    fn visibility_changed(&mut self, windows: Vec<WindowKey>) {
        if !windows.is_empty() && !self.zones.is_empty() {
            self.work_area_changed();
        }
        for win in windows {
            if !self.showing(win) {
                self.dismiss_popups_of(win);
            }
        }
    }

    /// Re-run pointer enter/leave at the pointer's current position.
    ///
    /// The enter/leave half of `move_pointer`, for the case where what is
    /// under the pointer changes without the pointer moving: a window or
    /// popup mapped, unmapped, closed, restacked, moved or resized under
    /// it, or a grab that ended. See [`Server::pointer_refresh`] for who
    /// asks.
    fn refresh_pointer_over(&mut self) {
        if !self.pointer.present {
            return;
        }
        // A window drag owns the pointer the way `move_pointer` lets it:
        // no enter/leave while the window follows the pointer. The drag's
        // end asks for the re-check (`pointer_button`).
        if self.wm.drag().is_some() {
            return;
        }
        // During a drag-and-drop the pointer belongs to the drag: what
        // mapped or unmapped under it is a new drop target, not a new
        // pointer focus. Same stationary re-check, `DragEnter`/`DragLeave`.
        if self.dnd_grabbing() {
            self.drive_dnd(monotonic_ns(), false);
            return;
        }
        // Mid-grab, focus is pinned: a window mapped or unmapped under a
        // still pointer must not steal it. `end_pointer_grab` sets the
        // refresh flag *after* clearing the grab, so the re-derivation
        // it asks for runs on the next settle.
        if self.pointer.grab.is_some() {
            return;
        }
        let time_ns = monotonic_ns();
        let point = self.pointer.position();
        let target =
            input::output_at(&self.scene, point).and_then(|id| self.pointer_target(id, point));
        let now_over = target.as_ref().map(|t| t.window);
        if now_over == self.pointer.over {
            return;
        }
        if let Some(left) = self.pointer.over {
            self.send_to_window(left, |id| {
                ServerMsg::PointerLeave(msg::PointerLeave {
                    window: id,
                    time_ns,
                })
            });
        }
        self.set_pointer_over(now_over);
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
        // Enter/leave went out after `flush_wire_clients` would otherwise
        // have run for this wakeup's input; the settle that called us
        // flushes at its end.
    }

    /// Send the `PopupDone`s dismissal queued. See
    /// [`Server::dismiss_chain`] for why this is the only sender.
    fn flush_popup_done(&mut self) {
        if self.pending_popup_done.is_empty() {
            return;
        }
        for win in std::mem::take(&mut self.pending_popup_done) {
            self.send_to_window(win, |popup| ServerMsg::PopupDone(msg::PopupDone { popup }));
        }
    }
}

// ------------------------------------------------------------ planes (#3899)

impl Server {
    /// Which visible Surfaces on output `index` could go on a plane, bottom
    /// to top: backed by a KMS framebuffer (server-allocated, or an
    /// imported dma-buf), opaque, axis-aligned, at full opacity. Each says
    /// whether anything painted above it — or the software cursor —
    /// touches its visible rect.
    fn plane_candidates(&self, index: usize, out: &mut Vec<planes::Candidate>) {
        use nitro_scene::PaintKind;
        let o = &self.outputs[index];
        let Some((orect, _)) = self.scene.output_info(o.scene_id) else {
            return;
        };
        let kms_of: HashMap<BufferKey, nitro_kms::BufferId> = self
            .wire_clients
            .values()
            .flat_map(|c| c.buffers.values())
            .filter_map(|h| Some((h.key, h.scanout?)))
            .collect();
        if kms_of.is_empty() {
            return;
        }
        let mut items = Vec::new();
        self.scene.paint_list(o.scene_id, &orect, &mut items);
        let cs = self.cursor_state(o.scene_id);
        let cursor = cs.visible.then(|| {
            Cursor::rect_scaled(cs.x, cs.y, cs.shape, cs.scale).translate(orect.x, orect.y)
        });
        for (i, item) in items.iter().enumerate() {
            let (PaintKind::Surface { size, .. } | PaintKind::Hole { size }) = item.kind else {
                continue;
            };
            let t = item.transform;
            if !t.is_axis_aligned() || t.a <= 0.0 || t.d <= 0.0 || item.opacity < 1.0 {
                continue;
            }
            let Some(content) = self
                .scene
                .node(item.node)
                .ok()
                .and_then(nitro_scene::Node::surface)
                .and_then(|s| s.content)
            else {
                continue;
            };
            let Some(&kms) = kms_of.get(&content.buffer) else {
                continue;
            };
            if !self
                .scene
                .buffer(content.buffer)
                .is_ok_and(|b| b.desc().is_opaque())
            {
                continue;
            }
            let Some(info) = self.backend.buffer_info(kms) else {
                continue;
            };
            let dst = t
                .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1))
                .round_out();
            let visible = dst.intersect(&item.clip).intersect(&orect);
            if visible.is_empty() {
                continue;
            }
            let obscured = items[i + 1..].iter().any(|l| l.bounds.intersects(&visible))
                || cursor.is_some_and(|c| c.intersects(&visible));
            let local = |r: nitro_core::IRect| r.translate(-orect.x, -orect.y);
            out.push(planes::Candidate {
                node: item.node,
                buffer: kms,
                format: info.format,
                modifier: info.modifier,
                src: planes::crop(content.src, local(dst), local(visible)),
                dst: local(visible),
                obscured,
                color: content.color,
            });
        }
    }

    /// Decide output `index`'s plane layout for the frame about to be
    /// painted, and stage it if it changed.
    fn plan_planes(&mut self, index: usize) {
        let o = &self.outputs[index];
        if o.plane_info.is_empty() || !o.lit || !self.active {
            return;
        }
        let mut cands = Vec::new();
        self.plane_candidates(index, &mut cands);
        let (gpu_nodes, helper) = self.gpu_inputs(index);
        let now = monotonic_ns();
        let o = &mut self.outputs[index];
        let id = o.kms_id;
        let inp = planes::Inputs {
            candidates: &cands,
            planes: &o.plane_info,
            size: (o.width, o.height),
            alpha: self.backend.scanout_alpha(id),
            gpu: &gpu_nodes,
            helper,
        };
        let backend = &mut self.backend;
        let d = o
            .planner
            .decide(&inp, now, &mut |a| backend.test_layout(id, a).ok());
        if !gpu_nodes.is_empty() || self.gpu.owner == Some(id) {
            self.gpu_want(index, &d);
        }
        self.apply_decision(index, d);
    }

    /// Make `d` output `index`'s layout: flag the placed Surfaces as holes,
    /// stage the planes, and — when more than the buffers changed —
    /// repaint the output in full.
    fn apply_decision(&mut self, index: usize, d: planes::Decision) {
        let o = &mut self.outputs[index];
        if d == o.decision {
            return;
        }
        let id = o.kms_id;
        let reshaped = !d.same_shape(&o.decision);
        if reshaped {
            info!(
                "{id}: planes {:?} -> {:?} ({} placed)",
                o.decision.mode,
                d.mode,
                d.placed.len()
            );
            for n in o.decision.nodes() {
                if !d.shows(n) {
                    let _ = self.scene.set_surface_on_plane(n, false);
                }
            }
            for n in d.nodes() {
                let _ = self.scene.set_surface_on_plane(n, true);
            }
        }
        // IN_FENCE_FD (#3938): an early-latched frame's fence is handed
        // over just before the commit, `stage_plane_fences`. Mode 2 stages
        // at its commit, when the helper's buffer is known (#3922).
        let staged = if d.mode == planes::Mode::Gpu {
            Ok(())
        } else {
            self.backend.set_plane_state(id, &d.layout)
        };
        let o = &mut self.outputs[index];
        o.decision = d;
        o.planes_dirty = true;
        if reshaped {
            o.invalidate();
        }
        if let Err(e) = staged {
            warn!("{id}: staging planes: {e}");
            self.planes_fallback(index);
        }
    }

    /// A plane-only commit: the staged layout with the current output
    /// buffer, no raster, no copy — a video frame on a plane.
    fn flip_planes(&mut self, index: usize) -> bool {
        self.select_scanout_alpha(index);
        self.stage_plane_fences(index);
        let id = self.outputs[index].kms_id;
        match self.backend.commit_planes(id) {
            Ok(()) => {
                self.plane_flips += 1;
                self.outputs[index].planes_committed();
                self.note_on_kms(index);
                true
            }
            Err(e) => {
                warn!("{id}: plane commit: {e}");
                self.planes_fallback(index);
                false
            }
        }
    }

    /// Hand the acquire fences of early-latched frames (#3938) to the
    /// planes that show them, as `IN_FENCE_FD` for the coming commit. A
    /// fence whose node is no longer placed is dropped: that frame is
    /// composited instead (the residual in `docs/surfaces.md`).
    fn stage_plane_fences(&mut self, index: usize) {
        let o = &mut self.outputs[index];
        if o.plane_fences.is_empty() {
            return;
        }
        let id = o.kms_id;
        for (node, fd) in std::mem::take(&mut o.plane_fences) {
            let Some(&(_, plane)) = o.decision.placed.iter().find(|(n, _)| *n == node) else {
                continue;
            };
            match self.backend.set_plane_fence(id, plane, fd) {
                Ok(()) => self.plane_fences += 1,
                Err(e) => debug!("{id}: IN_FENCE_FD on {plane:?}: {e}"),
            }
        }
    }

    /// The kernel refused a layout it had accepted in a test (or it could
    /// not be staged): back to the default and composite that shape.
    fn planes_fallback(&mut self, index: usize) {
        let o = &mut self.outputs[index];
        o.planner.fallback();
        o.plane_fences.clear();
        let old = std::mem::take(&mut o.decision);
        o.planes_dirty = false;
        o.gpu_pending = None;
        o.invalidate();
        let id = o.kms_id;
        for n in old.nodes() {
            let _ = self.scene.set_surface_on_plane(n, false);
        }
        let _ = self.backend.set_plane_state(id, &[]);
    }

    /// A modeset put every output's planes (but `except`'s) back to the
    /// default: forget their decisions and repaint them fully.
    fn planes_reset(&mut self, except: Option<usize>) {
        for (i, o) in self.outputs.iter_mut().enumerate() {
            if Some(i) == except {
                continue;
            }
            o.planner.reset();
            o.plane_fences.clear();
            if o.decision == planes::Decision::default() {
                continue;
            }
            let old = std::mem::take(&mut o.decision);
            o.planes_dirty = false;
            o.gpu_pending = None;
            o.invalidate();
            for n in old.nodes() {
                let _ = self.scene.set_surface_on_plane(n, false);
            }
        }
        self.drain_kms_releases();
    }

    /// Remember the framebuffers output `index`'s committed layout reads.
    fn note_on_kms(&mut self, index: usize) {
        let ids: Vec<nitro_kms::BufferId> = self.outputs[index].decision.buffers().collect();
        self.on_kms.extend(ids);
    }

    /// Framebuffers the display stopped reading: send the `BufferReleased`s
    /// held back for them.
    fn drain_kms_releases(&mut self) {
        for id in self.backend.take_released_buffers() {
            self.on_kms.remove(&id);
            // A helper ring slot the display stopped reading (#3922).
            if let Some(i) = self.gpu.ring.slot_of(id)
                && self.gpu.ring.slots[i].state == gpu::SlotState::Shown
            {
                self.gpu.ring.slots[i].state = gpu::SlotState::Free;
            }
        }
        if !self.on_kms.is_empty() {
            // Freed ids (the client destroyed the buffer) are not reported.
            let backend = &self.backend;
            self.on_kms.retain(|k| backend.buffer_info(*k).is_some());
        }
        if self.held_releases.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held_releases);
        for (k, owner, key) in held {
            if self.on_kms.contains(&k) {
                self.held_releases.push((k, owner, key));
                continue;
            }
            // Shown again since: the scene will release it again.
            if self.scene.buffer_in_use(key) {
                continue;
            }
            let Some(client) = self.wire_clients.values_mut().find(|c| c.id == owner) else {
                continue;
            };
            if let Some(id) = client.buffer_id(key) {
                client.send(&ServerMsg::BufferReleased(msg::BufferReleased { id }));
            }
        }
    }

    /// The `planes_*` lines of `stats`.
    fn planes_stats(&self, pairs: &mut Vec<(&'static str, u64)>) {
        let sum = |f: fn(&planes::PlannerStats) -> u64| {
            self.outputs
                .iter()
                .map(|o| f(&o.planner.stats))
                .sum::<u64>()
        };
        pairs.push((
            "planes_mode",
            self.outputs
                .iter()
                .map(|o| o.decision.mode.number())
                .max()
                .unwrap_or(0),
        ));
        pairs.push((
            "planes_in_use",
            self.outputs
                .iter()
                .map(|o| o.decision.placed.len() as u64)
                .sum(),
        ));
        pairs.push(("planes_tests", sum(|s| s.tests)));
        pairs.push(("planes_cache_hits", sum(|s| s.cache_hits)));
        pairs.push(("planes_fallbacks", sum(|s| s.fallbacks)));
        pairs.push(("planes_switches", sum(|s| s.switches)));
        pairs.push(("planes_candidates", sum(|s| s.candidates)));
        pairs.push(("planes_obscured", sum(|s| s.obscured)));
        pairs.push(("plane_flips", self.plane_flips));
        pairs.push(("plane_fences", self.plane_fences));
        pairs.push(("plane_fence_latches", self.plane_fence_latches));
        pairs.push(("plane_releases_held", self.held_releases.len() as u64));
    }
}

// ------------------------------------------------------ drag and drop (M5-I)

impl Server {
    /// Whether a drag-and-drop holds the pointer.
    fn dnd_grabbing(&self) -> bool {
        self.dnd.as_ref().is_some_and(data::Dnd::grabbing)
    }

    /// The cursor a drag shows: the arrow over a target that accepted,
    /// the slashed ring everywhere else. Owned by the drag while it
    /// grabs, the way a window drag owns its move cross.
    fn dnd_shape(&self) -> crate::cursor::Shape {
        if self.dnd.as_ref().is_some_and(data::Dnd::accepted) {
            crate::cursor::Shape::Arrow
        } else {
            crate::cursor::Shape::NotAllowed
        }
    }

    /// `StartDrag`, validated at the commit and run from `settle`.
    ///
    /// Authorization is silent on failure, `StartMove`'s rule for its
    /// reason (`Server::drag_request`): each check is a race a correct
    /// client can lose — the user let go before the request arrived.
    fn start_dnd(&mut self, token: u64, start: clients::DragStart) {
        let Some(client) = self.wire_clients.get(&token) else {
            return;
        };
        let refused = if self
            .dnd
            .as_ref()
            .is_some_and(|d| d.phase() != data::Phase::Finished)
        {
            Some("a drag is already in flight")
        } else if self.wm.drag().is_some() {
            Some("a window drag is in flight")
        } else if !self.pointer.any_button_down() || self.dnd_swallow {
            Some("no pointer button is down")
        } else if !self.pointer.over.is_some_and(|w| client.owns_window(w)) {
            Some("the client does not hold pointer focus")
        } else if self.lock.is_locked() {
            Some("the session is locked")
        } else if !client.owns_window(start.window) {
            Some("the window is gone")
        } else {
            None
        };
        if let Some(why) = refused {
            debug!("StartDrag: {why}: ignored");
            return;
        }
        // A drag whose source has yet to `FinishDrag` is superseded: its
        // offer is gone, so whatever is still parked against it ends at EOF.
        if self.dnd.take().is_some() {
            self.release_dnd_transfers();
        }
        // One pointer: a grabbing menu goes first, even the source's own.
        if let Some(root) = self.popup_seat.grab {
            self.dismiss_chain(root);
        }
        // The source loses pointer focus for the drag, as a Wayland DnD
        // source surface does; it never sees the release of its press.
        let now = monotonic_ns();
        if let Some(left) = self.pointer.over {
            let sent_to = self.send_input(left, |id| {
                ServerMsg::PointerLeave(msg::PointerLeave {
                    window: id,
                    time_ns: now,
                })
            });
            self.note_client_input(sent_to);
        }
        self.pointer.grab = None;
        self.set_pointer_over(None);
        // A button release that a popup grab would have swallowed now
        // belongs to the drag.
        self.popup_seat.click_consumed = false;
        if let Some(icon) = start.icon {
            self.adopt_drag_icon(icon);
        }
        self.dnd = Some(data::Dnd::new(
            token,
            start.icon,
            start.actions,
            start.mimes,
        ));
        self.place_drag_icon();
        self.set_cursor(Some(self.dnd_shape()));
        // The first `DragEnter` (usually the source's own window) goes out
        // from `refresh_pointer_over`, after the scene update has seen the
        // icon placed.
        self.pointer_refresh = true;
        self.note_input(now);
    }

    /// Make a window the drag icon: out of the window manager and the
    /// shell's list, on the overlay layer, invisible to hit testing.
    fn adopt_drag_icon(&mut self, icon: WindowKey) {
        if !self.drag_icons.insert(icon) {
            return;
        }
        self.wm.remove(icon);
        self.notify_window_gone(icon);
        if let Err(e) = self.scene.set_layer(icon, nitro_scene::Layer::Overlay) {
            warn!("drag icon layer: {e}");
        }
        if let Err(e) = self.scene.set_hit_exempt(icon, true) {
            warn!("drag icon: {e}");
        }
    }

    /// Put the drag icon under the pointer at its `SetDragIconOffset`
    /// hotspot offset (centred if none was set), on whichever output the
    /// pointer is on — which is what carries it across outputs.
    ///
    /// This moves a window, so it is content damage, and a frame with an
    /// icon in motion is never a cursor-only flip (`defer.rs`). That is
    /// deliberate — there is new content to show — and not to be
    /// "optimised" into cursor damage: a drag without an icon stays on the
    /// cursor-only path exactly as ordinary motion does.
    fn place_drag_icon(&mut self) {
        let Some(icon) = self.dnd.as_ref().and_then(|d| d.icon) else {
            return;
        };
        let point = self.pointer.position();
        let output = input::output_at(&self.scene, point);
        let size = self
            .scene
            .window_info(icon)
            .map_or(Size::ZERO, nitro_scene::Window::frame_size);
        let offset = self
            .drag_icon_offsets
            .get(&icon)
            .copied()
            .unwrap_or_else(|| Point::new(-size.w / 2.0, -size.h / 2.0));
        let local = output.and_then(|id| self.scene.output_info(id)).map_or(
            Point::ZERO,
            |(rect, scale)| {
                let s = if scale > 0.0 { scale } else { 1.0 };
                Point::new(
                    ((point.x - rect.x as f32) / s + offset.x).round(),
                    ((point.y - rect.y as f32) / s + offset.y).round(),
                )
            },
        );
        if let Err(e) = self.scene.place_window(icon, output, local) {
            warn!("placing the drag icon: {e}");
        }
    }

    /// Re-derive the drop target at the pointer and tell whoever needs to
    /// know: `DragLeave`/`DragEnter` on a change, `DragMotion` otherwise
    /// (only when the pointer `moved`). The icon follows the pointer.
    fn drive_dnd(&mut self, time_ns: u64, moved: bool) {
        if moved {
            self.place_drag_icon();
        }
        let point = self.pointer.position();
        let hit =
            input::output_at(&self.scene, point).and_then(|id| input::hit(&self.scene, id, point));
        // A target is a window whose client listed `DATA` (rule 1: never
        // push what a client did not list) and that the lock admits.
        // Anything else is "no target", which rejects.
        let target = hit.and_then(|t| {
            let (token, client) = self
                .wire_clients
                .iter()
                .find(|(_, c)| c.owns_window(t.window))?;
            let listed = client.client_caps & nitro_wire::types::caps::DATA != 0;
            (listed && self.scene.admits_window(t.window) && !self.drag_icons.contains(&t.window))
                .then_some((
                    data::DragTarget {
                        token: *token,
                        window: t.window,
                    },
                    t.local,
                ))
        });
        let Some(dnd) = self.dnd.as_mut() else {
            return;
        };
        match dnd.retarget(target.map(|(t, _)| t)) {
            data::Retarget::Same => {
                if let Some((t, pos)) = target.filter(|_| moved) {
                    let sent_to = self.send_input(t.window, |id| {
                        ServerMsg::DragMotion(msg::DragMotion {
                            window: id,
                            pos,
                            time_ns,
                        })
                    });

                    self.note_client_input(sent_to);
                }
            }
            data::Retarget::Changed { left } => {
                let (actions, mimes) = (dnd.actions, dnd.mimes.clone());
                if let Some(l) = left {
                    self.send_to_window(l.window, |id| {
                        ServerMsg::DragLeave(msg::DragLeave { window: id })
                    });
                }
                if let Some((t, pos)) = target {
                    let sent_to = self.send_input(t.window, |id| {
                        ServerMsg::DragEnter(msg::DragEnter {
                            window: id,
                            pos,
                            actions,
                            mimes: mimes.clone(),
                        })
                    });
                    self.note_client_input(sent_to);
                }
                // A new target has accepted nothing yet.
                self.set_cursor(Some(self.dnd_shape()));
            }
        }
    }

    /// An Escape key event for the grabs; returns whether it was consumed.
    ///
    /// A grabbing popup chain owns Escape: it dismisses the whole chain and
    /// is consumed, and so is its release. `cancel_tap` rather than feeding
    /// `hotkeys.key`: something that was not a bare-modifier tap happened,
    /// and discarding a binding list here would swallow a hotkey's release
    /// half. A drag-and-drop owns it the same way ([`Server::dnd_escape`]),
    /// and is asked first; the two never coexist.
    fn escape_grabs(&mut self, pressed: bool, time_ns: u64) -> bool {
        if self.dnd_escape(pressed, time_ns) {
            return true;
        }
        if !pressed && self.popup_seat.escape_consumed {
            self.popup_seat.escape_consumed = false;
            self.note_input(time_ns);
            return true;
        }
        if pressed && let Some(root) = self.popup_seat.grab {
            self.hotkeys.cancel_tap();
            self.dismiss_chain(root);
            self.popup_seat.escape_consumed = true;
            self.note_input(time_ns);
            return true;
        }
        false
    }

    /// The drag's share of an Escape key event; returns whether it was
    /// consumed. A press cancels a drag holding the pointer, as a rejected
    /// drop, and its release is swallowed. There is never a popup grab to
    /// compete: none can exist during a drag.
    fn dnd_escape(&mut self, pressed: bool, time_ns: u64) -> bool {
        if !pressed && self.dnd_escape_consumed {
            self.dnd_escape_consumed = false;
        } else if pressed && self.dnd_grabbing() {
            self.hotkeys.cancel_tap();
            self.dnd_step(data::Dnd::cancel);
            self.dnd_escape_consumed = true;
        } else {
            return false;
        }
        self.note_input(time_ns);
        true
    }

    /// The drag's share of a button event. Returns whether it consumed
    /// it. Called at the very top of `pointer_button`, with the button
    /// state already recorded.
    fn dnd_button(&mut self, state: ButtonState, time_ns: u64) -> bool {
        if self.dnd_swallow {
            // A drag ended with buttons still held: nobody saw their
            // presses, so nobody may see their releases.
            if !self.pointer.any_button_down() {
                self.dnd_swallow = false;
            }
            self.note_input(time_ns);
            return true;
        }
        if !self.dnd_grabbing() {
            return false;
        }
        // Every press and release during the drag is the drag's; the one
        // that brings the last button up drops.
        if state == ButtonState::Released && !self.pointer.any_button_down() {
            self.dnd_step(data::Dnd::release);
        }
        self.note_input(time_ns);
        true
    }

    /// Run one transition of the drag and act on what it says: send the
    /// messages, end the pointer grab if it ended, and forget the drag if
    /// it is over.
    fn dnd_step(&mut self, f: impl FnOnce(&mut data::Dnd) -> data::Outcome) {
        let Some(dnd) = self.dnd.as_mut() else {
            return;
        };
        let was_grabbing = dnd.grabbing();
        let icon = dnd.icon;
        let outcome = f(dnd);
        // A release forgets the drag outright, whatever phase it was in.
        let released_now = matches!(outcome, data::Outcome::Released { .. });
        let (source, still_grabbing) = (dnd.source, dnd.grabbing() && !released_now);
        let released = match outcome {
            data::Outcome::Nothing => None,
            data::Outcome::Drop(t) => {
                self.dnd_drops += 1;
                self.dnd_send(t.window, |id| {
                    ServerMsg::DragDrop(msg::DragDrop { window: id })
                });
                None
            }
            data::Outcome::Finished {
                leave,
                accepted,
                action,
            } => {
                if let Some(l) = leave {
                    self.dnd_send(l.window, |id| {
                        ServerMsg::DragLeave(msg::DragLeave { window: id })
                    });
                }
                if !accepted {
                    self.dnd_cancels += 1;
                }
                self.send_drag_finished(source, accepted, action);
                None
            }
            data::Outcome::Released { leave } => Some(leave),
        };
        if let Some(Some(l)) = released {
            self.dnd_send(l.window, |id| {
                ServerMsg::DragLeave(msg::DragLeave { window: id })
            });
        }
        if was_grabbing && !still_grabbing {
            if released.is_some() {
                // Released mid-grab (the source went): it never reached an
                // outcome, which counts as cancelled.
                self.dnd_cancels += 1;
            }
            self.end_dnd_grab(icon);
        } else if still_grabbing {
            // The target may have gone from under the pointer.
            self.set_cursor(Some(self.dnd_shape()));
        }
        if released.is_some() {
            self.dnd = None;
            self.release_dnd_transfers();
        }
    }

    /// The pointer grab is over: the icon comes down (not at the source's
    /// `FinishDrag` — a source that never finishes must not leave an image
    /// on screen), the cursor is re-derived, and pointer focus comes back
    /// through the stationary re-check, which sends the `PointerEnter`.
    fn end_dnd_grab(&mut self, icon: Option<WindowKey>) {
        if let Some(icon) = icon {
            let pos = self
                .scene
                .window_info(icon)
                .map_or(Point::ZERO, nitro_scene::Window::position);
            if let Err(e) = self.scene.place_window(icon, None, pos) {
                debug!("unmapping the drag icon: {e}");
            }
        }
        self.pointer_refresh = true;
        self.cursor_stale = true;
        if self.pointer.any_button_down() {
            self.dnd_swallow = true;
        }
    }

    /// Answer every parked drag transfer at EOF: the drag offer is gone.
    fn release_dnd_transfers(&mut self) {
        for t in self.data.cancel_drag() {
            self.answer_eof(t.requester, t.reply_to);
        }
    }

    /// Send a drag message to a window's client and make sure it goes out
    /// this wakeup: this can run from `disconnect` inside
    /// `flush_wire_clients`, after the recipient's own flush.
    fn dnd_send<F>(&mut self, win: WindowKey, build: F)
    where
        F: Fn(NodeId) -> ServerMsg,
    {
        if let Some(token) = self.send_to_window(win, build) {
            self.arm_wire_client(token);
        }
    }

    /// `DragFinished` to the source — or, if the source is out of the map
    /// because its own commit ended the drag (source == target, destroying
    /// the window dropped on), parked for `settle`. A token that is simply
    /// gone is dropped there.
    fn send_drag_finished(
        &mut self,
        token: u64,
        accepted: bool,
        action: nitro_wire::types::DragAction,
    ) {
        let Some(client) = self.wire_clients.get_mut(&token) else {
            self.pending_drag_finished.push((token, accepted, action));
            return;
        };
        client.send(&ServerMsg::DragFinished(msg::DragFinished {
            accepted,
            action,
        }));
        self.arm_wire_client(token);
    }

    /// `AcceptDrop`, at receipt. Ignored unless the sender is the target
    /// of a drag holding the pointer; a mismatch is a rejection, never an
    /// error — a stale answer to a previous drag is a legitimate race.
    fn accept_drop(
        &mut self,
        token: u64,
        action: nitro_wire::types::DragAction,
        mime: String,
    ) -> bool {
        if !self.data_allowed(token, "AcceptDrop") {
            return false;
        }
        // The target has answered the motion: release a flip held for it.
        self.defer.forget(token);
        if self
            .dnd
            .as_mut()
            .and_then(|d| d.accept(token, action, mime))
            .is_some()
        {
            self.set_cursor(Some(self.dnd_shape()));
        }
        true
    }

    /// `FinishDrag`, at receipt: the target completing a drop, or the
    /// source releasing a finished drag. See [`data::Dnd::finish`].
    fn finish_drag(&mut self, token: u64) -> bool {
        if !self.data_allowed(token, "FinishDrag") {
            return false;
        }
        self.dnd_step(|d| d.finish(token));
        true
    }
}

// ------------------------------------------------------------ overview mode

/// Overview mode: entering, leaving and selecting. The design is
/// `docs/wm.md` §Overview mode; the scene-level helpers are in
/// [`overview`], and this is the part that needs the server's state —
/// which windows, the work area, the text and icon engines, focus.
impl Server {
    /// The output in overview, if any.
    fn overview_output(&self) -> Option<SceneOutputId> {
        self.wm.overview().map(|o| o.output)
    }

    /// The pointer in desktop coordinates for a **frame** press: `None`
    /// on the output in overview, where no Super-drag and no frame region
    /// means anything (every frame is hidden or scaled).
    fn frame_point(&self) -> Option<Point> {
        self.pointer_desktop()
            .filter(|_| !self.pointer_in_overview())
    }

    /// Whether the pointer is on the output in overview.
    fn pointer_in_overview(&self) -> bool {
        self.pointer.output.is_some() && self.pointer.output == self.overview_output()
    }

    /// Whether `win` is the overview's scrim, which is a scene window but
    /// not an application's and must never be treated as one.
    fn is_scrim(&self, win: WindowKey) -> bool {
        self.wm.overview().is_some_and(|o| o.scrim == win)
    }

    /// An app icon for `app_id`, falling back to the `window` glyph: the
    /// chain [`Server::reicon`] uses for the title bar.
    fn resolve_app_icon(&mut self, app_id: &str, tint: nitro_core::Role) -> Option<(u32, u8)> {
        self.icons
            .lookup_app(app_id)
            .map(|icon| (icon.handle(), icon.role(tint)))
            .or_else(|| {
                self.icons
                    .lookup(wm::icon_names::FALLBACK_APP)
                    .map(|handle| (handle, wm::role_byte(tint)))
            })
    }

    /// Put `output` into overview mode, leaving any other overview first.
    ///
    /// Every `Normal`-layer toplevel on the output — **including
    /// `Minimized` ones** — is scaled onto a slot of
    /// [`overview::layout`] over the output's **work area** (so the bar
    /// keeps its strip and stays usable), its decorations are hidden, and
    /// it gets an unscaled icon-and-caption badge. The scrim goes under
    /// all of them. Zero windows is a valid overview: just the scrim.
    ///
    /// With `animate` the badges fade in over [`overview::BADGE_FADE_NS`]
    /// (see [`Server::step_overview_fade`]); a relayout passes `false`, so
    /// a window mapping or closing does not re-flash every badge.
    fn enter_overview(&mut self, output: SceneOutputId, animate: bool) {
        if self.wm.overview().is_some() {
            self.leave_overview(None);
        }
        if self.lock.is_locked() {
            return;
        }
        let Some((rect, scale)) = self.scene.output_info(output) else {
            return;
        };
        let s = if scale > 0.0 { scale } else { 1.0 };
        let size = Size::new(rect.w as f32 / s, rect.h as f32 / s);
        // A drag must not carry on under a scaled window — nor an implicit
        // grab: the thumbnail's client gets its `PointerLeave` from the
        // refresh below and drops its pressed state; the eventual release
        // is swallowed by overview, like a frame drag's.
        let _ = self.wm.end_drag();
        self.pointer.grab = None;
        let wins: Vec<WindowKey> = self
            .scene
            .windows(output)
            .filter(|w| !self.drag_icons.contains(w) && overview::wants_thumb(&self.scene, *w))
            .collect();
        for w in &wins {
            self.dismiss_popups_of(*w);
        }
        let thumbs: Vec<overview::Thumb> = wins
            .iter()
            .filter_map(|w| {
                self.scene
                    .window_info(*w)
                    .ok()
                    .map(|i| overview::thumb_of(*w, i))
            })
            .collect();
        let area = overview::grid_area(self.local_work_area(output));
        let slots = overview::layout(&thumbs, area, size.h);
        let scrim = match overview::create_scrim(&mut self.scene, output, size) {
            Ok(w) => w,
            Err(e) => {
                warn!("creating the overview scrim: {e}");
                return;
            }
        };
        let scrim_root = self
            .scene
            .window_info(scrim)
            .map(nitro_scene::Window::root)
            .ok();
        // Atlas mode when this output has one (allocated with the output;
        // nothing here allocates a buffer). Without it: the snap path,
        // exactly as before the atlas.
        let atlas = self
            .outputs
            .iter()
            .find(|o| o.scene_id == output)
            .and_then(|o| o.atlas)
            .filter(|_| scrim_root.is_some());
        let mut states: Vec<overview::ThumbState> = slots
            .into_iter()
            .filter_map(|slot| self.make_thumb(slot, scrim_root, atlas.map(|a| (a, s))))
            .collect();
        // Atlas mode: every badge goes in the scrim, after every image so
        // it draws on top of them. The frame roots are offscreen, so a
        // badge hung off one would not paint.
        if atlas.is_some() {
            self.build_scrim_badges(&mut states, scrim_root);
        }
        let scrim_rect = atlas.and_then(|_| overview::scrim_rect(&self.scene, scrim));
        info!(
            "overview on output {}: {} thumbnail(s)",
            output.0,
            states.len()
        );
        // Only with a badge to fade: a step that changes nothing damages
        // nothing, so no flip would come to finish the fade — and an
        // empty overview (just the scrim) would never read as settled.
        // In atlas mode the scrim fades too, and the thumbnails slide in
        // from where their windows were. Only with a thumbnail: an empty
        // overview's scrim at opacity 0 would damage nothing, so no flip
        // would come to step the fade and it would never settle.
        let fade_start_ns = (animate
            && !states.is_empty()
            && (atlas.is_some() || states.iter().any(|t| t.badge.is_some())))
        .then(|| {
            overview::set_badge_opacity(&mut self.scene, &states, 0.0);
            if atlas.is_some() {
                overview::set_scrim_opacity(&mut self.scene, scrim_rect, 0.0);
                overview::place_thumb_images(&mut self.scene, &states, 0.0, s);
            }
            monotonic_ns()
        });
        self.wm.begin_overview(overview::Overview {
            output,
            scrim,
            thumbs: states,
            fade_start_ns,
            grid_hidden: false,
            atlas: atlas.is_some(),
            rendered: false,
            scrim_rect,
        });
        // Every thumbnail keeps its title bar, scaled; its title is
        // reshaped at a size that lands on whole device pixels, so the
        // thumbnails share a few glyph sizes instead of one each.
        let framed: Vec<WindowKey> = self
            .wm
            .overview()
            .map(|o| o.thumbs.iter().map(|t| t.window).collect())
            .unwrap_or_default();
        for win in framed {
            if self.decorations.contains_key(&win) {
                self.retitle(win);
            }
        }
        // No frame affordance survives: every frame on this output is
        // scaled and inert, and a lit border would be misleading.
        self.set_resize_hint(None);
        self.set_button_hover(None);
        self.pointer_refresh = true;
        self.cursor_stale = true;
    }

    /// Turn one window into a thumbnail on `slot`: scale it (frame and
    /// all), un-hide it if minimized, badge it. `None` when the window is
    /// gone or the scene refuses the transform.
    ///
    /// With `atlas` (the output's atlas and scale) the window also goes
    /// offscreen and gets an image node in the scrim instead of a badge;
    /// the caller builds the badges afterwards, on top of every image.
    fn make_thumb(
        &mut self,
        slot: overview::Slot,
        scrim_root: Option<nitro_scene::NodeKey>,
        atlas: Option<(overview::Atlas, f32)>,
    ) -> Option<overview::ThumbState> {
        let win = slot.window;
        let info = self.scene.window_info(win).ok()?;
        // Where the entry slide starts: the thumbnail centred on the
        // window as it was. A minimized window was nowhere; it starts on
        // its slot.
        let start = if info.state() == WindowState::Minimized {
            slot.pos
        } else {
            let f = info.frame_rect();
            Point::new(
                f.x + f.w / 2.0 - slot.size.w / 2.0,
                f.y + f.h / 2.0 - slot.size.h / 2.0,
            )
        };
        let (root, framed, frame_size, minimized) = (
            info.root(),
            info.is_framed(),
            info.frame_size(),
            info.state() == WindowState::Minimized,
        );
        let app_id = info.app_id().to_owned();
        let (saved_transform, saved_position) =
            match overview::apply_thumb(&mut self.scene, win, &slot) {
                Ok(saved) => saved,
                Err(e) => {
                    warn!("scaling a thumbnail: {e}");
                    return None;
                }
            };
        let unhid = minimized && self.scene.set_visible(ClientId::SERVER, root, true).is_ok();
        let (badge, image) = if let (Some((atlas, scale)), Some(parent)) = (atlas, scrim_root) {
            if let Err(e) = self.scene.set_offscreen(win, true) {
                warn!("taking a thumbnail offscreen: {e}");
            }
            let image = overview::build_thumb_image(&mut self.scene, parent, &atlas, &slot, scale)
                .map_err(|e| warn!("building a thumbnail image: {e}"))
                .ok();
            (None, image)
        } else {
            let frame = framed.then_some((root, frame_size));
            (
                self.build_thumb_badge(&slot, &app_id, frame, scrim_root),
                None,
            )
        };
        let r = slot.rect();
        Some(overview::ThumbState {
            window: win,
            slot,
            hit: Rect::new(r.x, r.y, r.w, r.h + overview::badge_below()),
            saved_transform,
            saved_position,
            unhid,
            badge,
            image,
            start,
        })
    }

    /// Atlas mode's badges: every one in the scrim, built after every
    /// image so it draws on top.
    fn build_scrim_badges(
        &mut self,
        states: &mut [overview::ThumbState],
        scrim_root: Option<nitro_scene::NodeKey>,
    ) {
        for t in states {
            let app_id = self
                .scene
                .window_info(t.window)
                .map(|i| i.app_id().to_owned())
                .unwrap_or_default();
            t.badge = self.build_thumb_badge(&t.slot, &app_id, None, scrim_root);
        }
    }

    /// One thumbnail's badge: the app icon, drawn unscaled. Returns the
    /// group.
    ///
    /// `frame` is the framed window's `(root, frame size)`: the badge
    /// hangs off its (scaled) frame root with a counter-scale. An
    /// undecorated window's root is the client's clipping content group,
    /// where a badge would be cut off at the window's edge, so its badge
    /// goes in the scrim instead, at the slot's bottom-centre — under the
    /// thumbnail, which covers the top 70 % of the icon.
    fn build_thumb_badge(
        &mut self,
        slot: &overview::Slot,
        app_id: &str,
        frame: Option<(nitro_scene::NodeKey, Size)>,
        scrim_root: Option<nitro_scene::NodeKey>,
    ) -> Option<nitro_scene::NodeKey> {
        let (parent, origin, inv) = if let Some((root, size)) = frame {
            (root, Point::new(size.w / 2.0, size.h), 1.0 / slot.scale)
        } else {
            (
                scrim_root?,
                Point::new(slot.pos.x + slot.size.w / 2.0, slot.pos.y + slot.size.h),
                1.0,
            )
        };
        let icon = self.resolve_app_icon(app_id, nitro_core::Role::Text);
        let badge = overview::Badge { icon };
        match overview::build_badge(&mut self.scene, parent, origin, inv, &badge) {
            Ok(group) => Some(group),
            Err(e) => {
                warn!("building a thumbnail badge: {e}");
                None
            }
        }
    }

    /// The size a framed window's title is shaped at: the title bar's
    /// own, or — for a thumbnail — that size snapped so it rasterizes at a
    /// whole device pixel size under the thumbnail's scale (see
    /// [`overview::snapped_text_size`]).
    fn title_size_px(&self, win: WindowKey) -> f32 {
        let px = wm::theme::TITLE_SIZE_PX;
        let Some(ov) = self.wm.overview() else {
            return px;
        };
        let Some(t) = ov.thumbs.iter().find(|t| t.window == win) else {
            return px;
        };
        let out = self
            .scene
            .output_info(ov.output)
            .map_or(1.0, |(_, s)| if s > 0.0 { s } else { 1.0 });
        overview::snapped_text_size(px, t.slot.scale * out)
    }

    /// Whether outputs should carry an overview thumbnail atlas: the
    /// environment's override, else `overview.animate` (default off).
    fn wants_atlas(&self) -> bool {
        self.overview_atlas
            .unwrap_or_else(|| self.settings.overview.animate())
    }

    /// Bring every output's atlas in line with [`Server::wants_atlas`]:
    /// allocate the missing ones (a failure leaves that output snapping,
    /// as at startup) or free them. An overview animating out of an atlas
    /// about to be freed is left first — its images reference the buffer
    /// — exactly as the resize path in `layout_outputs` does. Turning the
    /// setting on while a snap overview is open only allocates: that
    /// overview stays snap, and the next entry animates. Idempotent.
    fn apply_overview_atlas(&mut self) {
        let want = self.wants_atlas();
        for i in 0..self.outputs.len() {
            let (scene_id, w, h) = {
                let o = &self.outputs[i];
                (o.scene_id, o.width, o.height)
            };
            match (want, self.outputs[i].atlas) {
                (true, None) => {
                    self.outputs[i].atlas = overview::Atlas::allocate(&mut self.scene, w, h);
                }
                (false, Some(atlas)) => {
                    if self
                        .wm
                        .overview()
                        .is_some_and(|o| o.output == scene_id && o.atlas)
                    {
                        self.leave_overview(None);
                    }
                    atlas.free(&mut self.scene);
                    self.outputs[i].atlas = None;
                }
                _ => {}
            }
        }
    }

    /// Leave overview mode, putting every window back exactly as it was,
    /// then — when `select` names a thumbnail — un-minimize, raise and
    /// focus it.
    fn leave_overview(&mut self, select: Option<WindowKey>) {
        let Some(ov) = self.wm.take_overview() else {
            return;
        };
        let s = ClientId::SERVER;
        // Search had hidden the grid: show it again first, so the restore
        // below starts from the state it was written against — and the
        // minimized re-hide re-hides exactly the set this overview un-hid.
        if ov.grid_hidden {
            overview::set_grid_visible(&mut self.scene, &ov, true);
        }
        for t in &ov.thumbs {
            let Ok(info) = self.scene.window_info(t.window) else {
                continue;
            };
            let (root, framed, state) = (info.root(), info.is_framed(), info.state());
            // An undecorated window's badge lives in the scrim and goes
            // with it; a framed one's hangs off the frame root — except in
            // atlas mode, where every badge and image is in the scrim.
            if framed
                && !ov.atlas
                && let Some(badge) = t.badge
            {
                let _ = self.scene.destroy_node(s, badge);
            }
            // Back on the output before the restore, so the move back is
            // ordinary output damage.
            if ov.atlas {
                let _ = self.scene.set_offscreen(t.window, false);
            }
            if let Err(e) = overview::restore_thumb(
                &mut self.scene,
                t.window,
                t.saved_transform,
                t.saved_position,
            ) {
                warn!("restoring a thumbnail: {e}");
            }
            // The title goes back to the title bar's own size: the
            // overview is taken, so `title_size_px` answers that.
            if framed {
                self.retitle(t.window);
            }
            // Re-hide exactly the set this overview un-hid, and only those
            // still minimized: one un-minimized meanwhile is showing.
            if t.unhid && state == WindowState::Minimized {
                let _ = self.scene.set_visible(s, root, false);
            }
        }
        if let Err(e) = self.scene.destroy_window(s, ov.scrim) {
            warn!("destroying the overview scrim: {e}");
        }
        info!("overview off");
        if let Some(win) = select.filter(|w| ov.contains(*w)) {
            // Un-minimize first: `focusable` refuses a minimized window.
            if self
                .scene
                .window_info(win)
                .is_ok_and(|i| i.state() == WindowState::Minimized)
            {
                self.set_state(win, WindowState::Normal);
            }
            self.raise_and_focus(win);
        }
        self.pointer_refresh = true;
        self.cursor_stale = true;
    }

    /// Leave and re-enter on the same output: a window came or went, the
    /// palette changed, or a thumbnail's geometry did.
    ///
    /// Carries [`overview::Overview::grid_hidden`] across: a window mapping
    /// while the shell is showing search results must not bring the grid
    /// back. The leave shows the roots and the enter hides them again, all
    /// before the next scene update, so the round trip is one repaint of
    /// the relaid-out output like any relayout.
    fn relayout_overview(&mut self) {
        if let Some(output) = self.overview_output() {
            let hidden = self.wm.overview().is_some_and(|o| o.grid_hidden);
            self.leave_overview(None);
            self.enter_overview(output, false);
            if hidden {
                self.set_overview_grid(false);
            }
        }
    }

    /// Search results replace the grid (`visible == false`) or the grid
    /// comes back. A no-op outside overview mode or when nothing changes.
    ///
    /// Instant: one `SetVisible` per thumbnail root (and per scrim-held
    /// badge), no cross-fade — `docs/wm.md` §Overview mode says why that
    /// is deferred. While hidden, [`overview::Overview::slot_at`] selects
    /// nothing, so a click on the hidden grid leaves without selecting.
    /// Focus is untouched: nothing here asks [`Server::showing`] or
    /// [`Server::focusable`], and neither looks at the root this hides.
    fn set_overview_grid(&mut self, visible: bool) {
        let Some(ov) = self.wm.overview_mut() else {
            return;
        };
        if ov.grid_hidden != visible {
            return;
        }
        ov.grid_hidden = !visible;
        // A badge fade still running would step nodes that are no longer
        // drawn: no damage, no flip, and the fade would never read as
        // settled. Finish it now instead.
        if !visible && ov.fade_start_ns.take().is_some() {
            self.finish_overview_anim();
        }
        let Some(ov) = self.wm.overview() else {
            return;
        };
        overview::set_grid_visible(&mut self.scene, ov, visible);
    }

    /// Advance the overview badges' fade-in to `now_ns`
    /// (`CLOCK_MONOTONIC`, the clock `enter_overview` stamped the start
    /// with). `now_ns` must be strictly after the start stamp, or the
    /// step changes nothing and no further flip comes to step again.
    ///
    /// Driven from `on_flip`, not a timer: each step changes only the
    /// badges' opacity, which damages only their rects, which paints
    /// and flips, which steps again. The last step sets exactly `1.0`
    /// and clears `fade_start_ns`, so nothing is dirty afterwards and
    /// the desktop goes quiet — no timerfd is left running. With no
    /// flips (the output inactive, the VT switched away) the fade simply
    /// waits and, time having passed, snaps to `1.0` on the next flip.
    ///
    /// Returns whether it stepped, i.e. the scene needs an update.
    fn step_overview_fade(&mut self, now_ns: u64) -> bool {
        let Some(ov) = self.wm.overview_mut() else {
            return false;
        };
        let Some(start) = ov.fade_start_ns else {
            return false;
        };
        let elapsed = now_ns.saturating_sub(start);
        if elapsed >= overview::BADGE_FADE_NS {
            ov.fade_start_ns = None;
        }
        let t = overview::enter_progress(elapsed);
        self.set_overview_anim(t);
        true
    }

    /// Put the entry animation at progress `t` (`1.0` is settled): the
    /// badges' opacity, and in atlas mode the scrim's opacity and every
    /// thumbnail image's position. All server nodes; the images are 1:1
    /// copies of the atlas wherever they are, so no frame downscales.
    fn set_overview_anim(&mut self, t: f32) {
        let Some(ov) = self.wm.overview() else {
            return;
        };
        overview::set_badge_opacity(&mut self.scene, &ov.thumbs, t);
        if ov.atlas {
            let scale = self
                .scene
                .output_info(ov.output)
                .map_or(1.0, |(_, s)| if s > 0.0 { s } else { 1.0 });
            overview::set_scrim_opacity(&mut self.scene, ov.scrim_rect, t);
            overview::place_thumb_images(&mut self.scene, &ov.thumbs, t, scale);
        }
    }

    /// Jump the entry animation to its settled state (exact final values).
    /// The caller has cleared the stamp.
    fn finish_overview_anim(&mut self) {
        self.set_overview_anim(1.0);
    }

    /// A click (or a touch-down) at device point `point` on `output`,
    /// over no `Top`/`Overlay` window: select the thumbnail under it, or
    /// leave without selecting when it is on the bare scrim — GNOME's
    /// behaviour.
    fn overview_click(&mut self, output: SceneOutputId, point: Point) {
        let Some(ov) = self.wm.overview().filter(|o| o.output == output) else {
            return;
        };
        let Some((rect, scale)) = self.scene.output_info(output) else {
            return;
        };
        let s = if scale > 0.0 { scale } else { 1.0 };
        let local = Point::new((point.x - rect.x as f32) / s, (point.y - rect.y as f32) / s);
        let select = ov.slot_at(local);
        self.leave_overview(select);
    }

    /// What the pointer at device `point` on `output` is over, for pointer
    /// focus: [`input::hit`], except on the output in overview, where a
    /// `Normal`-layer hit belongs to the window manager and is `None` —
    /// so the thumbnail's client gets a `PointerLeave` and nothing after.
    fn pointer_target(&self, output: SceneOutputId, point: Point) -> Option<input::PointerTarget> {
        if self.overview_output() == Some(output) {
            input::overview_hit(&self.scene, output, point)
        } else {
            input::hit(&self.scene, output, point)
        }
    }

    /// Everything that may consume a button event before the ordinary
    /// path sees it, in order: a drag-and-drop, a popup grab, overview
    /// mode. See `Server::dnd_button` and `Server::popup_grab_button`.
    fn button_grabs(&mut self, button: u32, state: ButtonState, time_ns: u64) -> bool {
        self.dnd_button(state, time_ns)
            || self.popup_grab_button(state, time_ns)
            || self.overview_button(button, state, time_ns)
    }

    /// Overview mode's share of a button event; returns whether it was
    /// consumed. Runs after the drag-and-drop and popup grabs.
    ///
    /// Over a `Top` or `Overlay` window (`pointer.over` is set) the event
    /// falls through to the ordinary client path. Anywhere else on the
    /// output in overview a left press selects (or, on the bare scrim,
    /// leaves) and every other button event is dropped — including the
    /// release of that press, which arrives after the overview is gone.
    fn overview_button(&mut self, button: u32, state: ButtonState, time_ns: u64) -> bool {
        if state == ButtonState::Released && self.overview_swallow == Some(button) {
            self.overview_swallow = None;
            self.note_input(time_ns);
            return true;
        }
        let point = self.pointer.position();
        let Some(output) = self.overview_output() else {
            return false;
        };
        if input::output_at(&self.scene, point) != Some(output) || self.pointer.over.is_some() {
            return false;
        }
        if state == ButtonState::Pressed && button == input::BTN_LEFT {
            self.hotkeys.cancel_tap();
            self.overview_swallow = Some(button);
            self.overview_click(output, point);
        }
        self.note_input(time_ns);
        true
    }

    /// Apply one shell `SetOverview`. The server is authoritative: this
    /// is a request, and whatever it did is what `announce_overview`
    /// then reports. `Enter` opens on the output under the pointer, else
    /// the primary one, and is refused while locked (`enter_overview`
    /// refuses it).
    fn apply_overview_request(&mut self, request: nitro_wire::types::OverviewRequest) {
        use nitro_wire::types::OverviewRequest as R;
        self.overview_requests += 1;
        let target = self.pointer.output.or_else(|| self.primary_output());
        let active = self.overview_output();
        match request {
            R::Watch => {}
            R::Leave => self.leave_overview(None),
            R::Enter => {
                if let Some(out) = target
                    && active != Some(out)
                {
                    self.enter_overview(out, true);
                }
            }
            R::Toggle => {
                if active.is_some() {
                    self.leave_overview(None);
                } else if let Some(out) = target {
                    self.enter_overview(out, true);
                }
            }
            R::Search => self.set_overview_grid(false),
            R::Grid => self.set_overview_grid(true),
        }
    }

    /// Tell the overview watchers what changed. When the state differs
    /// from the last one announced every watcher hears it; otherwise only
    /// `requester` does, because every `SetOverview` gets an answer.
    fn announce_overview(&mut self, requester: Option<u64>) {
        let now = self.overview_output();
        let state = ServerMsg::OverviewState(msg::OverviewState {
            active: now.is_some(),
            output: now.map_or(0, |o| o.0),
        });
        let to: Vec<u64> = if now == self.overview_announced {
            requester
                .filter(|t| self.overview_watchers.contains(t))
                .into_iter()
                .collect()
        } else {
            self.overview_announced = now;
            self.overview_watchers.clone()
        };
        for token in to {
            if let Some(client) = self.wire_clients.get_mut(&token) {
                client.send(&state);
            }
        }
    }

    /// The `overview` control request: the test and debug way in. The
    /// shell's is `SetOverview`; both end at `settle`, which announces
    /// the change to the shell's watchers.
    fn overview_request(&mut self, on: bool, output: Option<&str>) -> Vec<u8> {
        if on {
            let id = match self.shot_output(output) {
                Ok(id) => SceneOutputId(id.0),
                Err(reply) => return reply,
            };
            if self.lock.is_locked() {
                return protocol::err_reply("the session is locked");
            }
            self.enter_overview(id, true);
        } else {
            self.leave_overview(None);
        }
        self.settle();
        protocol::ok_reply()
    }

    /// `input ...`: expand the request into timed events, route the ones
    /// due now through [`Server::route_input`] before replying, and queue
    /// the rest on the injection timer. See [`inject`] and the `protocol`
    /// module docs.
    fn input_request(&mut self, spec: &protocol::InputSpec) -> Vec<u8> {
        use protocol::InputAction;
        if !self.active {
            return protocol::err_reply("session inactive");
        }
        // `motion X Y OUTPUT`: X, Y are relative to that output's device
        // rectangle, so add its origin to reach the pointer's global space.
        let origin = match &spec.action {
            InputAction::Motion {
                output: Some(name), ..
            } => {
                let Some(info) = self.backend.outputs().iter().find(|o| &o.name == name) else {
                    return protocol::err_reply(&format!("no output named {name}"));
                };
                match self.scene.output_info(SceneOutputId(info.id.0)) {
                    Some((rect, _)) => (f64::from(rect.x), f64::from(rect.y)),
                    None => return protocol::err_reply(&format!("output {name} is not placed")),
                }
            }
            _ => (0.0, 0.0),
        };
        let now = monotonic_ns();
        let items = inject::expand(spec, now, origin);

        let n = items.len();
        let mut routed_now = false;
        for (due, what) in items {
            if due <= now {
                self.fire_injected(due, &what);
                routed_now = true;
            } else {
                self.injector.schedule(due, what);
            }
        }
        if routed_now {
            // As `on_input` does: the `ok` then means "routed and sent".
            self.flush_wire_clients();
            self.settle();
        }
        if let Err(e) = self.injector.rearm() {
            warn!("input injection: arm: {e}");
        }
        format!("ok {n}\n").into_bytes()
    }

    /// Route one injected event, stamped with its due time.
    fn fire_injected(&mut self, due: u64, what: &inject::Pending) {
        let event = match *what {
            // Absolute to relative against where the pointer is *now*:
            // `route_input` adds a `PointerMotion` delta to the position
            // without re-applying acceleration, so this lands exactly.
            inject::Pending::MotionTo { x, y } => InputEvent::PointerMotion {
                dx: x - self.pointer.x,
                dy: y - self.pointer.y,
                time_ns: due,
            },
            inject::Pending::Event(ref e) => e.clone(),
        };
        self.injector.injected += 1;
        self.route_input(&event);
    }

    /// The injection timer fired: route whatever of a scripted `input`
    /// sequence is due, then re-arm for the next.
    fn on_inject(&mut self) {
        let now = monotonic_ns();
        let due = self.injector.take_due(now);
        if let Some(&(first, _)) = due.first()
            && now.saturating_sub(first) > 5_000_000
        {
            debug!(
                "input injection: {} event(s) {} µs late",
                due.len(),
                (now - first) / 1_000
            );
        }
        let fired = !due.is_empty();
        if self.active {
            for (t, what) in &due {
                self.fire_injected(*t, what);
            }
        }
        if let Err(e) = self.injector.rearm() {
            warn!("input injection: re-arm: {e}");
        }
        if fired {
            self.flush_wire_clients();
            self.settle();
        }
    }
}

/// Tell a client about every icon name in its commit the set did not have.
///
/// The **only** non-fatal error a local client can earn, and deliberately
/// so: the node was cleared, the rest of the batch applied, and saying so
/// costs a gap in the UI rather than an application. Every other error in
/// this protocol closes the connection, which is exactly why this one
/// needs to be written down somewhere a reader will find it — see
/// `docs/icons.md`.
///
/// Two forms of the same fact, chosen per client:
///
/// * a client that listed `ICONS` in its `ClientCaps` gets
///   `IconRefused { serial, node, name }` (0x8303), which names the node
///   so a toolkit can route the failure to the widget that asked;
/// * any other client gets `Error { BadIcon, "no icon named \"…\"" }`,
///   exactly as before `IconRefused` existed — it may not know the op,
///   and an unknown op is fatal to it (`docs/wire.md` § Capability
///   opt-in, rule 1).
///
/// Sent *after* the transaction was applied, like `TextMetrics`, so a
/// client sees the whole commit take effect before the complaint about
/// one node of it.
fn report_bad_icons(client: &mut clients::WireClient, serial: u32, bad: Vec<(NodeId, String)>) {
    let opted_in = client.client_caps & nitro_wire::types::caps::ICONS != 0;
    for (node, name) in bad {
        warn!(
            "client {}: node {} asked for unknown icon {name:?}",
            client.id.0,
            node.raw()
        );
        if opted_in {
            client.send(&ServerMsg::IconRefused(msg::IconRefused {
                serial,
                node,
                name,
            }));
        } else {
            client.send(&ServerMsg::Error(msg::Error {
                serial,
                code: ErrorCode::BadIcon,
                msg: format!("no icon named {name:?}"),
            }));
        }
    }
}

/// Everything [`paint_shadow`] borrows from the server, apart from the
/// shadow itself (which lives in the output the server also borrows).
struct ShadowPaint<'a> {
    scene: &'a Scene,
    text: &'a mut crate::text::TextEngine,
    icons: &'a mut IconEngine,
    items: &'a mut Vec<nitro_scene::PaintItem>,
    palette: &'a nitro_core::Palette,
    output: SceneOutputId,
    bounds: nitro_core::IRect,
    cursor: (&'a Cursor, CursorState),
    /// [`frame::paint_region`]'s `fast_scaled`: this output shows a snap
    /// overview.
    fast_scaled: bool,
}

/// What [`paint_shadow`] did, for the statistics.
struct ShadowPainted {
    paint_us: u64,
    blitted: bool,
    raster_px: u64,
    moved_px: u64,
}

/// Rasterize `rasterize` into the shadow — through the scroll blit when a
/// hint is pending and every precondition holds (`scroll_blit_region`),
/// the ordinary paint otherwise. The timer covers the region arithmetic
/// too: deciding what may be moved is part of what the blit costs.
fn paint_shadow(
    shadow: &mut frame::Shadow,
    p: &mut ShadowPaint<'_>,
    rasterize: &[nitro_core::IRect],
    scroll: Option<frame::PendingScroll>,
) -> ShadowPainted {
    let start = Instant::now();
    // The cursor is drawn over whatever the blit moves, so its current
    // rect is never copied into or out of.
    let (_, cs) = p.cursor;
    let cursor_rect = Cursor::rect_scaled(cs.x, cs.y, cs.shape, cs.scale);
    let blit = scroll.filter(|_| shadow.is_complete()).and_then(|s| {
        scroll_blit_region(p.scene, p.output, p.bounds, rasterize, &s, cursor_rect)
            .map(|d| (s.delta, d))
    });
    let mut raster_px = frame::region_area(rasterize);
    let mut moved_px = 0;
    if let Some((delta, moved)) = &blit {
        shadow.translate_region(moved, delta.0, delta.1);
        let leftover = nitro_core::Region::from_rects(rasterize)
            .subtract(moved)
            .rects();
        raster_px = frame::region_area(&leftover);
        moved_px = moved.area().cast_unsigned();
        frame::paint_region_shared(
            &mut shadow.canvas(),
            p.scene,
            &mut *p.text,
            &mut *p.icons,
            p.output,
            &leftover,
            p.cursor,
            &mut *p.items,
            p.palette,
            p.fast_scaled,
        );
    } else {
        frame::paint_region(
            &mut shadow.canvas(),
            p.scene,
            &mut *p.text,
            &mut *p.icons,
            p.output,
            rasterize,
            p.cursor,
            &mut *p.items,
            p.palette,
            p.fast_scaled,
        );
    }
    ShadowPainted {
        paint_us: u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
        blitted: blit.is_some(),
        raster_px,
        moved_px,
    }
}

/// The pixels this paint may move rather than rasterize, when every
/// precondition of the scroll blit holds: the hint is not blocked, the
/// output starts at the device origin (so its local pixels are the global
/// ones `paint_list` works in, exactly as `frame::paint_region` uses it),
/// the scene can describe the moved subtree's cover, and the resulting
/// region is non-empty. See [`frame::blit_region`] for the rule.
fn scroll_blit_region(
    scene: &Scene,
    output: SceneOutputId,
    bounds: nitro_core::IRect,
    rasterize: &[nitro_core::IRect],
    scroll: &frame::PendingScroll,
    cursor: nitro_core::IRect,
) -> Option<nitro_core::Region> {
    if scroll.blocked {
        return None;
    }
    let (rect, _) = scene.output_info(output)?;
    if (rect.x, rect.y) != (0, 0) {
        return None;
    }
    let (cover, above) =
        scene.translation_cover(output, scroll.node, scroll.moves_node, &scroll.clip)?;
    let mut foreign: Vec<nitro_core::IRect> = scroll.foreign.rects().to_vec();
    foreign.push(cursor);
    frame::blit_region(
        scroll.delta,
        scroll.clip,
        bounds,
        rasterize,
        &cover,
        &above,
        &nitro_core::Region::from_rects(&foreign),
    )
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
    fn the_fake_config_puts_all_three_sockets_in_one_directory() {
        let c = Config::fake(320, 200, "/run/x/nitro/control.sock");
        assert_eq!(c.control_path, PathBuf::from("/run/x/nitro/control.sock"));
        assert_eq!(c.wire_path, PathBuf::from("/run/x/nitro/wire.sock"));
        assert_eq!(c.shell_path, PathBuf::from("/run/x/nitro/shell.sock"));
        assert!(!c.handle_signals);
        assert!(c.input_dir.is_none());
    }

    #[test]
    fn only_shell_tokens_are_privileged() {
        // The token range *is* the capability check, so it is worth an
        // assertion of its own: a wire token must never read as a shell one,
        // however many clients have connected.
        assert!(Server::is_shell(TOK_SHELL_BASE));
        assert!(Server::is_shell(TOK_SHELL_BASE + 9_999));
        assert!(!Server::is_shell(TOK_WIRE_BASE));
        assert!(!Server::is_shell(TOK_WIRE_BASE + 9_999));
        assert!(!Server::is_shell(TOK_CLIENT_BASE));
        assert!(!Server::is_shell(TOK_WIRE_LISTENER));
        assert!(!Server::is_shell(TOK_SHELL_LISTENER));
        // A **remote** token sorts above the shell range, so `is_shell`
        // is a window and not a threshold. Asserted rather than assumed:
        // a remote client reading as privileged would hand `caps::SHELL`
        // — layers, hotkeys, other clients' windows — to anything that
        // can open a TCP port, which is the whole thing `docs/remote.md`
        // promises cannot happen.
        assert!(!Server::is_shell(TOK_REMOTE_BASE));
        assert!(!Server::is_shell(TOK_REMOTE_BASE + 9_999));
        assert!(!Server::is_shell(TOK_REMOTE_LISTENER));
        assert!(Server::is_remote(TOK_REMOTE_BASE));
        assert!(Server::is_remote(TOK_REMOTE_BASE + 9_999));
        assert!(!Server::is_remote(TOK_SHELL_BASE));
        assert!(!Server::is_remote(TOK_WIRE_BASE));
        assert!(!Server::is_remote(TOK_CLIENT_BASE));
    }

    #[test]
    fn every_shell_op_is_recognised_as_one() {
        use nitro_wire::types::{Edge, Layer, WindowRef};
        let shell: Vec<ClientMsg> = vec![
            msg::SetLayer {
                window: NodeId(1),
                layer: Layer::Top,
            }
            .into(),
            msg::SetExclusiveZone {
                window: NodeId(1),
                edge: Edge::Top,
                px: 1,
            }
            .into(),
            msg::SetAnchor {
                window: NodeId(1),
                edges: 0,
                margin: 0,
                output: 0,
            }
            .into(),
            msg::BindKey {
                id: 1,
                mods: 0,
                keysym: 1,
            }
            .into(),
            msg::UnbindKey { id: 1 }.into(),
            msg::GrabKeyboard {
                window: NodeId(1),
                on: true,
            }
            .into(),
            msg::WindowList.into(),
            msg::Outputs.into(),
            msg::Lock.into(),
            msg::Unlock.into(),
            msg::SetOverview {
                request: nitro_wire::types::OverviewRequest::Toggle,
            }
            .into(),
            msg::FocusWindow {
                window: WindowRef(1),
            }
            .into(),
            msg::CloseWindow {
                window: WindowRef(1),
            }
            .into(),
            msg::SetWindowStateFor {
                window: WindowRef(1),
                state: nitro_wire::types::WindowState::Normal,
            }
            .into(),
        ];
        for m in &shell {
            assert!(is_shell_op(m), "{} must need caps::SHELL", m.name());
            assert_eq!(
                m.op() & 0xff00,
                0x0400,
                "{} is in the shell op block",
                m.name()
            );
        }
        // And the ordinary ops are not: a false positive here would make the
        // whole unprivileged protocol unusable on the wire socket.
        for m in [
            ClientMsg::from(msg::Commit { serial: 1 }),
            msg::DestroyNode { id: NodeId(1) }.into(),
            msg::SetWindowState {
                window: NodeId(1),
                state: nitro_wire::types::WindowState::Normal,
            }
            .into(),
            msg::SetAppId {
                window: NodeId(1),
                app_id: String::new(),
            }
            .into(),
            // The clipboard ops are open to every local client: routing
            // them through `handle_shell_msg` would make paste a privilege.
            msg::SetSelection { mimes: Vec::new() }.into(),
            msg::RequestSelection {
                request: 1,
                source: nitro_wire::types::DataSource::Clipboard,
                mime: "text/plain".to_owned(),
            }
            .into(),
            // `ListOutputs` is answered in the shell block but is not a
            // shell op: folding it into `handle_shell_msg` would make
            // output enumeration a privilege again (M5-D).
            msg::ListOutputs.into(),
            // `SetCursor` (M5-E) is open to every client holding the
            // pointer; see `Server::set_cursor_request`.
            msg::SetCursor {
                shape: nitro_wire::types::CursorShape::Text,
            }
            .into(),
            // `StartMove` / `StartResize` (M5-F) are open to every client
            // holding the pointer with a button down.
            msg::StartMove { window: NodeId(1) }.into(),
            msg::StartResize {
                window: NodeId(1),
                edges: 0,
            }
            .into(),
        ] {
            assert!(!is_shell_op(&m), "{} is not a shell op", m.name());
        }
    }
}

// ------------------------------------------------- the GPU helper (#3922)

/// A client dma-buf's layout and fds as a helper texture source, when the
/// helper knows the format.
fn gpu_source(import: &dmabuf::ImportRequest) -> Option<gpu::Source> {
    let d = &import.desc;
    let n = nitro_gpu::proto::plane_count(d.format.0)?;
    if n != usize::from(d.planes) {
        return None;
    }
    let mut fds = Vec::with_capacity(n);
    for fd in &import.fds {
        fds.push(fd.try_clone().ok()?);
    }
    Some(gpu::Source {
        desc: nitro_gpu::proto::DmabufDesc {
            id: 0,
            w: d.width,
            h: d.height,
            fourcc: d.format.0,
            modifier: d.modifier,
            planes: (0..n)
                .map(|i| nitro_gpu::proto::PlaneDesc {
                    offset: d.offsets[i],
                    pitch: d.pitches[i],
                })
                .collect(),
            ..nitro_gpu::proto::DmabufDesc::default()
        },
        fds,
    })
}

const fn gpu_encoding(m: nitro_scene::ColorMatrix) -> nitro_gpu::proto::ColorEncoding {
    use nitro_gpu::proto::ColorEncoding as E;
    match m {
        nitro_scene::ColorMatrix::Bt601 => E::Bt601,
        nitro_scene::ColorMatrix::Bt709 => E::Bt709,
        nitro_scene::ColorMatrix::Bt2020 => E::Bt2020,
    }
}

const fn gpu_range(r: nitro_scene::ColorRange) -> nitro_gpu::proto::ColorRange {
    match r {
        nitro_scene::ColorRange::Limited => nitro_gpu::proto::ColorRange::Limited,
        nitro_scene::ColorRange::Full => nitro_gpu::proto::ColorRange::Full,
    }
}

/// At most [`nitro_gpu::proto::MAX_RECTS`] rects, in `bounds`: the
/// bounding box when there are more.
fn gpu_rects(rects: &[nitro_core::IRect], bounds: nitro_core::IRect) -> Vec<nitro_core::IRect> {
    let v: Vec<nitro_core::IRect> = rects
        .iter()
        .map(|r| r.intersect(&bounds))
        .filter(|r| !r.is_empty())
        .collect();
    if v.len() <= nitro_gpu::proto::MAX_RECTS {
        return v;
    }
    let b = v.iter().skip(1).fold(v[0], |a, r| a.union(r));
    vec![b]
}

impl Server {
    /// Start the helper (always-on at startup and resume, on demand when
    /// an output first wants mode 2).
    fn gpu_spawn(&mut self) {
        if self.gpu.running() {
            return;
        }
        // The helper refuses to start holding a DRM primary node or an
        // input device; every such fd of ours must be close-on-exec.
        for (fd, target) in gpu::inheritable_fds() {
            if target.starts_with("/dev/dri/card") || target.starts_with("/dev/input/") {
                warn!("fd {fd} ({target}) is not close-on-exec: the gpu helper will refuse it");
            }
        }
        self.gpu.spawn(&EpollPoll(&self.epoll), TOK_GPU);
    }

    /// The helper socket is ready.
    fn on_gpu(&mut self) {
        let (replies, gone) = self.gpu.pump(&EpollPoll(&self.epoll), TOK_GPU);
        for r in replies {
            self.on_gpu_reply(r);
        }
        if gone {
            self.gpu_lost();
        }
        self.settle();
    }

    fn on_gpu_reply(&mut self, r: gpu::Reply) {
        match r {
            gpu::Reply::Ready => {
                // Outputs with helper-able Surfaces decide again.
                for o in &mut self.outputs {
                    if !o.gpu_layers.is_empty() {
                        o.planes_dirty = true;
                    }
                }
            }
            gpu::Reply::Ring {
                size,
                fourcc,
                modifier,
                slots,
            } => self.gpu_import_ring(size, fourcc, modifier, slots),
            gpu::Reply::RingFailed => warn!("gpu helper: no output ring; mode 2 off for this output"),
            gpu::Reply::ShadowRefused => {
                warn!("gpu helper: shadow import refused; mode 2 off for this output");
            }
            gpu::Reply::Composited { serial, fence } => {
                let poll = EpollPoll(&self.epoll);
                let Some(f) = self
                    .gpu
                    .ring
                    .in_flight
                    .take()
                    .filter(|f| f.serial == serial)
                else {
                    // A frame of a ring since dropped: only its borrows.
                    self.gpu.keep_fence(
                        &poll,
                        TOK_GPU_FENCE_BASE + serial,
                        serial,
                        &fence,
                        Instant::now(),
                    );
                    return;
                };
                self.gpu
                    .composite_us
                    .push(f.sent.elapsed().as_micros() as u64);
                self.gpu.counters.frames += 1;
                self.gpu
                    .keep_fence(&poll, TOK_GPU_FENCE_BASE + serial, serial, &fence, f.sent);
                self.gpu.ring.composited = Some(gpu::Composited {
                    serial,
                    slot: f.slot,
                    fence,
                });
                self.gpu.arm();
                if let Some(owner) = self.gpu.owner
                    && let Some(i) = self.outputs.iter().position(|o| o.kms_id == owner)
                {
                    self.gpu_commit(i);
                }
            }
            gpu::Reply::CompositeFailed { serial, code } => {
                self.gpu.counters.refused_frames += 1;
                self.gpu.borrows.done(serial);
                if let Some(f) = self.gpu.ring.in_flight.take() {
                    if let Some(s) = self.gpu.ring.slots.get_mut(f.slot) {
                        s.state = gpu::SlotState::Free;
                    }
                }
                self.gpu.arm();
                debug!("gpu frame {serial} refused: {code:?}");
                if let Some(owner) = self.gpu.owner
                    && let Some(o) = self.outputs.iter_mut().find(|o| o.kms_id == owner)
                    && o.gpu_pending == Some(serial)
                {
                    o.gpu_pending = None;
                    // The damage went with the frame: repaint.
                    o.invalidate();
                }
            }
        }
    }

    /// `OutputRing`: `AddFB2` every slot once.
    fn gpu_import_ring(
        &mut self,
        size: (u32, u32),
        fourcc: u32,
        modifier: u64,
        slots: Vec<(nitro_gpu::proto::SlotLayout, OwnedFd)>,
    ) {
        let Some(owner) = self.gpu.owner else {
            return;
        };
        if self.gpu.ring.size != size {
            return;
        }
        let mut fbs = Vec::new();
        for (layout, fd) in &slots {
            let desc = nitro_kms::ImportDesc {
                format: nitro_kms::Fourcc(fourcc),
                width: size.0,
                height: size.1,
                modifier,
                planes: 1,
                offsets: [layout.offset, 0, 0, 0],
                pitches: [layout.pitch, 0, 0, 0],
            };
            match self.backend.import_buffer(&desc, &[fd.as_fd()]) {
                Ok(fb) => fbs.push(fb),
                Err(e) => {
                    warn!("gpu ring: AddFB2: {e}; mode 2 off for {owner}");
                    for fb in fbs {
                        self.backend.free_buffer(fb);
                    }
                    return;
                }
            }
        }
        info!(
            "{owner}: gpu ring of {} ({}x{}, modifier {modifier:#x})",
            fbs.len(),
            size.0,
            size.1
        );
        self.gpu.ring.slots = fbs
            .into_iter()
            .map(|fb| gpu::Slot {
                fb,
                state: gpu::SlotState::Free,
                last: None,
            })
            .collect();
        if let Some(o) = self.outputs.iter_mut().find(|o| o.kms_id == owner) {
            o.planes_dirty = true;
        }
    }

    /// The helper's timer: a restart is due, or it is not answering.
    fn on_gpu_timer(&mut self) {
        let (respawn, hung) = self.gpu.on_timer();
        if hung {
            self.gpu.kill(&EpollPoll(&self.epoll));
            self.gpu_lost();
        }
        if respawn {
            if self.gpu.mode == config::GpuHelper::On {
                self.gpu_spawn();
            } else {
                // On demand: the next decision that wants it starts it.
                for o in &mut self.outputs {
                    if !o.gpu_layers.is_empty() {
                        o.planes_dirty = true;
                    }
                }
            }
        }
        self.settle();
    }

    /// A helper frame's completion fence signalled: the buffers it
    /// borrowed may go back.
    fn on_gpu_fence(&mut self, serial: u64) {
        self.gpu.fence_signalled(&EpollPoll(&self.epoll), serial);
        self.send_gpu_releases();
        // A slot may be usable again for damage that waited for one.
        self.settle();
    }

    /// `BufferReleased`s held for helper frames that are done now.
    fn send_gpu_releases(&mut self) {
        if self.gpu.held.is_empty() {
            return;
        }
        for (owner, key) in std::mem::take(&mut self.gpu.held) {
            if self.gpu.borrows.holds(key) {
                self.gpu.held.push((owner, key));
                continue;
            }
            if self.scene.buffer_in_use(key) {
                continue;
            }
            let Some(client) = self.wire_clients.values_mut().find(|c| c.id == owner) else {
                continue;
            };
            if let Some(id) = client.buffer_id(key) {
                client.send(&ServerMsg::BufferReleased(msg::BufferReleased { id }));
            }
        }
    }

    /// The helper is gone (EOF, hung and killed): fall back, count, and
    /// schedule a restart.
    fn gpu_lost(&mut self) {
        let idle = self.gpu.idle();
        self.gpu_drop_owner(true);
        if let Some(f) = self.gpu.ring.in_flight.take() {
            self.gpu.borrows.done(f.serial);
        }
        self.gpu.died(idle);
        // Borrows whose fence dup is registered go when it signals; with
        // the helper dead a Vulkan fence signals or errors on its own.
        self.send_gpu_releases();
    }

    /// Give up the mode-2 output's resources: the ring framebuffers (the
    /// backend defers the free while one is on screen) and, when the
    /// output is in mode 2, its decision — a full repaint on the CPU or
    /// planes follows. `fallback` counts it as a helper fallback.
    fn gpu_drop_owner(&mut self, fallback: bool) {
        let Some(owner) = self.gpu.owner.take() else {
            return;
        };
        let ring = std::mem::take(&mut self.gpu.ring);
        for s in &ring.slots {
            self.backend.free_buffer(s.fb);
        }
        if let Some(f) = ring.in_flight
            && !self.gpu.running()
        {
            self.gpu.borrows.done(f.serial);
        }
        self.gpu.arm();
        let Some(index) = self.outputs.iter().position(|o| o.kms_id == owner) else {
            return;
        };
        let o = &mut self.outputs[index];
        o.gpu_pending = None;
        o.gpu_last.clear();
        o.planner.helper_lost();
        if o.decision.mode != planes::Mode::Gpu {
            return;
        }
        let old = std::mem::take(&mut o.decision);
        o.planes_dirty = false;
        o.plane_fences.clear();
        o.invalidate();
        for n in old.nodes() {
            let _ = self.scene.set_surface_on_plane(n, false);
        }
        let _ = self.backend.set_plane_state(owner, &[]);
        if fallback {
            self.gpu.counters.fallbacks += 1;
            info!("{owner}: gpu helper gone: back to planes and the CPU");
        }
    }

    /// VT switch away: stop the helper and drop what it held.
    fn gpu_pause(&mut self) {
        self.gpu_drop_owner(false);
        self.gpu.stop(&EpollPoll(&self.epoll), TOK_GPU);
    }

    /// Release the textures (and sources) of buffers that are gone.
    fn gpu_prune(&mut self) {
        if self.gpu.sources.is_empty() && self.gpu.textures() == 0 {
            return;
        }
        let scene = &self.scene;
        self.gpu
            .prune(&EpollPoll(&self.epoll), TOK_GPU, |k| scene.buffer(k).is_ok());
    }

    /// The first time output `index` wants mode 2: its shadow into a
    /// memfd the helper imports, and a ring for its primary.
    fn gpu_prepare(&mut self, index: usize) {
        if self.gpu.owner.is_some() || !self.gpu.ready() {
            return;
        }
        let o = &mut self.outputs[index];
        let id = o.kms_id;
        let Some(primary) = o
            .plane_info
            .iter()
            .find(|p| p.kind == nitro_kms::PlaneKind::Primary)
        else {
            return;
        };
        let mods: Vec<u64> = primary
            .formats
            .iter()
            .find(|(f, _)| *f == nitro_kms::Fourcc::XRGB8888)
            .map(|(_, m)| m.clone())
            .unwrap_or_default();
        let mods = self.gpu.ring_modifiers(nitro_gpu::proto::XR24, &mods);
        if mods.is_empty() {
            debug!("{id}: no ring modifier both the helper and the primary take");
            return;
        }
        let Some(shadow) = o.shadow.as_mut() else {
            return;
        };
        if let Err(e) = shadow.to_memfd() {
            warn!("{id}: shadow memfd: {e}");
            return;
        }
        let Some(fd) = shadow.memfd().and_then(|f| f.try_clone_to_owned().ok()) else {
            return;
        };
        let desc = nitro_gpu::proto::ShadowDesc {
            id: 0,
            w: shadow.width(),
            h: shadow.height(),
            stride: shadow.stride(),
            fourcc: nitro_gpu::proto::AR24,
        };
        let (w, h) = (o.width, o.height);
        let poll = EpollPoll(&self.epoll);
        let sid = self.gpu.tex_id();
        let ok = self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::ImportShadow(nitro_gpu::proto::ShadowDesc { id: sid, ..desc }),
            vec![fd],
        ) && self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::AllocOutputRing {
                n: gpu::RING_SLOTS,
                w,
                h,
                fourcc: nitro_gpu::proto::XR24,
                modifiers: mods,
            },
            Vec::new(),
        );
        if !ok {
            return;
        }
        info!("{id}: preparing gpu composite ({w}x{h})");
        self.gpu.owner = Some(id);
        self.gpu.ring = gpu::Ring {
            size: (w, h),
            requested: true,
            shadow: Some(sid),
            ..gpu::Ring::default()
        };
        self.gpu_reshadow = false;
    }

    /// Re-import the owner's shadow after it was reallocated.
    fn gpu_reimport_shadow(&mut self, index: usize) {
        let Some(old) = self.gpu.ring.shadow else {
            return;
        };
        let Some(shadow) = self.outputs[index].shadow.as_mut() else {
            return;
        };
        if shadow.to_memfd().is_err() {
            return;
        }
        let Some(fd) = shadow.memfd().and_then(|f| f.try_clone_to_owned().ok()) else {
            return;
        };
        let (w, h, stride) = (shadow.width(), shadow.height(), shadow.stride());
        let poll = EpollPoll(&self.epoll);
        let sid = self.gpu.tex_id();
        self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::Release { id: old },
            Vec::new(),
        );
        self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::ImportShadow(nitro_gpu::proto::ShadowDesc {
                id: sid,
                w,
                h,
                stride,
                fourcc: nitro_gpu::proto::AR24,
            }),
            vec![fd],
        );
        self.gpu.ring.shadow = Some(sid);
    }

    /// Leaving mode 2 on demand: give everything back so the helper can
    /// idle-exit.
    fn gpu_release_owner(&mut self) {
        let shadow = self.gpu.ring.shadow;
        self.gpu_drop_owner(false);
        let poll = EpollPoll(&self.epoll);
        if let Some(id) = shadow {
            self.gpu
                .send(&poll, TOK_GPU, &nitro_gpu::ToHelper::Release { id }, Vec::new());
        }
        self.gpu.release_all(&poll, TOK_GPU);
    }

    /// Helper texture sources for server-allocated scanout buffers
    /// (`export_buffer`), made on first sight.
    fn gpu_export_scanouts(&mut self) {
        let want: Vec<(BufferKey, nitro_kms::BufferId)> = self
            .wire_clients
            .values()
            .flat_map(|c| c.buffers.values())
            .filter(|h| !h.dmabuf && !self.gpu.sources.contains_key(&h.key))
            .filter_map(|h| Some((h.key, h.scanout?)))
            .collect();
        for (key, k) in want {
            let Some(info) = self.backend.buffer_info(k) else {
                continue;
            };
            let Some(n) = nitro_gpu::proto::plane_count(info.format.0) else {
                continue;
            };
            let Ok(fd) = self.backend.export_buffer(k) else {
                continue;
            };
            let mut fds = Vec::with_capacity(n);
            for _ in 1..n {
                let Ok(d) = fd.try_clone() else {
                    break;
                };
                fds.push(d);
            }
            fds.insert(0, fd);
            if fds.len() != n {
                continue;
            }
            self.gpu.sources.insert(
                key,
                gpu::Source {
                    desc: nitro_gpu::proto::DmabufDesc {
                        id: 0,
                        w: info.width,
                        h: info.height,
                        fourcc: info.format.0,
                        modifier: info.modifier,
                        planes: (0..n)
                            .map(|i| nitro_gpu::proto::PlaneDesc {
                                offset: info.offsets[i.min(1)],
                                pitch: info.pitches[i.min(1)],
                            })
                            .collect(),
                        ..nitro_gpu::proto::DmabufDesc::default()
                    },
                    fds,
                },
            );
        }
    }

    /// Visible Surfaces on output `index` the helper could composite,
    /// bottom to top: a dma-buf (a client's, or an exported scanout
    /// buffer) the helper samples, opaque, axis-aligned, fully opaque.
    fn gpu_layers(&self, index: usize, out: &mut Vec<gpu::Layer>) {
        use nitro_scene::PaintKind;
        if self.gpu.sources.is_empty() {
            return;
        }
        let o = &self.outputs[index];
        let Some((orect, _)) = self.scene.output_info(o.scene_id) else {
            return;
        };
        let mut items = Vec::new();
        self.scene.paint_list(o.scene_id, &orect, &mut items);
        for item in &items {
            let (PaintKind::Surface { size, .. } | PaintKind::Hole { size }) = item.kind else {
                continue;
            };
            let t = item.transform;
            if !t.is_axis_aligned() || t.a <= 0.0 || t.d <= 0.0 || item.opacity < 1.0 {
                continue;
            }
            let Some(content) = self
                .scene
                .node(item.node)
                .ok()
                .and_then(nitro_scene::Node::surface)
                .and_then(|s| s.content)
            else {
                continue;
            };
            let Some(src) = self.gpu.sources.get(&content.buffer) else {
                continue;
            };
            if self.gpu.refused(content.buffer)
                || (self.gpu.info.is_some()
                    && !self.gpu.samples(src.desc.fourcc, src.desc.modifier))
                || !self
                    .scene
                    .buffer(content.buffer)
                    .is_ok_and(|b| b.desc().is_opaque())
            {
                continue;
            }
            let dst = t
                .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1))
                .round_out();
            let visible = dst.intersect(&item.clip).intersect(&orect);
            if visible.is_empty() || dst.is_empty() || content.src.is_empty() {
                continue;
            }
            let fx = content.src.w as f32 / dst.w as f32;
            let fy = content.src.h as f32 / dst.h as f32;
            let s = [
                content.src.x as f32 + (visible.x - dst.x) as f32 * fx,
                content.src.y as f32 + (visible.y - dst.y) as f32 * fy,
                visible.w as f32 * fx,
                visible.h as f32 * fy,
            ];
            out.push(gpu::Layer {
                node: item.node,
                key: content.buffer,
                dst: visible.translate(-orect.x, -orect.y),
                src: s,
                encoding: gpu_encoding(content.color.matrix),
                range: gpu_range(content.color.range),
            });
        }
        // One layer is the shadow.
        let max = nitro_gpu::proto::MAX_LAYERS - 1;
        if out.len() > max {
            out.drain(..out.len() - max);
        }
    }

    /// A frame in mode 2: rasterize the damage into the shadow, and hand
    /// the helper `{slot, damage, layers}`. Never waits: the commit
    /// follows `Composited` (`gpu_commit`).
    fn paint_gpu(&mut self, index: usize) -> bool {
        if self.outputs[index].gpu_pending.is_some() {
            return false;
        }
        let poll = EpollPoll(&self.epoll);
        let gpu = &self.gpu;
        let Some(slot) = gpu::pick_free(&gpu.ring.slots, |s| gpu.fence_pending(s)) else {
            // Keep the damage; a released slot or a signalled fence
            // brings the paint back.
            self.gpu.counters.busy_slots += 1;
            return false;
        };
        let _ = &poll;
        let scene_id = self.outputs[index].scene_id;
        let cursor_state = self.cursor_state(scene_id);
        let scroll = self.outputs[index].take_scroll();
        let fast_scaled = self
            .wm
            .overview()
            .is_some_and(|o| o.output == scene_id && !o.atlas);
        let output = &mut self.outputs[index];
        let bounds = output.bounds();
        let mut rasterize = output.rasterize_region();
        let Some(shadow) = output.shadow.as_mut() else {
            return false;
        };
        let stride = shadow.stride().max(output.width * 4);
        if shadow.ensure(output.width, output.height, stride) {
            rasterize = vec![bounds];
            self.gpu_reshadow = true;
        }
        let painted = if rasterize.is_empty() {
            None
        } else {
            let p = paint_shadow(
                shadow,
                &mut ShadowPaint {
                    scene: &self.scene,
                    text: &mut self.text,
                    icons: &mut self.icons,
                    items: &mut self.paint_items,
                    palette: &self.palette,
                    output: scene_id,
                    bounds,
                    cursor: (&self.cursor, cursor_state),
                    fast_scaled,
                },
                &rasterize,
                scroll.filter(|_| true),
            );
            shadow.note_painted(&rasterize);
            Some(p)
        };
        self.text.next_frame();
        if std::mem::take(&mut self.gpu_reshadow) {
            self.gpu_reimport_shadow(index);
        }
        let Some(shadow_id) = self.gpu.ring.shadow else {
            return false;
        };
        // Damage: the raster, and every helper layer that changed or
        // moved (both where it is and where it was).
        let o = &self.outputs[index];
        let now: Vec<(nitro_scene::NodeKey, BufferKey, nitro_core::IRect)> = o
            .gpu_layers
            .iter()
            .filter(|l| o.decision.gpu.contains(&l.node))
            .map(|l| (l.node, l.key, l.dst))
            .collect();
        let mut damage = Damage::new();
        for r in &rasterize {
            damage.add(*r);
        }
        for l in &now {
            if !o.gpu_last.contains(l) {
                damage.add(l.2);
            }
        }
        for l in &o.gpu_last {
            if !now.contains(l) {
                damage.add(l.2);
            }
        }
        let damage = gpu_rects(damage.rects(), bounds);
        let upload = gpu_rects(&rasterize, bounds);
        let layers_in: Vec<gpu::Layer> = o
            .gpu_layers
            .iter()
            .filter(|l| o.decision.gpu.contains(&l.node))
            .copied()
            .collect();
        let poll = EpollPoll(&self.epoll);
        let mut layers = Vec::with_capacity(layers_in.len() + 1);
        let mut keys = Vec::with_capacity(layers_in.len());
        for l in &layers_in {
            let Some(tex) = self
                .gpu
                .texture(&poll, TOK_GPU, l.key, l.encoding, l.range)
            else {
                continue;
            };
            layers.push(nitro_gpu::proto::Layer {
                tex,
                src: l.src,
                dst: l.dst,
                blend: nitro_gpu::proto::Blend::Opaque,
            });
            keys.push(l.key);
        }
        layers.push(nitro_gpu::proto::Layer {
            tex: shadow_id,
            src: [0.0, 0.0, bounds.w as f32, bounds.h as f32],
            dst: bounds,
            blend: nitro_gpu::proto::Blend::PremulOver,
        });
        if !upload.is_empty() {
            self.gpu.send(
                &poll,
                TOK_GPU,
                &nitro_gpu::ToHelper::UploadDamage {
                    id: shadow_id,
                    rects: upload,
                },
                Vec::new(),
            );
        }
        let serial = self.gpu.serial();
        let sent = self.gpu.send(
            &poll,
            TOK_GPU,
            &nitro_gpu::ToHelper::Composite(nitro_gpu::proto::Composite {
                serial,
                out_idx: slot as u32,
                damage: damage.clone(),
                layers,
                fence_mask: 0,
            }),
            Vec::new(),
        );
        if !sent {
            return false;
        }
        self.gpu.borrows.add(serial, keys);
        let s = &mut self.gpu.ring.slots[slot];
        s.state = gpu::SlotState::Submitted(serial);
        s.last = Some(serial);
        self.gpu.ring.in_flight = Some(gpu::InFlight {
            serial,
            slot,
            sent: Instant::now(),
            keys: Vec::new(),
        });
        self.gpu.arm();
        let o = &mut self.outputs[index];
        o.gpu_last = now;
        o.gpu_submitted(serial);
        if let Some(p) = painted {
            self.stats.paint_us.push(p.paint_us);
            self.stats.raster_px.push(p.raster_px);
            self.stats.blit_px.push(p.moved_px);
            self.blit_frames += u64::from(p.blitted);
            self.stats.paint_log.push(p.paint_us);
        }
        let damage_px = frame::region_area(&damage);
        self.stats.damage_px.push(damage_px);
        self.stats.damage_log.push(damage_px);
        true
    }

    /// Commit the helper's finished frame on output `index`: its slot on
    /// the primary with the completion `sync_file` as `IN_FENCE_FD`. The
    /// display waits on the fence, the server does not. Deferred to
    /// `on_flip` while a flip is pending.
    fn gpu_commit(&mut self, index: usize) {
        let id = self.outputs[index].kms_id;
        if self.gpu.owner != Some(id) || self.backend.flip_pending(id) {
            return;
        }
        let Some(c) = self.gpu.ring.composited.take() else {
            return;
        };
        let Some(fb) = self.gpu.ring.slots.get(c.slot).map(|s| s.fb) else {
            return;
        };
        let o = &mut self.outputs[index];
        if o.decision.mode != planes::Mode::Gpu || o.decision.layout.is_empty() {
            // Left mode 2 meanwhile: the frame is not shown.
            if let Some(s) = self.gpu.ring.slots.get_mut(c.slot) {
                s.state = gpu::SlotState::Free;
            }
            o.gpu_pending = None;
            return;
        }
        o.decision.layout[0].source = nitro_kms::PlaneSource::Buffer(fb);
        let primary = o.decision.layout[0].plane;
        let layout = o.decision.layout.clone();
        let r = self.backend.set_plane_state(id, &layout).and_then(|()| {
            self.stage_plane_fences(index);
            self.backend.set_plane_fence(id, primary, c.fence)?;
            self.backend.commit_planes(id)
        });
        match r {
            Ok(()) => {
                self.plane_fences += 1;
                if let Some(s) = self.gpu.ring.slots.get_mut(c.slot) {
                    s.state = gpu::SlotState::Shown;
                }
                self.gpu.ring.shown = Some(c.slot);
                self.outputs[index].gpu_committed();
                self.note_on_kms(index);
            }
            Err(e) => {
                warn!("{id}: gpu commit: {e}");
                if let Some(s) = self.gpu.ring.slots.get_mut(c.slot) {
                    s.state = gpu::SlotState::Free;
                }
                self.outputs[index].gpu_pending = None;
                self.planes_fallback(index);
            }
        }
    }

    /// Output `index`'s helper inputs for the planner, and whether it
    /// wants the helper at all. Starts it (on demand) or prepares the
    /// output's resources (first entry) as needed.
    fn gpu_inputs(&mut self, index: usize) -> (Vec<nitro_scene::NodeKey>, Option<nitro_kms::BufferId>) {
        let id = self.outputs[index].kms_id;
        if !self.gpu.enabled()
            || self.outputs[index].shadow.is_none()
            || self.gpu.owner.is_some_and(|o| o != id)
        {
            self.outputs[index].gpu_layers.clear();
            return (Vec::new(), None);
        }
        if self.gpu.owner == Some(id)
            && self.gpu.ring.size != (self.outputs[index].width, self.outputs[index].height)
        {
            // A mode change: a new ring for the new size.
            self.gpu_drop_owner(false);
        }
        self.gpu_export_scanouts();
        let mut layers = Vec::new();
        self.gpu_layers(index, &mut layers);
        let nodes = layers.iter().map(|l| l.node).collect();
        self.outputs[index].gpu_layers = layers;
        let in_fence = self.outputs[index]
            .plane_info
            .iter()
            .any(|p| p.kind == nitro_kms::PlaneKind::Primary && p.in_fence);
        let ring = &self.gpu.ring;
        let helper = (in_fence && self.gpu.ready() && self.gpu.owner == Some(id) && ring.ready())
            .then(|| ring.slots[ring.shown.unwrap_or(0)].fb);
        (nodes, helper)
    }

    /// After a decision without the helper: does output `index` want it?
    /// (A helper-able Surface the planes did not take.)
    fn gpu_want(&mut self, index: usize, d: &planes::Decision) {
        let o = &self.outputs[index];
        let want = o.gpu_layers.iter().any(|l| !d.places(l.node));
        let id = o.kms_id;
        if !want {
            if self.gpu.owner == Some(id)
                && d.mode != planes::Mode::Gpu
                && self.gpu.mode == config::GpuHelper::OnDemand
            {
                self.gpu_release_owner();
            }
            return;
        }
        if !self.gpu.running() && self.gpu.state == gpu::State::Off {
            self.gpu_spawn();
        } else if self.gpu.ready() && self.gpu.owner.is_none() {
            self.gpu_prepare(index);
        }
    }

    /// The `gpu_*` lines of `stats`.
    fn gpu_stats(&self, pairs: &mut Vec<(&'static str, u64)>) {
        let g = &self.gpu;
        let c = g.counters;
        pairs.push(("gpu_state", g.state.number()));
        pairs.push(("gpu_spawns", c.spawns));
        pairs.push(("gpu_crashes", c.crashes));
        pairs.push(("gpu_fallbacks", c.fallbacks));
        pairs.push(("gpu_frames", c.frames));
        pairs.push(("gpu_busy_slots", c.busy_slots));
        pairs.push(("gpu_refused_frames", c.refused_frames));
        pairs.push(("gpu_composite_us_avg", g.composite_us.mean()));
        pairs.push(("gpu_composite_us_max", g.composite_us.max()));
        pairs.push(("gpu_busy_us", c.busy_us));
        pairs.push(("gpu_textures", g.textures() as u64));
        pairs.push(("gpu_import_refused", c.import_refused));
        pairs.push(("gpu_releases_held", g.held.len() as u64));
        pairs.push(("gpu_fences_pending", g.fences_pending() as u64));
        pairs.push(("gpu_ring_slots", g.ring.slots.len() as u64));
        pairs.push(("gpu_helper_rss", g.last_stats.rss));
        pairs.push(("gpu_helper_pss", g.last_stats.pss));
        pairs.push(("gpu_helper_drm_total", g.last_stats.drm_total));
    }
}
