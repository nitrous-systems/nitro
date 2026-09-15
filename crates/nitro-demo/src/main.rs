//! `nitro-demo`: the M1 measurement client. See the crate docs in
//! `lib.rs` for what it draws and why; `--help` for the command line.
//!
//! ```text
//! ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-demo --follow --stats'
//! ```

use std::process::ExitCode;
use std::time::Duration;

use nitro_demo::app::{App, Error, SUMMARY_INTERVAL, emit};
use nitro_demo::args::{self, Args, Mode};
use nitro_demo::{control, latency, png, scene};
use rustix::event::{PollFd, PollFlags};

/// One refresh at 60 Hz, in microseconds: the tolerance the client's and
/// the server's views of latency may differ by when the server has not
/// said what a frame actually is.
///
/// A **fallback**, since #3718: the tolerance is one frame, and a frame is
/// 8 333 µs at 120 Hz, so a fixed 16 667 would quietly accept a
/// half-frame disagreement the check exists to catch. The real period
/// comes off the server's `Frame` callback ([`App::refresh_ns`]) and this
/// only applies before the first one has arrived.
const FRAME_US_AT_60: u64 = 16_667;

fn main() -> ExitCode {
    let show_damage = std::env::var_os("NITRO_DEMO_SHOW_DAMAGE").is_some_and(|v| v == "1");
    let args = match args::parse(std::env::args().skip(1), show_damage) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nitro-demo: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = args.save_small.clone() {
        return save_small(&path);
    }

    // SIGINT before connecting, so Ctrl-C during the handshake still
    // exits cleanly rather than leaving a half-built window behind.
    let signals = Signals::install()?;
    let mut app = App::start(args)?;
    emit(&format!(
        "connected to {:?}: mode={} windows={} damage={}",
        app.server_name(),
        app.args.mode,
        app.windows.len(),
        app.show_damage
    ))?;
    if app.args.mode == Mode::Follow {
        emit("follow mode: move the pointer over a window; the demo commits only on input")?;
    }

    let mut events = Vec::new();
    while !app.done {
        let timeout = next_timeout(&app);
        // One poll for both descriptors: the connection is handled by
        // `tick`, and the signal pipe only has to make the poll return.
        let conn_fd = app.conn.as_fd();
        let sig_fd = signals.as_fd();
        let mut fds = [
            PollFd::new(&conn_fd, PollFlags::IN),
            PollFd::new(&sig_fd, PollFlags::IN),
        ];
        let ts = to_timespec(timeout);
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(Box::new(Error::Io(e))),
        }
        if signals.drain() {
            emit("SIGINT")?;
            break;
        }
        if fds[0].revents().intersects(PollFlags::IN | PollFlags::HUP) {
            // Zero timeout: the data is already there.
            app.tick(Some(Duration::ZERO), &mut events)?;
        }
        if app.summary_due() {
            report(&mut app, false)?;
        }
        if app.deadline() == Some(Duration::ZERO) {
            break;
        }
    }
    report(&mut app, true)?;
    Ok(())
}

/// How long the next poll may block: the shorter of the summary interval
/// and whatever is left of `--seconds`.
///
/// Never unbounded. An idle `--follow` demo genuinely has nothing to wake
/// for, but then the periodic summary would never print, and the summary
/// is the deliverable.
fn next_timeout(app: &App) -> Duration {
    app.deadline()
        .map_or(SUMMARY_INTERVAL, |d| d.min(SUMMARY_INTERVAL))
}

fn to_timespec(d: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    }
}

