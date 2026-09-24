//! The M3-B shell socket, driven end to end through the real event loop on
//! the fake backend: layers, exclusive zones, anchors, global hotkeys, the
//! window list, window control and output enumeration.
//!
//! Everything here goes through the paths a real desktop does — a real
//! privileged `nitro-wire` connection on `shell.sock`, real synthetic input
//! through the `FakeInput` eventfd, real screenshots off the front buffer.
//! The pure policy (zone arithmetic, anchor rectangles, the hotkey table and
//! the tap state machine) is unit-tested in `src/shell.rs`; this file is
//! about whether the *server* wires it up, and about the one thing a unit
//! test cannot see: that a privilege granted by a socket is actually refused
//! on the other one.

// Every geometry number here is exact arithmetic on exact inputs — whole
// pixel sizes, sums and halves of small integers — so equality is the
// assertion that means what it says; an epsilon would only hide a wrong
// formula.
#![allow(clippy::float_cmp)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::{BackendKind, Config, run, wm};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{
    Edge, Layer, NodeId, WindowRef, WindowState, anchor, caps, mod_mask, window_flags,
};

// evdev keycodes, from `linux/input-event-codes.h`.
const KEY_ENTER: u32 = 28;
const KEY_A: u32 = 30;
const KEY_ESC: u32 = 1;
const KEY_LEFTMETA: u32 = 125;
const KEY_LEFTSHIFT: u32 = 42;

