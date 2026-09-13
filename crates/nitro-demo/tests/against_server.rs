//! The demo against a real `nitro-server`, on the fake backend, in
//! process.
//!
//! Same harness shape as `crates/nitro-server/tests/fake_loop.rs`: the
//! server's own `run(Config)` on a thread with a [`FakeBackend`] and a
//! [`FakeInput`], so there is no seat, no DRM device and no evdev node.
//! What is new here is the *client*: the binary's own [`App`] over a real
//! socket, with its real scene and its real latency bookkeeping. Nothing
//! is reimplemented for the test, which is the point — a copy of the demo
//! would drift from the demo.
//!
//! What it proves, in order:
//!
//! 1. The scene the demo builds is one the server accepts, and its pixels
//!    land where the client asked (`shot`).
//! 2. Synthetic pointer motion produces `PointerMotion`, the demo answers
//!    with a commit, and the commit comes back `Presented` — giving a real
//!    latency sample measured end to end.
//! 3. `--animate` commits exactly once per `Frame` callback.
//! 4. The damage outlines appear in a screenshot when they are on.
//! 5. `--windows N` opens N windows and the cascade separates them.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_demo::app::App;
use nitro_demo::args::{Args, Mode};
use nitro_demo::geom::FOLLOWER_SIZE;
use nitro_demo::scene::{Ids, WINDOW_SIZE};
use nitro_kms::Image;
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;

