//! Symbolic icons, driven end to end through the real event loop: a
//! `SetIcon` puts ink on the screen, a scheme flip recolours it with no
//! client message, an unknown name is a **non-fatal** error, and a 2×
//! output rasterises a real 2× icon rather than blowing up a 1× one.
//!
//! The shape is `tests/theme.rs`': the real [`run`](nitro_server::run) on
//! a thread with the fake backend, real `nitro-wire` clients, the v0
//! control socket, and a configuration directory of its own.

// The geometry is whole-pixel arithmetic on whole-pixel inputs, so
// equality means what it says.
#![allow(clippy::float_cmp)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Palette, Rect, Role, Size};
use nitro_kms::Image;
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ErrorCode, Layer, NodeId, caps, window_flags};

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
/// A flat backdrop, so any non-background pixel in the icon's box is ink.
const BACKDROP: Color = Color::rgb(0x00, 0x00, 0x00);

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
    icon_dir: PathBuf,
    desktop_dir: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, conf: &str) -> Self {
        Self::start_with_icons(name, conf, false)
    }

    /// A harness whose icon search path is a fixture tree of its own,
    /// populated by [`Harness::install_app_icon`].
    ///
    /// Hermetic on purpose: an application-icon test that used the real
    /// `/usr/share/icons` would assert about whatever theme the machine
    /// running it happens to have, which is a test that passes on the
    /// author's box and fails in CI — or, worse, the other way round.
    fn start_with_app_icons(name: &str, conf: &str) -> Self {
        Self::start_with_icons(name, conf, true)
    }

    fn start_with_icons(name: &str, conf: &str, app_icons: bool) -> Self {
        Self::start_full(name, conf, app_icons, &[])
    }

    /// A harness with an icon fixture tree **and** a `.desktop` fixture
    /// directory holding `(basename, Icon=)` entries.
    ///
    /// The third resolution step (#3715) reads files the distribution
    /// wrote, so it gets a fixture directory for exactly the reason the
    /// icon tree has one: a test pointed at the box's own
    /// `/usr/share/applications` would resolve `nitro-calc` on a
    /// packager's machine and nowhere else.
    fn start_with_desktop(name: &str, conf: &str, entries: &[(&str, &str)]) -> Self {
        Self::start_full(name, conf, true, entries)
    }

    fn start_full(name: &str, conf: &str, app_icons: bool, entries: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-icons-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(&config_path, conf).expect("write server.conf");
        let icon_dir = dir.join("icons");
        let desktop_dir = dir.join("applications");

        let mut config = Config::fake(OUT.0, OUT.1, &path);
        config.config_path = Some(config_path.clone());
        if app_icons {
            std::fs::create_dir_all(icon_dir.join("hicolor")).expect("icon dir");
            std::fs::write(
                icon_dir.join("hicolor").join("index.theme"),
                "[Icon Theme]\n\
                 Directories=16x16/apps,24x24/apps,32x32/apps,48x48/apps\n\
                 [16x16/apps]\nSize=16\nType=Fixed\n\
                 [24x24/apps]\nSize=24\nType=Fixed\n\
                 [32x32/apps]\nSize=32\nType=Fixed\n\
                 [48x48/apps]\nSize=48\nType=Fixed\n",
            )
            .expect("index.theme");
            config.icon_dirs = Some(vec![icon_dir.clone()]);
        }
        std::fs::create_dir_all(&desktop_dir).expect("applications dir");
        for (basename, icon) in entries {
            std::fs::write(
                desktop_dir.join(format!("{basename}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName={basename}\nIcon={icon}\n"),
            )
            .expect("a desktop entry");
        }
        config.desktop_dirs = Some(vec![desktop_dir.clone()]);
        let wire_path = config.wire_path.clone();
        let shell_path = config.shell_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            config_dir,
            config_path,
            icon_dir,
            desktop_dir,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        wait_for("the shell socket", || shell_path.exists());
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

    fn rewrite_config(&self, conf: &str) {
        let tmp = self.config_dir.join("server.conf.tmp");
        std::fs::write(&tmp, conf).expect("write temp");
        std::fs::rename(&tmp, &self.config_path).expect("rename into place");
    }

    /// Put a `side × side` PNG of one solid colour into the harness's
    /// fixture theme, as `hicolor/<side>x<side>/apps/<name>.png`.
    ///
    /// The colour is given in `0xrrggbb`, which is how the readback
    /// reports it, so a test compares what it installed with what it sees
    /// without reordering anything in its head.
    fn install_app_icon(&self, name: &str, side: u32, rgb: u32) {
        let dir = self
            .icon_dir
            .join("hicolor")
            .join(format!("{side}x{side}"))
            .join("apps");
        std::fs::create_dir_all(&dir).expect("the apps directory");
        std::fs::write(dir.join(format!("{name}.png")), solid_png(side, rgb)).expect("the icon");
    }

    /// Write a `.desktop` entry into the harness's applications
    /// directory after the server has started.
    ///
    /// The index is built at start and again on `reload`, so an entry
    /// installed here is invisible until a `reload` happens — which is
    /// exactly the property
    /// `a_reload_rescans_the_desktop_index` is about.
    fn install_desktop_entry(&self, basename: &str, icon: &str) {
        std::fs::write(
            self.desktop_dir.join(format!("{basename}.desktop")),
            format!("[Desktop Entry]\nType=Application\nName={basename}\nIcon={icon}\n"),
        )
        .expect("a desktop entry");
    }

    fn shot(&self) -> Image {
        let mut c = self.connect();
        c.get_mut().write_all(b"shot\n").unwrap();
        let mut status = String::new();
        c.read_line(&mut status).unwrap();
        let mut words = status.split_ascii_whitespace();
        assert_eq!(words.next(), Some("ok"), "{status:?}");
        let w: u32 = words.next().unwrap().parse().unwrap();
        let h: u32 = words.next().unwrap().parse().unwrap();
        let stride: u32 = words.next().unwrap().parse().unwrap();
        let mut data = vec![0u8; (stride * h) as usize];
        c.read_exact(&mut data).unwrap();
        Image {
            width: w,
            height: h,
            stride,
            data,
        }
    }

    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let lines = self.request_text("stats\n");
            let value = |key: &str| -> u64 {
                lines
                    .iter()
                    .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let frames = value("frames");
            if value("flips_pending") == 0 && frames == last {
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
        assert_eq!(line, "ok\n");
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
        if let Some(found) = seen.iter().rev().find_map(&f) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "no {what}; got {} msgs",
            seen.len()
        );
        conn.flush().unwrap();
        conn.poll(seen).unwrap_or_else(|e| panic!("{what}: {e}"));
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A window with a flat backdrop and one icon node in its top-left
/// corner, returning the window root and the `Configure` that placed it.
///
/// **Undecorated**, and that is load-bearing since #3715: a decorated
/// frame draws its own icons — the app icon and three button glyphs — so
/// a counter like `icon_renders` would answer about four icons plus the
/// one under test. These tests are about the *client's* icon, so the
/// frame is opted out of rather than subtracted, which would be a
/// magic number that silently goes stale the day the frame changes.
/// `crates/nitro-server/tests/wm.rs` is where the frame's own icons are
/// counted.
fn window_with_icon(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    name: &str,
    size: f32,
) -> (NodeId, nitro_wire::msg::Configure) {
    let root = NodeId(1);
    let back = NodeId(2);
    let icon = NodeId(3);
    conn.tx()
        .create_window_with(root, "icons", WIN, Layer::Normal, window_flags::UNDECORATED)
        .create_rect(back, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(back, BACKDROP)
        .create_icon(icon, root, Rect::new(0.0, 0.0, size, size))
        .set_icon(icon, name, size, Role::Text.index() as u8)
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "the first Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    (root, c)
}

/// The icon box's pixels in **device** space, as `0x00rrggbb`.
///
/// `Configure.position` is logical, so the device origin is the position
/// times the output scale — which is exactly the thing this file is about
/// and exactly the thing a test of it must not get wrong.
fn icon_box(img: &Image, c: &nitro_wire::msg::Configure, logical_size: u32) -> Vec<u32> {
    let ox = (c.position.x * c.scale) as u32;
    let oy = (c.position.y * c.scale) as u32;
    let size = (logical_size as f32 * c.scale) as u32;
    let mut px = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            px.push(img.pixel(ox + x, oy + y) & 0x00ff_ffff);
        }
    }
    px
}

/// Pixels in the box that are neither the backdrop nor fully saturated —
/// the anti-aliased edge, which is what tells a real raster from a blit.
fn edge_pixels(px: &[u32], background: u32, foreground: u32) -> usize {
    px.iter()
        .filter(|p| **p != background && **p != foreground)
        .count()
}

/// A `side × side` opaque RGBA PNG of one colour, given `0xrrggbb`.
///
/// Written here rather than checked in as a binary fixture: a test that
/// reads a `.png` from the repository asserts about a file nobody can
/// read in a diff, and every byte of this one is validated by the real
/// decoder the moment the server loads it. Stored deflate blocks and
/// unfiltered scanlines — it is a fixture writer, not a compressor.
fn solid_png(side: u32, rgb: u32) -> Vec<u8> {
    let (r, g, b) = (
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    );
    let mut raw = Vec::new();
    for _ in 0..side {
        // Filter byte 0 ("none"), then the row.
        raw.extend_from_slice(&[0u8]);
        for _ in 0..side {
            raw.extend_from_slice(&[r, g, b, 0xff]);
        }
    }
    let mut zlib = vec![0x78u8, 0x01];
    for (i, block) in raw.chunks(65_535).enumerate() {
        zlib.push(u8::from((i + 1) * 65_535 >= raw.len()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&side.to_be_bytes());
    // depth 8, colour type 6 (RGBA), deflate, adaptive filtering, no
    // interlace: the shape every icon in every theme is written in.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &zlib);
    chunk(b"IEND", &[]);
    out
}

/// Adler-32, for [`solid_png`]'s zlib wrapper.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += u32::from(x);
            b += a;
        }
        a %= 65_521;
        b %= 65_521;
    }
    (b << 16) | a
}

/// CRC-32 as PNG defines it, for [`solid_png`]'s chunks.
fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &byte in data {
        c ^= u32::from(byte);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    c ^ 0xFFFF_FFFF
}

/// A window with a flat backdrop and one **coloured** application icon
/// node, the `AS_COLOURED` twin of [`window_with_icon`]. Undecorated for
/// the same reason.
fn window_with_app_icon(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    name: &str,
    size: f32,
) -> (NodeId, nitro_wire::msg::Configure) {
    let root = NodeId(1);
    let back = NodeId(2);
    let icon = NodeId(3);
    conn.tx()
        .create_window_with(root, "icons", WIN, Layer::Normal, window_flags::UNDECORATED)
        .create_rect(back, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(back, BACKDROP)
        .create_icon(icon, root, Rect::new(0.0, 0.0, size, size))
        .set_icon(icon, name, size, nitro_wire::msg::SetIcon::AS_COLOURED)
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let c = expect(conn, seen, "the first Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    (root, c)
}

#[test]
fn a_coloured_application_icon_is_painted_in_its_own_colours() {
    // The whole point of `AS_COLOURED`, settled on pixels: the tile that
    // reaches the screen is the file's colour, not a palette role's. The
    // discriminator is the colour itself — `#c86414` is in no palette, so
    // a server that tinted a coverage mask could not produce it.
    let h = Harness::start_with_app_icons("coloured", "");
    h.install_app_icon("testapp", 48, 0x00c8_6414);
    let mut seen = Vec::new();
    let mut conn = h.client("coloured");
    let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "testapp", 24.0);
    h.settle();

    let px = icon_box(&h.shot(), &c, 24);
    let hits = px.iter().filter(|p| **p == 0x00c8_6414).count();
    assert!(
        hits > 400,
        "only {hits} pixels of the icon's own colour in a 24x24 box"
    );
    // The symbolic side is untouched: no mask was rasterised for this.
    assert_eq!(h.stat("icon_renders"), 0);
    assert_eq!(h.stat("app_icon_loads"), 1, "decoded exactly once");
    assert_eq!(h.stat("app_icons_cached"), 1);
    assert_eq!(h.stat("app_icon_bytes"), 24 * 24 * 4);
    assert_eq!(h.stat("app_icon_misses"), 0);

    // And a settled desktop decodes nothing further, however much it
    // repaints — the "never per frame" half of the lazy-decode claim.
    let frames = h.stat("frames");
    for i in 0..6u32 {
        conn.tx()
            .bounds(NodeId(3), Rect::new(i as f32, i as f32, 24.0, 24.0))
            .commit(10 + i)
            .unwrap();
        conn.flush().unwrap();
        h.settle();
    }
    assert!(h.stat("frames") >= frames + 6, "it really did repaint");
    assert_eq!(h.stat("app_icon_loads"), 1);

    drop(conn);
    h.quit();
}

#[test]
fn an_application_name_the_theme_lacks_is_a_bad_icon_and_the_client_survives() {
    // `BadIcon` covers both sets, on the same terms: a name the machine's
    // theme does not have is a gap, not a disconnect — which is what a
    // client's `.fallback(…)` depends on being told about.
    let h = Harness::start_with_app_icons("coloured-missing", "");
    h.install_app_icon("testapp", 48, 0x0011_2233);
    let mut seen = Vec::new();
    let mut conn = h.client("coloured-missing");
    let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "no-such-application", 24.0);

    let code = expect(&mut conn, &mut seen, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadIcon);
    h.settle();
    let px = icon_box(&h.shot(), &c, 24);
    let back = u32::from(BACKDROP.r) << 16 | u32::from(BACKDROP.g) << 8 | u32::from(BACKDROP.b);
    assert!(px.iter().all(|p| *p == back), "it drew something anyway");
    assert_eq!(h.stat("app_icon_loads"), 0);

    // The fallback a client would send next is honoured on the same
    // connection, which is the property that makes `.fallback(…)` work.
    conn.tx()
        .set_icon(
            NodeId(3),
            "testapp",
            24.0,
            nitro_wire::msg::SetIcon::AS_COLOURED,
        )
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("clients"), 1, "the client is still connected");
    assert_eq!(h.stat("app_icon_loads"), 1, "and its fallback drew");

    drop(conn);
    h.quit();
}

