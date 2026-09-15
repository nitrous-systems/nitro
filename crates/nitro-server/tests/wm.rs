//! M3 window management, driven end to end through the real event loop on
//! the fake backend: decorations, drags, states, focus, MRU and
//! multi-output.
//!
//! Everything here goes through the same paths a real desktop does — real
//! `nitro-wire` clients, real synthetic input through the `FakeInput`
//! eventfd, real screenshots off the front buffer. The pure geometry
//! (hit regions, placement, resize arithmetic, MRU order) is unit-tested
//! in `src/wm.rs`; this file is about whether the *server* wires it up.

// Every geometry number here is produced by exact arithmetic on exact
// inputs — whole-pixel placements, sums and halves of small integers — so
// equality is the assertion that means what it says; an epsilon would
// only hide a wrong formula.
#![allow(clippy::float_cmp)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Palette, Point, Rect, Role, Size};
use nitro_kms::Image;
use nitro_server::input::{BTN_LEFT, FakeInput, InputEvent};
use nitro_server::wm;
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ButtonState, Layer, NodeId, WindowState, caps, window_flags};

/// evdev `BTN_RIGHT`.
const BTN_RIGHT: u32 = 0x111;

// evdev keycodes, from `linux/input-event-codes.h`.
const KEY_A: u32 = 30;
const KEY_Q: u32 = 16;
const KEY_M: u32 = 50;
const KEY_H: u32 = 35;
const KEY_TAB: u32 = 15;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;

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
    shell_path: PathBuf,
    input: FakeInput,
    /// Monotonically increasing, so every injected event has a distinct
    /// timestamp — a double click is decided by the gap between two of
    /// them.
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, width: u32, height: u32) -> Self {
        Self::start_with(name, width, height, |_| {})
    }

    fn start_with(name: &str, width: u32, height: u32, tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-wm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake { width, height };
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        tweak(&mut config);
        let wire_path = config.wire_path.clone();
        let shell_path = config.shell_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            shell_path,
            input,
            time_ns: 1_000_000,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        wait_for("the shell socket", || h.shell_path.exists());
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

    /// A privileged client on the shell socket, which is the only one that
    /// may put a window on a shell layer.
    fn shell(&self, name: &str) -> Connection {
        Connection::connect(&self.shell_path, name).expect("shell connect")
    }

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

    fn stat(&self, key: &str) -> u64 {
        let lines = self.request_text("stats\n");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
            .parse()
            .unwrap()
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

    /// Wait until the server has finished reacting: no flip in flight and
    /// the frame counter has stopped moving. Not "wait N frames" — a
    /// server with nothing to do stops flipping entirely.
    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let lines = self.request_text("stats\n");
            let value = |key: &str| -> u64 {
                lines
                    .iter()
                    .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let frames = value("frames");
            if value("flips_pending") == 0 && frames == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = frames;
            std::thread::sleep(Duration::from_millis(8));
            stable >= 3
        });
    }

    /// Move the pointer to a device-pixel position on the first output.
    ///
    /// Absolute rather than relative so a test says where it wants the
    /// pointer rather than where it wants it to go next; libinput's
    /// acceleration is not in the picture for an absolute device.
    fn point_at(&mut self, x: f32, y: f32, size: (u32, u32)) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(x) / f64::from(size.0),
            y: f64::from(y) / f64::from(size.1),
            time_ns: self.time_ns,
        });
    }

    fn button(&mut self, button: u32, state: ButtonState) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerButton {
            button,
            state,
            time_ns: self.time_ns,
        });
    }

    fn key(&mut self, keycode: u32, pressed: bool) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::Key {
            keycode,
            pressed,
            time_ns: self.time_ns,
        });
    }

    /// Press at `from`, drag through a few intermediate points, release at
    /// `to`. Several motions rather than one, because a drag is driven by
    /// motion events and a single jump would not exercise the path a real
    /// pointer takes.
    fn drag(&mut self, from: (f32, f32), to: (f32, f32), size: (u32, u32)) {
        self.point_at(from.0, from.1, size);
        self.settle();
        self.button(BTN_LEFT, ButtonState::Pressed);
        self.settle();
        for i in 1..=4 {
            let t = i as f32 / 4.0;
            self.point_at(
                from.0 + (to.0 - from.0) * t,
                from.1 + (to.1 - from.1) * t,
                size,
            );
            self.settle();
        }
        self.button(BTN_LEFT, ButtonState::Released);
        self.settle();
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

/// Drain a client's socket until `f` matches, or time out. Anything else
/// that arrives is kept, so a later call can still see it.
fn expect<T>(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    what: &str,
    f: impl Fn(&ServerMsg) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = seen.iter().rev().find_map(&f) {
            return found;
        }
        assert!(Instant::now() < deadline, "no {what}; got {seen:?}");
        conn.flush().unwrap();
        conn.poll(seen).unwrap_or_else(|e| panic!("{what}: {e}"));
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Everything one client received, in arrival order.
///
/// One buffer per *connection*, not per window: a `Focus` or a `Closed`
/// names a window but arrives on the connection, and draining into
/// per-window buffers would let one window's wait swallow another's
/// message.
#[derive(Default)]
struct Inbox(Vec<ServerMsg>);

/// A window: the client's node id plus the geometry the server gave it.
#[derive(Clone, Copy)]
struct Win {
    root: NodeId,
    /// Content position and size, from the newest `Configure`.
    pos: Point,
    size: Size,
}

impl Win {
    /// The window's *frame* rectangle: the content rectangle grown by the
    /// insets, which is what the user sees and what a drag grabs.
    fn frame(&self, decorated: bool) -> Rect {
        if !decorated {
            return Rect::new(self.pos.x, self.pos.y, self.size.w, self.size.h);
        }
        let i = wm::frame_insets();
        Rect::new(
            self.pos.x - i.left,
            self.pos.y - i.top,
            self.size.w + i.width(),
            self.size.h + i.height(),
        )
    }

    /// The middle of the title bar, away from the buttons.
    fn title_bar(&self) -> (f32, f32) {
        let f = self.frame(true);
        (f.x + f.w / 3.0, f.y + wm::TITLE_H / 2.0)
    }

    /// The middle of the content.
    fn content(&self) -> (f32, f32) {
        (
            self.pos.x + self.size.w / 2.0,
            self.pos.y + self.size.h / 2.0,
        )
    }
}

/// Create a window filled with one solid rect, and wait for the
/// `Configure` that says where the server put it.
// Six facts about one window plus the two handles it is created through;
// a struct here would be this argument list with a name on it.
#[allow(clippy::too_many_arguments)]
fn make_window(
    conn: &mut Connection,
    inbox: &mut Inbox,
    id: u32,
    title: &str,
    size: Size,
    color: Color,
    flags: u32,
    serial: u32,
) -> Win {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, title, size, Layer::Normal, flags)
        .create_rect(rect, root, Rect::new(0.0, 0.0, size.w, size.h))
        .fill_solid(rect, color)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    let (pos, size) = expect(conn, &mut inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some((c.position, c.size)),
        _ => None,
    });
    Win { root, pos, size }
}

/// Re-read a window's geometry from the newest `Configure` that arrived.
fn refresh(conn: &mut Connection, inbox: &mut Inbox, win: &mut Win) {
    conn.flush().unwrap();
    let _ = conn.poll(&mut inbox.0);
    for m in inbox.0.iter().rev() {
        if let ServerMsg::Configure(c) = m
            && c.window == win.root
        {
            win.pos = c.position;
            win.size = c.size;
            return;
        }
    }
}

