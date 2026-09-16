//! Every scenario against a real `nitro-server`, on the fake backend, in
//! process.
//!
//! Same harness shape as `crates/nitro-demo/tests/against_server.rs` and
//! `crates/nitro-server/tests/fake_loop.rs`: the server's own
//! `run(Config)` on a thread with a fake backend, so there is no seat, no
//! DRM device and no evdev node. The client is the benchmark's own
//! [`Harness`] and its real scenarios, not a reimplementation — a copy
//! would drift from the thing being shipped.
//!
//! # What this test is for, and what it deliberately is not
//!
//! It is **not** a performance test. A fake backend flips against a timer
//! and a CI machine is not the box, so any microsecond asserted here
//! would be noise dressed as a result. The numbers in `docs/bench.md`
//! come from the box and only from the box.
//!
//! What it pins is the half that *can* be wrong silently and would
//! invalidate every number: that each scenario's mutations are ones the
//! real server **accepts** (a rejected op is a fatal `Error` and a
//! benchmark that measured a dead connection would report a beautifully
//! low CPU figure), that a run produces frames, and that the four
//! structural claims the report rests on are true against the real
//! server rather than against the test's idea of it:
//!
//! 1. every scenario commits and gets `Presented` back;
//! 2. `text-static` does not move `text_layouts` — the retained-text
//!    claim, checked on the server's own counter;
//! 3. `create` returns the server's `nodes` count to where it started —
//!    the leak check;
//! 4. the pixel path really does read the client's buffer (the pixels the
//!    effect computed are on the screen), so `putimage` is measuring an
//!    upload and not a no-op.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_bench::harness::{Harness, RunConfig, run_with};
use nitro_bench::record::Record;
use nitro_bench::scenario;
use nitro_kms::Image;
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;

