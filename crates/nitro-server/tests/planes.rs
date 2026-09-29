//! Planes (#3899) end to end on the fake backend with a Haswell-shaped and
//! a Kaby-Lake-shaped plane inventory: a server-allocated YUYV/NV12
//! Surface that covers the output goes on a plane (direct scanout) after
//! the hysteresis, its later frames flip without painting, anything drawn
//! over it puts it back on the CPU at once with a full repaint, a plane
//! buffer is released only after the flip that replaces it, and
//! destroying the Surface while on a plane leaves the default layout.

#![allow(clippy::many_single_char_names)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Rect, Size};
use nitro_kms::{ColorEncoding, ColorRange as KmsRange, FakePlaneSpec, Fourcc};
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{AllocSurfaceBuffers, PresentSurface, ServerMsg, SurfaceBufferAllocated};
use nitro_wire::types::{
    BufferId, ColorMatrix, ColorRange, Layer, NodeId, WindowState, caps, format,
};

const OUT: (u32, u32) = (320, 240);
const ROOT: NodeId = NodeId(1);
const SURF: NodeId = NodeId(2);
const COVER: NodeId = NodeId(3);

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn hsw() -> Vec<FakePlaneSpec> {
    vec![
        FakePlaneSpec::default_primary().zpos(0, 0, 0, true),
        FakePlaneSpec::overlay()
            .formats(&[Fourcc::XRGB8888, Fourcc::YUYV, Fourcc::UYVY])
            .zpos(1, 1, 1, true)
            .color(
                &[ColorEncoding::Bt601, ColorEncoding::Bt709],
                &[KmsRange::Limited, KmsRange::Full],
            ),
    ]
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
        .color(
            &[ColorEncoding::Bt601, ColorEncoding::Bt709],
            &[KmsRange::Limited, KmsRange::Full],
        )
    };
    vec![
        p(FakePlaneSpec::primary()).zpos(0, 0, 0, true),
        p(FakePlaneSpec::overlay()).zpos(1, 1, 1, true),
    ]
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, planes: Vec<FakePlaneSpec>) -> Self {
        Self::start_at(name, planes, 60_000)
    }

    fn start_at(name: &str, planes: Vec<FakePlaneSpec>, mhz: u32) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-planes-{}-{name}", std::process::id()));
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

    /// `samples paint`: total ever painted frames.
    fn paints(&self) -> u64 {
        let lines = self.request_text("samples paint\n");
        lines[0].strip_prefix("ok ").unwrap().parse().unwrap()
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

/// A fullscreen window whose only content is Surface `SURF` over the
/// whole output, with `count` server-allocated buffers in `fmt`.
fn fullscreen_video(
    h: &Harness,
    name: &str,
    fmt: u32,
    count: u8,
) -> (Connection, Vec<ServerMsg>, Vec<SurfaceBufferAllocated>) {
    let mut conn = Connection::connect(&h.wire_path, name).expect("wire connect");
    assert!(conn.has_caps(caps::SURFACE));
    conn.client_caps(caps::SURFACE | caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    let (w, hh) = (OUT.0 as f32, OUT.1 as f32);
    conn.tx()
        .create_window(ROOT, name, Size::new(w, hh), Layer::Normal)
        .create_surface(SURF, ROOT, Rect::new(0.0, 0.0, w, hh))
        .set_window_state(ROOT, WindowState::Fullscreen)
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "fullscreen", |m| match m {
        ServerMsg::WindowState(s) if s.state == WindowState::Fullscreen => Some(()),
        _ => None,
    });
    presented(&mut conn, &mut seen, 1);
    conn.alloc_surface_buffers(AllocSurfaceBuffers {
        node: SURF,
        first_id: BufferId(10),
        count,
        format: fmt,
        width: OUT.0,
        height: OUT.1,
    })
    .unwrap();
    conn.flush().unwrap();
    let last = BufferId(10 + u32::from(count) - 1);
    expect(&mut conn, &mut seen, "SurfaceBufferAllocated", |m| {
        matches!(m, ServerMsg::SurfaceBufferAllocated(a) if a.id == last).then_some(())
    });
    let mut bufs = Vec::new();
    let mut rest = Vec::new();
    for m in seen.drain(..) {
        match m {
            ServerMsg::SurfaceBufferAllocated(a) => bufs.push(a),
            other => rest.push(other),
        }
    }
    seen = rest;
    bufs.sort_by_key(|a| a.id.raw());
    (conn, seen, bufs)
}

fn present(conn: &mut Connection, id: BufferId, serial: u32) {
    conn.present_surface(PresentSurface {
        id: SURF,
        buffer: id,
        serial,
        src: IRect::new(0, 0, OUT.0.cast_signed(), OUT.1.cast_signed()),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        damage: vec![],
    })
    .unwrap();
    conn.flush().unwrap();
}

/// Present frames round-robin, one per `Presented`, until `until` holds.
fn play(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    bufs: &[SurfaceBufferAllocated],
    serial: &mut u32,
    what: &str,
    mut until: impl FnMut() -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !until() {
        assert!(Instant::now() < deadline, "timed out playing until {what}");
        let b = &bufs[*serial as usize % bufs.len()];
        present(conn, b.id, *serial);
        presented(conn, seen, *serial);
        *serial += 1;
    }
}

#[test]
fn fullscreen_video_scans_out_directly_and_flips_without_painting() {
    let h = Harness::start("direct", hsw());
    let (mut conn, mut seen, bufs) = fullscreen_video(&h, "direct", 0, 3);
    // The server's choice on this inventory: YUYV, the overlay's format.
    assert_eq!(bufs[0].format, format::YUYV);
    let mut serial = 2;
    play(&mut conn, &mut seen, &bufs, &mut serial, "direct", || {
        h.stat("planes_mode") == 3
    });
    assert_eq!(h.stat("planes_in_use"), 1);

    // Steady state: frames flip on the plane, nothing is painted and no
    // TEST_ONLY is asked.
    let (paints, flips, tests) = (h.paints(), h.stat("plane_flips"), h.stat("planes_tests"));
    let frames = h.stat("frames");
    let mut n = 0;
    play(
        &mut conn,
        &mut seen,
        &bufs,
        &mut serial,
        "10 frames",
        || {
            n += 1;
            n > 10
        },
    );
    assert!(h.stat("plane_flips") >= flips + 10);
    assert!(h.stat("frames") >= frames + 10);
    assert_eq!(h.paints(), paints, "no paint for a video frame on a plane");
    assert_eq!(h.stat("planes_tests"), tests, "cached");
    assert_eq!(h.stat("planes_mode"), 3);

    // Something over it (controls): composite in the next frame, and the
    // output buffer is repainted in full.
    let damage_before = h.request_text("samples damage\n")[0].clone();
    conn.tx()
        .create_rect(COVER, ROOT, Rect::new(10.0, 200.0, 100.0, 20.0))
        .fill_solid(COVER, Color::WHITE)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, serial);
    serial += 1;
    wait_for("composite", || h.stat("planes_mode") == 0);
    assert!(h.paints() > paints);
    let damage = h.request_text("samples damage\n");
    assert_ne!(damage[0], damage_before);
    let full = u64::from(OUT.0 * OUT.1);
    assert!(
        damage[1..]
            .iter()
            .any(|l| l.parse::<u64>().unwrap() == full),
        "a full repaint: {damage:?}"
    );

    // Controls gone: back on the plane after the hysteresis.
    conn.tx().destroy_node(COVER).commit(serial).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, serial);
    serial += 1;
    play(
        &mut conn,
        &mut seen,
        &bufs,
        &mut serial,
        "direct again",
        || h.stat("planes_mode") == 3,
    );
    h.quit();
}

