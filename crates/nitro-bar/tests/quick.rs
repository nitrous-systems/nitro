//! The quick-settings menu, end to end: a real server, the bar on the
//! shell socket, a fake `wpctl` shell script and a fake `session.sock`.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use nitro_bar::quick::{View, names as q};
use nitro_bar::{Bar, build, names};
use nitro_ui::event::key;
use nitro_ui::quick::{ChoiceRow, RoundButton, Tile};
use nitro_ui::shell::Surface;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Icon, Label, Slider};
use nitro_ui::{Point, Size, WidgetId, WindowId};

const BAR_H: f32 = 32.0;
const OUT: (u32, u32) = (640, 480);

/// A scratch directory under the target dir, fresh per test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nitro-bar-quick-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A fake `wpctl` in `dir` that keeps volume, mute and the default sink
/// in files and logs every call to `dir/log`.
fn fake_wpctl(dir: &Path) {
    std::fs::write(dir.join("vol"), "0.60").unwrap();
    std::fs::write(dir.join("mute"), "0").unwrap();
    std::fs::write(dir.join("default"), "46").unwrap();
    let script = format!(
        r#"#!/bin/sh
d='{dir}'
echo "$*" >> "$d/log"
def=$(cat "$d/default")
mark() {{ [ "$1" = "$def" ] && echo '*' || echo ' '; }}
case "$1" in
  get-volume)
    if [ "$(cat "$d/mute")" = 1 ]; then echo "Volume: $(cat "$d/vol") [MUTED]"; else echo "Volume: $(cat "$d/vol")"; fi ;;
  set-volume) v=${{3%\%}}; printf '0.%02d' "$v" > "$d/vol"; if [ "$v" = 100 ]; then echo 1.00 > "$d/vol"; fi ;;
  set-mute) echo "$3" > "$d/mute" ;;
  status)
    echo 'Audio'
    echo ' ├─ Sinks:'
    echo " │  $(mark 46)   46. Built-in Audio Analog Stereo [vol: 0.60]"
    echo " │  $(mark 51)   51. HDMI Output [vol: 1.00]"
    echo ' │  '
    echo ' ├─ Sources:'
    echo ' │  *   47. Microphone [vol: 1.00]'
    ;;
  set-default) echo "$2" > "$d/default" ;;
  *) exit 1 ;;
esac
"#,
        dir = dir.display()
    );
    let bin = dir.join("wpctl");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default()
}

/// A fake `session.sock` answering `reply` to one request per
/// connection; the verbs it received go to the returned channel.
fn fake_session(dir: &Path, reply: &'static str) -> (PathBuf, std::sync::mpsc::Receiver<String>) {
    let path = dir.join("session.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { return };
            let mut line = String::new();
            if BufReader::new(&c).read_line(&mut line).is_ok() {
                let _ = tx.send(line.trim().to_owned());
                let _ = writeln!(c, "{reply}");
            }
        }
    });
    (path, rx)
}

fn harness(state: Bar) -> Harness<Bar> {
    let mut h = Harness::shell_on(
        "nitro-bar",
        state
            .with_fake_time_ms((9 * 3600 + 41 * 60 + 5) * 1000)
            .with_sensors(|| nitro_bar::Readings {
                battery: Some("87%".to_owned()),
                load: None,
                mem: None,
            })
            .with_settings_command(vec!["/bin/true".to_owned()]),
        Surface::bar(BAR_H as u32),
        Some(Size::new(320.0, BAR_H)),
        OUT,
        build,
    );
    h.settle();
    h
}

fn bar_with(dir: &Path) -> Harness<Bar> {
    harness(
        Bar::new()
            .with_audio_dirs(vec![dir.to_path_buf()])
            .with_config_path(dir.join("server.conf")),
    )
}

fn named(h: &mut Harness<Bar>, path: &str) -> WidgetId {
    nitro_ui::introspect::resolve(h.ui(), path).unwrap_or_else(|| panic!("no widget at {path}"))
}

/// The open popup, found through its window index.
fn popup(h: &Harness<Bar>) -> WindowId {
    h.state().quick().popup().expect("the menu is open")
}

/// A widget in the open menu, by name.
fn in_menu(h: &mut Harness<Bar>, name: &str) -> WidgetId {
    let win = popup(h);
    let idx = h.ui().windows().iter().position(|w| *w == win).unwrap();
    named(h, &format!("window[{idx}]/{name}"))
}

