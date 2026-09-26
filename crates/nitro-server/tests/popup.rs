//! M5-G popups, end to end on the fake backend: placement against the
//! anchor and the work area, the constraint adjustments, the no-anchor
//! fallback, the pointer grab, Escape, chains, authorization, and what
//! happens when the parent goes away.
//!
//! The pure geometry (every anchor × gravity, each constraint bit) is
//! unit-tested in `src/popup.rs`; this file is about whether the server
//! wires it up — the parent's content origin, the work area, the
//! `Configure`, the grab and the dismissal ordering.

// Every geometry number here is whole-pixel arithmetic on whole-pixel
// inputs, so equality is the assertion that means what it says.
#![allow(clippy::float_cmp)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Point, Rect, Size};
use nitro_kms::Image;
use nitro_server::input::{BTN_LEFT, FakeInput, InputEvent};
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{CreatePopup, ServerMsg};
use nitro_wire::types::{
    ButtonState, ErrorCode, Layer, NodeId, PopupAnchor, PopupGravity, caps, constraint_adjust,
    popup_flags, window_flags,
};

/// evdev `KEY_ESC`.
const KEY_ESC: u32 = 1;
/// evdev `KEY_A`.
const KEY_A: u32 = 30;

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
        let dir = std::env::temp_dir().join(format!("nitro-popup-{}-{name}", std::process::id()));
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
#[derive(Default)]
struct Inbox(Vec<ServerMsg>);

impl Inbox {
    /// Pull whatever has arrived without waiting for anything.
    fn pump(&mut self, conn: &mut Connection) {
        conn.flush().unwrap();
        let _ = conn.poll(&mut self.0);
    }

    /// The popups this client was told are done, in arrival order.
    fn done(&self) -> Vec<NodeId> {
        self.0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::PopupDone(d) => Some(d.popup),
                _ => None,
            })
            .collect()
    }

    fn buttons(&self, window: NodeId) -> Vec<ButtonState> {
        self.0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::PointerButton(b) if b.window == window => Some(b.state),
                _ => None,
            })
            .collect()
    }
}

/// A window or popup: its node id and where the server put its content.
#[derive(Clone, Copy, Debug)]
struct Win {
    root: NodeId,
    pos: Point,
    size: Size,
}

impl Win {
    fn centre(&self) -> (f32, f32) {
        (
            self.pos.x + self.size.w / 2.0,
            self.pos.y + self.size.h / 2.0,
        )
    }
}

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
const MENU: Size = Size::new(60.0, 40.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);
const GREEN: Color = Color::rgb(0x00, 0xFF, 0x00);
const BLUE: Color = Color::rgb(0x00, 0x00, 0xFF);

fn rgb(px: u32) -> u32 {
    px & 0x00ff_ffff
}

fn to_rgb(c: Color) -> u32 {
    u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

/// A client that opted into popups, the way every conformant one must.
fn popup_client(h: &Harness, name: &str) -> Connection {
    let mut conn = h.client(name);
    assert!(conn.has_caps(caps::POPUP), "the server advertises POPUP");
    conn.client_caps(caps::POPUP).unwrap();
    conn.flush().unwrap();
    conn
}

fn configure_of(conn: &mut Connection, inbox: &mut Inbox, root: NodeId) -> Win {
    let (pos, size) = expect(conn, &mut inbox.0, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some((c.position, c.size)),
        _ => None,
    });
    Win { root, pos, size }
}

/// An undecorated toplevel filled with one colour, so content coordinates
/// are screen coordinates and nothing about the frame is in play.
fn make_window(
    conn: &mut Connection,
    inbox: &mut Inbox,
    id: u32,
    color: Color,
    serial: u32,
) -> Win {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, "w", WIN, Layer::Normal, window_flags::UNDECORATED)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, color)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    configure_of(conn, inbox, root)
}

/// What a test says about one popup.
#[derive(Clone, Copy)]
struct Spec {
    anchor_rect: IRect,
    anchor: PopupAnchor,
    gravity: PopupGravity,
    constraint: u32,
    flags: u32,
}