#[test]
fn a_symbolic_name_is_not_an_application_name_and_the_reverse() {
    // The namespace decision, from the outside: the role byte picks the
    // set and nothing falls back between them. `gear` is ours and the
    // fixture theme does not have it; `testapp` is the theme's and the
    // symbolic set does not have it. Each is a `BadIcon` in the other's
    // role, which is what makes `icon("list")` mean the same thing on
    // every box.
    let h = Harness::start_with_app_icons("namespaces", "");
    h.install_app_icon("testapp", 32, 0x0044_8866);
    let mut seen = Vec::new();
    let mut conn = h.client("namespaces");
    let (_root, _c) = window_with_app_icon(&mut conn, &mut seen, "gear", 16.0);
    let code = expect(&mut conn, &mut seen, "an Error for gear", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadIcon, "a symbolic name is not an app");
    h.settle();
    assert_eq!(h.stat("app_icon_loads"), 0);
    assert_eq!(h.stat("icon_renders"), 0);

    seen.clear();
    conn.tx()
        .set_icon(NodeId(3), "testapp", 16.0, Role::Text.index() as u8)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    let code = expect(&mut conn, &mut seen, "an Error for testapp", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(
        code,
        ErrorCode::BadIcon,
        "a theme name tinted by a role is refused rather than silhouetted"
    );
    h.settle();
    assert_eq!(h.stat("icon_renders"), 0);
    assert_eq!(h.stat("clients"), 1);

    drop(conn);
    h.quit();
}

#[test]
fn a_two_times_output_reads_the_bigger_source_for_a_coloured_icon() {
    // The scale claim for application icons, which is a *different* claim
    // from the symbolic one: there is no rasteriser here, so being crisp
    // means reading the file the theme ships for that size rather than
    // blowing up a smaller one. The theme has 24 and 48; a 24-logical
    // icon on a 2× output is 48 device px and must take the 48 px file.
    //
    // Two servers rather than a reload, matching
    // `a_two_times_output_rasterises_a_real_two_times_icon`: the scale is
    // read at start-up and a fresh process is the honest way to change
    // the one variable under test.
    let shot_at = |name: &str, conf: &str| -> (Vec<u32>, u64, u64) {
        let h = Harness::start_with_app_icons(name, conf);
        h.install_app_icon("testapp", 24, 0x0011_2233);
        h.install_app_icon("testapp", 48, 0x00cc_4422);
        let mut seen = Vec::new();
        let mut conn = h.client(name);
        let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "testapp", 24.0);
        h.settle();
        let px = icon_box(&h.shot(), &c, 24);
        let (bytes, loads) = (h.stat("app_icon_bytes"), h.stat("app_icon_loads"));
        drop(conn);
        h.quit();
        (px, bytes, loads)
    };

    let (one, one_bytes, one_loads) = shot_at("coloured-scale1", "");
    assert_eq!(one_loads, 1);
    assert_eq!(one_bytes, 24 * 24 * 4, "one 24 device px tile");
    assert!(
        one.iter().filter(|p| **p == 0x0011_2233).count() > 400,
        "scale 1 took something other than the 24 px file"
    );

    let (two, two_bytes, two_loads) = shot_at("coloured-scale2", "output.Virtual-1.scale = 2\n");
    assert_eq!(two_loads, 1);
    assert_eq!(two_bytes, 48 * 48 * 4, "one 48 device px tile");
    // The *colour* is the discriminator, not the byte count: a doubled
    // 24 px tile would also be 48²×4 bytes, but it could not contain a
    // colour that is only in the 48 px file.
    let big = two.iter().filter(|p| **p == 0x00cc_4422).count();
    assert!(
        big > 1600,
        "only {big} pixels of the 48 px source at scale 2; a scaled 24 px \
         tile could not contain that colour at all"
    );
    assert_eq!(
        one.iter().filter(|p| **p == 0x00cc_4422).count(),
        0,
        "and the scale-1 shot has none of it, so the two really differ"
    );
}

