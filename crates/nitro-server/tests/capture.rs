//! Screen recording (#676 B) end to end on the fake backend with the fake
//! GPU helper on a thread: the cap gate, the minimal permission gate,
//! frames paced by flips and `max_fps`, drops with no free slot, damage,
//! and the ring's lifetime (client gone, helper dead).

#![allow(clippy::many_single_char_names)]

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Rect, Size};
use nitro_gpu::RingId;
use nitro_gpu::fake::{Call, FakeBackend as FakeGpu};
use nitro_server::gpu::Spawner;
use nitro_server::{BackendKind, Config, config::GpuHelper, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{CaptureBuffers, CaptureFrame, ServerMsg};
use nitro_wire::types::{CaptureStopReason, ErrorCode, Layer, NodeId, caps};

const OUT: (u32, u32) = (320, 240);
const ROOT: NodeId = NodeId(1);
const RECT: NodeId = NodeId(2);

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
    fake: FakeGpu,
}

impl Harness {
    fn start(name: &str, mode: GpuHelper, allow: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-cap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: OUT.0,
            height: OUT.1,
        };
        config.fake_modes = vec![(OUT.0, OUT.1, 60_000)];
        config.gpu = Some(mode);
        config.capture_allow = allow;
        let fake = FakeGpu::auto_signal();
        let f = fake.clone();
        let cfg = nitro_gpu::Config {
            idle_exit: (mode == GpuHelper::OnDemand).then_some(Duration::from_millis(100)),
        };
        config.gpu_spawner = Some(Spawner(Arc::new(move |fd: OwnedFd| {
            let f = f.clone();
            std::thread::spawn(move || {
                let sock = nitro_wire::Socket::from_fd(fd).unwrap();
                let _ = nitro_gpu::run(sock, f, cfg);
            });
        })));
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            thread: Some(thread),
            fake,
        };
        wait_for("the control socket", || {
            std::os::unix::net::UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        if mode == GpuHelper::On {
            wait_for("the helper", || h.stat("gpu_state") == 2);
        }
        h
    }

    fn request_text(&self, req: &str) -> Vec<String> {
        use std::io::{BufRead as _, Write as _};
        let s = std::os::unix::net::UnixStream::connect(&self.path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = std::io::BufReader::new(s);
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            assert!(c.read_line(&mut line).unwrap() > 0);
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

    fn quit(mut self) {
        self.fake.state().stall = false;
        {
            use std::io::{Read as _, Write as _};
            let mut s = std::os::unix::net::UnixStream::connect(&self.path).unwrap();
            s.write_all(b"quit\n").unwrap();
            let mut b = [0u8; 3];
            let _ = s.read(&mut b);
        }
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    conn: Connection,
    seen: Vec<ServerMsg>,
    output: u32,
    serial: u32,
}

impl Client {
    fn new(h: &Harness, caps_listed: u32) -> Self {
        let mut conn = Connection::connect(&h.wire_path, "rec").expect("wire connect");
        conn.client_caps(caps_listed).unwrap();
        conn.tx()
            .create_window(ROOT, "rec", Size::new(128.0, 96.0), Layer::Normal)
            .create_rect(RECT, ROOT, Rect::new(8.0, 8.0, 32.0, 32.0))
            .fill_solid(RECT, Color::rgba(10, 20, 30, 255))
            .commit(1)
            .unwrap();
        conn.flush().unwrap();
        let mut c = Self {
            conn,
            seen: Vec::new(),
            output: 0,
            serial: 1,
        };
        c.output = c.expect("Configure", |m| match m {
            ServerMsg::Configure(c) if c.window == ROOT => Some(c.output),
            _ => None,
        });
        c.expect("Presented", |m| {
            matches!(m, ServerMsg::Presented(p) if p.serial == 1).then_some(())
        });
        c
    }

    fn pump(&mut self) -> Result<(), nitro_wire::Error> {
        let _ = self.conn.flush();
        self.conn.poll(&mut self.seen).map(|_| ())
    }

    fn expect<T>(&mut self, what: &str, f: impl Fn(&ServerMsg) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(i) = self.seen.iter().position(|m| f(m).is_some()) {
                let m = self.seen.remove(i);
                return f(&m).unwrap();
            }
            assert!(Instant::now() < deadline, "no {what}; got {:?}", self.seen);
            if let Err(e) = self.pump() {
                if let Some(i) = self.seen.iter().position(|m| f(m).is_some()) {
                    let m = self.seen.remove(i);
                    return f(&m).unwrap();
                }
                panic!("{what}: {e}; got {:?}", self.seen);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn buffers(&mut self) -> CaptureBuffers {
        let i = self.take(
            |m| matches!(m, ServerMsg::CaptureBuffers(_)),
            "CaptureBuffers",
        );
        match self.seen.remove(i) {
            ServerMsg::CaptureBuffers(b) => b,
            _ => unreachable!(),
        }
    }

    fn frame(&mut self) -> CaptureFrame {
        let i = self.take(|m| matches!(m, ServerMsg::CaptureFrame(_)), "CaptureFrame");
        match self.seen.remove(i) {
            ServerMsg::CaptureFrame(f) => f,
            _ => unreachable!(),
        }
    }

    fn take(&mut self, f: impl Fn(&ServerMsg) -> bool, what: &str) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(i) = self.seen.iter().position(&f) {
                return i;
            }
            assert!(Instant::now() < deadline, "no {what}; got {:?}", self.seen);
            self.pump().unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn stopped(&mut self, id: u32) -> CaptureStopReason {
        self.expect("CaptureStopped", |m| match m {
            ServerMsg::CaptureStopped(s) if s.capture_id == id => Some(s.reason),
            _ => None,
        })
    }

    /// Change the rect's colour and wait for the frame to be on screen.
    fn repaint(&mut self, k: u8) {
        self.serial += 1;
        self.conn
            .tx()
            .fill_solid(RECT, Color::rgba(k, 20, 30, 255))
            .commit(self.serial)
            .unwrap();
        let s = self.serial;
        self.expect("Presented", |m| {
            matches!(m, ServerMsg::Presented(p) if p.serial == s).then_some(())
        });
    }

    fn frames_now(&mut self) -> Vec<CaptureFrame> {
        let _ = self.pump();
        let (f, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.seen)
            .into_iter()
            .partition(|m| matches!(m, ServerMsg::CaptureFrame(_)));
        self.seen = rest;
        f.into_iter()
            .map(|m| match m {
                ServerMsg::CaptureFrame(f) => f,
                _ => unreachable!(),
            })
            .collect()
    }
}

fn capture_calls(h: &Harness) -> usize {
    h.fake
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::CaptureComposite(..)))
        .count()
}

#[test]
fn the_ops_need_the_cap_listed() {
    let h = Harness::start("gate", GpuHelper::On, true);
    let mut c = Client::new(&h, caps::RELEASE);
    let out = c.output;
    c.conn.capture_start(1, out, 0).unwrap();
    let e = c.expect("Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(e, ErrorCode::Protocol);
    h.quit();
}

#[test]
fn without_a_grant_it_is_denied_and_the_connection_lives() {
    let h = Harness::start("denied", GpuHelper::On, false);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(1, out, 0).unwrap();
    assert_eq!(c.stopped(1), CaptureStopReason::Denied);
    // Still alive: a repaint is answered.
    c.repaint(1);
    assert_eq!(h.stat("capture_rings_bytes"), 0);
    // Unknown output, and no helper: stopped too.
    h.quit();
}

#[test]
fn a_bad_output_and_no_helper_are_refused() {
    let h = Harness::start("unsup", GpuHelper::Off, true);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(1, out + 77, 0).unwrap();
    assert_eq!(c.stopped(1), CaptureStopReason::OutputGone);
    c.conn.capture_start(2, out, 0).unwrap();
    assert_eq!(c.stopped(2), CaptureStopReason::Unsupported);
    h.quit();
}

#[test]
fn frames_follow_flips_with_damage_and_the_ring_goes_at_stop() {
    let h = Harness::start("frames", GpuHelper::On, true);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(7, out, 0).unwrap();
    let b = c.buffers();
    assert_eq!((b.width, b.height, b.slots.len()), (OUT.0, OUT.1, 3));
    assert_eq!(b.modifier, 0, "LINEAR first");
    assert!(h.stat("capture_rings_bytes") >= u64::from(OUT.0 * OUT.1 * 4 * 3));
    // The first frame damages everything.
    let f = c.frame();
    assert_eq!(f.damage, vec![IRect::new(0, 0, 320, 240)]);
    c.conn.capture_release(7, f.slot).unwrap();
    // No change, no frame.
    std::thread::sleep(Duration::from_millis(100));
    assert!(c.frames_now().is_empty());
    // A change: one frame whose damage covers the rect.
    c.repaint(2);
    let f = c.frame();
    let win = h.stat("capture_frames");
    assert!(win >= 2);
    assert!(
        f.damage.iter().any(|r| r.w < 320 && r.h < 240),
        "partial damage: {:?}",
        f.damage
    );
    c.conn.capture_release(7, f.slot).unwrap();
    c.conn.capture_stop(7).unwrap();
    assert_eq!(c.stopped(7), CaptureStopReason::Client);
    assert_eq!(h.stat("capture_rings_bytes"), 0);
    assert_eq!(h.stat("capture_active"), 0);
    wait_for("the ring to be freed", || {
        h.fake
            .calls()
            .iter()
            .any(|c| matches!(c, Call::FreeRing(RingId::Capture(_))))
    });
    h.quit();
}

#[test]
fn a_client_holding_every_slot_drops_frames_and_damage_accumulates() {
    let h = Harness::start("drops", GpuHelper::On, true);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(1, out, 0).unwrap();
    let _b = c.buffers();
    let mut held = vec![c.frame()];
    for k in 3..6 {
        c.repaint(k);
    }
    std::thread::sleep(Duration::from_millis(100));
    held.extend(c.frames_now());
    assert_eq!(held.len(), 3, "one frame per slot");
    c.repaint(9);
    std::thread::sleep(Duration::from_millis(50));
    assert!(c.frames_now().is_empty());
    assert!(h.stat("capture_drops") >= 1);
    // A release: the kept damage comes out in the next frame.
    let calls = capture_calls(&h);
    c.conn.capture_release(1, held[0].slot).unwrap();
    let f = c.frame();
    assert_eq!(f.slot, held[0].slot);
    assert!(!f.damage.is_empty());
    assert_eq!(capture_calls(&h), calls + 1);
    h.quit();
}

#[test]
fn max_fps_caps_the_rate() {
    let h = Harness::start("fps", GpuHelper::On, true);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(1, out, 5).unwrap();
    let _b = c.buffers();
    let f = c.frame();
    c.conn.capture_release(1, f.slot).unwrap();
    let t0 = Instant::now();
    let mut n = 0;
    let mut k = 0u8;
    while t0.elapsed() < Duration::from_millis(600) {
        k = k.wrapping_add(1);
        c.repaint(k);
        for f in c.frames_now() {
            n += 1;
            c.conn.capture_release(1, f.slot).unwrap();
        }
    }
    // 600 ms at ≤5 fps: at most 3 frames after the first (plus slack).
    assert!(n <= 4, "{n} frames in 600 ms at 5 fps");
    // The last change is not lost: the rate timer brings it.
    std::thread::sleep(Duration::from_millis(300));
    let late = c.frames_now();
    assert!(n + late.len() >= 2, "{n} + {}", late.len());
    h.quit();
}

#[test]
fn a_client_that_goes_away_frees_its_ring() {
    let h = Harness::start("gone", GpuHelper::On, true);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(1, out, 0).unwrap();
    let _b = c.buffers();
    let _f = c.frame();
    assert!(h.stat("capture_rings_bytes") > 0);
    drop(c);
    wait_for("the ring to go", || h.stat("capture_active") == 0);
    assert_eq!(h.stat("capture_rings_bytes"), 0);
    wait_for("FreeCaptureRing", || {
        h.fake
            .calls()
            .iter()
            .any(|c| matches!(c, Call::FreeRing(RingId::Capture(_))))
    });
    h.quit();
}

#[test]
fn a_dead_helper_stops_the_capture_as_helper_lost() {
    let h = Harness::start("lost", GpuHelper::On, true);
    let mut c = Client::new(&h, caps::CAPTURE);
    let out = c.output;
    c.conn.capture_start(1, out, 0).unwrap();
    let _b = c.buffers();
    let f = c.frame();
    c.conn.capture_release(1, f.slot).unwrap();
    // The helper hangs mid-frame; the server kills it after its timeout.
    h.fake.state().stall = true;
    c.serial += 1;
    c.conn
        .tx()
        .fill_solid(RECT, Color::rgba(99, 20, 30, 255))
        .commit(c.serial)
        .unwrap();
    assert_eq!(c.stopped(1), CaptureStopReason::HelperLost);
    h.fake.state().stall = false;
    assert_eq!(h.stat("capture_rings_bytes"), 0);
    h.quit();
}
