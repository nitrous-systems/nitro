//! The heap shadow buffer (#539), driven through the real event loop on
//! the fake backend.
//!
//! What is under test is not the rasterizer — that is `frame.rs`'s and
//! `nitro-raster`'s business — but the *bookkeeping* around it: that the
//! server can rasterize `damage(n)` alone into the shadow and still put
//! `damage(n) ∪ damage(n-1)` into the age-2 back buffer, frame after
//! frame, without ever leaving a stale pixel behind.
//!
//! Every assertion here is therefore an equality between two whole
//! buffers, not a spot check on a pixel: a shadow that drifts from what a
//! full repaint would produce is exactly the bug this file exists to
//! catch, and it can drift anywhere.
//!
//! The fake backend is heap memory itself, so the shadow buys nothing here
//! — it is a pure extra copy. It stays on by default anyway, so these
//! tests exercise the path that ships; `NITRO_SHADOW=0` is compared
//! against it rather than replacing it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_kms::Image;
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::{BackendKind, Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{Configure, ServerMsg};
use nitro_wire::types::{Layer, NodeId};

const OUT: (u32, u32) = (240, 160);

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
    input: FakeInput,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    /// A server on the fake backend with the shadow on (`shadow: true`) or
    /// off, which is what `NITRO_SHADOW=0` sets. The flag is a `Config`
    /// field rather than an environment variable precisely so two servers
    /// can differ on it inside one test process.
    fn start(name: &str, shadow: bool) -> Self {
        Self::start_with(name, shadow, true, OUT)
    }

    /// As [`Harness::start`], choosing the scroll blit (`NITRO_SCROLL_BLIT`)
    /// and the output size too.
    fn start_with(name: &str, shadow: bool, scroll_blit: bool, out: (u32, u32)) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-shadow-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: out.0,
            height: out.1,
        };
        config.shadow = shadow;
        config.scroll_blit = scroll_blit;
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            input,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        // The cursor is drawn into the same buffers as everything else, so
        // park it out of the way and keep it there: an arrow that happened
        // to sit in different places in two servers would make every
        // comparison below fail for the wrong reason.
        h.input.push(InputEvent::PointerAbsolute {
            x: 0.99,
            y: 0.99,
            time_ns: 1_000_000,
        });
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

    /// A screenshot the way any client takes one: out of the shadow when
    /// there is one.
    fn shot(&self) -> Image {
        self.pixels("shot\n")
    }

    /// A screenshot of the buffer the display actually scans out, whatever
    /// the shadow holds. This is the one that can catch a bad copy.
    fn front(&self) -> Image {
        self.pixels("shot-front\n")
    }

    fn pixels(&self, req: &str) -> Image {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let header = header.trim_end();
        let fields: Vec<u32> = header
            .strip_prefix("ok ")
            .unwrap_or_else(|| panic!("{req:?} failed: {header}"))
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let (width, height, stride) = (fields[0], fields[1], fields[2]);
        let mut data = vec![0u8; (stride * height) as usize];
        c.read_exact(&mut data).unwrap();
        Image {
            width,
            height,
            stride,
            data,
        }
    }

    fn frames(&self) -> u64 {
        self.stat("frames")
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

    fn request_line(&self, req: &str) -> String {
        let mut c = self.connect();
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
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

impl Drop for Harness {
    fn drop(&mut self) {
        // A panicking test still has to stop its server, or the next one
        // inherits a live thread on the same socket directory. The reply
        // is read before the connection goes: the server answers `ok` and
        // only then tears the loop down, and hanging up first races that
        // against its own hangup handling.
        if let Some(t) = self.thread.take() {
            if let Ok(c) = UnixStream::connect(&self.path) {
                let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
                let mut c = BufReader::new(c);
                if c.get_mut().write_all(b"quit\n").is_ok() {
                    let mut line = String::new();
                    let _ = c.read_line(&mut line);
                }
            }
            let _ = t.join();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
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

/// A window with one solid rect filling it, committed and configured.
fn window(h: &Harness, conn: &mut Connection, size: Size, color: Color) -> Configure {
    let root = NodeId(1);
    let rect = NodeId(2);
    conn.tx()
        .create_window(root, "shadow", size, Layer::Normal)
        .create_rect(rect, root, Rect::new(0.0, 0.0, size.w, size.h))
        .fill_solid(rect, color)
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    let mut seen = Vec::new();
    let c = expect(conn, &mut seen, "Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    });
    h.settle();
    c
}

/// Repaint the window's rect in a new colour and wait for it to land.
///
/// One small rect per step, deliberately: the interesting case is a long
/// run of *partial* damages, where a shadow that lost a pixel has nothing
/// to bring it back.
fn recolour(h: &Harness, conn: &mut Connection, _size: Size, color: Color, serial: u32) {
    conn.tx()
        .fill_solid(NodeId(2), color)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
}

/// The whole image, so a mismatch is reported as "these two buffers
/// differ", not as one unlucky pixel.
fn assert_same(a: &Image, b: &Image, what: &str) {
    assert_eq!((a.width, a.height), (b.width, b.height), "{what}: size");
    let mut differing = 0u32;
    let mut first = None;
    for y in 0..a.height {
        for x in 0..a.width {
            if a.pixel(x, y) != b.pixel(x, y) {
                differing += 1;
                first.get_or_insert((x, y, a.pixel(x, y), b.pixel(x, y)));
            }
        }
    }
    assert_eq!(
        differing, 0,
        "{what}: {differing} pixel(s) differ, first {first:?}"
    );
}

/// After a run of partial damages the scanout buffer holds exactly what a
/// full repaint would have put there.
///
/// This is the headline property. Each `recolour` damages one rect, the
/// server rasterizes only that rect into the shadow, and streams the
/// age-2 union out of it; if any of those steps dropped or misplaced a
/// row, the buffer diverges from a freshly-repainted one and never
/// recovers.
///
/// The reference is the *same* server forced to repaint everything, so
/// nothing but the incremental path differs between the two images.
/// Plugging a second output is what forces it: every rescan invalidates
/// the outputs it kept, which is the "both buffers hold unknown pixels"
/// path, and it leaves the first output's geometry and its window exactly
/// where they were.
#[test]
fn partial_damage_leaves_the_scanout_buffer_byte_identical_to_a_full_repaint() {
    let h = Harness::start("partial", true);
    let mut conn = h.client("painter");
    let size = Size::new(96.0, 64.0);
    window(&h, &mut conn, size, Color::rgb(0x20, 0x20, 0x20));

    let colours = [
        Color::rgb(0xFF, 0x00, 0x00),
        Color::rgb(0x00, 0xFF, 0x00),
        Color::rgb(0x00, 0x00, 0xFF),
        Color::rgb(0xFF, 0xFF, 0x00),
        Color::rgb(0x40, 0x80, 0xC0),
    ];
    for (i, c) in colours.iter().enumerate() {
        recolour(&h, &mut conn, size, *c, 10 + i as u32);
    }
    let incremental = h.front();

    assert_eq!(h.request_line(&format!("plug {}x{}\n", OUT.0, OUT.1)), "ok");
    h.settle();
    assert_eq!(h.stat("outputs"), 2);
    let full = h.front();

    assert_same(
        &incremental,
        &full,
        "five partial damages vs a full repaint",
    );
    h.quit();
}

/// Damage from frame `n-1` reaches the back buffer at frame `n+1`: the
/// age-2 rule, still true with the shadow in the way.
///
/// The check is against the *scanout* buffer specifically. The shadow is
/// complete from the moment it is painted, so a `shot` cannot tell whether
/// the copy out of it covered the older damage as well — only the buffer
/// the display scans can.
#[test]
fn the_age_2_carry_still_reaches_the_second_buffer() {
    let h = Harness::start("age2", true);
    let mut conn = h.client("painter");
    let size = Size::new(96.0, 64.0);
    let c = window(&h, &mut conn, size, Color::rgb(0x20, 0x20, 0x20));

    let red = Color::rgb(0xFF, 0x00, 0x00);
    recolour(&h, &mut conn, size, red, 20);

    // Both buffers now hold the red window. Confirm on the one on screen,
    // then damage something *else* — far from the window — and let it
    // settle. If the age-2 carry were dropped from the copy, the frame
    // that lands the new damage would take the buffer that still had the
    // *old* window colour in it and put it on screen.
    let (wx, wy) = (c.position.x as u32 + 4, c.position.y as u32 + 4);
    assert_eq!(h.front().pixel(wx, wy), 0x00FF_0000, "the window is red");

    let blue = Color::rgb(0x00, 0x00, 0xFF);
    recolour(&h, &mut conn, size, blue, 21);
    // Two frames land it in both buffers; check the one on screen after
    // each of them, so a carry lost on the second frame is visible.
    let front = h.front();
    assert_eq!(front.pixel(wx, wy), 0x0000_00FF, "and now blue");

    // A frame that changes nothing must not disturb either buffer: the
    // shadow rasterizes an empty region, the copy an empty union.
    let before = h.frames();
    h.settle();
    assert_eq!(h.frames(), before, "a settled server makes no frames");
    assert_same(&h.front(), &front, "an idle frame changed the buffer");

    h.quit();
}

/// `NITRO_SHADOW=0` and the default produce the same picture.
///
/// Two servers, same scene, same synthetic input, compared pixel for
/// pixel. This is what makes the A/B on the box a measurement of *speed*
/// and nothing else: if the two ever disagreed, the faster one would just
/// be wrong faster.
#[test]
fn the_shadow_and_the_direct_path_paint_the_same_pixels() {
    let size = Size::new(96.0, 64.0);
    let colour = Color::rgb(0x30, 0x90, 0x60);

    let with = Harness::start("with", true);
    let mut c1 = with.client("painter");
    window(&with, &mut c1, size, colour);
    recolour(&with, &mut c1, size, Color::rgb(0xC0, 0x30, 0x30), 30);
    let shadowed = with.front();
    assert!(with.stat("shadow_bytes") > 0, "the shadow is on");

    let without = Harness::start("without", false);
    let mut c2 = without.client("painter");
    window(&without, &mut c2, size, colour);
    recolour(&without, &mut c2, size, Color::rgb(0xC0, 0x30, 0x30), 30);
    let direct = without.front();
    assert_eq!(without.stat("shadow_bytes"), 0, "NITRO_SHADOW=0");

    assert_same(&shadowed, &direct, "shadow vs NITRO_SHADOW=0");

    // And a `shot` — which reads the shadow when there is one — agrees
    // with the buffer the display scans. That is what keeps `nitro-shot`
    // honest.
    assert_same(&with.shot(), &shadowed, "shot vs the scanout buffer");
    assert_same(&without.shot(), &direct, "shot with no shadow");

    with.quit();
    without.quit();
}

/// The shadow's memory is accounted for, appears with the output and goes
/// away with it.
#[test]
fn removing_an_output_frees_its_shadow() {
    let h = Harness::start("lifecycle", true);
    h.settle();
    let one = h.stat("shadow_bytes");
    assert_eq!(
        one,
        u64::from(OUT.0 * OUT.1 * 4),
        "one output's worth of XRGB8888"
    );
    assert_eq!(h.stat("outputs"), 1);

    // A second output doubles it...
    assert_eq!(h.request_line(&format!("plug {}x{}\n", OUT.0, OUT.1)), "ok");
    h.settle();
    assert_eq!(h.stat("outputs"), 2);
    assert_eq!(h.stat("shadow_bytes"), 2 * one);

    // ...and unplugging gives it back, both times.
    assert_eq!(h.request_line("unplug\n"), "ok");
    h.settle();
    assert_eq!(h.stat("shadow_bytes"), one);
    assert_eq!(h.request_line("unplug\n"), "ok");
    h.settle();
    assert_eq!(h.stat("outputs"), 0);
    assert_eq!(h.stat("shadow_bytes"), 0, "the last shadow is freed");

    h.quit();
}

/// With no shadow there is no copy, and `copy_us` says so; with one, the
/// two halves of the frame are reported separately.
#[test]
fn the_copy_is_measured_separately_from_the_paint() {
    let h = Harness::start("stats", true);
    let mut conn = h.client("painter");
    let size = Size::new(96.0, 64.0);
    window(&h, &mut conn, size, Color::rgb(0x80, 0x80, 0x80));
    recolour(&h, &mut conn, size, Color::rgb(0x10, 0x20, 0x30), 40);
    // A max rather than a mean: the copy of one small rect can round to
    // zero microseconds, and a window of frames that includes the initial
    // full-screen ones cannot.
    assert!(h.stat("copy_us_max") > 0, "the shadow was streamed out");
    assert!(h.stat("paint_us_max") > 0);
    h.quit();

    let plain = Harness::start("stats-off", false);
    let mut conn = plain.client("painter");
    window(&plain, &mut conn, size, Color::rgb(0x80, 0x80, 0x80));
    recolour(&plain, &mut conn, size, Color::rgb(0x10, 0x20, 0x30), 40);
    assert_eq!(plain.stat("copy_us_max"), 0, "there is no copy to make");
    assert!(plain.stat("paint_us_max") > 0);
    plain.quit();
}

/// An opaque image occludes what is behind it, and the picture is exactly
/// what it was when nothing was occluded.
///
/// This is the end-to-end pin on the occlusion lever (#3728). A fullscreen
/// opaque `XR24` image used to force the server to paint the desktop
/// background and every item beneath it first, only to overwrite the lot;
/// `PaintItem::opaque_cover` now qualifies such an image, so those passes
/// are skipped. The property that must survive is pixel identity: a solid
/// rect and an image of the same solid colour, in the same place, have to
/// produce the same screen — the second one just gets there without
/// painting what nobody can see.
///
/// The image is added *over* the rect so the occlusion actually fires:
/// adding it damages exactly its own bounds, the item covers that clip,
/// and everything below — the rect, the window's frame background, the
/// desktop — is skipped. If the cover were wrong in either direction the
/// two screenshots diverge.
#[test]
fn an_opaque_image_occludes_without_changing_a_pixel() {
    use nitro_wire::msg::CreateBuffer;
    use nitro_wire::types::{BufferId, format};

    let h = Harness::start("occlusion", true);
    let mut conn = h.client("painter");
    let size = Size::new(96.0, 64.0);
    // (b, g, r) as the buffer stores them; the same colour as the rect.
    let (b, g, r) = (0x20u8, 0xC0u8, 0x80u8);
    window(&h, &mut conn, size, Color::rgb(r, g, b));
    let with_rect = h.front();

    // An XR24 buffer of that colour, the exact size of the window content,
    // with a garbage X byte to prove the blit masks it off. Sealed: the
    // server maps client buffers and refuses unsealed ones (#569).
    let (bw, bh) = (size.w as u32, size.h as u32);
    let stride = bw * 4;
    let pixels: Vec<u8> = (0..(stride * bh))
        .map(|i| match i % 4 {
            0 => b,
            1 => g,
            2 => r,
            _ => 0xFF,
        })
        .collect();
    let fd = nitro_shm::memfd_with("nitro-occlusion", &pixels).unwrap();

    let image = NodeId(3);
    conn.tx()
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: bw,
            height: bh,
            stride,
            format: format::XR24,
            size: stride * bh,
            fd,
        })
        .create_image(image, NodeId(1), Rect::new(0.0, 0.0, size.w, size.h))
        .image(image, BufferId(1), nitro_core::IRect::new(0, 0, 96, 64))
        .commit(50)
        .unwrap();
    conn.flush().unwrap();
    h.settle();

    assert_same(
        &h.front(),
        &with_rect,
        "an opaque image over an identical rect",
    );
    h.quit();
}

/// An image that carries alpha does *not* occlude: what is behind it still
/// has to be painted, or the blend has nothing to blend against.
///
/// The negative half of the occlusion pin. An `AR24` buffer at 50 % alpha
/// over a known rect must come out as the blend of the two; if the scene
/// wrongly reported a cover, the server would skip the rect and the frame
/// background and blend against whatever the buffer happened to hold.
#[test]
fn an_image_with_alpha_does_not_occlude() {
    use nitro_wire::msg::CreateBuffer;
    use nitro_wire::types::{BufferId, format};

    let h = Harness::start("occlusion-alpha", true);
    let mut conn = h.client("painter");
    let size = Size::new(96.0, 64.0);
    let c = window(&h, &mut conn, size, Color::rgb(0x00, 0x00, 0xFF));

    let (bw, bh) = (size.w as u32, size.h as u32);
    let stride = bw * 4;
    // Straight-alpha red at a = 128 over the blue rect.
    let pixels: Vec<u8> = (0..(stride * bh))
        .map(|i| match i % 4 {
            0 | 1 => 0x00,
            2 => 0xFF,
            _ => 0x80,
        })
        .collect();
    let fd = nitro_shm::memfd_with("nitro-occlusion-alpha", &pixels).unwrap();

    let image = NodeId(3);
    conn.tx()
        .create_buffer(CreateBuffer {
            id: BufferId(1),
            width: bw,
            height: bh,
            stride,
            format: format::AR24,
            size: stride * bh,
            fd,
        })
        .create_image(image, NodeId(1), Rect::new(0.0, 0.0, size.w, size.h))
        .image(image, BufferId(1), nitro_core::IRect::new(0, 0, 96, 64))
        .commit(50)
        .unwrap();
    conn.flush().unwrap();
    h.settle();

    let px = h
        .front()
        .pixel(c.position.x as u32 + 8, c.position.y as u32 + 8);
    let (red, green, blue) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
    assert!(red > 0x70 && red < 0x90, "half red, got {px:#010x}");
    assert_eq!(green, 0, "no green anywhere, got {px:#010x}");
    assert!(blue > 0x70 && blue < 0x90, "half blue, got {px:#010x}");

    h.quit();
}

// ---------------------------------------------------------------------------
// The scroll blit (#592 / task 3780)
//
// Every test below drives two servers identically — one with the blit
// (`scroll_blit: true`, the default) and a reference without it — and
// compares **both whole buffers** after every step: the scanout buffer on
// screen (`front`) and the shadow (`shot`). A blit that is wrong is wrong
// somewhere in the middle of the screen, so nothing less will do. Each
// test also says whether the fast path must have been taken
// (`blit_frames` moved) or declined (it did not), so a test cannot pass by
// quietly exercising the ordinary path.
// ---------------------------------------------------------------------------

use nitro_core::Transform;
use nitro_wire::types::NodeKind;

const CLIPPER: NodeId = NodeId(2);
const CONTENT: NodeId = NodeId(3);
const ROWS: u32 = 30;

fn row_id(i: u32) -> NodeId {
    NodeId(100 + i)
}

/// Two servers, blit on and off, each with one client.
struct Pair {
    on: Harness,
    off: Harness,
    c_on: Connection,
    c_off: Connection,
    serial: u32,
}

impl Pair {
    fn new(name: &str, out: (u32, u32)) -> Self {
        Self::with_shadow(name, out, true)
    }

    fn with_shadow(name: &str, out: (u32, u32), shadow: bool) -> Self {
        let on = Harness::start_with(&format!("{name}-on"), shadow, true, out);
        let off = Harness::start_with(&format!("{name}-off"), shadow, false, out);
        let c_on = on.client("scroller");
        let c_off = off.client("scroller");
        Self {
            on,
            off,
            c_on,
            c_off,
            serial: 1,
        }
    }

    /// Apply the same transaction on both servers and wait for both.
    fn step(
        &mut self,
        f: impl Fn(nitro_wire::client::Transaction<'_>) -> nitro_wire::client::Transaction<'_>,
    ) {
        self.serial += 1;
        for (h, c) in [(&self.on, &mut self.c_on), (&self.off, &mut self.c_off)] {
            f(c.tx()).commit(self.serial).unwrap();
            c.flush().unwrap();
            // Drain whatever the server sends so its socket never fills.
            let mut seen = Vec::new();
            let _ = c.poll(&mut seen);
            h.settle();
        }
    }

    fn input(&self, ev: InputEvent) {
        self.on.input.push(ev.clone());
        self.off.input.push(ev);
        self.on.settle();
        self.off.settle();
    }

    fn assert_same(&self, what: &str) {
        assert_same(
            &self.on.front(),
            &self.off.front(),
            &format!("{what}: front"),
        );
        assert_same(&self.on.shot(), &self.off.shot(), &format!("{what}: shot"));
    }

    fn blits(&self) -> u64 {
        self.on.stat("blit_frames")
    }

    fn quit(self) {
        self.on.quit();
        self.off.quit();
    }
}

/// The `nitro-bench scroll` shape: a clipping viewport, a content group
/// of differently coloured 15-px rows with a 1-px gap (through which the
/// desktop's vertical gradient shows), scrolled by `SetBounds`.
fn bench_window(p: &mut Pair, size: Size, row_h: f32, gap: f32) {
    let root = NodeId(1);
    p.step(|tx| {
        let mut tx = tx
            .create_window(root, "scroll", size, Layer::Normal)
            .create_group(CLIPPER, root)
            .bounds(CLIPPER, Rect::new(0.0, 0.0, size.w, size.h))
            .clip(CLIPPER, true)
            .create_group(CONTENT, CLIPPER)
            .bounds(CONTENT, Rect::new(0.0, 0.0, size.w, ROWS as f32 * row_h));
        for i in 0..ROWS {
            let c = Color::rgb(
                (i * 37 % 256) as u8,
                (i * 91 % 256) as u8,
                (0x80 + i * 13 % 128) as u8,
            );
            tx = tx
                .create_rect(
                    row_id(i),
                    CONTENT,
                    Rect::new(0.0, i as f32 * row_h, size.w, row_h - gap),
                )
                .fill_solid(row_id(i), c);
        }
        tx
    });
}

fn scroll_to(p: &mut Pair, size: Size, row_h: f32, x: f32, y: f32) {
    p.step(|tx| tx.bounds(CONTENT, Rect::new(x, y, size.w, ROWS as f32 * row_h)));
}

#[test]
fn a_scroll_blit_paints_what_a_full_rasterize_paints() {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::new("blit-bench", OUT);
    bench_window(&mut p, size, 16.0, 1.0);
    p.assert_same("initial");
    let before = p.blits();
    // Down, down, a non-row-multiple, back up, sideways, diagonal, and a
    // jump larger than the viewport (nothing to copy: D is empty).
    let steps = [
        (0.0, -16.0),
        (0.0, -32.0),
        (0.0, -39.0),
        (0.0, -23.0),
        (5.0, -23.0),
        (-3.0, -30.0),
        (-3.0, -300.0),
        (0.0, 0.0),
    ];
    for (i, (x, y)) in steps.iter().enumerate() {
        scroll_to(&mut p, size, 16.0, *x, *y);
        p.assert_same(&format!("step {i} to ({x}, {y})"));
    }
    assert!(p.blits() > before, "the fast path was taken");
    p.quit();
}

#[test]
fn a_scroll_blit_of_opaque_rows_with_text_by_transform() {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::new("blit-ui", OUT);
    let root = NodeId(1);
    p.step(|tx| {
        let mut tx = tx
            .create_window(root, "list", size, Layer::Normal)
            .create_group(CLIPPER, root)
            .bounds(CLIPPER, Rect::new(0.0, 0.0, size.w, size.h))
            .clip(CLIPPER, true);
        for i in 0..ROWS {
            let c = if i % 2 == 0 {
                Color::rgb(0xF0, 0xF0, 0xF0)
            } else {
                Color::rgb(0xE0, 0xE8, 0xF0)
            };
            let label = NodeId(1000 + i);
            tx = tx
                .create_rect(
                    row_id(i),
                    CLIPPER,
                    Rect::new(0.0, i as f32 * 20.0, size.w, 20.0),
                )
                .fill_solid(row_id(i), c)
                .create_node(label, NodeKind::Text, CLIPPER)
                .bounds(label, Rect::new(4.0, i as f32 * 20.0 + 2.0, 120.0, 16.0))
                .set_text(label, "sans", 12.0, Color::BLACK, &format!("row {i}"));
        }
        tx
    });
    p.assert_same("initial");
    let before = p.blits();
    for (i, y) in [-20.0f32, -40.0, -47.0, -27.0].iter().enumerate() {
        p.step(|tx| tx.transform(CLIPPER, Transform::translate(0.0, *y)));
        p.assert_same(&format!("step {i}"));
    }
    assert!(p.blits() > before, "the fast path was taken");
    p.quit();
}

#[test]
fn a_pointer_over_the_viewport_is_not_copied() {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::new("blit-cursor", OUT);
    bench_window(&mut p, size, 16.0, 1.0);
    // Into the middle of the window, which is centred on the output.
    p.input(InputEvent::PointerAbsolute {
        x: 0.5,
        y: 0.5,
        time_ns: 2_000_000,
    });
    p.assert_same("pointer parked");
    let before = p.blits();
    for (i, y) in [-16.0f32, -32.0, -48.0].iter().enumerate() {
        scroll_to(&mut p, size, 16.0, 0.0, *y);
        p.assert_same(&format!("step {i}"));
    }
    assert!(
        p.blits() > before,
        "the fast path was taken around the cursor"
    );
    p.quit();
}

/// Each declined precondition: the frames agree and `blit_frames` does
/// not move.
fn declined(name: &str, setup: impl Fn(&mut Pair, Size), step: impl Fn(&mut Pair, Size)) {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::new(name, OUT);
    bench_window(&mut p, size, 16.0, 1.0);
    setup(&mut p, size);
    p.assert_same("setup");
    let before = p.blits();
    step(&mut p, size);
    p.assert_same("declined step");
    assert_eq!(p.blits(), before, "{name}: the fast path must be declined");
    p.quit();
}

#[test]
fn a_fractional_scroll_is_declined() {
    declined(
        "blit-frac",
        |_, _| {},
        |p, s| scroll_to(p, s, 16.0, 0.0, -7.5),
    );
}

#[test]
fn a_content_change_in_the_same_commit_is_declined() {
    declined(
        "blit-content",
        |_, _| {},
        |p, s| {
            p.step(|tx| {
                tx.bounds(CONTENT, Rect::new(0.0, -16.0, s.w, ROWS as f32 * 16.0))
                    .fill_solid(row_id(3), Color::rgb(0xFF, 0, 0))
            });
        },
    );
}

#[test]
fn a_scale_change_is_declined() {
    declined(
        "blit-scale",
        |_, _| {},
        |p, _| p.step(|tx| tx.transform(CONTENT, Transform::scale(1.0, 2.0))),
    );
}

/// A second client's window over the viewport: the blit may still run
/// around it, but the picture must be exact — including under the window
/// and where the viewport's content slides out from under it.
#[test]
fn a_window_above_the_viewport_is_never_copied() {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::new("blit-above", OUT);
    bench_window(&mut p, size, 16.0, 1.0);
    let mut o_on = p.on.client("overlay");
    let mut o_off = p.off.client("overlay");
    window(
        &p.on,
        &mut o_on,
        Size::new(40.0, 30.0),
        Color::rgb(0x10, 0xC0, 0x10),
    );
    window(
        &p.off,
        &mut o_off,
        Size::new(40.0, 30.0),
        Color::rgb(0x10, 0xC0, 0x10),
    );
    p.assert_same("overlay mapped");
    for (i, y) in [-16.0f32, -32.0, -48.0].iter().enumerate() {
        scroll_to(&mut p, size, 16.0, 0.0, *y);
        p.assert_same(&format!("step {i}"));
    }
    p.quit();
}

#[test]
fn without_a_shadow_there_is_no_blit() {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::with_shadow("blit-noshadow", OUT, false);
    bench_window(&mut p, size, 16.0, 1.0);
    for y in [-16.0f32, -32.0] {
        scroll_to(&mut p, size, 16.0, 0.0, y);
        p.assert_same("step");
    }
    assert_eq!(p.blits(), 0);
    p.quit();
}

/// A scroll racing a plug: every kept output is invalidated, and whether
/// the scroll lands in the invalidated frame or after it, the picture is
/// exact. (Which of the two happens is timing; the deterministic halves —
/// damage already waiting when a hint arrives is foreign, so a full
/// invalidation leaves nothing to copy, and `invalidate` after a hint
/// drops it — are `frame.rs` unit tests.)
#[test]
fn a_scroll_racing_an_invalidated_output_is_exact() {
    let size = Size::new(160.0, 112.0);
    let mut p = Pair::new("blit-plug", OUT);
    bench_window(&mut p, size, 16.0, 1.0);
    let plug = format!("plug {}x{}\n", OUT.0, OUT.1);
    for h in [&p.on, &p.off] {
        assert_eq!(h.request_line(&plug), "ok");
    }
    scroll_to(&mut p, size, 16.0, 0.0, -16.0);
    p.assert_same("scroll after plug");
    scroll_to(&mut p, size, 16.0, 0.0, -32.0);
    p.assert_same("and the one after");
    p.quit();
}

/// What the blit is for, measured: on a full-width scroll the rasterizer
/// draws the exposed band and the gaps rather than the viewport,
/// `paint_us` falls with it — and `damage_px` does not move at all.
#[test]
fn a_scroll_blit_rasterizes_a_band_and_leaves_the_damage_alone() {
    let out = (640, 480);
    let size = Size::new(560.0, 400.0);
    let mut p = Pair::new("blit-measure", out);
    bench_window(&mut p, size, 16.0, 0.0);
    let before = p.blits();
    // Each settled step is two frames (the move, then the age-2 carry), so
    // 70 steps fill the 120-frame statistics window with scroll frames
    // alone — the setup frames, whose batching is timing-dependent, have
    // left it by the time it is read.
    for i in 1..=70 {
        scroll_to(&mut p, size, 16.0, 0.0, -16.0 * (i % 12) as f32);
    }
    assert!(p.blits() >= before + 50, "{} blits", p.blits() - before);
    let (on, off) = (&p.on, &p.off);
    let raster_on = on.stat("raster_px_mean");
    let raster_off = off.stat("raster_px_mean");
    assert!(
        raster_on * 4 < raster_off,
        "raster_px_mean {raster_on} with the blit vs {raster_off} without"
    );
    assert!(on.stat("blit_px_mean") > 0);
    assert_eq!(off.stat("blit_px_mean"), 0);
    assert_eq!(
        on.stat("damage_px_mean"),
        off.stat("damage_px_mean"),
        "the copy region is the same either way"
    );
    let (us_on, us_off) = (on.stat("paint_us_mean"), off.stat("paint_us_mean"));
    assert!(
        us_on < us_off,
        "paint_us_mean {us_on} with the blit vs {us_off} without"
    );
    p.quit();
}
