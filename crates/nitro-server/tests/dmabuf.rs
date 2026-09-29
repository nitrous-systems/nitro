//! Client dma-buf Surfaces (#3918) end to end on the fake backend.
//!
//! Sealed memfds stand in for dma-bufs (the fake has no DRM device): they
//! pass the import checks, map on the CPU path, and have no implicit fence
//! (`EXPORT_SYNC_FILE` does not apply), so an implicit `PresentSurface` is
//! ready at once. Pipes stand in for `sync_file`s: the read end is the
//! fence, a byte written to the other end signals it.
//!
//! Covered: the `DMABUF` cap and its gate, every fatal `BadBuffer` of the
//! import checks, the CPU path's pixels, the placeholder for a layout the
//! CPU cannot read, the fenced latch queue's orderings and its limits,
//! presenting into a shared node, `SetSurface` refusing a dma-buf, and the
//! default and per-node `DmabufFeedback`.

#![allow(clippy::many_single_char_names)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{IRect, Rect, Size};
use nitro_kms::{FakePlaneSpec, Fourcc};
use nitro_raster::{Canvas, Nv12, YuvEncoding, YuvMatrix, YuvRange};
use nitro_server::{BackendKind, Config, run};
use nitro_shm::MappingMut;
use nitro_wire::client::Connection;
use nitro_wire::msg::{
    Configure, CreateDmabufBuffer, DmabufFeedback, DmabufPlane, PresentSurface, ServerMsg,
};
use nitro_wire::types::{
    BufferId, ColorMatrix, ColorRange, DmabufFormat, ErrorCode, Layer, NodeId, ShareToken, caps,
    dmabuf_flags, format, modifier,
};

const OUT: (u32, u32) = (320, 240);
/// Surface buffers are SIDE×SIDE.
const SIDE: u32 = 32;
/// Bytes of a SIDE×SIDE linear NV12 buffer.
const NV12_LEN: u32 = SIDE * SIDE * 3 / 2;

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
}

impl Harness {
    /// A fake output at `mhz` millihertz.
    fn start(name: &str, mhz: u32) -> Self {
        Self::start_with(name, mhz, Vec::new())
    }

    /// As [`Harness::start`], with a plane inventory for the fake output.
    fn start_with(name: &str, mhz: u32, planes: Vec<FakePlaneSpec>) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-dmabuf-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: OUT.0,
            height: OUT.1,
        };
        config.fake_modes = vec![(OUT.0, OUT.1, mhz)];
        config.fake_planes = planes;
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
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

    /// The front buffer as `(stride, bytes)`.
    fn shot(&self) -> (u32, Vec<u8>) {
        let mut c = self.connect();
        c.get_mut().write_all(b"shot\n").unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let f: Vec<u32> = header
            .trim_end()
            .strip_prefix("ok ")
            .expect("ok header")
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let mut data = vec![0u8; (f[2] * f[1]) as usize];
        c.read_exact(&mut data).unwrap();
        (f[2], data)
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

/// Poll until `f` matches something in `seen`.
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

/// Whatever is readable now, into `seen`.
fn drain(conn: &mut Connection, seen: &mut Vec<ServerMsg>) {
    let _ = conn.flush();
    let _ = conn.poll(seen);
}

/// Wait for `Presented{serial}`; its index in `seen`.
fn presented(conn: &mut Connection, seen: &mut Vec<ServerMsg>, serial: u32) -> usize {
    expect(conn, seen, &format!("Presented {serial}"), |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == serial).then_some(())
    });
    seen.iter()
        .position(|m| matches!(m, ServerMsg::Presented(p) if p.serial == serial))
        .unwrap()
}

fn was_presented(seen: &[ServerMsg], serial: u32) -> bool {
    seen.iter()
        .any(|m| matches!(m, ServerMsg::Presented(p) if p.serial == serial))
}

fn released_at(seen: &[ServerMsg], id: BufferId) -> Option<usize> {
    seen.iter()
        .position(|m| matches!(m, ServerMsg::BufferReleased(r) if r.id == id))
}

