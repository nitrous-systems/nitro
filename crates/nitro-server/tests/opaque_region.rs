//! `SetOpaqueRegion` (#3877) end to end on the fake backend: the cap is
//! advertised, the op is refused without it in `ClientCaps`, refused on a
//! non-image node, and — with it — the server paints the region without
//! looking at the source alpha (proved by a client that lies). Since #3919
//! the same holds for an AR24 buffer presented on a `Surface` node (the
//! Chromium GPU process's window).

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Rect, Size};
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, CreateBuffer, Fill, PresentSurface, ServerMsg};
use nitro_wire::types::{
    BufferId, ColorMatrix, ColorRange, ErrorCode, Layer, NodeId, caps, format, window_flags,
};

const OUT: (u32, u32) = (200, 150);
const SIDE: u32 = 16;
/// `SIDE` as the `i32` that `IRect` wants.
const SIDE_I32: i32 = 16;
const _: () = assert!(SIDE_I32 as i64 == SIDE as i64);

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
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str) -> Self {
        Self::start_scaled(name, None)
    }

    /// As [`Harness::start`], with the output at `scale` if given.
    fn start_scaled(name: &str, scale: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-opq-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: OUT.0,
            height: OUT.1,
        };
        if let Some(scale) = scale {
            config.scales = nitro_server::parse_scales(&format!("Virtual-1={scale}"));
        }
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

    /// The shadow buffer as `(stride, bytes)`.
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

/// Poll until `f` matches; a closed connection after an `Error` is fine.
fn expect<T>(conn: &mut Connection, what: &str, f: impl Fn(&ServerMsg) -> Option<T>) -> T {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = seen.iter().find_map(&f) {
            return found;
        }
        assert!(Instant::now() < deadline, "no {what}; got {seen:?}");
        let _ = conn.flush();
        if let Err(e) = conn.poll(&mut seen) {
            if let Some(found) = seen.iter().find_map(&f) {
                return found;
            }
            panic!("{what}: {e}; got {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn protocol_error(conn: &mut Connection) -> (ErrorCode, String) {
    expect(conn, "Error", |m| match m {
        ServerMsg::Error(e) => Some((e.code, e.msg.clone())),
        _ => None,
    })
}

/// A window with a SIDE×SIDE AR24 image at its origin, over a white rect.
/// Every pixel is pure red at alpha 0: a straight-alpha blend shows the
/// white behind it; an opaque copy shows red.
fn window(conn: &mut Connection) -> (NodeId, Configure) {
    let (root, back, image) = (NodeId(1), NodeId(2), NodeId(3));
    let stride = SIDE * 4;
    let pixels: Vec<u8> = (0..stride * SIDE)
        .map(|i| if i % 4 == 2 { 0xff } else { 0 })
        .collect();
    let fd = nitro_shm::memfd_with("nitro-opaque-region", &pixels).unwrap();
    let r = Rect::new(0.0, 0.0, SIDE as f32, SIDE as f32);
    conn.tx()
        .create_window(root, "opq", Size::new(32.0, 32.0), Layer::Normal)
        .create_rect(back, root, r)
        .fill(back, Fill::Solid(Color::WHITE))
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: SIDE,
            height: SIDE,
            stride,
            format: format::AR24,
            size: stride * SIDE,
            fd,
        })
        .create_image(image, root, r)
        .image(image, BufferId(1), IRect::new(0, 0, 16, 16))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(conn, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    expect(conn, "Presented", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 1).then_some(())
    });
    (image, c)
}

fn pixel(h: &Harness, win: &Configure, px: u32, py: u32) -> [u8; 3] {
    let (stride, data) = h.shot();
    let o = ((win.position.y as u32 + py) * stride + (win.position.x as u32 + px) * 4) as usize;
    [data[o], data[o + 1], data[o + 2]]
}

#[test]
fn the_cap_is_advertised_and_the_region_skips_the_alpha() {
    let h = Harness::start("paint");
    let mut conn = h.client("opq");
    assert_ne!(conn.caps() & caps::OPAQUE_REGION, 0);
    conn.client_caps(caps::OPAQUE_REGION).unwrap();
    let (image, c) = window(&mut conn);
    // Alpha 0 everywhere: white shows through.
    assert_eq!(pixel(&h, &c, 4, 4), [0xff, 0xff, 0xff]);
    // Declare the left half opaque (buffer px): the client lied, so red
    // shows there and white stays on the right.
    conn.tx()
        .opaque_region(image, vec![IRect::new(0, 0, 8, 16)])
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, "Presented 2", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 2).then_some(())
    });
    assert_eq!(pixel(&h, &c, 4, 4), [0, 0, 0xff], "copied, alpha ignored");
    assert_eq!(pixel(&h, &c, 12, 4), [0xff, 0xff, 0xff], "still blended");
    // An empty list clears it.
    conn.tx().opaque_region(image, vec![]).commit(3).unwrap();
    conn.flush().unwrap();
    expect(&mut conn, "Presented 3", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 3).then_some(())
    });
    assert_eq!(pixel(&h, &c, 4, 4), [0xff, 0xff, 0xff]);
    h.quit();
}

