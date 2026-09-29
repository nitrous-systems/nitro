//! Surface v1 (#3897) end to end on the fake backend: shm Surface buffers
//! in NV12 / YUYV land with the right pixels, `PresentSurface` latches at
//! the next paint opportunity with the newest frame winning, releases are
//! ordered before the `Presented` they make room for, a committed
//! `SetSurface` cancels a queued frame, only the surface rect is damaged,
//! the ops are gated on `SURFACE` in `ClientCaps`, and `SurfaceHint`
//! follows the node's device size.
//!
//! The fake output runs at **2 Hz**, so "a flip is pending" is a
//! half-second window a test can act inside without racing.

#![allow(clippy::many_single_char_names)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{IRect, Rect, Size};
use nitro_raster::{Canvas, Nv12, Packed422, Packed422Order, YuvEncoding, YuvMatrix, YuvRange};
use nitro_server::{BackendKind, Config, run};
use nitro_shm::MappingMut;
use nitro_wire::client::Connection;
use nitro_wire::msg::{
    AllocSurfaceBuffers, ClientMsg, Configure, CreateSurfaceBuffer, PresentSurface, ServerMsg,
    SetBounds, SurfaceBufferAllocated,
};
use nitro_wire::types::{
    AllocRefusal, BufferId, ColorMatrix, ColorRange, ErrorCode, Layer, NodeId, caps, format,
};

const OUT: (u32, u32) = (320, 240);
/// Surface buffers are SIDE×SIDE.
const SIDE: u32 = 32;

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
    fn start_with(name: &str, mhz: u32, planes: Vec<nitro_kms::FakePlaneSpec>) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-surf-{}-{name}", std::process::id()));
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

    /// `samples damage`: (total ever, values oldest first).
    fn damage_samples(&self) -> (u64, Vec<u64>) {
        let lines = self.request_text("samples damage\n");
        let total = lines[0].strip_prefix("ok ").unwrap().parse().unwrap();
        let vals = lines[1..].iter().map(|l| l.parse().unwrap()).collect();
        (total, vals)
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

fn presented(conn: &mut Connection, seen: &mut Vec<ServerMsg>, serial: u32) -> usize {
    expect(conn, seen, &format!("Presented {serial}"), |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == serial).then_some(())
    });
    seen.iter()
        .position(|m| matches!(m, ServerMsg::Presented(p) if p.serial == serial))
        .unwrap()
}

fn released_at(seen: &[ServerMsg], id: BufferId) -> Option<usize> {
    seen.iter()
        .position(|m| matches!(m, ServerMsg::BufferReleased(r) if r.id == id))
}

/// A client-side surface buffer in `format`, with a writable mapping.
struct Buf {
    id: BufferId,
    format: u32,
    map: MappingMut,
    fd: OwnedFd,
}

impl Buf {
    fn size(format: u32) -> u32 {
        if format == format::NV12 {
            SIDE * SIDE * 3 / 2
        } else {
            SIDE * SIDE * 2
        }
    }

    fn new(id: u32, format: u32, seed: u8) -> Self {
        let len = Self::size(format);
        let fd = nitro_shm::create_sealed("nitro-surface-test", u64::from(len)).unwrap();
        let map = MappingMut::map_mut(fd.as_fd(), len as usize).unwrap();
        let mut b = Self {
            id: BufferId(id),
            format,
            map,
            fd,
        };
        b.fill(seed);
        b
    }

    /// A deterministic pattern that exercises every channel.
    fn fill(&mut self, seed: u8) {
        let s = u32::from(seed);
        let bytes = self.map.as_bytes_mut();
        for (i, b) in bytes.iter_mut().enumerate() {
            let i = i as u32;
            *b = ((i * 7 + (i / SIDE) * 13 + s * 41) % 220 + 16) as u8;
        }
    }

