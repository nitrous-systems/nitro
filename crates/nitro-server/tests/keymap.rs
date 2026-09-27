//! Keymap transfer (M5-C), end to end: a client that lists
//! `caps::KEYMAP` receives the server's compiled xkb keymap in a sealed
//! memfd and the four xkb modifier masks, and a client that does not sees
//! exactly what it saw before.
//!
//! The shape is `tests/config.rs`': the real [`run`] on a thread with the
//! fake backend, a configuration file of its own, real `nitro-wire`
//! clients and the v0 control socket. Every keymap-dependent test skips
//! (not fails) on a box with no `xkeyboard-config` data, as
//! `keyboard.rs`' own tests do.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::config::KeyboardSettings;
use nitro_server::input::{FakeInput, InputEvent};
use nitro_server::keyboard::{Keyboard, ModMasks};
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{Key, Modifiers, ServerMsg};
use nitro_wire::types::{ButtonState, KeymapFormat, Layer, NodeId, caps};
use xkbcommon::xkb;

const OUT: (u32, u32) = (640, 480);
const WIN: Size = Size::new(200.0, 120.0);

const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;
const KEY_RIGHTALT: u32 = 100;

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
    time_ns: u64,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    fn start(name: &str, conf: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-keymap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(&config_path, conf).expect("write server.conf");
        let mut config = Config::fake(OUT.0, OUT.1, &path);
        config.config_path = Some(config_path);
        let input = FakeInput::new().expect("eventfd");
        config.fake_input = Some(input.clone());
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
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

    /// Wait until every pushed input event has been handled: the server
    /// counts them, and the fake input's queue is drained in order.
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

    fn key(&mut self, keycode: u32, pressed: bool) {
        self.time_ns += 5_000_000;
        self.input.push(InputEvent::Key {
            keycode,
            pressed,
            time_ns: self.time_ns,
        });
    }

    fn quit(mut self) {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
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

/// Drain a client's socket until `f` matches, or time out. Matches from
/// the front, so the *first* matching message wins.
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

/// Read whatever the socket has right now.
fn drain(conn: &mut Connection, seen: &mut Vec<ServerMsg>) {
    conn.flush().unwrap();
    let _ = conn.poll(seen);
}

fn make_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: u32) -> NodeId {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    conn.tx()
        .create_window_with(root, "keymap", WIN, Layer::Normal, 0)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, Color::rgb(0xFF, 0, 0))
        .commit(1)
        .unwrap();
    conn.flush().unwrap();
    expect(conn, seen, "the first Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(()),
        _ => None,
    });
    expect(conn, seen, "focus", |m| match m {
        ServerMsg::Focus(f) if f.window == root && f.focused => Some(()),
        _ => None,
    });
    root
}

