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
pub mod defer;
pub mod desktop_index;
pub mod frame;
pub mod icon_theme;
pub mod icons;
pub mod input;
pub mod keyboard;
pub mod logging;
pub mod protocol;
pub mod remote;
pub mod render;
pub mod shell;
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
    Backend, DrmBackend, DrmOptions, Error as KmsError, Event, FakeBackend, ModeRequest, Modeline,
    OutputId as KmsOutputId, OutputInfo, Rect as KmsRect,
};
use nitro_raster::Canvas;
use nitro_scene::{ClientId, DamageSink, OutputId as SceneOutputId, Scene, WindowKey, WindowState};
use nitro_seat::{Device, Seat, SeatEvent};
use nitro_wire::msg::{self, ClientMsg, ServerMsg};
use nitro_wire::server::Listener as WireListener;
use nitro_wire::types::{ButtonState, ErrorCode, NodeId};
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::clients::{ApplyError, Pending, WireClient};
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

/// What a **remote** client is told when it sends a buffer op.
///
/// One constant because it is sent from two places — the decoder's
/// refusal of an fd-carrying `CreateBuffer`, and the check on the two
/// ops that only *name* a buffer — and a client that meets both should
/// not get two different explanations of one rule.
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
    /// Paint into a heap shadow buffer per output and stream the damage
    /// into the scanout buffer, rather than rasterizing straight into it.
    ///
    /// On by default — it is ~4× on real (write-combined) framebuffers,
    /// see `crates/nitro-server/src/frame.rs`. `NITRO_SHADOW=0` turns it
    /// off so the two can be measured against each other on hardware; a
    /// test sets it directly, for the same reason `scales` is a field.
    pub shadow: bool,
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
            shadow: true,
            config_path: None,
            icon_dirs: None,
            desktop_dirs: Some(Vec::new()),
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

/// The running server. Field order is drop order: clients first, then the
/// sockets, then input (whose device fds belong to the seat), then the
/// backend (which holds a dup of the DRM fd), then the seat's `Device`, and
/// the seat last.
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
    /// Which cursor shape the pointer is showing.
    ///
    /// Cached for the same reason [`Server::resize_hint`] is, and with the
    /// same damage rule: a shape change damages the **old rect ∪ the new**
    /// (they differ, because the hotspots do) and nothing else — no scene
    /// node moved, so no scene damage and no restyle. Chosen from the same
    /// single `frame_hit` per motion that already drives the hint and the
    /// hover; see [`Server::set_cursor_shape`].
    cursor_shape: crate::cursor::Shape,
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
    /// Windows created while no output existed, waiting for one.
    unplaced: Vec<(ClientId, WindowKey)>,
    /// Newest input timestamp not yet consumed by a frame; see
    /// [`Server::note_input`].
    pending_input_ns: u64,
    /// The clients whose answer a cursor-only flip is waiting for, and the
    /// timer that bounds the wait. See [`defer`].
    defer: DeferredFlip,

    /// Exclusive zones and anchors set by shell clients; see [`shell`].
    zones: shell::Zones,
    /// Server-global hotkey bindings.
    hotkeys: shell::HotKeys,
    /// Server-global window ids, minted for the shell's window list.
    window_refs: shell::WindowRefs,
    /// Shell clients subscribed to the window list, by token.
    window_watchers: Vec<u64>,
    /// Shell clients subscribed to output hotplug, by token.
    output_watchers: Vec<u64>,
    /// The window holding an explicit keyboard grab: every key goes there
    /// instead of to the focused window. See
    /// [`GrabKeyboard`](nitro_wire::msg::GrabKeyboard).
    grab: Option<WindowKey>,
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

    let mut server = Server {
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
        cursor: Cursor::new(),
        pointer: Pointer::default(),
        wm: WindowManager::new(),
        decorations: HashMap::new(),
        resize_hint: None,
        button_hover: None,
        cursor_shape: crate::cursor::Shape::Arrow,
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
        input_hotplug,
        input_dir: config.input_dir.clone(),
        focus: None,
        pending_focus: None,
        touch_targets: HashMap::new(),
        unplaced: Vec::new(),
        pending_input_ns: 0,
        defer: defer::DeferredFlip::new().map_err(errno("create the deferred-flip timer"))?,
        zones: shell::Zones::new(),
        hotkeys: shell::HotKeys::new(),
        window_refs: shell::WindowRefs::new(),
        window_watchers: Vec::new(),
        output_watchers: Vec::new(),
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
    // The third listener, and the only one that is optional. Applied here
    // through the same function the reload path uses, so "what
    // `remote.listen` means" has exactly one implementation.
    server.apply_remote_listen();
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
    )
}

