//! The settings dialog, driven through a real server.
//!
//! Every test here builds the tree the binary builds
//! ([`nitro_settings::build`]) on a real connection to a real server, and
//! asserts on what the widgets then say or what landed on disk. Nothing
//! pokes the state directly to set up a case the wire would not produce:
//! the display rows arrive as real `OutputInfo` events from the harness's
//! own fake output, which is the path the bugs live in.
//!
//! Two things are injected rather than faked globally, and both for the
//! same reason — `std::env::set_var` is `unsafe` and process-global, so a
//! test that set `PATH` or `NITRO_CONFIG` would be setting it for every
//! other test in this binary at once. The config path comes in through
//! [`Settings::with_config_path`] and the audio search path through
//! [`Settings::with_audio_dirs`], the way `nitro-bar` injects its sensor
//! source.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nitro_settings::{Settings, build, conf, names};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Checkbox, Label, Slider, TextField};
use nitro_ui::{Size, WidgetId};

/// The window the dialog opens in these tests.
///
/// The harness runs a 320×240 output and the server decorates a window
/// with a 28 px title bar and a 1 px border, so anything much larger has
/// its bottom rows off-screen — and a click on a widget down there lands
/// on whatever the pointer clamp leaves under it. 300×210 fits, and the
/// rows shrink to it the way a real `Configure` to a small screen makes
/// them, which is the awkward case rather than a soft one.
const WINDOW: Size = Size::new(300.0, 210.0);

/// The connector the harness's fake output reports.
const CONNECTOR: &str = "Virtual-1";

