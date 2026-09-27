//! M5-I: drag and drop, end to end on the fake backend with injected
//! pointer input and real pipes. `docs/wire.md` § The drag-and-drop
//! sequence is the contract; the state machine is unit-tested in
//! `src/data.rs`, and this file is about the wiring: the grab, the
//! enter/leave/motion stream, the drop, the data transfer, the icon,
//! disconnects mid-drag, outputs, popups and descriptor accounting.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Point, Rect, Size};
use nitro_kms::Image;
use nitro_server::input::{BTN_LEFT, FakeInput, InputEvent};
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{CreatePopup, ServerMsg, StartDrag};
use nitro_wire::types::{
    ButtonState, DataSource, DragAction, ErrorCode, Layer, NodeId, PopupAnchor, PopupGravity, caps,
    drag_actions, popup_flags, window_flags,
};

/// evdev `KEY_ESC`.
const KEY_ESC: u32 = 1;
/// evdev `KEY_LEFTMETA`.
const KEY_LEFTMETA: u32 = 125;

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);
const GREEN: Color = Color::rgb(0x00, 0xFF, 0x00);
const BLUE: Color = Color::rgb(0x00, 0x00, 0xFF);
const TEXT: &str = "text/plain";

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Every test holds this for reading, and the descriptor-accounting test
/// for writing: `/proc/self/fd` counts the whole test process.
static FDS: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
    FDS.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
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
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-dnd-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: OUT.0,
            height: OUT.1,
        };
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

    /// A client that listed `DATA` and `POPUP`.
    fn peer(&self, name: &str) -> Peer {
        let mut conn = Connection::connect(&self.wire_path, name).expect("wire connect");
        assert!(conn.has_caps(caps::DATA | caps::POPUP));
        conn.client_caps(caps::DATA | caps::POPUP).unwrap();
        conn.flush().unwrap();
        Peer {
            conn,
            seen: Vec::new(),
            serial: 0,
            released: 0,
            escapes: 0,
        }
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

    fn shot(&self, output: Option<&str>) -> Image {
        let mut c = self.connect();
        let req = output.map_or_else(|| "shot\n".to_owned(), |n| format!("shot {n}\n"));
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let fields: Vec<u32> = header
            .trim_end()
            .strip_prefix("ok ")
            .unwrap_or_else(|| panic!("shot: {header}"))
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

    /// Move the pointer to a device-pixel position (the first output's
    /// width normalises it, so x past it lands on a second output).
    fn point_at(&mut self, x: f32, y: f32) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(x) / f64::from(OUT.0),
            y: f64::from(y) / f64::from(OUT.1),
            time_ns: self.time_ns,
        });
        self.settle();
    }

    fn button(&mut self, state: ButtonState) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerButton {
            button: BTN_LEFT,
            state,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    fn key(&mut self, keycode: u32, pressed: bool) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::Key {
            keycode,
            pressed,
            time_ns: self.time_ns,
        });
        self.settle();
    }

    /// Super + drag: move a window by `delta` from a point on it.
    fn super_drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        self.key(KEY_LEFTMETA, true);
        self.point_at(from.0, from.1);
        self.button(ButtonState::Pressed);
        for i in 1..=4 {
            let t = i as f32 / 4.0;
            self.point_at(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        }
        self.button(ButtonState::Released);
        self.key(KEY_LEFTMETA, false);
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

/// One wire client, with everything it has received not yet taken.
struct Peer {
    conn: Connection,
    seen: Vec<ServerMsg>,
    serial: u32,
    /// Every `PointerButton Released` ever received, taken or not.
    released: usize,
    /// Every `Key` for Escape ever received.
    escapes: usize,
}

impl Peer {
    fn poll(&mut self) -> bool {
        let _ = self.conn.flush();
        let before = self.seen.len();
        let ok = self.conn.poll(&mut self.seen).is_ok();
        for m in &self.seen[before..] {
            match m {
                ServerMsg::PointerButton(b) if b.state == ButtonState::Released => {
                    self.released += 1;
                }
                ServerMsg::Key(k) if k.keycode == KEY_ESC => self.escapes += 1,
                _ => {}
            }
        }
        ok
    }

    /// Wait until a received message matches `f`, and remove it.
    fn take(&mut self, what: &str, f: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(i) = self.seen.iter().position(&f) {
                return self.seen.remove(i);
            }
            assert!(Instant::now() < deadline, "no {what}; got {:?}", self.seen);
            assert!(self.poll(), "{what}: connection lost; got {:?}", self.seen);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Whether anything received so far matches, after draining.
    fn saw(&mut self, f: impl Fn(&ServerMsg) -> bool) -> bool {
        self.poll();
        self.seen.iter().any(f)
    }

    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    /// An undecorated window of one colour; returns its root and the
    /// output-local position the server gave it.
    fn window(&mut self, id: u32, color: Color) -> (NodeId, Point) {
        let root = NodeId(id);
        let serial = self.next_serial();
        self.conn
            .tx()
            .create_window_with(root, "w", WIN, Layer::Normal, window_flags::UNDECORATED)
            .create_rect(NodeId(id + 1), root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
            .fill_solid(NodeId(id + 1), color)
            .commit(serial)
            .unwrap();
        let ServerMsg::Configure(c) = self.take(
            "Configure",
            |m| matches!(m, ServerMsg::Configure(c) if c.window == root),
        ) else {
            unreachable!()
        };
        (root, c.position)
    }

    fn start_drag(&mut self, window: NodeId, icon: NodeId) {
        self.conn
            .start_drag(StartDrag {
                window,
                icon,
                actions: drag_actions::COPY | drag_actions::MOVE,
                mimes: vec![TEXT.to_owned()],
            })
            .unwrap();
        let serial = self.next_serial();
        self.conn.commit(serial).unwrap();
        self.conn.flush().unwrap();
    }

    fn drag_enter(&mut self, window: NodeId) -> nitro_wire::msg::DragEnter {
        match self.take(
            "DragEnter",
            |m| matches!(m, ServerMsg::DragEnter(e) if e.window == window),
        ) {
            ServerMsg::DragEnter(e) => e,
            _ => unreachable!(),
        }
    }

    fn drag_leave(&mut self, window: NodeId) {
        self.take(
            "DragLeave",
            |m| matches!(m, ServerMsg::DragLeave(l) if l.window == window),
        );
    }

    fn finished(&mut self) -> (bool, DragAction) {
        match self.take("DragFinished", |m| matches!(m, ServerMsg::DragFinished(_))) {
            ServerMsg::DragFinished(f) => (f.accepted, f.action),
            _ => unreachable!(),
        }
    }

    fn accept(&mut self, action: DragAction, mime: &str) {
        self.conn.accept_drop(action, mime).unwrap();
        self.conn.flush().unwrap();
    }

    fn finish(&mut self) {
        self.conn.finish_drag().unwrap();
        self.conn.flush().unwrap();
    }

    fn request(&mut self, request: u32) {
        self.conn
            .request_selection(request, DataSource::Drag, TEXT)
            .unwrap();
        self.conn.flush().unwrap();
    }

    /// Answer the next drag `SelectionRequest` with a pipe carrying `bytes`.
    fn serve(&mut self, bytes: &[u8]) {
        let ServerMsg::SelectionRequest(r) = self.take("SelectionRequest", |m| {
            matches!(m, ServerMsg::SelectionRequest(_))
        }) else {
            unreachable!()
        };
        assert_eq!((r.source, r.mime.as_str()), (DataSource::Drag, TEXT));
        let (rd, w) = rustix::pipe::pipe().unwrap();
        self.conn.send_selection(r.request, rd).unwrap();
        self.conn.flush().unwrap();
        rustix::io::write(&w, bytes).unwrap();
    }

    fn data(&mut self, request: u32) -> OwnedFd {
        match self.take(
            "SelectionData",
            |m| matches!(m, ServerMsg::SelectionData(d) if d.request == request),
        ) {
            ServerMsg::SelectionData(d) => d.fd,
            _ => unreachable!(),
        }
    }

    fn error(&mut self) -> ErrorCode {
        match self.take("Error", |m| matches!(m, ServerMsg::Error(_))) {
            ServerMsg::Error(e) => e.code,
            _ => unreachable!(),
        }
    }
}

fn read_all(fd: &OwnedFd) -> Vec<u8> {
    let flags = rustix::fs::fcntl_getfl(fd).unwrap();
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(rustix::io::Errno::AGAIN) => {
                assert!(Instant::now() < deadline, "no EOF on the drag fd");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("read: {e}"),
        }
    }
}

fn rgb(px: u32) -> u32 {
    px & 0x00ff_ffff
}

fn to_rgb(c: Color) -> u32 {
    u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

/// Two clients, A (red) and B (green), with their windows pulled apart
/// so they sit side by side without overlapping. Returns the peers, the
/// window roots and a point in the middle of each.
struct Scene2 {
    a: Peer,
    b: Peer,
    wa: NodeId,
    wb: NodeId,
    at_a: (f32, f32),
    at_b: (f32, f32),
}

fn two(h: &mut Harness) -> Scene2 {
    let mut a = h.peer("a");
    let mut b = h.peer("b");
    let (wa, pa) = a.window(1, RED);
    let (wb, pb) = b.window(1, GREEN);
    h.settle();
    // The cascade overlaps them by design; Super-drag A left, B right.
    h.super_drag((pa.x + 10.0, pa.y + 10.0), (pa.x - 190.0, pa.y + 10.0));
    let near_b = (pb.x + WIN.w - 10.0, pb.y + WIN.h - 10.0);
    h.super_drag(near_b, (near_b.0 + 150.0, near_b.1));
    let at_a = (pa.x - 200.0 + WIN.w / 2.0, pa.y + WIN.h / 2.0);
    let at_b = (pb.x + 150.0 + WIN.w / 2.0, pb.y + WIN.h / 2.0);
    assert!(at_a.0 + WIN.w / 2.0 < at_b.0 - WIN.w / 2.0, "side by side");
    Scene2 {
        a,
        b,
        wa,
        wb,
        at_a,
        at_b,
    }
}

/// Press on A, start a drag of `text/plain` from it, and carry it onto B:
/// the half every test shares. Returns B's `DragEnter`.
fn drag_onto_b(h: &mut Harness, s: &mut Scene2, icon: NodeId) -> nitro_wire::msg::DragEnter {
    h.point_at(s.at_a.0, s.at_a.1);
    h.button(ButtonState::Pressed);
    let wa = s.wa;
    s.a.take(
        "the press",
        |m| matches!(m, ServerMsg::PointerButton(b) if b.window == wa && b.state == ButtonState::Pressed),
    );
    s.a.start_drag(wa, icon);
    s.a.take(
        "PointerLeave",
        |m| matches!(m, ServerMsg::PointerLeave(l) if l.window == wa),
    );
    s.a.drag_enter(wa);
    h.point_at(s.at_b.0, s.at_b.1);
    s.a.drag_leave(wa);
    let enter = s.b.drag_enter(s.wb);
    h.point_at(s.at_b.0 + 5.0, s.at_b.1 + 5.0);
    let wb = s.wb;
    s.b.take(
        "DragMotion",
        |m| matches!(m, ServerMsg::DragMotion(d) if d.window == wb),
    );
    enter
}

fn accept_and_drop(h: &mut Harness, s: &mut Scene2) {
    s.b.accept(DragAction::Copy, TEXT);
    wait_for("the acceptance", || h.stat("dnd_accepted") == 1);
    h.button(ButtonState::Released);
    let wb = s.wb;
    s.b.take(
        "DragDrop",
        |m| matches!(m, ServerMsg::DragDrop(d) if d.window == wb),
    );
}

/// The drop's second half: B reads the data and finishes, A hears and
/// finishes, and nothing is left held.
fn transfer_and_finish(h: &Harness, s: &mut Scene2, request: u32, bytes: &[u8]) {
    s.b.request(request);
    s.a.serve(bytes);
    assert_eq!(read_all(&s.b.data(request)), bytes);
    s.b.finish();
    assert_eq!(s.a.finished(), (true, DragAction::Copy));
    s.a.finish();
    wait_for("the drag to be released", || h.stat("dnd_active") == 0);
}

// ------------------------------------------------------------------ tests

#[test]
fn a_full_drag_between_two_clients_moves_real_data() {
    let _fds = shared();
    let mut h = Harness::start("full");
    let mut s = two(&mut h);
    let enter = drag_onto_b(&mut h, &mut s, NodeId::NONE);
    assert_eq!(enter.mimes, [TEXT]);
    assert_eq!(enter.actions, drag_actions::COPY | drag_actions::MOVE);
    assert_eq!(h.stat("dnd_grab"), 1);
    assert_eq!(h.stat("dnd_accepted"), 0, "nothing accepted yet");
    accept_and_drop(&mut h, &mut s);
    assert_eq!(h.stat("dnd_grab"), 0, "the grab ends at the drop");
    assert_eq!(h.stat("dnd_active"), 1, "the target is still reading");
    transfer_and_finish(&h, &mut s, 7, b"hello, drop");
    let wb = s.wb;
    s.b.take(
        "PointerEnter",
        |m| matches!(m, ServerMsg::PointerEnter(e) if e.window == wb),
    );
    assert!(s.a.poll());
    assert_eq!(s.a.released, 0, "the source never sees its release");
    assert_eq!(s.b.released, 0, "nor does the target");
    assert_eq!(h.stat("dnd_drops"), 1);
    assert_eq!(h.stat("selections_pending"), 0);

    // The pointer is B's again: a click is an ordinary click.
    h.button(ButtonState::Pressed);
    h.button(ButtonState::Released);
    s.b.take("an ordinary release", |m| {
        matches!(m, ServerMsg::PointerButton(b) if b.window == wb && b.state == ButtonState::Released)
    });
    h.quit();
}

#[test]
fn a_rejecting_target_gives_the_source_accepted_false() {
    let _fds = shared();
    let mut h = Harness::start("reject");
    let mut s = two(&mut h);
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    s.b.accept(DragAction::Copy, TEXT);
    wait_for("the acceptance", || h.stat("dnd_accepted") == 1);
    s.b.accept(DragAction::None, "");
    wait_for("the rejection", || h.stat("dnd_accepted") == 0);
    h.button(ButtonState::Released);
    s.b.drag_leave(s.wb);
    assert_eq!(s.a.finished(), (false, DragAction::None));
    assert!(!s.b.saw(|m| matches!(m, ServerMsg::DragDrop(_))));
    // The offer is gone for B: a drag read now is a protocol error.
    assert_eq!(h.stat("dnd_active"), 1, "the source still owes FinishDrag");
    s.a.finish();
    wait_for("the release", || h.stat("dnd_active") == 0);
    assert_eq!(h.stat("dnd_cancels"), 1);
    s.b.request(1);
    assert_eq!(s.b.error(), ErrorCode::Protocol);
    h.quit();
}

#[test]
fn the_source_disconnecting_mid_drag_releases_the_grab() {
    let _fds = shared();
    let mut h = Harness::start("srcgone");
    let mut s = two(&mut h);
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    s.b.request(3);
    drop(s.a);
    s.b.drag_leave(s.wb);
    assert!(
        read_all(&s.b.data(3)).is_empty(),
        "the parked read ends at EOF"
    );
    wait_for("the grab to go", || h.stat("dnd_grab") == 0);
    assert_eq!(h.stat("dnd_active"), 0);
    // The button still held from the drag comes up unseen; the next
    // click is B's.
    h.button(ButtonState::Released);
    assert!(s.b.poll());
    assert_eq!(s.b.released, 0, "no release without its press");
    h.button(ButtonState::Pressed);
    h.button(ButtonState::Released);
    let wb = s.wb;
    s.b.take("a click", |m| {
        matches!(m, ServerMsg::PointerButton(b) if b.window == wb && b.state == ButtonState::Released)
    });
    h.quit();
}

#[test]
fn the_target_disconnecting_mid_drag_or_after_the_drop_fails_the_drag() {
    let _fds = shared();
    let mut h = Harness::start("dstgone");
    // (a) mid-drag: the drag carries on over nothing and rejects.
    let mut s = two(&mut h);
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    s.b.accept(DragAction::Copy, TEXT);
    wait_for("the acceptance", || h.stat("dnd_accepted") == 1);
    drop(s.b);
    wait_for("the acceptance to go with it", || {
        h.stat("dnd_accepted") == 0
    });
    assert_eq!(h.stat("dnd_grab"), 1, "the drag carries on");
    h.button(ButtonState::Released);
    assert_eq!(s.a.finished(), (false, DragAction::None));
    s.a.finish();
    wait_for("the release", || h.stat("dnd_active") == 0);
    drop(s.a);

    // (b) after the drop, before the target's FinishDrag.
    let mut s = two(&mut h);
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    accept_and_drop(&mut h, &mut s);
    drop(s.b);
    assert_eq!(s.a.finished(), (false, DragAction::None));
    s.a.finish();
    wait_for("the release", || h.stat("dnd_active") == 0);
    h.quit();
}

#[test]
fn a_drag_crosses_an_output_boundary_icon_and_all() {
    let _fds = shared();
    let mut h = Harness::start("outputs");
    let mut s = two(&mut h);
    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the second output", || h.stat("outputs") == 2);
    h.settle();
    // Carry B onto the second output, which starts at x = 640.
    let target = (OUT.0 as f32 + 150.0, 120.0);
    h.super_drag(s.at_b, target);
    s.at_b = target;

    let icon = icon_window(&mut s.a, 50);
    h.point_at(s.at_a.0, s.at_a.1);
    h.button(ButtonState::Pressed);
    s.a.start_drag(s.wa, icon);
    s.a.drag_enter(s.wa);
    // Just short of the boundary the icon is drawn on output 1, centred.
    h.point_at(600.0, 120.0);
    let img = h.shot(None);
    assert_eq!(rgb(img.pixel(575, 105)), to_rgb(BLUE), "on output 1");
    // Past it, the icon has left output 1 with the pointer, and the
    // window on output 2 is the target. Checked by its absence from
    // output 1 rather than a pixel on output 2: a `shot` of the hot-plugged
    // fake output does not show B's window either, so it cannot tell a
    // placed icon from a lost one.
    h.point_at(target.0, target.1);
    let img = h.shot(None);
    let blue = (0..img.height)
        .flat_map(|y| (0..img.width).map(move |x| (x, y)))
        .any(|(x, y)| rgb(img.pixel(x, y)) == to_rgb(BLUE));
    assert!(!blue, "the icon moved to output 2 with the pointer");
    s.b.drag_enter(s.wb);
    h.point_at(target.0 + 5.0, target.1 + 5.0);
    let wb = s.wb;
    s.b.take(
        "DragMotion",
        |m| matches!(m, ServerMsg::DragMotion(d) if d.window == wb),
    );
    accept_and_drop(&mut h, &mut s);
    transfer_and_finish(&h, &mut s, 1, b"far away");
    h.quit();
}

/// A 60x40 blue drag-icon window, created but not yet adopted.
fn icon_window(p: &mut Peer, id: u32) -> NodeId {
    let root = NodeId(id);
    let size = Size::new(60.0, 40.0);
    p.conn
        .tx()
        .create_window_with(
            root,
            "icon",
            size,
            Layer::Normal,
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
        )
        .create_rect(NodeId(id + 1), root, Rect::new(0.0, 0.0, size.w, size.h))
        .fill_solid(NodeId(id + 1), BLUE)
        .finish()
        .unwrap();
    root
}

#[test]
fn the_icon_follows_the_pointer_is_not_hit_and_comes_down_at_release() {
    let _fds = shared();
    let mut h = Harness::start("icon");
    let mut s = two(&mut h);
    let icon = icon_window(&mut s.a, 50);
    drag_onto_b(&mut h, &mut s, icon);
    // B got its DragEnter although the icon sits under the pointer: the
    // icon is hit-exempt. It is drawn centred on the pointer, clear of
    // the (centred, 24x24) cursor.
    let (px, py) = (s.at_b.0 + 5.0, s.at_b.1 + 5.0);
    let img = h.shot(None);
    assert_eq!(
        rgb(img.pixel((px - 25.0) as u32, (py - 15.0) as u32)),
        to_rgb(BLUE)
    );
    assert_eq!(
        rgb(img.pixel((px - 35.0) as u32, (py - 15.0) as u32)),
        to_rgb(GREEN),
        "and no wider than itself"
    );

    // It is not an application window.
    let mut shell = Connection::connect(&h.shell_path, "bar").unwrap();
    shell.window_list().unwrap();
    shell.flush().unwrap();
    let mut got = Vec::new();
    wait_for("the window list", || {
        let _ = shell.poll(&mut got);
        got.iter().any(|m| matches!(m, ServerMsg::WindowListEnd(_)))
    });
    let listed = got
        .iter()
        .filter(|m| matches!(m, ServerMsg::WindowInfo(_)))
        .count();
    assert_eq!(listed, 2, "A's window and B's, not the icon: {got:?}");

    accept_and_drop(&mut h, &mut s);
    let img = h.shot(None);
    assert_eq!(
        rgb(img.pixel((px - 25.0) as u32, (py - 15.0) as u32)),
        to_rgb(GREEN),
        "the icon came down at the release"
    );
    transfer_and_finish(&h, &mut s, 1, b"x");
    h.quit();
}

#[test]
fn a_decorated_or_focusable_icon_is_fatal() {
    let _fds = shared();
    let mut h = Harness::start("badicon");
    for (i, (flags, name)) in [
        (window_flags::NO_FOCUS, "decorated"),
        (window_flags::UNDECORATED, "focusable"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut a = h.peer(name);
        let (wa, pa) = a.window(1, RED);
        a.conn
            .tx()
            .create_window_with(
                NodeId(50),
                "icon",
                Size::new(20.0, 20.0),
                Layer::Normal,
                flags,
            )
            .finish()
            .unwrap();
        h.point_at(pa.x + 20.0 + i as f32, pa.y + 20.0);
        h.button(ButtonState::Pressed);
        a.start_drag(wa, NodeId(50));
        assert_eq!(a.error(), ErrorCode::Protocol, "{name}");
        h.button(ButtonState::Released);
    }
    h.quit();
}

/// Set `icon`'s hotspot offset and commit it.
fn set_offset(p: &mut Peer, icon: NodeId, x: f32, y: f32) {
    p.conn.set_drag_icon_offset(icon, Point::new(x, y)).unwrap();
    let serial = p.next_serial();
    p.conn.commit(serial).unwrap();
    p.conn.flush().unwrap();
}

#[test]
fn an_icon_offset_puts_the_grab_point_under_the_pointer() {
    let _fds = shared();
    let mut h = Harness::start("iconoff");
    let mut s = two(&mut h);
    let icon = icon_window(&mut s.a, 50);
    // Grabbed 5 px in from its top-left, set before the drag.
    set_offset(&mut s.a, icon, -5.0, -5.0);
    drag_onto_b(&mut h, &mut s, icon);
    let (px, py) = (s.at_b.0 + 5.0, s.at_b.1 + 5.0);
    let img = h.shot(None);
    assert_eq!(
        rgb(img.pixel((px + 50.0) as u32, (py + 30.0) as u32)),
        to_rgb(BLUE),
        "the 60x40 icon starts 5 px up and left of the pointer"
    );
    assert_eq!(
        rgb(img.pixel((px - 20.0) as u32, (py - 12.0) as u32)),
        to_rgb(GREEN),
        "where centring would have put it"
    );
    accept_and_drop(&mut h, &mut s);
    transfer_and_finish(&h, &mut s, 1, b"x");
    h.quit();
}

#[test]
fn an_icon_offset_changed_mid_drag_moves_the_icon_at_once() {
    let _fds = shared();
    let mut h = Harness::start("iconmove");
    let mut s = two(&mut h);
    let icon = icon_window(&mut s.a, 50);
    drag_onto_b(&mut h, &mut s, icon);
    let (px, py) = (s.at_b.0 + 5.0, s.at_b.1 + 5.0);
    let img = h.shot(None);
    let (fx, fy) = ((px + 50.0) as u32, (py + 30.0) as u32);
    assert_eq!(rgb(img.pixel(fx, fy)), to_rgb(GREEN), "centred at first");
    // The pointer does not move; the icon does.
    set_offset(&mut s.a, icon, 0.0, 0.0);
    h.settle();
    let img = h.shot(None);
    assert_eq!(
        rgb(img.pixel(fx, fy)),
        to_rgb(BLUE),
        "its top-left is at the pointer now"
    );
    assert_eq!(
        rgb(img.pixel((px - 20.0) as u32, (py - 12.0) as u32)),
        to_rgb(GREEN)
    );
    accept_and_drop(&mut h, &mut s);
    transfer_and_finish(&h, &mut s, 1, b"x");
    h.quit();
}

#[test]
fn an_icon_offset_without_data_is_fatal() {
    let _fds = shared();
    let h = Harness::start("iconcaps");
    let mut p = Peer {
        conn: Connection::connect(&h.wire_path, "rude").expect("wire connect"),
        seen: Vec::new(),
        serial: 0,
        released: 0,
        escapes: 0,
    };
    let icon = icon_window(&mut p, 50);
    set_offset(&mut p, icon, -1.0, -1.0);
    assert_eq!(p.error(), ErrorCode::Protocol);
    h.quit();
}

#[test]
fn a_bad_icon_offset_is_fatal() {
    let _fds = shared();
    let h = Harness::start("badoff");
    for (i, (flags, x, name)) in [
        (window_flags::NO_FOCUS, 0.0, "decorated"),
        (window_flags::UNDECORATED, 0.0, "focusable"),
        (
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
            f32::NAN,
            "nan",
        ),
        (
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
            f32::INFINITY,
            "inf",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut a = h.peer(name);
        let id = NodeId(50 + i as u32);
        a.conn
            .tx()
            .create_window_with(id, "icon", Size::new(20.0, 20.0), Layer::Normal, flags)
            .finish()
            .unwrap();
        set_offset(&mut a, id, x, 0.0);
        assert_eq!(a.error(), ErrorCode::Protocol, "{name}");
    }
    // Not a window at all.
    let mut a = h.peer("none");
    set_offset(&mut a, NodeId::NONE, 0.0, 0.0);
    assert_eq!(a.error(), ErrorCode::Protocol, "none");
    h.quit();
}

#[test]
fn a_start_drag_without_a_button_or_pointer_focus_is_ignored() {
    let _fds = shared();
    let mut h = Harness::start("auth");
    let mut s = two(&mut h);
    // No button down.
    h.point_at(s.at_a.0, s.at_a.1);
    s.a.start_drag(s.wa, NodeId::NONE);
    h.settle();
    // Button down, but B does not hold pointer focus.
    h.button(ButtonState::Pressed);
    s.b.start_drag(s.wb, NodeId::NONE);
    h.settle();
    h.button(ButtonState::Released);
    assert!(s.a.poll() && s.b.poll(), "both survive");
    assert_eq!(h.stat("dnd_active"), 0);
    assert!(!s.a.saw(|m| matches!(m, ServerMsg::DragEnter(_))));
    assert!(!s.b.saw(|m| matches!(m, ServerMsg::DragEnter(_))));
    assert_eq!(s.a.released, 1, "an ordinary click");
    h.quit();
}

fn menu(p: &mut Peer, id: u32, parent: NodeId) {
    let root = NodeId(id);
    let size = Size::new(60.0, 40.0);
    p.conn
        .tx()
        .create_popup(CreatePopup {
            id: root,
            parent,
            anchor_rect: IRect::new(10, 10, 10, 10),
            anchor: PopupAnchor::BottomLeft,
            gravity: PopupGravity::BottomRight,
            constraint: 0,
            size,
            flags: popup_flags::GRAB,
        })
        .create_rect(NodeId(id + 1), root, Rect::new(0.0, 0.0, size.w, size.h))
        .fill_solid(NodeId(id + 1), BLUE)
        .finish()
        .unwrap();
    let serial = p.next_serial();
    p.conn.commit(serial).unwrap();
    p.conn.flush().unwrap();
}

#[test]
fn popups_and_drags_exclude_each_other() {
    let _fds = shared();
    let mut h = Harness::start("popups");
    let mut s = two(&mut h);
    // A drag started from inside an open menu dismisses the menu.
    menu(&mut s.a, 30, s.wa);
    let ServerMsg::Configure(c) = s.a.take(
        "the menu's Configure",
        |m| matches!(m, ServerMsg::Configure(c) if c.window == NodeId(30)),
    ) else {
        unreachable!()
    };
    h.point_at(c.position.x + 30.0, c.position.y + 20.0);
    h.button(ButtonState::Pressed);
    s.a.start_drag(s.wa, NodeId::NONE);
    s.a.take(
        "PopupDone",
        |m| matches!(m, ServerMsg::PopupDone(d) if d.popup == NodeId(30)),
    );
    wait_for("the drag", || h.stat("dnd_grab") == 1);

    // A grabbing menu opened mid-drag is done before it is ever mapped.
    menu(&mut s.a, 40, s.wa);
    s.a.take(
        "PopupDone",
        |m| matches!(m, ServerMsg::PopupDone(d) if d.popup == NodeId(40)),
    );
    assert_eq!(h.stat("dnd_grab"), 1, "the drag is unaffected");
    h.button(ButtonState::Released);
    assert_eq!(s.a.finished(), (false, DragAction::None));
    s.a.finish();
    wait_for("the release", || h.stat("dnd_active") == 0);
    assert!(s.a.poll());
    assert_eq!(s.a.released, 0);
    h.quit();
}

#[test]
fn escape_cancels_a_drag_and_swallows_what_it_consumed() {
    let _fds = shared();
    let mut h = Harness::start("escape");
    let mut s = two(&mut h);
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    s.b.accept(DragAction::Copy, TEXT);
    wait_for("the acceptance", || h.stat("dnd_accepted") == 1);
    h.key(KEY_ESC, true);
    s.b.drag_leave(s.wb);
    assert_eq!(s.a.finished(), (false, DragAction::None));
    assert_eq!(h.stat("dnd_grab"), 0);
    h.key(KEY_ESC, false);
    // The button that carried the drag comes up unseen.
    h.button(ButtonState::Released);
    s.a.finish();
    wait_for("the release", || h.stat("dnd_active") == 0);
    assert!(s.a.poll() && s.b.poll());
    assert_eq!((s.a.escapes, s.b.escapes), (0, 0), "Escape was the drag's");
    assert_eq!((s.a.released, s.b.released), (0, 0));
    assert!(!s.b.saw(|m| matches!(m, ServerMsg::DragDrop(_))));
    h.quit();
}

#[test]
fn a_drag_read_from_a_non_target_is_fatal() {
    let _fds = shared();
    let mut h = Harness::start("nontarget");
    let mut s = two(&mut h);
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    // A is the source, not the target: it may not read its own offer.
    s.a.request(1);
    assert_eq!(s.a.error(), ErrorCode::Protocol);
    h.quit();
}

#[test]
fn no_descriptors_leak_across_many_drags() {
    let _only = FDS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut h = Harness::start("leak");
    let mut s = two(&mut h);
    // Warm up: one full drag, so every lazily-opened descriptor exists.
    drag_onto_b(&mut h, &mut s, NodeId::NONE);
    accept_and_drop(&mut h, &mut s);
    transfer_and_finish(&h, &mut s, 1_000, b"warm");
    let base = open_fds();
    for i in 0..30u32 {
        drag_onto_b(&mut h, &mut s, NodeId::NONE);
        accept_and_drop(&mut h, &mut s);
        transfer_and_finish(&h, &mut s, i, format!("n{i}").as_bytes());
    }
    assert_eq!(h.stat("dnd_drops"), 31);
    assert_eq!(h.stat("selections_pending"), 0);
    wait_for("the descriptor count to return to baseline", || {
        open_fds() == base
    });
    h.quit();
}