/// The keymap a `Keymap` message carried, compiled, after checking the
/// file is sealed, mappable and NUL-terminated within `size`.
fn compile_received(km: &nitro_wire::msg::Keymap) -> xkb::Keymap {
    assert_eq!(km.format, KeymapFormat::XkbV1);
    assert!(km.size > 0);
    let fd = rustix::io::dup(&km.fd).unwrap();
    nitro_shm::check_seals(std::os::fd::AsFd::as_fd(&fd)).expect("sealed");
    let len = usize::try_from(km.size).unwrap();
    let map = nitro_shm::Mapping::map(fd, len).expect("mappable");
    let bytes = map.as_bytes();
    assert_eq!(bytes.last(), Some(&0), "NUL-terminated within size");
    let text = std::str::from_utf8(&bytes[..len - 1]).unwrap().to_owned();
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(
        &ctx,
        text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("the received keymap compiles")
}

/// Connect, opt into `KEYMAP`, and return the first `Keymap`.
fn keymap_client(h: &Harness, name: &str, seen: &mut Vec<ServerMsg>) -> (Connection, xkb::Keymap) {
    let mut conn = h.client(name);
    assert!(conn.has_caps(caps::KEYMAP), "caps = {:#x}", conn.caps());
    conn.client_caps(caps::KEYMAP).unwrap();
    let km = {
        expect(&mut conn, seen, "a Keymap", |m| match m {
            ServerMsg::Keymap(_) => Some(()),
            _ => None,
        });
        let idx = seen
            .iter()
            .position(|m| matches!(m, ServerMsg::Keymap(_)))
            .unwrap();
        let ServerMsg::Keymap(km) = seen.remove(idx) else {
            unreachable!()
        };
        km
    };
    // Rule: a `Modifiers` snapshot follows every `Keymap`.
    let first = expect(&mut conn, seen, "the Modifiers after Keymap", |m| match m {
        ServerMsg::Modifiers(m) => Some(*m),
        _ => None,
    });
    assert_eq!(masks(first), ModMasks::default(), "nothing is held yet");
    seen.retain(|m| !matches!(m, ServerMsg::Modifiers(_)));
    (conn, compile_received(&km))
}

fn masks(m: Modifiers) -> ModMasks {
    ModMasks {
        depressed: m.depressed,
        latched: m.latched,
        locked: m.locked,
        group: m.group,
    }
}

fn has_keymap() -> bool {
    let ok = Keyboard::new().is_some();
    if !ok {
        eprintln!("no xkb keymap data on this box; skipping");
    }
    ok
}

#[test]
fn a_keymap_client_gets_a_keymap_it_can_compile() {
    if !has_keymap() {
        return;
    }
    let h = Harness::start("compile", "");
    let mut seen = Vec::new();
    let (conn, km) = keymap_client(&h, "compile", &mut seen);
    // Chromium's `xkb_modifier_converter.cc` looks these up by name; the
    // check is on the keymap the client *received*.
    for name in [
        "Shift", "Control", "Mod1", "Mod4", "Mod5", "Mod3", "Lock", "Mod2",
    ] {
        assert_ne!(km.mod_get_index(name), xkb::MOD_INVALID, "{name}");
    }
    // The repeat figures: nothing configured, so the default — which a
    // `KEYMAP` client repeats with itself, the server does not repeat
    // into it. Pinned in `tests/repeat.rs`.
    drop(conn);
    h.quit();
}

/// The masks the client receives are the ones the server's own
/// `Keyboard` computes, for a scripted sequence that holds, latches and
/// locks. `lv3:ralt_switch,lv3:caps_switch_latch` makes Caps Lock a
/// Level-3 *latch* while Right Alt is held, which is the one latching key
/// a stock keymap offers.
#[test]
fn the_masks_match_what_the_server_resolved() {
    const OPTIONS: &str = "lv3:ralt_switch,lv3:caps_switch_latch";
    let settings = KeyboardSettings {
        layout: Some("us".to_owned()),
        options: Some(OPTIONS.to_owned()),
        ..KeyboardSettings::default()
    };
    let Some(mut local) = Keyboard::with_settings(&settings) else {
        eprintln!("no xkb keymap data on this box; skipping");
        return;
    };
    let mut h = Harness::start(
        "masks",
        &format!("keyboard.layout = us\nkeyboard.options = {OPTIONS}\n"),
    );
    let mut seen = Vec::new();
    let (mut conn, _km) = keymap_client(&h, "masks", &mut seen);
    make_window(&mut conn, &mut seen, 1);
    seen.clear();

    let script: &[(u32, bool)] = &[
        (KEY_LEFTSHIFT, true),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_LEFTSHIFT, false),
        // Latch: Right Alt held (Level 3), then Caps (ISO_Level3_Latch).
        (KEY_RIGHTALT, true),
        (KEY_CAPSLOCK, true),
        (KEY_CAPSLOCK, false),
        (KEY_RIGHTALT, false),
        // The latch is consumed by the next key.
        (KEY_A, true),
        (KEY_A, false),
        // Lock: Num Lock on, and off again.
        (KEY_NUMLOCK, true),
        (KEY_NUMLOCK, false),
        (KEY_A, true),
        (KEY_A, false),
        (KEY_NUMLOCK, true),
        (KEY_NUMLOCK, false),
    ];
    let mut saw_latched = false;
    let mut saw_locked = false;
    for &(code, pressed) in script {
        local.key(code, pressed);
        let want = local.mod_masks();
        saw_latched |= want.latched != 0;
        saw_locked |= want.locked != 0;
        h.key(code, pressed);
        h.settle();
        drain(&mut conn, &mut seen);
        let key = expect(&mut conn, &mut seen, "the Key", |m| match m {
            ServerMsg::Key(k) => Some(k.clone()),
            _ => None,
        });
        assert_eq!(
            (key.keycode, key.state == ButtonState::Pressed),
            (code, pressed)
        );
        // Newest `Modifiers`, or the previous state when nothing moved.
        let got = seen.iter().rev().find_map(|m| match m {
            ServerMsg::Modifiers(m) => Some(masks(*m)),
            _ => None,
        });
        if let Some(got) = got {
            assert_eq!(got, want, "after {code} {pressed}");
        }
        // `Key.mods` is the effective (post-event) mask, the same state
        // the following `Modifiers` describes.
        assert_eq!(
            key.mods,
            want.depressed | want.latched | want.locked,
            "Key.mods after {code} {pressed}"
        );
        seen.clear();
    }
    assert!(saw_latched, "the script never latched anything");
    assert!(saw_locked, "the script never locked anything");
    drop(conn);
    h.quit();
}