    fn create(&self) -> CreateSurfaceBuffer {
        let nv12 = self.format == format::NV12;
        CreateSurfaceBuffer {
            id: self.id,
            width: SIDE,
            height: SIDE,
            format: self.format,
            size: Self::size(self.format),
            offset0: 0,
            stride0: if nv12 { SIDE } else { SIDE * 2 },
            offset1: if nv12 { SIDE * SIDE } else { 0 },
            stride1: if nv12 { SIDE } else { 0 },
            fd: self.fd.try_clone().unwrap(),
        }
    }

    /// What the rasterizer makes of this buffer blitted into `w×h`.
    fn expected(&mut self, w: u32, h: u32, enc: YuvEncoding) -> Vec<u8> {
        let mut out = vec![0u8; (w * h * 4) as usize];
        let mut canvas = Canvas::new(&mut out, w, h, w * 4);
        let dst = IRect::new(0, 0, w.cast_signed(), h.cast_signed());
        let full = IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed());
        let data = self.map.as_bytes_mut();
        if self.format == format::NV12 {
            let (y, uv) = data.split_at((SIDE * SIDE) as usize);
            let src = Nv12 {
                y,
                y_stride: SIDE,
                uv,
                uv_stride: SIDE,
                width: SIDE,
                height: SIDE,
            };
            canvas.blit_nv12(&dst, &dst, &src, &full, enc);
        } else {
            let src = Packed422 {
                data,
                stride: SIDE * 2,
                width: SIDE,
                height: SIDE,
                order: if self.format == format::YUYV {
                    Packed422Order::Yuyv
                } else {
                    Packed422Order::Uyvy
                },
            };
            canvas.blit_yuyv(&dst, &dst, &src, &full, enc);
        }
        out
    }
}

fn full() -> IRect {
    IRect::new(0, 0, SIDE.cast_signed(), SIDE.cast_signed())
}

const ROOT: NodeId = NodeId(1);
const SURF: NodeId = NodeId(2);

/// Open a 128×96 window holding surface node `SURF` of `size` at (8, 8),
/// with the given buffers registered. Returns the window's Configure.
fn window(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    bufs: &[&Buf],
    size: (f32, f32),
) -> Configure {
    let mut tx = conn
        .tx()
        .create_window(ROOT, "surf", Size::new(128.0, 96.0), Layer::Normal)
        .create_surface(SURF, ROOT, Rect::new(8.0, 8.0, size.0, size.1));
    for b in bufs {
        tx = tx.create_surface_buffer(b.create());
    }
    tx.commit(1).unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == ROOT => Some(*c),
        _ => None,
    });
    presented(conn, seen, 1);
    c
}

fn present(conn: &mut Connection, buf: &Buf, serial: u32, damage: Vec<IRect>) {
    conn.present_surface(PresentSurface {
        id: SURF,
        buffer: buf.id,
        serial,
        src: full(),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage,
    })
    .unwrap();
    conn.flush().unwrap();
}

/// The `w×h` rect of the front buffer at the surface's position.
fn grab(h: &Harness, c: &Configure, w: u32, hh: u32) -> Vec<u8> {
    let (stride, data) = h.shot();
    let (x0, y0) = (c.position.x as u32 + 8, c.position.y as u32 + 8);
    let mut out = Vec::with_capacity((w * hh * 4) as usize);
    for y in y0..y0 + hh {
        for x in x0..x0 + w {
            let o = (y * stride + x * 4) as usize;
            out.extend_from_slice(&[data[o], data[o + 1], data[o + 2], 0]);
        }
    }
    out
}

fn rgb(v: &[u8]) -> Vec<[u8; 3]> {
    v.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect()
}

fn surface_client(h: &Harness, name: &str) -> (Connection, Vec<ServerMsg>) {
    let mut conn = h.client(name);
    assert!(conn.has_caps(caps::SURFACE), "caps = {:#x}", conn.caps());
    assert!(
        conn.has_caps(caps::DMABUF),
        "DMABUF is advertised locally (#3918)"
    );
    conn.client_caps(caps::SURFACE | caps::RELEASE).unwrap();
    (conn, Vec::new())
}

