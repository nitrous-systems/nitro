//! Drive the whole server loop in-process on the fake backend: the v0
//! control socket, wire clients over `nitro-wire`, and synthetic input.
//!
//! These are the M1 acceptance tests. The server runs on a thread with a
//! [`nitro_kms::FakeBackend`] and a [`nitro_server::input::FakeInput`], so
//! there is no seat, no DRM device and no evdev node anywhere — and the
//! code exercised is the real event loop, not a stub of it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::time::{ClockId, Timespec, clock_gettime};

use nitro_core::{Color, IRect, Point, Rect, Size};
use nitro_kms::Image;
use nitro_server::cursor::CURSOR_SIZE;
use nitro_server::frame::FRAME_MARGIN_NS;
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::render::{FRAME, background_color};

/// The background colour at `(x, y)` as the `0x00RRGGBB` word a
/// screenshot pixel carries.
///
/// The palette is the default one, which is what a server with no
/// `server.conf` runs on — and every harness here is such a server. The
/// desktop gradient follows `theme.scheme` since M4-F, so a test that
/// named colours instead of asking would be asserting the scheme rather
/// than the compositing.
fn background_word(x: u32, y: u32, width: u32, height: u32) -> u32 {
    let px = background_color(x, y, width, height, &nitro_core::Palette::default());
    (u32::from(px.r) << 16) | (u32::from(px.g) << 8) | u32::from(px.b)
}
use nitro_server::{BackendKind, Config, run, wm};
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, ServerMsg};
use nitro_wire::types::{ButtonState, Layer, NodeId};

/// Wait for a condition, polling. Every wait in this file has a deadline:
/// a test that hangs tells you nothing.
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
    /// The output's device size, which absolute input is expressed in
    /// fractions of. `(0, 0)` until a headless server is plugged into.
    out: (u32, u32),
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, width: u32, height: u32) -> Self {
        Self::start_with(name, BackendKind::Fake { width, height })
    }

    /// A server with no output at all, which a test plugs one into later
    /// with the `plug` control request.
    fn start_headless(name: &str) -> Self {
        Self::start_with(name, BackendKind::FakeHeadless)
    }

    fn start_with(name: &str, backend: BackendKind) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = backend;
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let out = match config.backend {
            BackendKind::Fake { width, height } => (width, height),
            _ => (0, 0),
        };
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            input,
            out,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        h
    }

    fn connect(&self) -> BufReader<UnixStream> {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        BufReader::new(s)
    }

    fn client(&self, name: &str) -> Connection {
        Connection::connect(&self.wire_path, name).expect("wire connect")
    }

    /// Aim the pointer at `local`, a coordinate inside the *content* of the
    /// window `c` describes.
    ///
    /// An absolute device reports in the output's unit square, and since M3
    /// a window is wherever the window manager put it — decorated, and
    /// centred rather than at the origin. So a test that means "point at
    /// this spot in the window" has to go through the window's own
    /// `Configure`, which is the position of its content.
    fn point_at(&self, c: &Configure, local: Point, time_ns: u64) {
        let (w, h) = self.out;
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(c.position.x + local.x) / f64::from(w),
            y: f64::from(c.position.y + local.y) / f64::from(h),
            time_ns,
        });
    }

    /// Send a request whose reply is a bare status line with no body:
    /// `quit` and `plug`. `request_text` would block waiting for the blank
    /// line that terminates a *body*, and these have none.
    fn request_line(&self, req: &str) -> String {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
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

    fn shot(&self, name: Option<&str>) -> Result<Image, String> {
        let mut c = self.connect();
        let req = match name {
            Some(n) => format!("shot {n}\n"),
            None => "shot\n".to_owned(),
        };
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if let Some(msg) = header.strip_prefix("err ") {
            return Err(msg.to_owned());
        }
        let fields: Vec<u32> = header
            .strip_prefix("ok ")
            .expect("ok header")
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let (width, height, stride) = (fields[0], fields[1], fields[2]);
        let mut data = vec![0u8; (stride * height) as usize];
        c.read_exact(&mut data).unwrap();
        Ok(Image {
            width,
            height,
            stride,
            data,
        })
    }

    /// Frames the server has flipped so far.
    fn frames(&self) -> u64 {
        stat(&self.request_text("stats\n"), "frames")
    }

    /// Wait until the server has finished reacting to whatever we just
    /// did: no flip in flight and the frame counter has stopped moving.
    ///
    /// Not "wait for N more frames": the whole point of the design is that
    /// a server with nothing to do stops flipping entirely, so counting
    /// frames would hang the moment the thing under test has settled. A
    /// screenshot is honest as soon as the last commit went in — the front
    /// buffer is the most recently committed one, and the age-2 repaint
    /// region guarantees it is complete.
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

    fn quit(mut self) {
        let mut c = self.connect();
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        assert!(!self.path.exists(), "socket file removed on shutdown");
        assert!(!self.wire_path.exists(), "wire socket removed on shutdown");
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

/// Drain a client's socket until `f` matches, or time out. Anything else
/// that arrives is kept, so a later call can still see it.
///
/// Between polls it waits on the connection fd rather than sleeping a flat
/// interval, so an answer is picked up as soon as it lands. That matters
/// for the deferral tests, where the thing being measured *is* how quickly
/// the client answers.
fn expect<T>(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    what: &str,
    f: impl Fn(&ServerMsg) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = seen.iter().find_map(&f) {
            return found;
        }
        assert!(Instant::now() < deadline, "no {what}; got {seen:?}");
        conn.flush().unwrap();
        match conn.poll(seen) {
            Ok(_) => {}
            Err(e) => panic!("waiting for {what}: {e}"),
        }
        if seen.iter().any(|m| f(m).is_some()) {
            continue;
        }
        let fd = conn.as_fd();
        let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
        let _ = poll(
            &mut pfd,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 2_000_000,
            }),
        );
    }
}

/// `CLOCK_MONOTONIC` nanoseconds, the clock the server's frame deadlines
/// are expressed in.
fn monotonic_ns() -> u64 {
    let t = clock_gettime(ClockId::Monotonic);
    t.tv_sec.cast_unsigned() * 1_000_000_000 + t.tv_nsec.cast_unsigned()
}

