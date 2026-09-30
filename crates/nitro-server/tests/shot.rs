//! `shot` shows what is really on screen (#3962), end to end on the fake
//! backend: Surfaces on a plane (holes in the shadow) come out of their
//! buffer when the CPU can read it, out of a (fake) GPU helper `Capture`
//! when it cannot, and as the placeholder with a reason when neither can.
//!
//! Sealed memfds stand in for dma-bufs, as in `tests/dmabuf.rs`; the fake
//! helper (`nitro_gpu::fake`) runs on a thread and "captures" by filling
//! each layer with `CAPTURE_COLOR`.

#![allow(clippy::many_single_char_names)]

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{IRect, Rect, Size};
use nitro_gpu::fake::{CAPTURE_COLOR, Call, FakeBackend as FakeGpu};
use nitro_kms::{FakePlaneSpec, Fourcc};
use nitro_raster::{Canvas, Nv12, YuvEncoding, YuvMatrix, YuvRange};
use nitro_server::gpu::Spawner;
use nitro_server::{BackendKind, Config, config::GpuHelper, run};
use nitro_shm::MappingMut;
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, CreateDmabufBuffer, DmabufPlane, PresentSurface, ServerMsg};
use nitro_wire::types::{BufferId, ColorMatrix, ColorRange, Layer, NodeId, caps, format, modifier};

const OUT: (u32, u32) = (320, 240);
const SIDE: u32 = 32;
const NV12_LEN: u32 = SIDE * SIDE * 3 / 2;
const ROOT: NodeId = NodeId(1);
const SURF: NodeId = NodeId(2);
const GREY: [u8; 3] = [0x80, 0x80, 0x80];

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// KBL-shaped: a primary taking XR24/AR24 linear, an overlay taking NV12
/// linear and Y-tiled, both with `IN_FENCE_FD`.
fn kbl() -> Vec<FakePlaneSpec> {
    use nitro_kms::{ColorEncoding, ColorRange as KmsRange};
    let color = |s: FakePlaneSpec| {
        s.color(
            &[ColorEncoding::Bt601, ColorEncoding::Bt709],
            &[KmsRange::Limited, KmsRange::Full],
        )
        .in_fence(true)
    };
    vec![
        color(FakePlaneSpec::default_primary()).zpos(0, 0, 0, true),
        color(
            FakePlaneSpec::overlay()
                .format_mods(Fourcc::NV12, &[modifier::LINEAR, modifier::I915_Y_TILED]),
        )
        .zpos(1, 1, 1, true),
    ]
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
    fake: FakeGpu,
}

/// A parsed `shot meta=1` reply.
struct Shot {
    meta: HashMap<String, String>,
    stride: u32,
    data: Vec<u8>,
}

impl Shot {
    fn m(&self, k: &str) -> &str {
        self.meta.get(k).map_or("?", String::as_str)
    }

    fn px(&self, x: u32, y: u32) -> [u8; 3] {
        let o = (y * self.stride + x * 4) as usize;
        [self.data[o], self.data[o + 1], self.data[o + 2]]
    }

    /// The SIDE×SIDE rect at the Surface, `[b, g, r]`.
    fn surface(&self, c: &Configure) -> Vec<[u8; 3]> {
        let (x0, y0) = (c.position.x as u32 + 8, c.position.y as u32 + 8);
        let mut out = Vec::new();
        for y in y0..y0 + SIDE {
            for x in x0..x0 + SIDE {
                out.push(self.px(x, y));
            }
        }
        out
    }
}