/// A `Modifiers` change never arrives when nothing moved, and every change
/// arrives: after the whole sequence the client's last masks equal the
/// server's.
#[test]
fn modifiers_follow_the_key_that_caused_them() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("order", "");
    let mut seen = Vec::new();
    let (mut conn, km) = keymap_client(&h, "order", &mut seen);
    make_window(&mut conn, &mut seen, 1);
    seen.clear();
    let shift_bit = 1u32 << km.mod_get_index("Shift");

    h.key(KEY_LEFTSHIFT, true);
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.key(KEY_LEFTSHIFT, false);
    h.settle();
    drain(&mut conn, &mut seen);
    expect(&mut conn, &mut seen, "the Shift release", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_LEFTSHIFT && k.state == ButtonState::Released => {
            Some(())
        }
        _ => None,
    });
    drain(&mut conn, &mut seen);

    let pos = |f: &dyn Fn(&ServerMsg) -> bool| seen.iter().position(f).unwrap();
    let shift_down = pos(
        &|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_LEFTSHIFT && k.state == ButtonState::Pressed),
    );
    let held = pos(&|m| matches!(m, ServerMsg::Modifiers(m) if m.depressed & shift_bit != 0));
    let a_down = pos(
        &|m| matches!(m, ServerMsg::Key(k) if k.keycode == KEY_A && k.state == ButtonState::Pressed),
    );
    assert!(shift_down < held, "Key, then the Modifiers it caused");
    assert!(held < a_down);
    let ServerMsg::Key(a) = &seen[a_down] else {
        unreachable!()
    };
    assert_eq!(a.keysym, xkb::keysyms::KEY_A, "the `a` was shifted");
    // Two changes (Shift down, Shift up), not one per key.
    let count = seen
        .iter()
        .filter(|m| matches!(m, ServerMsg::Modifiers(_)))
        .count();
    assert_eq!(count, 2, "{seen:?}");
    let last = seen.iter().rev().find_map(|m| match m {
        ServerMsg::Modifiers(m) => Some(masks(*m)),
        _ => None,
    });
    assert_eq!(last, Some(ModMasks::default()));
    drop(conn);
    h.quit();
}

/// The spec's "byte-identical behaviour to today": a client that never
/// sends `ClientCaps` gets no `Keymap`/`Modifiers`, and its `Key` is field
/// for field the one a `KEYMAP` client gets for the same keystroke.
#[test]
fn a_client_that_never_asked_sees_exactly_what_it_saw_before() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("plain", "");
    let mut seen_km = Vec::new();
    let (mut km_conn, _) = keymap_client(&h, "with-keymap", &mut seen_km);
    make_window(&mut km_conn, &mut seen_km, 1);
    seen_km.clear();
    let press = |h: &mut Harness| {
        h.key(KEY_LEFTSHIFT, true);
        h.key(KEY_A, true);
        h.key(KEY_A, false);
        h.key(KEY_LEFTSHIFT, false);
        h.settle();
    };
    press(&mut h);
    drain(&mut km_conn, &mut seen_km);

    let mut seen_plain = Vec::new();
    let mut plain = h.client("plain");
    make_window(&mut plain, &mut seen_plain, 1);
    press(&mut h);
    drain(&mut plain, &mut seen_plain);
    expect(&mut plain, &mut seen_plain, "the last key", |m| match m {
        ServerMsg::Key(k) if k.keycode == KEY_LEFTSHIFT && k.state == ButtonState::Released => {
            Some(())
        }
        _ => None,
    });

    assert!(
        !seen_plain
            .iter()
            .any(|m| matches!(m, ServerMsg::Keymap(_) | ServerMsg::Modifiers(_))),
        "{seen_plain:?}"
    );
    let keys = |seen: &[ServerMsg]| -> Vec<(u32, ButtonState, u32, u32, String)> {
        seen.iter()
            .filter_map(|m| match m {
                ServerMsg::Key(Key {
                    keycode,
                    state,
                    mods,
                    keysym,
                    utf8,
                    ..
                }) => Some((*keycode, *state, *mods, *keysym, utf8.clone())),
                _ => None,
            })
            .collect()
    };
    let a = keys(&seen_km);
    assert_eq!(a.len(), 4, "{seen_km:?}");
    assert_eq!(a, keys(&seen_plain));
    drop(plain);
    drop(km_conn);
    h.quit();
}

/// nitro sends raw evdev codes; Chromium adds the X11 `+8` itself
/// (`keycode_converter.cc`). A `KEYMAP` client is no exception.
#[test]
fn raw_evdev_keycodes_are_unchanged() {
    if !has_keymap() {
        return;
    }
    let mut h = Harness::start("evdev", "");
    let mut seen = Vec::new();
    let (mut conn, km) = keymap_client(&h, "evdev", &mut seen);
    make_window(&mut conn, &mut seen, 1);
    h.key(KEY_A, true);
    h.key(KEY_A, false);
    h.settle();
    let code = expect(&mut conn, &mut seen, "a Key", |m| match m {
        ServerMsg::Key(k) => Some(k.keycode),
        _ => None,
    });
    assert_eq!(code, KEY_A);
    // ...and the received keymap names that key `+8`.
    let state = xkb::State::new(&km);
    assert_eq!(
        state.key_get_one_sym(xkb::Keycode::new(code + 8)).raw(),
        xkb::keysyms::KEY_a
    );
    drop(conn);
    h.quit();
}