/// Block until the server's frame clock has just rolled over, so a motion
/// injected next has most of a refresh period of deferral budget.
///
/// **Why a test needs this.** A held flip is bounded by
/// `frame::frame_deadline`: the next *extrapolated* vblank minus
/// [`FRAME_MARGIN_NS`]. The grid it extrapolates on is anchored at the last
/// real flip, so after a `settle()` the test sits at an arbitrary phase of
/// it — budgets measured across consecutive runs of the deferral test
/// ranged from 2.6 ms to 16.1 ms. Inject a motion with 2 ms left and the
/// client has 2 ms to answer; on a loaded box it does not, the deadline
/// fires, the cursor flips alone and the content takes a flip of its own.
/// That is the documented fallback, not a lost batch — but in a flip
/// *count* the two are indistinguishable, which is exactly what made
/// `a_client_that_answers_a_motion_rides_the_same_flip_as_the_cursor`
/// flaky (#532, #545: 9 flips instead of 8, ~1 in 7 under load).
///
/// **Why this is honest.** The property under test is "the client's answer
/// rides the cursor's flip", which presumes the client *can* answer inside
/// the budget. Gating on the phase fixes the budget rather than widening
/// the assertion, so a genuinely unbatched frame still fails.
///
/// `RequestFrame` on a quiescent server is answered off that same clock, so
/// `Frame.deadline_ns` *is* the budget a motion injected now would get, and
/// `deadline_ns + FRAME_MARGIN_NS` is the grid point it was derived from.
/// Ask; if the budget is already at least half a period, go; otherwise
/// sleep past that grid point and ask again.
fn at_frame_start(conn: &mut Connection, seen: &mut Vec<ServerMsg>, window: NodeId, serial: u32) {
    for attempt in 0..20 {
        conn.tx()
            .request_frame(window)
            .commit(serial + attempt)
            .unwrap();
        conn.flush().unwrap();
        let frame = expect(conn, seen, "Frame", |m| match m {
            ServerMsg::Frame(f) if f.window == window => Some(*f),
            _ => None,
        });
        seen.retain(|m| !matches!(m, ServerMsg::Frame(_)));
        let budget_ns = frame.deadline_ns.saturating_sub(monotonic_ns());
        if budget_ns >= u64::from(frame.refresh_ns) / 2 {
            return;
        }
        // Sleep past the grid point the deadline was derived from; the
        // next ask then starts a fresh period. A wakeup can be late but
        // never early, so the loop converges from below.
        let vblank_ns = frame.deadline_ns + FRAME_MARGIN_NS;
        while monotonic_ns() <= vblank_ns {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    panic!("never caught the start of a frame period");
}

/// A window with one solid rect filling it, committed. Returns the ids.
struct Window {
    root: NodeId,
    rect: NodeId,
}

fn make_window(conn: &mut Connection, root: u32, size: Size, color: Color, serial: u32) -> Window {
    let root = NodeId(root);
    let rect = NodeId(root.raw() + 1);
    conn.tx()
        .create_window(root, "test", size, Layer::Normal)
        .create_rect(rect, root, Rect::new(0.0, 0.0, size.w, size.h))
        .fill_solid(rect, color)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    Window { root, rect }
}

/// Where the window manager puts window number `index` of content `size`
/// on an unoccupied output of `out` device pixels at scale 1: the frame
/// rect and the content origin the client is told about.
///
/// This is the M3 placement model spelled out. The server wraps a window
/// in a frame it owns (a title bar and a border), places that *frame* by a
/// centred cascade in the output's work area, and reports the *content*
/// origin in `Configure.position`. Everything goes through `wm::place`
/// rather than re-deriving the policy: a test that reimplemented it would
/// only be asserting that two copies of the same arithmetic agree.
fn placement(index: u32, size: Size, out: (u32, u32)) -> (Rect, Point) {
    // At scale 1 the work area is the whole output; M3-A subtracts nothing
    // from it yet.
    let area = Rect::new(0.0, 0.0, out.0 as f32, out.1 as f32);
    let inset = wm::frame_insets();
    let frame = Size::new(size.w + inset.width(), size.h + inset.height());
    let at = wm::place(index, frame, area);
    (
        Rect::new(at.x, at.y, frame.w, frame.h),
        Point::new(at.x + inset.left, at.y + inset.top),
    )
}

/// The centre of `area` a frame of `size` would sit at, before any cascade
/// step: the position `wm::place` gives the very first window.
///
/// Rounded, because placement is: a window on a half pixel puts every edge
/// and every glyph in it between device pixels.
fn centred(size: Size, out: (u32, u32)) -> Point {
    Point::new(
        ((out.0 as f32 - size.w) / 2.0).round(),
        ((out.1 as f32 - size.h) / 2.0).round(),
    )
}

/// One cascade step for a window of content `size` on `out`, measured off
/// `wm::place` rather than named.
///
/// The step is private to the window manager, so a test that hard-coded
/// its current value would be pinning a coincidence rather than the
/// policy. Measuring it keeps the assertion "consecutive windows step down
/// and right by whatever the cascade's step is", which is the invariant
/// that actually matters.
fn cascade_step(size: Size, out: (u32, u32)) -> (f32, f32) {
    let (first, second) = (placement(0, size, out).0, placement(1, size, out).0);
    (second.x - first.x, second.y - first.y)
}

/// The cursor sits at the centre of the output and would otherwise
/// contaminate every pixel comparison; park it in a corner far from the
/// windows under test.
fn park_cursor(h: &Harness, x: f64, y: f64) {
    h.input.push(InputEvent::PointerAbsolute {
        x,
        y,
        time_ns: 1_000_000,
    });
}

#[test]
fn outputs_shot_stats_quit_on_fake_backend() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("static", w, h);

    assert_eq!(
        h_.request_text("outputs\n"),
        [
            "ok",
            // Scale, desktop-space origin and the primary flag are part of
            // the line since the configuration file could set all three;
            // with no file they are the defaults, and this pins them.
            &format!("Virtual-1 {w}x{h}@60000 scale=1 pos=0,0 primary=1")
        ]
    );

    wait_for("the first flip", || h_.frames() >= 1);
    park_cursor(&h_, 0.99, 0.99);
    h_.settle();

    let img = h_.shot(None).unwrap();
    assert_eq!((img.width, img.height, img.stride), (w, h, w * 4));
    // An empty desktop is the background everywhere the cursor is not.
    let cursor_area = IRect::new(
        w.cast_signed() - CURSOR_SIZE - 2,
        h.cast_signed() - CURSOR_SIZE - 2,
        CURSOR_SIZE + 4,
        CURSOR_SIZE + 4,
    );
    for y in 0..h {
        for x in 0..w {
            if cursor_area.contains(x.cast_signed(), y.cast_signed()) {
                continue;
            }
            assert_eq!(img.pixel(x, y), background_word(x, y, w, h), "({x},{y})");
        }
    }

    assert_eq!(h_.shot(Some("Virtual-1")).unwrap().width, w);
    assert_eq!(
        h_.shot(Some("HDMI-A-9")),
        Err("no output named HDMI-A-9".to_owned())
    );
    let mut c = h_.connect();
    c.get_mut().write_all(b"bogus\n").unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert_eq!(line, "err unknown request `bogus`\n");

    // Nothing is happening: the server stops committing entirely. That is
    // the zero-wakeup idle state, and it is the property the whole design
    // exists for.
    let before = h_.frames();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(h_.frames(), before, "an idle server keeps flipping");

    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "flips_pending"), 0);
    assert_eq!(stat(&s, "clients"), 0);
    assert_eq!(stat(&s, "windows"), 0);
    assert_eq!(stat(&s, "nodes"), 0);

    h_.quit();
}

/// `first_frame_ms` is how long the panel showed someone else's picture
/// before ours: it stays `0` while there is nothing to paint on, is set by
/// the first commit, and is not moved by any later one. The DRM backend's
/// deferred first modeset (`nitro-kms` README, "The first picture is a
/// finished frame") makes that commit the moment the panel changes hands,
/// which is what the number is for on the box.
#[test]
fn first_frame_ms_is_set_once_by_the_first_commit() {
    let h_ = Harness::start_headless("first-frame");
    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "first_frame_ms"), 0, "no output, so no frame yet");

    // Long enough that a first frame counted from the right origin cannot
    // round down to zero.
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(h_.request_line("plug 200x120\n"), "ok");
    wait_for("the first flip", || h_.frames() >= 1);
    let s = h_.request_text("stats\n");
    let first = stat(&s, "first_frame_ms");
    assert!(first >= 30, "counted from run(), before the plug: {first}");
    assert!(first <= stat(&s, "uptime_ms"));

    // More frames, same number.
    let frames = h_.frames();
    park_cursor(&h_, 0.5, 0.5);
    h_.settle();
    wait_for("another flip", || h_.frames() > frames);
    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "first_frame_ms"), first);

    h_.quit();
}

