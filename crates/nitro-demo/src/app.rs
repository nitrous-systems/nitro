//! The event loop: connect, build, react, measure.
//!
//! # Shape
//!
//! One `poll(2)` over two descriptors — the wire socket and the SIGINT
//! self-pipe — with a timeout only when something is actually due (the
//! 5-second summary, or `--seconds`). In `--follow` an idle demo blocks in
//! the kernel indefinitely and costs literally nothing, which is the
//! client-side half of the property the server is built around.
//!
//! # One commit per frame
//!
//! `--animate` commits exactly once per `Frame` callback and asks for the
//! next callback in the same transaction. That is the contract the design
//! calls present-time scheduling: no free-running render loop, no
//! speculative second commit. [`App::frames_committed`] and
//! [`App::frames_received`] are exported so the test (and `--stats`) can
//! assert the two counts stay in step.
//!
//! `--follow` commits once per *input burst*: a motion is answered
//! immediately, but if several motions arrive in one `poll` wakeup they
//! collapse into one commit carrying the latest position. Answering each
//! one separately would send more commits than there are frames, and the
//! server would coalesce them anyway — with the extra serials landing on
//! the same vblank and flattering the histogram with duplicate samples.
//!
//! # Keys
//!
//! `q` quits, `d` toggles the damage outlines, `Esc` closes the window
//! (the server answers with `Closed`), `n`/`p` select the next/previous
//! window. Keys are matched on **evdev keycodes**, not keysyms: the demo
//! must behave the same on a layout where `q` is somewhere else, and the
//! server may be running without a compiled keymap, in which case the
//! keysym is 0 and the keycode is all there is.

use std::io::Write as _;
use std::os::fd::BorrowedFd;
use std::time::{Duration, Instant};

