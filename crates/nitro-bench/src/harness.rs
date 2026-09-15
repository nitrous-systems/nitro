//! The harness every scenario shares: connect, open a window, run a
//! frame-paced loop for N seconds, and account for what it cost.
//!
//! # The one design decision that shapes the whole benchmark
//!
//! x11perf asks "how many of operation X can this server do per second"
//! and answers by doing X in a tight loop with no display involved.
//! Nitro cannot be measured that way and it is not a shortcoming: there
//! is no immediate-mode op to loop on. A client sends *mutations to a
//! retained tree* and the server decides when to paint. Running a tight
//! loop of mutations would measure the socket's throughput and nothing
//! else — the server would coalesce them all into one frame, exactly as
//! designed, and the number would say nothing about painting.
//!
//! So the honest port is **frame-paced**: one `RequestFrame`, one batch of
//! mutations, one `Commit`, per callback. That is what a real animating
//! client does (`docs/wire.md`, and `nitro-demo --animate`), it keeps
//! exactly one transaction in flight, and it turns the question from "how
//! many ops per second" — which the display caps at refresh × N — into
//! the two questions that still discriminate above the cap:
//!
//! 1. **How many mutations per frame does the server sustain** before
//!    `paint_us_max` crosses the frame period or a flip interval shows a
//!    gap? That is the x11perf number, transposed: sweep N until it
//!    breaks.
//! 2. **What did a presented frame cost in CPU**, on both sides? That
//!    number does not saturate at 60 Hz, which is why [`cpu`] exists.
//!
//! [`cpu`]: crate::cpu
//!
//! # Why not free-run
//!
//! A free-running loop that commits as fast as the socket accepts would
//! produce a bigger commits/s number and a meaningless one: the server
//! applies the newest transaction per vblank and discards nothing, so the
//! extra commits are work the client did that never reached a photon.
//! Measuring them would reward exactly the behaviour `DESIGN.md` exists
//! to forbid. The harness therefore refuses to have a `--free-run` flag,
//! and the `commits` and `presented` columns sitting next to each other
//! in the report are how a reader checks that discipline held.

use std::time::{Duration, Instant};

use nitro_core::Size;
use nitro_wire::client::Connection;
use nitro_wire::msg::{ClientMsg, Commit, CreateWindow, RequestFrame, ServerMsg};
use nitro_wire::types::{Layer, NodeId, window_flags};
use nitro_wire::{Error as WireError, Writer};
use rustix::event::{PollFd, PollFlags};

use crate::cpu::{self, CpuTicks};
use crate::record::Record;

/// The window every scenario draws into.
///
/// One id for every scenario, because a scenario owns the whole
/// connection: there is never a second window, so a constant is clearer
/// than an allocator.
pub const WINDOW: NodeId = NodeId(1);

/// First node id a scenario may allocate. Everything below is the
/// harness's.
pub const FIRST_NODE: u32 = 16;

/// Default size of the benchmark window in logical pixels, chosen because
/// it is the period-correct one: 640×480 is what every effect in this
/// crate originally ran at, and putting the fullscreen variants next to a
/// VGA-sized one is half the point of the exercise.
pub const PERIOD_SIZE: Size = Size::new(640.0, 480.0);

/// What a scenario has to implement.
///
/// Deliberately tiny: the harness owns the connection, the pacing, the
/// clock and the accounting, and a scenario owns only "what do I send for
/// frame `frame`". That split is what keeps the scenarios comparable —
/// they cannot accidentally differ in how they pace or how they count,
/// because they do not do either.
pub trait Scenario {
    /// Row label, and the `scenario` field of the JSON record.
    fn name(&self) -> &'static str;

    /// Mutations that build the initial scene, sent in the first
    /// transaction together with the window.
    ///
    /// # Errors
    /// Whatever building the scene needs (a memfd, mostly).
    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error>;