#[test]
fn a_client_window_is_configured_presented_and_painted_where_the_wm_put_it() {
    // M3 moved placement out from under this test twice over: the server
    // now wraps the window in a frame it owns, and puts that frame in the
    // *centre* of the work area rather than at the output's origin. So the
    // assertion is no longer "the pixels are at (0, 0)" but "the pixels are
    // where the server said they are" — `Configure.position` is the content
    // origin, and every coordinate below is relative to it. Placement
    // policy itself is checked against `wm::place`, once.
    let (w, h) = (320, 200);
    let h_ = Harness::start("client", w, h);
    park_cursor(&h_, 0.99, 0.99);

    let mut conn = h_.client("test");
    let mut seen = Vec::new();
    let size = Size::new(100.0, 60.0);
    let win = make_window(&mut conn, 1, size, Color::rgb(0xFF, 0x40, 0x40), 7);

    // The server answers with the size, scale and output it gave us.
    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    assert_eq!(configure.size, size);
    let (frame, content) = placement(0, size, (w, h));
    assert_eq!(
        configure.position, content,
        "the position is the content origin: the frame's origin plus the insets"
    );
    assert_eq!(
        frame.origin(),
        centred(frame.size(), (w, h)),
        "the first window's frame is centred in the work area"
    );
    assert!((configure.scale - 1.0).abs() < f32::EPSILON);

    // And reports the commit as presented once the frame lands.
    let presented = expect(&mut conn, &mut seen, "Presented", |m| match m {
        ServerMsg::Presented(p) => Some(*p),
        _ => None,
    });
    assert_eq!(presented.serial, 7);
    assert!(presented.time_ns > 0);

    h_.settle();
    let img = h_.shot(None).unwrap();
    // Inside the *content* is the client's rect; just outside it is either
    // the server's own frame or the desktop, and the frame is the server's
    // business, so only the content and the desktop beyond the frame are
    // asserted on here.
    let at = |dx: f32, dy: f32| ((content.x + dx) as u32, (content.y + dy) as u32);
    for (dx, dy) in [(10.0, 10.0), (size.w - 1.0, size.h - 1.0)] {
        let (x, y) = at(dx, dy);
        assert_eq!(
            img.pixel(x, y),
            0x00FF_4040,
            "({x},{y}) should be the client's rect"
        );
    }
    for (dx, dy) in [
        (size.w + wm::BORDER + 1.0, 30.0),
        (30.0, size.h + wm::BORDER + 1.0),
    ] {
        let (x, y) = at(dx, dy);
        assert_eq!(
            img.pixel(x, y),
            background_word(x, y, w, h),
            "({x},{y}) is beyond the frame, so it should be the desktop"
        );
    }

    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "clients"), 1);
    assert_eq!(stat(&s, "windows"), 1);
    assert_eq!(stat(&s, "decorated"), 1, "the window is server-decorated");
    // The client owns exactly two nodes (its content group and the rect);
    // the rest of the count is the frame the server drew around them, which
    // is why this is no longer the flat `2` of M1.
    let client_nodes = 2;
    assert!(
        stat(&s, "nodes") > client_nodes,
        "a decorated window has the client's nodes plus the server's: {s:?}"
    );
    assert!(stat(&s, "paint_us_max") > 0, "{s:?}");
    assert!(stat(&s, "damage_px_mean") > 0, "{s:?}");

    h_.quit();
}

/// The placement *policy*, which used to be half of the test above: the
/// cascade steps down and right from the centred first window, never off
/// screen, and each window keeps its own pixels where the next does not
/// cover them.
#[test]
fn a_second_window_cascades_off_the_first_and_stays_on_screen() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("cascade", w, h);
    park_cursor(&h_, 0.99, 0.99);

    let mut first = h_.client("first");
    let mut seen = Vec::new();
    let size = Size::new(100.0, 60.0);
    let win = make_window(&mut first, 1, size, Color::rgb(0xFF, 0x40, 0x40), 1);
    let configure = expect(&mut first, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    let content = placement(0, size, (w, h)).1;
    assert_eq!(configure.position, content);

    let mut other = h_.client("second");
    let mut seen2 = Vec::new();
    let size2 = Size::new(80.0, 50.0);
    let win2 = make_window(&mut other, 10, size2, Color::rgb(0x40, 0xFF, 0x40), 1);
    let configure2 = expect(&mut other, &mut seen2, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win2.root => Some(*c),
        _ => None,
    });
    let (frame2, content2) = placement(1, size2, (w, h));
    assert_eq!(configure2.position, content2);

    let centre2 = centred(frame2.size(), (w, h));
    let step2 = cascade_step(size2, (w, h));
    assert!(
        step2.0 > 0.0 && step2.1 > 0.0,
        "the cascade must actually step here, or the assertion below is vacuous: {step2:?}"
    );
    assert_eq!(
        (frame2.x - centre2.x, frame2.y - centre2.y),
        step2,
        "the second window steps down and right of the centred first one"
    );
    assert!(
        frame2.x >= 0.0
            && frame2.y >= 0.0
            && frame2.right() <= w as f32
            && frame2.bottom() <= h as f32,
        "a placed window stays inside the output: {frame2:?}"
    );

    h_.settle();
    let img = h_.shot(None).unwrap();
    assert_eq!(
        img.pixel((content2.x + 5.0) as u32, (content2.y + 5.0) as u32),
        0x0040_FF40
    );
    // The first window is still visible where the second does not cover it:
    // its own top-left content corner is above and left of the second's
    // frame, because the cascade only ever steps down and right.
    assert_eq!(
        img.pixel((content.x + 1.0) as u32, (content.y + 1.0) as u32),
        0x00FF_4040
    );

    h_.quit();
}

#[test]
fn pointer_motion_enters_the_window_and_reports_local_coordinates() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("pointer", w, h);
    let mut conn = h_.client("pointer");
    let mut seen = Vec::new();
    let win = make_window(
        &mut conn,
        1,
        Size::new(100.0, 60.0),
        Color::rgb(0, 0, 0xFF),
        1,
    );
    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });

    // Start outside the window, then move inside it. The coordinates a
    // client is told are relative to its *content* group, which decoration
    // does not move — so the assertions below are unchanged, but the
    // absolute point aimed at has to go through `Configure.position`.
    park_cursor(&h_, 0.9, 0.9);
    h_.point_at(&configure, Point::new(40.0, 25.0), 2_000_000);
    let enter = expect(&mut conn, &mut seen, "PointerEnter", |m| match m {
        ServerMsg::PointerEnter(e) => Some(*e),
        _ => None,
    });
    assert_eq!(enter.window, win.root);
    assert_eq!(enter.node, win.rect, "the rect is what the pointer is over");
    assert_eq!(enter.pos, Point::new(40.0, 25.0));

    // Another move inside is a motion, not a second enter.
    h_.point_at(&configure, Point::new(60.0, 30.0), 3_000_000);
    let motion = expect(&mut conn, &mut seen, "PointerMotion", |m| match m {
        ServerMsg::PointerMotion(m) => Some(*m),
        _ => None,
    });
    assert_eq!(motion.window, win.root);
    assert_eq!(motion.pos, Point::new(60.0, 30.0));

    // Leaving sends PointerLeave.
    h_.input.push(InputEvent::PointerAbsolute {
        x: 0.95,
        y: 0.95,
        time_ns: 4_000_000,
    });
    let leave = expect(&mut conn, &mut seen, "PointerLeave", |m| match m {
        ServerMsg::PointerLeave(l) => Some(*l),
        _ => None,
    });
    assert_eq!(leave.window, win.root);

    // The cursor is drawn where it was put: a screenshot shows it.
    h_.settle();
    let img = h_.shot(None).unwrap();
    let (cx, cy) = ((0.95 * f64::from(w)) as u32, (0.95 * f64::from(h)) as u32);
    assert_ne!(
        img.pixel(cx, cy),
        background_word(cx, cy, w, h),
        "the software cursor must be in the screenshot"
    );

    // And the latency loop closed: an input that produced a frame has an
    // input-to-photon sample.
    let s = h_.request_text("stats\n");
    assert!(stat(&s, "i2p_max_us") > 0, "{s:?}");

    h_.quit();
}