fn read_shot(c: &mut BufReader<UnixStream>) -> Shot {
    let mut header = String::new();
    c.read_line(&mut header).unwrap();
    let rest = header
        .trim_end()
        .strip_prefix("ok ")
        .unwrap_or_else(|| panic!("shot: {header:?}"));
    let mut words = rest.split(' ');
    let n: Vec<u32> = words.by_ref().take(3).map(|f| f.parse().unwrap()).collect();
    let meta = words
        .filter_map(|w| w.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let mut data = vec![0u8; (n[2] * n[1]) as usize];
    c.read_exact(&mut data).unwrap();
    Shot {
        meta,
        stride: n[2],
        data,
    }
}

impl Harness {
    fn start(name: &str, mode: GpuHelper) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-shot-{}-{name}", std::process::id()));
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

    fn request_text(&self, req: &str) -> Vec<String> {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        read_text(&mut c)
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

    fn shot(&self, req: &str) -> Shot {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        read_shot(&mut c)
    }

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

    fn quit(mut self) {
        self.fake.state().stall = false;
        let mut c = self.connect();
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn read_text(c: &mut BufReader<UnixStream>) -> Vec<String> {
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

/// A client with a 128×96 window holding Surface `SURF` (SIDE×SIDE at
/// (8, 8)).
fn client(h: &Harness) -> (Connection, Vec<ServerMsg>, Configure) {
    let mut conn = Connection::connect(&h.wire_path, "shot").expect("wire connect");
    conn.client_caps(caps::DMABUF | caps::SURFACE | caps::RELEASE)
        .unwrap();
    let mut seen = Vec::new();
    conn.tx()
        .create_window(ROOT, "shot", Size::new(128.0, 96.0), Layer::Normal)
        .create_surface(SURF, ROOT, Rect::new(8.0, 8.0, SIDE as f32, SIDE as f32))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(&mut conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == ROOT => Some(*c),
        _ => None,
    });
    presented(&mut conn, &mut seen, 1);
    (conn, seen, c)
}

fn full() -> IRect {
    IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed())
}

fn present(conn: &mut Connection, buffer: BufferId, serial: u32) {
    conn.present_surface(PresentSurface {
        id: SURF,
        buffer,
        serial,
        src: full(),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage: vec![],
    })
    .unwrap();
    conn.flush().unwrap();
}

/// A linear NV12 "dma-buf" (CPU-readable) with a red-ish pattern; its
/// rasterized pixels as `[b, g, r]`.
fn linear_nv12(conn: &mut Connection, id: u32) -> Vec<[u8; 3]> {
    let fd = nitro_shm::create_sealed("nitro-shot-test", u64::from(NV12_LEN)).unwrap();
    let mut map = MappingMut::map_mut(fd.as_fd(), NV12_LEN as usize).unwrap();
    for (i, b) in map.as_bytes_mut().iter_mut().enumerate() {
        let i = i as u32;
        *b = if i < SIDE * SIDE {
            (60 + i % 90) as u8
        } else if i.is_multiple_of(2) {
            90
        } else {
            230
        };
    }
    conn.create_dmabuf_buffer(CreateDmabufBuffer {
        id: BufferId(id),
        width: SIDE,
        height: SIDE,
        format: format::NV12,
        modifier: modifier::LINEAR,
        planes: vec![
            DmabufPlane {
                fd: fd.try_clone().unwrap(),
                offset: 0,
                stride: SIDE,
            },
            DmabufPlane {
                fd,
                offset: SIDE * SIDE,
                stride: SIDE,
            },
        ],
    })
    .unwrap();
    let mut out = vec![0u8; (SIDE * SIDE * 4) as usize];
    let mut canvas = Canvas::new(&mut out, SIDE, SIDE, SIDE * 4);
    let (y, uv) = map.as_bytes_mut().split_at((SIDE * SIDE) as usize);
    canvas.blit_nv12(
        &full(),
        &full(),
        &Nv12 {
            y,
            y_stride: SIDE,
            uv,
            uv_stride: SIDE,
            width: SIDE,
            height: SIDE,
        },
        &full(),
        YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited),
    );
    out.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect()
}

/// A Y-tiled NV12 "dma-buf": not CPU-readable.
fn tiled(conn: &mut Connection, id: u32) {
    let fd = nitro_shm::create_sealed("nitro-shot-tiled", 8192).unwrap();
    conn.create_dmabuf_buffer(CreateDmabufBuffer {
        id: BufferId(id),
        width: SIDE,
        height: SIDE,
        format: format::NV12,
        modifier: modifier::I915_Y_TILED,
        planes: vec![
            DmabufPlane {
                fd: fd.try_clone().unwrap(),
                offset: 0,
                stride: 128,
            },
            DmabufPlane {
                fd,
                offset: 4096,
                stride: 128,
            },
        ],
    })
    .unwrap();
}

/// Present `bufs` round-robin until the Surface is on the overlay.
fn onto_the_overlay(h: &Harness, conn: &mut Connection, seen: &mut Vec<ServerMsg>, bufs: &[u32]) {
    conn.commit(2).unwrap();
    conn.flush().unwrap();
    presented(conn, seen, 2);
    let mut serial = 3;
    let deadline = Instant::now() + Duration::from_secs(15);
    while h.stat("planes_mode") != 1 {
        assert!(Instant::now() < deadline, "never reached the overlay");
        present(conn, BufferId(bufs[serial as usize % bufs.len()]), serial);
        presented(conn, seen, serial);
        serial += 1;
    }
    h.settle();
}

fn tiled_on_the_overlay(h: &Harness) -> (Connection, Vec<ServerMsg>, Configure) {
    let (mut conn, mut seen, c) = client(h);
    tiled(&mut conn, 10);
    tiled(&mut conn, 11);
    onto_the_overlay(h, &mut conn, &mut seen, &[10, 11]);
    (conn, seen, c)
}

const CAPTURED: [u8; 3] = [CAPTURE_COLOR[0], CAPTURE_COLOR[1], CAPTURE_COLOR[2]];

#[test]
fn a_linear_dmabuf_on_a_plane_shows_its_pixels_not_the_hole() {
    let h = Harness::start("linear", GpuHelper::Off);
    let (mut conn, mut seen, c) = client(&h);
    let a = linear_nv12(&mut conn, 10);
    let b = linear_nv12(&mut conn, 11);
    assert_eq!(a, b);
    onto_the_overlay(&h, &mut conn, &mut seen, &[10, 11]);
    let s = h.shot("shot meta=1\n");
    assert_eq!(
        (
            s.m("surfaces"),
            s.m("cpu"),
            s.m("helper"),
            s.m("placeholder")
        ),
        ("1", "1", "0", "0")
    );
    assert_eq!(s.m("reason"), "none");
    assert_eq!(s.surface(&c), a, "the buffer, not the placeholder");
    // The default reply keeps its three fields.
    let mut raw = h.connect();
    raw.get_mut().write_all(b"shot\n").unwrap();
    let mut header = String::new();
    raw.read_line(&mut header).unwrap();
    assert_eq!(header.split_whitespace().count(), 4, "{header:?}");
    assert_eq!(h.stat("shot_cpu_surfaces"), 2, "both shots");
    h.quit();
}

#[test]
fn an_shm_surface_on_the_cpu_path_counts_as_cpu() {
    let h = Harness::start("shm", GpuHelper::Off);
    let (mut conn, mut seen, c) = client(&h);
    // No overlay slot for it: on the CPU path, so already in the shadow.
    let len = SIDE * SIDE * 4;
    let fd = nitro_shm::create_sealed("nitro-shot-shm", u64::from(len)).unwrap();
    let mut map = MappingMut::map_mut(fd.as_fd(), len as usize).unwrap();
    for p in map.as_bytes_mut().chunks_exact_mut(4) {
        p.copy_from_slice(&[0x20, 0x40, 0xd0, 0xff]);
    }
    conn.tx()
        .create_surface_buffer(nitro_wire::msg::CreateSurfaceBuffer {
            id: BufferId(10),
            width: SIDE,
            height: SIDE,
            format: format::XR24,
            size: len,
            offset0: 0,
            stride0: SIDE * 4,
            offset1: 0,
            stride1: 0,
            fd,
        })
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 2);
    present(&mut conn, BufferId(10), 3);
    presented(&mut conn, &mut seen, 3);
    h.settle();
    let s = h.shot("shot meta=1\n");
    assert_eq!((s.m("cpu"), s.m("placeholder")), ("1", "0"));
    assert!(s.surface(&c).iter().all(|p| *p == [0x20, 0x40, 0xd0]));
    h.quit();
}

#[test]
fn a_tiled_buffer_goes_through_the_helper_capture_and_nothing_stays() {
    let h = Harness::start("tiled", GpuHelper::On);
    wait_for("the helper", || h.stat("gpu_state") == 2);
    let (_conn, _seen, c) = tiled_on_the_overlay(&h);
    assert_eq!(
        h.stat("gpu_textures"),
        0,
        "on a plane: the helper holds nothing"
    );
    let s = h.shot("shot meta=1\n");
    assert_eq!(
        (
            s.m("surfaces"),
            s.m("cpu"),
            s.m("helper"),
            s.m("placeholder")
        ),
        ("1", "0", "1", "0")
    );
    assert!(
        s.surface(&c).iter().all(|p| *p == CAPTURED),
        "the capture's pixels: {:?}",
        &s.surface(&c)[..2]
    );
    let calls = h.fake.calls();
    let cap = calls
        .iter()
        .find_map(|c| match c {
            Call::Capture(w, h, t) => Some((*w, *h, t.clone())),
            _ => None,
        })
        .expect("a Capture");
    assert_eq!((cap.0, cap.1, cap.2.len()), (OUT.0, OUT.1, 1));
    // The texture imported for the shot is released afterwards.
    wait_for("the release", || {
        h.fake
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Release(t) if *t == cap.2[0]))
    });
    assert_eq!(h.stat("gpu_textures"), 0);
    assert_eq!(h.stat("shot_pending"), 0);
    assert_eq!(h.stat("shot_helper_captures"), 1);
    h.quit();
}