fn open(h: &mut Harness<Bar>) {
    let pill = named(h, &format!("window/{}", names::STATUS));
    h.click(pill);
    assert!(
        h.state().quick().popup().is_some(),
        "the pill opened the menu"
    );
}

fn pill_icon(h: &mut Harness<Bar>) -> String {
    let id = named(h, &format!("window/{}", q::STATUS_VOLUME));
    h.widget::<Icon>(id).name().to_owned()
}

fn act(h: &mut Harness<Bar>, id: WidgetId, action: &str, arg: Option<&str>) {
    let (ui, st) = h.parts();
    ui.action(st, id, action, arg).expect("action");
    h.settle();
}

#[test]
fn the_pill_opens_a_menu_under_the_bar_and_it_closes_every_way() {
    let dir = scratch("open");
    fake_wpctl(&dir);
    let mut h = bar_with(&dir);
    assert_eq!(pill_icon(&mut h), "volume-up", "read once at start-up");
    // The battery reading lives in the pill and keeps its name.
    let bat = named(&mut h, &format!("window/{}", names::BATTERY));
    assert_eq!(h.widget::<Label>(bat).text(), "87%");

    open(&mut h);
    let win = popup(&h);
    let pos = h.ui().window_position_of(win);
    let size = h.ui().window_size_of(win);
    assert!(pos.y >= BAR_H, "below the bar: {pos:?}");
    assert!(
        pos.x + size.w <= OUT.0 as f32 + 0.5,
        "on screen: {pos:?} {size:?}"
    );
    assert!(
        pos.x + size.w > OUT.0 as f32 - 40.0,
        "at the right end: {pos:?} {size:?}"
    );
    assert!((size.w - nitro_ui::quick::QS_WIDTH).abs() < 0.5);

    // A second click on the pill: the server consumes it and dismisses.
    let pill = named(&mut h, &format!("window/{}", names::STATUS));
    h.click(pill);
    assert!(
        h.state().quick().popup().is_none(),
        "the pill toggled it shut"
    );
    assert!(!h.ui().has_window(win));
    assert_eq!(h.state().quick().opens(), 1, "and did not re-open it");

    // Escape.
    open(&mut h);
    h.key(key::ESC);
    assert!(h.state().quick().popup().is_none(), "Escape closed it");

    // A click on the desktop.
    open(&mut h);
    let win = popup(&h);
    h.click_at_in(WindowId::MAIN, Point::new(20.0, 10.0));
    let _ = win;
    assert!(
        h.state().quick().popup().is_none(),
        "an outside click closed it"
    );
    h.quit();
}

#[test]
fn the_slider_and_mute_set_the_real_sink_and_the_pill_follows() {
    let dir = scratch("volume");
    fake_wpctl(&dir);
    let mut h = bar_with(&dir);
    open(&mut h);
    let vol = in_menu(&mut h, q::VOLUME);
    assert!((h.widget::<Slider<Bar>>(vol).value() - 0.6).abs() < 0.01);
    act(&mut h, vol, "set_value", Some("0.3"));
    assert!(
        log(&dir).contains("set-volume @DEFAULT_AUDIO_SINK@ 30%"),
        "{}",
        log(&dir)
    );
    assert_eq!(pill_icon(&mut h), "volume-down");

    let mute = in_menu(&mut h, q::MUTE);
    act(&mut h, mute, "click", None);
    assert!(
        log(&dir).contains("set-mute @DEFAULT_AUDIO_SINK@ 1"),
        "{}",
        log(&dir)
    );
    assert_eq!(pill_icon(&mut h), "volume-mute");
    assert_eq!(h.widget::<RoundButton<Bar>>(mute).icon(), "volume-mute");
    h.quit();
}