// X11 keysyms the bindings are made with.
const XK_RETURN: u32 = 0xff0d;
const XK_A: u32 = 0x0061;

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);
const BAR_BLUE: Color = Color::rgb(0x00, 0x00, 0xFF);
/// How much of the top edge the bar in these tests reserves.
const ZONE: u32 = 32;

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
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, width: u32, height: u32) -> Self {
        Self::start_with(name, width, height, |_| {})
    }

    /// [`Harness::start`] with a last say over the configuration.
    fn start_with(name: &str, width: u32, height: u32, tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-shell-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake { width, height };
        tweak(&mut config);
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
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

    /// An ordinary, unprivileged client on the wire socket.
    fn client(&self, name: &str) -> Connection {
        Connection::connect(&self.wire_path, name).expect("wire connect")
    }

    /// A privileged client on the shell socket.
    fn shell(&self, name: &str) -> Connection {
        Connection::connect(&self.shell_path, name).expect("shell connect")
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

    /// One request whose reply is a single status line (`plug`, `unplug`).
    fn request_line(&self, req: &str) -> String {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
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

    fn shot(&self) -> nitro_kms::Image {
        use std::io::Read as _;
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
        nitro_kms::Image {
            width,
            height,
            stride,
            data,
        }
    }

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

    fn point_at(&mut self, x: f32, y: f32, size: (u32, u32)) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(x) / f64::from(size.0),
            y: f64::from(y) / f64::from(size.1),
            time_ns: self.time_ns,
        });
    }

    fn button(&mut self, button: u32, state: nitro_wire::types::ButtonState) {
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

    /// Press and release one key with Super held.
    fn super_chord(&mut self, keycode: u32) {
        self.key(KEY_LEFTMETA, true);
        self.settle();
        self.key(keycode, true);
        self.settle();
        self.key(keycode, false);
        self.settle();
        self.key(KEY_LEFTMETA, false);
        self.settle();
    }

    /// Press and release Super with nothing in between: the launcher trigger.
    fn super_tap(&mut self) {
        self.key(KEY_LEFTMETA, true);
        self.settle();
        self.key(KEY_LEFTMETA, false);
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

/// Everything one connection received, in arrival order.
#[derive(Default)]
struct Inbox(Vec<ServerMsg>);

/// Drain a connection until `f` matches, or time out.
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

/// Pump a connection without waiting for anything in particular.
fn pump(conn: &mut Connection, inbox: &mut Inbox) {
    let _ = conn.flush();
    let _ = conn.poll(&mut inbox.0);
}

/// A window: the client's node id plus the geometry the server gave it.
#[derive(Clone, Copy)]
struct Win {
    root: NodeId,
    pos: nitro_core::Point,
    size: Size,
}

/// Create a window filled with one solid rect and wait for its `Configure`.
// Seven facts about one window plus the two handles it is created through; a
// struct here would be this argument list with a name on it, as in
// `tests/wm.rs`.
#[allow(clippy::too_many_arguments)]
fn make_window(
    conn: &mut Connection,
    inbox: &mut Inbox,
    id: u32,
    title: &str,
    size: Size,
    color: Color,
    flags: u32,
    layer: Layer,
    serial: u32,
) -> Win {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, title, size, layer, flags)
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
    pump(conn, inbox);
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
    let before = (win.pos, win.size);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        refresh(conn, inbox, win);
        if (win.pos, win.size) != before {
            return;
        }
        assert!(Instant::now() < deadline, "no Configure for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The newest `WindowInfo` for each window, keyed by its `WindowRef`.
fn window_map(inbox: &Inbox) -> std::collections::HashMap<WindowRef, nitro_wire::msg::WindowInfo> {
    let mut out = std::collections::HashMap::new();
    for m in &inbox.0 {
        match m {
            ServerMsg::WindowInfo(i) => {
                out.insert(i.window, i.clone());
            }
            ServerMsg::WindowGone(g) => {
                out.remove(&g.window);
            }
            _ => {}
        }
    }
    out
}

/// Drain a shell connection until its window list settles on `n` entries.
fn await_window_count(
    conn: &mut Connection,
    inbox: &mut Inbox,
    n: usize,
    what: &str,
) -> std::collections::HashMap<WindowRef, nitro_wire::msg::WindowInfo> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(conn, inbox);
        let map = window_map(inbox);
        if map.len() == n {
            return map;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: {} window(s), wanted {n}",
            map.len()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Drain a shell connection until `f` is true of its window list.
fn await_windows(
    conn: &mut Connection,
    inbox: &mut Inbox,
    what: &str,
    f: impl Fn(&std::collections::HashMap<WindowRef, nitro_wire::msg::WindowInfo>) -> bool,
) -> std::collections::HashMap<WindowRef, nitro_wire::msg::WindowInfo> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(conn, inbox);
        let map = window_map(inbox);
        if f(&map) {
            return map;
        }
        assert!(Instant::now() < deadline, "{what}; got {map:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn rgb(px: u32) -> u32 {
    px & 0x00ff_ffff
}

fn to_rgb(c: Color) -> u32 {
    u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

/// Park the pointer in a corner, where it cannot contaminate a pixel
/// assertion or hover a window under test.
fn park(h: &mut Harness) {
    h.point_at(OUT.0 as f32 - 2.0, OUT.1 as f32 - 2.0, OUT);
    h.settle();
}

/// A `Top`-layer bar spanning the top edge, with a 32-px exclusive zone: the
/// shape every one of these tests needs.
fn make_bar(h: &Harness, conn: &mut Connection, inbox: &mut Inbox, serial: u32) -> Win {
    let bar = make_window(
        conn,
        inbox,
        100,
        "bar",
        Size::new(OUT.0 as f32, ZONE as f32),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Top,
        serial,
    );
    conn.tx()
        .set_exclusive_zone(bar.root, Edge::Top, ZONE)
        .set_anchor(bar.root, anchor::TOP | anchor::LEFT | anchor::RIGHT, 0)
        .commit(serial + 1)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    bar
}

#[test]
fn the_shell_socket_grants_the_shell_capability_and_the_wire_socket_does_not() {
    let h = Harness::start("caps", OUT.0, OUT.1);

    let plain = h.client("plain");
    assert_eq!(
        plain.caps() & caps::SHELL,
        0,
        "an ordinary client is not privileged"
    );
    assert_eq!(plain.caps() & caps::WM, caps::WM, "but it still gets WM");

    let shell = h.shell("bar");
    assert_eq!(
        shell.caps() & caps::SHELL,
        caps::SHELL,
        "a client on the shell socket is"
    );
    assert_eq!(
        shell.caps() & caps::WM,
        caps::WM,
        "and keeps every ordinary capability"
    );
    assert_eq!(h.stat("shell_clients"), 1);

    // Several shell clients at once: the bar, the launcher and the wallpaper
    // are three separate processes.
    let second = h.shell("launcher");
    let third = h.shell("wallpaper");
    assert_eq!(second.caps() & caps::SHELL, caps::SHELL);
    assert_eq!(third.caps() & caps::SHELL, caps::SHELL);
    wait_for("three shell clients", || h.stat("shell_clients") == 3);

    drop((plain, shell, second, third));
    h.quit();
}

#[test]
fn an_unprivileged_client_sending_a_shell_op_is_disconnected_with_an_error() {
    let h = Harness::start("nopriv", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("sneaky");
    let win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );

    // `SetLayer` on the ordinary socket. Fatal, like every other protocol
    // error: the client asked for something the protocol says it may not
    // have, and carrying on would leave it believing it had it.
    conn.tx().set_layer(win.root, Layer::Top).finish().unwrap();
    conn.flush().unwrap();

    let code = expect(&mut conn, &mut inbox.0, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::Protocol);
    wait_for("the connection to close", || {
        pump(&mut conn, &mut inbox);
        conn.is_closed()
    });
    wait_for("the client to be gone", || h.stat("clients") == 0);

    h.quit();
}

#[test]
fn every_shell_op_is_refused_on_the_ordinary_socket() {
    // One connection per op, because the first one is fatal. The point is
    // that *no* op leaks through: a privilege check that covered ten of
    // eleven messages would look exactly like a working one until someone
    // found the eleventh.
    //
    // Note the `finish()` rather than `commit()` on the four buffered ops:
    // the privilege check is on **receipt**, so an unprivileged client is
    // disconnected whether or not it ever commits. Requiring a commit first
    // would let a client that never sends one sit there having spoken an op
    // it may not use.
    let h = Harness::start("nopriv-all", OUT.0, OUT.1);
    for (name, op) in [
        ("SetLayer", 0u8),
        ("SetExclusiveZone", 1),
        ("SetAnchor", 2),
        ("BindKey", 3),
        ("UnbindKey", 4),
        ("GrabKeyboard", 5),
        ("WindowList", 6),
        ("FocusWindow", 7),
        ("CloseWindow", 8),
        ("SetWindowStateFor", 9),
        ("Outputs", 10),
        ("Lock", 11),
        ("Unlock", 12),
    ] {
        let mut inbox = Inbox::default();
        let mut conn = h.client(name);
        let win = make_window(
            &mut conn,
            &mut inbox,
            1,
            name,
            WIN,
            RED,
            0,
            Layer::Normal,
            1,
        );
        match op {
            0 => conn.tx().set_layer(win.root, Layer::Top).finish(),
            1 => conn
                .tx()
                .set_exclusive_zone(win.root, Edge::Top, 8)
                .finish(),
            2 => conn.tx().set_anchor(win.root, anchor::TOP, 0).finish(),
            3 => conn.bind_key(1, mod_mask::SUPER, XK_RETURN),
            4 => conn.unbind_key(1),
            5 => conn.tx().grab_keyboard(win.root, true).finish(),
            6 => conn.window_list(),
            7 => conn.focus_window(WindowRef(1)),
            8 => conn.close_window(WindowRef(1)),
            9 => conn.set_window_state_for(WindowRef(1), WindowState::Minimized),
            10 => conn.outputs(),
            11 => conn.lock(),
            _ => conn.unlock(),
        }
        .unwrap();
        conn.flush().unwrap();
        let code = expect(&mut conn, &mut inbox.0, "an Error", |m| match m {
            ServerMsg::Error(e) => Some(e.code),
            _ => None,
        });
        assert_eq!(
            code,
            nitro_wire::types::ErrorCode::Protocol,
            "{name} must be refused on the wire socket"
        );
        wait_for("the connection to close", || {
            pump(&mut conn, &mut inbox);
            conn.is_closed()
        });
        drop(conn);
        wait_for("the client to be gone", || h.stat("clients") == 0);
    }
    h.quit();
}

#[test]
fn a_bar_can_create_anchor_and_reserve_in_one_transaction() {
    // The shape a real bar actually sends, and the bug the hardware probe
    // found: `CreateWindow`, `SetAnchor` and `SetExclusiveZone` in *one*
    // commit. The four window-targeting shell ops are buffered like every
    // other mutation precisely so this works — an anchor applied on receipt
    // would be looking for a window the commit has not created yet, and the
    // probe got `UnknownNode` and a dead connection.
    let h = Harness::start("one-tx", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let root = NodeId(1);
    let fill = NodeId(2);
    shell
        .tx()
        .create_window_with(
            root,
            "bar",
            Size::new(100.0, ZONE as f32),
            Layer::Top,
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
        )
        .create_rect(fill, root, Rect::new(0.0, 0.0, 100.0, ZONE as f32))
        .fill_solid(fill, BAR_BLUE)
        .set_anchor(root, anchor::TOP | anchor::LEFT | anchor::RIGHT, 0)
        .set_exclusive_zone(root, Edge::Top, ZONE)
        .commit(1)
        .unwrap();
    shell.flush().unwrap();
    h.settle();

    // Not disconnected, and the anchor took: the bar spans the top edge at
    // the size the anchor decided, not the 100 px it asked for.
    assert!(!shell.is_closed(), "one transaction must be enough");
    let (pos, size) = expect(
        &mut shell,
        &mut inbox.0,
        "the anchored Configure",
        |m| match m {
            ServerMsg::Configure(c) if c.window == root && c.size.w == OUT.0 as f32 => {
                Some((c.position, c.size))
            }
            _ => None,
        },
    );
    assert_eq!(pos, nitro_core::Point::new(0.0, 0.0));
    assert_eq!(size, Size::new(OUT.0 as f32, ZONE as f32));
    assert_eq!(h.stat("exclusive_zones"), 1, "and so did the zone");

    drop(shell);
    h.quit();
}

#[test]
fn a_top_exclusive_zone_shrinks_a_maximized_windows_configure() {
    let h = Harness::start("zone", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let bar = make_bar(&h, &mut shell, &mut shell_inbox, 1);
    assert_eq!(h.stat("exclusive_zones"), 1);

    // The bar anchored to the whole top edge, at the output's own origin.
    let mut bar_win = bar;
    refresh(&mut shell, &mut shell_inbox, &mut bar_win);
    assert_eq!(bar_win.pos, nitro_core::Point::new(0.0, 0.0));
    assert_eq!(bar_win.size, Size::new(OUT.0 as f32, ZONE as f32));

    // Now an ordinary, undecorated client maximizing. Undecorated so the
    // numbers are the work area itself rather than the work area minus a
    // frame: what is under test is the *work area*, and the inset arithmetic
    // is `tests/wm.rs`'s business.
    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    await_configure(&mut conn, &mut inbox, &mut win, "the maximize");

    assert_eq!(
        win.size,
        Size::new(OUT.0 as f32, OUT.1 as f32 - ZONE as f32),
        "the height is short by exactly the zone"
    );
    assert_eq!(
        win.pos,
        nitro_core::Point::new(0.0, ZONE as f32),
        "and it starts below the bar"
    );

    drop((conn, shell));
    h.quit();
}

#[test]
fn releasing_a_zone_gives_a_maximized_window_the_space_back() {
    let h = Harness::start("zone-release", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let bar = make_bar(&h, &mut shell, &mut shell_inbox, 1);

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    await_configure(&mut conn, &mut inbox, &mut win, "the maximize");
    assert_eq!(win.size.h, OUT.1 as f32 - ZONE as f32);

    // `px: 0` releases, and the maximized window is re-sized at once rather
    // than at its next maximize: a panel that hides itself must hand the
    // strip back immediately or the desktop stays permanently short.
    shell
        .tx()
        .set_exclusive_zone(bar.root, Edge::Top, 0)
        .commit(3)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the zone release");
    assert_eq!(win.size, Size::new(OUT.0 as f32, OUT.1 as f32));
    assert_eq!(win.pos, nitro_core::Point::new(0.0, 0.0));
    assert_eq!(h.stat("exclusive_zones"), 0);

    drop((conn, shell));
    h.quit();
}

#[test]
fn a_zone_moves_a_newly_placed_window_out_of_the_bars_way() {
    let h = Harness::start("zone-place", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    make_bar(&h, &mut shell, &mut shell_inbox, 1);

    // Placement is centred-cascade *inside the work area*, so the first
    // window lands where `wm::place` says for the shrunken area, not the
    // whole output. Asserted against the policy function rather than a
    // literal, so the two cannot drift.
    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    let area = Rect::new(0.0, ZONE as f32, OUT.0 as f32, OUT.1 as f32 - ZONE as f32);
    // Index 1, not 0: the bar itself took the first cascade slot. What is
    // under test is that placement happens inside the *shrunken* area, so
    // the expectation is `wm::place` on that area rather than a literal, and
    // the two cannot drift.
    assert_eq!(win.pos, wm::place(1, WIN, area));
    assert!(win.pos.y >= ZONE as f32, "below the bar");

    drop((conn, shell));
    h.quit();
}

#[test]
fn a_top_layer_bar_is_drawn_over_a_maximized_normal_window() {
    let mut h = Harness::start("layer", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    make_bar(&h, &mut shell, &mut shell_inbox, 1);

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    await_configure(&mut conn, &mut inbox, &mut win, "the maximize");
    park(&mut h);

    let img = h.shot();
    let at = |x: u32, y: u32| -> u32 {
        let o = (y * img.stride + x * 4) as usize;
        rgb(u32::from_le_bytes([
            img.data[o],
            img.data[o + 1],
            img.data[o + 2],
            img.data[o + 3],
        ]))
    };
    assert_eq!(at(320, ZONE / 2), to_rgb(BAR_BLUE), "the bar's own strip");
    // Inside the client's own rect, which is still `WIN`-sized at its content
    // origin: maximizing resizes the *window*, and it is the client's job to
    // grow its nodes. What matters here is the z-order, and the pixel just
    // below the bar inside the client's rect is where the two would fight.
    assert_eq!(at(10, ZONE + 10), to_rgb(RED), "the client below it");

    drop((conn, shell));
    h.quit();
}

#[test]
fn setting_the_normal_layer_from_the_shell_is_a_protocol_error() {
    // A shell surface asking to be an ordinary window has misunderstood the
    // op, and obliging silently would put a bar into the window-management
    // z-order where a click could raise a document over it.
    let h = Harness::start("layer-normal", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let win = make_window(
        &mut shell,
        &mut inbox,
        1,
        "bar",
        WIN,
        BAR_BLUE,
        window_flags::UNDECORATED,
        Layer::Top,
        1,
    );
    // Committed, because the four window-targeting shell ops are buffered
    // like every other mutation — a bar creates a window and anchors it in
    // one transaction, so they have to be.
    shell
        .tx()
        .set_layer(win.root, Layer::Normal)
        .commit(2)
        .unwrap();
    shell.flush().unwrap();
    let code = expect(&mut shell, &mut inbox.0, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::Protocol);
    drop(shell);
    h.quit();
}

#[test]
fn an_anchor_with_no_edges_centres_an_overlay() {
    let h = Harness::start("anchor-centre", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    let size = Size::new(400.0, 300.0);
    let mut win = make_window(
        &mut shell,
        &mut inbox,
        1,
        "launcher",
        size,
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Overlay,
        1,
    );
    shell.tx().set_anchor(win.root, 0, 0).commit(2).unwrap();
    shell.flush().unwrap();
    h.settle();
    refresh(&mut shell, &mut inbox, &mut win);
    assert_eq!(
        win.pos,
        nitro_core::Point::new((OUT.0 as f32 - size.w) / 2.0, (OUT.1 as f32 - size.h) / 2.0)
    );
    assert_eq!(win.size, size, "centring does not resize");
    drop(shell);
    h.quit();
}

#[test]
fn an_anchor_with_a_margin_insets_a_docked_bar() {
    let h = Harness::start("anchor-margin", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("dock");
    let mut win = make_window(
        &mut shell,
        &mut inbox,
        1,
        "dock",
        Size::new(100.0, 48.0),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Top,
        1,
    );
    shell
        .tx()
        .set_anchor(win.root, anchor::BOTTOM | anchor::LEFT | anchor::RIGHT, 8)
        .commit(2)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    refresh(&mut shell, &mut inbox, &mut win);
    assert_eq!(
        win.pos,
        nitro_core::Point::new(8.0, OUT.1 as f32 - 8.0 - 48.0)
    );
    assert_eq!(win.size, Size::new(OUT.0 as f32 - 16.0, 48.0));
    drop(shell);
    h.quit();
}

#[test]
fn reserved_anchor_bits_are_a_protocol_error() {
    let h = Harness::start("anchor-bits", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let win = make_window(
        &mut shell,
        &mut inbox,
        1,
        "bar",
        WIN,
        BAR_BLUE,
        window_flags::UNDECORATED,
        Layer::Top,
        1,
    );
    shell.tx().set_anchor(win.root, 0xf0, 0).commit(2).unwrap();
    shell.flush().unwrap();
    let code = expect(&mut shell, &mut inbox.0, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::Protocol);
    drop(shell);
    h.quit();
}

#[test]
fn a_shell_op_naming_someone_elses_window_is_an_unknown_node() {
    // The shell ops that take a `NodeId` take the *sender's own*: a shell
    // that named a window it does not own has lost track of its own tree,
    // which is the same reasoning as every other fatal error here. Acting on
    // another client's window goes through `WindowRef` instead.
    let h = Harness::start("shell-foreign", OUT.0, OUT.1);
    let mut app_inbox = Inbox::default();
    let mut app = h.client("app");
    let app_win = make_window(
        &mut app,
        &mut app_inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );

    let mut inbox = Inbox::default();
    let mut shell = h.shell("bar");
    shell
        .tx()
        .set_exclusive_zone(app_win.root, Edge::Top, 8)
        .commit(1)
        .unwrap();
    shell.flush().unwrap();
    let code = expect(&mut shell, &mut inbox.0, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::UnknownNode);
    drop((app, shell));
    h.quit();
}

#[test]
fn the_window_list_reflects_three_windows_and_follows_title_focus_and_close() {
    let h = Harness::start("list", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, Layer::Normal, 1);
    let b = make_window(&mut conn, &mut inbox, 3, "b", WIN, RED, 0, Layer::Normal, 2);
    let c = make_window(&mut conn, &mut inbox, 5, "c", WIN, RED, 0, Layer::Normal, 3);
    conn.tx()
        .set_app_id(c.root, "org.nitro.calc")
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    h.settle();

    // The snapshot: one `WindowInfo` per window, then `WindowListEnd`.
    shell.window_list().unwrap();
    shell.flush().unwrap();
    let map = await_window_count(&mut shell, &mut shell_inbox, 3, "the snapshot");
    expect(&mut shell, &mut shell_inbox.0, "WindowListEnd", |m| {
        matches!(m, ServerMsg::WindowListEnd(_)).then_some(())
    });
    let titles: std::collections::BTreeSet<String> =
        map.values().map(|i| i.title.clone()).collect();
    assert_eq!(
        titles,
        ["a", "b", "c"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    );
    // Exactly one focused, and it is the last window created — a new window
    // takes the focus, so the shell's list agrees with the desktop.
    let focused: Vec<&nitro_wire::msg::WindowInfo> = map.values().filter(|i| i.focused).collect();
    assert_eq!(focused.len(), 1);
    assert_eq!(focused[0].title, "c");
    assert_eq!(focused[0].app_id, "org.nitro.calc");
    // And every one of them is on the same, only, output.
    let outputs: std::collections::BTreeSet<u32> = map.values().map(|i| i.output).collect();
    assert_eq!(outputs.len(), 1, "one output, so one id: {outputs:?}");
    assert_ne!(
        outputs.iter().next().copied(),
        Some(u32::MAX),
        "u32::MAX means unplaced, and these are placed"
    );

    // A retitle arrives unasked: the subscription is live after the
    // snapshot, so a bar never polls.
    conn.tx()
        .set_window_title(a.root, "a-renamed")
        .commit(5)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    let map = await_windows(&mut shell, &mut shell_inbox, "the retitle", |m| {
        m.values().any(|i| i.title == "a-renamed")
    });
    assert_eq!(map.len(), 3, "a retitle does not add an entry");

    // A focus change moves `focused` on *both* entries.
    conn.tx()
        .set_window_state(b.root, WindowState::Minimized)
        .commit(6)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    let map = await_windows(&mut shell, &mut shell_inbox, "the minimize", |m| {
        m.values()
            .any(|i| i.title == "b" && i.state == WindowState::Minimized)
    });
    assert_eq!(
        map.values().filter(|i| i.focused).count(),
        1,
        "still exactly one focused window"
    );

    // A close produces `WindowGone` and the entry disappears.
    conn.tx().destroy_node(c.root).commit(7).unwrap();
    conn.flush().unwrap();
    h.settle();
    let map = await_window_count(&mut shell, &mut shell_inbox, 2, "the close");
    assert!(map.values().all(|i| i.title != "c"));

    drop((conn, shell));
    h.quit();
}

#[test]
fn the_window_list_carries_each_windows_layer() {
    // A task list lists *applications*. The wallpaper, a dock and the
    // launcher are windows as far as the server is concerned, so without
    // this a bar cannot tell them apart — and the M3 bar listed
    // `nitro-wallpaper` and `nitro-launcher` as if they were programs the
    // user had opened.
    //
    // The layer is *carried*, not filtered here: a pager or a dock wants
    // the full picture, so the server reports every window and the
    // consumer decides. `nitro-bar`'s
    // `a_shell_surface_is_not_a_window_in_the_task_list` is the other
    // half.
    let h = Harness::start("listlayer", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");

    // The ordinary client can only make `Normal` windows — a shell layer
    // on the wire socket is a fatal protocol error — so the shell
    // surfaces come from a second privileged connection, which is how a
    // real wallpaper and launcher arrive too.
    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );

    let mut shell_inbox2 = Inbox::default();
    let mut surfaces = h.shell("furniture");
    make_window(
        &mut surfaces,
        &mut shell_inbox2,
        1,
        "wallpaper",
        WIN,
        RED,
        0,
        Layer::Background,
        1,
    );
    make_window(
        &mut surfaces,
        &mut shell_inbox2,
        3,
        "dock",
        WIN,
        RED,
        0,
        Layer::Top,
        2,
    );
    h.settle();

    shell.window_list().unwrap();
    shell.flush().unwrap();
    let map = await_window_count(&mut shell, &mut shell_inbox, 3, "the snapshot");

    let layer_of = |title: &str| {
        map.values()
            .find(|i| i.title == title)
            .unwrap_or_else(|| panic!("no window titled {title}: {map:?}"))
            .layer
    };
    assert_eq!(layer_of("app"), Layer::Normal);
    assert_eq!(layer_of("wallpaper"), Layer::Background);
    assert_eq!(layer_of("dock"), Layer::Top);

    // And a layer *change* is announced, not just the layer a window was
    // born on. A bar filters its task list on the layer, so a window that
    // becomes a shell surface has to leave that list — which it can only
    // do if the change reaches the watchers at all.
    let app_ref = *map
        .iter()
        .find(|(_, i)| i.title == "app")
        .expect("the application")
        .0;
    surfaces
        .tx()
        .create_window(NodeId(5), "promoted", WIN, Layer::Normal)
        .commit(3)
        .unwrap();
    surfaces.flush().unwrap();
    h.settle();
    let map = await_window_count(&mut shell, &mut shell_inbox, 4, "the fourth window");
    assert_eq!(
        map.values()
            .find(|i| i.title == "promoted")
            .expect("promoted")
            .layer,
        Layer::Normal
    );

    surfaces
        .tx()
        .set_layer(NodeId(5), Layer::Top)
        .commit(4)
        .unwrap();
    surfaces.flush().unwrap();
    h.settle();
    let map = await_windows(&mut shell, &mut shell_inbox, "the layer change", |m| {
        m.values()
            .any(|i| i.title == "promoted" && i.layer == Layer::Top)
    });
    assert_eq!(map.len(), 4, "a layer change does not add an entry");
    // The other windows are untouched by it.
    assert_eq!(map[&app_ref].layer, Layer::Normal);

    drop((conn, surfaces, shell));
    h.quit();
}

#[test]
fn a_shell_can_focus_minimize_and_close_another_clients_window() {
    let h = Harness::start("control", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let a = make_window(&mut conn, &mut inbox, 1, "a", WIN, RED, 0, Layer::Normal, 1);
    let _b = make_window(&mut conn, &mut inbox, 3, "b", WIN, RED, 0, Layer::Normal, 2);
    h.settle();
    shell.window_list().unwrap();
    shell.flush().unwrap();
    let map = await_window_count(&mut shell, &mut shell_inbox, 2, "the snapshot");
    let a_ref = *map
        .iter()
        .find(|(_, i)| i.title == "a")
        .expect("window a in the list")
        .0;

    // `b` has the focus (it was created last). Focus `a` from the shell.
    shell.focus_window(a_ref).unwrap();
    shell.flush().unwrap();
    h.settle();
    expect(&mut conn, &mut inbox.0, "Focus on a", |m| match m {
        ServerMsg::Focus(f) if f.window == a.root && f.focused => Some(()),
        _ => None,
    });
    await_windows(&mut shell, &mut shell_inbox, "a focused in the list", |m| {
        m.get(&a_ref).is_some_and(|i| i.focused)
    });

    // Minimize it from the shell: the owning client is told, exactly as if
    // the user had pressed Super+H.
    shell
        .set_window_state_for(a_ref, WindowState::Minimized)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    expect(&mut conn, &mut inbox.0, "WindowState on a", |m| match m {
        ServerMsg::WindowState(s) if s.window == a.root && s.state == WindowState::Minimized => {
            Some(())
        }
        _ => None,
    });

    // Restore it, then close it: `CloseWindow` is a *request*, so the client
    // gets `Closed` and the window is still there until it acts.
    shell
        .set_window_state_for(a_ref, WindowState::Normal)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    shell.close_window(a_ref).unwrap();
    shell.flush().unwrap();
    expect(&mut conn, &mut inbox.0, "Closed on a", |m| match m {
        ServerMsg::Closed(c) if c.window == a.root => Some(()),
        _ => None,
    });
    assert_eq!(h.stat("windows"), 2, "the client has not acted on it yet");

    // Now it acts.
    conn.tx().destroy_node(a.root).commit(9).unwrap();
    conn.flush().unwrap();
    h.settle();
    let map = await_window_count(&mut shell, &mut shell_inbox, 1, "the close");
    assert!(!map.contains_key(&a_ref), "the ref is retired");

    // And a stale ref is simply \"no such window\": no error, no other
    // client's window.
    shell.focus_window(a_ref).unwrap();
    shell.close_window(a_ref).unwrap();
    shell.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("windows"), 1);
    assert!(!shell.is_closed(), "a stale ref is not fatal");

    drop((conn, shell));
    h.quit();
}

#[test]
fn a_bound_chord_fires_a_hotkey_and_never_reaches_the_focused_client() {
    let mut h = Harness::start("hotkey", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    shell.bind_key(7, mod_mask::SUPER, XK_RETURN).unwrap();
    shell.flush().unwrap();
    wait_for("the binding", || h.stat("hotkeys") == 1);

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );
    h.settle();
    expect(&mut conn, &mut inbox.0, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == win.root && f.focused => Some(()),
        _ => None,
    });

    h.super_chord(KEY_ENTER);

    // Press and release, in order, on the shell.
    let mut hotkeys = Vec::new();
    wait_for("two HotKey events", || {
        pump(&mut shell, &mut shell_inbox);
        hotkeys = shell_inbox
            .0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::HotKey(k) => Some((k.id, k.pressed)),
                _ => None,
            })
            .collect();
        hotkeys.len() == 2
    });
    assert_eq!(hotkeys, vec![(7, true), (7, false)]);
    assert!(hotkeys.iter().all(|(id, _)| *id == 7));

    // And nothing of the chord reached the focused client. Not even the
    // Super press: a bound global hotkey the application could also see
    // would be a keylogger and an ambiguity at once.
    pump(&mut conn, &mut inbox);
    let keys: Vec<u32> = inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Key(k) => Some(k.keycode),
            _ => None,
        })
        .collect();
    assert!(
        !keys.contains(&KEY_ENTER),
        "Super+Return must not reach the client; got {keys:?}"
    );

    // An *unbound* key with the same modifier still does, so the filter is
    // the binding and not "every key with Super".
    h.super_chord(KEY_A);
    pump(&mut conn, &mut inbox);
    let keys: Vec<u32> = inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Key(k) => Some(k.keycode),
            _ => None,
        })
        .collect();
    assert!(
        keys.contains(&KEY_A),
        "Super+A is nobody's chord; got {keys:?}"
    );

    drop((conn, shell));
    h.quit();
}

#[test]
fn a_bare_super_tap_fires_once_and_another_key_cancels_it() {
    let mut h = Harness::start("tap", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    shell.bind_key(9, mod_mask::SUPER, 0).unwrap();
    shell.flush().unwrap();
    wait_for("the binding", || h.stat("hotkeys") == 1);

    h.super_tap();
    let mut taps = Vec::new();
    wait_for("the tap", || {
        pump(&mut shell, &mut shell_inbox);
        taps = shell_inbox
            .0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::HotKey(k) => Some((k.id, k.pressed)),
                _ => None,
            })
            .collect();
        !taps.is_empty()
    });
    // Once, on the release: until the release the server cannot know it was
    // a tap rather than the start of a chord.
    assert_eq!(taps, vec![(9, false)]);

    // Super+A is not a tap. Asserted as "still exactly one", so a spurious
    // second tap fails rather than being swallowed by a `!is_empty`.
    h.super_chord(KEY_A);
    h.settle();
    pump(&mut shell, &mut shell_inbox);
    let taps: Vec<(u32, bool)> = shell_inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::HotKey(k) => Some((k.id, k.pressed)),
            _ => None,
        })
        .collect();
    assert_eq!(taps, vec![(9, false)], "Super+A must not be a Super tap");

    // Shift+Super is not either: a second modifier disarms.
    h.key(KEY_LEFTMETA, true);
    h.settle();
    h.key(KEY_LEFTSHIFT, true);
    h.settle();
    h.key(KEY_LEFTSHIFT, false);
    h.settle();
    h.key(KEY_LEFTMETA, false);
    h.settle();
    pump(&mut shell, &mut shell_inbox);
    let taps: Vec<(u32, bool)> = shell_inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::HotKey(k) => Some((k.id, k.pressed)),
            _ => None,
        })
        .collect();
    assert_eq!(taps.len(), 1, "Super+Shift is not a tap either");

    drop(shell);
    h.quit();
}

#[test]
fn unbinding_and_disconnecting_both_give_a_chord_back_to_the_client() {
    let mut h = Harness::start("unbind", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let _win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );
    h.settle();

    {
        let mut shell = h.shell("launcher");
        shell.bind_key(7, mod_mask::SUPER, XK_A).unwrap();
        shell.flush().unwrap();
        wait_for("the binding", || h.stat("hotkeys") == 1);
        h.super_chord(KEY_A);
        pump(&mut conn, &mut inbox);
        let n = inbox
            .0
            .iter()
            .filter(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_A))
            .count();
        assert_eq!(n, 0, "while bound, the client sees nothing");

        shell.unbind_key(7).unwrap();
        shell.flush().unwrap();
        wait_for("the unbind", || h.stat("hotkeys") == 0);
        h.super_chord(KEY_A);
        pump(&mut conn, &mut inbox);
        let n = inbox
            .0
            .iter()
            .filter(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_A))
            .count();
        assert!(n > 0, "unbound, it does again");

        // Re-bind, then let the shell go: a launcher that crashed must not
        // leave its chord swallowed for the rest of the session.
        shell.bind_key(7, mod_mask::SUPER, XK_A).unwrap();
        shell.flush().unwrap();
        wait_for("the re-bind", || h.stat("hotkeys") == 1);
    }
    wait_for("the shell's bindings to go with it", || {
        h.stat("hotkeys") == 0
    });
    let before = inbox
        .0
        .iter()
        .filter(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_A))
        .count();
    h.super_chord(KEY_A);
    pump(&mut conn, &mut inbox);
    let after = inbox
        .0
        .iter()
        .filter(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_A))
        .count();
    assert!(after > before, "the chord is the client's again");

    drop(conn);
    h.quit();
}