use nitro_core::{Point, Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::msg::{ClientMsg, RequestFrame, ServerMsg};
use nitro_wire::types::{ButtonState, NodeId};
use nitro_wire::{Error as WireError, Writer};
use rustix::event::{PollFd, PollFlags};

use crate::args::{Args, Mode};
use crate::latency::{Histogram, Ledger};
use crate::scene::{self, ANIMATE_STEP, IMG_EDGE, Ids, WINDOW_SIZE};

/// evdev keycodes the demo acts on (`KEY_*` in `linux/input-event-codes.h`).
///
/// Public so a test can press a key without hard-coding the number it is
/// also asserting on — a test that repeats the constant proves only that
/// two copies of a typo agree.
pub mod keys {
    /// `KEY_ESC`.
    pub const ESC: u32 = 1;
    /// `KEY_Q`.
    pub const Q: u32 = 16;
    /// `KEY_P`.
    pub const P: u32 = 25;
    /// `KEY_D`.
    pub const D: u32 = 32;
    /// `KEY_N`.
    pub const N: u32 = 49;
}

/// How often the running summary is printed.
pub const SUMMARY_INTERVAL: Duration = Duration::from_secs(5);

/// One of the demo's windows.
#[derive(Debug, Clone)]
pub struct Window {
    /// Node ids, derived from the window index.
    pub ids: Ids,
    /// Size the server configured, or what we asked for until it does.
    pub size: Size,
    /// Where the follower is now; empty until the first motion.
    pub follower: Rect,
    /// The damage rects of the last follow move, kept so toggling the
    /// outlines with `d` can redraw them without waiting for the pointer
    /// to move again. Without it `d` would be immediate when switching
    /// *off* (hiding needs no geometry) and a silent no-op when switching
    /// *on*, which is the more confusing half.
    pub last_damage: Vec<Rect>,
    /// Animation phase in logical pixels, and its direction.
    pub phase: (f32, f32),
    /// Whether the server has told us this window exists on an output.
    pub configured: bool,
    /// Whether the server has closed it.
    pub closed: bool,
}

impl Window {
    /// A window for index `i`, before any `Configure`.
    #[must_use]
    pub fn new(i: u32) -> Self {
        Self {
            ids: Ids::for_window(i),
            size: WINDOW_SIZE,
            follower: Rect::EMPTY,
            last_damage: Vec::new(),
            phase: (0.0, ANIMATE_STEP),
            configured: false,
            closed: false,
        }
    }
}

/// Everything the loop owns.
pub struct App {
    /// The connection.
    pub conn: Connection,
    /// The command line.
    pub args: Args,
    /// The windows, in creation order.
    pub windows: Vec<Window>,
    /// Which window `n`/`p` last selected.
    pub selected: usize,
    /// Next commit serial. Serials are the client's own counter.
    pub serial: u32,
    /// Unanswered serials and the input each answers.
    pub ledger: Ledger,
    /// Every latency sample of the run.
    pub hist: Histogram,
    /// How long each input sat before the *client* saw it: from the
    /// libinput event timestamp to the moment `poll` handed it over.
    ///
    /// The breakdown that turns a latency number into a diagnosis. The
    /// headline figure spans input to photon, which is the sum of a
    /// delivery leg (libinput → server → socket → this process) and a
    /// response leg (commit → paint → flip). Measuring the first leg
    /// separately is what says *which one* to go and fix; without it a
    /// regression in either looks identical from outside.
    ///
    /// It is the one figure taken against the client's own clock rather
    /// than differencing two server timestamps, so it carries whatever
    /// skew there is between the two `CLOCK_MONOTONIC` reads. They are
    /// the same clock on one machine, so that is nothing here, and it
    /// would matter the day this runs over a remote link.
    pub delivery: Histogram,
    /// Whether damage outlines are on right now (`d` toggles).
    pub show_damage: bool,
    /// Set by `q`, by `Closed`, or by SIGINT.
    pub done: bool,
    /// `Commit`s sent since start.
    pub frames_committed: u64,
    /// `Frame` callbacks received since start.
    pub frames_received: u64,
    /// `Presented` messages received since start.
    pub presented: u64,
    /// Bytes written to and read from the socket, for the budget table.
    ///
    /// Counted by re-encoding each message into a scratch [`Writer`]
    /// rather than by instrumenting the socket: the framing is
    /// deterministic, so the re-encode is exact to the byte, and the demo
    /// stays a *user* of `nitro-wire` rather than a reason to grow its
    /// API with a counter no real client wants.
    pub tx_bytes: u64,
    /// Bytes read; see [`App::tx_bytes`].
    pub rx_bytes: u64,
    /// Scratch buffer for that accounting, reused and cleared.
    scratch: Writer,
    /// When the first `Presented` arrived, relative to `started`.
    pub first_presented: Option<Duration>,
    /// When the app connected.
    pub started: Instant,
    /// Commits and frame callbacks at the last [`App::mark_pacing`].
    pacing_mark: (u64, u64),
    /// When the last periodic summary went out.
    last_summary: Instant,
}

impl App {
    /// Connect, build every window and commit the first frame.
    ///
    /// # Errors
    /// Connection, encode or socket failure.
    pub fn start(args: Args) -> Result<Self, Error> {
        let conn = Connection::connect_default("nitro-demo")?;
        Self::with_connection(conn, args)
    }

    /// As [`App::start`], over a connection the caller made — which is how
    /// the integration test points the demo at a server on a thread.
    ///
    /// # Errors
    /// Encode or socket failure.
    pub fn with_connection(conn: Connection, args: Args) -> Result<Self, Error> {
        let now = Instant::now();
        let mut app = Self {
            conn,
            windows: (0..args.windows).map(Window::new).collect(),
            show_damage: args.show_damage,
            args,
            selected: 0,
            serial: 0,
            ledger: Ledger::new(),
            hist: Histogram::new(),
            delivery: Histogram::new(),
            done: false,
            frames_committed: 0,
            frames_received: 0,
            presented: 0,
            tx_bytes: 0,
            rx_bytes: 0,
            scratch: Writer::new(),
            first_presented: None,
            started: now,
            pacing_mark: (0, 0),
            last_summary: now,
        };
        app.build()?;
        Ok(app)
    }

    /// Server name from the handshake.
    #[must_use]
    pub fn server_name(&self) -> &str {
        self.conn.server_name()
    }

    /// Build every window's scene and commit it as one transaction.
    fn build(&mut self) -> Result<(), Error> {
        let pixels = scene::checker(IMG_EDGE);
        let mut batch = Vec::new();
        for (i, win) in self.windows.iter().enumerate() {
            let fd = scene::memfd(&pixels)?;
            let title = format!("nitro-demo {}", i + 1);
            batch.extend(scene::build(win.ids, win.size, &title, fd));
        }
        // In `--animate` the first frame callback is what starts the
        // clock; asking for it in the build transaction means the loop has
        // nothing special to do at startup.
        if self.args.mode == Mode::Animate {
            for win in &self.windows {
                batch.push(
                    RequestFrame {
                        window: win.ids.window,
                    }
                    .into(),
                );
            }
        }
        self.commit(&batch, None)?;
        self.flush_blocking()
    }

    /// Send a batch and the `Commit` that ends it, recording the serial
    /// against `input_ns` when this transaction answers an input.
    ///
    /// # Errors
    /// Encode failure.
    pub fn commit(&mut self, batch: &[ClientMsg], input_ns: Option<u64>) -> Result<(), Error> {
        for msg in batch {
            self.conn.send(msg)?;
        }
        self.serial += 1;
        if let Some(ns) = input_ns {
            self.ledger.record(self.serial, ns);
        }
        self.conn.commit(self.serial)?;
        self.frames_committed += 1;
        self.count_tx(batch);
        self.count_tx(std::slice::from_ref(&ClientMsg::Commit(
            nitro_wire::msg::Commit {
                serial: self.serial,
            },
        )));
        Ok(())
    }

    /// Add the framed size of `msgs` to [`App::tx_bytes`].
    fn count_tx(&mut self, msgs: &[ClientMsg]) {
        self.scratch.clear();
        for msg in msgs {
            // An encode failure here is impossible for a message the
            // connection already accepted, and a byte counter is not worth
            // a second error path: skip and keep the count honest-ish.
            if msg.encode(&mut self.scratch).is_err() {
                self.scratch.clear();
                return;
            }
        }
        self.tx_bytes += self.scratch.len() as u64;
        self.scratch.clear();
    }

    /// Add the framed size of received `msgs` to [`App::rx_bytes`].
    fn count_rx(&mut self, msgs: &[ServerMsg]) {
        self.scratch.clear();
        for msg in msgs {
            if msg.encode(&mut self.scratch).is_err() {
                self.scratch.clear();
                return;
            }
        }
        self.rx_bytes += self.scratch.len() as u64;
        self.scratch.clear();
    }

    /// Push every queued byte, waiting for writability as needed.
    ///
    /// # Errors
    /// Socket failure.
    pub fn flush_blocking(&mut self) -> Result<(), Error> {
        while !self.conn.flush()? {
            wait(self.conn.as_fd(), PollFlags::OUT, None)?;
        }
        Ok(())
    }

    /// Handle one batch of server messages.
    ///
    /// Returns the number handled. Input is coalesced: the *last* motion
    /// per window in this batch is the one answered, so a burst of motions
    /// between two wakeups costs one commit.
    ///
    /// # Errors
    /// Encode or socket failure. A server hangup is not an error — it sets
    /// [`App::done`].
    pub fn handle(&mut self, events: &[ServerMsg]) -> Result<usize, Error> {
        // Per window: the latest pointer position and its input timestamp.
        let mut motions: Vec<Option<(Point, u64)>> = vec![None; self.windows.len()];
        let mut frames: Vec<Option<()>> = vec![None; self.windows.len()];
        let mut reconfigured: Vec<Option<Size>> = vec![None; self.windows.len()];
        // One clock read for the whole batch: these messages all arrived
        // in the same `poll` wakeup, so they were all delivered at the
        // same moment as far as this process is concerned, and reading
        // the clock per message would charge later ones for the time
        // spent handling the earlier ones.
        let now = crate::monotonic_ns();

        for msg in events {
            match msg {
                ServerMsg::PointerMotion(m) => {
                    if let Some(i) = self.index_of(m.window) {
                        motions[i] = Some((m.pos, m.time_ns));
                        self.delivery.push_interval(m.time_ns, now);
                    }
                }
                ServerMsg::PointerEnter(m) => {
                    if let Some(i) = self.index_of(m.window) {
                        motions[i] = Some((m.pos, m.time_ns));
                    }
                }
                ServerMsg::Presented(p) => {
                    self.presented += 1;
                    if self.first_presented.is_none() {
                        self.first_presented = Some(self.started.elapsed());
                    }
                    if let Some(input_ns) = self.ledger.take(p.serial) {
                        self.hist.push_interval(input_ns, p.time_ns);
                    }
                }
                ServerMsg::Frame(f) => {
                    self.frames_received += 1;
                    if let Some(i) = self.index_of(f.window) {
                        frames[i] = Some(());
                    }
                }
                ServerMsg::Configure(c) => {
                    if let Some(i) = self.index_of(c.window) {
                        self.windows[i].configured = true;
                        reconfigured[i] = Some(c.size);
                    }
                }
                ServerMsg::Closed(c) => {
                    if let Some(i) = self.index_of(c.window) {
                        self.windows[i].closed = true;
                    }
                    if self.windows.iter().all(|w| w.closed) {
                        self.done = true;
                    }
                }
                ServerMsg::Key(k) if k.state == ButtonState::Pressed => self.key(k.keycode)?,
                ServerMsg::Error(e) => {
                    return Err(Error::Server(format!("{:?}: {}", e.code, e.msg)));
                }
                _ => {}
            }
        }

        let mut batch = Vec::new();
        let mut input_ns = None;
        for i in 0..self.windows.len() {
            if let Some(size) = reconfigured[i] {
                self.windows[i].size = size;
                batch.extend(scene::reconfigure(self.windows[i].ids, size));
            }
            if let Some((pos, ns)) = motions[i] {
                let win = &self.windows[i];
                let (msgs, to, damage) = scene::follow(win.ids, win.size, win.follower, pos);
                batch.extend(msgs);
                batch.extend(scene::damage_outlines(win.ids, &damage, self.show_damage));
                self.windows[i].follower = to;
                self.windows[i].last_damage = damage;
                // The newest input in the batch is the one the frame
                // answers; see the module docs.
                input_ns = Some(input_ns.map_or(ns, |old: u64| old.max(ns)));
            }
            if frames[i].is_some() && self.args.mode == Mode::Animate {
                let win = &mut self.windows[i];
                let (x, dir) = scene::advance(win.size, win.phase.0, win.phase.1);
                win.phase = (x, dir);
                batch.push(
                    nitro_wire::msg::SetBounds {
                        id: win.ids.mover,
                        rect: scene::mover_rect(win.size, x),
                    }
                    .into(),
                );
                // Ask for the next one in the same transaction: one
                // request, one answer, one commit — no free-running loop.
                batch.push(
                    RequestFrame {
                        window: win.ids.window,
                    }
                    .into(),
                );
            }
        }
        if !batch.is_empty() {
            self.commit(&batch, input_ns)?;
            self.flush_blocking()?;
        }
        // The pacing interval starts once the animation is running and
        // its commit is out: from here on it is one commit per callback,
        // with exactly one `RequestFrame` in flight at all times. Marking
        // before that commit would bake the in-flight one into the offset
        // and report a constant startup violation forever; marking here
        // makes the offset cancel between any two marks.
        //
        // `>= 1`, not `== 1`: with several windows a single wakeup can
        // deliver one `Frame` per window and take the counter straight
        // from 0 to N, and an equality test would then never fire at all,
        // silently degenerating `pacing_since_mark` into raw totals.
        if self.frames_received >= 1 && self.pacing_mark == (0, 0) {
            self.mark_pacing();
        }
        Ok(events.len())
    }

    /// Act on a key press, by evdev keycode.
    fn key(&mut self, keycode: u32) -> Result<(), Error> {
        match keycode {
            keys::Q => self.done = true,
            keys::D => {
                self.show_damage = !self.show_damage;
                // Redraw the *last* damage rects at the new state right
                // away, so `d` is visible without moving the pointer.
                // Passing an empty list would resolve every slot to
                // `None` and hide all eight, which makes switching on a
                // no-op until the next motion.
                let show = self.show_damage;
                let batch: Vec<ClientMsg> = self
                    .windows
                    .iter()
                    .flat_map(|w| scene::damage_outlines(w.ids, &w.last_damage, show))
                    .collect();
                self.commit(&batch, None)?;
                self.flush_blocking()?;
            }
            keys::ESC => {
                let batch: Vec<ClientMsg> = self
                    .windows
                    .iter()
                    .filter(|w| !w.closed)
                    .map(|w| nitro_wire::msg::DestroyNode { id: w.ids.window }.into())
                    .collect();
                self.commit(&batch, None)?;
                self.flush_blocking()?;
                for w in &mut self.windows {
                    w.closed = true;
                }
                self.done = true;
            }
            keys::N | keys::P => {
                let n = self.windows.len();
                if n > 1 {
                    let old = self.selected;
                    self.selected = if keycode == keys::N {
                        (old + 1) % n
                    } else {
                        (old + n - 1) % n
                    };
                    let mut batch = scene::select(self.windows[old].ids, false);
                    batch.extend(scene::select(self.windows[self.selected].ids, true));
                    self.commit(&batch, None)?;
                    self.flush_blocking()?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Which of our windows a server message names.
    fn index_of(&self, window: NodeId) -> Option<usize> {
        let i = Ids::window_index(window)? as usize;
        (i < self.windows.len()).then_some(i)
    }

    /// Block until something happens, then drain and handle it.
    ///
    /// `timeout` bounds the wait so the periodic summary and `--seconds`
    /// still fire on an idle desktop.
    ///
    /// # Errors
    /// Socket or encode failure.
    pub fn tick(
        &mut self,
        timeout: Option<Duration>,
        events: &mut Vec<ServerMsg>,
    ) -> Result<(), Error> {
        wait(self.conn.as_fd(), PollFlags::IN, timeout)?;
        events.clear();
        match self.conn.poll(events) {
            Ok(_) => {}
            Err(WireError::Closed) => {
                self.done = true;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        self.count_rx(events);
        self.handle(events)?;
        Ok(())
    }

    /// Whether the 5-second summary is due, resetting the timer if it is.
    pub fn summary_due(&mut self) -> bool {
        if self.last_summary.elapsed() >= SUMMARY_INTERVAL {
            self.last_summary = Instant::now();
            return true;
        }
        false
    }

    /// Time left before `--seconds` expires, or `None` when it does not
    /// apply.
    #[must_use]
    pub fn deadline(&self) -> Option<Duration> {
        (self.args.seconds > 0)
            .then(|| Duration::from_secs(self.args.seconds).checked_sub(self.started.elapsed()))
            .map(Option::unwrap_or_default)
    }

    /// Commits and frame callbacks since the last [`App::mark_pacing`].
    ///
    /// The pacing rule is about *increments*, not totals, and the totals
    /// can never be equal: the build transaction is a commit no callback
    /// asked for, and the `RequestFrame` riding on the newest commit has
    /// not been answered yet. Comparing totals would report a constant
    /// startup offset as a violation forever.
    #[must_use]
    pub fn pacing_since_mark(&self) -> (u64, u64) {
        (
            self.frames_committed - self.pacing_mark.0,
            self.frames_received - self.pacing_mark.1,
        )
    }

    /// Start a new interval for [`App::pacing_since_mark`].
    pub fn mark_pacing(&mut self) {
        self.pacing_mark = (self.frames_committed, self.frames_received);
    }

    /// The line the `--animate` pacing check prints: commits, frame
    /// callbacks and presentations should move together.
    #[must_use]
    pub fn pacing_line(&self) -> String {
        let secs = self.started.elapsed().as_secs_f64().max(1e-9);
        format!(
            "pacing: commits={} frames={} presented={} over {:.1}s ({:.1} commits/s, {:.1} presented/s)",
            self.frames_committed,
            self.frames_received,
            self.presented,
            secs,
            self.frames_committed as f64 / secs,
            self.presented as f64 / secs,
        )
    }

    /// The line the budget table quotes: bytes on the wire and how long
    /// the first frame took to reach the screen.
    #[must_use]
    pub fn wire_line(&self) -> String {
        let first = self.first_presented.map_or_else(
            || "?".to_owned(),
            |d| format!("{:.1}ms", d.as_secs_f64() * 1e3),
        );
        format!(
            "wire: tx_bytes={} rx_bytes={} connect_to_first_presented={first}",
            self.tx_bytes, self.rx_bytes
        )
    }
}

/// Anything that stops the demo.
#[derive(Debug)]
pub enum Error {
    /// The wire protocol or its socket.
    Wire(WireError),
    /// A syscall (`memfd`, `poll`).
    Io(rustix::io::Errno),
    /// The server sent a fatal `Error` message.
    Server(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Wire(e) => write!(f, "wire: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Server(m) => write!(f, "server error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<WireError> for Error {
    fn from(e: WireError) -> Self {
        Error::Wire(e)
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Error::Io(e)
    }
}

/// Block until `fd` is ready for `events`, or `timeout` elapses.
///
/// `EINTR` returns rather than retrying: SIGINT is how the demo is asked
/// to print its summary, and a retry loop here would swallow it until the
/// next unrelated wakeup.
///
/// # Errors
/// Any `poll` failure other than `EINTR`.
pub fn wait(
    fd: BorrowedFd<'_>,
    events: PollFlags,
    timeout: Option<Duration>,
) -> Result<(), rustix::io::Errno> {
    let mut fds = [PollFd::new(&fd, events)];
    let ts = timeout.map(|d| rustix::event::Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    });
    match rustix::event::poll(&mut fds, ts.as_ref()) {
        Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Print one line and flush, so output survives a pipe (`ssh`, `grep`).
///
/// # Errors
/// Whatever stdout does.
pub fn emit(line: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_starts_unconfigured_with_no_follower() {
        let w = Window::new(3);
        assert_eq!(w.ids, Ids::for_window(3));
        assert!(w.follower.is_empty());
        assert!(!w.configured && !w.closed);
        assert_eq!(w.size, WINDOW_SIZE);
    }

    #[test]
    fn the_keycodes_are_the_evdev_ones() {
        // The values a `KEY_*` header gives; a typo here would silently
        // make `q` not quit on the box.
        assert_eq!(
            (keys::ESC, keys::Q, keys::D, keys::N, keys::P),
            (1, 16, 32, 49, 25)
        );
    }
}