    /// Mutations for frame `frame`. Called once per `Frame` callback.
    ///
    /// # Errors
    /// As [`Scenario::build`].
    fn frame(&mut self, ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error>;

    /// Microseconds this scenario spent in its own pixel work, so the
    /// report can subtract it: `frame = effect + upload + server paint +
    /// copy`, and a fullscreen effect whose own sine loop costs 9 ms has
    /// not told you anything about the compositor.
    ///
    /// Zero for the scenarios that push no pixels, which is the honest
    /// answer for them rather than a missing column.
    fn compute_us(&self) -> u64 {
        0
    }

    /// Microseconds spent writing pixels into the client buffer's memfd —
    /// the `pwrite` the server will `pread` back. Kept apart from
    /// [`Scenario::compute_us`] because they are different costs with
    /// different fixes: one is the effect, the other is the wire.
    fn upload_us(&self) -> u64 {
        0
    }
}

/// What a scenario is given each time it is asked for mutations.
///
/// It carries the window geometry (which the *server* decides, so a
/// scenario must not assume the size it asked for) and the scale, and
/// nothing else: a scenario that wanted the socket would be doing the
/// harness's job.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    /// Window size in logical pixels, as configured by the server.
    pub size: Size,
    /// Output scale factor.
    pub scale: f32,
    /// Output refresh interval in nanoseconds, from the last `Frame`
    /// callback; 0 until the first one arrives.
    pub refresh_ns: u32,
    /// The sweep point: node count, star count, whatever the scenario's
    /// `--n` means.
    pub n: u64,
    /// The other sweep point: a buffer edge in pixels, for `--size`.
    pub size_px: u32,
}

/// Everything that can stop a run.
#[derive(Debug)]
pub enum Error {
    /// The wire protocol or its socket.
    Wire(WireError),
    /// A syscall (`memfd`, `poll`, `pwrite`).
    Io(rustix::io::Errno),
    /// The server sent a fatal `Error` message.
    Server(String),
    /// The scenario name on the command line is not one we have.
    UnknownScenario(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Wire(e) => write!(f, "wire: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Server(m) => write!(f, "server error: {m}"),
            Error::UnknownScenario(s) => write!(f, "unknown scenario {s:?}"),
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

/// How a run is configured; the parts of the command line the harness
/// itself reads.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Wall-clock length of the measured window.
    pub seconds: f64,
    /// Requested window size in logical pixels, or `None` for the
    /// output's full size (the scenario then gets whatever `Configure`
    /// says, which is the point of the fullscreen arm).
    pub size: Option<Size>,
    /// Whether to open the window fullscreen.
    pub fullscreen: bool,
    /// Free text recorded with the run: which shell was up, whether this
    /// is the control arm.
    pub note: String,
    /// Build sha, recorded so a table row can be traced to a commit.
    pub sha: String,
    /// Host the run happened on.
    pub host: String,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            seconds: 5.0,
            size: Some(PERIOD_SIZE),
            fullscreen: false,
            note: String::new(),
            sha: String::new(),
            host: String::new(),
        }
    }
}

/// A connected, window-owning benchmark client.
///
/// Split out from [`run`] so the integration test can drive it against a
/// server on a thread with the same code the binary uses — the property
/// `nitro-demo`'s split exists for, and the only thing that stops the
/// tested path drifting from the measured one.
pub struct Harness {
    /// The connection.
    pub conn: Connection,
    /// Window geometry and sweep points, handed to the scenario.
    pub ctx: Ctx,
    /// Commit serial, the client's own counter.
    pub serial: u32,
    /// Commits sent.
    pub commits: u64,
    /// `Frame` callbacks received.
    pub frames_received: u64,
    /// `Presented` messages received.
    pub presented: u64,
    /// Mutation messages sent, `Commit` excluded — the x11perf "ops"
    /// column, and the thing a sweep sweeps.
    pub mutations: u64,
    /// Bytes written to and read from the socket.
    ///
    /// Counted by re-encoding each message into a scratch [`Writer`]
    /// rather than by instrumenting the socket: the framing is
    /// deterministic, so the re-encode is exact to the byte, and the
    /// benchmark stays a *user* of `nitro-wire` rather than a reason to
    /// grow its API with a counter no real client wants. The same
    /// technique, for the same reason, as `nitro-demo`'s.
    pub tx_bytes: u64,
    /// Bytes read; see [`Harness::tx_bytes`].
    pub rx_bytes: u64,
    /// Whether the server has configured the window.
    pub configured: bool,
    /// Whether the server closed it.
    pub closed: bool,
    /// Scratch for the byte accounting.
    scratch: Writer,
}

impl Harness {
    /// Connect and open the window.
    ///
    /// # Errors
    /// Connection or encode failure.
    pub fn start(cfg: &RunConfig, n: u64, size_px: u32) -> Result<Self, Error> {
        let conn = Connection::connect_default("nitro-bench")?;
        Self::with_connection(conn, cfg, n, size_px)
    }