#[test]
fn a_compositor_chord_cannot_be_bound_and_still_works() {
    let h = Harness::start("reserved", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("greedy");
    // Super+Q closes the focused window; a shell that could take it would
    // be able to make the desktop's own shortcuts unreachable.
    shell.bind_key(1, mod_mask::SUPER, 0x0071).unwrap();
    shell.flush().unwrap();
    let code = expect(&mut shell, &mut inbox.0, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::Protocol);
    assert_eq!(h.stat("hotkeys"), 0);
    drop(shell);
    h.quit();
}

#[test]
fn two_shell_clients_cannot_hold_the_same_chord() {
    let h = Harness::start("contested", OUT.0, OUT.1);
    let mut first = h.shell("first");
    first.bind_key(1, mod_mask::SUPER, XK_RETURN).unwrap();
    first.flush().unwrap();
    wait_for("the first binding", || h.stat("hotkeys") == 1);

    let mut inbox = Inbox::default();
    let mut second = h.shell("second");
    second.bind_key(1, mod_mask::SUPER, XK_RETURN).unwrap();
    second.flush().unwrap();
    let code = expect(&mut second, &mut inbox.0, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::Protocol);
    // The first client's binding is untouched: a second shell's mistake must
    // not take the launcher's key away.
    wait_for("only the first binding left", || h.stat("hotkeys") == 1);
    drop((first, second));
    h.quit();
}

#[test]
fn a_keyboard_grab_routes_keys_to_a_no_focus_overlay_and_back() {
    let mut h = Harness::start("grab", OUT.0, OUT.1);

    let mut app_inbox = Inbox::default();
    let mut app = h.client("app");
    let app_win = make_window(
        &mut app,
        &mut app_inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );
    h.settle();
    expect(&mut app, &mut app_inbox.0, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == app_win.root && f.focused => Some(()),
        _ => None,
    });

    // A `NO_FOCUS` overlay: it never takes focus, so without a grab it can
    // never see a key. That is the whole reason `GrabKeyboard` exists.
    let mut inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    let overlay = make_window(
        &mut shell,
        &mut inbox,
        10,
        "launcher",
        Size::new(400.0, 300.0),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Overlay,
        1,
    );
    h.settle();
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    pump(&mut shell, &mut inbox);
    assert!(
        !inbox.0.iter().any(|m| matches!(m, ServerMsg::Key(_))),
        "no grab, no keys"
    );
    expect(&mut app, &mut app_inbox.0, "the app's key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_A => Some(()),
        _ => None,
    });

    // Grab.
    shell
        .tx()
        .grab_keyboard(overlay.root, true)
        .commit(2)
        .unwrap();
    shell.flush().unwrap();
    wait_for("the grab", || h.stat("grabbed") == 1);
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    h.settle();
    expect(&mut shell, &mut inbox.0, "the overlay's key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_ESC && k.window == overlay.root => Some(()),
        _ => None,
    });
    // The app kept the focus — and its active frame — and simply stopped
    // receiving keys.
    pump(&mut app, &mut app_inbox);
    assert!(
        !app_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_ESC)),
        "a grab takes keys, not focus"
    );
    assert!(
        !app_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Focus(f) if f.window == app_win.root && !f.focused)),
        "the app was never told it lost focus"
    );
    assert_eq!(h.stat("focused"), 1);

    // Release: keys go back to the focused window with no extra `Focus`.
    shell
        .tx()
        .grab_keyboard(overlay.root, false)
        .commit(3)
        .unwrap();
    shell.flush().unwrap();
    wait_for("the release", || h.stat("grabbed") == 0);
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    h.settle();
    expect(
        &mut app,
        &mut app_inbox.0,
        "the app's key again",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_ESC => Some(()),
            _ => None,
        },
    );

    drop((app, shell));
    h.quit();
}