/// Wait for a condition, polling. Every wait here has a deadline: a test
/// that hangs tells you nothing.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A server on a thread, plus its two sockets.
struct Box_ {
    dir: PathBuf,
    control: PathBuf,
    wire: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Box_ {
    fn start(name: &str, width: u32, height: u32) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nitro-bench-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let control = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &control);
        config.backend = BackendKind::Fake { width, height };
        let wire = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let b = Self {
            dir,
            control,
            wire,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&b.control).is_ok()
        });
        wait_for("the wire socket", || b.wire.exists());
        b
    }

    /// Run `name` for a short window and hand back the record.
    ///
    /// Half a second, not five: the assertions below are about shape, and
    /// a suite that spent five seconds per scenario on sixteen scenarios
    /// would be a minute and a half of CI for nothing extra.
    fn bench(&self, name: &str, n: u64, size: u32) -> Record {
        let cfg = RunConfig {
            seconds: 0.5,
            size: Some(nitro_core::Size::new(320.0, 240.0)),
            fullscreen: false,
            note: "fake backend".to_owned(),
            sha: "test".to_owned(),
            host: "test".to_owned(),
        };
        let conn = Connection::connect(&self.wire, "nitro-bench").expect("wire connect");
        let mut h = Harness::with_connection(conn, &cfg, n, size).expect("harness");
        let mut s = scenario(name, n, size, 320, 240).expect("scenario");
        run_with(&mut h, s.as_mut(), &cfg).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn connect(&self) -> BufReader<UnixStream> {
        let s = UnixStream::connect(&self.control).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        BufReader::new(s)
    }

    fn request_text(&self, req: &str) -> Vec<String> {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut lines = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = c.read_line(&mut line).unwrap();
            assert!(n > 0, "connection closed mid-reply");
            let l = line.trim_end_matches('\n').to_owned();
            if l.is_empty() {
                break;
            }
            lines.push(l);
        }
        lines
    }

    fn stats(&self) -> Vec<String> {
        self.request_text("stats\n")
    }

    fn shot(&self) -> Image {
        let mut c = self.connect();
        c.get_mut().write_all(b"shot\n").unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let fields: Vec<u32> = header
            .trim_end()
            .strip_prefix("ok ")
            .expect("ok header")
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let (width, height, stride) = (fields[0], fields[1], fields[2]);
        let mut data = vec![0u8; (stride * height) as usize];
        c.read_exact(&mut data).unwrap();
        Image {
            width,
            height,
            stride,
            data,
        }
    }

    /// Wait until the server has stopped reacting. Not "wait for N more
    /// frames": an idle server deliberately stops flipping, so counting
    /// would hang.
    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let s = self.stats();
            let frames = stat(&s, "frames");
            let pending = stat(&s, "flips_pending");
            if pending == 0 && frames == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = frames;
            std::thread::sleep(Duration::from_millis(10));
            stable >= 3
        });
    }

    fn quit(mut self) {
        let mut c = self.connect();
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stat(lines: &[String], key: &str) -> u64 {
    lines
        .iter()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
        .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
        .parse()
        .unwrap()
}

/// The claim every other number depends on: the real server accepts every
/// scenario's mutations and turns them into frames.
///
/// A scenario that sent an op the server rejects would take a fatal
/// `Error`, the connection would close, and the run would report a
/// wonderfully low CPU cost for having done nothing. `run_with` turns
/// that into a panic here, which is why this test is a loop over
/// `SCENARIOS` rather than a spot check — a new scenario is covered by
/// existing at all.
#[test]
fn every_scenario_runs_against_a_real_server() {
    let b = Box_::start("all", 320, 240);
    for (name, _) in nitro_bench::SCENARIOS {
        // Small sweep points: the assertion is that it works, not that it
        // is fast, and 500 text nodes on a fake backend is a slow test
        // with no extra coverage.
        let rec = b.bench(name, 8, if *name == "putimage" { 32 } else { 0 });
        assert_eq!(&rec.scenario, name);
        assert!(rec.commits > 0, "{name}: no commits");
        assert!(rec.presented > 0, "{name}: nothing was presented");
        assert!(rec.tx_bytes > 0, "{name}: nothing went out on the wire");
        assert!(rec.seconds > 0.0);
        // The window is the one the server configured, not the one asked
        // for: a scenario laying out at the wrong size would be measuring
        // a scene nobody sees.
        assert!(rec.width > 0 && rec.height > 0, "{name}: no geometry");
    }
    b.quit();
}

/// The retained-text claim, checked on the server's own counter.
///
/// `text` relabels every node every frame and must make the server shape;
/// `text-static` moves the same nodes and must not. `text_layouts` is the
/// server's count of shaping passes, so the comparison is on the
/// server's books rather than on the client's intention — and the client
/// *cannot* be trusted here, since "I sent no `SetText`" is exactly what
/// a scenario with a bug would also say.
#[test]
fn moving_text_does_not_reshape_it_but_relabelling_does() {
    let b = Box_::start("text", 320, 240);

    let before = stat(&b.stats(), "text_layouts");
    let moved = b.bench("text-static", 8, 0);
    b.settle();
    let after_static = stat(&b.stats(), "text_layouts");

    let relabelled = b.bench("text", 8, 0);
    b.settle();
    let after_relabel = stat(&b.stats(), "text_layouts");

    // The static arm shapes its eight labels once, when it builds them,
    // and then never again however many frames it ran for. The bound is
    // "the build, and nothing per frame": anything proportional to the
    // frame count would fail it.
    let static_layouts = after_static - before;
    assert!(
        static_layouts <= 8 + 8,
        "text-static shaped {static_layouts} times over {} frames \
         — a moved label must not reshape",
        moved.commits
    );

    // The relabelling arm must shape roughly once per label per frame.
    // A weak lower bound rather than an equality: the server is free to
    // coalesce two commits that land on one vblank, and this test is
    // about the direction, not the constant.
    let relabel_layouts = after_relabel - after_static;
    assert!(
        relabel_layouts > static_layouts,
        "relabelling shaped {relabel_layouts} times, moving shaped {static_layouts}"
    );
    assert!(
        relabel_layouts >= relabelled.commits,
        "{relabel_layouts} layouts for {} commits of 8 labels each",
        relabelled.commits
    );
    b.quit();
}

/// The leak check `create` exists for: after a run that made and destroyed
/// nodes for half a second, the server's `nodes` count is back where it
/// started.
///
/// One leaked node per opened menu is invisible for an hour and fatal for
/// a session. This is the cheapest possible instrument for it and it is
/// the reason every record carries `stats_before` and `stats_after`.
#[test]
fn creating_and_destroying_nodes_leaves_the_count_where_it_was() {
    let b = Box_::start("create", 320, 240);
    b.settle();
    let before = stat(&b.stats(), "nodes");
    let rec = b.bench("create", 16, 0);
    assert!(rec.commits > 1, "the run never cycled a generation");
    // The client disconnects when the harness drops, which is what frees
    // its remaining nodes; settle so the server has processed the hangup.
    b.settle();
    let after = stat(&b.stats(), "nodes");
    assert_eq!(
        after,
        before,
        "{} node(s) leaked over {} generations",
        after.cast_signed() - before.cast_signed(),
        rec.commits
    );
    b.quit();
}

/// The pixel path really pushes pixels.
///
/// `putimage` is only a measurement of an upload if the bytes the effect
/// computed actually reach the screen. A `BufferDamage` the server
/// ignored, a stride mismatch, or a format the rasterizer silently
/// dropped would all produce a run with commits, frames and a perfectly
/// healthy CPU figure — and a black window. So the claim is settled on
/// **pixels**: the window's interior must not be uniform after the run,
/// because the plasma is a gradient and a failed upload is a flat fill.
///
/// This is the room's rule from `nitro-testbox` in its own costume: a
/// counter agreeing with the code proves nothing when the counter is
/// measuring the layer below the broken one.
#[test]
fn the_pixel_path_puts_the_clients_bytes_on_the_screen() {
    let b = Box_::start("putimage", 320, 240);
    let rec = b.bench("putimage", 0, 64);
    assert!(rec.presented > 0);
    b.settle();

    let img = b.shot();
    let mut seen = std::collections::BTreeSet::new();
    for y in 0..img.height {
        for x in 0..img.width {
            let off = (y * img.stride + x * 4) as usize;
            seen.insert([img.data[off], img.data[off + 1], img.data[off + 2]]);
            if seen.len() > 8 {
                break;
            }
        }
    }
    // The desktop backdrop is itself a gradient, so "more than one colour
    // on the screen" would pass with a black window. Eight distinct
    // colours is a plasma; a backdrop gradient over a 240-row output is a
    // handful of steps, and a *failed* upload leaves the window one flat
    // colour over it.
    assert!(
        seen.len() > 8,
        "only {} distinct colours on screen — the buffer never reached it",
        seen.len()
    );
    b.quit();
}

/// A run's own accounting adds up: every commit is a frame's worth of
/// mutations, and the `RequestFrame` the harness rides along is not
/// charged to the scenario.
///
/// `rects` sends exactly `n` `SetFill`s per frame, so `mutations /
/// commits` must come out at `n` — not `n + 1`. That one-off is the
/// easiest possible accounting bug and it would inflate every
/// mutations-per-frame figure in the report by a constant nobody would
/// notice.
#[test]
fn the_mutation_count_excludes_the_harnesss_own_frame_request() {
    let b = Box_::start("accounting", 320, 240);
    let rec = b.bench("rects", 10, 0);
    assert!(rec.commits >= 2, "not enough frames to check the ratio");
    let per_frame = rec.mutations_per_frame();
    assert!(
        (per_frame - 10.0).abs() < 0.5,
        "{per_frame} mutations per frame for 10 rects — \
         the RequestFrame is being charged to the scenario"
    );
    b.quit();
}

/// The record a run produces is the record the report reads back.
///
/// The ledger is a file written by one process and read by another, so a
/// field that serialised but did not parse would lose a column silently —
/// and the first anyone would know is a table of `?`.
#[test]
fn a_real_record_round_trips_through_the_ledger_format() {
    let b = Box_::start("roundtrip", 320, 240);
    let rec = b.bench("boing-node", 0, 32);
    let line = rec.to_json();
    assert!(!line.contains('\n'), "a jsonl record must be one line");
    let back = Record::from_json(&line).expect("re-parse");
    assert_eq!(back, rec);
    // And the report renders it without panicking on a real record's
    // shape, which is the other half of the contract.
    let md = nitro_bench::report::markdown(std::slice::from_ref(&rec));
    assert!(md.contains("boing-node"), "{md}");
    b.quit();
}