#[test]
fn a_known_nv12_frame_lands_with_the_expected_pixels() {
    let h = Harness::start("nv12", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "nv12");
    let mut a = Buf::new(1, format::NV12, 1);
    let c = window(&mut conn, &mut seen, &[&a], (SIDE as f32, SIDE as f32));
    let bt709 = YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited);

    // 1:1, via the committed path.
    conn.tx()
        .set_surface(SURF, a.id, full(), ColorMatrix::Bt709, ColorRange::Limited)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 2);
    h.settle();
    let want = a.expected(SIDE, SIDE, bt709);
    assert_eq!(rgb(&grab(&h, &c, SIDE, SIDE)), rgb(&want), "NV12 1:1");

    // The colour metadata matters: BT.601 full range is other pixels.
    conn.tx()
        .set_surface(SURF, a.id, full(), ColorMatrix::Bt601, ColorRange::Full)
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 3);
    h.settle();
    let want601 = a.expected(
        SIDE,
        SIDE,
        YuvEncoding::new(YuvMatrix::Bt601, YuvRange::Full),
    );
    assert_ne!(rgb(&want601), rgb(&want));
    assert_eq!(rgb(&grab(&h, &c, SIDE, SIDE)), rgb(&want601), "BT.601 full");

    // Scaled: the node grows to 64×48.
    conn.tx()
        .bounds(SURF, Rect::new(8.0, 8.0, 64.0, 48.0))
        .set_surface(SURF, a.id, full(), ColorMatrix::Bt709, ColorRange::Limited)
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 4);
    h.settle();
    let want = a.expected(64, 48, bt709);
    assert_eq!(rgb(&grab(&h, &c, 64, 48)), rgb(&want), "NV12 scaled");
    h.quit();
}

#[test]
fn a_yuyv_frame_lands_with_the_expected_pixels() {
    let h = Harness::start("yuyv", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "yuyv");
    let mut a = Buf::new(1, format::YUYV, 2);
    let mut b = Buf::new(2, format::UYVY, 3);
    let c = window(&mut conn, &mut seen, &[&a, &b], (SIDE as f32, SIDE as f32));
    let enc = YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited);
    present(&mut conn, &a, 10, vec![]);
    presented(&mut conn, &mut seen, 10);
    h.settle();
    assert_eq!(
        rgb(&grab(&h, &c, SIDE, SIDE)),
        rgb(&a.expected(SIDE, SIDE, enc)),
        "YUYV"
    );
    present(&mut conn, &b, 11, vec![]);
    presented(&mut conn, &mut seen, 11);
    h.settle();
    assert_eq!(
        rgb(&grab(&h, &c, SIDE, SIDE)),
        rgb(&b.expected(SIDE, SIDE, enc)),
        "UYVY"
    );
    h.quit();
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

