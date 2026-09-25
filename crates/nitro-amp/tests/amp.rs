//! The player's window, driven through a real server.
//!
//! Every test builds the tree the binary builds ([`nitro_amp::build`])
//! and drives it the way `hey` or a user would — actions on named
//! widgets, key presses — then asserts on what the widgets say. The
//! engine runs for real, on WAV files written here, with the unpaced
//! silent output, so a track ends as fast as it decodes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nitro_amp::engine::State;
use nitro_amp::vis::{Mode, Vis};
use nitro_amp::{Amp, Config, build, wav};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Label, Slider};
use nitro_ui::{List, Size, WidgetId};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("nitro-amp-ui-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A short stereo tone.
fn tone(dir: &Path, name: &str, secs: f32) -> PathBuf {
    let rate = 8_000;
    let frames = (rate as f32 * secs) as usize;
    let samples: Vec<f32> = (0..frames)
        .flat_map(|i| {
            let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin();
            [v, v]
        })
        .collect();
    let p = dir.join(name);
    std::fs::write(&p, wav::encode_i16(rate, 2, &samples)).unwrap();
    p
}

fn harness_with(config: Config) -> Harness<Amp> {
    let amp = Amp::new(config).unwrap();
    Harness::new("amp", amp, build)
}

fn harness() -> Harness<Amp> {
    harness_with(Config::headless())
}

fn named(h: &mut Harness<Amp>, name: &str) -> WidgetId {
    nitro_ui::introspect::resolve(h.ui(), &format!("window/{name}"))
        .unwrap_or_else(|| panic!("no widget named {name}"))
}

fn text(h: &mut Harness<Amp>, name: &str) -> String {
    let id = named(h, name);
    h.widget::<Label>(id).text().to_owned()
}

/// Invoke `action` on the widget named `name`, as `hey … do` does.
fn act(h: &mut Harness<Amp>, name: &str, action: &str, arg: Option<&str>) {
    let id = named(h, name);
    let (ui, state) = h.parts();
    ui.action(state, id, action, arg)
        .unwrap_or_else(|e| panic!("{name} {action}: {e}"));
    h.settle();
}

/// Run the app loop — input, timers, flushes — until `f` holds.
fn run_until(h: &mut Harness<Amp>, what: &str, mut f: impl FnMut(&mut Harness<Amp>) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f(h) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        h.pump();
        h.run_timers();
        h.flush();
        std::thread::sleep(Duration::from_millis(3));
    }
}

/// Add `path` through the path field and the add button.
fn add(h: &mut Harness<Amp>, path: &Path) {
    act(h, "path", "set_text", Some(&path.display().to_string()));
    act(h, "add", "activate", None);
}

#[test]
fn every_control_is_named_for_hey() {
    let mut h = harness();
    for n in [
        "vis",
        "clock",
        "title",
        "info",
        "status",
        "seek",
        "prev",
        "play",
        "pause",
        "stop",
        "next",
        "eject",
        "shuffle",
        "repeat",
        "eq",
        "pl",
        "volume",
        "balance",
        "eq_on",
        "preamp",
        "eq_60",
        "eq_1k",
        "eq_16k",
        "preset_rock",
        "playlist",
        "path",
        "add",
        "remove",
        "clear",
        "total",
    ] {
        named(&mut h, n);
    }
    // Nothing is loaded and no output was found: the window says so
    // rather than showing a confident blank.
    assert_eq!(text(&mut h, "clock_text"), "00:00");
    assert!(text(&mut h, "status").contains("playing silently"));
}

#[test]
fn adding_a_folder_fills_the_playlist_and_play_runs_through_it() {
    let d = TempDir::new("folder");
    tone(&d.0, "b_second.wav", 0.2);
    tone(&d.0, "a_first.wav", 0.2);
    std::fs::write(d.0.join("notes.txt"), "not audio").unwrap();
    let mut h = harness();
    add(&mut h, &d.0);
    assert_eq!(h.state().playlist().len(), 2, "the .txt is skipped");
    assert_eq!(h.state().playlist().get(0).unwrap().title, "a first");
    assert_eq!(text(&mut h, "total"), "2 tracks  0:00+");
    let list = named(&mut h, "playlist");
    assert_eq!(h.widget::<List<Amp>>(list).len(), 2);

    act(&mut h, "play", "activate", None);
    // Both tracks play and the list stops at its end (repeat is off).
    run_until(&mut h, "the list to finish", |h| {
        let st = h.state().status();
        h.state().playlist().current() == Some(1) && st.ended && !h.state().is_ticking()
    });
    let st = h.state().status();
    assert_eq!(st.state, State::Stopped);
    // Durations were learned as the tracks were opened.
    assert_eq!(text(&mut h, "total"), "2 tracks  0:00");
    assert!(
        text(&mut h, "title").starts_with("2. b second"),
        "{}",
        text(&mut h, "title")
    );
    let row = h.widget::<List<Amp>>(list).row(1).unwrap();
    assert_eq!(
        row.icon.as_deref(),
        Some("play-fill"),
        "the current row is marked"
    );
    assert_eq!(row.detail, "0:00");
}

