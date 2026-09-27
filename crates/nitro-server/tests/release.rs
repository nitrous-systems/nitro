//! `BufferReleased` (M5-B), end to end on the fake backend: a client that
//! waits for the release before rewriting a buffer never tears, a buffer
//! shown on two outputs is released once, a client without `RELEASE`
//! hears nothing, and a release never costs an idle desktop a wakeup.
//!
//! The server maps the client's pages and reads them at every repaint
//! that touches an image, so "released" means "no image node references
//! it" — see `Scene::take_released_buffers`.

// `a`/`b` are the two buffers of a double-buffered client, which is what
// the protocol docs call them; longer names would obscure the rotation.
#![allow(clippy::many_single_char_names)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{IRect, Rect, Size};
use nitro_kms::Image;
use nitro_server::input::{BTN_LEFT, FakeInput, InputEvent};
use nitro_server::wm;
use nitro_server::{BackendKind, Config, run};
use nitro_shm::MappingMut;
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, CreateBuffer, ServerMsg};
use nitro_wire::types::{BufferId, ButtonState, Layer, NodeId, caps, format};

const OUT: (u32, u32) = (400, 300);
/// Buffers are 32x32 XR24, shown 1:1 by a 32x32 image node.
const SIDE: u32 = 32;
const STRIDE: u32 = SIDE * 4;

const RED: u32 = 0x00FF_0000;
const GREEN: u32 = 0x0000_FF00;
const BLUE: u32 = 0x0000_00FF;

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
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-rel-{}-{name}", std::process::id()));
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
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            input,
            time_ns: 1_000_000,
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

    /// No flip in flight and the frame counter has stopped moving.
    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let frames = self.stat("frames");
            if self.stat("flips_pending") == 0 && frames == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = frames;
            std::thread::sleep(Duration::from_millis(8));
            stable >= 3
        });
    }

    /// Move the pointer to a device-pixel position, in units of the first
    /// output (so `x > OUT.0` reaches an output plugged to its right).
    fn point_at(&mut self, x: f32, y: f32) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(x) / f64::from(OUT.0),
            y: f64::from(y) / f64::from(OUT.1),
            time_ns: self.time_ns,
        });
    }

    fn button(&mut self, state: ButtonState) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerButton {
            button: BTN_LEFT,
            state,
            time_ns: self.time_ns,
        });
    }

    fn drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        self.point_at(from.0, from.1);
        self.settle();
        self.button(ButtonState::Pressed);
        self.settle();
        for i in 1..=4 {
            let t = i as f32 / 4.0;
            self.point_at(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.settle();
        }
        self.button(ButtonState::Released);
        self.settle();
    }

    /// Sweep the pointer across `c`'s content and park it in the far
    /// corner, settling after each step: every step repaints the cursor's
    /// old and new rects, which re-reads any image under them.
    fn sweep(&mut self, c: &Configure) {
        for i in 0..=8 {
            let t = i as f32 / 8.0;
            self.point_at(
                c.position.x + t * SIDE as f32,
                c.position.y + t * SIDE as f32,
            );
            self.settle();
        }
        self.park();
    }

    fn park(&mut self) {
        self.point_at(OUT.0 as f32 - 2.0, OUT.1 as f32 - 2.0);
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

/// Drain a client's socket until `f` matches, or time out.
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

/// Wait for `Presented { serial }`, then pick up anything else queued.
fn presented(conn: &mut Connection, seen: &mut Vec<ServerMsg>, serial: u32) {
    expect(conn, seen, &format!("Presented {serial}"), |m| match m {
        ServerMsg::Presented(p) if p.serial == serial => Some(()),
        _ => None,
    });
    let _ = conn.poll(seen);
}

fn releases(seen: &[ServerMsg]) -> Vec<BufferId> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::BufferReleased(r) => Some(r.id),
            _ => None,
        })
        .collect()
}

/// A client-side buffer: the fd to send and a writable mapping of it.
struct Buf {
    id: BufferId,
    map: MappingMut,
    fd: OwnedFd,
}

impl Buf {
    fn new(id: u32, color: u32) -> Self {
        let fd = nitro_shm::create_sealed("nitro-release-test", u64::from(STRIDE * SIDE)).unwrap();
        let map = MappingMut::map_mut(fd.as_fd(), (STRIDE * SIDE) as usize).unwrap();
        let mut b = Self {
            id: BufferId(id),
            map,
            fd,
        };
        b.fill(color);
        b
    }

    fn fill(&mut self, color: u32) {
        for px in self.map.as_bytes_mut().chunks_exact_mut(4) {
            px.copy_from_slice(&color.to_le_bytes());
        }
    }

