//! Key repeat, end to end: the real [`run`] on a thread with the fake
//! backend, fake input and real `nitro-wire` clients — the shape of
//! `tests/keymap.rs`. Every test skips (not fails) on a box with no
//! `xkeyboard-config` data, because without a keymap nothing repeats.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::keyboard::Keyboard;
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ButtonState, Layer, NodeId, caps, mod_mask};

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);

const KEY_Q: u32 = 16;
const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

/// Fast enough that a test waits a fraction of a second, slow enough that
/// scheduling noise on a loaded CI box cannot fake a repeat that is not
/// there: 150 ms, then every 20 ms.
const FAST: &str = "keyboard.repeat = 150,50\n";

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
    input: FakeInput,
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, conf: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-repeat-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(&config_path, conf).expect("write server.conf");
        let mut config = Config::fake(OUT.0, OUT.1, &path);
        config.config_path = Some(config_path.clone());
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
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
            input,
            time_ns: 1_000_000,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        wait_for("the shell socket", || h.shell_path.exists());
        h
    }

    fn shell(&self, name: &str) -> Connection {
        Connection::connect(&self.shell_path, name).expect("shell connect")
    }

    fn client(&self, name: &str) -> Connection {
        Connection::connect(&self.wire_path, name).expect("wire connect")
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

    /// Push a key and wait until the server has routed it. Routing is
    /// visible in the stats as `key_repeating` only for a repeating key,
    /// so this waits on the input queue instead: a `stats` round trip is
    /// answered after every event already on the fd was dispatched.
    fn key(&mut self, keycode: u32, pressed: bool) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::Key {
            keycode,
            pressed,
            time_ns: self.time_ns,
        });
        // Two round trips: the first may race the eventfd wakeup.
        let _ = self.request_text("stats\n");
        let _ = self.request_text("stats\n");
    }

    /// A one-line request (`reload`, `quit`): the answer, then the server
    /// closes the connection.
    fn request_line(&self, req: &str) -> String {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        line.trim_end_matches('\n').to_owned()
    }

    fn rewrite_config(&self, conf: &str) {
        let tmp = self.config_dir.join("server.conf.tmp");
        std::fs::write(&tmp, conf).expect("write temp");
        std::fs::rename(&tmp, &self.config_path).expect("rename into place");
    }

    fn quit(mut self) {
        assert_eq!(self.request_line("quit\n"), "ok");
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Drain a client's socket until `f` matches, or time out.
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

/// Read whatever the socket has for `ms` milliseconds.
fn collect(conn: &mut Connection, seen: &mut Vec<ServerMsg>, ms: u64) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        conn.flush().unwrap();
        let _ = conn.poll(seen);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn make_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: u32) -> NodeId {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, "repeat", WIN, Layer::Normal, 0)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, Color::rgb(0xFF, 0, 0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    expect(conn, seen, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == root && f.focused => Some(()),
        _ => None,
    });
    root
}

/// Presses of `keycode` delivered to `window`.
fn presses(seen: &[ServerMsg], window: NodeId, keycode: u32) -> Vec<nitro_wire::msg::Key> {
    seen.iter()
        .filter_map(|m| match m {
            ServerMsg::Key(k)
                if k.window == window
                    && k.keycode == keycode
                    && k.state == ButtonState::Pressed =>
            {
                Some(k.clone())
            }
            _ => None,
        })
        .collect()
}

fn has_keymap() -> bool {
    let ok = Keyboard::new().is_some();
    if !ok {
        eprintln!("no xkb keymap data on this box; skipping");
    }
    ok
}

#[test]
fn a_held_key_repeats_and_stops_on_release() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("held", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("held");
    let win = make_window(&mut conn, &mut seen, 1);

    h.key(KEY_A, true);
    assert_eq!(h.stat("key_repeating"), 1);
    collect(&mut conn, &mut seen, 400);
    let held = presses(&seen, win, KEY_A);
    // 1 real press, then repeats from 150 ms every 20 ms: ~13 by 400 ms.
    // The lower bound is what matters, and is far below that.
    assert!(held.len() >= 4, "only {} presses: {seen:?}", held.len());
    for k in &held {
        assert_eq!(k.utf8, "a");
    }
    // Each repeat is a fresh event, not a replay.
    assert!(held.windows(2).skip(1).all(|p| p[1].time_ns > p[0].time_ns));

    h.key(KEY_A, false);
    assert_eq!(h.stat("key_repeating"), 0);
    seen.clear();
    collect(&mut conn, &mut seen, 250);
    assert!(
        presses(&seen, win, KEY_A).is_empty(),
        "repeated after release: {seen:?}"
    );
    assert!(h.stat("key_repeats") >= 3);

    drop(conn);
    h.quit();
}