#[test]
fn a_click_focuses_and_raises_over_another_window() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("focus", w, h);

    let mut first = h_.client("first");
    let mut seen1 = Vec::new();
    // Small enough that the centred cascade has room for a step: on a
    // 320x200 output a 150x120 frame is already so close to the edges that
    // `wm::place` clamps every window onto the same spot, and then there is
    // no overlap to raise anything over.
    let size = Size::new(100.0, 60.0);
    let w1 = make_window(&mut first, 1, size, Color::rgb(0xFF, 0, 0), 1);
    let c1 = expect(&mut first, &mut seen1, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == w1.root => Some(*c),
        _ => None,
    });
    assert_eq!(c1.position, placement(0, size, (w, h)).1);

    let mut second = h_.client("second");
    let mut seen2 = Vec::new();
    let w2 = make_window(&mut second, 1, size, Color::rgb(0, 0xFF, 0), 1);
    let c2 = expect(&mut second, &mut seen2, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == w2.root => Some(*c),
        _ => None,
    });
    // Since M3 the cascade is centred rather than run from the origin, so
    // "one step down and right" is a statement about the *difference*
    // between the two positions, not about either one's absolute value.
    let step = cascade_step(size, (w, h));
    assert!(
        step.0 > 0.0 && step.1 > 0.0,
        "the cascade must actually step here, or there is no overlap to raise: {step:?}"
    );
    assert_eq!(
        (c2.position.x - c1.position.x, c2.position.y - c1.position.y),
        step,
        "the second window is one cascade step down and to the right"
    );

    // The second window cascaded over the first; where they overlap, the
    // newest is on top. The overlap is inside both windows' *content*,
    // which is what the clients painted.
    h_.settle();
    let overlap = ((c2.position.x + 10.0) as u32, (c2.position.y + 10.0) as u32);
    let img = h_.shot(None).unwrap();
    assert_eq!(img.pixel(overlap.0, overlap.1), 0x0000_FF00);

    // Click on the part of the first window the second does not cover: the
    // top-left of its content, a step above and left of the second window.
    let local = Point::new(10.0, 10.0);
    h_.point_at(&c1, local, 5_000_000);
    h_.input.push(InputEvent::PointerButton {
        button: nitro_server::input::BTN_LEFT,
        state: ButtonState::Pressed,
        time_ns: 6_000_000,
    });

    let focus = expect(&mut first, &mut seen1, "Focus", |m| match m {
        ServerMsg::Focus(f) if f.focused => Some(*f),
        _ => None,
    });
    assert_eq!(focus.window, w1.root);
    let button = expect(&mut first, &mut seen1, "PointerButton", |m| match m {
        ServerMsg::PointerButton(b) => Some(*b),
        _ => None,
    });
    assert_eq!(button.state, ButtonState::Pressed);

    // And the click raised it: the overlap is now the first window's red.
    h_.settle();
    let img = h_.shot(None).unwrap();
    assert_eq!(
        img.pixel(overlap.0, overlap.1),
        0x00FF_0000,
        "the clicked window must be on top"
    );

    h_.quit();
}

#[test]
fn disconnecting_destroys_everything_the_client_owned_and_repaints() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("disconnect", w, h);
    park_cursor(&h_, 0.99, 0.99);

    let mut conn = h_.client("doomed");
    let mut seen = Vec::new();
    let size = Size::new(120.0, 80.0);
    let win = make_window(&mut conn, 1, size, Color::rgb(0xFF, 0, 0xFF), 1);
    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    h_.settle();
    let at = |dx: f32, dy: f32| {
        (
            (configure.position.x + dx) as u32,
            (configure.position.y + dy) as u32,
        )
    };
    let middle = at(50.0, 40.0);
    assert_eq!(
        h_.shot(None).unwrap().pixel(middle.0, middle.1),
        0x00FF_00FF
    );
    // The client owns its content group and one rect; the count also holds
    // the frame the server drew, which is not the client's to destroy.
    let with_client = stat(&h_.request_text("stats\n"), "nodes");
    assert!(with_client >= 2);

    drop(conn);
    wait_for("the client to be reaped", || {
        stat(&h_.request_text("stats\n"), "clients") == 0
    });
    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "windows"), 0);
    assert_eq!(
        stat(&s, "nodes"),
        0,
        "the client's nodes *and* the frame the server hung on them are gone"
    );
    assert_eq!(stat(&s, "decorated"), 0);

    // The area it covered is repainted with the desktop underneath.
    h_.settle();
    let img = h_.shot(None).unwrap();
    for (dx, dy) in [(50.0, 40.0), (10.0, 10.0), (size.w - 1.0, size.h - 1.0)] {
        let (px, py) = at(dx, dy);
        assert_eq!(
            img.pixel(px, py),
            background_word(px, py, w, h),
            "({px},{py}) still holds the dead client's pixels"
        );
    }

    h_.quit();
}

#[test]
fn a_bad_message_gets_an_error_and_a_disconnect() {
    let h_ = Harness::start("bad", 200, 120);
    let mut conn = h_.client("naughty");
    let mut seen = Vec::new();

    // A node whose parent does not exist: the decoder cannot catch this,
    // only the server's id map can.
    conn.tx()
        .create_rect(NodeId(9), NodeId(404), Rect::new(0.0, 0.0, 10.0, 10.0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();

    let err = expect(&mut conn, &mut seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some(e.clone()),
        _ => None,
    });
    assert_eq!(err.serial, 1);
    assert_eq!(err.code, nitro_wire::types::ErrorCode::UnknownNode);

    // And the connection is closed.
    wait_for("the connection to close", || {
        let mut out = Vec::new();
        matches!(conn.poll(&mut out), Err(nitro_wire::error::Error::Closed))
    });
    wait_for("the client to be reaped", || {
        stat(&h_.request_text("stats\n"), "clients") == 0
    });

    h_.quit();
}

#[test]
fn a_buffer_is_mapped_from_the_memfd_and_blitted() {
    use nitro_wire::msg::CreateBuffer;
    use nitro_wire::types::{BufferId, format};

    let (w, h) = (200, 120);
    let h_ = Harness::start("buffer", w, h);
    park_cursor(&h_, 0.99, 0.99);
    let mut conn = h_.client("images");
    let mut seen = Vec::new();

    // A 16x16 buffer of one solid colour, written through the fd. Sealed,
    // because the server maps it and refuses anything it cannot prove
    // will not shrink under the mapping (#569).
    let (bw, bh, stride) = (16u32, 16u32, 16u32 * 4);
    let pixels: Vec<u8> = (0..(stride * bh))
        .map(|i| match i % 4 {
            0 => 0x20, // B
            1 => 0xC0, // G
            2 => 0x80, // R
            _ => 0xFF, // A (unused for XR24)
        })
        .collect();
    let fd = nitro_shm::memfd_with("nitro-test-buffer", &pixels).unwrap();

    let root = NodeId(1);
    let image = NodeId(2);
    conn.tx()
        .create_window(root, "img", Size::new(64.0, 64.0), Layer::Normal)
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: bw,
            height: bh,
            stride,
            format: format::XR24,
            size: stride * bh,
            fd,
        })
        .create_image(image, root, Rect::new(0.0, 0.0, 32.0, 32.0))
        .image(image, BufferId(1), IRect::new(0, 0, 16, 16))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });

    h_.settle();
    let img = h_.shot(None).unwrap();
    // The image node sits at the content's origin, wherever the window
    // manager put the window; the desktop is only visible *outside the
    // frame*, since the server's own frame background fills the rest of it.
    let (x, y) = (
        (configure.position.x + 16.0) as u32,
        (configure.position.y + 16.0) as u32,
    );
    assert_eq!(img.pixel(x, y), 0x0080_C020, "the buffer's pixels");
    // The image node is 32x32 in a 64x64 window, so the rest of the content
    // is not the buffer. It is no longer the *desktop* either: the server's
    // frame background is behind the client's nodes now, so the honest
    // assertion is "not the buffer's pixels" inside the window, and "the
    // desktop" only beyond the frame.
    let (ix, iy) = (
        (configure.position.x + 40.0) as u32,
        (configure.position.y + 40.0) as u32,
    );
    assert_ne!(img.pixel(ix, iy), 0x0080_C020, "outside the image node");
    let (ox, oy) = (
        (configure.position.x + configure.size.w + wm::BORDER + 2.0) as u32,
        (configure.position.y + configure.size.h + wm::BORDER + 2.0) as u32,
    );
    assert_eq!(
        img.pixel(ox, oy),
        background_word(ox, oy, w, h),
        "outside the window's frame"
    );

    h_.quit();
}