#[test]
fn the_output_view_lists_sinks_and_switching_works() {
    let dir = scratch("outputs");
    fake_wpctl(&dir);
    let mut h = bar_with(&dir);
    open(&mut h);
    let p = popup(&h);
    let width = h.ui().window_size_of(p).w;
    let out = in_menu(&mut h, q::OUTPUT);
    act(&mut h, out, "click", None);
    assert_eq!(h.state().quick().view(), View::Outputs);
    assert_eq!(h.ui().windows().len(), 2, "the old popup went in the swap");
    let p = popup(&h);
    assert!(
        (h.ui().window_size_of(p).w - width).abs() < 0.5,
        "same width"
    );
    let s0 = in_menu(&mut h, "sink0");
    let s1 = in_menu(&mut h, "sink1");
    assert!(h.widget::<ChoiceRow<Bar>>(s0).is_selected());
    assert!(!h.widget::<ChoiceRow<Bar>>(s1).is_selected());
    assert_eq!(h.widget::<ChoiceRow<Bar>>(s1).label(), "HDMI Output");
    h.click(s1);
    assert!(log(&dir).contains("set-default 51"), "{}", log(&dir));
    assert_eq!(
        h.state().quick().view(),
        View::Main,
        "back to the main view"
    );
    let sink = in_menu(&mut h, q::SINK);
    assert_eq!(h.widget::<Label>(sink).text(), "HDMI Output");

    // Re-opened, the new default is the checked one.
    h.key(key::ESC);
    open(&mut h);
    let out = in_menu(&mut h, q::OUTPUT);
    act(&mut h, out, "click", None);
    let s1 = in_menu(&mut h, "sink1");
    assert!(h.widget::<ChoiceRow<Bar>>(s1).is_selected());
    // The back arrow returns to Main.
    let back = in_menu(&mut h, q::BACK);
    act(&mut h, back, "click", None);
    assert_eq!(h.state().quick().view(), View::Main);
    h.quit();
}

#[test]
fn with_no_mixer_the_card_says_so_and_the_controls_are_disabled() {
    let dir = scratch("nomixer");
    let mut h = bar_with(&dir);
    assert_eq!(pill_icon(&mut h), "sliders");
    open(&mut h);
    let vol = in_menu(&mut h, q::VOLUME);
    assert!(!h.widget::<Slider<Bar>>(vol).is_enabled());
    let out = in_menu(&mut h, q::OUTPUT);
    assert!(!h.widget::<RoundButton<Bar>>(out).is_enabled());
    let note = in_menu(&mut h, q::SINK);
    assert!(h.widget::<Label>(note).text().contains("No wpctl or pactl"));
    h.quit();
}

#[test]
fn the_power_view_logs_out_through_session_sock_and_shows_errors() {
    let dir = scratch("power");
    let (sock, rx) = fake_session(&dir, "ok");
    let mut h = harness(Bar::new().with_audio_dirs(vec![]).with_session_socket(sock));
    open(&mut h);
    let lock = in_menu(&mut h, q::LOCK);
    assert!(
        h.widget::<RoundButton<Bar>>(lock).is_enabled(),
        "lock is live"
    );
    let power = in_menu(&mut h, q::POWER);
    act(&mut h, power, "click", None);
    assert_eq!(h.state().quick().view(), View::Power);
    let logout = in_menu(&mut h, q::LOGOUT);
    h.click(logout);
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        "logout"
    );
    assert!(h.state().quick().popup().is_none(), "ok closes the menu");
    h.quit();

    let dir = scratch("power-err");
    let (sock, rx) = fake_session(&dir, "err nope");
    let mut h = harness(Bar::new().with_audio_dirs(vec![]).with_session_socket(sock));
    open(&mut h);
    let power = in_menu(&mut h, q::POWER);
    act(&mut h, power, "click", None);
    let suspend = in_menu(&mut h, q::SUSPEND);
    act(&mut h, suspend, "click", None);
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        "suspend"
    );
    assert_eq!(h.state().quick().message(), Some("suspend: nope"));
    assert_eq!(
        h.state().quick().view(),
        View::Power,
        "stays open on the error"
    );
    let msg = in_menu(&mut h, q::MESSAGE);
    assert_eq!(h.widget::<Label>(msg).text(), "suspend: nope");
    h.quit();
}

#[test]
fn the_dark_style_tile_flips_theme_scheme_and_keeps_the_rest() {
    let dir = scratch("dark");
    let conf = dir.join("server.conf");
    std::fs::write(&conf, "keyboard.layout = de\ntheme.scheme = light\n").unwrap();
    let mut h = bar_with(&dir);
    open(&mut h);
    let dark = in_menu(&mut h, q::DARK);
    assert!(!h.widget::<Tile<Bar>>(dark).is_on());
    h.click(dark);
    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(text.contains("theme.scheme = dark"), "{text}");
    assert!(text.contains("keyboard.layout = de"), "{text}");
    assert!(h.widget::<Tile<Bar>>(dark).is_on());

    // The server's reload pushes the new palette; the open menu repaints
    // through the normal path. (The harness server does not watch this
    // file, so the push is simulated with the palette it would send.
    // Client-side only: server-tinted icons keep the server's scheme,
    // which is fine for a before/after pixel comparison of the panel.)
    let before = h.shot_window(popup(&h));
    h.ui().set_palette(nitro_ui::Palette::dark());
    h.settle();
    let after = h.shot_window(popup(&h));
    assert_ne!(
        before.pixel(20, 20),
        after.pixel(20, 20),
        "the menu recoloured"
    );

    // Re-opened, the tile reads the file.
    h.key(key::ESC);
    open(&mut h);
    let dark = in_menu(&mut h, q::DARK);
    assert!(h.widget::<Tile<Bar>>(dark).is_on());
    h.click(dark);
    assert!(
        std::fs::read_to_string(&conf)
            .unwrap()
            .contains("theme.scheme = light")
    );
    h.quit();
}