/// A scratch directory of this test's own.
///
/// The name is deliberately free of shell metacharacters: `ThreadId(2)`
/// is what `{:?}` on a thread id prints, and an unquoted `(` in the path
/// a generated `/bin/sh` script writes to is a syntax error that costs an
/// afternoon. The thread's *number* is enough to keep two tests apart.
fn scratch(what: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "nitro-settings-test-{}-{what}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A dialog on the shell socket, with its config file in `dir`.
///
/// `Harness::shell` is how a test gets a connection carrying
/// `caps::SHELL`, and it takes a `Surface` because a bar always has one.
/// The dialog passes [`nitro_settings::ORDINARY_WINDOW`] — `Normal`
/// layer, no flags, no anchor, no zone — which is exactly the window the
/// binary opens by connecting with `App::shell` and never calling
/// `App::surface`.
fn harness_in(dir: &Path, state: Settings) -> Harness<Settings> {
    let path = dir.join(conf::FILE_NAME);
    Harness::shell(
        "nitro-settings",
        state.with_config_path(path),
        nitro_settings::ORDINARY_WINDOW,
        Some(WINDOW),
        build,
    )
}

/// The common case: a dialog with no audio backend and a fresh file.
fn harness(dir: &Path) -> Harness<Settings> {
    let mut h = harness_in(dir, Settings::new().with_audio_dirs(Vec::new()));
    // The output list is a subscription, so the rows arrive a round trip
    // after the tree is built rather than inside it.
    h.wait_for("the display rows", |h| !h.state().connectors().is_empty());
    h.settle();
    h
}

/// The widget at `path`, found the way `hey` finds it.
fn named(h: &mut Harness<Settings>, path: &str) -> WidgetId {
    nitro_ui::introspect::resolve(h.ui(), &format!("window/{path}"))
        .unwrap_or_else(|| panic!("no widget at {path}"))
}

/// A text field's contents.
fn field(h: &mut Harness<Settings>, path: &str) -> String {
    let id = named(h, path);
    h.widget::<TextField<Settings>>(id).text().to_owned()
}

/// Type `text` into the field at `path`, replacing what is there.
///
/// Through the introspection path, which is what `hey` drives — so a
/// scripted setting and a test setting go through the same code.
fn set_field(h: &mut Harness<Settings>, path: &str, text: &str) {
    set_value(h, path, text);
}

/// Set a slider's value the way `hey set … value` does.
fn set_value(h: &mut Harness<Settings>, path: &str, value: &str) {
    let (ui, state) = h.parts();
    nitro_ui::introspect::set(ui, state, &format!("window/{path}"), "value", value)
        .unwrap_or_else(|e| panic!("set {path}: {e}"));
    h.settle();
}

/// Invoke an action the way `hey do … click` does.
fn do_action(h: &mut Harness<Settings>, path: &str, action: &str) {
    let (ui, state) = h.parts();
    nitro_ui::introspect::invoke(ui, state, &format!("window/{path}"), action, None)
        .unwrap_or_else(|e| panic!("do {path} {action}: {e}"));
    h.settle();
}

/// The status line's text.
fn status(h: &mut Harness<Settings>) -> String {
    let id = named(h, names::STATUS);
    h.widget::<Label>(id).text().to_owned()
}

/// Write an executable shell script.
fn script(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write script");
    let mut perms = std::fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

/// A fake `wpctl` in `dir` that reports `volume` and records what it is
/// asked to do into `dir/calls`.
///
/// A shell script rather than a mock object, deliberately: it exercises
/// the argument vector, the process spawn *and* the output parsing, which
/// is where three of the four things that can go wrong live. A trait
/// implemented by the test would check only the fourth.
///
/// The path is quoted inside the script, because a temporary directory's
/// name is not this test's to choose and a space in `$TMPDIR` would
/// otherwise turn the redirect into two arguments.
fn fake_wpctl(dir: &Path, volume: &str) {
    let calls = dir.join("calls");
    script(
        &dir.join("wpctl"),
        &format!(
            "#!/bin/sh\n\
             echo \"$@\" >> '{calls}'\n\
             case \"$1\" in\n\
             get-volume) echo '{volume}' ;;\n\
             esac\n\
             exit 0\n",
            calls = calls.display(),
        ),
    );
}

/// What the fake `wpctl` was asked to do, in order.
fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn apply_writes_exactly_the_expected_file() {
    // The headline: the whole file, byte for byte, out of the widgets.
    //
    // Two outputs, because the expected file has two — and the way to get
    // a second one is the file itself, through the no-shell-socket path
    // that builds a row per connector the file mentions. Inventing an
    // `OutputInfo` the server never sent would be testing a dialog nobody
    // runs.
    let dir = scratch("apply");
    let path = dir.join(conf::FILE_NAME);
    std::fs::write(&path, "output.HDMI-A-1.scale = 2\noutput.VGA-1.scale = 1\n").expect("seed");
    let mut h2 = Harness::sized(
        "nitro-settings",
        Settings::new()
            .with_config_path(path.clone())
            .with_audio_dirs(Vec::new())
            .with_reload_wait(Duration::from_millis(50)),
        WINDOW,
        build,
    );
    h2.settle();
    assert_eq!(
        h2.state().connectors(),
        ["HDMI-A-1", "VGA-1"],
        "the file's connectors, in order"
    );

    set_value(&mut h2, "displays/HDMI-A-1/scale", "2");
    set_field(&mut h2, "displays/HDMI-A-1/x", "0");
    set_field(&mut h2, "displays/HDMI-A-1/y", "0");
    do_action(&mut h2, "displays/HDMI-A-1/primary", "click");
    set_value(&mut h2, "displays/VGA-1/scale", "1");
    set_field(&mut h2, "displays/VGA-1/x", "1920");
    set_field(&mut h2, "displays/VGA-1/y", "0");
    set_field(&mut h2, names::LAYOUT, "de");
    set_field(&mut h2, names::VARIANT, "");
    set_field(&mut h2, names::OPTIONS, "ctrl:nocaps");
    do_action(&mut h2, names::APPLY, "click");

    let written = std::fs::read_to_string(&path).expect("the file Apply wrote");
    assert_eq!(
        written,
        "\
# nitro server configuration — written by nitro-settings.
# Plain `key = value` lines; see docs/settings.md.

output.HDMI-A-1.scale = 2
output.HDMI-A-1.position = 0,0
output.HDMI-A-1.primary = true

output.VGA-1.scale = 1
output.VGA-1.position = 1920,0

keyboard.layout = de
keyboard.variant =
keyboard.options = ctrl:nocaps
"
    );
    assert_eq!(h2.state().applies(), 1);
    h2.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_untouched_scale_is_not_pinned_into_the_file() {
    // A row whose connector the file never mentioned is seeded from the
    // *live* scale — the EDID default, or a `NITRO_SCALE` dev override.
    // Writing that back would be the app inventing an opinion the user
    // never expressed: today's EDID answer frozen in, so a replaced
    // monitor stops being measured, and a `NITRO_SCALE=…` meant for one
    // `just fake` run made permanent.
    //
    // So Apply writes no `scale` line for a slider still sitting where it
    // was seeded, and does write one the moment the user moves it.
    let dir = scratch("untouched-scale");
    let path = dir.join(conf::FILE_NAME);
    // A file that positions the output but says nothing about its scale.
    std::fs::write(&path, "output.HDMI-A-1.position = 0,0\n").expect("seed");
    let mut h = Harness::sized(
        "nitro-settings",
        Settings::new()
            .with_config_path(path.clone())
            .with_audio_dirs(Vec::new())
            .with_reload_wait(Duration::from_millis(50)),
        WINDOW,
        build,
    );
    h.settle();

    do_action(&mut h, names::APPLY, "click");
    let written = std::fs::read_to_string(&path).expect("the file Apply wrote");
    assert!(
        !written.contains("output.HDMI-A-1.scale"),
        "an untouched slider must not pin a scale the file never had:\n{written}"
    );
    // The rest of the row is still written: this is about the one line.
    assert!(
        written.contains("output.HDMI-A-1.position = 0,0"),
        "{written}"
    );

    // Now move it, and the user's choice is persisted.
    set_value(&mut h, "displays/HDMI-A-1/scale", "2");
    do_action(&mut h, names::APPLY, "click");
    let written = std::fs::read_to_string(&path).expect("the file Apply wrote");
    assert!(
        written.contains("output.HDMI-A-1.scale = 2"),
        "a moved slider is the user's opinion and is written:\n{written}"
    );

    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_server_parser_reads_back_what_we_write() {
    // The test that keeps `conf.rs` and `nitro_server::config` honest: a
    // second implementation of a format is only safe if something asserts
    // they agree, and this is that something. It is the one place in the
    // crate that names `nitro-server`, and it is a dev-dependency.
    let mut c = conf::Conf::new();
    let hdmi = c.output_mut("HDMI-A-1");
    hdmi.scale = Some(2.0);
    hdmi.position = Some((0, 0));
    hdmi.primary = true;
    let vga = c.output_mut("VGA-1");
    vga.scale = Some(1.25);
    vga.position = Some((-1920, 40));
    c.keyboard = conf::KeyboardConf {
        layout: "de".to_owned(),
        variant: String::new(),
        options: "ctrl:nocaps".to_owned(),
    };

    let text = conf::render(&c);
    let parsed = nitro_server::config::parse(&text);
    assert!(
        parsed.warnings.is_empty(),
        "the server warned about our own file: {:?}\n{text}",
        parsed.warnings
    );

    let hdmi = parsed.output("HDMI-A-1").expect("HDMI");
    assert_eq!(hdmi.scale, Some(2.0));
    assert_eq!(hdmi.position, Some((0, 0)));
    assert!(hdmi.primary);
    let vga = parsed.output("VGA-1").expect("VGA");
    assert_eq!(vga.scale, Some(1.25));
    assert_eq!(vga.position, Some((-1920, 40)));
    assert!(!vga.primary);
    assert_eq!(parsed.primary(), Some("HDMI-A-1"));
    assert_eq!(parsed.keyboard.layout.as_deref(), Some("de"));
    // The distinction the server's `Option<String>` exists for: an
    // explicit empty variant is `Some("")`, not `None`, and this app's
    // always-written line has to land on the right side of it.
    assert_eq!(parsed.keyboard.variant.as_deref(), Some(""));
    assert_eq!(parsed.keyboard.options.as_deref(), Some("ctrl:nocaps"));
}

#[test]
fn revert_restores_the_widgets_from_the_file() {
    let dir = scratch("revert");
    let path = dir.join(conf::FILE_NAME);
    std::fs::write(
        &path,
        format!(
            "output.{CONNECTOR}.scale = 2\n\
             output.{CONNECTOR}.position = 100,200\n\
             output.{CONNECTOR}.primary = true\n\
             keyboard.layout = de\n\
             keyboard.options = ctrl:nocaps\n"
        ),
    )
    .expect("seed");
    let mut h = harness(&dir);

    // What the file says is what the dialog opened with.
    assert_eq!(field(&mut h, names::LAYOUT), "de");
    assert_eq!(field(&mut h, &format!("displays/{CONNECTOR}/x")), "100");

    // Change everything, without applying.
    set_field(&mut h, names::LAYOUT, "fr");
    set_field(&mut h, names::OPTIONS, "");
    set_value(&mut h, &format!("displays/{CONNECTOR}/scale"), "1");
    set_field(&mut h, &format!("displays/{CONNECTOR}/x"), "7");
    set_field(&mut h, &format!("displays/{CONNECTOR}/y"), "8");
    do_action(&mut h, &format!("displays/{CONNECTOR}/primary"), "click");
    assert_eq!(field(&mut h, names::LAYOUT), "fr");

    do_action(&mut h, names::REVERT, "click");

    assert_eq!(field(&mut h, names::LAYOUT), "de", "layout came back");
    assert_eq!(field(&mut h, names::VARIANT), "", "and so did the variant");
    assert_eq!(field(&mut h, names::OPTIONS), "ctrl:nocaps");
    assert_eq!(field(&mut h, &format!("displays/{CONNECTOR}/x")), "100");
    assert_eq!(field(&mut h, &format!("displays/{CONNECTOR}/y")), "200");
    let scale = named(&mut h, &format!("displays/{CONNECTOR}/scale"));
    assert!(
        (h.widget::<Slider<Settings>>(scale).value() - 2.0).abs() < 1e-6,
        "the scale slider came back to 2"
    );
    let value = named(&mut h, &format!("displays/{CONNECTOR}/scale_value"));
    assert_eq!(
        h.widget::<Label>(value).text(),
        "2",
        "and so did the label beside it"
    );
    let primary = named(&mut h, &format!("displays/{CONNECTOR}/primary"));
    assert!(
        h.widget::<Checkbox<Settings>>(primary).is_checked(),
        "and the primary box"
    );
    assert_eq!(h.state().reverts(), 1);
    assert_eq!(status(&mut h), "reverted");

    // And the file was not written on the way: Revert reads, it does not
    // save. A dialog that persisted on Revert would be the opposite of
    // what the button says.
    let on_disk = std::fs::read_to_string(&path).expect("read");
    assert!(
        !on_disk.contains("nitro-settings"),
        "Revert rewrote the file: {on_disk}"
    );
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_audio_section_drives_a_fake_wpctl() {
    let dir = scratch("audio");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    fake_wpctl(&bin, "Volume: 0.65 [MUTED]");

    let mut h = harness_in(&dir, Settings::new().with_audio_dirs(vec![bin.clone()]));
    h.wait_for("the volume to be read", |_| true);
    h.settle();

    // What the fake reported is what the widgets show.
    let volume = named(&mut h, names::VOLUME);
    assert!(
        (h.widget::<Slider<Settings>>(volume).value() - 0.65).abs() < 1e-6,
        "the slider took the fake's volume"
    );
    let mute = named(&mut h, names::MUTE);
    assert!(
        h.widget::<Checkbox<Settings>>(mute).is_checked(),
        "and `[MUTED]` ticked the box"
    );
    let value = named(&mut h, names::VOLUME_VALUE);
    assert_eq!(h.widget::<Label>(value).text(), "65 %");
    let audio_status = named(&mut h, names::AUDIO_STATUS);
    assert_eq!(h.widget::<Label>(audio_status).text(), "via wpctl");

    // And moving the slider drives it back out as a percentage.
    set_value(&mut h, names::VOLUME, "0.4");
    do_action(&mut h, names::MUTE, "toggle");
    let seen = calls(&bin);
    assert!(
        seen.iter().any(|c| c == "get-volume @DEFAULT_AUDIO_SINK@"),
        "read the volume once: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|c| c == "set-volume @DEFAULT_AUDIO_SINK@ 40%"),
        "wrote the new volume as a percentage: {seen:?}"
    );
    assert!(
        seen.iter().any(|c| c == "set-mute @DEFAULT_AUDIO_SINK@ 0"),
        "and unmuted: {seen:?}"
    );
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_search_path_says_no_audio_backend_found() {
    // The words are the spec's, and they are the whole point: a section
    // that cannot work has to say so rather than showing a slider that
    // silently does nothing.
    let dir = scratch("noaudio");
    let empty = dir.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");
    let mut h = harness_in(&dir, Settings::new().with_audio_dirs(vec![empty]));
    h.settle();

    let id = named(&mut h, names::AUDIO_STATUS);
    assert_eq!(h.widget::<Label>(id).text(), "no audio backend found");
    assert!(h.state().audio().is_none());

    // And the controls are disabled rather than absent: a greyed slider
    // says "this machine has no mixer", a missing one would say
    // "settings has no audio section".
    let volume = named(&mut h, names::VOLUME);
    assert!(!h.widget::<Slider<Settings>>(volume).is_enabled());
    let mute = named(&mut h, names::MUTE);
    assert!(!h.widget::<Checkbox<Settings>>(mute).is_enabled());
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_shell_connection_still_opens_an_ordinary_window() {
    // The question the crate docs answer: `App::shell` buys the output
    // list, and a shell connection with no shell surface is an ordinary
    // decorated window. If that were not true the dialog would have to
    // fall back to `App::new` and lose the display section.
    let dir = scratch("ordinary");
    let mut h = harness(&dir);
    assert!(h.ui().is_shell(), "the connection carries caps::SHELL");
    assert_eq!(h.ui().surface(), Some(nitro_settings::ORDINARY_WINDOW));
    assert!(
        h.state().is_live(),
        "so the output list came from the socket"
    );
    // Decorated: the server places a decorated window below the title
    // bar, so the content origin is not the output's origin.
    assert!(
        h.ui().window_position().y > 0.0,
        "the window has a title bar above it: {:?}",
        h.ui().window_position()
    );
    // And focusable: a shell *panel* is `NO_FOCUS`, and this is not one.
    let layout = named(&mut h, names::LAYOUT);
    h.ui().focus(layout);
    h.settle();
    assert_eq!(h.ui().focused(), Some(layout));
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_section_is_hey_addressable() {
    // Every path the crate docs and the README promise resolves, over the
    // real introspection socket rather than through a library call: a
    // rename that broke a documented command would otherwise only show up
    // on the box.
    //
    // The documented paths are the *short* ones — `keyboard/layout`,
    // `apply` — because that is how `hey` addresses a widget: a segment
    // that names no direct child is looked for by name in the subtree,
    // and the answer is refused unless it is unique. `list` prints the
    // canonical path instead (`keyboard/container[0]/layout`), which is
    // why this asserts on what `get` answers rather than on what `list`
    // prints. A caption beside a field would make the short form
    // ambiguous, which is precisely why the captions are unnamed.
    let dir = scratch("hey");
    let mut h = harness(&dir);
    let socket = h.open_socket("nitro-settings");
    h.settle();

    for (path, role) in [
        ("displays", "container"),
        ("displays_note", "label"),
        (&format!("displays/{CONNECTOR}"), "container"),
        (&format!("displays/{CONNECTOR}/name"), "label"),
        (&format!("displays/{CONNECTOR}/mode"), "label"),
        (&format!("displays/{CONNECTOR}/scale"), "slider"),
        (&format!("displays/{CONNECTOR}/scale_value"), "label"),
        (&format!("displays/{CONNECTOR}/primary"), "checkbox"),
        (&format!("displays/{CONNECTOR}/x"), "textfield"),
        (&format!("displays/{CONNECTOR}/y"), "textfield"),
        ("keyboard", "container"),
        ("keyboard/layout", "textfield"),
        ("keyboard/variant", "textfield"),
        ("keyboard/options", "textfield"),
        ("keyboard/test", "textfield"),
        ("audio", "container"),
        ("audio/volume", "slider"),
        ("audio/volume_value", "label"),
        ("audio/mute", "checkbox"),
        ("audio_status", "label"),
        ("apply", "button"),
        ("revert", "button"),
        ("status", "label"),
    ] {
        let answer = ask(&mut h, &socket, &format!("get {path} role\n"));
        assert_eq!(
            answer,
            vec![role.to_owned()],
            "`hey nitro-settings get {path} role` should answer {role}"
        );
    }

    // And the worked example from the crate docs really runs: the real
    // callback, with the real `&mut Settings`, through the real socket.
    let reply = ask(
        &mut h,
        &socket,
        &format!("do displays/{CONNECTOR}/primary click\n"),
    );
    assert!(reply.is_empty(), "a `do` answers with a bare ok");
    let primary = named(&mut h, &format!("displays/{CONNECTOR}/primary"));
    assert!(
        h.widget::<Checkbox<Settings>>(primary).is_checked(),
        "`hey do displays/{CONNECTOR}/primary click` ticked it"
    );

    // The `list` the README shows is a real listing, with every named
    // widget in it — by name, wherever the canonical path puts it.
    let listing = ask(&mut h, &socket, "list\n");
    for name in [
        names::DISPLAYS,
        names::DISPLAYS_NOTE,
        names::LAYOUT,
        names::TEST,
        names::VOLUME,
        names::AUDIO_STATUS,
        names::APPLY,
        names::REVERT,
        names::STATUS,
        CONNECTOR,
    ] {
        assert!(
            listing.iter().any(|l| l.split('\t').nth(2) == Some(name)),
            "`{name}` is not in the listing:\n{}",
            listing.join("\n")
        );
    }
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_slider_step_is_one_set_text_and_the_sliders_own_repaint() {
    // The cost claim, asserted from outside by counting mutations. A
    // scale change writes exactly one thing into the tree — the label
    // beside the slider — and the slider repaints its own knob. Anything
    // else in this list would be a widget reacting to a value it has no
    // business knowing about yet: nothing reads the scale until Apply.
    let dir = scratch("cost");
    let mut h = harness(&dir);
    let scale = format!("displays/{CONNECTOR}/scale");
    set_value(&mut h, &scale, "1.5");
    h.settle();

    h.tap();
    h.clear_tap();
    let commits = h.commits();
    set_value(&mut h, &scale, "2");
    h.settle();

    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    let set_texts = ops.iter().filter(|o| **o == "SetText").count();
    assert_eq!(set_texts, 1, "one SetText, for the value label: {ops:?}");
    assert!(
        ops.iter().all(|o| matches!(
            *o,
            "SetText" | "SetBounds" | "SetFill" | "SetRadius" | "Commit"
        )),
        "a step is the label and the slider's own rects: {ops:?}"
    );
    assert_eq!(h.commits() - commits, 1, "and exactly one commit");
    let value = named(&mut h, &format!("displays/{CONNECTOR}/scale_value"));
    assert_eq!(h.widget::<Label>(value).text(), "2");
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_is_sent_while_it_sits_there() {
    // The property the whole retained design exists for, and the one the
    // audio section could most easily have broken: there is no polling
    // timer behind the volume, so a settled dialog is silent.
    let dir = scratch("idle");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    fake_wpctl(&bin, "Volume: 0.30");
    let mut h = harness_in(&dir, Settings::new().with_audio_dirs(vec![bin.clone()]));
    h.settle();

    let before = calls(&bin).len();
    h.assert_idle(300);
    assert_eq!(
        calls(&bin).len(),
        before,
        "the mixer was not polled while idle"
    );
    assert_eq!(
        h.ui().next_timeout(),
        None,
        "and no timer is left armed at all"
    );
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_output_that_is_unplugged_loses_its_row() {
    let dir = scratch("hotplug");
    let mut h = harness(&dir);
    assert_eq!(h.state().connectors(), [CONNECTOR]);
    let mode = named(&mut h, &format!("displays/{CONNECTOR}/mode"));
    assert_eq!(
        h.widget::<Label>(mode).text(),
        "320×240 @ 60 Hz",
        "the resolution came from the live output, not the file"
    );

    // The real hotplug path: the server's control socket unplugs it and
    // the `OutputGone` arrives on the shell connection. `request_line`,
    // not `request`: `unplug` answers with a bare status line and no
    // body, so waiting for the blank line that terminates one would hang.
    assert_eq!(h.server().request_line("unplug\n"), "ok");
    h.wait_for("the row to go", |h| h.state().connectors().is_empty());
    h.settle();
    assert!(
        nitro_ui::introspect::resolve(h.ui(), &format!("window/displays/{CONNECTOR}")).is_none(),
        "and the widgets went with it"
    );
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_shell_socket_the_note_says_so() {
    // The fallback the spec requires to be visible: a dialog showing
    // fewer monitors than are plugged in, with no explanation, is worse
    // than one that admits it is working from the file.
    let dir = scratch("noshell");
    let path = dir.join(conf::FILE_NAME);
    std::fs::write(&path, "output.DP-2.scale = 1.5\n").expect("seed");
    let mut h = Harness::sized(
        "nitro-settings",
        Settings::new()
            .with_config_path(path)
            .with_audio_dirs(Vec::new()),
        WINDOW,
        build,
    );
    h.settle();

    assert!(!h.state().is_live());
    assert_eq!(h.state().connectors(), ["DP-2"], "rows from the file");
    let note = named(&mut h, names::DISPLAYS_NOTE);
    let text = h.widget::<Label>(note).text().to_owned();
    assert!(
        text.contains("No shell socket"),
        "the note says why: {text:?}"
    );
    // And the row has no resolution to show, rather than a made-up one.
    let mode = named(&mut h, "displays/DP-2/mode");
    assert_eq!(h.widget::<Label>(mode).text(), "—");
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_reports_what_the_counter_did() {
    // The verdict the whole `control` module exists for. The harness's
    // server is started with **no** config path, so it watches nothing
    // and its `config_reloads` never moves on its own — which is exactly
    // the "the server said nothing" case, and with a short wait it is the
    // rejection verdict, word for word.
    //
    // The wait is shortened because the verdict is *defined* as "the
    // counter did not move before the deadline": the deadline is this
    // test's entire runtime, and a suite must not spend a second and a
    // half proving a timeout it can prove in fifty milliseconds.
    let dir = scratch("verdict");
    let mut h = harness_in(
        &dir,
        Settings::new()
            .with_audio_dirs(Vec::new())
            .with_reload_wait(Duration::from_millis(50)),
    );
    h.settle();
    // The harness points the `Ui` at its own server's control socket, so
    // the dialog asks the right one without the test naming it.
    do_action(&mut h, names::APPLY, "click");
    assert_eq!(
        status(&mut h),
        "server rejected: see log",
        "a counter that did not move is not a confirmation"
    );
    assert!(
        dir.join(conf::FILE_NAME).exists(),
        "and the file was written anyway — the verdict is about the server"
    );
    h.quit();

    // With no server at all it is neither applied nor rejected: the file
    // is written and nothing confirmed it, which must not be drawn as a
    // refusal.
    let mut h2 = harness_in(
        &dir,
        Settings::new()
            .with_audio_dirs(Vec::new())
            .with_control_path(dir.join("no-such-control.sock"))
            .with_reload_wait(Duration::from_millis(50)),
    );
    h2.settle();
    do_action(&mut h2, names::APPLY, "click");
    assert!(
        status(&mut h2).starts_with("saved to "),
        "unconfirmed is not rejected: {:?}",
        status(&mut h2)
    );
    h2.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_position_is_skipped_and_named() {
    // A number nobody can parse must not be written into a file the
    // compositor would then warn about — and the user has to be told
    // which row it was, or they will look at four identical fields.
    let dir = scratch("badpos");
    let path = dir.join(conf::FILE_NAME);
    let mut h = harness(&dir);
    set_field(&mut h, &format!("displays/{CONNECTOR}/x"), "left");
    do_action(&mut h, names::APPLY, "click");

    let written = std::fs::read_to_string(&path).expect("written");
    assert!(
        !written.contains("position"),
        "no position line was written: {written}"
    );
    assert!(
        status(&mut h).contains(CONNECTOR),
        "and the status named the row: {:?}",
        status(&mut h)
    );
    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Send one request over the app's introspection socket and return the
/// body lines.
///
/// The app is served by the test thread, so the request goes out on
/// another one and this pumps until the reply is back — the shape every
/// socket test in the tree uses.
fn ask(h: &mut Harness<Settings>, socket: &Path, request: &str) -> Vec<String> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<String>>();
    let path = socket.to_path_buf();
    let req = request.to_owned();
    let handle = std::thread::spawn(move || {
        let mut sock = UnixStream::connect(&path).expect("connect to the app socket");
        sock.write_all(req.as_bytes()).expect("write");
        let mut reader = BufReader::new(sock);
        let mut status = String::new();
        reader.read_line(&mut status).expect("status");
        assert!(
            status.starts_with("ok"),
            "the app refused {req:?}: {status:?}"
        );
        let mut body = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\n" {
                break;
            }
            body.push(line.trim_end_matches('\n').to_owned());
        }
        let _ = tx.send(body);
    });
    let mut out = None;
    h.pump_socket_until("the reply", |_| {
        if out.is_none() {
            out = rx.try_recv().ok();
        }
        out.is_some()
    });
    handle.join().expect("the client thread");
    out.expect("a reply")
}

#[test]
fn the_dark_checkbox_writes_the_scheme_at_once() {
    // The appearance section's whole behaviour: no Apply. Ticking the box
    // writes `server.conf`, the server's inotify watch picks it up and
    // the palette reaches every client — including this one, which is
    // why the test can also assert the dialog's own colours moved.
    let dir = scratch("scheme");
    let path = dir.join(conf::FILE_NAME);
    let mut h = harness(&dir);
    let before = h.state().applies();

    do_action(&mut h, names::DARK, "toggle");

    let text = std::fs::read_to_string(&path).expect("the file was written");
    assert!(
        text.contains("theme.scheme = dark"),
        "the file says dark:\n{text}"
    );
    // And it is the file the *server* would accept, not merely one that
    // contains the right substring.
    let parsed = nitro_server::config::parse(&text);
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    assert_eq!(parsed.theme.scheme, Some(nitro_ui::Scheme::Dark));
    assert_eq!(
        h.state().applies(),
        before,
        "Apply was not involved: the scheme saves on its own"
    );
    assert_eq!(status(&mut h), "scheme: dark");

    // Untick: back to light, written out explicitly rather than by
    // deleting the key — the user asked for light, and a file that said
    // nothing would follow whatever the default becomes.
    do_action(&mut h, names::DARK, "toggle");
    let text = std::fs::read_to_string(&path).expect("read back");
    assert!(
        text.contains("theme.scheme = light"),
        "the file says light:\n{text}"
    );
    h.quit();
}

#[test]
fn the_checkbox_starts_on_what_the_file_says() {
    let dir = scratch("scheme-start");
    std::fs::write(dir.join(conf::FILE_NAME), "theme.scheme = dark\n").expect("write");
    let mut h = harness(&dir);
    let dark = named(&mut h, names::DARK);
    assert!(
        h.widget::<Checkbox<Settings>>(dark).is_checked(),
        "the box reflects the file it opened on"
    );
    h.quit();
}

#[test]
fn apply_keeps_a_scheme_and_the_per_role_overrides_it_cannot_edit() {
    // The rule that stops Apply being destructive. `theme.accent` has no
    // widget in this dialog at all, and the file is rewritten wholesale
    // — so without the carry-over in `collect`, pressing Apply would
    // silently delete a colour the user hand-picked.
    let dir = scratch("scheme-keep");
    let path = dir.join(conf::FILE_NAME);
    std::fs::write(
        &path,
        "theme.scheme = dark\ntheme.accent = #ff0000\nkeyboard.layout = us\n",
    )
    .expect("write");
    let mut h = harness(&dir);

    set_value(&mut h, names::LAYOUT, "de");
    do_action(&mut h, names::APPLY, "click");

    let text = std::fs::read_to_string(&path).expect("read back");
    assert!(text.contains("keyboard.layout = de"), "{text}");
    assert!(
        text.contains("theme.scheme = dark"),
        "the scheme survived Apply:\n{text}"
    );
    assert!(
        text.contains("theme.accent = #ff0000"),
        "an override this dialog cannot edit survived Apply:\n{text}"
    );
    let parsed = nitro_server::config::parse(&text);
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    h.quit();
}

#[test]
fn revert_puts_the_checkbox_back_without_writing() {
    let dir = scratch("scheme-revert");
    let path = dir.join(conf::FILE_NAME);
    std::fs::write(&path, "theme.scheme = light\n").expect("write");
    let mut h = harness(&dir);

    // Edit the file behind the dialog's back, then Revert: the checkbox
    // has to follow what is on disk, and the ticking must not itself
    // write — a Revert that saves is not a revert.
    std::fs::write(&path, "theme.scheme = dark\n").expect("rewrite");
    let writes = h.state().scheme_writes();
    do_action(&mut h, names::REVERT, "click");

    let dark = named(&mut h, names::DARK);
    assert!(
        h.widget::<Checkbox<Settings>>(dark).is_checked(),
        "Revert showed what the file says"
    );
    assert_eq!(
        h.state().scheme_writes(),
        writes,
        "setting the checkbox from Revert did not write the file back"
    );
    h.quit();
}

// ---------------------------------------------------------------------
// layout: nothing is laid out smaller than it measures
// ---------------------------------------------------------------------

/// A dialog at the size the **binary** opens, rather than [`WINDOW`].
///
/// Every other test in this file uses `WINDOW` — 300×210, deliberately
/// tighter than the harness's 320×240 output — because those tests are
/// about behaviour on a screen too small for the dialog, which is the
/// awkward case rather than a soft one. The layout tests ask the
/// opposite question: is [`nitro_settings::WINDOW_SIZE`] itself big
/// enough for the tree it was chosen for. So they open at that constant,
/// and a failure here means the constant is wrong rather than that a
/// small screen is small.
fn layout_harness(dir: &Path) -> Harness<Settings> {
    let path = dir.join(conf::FILE_NAME);
    let mut h = Harness::shell(
        "nitro-settings",
        Settings::new()
            .with_config_path(path)
            .with_audio_dirs(Vec::new()),
        nitro_settings::ORDINARY_WINDOW,
        Some(nitro_settings::WINDOW_SIZE),
        build,
    );
    h.wait_for("the display rows", |h| !h.state().connectors().is_empty());
    h.settle();
    h
}

/// Every widget in the tree, as `hey list` walks it: the canonical path
/// and the window-coordinate bounds — the same two numbers
/// `hey nitro-settings list` prints, read through the same
/// `Ui::introspect` pass, so this test and the box agree by construction
/// rather than by transcription.
fn tree(h: &mut Harness<Settings>) -> Vec<(String, nitro_ui::Rect)> {
    let mut nodes = Vec::new();
    h.ui().introspect(&mut nodes);
    nodes
        .iter()
        .map(|n| {
            let path = nitro_ui::introspect::path_of(h.ui(), n.id).unwrap_or_default();
            (path, n.bounds)
        })
        .collect()
}

#[test]
fn no_widget_is_laid_out_smaller_than_it_measures() {
    // The regression test for the whole bug, and for the thing eighteen
    // passing tests could not see.
    //
    // Nothing about it was a *measurement* failure: every widget measured
    // correctly and was then laid out smaller than it measured, because
    // the root column's intrinsic height (~400 px with one output) exceeded
    // a 320 px window and a flex container hands its overflow back to its
    // children as `flex_shrink`, weighted by size. On the box that read as
    // headings with the descenders sliced off ("Displays" — 17.5 px of
    // text in an 11.8 px box, so no tail on the `p`), the two-line notes
    // cut to ~1.3 lines, the keyboard captions rendering as
    // "Layc"/"Varia"/"Optic", and the display row's `y` field ending at
    // x=452 in a 440-wide window.
    //
    // So this pins the invariant rather than any one symptom: at
    // `WINDOW_SIZE`, with the harness's one output, nothing in the tree is
    // laid out shorter or narrower than what the font engine says it
    // needs. Every "want" below is measured through that engine rather
    // than written down, so the assertions follow the theme and the
    // constants instead of freezing today's numbers.
    let dir = scratch("layout-intrinsic");
    let mut h = layout_harness(&dir);
    let theme = nitro_ui::Theme::default();
    let text_style = nitro_ui::TextStyle::new(theme.font_family.clone(), nitro_settings::TEXT_SIZE);

    // 1. Every `control_row()` is exactly ROW_HEIGHT tall.
    //
    // An equality, not a tolerance: `control_row` asks for an explicit
    // `height(ROW_HEIGHT)`, and the bug was precisely that asking is not
    // getting — an explicit length is folded into the constraints a child
    // is *measured* with, and the solver then shrinks it anyway. Under the
    // old constants every one of these came out at 17.5.
    let rows: Vec<(String, nitro_ui::Rect)> = tree(&mut h)
        .into_iter()
        .filter(|(p, _)| {
            p == &format!("window/displays/{CONNECTOR}")
                || p == "window/keyboard/container[0]"
                || p == "window/keyboard/container[1]"
                || p == "window/audio"
                || p == "window/appearance"
                || p == "window/container[4]"
        })
        .collect();
    assert_eq!(rows.len(), 6, "found every control row: {rows:?}");
    for (path, b) in &rows {
        assert!(
            (b.h - nitro_settings::ROW_HEIGHT).abs() < 0.01,
            "{path} is {} tall, not ROW_HEIGHT {}",
            b.h,
            nitro_settings::ROW_HEIGHT,
        );
    }

    // 2. Every heading is at least as tall as its own text measures.
    let mut head =
        nitro_ui::TextStyle::new(theme.font_family.clone(), nitro_settings::HEADING_SIZE);
    head.weight = 600;
    let all = tree(&mut h);
    for text in ["Displays", "Keyboard", "Audio", "Appearance"] {
        let want = h
            .ui()
            .measure_text(text, &head, 0.0)
            .expect("measure the heading")
            .height;
        let (path, b) = all
            .iter()
            .find(|(p, _)| {
                p.starts_with("window/label[")
                    && nitro_ui::introspect::resolve(h.ui(), p).is_some_and(|id| {
                        h.ui()
                            .widget::<Label>(id)
                            .is_ok_and(|l: &Label| l.text() == text)
                    })
            })
            .unwrap_or_else(|| panic!("no heading {text} in {all:?}"));
        assert!(
            b.h >= want - 0.01,
            "heading {path} ({text}) is {} tall but its text measures {want}: \
             the missing pixels are the descenders — the `p` in Displays, the \
             `y` in Keyboard, the `pp` in Appearance",
            b.h,
        );
    }

    // 3. Every caption is as wide as an unconstrained measure of the same
    //    string.
    //
    // "Layout" is the one the box rendered as "Layc". Three fields whose
    // width resolves to ~102 px each do not fit beside three captions in
    // 420 px of inner width, and with every child shrinking by weight the
    // captions lost 40 %. A caption is the one thing in a row that cannot
    // usefully be narrowed — a field degrades gracefully at any width, a
    // six-letter word does not. Since #561 that is the toolkit's default
    // rather than a `shrink(0.0)` this app spells: a caption keeps what
    // it measured, and the fields, which say they are viewports over
    // their own text, absorb the deficit.
    for (path, caption) in [
        ("keyboard/container[0]/label[0]", "Layout"),
        ("keyboard/container[0]/label[1]", "Variant"),
        ("keyboard/container[0]/label[2]", "Options"),
        ("keyboard/container[1]/label[0]", "Test here"),
        ("audio/label[0]", "Volume"),
        ("appearance/label[0]", "Colour scheme"),
    ] {
        let want = h
            .ui()
            .measure_text(caption, &text_style, 0.0)
            .expect("measure the caption")
            .width;
        let id = named(&mut h, path);
        let b = h.ui().window_bounds(id);
        assert!(
            b.w >= want - 0.01,
            "the caption {caption:?} at {path} is {} wide but measures {want} \
             free: a clipped caption reads `Layc`",
            b.w,
        );
    }

    // 4. Every wrapped line of prose gets its full height.
    //
    // A wrapped label's height depends on the width it is given, so each
    // one is re-measured at the width it was actually allotted — the
    // honest comparison, and the one that catches "two lines rendered in
    // 1.3 lines of box". `audio_status` is in here because it was the
    // worst of them: 15.1 px of text in 10.2 px of box.
    for path in ["displays_note", "appearance_note", "audio_status"] {
        let id = named(&mut h, path);
        let text = h.widget::<Label>(id).text().to_owned();
        let b = h.ui().window_bounds(id);
        let want = h
            .ui()
            .measure_text(&text, &text_style, b.w)
            .expect("measure the note")
            .height;
        assert!(
            b.h >= want - 0.01,
            "{path} is {} tall but its text wraps to {want} at {} wide: `{text}`",
            b.h,
            b.w,
        );
    }

    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_in_the_tree_overhangs_the_window() {
    // The other face of the same bug, and the one that needs its own test
    // because it is not about any widget's *own* size: seven children
    // whose widths are all pinned simply do not fit in 420 px of inner
    // width, and no amount of shrinking elsewhere fixes an arrangement
    // that is too wide. On the box the display row's `y` field ended at
    // x=452 in a 440-wide window — visibly half off the edge.
    //
    // Every widget in the tree, not a chosen few: a check that named the
    // fields it already knew about would not have caught the field that
    // moves next.
    let dir = scratch("layout-overhang");
    let mut h = layout_harness(&dir);
    let size = h.ui().window_size();

    for (path, b) in tree(&mut h) {
        // The buttons row's spacer is a zero-size strut placed at the far
        // edge: no area, no ink, nothing to clip.
        if b.w <= 0.0 || b.h <= 0.0 {
            continue;
        }
        assert!(
            b.x + b.w <= size.w + 0.01,
            "{path} ends at x={} in a {}-wide window",
            b.x + b.w,
            size.w,
        );
        assert!(
            b.y + b.h <= size.h + 0.01,
            "{path} ends at y={} in a {}-tall window",
            b.y + b.h,
            size.h,
        );
    }

    // And the root's last child — the buttons row — ends inside the
    // padding rather than merely inside the window. Apply sitting on the
    // bottom edge is the failure this catches.
    let (_, buttons) = tree(&mut h)
        .into_iter()
        .find(|(p, _)| p == "window/container[4]")
        .expect("the buttons row");
    assert!(
        buttons.y + buttons.h <= size.h - nitro_settings::PAD + 0.01,
        "the buttons row ends at {} and the window's content stops at {}",
        buttons.y + buttons.h,
        size.h - nitro_settings::PAD,
    );

    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_outputs_fit_the_window_and_a_third_clips_rather_than_overlaps() {
    // Two claims the first draft of this fix got wrong, and they are
    // paired here because the second is what made the first survive.
    //
    // 1. `WINDOW_SIZE` must actually hold **two** outputs. The docs said
    //    so while the constant was the one-output figure rounded up: the
    //    tree measures 391.2 px for one output and exactly
    //    `ROW_HEIGHT + GAP` = 32 px more per output after it, so two need
    //    423.2 and the old 400 was short by 23. The old version of this
    //    test asserted only `x + w <= width` and so never noticed — its
    //    own comment said "the tree now exceeds the window" while the
    //    docs two files away said two outputs fit.
    //
    // 2. Past whatever the constant holds, the overflow must **clip**,
    //    not overlap. That used not to be automatic: the rows carry
    //    `min_height(ROW_HEIGHT)`, but the `displays` column holding them
    //    had the default `flex_shrink` of 1, so on overflow the column
    //    was laid out shorter than its own rows and the last row was
    //    drawn over `displays_note` — 0.8 px at two outputs, 15.6 px at
    //    three. Since #561 a container's measured size is its own floor
    //    and already sums its children, so the column cannot end before
    //    its rows and this test passes unchanged across that move. A
    //    window that ends early is a window; one that writes a row on
    //    top of a sentence is a bug report.
    //
    // The rows arrive as real `Output` events from real hotplugs, not as
    // invented `OutputInfo`s: the point of the harness is that they come
    // down the wire the way they do on the box.
    let dir = scratch("layout-two-outputs");
    let mut h = layout_harness(&dir);
    h.server().request_line("plug 2560x1440\n");
    h.wait_for("the second display row", |h| {
        h.state().connectors().len() > 1
    });
    h.settle();

    let size = h.ui().window_size();
    let rows: Vec<(String, nitro_ui::Rect)> = tree(&mut h)
        .into_iter()
        .filter(|(p, _)| p.starts_with("window/displays/") && p.matches('/').count() == 2)
        .collect();
    assert_eq!(rows.len(), 2, "two display rows: {rows:?}");

    // Every row: full height, inside the window, both axes. The vertical
    // half is the one that was missing.
    for (path, b) in &rows {
        assert!(
            (b.h - nitro_settings::ROW_HEIGHT).abs() < 0.01,
            "{path} is {} tall, not ROW_HEIGHT: a second monitor must \
             shorten neither row",
            b.h,
        );
        assert!(
            b.x + b.w <= size.w + 0.01,
            "{path} ends at x={} in a {}-wide window",
            b.x + b.w,
            size.w,
        );
    }

    // The mode label keeps the width its longer string measures: a
    // hotplugged 2560×1440 makes the longest mode string the dialog can
    // show (135.6 px against the harness output's 119.1), and the only
    // child of this row with any give is the slider, so a `WINDOW_SIZE.w`
    // chosen for the narrow case would overflow right here.
    let mode_id = named(&mut h, "displays/Virtual-2/mode");
    let mode = h.ui().window_bounds(mode_id);
    let theme = nitro_ui::Theme::default();
    let style = nitro_ui::TextStyle::new(theme.font_family.clone(), nitro_settings::TEXT_SIZE);
    let text = h.widget::<Label>(mode_id).text().to_owned();
    let want = h
        .ui()
        .measure_text(&text, &style, 0.0)
        .expect("measure the mode")
        .width;
    assert!(
        mode.w >= want - 0.01,
        "the mode label is {} wide but `{text}` measures {want}",
        mode.w,
    );

    // Claim 1: the whole tree still fits, buttons row included. This is
    // the assertion the docs' "two outputs fit" rests on, so it is here
    // rather than in prose.
    let (_, buttons) = tree(&mut h)
        .into_iter()
        .find(|(p, _)| p == "window/container[4]")
        .expect("the buttons row");
    assert!(
        buttons.y + buttons.h <= size.h - nitro_settings::PAD + 0.01,
        "with two outputs the buttons row ends at {} and the window's \
         content stops at {}: WINDOW_SIZE.h is short by {}",
        buttons.y + buttons.h,
        size.h - nitro_settings::PAD,
        buttons.y + buttons.h - (size.h - nitro_settings::PAD),
    );

    // Claim 2: a third output does not fit — and degrades by clipping.
    // The rows keep their height and stay inside their column, and the
    // column still ends above the note rather than through it.
    h.server().request_line("plug 1280x1024\n");
    h.wait_for("the third display row", |h| {
        h.state().connectors().len() > 2
    });
    h.settle();

    let all = tree(&mut h);
    let find = |needle: &str| -> nitro_ui::Rect {
        all.iter()
            .find(|(p, _)| p == needle)
            .unwrap_or_else(|| panic!("no {needle} in {all:?}"))
            .1
    };
    let column = find("window/displays");
    let note = find("window/displays_note");
    let last = find("window/displays/Virtual-3");

    assert!(
        (last.h - nitro_settings::ROW_HEIGHT).abs() < 0.01,
        "the third row is {} tall, not ROW_HEIGHT",
        last.h,
    );
    assert!(
        last.y + last.h <= column.y + column.h + 0.01,
        "the third row ends at {} but its column ends at {}: a column \
         shrunk below the rows it contains is how a row ends up drawn \
         over the note beneath it",
        last.y + last.h,
        column.y + column.h,
    );
    assert!(
        column.y + column.h <= note.y + 0.01,
        "the displays column ends at {} and the note starts at {}: they \
         overlap by {}, which is the failure the toolkit's content \
         shrink floor exists to prevent",
        column.y + column.h,
        note.y,
        column.y + column.h - note.y,
    );

    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_window_declares_its_tree_as_its_minimum_size() {
    // The other half of the fix, and the reason a bigger constant alone is
    // not enough: a window the user can drag smaller can be dragged back
    // into the bug. So the dialog publishes `WINDOW_SIZE` as the window's
    // **minimum** through `SetWindowLimits`, and it is the *server* that
    // refuses the drag — a client which merely clamped its own layout
    // would draw a letterbox inside a window the user is still shrinking.
    //
    // The assertion leans on a property of `Ui::set_window_limits` that
    // makes it a real test rather than a restatement: **re-declaring the
    // same limits sends nothing.** So a second call with exactly
    // `(WINDOW_SIZE, no maximum)` producing no message is proof that those
    // are the limits already on the wire — it cannot pass if the dialog
    // declared a smaller minimum, a different maximum, or none at all.
    // A control follows: different limits *do* produce a message, so the
    // silence above is the de-duplication and not a dead tap.
    //
    // What this does not claim is that the server honours them; that is
    // `a_resize_respects_the_limits_the_client_declared` in
    // `crates/nitro-server/tests/wm.rs`, which drives a real edge drag.
    // It cannot be re-run here: the harness's output is 320×240 and this
    // window is 560×440, so its right edge — the thing a drag grabs — is
    // off-screen.
    let dir = scratch("layout-limits");
    let path = dir.join(conf::FILE_NAME);
    let mut h = Harness::shell(
        "nitro-settings",
        Settings::new()
            .with_config_path(path)
            .with_audio_dirs(Vec::new()),
        nitro_settings::ORDINARY_WINDOW,
        Some(nitro_settings::WINDOW_SIZE),
        build,
    );
    h.settle();

    // No maximum: a machine with four monitors wants to drag this window
    // taller, and nothing here should stop it.
    let no_max = Size::ZERO;
    h.ui().tap(true);
    h.ui()
        .set_window_limits(nitro_settings::WINDOW_SIZE, no_max)
        .expect("re-declare the same limits");
    h.flush();
    assert!(
        !h.mutations().iter().any(|m| m.op == "SetWindowLimits"),
        "re-declaring the same limits sends nothing, so the dialog had \
         already declared min={:?} max={no_max:?}; it sent {:?}",
        nitro_settings::WINDOW_SIZE,
        h.mutations(),
    );

    // The control: the tap is live and this path does emit.
    h.ui()
        .set_window_limits(Size::new(320.0, 240.0), no_max)
        .expect("declare different limits");
    h.flush();
    assert!(
        h.mutations().iter().any(|m| m.op == "SetWindowLimits"),
        "a *different* minimum does reach the wire, so the silence above \
         was de-duplication rather than a dead tap: {:?}",
        h.mutations(),
    );
    // Put the real limits back. Nothing below asserts on them today, but
    // a test that leaves the control's 320×240 installed is a trap for
    // whoever adds an assertion after it.
    h.ui()
        .set_window_limits(nitro_settings::WINDOW_SIZE, no_max)
        .expect("restore the dialog's own limits");
    h.flush();
    h.ui().tap(false);

    // And the minimum is the window's own opening size, so there is no
    // width at which the tree is asked to fit in less than it measures.
    assert_eq!(
        nitro_settings::WINDOW_SIZE,
        h.ui().window_size(),
        "the window opens at exactly the size it declares as its minimum"
    );

    h.quit();
    let _ = std::fs::remove_dir_all(&dir);
}