#[test]
fn many_control_clients_and_partial_lines() {
    let h_ = Harness::start("clients", 64, 64);
    let mut conns: Vec<_> = (0..8).map(|_| h_.connect()).collect();
    for c in &mut conns {
        c.get_mut().write_all(b"out").unwrap();
    }
    for c in &mut conns {
        c.get_mut().write_all(b"puts\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
    }
    // Two requests on one connection.
    let mut c = h_.connect();
    c.get_mut().write_all(b"stats\nstats\n").unwrap();
    let mut blank = 0;
    let mut line = String::new();
    while blank < 2 {
        line.clear();
        assert!(c.read_line(&mut line).unwrap() > 0);
        if line == "\n" {
            blank += 1;
        }
    }
    drop(conns);
    h_.quit();
}

#[test]
fn the_desktop_frame_is_still_painted_under_everything() {
    // The M0 frame is the cheapest end-to-end check that the background
    // path runs at all: it is the only thing on an empty desktop whose
    // colour differs from its neighbours' by construction.
    let (w, h) = (128, 96);
    let h_ = Harness::start("frame", w, h);
    park_cursor(&h_, 0.5, 0.5);
    h_.settle();
    let img = h_.shot(None).unwrap();
    assert_eq!(img.pixel(0, 0), background_word(0, 0, w, h));
    assert_eq!(
        img.pixel(FRAME - 1, 50),
        background_word(FRAME - 1, 50, w, h)
    );
    assert_ne!(img.pixel(0, 50), img.pixel(FRAME + 1, 50));
    h_.quit();
}

#[test]
fn cycling_buffers_does_not_leak_the_clients_mappings() {
    use nitro_wire::msg::CreateBuffer;
    use nitro_wire::types::{BufferId, format};

    // A client that pushes changing pixels does create → damage → destroy
    // once per frame. Since #569 the server *maps* each buffer instead of
    // copying it, so what must be given back is the mapping rather than a
    // descriptor: `DestroyBuffer` drops the scene's buffer, whose store's
    // `Drop` is the `munmap`. Without that the server leaks an 8 KiB
    // mapping per frame until it runs out of address space or
    // `vm.max_map_count`.
    //
    // The *descriptor* side of this is now trivially safe — `Mapping::map`
    // closes the client's fd the moment the pages are mapped, so the
    // server holds none to leak — and counting fds would therefore assert
    // nothing. Counting `/proc/self/maps` entries by memfd name asserts
    // the thing that can still go wrong, and for the same reason the fd
    // count was chosen over a process-wide total: a sibling test's server
    // cannot move it.
    let h_ = Harness::start("bufcycle", 200, 120);
    let mut conn = h_.client("cycler");
    let mut seen = Vec::new();

    let root = NodeId(1);
    let image = NodeId(2);
    conn.tx()
        .create_window(root, "cycle", Size::new(64.0, 64.0), Layer::Normal)
        .create_image(image, root, Rect::new(0.0, 0.0, 32.0, 32.0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });

    let before = mapped_buffers();
    let (bw, bh, stride) = (8u32, 8u32, 8u32 * 4);
    for i in 0..64u32 {
        let id = BufferId(i + 1);
        let fd =
            nitro_shm::create_sealed(BUFFER_MEMFD_NAME, u64::from(stride) * u64::from(bh)).unwrap();
        conn.tx()
            .create_buffer(CreateBuffer {
                id,
                width: bw,
                height: bh,
                stride,
                format: format::XR24,
                size: stride * bh,
                fd,
            })
            .image(image, id, IRect::new(0, 0, 8, 8))
            .buffer_damage(id, vec![IRect::new(0, 0, 8, 8)])
            .commit(i + 2)
            .unwrap();
        conn.flush().unwrap();
        // Detach and release it, exactly as a client reusing a slot would.
        conn.tx()
            .image(image, BufferId::NONE, IRect::new(0, 0, 8, 8))
            .destroy_buffer(id)
            .commit(i + 200)
            .unwrap();
        conn.flush().unwrap();
        // Let the server work through it before queueing the next one.
        h_.request_text("stats\n");
    }
    h_.settle();

    let after = mapped_buffers();
    assert!(
        after <= before + 4,
        "leaked buffer mappings over 64 cycles: {before} -> {after}"
    );
    h_.quit();
}

/// How many of *this test's* buffer memfds the process has mapped.
///
/// The server runs on a thread of this same process, so its leaks are
/// ours to see — but so is every other test's, and the tests in this file
/// run in parallel by default. Anything process-wide (a count of
/// `/proc/self/maps` lines, or of `/proc/self/fd`) therefore measures
/// every sibling server starting up as well, and this test would report a
/// leak that is somebody else's server doing its job. That failure was
/// observed at roughly one run in five on the fd version of this test,
/// and it never reproduced single-threaded, which is the signature of
/// exactly this.
///
/// So count the thing under test. A mapping of a memfd shows up in
/// `/proc/self/maps` as `/memfd:nitro-cycle (deleted)`, carrying the name
/// the memfd was created with, which is immune to anything another test is
/// doing and is also sharper: a leak of 64 mappings shows up as 64 rather
/// than diluted into a process-wide total.
fn mapped_buffers() -> usize {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return 0;
    };
    maps.lines()
        .filter(|l| l.contains(&format!("/memfd:{BUFFER_MEMFD_NAME}")))
        .count()
}

/// The name `cycling_buffers_does_not_leak_the_clients_mappings` gives
/// its memfds, and the string `mapped_buffers` recognises them by.
const BUFFER_MEMFD_NAME: &str = "nitro-cycle";

/// An unsealed `CreateBuffer` is refused and the client is disconnected.
///
/// The protocol-level half of #569's no-fallback rule: the server maps the
/// descriptor, so it must be able to prove the file cannot shrink under
/// the mapping, and a client that did not seal gets `BadBuffer` rather
/// than a silent `pread` path nobody exercises.
#[test]
fn an_unsealed_buffer_is_refused_and_disconnects() {
    use nitro_wire::msg::CreateBuffer;
    use nitro_wire::types::{BufferId, ErrorCode, format};
    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

    let h_ = Harness::start("unsealed", 200, 120);
    let mut conn = h_.client("hostile");
    let mut seen = Vec::new();

    let root = NodeId(1);
    let image = NodeId(2);
    conn.tx()
        .create_window(root, "unsealed", Size::new(64.0, 64.0), Layer::Normal)
        .create_image(image, root, Rect::new(0.0, 0.0, 32.0, 32.0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });

    let (bw, bh, stride) = (8u32, 8u32, 8u32 * 4);
    let fd = memfd_create("nitro-unsealed", MemfdFlags::CLOEXEC).unwrap();
    ftruncate(&fd, u64::from(stride) * u64::from(bh)).unwrap();
    conn.tx()
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: bw,
            height: bh,
            stride,
            format: format::XR24,
            size: stride * bh,
            fd,
        })
        .commit(2)
        .unwrap();
    conn.flush().unwrap();

    let err = expect(&mut conn, &mut seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some(e.clone()),
        _ => None,
    });
    assert_eq!(err.code, ErrorCode::BadBuffer);
    assert!(
        err.msg.contains("F_SEAL_SHRINK"),
        "the error should name the missing seal: {:?}",
        err.msg
    );
    h_.quit();
}

