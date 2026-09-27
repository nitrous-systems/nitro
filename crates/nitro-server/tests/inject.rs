//! Control-socket input injection (`input`), end to end: the real [`run`]
//! on a thread with the fake backend, a real `nitro-wire` client, and every
//! event driven through the control socket rather than `FakeInput` — the
//! point being that the events take the path hardware input takes.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::input::FakeInput;
use nitro_server::keyboard::Keyboard;
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, ServerMsg};
use nitro_wire::types::{AxisSource, ButtonState, Layer, NodeId};

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
    // Present so the server takes the fake input path, never pushed to.
    _input: FakeInput,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-inject-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(OUT.0, OUT.1, &path);
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            _input: input,
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

    fn line(&self, req: &str) -> String {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
    }

    fn input(&self, args: &str) -> String {
        self.line(&format!("input {args}\n"))
    }

    /// Status line plus body up to the blank line.
    fn body(&self, req: &str) -> (String, Vec<String>) {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut status = String::new();
        c.read_line(&mut status).unwrap();
        let mut lines = Vec::new();
        loop {
            let mut l = String::new();
            assert!(c.read_line(&mut l).unwrap() > 0, "closed mid-reply");
            let l = l.trim_end_matches('\n').to_owned();
            if l.is_empty() {
                break;
            }
            lines.push(l);
        }
        (status.trim_end().to_owned(), lines)
    }

    fn stat(&self, key: &str) -> u64 {
        let (_, lines) = self.body("stats\n");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
            .parse()
            .unwrap()
    }

    fn quit(mut self) {
        assert_eq!(self.line("quit\n"), "ok");
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

fn collect(conn: &mut Connection, seen: &mut Vec<ServerMsg>, ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        conn.flush().unwrap();
        let _ = conn.poll(seen);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn make_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> (NodeId, Configure) {
    let root = NodeId(1);
    let rect = NodeId(2);
    conn.tx()
        .create_window_with(root, "inject", WIN, Layer::Normal, 0)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, Color::rgb(0xFF, 0, 0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    (root, c)
}

fn keys(seen: &[ServerMsg], window: NodeId) -> Vec<nitro_wire::msg::Key> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Key(k) if k.window == window => Some(k.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn injected_pointer_and_keys_reach_the_client_through_the_real_path() {
    let h = Harness::start("path");
    let mut conn = Connection::connect(&h.wire_path, "inject").expect("wire connect");
    let mut seen = Vec::new();
    let (win, c) = make_window(&mut conn, &mut seen);

    // Motion in device pixels (scale 1 here): local == global - origin.
    let (gx, gy) = (c.position.x + 40.0, c.position.y + 25.0);
    assert_eq!(h.input(&format!("motion {gx} {gy}")), "ok 1");
    let pos = expect(&mut conn, &mut seen, "PointerEnter", |m| match m {
        ServerMsg::PointerEnter(e) if e.window == win => Some(e.pos),
        _ => None,
    });
    assert!(
        (pos.x - 40.0).abs() < 0.5 && (pos.y - 25.0).abs() < 0.5,
        "{pos:?}"
    );
    let (gx, gy) = (c.position.x + 60.0, c.position.y + 30.0);
    // Named output: relative to its device rect, which is at (0, 0) here.
    assert_eq!(h.input(&format!("motion {gx} {gy} Virtual-1")), "ok 1");
    let pos = expect(&mut conn, &mut seen, "PointerMotion", |m| match m {
        ServerMsg::PointerMotion(e) if e.window == win => Some(e.pos),
        _ => None,
    });
    assert!(
        (pos.x - 60.0).abs() < 0.5 && (pos.y - 30.0).abs() < 0.5,
        "{pos:?}"
    );

    // A click: press then release, and the window is focused.
    seen.clear();
    assert_eq!(h.input("button left click"), "ok 2");
    let buttons: Vec<ButtonState> = {
        expect(&mut conn, &mut seen, "release", |m| match m {
            ServerMsg::PointerButton(b) if b.state == ButtonState::Released => Some(()),
            _ => None,
        });
        seen.iter()
            .filter_map(|m| match m {
                ServerMsg::PointerButton(b) if b.window == win && b.button == 0x110 => {
                    Some(b.state)
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(buttons, [ButtonState::Pressed, ButtonState::Released]);
    assert_eq!(h.stat("focused"), 1);

    // Wheel, discrete and smooth.
    seen.clear();
    assert_eq!(h.input("wheel 0 15"), "ok 1");
    assert_eq!(h.input("wheel 0 -2.5 finger"), "ok 1");
    let axes = {
        expect(&mut conn, &mut seen, "finger axis", |m| match m {
            ServerMsg::PointerAxis(a) if a.source == AxisSource::Finger => Some(()),
            _ => None,
        });
        seen.iter()
            .filter_map(|m| match m {
                ServerMsg::PointerAxis(a) if a.window == win => Some((a.dy, a.source)),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        axes,
        [(15.0, AxisSource::Wheel), (-2.5, AxisSource::Finger)]
    );

    // Errors.
    assert!(
        h.input("motion 1 2 NOPE").starts_with("err "),
        "unknown output"
    );
    assert!(h.input("bogus").starts_with("err "));
    assert!(h.input("wheel 0 15 every=16").starts_with("err "));

    drop(conn);
    h.quit();
}

#[test]
fn injected_keys_and_text_reach_the_focused_window() {
    let h = Harness::start("keys");
    let mut conn = Connection::connect(&h.wire_path, "inject").expect("wire connect");
    let mut seen = Vec::new();
    let (win, _) = make_window(&mut conn, &mut seen);
    expect(&mut conn, &mut seen, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == win && f.focused => Some(()),
        _ => None,
    });
    let has_keymap = Keyboard::new().is_some();

    seen.clear();
    assert_eq!(h.input("key 30 tap"), "ok 2");
    expect(&mut conn, &mut seen, "key release", |m| match m {
        ServerMsg::Key(k) if k.state == ButtonState::Released => Some(()),
        _ => None,
    });
    let k = keys(&seen, win);
    assert_eq!(k.len(), 2, "{k:?}");
    assert_eq!((k[0].keycode, k[0].state), (30, ButtonState::Pressed));
    assert_eq!((k[1].keycode, k[1].state), (30, ButtonState::Released));
    if has_keymap {
        assert_eq!(k[0].utf8, "a");
    }

    seen.clear();
    assert_eq!(h.input("type Ab"), "ok 6");
    expect(&mut conn, &mut seen, "b release", |m| match m {
        ServerMsg::Key(k) if k.keycode == 48 && k.state == ButtonState::Released => Some(()),
        _ => None,
    });
    let k = keys(&seen, win);
    let codes: Vec<(u32, bool)> = k
        .iter()
        .map(|k| (k.keycode, k.state == ButtonState::Pressed))
        .collect();
    assert_eq!(
        codes,
        [
            (42, true),
            (30, true),
            (30, false),
            (42, false),
            (48, true),
            (48, false)
        ]
    );
    if has_keymap {
        let typed: String = k
            .iter()
            .filter(|k| k.state == ButtonState::Pressed)
            .map(|k| k.utf8.as_str())
            .collect();
        assert_eq!(typed, "Ab");
    }

    drop(conn);
    h.quit();
}

#[test]
fn a_scripted_sequence_is_paced_server_side_and_feeds_i2p() {
    let h = Harness::start("seq");
    let mut conn = Connection::connect(&h.wire_path, "inject").expect("wire connect");
    let mut seen = Vec::new();
    let (win, c) = make_window(&mut conn, &mut seen);
    h.input(&format!(
        "motion {} {}",
        c.position.x + 10.0,
        c.position.y + 10.0
    ));
    let before = h.stat("input_injected");
    let i2p_before: u64 = h.body("samples i2p\n").0["ok ".len()..].parse().unwrap();

    seen.clear();
    let t0 = Instant::now();
    assert_eq!(h.input("wheel 0 15 count=5 every=20"), "ok 5");
    // The client answers every scroll with a commit, which is what closes
    // the input-to-photon loop.
    let mut serial = 2;
    let mut axes = Vec::new();
    wait_for("five axis events", || {
        conn.flush().unwrap();
        let mut batch = Vec::new();
        let _ = conn.poll(&mut batch);
        for m in batch {
            if let ServerMsg::PointerAxis(a) = &m
                && a.window == win
            {
                axes.push(a.time_ns);
                conn.tx()
                    .fill_solid(NodeId(2), Color::rgb(0, (serial * 40) as u8, 0))
                    .commit(serial)
                    .unwrap();
                serial += 1;
            }
            seen.push(m);
        }
        axes.len() >= 5
    });
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(75),
        "arrived in {elapsed:?}"
    );
    for pair in axes.windows(2) {
        let dt = pair[1] - pair[0];
        assert!(
            (18_000_000..=22_000_000).contains(&dt),
            "stamps {dt} ns apart: {axes:?}"
        );
    }
    assert_eq!(h.stat("input_injected") - before, 5);
    wait_for("the queue to drain", || h.stat("input_inject_pending") == 0);

    collect(&mut conn, &mut seen, 200);
    wait_for("an i2p sample", || h.stat("i2p_max_us") > 0);
    let (status, values) = h.body("samples i2p\n");
    let total: u64 = status["ok ".len()..].parse().unwrap();
    assert!(total > i2p_before, "{status}");
    assert!(!values.is_empty());
    let (status, _) = h.body("samples flip\n");
    assert!(status.starts_with("ok "), "{status}");

    drop(conn);
    h.quit();
}