#[test]
fn a_bound_chord_under_a_grab_fires_as_a_hotkey_not_a_key() {
    // The order the review asked to have pinned down. A grab replaces
    // *focus*, not the bindings: the compositor's chords and the shell's own
    // `BindKey`s still run first, so a bound chord pressed while a grab is
    // held arrives as a `HotKey` and is **not** also delivered as a `Key` to
    // the grabbing window.
    //
    // This is the behaviour a launcher needs rather than an accident: one
    // opened by a bare-Super tap has to be closable by a second tap *while*
    // it holds the grab, and if the grab outranked bindings that tap would
    // arrive as an ordinary key and the launcher would have to reimplement
    // tap detection. The cost — do not bind a chord you also want as a key —
    // is stated in `docs/wire.md` and `docs/shell.md`.
    let mut h = Harness::start("grab-vs-bind", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    // Two bindings: a chord and the bare-Super tap the launcher toggles on.
    shell.bind_key(7, mod_mask::SUPER, XK_A).unwrap();
    shell.bind_key(9, mod_mask::SUPER, 0).unwrap();
    shell.flush().unwrap();
    wait_for("the bindings", || h.stat("hotkeys") == 2);

    let overlay = make_window(
        &mut shell,
        &mut inbox,
        10,
        "launcher",
        Size::new(400.0, 300.0),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Overlay,
        1,
    );
    shell
        .tx()
        .grab_keyboard(overlay.root, true)
        .commit(2)
        .unwrap();
    shell.flush().unwrap();
    wait_for("the grab", || h.stat("grabbed") == 1);

    // An *unbound* key reaches the grab holder, which is the grab working.
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    h.settle();
    expect(&mut shell, &mut inbox.0, "the unbound key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_ESC && k.window == overlay.root => Some(()),
        _ => None,
    });

    // The *bound* chord does not: it comes back as a HotKey instead.
    h.super_chord(KEY_A);
    let mut chords = Vec::new();
    wait_for("the chord's HotKey", || {
        pump(&mut shell, &mut inbox);
        chords = inbox
            .0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::HotKey(k) if k.id == 7 => Some(k.pressed),
                _ => None,
            })
            .collect();
        chords.len() == 2
    });
    assert_eq!(chords, vec![true, false], "press and release");
    assert!(
        !inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_A)),
        "a bound chord is a HotKey, never also a Key to the grab holder"
    );

    // And the tap still fires under the grab, which is the whole point of
    // this ordering: the launcher can close itself the way it opened.
    h.super_tap();
    wait_for("the tap under the grab", || {
        pump(&mut shell, &mut inbox);
        inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::HotKey(k) if k.id == 9))
    });
    assert_eq!(h.stat("grabbed"), 1, "and the grab is still held");

    drop(shell);
    h.quit();
}