#[test]
fn a_commit_that_changes_no_pixels_is_still_presented() {
    // `Presented` is the client's flow control. A transaction that damages
    // nothing — a hidden subtree, a no-op property write — still has to be
    // acknowledged, or a client that waits for it before sending the next
    // frame stalls for ever on a desktop where nothing else moves.
    let h_ = Harness::start("nodamage", 200, 120);
    let mut conn = h_.client("quiet");
    let mut seen = Vec::new();
    let win = make_window(
        &mut conn,
        1,
        Size::new(80.0, 50.0),
        Color::rgb(0, 0, 0xFF),
        1,
    );
    expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    expect(
        &mut conn,
        &mut seen,
        "Presented for serial 1",
        |m| match m {
            ServerMsg::Presented(p) if p.serial == 1 => Some(*p),
            _ => None,
        },
    );
    h_.settle();

    // Now commit something that cannot change a pixel: setting a property
    // to the value it already has.
    for serial in 2..6u32 {
        conn.tx().visible(win.rect, true).commit(serial).unwrap();
        conn.flush().unwrap();
        expect(
            &mut conn,
            &mut seen,
            "Presented for a no-op commit",
            |m| match m {
                ServerMsg::Presented(p) if p.serial == serial => Some(*p),
                _ => None,
            },
        );
    }
    h_.quit();
}

#[test]
fn a_frame_request_from_a_quiescent_desktop_is_answered() {
    // How a client starts an animation: ask for a frame, get a deadline,
    // draw for it. If the answer waited for a flip, and a flip waited for
    // damage, and the damage was going to be the client's response to the
    // answer, nothing would ever happen.
    let h_ = Harness::start("framereq", 200, 120);
    let mut conn = h_.client("animator");
    let mut seen = Vec::new();
    let win = make_window(
        &mut conn,
        1,
        Size::new(80.0, 50.0),
        Color::rgb(0, 0xFF, 0),
        1,
    );
    expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    // Let everything settle, so the next commit starts from a genuinely
    // idle server with no flip pending and nothing to paint.
    h_.settle();
    seen.clear();

    conn.tx().request_frame(win.root).commit(99).unwrap();
    conn.flush().unwrap();
    let frame = expect(&mut conn, &mut seen, "Frame", |m| match m {
        ServerMsg::Frame(f) => Some(*f),
        _ => None,
    });
    assert_eq!(frame.window, win.root);
    assert!(frame.refresh_ns > 0);
    assert!(
        frame.deadline_ns > 0,
        "the deadline comes from the extrapolated vblank clock"
    );
    h_.quit();
}

#[test]
fn a_window_created_before_any_output_is_placed_when_one_appears() {
    // The state a real server is in between starting and the connector
    // reporting a mode. A window created here is real and owns its nodes;
    // it simply has nowhere to be, and the client must still get its
    // `Configure` once somewhere exists — otherwise it waits for ever for
    // a size it will never be told.
    let h_ = Harness::start_headless("nooutput");
    assert_eq!(h_.request_text("outputs\n"), ["ok"]);

    let mut conn = h_.client("early");
    let mut seen = Vec::new();
    let win = make_window(
        &mut conn,
        1,
        Size::new(100.0, 60.0),
        Color::rgb(0xFF, 0, 0),
        1,
    );

    // Nothing to configure against yet, but the window exists.
    wait_for("the window to reach the scene", || {
        stat(&h_.request_text("stats\n"), "windows") == 1
    });

    // The commit still has to be acknowledged. Its pixels have nowhere to
    // appear, so no frame will ever carry the serial; holding it would
    // stall a client that waits for `Presented` before sending the next
    // transaction, and would grow the pending list for ever.
    let presented = expect(
        &mut conn,
        &mut seen,
        "Presented for serial 1",
        |m| match m {
            ServerMsg::Presented(p) if p.serial == 1 => Some(*p),
            _ => None,
        },
    );
    assert_eq!(presented.serial, 1);

    let mut out = Vec::new();
    let _ = conn.poll(&mut out);
    seen.extend(out);
    // Against `seen`, not just this last poll: `expect` above drained the
    // socket into `seen`, so a wrongly-sent `Configure` would have landed
    // there and a check of `out` alone would pass regardless.
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Configure(_))),
        "nothing to configure against: {seen:?}"
    );
    // From here on only messages *after* this point may be inspected for a
    // `Configure`; the post-hotplug `expect` below does exactly that.
    seen.clear();

    // Plug a screen in.
    assert_eq!(h_.request_line("plug 200x120\n"), "ok");
    wait_for("the output to appear", || {
        h_.request_text("outputs\n").len() > 1
    });

    let configure = expect(
        &mut conn,
        &mut seen,
        "Configure after the hotplug",
        |m| match m {
            ServerMsg::Configure(c) if c.window == win.root => Some(*c),
            _ => None,
        },
    );
    assert_eq!(configure.size, Size::new(100.0, 60.0));

    // And it is actually on screen, where the hotplug placed it: the first
    // window of the freshly-plugged output, so `Configure.position` is the
    // content origin of a centred, decorated frame.
    h_.settle();
    let img = h_.shot(None).unwrap();
    assert_eq!(
        configure.position,
        placement(0, configure.size, (200, 120)).1
    );
    let (x, y) = (
        (configure.position.x + 10.0) as u32,
        (configure.position.y + 10.0) as u32,
    );
    assert_eq!(img.pixel(x, y), 0x00FF_0000);
    h_.quit();
}

#[test]
fn no_cursor_is_drawn_until_a_pointer_device_reports_something() {
    // A keyboard-only machine should not show an arrow the user cannot
    // move. The cursor appears the first time a pointer event arrives,
    // not merely because an output exists.
    let (w, h) = (128, 96);
    let h_ = Harness::start("nopointer", w, h);
    h_.settle();

    let img = h_.shot(None).unwrap();
    for y in 0..h {
        for x in 0..w {
            assert_eq!(
                img.pixel(x, y),
                background_word(x, y, w, h),
                "({x},{y}) is not the bare desktop, so something drew a cursor"
            );
        }
    }

    // One motion, and it appears.
    h_.input.push(InputEvent::PointerAbsolute {
        x: 0.5,
        y: 0.5,
        time_ns: 1_000_000,
    });
    h_.settle();
    let img = h_.shot(None).unwrap();
    let (cx, cy) = (w / 2, h / 2);
    assert_ne!(
        img.pixel(cx, cy),
        background_word(cx, cy, w, h),
        "the cursor should be drawn once a pointer has reported"
    );
    h_.quit();
}

/// Whether the server found any font at all. Without one there is nothing
/// to shape and nothing to draw, and the text tests say so and stop rather
/// than failing on a box that simply has no fonts installed.
fn has_text(conn: &Connection) -> bool {
    use nitro_wire::types::caps;
    if conn.has_caps(caps::TEXT) {
        return true;
    }
    eprintln!("skipping: the server reports no TEXT capability (no fonts on this box)");
    false
}

