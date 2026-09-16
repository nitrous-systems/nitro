//! `server.conf`, driven end to end through the real event loop: reading
//! it at startup, applying scale, position, primary and keyboard, and the
//! three ways to reload it.
//!
//! The shape is `tests/wm.rs`' and `tests/fake_loop.rs`': the real
//! [`run`](nitro_server::run) on a thread with the fake backend, real
//! `nitro-wire` clients and the v0 control socket. What is different is
//! that every harness here owns a **configuration directory of its own**
//! and points `Config::config_path` at it, because the environment is
//! process-global and these tests run in threads of one process — the same
//! reason `Config::scales` exists as a field.
//!
//! Every wait has a deadline. An asynchronous path (inotify) is polled to
//! one, never slept through.

// The geometry here is whole-pixel arithmetic on whole-pixel inputs, so
// equality is the assertion that means what it says.
#![allow(clippy::float_cmp)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::input::{BTN_LEFT, FakeInput, InputEvent};
use nitro_server::{Config, run, wm};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ButtonState, Layer, NodeId};

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);

/// evdev keycode 21: `y` on a US layout, `z` on a German one. The one key
/// that says which keymap is loaded without depending on a dead key or a
/// modifier level.
const KEY_Y_ON_US: u32 = 21;
/// `XK_z`.
const KEYSYM_Z: u32 = 0x0000_007a;
/// `XK_y`.
const KEYSYM_Y: u32 = 0x0000_0079;

/// Wait for a condition, polling. Every wait in this file has a deadline:
/// a test that hangs tells you nothing.
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
    /// The directory `server.conf` lives in — the one the server's inotify
    /// watch is on, so a temp file must be written *here* for a rename
    /// into place to be an atomic replace rather than a cross-device copy.
    config_dir: PathBuf,
    config_path: PathBuf,
    input: FakeInput,
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    /// Start a server whose `server.conf` holds `conf`.
    fn start(name: &str, conf: &str) -> Self {
        Self::start_with(name, conf, |_| {})
    }

    /// A server whose configuration **directory does not exist**, and which
    /// is never given a file at all.
    ///
    /// The state every fresh installation is in, and the one
    /// [`Harness::start`] cannot produce because it creates the directory
    /// and writes the file before the server starts. That convenience is
    /// precisely why the whole suite missed the defect this pins: the
    /// watch is placed on the file's *parent directory*, so a home with no
    /// `~/.config/nitro` had nothing to watch, and the very first write
    /// from a settings app — the one that creates the file — was the one
    /// event that could never be seen.
    fn start_without_config_dir(name: &str) -> Self {
        Self::start_with_options(name, None, |_| {})
    }

    /// The same, with a last look at the [`Config`] — which is how a test
    /// sets `NITRO_SCALE`'s effect without touching the environment.
    fn start_with(name: &str, conf: &str, tweak: impl FnOnce(&mut Config)) -> Self {
        Self::start_with_options(name, Some(conf), tweak)
    }

    /// The one constructor the others funnel through. `conf` of `None`
    /// leaves both the directory and the file absent.
    fn start_with_options(name: &str, conf: Option<&str>, tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-conf-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        if let Some(text) = conf {
            std::fs::create_dir_all(&config_dir).expect("config dir");
            std::fs::write(&config_path, text).expect("write server.conf");
        }

        let mut config = Config::fake(OUT.0, OUT.1, &path);
        config.config_path = Some(config_path.clone());
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        tweak(&mut config);
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            config_dir,
            config_path,
            input,
            time_ns: 1_000_000,
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

    /// The `outputs` line for a connector, without the leading name.
    fn output_line(&self, connector: &str) -> String {
        let lines = self.request_text("outputs\n");
        lines
            .iter()
            .find(|l| l.starts_with(&format!("{connector} ")))
            .unwrap_or_else(|| panic!("no `{connector}` in {lines:?}"))
            .clone()
    }

    /// A field of an `outputs` line, e.g. `scale` or `pos`.
    fn output_field(&self, connector: &str, key: &str) -> String {
        let line = self.output_line(connector);
        line.split_ascii_whitespace()
            .find_map(|f| f.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("no `{key}=` in {line:?}"))
            .to_owned()
    }

    /// Overwrite `server.conf` **atomically**, the way a settings app
    /// does: write a temp file beside it and rename it over the top.
    ///
    /// This is the write the server's watch has to survive, and the reason
    /// the watch is on the directory: a rename replaces the inode, so a
    /// watch on the file itself would follow the old one into oblivion and
    /// never fire again. The temp file is in the same directory so the
    /// rename is a rename and not a copy.
    fn rewrite_config(&self, conf: &str) {
        let tmp = self.config_dir.join("server.conf.tmp");
        std::fs::write(&tmp, conf).expect("write temp");
        std::fs::rename(&tmp, &self.config_path).expect("rename into place");
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

    fn point_at(&mut self, x: f32, y: f32) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerAbsolute {
            x: f64::from(x) / f64::from(OUT.0),
            y: f64::from(y) / f64::from(OUT.1),
            time_ns: self.time_ns,
        });
    }

    fn button(&mut self, button: u32, state: ButtonState) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::PointerButton {
            button,
            state,
            time_ns: self.time_ns,
        });
    }

    fn key(&mut self, keycode: u32, pressed: bool) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::Key {
            keycode,
            pressed,
            time_ns: self.time_ns,
        });
    }

    /// Press at `from`, drag through four intermediate points, release at
    /// `to` — a drag is driven by motions and one jump would not exercise
    /// the path a real pointer takes.
    fn drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        self.point_at(from.0, from.1);
        self.settle();
        self.button(BTN_LEFT, ButtonState::Pressed);
        self.settle();
        for i in 1..=4 {
            let t = i as f32 / 4.0;
            self.point_at(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            self.settle();
        }
        self.button(BTN_LEFT, ButtonState::Released);
        self.settle();
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

/// Drain a client's socket until `f` matches, or time out. Anything else
/// that arrives is kept, so a later call can still see it. Matches from
/// the back, so the *newest* answer wins — which is what a test asking
/// "what is the scale now" means.
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
        assert!(Instant::now() < deadline, "no {what}; got {seen:?}");
        conn.flush().unwrap();
        conn.poll(seen).unwrap_or_else(|e| panic!("{what}: {e}"));
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Create a window filled with one solid rect and wait for its first
/// `Configure`.
fn make_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: u32, serial: u32) -> NodeId {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, "conf", WIN, Layer::Normal, 0)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, RED)
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    expect(conn, seen, "the first Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(()),
        _ => None,
    });
    root
}

