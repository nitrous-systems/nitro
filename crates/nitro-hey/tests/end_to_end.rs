//! `hey` against a real app: a real server on the fake backend, a real
//! widget tree, a real Unix socket, and the real `hey` binary as a child
//! process.
//!
//! Nothing here reaches into the app. Every assertion is made by asking
//! `hey`, and every change is made by telling `hey` — which is the claim
//! the introspection socket exists to support.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use nitro_core::Size;
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Label, button, column, label, text_field};
use nitro_ui::{Ui, WidgetId};

/// The app under test.
struct S {
    clicks: u32,
    message: Option<WidgetId>,
}

fn build(ui: &mut Ui<S>) -> WidgetId {
    let message = ui.build(label("before").name("message"));
    let ok = ui.build(
        button("OK")
            .name("ok")
            .on_click(|s: &mut S, ui: &mut Ui<S>| {
                s.clicks += 1;
                if let Some(m) = s.message
                    && let Ok(mut l) = ui.widget_mut::<Label>(m)
                {
                    l.set_text("after");
                }
            }),
    );
    let root = ui.build(
        column()
            .gap(6.0)
            .padding(8.0)
            .child(text_field("").name("input").placeholder("type")),
    );
    ui.attach(root, message).unwrap();
    ui.attach(root, ok).unwrap();
    root
}

/// The `hey` binary cargo just built.
fn hey() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hey"))
}