/// Wait for `BufferReleased{id}`; its index in `seen`.
fn released(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: BufferId) -> usize {
    expect(conn, seen, &format!("release of {}", id.raw()), |m| {
        matches!(m, ServerMsg::BufferReleased(r) if r.id == id).then_some(())
    });
    released_at(seen, id).unwrap()
}

/// A fatal error: the `Error`, then the server closes the socket.
fn fatal(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> (ErrorCode, String) {
    let e = expect(conn, seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some((e.code, e.msg.clone())),
        _ => None,
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match conn.poll(seen) {
            Err(_) => break,
            Ok(_) => assert!(
                Instant::now() < deadline,
                "still connected after {e:?}: the error must be fatal"
            ),
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    e
}

/// A local client that listed `DMABUF | SURFACE | RELEASE` and `extra`.
fn dma_client(h: &Harness, name: &str, extra: u32) -> (Connection, Vec<ServerMsg>) {
    let mut conn = h.client(name);
    assert!(conn.has_caps(caps::DMABUF), "caps = {:#x}", conn.caps());
    conn.client_caps(caps::DMABUF | caps::SURFACE | caps::RELEASE | extra)
        .unwrap();
    (conn, Vec::new())
}

/// A client-allocated "dma-buf": a sealed memfd holding a linear
/// SIDE×SIDE NV12 frame, Y at 0 and UV at `SIDE * SIDE`.
struct Dma {
    id: BufferId,
    map: MappingMut,
    fd: OwnedFd,
}

impl Dma {
    fn new(id: u32, seed: u8) -> Self {
        let fd = nitro_shm::create_sealed("nitro-dmabuf-test", u64::from(NV12_LEN)).unwrap();
        let mut map = MappingMut::map_mut(fd.as_fd(), NV12_LEN as usize).unwrap();
        let s = u32::from(seed);
        for (i, b) in map.as_bytes_mut().iter_mut().enumerate() {
            let i = i as u32;
            *b = ((i * 7 + (i / SIDE) * 13 + s * 41) % 220 + 16) as u8;
        }
        Self {
            id: BufferId(id),
            map,
            fd,
        }
    }

    fn plane(&self, offset: u32, stride: u32) -> DmabufPlane {
        DmabufPlane {
            fd: self.fd.try_clone().unwrap(),
            offset,
            stride,
        }
    }

    /// Two planes, each a dup of the one fd.
    fn create(&self) -> CreateDmabufBuffer {
        CreateDmabufBuffer {
            id: self.id,
            width: SIDE,
            height: SIDE,
            format: format::NV12,
            modifier: modifier::LINEAR,
            planes: vec![self.plane(0, SIDE), self.plane(SIDE * SIDE, SIDE)],
        }
    }

    /// What the rasterizer makes of it at 1:1, BT.709 limited.
    fn expected(&mut self) -> Vec<u8> {
        let mut out = vec![0u8; (SIDE * SIDE * 4) as usize];
        let mut canvas = Canvas::new(&mut out, SIDE, SIDE, SIDE * 4);
        let full = full();
        let (y, uv) = self.map.as_bytes_mut().split_at((SIDE * SIDE) as usize);
        let src = Nv12 {
            y,
            y_stride: SIDE,
            uv,
            uv_stride: SIDE,
            width: SIDE,
            height: SIDE,
        };
        canvas.blit_nv12(
            &full,
            &full,
            &src,
            &full,
            YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited),
        );
        out
    }
}

fn full() -> IRect {
    IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed())
}

const ROOT: NodeId = NodeId(1);
const SURF: NodeId = NodeId(2);
/// The importer's id for a shared node.
const IMP: NodeId = NodeId(7);

/// Open a 128×96 window holding Surface `SURF` (SIDE×SIDE at (8, 8)).
/// Returns the window's Configure.
fn window(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> Configure {
    conn.tx()
        .create_window(ROOT, "dmabuf", Size::new(128.0, 96.0), Layer::Normal)
        .create_surface(SURF, ROOT, Rect::new(8.0, 8.0, SIDE as f32, SIDE as f32))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == ROOT => Some(*c),
        _ => None,
    });
    presented(conn, seen, 1);
    c
}