impl Spec {
    /// A menu dropping down from the bottom-left of `anchor_rect`.
    fn menu(anchor_rect: IRect) -> Self {
        Self {
            anchor_rect,
            anchor: PopupAnchor::BottomLeft,
            gravity: PopupGravity::BottomRight,
            constraint: 0,
            flags: popup_flags::GRAB,
        }
    }
}

fn send_popup(conn: &mut Connection, id: u32, parent: NodeId, spec: Spec, serial: u32) {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_popup(CreatePopup {
            id: root,
            parent,
            anchor_rect: spec.anchor_rect,
            anchor: spec.anchor,
            gravity: spec.gravity,
            constraint: spec.constraint,
            size: MENU,
            flags: spec.flags,
        })
        .create_rect(rect, root, Rect::new(0.0, 0.0, MENU.w, MENU.h))
        .fill_solid(rect, GREEN)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
}

fn make_popup(
    conn: &mut Connection,
    inbox: &mut Inbox,
    id: u32,
    parent: NodeId,
    spec: Spec,
    serial: u32,
) -> Win {
    send_popup(conn, id, parent, spec, serial);
    configure_of(conn, inbox, NodeId(id))
}

/// Wait until every popup in `want` has been reported done, in that order
/// relative to each other.
fn await_done(conn: &mut Connection, inbox: &mut Inbox, want: &[NodeId]) {
    wait_for("PopupDone", || {
        inbox.pump(conn);
        let done = inbox.done();
        want.iter().all(|w| done.contains(w))
    });
    let done = inbox.done();
    let order: Vec<NodeId> = done.iter().copied().filter(|d| want.contains(d)).collect();
    assert_eq!(order, want, "PopupDone order");
}

fn park(h: &mut Harness) {
    h.point_at(OUT.0 as f32 - 2.0, OUT.1 as f32 - 2.0, OUT);
    h.settle();
}

fn click(h: &mut Harness, at: (f32, f32)) {
    h.point_at(at.0, at.1, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Released);
    h.settle();
}

fn green_at(img: &Image, x: f32, y: f32) -> bool {
    rgb(img.pixel(x as u32, y as u32)) == to_rgb(GREEN)
}

// ------------------------------------------------------------------ tests

#[test]
fn a_popup_without_client_caps_is_refused() {
    let h = Harness::start("nocaps", OUT.0, OUT.1);
    let mut conn = h.client("nocaps");
    let mut inbox = Inbox::default();
    assert!(conn.has_caps(caps::POPUP));
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    // Never sent `ClientCaps`.
    send_popup(
        &mut conn,
        10,
        parent.root,
        Spec::menu(IRect::new(0, 0, 10, 10)),
        2,
    );
    let code = expect(&mut conn, &mut inbox.0, "Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::Protocol);
    h.quit();
}

#[test]
fn anchor_and_gravity_place_the_popup_relative_to_the_parents_content() {
    let mut h = Harness::start("place", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "place");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);

    let table = [
        (
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            10.0,
            30.0,
        ),
        (
            PopupAnchor::TopRight,
            PopupGravity::TopLeft,
            50.0 - 60.0,
            10.0 - 40.0,
        ),
        (
            PopupAnchor::None,
            PopupGravity::None,
            30.0 - 30.0,
            20.0 - 20.0,
        ),
        (PopupAnchor::Right, PopupGravity::Right, 50.0, 20.0 - 20.0),
    ];
    for (i, (anchor, gravity, dx, dy)) in table.into_iter().enumerate() {
        let spec = Spec {
            anchor_rect: IRect::new(10, 10, 40, 20),
            anchor,
            gravity,
            constraint: 0,
            flags: 0,
        };
        let id = 100 + 10 * i as u32;
        let p = make_popup(&mut conn, &mut inbox, id, parent.root, spec, 2 + i as u32);
        assert_eq!(
            (p.pos.x, p.pos.y),
            (parent.pos.x + dx, parent.pos.y + dy),
            "{anchor:?}/{gravity:?}"
        );
        assert_eq!(p.size, MENU);
        conn.tx()
            .destroy_node(NodeId(id))
            .commit(50 + i as u32)
            .unwrap();
        conn.flush().unwrap();
    }
    h.quit();
}