/// Wait for a `Configure` that actually changed the geometry, and take it.
fn await_configure(conn: &mut Connection, inbox: &mut Inbox, win: &mut Win, what: &str) {
    let (before_pos, before_size) = (win.pos, win.size);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        refresh(conn, inbox, win);
        if (win.pos, win.size) != (before_pos, before_size) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no Configure for {what}; still at {before_pos:?} {before_size:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait until the server reports `window` focused.
///
/// Polls rather than reading the buffer we already have: a focus change
/// is what we are waiting *for*, so an older `Focus{true}` sitting in the
/// inbox is exactly the answer that must not count.
fn await_focus(conn: &mut Connection, inbox: &mut Inbox, window: NodeId, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        conn.flush().unwrap();
        let _ = conn.poll(&mut inbox.0);
        let last = inbox.0.iter().rev().find_map(|m| match m {
            ServerMsg::Focus(f) if f.focused => Some(f.window),
            _ => None,
        });
        if last == Some(window) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: focus is {last:?}, wanted {window:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);
const GREEN: Color = Color::rgb(0x00, 0xFF, 0x00);
const BLUE: Color = Color::rgb(0x00, 0x00, 0xFF);

fn rgb(px: u32) -> u32 {
    px & 0x00ff_ffff
}

fn to_rgb(c: Color) -> u32 {
    u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

/// A decoration colour from the palette a server with no `server.conf`
/// runs on — the default scheme. Tests that assert on title-bar pixels
/// go through this rather than naming a colour, which is the whole point
/// of M4-F: the constants they used to read no longer exist, because the
/// colours are the user's to change.
fn role(r: Role) -> Color {
    Palette::default().get(r)
}

/// The title-bar colour for a focus state.
fn bar(focused: bool) -> Color {
    role(if focused {
        Role::TitleBarActive
    } else {
        Role::TitleBarInactive
    })
}

/// Park the pointer in a corner, where it cannot contaminate a pixel
/// assertion or hover a window under test.
fn park(h: &mut Harness) {
    h.point_at(OUT.0 as f32 - 2.0, OUT.1 as f32 - 2.0, OUT);
    h.settle();
}

#[test]
fn the_server_advertises_window_management() {
    let h = Harness::start("caps", OUT.0, OUT.1);
    let conn = h.client("caps");
    assert_eq!(
        conn.caps() & caps::WM,
        caps::WM,
        "the WM bit is unconditional: the server always manages windows"
    );
    drop(conn);
    h.quit();
}

#[test]
fn a_decorated_window_gets_a_title_bar_above_its_content() {
    let mut h = Harness::start("decor", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("decor");
    let win = make_window(&mut conn, &mut inbox, 1, "Hello", WIN, RED, 0, 1);
    park(&mut h);

    // The content is pushed down by the title bar and in by the border,
    // and `Configure.position` reports the *content* origin, so the two
    // agree by construction.
    let frame = win.frame(true);
    assert!(frame.y < win.pos.y, "the bar is above the content");
    assert_eq!(win.pos.y - frame.y, wm::TITLE_H);
    assert_eq!(win.pos.x - frame.x, wm::BORDER);
    assert_eq!(h.stat("decorated"), 1);

    let img = h.shot();
    // Pixels in the title bar: the bar is painted, and it is not the
    // desktop and not the client's red.
    let bar_px = img.pixel(
        (frame.x + frame.w / 2.0) as u32,
        (frame.y + wm::TITLE_H / 2.0) as u32,
    );
    assert_eq!(
        rgb(bar_px),
        to_rgb(bar(true)),
        "the focused title bar is painted"
    );
    // The client's own pixels are untouched by the frame.
    let (cx, cy) = win.content();
    assert_eq!(rgb(img.pixel(cx as u32, cy as u32)), to_rgb(RED));

    // The close button's glyph, which since #3715 is a symbolic `x`
    // tinted like the title rather than a red disc. Its centre is the
    // crossing of the two strokes, so it is ink: the discriminator is
    // that the button's *corner* is still bar colour, i.e. there is no
    // disc under it while nothing is pointing at it.
    let close = wm::buttons(frame, false)[0].1;
    let centre = img.pixel(
        (close.x + close.w / 2.0) as u32,
        (close.y + close.h / 2.0) as u32,
    );
    assert_ne!(rgb(centre), to_rgb(bar(true)), "the x glyph is drawn");
    assert_eq!(
        rgb(img.pixel(close.x as u32, close.y as u32)),
        to_rgb(bar(true)),
        "a resting button has no disc: its corner is the bar"
    );
    // And the red is gone from the resting frame entirely — it is a
    // hover colour now, which is the whole visual change.
    let mut reds = 0;
    for y in frame.y as u32..(frame.y + wm::TITLE_H) as u32 {
        for x in frame.x as u32..(frame.x + frame.w) as u32 {
            if rgb(img.pixel(x, y)) == to_rgb(role(Role::TitleClose)) {
                reds += 1;
            }
        }
    }
    assert_eq!(reds, 0, "title_close is painted only on hover");

    drop(conn);
    h.quit();
}

#[test]
fn an_undecorated_window_gets_no_frame_at_all() {
    let mut h = Harness::start("bare", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("bare");
    let win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "bare",
        WIN,
        RED,
        window_flags::UNDECORATED,
        1,
    );
    park(&mut h);

    assert_eq!(h.stat("decorated"), 0, "it opted out");
    assert_eq!(h.stat("windows"), 1);
    // Its content starts exactly where its frame does: nothing was added.
    assert_eq!(win.frame(false).x, win.pos.x);

    let img = h.shot();
    // Everything from the content's top-left corner is the client's.
    assert_eq!(
        rgb(img.pixel(win.pos.x as u32, win.pos.y as u32)),
        to_rgb(RED)
    );
    // And one pixel above it is still the desktop, not a title bar.
    assert_ne!(
        rgb(img.pixel(win.pos.x as u32, win.pos.y as u32 - 1)),
        to_rgb(bar(true))
    );

    drop(conn);
    h.quit();
}

#[test]
fn dragging_the_title_bar_moves_the_window_by_the_drag_delta() {
    let mut h = Harness::start("drag", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("drag");
    let mut win = make_window(&mut conn, &mut inbox, 1, "drag", WIN, RED, 0, 1);
    let before = win.frame(true);

    let (bx, by) = win.title_bar();
    let (dx, dy) = (-40.0, 30.0);
    let frames_before = h.stat("frames");
    h.drag((bx, by), (bx + dx, by + dy), OUT);

    refresh(&mut conn, &mut inbox, &mut win);
    let after = win.frame(true);
    assert_eq!(
        (after.x - before.x, after.y - before.y),
        (dx, dy),
        "the frame moved by exactly the drag delta"
    );
    assert_eq!(
        (after.w, after.h),
        (before.w, before.h),
        "a move does not resize"
    );

    // The content moved with it, and the client was told where it is now:
    // `position` is what a client crops a screenshot with, so a pure move
    // has to produce a `Configure` even though the size never changed.
    let img = h.shot();
    let (cx, cy) = win.content();
    assert_eq!(rgb(img.pixel(cx as u32, cy as u32)), to_rgb(RED));

    // Four motions, so at most four frames plus the press and release:
    // the drag is *not* repainting the whole screen per motion. The real
    // bound is the damage, checked below.
    let painted = h.stat("frames") - frames_before;
    assert!(painted <= 12, "a six-event drag painted {painted} frames");

    // Old ∪ new per motion, not a full screen: one step of the drag
    // damages about twice the window's area.
    let step_area = f64::from(before.w * before.h) * 2.0;
    let mean = h.stat("damage_px_mean") as f64;
    assert!(
        mean < step_area * 1.6,
        "damage_px_mean {mean} is not ~2x the window ({step_area})"
    );

    drop(conn);
    h.quit();
}

#[test]
fn dragging_an_edge_resizes_and_configures_the_client() {
    let mut h = Harness::start("resize", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("resize");
    let mut win = make_window(&mut conn, &mut inbox, 1, "resize", WIN, RED, 0, 1);
    let before = win.frame(true);

    // Grab the right edge, in the middle vertically so only one axis moves.
    let grab = (
        before.x + before.w - wm::BORDER / 2.0,
        before.y + before.h / 2.0,
    );
    h.drag(grab, (grab.0 + 60.0, grab.1), OUT);

    await_configure(&mut conn, &mut inbox, &mut win, "the resize");
    let after = win.frame(true);
    assert_eq!(after.w - before.w, 60.0, "the frame grew by the drag");
    assert_eq!(after.h, before.h, "the other axis did not move");
    assert_eq!(after.x, before.x, "the far edge stayed put");
    assert_eq!(
        win.size.w,
        after.w - wm::frame_insets().width(),
        "the client is told its content size, not the frame's"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_resize_respects_the_limits_the_client_declared() {
    let mut h = Harness::start("limits", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("limits");
    let mut win = make_window(&mut conn, &mut inbox, 1, "limits", WIN, RED, 0, 1);
    conn.tx()
        .set_window_limits(win.root, Size::new(150.0, 100.0), Size::new(260.0, 200.0))
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();

    let before = win.frame(true);
    let grab = (
        before.x + before.w - wm::BORDER / 2.0,
        before.y + before.h / 2.0,
    );
    // Pull far past the maximum.
    h.drag(grab, (grab.0 + 400.0, grab.1), OUT);
    await_configure(&mut conn, &mut inbox, &mut win, "the clamped resize");
    assert_eq!(win.size.w, 260.0, "clamped to the declared maximum");

    // And far past the minimum, the other way.
    let now = win.frame(true);
    let grab = (now.x + now.w - wm::BORDER / 2.0, now.y + now.h / 2.0);
    h.drag(grab, (grab.0 - 400.0, grab.1), OUT);
    await_configure(&mut conn, &mut inbox, &mut win, "the clamped shrink");
    assert_eq!(win.size.w, 150.0, "clamped to the declared minimum");

    drop(conn);
    h.quit();
}

#[test]
fn the_close_button_sends_closed_and_the_client_decides() {
    let mut h = Harness::start("close", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("close");
    let win = make_window(&mut conn, &mut inbox, 1, "close", WIN, RED, 0, 1);
    let frame = win.frame(true);
    let close = wm::buttons(frame, false)[0].1;

    h.point_at(close.x + close.w / 2.0, close.y + close.h / 2.0, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    expect(&mut conn, &mut inbox.0, "Closed", |m| match m {
        ServerMsg::Closed(c) if c.window == win.root => Some(()),
        _ => None,
    });
    // The window is still there: `Closed` is an *ask*, and the client has
    // not destroyed its root. A server that tore it down itself would
    // take an unsaved document with it.
    assert_eq!(h.stat("windows"), 1);

    drop(conn);
    h.quit();
}

#[test]
fn the_maximize_button_and_a_double_click_both_fill_the_work_area() {
    let mut h = Harness::start("max", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("max");
    let mut win = make_window(&mut conn, &mut inbox, 1, "max", WIN, RED, 0, 1);
    let restored = win.frame(true);
    let frame = win.frame(true);
    let max = wm::buttons(frame, false)[1].1;

    h.point_at(max.x + max.w / 2.0, max.y + max.h / 2.0, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    await_configure(&mut conn, &mut inbox, &mut win, "maximize");
    let after = win.frame(true);
    assert_eq!(
        (after.x, after.y, after.w, after.h),
        (0.0, 0.0, OUT.0 as f32, OUT.1 as f32),
        "maximized fills the work area, which in M3-A is the whole output"
    );
    let state = expect(&mut conn, &mut inbox.0, "WindowState", |m| match m {
        ServerMsg::WindowState(s) if s.window == win.root => Some(s.state),
        _ => None,
    });
    assert_eq!(state, WindowState::Maximized);

    // A double click on the title bar toggles it back to exactly where it
    // was: the restore rectangle is remembered, not recomputed.
    let (bx, by) = (after.x + after.w / 3.0, after.y + wm::TITLE_H / 2.0);
    h.point_at(bx, by, OUT);
    h.settle();
    for _ in 0..2 {
        h.button(BTN_LEFT, ButtonState::Pressed);
        h.button(BTN_LEFT, ButtonState::Released);
    }
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the restore");
    assert_eq!(win.frame(true), restored, "back to where it started");

    drop(conn);
    h.quit();
}

#[test]
fn super_drag_moves_and_resizes_an_undecorated_window() {
    let mut h = Harness::start("superdrag", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("superdrag");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "bare",
        WIN,
        RED,
        window_flags::UNDECORATED,
        1,
    );
    let before = win.frame(false);
    let (cx, cy) = win.content();

    // Super + left-drag, starting in the middle of the client's content —
    // which is exactly where an undecorated window has nothing else to
    // grab, and the reason the modifier exists.
    h.key(KEY_LEFTMETA, true);
    h.settle();
    h.drag((cx, cy), (cx + 50.0, cy - 20.0), OUT);
    h.key(KEY_LEFTMETA, false);
    h.settle();

    refresh(&mut conn, &mut inbox, &mut win);
    let after = win.frame(false);
    assert_eq!((after.x - before.x, after.y - before.y), (50.0, -20.0));

    // Super + right-drag resizes from the nearest corner. Grab near the
    // bottom-right so that is the corner chosen.
    let now = win.frame(false);
    let near = (now.x + now.w * 0.9, now.y + now.h * 0.9);
    h.key(KEY_LEFTMETA, true);
    h.settle();
    h.point_at(near.0, near.1, OUT);
    h.settle();
    h.button(BTN_RIGHT, ButtonState::Pressed);
    h.settle();
    h.point_at(near.0 + 40.0, near.1 + 30.0, OUT);
    h.settle();
    h.button(BTN_RIGHT, ButtonState::Released);
    h.key(KEY_LEFTMETA, false);
    h.settle();

    await_configure(&mut conn, &mut inbox, &mut win, "the Super resize");
    let grown = win.frame(false);
    assert_eq!((grown.w - now.w, grown.h - now.h), (40.0, 30.0));
    assert_eq!((grown.x, grown.y), (now.x, now.y), "the far corner held");

    drop(conn);
    h.quit();
}

#[test]
fn alt_tab_walks_three_windows_in_mru_order() {
    let mut h = Harness::start("mru", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("mru");
    let a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);
    let b = make_window(&mut conn, &mut inbox, 3, "b", WIN, GREEN, 0, 2);
    let c = make_window(&mut conn, &mut inbox, 5, "c", WIN, BLUE, 0, 3);
    h.settle();

    // Each window took focus as it was created, so the MRU order is
    // c, b, a and `c` has it now.
    await_focus(&mut conn, &mut inbox, c.root, "the newest window has focus");

    // One Alt+Tab: the previously used window, which makes a single
    // Alt+Tab a toggle between the last two.
    h.key(KEY_LEFTALT, true);
    h.key(KEY_TAB, true);
    h.key(KEY_TAB, false);
    h.settle();
    await_focus(&mut conn, &mut inbox, b.root, "one Alt+Tab");

    // A second Tab with Alt still held walks *further*, rather than
    // bouncing back: the MRU list is not reordered until Alt comes up.
    h.key(KEY_TAB, true);
    h.key(KEY_TAB, false);
    h.settle();
    await_focus(&mut conn, &mut inbox, a.root, "a second Tab walks further");

    h.key(KEY_LEFTALT, false);
    h.settle();

    // Alt came up on `a`, so `a` is most recent now and the next
    // Alt+Tab goes to `c` — the one that was focused before the cycle.
    h.key(KEY_LEFTALT, true);
    h.key(KEY_TAB, true);
    h.key(KEY_TAB, false);
    h.key(KEY_LEFTALT, false);
    h.settle();
    await_focus(
        &mut conn,
        &mut inbox,
        c.root,
        "a fresh cycle after Alt came up",
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_minimized_window_leaves_the_screen_but_not_the_alt_tab_order() {
    let mut h = Harness::start("minimize", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("minimize");
    let a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);
    let mut b = make_window(&mut conn, &mut inbox, 3, "b", WIN, GREEN, 0, 2);
    park(&mut h);
    let (bx, by) = b.content();
    assert_eq!(rgb(h.shot().pixel(bx as u32, by as u32)), to_rgb(GREEN));

    // Super+H on the focused window (`b`, the newest).
    h.key(KEY_LEFTMETA, true);
    h.key(KEY_H, true);
    h.key(KEY_H, false);
    h.key(KEY_LEFTMETA, false);
    h.settle();

    assert_eq!(h.stat("minimized"), 1);
    assert_eq!(h.stat("windows"), 2, "still a window, just hidden");
    assert_ne!(
        rgb(h.shot().pixel(bx as u32, by as u32)),
        to_rgb(GREEN),
        "its pixels are gone"
    );

    // It is also gone from the hit test: clicking where it was reaches
    // whatever is underneath, not the hidden window.
    h.point_at(bx, by, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();
    conn.flush().unwrap();
    let _ = conn.poll(&mut inbox.0);
    assert!(
        !inbox.0.iter().any(|m| matches!(
            m,
            ServerMsg::PointerButton(p) if p.window == b.root
        )),
        "a minimized window must not swallow a click"
    );

    // One Alt+Tab brings it back. Found on the box: minimizing used to
    // leave the window at the *front* of the MRU list, so the first
    // Alt+Tab landed on the window that had just inherited the focus and
    // appeared to do nothing at all.
    h.key(KEY_LEFTALT, true);
    h.key(KEY_TAB, true);
    h.key(KEY_TAB, false);
    h.key(KEY_LEFTALT, false);
    h.settle();
    await_focus(
        &mut conn,
        &mut inbox,
        b.root,
        "Alt+Tab reached the hidden window",
    );
    refresh(&mut conn, &mut inbox, &mut b);
    park(&mut h);
    assert_eq!(h.stat("minimized"), 0, "Alt+Tab un-minimized it");
    assert_eq!(rgb(h.shot().pixel(bx as u32, by as u32)), to_rgb(GREEN));
    let _ = a;

    drop(conn);
    h.quit();
}

#[test]
fn super_q_closes_and_super_m_maximizes_the_focused_window() {
    let mut h = Harness::start("chords", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("chords");
    let mut win = make_window(&mut conn, &mut inbox, 1, "chords", WIN, RED, 0, 1);
    h.settle();

    h.key(KEY_LEFTMETA, true);
    h.key(KEY_M, true);
    h.key(KEY_M, false);
    h.key(KEY_LEFTMETA, false);
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "Super+M");
    assert_eq!(win.frame(true).w, OUT.0 as f32);

    h.key(KEY_LEFTMETA, true);
    h.key(KEY_Q, true);
    h.key(KEY_Q, false);
    h.key(KEY_LEFTMETA, false);
    h.settle();
    expect(&mut conn, &mut inbox.0, "Closed", |m| match m {
        ServerMsg::Closed(c) if c.window == win.root => Some(()),
        _ => None,
    });

    drop(conn);
    h.quit();
}

#[test]
fn a_client_asks_for_a_state_and_is_told_what_it_got() {
    let mut h = Harness::start("state", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("state");
    let mut win = make_window(&mut conn, &mut inbox, 1, "state", WIN, RED, 0, 1);
    park(&mut h);

    conn.tx()
        .set_window_state(win.root, WindowState::Fullscreen)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "fullscreen");

    assert_eq!(
        (win.pos, win.size),
        (Point::ZERO, Size::new(OUT.0 as f32, OUT.1 as f32)),
        "fullscreen covers the output, with no insets left"
    );
    let state = expect(&mut conn, &mut inbox.0, "WindowState", |m| match m {
        ServerMsg::WindowState(s) if s.window == win.root => Some(s.state),
        _ => None,
    });
    assert_eq!(state, WindowState::Fullscreen);
    // Decorations are hidden, not destroyed: the top-left pixel is the
    // client's, and the window is still counted as decorated.
    assert_eq!(rgb(h.shot().pixel(0, 0)), to_rgb(RED));
    assert_eq!(h.stat("decorated"), 1);

    conn.tx()
        .set_window_state(win.root, WindowState::Normal)
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "back to normal");
    assert_eq!(win.size, WIN, "the restore rectangle came back");
    assert_eq!(
        rgb(h.shot().pixel(
            (win.pos.x + win.size.w / 2.0) as u32,
            (win.pos.y - wm::TITLE_H / 2.0) as u32
        )),
        to_rgb(bar(true)),
        "the title bar is back"
    );

    drop(conn);
    h.quit();
}

#[test]
fn maximizing_from_minimized_still_remembers_where_the_window_was() {
    // `Minimized` keeps a window's geometry, so a client may legally
    // minimize and then maximize. If the restore rectangle were only taken
    // on the way out of `Normal`, that path would lose it and the window
    // could never get its own size back.
    let mut h = Harness::start("restore", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("restore");
    let mut win = make_window(&mut conn, &mut inbox, 1, "restore", WIN, RED, 0, 1);
    park(&mut h);
    let before = (win.pos, win.size);

    conn.tx()
        .set_window_state(win.root, WindowState::Minimized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("minimized"), 1);

    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "maximize");
    assert_ne!((win.pos, win.size), before, "it did maximize");

    conn.tx()
        .set_window_state(win.root, WindowState::Normal)
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "back to normal");
    assert_eq!(
        (win.pos, win.size),
        before,
        "the pre-minimize rectangle survived the detour"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_fixed_size_window_cannot_be_maximized_and_has_no_maximize_button() {
    let h = Harness::start("fixed", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("fixed");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "fixed",
        WIN,
        RED,
        window_flags::FIXED_SIZE,
        1,
    );
    let before = win.frame(true);

    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    refresh(&mut conn, &mut inbox, &mut win);
    assert_eq!(win.frame(true), before, "the request was refused");
    // Silently: the protocol has no per-request error, and every error it
    // does have is fatal.
    conn.flush().unwrap();
    let _ = conn.poll(&mut inbox.0);
    assert!(
        !inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Error(_) | ServerMsg::WindowState(_))),
        "no error and no state event: {:?}",
        inbox.0
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_second_output_takes_a_window_dragged_onto_it_and_gives_it_back() {
    let mut h = Harness::start("twoout", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("twoout");
    let mut win = make_window(&mut conn, &mut inbox, 1, "roam", WIN, RED, 0, 1);
    let first_output = expect(&mut conn, &mut inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(c.output),
        _ => None,
    });

    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the second output", || h.stat("outputs") == 2);
    h.settle();
    assert_eq!(
        h.request_text("outputs\n").len(),
        3,
        "ok plus one line per output"
    );

    // Outputs are laid out left to right, so the second one starts at the
    // first one's width. Drag the window's centre well into it.
    let (bx, by) = win.title_bar();
    let target_x = OUT.0 as f32 + 150.0;
    h.drag((bx, by), (target_x, 120.0), OUT);

    await_configure(&mut conn, &mut inbox, &mut win, "the output change");
    let now = expect(&mut conn, &mut inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(c.output),
        _ => None,
    });
    assert_ne!(
        now, first_output,
        "the window belongs to the output it is on"
    );

    // Unplugging that output must not lose the window: it migrates back
    // to the primary, because an unplaced window is in no z-order and no
    // click or Alt+Tab could ever reach it again.
    assert_eq!(h.request_line("unplug\n"), "ok");
    wait_for("the output to go", || h.stat("outputs") == 1);
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the migration");
    let back = expect(&mut conn, &mut inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(c.output),
        _ => None,
    });
    assert_eq!(back, first_output, "migrated to the primary output");
    let frame = win.frame(true);
    assert!(
        frame.x >= 0.0 && frame.x + frame.w <= OUT.0 as f32,
        "clamped into the primary's work area: {frame:?}"
    );
    assert_eq!(h.stat("windows"), 1);

    drop(conn);
    h.quit();
}

#[test]
fn an_output_scale_of_two_doubles_the_device_pixels() {
    // The scale override goes through `Config` rather than `NITRO_SCALE`:
    // the harness runs the server on a thread in *this* process, and the
    // environment is process-global state parallel tests must not fight
    // over.
    let mut h = Harness::start_with("scale2", OUT.0, OUT.1, |c| {
        c.scales = nitro_server::parse_scales("Virtual-1=2");
    });

    let mut inbox = Inbox::default();
    let mut conn = h.client("scale2");
    let win = make_window(&mut conn, &mut inbox, 1, "big", WIN, RED, 0, 1);
    park(&mut h);

    let scale = expect(&mut conn, &mut inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == win.root => Some(c.scale),
        _ => None,
    });
    assert_eq!(scale, 2.0, "the override reached the client");
    // The logical size is unchanged; the device pixels are doubled. Count
    // the red ones: a 200x120 logical window at 2x is 400x240 device
    // pixels, and every one of them is the client's fill.
    let img = h.shot();
    let mut red = 0u32;
    for y in 0..img.height {
        for x in 0..img.width {
            if rgb(img.pixel(x, y)) == to_rgb(RED) {
                red += 1;
            }
        }
    }
    let want = (WIN.w * 2.0) as u32 * (WIN.h * 2.0) as u32;
    assert_eq!(red, want, "a 2x output draws 2x the device pixels");

    drop(conn);
    h.quit();
}

#[test]
fn clicking_a_window_raises_and_focuses_it_and_restyles_both_frames() {
    let mut h = Harness::start("raise", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("raise");
    let mut a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);
    let b = make_window(&mut conn, &mut inbox, 3, "b", WIN, GREEN, 0, 2);
    h.settle();

    // Pull `a` clear of `b`, so raising one cannot cover the other's bar
    // and each sample point stays honest whichever is on top. The cascade
    // deliberately overlaps them, which is right for a desktop and wrong
    // for a pixel assertion.
    let (bx, by) = a.title_bar();
    h.drag((bx, by), (bx - 200.0, by - 120.0), OUT);
    refresh(&mut conn, &mut inbox, &mut a);
    let bar_of = |w: &Win| {
        let f = w.frame(true);
        ((f.x + f.w / 3.0) as u32, (f.y + wm::TITLE_H / 2.0) as u32)
    };
    let (a_bar, b_bar) = (bar_of(&a), bar_of(&b));
    assert!(
        a.frame(true).x + a.frame(true).w < b.frame(true).x
            || a.frame(true).y + a.frame(true).h < b.frame(true).y,
        "the two windows must not overlap for this test"
    );

    // The drag focused `a`, so `b`'s bar is the inactive one now.
    park(&mut h);
    let img = h.shot();
    assert_eq!(rgb(img.pixel(a_bar.0, a_bar.1)), to_rgb(bar(true)));
    assert_eq!(
        rgb(img.pixel(b_bar.0, b_bar.1)),
        to_rgb(bar(false)),
        "an unfocused frame is drawn differently"
    );

    // Click `b`'s title bar: it raises, focuses and the colours swap.
    h.point_at(b_bar.0 as f32, b_bar.1 as f32, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();
    await_focus(&mut conn, &mut inbox, b.root, "the click focused it");
    park(&mut h);

    let img = h.shot();
    assert_eq!(rgb(img.pixel(b_bar.0, b_bar.1)), to_rgb(bar(true)));
    assert_eq!(rgb(img.pixel(a_bar.0, a_bar.1)), to_rgb(bar(false)));

    drop(conn);
    h.quit();
}

#[test]
fn a_no_focus_window_never_takes_the_keyboard() {
    let mut h = Harness::start("nofocus", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("nofocus");
    let normal = make_window(&mut conn, &mut inbox, 1, "normal", WIN, RED, 0, 1);
    let panel = make_window(
        &mut conn,
        &mut inbox,
        3,
        "panel",
        WIN,
        GREEN,
        window_flags::NO_FOCUS | window_flags::UNDECORATED,
        2,
    );
    h.settle();

    // The NO_FOCUS window was created last but did not steal the focus.
    await_focus(
        &mut conn,
        &mut inbox,
        normal.root,
        "the focusable window keeps it",
    );

    // Clicking it does not either — it still gets the click, it just does
    // not get the keyboard.
    let (px, py) = panel.content();
    h.point_at(px, py, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();
    conn.flush().unwrap();
    let _ = conn.poll(&mut inbox.0);
    assert!(
        inbox.0.iter().any(|m| matches!(
            m,
            ServerMsg::PointerButton(p) if p.window == panel.root
        )),
        "it still receives pointer input"
    );
    assert!(
        !inbox.0.iter().any(|m| matches!(
            m,
            ServerMsg::Focus(f) if f.window == panel.root && f.focused
        )),
        "but never the focus"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_click_on_the_empty_desktop_keeps_the_focus() {
    // The desktop is not a focus target. Dropping focus on a click that hit
    // nothing would leave a screen full of windows with the keyboard going
    // nowhere until the next Alt+Tab — `focused 0` with windows on screen,
    // which the server README calls a bug outright.
    let mut h = Harness::start("desktop-click", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("desktop-click");
    let win = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);
    h.settle();
    await_focus(&mut conn, &mut inbox, win.root, "the new window has focus");

    // The top-left corner: the window is centred, so this is bare desktop,
    // well clear of even the resize band's six pixels of outward slop.
    h.point_at(4.0, 4.0, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    assert_eq!(
        h.stat("focused"),
        1,
        "a click on nothing must not drop the focus"
    );
    conn.flush().unwrap();
    let _ = conn.poll(&mut inbox.0);
    assert!(
        !inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Focus(f) if f.window == win.root && !f.focused)),
        "and must not tell the window it lost it"
    );

    // The real test of "still focused": a key still arrives.
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    let got = expect(&mut conn, &mut inbox.0, "a Key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_A => Some(k.window),
        _ => None,
    });
    assert_eq!(got, win.root, "keys still reach the focused window");

    drop(conn);
    h.quit();
}

#[test]
fn a_click_on_a_no_focus_shell_window_keeps_the_focus_too() {
    // Same rule one step further in: a window that refused the keyboard is
    // no more a focus target than the desktop is, so clicking a bar or a
    // launcher must not park the focus on nothing either. The click itself
    // still reaches it — that is how a bar's buttons work.
    let mut h = Harness::start("panel-click", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("panel-click");
    let win = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);

    // A shell client, because the `Top` layer is the shell socket's alone.
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("panel");
    let panel_root = NodeId(11);
    let panel_rect = NodeId(12);
    let panel_size = Size::new(120.0, 40.0);
    shell
        .tx()
        .create_window_with(
            panel_root,
            "panel",
            panel_size,
            Layer::Top,
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
        )
        .create_rect(
            panel_rect,
            panel_root,
            Rect::new(0.0, 0.0, panel_size.w, panel_size.h),
        )
        .fill_solid(panel_rect, GREEN)
        .commit(1)
        .unwrap();
    shell.flush().unwrap();
    let (panel_pos, panel_size) =
        expect(&mut shell, &mut shell_inbox.0, "Configure", |m| match m {
            ServerMsg::Configure(c) if c.window == panel_root => Some((c.position, c.size)),
            _ => None,
        });
    h.settle();

    // The panel took no focus when it appeared; the ordinary window has it.
    await_focus(
        &mut conn,
        &mut inbox,
        win.root,
        "the focusable window has it",
    );

    h.point_at(
        panel_pos.x + panel_size.w / 2.0,
        panel_pos.y + panel_size.h / 2.0,
        OUT,
    );
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    assert_eq!(
        h.stat("focused"),
        1,
        "clicking a NO_FOCUS window must not drop the focus"
    );

    // The click still reached the panel: that is how a bar's buttons work.
    shell.flush().unwrap();
    let _ = shell.poll(&mut shell_inbox.0);
    assert!(
        shell_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::PointerButton(p) if p.window == panel_root)),
        "the NO_FOCUS window still receives the click"
    );
    assert!(
        !shell_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Focus(f) if f.window == panel_root && f.focused)),
        "but never the focus"
    );

    // And the keyboard never left the window that had it.
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    let got = expect(&mut conn, &mut inbox.0, "a Key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_A => Some(k.window),
        _ => None,
    });
    assert_eq!(
        got, win.root,
        "keys still reach the previously focused window"
    );

    drop((conn, shell));
    h.quit();
}

#[test]
fn new_windows_are_centred_and_cascade_inside_the_work_area() {
    let h = Harness::start("place", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("place");
    let area = Rect::new(0.0, 0.0, OUT.0 as f32, OUT.1 as f32);
    let insets = wm::frame_insets();
    let frame_size = Size::new(WIN.w + insets.width(), WIN.h + insets.height());

    let mut frames = Vec::new();
    for i in 0..4u32 {
        let win = make_window(&mut conn, &mut inbox, 1 + i * 2, "w", WIN, RED, 0, 1 + i);
        frames.push(win.frame(true));
    }
    h.settle();

    // The first is centred, and each later one steps down and right.
    assert_eq!(
        Point::new(frames[0].x, frames[0].y),
        wm::place(0, frame_size, area)
    );
    for pair in frames.windows(2) {
        assert!(
            pair[1].x > pair[0].x && pair[1].y > pair[0].y,
            "{:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    // And every one of them is entirely on screen: a window whose title
    // bar is off the top is one the user cannot recover without a
    // keyboard shortcut.
    for f in &frames {
        assert!(
            f.x >= area.x && f.y >= area.y && f.x + f.w <= area.w && f.y + f.h <= area.h,
            "{f:?} left the work area"
        );
    }

    drop(conn);
    h.quit();
}

#[test]
fn super_arrows_tile_the_focused_window_to_half_the_work_area() {
    let mut h = Harness::start("tile", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("tile");
    let mut win = make_window(&mut conn, &mut inbox, 1, "tile", WIN, RED, 0, 1);
    h.settle();

    h.key(KEY_LEFTMETA, true);
    h.key(KEY_LEFT, true);
    h.key(KEY_LEFT, false);
    h.key(KEY_LEFTMETA, false);
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the left tile");
    let left = win.frame(true);
    assert_eq!(
        (left.x, left.y, left.w, left.h),
        (0.0, 0.0, OUT.0 as f32 / 2.0, OUT.1 as f32)
    );

    h.key(KEY_LEFTMETA, true);
    h.key(KEY_RIGHT, true);
    h.key(KEY_RIGHT, false);
    h.key(KEY_LEFTMETA, false);
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the right tile");
    let right = win.frame(true);
    assert_eq!(right.x, OUT.0 as f32 / 2.0);
    assert_eq!(right.w, left.w, "the halves are the same size");
    assert_eq!(left.x + left.w, right.x, "and they meet exactly");

    drop(conn);
    h.quit();
}

/// The server's own contribution to `nodes`, per decorated window.
///
/// #538 measured ~50 nodes per decorated window on the box and asked what
/// a frame actually contains. It was six; since #3715 it is **eleven** —
/// the frame group, the background, the bar, the application icon, the
/// title, and three buttons of a disc plus a glyph each — and the rest of
/// that number was the client's own tree, which the server does not
/// choose. Pinned here because `nodes` is what `docs/budget.md`
/// multiplies by the 240 bytes a `Node` costs: a frame that quietly grew
/// to twenty nodes would move the budget line without anyone noticing.
///
/// The +5 is argued in `wm::build_frame`'s documentation, and the one
/// alternative worth naming is recorded there too: a *single* hover disc
/// moved between the buttons would be nine, and was not taken because it
/// would damage two rectangles per hover instead of one.
#[test]
fn a_frame_costs_eleven_scene_nodes_and_a_fixed_window_nine() {
    let mut h = Harness::start("framecost", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("framecost");

    let empty = h.stat("nodes");
    assert_eq!(empty, 0, "an empty desktop holds no nodes of its own");

    // One client window of exactly two nodes: a root group and one rect.
    let _win = make_window(&mut conn, &mut inbox, 1, "Hello", WIN, RED, 0, 1);
    park(&mut h);
    assert_eq!(h.stat("decorated"), 1);
    assert_eq!(
        h.stat("nodes") - empty,
        13,
        "two client nodes plus an eleven-node frame"
    );

    // A FIXED_SIZE window has no maximize button, so its frame is nine:
    // one disc and one glyph fewer.
    let mut c2 = h.client("framecost2");
    let mut in2 = Inbox::default();
    let _fixed = make_window(
        &mut c2,
        &mut in2,
        10,
        "Fixed",
        WIN,
        GREEN,
        window_flags::FIXED_SIZE,
        1,
    );
    park(&mut h);
    assert_eq!(h.stat("decorated"), 2);
    assert_eq!(
        h.stat("nodes") - empty,
        13 + 11,
        "the second window adds two of its own plus a nine-node frame"
    );

    // An undecorated window pays nothing: the server adds no node at all.
    let mut c3 = h.client("framecost3");
    let mut in3 = Inbox::default();
    let _bare = make_window(
        &mut c3,
        &mut in3,
        20,
        "Bare",
        WIN,
        BLUE,
        window_flags::UNDECORATED,
        1,
    );
    park(&mut h);
    assert_eq!(h.stat("decorated"), 2, "the third window opted out");
    assert_eq!(
        h.stat("nodes") - empty,
        13 + 11 + 2,
        "an undecorated window is its own two nodes and nothing else"
    );

    drop(conn);
    drop(c2);
    drop(c3);
    h.quit();
}

/// #3713: a **hidden** overlay swallowed every frame interaction.
///
/// Reported from the box as "I cannot move windows" right after a server
/// restart, with the title-bar buttons dead too while content clicks
/// still worked. That split is the whole diagnosis: a content click goes
/// through `pointer.over`, which the scene's own hit test fills in and
/// which honours node visibility; a title press, a button and a resize
/// band all go through `frame_hit`, which walked the z-order skipping
/// only `Minimized` windows and never asked whether the window was
/// *showing*.
///
/// The launcher is exactly that window: a centred 600x400 `Overlay`
/// created visible and hidden on the loop's first turn (`SetVisible`,
/// not a state change), sitting in front of everything on the desktop.
/// Every press on a window under it hit-tested to the invisible overlay's
/// `Region::Content`, so the window manager saw a content hit and did
/// nothing — `dragging` stayed 0. It "started working" later on the box
/// because a real desktop eventually moves the window out from under the
/// centred 600x400 rectangle.
///
/// On `8d31cc3` this fails at the first assertion with `dragging 0` and a
/// frame that never moved.
#[test]
fn a_hidden_overlay_does_not_swallow_the_title_bar_under_it() {
    let mut h = Harness::start("hidden-overlay", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("hidden-overlay");
    let mut win = make_window(&mut conn, &mut inbox, 1, "under", WIN, RED, 0, 1);

    // A launcher-shaped overlay: shell socket, `Overlay` layer, undecorated
    // and NO_FOCUS, covering the middle of the screen — then hidden, the
    // way the real launcher hides itself before its first frame.
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("overlay");
    let overlay_root = NodeId(11);
    let overlay_rect = NodeId(12);
    let overlay_size = Size::new(600.0, 400.0);
    shell
        .tx()
        .create_window_with(
            overlay_root,
            "overlay",
            overlay_size,
            Layer::Overlay,
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
        )
        .create_rect(
            overlay_rect,
            overlay_root,
            Rect::new(0.0, 0.0, overlay_size.w, overlay_size.h),
        )
        .fill_solid(overlay_rect, GREEN)
        .commit(1)
        .unwrap();
    shell.flush().unwrap();
    let _ = expect(&mut shell, &mut shell_inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == overlay_root => Some(c.position),
        _ => None,
    });
    shell.tx().visible(overlay_root, false).commit(2).unwrap();
    shell.flush().unwrap();
    h.settle();

    // It really is off screen: the window under it is what you see.
    let (cx, cy) = win.content();
    assert_eq!(
        rgb(h.shot().pixel(cx as u32, cy as u32)),
        to_rgb(RED),
        "the hidden overlay paints nothing"
    );

    // The title bar of the window *under* the hidden overlay still drags.
    let before = win.frame(true);
    let (bx, by) = win.title_bar();
    h.point_at(bx, by, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    assert_eq!(
        h.stat("dragging"),
        1,
        "a press on the title bar starts a drag even with a hidden overlay over it"
    );
    h.point_at(bx - 40.0, by + 30.0, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    refresh(&mut conn, &mut inbox, &mut win);
    let after = win.frame(true);
    assert_eq!(
        (after.x - before.x, after.y - before.y),
        (-40.0, 30.0),
        "the window followed the pointer"
    );

    drop((conn, shell));
    h.quit();
}

/// #3713's second half: the resize band is invisible, so nobody finds it.
///
/// The band straddles the frame edge and always did — six logical pixels
/// out, and inwards as far as the frame's own border — so pressing *on*
/// the border has always resized. What was missing was any way to know
/// that: the border is one pixel wide and cursor shapes are M4, so the
/// human on the box grabbed the border, saw nothing happen anywhere near
/// it, and reported that resizing does not work.
///
/// So the border lights up in `resize_hint` while the pointer is
/// somewhere a press would resize. This asserts both halves: the colour
/// appears on hover and goes away again, and a press *on the lit border*
/// really does resize.
#[test]
fn the_frame_edge_lights_up_where_a_press_would_resize_it() {
    let mut h = Harness::start("resize-hint", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("resize-hint");
    let mut win = make_window(&mut conn, &mut inbox, 1, "hint", WIN, RED, 0, 1);
    park(&mut h);

    // The left border: one pixel wide, and the pixel the user aims at when
    // they mean "resize this". Sampled well above where the pointer will
    // hover, because the software cursor is 24 px square and would paint
    // over the very pixel under test — the whole edge lights up, so any
    // point on it answers the question.
    let f = win.frame(true);
    let (ex, ey) = (f.x as u32, (f.y + wm::TITLE_H + 6.0) as u32);
    // Inside the 1-px border, and low enough that the cursor drawn there
    // cannot reach the sample point above.
    let (hover_x, hover_y) = (f.x + 0.5, f.y + f.h - 8.0);
    assert_eq!(
        rgb(h.shot().pixel(ex, ey)),
        to_rgb(role(Role::WindowBorderActive)),
        "at rest the focused window's border is its focus colour"
    );

    // Hover the border itself — inside the window, not in the outward slop.
    h.point_at(hover_x, hover_y, OUT);
    h.settle();
    assert_eq!(
        rgb(h.shot().pixel(ex, ey)),
        to_rgb(role(Role::ResizeHint)),
        "the border says a press here would resize"
    );

    // And the affordance does not lie: pressing there starts a resize.
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    assert_eq!(h.stat("dragging"), 1, "the lit border really is grabbable");
    h.point_at(hover_x - 30.0, hover_y, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the left-edge resize");
    assert_eq!(
        win.frame(true).w,
        f.w + 30.0,
        "a left-edge drag grew the window leftwards"
    );

    // Hovering in and out of the band is a *colour* change and nothing
    // else: the title cannot have changed, so nothing may be re-shaped.
    // The first cut of this feature restyled through the full `restyle`,
    // which elides (a binary search of `measure` calls) and re-shapes on
    // every call, so wiggling the pointer over a window edge did text
    // layout — on the motion path, and into `shape_us_mean`. A monotonic
    // counter is what can say "none at all"; a mean cannot, because the
    // work is real work and the average stays plausible.
    let layouts = h.stat("text_layouts");
    for _ in 0..3 {
        h.point_at(hover_x, hover_y, OUT);
        h.settle();
        h.point_at(f.x + 40.0, hover_y, OUT);
        h.settle();
    }
    assert_eq!(
        h.stat("text_layouts"),
        layouts,
        "crossing a resize band must not shape any text"
    );

    // Move away and the border goes back to its focus colour: the hint is
    // a hover state, not a new look.
    park(&mut h);
    let f = win.frame(true);
    let (ex, ey) = (f.x as u32, (f.y + wm::TITLE_H + 6.0) as u32);
    assert_eq!(
        rgb(h.shot().pixel(ex, ey)),
        to_rgb(role(Role::WindowBorderActive)),
        "the hint goes away with the pointer"
    );

    drop(conn);
    h.quit();
}

/// A window that *cannot* be resized must not offer to be.
///
/// `hit_frame` gives a `FIXED_SIZE` window no bands at all, so there is
/// nothing to advertise; lighting its border would be an affordance that
/// does nothing, which is worse than none. The same rule covers a
/// maximized window, whose geometry the window manager owns.
#[test]
fn a_fixed_size_window_never_lights_its_border() {
    let mut h = Harness::start("hint-fixed", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("hint-fixed");
    let win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "fixed",
        WIN,
        RED,
        window_flags::FIXED_SIZE,
        1,
    );
    park(&mut h);

    // Sampled clear of the pointer, which paints a 24 px cursor over
    // whatever it hovers.
    let f = win.frame(true);
    let (ex, ey) = (f.x as u32, (f.y + wm::TITLE_H + 6.0) as u32);
    h.point_at(f.x + 0.5, f.y + f.h - 8.0, OUT);
    h.settle();
    assert_eq!(
        rgb(h.shot().pixel(ex, ey)),
        to_rgb(role(Role::WindowBorderActive)),
        "a fixed-size window's border promises nothing"
    );
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    assert_eq!(h.stat("dragging"), 0, "and there is nothing to grab");
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    drop(conn);
    h.quit();
}

// ---------------------------------------------------------------------
// #3715: the frame's icons — the app icon, the three buttons, the hover
// ---------------------------------------------------------------------

/// The pixels of a rectangle, as `0x00rrggbb`.
fn crop(img: &Image, r: Rect) -> Vec<u32> {
    let mut out = Vec::new();
    for y in r.y as u32..(r.y + r.h) as u32 {
        for x in r.x as u32..(r.x + r.w) as u32 {
            out.push(rgb(img.pixel(x, y)));
        }
    }
    out
}

/// How many pixels of `rect` are exactly `want`.
fn census(img: &Image, rect: Rect, want: u32) -> usize {
    crop(img, rect).into_iter().filter(|p| *p == want).count()
}

/// Squared distance between two `0x00rrggbb` colours.
///
/// For the assertions that cannot use exact equality because the pixel
/// under test is anti-aliased: an icon's strokes are blended with
/// whatever is behind them, so "how far is this from the background" is
/// the honest question, not "is it exactly the tint".
fn distance(a: u32, b: u32) -> i64 {
    let ch = |v: u32, s: u32| i64::from((v >> s) & 0xff);
    let (dr, dg, db) = (
        ch(a, 16) - ch(b, 16),
        ch(a, 8) - ch(b, 8),
        ch(a, 0) - ch(b, 0),
    );
    dr * dr + dg * dg + db * db
}

/// Whether `px` could be `fg` composited over `bg` at some coverage.
///
/// Every channel has to lie between the two **and** imply the same
/// coverage to within a rounding step: a pixel that is 70 % of one tint
/// is not 70 % of another, so this is what tells two tints apart on
/// anti-aliased artwork where nothing is ever the full colour.
fn blend_of(px: u32, bg: u32, fg: u32) -> bool {
    let ch = |v: u32, s: u32| f32::from(((v >> s) & 0xff) as u8);
    let mut alphas = Vec::new();
    for s in [16u32, 8, 0] {
        let (p, b, f) = (ch(px, s), ch(bg, s), ch(fg, s));
        if (f - b).abs() < 1.0 {
            // This channel says nothing: the two colours agree on it.
            continue;
        }
        let a = (p - b) / (f - b);
        if !(-0.02..=1.02).contains(&a) {
            return false;
        }
        alphas.push(a);
    }
    let Some(first) = alphas.first().copied() else {
        return px == bg;
    };
    // 2/255 per channel is the widest a rounding difference can be.
    alphas.iter().all(|a| (a - first).abs() < 0.02)
}

/// The button rectangle a region names, in desktop coordinates.
fn button_rect(win: &Win, region: wm::Region, fixed: bool) -> Rect {
    wm::buttons(win.frame(true), fixed)
        .into_iter()
        .find(|(r, _)| *r == region)
        .unwrap_or_else(|| panic!("no {region:?} button"))
        .1
}

/// A title bar draws an application icon and three symbolic buttons, and
/// none of them is a coloured disc.
///
/// The frame is the server's own tree, so this is also the test that the
/// internal icon path works at all: there is no `SetIcon` on the wire
/// for any of these four nodes.
#[test]
fn a_title_bar_draws_an_app_icon_and_three_symbolic_buttons() {
    let mut h = Harness::start("frameicons", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("frameicons");
    let win = make_window(&mut conn, &mut inbox, 1, "Hello", WIN, RED, 0, 1);
    park(&mut h);

    let f = win.frame(true);
    let img = h.shot();
    let bar_rgb = to_rgb(bar(true));

    // The application icon: ink where the artwork is, in the box
    // `layout_frame` reserved for it. `app_id` is unset on this window,
    // so what is drawn is the `window` fallback — resolved by the server
    // itself, synchronously, with no round trip for the client to make.
    let icon_box = Rect::new(
        f.x + wm::BUTTON_GAP,
        f.y + (wm::TITLE_H - wm::APP_ICON) / 2.0,
        wm::APP_ICON,
        wm::APP_ICON,
    );
    let ink = crop(&img, icon_box)
        .into_iter()
        .filter(|p| *p != bar_rgb)
        .count();
    assert!(ink > 8, "only {ink} px of app icon in a 16x16 box");

    // Three buttons, each with ink and no disc. "Ink" is the
    // discriminator that a coloured rect would also satisfy, so the disc
    // is ruled out separately: a filled 14 px circle is ~150 px of one
    // colour, and a glyph is a handful of strokes.
    for region in [
        wm::Region::Close,
        wm::Region::Maximize,
        wm::Region::Minimize,
    ] {
        let rect = button_rect(&win, region, false);
        let px = crop(&img, rect);
        let ink = px.iter().filter(|p| **p != bar_rgb).count();
        assert!(ink > 4, "{region:?} drew only {ink} px");
        assert!(
            ink < 80,
            "{region:?} covered {ink} of {} px — that is a disc, not a glyph",
            px.len()
        );
        // And the corners are bar colour: nothing is painted behind it.
        assert_eq!(
            rgb(img.pixel(rect.x as u32, rect.y as u32)),
            bar_rgb,
            "{region:?} has a background at rest"
        );
    }

    // The old look is gone from the resting frame: neither button
    // colour appears anywhere in the title bar.
    let bar_rect = Rect::new(f.x, f.y, f.w, wm::TITLE_H);
    for r in [Role::TitleClose, Role::TitleMaximize] {
        assert_eq!(
            census(&img, bar_rect, to_rgb(role(r))),
            0,
            "{} is still painted at rest",
            r.key()
        );
    }

    drop(conn);
    h.quit();
}

/// Hovering a button paints its disc; moving off it takes the disc away.
///
/// Settled on a **pixel census** of the button's own rectangle rather
/// than on one sample, because one sample inside a glyph stroke would
/// answer about the glyph and one outside it would answer about the bar.
/// The disc is the thing that appears, so its area is what is counted.
#[test]
fn hovering_a_frame_button_paints_a_disc_and_leaving_it_takes_it_away() {
    let mut h = Harness::start("framehover", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("framehover");
    let win = make_window(&mut conn, &mut inbox, 1, "Hover", WIN, RED, 0, 1);
    park(&mut h);

    let close = button_rect(&win, wm::Region::Close, false);
    let minimize = button_rect(&win, wm::Region::Minimize, false);
    let red = to_rgb(role(Role::TitleClose));
    let hover = to_rgb(role(Role::TitleButtonHover));

    assert_eq!(census(&h.shot(), close, red), 0, "no disc at rest");

    // The pointer paints a 24 px cursor over whatever it hovers, so it
    // is parked at the button's **bottom-right** inside corner: still
    // inside the hit region (`contains` is half-open on the far edges),
    // and the cursor's body then extends down and right, away from the
    // rectangle the census counts.
    let corner = |r: Rect| (r.x + r.w - 1.0, r.y + r.h - 1.0);
    let (cx, cy) = corner(close);
    h.point_at(cx, cy, OUT);
    h.settle();
    let lit = census(&h.shot(), close, red);
    assert!(lit > 60, "the close disc is only {lit} px");
    assert_eq!(
        census(&h.shot(), minimize, hover),
        0,
        "a hover lit a button the pointer is not on"
    );

    // Slide to minimize: its disc lights, close's goes out. The two are
    // different roles, which is the whole reason close keeps the red.
    let (mx, my) = corner(minimize);
    h.point_at(mx, my, OUT);
    h.settle();
    let img = h.shot();
    assert!(
        census(&img, minimize, hover) > 60,
        "the minimize disc is only {} px",
        census(&img, minimize, hover)
    );
    assert_eq!(census(&img, close, red), 0, "close went out");

    // Off the frame entirely and every disc is gone.
    park(&mut h);
    let img = h.shot();
    assert_eq!(census(&img, close, red), 0);
    assert_eq!(census(&img, minimize, hover), 0);

    // A hover is a *colour* change and nothing else, exactly as the
    // resize hint is (#3713): no text may be shaped and no icon may be
    // rasterised for it. A monotonic counter is what can say "none at
    // all"; a mean cannot.
    let layouts = h.stat("text_layouts");
    let renders = h.stat("icon_renders");
    for _ in 0..3 {
        h.point_at(cx, cy, OUT);
        h.settle();
        h.point_at(mx, my, OUT);
        h.settle();
        park(&mut h);
    }
    assert_eq!(
        h.stat("text_layouts"),
        layouts,
        "hovering a frame button shaped text"
    );
    assert_eq!(
        h.stat("icon_renders"),
        renders,
        "a tint is a role index, not a raster: hovering re-rasterised an icon"
    );

    drop(conn);
    h.quit();
}

/// The minimize button puts the window away, and `Alt+Tab` brings it
/// back — the action `Region::Minimize` has had since M3-A, with a
/// button on it at last.
#[test]
fn the_minimize_button_minimizes_and_alt_tab_brings_it_back() {
    let mut h = Harness::start("minbutton", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("minbutton");
    let a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);
    let mut b = make_window(&mut conn, &mut inbox, 3, "b", WIN, GREEN, 0, 2);
    park(&mut h);
    let (bx, by) = b.content();
    assert_eq!(rgb(h.shot().pixel(bx as u32, by as u32)), to_rgb(GREEN));

    // Press and release inside the button: an action fires on release
    // *inside itself*, which is what lets a user change their mind.
    let rect = button_rect(&b, wm::Region::Minimize, false);
    h.point_at(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();

    assert_eq!(h.stat("minimized"), 1, "the button minimized it");
    assert_eq!(h.stat("windows"), 2, "still a window, just hidden");
    park(&mut h);
    assert_ne!(
        rgb(h.shot().pixel(bx as u32, by as u32)),
        to_rgb(GREEN),
        "its pixels are gone"
    );

    h.key(KEY_LEFTALT, true);
    h.key(KEY_TAB, true);
    h.key(KEY_TAB, false);
    h.key(KEY_LEFTALT, false);
    h.settle();
    await_focus(&mut conn, &mut inbox, b.root, "Alt+Tab reached it");
    refresh(&mut conn, &mut inbox, &mut b);
    park(&mut h);
    assert_eq!(h.stat("minimized"), 0, "Alt+Tab un-minimized it");
    assert_eq!(rgb(h.shot().pixel(bx as u32, by as u32)), to_rgb(GREEN));
    let _ = a;

    drop(conn);
    h.quit();
}

/// Sliding off a button before releasing cancels it, for all three.
///
/// The property is not new — it is what `Drag::Button` is for — but the
/// third button is, and a minimize that fired on press would be a window
/// the user cannot stop putting away.
#[test]
fn a_frame_button_fires_only_on_a_release_inside_itself() {
    let mut h = Harness::start("buttoncancel", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("buttoncancel");
    let win = make_window(&mut conn, &mut inbox, 1, "cancel", WIN, RED, 0, 1);
    park(&mut h);

    for region in [
        wm::Region::Close,
        wm::Region::Maximize,
        wm::Region::Minimize,
    ] {
        let rect = button_rect(&win, region, false);
        let before = win.frame(true);
        h.point_at(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, OUT);
        h.settle();
        h.button(BTN_LEFT, ButtonState::Pressed);
        h.settle();
        // Slide off, onto the title bar, and let go there.
        let (tx, ty) = win.title_bar();
        h.point_at(tx, ty, OUT);
        h.settle();
        h.button(BTN_LEFT, ButtonState::Released);
        h.settle();
        assert_eq!(
            h.stat("minimized"),
            0,
            "{region:?} minimized on a slide-off"
        );
        assert_eq!(h.stat("windows"), 1, "{region:?} closed on a slide-off");
        // There is no `maximized` counter, so the geometry is the
        // observable: a maximize would have filled the work area.
        let mut after = win;
        refresh(&mut conn, &mut inbox, &mut after);
        assert_eq!(
            after.frame(true),
            before,
            "{region:?} changed the geometry on a slide-off"
        );
    }

    drop(conn);
    h.quit();
}

/// A drag repaints the frame and re-lays it out, and must rasterise
/// nothing: not a glyph, not an icon.
///
/// `text_layouts` was already pinned by #3713's review; `icon_renders`
/// is the same claim for the four icons #3715 added, and it is the one
/// that would catch a `layout_frame` that re-sent the icons with a new
/// size on every motion.
#[test]
fn dragging_a_frame_rasterises_neither_text_nor_icons() {
    let mut h = Harness::start("dragicons", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("dragicons");
    let mut win = make_window(&mut conn, &mut inbox, 1, "Drag me", WIN, RED, 0, 1);
    park(&mut h);

    let layouts = h.stat("text_layouts");
    let renders = h.stat("icon_renders");
    let cached = h.stat("icons_cached");
    let frames = h.stat("frames");

    let (tx, ty) = win.title_bar();
    h.point_at(tx, ty, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    for i in 1..=30 {
        h.point_at(tx + i as f32 * 2.0, ty + i as f32, OUT);
        h.settle();
    }
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the drag");

    assert!(
        h.stat("frames") > frames + 10,
        "the window really did repaint: {} frames for 30 motions",
        h.stat("frames") - frames
    );
    assert_eq!(h.stat("text_layouts"), layouts, "a drag shaped text");
    assert_eq!(h.stat("icon_renders"), renders, "a drag rasterised an icon");
    assert_eq!(h.stat("icons_cached"), cached);

    drop(conn);
    h.quit();
}

/// A focus change retints the frame's symbolic icons without
/// re-rasterising them, exactly as it retints the title.
#[test]
fn a_focus_change_retints_the_frames_icons_with_no_raster() {
    let mut h = Harness::start("framefocus", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("framefocus");
    let a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, 1);
    park(&mut h);
    let close = button_rect(&a, wm::Region::Close, false);
    let focused = crop(&h.shot(), close);
    let renders = h.stat("icon_renders");

    // A second window takes the focus, so `a` is now inactive.
    let b = make_window(&mut conn, &mut inbox, 3, "b", WIN, GREEN, 0, 2);
    park(&mut h);
    let unfocused = crop(&h.shot(), close);
    assert_ne!(
        focused, unfocused,
        "an unfocused frame's glyphs did not recede with its title"
    );
    // Which colours they moved *between*. Not an exact-equality census:
    // a 10 px glyph drawn from a 16-unit grid is anti-aliased
    // everywhere, so **no** pixel of it is the full tint — the deepest
    // ink in the focused crop measures 70 % coverage, which is a blend
    // that happens to land close to the *inactive* title colour. A
    // nearest-colour test would therefore report the opposite of the
    // truth, which is what this assertion did first.
    //
    // What is true of a blend and of nothing else: every channel lies
    // between the background and the tint. That separates the two cases
    // cleanly here, because the focused ink is darker than the inactive
    // tint ever gets on its own lighter bar.
    let deepest = |px: &[u32], bg: u32| {
        *px.iter()
            .max_by_key(|p| distance(**p, bg))
            .expect("the button box is not empty")
    };
    let ink = deepest(&focused, to_rgb(bar(true)));
    assert!(
        blend_of(ink, to_rgb(bar(true)), to_rgb(role(Role::TitleTextActive))),
        "the focused glyph's deepest ink {ink:06x} is not \
         title_text_active over title_bar_active"
    );
    assert!(
        !blend_of(
            ink,
            to_rgb(bar(false)),
            to_rgb(role(Role::TitleTextInactive))
        ),
        "and it is not the inactive pair either, so the test discriminates"
    );
    let ink = deepest(&unfocused, to_rgb(bar(false)));
    assert!(
        blend_of(
            ink,
            to_rgb(bar(false)),
            to_rgb(role(Role::TitleTextInactive))
        ),
        "the unfocused glyph's deepest ink {ink:06x} is not \
         title_text_inactive over title_bar_inactive"
    );
    // And nothing was rasterised for any of it: the cache holds
    // coverage, and a tint is a role index the painter resolves per
    // frame (`docs/icons.md`).
    assert_eq!(
        h.stat("icon_renders"),
        renders,
        "a focus change re-rasterised the frame's icons"
    );
    let _ = b;

    drop(conn);
    h.quit();
}

/// An `app_id` that changes **after** the window is mapped re-resolves
/// the frame's icon.
///
/// The protocol allows it and `nitro-term` uses it — a terminal learns
/// what it is running after it has a window — so a frame that resolved
/// once at map time would keep the generic fallback for the rest of the
/// session. The whole thing is the server's own tree, so there is no
/// message to the client in either direction.
///
/// The app id has to reach the frame's icon **through the `.desktop`
/// hop**, which is the only way an app id becomes a shape: an
/// `AS_COLOURED` request never consults the symbolic set for the name it
/// was given (that is the selector rule `docs/icons.md` argues for), so
/// a bare `set_app_id("terminal")` against a server with no applications
/// directory resolves to nothing and keeps the fallback — which is what
/// the first version of this test measured, and mistook for a bug.
#[test]
fn an_app_id_set_after_mapping_re_resolves_the_frame_icon() {
    let dir = std::env::temp_dir().join(format!("nitro-wm-apps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("applications dir");
    std::fs::write(
        dir.join("nitro-term.desktop"),
        "[Desktop Entry]\nType=Application\nName=Terminal\nIcon=terminal\n",
    )
    .expect("a desktop entry");
    let mut h = Harness::start_with("frameappid", OUT.0, OUT.1, |config| {
        config.desktop_dirs = Some(vec![dir.clone()]);
    });
    let mut inbox = Inbox::default();
    let mut conn = h.client("frameappid");
    let win = make_window(&mut conn, &mut inbox, 1, "Term", WIN, RED, 0, 1);
    park(&mut h);

    let f = win.frame(true);
    let icon_box = Rect::new(
        f.x + wm::BUTTON_GAP,
        f.y + (wm::TITLE_H - wm::APP_ICON) / 2.0,
        wm::APP_ICON,
        wm::APP_ICON,
    );
    // No app id yet, so this is the `window` fallback — the server's own
    // synchronous answer, with no `BadIcon` and no round trip.
    let before = crop(&h.shot(), icon_box);
    let bar_rgb = to_rgb(bar(true));
    assert!(
        before.iter().any(|p| *p != bar_rgb),
        "the fallback drew nothing"
    );
    let renders = h.stat("icon_renders");
    assert_eq!(h.stat("app_icon_indirections"), 0);

    conn.tx()
        .set_app_id(win.root, "nitro-term")
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    park(&mut h);

    let after = crop(&h.shot(), icon_box);
    assert_ne!(
        before, after,
        "the frame kept its old icon after the app id changed"
    );
    assert!(
        h.stat("icon_renders") > renders,
        "a genuinely different shape was rasterised"
    );
    assert_eq!(
        h.stat("app_icon_indirections"),
        1,
        "and it came through the .desktop entry, which is the only way an \
         app id becomes a shape"
    );
    // The client heard nothing about any of it: the frame is not its
    // tree, and a `SetAppId` earns no reply.
    conn.flush().unwrap();
    let _ = conn.poll(&mut inbox.0);
    assert!(
        !inbox.0.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "the server complained to the client: {:?}",
        inbox.0
    );

    drop(conn);
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}