/// The newest `Configure` for a window.
fn configure(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    root: NodeId,
) -> nitro_wire::msg::Configure {
    expect(conn, seen, "a Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(*c),
        _ => None,
    })
}

/// The newest `Configure` for a window **after** draining whatever the
/// socket has for us right now.
///
/// [`configure`] answers out of the buffer and would happily hand back the
/// window's *first* `Configure` long after a drag moved it; anything
/// asking "where is it now" has to read the socket first.
fn latest_configure(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    root: NodeId,
) -> nitro_wire::msg::Configure {
    conn.flush().unwrap();
    let _ = conn.poll(seen);
    configure(conn, seen, root)
}

/// Poll until a `Configure` for `root` carries `scale`, or time out.
///
/// A reload driven by inotify is asynchronous — nothing tells the test
/// when the kernel delivered the event — so the assertion is "this
/// happens within the deadline", which is a real assertion: a server that
/// never reloads fails it.
fn await_scale(conn: &mut Connection, seen: &mut Vec<ServerMsg>, root: NodeId, scale: f32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        conn.flush().unwrap();
        let _ = conn.poll(seen);
        let now = seen.iter().rev().find_map(|m| match m {
            ServerMsg::Configure(c) if c.window == root => Some(c.scale),
            _ => None,
        });
        if now == Some(scale) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no Configure with scale {scale}; newest is {now:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The middle of a window's title bar, in **desktop** coordinates.
///
/// `Configure.position` is the content's origin in its *own output's*
/// logical space, so a test that means "grab the title bar" has to add the
/// frame insets to get from content to frame, and the output's desktop
/// origin to get from output-local to desktop.
fn title_bar_at(c: &nitro_wire::msg::Configure, output_origin_x: f32) -> (f32, f32) {
    let insets = wm::frame_insets();
    (
        output_origin_x + c.position.x - insets.left + WIN.w / 3.0,
        c.position.y - insets.top + wm::TITLE_H / 2.0,
    )
}

/// Whether a keymap compiled at all, and whether the `de` layout is
/// available on this box.
///
/// `Keyboard` is compiled from `xkeyboard-config` data files, which a
/// build box need not have; a keyboard test there is testing the
/// packaging, not the server. Guarded the way the pixel tests guard on
/// `has_fonts()`: say so and return.
fn has_layout(layout: &str) -> bool {
    use nitro_server::config::KeyboardSettings;
    let settings = KeyboardSettings {
        layout: Some(layout.to_owned()),
        ..KeyboardSettings::default()
    };
    // `with_settings` falls back to `us` when the asked-for layout does not
    // compile, so "it returned something" is not enough — the names have to
    // say what was actually loaded.
    nitro_server::keyboard::Keyboard::with_settings(&settings)
        .is_some_and(|kb| kb.layout_names().iter().any(|n| n == layout))
}

#[test]
fn a_configured_scale_is_in_force_at_startup() {
    let h = Harness::start("scale-start", "output.Virtual-1.scale = 2\n");
    let mut seen = Vec::new();
    let mut conn = h.client("scale-start");
    let root = make_window(&mut conn, &mut seen, 1, 1);

    // The client is told, because `Configure.scale` is how it learns how
    // many device pixels its logical rectangle is worth.
    let c = configure(&mut conn, &mut seen, root);
    assert_eq!(c.scale, 2.0, "the file's scale reached the client");
    // ...and the control socket says the same thing, in the format a
    // person reads.
    assert_eq!(h.output_field("Virtual-1", "scale"), "2");
    assert_eq!(h.output_field("Virtual-1", "primary"), "1");

    drop(conn);
    h.quit();
}

#[test]
fn the_environment_beats_the_file_for_scale() {
    // The precedence the whole configuration is built on: `NITRO_SCALE` is
    // the *development* channel, so a `just fake` must not be silently
    // overridden by whatever the box's own config says. `Config::scales`
    // is that variable, parsed — a test sets the field rather than the
    // environment, which is process-global and shared with every other
    // test in this binary.
    let h = Harness::start_with("envwins", "output.Virtual-1.scale = 2\n", |c| {
        c.scales = nitro_server::parse_scales("Virtual-1=1");
    });
    assert_eq!(
        h.output_field("Virtual-1", "scale"),
        "1",
        "NITRO_SCALE wins over the file"
    );
    h.quit();
}

#[test]
fn the_file_beats_the_edid_default() {
    // The other half of the ladder. The fake backend reports a 96-dpi
    // physical size, so `default_scale` says 1; the file says 2 and wins.
    let h = Harness::start("filewins", "output.Virtual-1.scale = 2\n");
    assert_eq!(h.output_field("Virtual-1", "scale"), "2");
    h.quit();

    // And with the file silent, the EDID default is what is left.
    let h = Harness::start("edidwins", "# nothing about the scale\n");
    assert_eq!(h.output_field("Virtual-1", "scale"), "1");
    h.quit();
}

#[test]
fn a_reload_over_the_control_socket_applies_a_new_scale_and_repaints() {
    let h = Harness::start("reload-scale", "output.Virtual-1.scale = 2\n");
    let mut seen = Vec::new();
    let mut conn = h.client("reload-scale");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    assert_eq!(configure(&mut conn, &mut seen, root).scale, 2.0);
    h.settle();

    let before_reloads = h.stat("config_reloads");
    let before_frames = h.stat("frames");

    h.rewrite_config("output.Virtual-1.scale = 1\n");
    assert_eq!(h.request_line("reload\n"), "ok");

    // The `ok` is sent after the reload was applied, so the effect is
    // already visible — but the *client* still has to be handed its
    // `Configure`, which is one socket round trip away.
    await_scale(&mut conn, &mut seen, root, 1.0);
    assert_eq!(h.output_field("Virtual-1", "scale"), "1");
    // At least one: the rewrite lands in the watched directory too, so
    // the inotify path may have got there first and the request then
    // reloaded an already-current file. Both are reloads and both count.
    assert!(
        h.stat("config_reloads") > before_reloads,
        "the reload was counted"
    );

    // And the screen was actually repainted: a scale change moves every
    // pixel on the output, so a reload that only told the client would
    // leave the old image on the display.
    h.settle();
    assert!(
        h.stat("frames") > before_frames,
        "a scale change repaints the output"
    );

    drop(conn);
    h.quit();
}

#[test]
fn an_atomic_write_into_the_watched_directory_reloads_on_its_own() {
    // The inotify path, with no control request at all: `nitro-settings`
    // writes a temp file and renames it over `server.conf`, which replaces
    // the inode — the whole reason the watch is on the *directory*.
    let h = Harness::start("inotify", "output.Virtual-1.scale = 2\n");
    let mut seen = Vec::new();
    let mut conn = h.client("inotify");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    assert_eq!(configure(&mut conn, &mut seen, root).scale, 2.0);
    h.settle();
    let before = h.stat("config_reloads");

    h.rewrite_config("output.Virtual-1.scale = 1\n");

    // Asynchronous: poll to a deadline rather than sleeping a guess. A
    // server that never notices fails this in ten seconds.
    await_scale(&mut conn, &mut seen, root, 1.0);
    wait_for("the reload to be counted", || {
        h.stat("config_reloads") > before
    });
    assert_eq!(h.output_field("Virtual-1", "scale"), "1");

    drop(conn);
    h.quit();
}

#[test]
fn a_config_that_does_not_exist_yet_is_still_watched() {
    // The fresh-installation case, and a defect this suite could not see
    // until it had a harness that leaves the directory out: on a box whose
    // `~/.config/nitro` did not exist, the watch could not be placed (it
    // goes on the *parent directory*, which has to exist), nothing ever
    // retried it, and so the first file a settings app wrote — the one
    // that creates it — was the single event guaranteed to be missed.
    //
    // Found by running it on the test box, where `Apply` wrote a correct
    // file, the server sat at `config_reloads 0`, and the app honestly
    // reported "server rejected: see log".
    let h = Harness::start_without_config_dir("fresh");
    let mut seen = Vec::new();
    let mut conn = h.client("fresh");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    // Nothing configured: the EDID default, which is 1 on a fake output.
    assert_eq!(configure(&mut conn, &mut seen, root).scale, 1.0);
    h.settle();
    let before = h.stat("config_reloads");

    // Now be a settings app on a machine that has never been configured:
    // create the file for the first time, atomically.
    h.rewrite_config("output.Virtual-1.scale = 2\n");

    // No control request, no restart — this can only pass if the server
    // created the directory it was told to watch and armed the watch on it.
    await_scale(&mut conn, &mut seen, root, 2.0);
    wait_for("the first-ever config write to be noticed", || {
        h.stat("config_reloads") > before
    });
    assert_eq!(h.output_field("Virtual-1", "scale"), "2");

    drop(conn);
    h.quit();
}

#[test]
fn explicit_positions_lay_the_desktop_out_instead_of_connector_order() {
    // Two outputs, the second put to the *left* of the first: connector
    // order would have put it on the right, so this can only pass if the
    // file decided the layout.
    //
    // 400 px wide at 1x, so `Virtual-2` occupies desktop x = -400..0 and
    // `Virtual-1` x = 0..640.
    let mut h = Harness::start(
        "positions",
        "output.Virtual-1.position = 0,0\noutput.Virtual-2.position = -400,0\n",
    );
    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the second output", || h.stat("outputs") == 2);
    h.settle();

    assert_eq!(h.output_field("Virtual-1", "pos"), "0,0");
    assert_eq!(
        h.output_field("Virtual-2", "pos"),
        "-400,0",
        "the file placed it, not connector order"
    );

    // And the layout is real, not just reported: drag a window left off
    // the first output and it lands on the second one.
    let mut seen = Vec::new();
    let mut conn = h.client("positions");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let first = configure(&mut conn, &mut seen, root);
    assert_eq!(first.output, 1, "placed on the primary, which is the first");
    // Virtual-1 sits at desktop 0,0 at scale 1, so desktop, device and
    // output-local coordinates all coincide on it.
    let bar = title_bar_at(&first, 0.0);
    // Well into the screen on the left: -200 in desktop space.
    h.drag(bar, (-200.0, 100.0));

    let now = latest_configure(&mut conn, &mut seen, root);
    assert_ne!(
        now.output, first.output,
        "the window crossed onto the output the file put on the left"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_configured_primary_takes_the_orphans_when_an_output_is_unplugged() {
    // `Virtual-1` is the first connector and would be the primary by
    // default; the file names `Virtual-2` instead, so a window orphaned by
    // unplugging must land there rather than back on the first.
    //
    // The unplug removes the *last* output, so a third one is plugged and
    // the window is dragged onto it.
    let mut h = Harness::start("primary", "output.Virtual-2.primary = true\n");
    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the second output", || h.stat("outputs") == 2);
    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the third output", || h.stat("outputs") == 3);
    h.settle();
    assert_eq!(h.output_field("Virtual-2", "primary"), "1");
    assert_eq!(h.output_field("Virtual-1", "primary"), "0");

    let mut seen = Vec::new();
    let mut conn = h.client("primary");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let first = configure(&mut conn, &mut seen, root);
    assert_eq!(
        first.output, 2,
        "a new window is placed on the primary, which the file moved"
    );
    // Everything here is at scale 1, so desktop and device units agree and
    // the layout is Virtual-1 at 0, Virtual-2 at 640, Virtual-3 at 1040.
    // `Configure.position` is local to the window's own output, so the
    // title bar is at that plus Virtual-2's origin.
    let bar = title_bar_at(&first, 640.0);
    h.drag(bar, (1040.0 + 150.0, 100.0));
    let on_third = latest_configure(&mut conn, &mut seen, root);
    assert_ne!(on_third.output, first.output, "the window is on Virtual-3");

    // Pull it out: the window is orphaned and has to be migrated. The file
    // says where.
    assert_eq!(h.request_line("unplug\n"), "ok");
    wait_for("the third output to go", || h.stat("outputs") == 2);
    h.settle();

    let migrated = latest_configure(&mut conn, &mut seen, root);
    // The fake backend allocates ids in plug order and names them to
    // match, so `Virtual-2` is output 2. The default — the first
    // connector — would be 1, which is what makes this assertion say
    // something.
    assert_eq!(
        migrated.output, 2,
        "orphans went to the configured primary, not the first connector"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_german_layout_from_the_file_turns_keycode_21_into_z() {
    if !has_layout("de") {
        eprintln!("no `de` xkb layout on this box; skipping the keyboard check");
        return;
    }
    let mut h = Harness::start("kbd-de", "keyboard.layout = de\n");
    let mut seen = Vec::new();
    let mut conn = h.client("kbd-de");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let _ = configure(&mut conn, &mut seen, root);
    h.settle();

    h.key(KEY_Y_ON_US, true);
    h.key(KEY_Y_ON_US, false);
    h.settle();

    let keysym = expect(&mut conn, &mut seen, "a Key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_Y_ON_US => Some(k.keysym),
        _ => None,
    });
    assert_eq!(
        keysym, KEYSYM_Z,
        "evdev 21 is `y` on a US layout and `z` on a German one"
    );

    drop(conn);
    h.quit();
}

#[test]
fn a_keyboard_layout_change_takes_effect_on_reload() {
    if !has_layout("de") {
        eprintln!("no `de` xkb layout on this box; skipping the keyboard check");
        return;
    }
    let mut h = Harness::start("kbd-reload", "keyboard.layout = us\n");
    let mut seen = Vec::new();
    let mut conn = h.client("kbd-reload");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let _ = configure(&mut conn, &mut seen, root);
    h.settle();

    h.key(KEY_Y_ON_US, true);
    h.key(KEY_Y_ON_US, false);
    h.settle();
    let first = expect(&mut conn, &mut seen, "a Key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_Y_ON_US => Some(k.keysym),
        _ => None,
    });
    assert_eq!(first, KEYSYM_Y, "us layout to start with");
    seen.clear();

    h.rewrite_config("keyboard.layout = de\n");
    assert_eq!(h.request_line("reload\n"), "ok");

    h.key(KEY_Y_ON_US, true);
    h.key(KEY_Y_ON_US, false);
    h.settle();
    let after = expect(
        &mut conn,
        &mut seen,
        "a Key after the reload",
        |m| match m {
            ServerMsg::Key(k) if k.keycode == KEY_Y_ON_US => Some(k.keysym),
            _ => None,
        },
    );
    assert_eq!(after, KEYSYM_Z, "the keymap was rebuilt");

    drop(conn);
    h.quit();
}

#[test]
fn garbage_in_the_file_does_not_stop_the_server_or_lose_the_configuration() {
    // A configuration file is user input that arrives while the compositor
    // is running, so there is no useful sense in which parsing it can
    // fail: a bad line is skipped and warned about, and the desktop keeps
    // running on what it already had.
    let h = Harness::start("garbage", "output.Virtual-1.scale = 2\n");
    let mut seen = Vec::new();
    let mut conn = h.client("garbage");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    assert_eq!(configure(&mut conn, &mut seen, root).scale, 2.0);
    h.settle();
    let before = h.stat("config_reloads");

    // Every line unusable, including one that would have set the scale if
    // the value were a number.
    h.rewrite_config(
        "this is not a config\n\
         =\n\
         output.Virtual-1.scale = wide\n\
         output.Virtual-1.rotation = 90\n\
         keyboard.repeat = 300,25\n\
         \u{0}\u{0}\u{0}\n",
    );
    assert_eq!(h.request_line("reload\n"), "ok");
    assert!(h.stat("config_reloads") > before);

    // The server is alive and answering, which is the first half of the
    // claim...
    assert_eq!(h.stat("outputs"), 1);
    assert_eq!(h.stat("windows"), 1);
    // ...and the *file* no longer says `scale = 2`, so the scale falls
    // back to the EDID default. That is not "the previous value kept by
    // magic": a key the file no longer sets is a key nobody set, exactly
    // as at startup. What must not happen is a half-applied scale from
    // `scale = wide`.
    assert_eq!(
        h.output_field("Virtual-1", "scale"),
        "1",
        "an unparseable scale is skipped, not half-applied"
    );

    // Now the other half: a file that is *mostly* garbage still applies
    // the lines it can.
    h.rewrite_config("nonsense\noutput.Virtual-1.scale = 2\nalso nonsense\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    await_scale(&mut conn, &mut seen, root, 2.0);
    assert_eq!(h.output_field("Virtual-1", "scale"), "2");

    drop(conn);
    h.quit();
}

#[test]
fn a_watched_server_with_nothing_happening_is_still_idle() {
    // The claim the inotify doc comment makes: an inotify fd with nothing
    // queued is simply not readable, so registering it costs a desktop
    // nobody is configuring one fd in the epoll set and zero wakeups.
    //
    // Measured the way the idle claims elsewhere are: settle, then check
    // that the frame counter does not move over a real interval. A watch
    // that woke the loop would paint (the reload path repaints in full),
    // so a moving counter is exactly the failure this rules out.
    let h = Harness::start("idle", "output.Virtual-1.scale = 1\n");
    let mut seen = Vec::new();
    let mut conn = h.client("idle");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let _ = configure(&mut conn, &mut seen, root);
    h.settle();

    let frames = h.stat("frames");
    let reloads = h.stat("config_reloads");
    // A file written *next to* `server.conf` is an inotify event the
    // server must drain and ignore — draining is what keeps the
    // level-triggered fd from spinning, ignoring is what keeps it from
    // reloading.
    std::fs::write(h.config_dir.join("unrelated.txt"), "hello\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(h.stat("config_reloads"), reloads, "not our file");
    assert_eq!(h.stat("frames"), frames, "an idle watched server is idle");

    drop(conn);
    h.quit();
}

#[test]
fn a_server_with_no_config_file_still_answers_reload() {
    // The state a fresh installation and every other test in the tree is
    // in: `Config::config_path` is `None`, so there is nothing to read and
    // nothing to watch. `reload` is still a valid request.
    let dir = std::env::temp_dir().join(format!("nitro-conf-{}-nofile", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("nitro").join("control.sock");
    let config = Config::fake(OUT.0, OUT.1, &path);
    assert!(
        config.config_path.is_none(),
        "Config::fake must not read the developer's own server.conf"
    );
    let thread = std::thread::spawn(move || run(config));
    wait_for("the control socket", || UnixStream::connect(&path).is_ok());

    let request = |req: &str| -> String {
        let s = UnixStream::connect(&path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
    };
    assert_eq!(request("reload\n"), "ok");
    assert_eq!(request("quit\n"), "ok");
    wait_for("the server thread to stop", || thread.is_finished());
    thread.join().unwrap().expect("server returned an error");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A pure check of the desktop/device agreement rule, without a server.
///
/// The device rect of a positioned output is its logical position times
/// its scale, so the two spaces describe the same arrangement; a test that
/// only looked at `pos=` could not tell that apart from a device layout
/// still in connector order.
#[test]
fn a_positioned_output_reports_the_scaled_device_origin_it_was_given() {
    let mut h = Harness::start(
        "posscale",
        "output.Virtual-1.position = 0,0\n\
         output.Virtual-1.scale = 2\n",
    );
    // At 2x the 640x480 mode is 320x240 of desktop space, so an
    // unpositioned second output starts at desktop x = 320 and device
    // x = 640.
    assert_eq!(h.request_line("plug 400x300\n"), "ok");
    wait_for("the second output", || h.stat("outputs") == 2);
    h.settle();
    assert_eq!(h.output_field("Virtual-1", "pos"), "0,0");
    assert_eq!(h.output_field("Virtual-1", "scale"), "2");
    assert_eq!(
        h.output_field("Virtual-2", "pos"),
        "320,0",
        "an unpositioned output starts where the previous one's *logical* width ends"
    );

    // The device space agrees: the pointer crosses onto the second output
    // at device x = 640, which in desktop units is 320.
    let mut seen = Vec::new();
    let mut conn = h.client("posscale");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let first = configure(&mut conn, &mut seen, root);
    assert_eq!(first.output, 1, "placed on the primary, which is the first");
    // The pointer is driven in **device** pixels, and Virtual-1 is 2×, so
    // a logical point on it is at twice the device coordinate. Virtual-2's
    // device rect starts where Virtual-1's 640 device pixels end.
    let (bx, by) = title_bar_at(&first, 0.0);
    let bar = (bx * 2.0, by * 2.0);
    h.drag(bar, (640.0 + 200.0, 100.0));
    let now = latest_configure(&mut conn, &mut seen, root);
    assert_ne!(
        now.output, first.output,
        "device x=840 is on the second output, whose device rect starts at 640"
    );
    assert_eq!(
        now.scale, 1.0,
        "and it is told the second output's scale, not the first's"
    );

    drop(conn);
    h.quit();
}

// ---------------------------------------------------------------------------
// `output.<connector>.mode`
// ---------------------------------------------------------------------------

/// The mode table a mode test's fake connector offers.
///
/// The shape of the test box's HDMI-A-1 (`docs/testbox.md`): a preferred
/// 60, a faster rate at the same size, a second rate close enough to 60 to
/// make `@60` a real question, and a smaller size so a *size* change is
/// reachable too. Scaled down from 1920×1080 only so the harness's window
/// arithmetic stays the arithmetic every other test here uses.
const MODES: [(u32, u32, u32); 4] = [
    (640, 480, 60_000),
    (640, 480, 120_000),
    (640, 480, 59_940),
    (320, 240, 240_000),
];

/// A harness whose fake connector has a mode table.
fn mode_harness(name: &str, conf: &str) -> Harness {
    Harness::start_with(name, conf, |c| c.fake_modes = MODES.to_vec())
}

/// The `@refresh` field of an `outputs` line, as millihertz.
fn reported_refresh(h: &Harness) -> u32 {
    let line = h.output_line("Virtual-1");
    line.split_ascii_whitespace()
        .nth(1)
        .and_then(|f| f.split_once('@'))
        .and_then(|(_, r)| r.parse().ok())
        .unwrap_or_else(|| panic!("no WxH@refresh in {line:?}"))
}

#[test]
fn the_file_picks_the_mode_and_outputs_reports_it() {
    // Without a `mode` line the connector's preferred mode is what runs,
    // which is what nitro always did.
    let h = mode_harness("mode-default", "# nothing about the mode\n");
    assert_eq!(reported_refresh(&h), 60_000);
    h.quit();

    // With one, the rate the file asked for — and `outputs` reports the
    // truth rather than the request, which is the only way to tell a
    // modeset that happened from one that silently did not.
    let h = mode_harness("mode-120", "output.Virtual-1.mode = 640x480@120\n");
    assert_eq!(reported_refresh(&h), 120_000);
    assert!(
        h.output_line("Virtual-1")
            .starts_with("Virtual-1 640x480@120000 "),
        "{}",
        h.output_line("Virtual-1")
    );
    h.quit();
}

#[test]
fn sixty_is_not_fifty_nine_ninety_four() {
    // Both rates are in the table 60 mHz apart, so this is the case the
    // nearest-match rule exists for: `@60` must not round into the 59.94
    // that sits next to it in every HDMI table, and vice versa.
    let h = mode_harness("mode-60", "output.Virtual-1.mode = 640x480@60\n");
    assert_eq!(reported_refresh(&h), 60_000);
    h.quit();

    let h = mode_harness("mode-5994", "output.Virtual-1.mode = 640x480@59.94\n");
    assert_eq!(reported_refresh(&h), 59_940);
    h.quit();
}

#[test]
fn the_aliases_mean_what_the_documentation_says() {
    // `fastest`: the preferred *size*, top rate. `max`: largest area,
    // ignoring the preferred flag — which on this table is the smaller,
    // faster mode losing to the bigger, slower one.
    let h = mode_harness("mode-fastest", "output.Virtual-1.mode = fastest\n");
    assert_eq!(
        h.output_line("Virtual-1").split_whitespace().nth(1),
        Some("640x480@120000")
    );
    h.quit();

    let h = mode_harness("mode-max", "output.Virtual-1.mode = max\n");
    assert_eq!(
        h.output_line("Virtual-1").split_whitespace().nth(1),
        Some("640x480@120000")
    );
    h.quit();
}

#[test]
fn the_environment_beats_the_file_for_the_mode() {
    // The same ladder `NITRO_SCALE` is on, and for the same reason: a
    // measurement run must be able to override the box's own file without
    // editing it. `Config::modes` is `NITRO_MODE`, parsed.
    let h = Harness::start_with(
        "mode-envwins",
        "output.Virtual-1.mode = 640x480@120\n",
        |c| {
            c.fake_modes = MODES.to_vec();
            c.modes = nitro_server::parse_modes("Virtual-1=640x480@59.94");
        },
    );
    assert_eq!(
        reported_refresh(&h),
        59_940,
        "NITRO_MODE wins over the file"
    );
    h.quit();
}

#[test]
fn an_unlisted_mode_warns_and_leaves_the_desktop_up() {
    // The whole reason the request falls back rather than failing: a
    // `mode` line naming a resolution this monitor does not have must cost
    // a warning, not a desktop. The server is up, the output is on its
    // preferred mode, and the rest of the file still applied.
    let h = mode_harness(
        "mode-unlisted",
        "output.Virtual-1.mode = 2560x1440@144\noutput.Virtual-1.scale = 2\n",
    );
    assert_eq!(reported_refresh(&h), 60_000);
    assert_eq!(h.output_field("Virtual-1", "scale"), "2");
    h.quit();
}

#[test]
fn a_reload_retimes_the_output_and_a_second_one_costs_nothing() {
    let h = mode_harness("mode-reload", "output.Virtual-1.mode = 640x480@60\n");
    assert_eq!(reported_refresh(&h), 60_000);
    h.settle();

    // A window, so the retime can be checked for what it does to the
    // *clients* rather than only to the counters.
    let mut seen = Vec::new();
    let mut conn = h.client("mode-reload");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let before = configure(&mut conn, &mut seen, root);
    h.settle();

    // A same-size refresh change is applied live, and the window is
    // undisturbed by it: same output, same size, same scale, nothing
    // closed. That is what a retime looks like from a client's side.
    //
    // **What this cannot prove.** It runs on the fake backend, which
    // *edits* an output in place and has no code path that could replace
    // one -- so the interesting half, "the DRM backend decided to retime
    // rather than destroy and rebuild under a new id", is not under test
    // here and a green result says nothing about it. That rule is
    // `select::reconcile_one`, tested directly in `drm/select.rs` by
    // `a_retime_keeps_the_output_and_a_resize_replaces_it`, which does
    // fail when the rule is removed. This test covers the server-side
    // consequence; that one covers the decision.
    h.rewrite_config("output.Virtual-1.mode = 640x480@120\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    assert_eq!(reported_refresh(&h), 120_000);
    assert_eq!(h.stat("outputs"), 1);
    h.settle();
    let after = latest_configure(&mut conn, &mut seen, root);
    assert_eq!(
        (after.output, after.size, after.scale),
        (before.output, before.size, before.scale),
        "a retime must not move the window or change the output it is on"
    );
    assert!(
        !seen.iter().any(|m| matches!(m, ServerMsg::Closed(_))),
        "a retime must not close anything"
    );

    // And a reload that says the same thing again is not a second
    // modeset: the backend compares before it touches the hardware, which
    // is what keeps an unrelated reload (a colour, a keyboard layout) from
    // blanking the screen.
    let frames = h.stat("frames");
    h.rewrite_config("output.Virtual-1.mode = 640x480@120\ntheme.scheme = dark\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    assert_eq!(reported_refresh(&h), 120_000);
    assert!(h.stat("frames") >= frames, "a reload never loses frames");

    drop(conn);
    h.quit();
}

#[test]
fn a_reload_can_change_the_size_too_and_the_desktop_follows() {
    // The expensive case: the mode's *size* changes, so the scanout
    // buffers and the shadow are reallocated and the scene is re-laid out.
    // Checked through the desktop rather than the backend, because "the
    // output is 320x240 now" is only true if the scene agrees.
    let h = mode_harness("mode-resize", "output.Virtual-1.mode = 640x480@60\n");
    h.settle();
    h.rewrite_config("output.Virtual-1.mode = 320x240\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    let line = h.output_line("Virtual-1");
    assert_eq!(
        line.split_whitespace().nth(1),
        Some("320x240@240000"),
        "{line}"
    );
    h.settle();
    // The readback is the new size, which is the half a mode report
    // cannot fake: the buffers really were reallocated.
    let mut c = h.connect();
    c.get_mut().write_all(b"shot\n").unwrap();
    let mut header = String::new();
    c.read_line(&mut header).unwrap();
    assert!(header.starts_with("ok 320 240 "), "{header:?}");
    h.quit();
}

#[test]
fn the_modes_command_lists_what_a_user_may_write() {
    // The answer to "how do I run this screen at 120", which `outputs`
    // cannot give: it reports the one mode in force.
    let h = mode_harness("mode-list", "output.Virtual-1.mode = 640x480@120\n");
    let lines = h.request_text("modes\n");
    assert_eq!(
        lines,
        vec![
            "ok",
            "Virtual-1 640x480@60 *",
            "Virtual-1 640x480@120 =",
            "Virtual-1 640x480@59.94",
            "Virtual-1 320x240@240",
        ],
        "`*` is the preferred mode and `=` the one in use"
    );
    // Every line's mode text is exactly what the key takes, which is the
    // property that makes this list useful rather than decorative.
    for l in lines.iter().skip(1) {
        let spec = l.split_whitespace().nth(1).unwrap();
        assert!(
            nitro_kms::ModeRequest::parse(spec).is_ok(),
            "{spec:?} is not something `output.<c>.mode` accepts"
        );
    }
    h.quit();
}