#[test]
fn a_popup_overflows_its_parents_bounds() {
    let mut h = Harness::start("overflow", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "overflow");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    // Anchored on the parent's bottom edge, dropping down: every pixel of
    // it is outside the parent, where the parent's content clip would
    // have cut it off if a popup were a node inside the window.
    let spec = Spec::menu(IRect::new(10, WIN.h as i32 - 10, 40, 10));
    let p = make_popup(&mut conn, &mut inbox, 10, parent.root, spec, 2);
    assert_eq!(p.pos.y, parent.pos.y + WIN.h);
    h.settle();
    let img = h.shot();
    assert!(
        green_at(&img, p.pos.x + 5.0, p.pos.y + 5.0),
        "the popup paints outside its parent"
    );
    assert!(green_at(&img, p.pos.x + 5.0, p.pos.y + MENU.h - 2.0));
    h.quit();
}

/// Make a parent and one popup under `spec`, return the popup's rectangle.
fn constrained(name: &str, spec: Spec) -> (Win, Win) {
    let h = Harness::start(name, OUT.0, OUT.1);
    let mut conn = popup_client(&h, name);
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    let p = make_popup(&mut conn, &mut inbox, 10, parent.root, spec, 2);
    h.quit();
    (parent, p)
}

/// The parent is centred on a 640x480 output: content at (220, 180).
/// These anchor rectangles are chosen relative to that to push the popup
/// over exactly one edge.
const PARENT: Point = Point::new(220.0, 180.0);

#[test]
fn an_unconstrained_popup_is_allowed_to_overflow() {
    let spec = Spec {
        anchor: PopupAnchor::BottomRight,
        ..Spec::menu(IRect::new(410, 10, 20, 10))
    };
    let (parent, p) = constrained("free", spec);
    assert_eq!(parent.pos, PARENT);
    assert_eq!(p.pos.x, 650.0, "past the right edge, as asked");
}

#[test]
fn flip_x_triggers_at_the_right_edge() {
    let spec = Spec {
        anchor: PopupAnchor::BottomRight,
        constraint: constraint_adjust::FLIP_X,
        ..Spec::menu(IRect::new(410, 10, 20, 10))
    };
    let (_, p) = constrained("flipx", spec);
    // Hangs off the anchor's left edge instead, growing left.
    assert_eq!(p.pos.x, 220.0 + 410.0 - MENU.w);
}

#[test]
fn flip_y_triggers_at_the_bottom_edge() {
    let spec = Spec {
        constraint: constraint_adjust::FLIP_Y,
        ..Spec::menu(IRect::new(10, 280, 40, 10))
    };
    let (_, p) = constrained("flipy", spec);
    assert_eq!(p.pos.y, 180.0 + 280.0 - MENU.h);
}

#[test]
fn a_flip_that_does_not_help_keeps_the_original_side() {
    // A 30px-tall anchor near the bottom, but anchored so that the flipped
    // side would be off the top of the screen as well.
    let spec = Spec {
        anchor_rect: IRect::new(10, -170, 40, 460),
        anchor: PopupAnchor::BottomLeft,
        gravity: PopupGravity::BottomRight,
        constraint: constraint_adjust::FLIP_Y,
        flags: 0,
    };
    let (_, p) = constrained("noflip", spec);
    // Down from y = 180 - 170 + 460 = 470: overflows, and up from 10 would
    // too, so it stays put.
    assert_eq!(p.pos.y, 470.0);
}

#[test]
fn slide_x_triggers_at_the_right_edge() {
    let spec = Spec {
        anchor: PopupAnchor::BottomRight,
        constraint: constraint_adjust::SLIDE_X,
        ..Spec::menu(IRect::new(410, 10, 20, 10))
    };
    let (_, p) = constrained("slidex", spec);
    assert_eq!(p.pos.x, 640.0 - MENU.w);
}

#[test]
fn slide_y_triggers_at_the_bottom_edge() {
    let spec = Spec {
        constraint: constraint_adjust::SLIDE_Y,
        ..Spec::menu(IRect::new(10, 280, 40, 10))
    };
    let (_, p) = constrained("slidey", spec);
    assert_eq!(p.pos.y, 480.0 - MENU.h);
}