#[test]
fn the_newest_frame_wins_and_superseded_ones_are_released_at_once() {
    // 2 Hz: a pending flip lasts half a second.
    let h = Harness::start("newest", 2_000);
    let (mut conn, mut seen) = surface_client(&h, "newest");
    let (a, b, cc, d) = (
        Buf::new(1, format::NV12, 1),
        Buf::new(2, format::NV12, 2),
        Buf::new(3, format::NV12, 3),
        Buf::new(4, format::NV12, 4),
    );
    let c = window(
        &mut conn,
        &mut seen,
        &[&a, &b, &cc, &d],
        (SIDE as f32, SIDE as f32),
    );
    // D is current.
    present(&mut conn, &d, 5, vec![]);
    presented(&mut conn, &mut seen, 5);
    h.settle();
    seen.clear();

    hold_a_flip(&h, &mut conn, 6, 1.0);
    present(&mut conn, &a, 7, vec![]);
    present(&mut conn, &b, 8, vec![]);
    present(&mut conn, &cc, 9, vec![]);
    // A and B come back before any flip: superseded, never shown.
    expect(&mut conn, &mut seen, "release of B", |m| {
        matches!(m, ServerMsg::BufferReleased(r) if r.id == b.id).then_some(())
    });
    assert_eq!(h.stat("flips_pending"), 1, "released inside the flip");
    let (ra, rb) = (
        released_at(&seen, a.id).unwrap(),
        released_at(&seen, b.id).unwrap(),
    );
    assert!(ra < rb, "in order: {seen:?}");
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Presented(_))),
        "no Presented yet: {seen:?}"
    );
    // C is shown after the flip; D (the old current) is released before
    // C's Presented.
    let pc = presented(&mut conn, &mut seen, 9);
    let rd = released_at(&seen, d.id).expect("D released");
    assert!(
        rd < pc,
        "old buffer released before the new frame's Presented: {seen:?}"
    );
    h.settle();
    let _ = conn.poll(&mut seen);
    for s in [7, 8] {
        assert!(
            !seen
                .iter()
                .any(|m| matches!(m, ServerMsg::Presented(p) if p.serial == s)),
            "dropped frame {s} was never presented: {seen:?}"
        );
    }
    assert!(released_at(&seen, cc.id).is_none(), "C is current");
    let mut want_c = Buf::new(99, format::NV12, 3);
    let enc = YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited);
    assert_eq!(
        rgb(&grab(&h, &c, SIDE, SIDE)),
        rgb(&want_c.expected(SIDE, SIDE, enc))
    );
    h.quit();
}

#[test]
fn a_committed_set_surface_cancels_a_queued_frame() {
    let h = Harness::start("cancel", 2_000);
    let (mut conn, mut seen) = surface_client(&h, "cancel");
    let (a, b) = (Buf::new(1, format::NV12, 1), Buf::new(2, format::NV12, 2));
    window(&mut conn, &mut seen, &[&a, &b], (SIDE as f32, SIDE as f32));
    hold_a_flip(&h, &mut conn, 2, 1.0);
    present(&mut conn, &a, 3, vec![]);
    conn.tx()
        .set_surface(SURF, b.id, full(), ColorMatrix::Bt709, ColorRange::Limited)
        .commit(4)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "release of A", |m| {
        matches!(m, ServerMsg::BufferReleased(r) if r.id == a.id).then_some(())
    });
    presented(&mut conn, &mut seen, 4);
    h.settle();
    let _ = conn.poll(&mut seen);
    assert!(
        !seen
            .iter()
            .any(|m| matches!(m, ServerMsg::Presented(p) if p.serial == 3)),
        "the cancelled frame is never presented: {seen:?}"
    );
    assert!(released_at(&seen, b.id).is_none(), "B is current");
    h.quit();
}

#[test]
fn only_the_surface_rect_or_its_damage_is_repainted() {
    let h = Harness::start("damage", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "damage");
    let (a, b) = (Buf::new(1, format::NV12, 1), Buf::new(2, format::NV12, 2));
    window(&mut conn, &mut seen, &[&a, &b], (SIDE as f32, SIDE as f32));
    // Show both once, so the swap rule applies afterwards.
    present(&mut conn, &a, 2, vec![]);
    presented(&mut conn, &mut seen, 2);
    h.settle();
    let (before, _) = h.damage_samples();
    present(&mut conn, &b, 3, vec![]);
    presented(&mut conn, &mut seen, 3);
    h.settle();
    let (after, vals) = h.damage_samples();
    let new = &vals[vals.len() - (after - before) as usize..];
    assert_eq!(
        new[0],
        u64::from(SIDE * SIDE),
        "a first swap damages the rect: {new:?}"
    );

    // Back to A (shown before) with a partial damage rect.
    let (before, _) = h.damage_samples();
    present(&mut conn, &a, 4, vec![IRect::new(4, 4, 8, 6)]);
    presented(&mut conn, &mut seen, 4);
    h.settle();
    let (after, vals) = h.damage_samples();
    let new = &vals[vals.len() - (after - before) as usize..];
    assert_eq!(new[0], 8 * 6, "only the frame's damage: {new:?}");
    h.quit();
}

