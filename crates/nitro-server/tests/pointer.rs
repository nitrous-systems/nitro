//! `pointer.*`, end to end: the real [`run`] on a thread with the fake
//! backend and fake input — the harness shape of `tests/repeat.rs`.
//!
//! Scroll direction is applied by the server, so it is visible on the
//! wire. Speed and acceleration are libinput's job; here the fake source
//! records what it was handed, which is what the server controls.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::config::{AccelProfile, PointerSettings};
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, ServerMsg};
use nitro_wire::types::{AxisSource, Layer, NodeId};

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);

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
    config_dir: PathBuf,
    config_path: PathBuf,
    input: FakeInput,
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, conf: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-pointer-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(&config_path, conf).expect("write server.conf");
        let mut config = Config::fake(OUT.0, OUT.1, &path);
        config.config_path = Some(config_path.clone());
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            config_dir,
            config_path,
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

    fn client(&self, name: &str) -> Connection {
        Connection::connect(&self.wire_path, name).expect("wire connect")
    }

    fn request_line(&self, req: &str) -> String {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
    }

    fn rewrite_config(&self, conf: &str) {
        let tmp = self.config_dir.join("server.conf.tmp");
        std::fs::write(&tmp, conf).expect("write temp");
        std::fs::rename(&tmp, &self.config_path).expect("rename into place");
    }

    fn reload(&self, conf: &str) {
        self.rewrite_config(conf);
        assert_eq!(self.request_line("reload\n"), "ok");
    }

    fn tick(&mut self) -> u64 {
        self.time_ns += 5_000_000;
        self.time_ns
    }

    fn point_at(&mut self, c: &Configure, x: f32, y: f32) {
        let time_ns = self.tick();
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(c.position.x + x) / f64::from(OUT.0),
            y: f64::from(c.position.y + y) / f64::from(OUT.1),
            time_ns,
        });
    }

    fn scroll(&mut self, dx: f32, dy: f32, source: AxisSource) {
        let time_ns = self.tick();
        self.input.push(InputEvent::PointerAxis {
            dx,
            dy,
            source,
            time_ns,
        });
    }

    fn quit(mut self) {
        assert_eq!(self.request_line("quit\n"), "ok");
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

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
        conn.poll(seen).unwrap_or_else(|e| panic!("{what}: {e}"));
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A window, its `Configure` (for its position), and the pointer inside it.
fn window_under_pointer(
    h: &mut Harness,
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
) -> NodeId {
    let root = NodeId(1);
    let rect = NodeId(2);
    conn.tx()
        .create_window_with(root, "pointer", WIN, Layer::Normal, 0)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, Color::rgb(0xFF, 0, 0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let configure = expect(conn, seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    h.point_at(&configure, 50.0, 40.0);
    expect(conn, seen, "PointerEnter", |m| match m {
        ServerMsg::PointerEnter(e) if e.window == root => Some(()),
        _ => None,
    });
    root
}

/// Scroll once and return the `(dx, dy)` the client was sent.
fn scrolled(
    h: &mut Harness,
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    win: NodeId,
    source: AxisSource,
) -> (f32, f32) {
    seen.retain(|m| !matches!(m, ServerMsg::PointerAxis(_)));
    h.scroll(3.0, 15.0, source);
    expect(conn, seen, "PointerAxis", |m| match m {
        ServerMsg::PointerAxis(a) if a.window == win => Some((a.dx, a.dy)),
        _ => None,
    })
}

#[test]
fn natural_scroll_inverts_every_axis_source_and_a_reload_reverts_it() {
    let mut h = Harness::start("natural", "");
    let mut conn = h.client("natural");
    let mut seen = Vec::new();
    let win = window_under_pointer(&mut h, &mut conn, &mut seen);

    // The wire convention (docs/wire.md): positive `dy` is down, 15 a
    // wheel notch, libinput's values passed through. Clients never
    // invert; the server does, here.
    assert_eq!(
        scrolled(&mut h, &mut conn, &mut seen, win, AxisSource::Wheel),
        (3.0, 15.0),
        "traditional by default"
    );

    h.reload("pointer.natural_scroll = true\n");
    for source in [
        AxisSource::Wheel,
        AxisSource::Finger,
        AxisSource::Continuous,
    ] {
        assert_eq!(
            scrolled(&mut h, &mut conn, &mut seen, win, source),
            (-3.0, -15.0),
            "{source:?} inverted"
        );
    }

    // Deleting the line reverts it.
    h.reload("");
    assert_eq!(
        scrolled(&mut h, &mut conn, &mut seen, win, AxisSource::Wheel),
        (3.0, 15.0),
        "back to traditional"
    );
    h.quit();
}

#[test]
fn speed_and_acceleration_reach_the_input_source_at_startup_and_on_reload() {
    let h = Harness::start("speed", "pointer.speed = 0.5\npointer.accel = flat\n");
    wait_for("the startup configuration", || {
        h.input.pointer_configs() >= 1
    });
    assert_eq!(h.input.pointer_configs(), 1);
    assert_eq!(
        h.input.pointer_config(),
        Some(PointerSettings {
            speed: Some(0.5),
            accel: Some(AccelProfile::Flat),
            natural_scroll: None,
        })
    );

    // A reload that changes nothing about the pointer does not reconfigure.
    h.reload("pointer.speed = 0.5\npointer.accel = flat\nkeyboard.repeat = 500,20\n");
    assert_eq!(h.input.pointer_configs(), 1, "nothing moved");

    // One that does, does.
    h.reload("pointer.speed = -0.25\n");
    assert_eq!(h.input.pointer_configs(), 2);
    assert_eq!(
        h.input.pointer_config(),
        Some(PointerSettings {
            speed: Some(-0.25),
            accel: None,
            natural_scroll: None,
        }),
        "an absent accel is handed over as None: the device default"
    );
    h.quit();
}