#[test]
fn modifiers_do_not_repeat_but_a_key_held_under_one_repeats_shifted() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("mods", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("mods");
    let win = make_window(&mut conn, &mut seen, 1);

    h.key(KEY_LEFTSHIFT, true);
    assert_eq!(h.stat("key_repeating"), 0, "Shift must not repeat");
    collect(&mut conn, &mut seen, 300);
    assert_eq!(presses(&seen, win, KEY_LEFTSHIFT).len(), 1, "{seen:?}");
    h.key(KEY_LEFTSHIFT, false);

    // Hold `a`, then add Shift: the repeat continues, now as `A`.
    seen.clear();
    h.key(KEY_A, true);
    collect(&mut conn, &mut seen, 250);
    h.key(KEY_LEFTSHIFT, true);
    assert_eq!(
        h.stat("key_repeating"),
        1,
        "a modifier press does not stop it"
    );
    seen.clear();
    collect(&mut conn, &mut seen, 150);
    let shifted = presses(&seen, win, KEY_A);
    assert!(!shifted.is_empty(), "{seen:?}");
    // A repeat written just before Shift was routed may still be in the
    // socket as `a`; from the first `A` on, every repeat is shifted.
    let first_upper = shifted
        .iter()
        .position(|k| k.utf8 == "A")
        .unwrap_or_else(|| panic!("no shifted repeat: {shifted:?}"));
    assert!(first_upper <= 1, "{shifted:?}");
    assert!(
        shifted[first_upper..].iter().all(|k| k.utf8 == "A"),
        "{shifted:?}"
    );
    h.key(KEY_A, false);
    h.key(KEY_LEFTSHIFT, false);

    drop(conn);
    h.quit();
}

#[test]
fn a_held_compositor_hotkey_fires_once() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("hotkey", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("hotkey");
    let win = make_window(&mut conn, &mut seen, 1);

    // Super+Q, held for many repeat periods: one `Closed`, not a stream.
    h.key(KEY_LEFTMETA, true);
    h.key(KEY_Q, true);
    assert_eq!(h.stat("key_repeating"), 0);
    collect(&mut conn, &mut seen, 400);
    let closed = seen
        .iter()
        .filter(|m| matches!(m, ServerMsg::Closed(c) if c.window == win))
        .count();
    assert_eq!(closed, 1, "{seen:?}");
    assert!(
        presses(&seen, win, KEY_Q).is_empty(),
        "the chord is not the client's"
    );
    h.key(KEY_Q, false);
    h.key(KEY_LEFTMETA, false);

    drop(conn);
    h.quit();
}

#[test]
fn a_focus_change_stops_the_repeat() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("focus", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("focus");
    let first = make_window(&mut conn, &mut seen, 1);

    h.key(KEY_A, true);
    collect(&mut conn, &mut seen, 250);
    assert!(presses(&seen, first, KEY_A).len() >= 2);

    // A new window takes the focus while `a` is still held.
    let mut other_seen = Vec::new();
    let mut other = h.client("focus-2");
    let second = make_window(&mut other, &mut other_seen, 10);
    assert_eq!(h.stat("key_repeating"), 0, "focus moved: the repeat ends");
    other_seen.clear();
    collect(&mut conn, &mut seen, 250);
    collect(&mut other, &mut other_seen, 50);
    // A repeat already written before the focus moved may still be in
    // the socket; what matters is that none follows the `Focus(false)`.
    let blurred = seen
        .iter()
        .position(|m| matches!(m, ServerMsg::Focus(f) if f.window == first && !f.focused))
        .expect("the first window was told it lost focus");
    let after = &seen[blurred..];
    assert!(presses(after, first, KEY_A).is_empty(), "{seen:?}");
    assert!(
        presses(&other_seen, second, KEY_A).is_empty(),
        "the new window must not start receiving a key it never saw pressed"
    );
    h.key(KEY_A, false);

    drop(other);
    drop(conn);
    h.quit();
}

#[test]
fn rate_zero_disables_and_a_reload_applies_it() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("reload", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("reload");
    let win = make_window(&mut conn, &mut seen, 1);

    h.key(KEY_A, true);
    collect(&mut conn, &mut seen, 250);
    h.key(KEY_A, false);
    assert!(
        presses(&seen, win, KEY_A).len() >= 2,
        "on before the reload"
    );

    // Drain any repeat that was in flight at the release.
    collect(&mut conn, &mut seen, 50);
    h.rewrite_config("keyboard.repeat = 0,0\n");
    assert_eq!(h.request_line("reload\n"), "ok");

    seen.clear();
    h.key(KEY_A, true);
    assert_eq!(h.stat("key_repeating"), 0);
    collect(&mut conn, &mut seen, 300);
    assert_eq!(
        presses(&seen, win, KEY_A).len(),
        1,
        "off after it: {seen:?}"
    );
    h.key(KEY_A, false);

    drop(conn);
    h.quit();
}