#[test]
fn without_the_cap_listed_it_is_a_protocol_error() {
    let h = Harness::start("nocap");
    let mut conn = h.client("old");
    // An old client: no `ClientCaps` at all.
    let (image, _) = window(&mut conn);
    conn.tx()
        .opaque_region(image, vec![IRect::new(0, 0, 8, 8)])
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    let (code, msg) = protocol_error(&mut conn);
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("OPAQUE_REGION"), "{msg}");
    h.quit();
}

#[test]
fn on_a_node_that_is_neither_image_nor_surface_it_is_wrong_kind() {
    let h = Harness::start("kind");
    let mut conn = h.client("opq");
    conn.client_caps(caps::OPAQUE_REGION).unwrap();
    let _ = window(&mut conn);
    conn.tx()
        .opaque_region(NodeId(2), vec![IRect::new(0, 0, 8, 8)])
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    assert_eq!(protocol_error(&mut conn).0, ErrorCode::WrongKind);
    h.quit();
}

#[test]
fn on_a_surface_node_the_region_skips_the_alpha_too() {
    let h = Harness::start("surface");
    let mut conn = h.client("opq-surface");
    conn.client_caps(caps::OPAQUE_REGION | caps::SURFACE)
        .unwrap();
    let (root, back, surf) = (NodeId(1), NodeId(2), NodeId(3));
    let stride = SIDE * 4;
    let pixels: Vec<u8> = (0..stride * SIDE)
        .map(|i| if i % 4 == 2 { 0xff } else { 0 })
        .collect();
    let fd = nitro_shm::memfd_with("nitro-opaque-surface", &pixels).unwrap();
    let r = Rect::new(0.0, 0.0, SIDE as f32, SIDE as f32);
    conn.tx()
        .create_window(root, "opq", Size::new(32.0, 32.0), Layer::Normal)
        .create_rect(back, root, r)
        .fill(back, Fill::Solid(Color::WHITE))
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: SIDE,
            height: SIDE,
            stride,
            format: format::AR24,
            size: stride * SIDE,
            fd,
        })
        .create_surface(surf, root, r)
        .opaque_region(surf, vec![IRect::new(0, 0, 8, 16)])
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(&mut conn, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    expect(&mut conn, "Presented 1", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 1).then_some(())
    });
    // Through the latch, as the GPU process presents.
    conn.present_surface(PresentSurface {
        id: surf,
        buffer: BufferId(1),
        serial: 2,
        src: IRect::new(0, 0, SIDE_I32, SIDE_I32),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Full,
        damage: vec![],
    })
    .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, "Presented 2", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 2).then_some(())
    });
    assert_eq!(pixel(&h, &c, 4, 4), [0, 0, 0xff], "copied, alpha ignored");
    assert_eq!(pixel(&h, &c, 12, 4), [0xff, 0xff, 0xff], "still blended");
    h.quit();
}