    fn create(&self) -> CreateBuffer {
        CreateBuffer {
            id: self.id,
            width: SIDE,
            height: SIDE,
            stride: STRIDE,
            format: format::XR24,
            size: STRIDE * SIDE,
            fd: self.fd.try_clone().unwrap(),
        }
    }
}

fn full() -> IRect {
    IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed())
}

/// A 64x64 window whose top-left 32x32 is image node `root + 1`.
fn image_window(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    root: u32,
    buf: &Buf,
    serial: u32,
) -> (NodeId, NodeId, Configure) {
    let (root, image) = (NodeId(root), NodeId(root + 1));
    conn.tx()
        .create_window(root, "rel", Size::new(64.0, 64.0), Layer::Normal)
        .create_image(image, root, Rect::new(0.0, 0.0, SIDE as f32, SIDE as f32))
        .image(image, buf.id, full())
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    presented(conn, seen, serial);
    (root, image, c)
}

/// Every pixel of the image node, which sits at the content origin.
fn assert_image(h: &Harness, c: &Configure, color: u32, what: &str) {
    let img = h.shot();
    let (x0, y0) = (c.position.x as u32, c.position.y as u32);
    for y in y0..y0 + SIDE {
        for x in x0..x0 + SIDE {
            assert_eq!(img.pixel(x, y) & 0x00FF_FFFF, color, "{what} at ({x},{y})");
        }
    }
}

/// Swap `image` onto `buf` and commit; the rotation step of a
/// double-buffered client.
fn attach(conn: &mut Connection, image: NodeId, buf: &Buf, serial: u32) {
    conn.tx()
        .image(image, buf.id, full())
        .buffer_damage(buf.id, vec![full()])
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
}

#[test]
fn a_client_that_waits_for_release_never_tears() {
    let mut h = Harness::start("notear");
    h.park();
    let mut conn = h.client("rotor");
    conn.client_caps(caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    let mut a = Buf::new(1, RED);
    let b = Buf::new(2, GREEN);
    conn.tx()
        .create_buffer(a.create())
        .create_buffer(b.create())
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let (_, image, c) = image_window(&mut conn, &mut seen, 10, &a, 2);
    h.settle();
    assert_image(&h, &c, RED, "A");

    // Repaints of an attached buffer are reads: no release, however many.
    h.sweep(&c);
    let _ = conn.poll(&mut seen);
    assert_eq!(releases(&seen), [], "an attached buffer is never released");

    // Swap to B: A is released in the same batch as the commit's reply.
    seen.clear();
    attach(&mut conn, image, &b, 3);
    presented(&mut conn, &mut seen, 3);
    assert_eq!(releases(&seen), [a.id], "A released, B not");
    let rel = seen
        .iter()
        .position(|m| matches!(m, ServerMsg::BufferReleased(_)))
        .unwrap();
    let pres = seen
        .iter()
        .position(|m| matches!(m, ServerMsg::Presented(p) if p.serial == 3))
        .unwrap();
    assert!(
        rel < pres,
        "the release is not held back to the flip: {seen:?}"
    );

    // The client takes it at its word and scribbles into A. Nothing the
    // server repaints may show it.
    a.fill(BLUE);
    h.sweep(&c);
    assert_image(&h, &c, GREEN, "B, never the rewritten A");
    let _ = conn.poll(&mut seen);
    assert_eq!(releases(&seen), [a.id], "B is still attached");

    // Rotate back: B released, A's new pixels shown.
    seen.clear();
    attach(&mut conn, image, &a, 4);
    presented(&mut conn, &mut seen, 4);
    assert_eq!(releases(&seen), [b.id]);
    h.settle();
    assert_image(&h, &c, BLUE, "A, rewritten");

    drop(conn);
    h.quit();
}

#[test]
fn a_buffer_on_two_outputs_is_released_once_after_both_let_go() {
    let mut h = Harness::start("twoout");
    h.park();
    let mut conn = h.client("shared");
    conn.client_caps(caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    let x = Buf::new(1, GREEN);
    conn.tx().create_buffer(x.create()).commit(1).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let (_, one, _) = image_window(&mut conn, &mut seen, 10, &x, 2);
    let (win2, two, c2) = image_window(&mut conn, &mut seen, 20, &x, 3);

    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the second output", || h.stat("outputs") == 2);
    h.settle();
    let inset = wm::frame_insets();
    let bar = (
        c2.position.x - inset.left + 20.0,
        c2.position.y - inset.top + wm::TITLE_H / 2.0,
    );
    h.drag(bar, (OUT.0 as f32 + 150.0, 120.0));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        conn.flush().unwrap();
        let _ = conn.poll(&mut seen);
        let newest = seen.iter().rev().find_map(|m| match m {
            ServerMsg::Configure(c) if c.window == win2 => Some(c.output),
            _ => None,
        });
        if newest.is_some_and(|o| o != c2.output) {
            break;
        }
        assert!(Instant::now() < deadline, "window 2 never reached output 2");
        std::thread::sleep(Duration::from_millis(5));
    }
    h.park();
    assert_eq!(releases(&seen), []);

    conn.tx()
        .image(one, BufferId::NONE, full())
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 4);
    h.settle();
    let _ = conn.poll(&mut seen);
    assert_eq!(releases(&seen), [], "output 2 still reads it");

    conn.tx()
        .image(two, BufferId::NONE, full())
        .commit(5)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 5);
    h.settle();
    h.sweep(&c2);
    let _ = conn.poll(&mut seen);
    assert_eq!(releases(&seen), [x.id], "once, after both let go");

    drop(conn);
    h.quit();
}