#[test]
fn the_icons_capability_is_advertised_and_an_icon_puts_ink_on_the_screen() {
    // The base claim, settled on **pixels**: a name and a role go over
    // the wire and something is drawn. `stats icons_cached 1` would be
    // satisfied by a server that rasterised an icon and never blitted
    // it, which is exactly the failure worth catching.
    let h = Harness::start("ink", "");
    let mut seen = Vec::new();
    let mut conn = h.client("ink");
    assert_ne!(conn.caps() & caps::ICONS, 0, "the ICONS bit is advertised");
    let (_root, c) = window_with_icon(&mut conn, &mut seen, "gear", 16.0);
    h.settle();

    let px = icon_box(&h.shot(), &c, 16);
    let back = u32::from(BACKDROP.r) << 16 | u32::from(BACKDROP.g) << 8 | u32::from(BACKDROP.b);
    let ink = px.iter().filter(|p| **p != back).count();
    assert!(ink > 20, "only {ink} non-background pixels in the icon box");

    assert_eq!(h.stat("icons_cached"), 1);
    assert_eq!(h.stat("icon_renders"), 1);
    assert_eq!(h.stat("icon_bytes"), 16 * 16);

    drop(conn);
    h.quit();
}

#[test]
fn an_unknown_icon_name_is_reported_and_the_client_survives() {
    // The one property a missing icon must have: it is a gap, not a
    // disconnect. Every other error in this protocol is fatal, so this
    // is the case that would be got wrong by default.
    let h = Harness::start("unknown", "");
    let mut seen = Vec::new();
    let mut conn = h.client("unknown");
    let (_root, c) = window_with_icon(&mut conn, &mut seen, "no-such-icon-anywhere", 16.0);

    let code = expect(&mut conn, &mut seen, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadIcon);
    h.settle();

    // The node draws nothing: the box is pure backdrop.
    let px = icon_box(&h.shot(), &c, 16);
    let back = u32::from(BACKDROP.r) << 16 | u32::from(BACKDROP.g) << 8 | u32::from(BACKDROP.b);
    assert!(
        px.iter().all(|p| *p == back),
        "an unknown icon drew something"
    );
    assert_eq!(h.stat("icon_renders"), 0, "and rasterised nothing either");

    // And the connection still works: a further commit is applied and
    // answered, which is the whole point of the error being non-fatal.
    assert_eq!(h.stat("clients"), 1);
    conn.tx()
        .set_icon(NodeId(3), "gear", 16.0, Role::Text.index() as u8)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("clients"), 1, "the client is still connected");
    assert_eq!(h.stat("icon_renders"), 1, "and its next icon drew");

    drop(conn);
    h.quit();
}