#[test]
fn an_on_demand_helper_is_started_for_the_shot() {
    let h = Harness::start("demand", GpuHelper::OnDemand);
    let (_conn, _seen, c) = tiled_on_the_overlay(&h);
    // Give an idle on-demand helper time to exit: the shot starts one.
    // Not polled meanwhile: every `stats` asks the helper for its own,
    // which is activity and holds off the idle exit.
    std::thread::sleep(Duration::from_millis(500));
    wait_for("the helper to be off", || h.stat("gpu_state") == 0);
    let spawns = h.stat("gpu_spawns");
    let s = h.shot("shot meta=1\n");
    assert_eq!(s.m("helper"), "1");
    assert!(s.surface(&c).iter().all(|p| *p == CAPTURED));
    assert_eq!(h.stat("gpu_spawns"), spawns + 1);
    assert_eq!(h.stat("gpu_textures"), 0);
    h.quit();
}

#[test]
fn without_a_helper_the_placeholder_says_why() {
    let h = Harness::start("off", GpuHelper::Off);
    let (_conn, _seen, c) = tiled_on_the_overlay(&h);
    let s = h.shot("shot meta=1\n");
    assert_eq!((s.m("placeholder"), s.m("reason")), ("1", "helper-off"));
    assert!(s.surface(&c).iter().all(|p| *p == GREY));
    assert_eq!(h.stat("shot_placeholders"), 1);
    h.quit();
}