#[test]
fn a_key_typed_before_the_shell_answers_its_hotkey_reaches_nobody() {
    // The race one level below the launcher, stated without reference to
    // grabs so it holds for any shell: a binding fires, the `HotKey` goes
    // out over a socket, and until that client has had its turn the
    // keyboard is *not* handed to whoever merely still has focus.
    //
    // Without this, the gap is a full client round trip — write, wake,
    // build the tree, commit — and every key in it is routed by focus.
    // That is how a user tapping Super and typing "quit" typed it into
    // the calculator, which quit (#3713).
    let mut h = Harness::start("hotkey-window", OUT.0, OUT.1);

    // An ordinary client holding the focus: the window that must *not*
    // see the key.
    let mut app_inbox = Inbox::default();
    let mut app = h.client("app");
    let app_win = make_window(
        &mut app,
        &mut app_inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );
    h.settle();
    expect(&mut app, &mut app_inbox.0, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == app_win.root && f.focused => Some(()),
        _ => None,
    });
    // An unbound key does reach it, so the rest of the test is about the
    // withholding and not about a client that was never going to hear
    // anything.
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    expect(
        &mut app,
        &mut app_inbox.0,
        "the ordinary key",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_A && k.window == app_win.root => Some(()),
            _ => None,
        },
    );
    app_inbox.0.clear();

    // A shell that binds the bare-Super tap and then, like a real
    // launcher, takes a round trip to answer it. This one never answers
    // at all, which is the worst case of the same shape.
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    shell.bind_key(9, mod_mask::SUPER, 0).unwrap();
    shell.flush().unwrap();
    wait_for("the binding", || h.stat("hotkeys") == 1);
    assert_eq!(h.stat("keys_withheld"), 0);

    h.super_tap();
    wait_for("the tap's HotKey", || {
        pump(&mut shell, &mut shell_inbox);
        shell_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::HotKey(k) if k.id == 9))
    });

    // Now the key that races the answer. The deadline is measured on the
    // input clock, which the harness advances by 5 ms per event, so these
    // two events are 10 ms into a 50 ms window however long the test's
    // own socket round trips take.
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    wait_for("the keys to be withheld", || h.stat("keys_withheld") == 2);
    pump(&mut app, &mut app_inbox);
    // The Super *press* does reach it: a tap is decided on the release, so
    // at press time nothing has fired and there is nothing to withhold.
    // That is a pre-existing leak of the modifier itself, cosmetic and
    // separate — named here rather than filtered away, so it cannot grow.
    let leaked: Vec<u32> = app_inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Key(k) => Some(k.keycode),
            _ => None,
        })
        .collect();
    assert_eq!(
        leaked,
        vec![KEY_LEFTMETA],
        "the focused window saw a key meant for the shell"
    );

    // Press *and* release are withheld together — a client handed a
    // release for a press it never got would think the key was stuck.
    // That is what `== 2` above says.

    // The shell answering ends the wait, grab or no grab: it has had its
    // turn, so ordinary routing resumes and the focused window is a
    // normal window again.
    shell.commit(1).unwrap();
    shell.flush().unwrap();
    h.settle();
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    expect(
        &mut app,
        &mut app_inbox.0,
        "keys flowing again",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_A && k.window == app_win.root => Some(()),
            _ => None,
        },
    );
    assert_eq!(h.stat("keys_withheld"), 2, "and nothing more was withheld");

    drop(shell);
    drop(app);
    h.quit();
}