#[test]
fn flipping_the_scheme_recolours_an_icon_with_no_client_message_and_no_re_raster() {
    // The theme claim, and the reason the cache holds coverage rather
    // than tinted pixels: the icon's pixels move, the client sent
    // nothing, and `icon_renders` does not budge.
    let h = Harness::start("scheme", "");
    let mut seen = Vec::new();
    let mut conn = h.client("scheme");
    let (_root, c) = window_with_icon(&mut conn, &mut seen, "gear", 16.0);
    h.settle();

    let before = icon_box(&h.shot(), &c, 16);
    let renders = h.stat("icon_renders");
    let reloads = h.stat("config_reloads");
    let frames = h.stat("frames");

    h.rewrite_config("theme.scheme = dark\n");
    // The state being measured has to have actually happened: a `sed`
    // the watch missed would make every assertion below a tautology.
    wait_for("the config reload", || h.stat("config_reloads") > reloads);
    h.settle();

    let after = icon_box(&h.shot(), &c, 16);
    assert_ne!(before, after, "the icon did not follow the scheme");
    assert_eq!(
        h.stat("icon_renders"),
        renders,
        "a recolour must cost no raster: the cache is coverage, not pixels"
    );
    // The same frame budget as text: a scheme switch is a repaint, not a
    // round trip per icon.
    let spent = h.stat("frames") - frames;
    assert!(spent <= 4, "the switch took {spent} frames");
    // And the client said nothing at all in between.
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "no error was raised: {seen:?}"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_two_times_output_rasterises_a_real_two_times_icon() {
    // The crispness claim, settled on the count of anti-aliased edge
    // pixels rather than on a description. A 2× blit of the 1× tile
    // would have exactly four times as many intermediate pixels; a real
    // 2× raster has fewer, because the artwork's edges are curves whose
    // length grows by 2 and not by 4.
    let h = Harness::start("scale1", "");
    let mut seen = Vec::new();
    let mut conn = h.client("scale1");
    let (_root, c) = window_with_icon(&mut conn, &mut seen, "circle-fill", 16.0);
    h.settle();
    let one = icon_box(&h.shot(), &c, 16);
    assert_eq!(h.stat("icon_bytes"), 16 * 16);
    drop(conn);
    h.quit();

    let h = Harness::start("scale2", "output.Virtual-1.scale = 2\n");
    let mut seen = Vec::new();
    let mut conn = h.client("scale2");
    let (_root, c) = window_with_icon(&mut conn, &mut seen, "circle-fill", 16.0);
    h.settle();
    // At scale 2 the 16-logical-pixel box is 32 device pixels.
    let two = icon_box(&h.shot(), &c, 16);
    assert_eq!(
        h.stat("icon_bytes"),
        32 * 32,
        "the mask was rasterised at the device size, not the logical one"
    );

    let back = u32::from(BACKDROP.r) << 16 | u32::from(BACKDROP.g) << 8 | u32::from(BACKDROP.b);
    // The icon is drawn in `Role::Text`, which the light scheme puts at
    // a single saturated value; whatever it is, it is the most common
    // non-background pixel in the box.
    let fg = |px: &[u32]| {
        let mut counts = std::collections::HashMap::new();
        for p in px.iter().filter(|p| **p != back) {
            *counts.entry(*p).or_insert(0u32) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(p, _)| p)
            .expect("the icon drew something")
    };
    let e1 = edge_pixels(&one, back, fg(&one));
    let e2 = edge_pixels(&two, back, fg(&two));
    assert!(e2 > e1, "a bigger icon has a longer edge: {e2} vs {e1}");
    assert!(
        e2 < 4 * e1,
        "a 2x blit of the 16 px tile would have {} edge pixels; a real \
         32 px raster has {e2}",
        4 * e1
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_settled_desktop_re_rasterises_nothing() {
    // The idle contract, in the counter that would say so: an icon is
    // static, so after the first paint `icon_renders` must never move
    // again, however many frames go past.
    let h = Harness::start("idle", "");
    let mut seen = Vec::new();
    let mut conn = h.client("idle");
    let (_root, _c) = window_with_icon(&mut conn, &mut seen, "list", 16.0);
    h.settle();
    let renders = h.stat("icon_renders");
    assert_eq!(renders, 1);
    let frames = h.stat("frames");

    // Move the icon around inside its window: every step repaints it,
    // and not one of them may rasterise it again.
    for i in 0..10u32 {
        conn.tx()
            .bounds(NodeId(3), Rect::new(i as f32, i as f32, 16.0, 16.0))
            .commit(10 + i)
            .unwrap();
        conn.flush().unwrap();
        h.settle();
    }
    assert!(
        h.stat("frames") >= frames + 10,
        "the icon really did repaint: {} frames for 10 moves",
        h.stat("frames") - frames
    );
    assert_eq!(h.stat("icon_renders"), renders);
    assert_eq!(h.stat("icons_cached"), 1);

    drop(conn);
    h.quit();
}

// ---------------------------------------------------------------------
// #3715: the third lookup step — app_id → .desktop → Icon=
// ---------------------------------------------------------------------

/// An app id the theme has never heard of resolves through its
/// `.desktop` file to one of the server's **own** symbolic shapes, and
/// is drawn tinted.
///
/// This is the defect the step exists for, end to end through the real
/// server: `nitro-calc` is an app id, `calculator` is the shape, and the
/// only thing on the machine that ties them together is
/// `deploy/nitro-calc.desktop`. Before #3715 the bar showed the generic
/// `window` glyph for every one of our own applications.
#[test]
fn an_app_id_resolves_through_a_desktop_entry_to_a_symbolic_icon() {
    let h = Harness::start_with_desktop("indirect", "", &[("nitro-calc", "calculator")]);
    let mut seen = Vec::new();
    let mut conn = h.client("indirect");
    // `AS_COLOURED`, as `nitro-bar` sends for a window-list button: the
    // client asks for the app id and knows nothing about `.desktop`
    // files, which is the whole point of putting the hop in the server.
    let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "nitro-calc", 16.0);
    h.settle();

    // No `BadIcon`: the name resolved, so the client's fallback never
    // had to fire.
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "the indirected name was refused: {seen:?}"
    );

    // And it is drawn **tinted**, not blitted: the pixels are the
    // palette's `text`, which is what a coverage mask composited over
    // the backdrop gives and what a coloured PNG could not.
    let px = icon_box(&h.shot(), &c, 16);
    let back = u32::from(BACKDROP.r) << 16 | u32::from(BACKDROP.g) << 8 | u32::from(BACKDROP.b);
    let text = Palette::default().get(Role::Text);
    let tint = u32::from(text.r) << 16 | u32::from(text.g) << 8 | u32::from(text.b);
    let ink = px.iter().filter(|p| **p != back).count();
    assert!(ink > 20, "only {ink} px of ink in the icon box");
    assert!(
        px.contains(&tint),
        "the indirected icon is not painted in a palette role, so it was \
         not resolved to the symbolic set"
    );

    // The counters say which path answered, which is the half a pixel
    // test cannot: a symbolic answer rasterises a mask and decodes no
    // file at all.
    assert_eq!(h.stat("icon_renders"), 1);
    assert_eq!(h.stat("app_icon_loads"), 0, "nothing was decoded");
    assert_eq!(h.stat("desktop_entries"), 1);
    assert_eq!(h.stat("app_icon_indirections"), 1);

    drop(conn);
    h.quit();
}

/// The same hop landing in the icon **theme** instead: the `Icon=` names
/// a PNG, so the tile is blitted in its own colours and `AS_COLOURED`
/// survives all the way to the node.
#[test]
fn an_indirected_name_that_is_a_theme_png_keeps_its_own_colours() {
    let h = Harness::start_with_desktop(
        "indirect-png",
        "",
        &[("org.example.Browser", "browser-art")],
    );
    h.install_app_icon("browser-art", 48, 0x00c8_6414);
    let mut seen = Vec::new();
    let mut conn = h.client("indirect-png");
    let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "org.example.Browser", 24.0);
    h.settle();

    let px = icon_box(&h.shot(), &c, 24);
    let hits = px.iter().filter(|p| **p == 0x00c8_6414).count();
    assert!(
        hits > 400,
        "only {hits} px of the file's own colour: a tinted mask, not a blit"
    );
    assert_eq!(h.stat("icon_renders"), 0, "nothing symbolic was rasterised");
    assert_eq!(h.stat("app_icon_loads"), 1, "decoded exactly once");
    assert_eq!(h.stat("app_icon_indirections"), 1);

    drop(conn);
    h.quit();
}