    /// As [`Harness::start`], over a connection the caller made.
    ///
    /// # Errors
    /// Encode or socket failure.
    pub fn with_connection(
        conn: Connection,
        cfg: &RunConfig,
        n: u64,
        size_px: u32,
    ) -> Result<Self, Error> {
        let want = cfg.size.unwrap_or(PERIOD_SIZE);
        Ok(Self {
            conn,
            ctx: Ctx {
                size: want,
                scale: 1.0,
                refresh_ns: 0,
                n,
                size_px,
            },
            serial: 0,
            commits: 0,
            frames_received: 0,
            presented: 0,
            mutations: 0,
            tx_bytes: 0,
            rx_bytes: 0,
            configured: false,
            closed: false,
            scratch: Writer::new(),
        })
    }

    /// The `CreateWindow` every scenario opens with.
    ///
    /// Undecorated: the server's title bar is real work on a real frame,
    /// and including it would fold a constant of "one decorated window"
    /// into every scenario's per-frame cost — except in `move`, whose
    /// whole subject *is* the decoration, and which therefore asks for
    /// decorated windows of its own.
    #[must_use]
    pub fn create_window(&self, title: &str) -> ClientMsg {
        CreateWindow {
            id: WINDOW,
            size: self.ctx.size,
            layer: Layer::Normal,
            flags: window_flags::UNDECORATED,
            title: title.to_owned(),
        }
        .into()
    }

    /// Send a batch and the `Commit` that closes it.
    ///
    /// # Errors
    /// Encode or socket failure.
    pub fn commit(&mut self, batch: &[ClientMsg]) -> Result<(), Error> {
        for msg in batch {
            self.conn.send(msg)?;
        }
        self.serial += 1;
        self.conn.commit(self.serial)?;
        self.commits += 1;
        self.mutations += batch.len() as u64;
        self.count_tx(batch);
        self.count_tx(&[ClientMsg::Commit(Commit {
            serial: self.serial,
        })]);
        self.flush_blocking()
    }

    /// Ask for the next frame callback. Rides in the same transaction as
    /// the mutations it answers, which is what keeps exactly one
    /// transaction in flight.
    #[must_use]
    pub fn request_frame() -> ClientMsg {
        RequestFrame { window: WINDOW }.into()
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

    /// Block for at most `timeout`, then drain and classify what arrived.
    ///
    /// Returns how many `Frame` callbacks were in the batch: the loop
    /// commits once per callback, and a wakeup carrying none is a
    /// `Presented` or an input event, which costs nothing.
    ///
    /// # Errors
    /// Socket failure, or a fatal `Error` from the server.
    pub fn pump(&mut self, timeout: Duration, events: &mut Vec<ServerMsg>) -> Result<u32, Error> {
        wait(self.conn.as_fd(), PollFlags::IN, Some(timeout))?;
        events.clear();
        match self.conn.poll(events) {
            Ok(_) => {}
            Err(WireError::Closed) => {
                self.closed = true;
                return Ok(0);
            }
            Err(e) => return Err(e.into()),
        }
        self.count_rx(events);
        let mut frames = 0;
        for msg in events.iter() {
            match msg {
                ServerMsg::Frame(f) => {
                    self.frames_received += 1;
                    self.ctx.refresh_ns = f.refresh_ns;
                    frames += 1;
                }
                ServerMsg::Presented(_) => self.presented += 1,
                ServerMsg::Configure(c) if c.window == WINDOW => {
                    self.configured = true;
                    self.ctx.size = c.size;
                    self.ctx.scale = c.scale;
                }
                ServerMsg::Closed(c) if c.window == WINDOW => self.closed = true,
                ServerMsg::Error(e) => {
                    return Err(Error::Server(format!("{:?}: {}", e.code, e.msg)));
                }
                _ => {}
            }
        }
        Ok(frames)
    }

    /// Wait until the server has configured the window, or `timeout`
    /// expires.
    ///
    /// A scenario that laid out before the first `Configure` would build
    /// its scene for the size it *asked* for, which on a fullscreen run is
    /// never the size it gets. Every reported geometry is therefore the
    /// server's, obtained here — the #3704 rule from chat `nitro-testbox`:
    /// check the state you are about to measure actually happened.
    ///
    /// # Errors
    /// Socket failure.
    pub fn await_configure(&mut self, timeout: Duration) -> Result<bool, Error> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        while !self.configured && !self.closed {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            self.pump(left, &mut events)?;
        }
        Ok(self.configured)
    }