/// Wait for a condition, polling. Every wait here has a deadline: a test
/// that hangs tells you nothing.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    input: FakeInput,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, width: u32, height: u32) -> Self {
        let dir =
            std::env::temp_dir().join(format!("nitro-demo-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake { width, height };
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            input,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        h
    }

    /// The demo, connected to this server, with `args`.
    fn demo(&self, args: Args) -> App {
        let conn = Connection::connect(&self.wire_path, "nitro-demo").expect("wire connect");
        App::with_connection(conn, args).expect("demo start")
    }

    fn connect(&self) -> BufReader<UnixStream> {
        let s = UnixStream::connect(&self.path).expect("connect");
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

    /// Wait until the server has stopped reacting: no flip in flight and
    /// the frame counter still. Not "wait for N more frames" — an idle
    /// server deliberately stops flipping, so counting would hang.
    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let s = self.request_text("stats\n");
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

    /// Park the pointer far from the windows under test: the software
    /// cursor would otherwise contaminate a pixel comparison.
    fn park_cursor(&self, x: f64, y: f64) {
        self.input.push(InputEvent::PointerAbsolute {
            x,
            y,
            time_ns: 1_000_000,
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

/// Pump the demo until `f` is satisfied, or time out. Everything the
/// server sends goes through the demo's own `handle`, so the demo reacts
/// exactly as the binary would.
fn pump(app: &mut App, what: &str, mut f: impl FnMut(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    while !f(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.tick(Some(Duration::from_millis(20)), &mut events)
            .expect("tick");
    }
}

fn follow_args() -> Args {
    Args {
        mode: Mode::Follow,
        seconds: 0,
        ..Args::default()
    }
}

#[test]
fn the_demo_scene_is_accepted_and_painted_where_it_asked() {
    let (w, h) = (1280, 720);
    let harness = Harness::start("scene", w, h);
    harness.park_cursor(0.99, 0.99);
    let mut app = harness.demo(follow_args());

    pump(&mut app, "Configure", |a| a.windows[0].configured);
    pump(&mut app, "Presented", |a| a.presented > 0);
    // The first frame reached the screen, so the budget line has a number.
    assert!(app.first_presented.is_some());
    assert!(app.tx_bytes > 0, "the demo sent something");

    harness.settle();
    let img = harness.shot();

    // The window cascades to (0, 0), so window coordinates are device
    // coordinates. The gradient backdrop is dark at the top and lighter at
    // the bottom, and neither is the desktop's own background.
    let top = img.pixel(400, 4);
    let bottom = img.pixel(400, WINDOW_SIZE.h as u32 - 4);
    assert_ne!(top, bottom, "the backdrop gradient is flat");
    let blue = |p: u32| p & 0xFF;
    assert!(blue(bottom) > blue(top), "gradient runs the wrong way");

    // The first card is a solid blue rounded rect; sample its middle,
    // away from the corners the radius rounds off.
    let cards = nitro_demo::scene::cards(WINDOW_SIZE);
    let c = cards[0];
    let px = img.pixel((c.x + c.w / 2.0) as u32, (c.y + c.h / 2.0) as u32);
    assert_eq!(px & 0x00FF_FFFF, 0x0033_88FF, "card 0 is not its colour");

    // The stats agree that a client with a real tree is connected.
    let s = harness.request_text("stats\n");
    assert_eq!(stat(&s, "clients"), 1);
    assert_eq!(stat(&s, "windows"), 1);
    // Root + background + 4 cards + image + mover + trail + follower + 8 outlines.
    assert_eq!(stat(&s, "nodes"), 18);

    harness.quit();
}

#[test]
fn a_pointer_move_produces_a_latency_sample_measured_end_to_end() {
    let (w, h) = (1280, 720);
    let harness = Harness::start("latency", w, h);
    let mut app = harness.demo(follow_args());
    pump(&mut app, "Configure", |a| a.windows[0].configured);
    pump(&mut app, "the first Presented", |a| a.presented > 0);

    // Walk the pointer across the window. Absolute motion in the fake
    // backend's unit square; the window is at the origin, 800x500 of a
    // 1280x720 output, so these all land inside it.
    //
    // The timestamps must be *now*, not a synthetic constant: the whole
    // measurement is `Presented.time_ns - input.time_ns` against the
    // server's `CLOCK_MONOTONIC`, so an input stamped at t=10ms produces
    // a perfectly correct latency of "however long this machine has been
    // up". The server's own 200 ms input-stamp carry exists for the same
    // reason. Stamping from the same clock is what makes the number mean
    // anything, and this test is the place that proves the units line up.
    let commits_before = app.frames_committed;
    for i in 1..=20u64 {
        let t = i as f64 / 40.0;
        harness.input.push(InputEvent::PointerAbsolute {
            x: 0.1 + t * 0.3,
            y: 0.1 + t * 0.3,
            time_ns: nitro_demo::monotonic_ns(),
        });
        pump(&mut app, "the motion to be answered", |a| {
            a.frames_committed > commits_before && !a.hist.is_empty()
        });
        if !app.hist.is_empty() {
            break;
        }
    }

    // The whole deliverable: a client-side input-to-photon sample. One
    // second is absurdly generous for a 60 Hz fake backend under a test
    // runner, and still catches a clock or a unit mixed up.
    let summary = app.hist.summary().expect("no latency sample");
    assert!(summary.count >= 1);
    assert!(
        summary.max < 1_000_000,
        "{} us is not a latency",
        summary.max
    );
    assert!(summary.min <= summary.median && summary.median <= summary.max);

    // The follower moved to where the pointer is and the trail is behind it.
    assert!(
        !app.windows[0].follower.is_empty(),
        "the follower never moved"
    );
    assert!((app.windows[0].follower.w - FOLLOWER_SIZE).abs() < f32::EPSILON);

    // And the server agrees it saw input reach a frame.
    let s = harness.request_text("stats\n");
    assert!(stat(&s, "i2p_max_us") > 0, "server saw no i2p: {s:?}");

    harness.settle();
    let img = harness.shot();
    let f = app.windows[0].follower;
    // Sample the follower's *upper-left* quadrant, not its centre: the
    // follower is centred on the pointer and the software cursor is drawn
    // from the pointer down and to the right, so the centre pixel belongs
    // to the arrow. (The cursor is composited into the framebuffer on
    // purpose — a KMS cursor plane would not appear in a screenshot at
    // all — which is exactly why it lands in this assertion.)
    let px = img.pixel(f.x as u32 + 6, f.y as u32 + 6);
    assert_eq!(
        px & 0x00FF_FFFF,
        0x00FF_E040,
        "the follower is not on screen"
    );

    harness.quit();
}

#[test]
fn animate_commits_exactly_once_per_frame_callback() {
    let harness = Harness::start("animate", 1280, 720);
    harness.park_cursor(0.99, 0.99);
    let mut app = harness.demo(Args {
        mode: Mode::Animate,
        ..Args::default()
    });

    pump(&mut app, "ten frame callbacks", |a| a.frames_received >= 10);

    // Steady state is one commit per callback, but the *totals* are not
    // equal and never will be: the build transaction is a commit with no
    // callback behind it, and the `RequestFrame` riding on the newest
    // commit has not been answered yet. So the invariant to assert is on
    // the increments, not the totals — which is also the honest statement
    // of "never more than one commit per frame".
    let (c0, f0) = (app.frames_committed, app.frames_received);
    pump(&mut app, "ten more frame callbacks", |a| {
        a.frames_received >= f0 + 10
    });
    let (commits, callbacks) = (app.frames_committed - c0, app.frames_received - f0);
    assert_eq!(
        commits, callbacks,
        "{commits} commits for {callbacks} frame callbacks in steady state"
    );

    // And the demo's own pacing accounting says the same, which is what
    // the binary's WARNING is computed from: its mark is taken once the
    // animation is running, so the in-flight `RequestFrame` cancels.
    let (marked_commits, marked_callbacks) = app.pacing_since_mark();
    assert_eq!(
        marked_commits, marked_callbacks,
        "the demo's own pacing counter disagrees"
    );
    // And the rect actually moved.
    assert!(app.windows[0].phase.0 > 0.0, "the animation never advanced");
    assert!(app.pacing_line().contains("commits="));

    harness.quit();
}

#[test]
fn damage_outlines_are_drawn_when_they_are_on() {
    let harness = Harness::start("damage", 1280, 720);
    let mut app = harness.demo(Args {
        show_damage: true,
        ..Args::default()
    });
    pump(&mut app, "Configure", |a| a.windows[0].configured);

    // Two motions: the first has nothing to leave behind, the second
    // damages the rect it left and the one it arrived at.
    for (i, (x, y)) in [(0.15, 0.15), (0.35, 0.35)].into_iter().enumerate() {
        harness.input.push(InputEvent::PointerAbsolute {
            x,
            y,
            time_ns: 20_000_000 + i as u64 * 5_000_000,
        });
        let before = app.frames_committed;
        pump(&mut app, "the motion to be answered", |a| {
            a.frames_committed > before
        });
    }
    harness.settle();

    let img = harness.shot();
    // The outline is drawn around the follower's own rect, so its top edge
    // is the outline colour rather than the follower's yellow.
    let f = app.windows[0].follower;
    let edge = img.pixel((f.x + f.w / 2.0) as u32, f.y as u32 + 1);
    assert_eq!(
        edge & 0x00FF_FFFF,
        0x00FF_3030,
        "no damage outline at {f:?}"
    );

    harness.quit();
}

#[test]
fn several_windows_are_cascaded_and_each_gets_its_own_ids() {
    let harness = Harness::start("windows", 1280, 720);
    harness.park_cursor(0.99, 0.99);
    let mut app = harness.demo(Args {
        windows: 3,
        ..Args::default()
    });
    pump(&mut app, "every window configured", |a| {
        a.windows.iter().all(|w| w.configured)
    });

    let s = harness.request_text("stats\n");
    assert_eq!(stat(&s, "windows"), 3);
    assert_eq!(stat(&s, "nodes"), 3 * 18);

    // Ids do not collide: each window's root is `STRIDE` apart, and the
    // server accepted all three (a collision would have been a fatal
    // `Protocol` error closing the connection).
    for (i, w) in app.windows.iter().enumerate() {
        assert_eq!(w.ids, Ids::for_window(i as u32));
    }

    harness.settle();
    let img = harness.shot();
    // The cascade puts each window CASCADE_STEP px down and right of the
    // last, so window 2's card 3 is two steps off window 0's. Sampling at
    // the *cascaded* position is the test: at the un-shifted coordinate
    // there is only window 0's backdrop, which is exactly the bug a fixed
    // coordinate would hide.
    let step = nitro_server::clients::CASCADE_STEP * 2.0;
    let c = nitro_demo::scene::cards(WINDOW_SIZE)[3];
    let px = img.pixel(
        (c.x + c.w / 2.0 + step) as u32,
        (c.y + c.h / 2.0 + step) as u32,
    );
    assert_eq!(px & 0x00FF_FFFF, 0x00E0_3B8B, "card 3 of the top window");

    harness.quit();
}

/// A client that keeps its window but stops committing must leave the
/// server completely idle: this is the M1 headline property, seen from
/// the other end of the socket.
#[test]
fn an_idle_demo_leaves_the_server_flipping_nothing() {
    let harness = Harness::start("idle", 640, 480);
    harness.park_cursor(0.99, 0.99);
    let mut app = harness.demo(follow_args());
    pump(&mut app, "Presented", |a| a.presented > 0);
    harness.settle();

    let before = stat(&harness.request_text("stats\n"), "frames");
    std::thread::sleep(Duration::from_millis(250));
    let after = stat(&harness.request_text("stats\n"), "frames");
    assert_eq!(before, after, "an idle demo still costs frames");

    // And the demo itself is blocked, not spinning: a poll with a short
    // timeout returns having read nothing.
    let mut events: Vec<ServerMsg> = Vec::new();
    app.tick(Some(Duration::from_millis(50)), &mut events)
        .unwrap();
    assert!(events.is_empty(), "an idle server sent {events:?}");

    harness.quit();
}
