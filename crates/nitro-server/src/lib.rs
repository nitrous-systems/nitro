//! `nitro-server` — the display server. M0 shape: one thread, one epoll,
//! a seat, a KMS backend, a placeholder scene and a control socket.
//!
//! [`run`] takes a [`Config`] and returns when asked to quit (signal or
//! `quit` request), so tests drive the whole loop in-process against
//! [`nitro_kms::FakeBackend`]. `main.rs` only turns environment variables
//! into a `Config`.
//!
//! Event loop (level-triggered epoll, no timers — an idle server never
//! wakes):
//!
//! | fd                 | on readable                                         |
//! |--------------------|-----------------------------------------------------|
//! | seat               | `Seat::dispatch`: `Disable` → pause + ack, `Enable` → resume + full repaint |
//! | backend `poll_fds` | `Backend::dispatch`: `Flipped` → paint next frame, `Hotplug` → rescan |
//! | signal self-pipe   | SIGTERM/SIGINT → orderly shutdown                    |
//! | control listener   | accept, register client                             |
//! | control client     | read lines, answer (`protocol`), drop on hangup      |
//!
//! Frames are painted only in response to `Flipped` (never free-running)
//! and only while the session is active and no flip is pending. Drop
//! order on shutdown is clients → backend → DRM device → seat; the
//! `Server` fields are declared in exactly that order.

pub mod control;
pub mod logging;
pub mod protocol;
pub mod render;
pub mod signals;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nitro_kms::{
    Backend, DrmBackend, DrmOptions, Error as KmsError, Event, FakeBackend, OutputId, OutputInfo,
    Rect,
};
use nitro_seat::{Device, Seat, SeatEvent};
use rustix::event::epoll::{self, EventData, EventFlags};

use crate::control::{Client, ReadOutcome};
use crate::protocol::Request;
use crate::render::Scene;

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
}

/// Everything [`run`] needs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Display backend.
    pub backend: BackendKind,
    /// Stop moving the bar after this long (`None`: keep moving). The
    /// default, 3 s, is what makes the server idle at 0 % CPU.
    pub bar_stop: Option<Duration>,
    /// Control socket path (see [`control::resolve`]).
    pub control_path: PathBuf,
    /// Install SIGTERM/SIGINT handlers. Tests turn this off.
    pub handle_signals: bool,
}