#[test]
fn a_show_and_a_grab_in_one_commit_take_effect_together() {
    // The property the launcher's `show()` depends on, and which #3752's
    // issue guessed wrong: a `SetVisible(true)` and a `GrabKeyboard(true)`
    // in the *same* commit do not race each other, even though
    // `grab_target()` drops a grab on a window that is not showing.
    //
    // The server does not apply a batch in arrival order: `SetVisible` is
    // applied as the message is read, while `GrabKeyboard` is deferred
    // into the transaction's shell ops and drained after the whole batch.
    // So the window is always showing by the time the grab is taken. That
    // ordering is deliberate and load-bearing — a client that had to send
    // two commits to open a grabbing overlay would have a race it could
    // not close from its side — so it is pinned here rather than left as
    // a property everyone believes.
    let mut h = Harness::start("show-and-grab", OUT.0, OUT.1);
    let mut app_inbox = Inbox::default();
    let mut app = h.client("app");
    let app_win = make_window(
        &mut app,
        &mut app_inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );
    h.settle();
    expect(&mut app, &mut app_inbox.0, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == app_win.root && f.focused => Some(()),
        _ => None,
    });

    let mut inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    let overlay = make_window(
        &mut shell,
        &mut inbox,
        10,
        "launcher",
        Size::new(400.0, 300.0),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Overlay,
        1,
    );
    // Start hidden, the way a launcher waiting for its trigger is.
    shell.tx().visible(overlay.root, false).commit(2).unwrap();
    shell.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("grabbed"), 0);

    // Show and grab in one commit, in the launcher's own order.
    shell
        .tx()
        .visible(overlay.root, true)
        .grab_keyboard(overlay.root, true)
        .commit(3)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("grabbed"), 1, "the grab stuck despite the ordering");

    // And it is a real grab: the next key goes past the focused window.
    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    h.settle();
    expect(&mut shell, &mut inbox.0, "the grabbed key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_ESC && k.window == overlay.root => Some(()),
        _ => None,
    });
    pump(&mut app, &mut app_inbox);
    assert!(
        !app_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_ESC)),
        "and the focused window did not also get it"
    );

    drop((app, shell));
    h.quit();
}

#[test]
fn hiding_a_grabbing_window_releases_the_grab() {
    // The launcher hides itself on Escape. Requiring an explicit
    // `GrabKeyboard { on: false }` as well would mean one forgotten message
    // swallows the keyboard for the whole session.
    let mut h = Harness::start("grab-hide", OUT.0, OUT.1);
    let mut app_inbox = Inbox::default();
    let mut app = h.client("app");
    let app_win = make_window(
        &mut app,
        &mut app_inbox,
        1,
        "app",
        WIN,
        RED,
        0,
        Layer::Normal,
        1,
    );
    h.settle();
    expect(&mut app, &mut app_inbox.0, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == app_win.root && f.focused => Some(()),
        _ => None,
    });

    let mut inbox = Inbox::default();
    let mut shell = h.shell("launcher");
    let overlay = make_window(
        &mut shell,
        &mut inbox,
        10,
        "launcher",
        Size::new(400.0, 300.0),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Overlay,
        1,
    );
    shell
        .tx()
        .grab_keyboard(overlay.root, true)
        .commit(2)
        .unwrap();
    shell.flush().unwrap();
    wait_for("the grab", || h.stat("grabbed") == 1);

    // Hide, without releasing the grab.
    shell.tx().visible(overlay.root, false).commit(3).unwrap();
    shell.flush().unwrap();
    h.settle();

    h.key(KEY_ESC, true);
    h.key(KEY_ESC, false);
    h.settle();
    expect(&mut app, &mut app_inbox.0, "the app's key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_ESC => Some(()),
        _ => None,
    });
    assert_eq!(h.stat("grabbed"), 0, "the grab went with the visibility");

    drop((app, shell));
    h.quit();
}

#[test]
fn super_drag_still_moves_a_window_with_the_shell_connected() {
    // The compositor's Super-drag (#3688) and the shell's Super hotkeys share
    // a modifier, so the two have to coexist: a bar holding a Super chord
    // must not make dragging windows stop working.
    use nitro_server::input::BTN_LEFT;
    use nitro_wire::types::ButtonState;

    let mut h = Harness::start("superdrag", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    shell.bind_key(7, mod_mask::SUPER, XK_RETURN).unwrap();
    shell.flush().unwrap();
    wait_for("the binding", || h.stat("hotkeys") == 1);
    make_bar(&h, &mut shell, &mut shell_inbox, 1);

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    let before = win.pos;
    let (cx, cy) = (win.pos.x + win.size.w / 2.0, win.pos.y + win.size.h / 2.0);

    h.key(KEY_LEFTMETA, true);
    h.settle();
    h.point_at(cx, cy, OUT);
    h.settle();
    h.button(BTN_LEFT, ButtonState::Pressed);
    h.settle();
    for i in 1..=4 {
        let t = i as f32 / 4.0;
        h.point_at(cx + 40.0 * t, cy + 20.0 * t, OUT);
        h.settle();
    }
    h.button(BTN_LEFT, ButtonState::Released);
    h.key(KEY_LEFTMETA, false);
    h.settle();

    refresh(&mut conn, &mut inbox, &mut win);
    assert_eq!(
        (win.pos.x - before.x, win.pos.y - before.y),
        (40.0, 20.0),
        "the Super drag still moves the window"
    );
    // And releasing Super after a drag did not look like a launcher tap: the
    // pointer button broke the tap candidate.
    pump(&mut shell, &mut shell_inbox);
    assert!(
        !shell_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::HotKey(_))),
        "a Super drag is not a Super chord"
    );

    drop((conn, shell));
    h.quit();
}

#[test]
fn outputs_are_listed_and_hotplug_is_reported() {
    let h = Harness::start("outputs", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("bar");
    shell.outputs().unwrap();
    shell.flush().unwrap();

    let mut infos: Vec<nitro_wire::msg::OutputInfo> = Vec::new();
    wait_for("the output snapshot", || {
        pump(&mut shell, &mut inbox);
        infos = inbox
            .0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::OutputInfo(i) => Some(i.clone()),
                _ => None,
            })
            .collect();
        infos.len() == 1
            && inbox
                .0
                .iter()
                .any(|m| matches!(m, ServerMsg::OutputsEnd(_)))
    });
    assert_eq!((infos[0].w, infos[0].h), OUT);
    assert_eq!((infos[0].x, infos[0].y), (0, 0));
    assert_eq!(infos[0].scale, 1.0);
    assert!(infos[0].refresh_mhz > 0, "a real mode has a refresh rate");
    assert!(!infos[0].name.is_empty(), "and a connector name");
    let first_id = infos[0].id;

    // Hotplug a second output in: the whole list comes again, because
    // positions are relative to each other and a diff a shell had to
    // reassemble would be a second source of truth about the layout.
    assert_eq!(h.request_line("plug 800x600\n"), "ok");
    inbox.0.clear();
    wait_for("the hotplug", || {
        pump(&mut shell, &mut inbox);
        let infos: Vec<&nitro_wire::msg::OutputInfo> = inbox
            .0
            .iter()
            .filter_map(|m| match m {
                ServerMsg::OutputInfo(i) => Some(i),
                _ => None,
            })
            .collect();
        infos.len() >= 2 && infos.iter().any(|i| (i.w, i.h) == (800, 600))
    });
    // Left to right in connector order: the second output starts where the
    // first ends.
    let infos: Vec<nitro_wire::msg::OutputInfo> = inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::OutputInfo(i) => Some(i.clone()),
            _ => None,
        })
        .collect();
    let second = infos
        .iter()
        .find(|i| (i.w, i.h) == (800, 600))
        .expect("the new output");
    assert_eq!(second.x, OUT.0.cast_signed());
    assert_ne!(second.id, first_id);

    // And unplugging it produces `OutputGone` naming that id.
    let gone_id = second.id;
    inbox.0.clear();
    assert_eq!(h.request_line("unplug\n"), "ok");
    wait_for("the unplug", || {
        pump(&mut shell, &mut inbox);
        inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::OutputGone(g) if g.id == gone_id))
    });

    drop(shell);
    h.quit();
}