#[test]
fn a_plane_buffer_is_released_after_the_flip_that_replaces_it() {
    let h = Harness::start_at("release", hsw(), 10_000);
    let (mut conn, mut seen, bufs) = fullscreen_video(&h, "release", format::YUYV, 3);
    let mut serial = 2;
    play(&mut conn, &mut seen, &bufs, &mut serial, "direct", || {
        h.stat("planes_mode") == 3
    });
    // A is on screen; B replaces it. At 10 Hz the replacing flip is in
    // flight for ~100 ms after B latches: A must be held back for that
    // whole window, and released with the flip.
    seen.clear();
    let a = &bufs[serial as usize % 3];
    present(&mut conn, a.id, serial);
    presented(&mut conn, &mut seen, serial);
    serial += 1;
    let b = &bufs[serial as usize % 3];
    present(&mut conn, b.id, serial);
    wait_for("A held back", || h.stat("plane_releases_held") == 1);
    let _ = conn.poll(&mut seen);
    assert!(
        released_at(&seen, a.id).is_none(),
        "released while still scanned out: {seen:?}"
    );
    let p = presented(&mut conn, &mut seen, serial);
    expect(&mut conn, &mut seen, "the release of A", |m| {
        matches!(m, ServerMsg::BufferReleased(r) if r.id == a.id).then_some(())
    });
    let r = released_at(&seen, a.id).unwrap();
    assert_eq!(r + 1, p, "released with the replacing flip: {seen:?}");
    assert_eq!(h.stat("plane_releases_held"), 0);
    h.quit();
}