#[test]
fn a_keymap_client_is_told_the_rate_and_repeats_for_itself() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("keymap", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("keymap");
    assert!(conn.has_caps(caps::KEYMAP));
    conn.client_caps(caps::KEYMAP).unwrap();
    let (rate, delay) = expect(&mut conn, &mut seen, "a Keymap", |m| match m {
        ServerMsg::Keymap(k) => Some((k.rate_hz, k.delay_ms)),
        _ => None,
    });
    assert_eq!((rate, delay), (50, 150), "the configured figures");
    let win = make_window(&mut conn, &mut seen, 1);

    seen.clear();
    h.key(KEY_A, true);
    assert_eq!(
        h.stat("key_repeating"),
        0,
        "no server repeat into a KEYMAP client"
    );
    collect(&mut conn, &mut seen, 300);
    assert_eq!(presses(&seen, win, KEY_A).len(), 1, "{seen:?}");
    h.key(KEY_A, false);

    // A reload that only changes the repeat re-sends `Keymap` with the
    // new figures, and does not reset anything else.
    seen.clear();
    h.rewrite_config("keyboard.repeat = 400,30\n");
    assert_eq!(h.request_line("reload\n"), "ok");
    let (rate, delay) = expect(&mut conn, &mut seen, "a second Keymap", |m| match m {
        ServerMsg::Keymap(k) => Some((k.rate_hz, k.delay_ms)),
        _ => None,
    });
    assert_eq!((rate, delay), (30, 400));

    drop(conn);
    h.quit();
}

#[test]
fn an_idle_server_is_not_woken_by_the_repeat_timer() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("idle", FAST);
    let mut seen = Vec::new();
    let mut conn = h.client("idle");
    let _win = make_window(&mut conn, &mut seen, 1);
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    let before = h.stat("key_repeats");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(h.stat("key_repeats"), before);
    assert_eq!(h.stat("key_repeating"), 0);
    drop(conn);
    h.quit();
}

#[test]
fn a_release_withheld_for_a_shell_still_ends_the_repeat() {
    // The tripwire for where the cancel lives. A shell's binding fires
    // while `a` is held; the release of `a` then arrives inside the
    // withheld window and is *not delivered*. The repeat must end anyway:
    // the cancel sits at the top of `route_key`, before every early
    // return. Were it moved onto the delivery path, the release would be
    // swallowed with the repeat still armed, and once the shell answered
    // the timer would type `a` into the app for ever.
    //
    // A long delay (400 ms) so the whole sequence happens before the
    // first repeat is due: the per-fire backstop never gets a chance to
    // stop it for us, and the only thing that can is the release.
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("withheld", "keyboard.repeat = 400,5\n");
    let mut seen = Vec::new();
    let mut app = h.client("withheld-app");
    let win = make_window(&mut app, &mut seen, 1);

    let mut shell = h.shell("withheld-shell");
    let mut shell_seen = Vec::new();
    shell.bind_key(9, mod_mask::SUPER, 0).unwrap();
    shell.flush().unwrap();
    wait_for("the binding", || h.stat("hotkeys") == 1);
    let withheld_before = h.stat("keys_withheld");

    h.key(KEY_A, true);
    assert_eq!(h.stat("key_repeating"), 1, "armed");
    // The bare-Super tap: the binding fires on the Super release.
    h.key(KEY_LEFTMETA, true);
    h.key(KEY_LEFTMETA, false);
    h.key(KEY_A, false);
    assert!(
        h.stat("keys_withheld") > withheld_before,
        "the release of `a` was withheld (else this test proves nothing)"
    );
    assert_eq!(
        h.stat("key_repeating"),
        0,
        "the withheld release still cancelled"
    );

    // The shell answers; ordinary routing resumes.
    expect(&mut shell, &mut shell_seen, "the HotKey", |m| match m {
        ServerMsg::HotKey(k) if k.id == 9 => Some(()),
        _ => None,
    });
    shell.commit(1).unwrap();
    shell.flush().unwrap();

    // The original press may still be unread in the socket; it carries
    // the harness's input clock. A synthesised repeat is stamped with the
    // monotonic clock, far beyond it.
    collect(&mut app, &mut seen, 700);
    let repeats: Vec<_> = presses(&seen, win, KEY_A)
        .into_iter()
        .filter(|k| k.time_ns > h.time_ns)
        .collect();
    assert!(
        repeats.is_empty(),
        "`a` kept repeating after its release: {repeats:?}"
    );
    assert_eq!(h.stat("key_repeating"), 0);

    drop(shell);
    drop(app);
    h.quit();
}