#[test]
fn set_text_answers_with_metrics_and_puts_glyphs_on_screen() {
    use nitro_wire::msg::SetText;
    use nitro_wire::types::{Align, NodeKind};

    let (w, h) = (320, 200);
    let h_ = Harness::start("text", w, h);
    park_cursor(&h_, 0.99, 0.99);
    let mut conn = h_.client("text");
    if !has_text(&conn) {
        h_.quit();
        return;
    }
    let mut seen = Vec::new();

    // A window with a dark rect under a white label, so a glyph pixel is
    // unmistakably different from both the background and the rect.
    let root = NodeId(1);
    let panel = NodeId(2);
    let label = NodeId(3);
    let (bx, by, bw, bh) = (10.0f32, 10.0f32, 240.0f32, 40.0f32);
    conn.tx()
        .create_window(root, "text", Size::new(300.0, 120.0), Layer::Normal)
        .create_rect(panel, root, Rect::new(bx, by, bw, bh))
        .fill_solid(panel, Color::rgb(0x10, 0x10, 0x10))
        .create_node(label, NodeKind::Text, root)
        .bounds(label, Rect::new(bx, by + 6.0, bw, 28.0))
        .set_text_full(SetText {
            node: label,
            size_px: 20.0,
            weight: 400,
            italic: false,
            max_width: 0.0,
            wrap: false,
            align: Align::Left,
            color: Color::WHITE,
            family: "sans".to_owned(),
            text: "Hello".to_owned(),
        })
        .commit(1)
        .unwrap();
    conn.flush().unwrap();

    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    // The commit that shaped the node answers with its measured size.
    let metrics = expect(&mut conn, &mut seen, "TextMetrics", |m| match m {
        ServerMsg::TextMetrics(t) if t.node == label => Some(*t),
        _ => None,
    });
    assert!(metrics.width > 0.0, "a shaped label has a width");
    assert!(metrics.height > 0.0);
    assert!(metrics.ascent > 0.0);
    assert_eq!(metrics.line_count, 1);
    assert!(
        metrics.width <= bw,
        "\"Hello\" at 20 px fits in {bw} units: {}",
        metrics.width
    );

    h_.settle();
    let img = h_.shot(None).unwrap();
    // `bx`/`by` came from this window's own `Configure`, so they are
    // already the content's device origin whatever the window manager did
    // with the frame around it.
    assert_eq!(configure.size, Size::new(300.0, 120.0));
    let origin = (bx as u32, (by + 6.0) as u32);

    // Somewhere in the text's box there must be a pixel that is neither the
    // panel's colour nor the desktop behind it: that is a glyph.
    let mut lit = 0u32;
    for y in origin.1..origin.1 + 24 {
        for x in origin.0..origin.0 + metrics.width as u32 {
            let px = img.pixel(x, y);
            if px != 0x0010_1010 && px != background_word(x, y, w, h) {
                lit += 1;
            }
        }
    }
    assert!(
        lit > 20,
        "expected glyph pixels inside the label's bounds, found {lit}"
    );

    // And the atlas actually cached them.
    let stats = h_.request_text("stats\n");
    assert!(stat(&stats, "glyphs_cached") > 0, "{stats:?}");
    assert!(stat(&stats, "atlas_pages") >= 1, "{stats:?}");
    assert!(stat(&stats, "text_runs") >= 1, "{stats:?}");
    assert!(stat(&stats, "fonts") > 0, "{stats:?}");

    // The font db is lazy (#528) and hands its bytes back when the loop goes
    // idle, which is exactly the state a `stats` request observes: the glyphs
    // are cached, the face that drew them is not. `fonts` (faces *indexed*)
    // stays whatever the box has installed; `font_bytes` is what is resident.
    assert!(stat(&stats, "fonts") > 0, "{stats:?}");
    assert_eq!(
        stat(&stats, "font_bytes"),
        0,
        "a settled server holds no font bytes: {stats:?}"
    );
    assert_eq!(stat(&stats, "fonts_loaded"), 0, "{stats:?}");
    // The masks survive the release — that is what makes it free.
    assert!(stat(&stats, "glyphs_cached") > 0, "{stats:?}");

    h_.quit();
}

#[test]
fn measure_text_is_answered_before_any_commit() {
    use nitro_wire::msg::MeasureText;

    let h_ = Harness::start("measure", 160, 120);
    let mut conn = h_.client("measure");
    if !has_text(&conn) {
        h_.quit();
        return;
    }
    let mut seen = Vec::new();

    // No window, no node, no commit: just a question.
    conn.measure_text(MeasureText {
        request: 0x1234,
        size_px: 16.0,
        weight: 400,
        italic: false,
        max_width: 0.0,
        wrap: false,
        family: "sans".to_owned(),
        text: "Hello".to_owned(),
    })
    .unwrap();
    conn.flush().unwrap();

    let m = expect(&mut conn, &mut seen, "TextMeasured", |m| match m {
        ServerMsg::TextMeasured(t) if t.request == 0x1234 => Some(t.clone()),
        _ => None,
    });
    assert!(m.width > 0.0);
    assert!(m.height > 0.0);
    assert_eq!(m.line_count, 1);
    // One cursor position per cluster boundary plus the end of the string.
    assert!(
        m.cursor_x.len() >= 5,
        "\"Hello\" has five clusters: {:?}",
        m.cursor_x
    );
    assert_eq!(m.cursor_x[0].offset, 0);
    assert!(
        m.cursor_x.windows(2).all(|w| w[0].x <= w[1].x),
        "cursor x is monotonic: {:?}",
        m.cursor_x
    );
    // Nothing was committed, so the scene is still empty.
    assert_eq!(stat(&h_.request_text("stats\n"), "nodes"), 0);
    // And nothing was stored either: a measurement is not a run.
    assert_eq!(stat(&h_.request_text("stats\n"), "text_runs"), 0);

    h_.quit();
}

#[test]
fn a_wrapped_label_reports_several_lines_and_respects_its_width() {
    use nitro_wire::msg::MeasureText;

    let h_ = Harness::start("wrap", 160, 120);
    let mut conn = h_.client("wrap");
    if !has_text(&conn) {
        h_.quit();
        return;
    }
    let mut seen = Vec::new();

    conn.measure_text(MeasureText {
        request: 1,
        size_px: 14.0,
        weight: 400,
        italic: false,
        max_width: 80.0,
        wrap: true,
        family: "sans".to_owned(),
        text: "the quick brown fox jumps over the lazy dog".to_owned(),
    })
    .unwrap();
    conn.flush().unwrap();

    let m = expect(&mut conn, &mut seen, "TextMeasured", |m| match m {
        ServerMsg::TextMeasured(t) if t.request == 1 => Some(t.clone()),
        _ => None,
    });
    assert!(
        m.line_count >= 2,
        "wrapping produced {} line(s)",
        m.line_count
    );
    assert!(
        m.width <= 80.5,
        "no line exceeds the wrap width: {}",
        m.width
    );
    h_.quit();
}

#[test]
fn text_runs_are_released_with_their_node_and_their_client() {
    use nitro_wire::types::NodeKind;

    let h_ = Harness::start("textlife", 160, 120);
    let mut conn = h_.client("textlife");
    if !has_text(&conn) {
        h_.quit();
        return;
    }
    let mut seen = Vec::new();

    let root = NodeId(1);
    let label = NodeId(2);
    conn.tx()
        .create_window(root, "t", Size::new(120.0, 60.0), Layer::Normal)
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    // Since M3 the server shapes a run of its own per decorated window (the
    // title in its bar), so the absolute count is not the client's alone.
    // Measure the *delta* across the client's actions instead: that is what
    // this test is about, and it does not care how many runs the shell
    // happens to own.
    let base = stat(&h_.request_text("stats\n"), "text_runs");
    assert!(
        base >= 1,
        "a decorated window carries the server's title run"
    );

    conn.tx()
        .create_node(label, NodeKind::Text, root)
        .bounds(label, Rect::new(0.0, 0.0, 120.0, 20.0))
        .set_text(label, "sans", 14.0, Color::WHITE, "one")
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "TextMetrics", |m| match m {
        ServerMsg::TextMetrics(t) if t.node == label => Some(*t),
        _ => None,
    });
    assert_eq!(stat(&h_.request_text("stats\n"), "text_runs"), base + 1);

    // Re-setting the text replaces the run rather than accumulating one.
    for (serial, s) in [(3u32, "two"), (4, "three"), (5, "four")] {
        conn.tx()
            .set_text(label, "sans", 14.0, Color::WHITE, s)
            .commit(serial)
            .unwrap();
        conn.flush().unwrap();
        wait_for("the reshape", || {
            stat(&h_.request_text("stats\n"), "text_runs") == base + 1
        });
    }

    // Destroying the node frees its run, and only its run.
    conn.tx().destroy_node(label).commit(6).unwrap();
    conn.flush().unwrap();
    wait_for("the run to be released", || {
        stat(&h_.request_text("stats\n"), "text_runs") == base
    });

    // And a disconnect frees everything the client owned.
    conn.tx()
        .create_node(NodeId(3), NodeKind::Text, root)
        .bounds(NodeId(3), Rect::new(0.0, 0.0, 120.0, 20.0))
        .set_text(NodeId(3), "sans", 14.0, Color::WHITE, "gone soon")
        .commit(7)
        .unwrap();
    conn.flush().unwrap();
    wait_for("the second run", || {
        stat(&h_.request_text("stats\n"), "text_runs") == base + 1
    });
    drop(conn);
    // Everything goes, the server's title run included: the window it
    // titled died with the client.
    wait_for("the client to be reaped", || {
        stat(&h_.request_text("stats\n"), "text_runs") == 0
    });

    h_.quit();
}