#[test]
fn destroy_and_disconnect_on_a_plane_return_to_the_default() {
    let h = Harness::start("destroy", hsw());
    let (mut conn, mut seen, bufs) = fullscreen_video(&h, "destroy", format::YUYV, 2);
    let mut serial = 2;
    play(&mut conn, &mut seen, &bufs, &mut serial, "direct", || {
        h.stat("planes_mode") == 3
    });
    conn.tx().destroy_node(SURF).commit(serial).unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, serial);
    wait_for("composite", || h.stat("planes_mode") == 0);
    assert_eq!(h.stat("planes_in_use"), 0);
    drop(conn);
    wait_for("the first client's buffers to go", || {
        h.stat("scanout_buffers") == 0
    });

    let (mut conn, mut seen, bufs) = fullscreen_video(&h, "vanish", format::YUYV, 2);
    let mut serial = 2;
    play(&mut conn, &mut seen, &bufs, &mut serial, "direct", || {
        h.stat("planes_mode") == 3
    });
    drop(conn);
    wait_for("composite after disconnect", || h.stat("planes_mode") == 0);
    wait_for("the scanout buffers to go", || {
        h.stat("scanout_buffers") == 0
    });
    assert!(h.stat("frames") > 0);
    h.quit();
}

#[test]
fn nv12_goes_on_the_primary_on_kbl_and_the_hint_follows_the_planes() {
    let h = Harness::start("kbl", kbl());
    let (mut conn, mut seen, bufs) = fullscreen_video(&h, "kbl", 0, 3);
    assert_eq!(bufs[0].format, format::NV12);
    let hint = expect(&mut conn, &mut seen, "SurfaceHint", |m| match m {
        ServerMsg::SurfaceHint(s) => Some(*s),
        _ => None,
    });
    assert_eq!(hint.format, format::NV12);
    let mut serial = 2;
    play(&mut conn, &mut seen, &bufs, &mut serial, "direct", || {
        h.stat("planes_mode") == 3
    });
    h.quit();

    // HSW: YUYV.
    let h = Harness::start("hsw-hint", hsw());
    let (mut conn, mut seen, _) = fullscreen_video(&h, "hsw", 0, 1);
    let hint = expect(&mut conn, &mut seen, "SurfaceHint", |m| match m {
        ServerMsg::SurfaceHint(s) => Some(*s),
        _ => None,
    });
    assert_eq!(hint.format, format::YUYV);
    h.quit();
}

#[test]
fn an_unobscured_window_goes_on_the_overlay_above_the_ui() {
    let h = Harness::start("window", hsw());
    let mut conn = Connection::connect(&h.wire_path, "window").expect("wire connect");
    conn.client_caps(caps::SURFACE | caps::RELEASE).unwrap();
    let mut seen = Vec::new();
    conn.tx()
        .create_window(ROOT, "window", Size::new(160.0, 120.0), Layer::Normal)
        .create_surface(SURF, ROOT, Rect::new(0.0, 0.0, 160.0, 120.0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    presented(&mut conn, &mut seen, 1);
    conn.alloc_surface_buffers(AllocSurfaceBuffers {
        node: SURF,
        first_id: BufferId(10),
        count: 2,
        format: 0,
        width: 0,
        height: 0,
    })
    .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, &mut seen, "SurfaceBufferAllocated", |m| {
        matches!(m, ServerMsg::SurfaceBufferAllocated(a) if a.id == BufferId(11)).then_some(())
    });
    let mut bufs = Vec::new();
    for m in std::mem::take(&mut seen) {
        if let ServerMsg::SurfaceBufferAllocated(a) = m {
            bufs.push(a);
        }
    }
    bufs.sort_by_key(|a| a.id.raw());
    // The hinted size: the node's device rect, 1:1 for the plane.
    assert_eq!(
        (bufs[0].format, bufs[0].width, bufs[0].height),
        (format::YUYV, 160, 120)
    );
    let mut serial = 2;
    let deadline = Instant::now() + Duration::from_secs(15);
    while h.stat("planes_mode") != 1 {
        assert!(Instant::now() < deadline, "no overlay");
        conn.present_surface(PresentSurface {
            id: SURF,
            buffer: bufs[serial as usize % 2].id,
            serial,
            src: IRect::new(0, 0, 160, 120),
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
            damage: vec![],
        })
        .unwrap();
        conn.flush().unwrap();
        presented(&mut conn, &mut seen, serial);
        serial += 1;
    }
    assert_eq!(h.stat("planes_in_use"), 1);
    h.quit();
}