#[test]
fn a_closed_menu_costs_nothing_and_leaves_no_timers() {
    let dir = scratch("idle");
    fake_wpctl(&dir);
    let mut h = bar_with(&dir);
    let before = h.next_timeout();
    h.assert_idle(200);
    open(&mut h);
    h.key(key::ESC);
    assert!(h.state().quick().popup().is_none());
    assert_eq!(
        h.next_timeout().is_some(),
        before.is_some(),
        "no timer left behind"
    );
    let calls = log(&dir).lines().count();
    h.assert_idle(200);
    assert_eq!(
        log(&dir).lines().count(),
        calls,
        "no mixer polling while closed"
    );
    h.quit();
}

/// nitro-shot's PNG encoder, reused as a module (it has no dependencies).
#[allow(dead_code)]
#[path = "../../nitro-shot/src/png.rs"]
mod png;

/// Write the README/docs screenshots: `cargo test -p nitro-bar --test
/// quick -- --ignored screenshots`. Crops the top-right of the output:
/// the bar's end, and the menu hanging under it.
#[test]
#[ignore = "writes docs/quick-settings-*.png"]
fn screenshots() {
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for scheme in ["light", "dark"] {
        let dir = scratch(&format!("shot-{scheme}"));
        fake_wpctl(&dir);
        // Both halves of the scheme: the *server* starts dark, so the
        // icons it tints (by role byte) get dark inks and the client is
        // pushed the dark palette; and the bar's own `server.conf` says
        // dark, so the Dark Style tile reads On.
        let conf = format!("theme.scheme = {scheme}\n");
        std::fs::write(dir.join("server.conf"), &conf).unwrap();
        let state = Bar::new()
            .with_audio_dirs(vec![dir.clone()])
            .with_config_path(dir.join("server.conf"))
            .with_fake_time_ms((9 * 3600 + 41 * 60 + 5) * 1000)
            .with_sensors(|| nitro_bar::Readings {
                battery: Some("87%".to_owned()),
                load: None,
                mem: None,
            })
            .with_settings_command(vec!["/bin/true".to_owned()]);
        let mut h = Harness::shell_configured(
            "nitro-bar",
            state,
            Surface::bar(BAR_H as u32),
            Some(Size::new(320.0, BAR_H)),
            OUT,
            &conf,
            build,
        );
        h.settle();
        open(&mut h);
        if scheme == "dark" {
            let dark = in_menu(&mut h, q::DARK);
            assert!(
                h.widget::<Tile<Bar>>(dark).is_on(),
                "the tile reads the file"
            );
        }
        write_crop(&mut h, &docs.join(format!("quick-settings-{scheme}.png")));
        let out = in_menu(&mut h, q::OUTPUT);
        act(&mut h, out, "click", None);
        write_crop(
            &mut h,
            &docs.join(format!("quick-settings-outputs-{scheme}.png")),
        );
        h.quit();
    }
}

fn write_crop(h: &mut Harness<Bar>, path: &Path) {
    // Park the pointer in the desktop's bottom-left, off the menu and the
    // pill, so neither is drawn hovered and no cursor is in the picture.
    h.move_pointer_in(WindowId::MAIN, Point::new(2.0, OUT.1 as f32 - 2.0));
    let img = h.output_shot();
    let win = popup(h);

    let pos = h.ui().window_position_of(win);
    let size = h.ui().window_size_of(win);
    let x0 = (pos.x - 40.0).max(0.0) as u32;
    let x1 = OUT.0;
    let y1 = ((pos.y + size.h + 16.0) as u32).min(img.height);
    let (w, hgt) = (x1 - x0, y1);
    let mut data = Vec::with_capacity((w * hgt * 4) as usize);
    for y in 0..hgt {
        let s = (y * img.stride + x0 * 4) as usize;
        data.extend_from_slice(&img.data[s..s + (w * 4) as usize]);
    }
    std::fs::write(path, png::encode_xrgb(w, hgt, w * 4, &data)).expect("write png");
}