/// Register `bufs` as dma-bufs, applied by commit `serial`.
fn register(conn: &mut Connection, seen: &mut Vec<ServerMsg>, bufs: &[&Dma], serial: u32) {
    for b in bufs {
        conn.create_dmabuf_buffer(b.create()).unwrap();
    }
    conn.commit(serial).unwrap();
    conn.flush().unwrap();
    presented(conn, seen, serial);
}

fn frame(id: NodeId, buffer: BufferId, serial: u32) -> PresentSurface {
    PresentSurface {
        id,
        buffer,
        serial,
        src: full(),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage: vec![],
    }
}

/// `PresentSurface`: implicit sync.
fn present(conn: &mut Connection, id: NodeId, buffer: BufferId, serial: u32) {
    conn.present_surface(frame(id, buffer, serial)).unwrap();
    conn.flush().unwrap();
}

/// `PresentSurfaceFenced` with a fresh, unsignalled pipe fence. Returns
/// the write end: keep it alive (a closed writer polls as signalled) and
/// [`signal`] it.
fn present_fenced(conn: &mut Connection, id: NodeId, buffer: BufferId, serial: u32) -> OwnedFd {
    let (r, w) = rustix::pipe::pipe().unwrap();
    conn.present_surface_fenced(frame(id, buffer, serial), r)
        .unwrap();
    conn.flush().unwrap();
    w
}

fn signal(w: &OwnedFd) {
    rustix::io::write(w, b"x").unwrap();
}

/// The SIDE×SIDE rect of the front buffer at the surface, as `[b, g, r]`.
fn grab(h: &Harness, c: &Configure) -> Vec<[u8; 3]> {
    let (stride, data) = h.shot();
    let (x0, y0) = (c.position.x as u32 + 8, c.position.y as u32 + 8);
    let mut out = Vec::new();
    for y in y0..y0 + SIDE {
        for x in x0..x0 + SIDE {
            let o = (y * stride + x * 4) as usize;
            out.push([data[o], data[o + 1], data[o + 2]]);
        }
    }
    out
}

fn rgb(v: &[u8]) -> Vec<[u8; 3]> {
    v.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect()
}

/// Put a flip in flight: change something visible, wait for the flip.
fn hold_a_flip(h: &Harness, conn: &mut Connection, serial: u32, x: f32) {
    conn.tx()
        .bounds(ROOT, Rect::new(0.0, 0.0, 128.0 + x, 96.0))
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    wait_for("a flip in flight", || h.stat("flips_pending") == 1);
}

// ------------------------------------------------------------------ caps