#[test]
fn the_surface_ops_need_the_cap_listed() {
    let h = Harness::start("nocap", 60_000);
    let mut conn = h.client("old");
    let mut seen = Vec::new();
    let a = Buf::new(1, format::NV12, 1);
    conn.tx()
        .create_window(ROOT, "old", Size::new(64.0, 64.0), Layer::Normal)
        .create_surface_buffer(a.create())
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let (code, msg) = expect(&mut conn, &mut seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some((e.code, e.msg.clone())),
        _ => None,
    });
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("SURFACE"), "{msg}");
    h.quit();
}

#[test]
fn a_bad_surface_buffer_is_refused() {
    let h = Harness::start("badbuf", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "badbuf");
    let a = Buf::new(1, format::NV12, 1);
    let mut m = a.create();
    // Chroma past the end of `size`.
    m.offset1 = m.size - 8;
    conn.tx().create_surface_buffer(m).commit(1).unwrap();
    conn.flush().unwrap();
    let code = expect(&mut conn, &mut seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadBuffer);
    h.quit();
}

#[test]
fn a_hint_follows_the_device_size() {
    let h = Harness::start("hint", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "hint");
    window(&mut conn, &mut seen, &[], (100.0, 50.0));
    let hint = expect(&mut conn, &mut seen, "SurfaceHint", |m| match m {
        ServerMsg::SurfaceHint(s) => Some(*s),
        _ => None,
    });
    assert_eq!(
        (hint.id, hint.format, hint.width, hint.height),
        (SURF, format::NV12, 100, 50)
    );
    seen.clear();
    conn.send(&ClientMsg::SetBounds(SetBounds {
        id: SURF,
        rect: Rect::new(0.0, 0.0, 64.0, 36.0),
    }))
    .unwrap();
    conn.commit(2).unwrap();
    conn.flush().unwrap();
    let hint = expect(&mut conn, &mut seen, "a second SurfaceHint", |m| match m {
        ServerMsg::SurfaceHint(s) => Some(*s),
        _ => None,
    });
    assert_eq!((hint.width, hint.height), (64, 36));
    h.quit();
}

#[test]
fn destroy_or_disconnect_with_a_queued_frame_is_quiet() {
    let h = Harness::start("destroy", 2_000);
    let (mut conn, mut seen) = surface_client(&h, "destroy");
    let (a, b) = (Buf::new(1, format::NV12, 1), Buf::new(2, format::NV12, 2));
    window(&mut conn, &mut seen, &[&a, &b], (SIDE as f32, SIDE as f32));
    hold_a_flip(&h, &mut conn, 2, 1.0);
    present(&mut conn, &a, 3, vec![]);
    conn.tx().destroy_node(SURF).commit(4).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 4);
    h.settle();
    let _ = conn.poll(&mut seen);
    assert!(released_at(&seen, a.id).is_none(), "{seen:?}");
    assert!(
        !seen
            .iter()
            .any(|m| matches!(m, ServerMsg::Presented(p) if p.serial == 3)),
        "{seen:?}"
    );

    // And a client that vanishes with a frame queued takes nothing down.
    let (mut other, mut seen2) = surface_client(&h, "vanish");
    window(&mut other, &mut seen2, &[&b], (SIDE as f32, SIDE as f32));
    hold_a_flip(&h, &mut other, 2, 2.0);
    present(&mut other, &b, 3, vec![]);
    drop(other);
    h.settle();
    assert!(h.stat("frames") > 0);
    h.quit();
}

// ------------------------------------------------ server-allocated (#3914)

fn alloc(conn: &mut Connection, first: u32, count: u8, fmt: u32, w: u32, h: u32) {
    conn.alloc_surface_buffers(AllocSurfaceBuffers {
        node: SURF,
        first_id: BufferId(first),
        count,
        format: fmt,
        width: w,
        height: h,
    })
    .unwrap();
    conn.flush().unwrap();
}