/// The theme still wins, the hop is one deep, and a name with neither is
/// still a `BadIcon`.
///
/// Three rules in one server because each is a *negative*: the cheapest
/// way to get them wrong is to have no test at all, and the cheapest way
/// to have one is to put them where a fixture already exists.
#[test]
fn the_desktop_hop_is_one_deep_and_never_shadows_the_theme() {
    let h = Harness::start_with_desktop(
        "indirect-rules",
        "",
        &[
            // The theme has `direct`, so the entry must not be consulted:
            // a `.desktop` is what answers a name the theme could not,
            // never an override of one it could.
            ("direct", "wrong-art"),
            // `a` names `b`, and `b` is itself only an entry. One hop, so
            // the chain stops and `a` is refused.
            ("a", "b"),
            ("b", "direct-art"),
        ],
    );
    h.install_app_icon("direct", 32, 0x0011_2233);
    h.install_app_icon("direct-art", 32, 0x0044_5566);
    h.install_app_icon("wrong-art", 32, 0x0077_8899);
    let mut seen = Vec::new();
    let mut conn = h.client("indirect-rules");
    let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "direct", 32.0);
    h.settle();
    let px = icon_box(&h.shot(), &c, 32);
    assert!(
        px.iter().filter(|p| **p == 0x0011_2233).count() > 600,
        "the .desktop entry shadowed the theme's own file"
    );
    assert_eq!(h.stat("app_icon_indirections"), 0, "no hop was needed");

    // `a` needs two hops, which is one more than there is.
    seen.clear();
    conn.tx()
        .set_icon(NodeId(3), "a", 32.0, nitro_wire::msg::SetIcon::AS_COLOURED)
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    let code = expect(&mut conn, &mut seen, "an Error for a", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadIcon, "the hop recursed");

    // And a name with neither an entry nor a file is refused as before.
    seen.clear();
    conn.tx()
        .set_icon(
            NodeId(3),
            "no-such-anything",
            32.0,
            nitro_wire::msg::SetIcon::AS_COLOURED,
        )
        .commit(3)
        .unwrap();
    conn.flush().unwrap();
    let code = expect(&mut conn, &mut seen, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadIcon);
    assert_eq!(h.stat("clients"), 1, "and the client survived both");

    drop(conn);
    h.quit();
}