// ------------------------------------------------------- deferred flips

/// One flip carries the cursor **and** the client's answer to the motion
/// that moved it — the whole point of issue #529.
///
/// Counted from the frame counter rather than from anything internal: an
/// isolated motion into a window whose client answers it produced *three*
/// flips before this change (the cursor's own two under the age-2 rule,
/// plus one for the content that arrived too late for them) and produces
/// two now, which is what a bare cursor costs anyway.
#[test]
fn a_client_that_answers_a_motion_rides_the_same_flip_as_the_cursor() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("defer-together", w, h);
    let mut conn = h_.client("answers");
    let mut seen = Vec::new();
    let win = make_window(
        &mut conn,
        1,
        Size::new(120.0, 80.0),
        Color::rgb(0, 0, 0xFF),
        1,
    );
    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });

    // Park the cursor inside the window and let everything settle, so the
    // motion under test is genuinely isolated: no flip in flight, no
    // damage owed to either buffer, and the client already entered. The
    // window is wherever the window manager put it, so aim at it through
    // its own `Configure`.
    h_.point_at(&configure, Point::new(20.0, 20.0), 1_000_000);
    expect(&mut conn, &mut seen, "PointerEnter", |m| match m {
        ServerMsg::PointerEnter(e) => Some(*e),
        _ => None,
    });
    conn.tx()
        .bounds(win.rect, Rect::new(0.0, 0.0, 120.0, 80.0))
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h_.settle();
    seen.clear();

    let before = h_.frames();
    for (serial, step) in (10..).zip(0..4u32) {
        // Start the motion at the top of a frame period. Without this the
        // deferral budget is whatever is left of the current one — as
        // little as the 2 ms margin — and a loaded box makes the client
        // miss it, which costs a flip and is what made this test flaky.
        // See `at_frame_start`.
        at_frame_start(&mut conn, &mut seen, win.root, 100 + step * 32);
        // One motion, and the client answers it the way `nitro-demo
        // --follow` does: a commit that moves a follower rect.
        h_.point_at(
            &configure,
            Point::new(30.0 + step as f32 * 8.0, 30.0),
            u64::from(2 + step) * 1_000_000,
        );
        let motion = expect(&mut conn, &mut seen, "PointerMotion", |m| match m {
            ServerMsg::PointerMotion(m) => Some(*m),
            _ => None,
        });

        seen.retain(|m| !matches!(m, ServerMsg::PointerMotion(_)));
        conn.tx()
            .bounds(
                win.rect,
                Rect::new(motion.pos.x - 10.0, motion.pos.y - 10.0, 20.0, 20.0),
            )
            .commit(serial)
            .unwrap();
        conn.flush().unwrap();
        expect(&mut conn, &mut seen, "Presented", |m| match m {
            ServerMsg::Presented(p) if p.serial == serial => Some(*p),
            _ => None,
        });
        h_.settle();
    }
    let flips = h_.frames() - before;

    // Two per move is the age-2 cost of the cursor alone (old rect and new
    // rect, into both buffers). Three would mean the content missed the
    // cursor's flip and needed one of its own — the bug.
    // Measured: 8 with the deferral, 12 without it — three flips per
    // motion instead of two, which is exactly the ratio issue #529
    // counted on the box.
    assert_eq!(
        flips, 8,
        "expected 2 flips per answered motion (the age-2 cursor cost); \
         {flips} means the content is not riding the cursor's flip"
    );

    // And the server says it held them back, without ever timing out: the
    // client answered every time.
    let s = h_.request_text("stats\n");
    assert!(stat(&s, "flips_deferred") > 0, "{s:?}");
    assert_eq!(
        stat(&s, "defer_timeouts"),
        0,
        "a client that answers must never hit the deadline: {s:?}"
    );
    h_.quit();
}

/// A client that is told about the motion and never answers must not stall
/// the cursor: the deadline fires and the frame goes in without it.
#[test]
fn a_client_that_never_answers_still_gets_a_flip_at_the_deadline() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("defer-timeout", w, h);
    let mut conn = h_.client("mute");
    let mut seen = Vec::new();
    let win = make_window(
        &mut conn,
        1,
        Size::new(120.0, 80.0),
        Color::rgb(0, 0xFF, 0),
        1,
    );
    let configure = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    // Aim at the window through its own `Configure`: since M3 it is
    // decorated and centred, not at the output's origin.
    h_.point_at(&configure, Point::new(20.0, 20.0), 1_000_000);
    expect(&mut conn, &mut seen, "PointerEnter", |m| match m {
        ServerMsg::PointerEnter(e) => Some(*e),
        _ => None,
    });

    h_.settle();

    // From here the client reads nothing and commits nothing — the wedged
    // client of the acceptance criteria, without needing SIGSTOP.
    let before = h_.frames();
    for step in 0..5u32 {
        h_.point_at(
            &configure,
            Point::new(30.0 + step as f32 * 8.0, 30.0),
            u64::from(2 + step) * 1_000_000,
        );

        h_.settle();
    }
    assert!(
        h_.frames() > before,
        "a wedged client must not stop the cursor"
    );
    let s = h_.request_text("stats\n");
    assert!(stat(&s, "flips_deferred") > 0, "{s:?}");
    assert!(
        stat(&s, "defer_timeouts") > 0,
        "the deadline is what released those flips: {s:?}"
    );
    h_.quit();
}

/// Cursor movement over the bare desktop has nobody to wait for, so it
/// must stay on the untouched fast path: two flips per isolated move (the
/// age-2 cost of old rect ∪ new rect) and nothing deferred.
#[test]
fn cursor_movement_over_the_desktop_is_never_deferred() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("defer-desktop", w, h);
    h_.input.push(InputEvent::PointerAbsolute {
        x: 0.1,
        y: 0.1,
        time_ns: 1_000_000,
    });
    h_.settle();

    let before = h_.frames();
    for step in 0..4u32 {
        h_.input.push(InputEvent::PointerAbsolute {
            x: f64::from(40 + step * 10) / f64::from(w),
            y: 0.5,
            time_ns: u64::from(2 + step) * 1_000_000,
        });
        h_.settle();
    }
    assert_eq!(
        h_.frames() - before,
        8,
        "two flips per isolated move, exactly as before the change"
    );
    let s = h_.request_text("stats\n");
    assert_eq!(stat(&s, "flips_deferred"), 0, "nobody to wait for: {s:?}");
    assert_eq!(stat(&s, "defer_timeouts"), 0, "{s:?}");
    h_.quit();
}

/// A deferral must never leave a timer armed behind it: once everything
/// has settled the server is back to zero wakeups, which is the property
/// the whole design exists for.
#[test]
fn a_deferral_leaves_no_timer_behind_and_the_server_goes_idle() {
    let (w, h) = (320, 200);
    let h_ = Harness::start("defer-idle", w, h);
    let mut conn = h_.client("idler");
    let mut seen = Vec::new();
    let win = make_window(&mut conn, 1, Size::new(120.0, 80.0), Color::WHITE, 1);
    expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(*c),
        _ => None,
    });
    h_.input.push(InputEvent::PointerAbsolute {
        x: 20.0 / f64::from(w),
        y: 20.0 / f64::from(h),
        time_ns: 1_000_000,
    });
    h_.input.push(InputEvent::PointerAbsolute {
        x: 40.0 / f64::from(w),
        y: 30.0 / f64::from(h),
        time_ns: 2_000_000,
    });
    h_.settle();

    // Nothing is happening any more. If the deferral timer were still
    // armed — or re-armed by its own expiry — the frame counter would keep
    // moving, and `voluntary_ctxt_switches` on the box would too.
    let before = h_.frames();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        h_.frames(),
        before,
        "a settled server after a deferral still makes no frames"
    );
    h_.quit();
}