/// Run `hey <args>` against `dir`, pumping the app until it exits.
///
/// The app is served by *this* thread — that is the whole design — so a
/// blocking `hey` would deadlock without the pumping.
fn run(h: &mut Harness<S>, dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(hey())
        .args(args)
        .env("NITRO_APPS_DIR", dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hey");
    h.pump_socket_until(&format!("hey {args:?}"), |_| {
        matches!(child.try_wait(), Ok(Some(_)))
    });
    child.wait_with_output().expect("hey output")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn harness() -> (Harness<S>, PathBuf) {
    let mut h = Harness::sized(
        "dialog",
        S {
            clicks: 0,
            message: None,
        },
        Size::new(240.0, 160.0),
        build,
    );
    let root = h.ui().root().unwrap();
    let message = h.ui().children(root)[1];
    h.state_mut().message = Some(message);
    h.open_socket("dialog");
    h.settle();
    let dir = h.socket_dir().expect("socket dir");
    (h, dir)
}

#[test]
fn hey_lists_the_running_app() {
    let (mut h, dir) = harness();
    let out = run(&mut h, &dir, &[]);
    assert!(out.status.success());
    let line = stdout(&out);
    assert!(line.starts_with("dialog"), "listed apps: {line:?}");
    assert!(
        line.split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u32>()
            .is_ok(),
        "with a pid: {line:?}"
    );
}

#[test]
fn hey_lists_the_tree_and_gets_a_value() {
    let (mut h, dir) = harness();
    let out = run(&mut h, &dir, &["dialog", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = stdout(&out);
    for want in ["window", "window/input", "window/message", "window/ok"] {
        assert!(body.contains(want), "`{want}` missing from:\n{body}");
    }

    let out = run(&mut h, &dir, &["dialog", "get", "window/message", "value"]);
    assert_eq!(stdout(&out).trim(), "before");

    // An app-name prefix is enough.
    let out = run(&mut h, &dir, &["dia", "get", "window/message", "value"]);
    assert_eq!(stdout(&out).trim(), "before");
}

#[test]
fn hey_do_click_flips_the_label_and_get_proves_it() {
    // The headline claim: an outside process runs the app's real
    // callback, and reads the result back through the same channel.
    let (mut h, dir) = harness();
    assert_eq!(
        stdout(&run(
            &mut h,
            &dir,
            &["dialog", "get", "window/message", "value"]
        ))
        .trim(),
        "before"
    );

    let out = run(&mut h, &dir, &["dialog", "do", "window/ok", "click"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    h.settle();
    assert_eq!(h.state().clicks, 1, "the app's own callback ran");

    let out = run(&mut h, &dir, &["dialog", "get", "window/message", "value"]);
    assert_eq!(stdout(&out).trim(), "after");
}

#[test]
fn hey_set_changes_a_text_field() {
    let (mut h, dir) = harness();
    let out = run(
        &mut h,
        &dir,
        &["dialog", "set", "window/input", "text", "hello there"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&mut h, &dir, &["dialog", "get", "window/input", "value"]);
    assert_eq!(
        stdout(&out).trim(),
        "hello there",
        "a value with a space in it survived the round trip"
    );
}

#[test]
fn hey_watch_sees_a_change_made_by_another_client() {
    let (mut h, dir) = harness();
    let mut watcher = Command::new(hey())
        .args(["dialog", "watch", "*"])
        .env("NITRO_APPS_DIR", &dir)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the watcher");

    // The watcher has to be connected and registered before the change,
    // or there is nothing for it to notice.
    for _ in 0..50 {
        h.serve_socket();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    run(&mut h, &dir, &["dialog", "do", "window/ok", "click"]);
    // Let the event reach the watcher, then stop it and read what it saw.
    for _ in 0..50 {
        h.serve_socket();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = watcher.kill();
    let out = watcher.wait_with_output().expect("watcher output");
    let seen = String::from_utf8_lossy(&out.stdout);
    assert!(
        seen.contains("event window/message value after"),
        "the watcher missed the change; it saw:\n{seen}"
    );
}

#[test]
fn hey_shot_writes_a_png_the_size_of_the_window() {
    let (mut h, dir) = harness();
    let out_dir = std::env::temp_dir().join(format!("hey-shot-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).unwrap();
    let png = out_dir.join("window.png");
    let out = run(
        &mut h,
        &dir,
        &["dialog", "shot", "-o", png.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bytes = std::fs::read(&png).expect("the PNG");
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'],
        "it is a PNG"
    );
    // IHDR's width and height are the first two big-endian u32s of the
    // first chunk's data, which starts at byte 16.
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let hgt = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    let size = h.ui().window_size();
    assert_eq!(
        (w, hgt),
        (size.w as u32, size.h as u32),
        "the PNG is exactly this window, not the whole output"
    );
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn hey_reports_an_error_reply_as_exit_1_and_a_missing_app_as_exit_2() {
    let (mut h, dir) = harness();
    let out = run(&mut h, &dir, &["dialog", "get", "window/nowhere"]);
    assert_eq!(out.status.code(), Some(1), "an `err` reply is exit 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no such widget"),
        "and the app's message is passed through"
    );

    let out = run(&mut h, &dir, &["nosuchapp", "list"]);
    assert_eq!(out.status.code(), Some(2), "no such app is exit 2");

    let out = run(&mut h, &dir, &["dialog", "list"]);
    assert_eq!(out.status.code(), Some(0), "and a good request is exit 0");
}

#[test]
fn a_stale_socket_next_to_a_live_app_is_pruned_rather_than_ambiguous() {
    // #536/#544: a SIGTERMed or SIGKILLed app never unlinks its socket,
    // and `nitro-session` restarts the piece that died — so after a few
    // crashes the directory holds one live socket and several ghosts,
    // and `hey nitro-bar list` refuses to run because the name is
    // "ambiguous" between apps that do not exist.
    //
    // The ghosts are planted rather than killed for real, and there are
    // two kinds, because the check has two halves:
    //
    //   * a pid far above the kernel's maximum — `/proc/<pid>` absent,
    //     which is the common case and the cheap half;
    //   * pid 1, which is alive by definition, whose socket is a plain
    //     file nothing listens on — which refuses the connection with
    //     ECONNREFUSED, and is the pid-reuse case the `connect` half
    //     exists to catch.
    const DEAD: u32 = u32::MAX - 11;
    let (mut h, dir) = harness();
    let live = dir.join(format!("dialog.{}.sock", std::process::id()));
    assert!(live.exists(), "the app under test is listening");

    let ghost = dir.join(format!("dialog.{DEAD}.sock"));
    std::fs::write(&ghost, b"").unwrap();
    let refused = dir.join("dialog.1.sock");
    std::fs::write(&refused, b"").unwrap();

    // The name resolves to the one live app: no "ambiguous", exit 0.
    let out = run(&mut h, &dir, &["dialog", "get", "window/message", "value"]);
    assert!(
        out.status.success(),
        "stale sockets made the name ambiguous: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out).trim(), "before");

    // And the ghosts are gone, so the directory heals rather than
    // accumulating until someone notices.
    assert!(!ghost.exists(), "a dead pid's socket is unlinked");
    assert!(!refused.exists(), "and so is one nothing answers on");
    assert!(live.exists(), "the live app's socket is untouched");

    // A bare `hey` lists what is actually running, which is the same
    // truth in the other direction.
    std::fs::write(&ghost, b"").unwrap();
    let out = run(&mut h, &dir, &[]);
    let listing = stdout(&out);
    assert_eq!(
        listing.lines().count(),
        1,
        "only the live app is listed:\n{listing}"
    );
    assert!(!listing.contains(&DEAD.to_string()), "{listing}");
    assert!(!ghost.exists(), "and listing pruned it too");
}

#[test]
fn two_live_copies_are_ambiguous_and_name_dot_pid_picks_one() {
    // The other side of the coin: pruning must not make `hey` guess
    // between apps that really are both there. Two copies of one name
    // are still an error — and the error now tells you the selector
    // that resolves it.
    let (mut h, dir) = harness();
    let mine = std::process::id();
    // A second *live* `dialog`: a real listener owned by pid 1, which
    // is alive by definition, so both halves of the liveness test pass.
    // It need not speak the protocol; `find` decides before anything is
    // sent.
    let twin = dir.join("dialog.1.sock");
    let twin_listener = std::os::unix::net::UnixListener::bind(&twin).unwrap();

    let out = run(&mut h, &dir, &["dialog", "list"]);
    assert_eq!(out.status.code(), Some(2), "two live apps is no such app");
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("ambiguous"), "{err}");
    assert!(
        err.contains(&format!("dialog.{mine}")),
        "and names them as selectors: {err}"
    );

    // Which is then usable verbatim.
    let out = run(
        &mut h,
        &dir,
        &[&format!("dialog.{mine}"), "get", "window/message", "value"],
    );
    assert!(
        out.status.success(),
        "`name.pid` picks one: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out).trim(), "before");

    drop(twin_listener);
    let _ = std::fs::remove_file(&twin);
}