/// Whether a message is one of the **M5-A** ops this server does not yet
/// implement.
///
/// The protocol surface landed ahead of the behaviour (task #3767), so the
/// twelve ops below decode but are refused: `Server::caps` advertises none
/// of the M5 bits, so no conformant client sends one, and a client that
/// does anyway hears why instead of being silently ignored.
///
/// `ClientCaps` (0x0003) is deliberately **not** here: it is accepted and
/// recorded, because the rule that makes it safe — the server must not
/// send what the client did not list — cannot be honoured by a server that
/// throws the list away, and each M5 task should gain one `if` rather than
/// re-litigate this.
///
/// A `match` over the variants rather than an op-code range test, for the
/// reason `is_shell_op` gives: a range would keep compiling after someone
/// implemented one of these, which is exactly when it must not.
fn is_m5_op(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::CreatePopup(_)
            | ClientMsg::RepositionPopup(_)
            | ClientMsg::SetCursor(_)
            | ClientMsg::StartMove(_)
            | ClientMsg::StartResize(_)
            | ClientMsg::ListOutputs(_)
            | ClientMsg::SetSelection(_)
            | ClientMsg::RequestSelection(_)
            | ClientMsg::SendSelection(_)
            | ClientMsg::StartDrag(_)
            | ClientMsg::AcceptDrop(_)
            | ClientMsg::FinishDrag(_)
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
            Ok(true) => info!("output modes re-applied"),
            Ok(false) => {}
            Err(e) => warn!("applying the configured modes: {e}"),
        }
        for w in self.backend.take_warnings() {
            warn!("{w}");
        }
    }

    fn sync_outputs(&mut self) {
        let infos: Vec<OutputInfo> = self.backend.outputs().to_vec();
        let mut lost = false;
        let mut gone: Vec<u32> = Vec::new();
        self.outputs.retain(|o| {
            let keep = infos.iter().any(|i| i.id == o.kms_id);
            if !keep {
                info!("{} gone", o.kms_id);
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
            self.outputs.push(OutputState::new(
                info.id,
                scene_id,
                info.width,
                info.height,
                info.refresh_mhz,
                self.shadow,
            ));
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
        let Some(primary) = self.primary_output() else {
            // Every output is gone; the windows wait, exactly as they do
            // between startup and the first connector.
            return;
        };
        let area = self.local_work_area(primary);
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
                if shadow.ensure(buf.width, buf.height, buf.stride) {
                    rasterize = vec![bounds];
                }
                let paint_us = frame::paint_region(
                    &mut shadow.canvas(),
                    &self.scene,
                    &mut self.text,
                    &mut self.icons,
                    scene_id,
                    &rasterize,
                    (&self.cursor, cursor_state),
                    &mut self.paint_items,
                    &self.palette,
                );
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
        match self.backend.commit(id, &kms_damage) {
            Ok(()) => {
                self.stats.paint_us.push(paint_us);
                self.stats.copy_us.push(copy_us);
                self.stats.damage_px.push(damage_px);
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
                false
            }
        }
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
            shape: self.cursor_shape,
            // Per **output**: the cursor is painted in device pixels, so
            // a 2x output needs a 2x cursor to be the same physical size,
            // and the pointer can be on either screen of a mixed-scale
            // desk. Each output paints it at its own factor.
            scale: Cursor::paint_scale(scale),
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
                    // Shell tokens sort above wire tokens, so this arm has
                    // to come first; both end up in `on_wire_client`,
                    // because a shell client *is* a wire client with an
                    // extra capability bit. Remote tokens sort above both,
                    // for the same reason and with the same answer.
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
                    // seen, so the modifier state is a guess: drop it. The
                    // shell's armed tap goes with it for the same reason —
                    // the release that would complete it never arrived.
                    if let Some(kb) = self.keyboard.as_mut() {
                        kb.reset();
                    }
                    self.hotkeys.reset();
                    self.hotkey_pending = None;
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
            match self.backend.rescan() {
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
            self.wire_clients.insert(token, WireClient::new(stream, id));
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
        }
        if removed > 0 {
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
        let keyboard_changed = settings.keyboard != self.settings.keyboard;
        let icons_changed = settings.theme.icon_theme() != self.settings.theme.icon_theme();
        let palette = settings.palette();
        self.settings = settings;
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
            match Keyboard::with_settings(&self.settings.keyboard) {
                Some(kb) => {
                    info!("xkb keymap: {}", kb.layout_names().join(", "));
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
            }
            self.hotkeys.reset();
            self.hotkey_pending = None;
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
        let old_shape = self.cursor_shape;
        let appeared = self.pointer.seen();
        if appeared {
            self.damage_cursor_at(old_x, old_y, old_shape);
        }
        if !self.pointer.move_to(x, y, bounds) && !appeared {
            return;
        }
        let (new_x, new_y) = self.pointer.device();
        if (old_x, old_y) != (new_x, new_y) {
            // Old ∪ new, exactly like the scene's own damage rule.
            self.damage_cursor_at(old_x, old_y, old_shape);
            self.damage_cursor_at(new_x, new_y, old_shape);
        }
        let point = self.pointer.position();
        let output = input::output_at(&self.scene, point);
        self.pointer.output = output;
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
            self.set_cursor_shape(Self::drag_shape(drag));
            self.note_input(time_ns);
            return;
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
        let frame_hit = self.pointer_desktop().and_then(|p| self.frame_hit(p));
        self.set_resize_hint(
            frame_hit.and_then(|(win, region)| matches!(region, Region::Resize(_)).then_some(win)),
        );
        self.set_button_hover(frame_hit.filter(|(_, region)| region.is_button()));
        // And the cursor shape, off that same one walk.
        self.set_cursor_shape(Self::shape_for(frame_hit, |w| self.resizable(w)));
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

        let Some(window) = self.pointer.over else {
            // A click on nothing changes nothing: the desktop is not a
            // focus target, so the keyboard stays where it was. Dropping
            // focus here would leave a screen full of windows and nowhere
            // for keys to go until the next Alt+Tab — `docs/wm.md` is
            // explicit that focus is only ever handed on, never dropped.
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
        // A shell's own bindings come next: after the compositor's, which are
        // not negotiable, and before any client's, because a global hotkey
        // the focused application could also see would be both a keylogger
        // and an ambiguity. `HotKeys::key` is fed every key, hotkey or not,
        // because the bare-modifier tap is decided by what did *not* happen
        // while a modifier was held.
        let fired = self.hotkeys.key(resolved.keysym, pressed, resolved.named);
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
        // A keyboard grab wins over focus: it is how a `NO_FOCUS` overlay
        // reads the keyboard without taking focus away, so the window that
        // was focused stays focused and keeps its active frame.
        let Some(window) = self.grab_target().or(self.focus) else {
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
            self.reflow_work_area();
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
            self.pointer.over = None;
        }
        self.touch_targets.retain(|_, (w, _)| *w != win);
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
        let old = self.focus;
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
        // The shell's window list carries `focused`, so both ends of the
        // change are announced — a bar highlighting the active window needs
        // to un-highlight the old one.
        if let Some(old) = old {
            self.notify_window(old);
        }
        if let Some(new) = window {
            self.notify_window(new);
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
    /// `hovering_a_band_changes_the_cursor_and_nothing_else` pins.
    fn set_cursor_shape(&mut self, shape: crate::cursor::Shape) {
        if self.cursor_shape == shape {
            return;
        }
        let old = self.cursor_shape;
        self.cursor_shape = shape;
        if !self.pointer.present {
            // Nothing is drawn, so nothing changed on screen. The shape is
            // still recorded: the first motion that makes the pointer
            // visible paints whatever it should already have been.
            return;
        }
        let (x, y) = self.pointer.device();
        self.damage_cursor_at(x, y, old);
        self.damage_cursor_at(x, y, shape);
    }

    /// The shape a frame region calls for, given a way to ask whether a
    /// window is resizable.
    ///
    /// A band the window can **actually** be resized by takes the shape
    /// that points along it; everything else takes the arrow. `resizable`
    /// is the same filter [`Server::set_resize_hint`] applies, and for the
    /// same reason — a diagonal cursor over a `FIXED_SIZE` window would
    /// promise a grab that does nothing, which is worse than promising
    /// none.
    ///
    /// A free function of the hit rather than a method, because the two
    /// callers hold `self` differently: the motion path has already
    /// borrowed it for the hit test, and the release path has not looked
    /// yet. Having one rule matters more than the shape of the call —
    /// the release exists precisely so a drag cannot leave a stale cursor
    /// behind, and a second copy of the rule would be a second thing to
    /// get wrong.
    fn shape_for(
        hit: Option<(WindowKey, Region)>,
        resizable: impl Fn(WindowKey) -> bool,
    ) -> crate::cursor::Shape {
        match hit {
            Some((win, Region::Resize(edges))) if resizable(win) => {
                crate::cursor::Shape::for_edges(edges)
            }
            _ => crate::cursor::Shape::Arrow,
        }
    }

    /// Re-derive the cursor shape from whatever is under the pointer now.
    ///
    /// The motion path does this inline, off the `frame_hit` it already
    /// has. This is for the one place that has no hit in hand and cannot
    /// skip the question: the **release** that ends a drag. A drag owns
    /// the shape while it lasts and the pointer is routinely nowhere near
    /// the frame by the time it ends, so without this a release over the
    /// bare desktop leaves the move cross there until the next motion —
    /// indefinitely, if the user lets go and does not move.
    fn update_cursor_shape(&mut self) {
        let hit = self.pointer_desktop().and_then(|p| self.frame_hit(p));
        self.set_cursor_shape(Self::shape_for(hit, |w| self.resizable(w)));
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
        // A window with an exclusive zone that became (or stopped being)
        // hidden changed the work area, and every maximized window has to be
        // re-sized for it. Guarded on the zone map being non-empty, so a
        // desktop with no shell running pays one `is_empty` per state change.
        if !self.zones.is_empty() {
            self.reflow_work_area();
        }
        if state == WindowState::Minimized {
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
                if !self.on_screen(win) {
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
            Ok(Request::Stats) => self.stats_reply(),
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
        // Completed reloads, however triggered: the control request,
        // SIGHUP and the inotify watch all land in one counter, because
        // what a caller wants to know is "did the server pick my edit up",
        // not which of the three doors it came through.
        pairs.push(("config_reloads", self.config_reloads));
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
    /// Accepted and **stored** even though nothing reads it yet. The rule
    /// that makes `ClientCaps` safe is "the server must not send a message
    /// belonging to a bit the client did not list", and a server that
    /// cannot store the list cannot honour that rule the moment it grows a
    /// bit — so M5-B through M5-I each gain one `if` instead of
    /// re-litigating this. See `docs/wire.md` § Capability opt-in.
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
        client.client_caps = caps;
        true
    }

    /// Buffer, or act on, one decoded client message. Returns whether the
    /// client survives it.
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
            ClientMsg::CreateBuffer(buffer) => {
                // The descriptor is checked and *mapped* now, not at commit:
                // the client may legitimately close or reuse its own
                // descriptor as soon as it has sent this message, and a
                // buffer whose fd is not sealed must be refused before any
                // of the batch is applied. From here on the scene holds the
                // mapping, so there is no server-side copy to keep in step
                // and `BufferDamage` only marks nodes for repaint.
                let id = buffer.id;
                let (desc, pixels) = match clients::map_buffer(buffer) {
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
            other => {
                // The M5-A ops decode but are not implemented: this server
                // advertises no bit above 7, so no conformant client sends
                // one. Refused **at receipt** rather than at the commit,
                // which matters for `SendSelection`: buffering it would
                // park a descriptor in `pending` until a commit that may
                // never come.
                if is_m5_op(&other) {
                    let name = other.name();
                    self.disconnect(
                        token,
                        Some((
                            0,
                            ErrorCode::Protocol,
                            format!("{name} needs a capability this server does not advertise"),
                        )),
                    );
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
    /// cannot keep. `DIRECT_SCANOUT` and `DMABUF` remain later milestones,
    /// and a zero bit is the protocol's way of saying "do not use this".
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
    fn caps(&self, shell: bool, remote: bool) -> u32 {
        let mut caps = nitro_wire::types::caps::WM | nitro_wire::types::caps::THEME;
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
                self.reflow_work_area();
            }
            shell::WindowOp::Anchor { edges, margin } => {
                self.zones.set_anchor(win, edges, margin);
                self.apply_anchor(win);
            }
            shell::WindowOp::Grab(on) => {
                if on {
                    self.grab = Some(win);
                } else if self.grab == Some(win) {
                    self.grab = None;
                }
            }
        }
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
    fn apply_anchor(&mut self, win: WindowKey) {
        let Some(a) = self.zones.anchor(win) else {
            return;
        };
        let Ok(info) = self.scene.window_info(win) else {
            return;
        };
        let size = info.frame_size();
        let output = info.output().or_else(|| self.primary_output());
        let Some(output) = output else {
            // No output yet; `sync_outputs` re-applies anchors when one
            // appears, so the window simply waits where it is.
            return;
        };
        let Some((rect, scale)) = self.scene.output_info(output) else {
            return;
        };
        let s = if scale > 0.0 { scale } else { 1.0 };
        let origin = self.desktop_origin(output);
        let full = Rect::new(origin.x, origin.y, rect.w as f32 / s, rect.h as f32 / s);
        let target = shell::anchor_rect(full, size, a);
        self.set_frame_rect(win, target);
    }

    /// Re-apply every anchor. Called when an output's geometry changes, so a
    /// bar keeps spanning across a mode change or a hotplug.
    fn reflow_anchors(&mut self) {
        let anchored: Vec<WindowKey> = self.zones.anchored().map(|(w, _)| w).collect();
        for win in anchored {
            self.apply_anchor(win);
        }
    }

    /// Re-apply the geometry of every window whose rectangle is *derived*
    /// from the work area, after an exclusive zone changed it.
    ///
    /// Only `Maximized` windows: a floating window is where the user put it,
    /// and a fullscreen one covers the output work area or not. This is also
    /// why a zone is cheap — the reflow is proportional to the number of
    /// maximized windows, not to the number of windows.
    fn reflow_work_area(&mut self) {
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

    /// Answer an `Outputs`: a snapshot, then `OutputsEnd`.
    fn send_output_list(&mut self, token: u64) {
        let infos = self.output_infos();
        let Some(client) = self.wire_clients.get_mut(&token) else {
            return;
        };
        for info in infos {
            client.send(&ServerMsg::OutputInfo(info));
        }
        client.send(&ServerMsg::OutputsEnd(msg::OutputsEnd));
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
        let infos = self.output_infos();
        for token in self.output_watchers.clone() {
            let Some(client) = self.wire_clients.get_mut(&token) else {
                continue;
            };
            for id in gone {
                client.send(&ServerMsg::OutputGone(msg::OutputGone { id: *id }));
            }
            for info in &infos {
                client.send(&ServerMsg::OutputInfo(info.clone()));
            }
            client.send(&ServerMsg::OutputsEnd(msg::OutputsEnd));
        }
    }

    /// Apply a client's transaction. Returns whether the client survives.
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
        client.frame_requests.extend(outcome.frame_requests);
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
        // The shell ops go *before* the state requests and after everything
        // else, for the same reason `SetWindowState` is last: an anchor
        // decides a window's whole rectangle, so it has to win over the
        // client's own `SetBounds` in the same batch — and a `Maximized`
        // asked for in that batch has to win over the anchor, which is the
        // shell deliberately handing its window to the window manager.
        for (win, op) in shell_ops {
            self.apply_shell_op(win, op);
        }
        for (win, state) in outcome.state_requests {
            self.set_state(win, state);
        }
        // A window that showed or hid may have been a bar holding a strip of
        // the desktop, and a zone is released the moment its bar stops
        // showing (`Server::showing`). Guarded on the zone map so a desktop
        // with no shell running pays one `is_empty` per transaction that
        // touched visibility at all.
        if visibility_changed && !self.zones.is_empty() {
            self.reflow_work_area();
        }
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
                self.pointer.over = None;
            }
            self.touch_targets.retain(|_, (w, _)| *w != win);
            if let Err(e) = self.scene.destroy_window(id, win) {
                warn!("destroying window of client {}: {e}", id.0);
            }
            self.forget_window(win);
        }
        for key in client.buffers.values().copied().collect::<Vec<_>>() {
            // Dropping the scene's buffer drops its mapping, which is the
            // `munmap`. Since #569 there is no descriptor to release
            // alongside it: `Mapping::map` closed the client's fd the
            // moment the pages were mapped.
            let _ = self.scene.destroy_buffer(id, key);
        }
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
        // Whatever this client held as a *shell*: its hotkey bindings (or the
        // launcher's Super would stay swallowed after the launcher died), its
        // subscriptions, and any grab it still had. Its windows' zones and
        // anchors went with `forget_window` above.
        self.hotkeys.forget_client(token);
        self.window_watchers.retain(|t| *t != token);
        self.output_watchers.retain(|t| *t != token);
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
        let Some(window) = self.scene.windows_front_to_back(scene_output).next() else {
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
            return protocol::shot_reply(&shadow.image());
        }
        match self.backend.read_front(id) {
            Ok(img) => protocol::shot_reply(&img),
            Err(e) => protocol::err_reply(&e.to_string()),
        }
    }

    /// Answer a `shot-front`: the same pixels, read off the scanout buffer
    /// whatever the shadow says.
    ///
    /// Test-only. It is the only way to check that the copy out of the
    /// shadow put the right bytes in the buffer the display scans, which
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

/// Tell a client about every icon name in its commit the set did not have.
///
/// The **only** non-fatal error a local client can earn, and deliberately
/// so: the node was cleared, the rest of the batch applied, and saying so
/// costs a gap in the UI rather than an application. Every other error in
/// this protocol closes the connection, which is exactly why this one
/// needs to be written down somewhere a reader will find it — see
/// `docs/icons.md`.
///
/// Sent *after* the transaction was applied, like `TextMetrics`, so a
/// client sees the whole commit take effect before the complaint about
/// one node of it.
fn report_bad_icons(client: &mut clients::WireClient, serial: u32, bad: Vec<(NodeId, String)>) {
    for (node, name) in bad {
        warn!(
            "client {}: node {} asked for unknown icon {name:?}",
            client.id.0,
            node.raw()
        );
        client.send(&ServerMsg::Error(msg::Error {
            serial,
            code: ErrorCode::BadIcon,
            // **The quoting here is parsed by the toolkit.** `nitro-ui`'s
            // `quoted()` (`crates/nitro-ui/src/ui.rs`) pulls the icon name
            // back out of this string to route the failure to the widget
            // that asked, because `Error` carries no node id. So this
            // format string is load-bearing prose: do not reword or
            // re-quote it. The replacement exists on the wire as
            // `ServerMsg::IconRefused { serial, node, name }` (0x8303,
            // M5-A/#3767) and task **#3786** switches both ends over to
            // it — after which this comment and the parser both go.
            msg: format!("no icon named {name:?}"),
        }));
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
        ] {
            assert!(!is_shell_op(&m), "{} is not a shell op", m.name());
        }
    }
}