#[test]
fn resize_x_triggers_at_the_right_edge() {
    let spec = Spec {
        constraint: constraint_adjust::RESIZE_X,
        ..Spec::menu(IRect::new(390, 10, 20, 10))
    };
    let (_, p) = constrained("resizex", spec);
    assert_eq!(p.pos.x, 610.0);
    assert_eq!(p.size, Size::new(30.0, MENU.h), "shrunk, and told so");
}

#[test]
fn resize_y_triggers_at_the_bottom_edge() {
    let spec = Spec {
        constraint: constraint_adjust::RESIZE_Y,
        ..Spec::menu(IRect::new(10, 270, 40, 10))
    };
    let (_, p) = constrained("resizey", spec);
    assert_eq!(p.pos.y, 460.0);
    assert_eq!(p.size, Size::new(MENU.w, 20.0));
}

#[test]
fn the_no_anchor_fallback_matches_the_documented_defaults() {
    // An empty anchor rectangle is the trigger. Whatever the other fields
    // say is overridden: TopLeft / BottomRight / FlipY.
    let spec = Spec {
        anchor_rect: IRect::new(15, 25, 0, 0),
        anchor: PopupAnchor::None,
        gravity: PopupGravity::TopLeft,
        constraint: 0,
        flags: 0,
    };
    let (_, p) = constrained("fallback", spec);
    assert_eq!((p.pos.x, p.pos.y), (220.0 + 15.0, 180.0 + 25.0));

    // And its FlipY is live: at the bottom of the screen it grows up.
    let spec = Spec {
        anchor_rect: IRect::new(15, 290, 0, 0),
        ..spec
    };
    let (_, p) = constrained("fallbackflip", spec);
    // Flipped to the 1x1 rectangle's *bottom-left*, growing up: 470 + 1 - 40.
    assert_eq!(p.pos.y, 180.0 + 290.0 + 1.0 - MENU.h);
}

#[test]
fn an_outside_click_dismisses_the_chain_and_is_consumed() {
    let mut h = Harness::start("outside", OUT.0, OUT.1);
    let mut a = popup_client(&h, "a");
    let mut a_in = Inbox::default();
    let parent = make_window(&mut a, &mut a_in, 1, RED, 1);
    let mut b = popup_client(&h, "b");
    let mut b_in = Inbox::default();
    let other = make_window(&mut b, &mut b_in, 1, BLUE, 1);
    park(&mut h);

    let menu = make_popup(
        &mut a,
        &mut a_in,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    let sub = make_popup(
        &mut a,
        &mut a_in,
        20,
        menu.root,
        Spec {
            anchor: PopupAnchor::TopRight,
            ..Spec::menu(IRect::new(0, 0, 60, 20))
        },
        3,
    );
    h.settle();

    // A press on the other client's window, clear of the whole chain.
    let target = (
        other.pos.x + other.size.w - 8.0,
        other.pos.y + other.size.h - 8.0,
    );
    click(&mut h, target);
    await_done(&mut a, &mut a_in, &[sub.root, menu.root]);
    b_in.pump(&mut b);
    assert!(
        b_in.buttons(other.root).is_empty(),
        "neither the press nor its release reached the other window"
    );
    let img = h.shot();
    assert!(
        !green_at(&img, menu.pos.x + 5.0, menu.pos.y + 5.0),
        "unmapped"
    );

    // The grab is gone: the next click is an ordinary one.
    click(&mut h, target);
    wait_for("the second click", || {
        b_in.pump(&mut b);
        b_in.buttons(other.root) == [ButtonState::Pressed, ButtonState::Released]
    });
    h.quit();
}

#[test]
fn a_click_inside_the_popup_is_delivered_to_the_popup() {
    let mut h = Harness::start("inside", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "inside");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let menu = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    h.settle();
    click(&mut h, menu.centre());
    wait_for("the click in the menu", || {
        inbox.pump(&mut conn);
        inbox.buttons(menu.root) == [ButtonState::Pressed, ButtonState::Released]
    });
    assert!(inbox.done().is_empty(), "the menu is still open");
    h.quit();
}

#[test]
fn a_popup_is_above_its_parent_and_below_another_clients_toplevel() {
    let mut h = Harness::start("stack", OUT.0, OUT.1);
    let mut a = popup_client(&h, "a");
    let mut a_in = Inbox::default();
    let parent = make_window(&mut a, &mut a_in, 1, RED, 1);
    let mut b = popup_client(&h, "b");
    let mut b_in = Inbox::default();
    let other = make_window(&mut b, &mut b_in, 1, BLUE, 1);
    park(&mut h);
    // A tooltip (no grab) over the parent, partly under the other window.
    let tip = make_popup(
        &mut a,
        &mut a_in,
        10,
        parent.root,
        Spec {
            flags: 0,
            ..Spec::menu(IRect::new(10, 10, 40, 20))
        },
        2,
    );
    h.settle();
    assert!(tip.pos.x < other.pos.x && tip.pos.x + MENU.w > other.pos.x);

    // Where the other window covers the tooltip: the other window. Asked
    // first, because a click on the tooltip raises its chain root.
    click(&mut h, (other.pos.x + 2.0, tip.pos.y + MENU.h - 2.0));
    wait_for("the click on the other window", || {
        b_in.pump(&mut b);
        !b_in.buttons(other.root).is_empty()
    });
    a_in.pump(&mut a);
    assert!(a_in.buttons(tip.root).is_empty(), "the tooltip is below it");
    // Over the tooltip but not the other window: the tooltip, not its parent.
    click(&mut h, (tip.pos.x + 2.0, tip.pos.y + MENU.h - 2.0));
    wait_for("the click on the tooltip", || {
        a_in.pump(&mut a);
        !a_in.buttons(tip.root).is_empty()
    });
    assert!(a_in.buttons(parent.root).is_empty());
    h.quit();
}

#[test]
fn escape_dismisses_a_grabbing_chain_and_is_not_delivered() {
    let mut h = Harness::start("escape", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "escape");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let menu = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    h.settle();
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    h.settle();
    await_done(&mut conn, &mut inbox, &[menu.root]);
    assert!(
        !inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_ESC)),
        "neither half of the Escape reached the client"
    );
    h.quit();
}