/// A `reload` re-scans the `.desktop` index, so an application installed
/// while the desktop is running gets its icon.
///
/// The index is built once at start, which is right — a `read_dir` per
/// unresolved name on the commit path would be filesystem work on the
/// client's first-paint latency — but that makes `reload` the only
/// moment it can notice a package arriving. Before the reload the name
/// is a `BadIcon`, which is the honest answer rather than a blank node.
#[test]
fn a_reload_rescans_the_desktop_index() {
    let h = Harness::start_with_desktop("indirect-reload", "", &[]);
    let mut seen = Vec::new();
    let mut conn = h.client("indirect-reload");
    let (_root, c) = window_with_app_icon(&mut conn, &mut seen, "nitro-settings", 16.0);
    let code = expect(&mut conn, &mut seen, "an Error", |m| match m {
        ServerMsg::Error(e) => Some(e.code),
        _ => None,
    });
    assert_eq!(code, ErrorCode::BadIcon, "nothing answers it yet");
    assert_eq!(h.stat("desktop_entries"), 0);

    // The package lands, and the user reloads.
    h.install_desktop_entry("nitro-settings", "gear");
    let reloads = h.stat("config_reloads");
    h.rewrite_config("# touched\n");
    // The state being measured has to have actually happened: a write
    // the watch missed would make every assertion below a tautology.
    wait_for("the config reload", || h.stat("config_reloads") > reloads);
    h.settle();
    assert_eq!(h.stat("desktop_entries"), 1, "the index was re-scanned");

    // The same name, re-sent as the client's next repaint would: now it
    // resolves, and it draws.
    seen.clear();
    conn.tx()
        .set_icon(
            NodeId(3),
            "nitro-settings",
            16.0,
            nitro_wire::msg::SetIcon::AS_COLOURED,
        )
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "still refused after the reload: {seen:?}"
    );
    let px = icon_box(&h.shot(), &c, 16);
    let back = u32::from(BACKDROP.r) << 16 | u32::from(BACKDROP.g) << 8 | u32::from(BACKDROP.b);
    let ink = px.iter().filter(|p| **p != back).count();
    assert!(ink > 20, "only {ink} px of ink after the reload");
    assert_eq!(h.stat("app_icon_indirections"), 1);

    drop(conn);
    h.quit();
}