#[test]
fn an_anchored_bar_respans_after_a_hotplug() {
    // The desktop got wider, so the edge the bar anchored to did too. An
    // anchor that silently stopped holding would leave a bar covering two
    // thirds of a screen with no way to notice from the protocol.
    let h = Harness::start("respan", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let bar = make_bar(&h, &mut shell, &mut inbox, 1);
    let mut bar_win = bar;
    refresh(&mut shell, &mut inbox, &mut bar_win);
    assert_eq!(bar_win.size.w, OUT.0 as f32);

    // Unplug the only output and plug a wider one: the fake backend's
    // `unplug` removes the last, so this is a *mode change* as the shell
    // sees it.
    assert_eq!(h.request_line("plug 1024x768\n"), "ok");
    h.settle();
    assert_eq!(h.request_line("unplug\n"), "ok");
    h.settle();
    // The bar is back on the first output, still spanning it.
    refresh(&mut shell, &mut inbox, &mut bar_win);
    assert_eq!(
        bar_win.size.w, OUT.0 as f32,
        "the bar still spans its output"
    );

    drop(shell);
    h.quit();
}

#[test]
fn a_minimized_bar_gives_its_strip_back() {
    // Hiding a panel has to release its zone, or a shell would have to
    // remember to release it first and a crashed one would leave the desktop
    // permanently short.
    let h = Harness::start("zone-min", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let bar = make_bar(&h, &mut shell, &mut shell_inbox, 1);

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    await_configure(&mut conn, &mut inbox, &mut win, "the maximize");
    assert_eq!(win.size.h, OUT.1 as f32 - ZONE as f32);

    shell
        .tx()
        .set_window_state(bar.root, WindowState::Minimized)
        .commit(3)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the bar hiding");
    assert_eq!(win.size.h, OUT.1 as f32, "the strip came back");

    // And un-minimizing takes it again: the zone is *skipped* while the bar
    // is not showing, not forgotten, so the shell does not have to re-send
    // `SetExclusiveZone` to get its strip back.
    shell
        .tx()
        .set_window_state(bar.root, WindowState::Normal)
        .commit(4)
        .unwrap();
    shell.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the bar returning");
    assert_eq!(win.size.h, OUT.1 as f32 - ZONE as f32);

    drop((conn, shell));
    h.quit();
}

#[test]
fn a_bar_hidden_with_set_visible_gives_its_strip_back() {
    // The *other* way a panel goes away, and the one the review caught: the
    // zone used to check only `Minimized`, so a bar that hid itself with
    // `SetVisible(false)` — which is what a toggling panel actually does,
    // and what the launcher does on Escape — kept its strip reserved. Both
    // now go through the one `Server::showing` predicate.
    let h = Harness::start("zone-hide", OUT.0, OUT.1);
    let mut shell_inbox = Inbox::default();
    let mut shell = h.shell("bar");
    let bar = make_bar(&h, &mut shell, &mut shell_inbox, 1);

    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let mut win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    conn.tx()
        .set_window_state(win.root, WindowState::Maximized)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    await_configure(&mut conn, &mut inbox, &mut win, "the maximize");
    assert_eq!(win.size.h, OUT.1 as f32 - ZONE as f32);
    assert_eq!(h.stat("exclusive_zones"), 1);

    shell.tx().visible(bar.root, false).commit(3).unwrap();
    shell.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the bar hiding");
    assert_eq!(win.size.h, OUT.1 as f32, "the strip came back");
    // Still *held*, just not honoured: the bar has not released anything.
    assert_eq!(
        h.stat("exclusive_zones"),
        1,
        "a hidden zone is skipped, not forgotten"
    );

    shell.tx().visible(bar.root, true).commit(4).unwrap();
    shell.flush().unwrap();
    h.settle();
    await_configure(&mut conn, &mut inbox, &mut win, "the bar returning");
    assert_eq!(
        win.size.h,
        OUT.1 as f32 - ZONE as f32,
        "and showing again takes it back"
    );

    drop((conn, shell));
    h.quit();
}

// ---------------------------------------------------------------------------
// The session lock (`Lock` / `Unlock`, `NITRO_LOCKED`).
// ---------------------------------------------------------------------------

const LOCK_GREEN: Color = Color::rgb(0x00, 0xC0, 0x00);

/// How many pixels of the screenshot are exactly `color`.
fn count(img: &nitro_kms::Image, color: Color) -> usize {
    let want = to_rgb(color);
    (0..img.height)
        .flat_map(|y| (0..img.width).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let o = (y * img.stride + x * 4) as usize;
            rgb(u32::from_le_bytes([
                img.data[o],
                img.data[o + 1],
                img.data[o + 2],
                img.data[o + 3],
            ])) == want
        })
        .count()
}

/// The lock screen: a shell client that locks, then shows one green
/// window. Returns the connection, its inbox and its window.
fn make_lock_screen(h: &Harness, name: &str) -> (Connection, Inbox, Win) {
    let mut inbox = Inbox::default();
    let mut conn = h.shell(name);
    conn.lock().unwrap();
    conn.flush().unwrap();
    let win = make_window(
        &mut conn,
        &mut inbox,
        50,
        "lock",
        Size::new(120.0, 80.0),
        LOCK_GREEN,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    // In the bottom-right corner, clear of the application in the middle:
    // a test that points at the application must not hit the lock screen
    // instead, or a leaking gate would go unnoticed.
    conn.tx()
        .set_anchor(win.root, anchor::BOTTOM | anchor::RIGHT, 0)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    let mut win = win;
    await_configure(&mut conn, &mut inbox, &mut win, "the lock screen's anchor");
    h.settle();
    (conn, inbox, win)
}

/// Whether `(x, y)` is inside `w`.
fn inside(w: &Win, (x, y): (f32, f32)) -> bool {
    x >= w.pos.x && y >= w.pos.y && x < w.pos.x + w.size.w && y < w.pos.y + w.size.h
}

/// An ordinary application: one red window, focused on creation.
fn make_app(h: &Harness) -> (Connection, Inbox, Win) {
    let mut inbox = Inbox::default();
    let mut conn = h.client("app");
    let win = make_window(
        &mut conn,
        &mut inbox,
        1,
        "app",
        WIN,
        RED,
        window_flags::UNDECORATED,
        Layer::Normal,
        1,
    );
    h.settle();
    (conn, inbox, win)
}

/// Whether anything that is **input** reached this connection.
fn got_input(inbox: &Inbox) -> Vec<&'static str> {
    inbox
        .0
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Key(_) => Some("Key"),
            ServerMsg::PointerEnter(_) => Some("PointerEnter"),
            ServerMsg::PointerMotion(_) => Some("PointerMotion"),
            ServerMsg::PointerButton(_) => Some("PointerButton"),
            ServerMsg::PointerAxis(_) => Some("PointerAxis"),
            ServerMsg::Focus(f) if f.focused => Some("Focus(true)"),
            _ => None,
        })
        .collect()
}

fn center(w: &Win) -> (f32, f32) {
    (w.pos.x + w.size.w / 2.0, w.pos.y + w.size.h / 2.0)
}

fn protocol_error(conn: &mut Connection, inbox: &mut Inbox, what: &str) {
    let code = expect(conn, &mut inbox.0, what, |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, nitro_wire::types::ErrorCode::Protocol, "{what}");
}

#[test]
fn a_server_started_locked_draws_nothing_but_its_lock_owner() {
    let mut h = Harness::start_with("lock-start", OUT.0, OUT.1, |c| c.locked = true);
    assert_eq!(h.stat("locked"), 1);
    assert_eq!(h.stat("lock_owned"), 0);

    // An application that connects to a locked session gets a window, and
    // nothing of it is drawn.
    let (app, _app_inbox, _) = make_app(&h);
    park(&mut h);
    assert_eq!(
        count(&h.shot(), RED),
        0,
        "no application pixel while locked"
    );

    // The first `Lock` takes the ownerless lock over, and its window, and
    // only its window, is drawn.
    let (mut lock, _lock_inbox, _) = make_lock_screen(&h, "lock");
    assert_eq!(h.stat("lock_owned"), 1);
    park(&mut h);
    let img = h.shot();
    assert!(count(&img, LOCK_GREEN) > 0, "the lock screen is drawn");
    assert_eq!(count(&img, RED), 0, "and still nothing else");

    // The owner unlocks, and the application is drawn: the control that
    // shows `count` can see a red window at all.
    lock.unlock().unwrap();
    lock.flush().unwrap();
    wait_for("the unlock", || h.stat("locked") == 0);
    park(&mut h);
    assert!(
        count(&h.shot(), RED) > 0,
        "the application after the unlock"
    );

    drop((app, lock));
    h.quit();
}

#[test]
fn while_locked_input_reaches_only_the_lock_owner_and_focus_comes_back() {
    let mut h = Harness::start("lock-input", OUT.0, OUT.1);
    let (mut app, mut app_inbox, app_win) = make_app(&h);
    expect(&mut app, &mut app_inbox.0, "the app's focus", |m| match m {
        ServerMsg::Focus(f) if f.focused => Some(()),
        _ => None,
    });

    // `Lock` alone, before any lock window exists, takes the keyboard away
    // from the application: it must not wait for a lock screen to appear
    // and take the focus itself.
    let mut early = h.shell("early");
    early.lock().unwrap();
    early.flush().unwrap();
    expect(
        &mut app,
        &mut app_inbox.0,
        "the app's focus loss",
        |m| match m {
            ServerMsg::Focus(f) if !f.focused => Some(()),
            _ => None,
        },
    );
    early.unlock().unwrap();
    early.flush().unwrap();
    wait_for("the early unlock", || h.stat("locked") == 0);
    drop(early);
    h.settle();

    let (mut lock, mut lock_inbox, lock_win) = make_lock_screen(&h, "lock");
    h.settle();
    pump(&mut app, &mut app_inbox);
    app_inbox.0.clear();

    // Pointer over where the application's window is, a click, a key
    // press: none of it reaches the application.
    let (x, y) = center(&app_win);
    assert!(
        !inside(&lock_win, (x, y)),
        "the test must point at the app, not the lock"
    );
    h.point_at(x, y, OUT);
    h.settle();
    h.button(0x110, nitro_wire::types::ButtonState::Pressed);
    h.button(0x110, nitro_wire::types::ButtonState::Released);
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    pump(&mut app, &mut app_inbox);
    assert_eq!(
        got_input(&app_inbox),
        Vec::<&str>::new(),
        "input reached the app"
    );

    // The lock screen, focused on creation, did get the key.
    expect(
        &mut lock,
        &mut lock_inbox.0,
        "the key at the lock screen",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_A => Some(()),
            _ => None,
        },
    );

    // The control socket's `focus` is refused too (it goes through the same
    // gate every focus path does).
    {
        let mut c = h.connect();
        c.get_mut().write_all(b"focus\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
    }
    h.settle();
    pump(&mut app, &mut app_inbox);
    assert_eq!(
        got_input(&app_inbox),
        Vec::<&str>::new(),
        "`focus` focused the app"
    );

    // Unlock: the application gets its focus back, and keys again.
    lock.unlock().unwrap();
    lock.flush().unwrap();
    expect(
        &mut app,
        &mut app_inbox.0,
        "focus back after the unlock",
        |m| match m {
            ServerMsg::Focus(f) if f.focused => Some(()),
            _ => None,
        },
    );
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    expect(
        &mut app,
        &mut app_inbox.0,
        "a key after the unlock",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_A => Some(()),
            _ => None,
        },
    );

    drop((app, lock));
    h.quit();
}