/// Wait for `count` `SurfaceBufferAllocated`s starting at `first`, and
/// take them out of `seen`.
fn allocated(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    first: u32,
    count: u32,
) -> Vec<SurfaceBufferAllocated> {
    let last = BufferId(first + count - 1);
    expect(conn, seen, "SurfaceBufferAllocated", |m| {
        matches!(m, ServerMsg::SurfaceBufferAllocated(a) if a.id == last).then_some(())
    });
    let mut out = Vec::new();
    let mut rest = Vec::new();
    for m in seen.drain(..) {
        match m {
            ServerMsg::SurfaceBufferAllocated(a) => out.push(a),
            other => rest.push(other),
        }
    }
    *seen = rest;
    out.sort_by_key(|a| a.id.raw());
    assert_eq!(out.len(), count as usize, "{out:?}");
    out
}

fn refused(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> AllocRefusal {
    let r = expect(conn, seen, "AllocSurfaceBuffersFailed", |m| match m {
        ServerMsg::AllocSurfaceBuffersFailed(f) => Some(f.reason),
        _ => None,
    });
    seen.retain(|m| !matches!(m, ServerMsg::AllocSurfaceBuffersFailed(_)));
    r
}

/// Fill a server-allocated NV12 buffer with a pattern through its
/// dma-buf mapping, and return what the rasterizer makes of it at 1:1.
fn fill_scanout_nv12(a: &SurfaceBufferAllocated, seed: u32) -> Vec<u8> {
    use nitro_shm::{DmaBufMapping, SyncAccess, sync_end, sync_start};
    let mut map = DmaBufMapping::map(a.fd.as_fd(), a.size as usize).unwrap();
    // The fake exports a memfd: no sync needed, and the bracket says so.
    assert_eq!(sync_start(&a.fd, SyncAccess::Write), Ok(false));
    let bytes = map.as_bytes_mut();
    for (i, b) in bytes.iter_mut().enumerate() {
        let i = i as u32;
        *b = ((i * 7 + (i / a.stride0) * 13 + seed * 41) % 220 + 16) as u8;
    }
    assert_eq!(sync_end(&a.fd, SyncAccess::Write), Ok(false));
    let (w, h) = (a.width, a.height);
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut canvas = Canvas::new(&mut out, w, h, w * 4);
    let dst = IRect::new(0, 0, w.cast_signed(), h.cast_signed());
    let data = map.as_bytes();
    let src = Nv12 {
        y: &data[a.offset0 as usize..],
        y_stride: a.stride0,
        uv: &data[a.offset1 as usize..],
        uv_stride: a.stride1,
        width: w,
        height: h,
    };
    canvas.blit_nv12(
        &dst,
        &dst,
        &src,
        &dst,
        YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited),
    );
    out
}