#[test]
fn escape_reaches_the_client_when_no_popup_grabs() {
    let mut h = Harness::start("escfree", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "escfree");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let spec = Spec {
        flags: 0,
        ..Spec::menu(IRect::new(10, 10, 40, 20))
    };
    make_popup(&mut conn, &mut inbox, 10, parent.root, spec, 2);
    h.settle();
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    expect(
        &mut conn,
        &mut inbox.0,
        "Escape at the parent",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_ESC && k.window == parent.root => Some(()),
            _ => None,
        },
    );
    assert!(inbox.done().is_empty(), "a tooltip does not eat Escape");
    h.quit();
}

#[test]
fn a_submenu_chain_unwinds_from_the_dismissed_level_down() {
    let mut h = Harness::start("chain", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "chain");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let right = |r: IRect| Spec {
        anchor: PopupAnchor::TopRight,
        ..Spec::menu(r)
    };
    let one = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    let two = make_popup(
        &mut conn,
        &mut inbox,
        20,
        one.root,
        right(IRect::new(0, 0, 60, 20)),
        3,
    );
    let three = make_popup(
        &mut conn,
        &mut inbox,
        30,
        two.root,
        right(IRect::new(0, 0, 60, 20)),
        4,
    );
    h.settle();

    // The client closes the middle level itself: it gets no PopupDone for
    // that one (it already knows), but everything below it goes.
    conn.tx().destroy_node(two.root).commit(5).unwrap();
    conn.flush().unwrap();
    h.settle();
    await_done(&mut conn, &mut inbox, &[three.root]);
    assert!(!inbox.done().contains(&two.root));
    assert!(!inbox.done().contains(&one.root), "level one stays");

    // ...and level one still hits.
    click(&mut h, (one.pos.x + 5.0, one.pos.y + 5.0));
    wait_for("the click on level one", || {
        inbox.pump(&mut conn);
        !inbox.buttons(one.root).is_empty()
    });
    h.quit();
}

