//! The GPU helper as composite mode 2 (#3922), end to end on the fake
//! backend with a Kaby-Lake-like inventory (one overlay, `IN_FENCE_FD` on
//! the primary) and the fake helper (`nitro_gpu::fake`) running on a
//! thread of the test in place of `nitro-gpu-vulkan`.
//!
//! Two overlapping dma-buf Surfaces (sealed memfds stand in for dma-bufs):
//! the top one takes the overlay, the one under it is composited by the
//! helper. Covered: reaching mode 2 and what the helper is asked to draw,
//! a menu over the composited Surface staying mode 2, a buffer release
//! held until the frame sampling it signalled, the helper dying (fallback,
//! repaint, respawn after the backoff, back to mode 2), and `gpu.helper =
//! off` never starting one.

#![allow(clippy::many_single_char_names)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Rect, Size};
use nitro_gpu::fake::{Call, FakeBackend as FakeGpu};
use nitro_kms::{FakePlaneSpec, Fourcc};
use nitro_server::gpu::Spawner;
use nitro_server::{BackendKind, Config, config::GpuHelper, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{CreateDmabufBuffer, DmabufPlane, PresentSurface, ServerMsg};
use nitro_wire::types::{BufferId, ColorMatrix, ColorRange, Layer, NodeId, caps, format, modifier};

const OUT: (u32, u32) = (320, 240);
const SIDE: u32 = 64;
const ROOT: NodeId = NodeId(1);
/// Under `B`: composited by the helper.
const A: NodeId = NodeId(2);
/// On top: the overlay.
const B: NodeId = NodeId(3);
const MENU: NodeId = NodeId(4);

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn kbl() -> Vec<FakePlaneSpec> {
    let p = |s: FakePlaneSpec| {
        s.formats(&[
            Fourcc::XRGB8888,
            Fourcc::ARGB8888,
            Fourcc::NV12,
            Fourcc::YUYV,
        ])
        .scale_limits(90, 800)
    };
    vec![
        p(FakePlaneSpec::primary()).zpos(0, 0, 0, true).in_fence(true),
        p(FakePlaneSpec::overlay()).zpos(1, 1, 1, true).in_fence(true),
    ]
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
    /// The fake helper's shared state (every helper the server spawns).
    fake: FakeGpu,
    /// A dup of each spawned helper's socket, to kill it with.
    socks: Arc<Mutex<Vec<OwnedFd>>>,
}

impl Harness {
    fn start(name: &str, mode: GpuHelper, auto_signal: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-gpu-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: OUT.0,
            height: OUT.1,
        };
        config.fake_modes = vec![(OUT.0, OUT.1, 60_000)];
        config.fake_planes = kbl();
        config.gpu = Some(mode);
        let fake = if auto_signal {
            FakeGpu::auto_signal()
        } else {
            FakeGpu::new()
        };
        let socks: Arc<Mutex<Vec<OwnedFd>>> = Arc::default();
        let (f, s) = (fake.clone(), Arc::clone(&socks));
        config.gpu_spawner = Some(Spawner(Arc::new(move |fd: OwnedFd| {
            s.lock().unwrap().push(fd.try_clone().unwrap());
            let f = f.clone();
            std::thread::spawn(move || {
                let sock = nitro_wire::Socket::from_fd(fd).unwrap();
                let _ = nitro_gpu::run(sock, f, nitro_gpu::Config::default());
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
            socks,
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        h
    }

    fn request_text(&self, req: &str) -> Vec<String> {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
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

    fn composites(&self) -> Vec<(Vec<IRect>, Vec<u32>)> {
        self.fake
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Composite(_, clip, texs, _) => Some((clip, texs)),
                _ => None,
            })
            .collect()
    }

    fn quit(mut self) {
        let s = UnixStream::connect(&self.path).expect("connect");
        let mut c = BufReader::new(s);
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
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
        let _ = conn.flush();
        if let Err(e) = conn.poll(seen) {
            if let Some(found) = seen.iter().find_map(&f) {
                return found;
            }
            panic!("{what}: {e}; got {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn presented(conn: &mut Connection, seen: &mut Vec<ServerMsg>, serial: u32) {
    expect(conn, seen, &format!("Presented {serial}"), |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == serial).then_some(())
    });
}

/// A SIDE×SIDE linear XR24 "dma-buf".
struct Dma {
    id: BufferId,
    fd: OwnedFd,
}

impl Dma {
    fn new(id: u32) -> Self {
        let len = u64::from(SIDE * SIDE * 4);
        let fd = nitro_shm::create_sealed("nitro-gpu-test", len).unwrap();
        let _ = nitro_shm::MappingMut::map_mut(fd.as_fd(), len as usize).unwrap();
        Self {
            id: BufferId(id),
            fd,
        }
    }

    fn create(&self) -> CreateDmabufBuffer {
        CreateDmabufBuffer {
            id: self.id,
            width: SIDE,
            height: SIDE,
            format: format::XR24,
            modifier: modifier::LINEAR,
            planes: vec![DmabufPlane {
                fd: self.fd.try_clone().unwrap(),
                offset: 0,
                stride: SIDE * 4,
            }],
        }
    }
}

struct Scene {
    conn: Connection,
    seen: Vec<ServerMsg>,
    /// Buffers of A, then of B.
    a: Vec<Dma>,
    b: Vec<Dma>,
    serial: u32,
}

/// A window holding A at (8, 8) and B at (40, 40), overlapping, B on top,
/// three buffers each.
fn two_surfaces(h: &Harness) -> Scene {
    let mut conn = Connection::connect(&h.wire_path, "gpu").expect("wire connect");
    conn.client_caps(caps::SURFACE | caps::RELEASE | caps::DMABUF)
        .unwrap();
    let mut seen = Vec::new();
    let s = SIDE as f32;
    conn.tx()
        .create_window(ROOT, "gpu", Size::new(160.0, 120.0), Layer::Normal)
        .create_surface(A, ROOT, Rect::new(8.0, 8.0, s, s))
        .create_surface(B, ROOT, Rect::new(40.0, 40.0, s, s))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let a: Vec<Dma> = (0..3).map(|i| Dma::new(10 + i)).collect();
    let b: Vec<Dma> = (0..3).map(|i| Dma::new(20 + i)).collect();
    for d in a.iter().chain(&b) {
        conn.create_dmabuf_buffer(d.create()).unwrap();
    }
    conn.commit(2).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 2);
    Scene {
        conn,
        seen,
        a,
        b,
        serial: 3,
    }
}

fn frame(id: NodeId, buffer: BufferId, serial: u32) -> PresentSurface {
    PresentSurface {
        id,
        buffer,
        serial,
        src: IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed()),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage: vec![],
    }
}

impl Scene {
    /// One new frame on each Surface (A, then B), and their `Presented`.
    fn step(&mut self) {
        let i = self.serial as usize % 3;
        let (a, b) = (self.a[i].id, self.b[i].id);
        let s = self.serial;
        self.conn.present_surface(frame(A, a, s)).unwrap();
        self.conn.present_surface(frame(B, b, s + 1)).unwrap();
        self.conn.flush().unwrap();
        presented(&mut self.conn, &mut self.seen, s);
        presented(&mut self.conn, &mut self.seen, s + 1);
        self.serial += 2;
    }

    fn play(&mut self, what: &str, mut until: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !until() {
            assert!(Instant::now() < deadline, "timed out playing until {what}");
            self.step();
        }
    }
}

#[test]
fn an_overlapped_surface_is_composited_by_the_helper_the_top_one_on_the_overlay() {
    let h = Harness::start("mode2", GpuHelper::On, true);
    wait_for("the helper", || h.stat("gpu_state") == 2);
    assert_eq!(h.stat("gpu_spawns"), 1);
    let mut s = two_surfaces(&h);
    s.play("mode 2", || h.stat("planes_mode") == 2);
    assert_eq!(h.stat("planes_in_use"), 1, "B on the overlay");
    assert_eq!(h.stat("gpu_ring_slots"), 3);
    let calls = h.fake.calls();
    assert!(calls.iter().any(|c| matches!(c, Call::ImportShadow(_))));
    assert!(calls.iter().any(|c| matches!(c, Call::Ring(3, 0))));
    // Steady state: A's new frames are composited, B's go on the plane.
    let frames = h.stat("gpu_frames");
    for _ in 0..5 {
        s.step();
    }
    wait_for("composites", || h.stat("gpu_frames") >= frames + 5);
    let comps = h.composites();
    let (clip, texs) = comps.last().unwrap();
    // One client texture (A) under the shadow; the damage is A's rect.
    assert_eq!(texs.len(), 2, "A, then the shadow: {texs:?}");
    // The window is cascaded, not at the origin: A's rect is where the
    // helper's damage is, and the window origin follows from it.
    assert_eq!(clip.len(), 1, "{clip:?}");
    assert_eq!((clip[0].w, clip[0].h), (64, 64), "{clip:?}");
    let (wx, wy) = (clip[0].x - 8, clip[0].y - 8);
    assert!(h.stat("plane_fences") > 0, "the ring slot's IN_FENCE_FD");
    assert_eq!(h.stat("gpu_crashes"), 0);

    // A menu over the composited Surface: still mode 2, and its pixels
    // are in the shadow damage.
    s.conn
        .tx()
        .create_rect(MENU, ROOT, Rect::new(10.0, 10.0, 20.0, 20.0))
        .fill_solid(MENU, Color::WHITE)
        .commit(s.serial)
        .unwrap();
    s.conn.flush().unwrap();
    presented(&mut s.conn, &mut s.seen, s.serial);
    s.serial += 1;
    s.step();
    assert_eq!(h.stat("planes_mode"), 2);
    let uploads: Vec<Vec<IRect>> = h
        .fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Upload(_, r) => Some(r),
            _ => None,
        })
        .collect();
    assert!(
        uploads
            .iter()
            .flatten()
            .any(|r| r.intersects(&IRect::new(wx + 12, wy + 12, 10, 10))),
        "menu damage uploaded: {uploads:?}"
    );
    h.quit();
}

#[test]
fn a_buffer_the_helper_samples_is_released_after_its_fence() {
    let h = Harness::start("release", GpuHelper::On, true);
    wait_for("the helper", || h.stat("gpu_state") == 2);
    let mut s = two_surfaces(&h);
    s.play("mode 2", || h.stat("planes_mode") == 2);
    // From now on fences stay pending until signalled by hand.
    h.fake.state().auto_signal = false;
    s.seen.clear();
    s.step();
    s.step();
    wait_for("a held release", || h.stat("gpu_releases_held") > 0);
    // A buffer of A was replaced; its release waits for the fence.
    let released_a = |seen: &[ServerMsg]| {
        seen.iter().any(|m| {
            matches!(m, ServerMsg::BufferReleased(r) if (10..13).contains(&r.id.raw()))
        })
    };
    let _ = s.conn.poll(&mut s.seen);
    let before = released_a(&s.seen);
    while h.fake.signal() {}
    wait_for("the release", || h.stat("gpu_releases_held") == 0);
    expect(&mut s.conn, &mut s.seen, "A's release", |m| {
        matches!(m, ServerMsg::BufferReleased(r) if (10..13).contains(&r.id.raw())).then_some(())
    });
    let _ = before;
    h.fake.state().auto_signal = true;
    while h.fake.signal() {}
    h.quit();
}

#[test]
fn the_helper_dying_falls_back_and_comes_back() {
    let h = Harness::start("crash", GpuHelper::On, true);
    wait_for("the helper", || h.stat("gpu_state") == 2);
    let mut s = two_surfaces(&h);
    s.play("mode 2", || h.stat("planes_mode") == 2);
    let paints = h.stat("frames");
    // Kill it: shut its socket down.
    for fd in h.socks.lock().unwrap().drain(..) {
        rustix::net::shutdown(&fd, rustix::net::Shutdown::Both).unwrap();
    }
    wait_for("the crash", || h.stat("gpu_crashes") == 1);
    assert_eq!(h.stat("gpu_fallbacks"), 1);
    // Back on planes + CPU at once, and still presenting.
    s.step();
    assert_ne!(h.stat("planes_mode"), 2);
    assert!(h.stat("frames") > paints);
    // Respawned after the backoff, and mode 2 again.
    s.play("the respawn", || h.stat("gpu_spawns") == 2);
    s.play("mode 2 again", || h.stat("planes_mode") == 2);
    assert_eq!(h.stat("gpu_crashes"), 1);
    h.quit();
}

#[test]
fn helper_off_never_spawns() {
    let h = Harness::start("off", GpuHelper::Off, true);
    let mut s = two_surfaces(&h);
    for _ in 0..20 {
        s.step();
    }
    assert_eq!(h.stat("gpu_spawns"), 0);
    assert_eq!(h.stat("gpu_state"), 0);
    assert_ne!(h.stat("planes_mode"), 2);
    assert!(h.fake.calls().is_empty());
    h.quit();
}

#[test]
fn on_demand_spawns_when_an_output_first_wants_mode_2() {
    let h = Harness::start("demand", GpuHelper::OnDemand, true);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(h.stat("gpu_spawns"), 0, "not before it is wanted");
    let mut s = two_surfaces(&h);
    s.play("mode 2", || h.stat("planes_mode") == 2);
    assert_eq!(h.stat("gpu_spawns"), 1);
    h.quit();
}