    /// Add the framed size of `msgs` to [`Harness::tx_bytes`].
    fn count_tx(&mut self, msgs: &[ClientMsg]) {
        self.scratch.clear();
        for msg in msgs {
            if msg.encode(&mut self.scratch).is_err() {
                self.scratch.clear();
                return;
            }
        }
        self.tx_bytes += self.scratch.len() as u64;
        self.scratch.clear();
    }

    /// Add the framed size of received `msgs` to [`Harness::rx_bytes`].
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
}

/// Run one scenario for `cfg.seconds` and return its record.
///
/// The shape, in order, and every step of it is load-bearing:
///
/// 1. Connect, create the window, let the server configure it.
/// 2. Build the scene and commit it — **not** measured; a benchmark that
///    included its own setup in the per-frame average would report a
///    scenario with a thousand nodes as slower per frame than one with
///    ten purely because creating them cost more.
/// 3. Read `stats` and both CPU counters. This is the zero.
/// 4. Loop: on each `Frame`, ask the scenario for its mutations, commit
///    them with the next `RequestFrame`.
/// 5. Read `stats` and the counters again, subtract, and write the row.
///
/// # Errors
/// Connection, encode, socket or scenario failure.
pub fn run(
    scenario: &mut dyn Scenario,
    cfg: &RunConfig,
    n: u64,
    size_px: u32,
) -> Result<Record, Error> {
    let mut h = Harness::start(cfg, n, size_px)?;
    run_with(&mut h, scenario, cfg)
}