impl Config {
    /// The fake backend at `width × height`, control socket at `path`,
    /// no signal handlers, bar frozen at the left edge. What tests want.
    #[must_use]
    pub fn fake(width: u32, height: u32, path: impl Into<PathBuf>) -> Self {
        Self {
            backend: BackendKind::Fake { width, height },
            bar_stop: Some(Duration::ZERO),
            control_path: path.into(),
            handle_signals: false,
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
const TOK_CLIENT_BASE: u64 = 1 << 32;

/// Flip-interval statistics for `stats` and the log.
#[derive(Debug, Default)]
struct FlipStats {
    last: Option<Duration>,
    count: u64,
    sum: Duration,
    min: Option<Duration>,
    max: Duration,
}

impl FlipStats {
    fn record(&mut self, t: Duration) {
        if let Some(prev) = self.last {
            let iv = t.saturating_sub(prev);
            self.count += 1;
            self.sum += iv;
            self.min = Some(self.min.map_or(iv, |m| m.min(iv)));
            self.max = self.max.max(iv);
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

/// Unlinks the control socket file on drop.
struct SocketFile(PathBuf);

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!("removing control socket: {e}");
        }
    }
}

/// The running server. Field order is drop order: clients first, then the
/// backend (which holds a dup of the DRM fd), then the seat's `Device`
/// (closes itself through the seat), then the seat.
struct Server {
    clients: HashMap<u64, Client>,
    listener: UnixListener,
    // Held for their drop side effects only.
    _socket_file: SocketFile,
    signals: Option<signals::Signals>,
    scenes: HashMap<OutputId, Scene>,
    backend: Box<dyn Backend>,
    _device: Option<Device>,
    seat: Option<Seat>,
    epoll: OwnedFd,

    config: Config,
    next_client: u64,
    active: bool,
    quit: bool,
    frames: u64,
    started: Instant,
    flips: FlipStats,
    events: Vec<Event>,
    damage: Vec<Rect>,
}

/// Run the server until `quit`, SIGTERM or SIGINT.
///
/// # Errors
/// Anything fatal at startup (no seat, no device, socket in use) or in
/// the loop (I/O on the epoll or DRM fd).
pub fn run(config: Config) -> Result<(), Error> {
    let epoll = epoll::create(epoll::CreateFlags::CLOEXEC).map_err(errno("epoll_create"))?;
    let signals = if config.handle_signals {
        let s = signals::Signals::install().map_err(io_err("install signal handlers"))?;
        add(&epoll, &s, TOK_SIGNALS)?;
        Some(s)
    } else {
        None
    };

    // Declared before `device`/`backend` so an early `?` drops it last.
    let mut seat: Option<Seat> = None;
    let mut device: Option<Device> = None;
    let backend: Box<dyn Backend> = match &config.backend {
        BackendKind::Fake { width, height } => {
            info!("fake backend {width}x{height}");
            Box::new(FakeBackend::single(*width, *height).map_err(io_err("create fake backend"))?)
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
            seat = Some(s);
            device = Some(dev);
            be
        }
    };

    let listener = control::bind(&config.control_path).map_err(io_err("bind control socket"))?;
    add(&epoll, &listener, TOK_LISTENER)?;
    info!("control socket at {}", config.control_path.display());

    let mut server = Server {
        clients: HashMap::new(),
        listener,
        _socket_file: SocketFile(config.control_path.clone()),
        signals,
        scenes: HashMap::new(),
        backend,
        _device: device,
        seat,
        epoll,
        config,
        next_client: 0,
        active: true,
        quit: false,
        frames: 0,
        started: Instant::now(),
        flips: FlipStats::default(),
        events: Vec::new(),
        damage: Vec::new(),
    };
    server.register_backend()?;
    server.sync_scenes();
    server.paint_all();
    let result = server.event_loop();
    info!(
        "shutting down after {} frames ({} clients connected)",
        server.frames,
        server.clients.len()
    );
    // Ordered teardown: clients, socket, backend, device, seat.
    drop(server);
    result
}

fn add(epoll: &OwnedFd, fd: &impl AsFd, token: u64) -> Result<(), Error> {
    epoll::add(epoll, fd, EventData::new_u64(token), EventFlags::IN).map_err(errno("epoll_ctl add"))
}

/// Block until the seat reports active. Returns `false` if a signal
/// arrived first (only possible when handlers are installed).
fn wait_active(epoll: &OwnedFd, seat: &mut Seat, signals: bool) -> Result<bool, Error> {
    let mut buf = [MaybeUninit::<epoll::Event>::uninit(); 8];
    while !seat.is_active() {
        info!("waiting for the seat to become active");
        let (ready, _) = epoll::wait(epoll, &mut buf, None).map_err(errno("epoll_wait"))?;
        for ev in ready.iter() {
            match ev.data.u64() {
                TOK_SIGNALS if signals => return Ok(false),
                TOK_SEAT => {
                    for e in seat.dispatch()? {
                        info!("seat event {e:?} (before device open)");
                        if e == SeatEvent::Disable {
                            seat.ack_disable()?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(true)
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

    /// Bring `scenes` in line with `backend.outputs()`; log changes.
    fn sync_scenes(&mut self) {
        let outputs: Vec<OutputInfo> = self.backend.outputs().to_vec();
        self.scenes.retain(|id, _| {
            let keep = outputs.iter().any(|o| o.id == *id);
            if !keep {
                info!("{id} gone");
            }
            keep
        });
        for o in &outputs {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.scenes.entry(o.id) {
                info!(
                    "{} {}: {}x{}@{}.{:03} Hz",
                    o.id,
                    o.name,
                    o.width,
                    o.height,
                    o.refresh_mhz / 1000,
                    o.refresh_mhz % 1000
                );
                slot.insert(Scene::new(o.width, o.height));
            }
        }
    }

    fn bar_moving(&self) -> bool {
        self.config
            .bar_stop
            .is_none_or(|stop| self.started.elapsed() < stop)
    }

    /// Paint and commit one output if it is writable right now.
    fn paint(&mut self, id: OutputId) {
        if !self.active || self.backend.flip_pending(id) {
            return;
        }
        let moving = self.bar_moving();
        let Some(scene) = self.scenes.get_mut(&id) else {
            return;
        };
        if !moving && !scene.needs_full_repaint() {
            return;
        }
        self.damage.clear();
        match self.backend.back_buffer(id) {
            Ok(mut buf) => scene.paint(&mut buf, &mut self.damage),
            Err(e) => {
                warn!("{id}: back buffer: {e}");
                return;
            }
        }
        if moving {
            scene.advance();
        }
        if let Err(e) = self.backend.commit(id, &self.damage) {
            warn!("{id}: commit: {e}");
        }
    }

    fn paint_all(&mut self) {
        let ids: Vec<OutputId> = self.scenes.keys().copied().collect();
        for id in ids {
            self.paint(id);
        }
    }

    fn event_loop(&mut self) -> Result<(), Error> {
        let mut buf = [MaybeUninit::<epoll::Event>::uninit(); 32];
        while !self.quit {
            let (ready, _) =
                epoll::wait(&self.epoll, &mut buf, None).map_err(errno("epoll_wait"))?;
            for ev in ready.iter() {
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
                    TOK_BACKEND => self.on_backend()?,
                    t if t >= TOK_CLIENT_BASE => self.on_client(t, flags),
                    t => warn!("unknown epoll token {t}"),
                }
            }
        }
        Ok(())
    }

    fn on_seat(&mut self) -> Result<(), Error> {
        let events = match self.seat.as_mut() {
            Some(seat) => seat.dispatch()?,
            None => return Ok(()),
        };
        for ev in events {
            match ev {
                SeatEvent::Disable => {
                    info!("session inactive: pausing");
                    self.backend.pause();
                    self.active = false;
                    if let Some(seat) = self.seat.as_mut() {
                        seat.ack_disable()?;
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
                    for scene in self.scenes.values_mut() {
                        scene.invalidate();
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
        for ev in events.drain(..) {
            match ev {
                Event::Flipped {
                    output,
                    sequence,
                    time,
                } => {
                    self.frames += 1;
                    self.flips.record(time);
                    debug!("{output} flipped seq={sequence} t={time:?}");
                    self.paint(output);
                }
                Event::Hotplug => {
                    info!("hotplug");
                    self.unregister_backend();
                    match self.backend.rescan() {
                        Ok(changed) => {
                            if changed {
                                self.sync_scenes();
                            }
                        }
                        Err(e) => warn!("rescan: {e}"),
                    }
                    self.register_backend()?;
                    self.paint_all();
                }
            }
        }
        self.events = events;
        Ok(())
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
                    debug!("client {} connected", token - TOK_CLIENT_BASE);
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
                    debug!("client {}: write: {e}", token - TOK_CLIENT_BASE);
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
            debug!("client {} closed", token - TOK_CLIENT_BASE);
            // Dropping the stream removes it from the epoll set.
        }
    }

    fn handle_request(&mut self, client: &mut Client, line: &str) {
        debug!("request {line:?}");
        let reply = match protocol::parse(line) {
            Err(msg) => protocol::err_reply(&msg),
            Ok(Request::Outputs) => protocol::outputs_reply(self.backend.outputs()),
            Ok(Request::Stats) => {
                let pending = self
                    .backend
                    .outputs()
                    .iter()
                    .filter(|o| self.backend.flip_pending(o.id))
                    .count() as u64;
                protocol::stats_reply(&[
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
                ])
            }
            Ok(Request::Shot(name)) => self.shot(name.as_deref()),
            Ok(Request::Quit) => {
                info!("quit requested");
                self.quit = true;
                protocol::ok_reply()
            }
        };
        client.send(reply);
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

    #[test]
    fn flip_stats_track_intervals() {
        let mut s = FlipStats::default();
        s.record(Duration::from_millis(100));
        assert_eq!(s.mean_us(), 0);
        s.record(Duration::from_millis(116));
        s.record(Duration::from_millis(134));
        assert_eq!(s.count, 2);
        assert_eq!(s.min, Some(Duration::from_millis(16)));
        assert_eq!(s.max, Duration::from_millis(18));
        assert_eq!(s.mean_us(), 17_000);
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
}