/// Print the latency summary, the pacing line and — with `--stats` — the
/// server's own view and the cross-check between the two.
fn report(app: &mut App, final_report: bool) -> Result<(), Box<dyn std::error::Error>> {
    let label = if final_report { "total" } else { "5s" };
    let window = if final_report {
        app.hist.summary()
    } else {
        app.hist.since_mark()
    };
    match window {
        Some(s) => emit(&format!("i2p[{label}] {}", s.line()))?,
        None => emit(&format!(
            "i2p[{label}] no samples yet (pending serials: {})",
            app.ledger.pending()
        ))?,
    }
    // The breakdown: how much of that was over before the client even saw
    // the event. `i2p - delivery` is what the client and the compositing
    // pass cost between them.
    let delivery = if final_report {
        app.delivery.summary()
    } else {
        app.delivery.since_mark()
    };
    if let Some(d) = delivery {
        emit(&format!(
            "  delivery[{label}] (libinput -> client) {}",
            d.line()
        ))?;
    }
    if !final_report {
        app.hist.mark();
        app.delivery.mark();
    }
    if final_report || app.args.mode == Mode::Animate {
        emit(&app.pacing_line())?;
    }
    if final_report {
        emit(&app.wire_line())?;
    }
    if app.args.mode == Mode::Animate {
        let (commits, callbacks) = app.pacing_since_mark();
        if commits > callbacks {
            emit(&format!(
                "WARNING: {commits} commits for {callbacks} frame callbacks in the last interval — more than one commit per frame"
            ))?;
        }
        app.mark_pacing();
    }
    if app.args.stats {
        match control::stats() {
            Ok(stats) => {
                emit(&control::i2p_line(&stats))?;
                if let (Some(client), Some(&server)) =
                    (app.hist.summary(), stats.get("i2p_mean_us"))
                {
                    // One *actual* frame: the rate the server says it is
                    // running, not the rate nitro used to always run at.
                    let frame_us = app
                        .refresh_ns
                        .map_or(FRAME_US_AT_60, |ns| u64::from(ns) / 1_000);
                    let ok = latency::agree(client.mean, server, frame_us);
                    emit(&format!(
                        "cross-check: client mean {} us vs server mean {server} us — {} (tolerance {frame_us} us)",
                        client.mean,
                        if ok { "agree" } else { "DISAGREE" }
                    ))?;
                }
            }
            Err(e) => emit(&format!("stats unavailable: {e}"))?,
        }
    }
    Ok(())
}

/// `--save-small`: grab a screenshot over the control socket, downscale it
/// and write a PNG small enough to check into `docs/`.
///
/// It lives in the demo rather than in `nitro-shot` on purpose: `nitro-shot`
/// is the raw readback tool and its stored-deflate encoder is the right
/// trade for a debugging dump, while this path exists to produce one
/// specific artefact — a documentation screenshot under a size budget.
fn save_small(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    /// Target width of a documentation screenshot.
    const MAX_WIDTH: u32 = 960;

    let shot = control::shot()?;
    let factor = scene::fit_factor(shot.width, MAX_WIDTH);
    let (w, h, data) = scene::downscale(shot.width, shot.height, shot.stride, &shot.data, factor);
    let png = png::encode_xrgb(w, h, &data);
    std::fs::write(path, &png)?;
    emit(&format!(
        "wrote {path}: {w}x{h} ({} bytes, downscaled {factor}x from {}x{})",
        png.len(),
        shot.width,
        shot.height
    ))?;
    Ok(())
}

/// SIGINT → a readable fd, via `signal-hook`'s self-pipe. Same shape as
/// the server's `signals.rs`: the "pipe" is a `UnixDatagram` pair because
/// an empty datagram is readable and an empty stream write is not.
struct Signals {
    read: std::os::unix::net::UnixDatagram,
    ids: Vec<signal_hook::SigId>,
}

impl Signals {
    fn install() -> std::io::Result<Self> {
        let (read, write) = std::os::unix::net::UnixDatagram::pair()?;
        read.set_nonblocking(true)?;
        let int = signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, write)?;
        let this = Self {
            read,
            ids: vec![int],
        };
        // `register` probes the fd with an empty send, which lands in the
        // queue exactly as a real signal would.
        this.drain();
        Ok(this)
    }

    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.read.as_fd()
    }

    fn drain(&self) -> bool {
        let mut got = false;
        let mut buf = [0u8; 64];
        loop {
            match self.read.recv(&mut buf) {
                Ok(_) => got = true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return got,
            }
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