#[test]
fn a_popup_cannot_be_created_for_another_clients_window() {
    let h = Harness::start("foreign", OUT.0, OUT.1);
    let mut victim = popup_client(&h, "victim");
    let mut v_in = Inbox::default();
    let win = make_window(&mut victim, &mut v_in, 7, RED, 1);
    let mut thief = popup_client(&h, "thief");
    let mut t_in = Inbox::default();
    // The thief names the victim's id; in its own id space it is nothing.
    send_popup(
        &mut thief,
        10,
        win.root,
        Spec::menu(IRect::new(0, 0, 10, 10)),
        1,
    );
    let code = expect(&mut thief, &mut t_in.0, "Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::UnknownNode);
    h.settle();
    assert_eq!(h.stat("windows"), 1, "the victim survives");
    h.quit();
}

#[test]
fn a_popup_gets_no_decorations() {
    let mut h = Harness::start("undecorated", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "undecorated");
    let mut inbox = Inbox::default();
    // A *decorated* parent: the popup must still get no frame.
    let root = NodeId(1);
    conn.tx()
        .create_window_with(root, "w", WIN, Layer::Normal, 0)
        .create_rect(NodeId(2), root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(NodeId(2), RED)
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let parent = configure_of(&mut conn, &mut inbox, root);
    park(&mut h);
    let before = h.shot();
    let menu = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 40, 40, 20)),
        2,
    );
    h.settle();
    let after = h.shot();
    // Its content starts exactly where the Configure says...
    assert!(green_at(&after, menu.pos.x, menu.pos.y));
    assert!(green_at(
        &after,
        menu.pos.x + MENU.w - 1.0,
        menu.pos.y + MENU.h - 1.0
    ));
    // ...and nothing appeared around it: no title bar above, no border.
    for (x, y) in [
        (menu.pos.x + 10.0, menu.pos.y - 10.0),
        (menu.pos.x - 1.0, menu.pos.y + 10.0),
        (menu.pos.x + MENU.w, menu.pos.y + 10.0),
        (menu.pos.x + 10.0, menu.pos.y + MENU.h),
    ] {
        assert_eq!(
            rgb(after.pixel(x as u32, y as u32)),
            rgb(before.pixel(x as u32, y as u32)),
            "({x}, {y}) changed: a frame"
        );
    }
    h.quit();
}

#[test]
fn a_popup_does_not_steal_keyboard_focus() {
    let mut h = Harness::start("focus", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "focus");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    expect(&mut conn, &mut inbox.0, "Focus", |m| match m {
        ServerMsg::Focus(f) if f.window == parent.root && f.focused => Some(()),
        _ => None,
    });
    park(&mut h);
    let menu = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    h.settle();
    // Even a click inside the menu does not move focus.
    click(&mut h, menu.centre());
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    expect(
        &mut conn,
        &mut inbox.0,
        "the key at the parent",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_A && k.window == parent.root => Some(()),
            _ => None,
        },
    );
    assert!(
        !inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Focus(f) if !f.focused || f.window == menu.root)),
        "focus never moved: {:?}",
        inbox.0
    );
    h.quit();
}

#[test]
fn destroying_the_parent_takes_its_popups_with_it() {
    // Also the commit-lifetime hazard: the parent is destroyed by the
    // popup owner's *own* `DestroyNode`, so the dismissal runs while that
    // client is lifted out of the map. The `PopupDone`s must survive it.
    let mut h = Harness::start("orphan", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "orphan");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let menu = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    let sub = make_popup(
        &mut conn,
        &mut inbox,
        20,
        menu.root,
        Spec {
            anchor: PopupAnchor::TopRight,
            ..Spec::menu(IRect::new(0, 0, 60, 20))
        },
        3,
    );
    h.settle();
    conn.tx().destroy_node(parent.root).commit(4).unwrap();
    conn.flush().unwrap();
    await_done(&mut conn, &mut inbox, &[sub.root, menu.root]);
    h.settle();
    let img = h.shot();
    assert!(
        !green_at(&img, menu.pos.x + 5.0, menu.pos.y + 5.0),
        "no longer painted"
    );
    assert!(!green_at(&img, sub.pos.x + 5.0, sub.pos.y + 5.0));
    // And no longer hit: a click there reaches nobody.
    click(&mut h, (menu.pos.x + 5.0, menu.pos.y + 5.0));
    inbox.pump(&mut conn);
    assert!(inbox.buttons(menu.root).is_empty());
    h.quit();
}