#[test]
fn the_lock_button_asks_the_session_to_lock_and_closes_the_menu() {
    let dir = scratch("lock");
    let (sock, rx) = fake_session(&dir, "ok");
    let mut h = harness(Bar::new().with_audio_dirs(vec![]).with_session_socket(sock));
    open(&mut h);
    let lock = in_menu(&mut h, q::LOCK);
    h.click(lock);
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        "lock"
    );
    assert!(h.state().quick().popup().is_none(), "ok closes the menu");
    h.quit();
}

/// Evdev keycodes for Super and `l`.
const KEY_LEFTMETA: u32 = 125;
const KEY_L: u32 = 38;

#[test]
fn super_l_asks_the_session_to_lock() {
    let dir = scratch("super-l");
    let (sock, rx) = fake_session(&dir, "ok");
    let mut h = harness(Bar::new().with_audio_dirs(vec![]).with_session_socket(sock));
    assert!(h.server().stat("hotkeys") >= 1, "Super+L is bound");
    h.key_down(KEY_LEFTMETA);
    h.key_down(KEY_L);
    h.key_up(KEY_L);
    h.key_up(KEY_LEFTMETA);
    h.settle();
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        "lock"
    );
    assert_eq!(h.state().lock_presses(), 1, "once, on the press");
    assert!(h.state().quick().popup().is_none(), "no menu opened");
    h.quit();
}

// ------------------------------------------ screen recording (#676 C)

fn shell_event(h: &mut Harness<Bar>, ev: &nitro_ui::shell::ShellEvent) {
    let (ui, st) = h.parts();
    ui.dispatch_shell(st, ev);
    h.settle();
}

fn prompt_widget(h: &mut Harness<Bar>, name: &str) -> WidgetId {
    let win = h.state().rec().popup().expect("the prompt is open");
    let idx = h.ui().windows().iter().position(|w| *w == win).unwrap();
    named(h, &format!("window[{idx}]/{name}"))
}

#[test]
fn the_rec_indicator_follows_the_capture_state() {
    let dir = scratch("rec");
    let mut h = bar_with(&dir);
    let rec = named(
        &mut h,
        &format!("window/{}", nitro_bar::rec::names::INDICATOR),
    );
    assert!(!h.ui().is_visible(rec), "hidden while nothing records");
    shell_event(
        &mut h,
        &nitro_ui::shell::ShellEvent::CaptureState {
            active: true,
            outputs_mask: 2,
        },
    );
    assert!(h.ui().is_visible(rec));
    assert_eq!(h.widget::<Label>(rec).text(), "● REC");
    assert!(h.state().rec().active());
    shell_event(
        &mut h,
        &nitro_ui::shell::ShellEvent::CaptureState {
            active: false,
            outputs_mask: 0,
        },
    );
    assert!(!h.ui().is_visible(rec));
}

#[test]
fn the_capture_prompt_answers_with_the_button_pressed_and_queues() {
    use nitro_bar::rec::names as r;
    use nitro_ui::shell::{CaptureAnswerKind as A, ShellEvent};
    let dir = scratch("prompt");
    let mut h = bar_with(&dir);
    for request in [5, 6, 7] {
        shell_event(
            &mut h,
            &ShellEvent::CapturePrompt {
                request,
                output: 1,
                client_name: "chrome".to_owned(),
            },
        );
    }
    assert_eq!(
        h.state().rec().showing().unwrap().request,
        5,
        "one at a time"
    );
    let q = prompt_widget(&mut h, r::QUESTION);
    assert!(h.widget::<Label>(q).text().starts_with("chrome wants"));
    let b = prompt_widget(&mut h, r::DENY);
    act(&mut h, b, "click", None);
    assert_eq!(
        h.state().rec().showing().unwrap().request,
        6,
        "the next one"
    );
    let b = prompt_widget(&mut h, r::ALLOW_ONCE);
    act(&mut h, b, "click", None);
    let b = prompt_widget(&mut h, r::ALLOW_SESSION);
    act(&mut h, b, "click", None);
    assert!(h.state().rec().popup().is_none());
    assert_eq!(
        h.state().rec().answers(),
        &[(5, A::Deny), (6, A::AllowOnce), (7, A::AllowSession)]
    );
}
