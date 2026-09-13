//! `nitro-calc` driven by the real `hey` binary, over a real socket.
//!
//! This is the agentic-use demonstration, and it is the reason the
//! introspection socket exists: **nothing here reaches into the app**.
//! Every button press is `hey … do … click`, every reading is `hey … get
//! …`, and the process doing it is a separate binary that has never
//! linked `nitro-calc`.
//!
//! The commands below are the ones in the crate's README, run against a
//! real server on the fake backend rather than a mock \u2014 so a README that
//! went stale would fail here.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use nitro_calc::{Calc, build};
use nitro_ui::Size;
use nitro_ui::test::Harness;

/// The `hey` binary cargo just built.
fn hey() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hey"))
}

/// Run `hey <args>` against `dir`, pumping the app until it exits.
///
/// The app is served by *this* thread — that is the whole design — so a
/// blocking `hey` would deadlock without the pumping.
fn run(h: &mut Harness<Calc>, dir: &Path, args: &[&str]) -> Output {
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

/// Assert the command succeeded, showing its stderr if it did not.
fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "hey failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    stdout(o).trim().to_owned()
}

/// A calculator with its introspection socket open.
///
/// The window fits the harness's 320×240 output; see `tests/calc.rs` for
/// why that matters.
fn harness() -> (Harness<Calc>, PathBuf) {
    let mut h = Harness::sized("nitro-calc", Calc::new(), Size::new(240.0, 228.0), build);
    h.open_socket("nitro-calc");
    h.settle();
    let dir = h.socket_dir().expect("socket dir");
    (h, dir)
}

#[test]
fn hey_lists_the_calculator_and_its_tree() {
    let (mut h, dir) = harness();

    let listed = ok(&run(&mut h, &dir, &[]));
    assert!(
        listed.starts_with("nitro-calc"),
        "the app is listed: {listed:?}"
    );

    let tree = ok(&run(&mut h, &dir, &["nitro-calc", "list"]));
    for want in ["window", "display", "history", "equals", "plus"] {
        assert!(tree.contains(want), "`{want}` missing from:\n{tree}");
    }
    h.quit();
}

#[test]
fn hey_clicks_the_buttons_and_reads_the_answer() {
    // The spec's exact commands, and the whole claim of goal 5: an
    // outside process runs the app's real callbacks and reads the result
    // back through the same channel.
    let (mut h, dir) = harness();

    ok(&run(
        &mut h,
        &dir,
        &["nitro-calc", "do", "window/7", "click"],
    ));
    assert_eq!(
        ok(&run(
            &mut h,
            &dir,
            &["nitro-calc", "get", "window/display", "value"]
        )),
        "7",
        "the display followed the scripted click"
    );

    ok(&run(
        &mut h,
        &dir,
        &["nitro-calc", "do", "window/plus", "click"],
    ));
    ok(&run(
        &mut h,
        &dir,
        &["nitro-calc", "do", "window/8", "click"],
    ));
    ok(&run(
        &mut h,
        &dir,
        &["nitro-calc", "do", "window/equals", "click"],
    ));

    assert_eq!(
        ok(&run(
            &mut h,
            &dir,
            &["nitro-calc", "get", "window/display", "value"]
        )),
        "15"
    );
    assert_eq!(
        ok(&run(
            &mut h,
            &dir,
            &["nitro-calc", "get", "window/history", "value"]
        )),
        "7 + 8 =",
        "and the history line agrees"
    );
    assert_eq!(
        h.state().presses(),
        4,
        "the app's own callbacks ran, once each"
    );
    h.quit();
}

#[test]
fn hey_addresses_a_button_by_name_without_naming_the_layout() {
    // `window/7` rather than `window/container[2]/7`: the path names the
    // widget, so rearranging the keypad cannot break a script. The
    // spelled-out path still works, which is what makes it a fallback
    // rather than a replacement.
    let (mut h, dir) = harness();
    let tree = ok(&run(&mut h, &dir, &["nitro-calc", "list"]));
    let full = tree
        .lines()
        .map(|l| l.split('\t').next().unwrap_or_default())
        .find(|p| p.ends_with("/7"))
        .expect("a path ending in /7")
        .to_owned();
    assert!(
        full.matches('/').count() >= 2,
        "the canonical path goes through the row: {full}"
    );

    ok(&run(&mut h, &dir, &["nitro-calc", "do", &full, "click"]));
    assert_eq!(
        ok(&run(
            &mut h,
            &dir,
            &["nitro-calc", "get", "window/display", "value"]
        )),
        "7"
    );
    h.quit();
}

#[test]
fn hey_shot_writes_a_png_the_size_of_the_window() {
    let (mut h, dir) = harness();
    ok(&run(
        &mut h,
        &dir,
        &["nitro-calc", "do", "window/7", "click"],
    ));

    let out_dir = std::env::temp_dir().join(format!("calc-shot-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).unwrap();
    let png = out_dir.join("calc.png");
    ok(&run(
        &mut h,
        &dir,
        &["nitro-calc", "shot", "-o", png.to_str().unwrap()],
    ));

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
    h.quit();
}

#[test]
fn hey_reports_a_bad_path_as_an_error_rather_than_a_guess() {
    let (mut h, dir) = harness();
    let out = run(&mut h, &dir, &["nitro-calc", "get", "window/nowhere"]);
    assert_eq!(out.status.code(), Some(1), "an `err` reply is exit 1");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no such widget"),
        "and the app's message is passed through"
    );
    h.quit();
}