#[test]
fn dmabuf_is_advertised_and_its_ops_need_the_caps_listed() {
    let h = Harness::start("caps", 60_000);
    let a = Dma::new(1, 1);

    // CreateDmabufBuffer without DMABUF listed.
    let mut conn = h.client("nocap-create");
    assert!(conn.has_caps(caps::DMABUF), "advertised locally");
    conn.client_caps(caps::SURFACE | caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    conn.create_dmabuf_buffer(a.create()).unwrap();
    conn.flush().unwrap();
    let (code, msg) = fatal(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("DMABUF"), "{msg}");

    // PresentSurfaceFenced without DMABUF listed.
    let mut conn = h.client("nocap-present");
    conn.client_caps(caps::SURFACE | caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    let _w = present_fenced(&mut conn, SURF, a.id, 5);
    let (code, msg) = fatal(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("DMABUF"), "{msg}");

    // DMABUF without SURFACE.
    let mut conn = h.client("nosurface");
    conn.client_caps(caps::DMABUF).unwrap();
    let mut seen = Vec::new();
    conn.create_dmabuf_buffer(a.create()).unwrap();
    conn.flush().unwrap();
    let (code, msg) = fatal(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("SURFACE"), "{msg}");

    wait_for("the clients to go", || h.stat("clients") == 0);
    h.quit();
}

// ------------------------------------------------------------ validation

fn sealed(len: u64) -> OwnedFd {
    nitro_shm::create_sealed("nitro-dmabuf-bad", len).unwrap()
}

fn xr24(planes: Vec<DmabufPlane>) -> CreateDmabufBuffer {
    CreateDmabufBuffer {
        id: BufferId(1),
        width: SIDE,
        height: SIDE,
        format: format::XR24,
        modifier: modifier::LINEAR,
        planes,
    }
}

fn plane(fd: OwnedFd, offset: u32, stride: u32) -> DmabufPlane {
    DmabufPlane { fd, offset, stride }
}

#[test]
fn malformed_imports_are_fatal_bad_buffer() {
    let h = Harness::start("bad", 60_000);
    let xr24_len = u64::from(SIDE * SIDE * 4);
    let (pipe_r, _pipe_w) = rustix::pipe::pipe().unwrap();
    let unsealed =
        rustix::fs::memfd_create("nitro-unsealed", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
    rustix::fs::ftruncate(&unsealed, xr24_len).unwrap();
    let a = Dma::new(1, 1);

    let mut cases: Vec<(&str, CreateDmabufBuffer)> = vec![
        ("a pipe", xr24(vec![plane(pipe_r, 0, SIDE * 4)])),
        (
            "an unsealed memfd",
            xr24(vec![plane(unsealed, 0, SIDE * 4)]),
        ),
        ("no planes", xr24(vec![])),
        ("NV12 with one plane", {
            let mut m = a.create();
            m.planes.truncate(1);
            m
        }),
        ("Y-tiled NV12 no plane lists", {
            let mut m = a.create();
            m.modifier = modifier::I915_Y_TILED;
            m
        }),
        ("MOD_INVALID", {
            let mut m = xr24(vec![plane(sealed(xr24_len), 0, SIDE * 4)]);
            m.modifier = modifier::INVALID;
            m
        }),
        ("an unknown format", {
            let mut m = xr24(vec![plane(sealed(xr24_len), 0, SIDE * 4)]);
            m.format = format::fourcc(b"ZZZZ");
            m
        }),
        ("a plane outside its fd", {
            // Chroma needs 1024 + 16 × 32 bytes; the fd has 1024.
            let fd = sealed(u64::from(SIDE * SIDE));
            let mut m = a.create();
            m.planes = vec![
                plane(fd.try_clone().unwrap(), 0, SIDE),
                plane(fd, SIDE * SIDE, SIDE),
            ];
            m
        }),
        ("width 0", {
            let mut m = xr24(vec![plane(sealed(xr24_len), 0, SIDE * 4)]);
            m.width = 0;
            m
        }),
        ("a zero stride", xr24(vec![plane(sealed(xr24_len), 0, 0)])),
    ];
    for (i, (what, m)) in cases.drain(..).enumerate() {
        let (mut conn, mut seen) = dma_client(&h, &format!("bad{i}"), 0);
        conn.create_dmabuf_buffer(m).unwrap();
        conn.flush().unwrap();
        let (code, msg) = fatal(&mut conn, &mut seen);
        assert_eq!(code, ErrorCode::BadBuffer, "{what}: {msg}");
    }
    assert_eq!(h.stat("dmabuf_buffers"), 0);
    h.quit();
}

// ------------------------------------------------------------- CPU path

#[test]
fn a_linear_nv12_dmabuf_lands_on_the_cpu_path_and_is_freed() {
    let h = Harness::start("cpu", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "cpu", 0);
    let c = window(&mut conn, &mut seen);
    let (mut a, b) = (Dma::new(10, 1), Dma::new(11, 2));
    register(&mut conn, &mut seen, &[&a, &b], 2);
    assert_eq!(h.stat("dmabuf_buffers"), 2);
    assert_eq!(h.stat("dmabuf_cpu_mapped"), 2);
    // The fake always has a primary plane, and linear imports.
    assert_eq!(h.stat("dmabuf_kms_imported"), 2);

    present(&mut conn, SURF, a.id, 3);
    presented(&mut conn, &mut seen, 3);
    h.settle();
    assert_eq!(grab(&h, &c), rgb(&a.expected()), "NV12 dma-buf 1:1");
    // A memfd has no implicit fence: nothing waited, nothing polled.
    assert_eq!(h.stat("fence_waits"), 0);
    assert_eq!(h.stat("fences_pending"), 0);
    assert_eq!(h.stat("implicit_fence_fallbacks"), 0);

    // DestroyBuffer frees the one not on screen, KMS import included.
    conn.tx().destroy_buffer(b.id).commit(4).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 4);
    assert_eq!(h.stat("dmabuf_buffers"), 1);
    assert_eq!(h.stat("dmabuf_cpu_mapped"), 1);
    assert_eq!(h.stat("dmabuf_kms_imported"), 1);

    // Disconnect frees the rest.
    drop(conn);
    wait_for("the dma-bufs to go", || h.stat("dmabuf_buffers") == 0);
    assert_eq!(h.stat("dmabuf_kms_imported"), 0);
    h.quit();
}

#[test]
fn set_surface_naming_a_dmabuf_is_bad_buffer() {
    let h = Harness::start("setsurface", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "setsurface", 0);
    window(&mut conn, &mut seen);
    let a = Dma::new(10, 1);
    register(&mut conn, &mut seen, &[&a], 2);
    conn.tx()
        .set_surface(SURF, a.id, full(), ColorMatrix::Bt709, ColorRange::Limited)
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    let (code, msg) = fatal(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::BadBuffer, "{msg}");
    assert!(msg.contains("PresentSurface"), "{msg}");
    h.quit();
}

// ----------------------------------------------------------- placeholder

/// Primary XR24/AR24 linear; overlay NV12 linear + Y-tiled, YUYV linear.
fn planes() -> Vec<FakePlaneSpec> {
    vec![
        FakePlaneSpec::default_primary(),
        FakePlaneSpec::overlay()
            .format_mods(Fourcc::NV12, &[modifier::LINEAR, modifier::I915_Y_TILED])
            .formats(&[Fourcc::YUYV]),
    ]
}

#[test]
fn a_tiled_dmabuf_validates_and_paints_the_placeholder() {
    let h = Harness::start_with("tiled", 60_000, planes());
    let (mut conn, mut seen) = dma_client(&h, "tiled", 0);
    let c = window(&mut conn, &mut seen);
    let fd = sealed(8192);
    conn.create_dmabuf_buffer(CreateDmabufBuffer {
        id: BufferId(10),
        width: SIDE,
        height: SIDE,
        format: format::NV12,
        modifier: modifier::I915_Y_TILED,
        planes: vec![plane(fd.try_clone().unwrap(), 0, 128), plane(fd, 4096, 128)],
    })
    .unwrap();
    conn.commit(2).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 2);
    assert_eq!(h.stat("dmabuf_buffers"), 1);
    assert_eq!(h.stat("dmabuf_cpu_mapped"), 0, "tiled: not CPU-readable");
    assert_eq!(h.stat("dmabuf_kms_imported"), 1);
    assert_eq!(h.stat("dmabuf_kms_refused"), 0);

    // Process-global counter: other tests in this binary paint too.
    let before = h.stat("dmabuf_placeholder_paints");
    present(&mut conn, SURF, BufferId(10), 3);
    presented(&mut conn, &mut seen, 3);
    h.settle();
    assert!(h.stat("dmabuf_placeholder_paints") > before);
    let px = grab(&h, &c);
    assert!(
        px.iter().all(|p| *p == [0x80, 0x80, 0x80]),
        "the placeholder grey fills the surface rect: {:?}",
        &px[..4]
    );
    h.quit();
}

// ---------------------------------------------------------------- fences

#[test]
fn an_unsignalled_fence_holds_the_frame_until_it_signals() {
    let h = Harness::start("fence", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "fence", 0);
    let c = window(&mut conn, &mut seen);
    let mut a = Dma::new(10, 3);
    register(&mut conn, &mut seen, &[&a], 2);
    let w = present_fenced(&mut conn, SURF, a.id, 3);
    wait_for("the fence to be pending", || h.stat("fences_pending") == 1);
    assert_eq!(h.stat("fence_waits"), 1);
    h.settle();
    drain(&mut conn, &mut seen);
    assert!(!was_presented(&seen, 3), "not before its fence: {seen:?}");

    signal(&w);
    presented(&mut conn, &mut seen, 3);
    assert_eq!(h.stat("fences_pending"), 0);
    h.settle();
    assert_eq!(grab(&h, &c), rgb(&a.expected()));
    h.quit();
}

#[test]
fn a_ready_frame_supersedes_an_unsignalled_one_at_once() {
    let h = Harness::start("ready-wins", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "ready-wins", 0);
    window(&mut conn, &mut seen);
    let (a, b) = (Dma::new(10, 1), Dma::new(11, 2));
    register(&mut conn, &mut seen, &[&a, &b], 2);
    let _wa = present_fenced(&mut conn, SURF, a.id, 3);
    wait_for("A's fence to be pending", || h.stat("fences_pending") == 1);
    present(&mut conn, SURF, b.id, 4);
    let ra = released(&mut conn, &mut seen, a.id);
    let pb = presented(&mut conn, &mut seen, 4);
    assert!(ra < pb, "A released as it lost, before B shows: {seen:?}");
    assert_eq!(h.stat("fences_pending"), 0, "A's fence dropped with it");
    h.settle();
    drain(&mut conn, &mut seen);
    assert!(!was_presented(&seen, 3), "A is never presented: {seen:?}");
    assert!(released_at(&seen, b.id).is_none(), "B is current");
    h.quit();
}

#[test]
fn a_signalled_frame_shows_while_a_newer_fenced_one_waits() {
    // 2 Hz: both frames queue inside one pending flip.
    let h = Harness::start("older-ready", 2_000);
    let (mut conn, mut seen) = dma_client(&h, "older-ready", 0);
    window(&mut conn, &mut seen);
    let (a, b) = (Dma::new(10, 1), Dma::new(11, 2));
    register(&mut conn, &mut seen, &[&a, &b], 2);
    h.settle();
    seen.clear();

    hold_a_flip(&h, &mut conn, 3, 1.0);
    // A's fence has signalled before it is sent.
    let (ra_fd, wa) = rustix::pipe::pipe().unwrap();
    signal(&wa);
    conn.present_surface_fenced(frame(SURF, a.id, 4), ra_fd)
        .unwrap();
    let wb = present_fenced(&mut conn, SURF, b.id, 5);
    wait_for("B's fence to be pending", || h.stat("fences_pending") == 1);

    presented(&mut conn, &mut seen, 4);
    h.settle();
    drain(&mut conn, &mut seen);
    assert!(!was_presented(&seen, 5), "B waits for its fence: {seen:?}");
    assert!(released_at(&seen, a.id).is_none(), "A is current: {seen:?}");
    assert_eq!(h.stat("fences_pending"), 1);

    signal(&wb);
    let pb = presented(&mut conn, &mut seen, 5);
    let ra = released_at(&seen, a.id).expect("A released once B shows");
    assert!(
        ra < pb,
        "old buffer released before the new Presented: {seen:?}"
    );
    assert_eq!(h.stat("fences_pending"), 0);
    h.quit();
}

#[test]
fn the_newest_frame_to_signal_wins_and_older_waiters_are_released() {
    let h = Harness::start("newest-signal", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "newest-signal", 0);
    window(&mut conn, &mut seen);
    let (a, b) = (Dma::new(10, 1), Dma::new(11, 2));
    register(&mut conn, &mut seen, &[&a, &b], 2);
    let _wa = present_fenced(&mut conn, SURF, a.id, 3);
    let wb = present_fenced(&mut conn, SURF, b.id, 4);
    wait_for("both fences to be pending", || {
        h.stat("fences_pending") == 2
    });
    h.settle();
    drain(&mut conn, &mut seen);
    assert!(!was_presented(&seen, 3) && !was_presented(&seen, 4));

    signal(&wb);
    let pb = presented(&mut conn, &mut seen, 4);
    let ra = released_at(&seen, a.id).expect("A superseded and released");
    assert!(ra < pb, "{seen:?}");
    wait_for("A's fence to be dropped", || h.stat("fences_pending") == 0);
    h.settle();
    drain(&mut conn, &mut seen);
    assert!(!was_presented(&seen, 3), "A is never presented: {seen:?}");
    h.quit();
}

#[test]
fn a_disconnect_with_a_fence_pending_drops_it() {
    let h = Harness::start("fence-gone", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "fence-gone", 0);
    window(&mut conn, &mut seen);
    let a = Dma::new(10, 1);
    register(&mut conn, &mut seen, &[&a], 2);
    let _w = present_fenced(&mut conn, SURF, a.id, 3);
    wait_for("the fence to be pending", || h.stat("fences_pending") == 1);
    drop(conn);
    wait_for("the fence to go", || h.stat("fences_pending") == 0);
    assert_eq!(h.stat("dmabuf_buffers"), 0);
    h.quit();
}

/// Each node queues at most `surface::MAX_QUEUED` (4) frames, so the
/// 64-fence client limit takes 17 nodes to reach.
#[test]
fn more_than_64_pending_fences_is_limit() {
    let h = Harness::start("fence-limit", 60_000);
    let (mut conn, mut seen) = dma_client(&h, "fence-limit", 0);
    let nodes: Vec<NodeId> = (2..19).map(NodeId).collect();
    let mut tx = conn
        .tx()
        .create_window(ROOT, "many", Size::new(200.0, 100.0), Layer::Normal);
    for (i, &n) in nodes.iter().enumerate() {
        let x = (i % 8) as f32 * 20.0;
        let y = (i / 8) as f32 * 20.0;
        tx = tx.create_surface(n, ROOT, Rect::new(x, y, 16.0, 16.0));
    }
    tx.commit(1).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    let a = Dma::new(100, 1);
    register(&mut conn, &mut seen, &[&a], 2);

    let mut writers = Vec::new();
    let mut serial = 10;
    // The per-node cap: a fifth unready frame drops the oldest, fence
    // and all.
    for _ in 0..5 {
        writers.push(present_fenced(&mut conn, nodes[0], a.id, serial));
        serial += 1;
    }
    conn.commit(3).unwrap();
    presented(&mut conn, &mut seen, 3);
    assert_eq!(h.stat("fences_pending"), 4, "capped per node");

    for (i, &n) in nodes[1..16].iter().enumerate() {
        for _ in 0..4 {
            writers.push(present_fenced(&mut conn, n, a.id, serial));
            serial += 1;
        }
        let s = 4 + i as u32;
        conn.commit(s).unwrap();
        presented(&mut conn, &mut seen, s);
    }
    assert_eq!(h.stat("fences_pending"), 64);

    writers.push(present_fenced(&mut conn, nodes[16], a.id, serial));
    let (code, msg) = fatal(&mut conn, &mut seen);
    assert_eq!(code, ErrorCode::Limit, "{msg}");
    assert!(msg.contains("fences"), "{msg}");
    wait_for("the fences to go", || h.stat("fences_pending") == 0);
    drop(writers);
    h.quit();
}

// --------------------------------------------------------------- sharing

#[test]
fn an_importer_presents_a_fenced_dmabuf_into_a_shared_node() {
    let h = Harness::start("share", 60_000);
    let mut owner = h.client("owner");
    owner
        .client_caps(caps::SHARE | caps::SURFACE | caps::RELEASE)
        .unwrap();
    let mut so = Vec::new();
    let c = window(&mut owner, &mut so);
    owner.export_surface(SURF).unwrap();
    owner.flush().unwrap();
    let token: ShareToken = expect(&mut owner, &mut so, "SurfaceExported", |m| match m {
        ServerMsg::SurfaceExported(e) if e.id == SURF => Some(e.token),
        _ => None,
    });

    let (mut b, mut sb) = dma_client(&h, "importer", caps::SHARE);
    let (mut x, y) = (Dma::new(1, 4), Dma::new(2, 5));
    register(&mut b, &mut sb, &[&x, &y], 1);
    b.import_surface(token, IMP).unwrap();
    b.flush().unwrap();
    let w = present_fenced(&mut b, IMP, x.id, 10);
    wait_for("the importer's fence", || h.stat("fences_pending") == 1);
    h.settle();
    drain(&mut b, &mut sb);
    assert!(!was_presented(&sb, 10), "{sb:?}");

    signal(&w);
    presented(&mut b, &mut sb, 10);
    h.settle();
    assert_eq!(grab(&h, &c), rgb(&x.expected()), "B's pixels in A's window");

    present(&mut b, IMP, y.id, 11);
    let py = presented(&mut b, &mut sb, 11);
    let rx = released(&mut b, &mut sb, x.id);
    assert!(rx < py, "{sb:?}");
    h.settle();
    drain(&mut owner, &mut so);
    assert!(
        !so.iter().any(|m| matches!(m, ServerMsg::BufferReleased(_))
            || matches!(m, ServerMsg::Presented(p) if p.serial >= 10)),
        "the owner hears nothing of B's frames: {so:?}"
    );
    h.quit();
}

// -------------------------------------------------------------- feedback

fn expected_feedback() -> Vec<DmabufFormat> {
    use dmabuf_flags::{CPU, IMPORT, SCANOUT};
    let e = |format, modifier, flags| DmabufFormat {
        format,
        modifier,
        flags,
    };
    let mut v = vec![
        e(format::NV12, modifier::LINEAR, CPU | IMPORT | SCANOUT),
        e(format::NV12, modifier::I915_Y_TILED, IMPORT | SCANOUT),
        e(format::YUYV, modifier::LINEAR, CPU | IMPORT | SCANOUT),
        e(format::UYVY, modifier::LINEAR, CPU | IMPORT),
        e(format::XR24, modifier::LINEAR, CPU | IMPORT | SCANOUT),
        e(format::AR24, modifier::LINEAR, CPU | IMPORT | SCANOUT),
    ];
    v.sort_by_key(|f| (f.format, f.modifier));
    v
}

fn feedback_for(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    what: &str,
    f: impl Fn(&DmabufFeedback) -> bool,
) -> DmabufFeedback {
    expect(conn, seen, what, |m| match m {
        ServerMsg::DmabufFeedback(d) if f(d) => Some(d.clone()),
        _ => None,
    })
}

#[test]
fn the_default_feedback_follows_client_caps() {
    let h = Harness::start_with("feedback", 60_000, planes());
    let (mut conn, mut seen) = dma_client(&h, "feedback", 0);
    let d = feedback_for(&mut conn, &mut seen, "the default feedback", |d| {
        d.id == NodeId(0)
    });
    assert_eq!(d.main_device, 0, "the fake has no device");
    assert_eq!((d.max_width, d.max_height), OUT);
    assert_eq!(d.formats, expected_feedback());

    // A client that did not list DMABUF hears none, even with a Surface.
    let mut old = h.client("old");
    old.client_caps(caps::SURFACE | caps::RELEASE).unwrap();
    let mut so = Vec::new();
    window(&mut old, &mut so);
    h.settle();
    drain(&mut old, &mut so);
    assert!(
        !so.iter().any(|m| matches!(m, ServerMsg::DmabufFeedback(_))),
        "{so:?}"
    );
    h.quit();
}

#[test]
fn a_surface_node_gets_its_outputs_feedback_and_again_after_a_hotplug() {
    let h = Harness::start_with("node-feedback", 60_000, planes());
    let (mut conn, mut seen) = dma_client(&h, "node-feedback", 0);
    window(&mut conn, &mut seen);
    let d = feedback_for(&mut conn, &mut seen, "the node's feedback", |d| {
        d.id == SURF
    });
    assert_eq!((d.max_width, d.max_height), OUT);
    assert_eq!(d.formats, expected_feedback());
    h.settle();
    drain(&mut conn, &mut seen);
    let count = |seen: &[ServerMsg]| {
        seen.iter()
            .filter(|m| matches!(m, ServerMsg::DmabufFeedback(d) if d.id == SURF))
            .count()
    };
    assert_eq!(count(&seen), 1, "once, not every frame: {seen:?}");
    seen.clear();

    assert_eq!(h.request_line("plug 640x480\n"), "ok");
    let d = feedback_for(&mut conn, &mut seen, "a new default feedback", |d| {
        d.id == NodeId(0) && d.max_width == 640
    });
    assert_eq!(d.max_height, 480);
    // The plugged output has only a default primary: nothing new, the
    // union is unchanged.
    assert_eq!(d.formats, expected_feedback());
    let d = feedback_for(&mut conn, &mut seen, "the node's feedback again", |d| {
        d.id == SURF
    });
    assert_eq!(
        (d.max_width, d.max_height),
        OUT,
        "still on the first output"
    );
    assert_eq!(d.formats, expected_feedback());
    h.quit();
}