#[test]
fn repeat_wraps_and_next_and_prev_move_the_cursor() {
    let d = TempDir::new("repeat");
    for n in ["1.wav", "2.wav", "3.wav"] {
        tone(&d.0, n, 0.05);
    }
    let mut h = harness();
    add(&mut h, &d.0);
    act(&mut h, "next", "activate", None);
    assert_eq!(h.state().playlist().current(), Some(0));
    act(&mut h, "next", "activate", None);
    assert_eq!(h.state().playlist().current(), Some(1));
    act(&mut h, "prev", "activate", None);
    assert_eq!(h.state().playlist().current(), Some(0));

    act(&mut h, "repeat", "activate", None);
    assert!(h.state().playlist().repeat());
    act(&mut h, "play", "activate", None);
    // With repeat on, the end of the list is the start again.
    run_until(&mut h, "a wrap to the top", |h| {
        let cur = h.state().playlist().current();
        let st = h.state().status();
        cur == Some(0) && st.token >= 4
    });
    act(&mut h, "stop", "activate", None);
}

#[test]
fn an_unplayable_track_is_skipped_and_reported() {
    let d = TempDir::new("skip");
    let good = tone(&d.0, "good.wav", 0.05);
    let mut h = harness();
    add(&mut h, &d.0.join("missing.wav"));
    add(&mut h, &good);
    act(&mut h, "play", "activate", None);
    run_until(&mut h, "the good track to play", |h| {
        h.state().playlist().current() == Some(1) && h.state().status().ended
    });
    // A list of nothing but unplayable files stops rather than spins.
    act(&mut h, "clear", "activate", None);
    add(&mut h, &d.0.join("gone1.wav"));
    add(&mut h, &d.0.join("gone2.wav"));
    act(&mut h, "play", "activate", None);
    run_until(&mut h, "the player to give up", |h| !h.state().is_ticking());
    assert!(text(&mut h, "status").contains("no such file"));
}

#[test]
fn the_equaliser_follows_its_sliders_and_presets() {
    let mut h = harness();
    act(&mut h, "eq_1k", "set_value", Some("6"));
    assert!((h.state().eq().bands[4] - 6.0).abs() < f32::EPSILON);
    act(&mut h, "preamp", "set_value", Some("-3"));
    assert!((h.state().eq().preamp + 3.0).abs() < f32::EPSILON);
    assert!(!h.state().eq().enabled);
    act(&mut h, "preset_rock", "activate", None);
    let eq = h.state().eq();
    assert!(eq.enabled, "choosing a preset switches the equaliser on");
    assert!((eq.bands[0] - 5.0).abs() < f32::EPSILON);
    let id = named(&mut h, "eq_60");
    assert!((h.widget::<Slider<Amp>>(id).value() - 5.0).abs() < f32::EPSILON);
    assert!(h.widget::<Slider<Amp>>(id).is_vertical());
    act(&mut h, "eq_on", "activate", None);
    assert!(!h.state().eq().enabled);
}

#[test]
fn folding_a_section_away_gives_its_space_back() {
    let mut h = harness();
    let pl = named(&mut h, "playlist_editor");
    let eq = named(&mut h, "equalizer");
    let open = h.bounds(pl).y;
    act(&mut h, "eq", "activate", None);
    assert!(h.ui().is_collapsed(eq));
    assert!(h.bounds(pl).y < open, "the playlist moved up");
    act(&mut h, "eq", "activate", None);
    assert!(!h.ui().is_collapsed(eq));
    assert!((h.bounds(pl).y - open).abs() < 0.5);
}

#[test]
fn the_window_fits_what_is_showing() {
    let mut h = harness();
    let full = h.ui().window_size();
    assert!(
        (full.h - h.ui().natural_size().h).abs() < 0.5,
        "opens at its natural size: {full:?}"
    );

    // Folding the playlist snaps the window to the rest, and locks it.
    act(&mut h, "pl", "activate", None);
    let natural = h.ui().natural_size();
    h.wait_for("the server's configure", |h| {
        (h.ui().window_size().h - natural.h).abs() < 0.5
    });
    assert!(h.ui().window_size().h < full.h - 100.0);

    // Folding the equaliser too leaves the main window alone.
    act(&mut h, "eq", "activate", None);
    let main_only = h.ui().natural_size().h;
    h.wait_for("the smaller window", |h| {
        (h.ui().window_size().h - main_only).abs() < 0.5
    });
    assert!(main_only < natural.h);

    // Unfolding both comes back to where it started.
    act(&mut h, "eq", "activate", None);
    act(&mut h, "pl", "activate", None);
    h.wait_for("the full window again", |h| {
        (h.ui().window_size().h - full.h).abs() < 0.5
    });
}