#[test]
fn without_the_cap_no_buffer_released_is_sent() {
    let mut h = Harness::start("nocap");
    h.park();
    let mut conn = h.client("legacy");
    let mut seen = Vec::new();
    let a = Buf::new(1, RED);
    let b = Buf::new(2, GREEN);
    conn.tx()
        .create_buffer(a.create())
        .create_buffer(b.create())
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let (_, image, c) = image_window(&mut conn, &mut seen, 10, &a, 2);
    for (serial, buf) in [(3, &b), (4, &a), (5, &b)] {
        attach(&mut conn, image, buf, serial);
        presented(&mut conn, &mut seen, serial);
    }
    h.settle();
    let _ = conn.poll(&mut seen);
    assert_eq!(releases(&seen), [], "{seen:?}");
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "{seen:?}"
    );
    assert_image(&h, &c, GREEN, "the last attached buffer");

    drop(conn);
    h.quit();
}

#[test]
fn releases_cause_no_flip_and_no_wakeup_when_idle() {
    let mut h = Harness::start("idle");
    h.park();
    let mut conn = h.client("idler");
    conn.client_caps(caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    let a = Buf::new(1, RED);
    let b = Buf::new(2, GREEN);
    let hidden = Buf::new(3, BLUE);
    conn.tx()
        .create_buffer(a.create())
        .create_buffer(b.create())
        .create_buffer(hidden.create())
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let (root, image, _) = image_window(&mut conn, &mut seen, 10, &a, 2);
    attach(&mut conn, image, &b, 3);
    presented(&mut conn, &mut seen, 3);
    assert_eq!(releases(&seen), [a.id]);

    // Let the swap's follow-up repaint land first: a commit arriving
    // while it is pending can go unpresented (#646, predates M5-B).
    h.settle();
    // An image node that is never on screen: hidden.
    let offscreen = NodeId(30);
    conn.tx()
        .create_image(
            offscreen,
            root,
            Rect::new(0.0, 0.0, SIDE as f32, SIDE as f32),
        )
        .visible(offscreen, false)
        .image(offscreen, hidden.id, full())
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 4);
    seen.clear();
    conn.tx()
        .image(offscreen, BufferId::NONE, full())
        .commit(5)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 5);
    assert_eq!(
        releases(&seen),
        [hidden.id],
        "a detach that paints nothing still releases, with the commit's reply"
    );

    h.settle();
    let frames = h.stat("frames");
    seen.clear();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(conn.poll(&mut seen).unwrap(), 0, "nothing more: {seen:?}");
    h.settle();
    assert_eq!(h.stat("frames"), frames, "no flip on an idle desktop");

    drop(conn);
    h.quit();
}

/// #646: an empty commit landing while a swap's age-2 follow-up flip is in
/// flight still gets its `Presented`.
#[test]
fn an_empty_commit_right_after_a_swap_is_presented() {
    let mut h = Harness::start("empty646");
    h.park();
    let mut conn = h.client("empty");
    let mut seen = Vec::new();
    let a = Buf::new(1, RED);
    let b = Buf::new(2, GREEN);
    conn.tx()
        .create_buffer(a.create())
        .create_buffer(b.create())
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let (_, image, _) = image_window(&mut conn, &mut seen, 10, &a, 2);
    attach(&mut conn, image, &b, 3);
    presented(&mut conn, &mut seen, 3);
    conn.tx().commit(40).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 40);
    drop(conn);
    h.quit();
}