#[test]
fn reposition_moves_the_popup_and_answers_with_a_configure() {
    let mut h = Harness::start("reposition", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "reposition");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let menu = make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    inbox.0.clear();
    conn.tx()
        .reposition_popup(
            menu.root,
            IRect::new(100, 50, 20, 10),
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            0,
        )
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    let moved = configure_of(&mut conn, &mut inbox, menu.root);
    assert_eq!(
        moved.pos,
        Point::new(parent.pos.x + 100.0, parent.pos.y + 60.0)
    );
    h.settle();
    assert!(green_at(&h.shot(), moved.pos.x + 5.0, moved.pos.y + 5.0));

    // Dismissed, then repositioned: it stays dismissed.
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    await_done(&mut conn, &mut inbox, &[menu.root]);
    inbox.0.clear();
    conn.tx()
        .reposition_popup(
            menu.root,
            IRect::new(10, 10, 20, 10),
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            0,
        )
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    inbox.pump(&mut conn);
    assert!(
        !inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Configure(c) if c.window == menu.root)),
        "a dismissed popup is not resurrected"
    );
    assert!(!green_at(
        &h.shot(),
        parent.pos.x + 15.0,
        parent.pos.y + 25.0
    ));
    h.quit();
}

#[test]
fn moving_the_parent_carries_the_popup() {
    let mut h = Harness::start("follow", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "follow");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    park(&mut h);
    let spec = Spec {
        flags: 0,
        ..Spec::menu(IRect::new(10, 10, 40, 20))
    };
    let tip = make_popup(&mut conn, &mut inbox, 10, parent.root, spec, 2);
    // Maximize moves the parent's content to the output's origin.
    conn.tx()
        .set_window_state(parent.root, nitro_wire::types::WindowState::Maximized)
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    // Undecorated windows are still maximizable; a fixed-size one is not.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        inbox.pump(&mut conn);
        let last = inbox.0.iter().rev().find_map(|m| match m {
            ServerMsg::Configure(c) if c.window == tip.root => Some(c.position),
            _ => None,
        });
        if last == Some(Point::new(10.0, 30.0)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the tooltip did not follow: {last:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(inbox.done().is_empty(), "moved, not dismissed");
    h.quit();
}

#[test]
fn a_popup_is_not_in_the_shell_window_list() {
    let h = Harness::start("list", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "list");
    let mut inbox = Inbox::default();
    let parent = make_window(&mut conn, &mut inbox, 1, RED, 1);
    make_popup(
        &mut conn,
        &mut inbox,
        10,
        parent.root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    h.settle();
    let mut shell = h.shell("bar");
    let mut s_in = Inbox::default();
    shell.window_list().unwrap();
    shell.flush().unwrap();
    expect(&mut shell, &mut s_in.0, "WindowListEnd", |m| match m {
        ServerMsg::WindowListEnd(_) => Some(()),
        _ => None,
    });
    let n = s_in
        .0
        .iter()
        .filter(|m| matches!(m, ServerMsg::WindowInfo(_)))
        .count();
    assert_eq!(n, 1, "the parent, and not its menu");
    h.quit();
}

#[test]
fn a_popup_whose_parent_has_no_output_is_dismissed_not_fatal() {
    let h = Harness::start("nooutput", OUT.0, OUT.1);
    let mut conn = popup_client(&h, "nooutput");
    let mut inbox = Inbox::default();
    assert_eq!(h.request_line("unplug\n"), "ok");
    wait_for("the output to go", || h.stat("outputs") == 0);
    // Parent and popup in one commit, with nowhere to put either.
    let root = NodeId(1);
    conn.tx()
        .create_window_with(root, "w", WIN, Layer::Normal, window_flags::UNDECORATED)
        .commit(1)
        .unwrap();
    send_popup(
        &mut conn,
        10,
        root,
        Spec::menu(IRect::new(10, 10, 40, 20)),
        2,
    );
    await_done(&mut conn, &mut inbox, &[NodeId(10)]);
    // The connection survives: it can keep committing, and its window is
    // placed when an output comes back.
    conn.tx()
        .create_rect(NodeId(2), root, Rect::new(0.0, 0.0, 10.0, 10.0))
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    assert_eq!(h.request_line("plug 640x480\n"), "ok");
    configure_of(&mut conn, &mut inbox, root);
    assert!(
        !inbox.0.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "{:?}",
        inbox.0
    );
    h.quit();
}
