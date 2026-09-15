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

use nitro_core::{Color, Rect, Role, Size};
use nitro_kms::Image;
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ErrorCode, Layer, NodeId, caps};

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
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, conf: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-icons-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(&config_path, conf).expect("write server.conf");

        let mut config = Config::fake(OUT.0, OUT.1, &path);
        config.config_path = Some(config_path.clone());
        let wire_path = config.wire_path.clone();
        let shell_path = config.shell_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            config_dir,
            config_path,
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
        .create_window_with(root, "icons", WIN, Layer::Normal, 0)
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