#[test]
fn a_dragged_open_playlist_keeps_its_height_across_folds() {
    let mut h = harness();
    let full = h.ui().window_size();
    // The user drags the window 100 px taller: the list takes it.
    h.configure(Size::new(full.w, full.h + 100.0));
    h.settle();
    let tall = h.ui().window_size().h;
    assert!((tall - (full.h + 100.0)).abs() < 0.5);

    // Folding the equaliser keeps the list's height, so the window
    // shrinks by the equaliser and one gap — to the pixel, since window
    // sizes are whole pixels.
    let eq = named(&mut h, "equalizer");
    let eq_h = h.bounds(eq).h;
    act(&mut h, "eq", "activate", None);
    h.wait_for("the shorter window", |h| {
        (h.ui().window_size().h - (tall - eq_h - 8.0)).abs() < 1.0
    });

    // Folding the playlist away and back restores the tall list.
    act(&mut h, "pl", "activate", None);
    act(&mut h, "pl", "activate", None);
    h.wait_for("the list at its dragged height", |h| {
        (h.ui().window_size().h - (tall - eq_h - 8.0)).abs() < 1.0
    });
}

#[test]
fn the_keys_are_winamps() {
    let d = TempDir::new("keys");
    tone(&d.0, "a.wav", 0.05);
    tone(&d.0, "b.wav", 0.05);
    let mut h = harness();
    add(&mut h, &d.0);
    // Keys go to the focused widget first; give the focus to something
    // that takes no letters.
    let vis = named(&mut h, "vis");
    h.ui().focus(vis);
    h.key(48); // b: next
    assert_eq!(h.state().playlist().current(), Some(0));
    h.key(48);
    assert_eq!(h.state().playlist().current(), Some(1));
    h.key(44); // z: previous
    assert_eq!(h.state().playlist().current(), Some(0));
    h.key(31); // s: shuffle
    assert!(h.state().playlist().shuffle());
    h.key(19); // r: repeat
    assert!(h.state().playlist().repeat());
    let before = h.state().volume();
    h.key(nitro_ui::event::key::DOWN);
    assert!((h.state().volume() - (before - 0.05)).abs() < 1e-6);
    h.key(45); // x: play
    run_until(&mut h, "playing", |h| h.state().status().token > 0);
    h.key(47); // v: stop
    run_until(&mut h, "stopped", |h| {
        h.state().status().state == State::Stopped
    });
}

#[test]
fn the_visualiser_cycles_and_a_stopped_player_goes_idle() {
    let d = TempDir::new("idle");
    let f = tone(&d.0, "t.wav", 0.3);
    let mut h = harness();
    let vis = named(&mut h, "vis");
    assert_eq!(h.widget::<Vis>(vis).mode(), Mode::Spectrum);
    act(&mut h, "vis", "set_value", Some("scope"));
    assert_eq!(h.widget::<Vis>(vis).mode(), Mode::Scope);
    act(&mut h, "vis", "set_value", Some("spectrum"));

    add(&mut h, &f);
    act(&mut h, "play", "activate", None);
    run_until(&mut h, "the track to end and the bars to fall", |h| {
        h.state().status().ended && !h.state().is_ticking()
    });
    assert!(h.widget::<Vis>(vis).is_settled());
    // Stopped, settled: no timer, and nothing sent.
    assert_eq!(h.next_timeout(), None);
    h.assert_idle(150);
}

#[test]
fn the_clock_flips_to_remaining_time() {
    let d = TempDir::new("clock");
    let f = tone(&d.0, "t.wav", 5.0);
    // Paced, so the track is still playing when the clock is read.
    let mut h = harness_with(Config {
        backend: nitro_amp::sink::Backend::Silent,
        ..Config::headless()
    });
    add(&mut h, &f);
    act(&mut h, "play", "activate", None);
    run_until(&mut h, "a second of play", |h| {
        text(h, "clock_text") == "00:01"
    });
    act(&mut h, "pause", "activate", None);
    run_until(&mut h, "the pause", |h| {
        h.state().status().state == State::Paused
    });
    act(&mut h, "clock", "activate", None);
    assert_eq!(text(&mut h, "clock_text"), "-00:03");
    act(&mut h, "clock", "activate", None);
    assert_eq!(text(&mut h, "clock_text"), "00:01");
    act(&mut h, "stop", "activate", None);
}

#[test]
fn state_survives_a_restart() {
    let d = TempDir::new("state");
    let f = tone(&d.0, "t.wav", 0.05);
    let config = Config {
        state_dir: Some(d.0.join("conf")),
        ..Config::headless()
    };
    {
        let mut h = harness_with(config.clone());
        add(&mut h, &f);
        act(&mut h, "volume", "set_value", Some("40"));
        act(&mut h, "eq_3k", "set_value", Some("-4.5"));
        act(&mut h, "shuffle", "activate", None);
        act(&mut h, "eq", "activate", None);
        // Dropping the harness drops the state, which saves.
    }
    let mut h = harness_with(config);
    assert_eq!(h.state().playlist().len(), 1);
    assert!((h.state().volume() - 0.4).abs() < 1e-6);
    assert!((h.state().eq().bands[5] + 4.5).abs() < f32::EPSILON);
    assert!(h.state().playlist().shuffle());
    let eq = named(&mut h, "equalizer");
    assert!(h.ui().is_collapsed(eq), "the fold is remembered");
    let id = named(&mut h, "volume");
    assert!((h.widget::<Slider<Amp>>(id).value() - 40.0).abs() < f32::EPSILON);
}