#[test]
fn while_locked_shell_bindings_and_window_chords_do_nothing() {
    let mut h = Harness::start("lock-keys", OUT.0, OUT.1);
    let (mut app, mut app_inbox, _) = make_app(&h);

    let mut bar_inbox = Inbox::default();
    let mut bar = h.shell("bar");
    bar.bind_key(1, mod_mask::SUPER, XK_RETURN).unwrap();
    bar.flush().unwrap();
    h.settle();
    // The control: the binding fires while unlocked.
    h.super_chord(KEY_ENTER);
    // Both edges, before the inbox is cleared: a release arriving after
    // the clear would read as a binding firing while locked.
    expect(
        &mut bar,
        &mut bar_inbox.0,
        "the HotKey unlocked",
        |m| match m {
            ServerMsg::HotKey(k) if k.id == 1 && !k.pressed => Some(()),
            _ => None,
        },
    );
    bar_inbox.0.clear();

    let (mut lock, mut lock_inbox, _) = make_lock_screen(&h, "lock");
    h.super_chord(KEY_ENTER);
    // Super+Q closes the focused window when unlocked. While locked the
    // focus is the lock screen's, so a chord that slipped through would
    // close *that*; the application is checked too.
    h.super_chord(16);
    h.settle();
    pump(&mut bar, &mut bar_inbox);
    pump(&mut app, &mut app_inbox);
    pump(&mut lock, &mut lock_inbox);
    assert!(
        !bar_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::HotKey(_))),
        "a shell binding fired while locked: {:?}",
        bar_inbox.0
    );
    for (who, inbox) in [("app", &app_inbox), ("lock", &lock_inbox)] {
        assert!(
            !inbox.0.iter().any(|m| matches!(m, ServerMsg::Closed(_))),
            "Super+Q closed the {who} window while locked"
        );
    }

    drop((app, bar, lock));
    h.quit();
}

#[test]
fn only_the_owner_unlocks_and_a_second_lock_is_refused() {
    let h = Harness::start("lock-owner", OUT.0, OUT.1);
    let (_owner, _owner_inbox, _) = make_lock_screen(&h, "owner");

    // Another shell client may not unlock...
    let mut other_inbox = Inbox::default();
    let mut other = h.shell("other");
    other.unlock().unwrap();
    other.flush().unwrap();
    protocol_error(&mut other, &mut other_inbox, "Unlock from a non-owner");
    assert_eq!(h.stat("locked"), 1, "a refused unlock changed nothing");

    // ...nor take the lock while it is owned.
    let mut second_inbox = Inbox::default();
    let mut second = h.shell("second");
    second.lock().unwrap();
    second.flush().unwrap();
    protocol_error(&mut second, &mut second_inbox, "Lock while owned");
    assert_eq!(h.stat("lock_owned"), 1);

    // And an ordinary client cannot even ask (covered for every shell op in
    // `every_shell_op_is_refused_on_the_ordinary_socket`).
    h.quit();
}

#[test]
fn unlocking_an_unlocked_session_is_refused() {
    let h = Harness::start("lock-none", OUT.0, OUT.1);
    let mut inbox = Inbox::default();
    let mut shell = h.shell("shell");
    shell.unlock().unwrap();
    shell.flush().unwrap();
    protocol_error(&mut shell, &mut inbox, "Unlock while unlocked");
    assert_eq!(h.stat("locked"), 0);
    h.quit();
}

#[test]
fn a_lock_screen_that_dies_leaves_the_session_locked_until_the_next_one() {
    let mut h = Harness::start("lock-crash", OUT.0, OUT.1);
    let (app, _app_inbox, _) = make_app(&h);
    let (first, _first_inbox, _) = make_lock_screen(&h, "first");
    park(&mut h);
    assert!(count(&h.shot(), LOCK_GREEN) > 0);

    // The lock screen goes away without unlocking.
    drop(first);
    wait_for("the owner to be forgotten", || h.stat("lock_owned") == 0);
    assert_eq!(h.stat("locked"), 1, "still locked");
    park(&mut h);
    let img = h.shot();
    assert_eq!(count(&img, RED), 0, "the application stays hidden");
    assert_eq!(
        count(&img, LOCK_GREEN),
        0,
        "and the dead lock screen is gone"
    );

    // A new lock screen takes it over, and can unlock.
    let (mut second, _second_inbox, _) = make_lock_screen(&h, "second");
    assert_eq!(h.stat("lock_owned"), 1);
    park(&mut h);
    assert!(count(&h.shot(), LOCK_GREEN) > 0);
    second.unlock().unwrap();
    second.flush().unwrap();
    wait_for("the unlock", || h.stat("locked") == 0);
    park(&mut h);
    assert!(count(&h.shot(), RED) > 0);

    drop((app, second));
    h.quit();
}

#[test]
fn a_launcher_holding_the_keyboard_loses_it_to_the_lock() {
    // An overlay with a keyboard grab is the one window that gets keys
    // without having focus. Locking must not leave that path open.
    let mut h = Harness::start("lock-grab", OUT.0, OUT.1);
    let mut launcher_inbox = Inbox::default();
    let mut launcher = h.shell("launcher");
    let over = make_window(
        &mut launcher,
        &mut launcher_inbox,
        10,
        "launcher",
        Size::new(200.0, 60.0),
        BAR_BLUE,
        window_flags::UNDECORATED | window_flags::NO_FOCUS,
        Layer::Overlay,
        1,
    );
    launcher
        .tx()
        .grab_keyboard(over.root, true)
        .commit(2)
        .unwrap();
    launcher.flush().unwrap();
    h.settle();
    // The control: the grab gets the key while unlocked.
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    // Wait for the *release*: clearing the inbox between the two would
    // leave the release to be counted as a leak below.
    expect(
        &mut launcher,
        &mut launcher_inbox.0,
        "a grabbed key",
        |m| match m {
            ServerMsg::Key(k)
                if k.keycode == KEY_A && k.state == nitro_wire::types::ButtonState::Released =>
            {
                Some(())
            }
            _ => None,
        },
    );
    launcher_inbox.0.clear();

    let (lock, _lock_inbox, _) = make_lock_screen(&h, "lock");
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    pump(&mut launcher, &mut launcher_inbox);
    assert_eq!(
        got_input(&launcher_inbox),
        Vec::<&str>::new(),
        "the grab outlived the lock"
    );
    drop((launcher, lock));
    h.quit();
}

#[test]
fn a_hidden_window_cannot_be_dragged_while_locked() {
    // Super+drag moves the window under the pointer by the server's own
    // frame hit test, not the scene's; it must not find a hidden window.
    let mut h = Harness::start("lock-drag", OUT.0, OUT.1);
    let (mut app, mut app_inbox, app_win) = make_app(&h);
    let (lock, _lock_inbox, lock_win) = make_lock_screen(&h, "lock");
    pump(&mut app, &mut app_inbox);
    app_inbox.0.clear();

    let (x, y) = center(&app_win);
    assert!(
        !inside(&lock_win, (x, y)),
        "the test must grab the app, not the lock"
    );
    h.key(KEY_LEFTMETA, true);
    h.settle();
    h.point_at(x, y, OUT);
    h.settle();
    h.button(0x110, nitro_wire::types::ButtonState::Pressed);
    h.settle();
    for i in 1..=4 {
        let t = i as f32 / 4.0;
        h.point_at(x + 60.0 * t, y + 40.0 * t, OUT);
        h.settle();
    }
    h.button(0x110, nitro_wire::types::ButtonState::Released);
    h.key(KEY_LEFTMETA, false);
    h.settle();
    pump(&mut app, &mut app_inbox);
    assert!(
        !app_inbox
            .0
            .iter()
            .any(|m| matches!(m, ServerMsg::Configure(_))),
        "the hidden window was moved: {:?}",
        app_inbox.0
    );
    assert_eq!(h.stat("dragging"), 0);
    drop((app, lock));
    h.quit();
}

#[test]
fn the_shell_cannot_focus_a_hidden_window_while_locked() {
    // `FocusWindow` is the one focus path that names a window directly,
    // bypassing the pointer and the keyboard: the task switcher's. It goes
    // through the same gate as every other.
    let h = Harness::start("lock-focus", OUT.0, OUT.1);
    let (mut app, mut app_inbox, _) = make_app(&h);
    let mut bar_inbox = Inbox::default();
    let mut bar = h.shell("bar");
    bar.window_list().unwrap();
    bar.flush().unwrap();
    let map = await_windows(&mut bar, &mut bar_inbox, "the app in the list", |m| {
        m.values().any(|i| i.title == "app")
    });
    let app_ref = *map
        .iter()
        .find(|(_, i)| i.title == "app")
        .map(|(r, _)| r)
        .unwrap();

    let (lock, _lock_inbox, _) = make_lock_screen(&h, "lock");
    pump(&mut app, &mut app_inbox);
    app_inbox.0.clear();
    bar.focus_window(app_ref).unwrap();
    bar.flush().unwrap();
    h.settle();
    pump(&mut app, &mut app_inbox);
    assert_eq!(
        got_input(&app_inbox),
        Vec::<&str>::new(),
        "FocusWindow focused a hidden window"
    );
    drop((app, bar, lock));
    h.quit();
}