#[test]
fn server_allocated_buffers_are_written_presented_and_freed() {
    let h = Harness::start("scanout", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "scanout");
    let c = window(&mut conn, &mut seen, &[], (SIDE as f32, SIDE as f32));
    alloc(&mut conn, 10, 3, format::NV12, SIDE, SIDE);
    let bufs = allocated(&mut conn, &mut seen, 10, 3);
    for (i, a) in bufs.iter().enumerate() {
        assert_eq!(a.node, SURF);
        assert_eq!(a.id, BufferId(10 + i as u32));
        assert_eq!((a.format, a.width, a.height), (format::NV12, SIDE, SIDE));
        // The fake's pitch is 64-byte aligned, like a dumb buffer's.
        assert_eq!(a.stride0, 64);
        assert!(!nitro_shm::is_dmabuf(&a.fd));
    }
    assert_eq!(h.stat("scanout_buffers"), 3);
    let per = u64::from(bufs[0].size);
    assert_eq!(h.stat("scanout_buffer_bytes"), 3 * per);

    // Write through the client's mapping, present, see it on the CPU path.
    for (serial, (a, seed)) in bufs.iter().zip([1, 2, 3]).enumerate() {
        let want = fill_scanout_nv12(a, seed);
        let serial = 20 + serial as u32;
        conn.present_surface(PresentSurface {
            id: SURF,
            buffer: a.id,
            serial,
            src: full(),
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            damage: vec![],
        })
        .unwrap();
        conn.flush().unwrap();
        presented(&mut conn, &mut seen, serial);
        h.settle();
        assert_eq!(rgb(&grab(&h, &c, SIDE, SIDE)), rgb(&want), "buffer {seed}");
    }

    // DestroyBuffer frees it (the one not on screen).
    conn.tx().destroy_buffer(bufs[0].id).commit(30).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 30);
    assert_eq!(h.stat("scanout_buffers"), 2);
    assert_eq!(h.stat("scanout_buffer_bytes"), 2 * per);
    // The id is free again for the client's own use.
    alloc(&mut conn, 10, 1, format::YUYV, SIDE, SIDE);
    let y = allocated(&mut conn, &mut seen, 10, 1);
    assert_eq!(
        (y[0].format, y[0].stride0, y[0].offset1),
        (format::YUYV, 64, 0)
    );
    assert_eq!(h.stat("scanout_buffers"), 3);

    // Disconnect cleanup.
    drop(conn);
    wait_for("the scanout buffers to go", || {
        h.stat("scanout_buffers") == 0
    });
    assert_eq!(h.stat("scanout_buffer_bytes"), 0);
    h.quit();
}

#[test]
fn refusals_are_not_fatal() {
    let h = Harness::start("scanout-refuse", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "refuse");
    window(&mut conn, &mut seen, &[], (SIDE as f32, SIDE as f32));
    alloc(&mut conn, 10, 1, format::NV12, 8193, 16);
    assert_eq!(refused(&mut conn, &mut seen), AllocRefusal::TooBig);
    alloc(&mut conn, 10, 1, format::fourcc(b"RG16"), 16, 16);
    assert_eq!(refused(&mut conn, &mut seen), AllocRefusal::Format);
    alloc(&mut conn, 10, 1, format::UYVY, 16, 16);
    assert_eq!(refused(&mut conn, &mut seen), AllocRefusal::Format);
    // 4 × 4096² XR24 = 256 MiB: past the per-client byte cap.
    alloc(&mut conn, 10, 4, format::XR24, 4096, 4096);
    assert_eq!(refused(&mut conn, &mut seen), AllocRefusal::Limit);
    // Past the buffer-count cap: 30 of its own, then 4 more.
    let mut tx = conn.tx();
    let own: Vec<Buf> = (100..130).map(|i| Buf::new(i, format::NV12, 1)).collect();
    for b in &own {
        tx = tx.create_surface_buffer(b.create());
    }
    tx.commit(2).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 2);
    alloc(&mut conn, 10, 4, format::NV12, 16, 16);
    assert_eq!(refused(&mut conn, &mut seen), AllocRefusal::Limit);
    assert_eq!(h.stat("scanout_buffers"), 0);
    // And the connection survived all of it.
    alloc(&mut conn, 10, 2, format::NV12, 16, 16);
    allocated(&mut conn, &mut seen, 10, 2);
    h.quit();
}

fn fatal(h: &Harness, name: &str, req: impl FnOnce(&mut Connection)) -> ErrorCode {
    let (mut conn, mut seen) = surface_client(h, name);
    window(
        &mut conn,
        &mut seen,
        &[&Buf::new(7, format::NV12, 1)],
        (16.0, 16.0),
    );
    req(&mut conn);
    let _ = conn.flush();
    expect(&mut conn, &mut seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    })
}