/// [`run`], over a harness the caller built — the entry point the
/// integration test uses.
///
/// # Errors
/// As [`run`].
pub fn run_with(
    h: &mut Harness,
    scenario: &mut dyn Scenario,
    cfg: &RunConfig,
) -> Result<Record, Error> {
    let title = format!("nitro-bench {}", scenario.name());
    let mut open = vec![h.create_window(&title)];
    if cfg.fullscreen {
        open.push(
            nitro_wire::msg::SetWindowState {
                window: WINDOW,
                state: nitro_wire::types::WindowState::Fullscreen,
            }
            .into(),
        );
    }
    h.commit(&open)?;
    // A second of grace: the server configures within a frame on a live
    // desktop, and a run that started laying out at the wrong size would
    // be measuring the wrong scene for its whole length.
    h.await_configure(Duration::from_secs(1))?;

    let mut ctx = h.ctx;
    let mut first = scenario.build(&mut ctx)?;
    h.ctx = ctx;
    first.push(Harness::request_frame());
    h.commit(&first)?;

    // Everything above is setup. The zero is here.
    let server_pid = cpu::find_by_comm("nitro-server").first().copied();
    let cpu0_server = server_pid
        .and_then(|p| cpu::read(p).ok())
        .unwrap_or_default();
    let cpu0_client = cpu::read(cpu::self_pid()).unwrap_or_default();
    let stats_before = crate::control::stats_or_empty();
    let started = Instant::now();
    let commits0 = h.commits;
    let presented0 = h.presented;
    let frames0 = h.frames_received;
    let mutations0 = h.mutations;
    let tx0 = h.tx_bytes;
    let rx0 = h.rx_bytes;

    let deadline = started + Duration::from_secs_f64(cfg.seconds);
    let mut events = Vec::new();
    let mut frame: u64 = 0;
    while Instant::now() < deadline && !h.closed {
        let left = deadline.saturating_duration_since(Instant::now());
        // Cap the block at a frame-ish interval so a server that stopped
        // sending callbacks (a VT switch, a `Deactivate`) ends the run at
        // its deadline instead of hanging until the next callback that
        // never comes.
        let callbacks = h.pump(left.min(Duration::from_millis(100)), &mut events)?;
        for _ in 0..callbacks {
            let mut ctx = h.ctx;
            let mut batch = scenario.frame(&mut ctx, frame)?;
            h.ctx = ctx;
            frame += 1;
            batch.push(Harness::request_frame());
            h.commit(&batch)?;
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let stats_after = crate::control::stats_or_empty();
    let cpu1_server = server_pid
        .and_then(|p| cpu::read(p).ok())
        .unwrap_or_default();
    let cpu1_client = cpu::read(cpu::self_pid()).unwrap_or_default();
    let hz = cpu::ticks_per_second();

    // `RequestFrame` is the harness's own message, not the scenario's, so
    // it is subtracted back out of the mutation count: the column has to
    // mean "mutations the scenario asked for", or a scenario with zero
    // mutations per frame would report one.
    let mutations = (h.mutations - mutations0).saturating_sub(h.commits - commits0);

    Ok(Record {
        scenario: scenario.name().to_owned(),
        n: h.ctx.n,
        size: h.ctx.size_px,
        width: h.ctx.size.w as u32,
        height: h.ctx.size.h as u32,
        refresh_mhz: refresh_mhz(h.ctx.refresh_ns),
        seconds,
        commits: h.commits - commits0,
        presented: h.presented - presented0,
        frames_received: h.frames_received - frames0,
        mutations,
        tx_bytes: h.tx_bytes - tx0,
        rx_bytes: h.rx_bytes - rx0,
        compute_us: scenario.compute_us(),
        upload_us: scenario.upload_us(),
        client_cpu_us: cpu1_client.since(cpu0_client).micros(hz),
        server_cpu_us: cpu1_server.since(cpu0_server).micros(hz),
        stats_before,
        stats_after,
        sha: cfg.sha.clone(),
        host: cfg.host.clone(),
        note: cfg.note.clone(),
    })
}

/// Turn a refresh *interval* in nanoseconds into a refresh *rate* in
/// millihertz, the unit the server's `outputs` line uses.
///
/// Rounded to the nearest millihertz rather than truncated: 16 666 667 ns
/// is 60 000.0012 mHz and must print as `60000`, and truncation would give
/// 59 999 — a number that looks like a real and slightly wrong refresh
/// rate, which is the worst kind of rounding error to put in a table.
#[must_use]
pub fn refresh_mhz(refresh_ns: u32) -> u32 {
    if refresh_ns == 0 {
        return 0;
    }
    let mhz = 1_000_000_000_000f64 / f64::from(refresh_ns);
    // `+ 0.5` then truncate: `f64::round` would do, and this spells the
    // intent next to the argument above it.
    (mhz + 0.5) as u32
}

/// Block until `fd` is ready for `events`, or `timeout` elapses.
///
/// `EINTR` returns rather than retrying, so a Ctrl-C ends a run instead of
/// being swallowed until the next callback.
///
/// # Errors
/// Any `poll` failure other than `EINTR`.
pub fn wait(
    fd: std::os::fd::BorrowedFd<'_>,
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

/// CPU deltas, kept together so a caller cannot accidentally pair a
/// client's `before` with a server's `after`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuPair {
    /// The client's counters.
    pub client: CpuTicks,
    /// The server's, or the default when there is no server to read.
    pub server: CpuTicks,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sixty_hertz_interval_is_sixty_thousand_millihertz() {
        assert_eq!(refresh_mhz(16_666_667), 60_000);
        assert_eq!(refresh_mhz(8_333_333), 120_000);
        // 85 Hz, the box's third mode: 11 764 706 ns.
        assert_eq!(refresh_mhz(11_764_706), 85_000);
    }

    /// Truncation would make 60 Hz print as 59 999, which reads as a real
    /// and slightly wrong rate rather than as a rounding bug.
    #[test]
    fn the_rate_rounds_rather_than_truncates() {
        assert_ne!(refresh_mhz(16_666_667), 59_999);
    }

    #[test]
    fn no_callback_yet_is_no_rate_rather_than_a_division_fault() {
        assert_eq!(refresh_mhz(0), 0);
    }

    #[test]
    fn the_period_correct_size_is_vga() {
        assert_eq!((PERIOD_SIZE.w, PERIOD_SIZE.h), (640.0, 480.0));
    }

    #[test]
    fn the_defaults_are_a_five_second_vga_run() {
        let c = RunConfig::default();
        assert!((c.seconds - 5.0).abs() < 1e-9);
        assert_eq!(c.size, Some(PERIOD_SIZE));
        assert!(!c.fullscreen);
    }

    #[test]
    fn scenario_node_ids_do_not_collide_with_the_windows() {
        assert!(FIRST_NODE > WINDOW.raw());
    }
}