#[test]
fn a_refused_or_hung_capture_is_the_placeholder_and_the_connection_lives() {
    let h = Harness::start("refused", GpuHelper::On);
    wait_for("the helper", || h.stat("gpu_state") == 2);
    let (_conn, _seen, c) = tiled_on_the_overlay(&h);
    h.fake.state().fail_next_capture = true;
    let s = h.shot("shot meta=1\n");
    assert_eq!((s.m("placeholder"), s.m("reason")), ("1", "helper-refused"));
    assert!(s.surface(&c).iter().all(|p| *p == GREY));
    assert_eq!(h.stat("gpu_textures"), 0);

    // A stalled helper: killed after the capture timeout.
    h.fake.state().stall = true;
    let mut conn = h.connect();
    let t0 = Instant::now();
    conn.get_mut().write_all(b"shot meta=1\nstats\n").unwrap();
    let s = read_shot(&mut conn);
    let took = t0.elapsed();
    assert_eq!(s.m("reason"), "helper-timeout");
    assert!(took >= Duration::from_millis(900), "{took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    let stats = read_text(&mut conn);
    assert!(stats.iter().any(|l| l.starts_with("shots ")), "then stats");
    h.fake.state().stall = false;
    assert!(h.stat("gpu_crashes") >= 1);
    h.quit();
}

#[test]
fn a_deferred_shot_keeps_replies_in_order() {
    let h = Harness::start("order", GpuHelper::On);
    wait_for("the helper", || h.stat("gpu_state") == 2);
    let (_conn, _seen, c) = tiled_on_the_overlay(&h);
    let mut conn = h.connect();
    conn.get_mut()
        .write_all(b"shot meta=1\nstats\nshot meta=1\n")
        .unwrap();
    let a = read_shot(&mut conn);
    assert_eq!(a.m("helper"), "1");
    let stats = read_text(&mut conn);
    assert!(stats.iter().any(|l| l.starts_with("frames ")));
    let b = read_shot(&mut conn);
    assert_eq!(b.surface(&c), a.surface(&c));
    h.quit();
}

#[test]
fn cursor_0_leaves_the_cursor_out() {
    let h = Harness::start("cursor", GpuHelper::Off);
    let (_conn, _seen, _c) = client(&h);
    let mut c = h.connect();
    c.get_mut().write_all(b"input motion 250 180\n").unwrap();
    let mut line = String::new();
    c.read_line(&mut line).unwrap();
    assert!(line.starts_with("ok"), "{line:?}");
    h.settle();
    let with = h.shot("shot\n");
    let without = h.shot("shot cursor=0\n");
    let differs = (250..260).any(|x| (180..190).any(|y| with.px(x, y) != without.px(x, y)));
    assert!(differs, "the cursor is in the default shot only");
    // Away from the cursor both are the same.
    assert_eq!(with.px(10, 10), without.px(10, 10));
    h.quit();
}