/// #3940: at scale 1.25 a window whose buffer is its device size — bounds
/// `px / 1.25` — is drawn 1:1, so its declared opaque region is copied. The
/// window's root is snapped to a whole device pixel, wherever the WM put it.
#[test]
fn at_a_fractional_scale_a_device_sized_buffer_is_copied() {
    let h = Harness::start_scaled("frac", Some("1.25"));
    let mut conn = h.client("opq-frac");
    conn.client_caps(caps::OPAQUE_REGION | caps::SURFACE)
        .unwrap();
    let (root, back, image, surf) = (NodeId(1), NodeId(2), NodeId(3), NodeId(4));
    let stride = SIDE * 4;
    let pixels: Vec<u8> = (0..stride * SIDE)
        .map(|i| if i % 4 == 2 { 0xff } else { 0 })
        .collect();
    let fd = nitro_shm::memfd_with("nitro-opaque-frac", &pixels).unwrap();
    let l = SIDE as f32 / 1.25;
    conn.tx()
        // Undecorated, as Chromium is: the content is the root.
        .create_window_with(
            root,
            "opq",
            Size::new(3.0 * l, l),
            Layer::Normal,
            window_flags::UNDECORATED,
        )
        .create_rect(back, root, Rect::new(0.0, 0.0, 3.0 * l, l))
        .fill(back, Fill::Solid(Color::WHITE))
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: SIDE,
            height: SIDE,
            stride,
            format: format::AR24,
            size: stride * SIDE,
            fd,
        })
        .create_image(image, root, Rect::new(0.0, 0.0, l, l))
        .image(image, BufferId(1), IRect::new(0, 0, SIDE_I32, SIDE_I32))
        .opaque_region(image, vec![IRect::new(0, 0, SIDE_I32, SIDE_I32)])
        .create_surface(surf, root, Rect::new(2.0 * l, 0.0, l, l))
        .opaque_region(surf, vec![IRect::new(0, 0, SIDE_I32, SIDE_I32)])
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(&mut conn, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    expect(&mut conn, "Presented 1", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 1).then_some(())
    });
    conn.present_surface(PresentSurface {
        id: surf,
        buffer: BufferId(1),
        serial: 2,
        src: IRect::new(0, 0, SIDE_I32, SIDE_I32),
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Full,
        damage: vec![],
    })
    .unwrap();
    conn.flush().unwrap();
    expect(&mut conn, "Presented 2", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 2).then_some(())
    });
    assert!((c.scale - 1.25).abs() < 1e-6, "scale {}", c.scale);
    let (stride, data) = h.shot();
    let ox = (c.position.x * 1.25).round() as u32;
    let oy = (c.position.y * 1.25).round() as u32;
    let px = |x: u32, y: u32| {
        let o = ((oy + y) * stride + (ox + x) * 4) as usize;
        [data[o], data[o + 1], data[o + 2]]
    };
    // Every pixel of both, edges included: a scaled blend would show white
    // (alpha 0) or a mix at the edges.
    for y in 0..SIDE {
        for x in 0..SIDE {
            assert_eq!(px(x, y), [0, 0, 0xff], "image ({x},{y})");
            assert_eq!(px(2 * SIDE + x, y), [0, 0, 0xff], "surface ({x},{y})");
        }
    }
    // Between them, the white rect.
    assert_eq!(px(SIDE + 4, 4), [0xff, 0xff, 0xff]);
    h.quit();
}

/// Gap G11 (#4003): at scale 1.25 a **decorated** window's content is
/// drawn 1:1 too. The frame insets are snapped to whole device pixels (the
/// 1 px border is 0.8 logical px, one device pixel), so the content — not
/// just the window root — lands on the device grid. Before the snap it sat
/// at `x + 1.25` device px and every pixel of the buffer was resampled.
#[test]
fn a_decorated_windows_content_is_blitted_1to1_at_1_25() {
    let h = Harness::start_scaled("frac-ssd", Some("1.25"));
    let mut conn = h.client("opq-frac-ssd");
    let (root, image) = (NodeId(1), NodeId(2));
    let stride = SIDE * 4;
    // A per-pixel distinct, opaque pattern: any resampling mixes
    // neighbours and shows.
    let mut pixels = vec![0u8; (stride * SIDE) as usize];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let o = (y * stride + x * 4) as usize;
            pixels[o] = ((x ^ y) * 7) as u8;
            pixels[o + 1] = (y * 17) as u8;
            pixels[o + 2] = (x * 13) as u8;
            pixels[o + 3] = 0xff;
        }
    }
    let fd = nitro_shm::memfd_with("nitro-opaque-frac-ssd", &pixels).unwrap();
    let l = SIDE as f32 / 1.25;
    conn.tx()
        // Decorated: the server draws a frame around it.
        .create_window(root, "ssd", Size::new(4.0 * l, 4.0 * l), Layer::Normal)
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: SIDE,
            height: SIDE,
            stride,
            format: format::AR24,
            size: stride * SIDE,
            fd,
        })
        .create_image(image, root, Rect::new(0.0, 0.0, l, l))
        .image(image, BufferId(1), IRect::new(0, 0, SIDE_I32, SIDE_I32))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(&mut conn, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    expect(&mut conn, "Presented 1", |m| {
        matches!(m, ServerMsg::Presented(p) if p.serial == 1).then_some(())
    });
    assert!((c.scale - 1.25).abs() < 1e-6, "scale {}", c.scale);
    // The window root may sit at a fractional logical position; the scene
    // snaps it to the device grid (`root_placement`). With whole-device
    // insets the content's device origin is then `round(position * s)`
    // exactly — without them it is a quarter pixel off that.
    let (dev_x, dev_y) = (c.position.x * 1.25, c.position.y * 1.25);
    let (stride_out, data) = h.shot();
    let ox = dev_x.round() as u32;
    let oy = dev_y.round() as u32;
    for y in 0..SIDE {
        for x in 0..SIDE {
            let o = ((oy + y) * stride_out + (ox + x) * 4) as usize;
            let src = (y * stride + x * 4) as usize;
            assert_eq!(
                [data[o], data[o + 1], data[o + 2]],
                [pixels[src], pixels[src + 1], pixels[src + 2]],
                "pixel ({x},{y})"
            );
        }
    }
    h.quit();
}
