//! The palette, driven end to end through the real event loop: every
//! client gets a `Theme` after its `Welcome`, editing `server.conf`
//! pushes a new one, an override moves exactly one role, and the
//! decorations follow.
//!
//! The shape is `tests/config.rs`': the real [`run`](nitro_server::run)
//! on a thread with the fake backend, real `nitro-wire` clients, the v0
//! control socket, and a configuration directory of its own (the
//! environment is process-global and these tests run in threads of one
//! process).
//!
//! Every wait has a deadline. The inotify path is asynchronous, so it is
//! polled to one rather than slept through.

// The geometry is whole-pixel arithmetic on whole-pixel inputs, so
// equality means what it says.
#![allow(clippy::float_cmp)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Palette, Rect, Role, Size};
use nitro_kms::Image;
use nitro_server::{Config, run, wm};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{Layer, NodeId, caps};

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);

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
    shell_path: PathBuf,
    config_dir: PathBuf,
    config_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, conf: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-theme-{}-{name}", std::process::id()));
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
            shell_path,
            config_dir,
            config_path,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        wait_for("the shell socket", || h.shell_path.exists());
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

    fn shell_client(&self, name: &str) -> Connection {
        Connection::connect(&self.shell_path, name).expect("shell connect")
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

    /// The `theme` reply: the status line, and the role table as pairs.
    fn theme(&self) -> (String, Vec<(String, String)>) {
        let mut c = self.connect();
        c.get_mut().write_all(b"theme\n").unwrap();
        let mut status = String::new();
        c.read_line(&mut status).unwrap();
        let mut rows = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = c.read_line(&mut line).unwrap();
            assert!(n > 0, "connection closed mid-reply");
            let l = line.trim_end_matches('\n');
            if l.is_empty() {
                break;
            }
            let (k, v) = l.split_once(' ').expect("`role #rrggbb`");
            rows.push((k.to_owned(), v.to_owned()));
        }
        (status.trim_end_matches('\n').to_owned(), rows)
    }

    /// Overwrite `server.conf` atomically, the way a settings app does.
    fn rewrite_config(&self, conf: &str) {
        let tmp = self.config_dir.join("server.conf.tmp");
        std::fs::write(&tmp, conf).expect("write temp");
        std::fs::rename(&tmp, &self.config_path).expect("rename into place");
    }

    fn remove_config(&self) {
        std::fs::remove_file(&self.config_path).expect("remove server.conf");
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
        use std::io::Read as _;
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

/// The newest `Theme` on a connection, waiting for one if need be.
fn theme_of(conn: &mut Connection, seen: &mut Vec<ServerMsg>) -> nitro_wire::msg::Theme {
    expect(conn, seen, "a Theme", |m| match m {
        ServerMsg::Theme(t) => Some(t.clone()),
        _ => None,
    })
}

/// Poll until a `Theme` newer than `after` arrives.
fn await_theme(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    after: u32,
) -> nitro_wire::msg::Theme {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        conn.flush().unwrap();
        let _ = conn.poll(seen);
        if let Some(t) = seen.iter().rev().find_map(|m| match m {
            ServerMsg::Theme(t) if t.serial > after => Some(t.clone()),
            _ => None,
        }) {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "no Theme with a serial past {after}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn make_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: u32, serial: u32) -> NodeId {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, "theme", WIN, Layer::Normal, 0)
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

fn rgb(px: u32) -> u32 {
    px & 0x00ff_ffff
}

fn to_rgb(c: Color) -> u32 {
    u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b)
}

/// The pixel in the middle of a window's title bar, in output space.
fn title_bar_pixel(img: &Image, c: &nitro_wire::msg::Configure) -> u32 {
    let insets = wm::frame_insets();
    let x = (c.position.x + WIN.w / 2.0) as u32;
    let y = (c.position.y - insets.top + wm::TITLE_H / 2.0) as u32;
    rgb(img.pixel(x, y))
}

#[test]
fn every_client_is_told_the_palette_right_after_its_welcome() {
    // The first thing a client needs is the colours, because its first
    // paint happens before anything else arrives. A palette that came a
    // round trip later would mean every app on the desktop flashes its
    // built-in defaults for one frame.
    let h = Harness::start("welcome", "");
    let mut seen = Vec::new();
    let mut conn = h.client("welcome");
    // The `Welcome` itself is consumed by `Connection::connect`, which
    // is why the caps are read off the connection rather than out of
    // `seen` — and why the ordering assertion below is "the Theme is the
    // very first message a client sees" rather than "it follows the
    // Welcome": from the client's side those are the same statement.
    assert_ne!(conn.caps() & caps::THEME, 0, "the THEME bit is advertised");
    let theme = theme_of(&mut conn, &mut seen);
    assert_eq!(theme.colors.len(), Role::COUNT);
    // The default scheme is light, deliberately: every screenshot in
    // `docs/` was taken on it and a missing file must not change the
    // desktop's appearance.
    assert_eq!(theme.palette(), Palette::light());
    assert!(
        matches!(seen.first(), Some(ServerMsg::Theme(_))),
        "the palette is the first thing a client is told, so its first \
         paint already has the user's colours"
    );

    // A shell client gets one too: the bar and the wallpaper are on the
    // other socket and need colours just as much.
    let mut shell_seen = Vec::new();
    let mut shell = h.shell_client("welcome-shell");
    let shell_theme = theme_of(&mut shell, &mut shell_seen);
    assert_ne!(shell.caps() & caps::THEME, 0);
    assert_ne!(shell.caps() & caps::SHELL, 0);
    assert_eq!(shell_theme.palette(), Palette::light());
    assert_eq!(shell_theme.serial, theme.serial);

    drop(conn);
    drop(shell);
    h.quit();
}

#[test]
fn a_configured_dark_scheme_is_in_force_at_startup() {
    let h = Harness::start("dark-start", "theme.scheme = dark\n");
    let mut seen = Vec::new();
    let mut conn = h.client("dark-start");
    assert_eq!(theme_of(&mut conn, &mut seen).palette(), Palette::dark());
    let (status, _) = h.theme();
    assert!(status.starts_with("ok dark "), "{status:?}");
    drop(conn);
    h.quit();
}

#[test]
fn switching_the_scheme_pushes_a_new_theme_and_restyles_the_decorations() {
    // The headline behaviour: one line in `server.conf`, and the whole
    // desktop changes colour — the clients because they were told, the
    // decorations because the server draws them itself.
    let h = Harness::start("switch", "");
    let mut seen = Vec::new();
    let mut conn = h.client("switch");
    let root = make_window(&mut conn, &mut seen, 1, 1);
    let c = configure(&mut conn, &mut seen, root);
    let first = theme_of(&mut conn, &mut seen);
    assert_eq!(first.palette(), Palette::light());
    h.settle();

    // The focused title bar is the light scheme's.
    assert_eq!(
        title_bar_pixel(&h.shot(), &c),
        to_rgb(Palette::light().get(Role::TitleBarActive)),
        "the light title bar"
    );

    h.rewrite_config("theme.scheme = dark\n");
    assert_eq!(h.request_line("reload\n"), "ok");

    let next = await_theme(&mut conn, &mut seen, first.serial);
    assert_eq!(next.palette(), Palette::dark());
    assert!(next.serial > first.serial, "the serial advanced");

    h.settle();
    assert_eq!(
        title_bar_pixel(&h.shot(), &c),
        to_rgb(Palette::dark().get(Role::TitleBarActive)),
        "the dark title bar"
    );

    drop(conn);
    h.quit();
}

#[test]
fn an_unfocused_title_bar_follows_the_scheme_too() {
    // Two windows: the second takes focus, so the first is drawn with
    // the *inactive* colours — a restyle that only handled the focused
    // window would leave a two-scheme desktop on screen.
    let h = Harness::start("inactive", "");
    let mut seen = Vec::new();
    let mut conn = h.client("inactive");
    let a = make_window(&mut conn, &mut seen, 1, 1);
    let a_conf = configure(&mut conn, &mut seen, a);
    let b = make_window(&mut conn, &mut seen, 10, 2);
    let b_conf = configure(&mut conn, &mut seen, b);
    assert_eq!(h.request_line("focus\n"), "ok");
    h.settle();

    let img = h.shot();
    let light = Palette::light();
    // Exactly one of them is active; which one is the WM's business, and
    // this test is about the *colours*, not the focus policy.
    let mut bars = [
        title_bar_pixel(&img, &a_conf),
        title_bar_pixel(&img, &b_conf),
    ];
    bars.sort_unstable();
    let mut want = [
        to_rgb(light.get(Role::TitleBarActive)),
        to_rgb(light.get(Role::TitleBarInactive)),
    ];
    want.sort_unstable();
    assert_eq!(bars, want, "both title bars are the light scheme's");

    let first = theme_of(&mut conn, &mut seen);
    h.rewrite_config("theme.scheme = dark\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    await_theme(&mut conn, &mut seen, first.serial);
    h.settle();

    let img = h.shot();
    let dark = Palette::dark();
    let mut bars = [
        title_bar_pixel(&img, &a_conf),
        title_bar_pixel(&img, &b_conf),
    ];
    bars.sort_unstable();
    let mut want = [
        to_rgb(dark.get(Role::TitleBarActive)),
        to_rgb(dark.get(Role::TitleBarInactive)),
    ];
    want.sort_unstable();
    assert_eq!(bars, want, "both title bars followed the scheme");

    drop(conn);
    h.quit();
}

#[test]
fn an_override_changes_exactly_one_role() {
    let h = Harness::start("override", "theme.scheme = dark\ntheme.accent = #ff0000\n");
    let mut seen = Vec::new();
    let mut conn = h.client("override");
    let theme = theme_of(&mut conn, &mut seen);
    let p = theme.palette();
    assert_eq!(p.get(Role::Accent), Color::rgb(0xff, 0, 0));
    // Everything else is untouched: an override that moved its
    // neighbours would be an off-by-one in the role table.
    let dark = Palette::dark();
    for role in Role::ALL {
        if *role == Role::Accent {
            continue;
        }
        assert_eq!(p.get(*role), dark.get(*role), "{}", role.key());
    }

    // And it is reported as such on the control socket, in a form that
    // could be pasted straight back into `server.conf`.
    let (_, rows) = h.theme();
    assert_eq!(rows.len(), Role::COUNT);
    let accent = rows
        .iter()
        .find(|(k, _)| k == "accent")
        .expect("an accent row");
    assert_eq!(accent.1, "#ff0000");

    drop(conn);
    h.quit();
}

#[test]
fn a_malformed_colour_is_ignored_and_the_rest_of_the_file_applies() {
    // The rule the whole config parser is built on: a bad line cannot
    // take the desktop down, and it cannot take the *good lines with it*
    // either.
    let h = Harness::start(
        "malformed",
        "theme.scheme = dark\ntheme.accent = chartreuse\ntheme.focus = #00ff00\n",
    );
    let mut seen = Vec::new();
    let mut conn = h.client("malformed");
    let p = theme_of(&mut conn, &mut seen).palette();
    assert_eq!(
        p.get(Role::Accent),
        Palette::dark().get(Role::Accent),
        "the unparseable colour was skipped, not guessed at"
    );
    assert_eq!(p.get(Role::Focus), Color::rgb(0, 0xff, 0));
    // The server is alive and serving, which is the actual assertion.
    assert_eq!(h.request_line("reload\n"), "ok");
    drop(conn);
    h.quit();
}

#[test]
fn nothing_is_sent_when_the_palette_did_not_change() {
    // A reload that moved something else must not cost a `Theme` on
    // every socket and a repaint of every decoration.
    let h = Harness::start("unchanged", "theme.scheme = dark\n");
    let mut seen = Vec::new();
    let mut conn = h.client("unchanged");
    let first = theme_of(&mut conn, &mut seen);
    h.settle();

    h.rewrite_config("theme.scheme = dark\nkeyboard.layout = us\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    wait_for("the reload to be counted", || h.stat("config_reloads") > 0);
    h.settle();

    // Drain whatever the socket has: there must be nothing newer.
    conn.flush().unwrap();
    let _ = conn.poll(&mut seen);
    let themes: Vec<u32> = seen
        .iter()
        .filter_map(|m| match m {
            ServerMsg::Theme(t) => Some(t.serial),
            _ => None,
        })
        .collect();
    assert_eq!(
        themes,
        vec![first.serial],
        "an unchanged palette is silence"
    );

    drop(conn);
    h.quit();
}

#[test]
fn deleting_the_config_file_restores_the_defaults() {
    // Issue #558: the watch used to ask for `CLOSE_WRITE|MOVED_TO|CREATE`
    // only, so `rm server.conf` was not an event at all and whatever the
    // file had said stayed in force until something else triggered a
    // reload. `rm` is the documented way back to defaults, so it has to
    // be one.
    let h = Harness::start("delete", "theme.scheme = dark\n");
    let mut seen = Vec::new();
    let mut conn = h.client("delete");
    let first = theme_of(&mut conn, &mut seen);
    assert_eq!(first.palette(), Palette::dark());
    h.settle();

    h.remove_config();

    // Asynchronous: poll to a deadline. A server that never notices
    // fails this in ten seconds, which is exactly the defect.
    let next = await_theme(&mut conn, &mut seen, first.serial);
    assert_eq!(
        next.palette(),
        Palette::light(),
        "a missing file means defaults"
    );
    let (status, _) = h.theme();
    assert!(status.starts_with("ok light "), "{status:?}");

    drop(conn);
    h.quit();
}

#[test]
fn an_atomic_rewrite_switches_the_scheme_without_a_control_request() {
    // What `nitro-settings`' Appearance section does: write a temp file
    // and rename it over `server.conf`. No `reload` anywhere.
    let h = Harness::start("inotify-scheme", "theme.scheme = light\n");
    let mut seen = Vec::new();
    let mut conn = h.client("inotify-scheme");
    let first = theme_of(&mut conn, &mut seen);
    assert_eq!(first.palette(), Palette::light());
    h.settle();

    h.rewrite_config("theme.scheme = dark\n");

    let next = await_theme(&mut conn, &mut seen, first.serial);
    assert_eq!(next.palette(), Palette::dark());

    drop(conn);
    h.quit();
}

#[test]
fn the_control_socket_prints_every_role_in_config_syntax() {
    let h = Harness::start("control", "");
    let (status, rows) = h.theme();
    assert!(status.starts_with("ok light "), "{status:?}");
    assert_eq!(rows.len(), Role::COUNT);
    let light = Palette::light();
    for (role, (key, value)) in Role::ALL.iter().zip(&rows) {
        assert_eq!(key, role.key(), "the table is in role order");
        assert_eq!(
            nitro_core::palette::parse_color(value),
            Some(light.get(*role)),
            "{key}"
        );
    }
    h.quit();
}