#[test]
fn malformed_requests_are_fatal() {
    let h = Harness::start("scanout-fatal", 60_000);
    for count in [0, 5] {
        let code = fatal(&h, "count", |c| alloc(c, 10, count, format::NV12, 16, 16));
        assert_eq!(code, ErrorCode::Protocol, "count {count}");
    }
    // Id 7 is already a buffer; id 0 is NONE.
    let code = fatal(&h, "dup", |c| alloc(c, 6, 2, format::NV12, 16, 16));
    assert_eq!(code, ErrorCode::BadBuffer);
    let code = fatal(&h, "zero", |c| alloc(c, 0, 1, format::NV12, 16, 16));
    assert_eq!(code, ErrorCode::BadBuffer);
    // Not a Surface.
    let code = fatal(&h, "kind", |c| {
        c.alloc_surface_buffers(AllocSurfaceBuffers {
            node: ROOT,
            first_id: BufferId(10),
            count: 1,
            format: 0,
            width: 16,
            height: 16,
        })
        .unwrap();
    });
    assert_eq!(code, ErrorCode::WrongKind);
    // Unknown node.
    let code = fatal(&h, "unknown", |c| {
        c.alloc_surface_buffers(AllocSurfaceBuffers {
            node: NodeId(99),
            first_id: BufferId(10),
            count: 1,
            format: 0,
            width: 16,
            height: 16,
        })
        .unwrap();
    });
    assert_eq!(code, ErrorCode::UnknownNode);

    // Without `SURFACE` in ClientCaps.
    let mut conn = h.client("nocap");
    let mut seen = Vec::new();
    alloc(&mut conn, 10, 1, format::NV12, 16, 16);
    let (code, msg) = expect(&mut conn, &mut seen, "Error", |m| match m {
        ServerMsg::Error(e) => Some((e.code, e.msg.clone())),
        _ => None,
    });
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("SURFACE"), "{msg}");
    h.quit();
}

#[test]
fn defaults_come_from_the_planes_and_the_hint() {
    use nitro_kms::{FakePlaneSpec, Fourcc};
    // No YUV plane: XR24, at the hinted 30×21.
    let h = Harness::start("scanout-default", 60_000);
    let (mut conn, mut seen) = surface_client(&h, "default");
    window(&mut conn, &mut seen, &[], (30.0, 21.0));
    alloc(&mut conn, 10, 1, 0, 0, 0);
    let a = allocated(&mut conn, &mut seen, 10, 1);
    assert_eq!(
        (a[0].format, a[0].width, a[0].height),
        (format::XR24, 30, 21)
    );
    h.quit();

    // A YUYV overlay (HSW's shape): YUYV, width rounded up to even.
    let planes = vec![
        FakePlaneSpec::default_primary(),
        FakePlaneSpec::overlay().formats(&[Fourcc::XRGB8888, Fourcc::YUYV]),
    ];
    let h = Harness::start_with("scanout-yuyv", 60_000, planes);
    let (mut conn, mut seen) = surface_client(&h, "yuyv");
    window(&mut conn, &mut seen, &[], (31.0, 21.0));
    alloc(&mut conn, 10, 1, 0, 0, 0);
    let a = allocated(&mut conn, &mut seen, 10, 1);
    assert_eq!(
        (a[0].format, a[0].width, a[0].height),
        (format::YUYV, 32, 21)
    );
    h.quit();

    // NV12 listed: NV12 wins, both dimensions rounded up to even.
    let planes = vec![
        FakePlaneSpec::default_primary(),
        FakePlaneSpec::overlay().formats(&[Fourcc::YUYV, Fourcc::NV12]),
    ];
    let h = Harness::start_with("scanout-nv12", 60_000, planes);
    let (mut conn, mut seen) = surface_client(&h, "nv12");
    window(&mut conn, &mut seen, &[], (31.0, 21.0));
    alloc(&mut conn, 10, 1, 0, 40, 0);
    let a = allocated(&mut conn, &mut seen, 10, 1);
    assert_eq!(
        (a[0].format, a[0].width, a[0].height),
        (format::NV12, 40, 22)
    );
    h.quit();
}
