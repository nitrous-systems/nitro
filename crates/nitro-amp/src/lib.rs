//! `nitro-amp` — a Winamp-style music player for nitro.
//!
//! Not a translation of Winamp's source, which is Windows C++ under its
//! own licence, and not a copy of its skins, which are artwork somebody
//! owns: a player that *behaves* like Winamp 2 — the main window, the
//! equaliser and the playlist editor docked beneath it — written the way
//! every app on this desktop is written: a state struct, a widget tree
//! built once, and callbacks.
//!
//! ```text
//! ┌──────────────────────────────────────────────┐
//! │ ┌────────────┐  01:23      128 kbps 44 kHz   │  vis, clock (click: remaining)
//! │ │▁▃▅▇▅▃▂▁▂▃▅▆│  ► 3. Artist - Title (4:05)   │  scrolling title
//! │ └────────────┘                               │
//! │ ═══════════●═════════════════════════════════ │  seek
//! │ ⏮ ▶ ⏸ ⏹ ⏭ ⏏            ☐ shuffle ☐ repeat │
//! │ vol ═════●══  bal ═══●═══       ☑ EQ ☑ PL │
//! │ output: pipewire                             │  status / errors
//! ├──────────────────────────────────────────────┤
//! │ ☑ on  Flat Rock Pop Classical Bass Treble    │  equaliser
//! │ ▮  ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮                        │  preamp + ten bands
//! ├──────────────────────────────────────────────┤
//! │ ► 1. First song                         3:12 │  playlist
//! │   2. Second song                        4:05 │
//! │ [ path or URL ] Add Files Folder Rm Clr      │
//! └──────────────────────────────────────────────┘
//! ```
//!
//! # Where the work happens
//!
//! | module | what |
//! |---|---|
//! | [`engine`] | the audio thread: decode → equalise → volume → output |
//! | [`source`] | decoders: WAV in-process ([`wav`]), everything else via `ffmpeg` |
//! | [`sink`] | output: `pw-cat`, `paplay` or `aplay`, or silence |
//! | [`dsp`] | the equaliser, the gain, the FFT and the analyser |
//! | [`playlist`] | the list, shuffle and repeat, M3U and PLS |
//! | [`vis`] | the spectrum analyser / oscilloscope widget |
//! | [`fmt`] | the clock and the scrolling title |
//!
//! This file is the window: it sends [`engine::Cmd`]s and, while
//! something is playing, reads the engine's [`engine::Status`] on a
//! 30 Hz tick to move the clock, the seek bar and the visualiser.
//!
//! # Idle when idle
//!
//! The tick runs only while there is something to show: while a track
//! plays, and after it stops for exactly as long as the analyser's bars
//! take to fall. A stopped or paused player has no timer, the audio
//! thread is blocked on its channel, and the process sleeps — the same
//! contract the bar and the terminal keep.
//!
//! # Scripting
//!
//! Every control is named, so `hey` drives the player with no code here
//! for it:
//!
//! ```text
//! hey nitro-amp do window/path set_text ~/Music
//! hey nitro-amp do window/add click
//! hey nitro-amp do window/add_folder click
//! hey nitro-amp do window[1]/picker_path set_text ~/Music
//! hey nitro-amp do window[1]/picker_ok click
//! hey nitro-amp do window/play click
//! hey nitro-amp get window/title value
//! hey nitro-amp do window/volume set_value 60
//! hey nitro-amp do window/eq_1k set_value 6
//! hey nitro-amp do window/vis set_value scope
//! ```
//!
//! # Opening files
//!
//! Winamp's split: *Open* — the eject button, `l`, `Ctrl+O` — shows a
//! [`nitro_ui::FilePicker`] for audio files and **replaces** the
//! playlist with what was picked, then plays it; `Shift+L` or
//! `Ctrl+Shift+O` does the same with a folder, and every playable track
//! under it. The playlist editor's *add files* and *add folder* buttons
//! **append** instead, and play nothing. One dialog at a time: asking
//! for another while one is up only says so on the status line. The
//! dialog starts where the last pick was made, remembered across runs.
//! The path field (`Ctrl+L`) still takes a path or a URL typed out.
//!
//! # Keys
//!
//! Winamp's: `z` previous, `x` play, `c` pause, `v` stop, `b` next,
//! `l` open files, `Shift+L` open a folder, `s` shuffle, `r` repeat,
//! `←`/`→` seek five seconds, `↑`/`↓` volume — each only when no widget
//! wanted the key (a focused slider keeps its arrows), and only in the
//! player's own window, never in the file dialog. `Ctrl+O` /
//! `Ctrl+Shift+O` open files / a folder, `Ctrl+L` focuses the path
//! field, `Ctrl+T` flips the clock, `Alt+G` the equaliser, `Alt+E` the
//! playlist, `Delete` removes the selected track and `Ctrl+Q` quits.

pub mod dsp;
pub mod engine;
pub mod fmt;
pub mod playlist;
pub mod sink;
pub mod source;
pub mod tap;
pub mod vis;
pub mod wav;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{Handled, KeyEvent, key, mods};
use nitro_ui::widgets::{
    Checkbox, Label, Slider, TextField, button, checkbox, column, label, panel, row, slider,
    spacer, text_field,
};
use nitro_ui::{
    App, ColorRole, CrossAlign, FilePicker, List, ListModel, Row, Size, Ui, WidgetId, WindowId,
    list,
};

use crate::dsp::{EQ_LABELS, EQ_RANGE_DB, EqSettings, PRESETS};
use crate::engine::{Cmd, Player, State, Status};
use crate::fmt::Marquee;
use crate::playlist::{Entry, Playlist};
use crate::sink::Backend;
use crate::source::Tools;
use crate::vis::{Vis, VisMut as _};

/// The name the app registers under, and the first argument to `hey`.
pub const APP_NAME: &str = "nitro-amp";

/// Milliseconds between ticks while something is moving: 30 Hz, the
/// rate the original's visualiser ran at by default, and plenty for a
/// clock with one-second resolution.
pub const TICK_MS: u64 = 33;

/// Ticks per step of the scrolling title (~5 characters a second).
const MARQUEE_EVERY: u64 = 6;

/// Cells in the title display.
const TITLE_CELLS: usize = 34;

/// Seconds `←` and `→` seek by.
const SEEK_STEP: f64 = 5.0;

/// The narrowest the window may be: the transport row — six buttons,
/// a spacer, `shuffle` and `repeat`, about 437 px — plus the main
/// panel's padding (2 × 10) and the root's (2 × 8), 473 px, rounded up
/// for slack in font metrics. Not `natural_size().w`: that is measured
/// unbounded, so it grows with the track title.
const MIN_WIDTH: f32 = 480.0;

/// The playlist's natural height, and the least it may be squeezed to.
const LIST_HEIGHT: f32 = 180.0;
/// See [`LIST_HEIGHT`]: about three rows.
const LIST_MIN_HEIGHT: f32 = 72.0;

/// Volume `↑` and `↓` step by, on the 0–100 slider.
const VOLUME_STEP: f32 = 5.0;

/// evdev codes for the letters the shortcuts use that `nitro_ui::key`
/// does not name.
mod code {
    /// `KEY_E`.
    pub const E: u32 = 18;
    /// `KEY_T`.
    pub const T: u32 = 20;
    /// `KEY_G`.
    pub const G: u32 = 34;
    /// `KEY_O`.
    pub const O: u32 = 24;
}

/// What the file dialog offers first. `source.rs` plays whatever
/// `ffmpeg` decodes, so this is a convenience, not a gate: "All files"
/// is always there too. `audio/*` covers the tracker formats
/// (`audio/x-it`, `audio/x-mod`, …) and, in freedesktop's `globs2`, the
/// playlists; `WebM` is filed under video. A folder is scanned by
/// [`playlist::AUDIO_EXTENSIONS`] instead.
const AUDIO_MIMES: &[&str] = &["audio/*", "video/webm"];

/// The playlist formats [`playlist::expand`] reads, as MIME types.
const PLAYLIST_MIMES: &[&str] = &[
    "audio/x-mpegurl",
    "audio/mpegurl",
    "application/vnd.apple.mpegurl",
    "audio/x-scpls",
];

/// Everything decided at start-up that the tests want to decide
/// differently.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where `ffmpeg` and `ffprobe` are.
    pub tools: Tools,
    /// The output.
    pub backend: Backend,
    /// Outputs to try, in order, if `backend`'s player will not start.
    pub fallbacks: Vec<Backend>,
    /// Where the playlist and settings are kept between runs, or `None`
    /// to keep nothing.
    pub state_dir: Option<PathBuf>,
    /// Seed for shuffle.
    pub seed: u64,
}

impl Config {
    /// The real thing: helpers from `$PATH`, the first audio player
    /// found, state under `$XDG_CONFIG_HOME/nitro`.
    #[must_use]
    pub fn detect() -> Self {
        let mut players = Backend::detect_all().into_iter();
        let backend = players.next().unwrap_or(Backend::Silent);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Self {
            tools: Tools::find(),
            backend,
            fallbacks: players.collect(),
            state_dir: config_dir(),
            seed,
        }
    }

    /// No helpers, no sound, no state on disk, and a track that plays as
    /// fast as it decodes: what the tests run.
    #[must_use]
    pub fn headless() -> Self {
        Self {
            tools: Tools::default(),
            backend: Backend::Unpaced,
            fallbacks: Vec::new(),
            state_dir: None,
            seed: 7,
        }
    }
}

/// `$XDG_CONFIG_HOME/nitro`, or `~/.config/nitro`.
fn config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && d.is_absolute()
    {
        return Some(d.join("nitro"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("nitro"))
}

/// The directories on `$PATH`.
pub(crate) fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// The first executable called `name` in `dirs`.
pub(crate) fn find_program(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    dirs.iter().map(|d| d.join(name)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// The widgets the app writes to after the tree is built.
#[derive(Debug, Clone, Copy)]
struct Ids {
    vis: WidgetId,
    clock: WidgetId,
    title: WidgetId,
    info: WidgetId,
    status: WidgetId,
    seek: WidgetId,
    volume: WidgetId,
    balance: WidgetId,
    shuffle: WidgetId,
    repeat: WidgetId,
    eq_toggle: WidgetId,
    pl_toggle: WidgetId,
    eq_section: WidgetId,
    eq_on: WidgetId,
    preamp: WidgetId,
    bands: [WidgetId; 10],
    pl_section: WidgetId,
    list: WidgetId,
    path: WidgetId,
    total: WidgetId,
}

/// The app's state.
pub struct Amp {
    player: Player,
    playlist: Playlist,
    ids: Option<Ids>,
    /// Token of the last load sent to the engine.
    token: u64,
    /// Whether that load was meant to play.
    autoplay: bool,
    /// The last token whose end or failure has been acted on.
    handled: u64,
    /// Loads that failed in a row while advancing, so a list of
    /// unplayable files stops rather than spinning.
    failures: usize,
    /// Clock shows time remaining.
    remaining: bool,
    marquee: Marquee,
    eq: EqSettings,
    volume: f32,
    balance: f32,
    /// A tick is scheduled.
    ticking: bool,
    ticks: u64,
    /// Which sections are unfolded; kept across runs.
    shown_sections: Sections,
    /// Window height beyond its natural height while the playlist shows:
    /// the part of the list the user dragged open. See
    /// [`remember_list_height`].
    list_extra: f32,
    /// The last status the window drew, to change only what changed.
    shown: Shown,
    state_dir: Option<PathBuf>,
    output: Output,
    /// The file dialog, while one is open: there is only ever one.
    picker: Option<WindowId>,
    /// Where the last pick was made, and where the next dialog starts.
    last_dir: Option<PathBuf>,
}

/// Which of the docked sections are unfolded.
#[derive(Debug, Clone, Copy)]
struct Sections {
    /// The equaliser.
    eq: bool,
    /// The playlist editor.
    pl: bool,
}

/// The output, as the status line describes it.
#[derive(Debug, Clone, Copy)]
struct Output {
    /// `pipewire`, `pulseaudio`, `alsa` or `silent`.
    name: &'static str,
    /// Whether anything will be heard.
    audible: bool,
}

/// What is on screen, for diffing against the next status.
#[derive(Debug, Clone, Default, PartialEq)]
struct Shown {
    state: State,
    second: Option<i64>,
    duration: Option<f64>,
    error: Option<String>,
    token: u64,
}

impl Amp {
    /// A player with an empty playlist.
    ///
    /// # Errors
    /// If the audio thread cannot be started.
    pub fn new(config: Config) -> std::io::Result<Self> {
        let output = Output {
            name: config.backend.name(),
            audible: config.backend.is_audible(),
        };
        let player = Player::spawn_with_fallbacks(config.tools, config.backend, config.fallbacks)?;
        let mut amp = Self {
            player,
            playlist: Playlist::with_seed(config.seed),
            ids: None,
            token: 0,
            autoplay: false,
            handled: 0,
            failures: 0,
            remaining: false,
            marquee: Marquee::default(),
            eq: EqSettings::default(),
            volume: 0.8,
            balance: 0.0,
            ticking: false,
            ticks: 0,
            shown_sections: Sections { eq: true, pl: true },
            list_extra: 0.0,
            shown: Shown::default(),
            state_dir: config.state_dir,
            output,
            picker: None,
            last_dir: None,
        };
        amp.restore();
        amp.player.send(Cmd::Volume(amp.volume));
        amp.player.send(Cmd::Balance(amp.balance));
        amp.player.send(Cmd::Eq(amp.eq));
        Ok(amp)
    }

    /// The playlist.
    #[must_use]
    pub fn playlist(&self) -> &Playlist {
        &self.playlist
    }

    /// The engine's status now.
    #[must_use]
    pub fn status(&self) -> Status {
        self.player.status()
    }

    /// The equaliser settings.
    #[must_use]
    pub fn eq(&self) -> EqSettings {
        self.eq
    }

    /// The volume, `0.0..=1.0`.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Whether a tick is scheduled — `false` is the idle state.
    #[must_use]
    pub fn is_ticking(&self) -> bool {
        self.ticking
    }

    /// Start playing as soon as the window is up: what naming files on
    /// the command line means.
    pub fn start_on_open(&mut self) {
        self.autoplay = true;
    }

    /// Add what `paths` name to the playlist, before the window exists:
    /// the command line.
    pub fn add_paths(&mut self, paths: &[PathBuf]) {
        for p in paths {
            self.playlist.extend(playlist::expand(p));
        }
    }

    /// Read the saved playlist and settings, if there is a state dir.
    fn restore(&mut self) {
        let Some(dir) = &self.state_dir else { return };
        if let Ok(text) = std::fs::read_to_string(dir.join("amp.m3u8")) {
            self.playlist.extend(playlist::parse_m3u(&text, dir));
        }
        let Ok(text) = std::fs::read_to_string(dir.join("amp.conf")) else {
            return;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            let f = v.parse::<f32>().ok().filter(|f| f.is_finite());
            match (k, f) {
                ("volume", Some(f)) => self.volume = f.clamp(0.0, 1.0),
                ("balance", Some(f)) => self.balance = f.clamp(-1.0, 1.0),
                ("eq.preamp", Some(f)) => self.eq.preamp = f.clamp(-EQ_RANGE_DB, EQ_RANGE_DB),
                ("eq.enabled", _) => self.eq.enabled = v == "1",
                ("shuffle", _) => self.playlist.set_shuffle(v == "1"),
                ("repeat", _) => self.playlist.set_repeat(v == "1"),
                ("remaining", _) => self.remaining = v == "1",
                ("show.eq", _) => self.shown_sections.eq = v != "0",
                ("show.pl", _) => self.shown_sections.pl = v != "0",
                ("list.extra", Some(f)) => self.list_extra = f.clamp(-1_000.0, 10_000.0),
                ("dir", _) if !v.is_empty() => self.last_dir = Some(PathBuf::from(v)),
                ("current", _) => {
                    if let Ok(i) = v.parse::<usize>() {
                        self.playlist.set_current(i);
                    }
                }
                ("eq.bands", _) => {
                    for (b, s) in self.eq.bands.iter_mut().zip(v.split(',')) {
                        if let Ok(f) = s.trim().parse::<f32>()
                            && f.is_finite()
                        {
                            *b = f.clamp(-EQ_RANGE_DB, EQ_RANGE_DB);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Write the playlist and settings back. Best-effort: a player that
    /// cannot save its playlist should still quit.
    fn save(&self) {
        let Some(dir) = &self.state_dir else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let _ = std::fs::write(dir.join("amp.m3u8"), self.playlist.to_m3u());
        let bands: Vec<String> = self.eq.bands.iter().map(ToString::to_string).collect();
        let b = |on: bool| if on { "1" } else { "0" };
        let conf = format!(
            "# nitro-amp settings, rewritten on quit\n\
             volume={}\nbalance={}\nremaining={}\nshuffle={}\nrepeat={}\ncurrent={}\n\
             show.eq={}\nshow.pl={}\nlist.extra={}\n\
             eq.enabled={}\neq.preamp={}\neq.bands={}\ndir={}\n",
            self.volume,
            self.balance,
            b(self.remaining),
            b(self.playlist.shuffle()),
            b(self.playlist.repeat()),
            self.playlist
                .current()
                .map_or_else(String::new, |c| c.to_string()),
            b(self.shown_sections.eq),
            b(self.shown_sections.pl),
            self.list_extra,
            b(self.eq.enabled),
            self.eq.preamp,
            bands.join(","),
            self.last_dir
                .as_deref()
                .map_or_else(String::new, |d| d.display().to_string()),
        );
        let _ = std::fs::write(dir.join("amp.conf"), conf);
    }
}

impl Drop for Amp {
    fn drop(&mut self) {
        self.save();
    }
}

// -- transport ------------------------------------------------------

/// Load track `i` and, if `play`, start it.
fn load(s: &mut Amp, ui: &mut Ui<Amp>, i: usize, play: bool) {
    let Some(e) = s.playlist.get(i) else { return };
    let (path, line) = (e.path.clone(), fmt::title_line(i + 1, &e.title, e.duration));
    s.playlist.set_current(i);
    s.token += 1;
    s.autoplay = play;
    s.player.send(Cmd::Load {
        path,
        play,
        token: s.token,
    });
    s.marquee.set(&line);
    show_title(s, ui);
    // The clock belongs to the old track until the engine reports on
    // the new one; zero it now rather than show the old time for a tick.
    if let Some(ids) = s.ids
        && let Ok(mut l) = ui.widget_mut::<Label>(ids.clock)
    {
        l.set_text(fmt::clock(0.0, false));
    }
    // A new current track is not an edit: re-point the rows, O(1).
    show_rows(s, ui);
    ensure_ticking(s, ui);
}

/// Play: resume a pause, restart a stopped track, or start the list.
pub fn play(s: &mut Amp, ui: &mut Ui<Amp>) {
    let st = s.player.with_status(|st| (st.state, st.token));
    let loaded = s.token > 0 && st.1 == s.token && s.playlist.current().is_some();
    // An error on a track that is still paused or playing is a notice
    // (the output moved to another player), not a failed load: resume.
    if loaded && (s.shown.error.is_none() || st.0 != State::Stopped) {
        s.player.send(Cmd::Play);
        s.autoplay = true;
        ensure_ticking(s, ui);
        return;
    }
    let Some(i) = s.playlist.current().or_else(|| s.playlist.first()) else {
        set_status(
            ui,
            s,
            Some("playlist is empty: add a file, a folder or a playlist"),
        );
        return;
    };
    s.failures = 0;
    load(s, ui, i, true);
}

/// Pause, or resume a pause.
pub fn pause(s: &mut Amp, ui: &mut Ui<Amp>) {
    s.player.send(Cmd::Pause);
    ensure_ticking(s, ui);
}

/// Stop and rewind.
pub fn stop(s: &mut Amp, ui: &mut Ui<Amp>) {
    s.autoplay = false;
    s.player.send(Cmd::Stop);
    ensure_ticking(s, ui);
}

/// Step through the list by `dir` (±1). Playing carries on playing; a
/// stopped player just moves the cursor.
fn skip(s: &mut Amp, ui: &mut Ui<Amp>, forward: bool) {
    let next = if forward {
        s.playlist.next()
    } else {
        s.playlist.prev()
    };
    let Some(i) = next else { return };
    let playing = s.player.with_status(|st| st.state == State::Playing);
    s.failures = 0;
    load(s, ui, i, playing);
}

/// Seek by `delta` seconds from where the listener is.
fn seek_by(s: &mut Amp, ui: &mut Ui<Amp>, delta: f64) {
    let at = s.player.with_status(|st| st.position);
    s.player.send(Cmd::Seek((at + delta).max(0.0)));
    ensure_ticking(s, ui);
}

/// Start the tick if it is not running.
fn ensure_ticking(s: &mut Amp, ui: &mut Ui<Amp>) {
    if !s.ticking {
        s.ticking = true;
        ui.set_timer(TICK_MS, tick);
    }
}

// -- the tick -------------------------------------------------------

/// Read the engine's status and bring the window up to date; re-arm
/// while anything is still moving.
fn tick(s: &mut Amp, ui: &mut Ui<Amp>) {
    s.ticking = false;
    s.ticks += 1;
    let st = s.player.status();
    let Some(ids) = s.ids else { return };

    // Tags arrive with the load: give the entry its real name. First,
    // because a short track can load, play and end between two ticks,
    // and the end is handled below by loading the next one.
    //
    // Compared through a shared borrow and written only on a real change:
    // the entries are shared with the list widget, and a write is an
    // edit that must first take the widget's handle back (`edit_list`).
    // Asking for `&mut` on every tick to find out would do that thirty
    // times a second.
    if st.token == s.token
        && let Some(i) = s.playlist.current()
        && let Some(e) = s.playlist.get(i)
    {
        let title = st.meta.display_title().filter(|t| *t != e.title);
        let duration = st.duration.filter(|d| e.duration != Some(*d));
        if title.is_some() || duration.is_some() {
            edit_list(s, ui, |p| {
                if let Some(e) = p.get_mut(i) {
                    if let Some(t) = title {
                        e.title = t;
                    }
                    if duration.is_some() {
                        e.duration = duration;
                    }
                }
            });
            if let Some(e) = s.playlist.get(i) {
                s.marquee.set(&fmt::title_line(i + 1, &e.title, e.duration));
            }
        }
    }

    // A load that failed, or a track that ended: act once per token.
    if st.token == s.token && s.handled != s.token {
        if st.ended {
            s.handled = s.token;
            s.failures = 0;
            advance(s, ui);
            // `advance` loaded something (a new token) or stopped; this
            // tick's status is about the old track either way.
            ensure_ticking(s, ui);
            return;
        }
        if st.error.is_some() && st.state == State::Stopped && s.autoplay {
            s.handled = s.token;
            s.failures += 1;
            if s.failures < s.playlist.len() {
                advance(s, ui);
            }
        }
    }

    // Clock, seek bar, info: only when the second or the track changed.
    let second = (st.token == s.token && st.state != State::Stopped || st.position > 0.0)
        .then(|| st.position.floor() as i64);
    let shown = Shown {
        state: st.state,
        second,
        duration: st.duration,
        error: st.error.clone(),
        token: st.token,
    };
    if shown != s.shown {
        show_clock(s, ui, &st);
        if let Ok(mut sl) = ui.widget_mut::<Slider<Amp>>(ids.seek) {
            sl.set_range(0.0, st.duration.unwrap_or(0.0) as f32);
            sl.set_value(st.position as f32);
            sl.set_enabled(st.duration.is_some());
        }
        show_info(ui, ids, &st);
        set_status(ui, s, st.error.as_deref());
        if st.state == State::Playing {
            s.failures = 0;
        }
        s.shown = shown;
    }

    // The visualiser moves while playing; stopped or paused, it is fed
    // silence and its bars fall.
    let playing = st.state == State::Playing;
    let settled = match ui.widget_mut::<Vis>(ids.vis) {
        Ok(mut v) => {
            if playing {
                v.feed(&st.scope, st.rate);
            } else {
                v.feed(&[], st.rate);
            }
            v.is_settled()
        }
        Err(_) => true,
    };

    if playing && s.ticks.is_multiple_of(MARQUEE_EVERY) {
        s.marquee.step(TITLE_CELLS);
        show_title(s, ui);
    }

    // Keep going while anything moves — and while the engine has not
    // yet seen the last command, whose effect this status cannot show.
    if playing || !settled || !s.player.caught_up() {
        ensure_ticking(s, ui);
    }
}

/// After a track ends (or will not load): the next one, or stop.
fn advance(s: &mut Amp, ui: &mut Ui<Amp>) {
    if let Some(i) = s.playlist.next() {
        load(s, ui, i, true);
    } else {
        s.autoplay = false;
        s.player.send(Cmd::Stop);
    }
}

// -- drawing --------------------------------------------------------

fn show_title(s: &Amp, ui: &mut Ui<Amp>) {
    let Some(ids) = s.ids else { return };
    let text = s.marquee.window(TITLE_CELLS);
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.title) {
        l.set_text(text);
    }
}

fn show_clock(s: &Amp, ui: &mut Ui<Amp>, st: &Status) {
    let Some(ids) = s.ids else { return };
    let text = if st.token == 0 || s.playlist.current().is_none() {
        "00:00".to_owned()
    } else if s.remaining
        && let Some(d) = st.duration
    {
        fmt::clock(d - st.position, true)
    } else {
        fmt::clock(st.position, false)
    };
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.clock) {
        l.set_text(text);
    }
}

fn show_info(ui: &mut Ui<Amp>, ids: Ids, st: &Status) {
    let m = &st.meta;
    let mut parts = Vec::new();
    if let Some(k) = m.kbps {
        parts.push(format!("{k} kbps"));
    }
    if let Some(r) = m.rate {
        parts.push(format!("{} kHz", (r + 500) / 1000));
    }
    match m.channels {
        Some(1) => parts.push("mono".to_owned()),
        Some(_) => parts.push("stereo".to_owned()),
        None => {}
    }
    let state = match st.state {
        State::Playing => "▶",
        State::Paused => "❚❚",
        State::Stopped => "■",
    };
    let text = format!("{state}  {}", parts.join("  "));
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.info) {
        l.set_text(text.trim_end());
    }
}

fn set_status(ui: &mut Ui<Amp>, s: &Amp, error: Option<&str>) {
    let Some(ids) = s.ids else { return };
    let (text, role) = match error {
        Some(e) => (e.to_owned(), ColorRole::Danger),
        // The engine's output, not the configured one: a player that
        // would not start has been replaced by the next.
        None => match s.player.with_status(|st| st.output) {
            name if name != "silent" => (format!("output: {name}"), ColorRole::TextDim),
            _ if s.output.audible => (
                format!(
                    "{}: would not start, nor any other player: playing silently",
                    s.output.name
                ),
                ColorRole::Warning,
            ),
            _ => (
                "no audio player found (pw-cat, paplay, aplay): playing silently".to_owned(),
                ColorRole::Warning,
            ),
        },
    };
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.status) {
        l.set_text(text);
        l.set_color_role(role);
    }
}

/// The list widget's model: the playlist's own entries, shared rather
/// than copied, formatted into a row only when that row is drawn.
///
/// `row(i)` runs for the materialised rows — a screenful — so the cost of
/// showing the list, and of moving the "now playing" mark, is the same
/// at ten tracks or a hundred thousand. See [`Playlist`]'s notes on the
/// sharing.
struct PlaylistRows {
    entries: Rc<Vec<Entry>>,
    current: Option<usize>,
}

impl ListModel for PlaylistRows {
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn row(&self, index: usize) -> Row {
        let Some(e) = self.entries.get(index) else {
            return Row::default();
        };
        let r = Row::new(format!("{}. {}", index + 1, e.title)).detail(fmt::length(e.duration));
        if self.current == Some(index) {
            r.icon("play-fill")
        } else {
            r
        }
    }
}

/// Stands in for [`PlaylistRows`] while the playlist is being edited: the
/// same length, so the list keeps its cursor, selection and scroll, and
/// no handle on the entries, so the edit is not a copy. It is never
/// drawn — the edit and the real model's return happen in one callback,
/// before any paint.
struct Releasing(usize);

impl ListModel for Releasing {
    fn len(&self) -> usize {
        self.0
    }

    fn row(&self, _index: usize) -> Row {
        Row::default()
    }
}

/// Edit the playlist's entries in place, and show the result.
///
/// Takes the list widget's handle on the entries back first, so the
/// edit does not copy them (`Rc::make_mut` on an unshared `Rc` is a
/// no-op). Anything that *moves the current track* is not an edit and
/// should not come here; [`show_rows`] is enough for that.
fn edit_list(s: &mut Amp, ui: &mut Ui<Amp>, f: impl FnOnce(&mut Playlist)) {
    if let Some(ids) = s.ids
        && let Ok(mut l) = ui.widget_mut::<List<Amp>>(ids.list)
    {
        l.set_model(Box::new(Releasing(s.playlist.len())));
    }
    f(&mut s.playlist);
    refresh_list(s, ui);
}

/// Point the list at the entries as they are now. O(1): a reference
/// count and a model swap, whatever the length — which is what makes
/// next, previous and "play this row" cost the same on any playlist.
fn show_rows(s: &Amp, ui: &mut Ui<Amp>) {
    let Some(ids) = s.ids else { return };
    let model = PlaylistRows {
        entries: s.playlist.shared(),
        current: s.playlist.current(),
    };
    if let Ok(mut l) = ui.widget_mut::<List<Amp>>(ids.list) {
        l.set_model(Box::new(model));
    }
}

/// [`show_rows`], and the total underneath, which does read every entry
/// — so this is for after an edit, not for a track change.
fn refresh_list(s: &Amp, ui: &mut Ui<Amp>) {
    let Some(ids) = s.ids else { return };
    show_rows(s, ui);
    let known: f64 = s.playlist.entries().iter().filter_map(|e| e.duration).sum();
    let unknown = s.playlist.entries().iter().any(|e| e.duration.is_none());
    let total = format!(
        "{} tracks  {}{}",
        s.playlist.len(),
        fmt::length(Some(known)),
        if unknown { "+" } else { "" }
    );
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.total) {
        l.set_text(total);
    }
}

// -- controls -------------------------------------------------------

fn add_from_field(s: &mut Amp, ui: &mut Ui<Amp>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let path = expand_tilde(text);
    let entries = playlist::expand(&path);
    let n = entries.len();
    edit_list(s, ui, |p| p.extend(entries));
    if let Some(ids) = s.ids
        && let Ok(mut f) = ui.widget_mut::<TextField<Amp>>(ids.path)
    {
        f.set_text("");
    }
    let msg = if n == 0 {
        Some(format!("{}: no audio files there", path.display()))
    } else {
        None
    };
    set_status(ui, s, msg.as_deref());
}

/// Whether the file dialog is up.
fn picker_open(s: &Amp, ui: &Ui<Amp>) -> bool {
    s.picker.is_some_and(|w| ui.has_window(w))
}

/// Whether a key belongs to the player: the app-level handlers are
/// offered every key no widget took, the dialog's window included, and
/// a letter typed at the file dialog is not "pause".
fn keys_are_ours(s: &Amp, ui: &Ui<Amp>) -> bool {
    !(picker_open(s, ui) && ui.active_window() != WindowId::MAIN)
}

/// Show the file dialog: files, or with `folder` a folder, whose
/// tracks then `replace` the playlist (and play) or are appended.
fn open_picker(s: &mut Amp, ui: &mut Ui<Amp>, folder: bool, replace: bool) {
    if picker_open(s, ui) {
        set_status(ui, s, Some("a file dialog is already open"));
        return;
    }
    let mut p = if folder {
        FilePicker::folder()
    } else {
        FilePicker::open()
            .filter("Audio", AUDIO_MIMES.iter().copied())
            .filter("Playlists", PLAYLIST_MIMES.iter().copied())
    }
    .multiple(true)
    .title(match (folder, replace) {
        (false, true) => "Open Files",
        (false, false) => "Add Files",
        (true, true) => "Open Folder",
        (true, false) => "Add Folders",
    })
    .on_done(move |s: &mut Amp, ui: &mut Ui<Amp>, picked| {
        s.picker = None;
        if let Some(paths) = picked {
            add_picked(s, ui, &paths, replace);
        }
    });
    if let Some(d) = s.last_dir.as_ref().filter(|d| d.is_dir()) {
        p = p.start_dir(d.clone());
    }
    match p.open_in(ui) {
        Ok(w) => s.picker = Some(w),
        Err(e) => set_status(ui, s, Some(&format!("cannot open a file dialog: {e}"))),
    }
}

/// What the dialog answered: every playable track under `paths`, in
/// place of the playlist (then playing) or after it.
fn add_picked(s: &mut Amp, ui: &mut Ui<Amp>, paths: &[PathBuf], replace: bool) {
    let Some(first) = paths.first() else { return };
    s.last_dir = if first.is_dir() {
        Some(first.clone())
    } else {
        first.parent().map(Path::to_path_buf)
    };
    let entries: Vec<Entry> = paths.iter().flat_map(|p| playlist::expand(p)).collect();
    if entries.is_empty() {
        // Nothing playable: leave the list alone, even for a replace.
        let msg = format!("{}: no audio files there", first.display());
        set_status(ui, s, Some(&msg));
        return;
    }
    if replace {
        // As `clear_list`: a fresh list, not an edit of the old one.
        s.playlist.clear();
        s.player.send(Cmd::Stop);
        s.playlist.extend(entries);
        refresh_list(s, ui);
        s.failures = 0;
        set_status(ui, s, None);
        if let Some(i) = s.playlist.first() {
            load(s, ui, i, true);
        }
    } else {
        edit_list(s, ui, |p| p.extend(entries));
        set_status(ui, s, None);
    }
}

/// Unfold the playlist and put the cursor in the path field.
fn focus_path(s: &mut Amp, ui: &mut Ui<Amp>) {
    if let Some(ids) = s.ids {
        toggle_section(s, ui, false, true);
        ui.focus(ids.path);
    }
}

fn expand_tilde(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest);
    }
    PathBuf::from(text)
}

fn remove_selected(s: &mut Amp, ui: &mut Ui<Amp>) {
    let Some(ids) = s.ids else { return };
    let Ok(l) = ui.widget::<List<Amp>>(ids.list) else {
        return;
    };
    let i = l.cursor();
    if i >= s.playlist.len() {
        return;
    }
    let was_current = s.playlist.current() == Some(i);
    edit_list(s, ui, |p| {
        p.remove(i);
    });
    if was_current {
        // The loaded track is gone from the list; it may keep playing,
        // but nothing will follow it until the user picks again.
        s.autoplay = false;
    }
}

fn clear_list(s: &mut Amp, ui: &mut Ui<Amp>) {
    // Not `edit_list`: clearing replaces the entries rather than editing
    // them, so it never copies, shared or not.
    s.playlist.clear();
    s.autoplay = false;
    s.player.send(Cmd::Stop);
    refresh_list(s, ui);
    ensure_ticking(s, ui);
}

fn set_eq(s: &mut Amp) {
    s.player.send(Cmd::Eq(s.eq));
}

fn apply_preset(s: &mut Amp, ui: &mut Ui<Amp>, i: usize) {
    let Some(ids) = s.ids else { return };
    s.eq.bands = PRESETS[i].bands;
    s.eq.enabled = true;
    for (id, v) in ids.bands.iter().zip(s.eq.bands) {
        if let Ok(mut sl) = ui.widget_mut::<Slider<Amp>>(*id) {
            sl.set_value(v);
        }
    }
    if let Ok(mut c) = ui.widget_mut::<Checkbox<Amp>>(ids.eq_on) {
        c.set_checked(true);
    }
    set_eq(s);
}

fn set_volume(s: &mut Amp, ui: &mut Ui<Amp>, percent: f32) {
    s.volume = (percent / 100.0).clamp(0.0, 1.0);
    s.player.send(Cmd::Volume(s.volume));
    if let Some(ids) = s.ids
        && let Ok(mut sl) = ui.widget_mut::<Slider<Amp>>(ids.volume)
    {
        sl.set_value(s.volume * 100.0);
    }
}

fn toggle_section(s: &mut Amp, ui: &mut Ui<Amp>, eq: bool, show: bool) {
    let Some(ids) = s.ids else { return };
    let (section, cb) = if eq {
        (ids.eq_section, ids.eq_toggle)
    } else {
        (ids.pl_section, ids.pl_toggle)
    };
    if let Ok(mut c) = ui.widget_mut::<Checkbox<Amp>>(cb) {
        c.set_checked(show);
    }
    if ui.is_collapsed(section) != show {
        return;
    }
    remember_list_height(s, ui, ids);
    ui.set_collapsed(section, !show);
    if eq {
        s.shown_sections.eq = show;
    } else {
        s.shown_sections.pl = show;
    }
    fit_window(s, ui);
}

/// Note how much taller than its natural size the window is while the
/// playlist shows: that surplus is the list the user dragged open, and
/// it is what folding and unfolding sections must keep.
fn remember_list_height(s: &mut Amp, ui: &mut Ui<Amp>, ids: Ids) {
    if !ui.is_collapsed(ids.pl_section) {
        s.list_extra = ui.window_size().h - ui.natural_size().h;
    }
}

/// Size the window to what is showing, as Winamp's docked windows do.
///
/// Without the playlist, the main window and the equaliser have one
/// right height — their content's — so the window is set to it and
/// **locked** there: the limits' minimum and maximum height are equal,
/// and the server refuses a vertical drag. With the playlist, the window
/// is resizable again, the list takes the slack, and it comes back at
/// the height it had when it was folded away. The width is left where
/// the user put it, above the minimum.
fn fit_window(s: &Amp, ui: &mut Ui<Amp>) {
    let Some(ids) = s.ids else { return };
    let natural = ui.natural_size();
    let width = ui.window_size().w.max(MIN_WIDTH);
    let (min, max, height) = if ui.is_collapsed(ids.pl_section) {
        (
            Size::new(MIN_WIDTH, natural.h),
            Size::new(0.0, natural.h),
            natural.h,
        )
    } else {
        // The list may be squeezed below its natural height, to a few
        // rows, but not away.
        let floor = natural.h - LIST_HEIGHT + LIST_MIN_HEIGHT;
        (
            Size::new(MIN_WIDTH, floor),
            Size::ZERO,
            natural.h + s.list_extra,
        )
    };
    // Limits first: the request is clamped to them, and on the way from
    // a locked height to a free one the old maximum would refuse it.
    let _ = ui.set_window_limits(min, max);
    let _ = ui.request_window_size(Size::new(width, height));
}

fn set_checkbox(ui: &mut Ui<Amp>, id: WidgetId, on: bool) {
    if let Ok(mut c) = ui.widget_mut::<Checkbox<Amp>>(id) {
        c.set_checked(on);
    }
}

fn toggle_remaining(s: &mut Amp, ui: &mut Ui<Amp>) {
    s.remaining = !s.remaining;
    let st = s.player.status();
    show_clock(s, ui, &st);
}

// -- the tree -------------------------------------------------------

/// A transport button: an icon when the server has them, the name when
/// it does not, and the name either way for `hey`.
fn transport(
    name: &'static str,
    icon: &'static str,
    f: fn(&mut Amp, &mut Ui<Amp>),
) -> nitro_ui::widgets::ButtonBuilder<Amp> {
    button("")
        .name(name)
        .icon(icon)
        .on_click(move |s: &mut Amp, ui: &mut Ui<Amp>| f(s, ui))
}

/// Build the whole window and return its root.
///
/// Public so the tests build the tree the binary builds. The tree is
/// built from default settings — the toolkit hands a tree builder no
/// state — and a deferred call, which runs before any input can arrive,
/// then gives the state its widget ids and moves every control to where
/// the saved settings say.
///
/// # Panics
/// Never in practice: every `attach` names an id just created here.
#[allow(clippy::too_many_lines)] // one tree, read top to bottom
pub fn build(ui: &mut Ui<Amp>) -> WidgetId {
    // ---- the main window ----
    let visw = ui.build(vis::vis().name("vis"));
    let clock = ui.build(
        label("00:00")
            .name("clock_text")
            .family("mono")
            .size(26.0)
            .weight(600),
    );
    let clock_tap = ui.build(
        tap::tap()
            .name("clock")
            .on_tap(|s: &mut Amp, ui: &mut Ui<Amp>| toggle_remaining(s, ui)),
    );
    ui.attach(clock_tap, clock).unwrap();
    let info = ui.build(
        label("■")
            .name("info")
            .size(11.0)
            .color_role(ColorRole::TextDim),
    );
    let title = ui.build(
        label("nitro-amp")
            .name("title")
            .family("mono")
            .size(13.0)
            .elide(true)
            .width_percent(1.0),
    );
    let readout = ui.build(
        // Shrinks to zero so the title elides instead of pushing past
        // the window.
        column().gap(2.0).grow(1.0).shrink_to_zero().child(
            row()
                .gap(10.0)
                .cross_align(CrossAlign::End)
                .width_percent(1.0),
        ),
    );
    let top_row = ui.children(readout)[0];
    ui.attach(top_row, clock_tap).unwrap();
    ui.attach(top_row, info).unwrap();
    ui.attach(readout, title).unwrap();
    let display = ui.build(row().gap(10.0).width_percent(1.0));
    ui.attach(display, visw).unwrap();
    ui.attach(display, readout).unwrap();

    let seek = ui.build(
        slider(0.0)
            .name("seek")
            .range(0.0, 0.0)
            .disabled()
            .width_percent(1.0)
            .on_change(|s: &mut Amp, ui: &mut Ui<Amp>, v: f32| {
                s.player.send(Cmd::Seek(f64::from(v)));
                ensure_ticking(s, ui);
            }),
    );

    let shuffle = ui.build(checkbox("shuffle").name("shuffle").on_toggle(
        |s: &mut Amp, _ui: &mut Ui<Amp>, on: bool| {
            s.playlist.set_shuffle(on);
        },
    ));
    let repeat = ui.build(checkbox("repeat").name("repeat").on_toggle(
        |s: &mut Amp, _ui: &mut Ui<Amp>, on: bool| {
            s.playlist.set_repeat(on);
        },
    ));
    let eq_toggle = ui.build(
        checkbox("EQ")
            .name("eq")
            .checked(true)
            .on_toggle(|s: &mut Amp, ui: &mut Ui<Amp>, on: bool| toggle_section(s, ui, true, on)),
    );
    let pl_toggle = ui.build(
        checkbox("PL")
            .name("pl")
            .checked(true)
            .on_toggle(|s: &mut Amp, ui: &mut Ui<Amp>, on: bool| toggle_section(s, ui, false, on)),
    );
    let transport_row = ui.build(
        row()
            .gap(4.0)
            .cross_align(CrossAlign::Center)
            .width_percent(1.0)
            .child(transport("prev", "skip-start-fill", |s, ui| {
                skip(s, ui, false);
            }))
            .child(transport("play", "play-fill", play))
            .child(transport("pause", "pause-fill", pause))
            .child(transport("stop", "stop-fill", stop))
            .child(transport("next", "skip-end-fill", |s, ui| {
                skip(s, ui, true);
            }))
            .child(transport("eject", "eject-fill", |s, ui| {
                open_picker(s, ui, false, true);
            }))
            .child(spacer().grow(1.0)),
    );
    for c in [shuffle, repeat] {
        ui.attach(transport_row, c).unwrap();
    }

    let volume = ui.build(
        slider(80.0)
            .name("volume")
            .range(0.0, 100.0)
            .step(1.0)
            .width(110.0)
            .on_change(|s: &mut Amp, ui: &mut Ui<Amp>, v: f32| set_volume(s, ui, v)),
    );
    let balance = ui.build(
        slider(0.0)
            .name("balance")
            .range(-100.0, 100.0)
            .step(1.0)
            .width(70.0)
            .on_change(|s: &mut Amp, _ui: &mut Ui<Amp>, v: f32| {
                s.balance = v / 100.0;
                s.player.send(Cmd::Balance(s.balance));
            }),
    );
    let status = ui.build(
        label("")
            .name("status")
            .size(11.0)
            .color_role(ColorRole::TextDim)
            .elide(true)
            .width_percent(1.0),
    );
    let mix_row = ui.build(
        row()
            .gap(6.0)
            .cross_align(CrossAlign::Center)
            .width_percent(1.0)
            .child(label("vol").size(11.0).color_role(ColorRole::TextDim)),
    );
    ui.attach(mix_row, volume).unwrap();
    let bal = ui.build(label("bal").size(11.0).color_role(ColorRole::TextDim));
    ui.attach(mix_row, bal).unwrap();
    ui.attach(mix_row, balance).unwrap();
    let sp = ui.build(spacer().grow(1.0));
    for c in [sp, eq_toggle, pl_toggle] {
        ui.attach(mix_row, c).unwrap();
    }

    let main = ui.build(
        panel()
            .background_role(ColorRole::Surface)
            .radius(6.0)
            .padding(10.0)
            .gap(8.0)
            .width_percent(1.0),
    );
    for c in [display, seek, transport_row, mix_row, status] {
        ui.attach(main, c).unwrap();
    }

    // ---- the equaliser ----
    let eq_on = ui.build(checkbox("on").name("eq_on").on_toggle(
        |s: &mut Amp, _ui: &mut Ui<Amp>, on: bool| {
            s.eq.enabled = on;
            set_eq(s);
        },
    ));
    let presets = ui.build(
        row()
            .gap(4.0)
            .cross_align(CrossAlign::Center)
            .width_percent(1.0),
    );
    ui.attach(presets, eq_on).unwrap();
    let sp = ui.build(spacer().grow(1.0));
    ui.attach(presets, sp).unwrap();
    for (i, p) in PRESETS.iter().enumerate() {
        let b = ui.build(
            button(p.name)
                .name(format!("preset_{}", p.name.to_ascii_lowercase()))
                .size(11.0)
                .on_click(move |s: &mut Amp, ui: &mut Ui<Amp>| apply_preset(s, ui, i)),
        );
        ui.attach(presets, b).unwrap();
    }
    let band = |ui: &mut Ui<Amp>, name: String, text: &str, idx: Option<usize>| {
        let sl = ui.build(
            slider(0.0)
                .name(name)
                .range(-EQ_RANGE_DB, EQ_RANGE_DB)
                .step(0.5)
                .vertical()
                .height(90.0)
                .on_change(move |s: &mut Amp, _ui: &mut Ui<Amp>, v: f32| {
                    match idx {
                        Some(i) => s.eq.bands[i] = v,
                        None => s.eq.preamp = v,
                    }
                    set_eq(s);
                }),
        );
        let col = ui.build(column().gap(3.0).cross_align(CrossAlign::Center).grow(1.0));
        ui.attach(col, sl).unwrap();
        let l = ui.build(label(text).size(10.0).color_role(ColorRole::TextDim));
        ui.attach(col, l).unwrap();
        (col, sl)
    };
    let bands_row = ui.build(row().gap(2.0).width_percent(1.0));
    let (pre_col, preamp) = band(ui, "preamp".to_owned(), "pre", None);
    ui.attach(bands_row, pre_col).unwrap();
    let gap = ui.build(spacer().width(10.0));
    ui.attach(bands_row, gap).unwrap();
    let mut bands = [preamp; 10];
    for (i, text) in EQ_LABELS.iter().enumerate() {
        let name = format!("eq_{}", text.to_ascii_lowercase());
        let (col, sl) = band(ui, name, text, Some(i));
        ui.attach(bands_row, col).unwrap();
        bands[i] = sl;
    }
    let eq_section = ui.build(
        panel()
            .name("equalizer")
            .background_role(ColorRole::Surface)
            .radius(6.0)
            .padding(10.0)
            .gap(8.0)
            .width_percent(1.0),
    );
    ui.attach(eq_section, presets).unwrap();
    ui.attach(eq_section, bands_row).unwrap();

    // ---- the playlist ----
    let list_w = ui.build(
        list::<Amp>()
            .name("playlist")
            .height(LIST_HEIGHT)
            .grow(1.0)
            .width_percent(1.0)
            .on_activate(|s: &mut Amp, ui: &mut Ui<Amp>, i: usize| {
                s.failures = 0;
                load(s, ui, i, true);
            }),
    );
    let path = ui.build(
        text_field("")
            .name("path")
            .placeholder("file, folder, playlist or URL")
            .grow(1.0)
            .on_submit(|s: &mut Amp, ui: &mut Ui<Amp>, t: &str| {
                let t = t.to_owned();
                add_from_field(s, ui, &t);
            }),
    );
    let add = ui.build(button("").name("add").icon("plus-lg").on_click(
        |s: &mut Amp, ui: &mut Ui<Amp>| {
            let Some(ids) = s.ids else { return };
            let text = ui
                .widget::<TextField<Amp>>(ids.path)
                .map(|f| f.text().to_owned())
                .unwrap_or_default();
            add_from_field(s, ui, &text);
        },
    ));
    let add_files = ui.build(
        button("")
            .name("add_files")
            .icon("file-earmark-music")
            .on_click(|s: &mut Amp, ui: &mut Ui<Amp>| open_picker(s, ui, false, false)),
    );
    let add_folder = ui.build(
        button("")
            .name("add_folder")
            .icon("folder2-open")
            .on_click(|s: &mut Amp, ui: &mut Ui<Amp>| open_picker(s, ui, true, false)),
    );
    let remove = ui.build(
        button("")
            .name("remove")
            .icon("dash")
            .on_click(|s: &mut Amp, ui: &mut Ui<Amp>| remove_selected(s, ui)),
    );
    let clear = ui.build(
        button("")
            .name("clear")
            .icon("trash3")
            .on_click(|s: &mut Amp, ui: &mut Ui<Amp>| clear_list(s, ui)),
    );
    let total = ui.build(
        label("0 tracks")
            .name("total")
            .size(11.0)
            .color_role(ColorRole::TextDim),
    );
    let pl_bar = ui.build(
        row()
            .gap(4.0)
            .cross_align(CrossAlign::Center)
            .width_percent(1.0),
    );
    for c in [path, add, add_files, add_folder, remove, clear, total] {
        ui.attach(pl_bar, c).unwrap();
    }
    let pl_section = ui.build(
        panel()
            .name("playlist_editor")
            .background_role(ColorRole::Surface)
            .radius(6.0)
            .padding(10.0)
            .gap(8.0)
            .grow(1.0)
            .shrink_to_zero()
            .width_percent(1.0),
    );
    ui.attach(pl_section, list_w).unwrap();
    ui.attach(pl_section, pl_bar).unwrap();

    let root = ui.build(column().gap(8.0).padding(8.0));
    for c in [main, eq_section, pl_section] {
        ui.attach(root, c).unwrap();
    }

    let _ = ui.set_window_limits(Size::new(MIN_WIDTH, 0.0), Size::ZERO);
    let ids = Ids {
        vis: visw,
        clock,
        title,
        info,
        status,
        seek,
        volume,
        balance,
        shuffle,
        repeat,
        eq_toggle,
        pl_toggle,
        eq_section,
        eq_on,
        preamp,
        bands,
        pl_section,
        list: list_w,
        path,
        total,
    };
    ids_install(ui, ids);
    root
}

/// Hand the ids to the state on the first callback: `build` runs before
/// the state is reachable, so they ride a deferred call, which runs
/// before any input can arrive.
fn ids_install(ui: &mut Ui<Amp>, ids: Ids) {
    ui.defer(move |s: &mut Amp, ui: &mut Ui<Amp>| {
        s.ids = Some(ids);
        refresh_list(s, ui);
        if let Some(i) = s.playlist.current() {
            if let Some(e) = s.playlist.get(i) {
                s.marquee.set(&fmt::title_line(i + 1, &e.title, e.duration));
            }
            show_title(s, ui);
        }
        sync_controls(s, ui, ids);
        // The folds the last run ended with, then a window to fit them.
        let shown = s.shown_sections;
        set_checkbox(ui, ids.eq_toggle, shown.eq);
        set_checkbox(ui, ids.pl_toggle, shown.pl);
        ui.set_collapsed(ids.eq_section, !shown.eq);
        ui.set_collapsed(ids.pl_section, !shown.pl);
        fit_window(s, ui);
        // A drag of the window's edge is the user choosing how tall the
        // list is; remember it for the next fold and the next run.
        ui.on_resize(|s: &mut Amp, ui: &mut Ui<Amp>, _size: Size| {
            if let Some(ids) = s.ids {
                remember_list_height(s, ui, ids);
            }
        });
        let st = s.player.status();
        show_clock(s, ui, &st);
        show_info(ui, ids, &st);
        set_status(ui, s, None);
        if s.autoplay {
            s.autoplay = false;
            play(s, ui);
        }
    });
    install_keyboard(ui);
}

/// The keyboard, as app-level handlers: only keys no widget took.
fn install_keyboard(ui: &mut Ui<Amp>) {
    ui.set_shortcut(mods::CTRL, key::Q, |_s: &mut Amp, ui: &mut Ui<Amp>| {
        ui.quit();
    });
    ui.set_shortcut(mods::CTRL, code::T, |s: &mut Amp, ui: &mut Ui<Amp>| {
        toggle_remaining(s, ui);
    });
    ui.set_shortcut(mods::ALT, code::G, |s: &mut Amp, ui: &mut Ui<Amp>| {
        if let Some(ids) = s.ids {
            let show = ui.is_collapsed(ids.eq_section);
            toggle_section(s, ui, true, show);
        }
    });
    ui.set_shortcut(mods::ALT, code::E, |s: &mut Amp, ui: &mut Ui<Amp>| {
        if let Some(ids) = s.ids {
            let show = ui.is_collapsed(ids.pl_section);
            toggle_section(s, ui, false, show);
        }
    });
    ui.set_shortcut(mods::CTRL, code::O, |s: &mut Amp, ui: &mut Ui<Amp>| {
        open_picker(s, ui, false, true);
    });
    ui.set_shortcut(
        mods::CTRL | mods::SHIFT,
        code::O,
        |s: &mut Amp, ui: &mut Ui<Amp>| open_picker(s, ui, true, true),
    );
    ui.set_shortcut(mods::CTRL, key::L, |s: &mut Amp, ui: &mut Ui<Amp>| {
        if keys_are_ours(s, ui) {
            focus_path(s, ui);
        }
    });
    // The bare keys below are the player's only in its own window: the
    // file dialog's leftovers (an arrow its list did not want, Delete)
    // must not seek, change the volume or drop a track.
    ui.on_key(|s: &mut Amp, ui: &mut Ui<Amp>, ev: &KeyEvent| {
        if ev.mods & mods::MASK != mods::NONE || !keys_are_ours(s, ui) {
            return Handled::No;
        }
        match ev.keycode {
            key::DELETE => remove_selected(s, ui),
            key::LEFT => seek_by(s, ui, -SEEK_STEP),
            key::RIGHT => seek_by(s, ui, SEEK_STEP),
            key::UP => {
                let v = s.volume * 100.0 + VOLUME_STEP;
                set_volume(s, ui, v);
            }
            key::DOWN => {
                let v = s.volume * 100.0 - VOLUME_STEP;
                set_volume(s, ui, v);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    });
    ui.on_key(|s: &mut Amp, ui: &mut Ui<Amp>, ev: &KeyEvent| {
        // Only unmodified letters: `Ctrl+C` in a text field is not
        // "pause".
        if ev.mods & (mods::CTRL | mods::ALT) != 0 || !keys_are_ours(s, ui) {
            return Handled::No;
        }
        if ev.keycode == key::L {
            // `l` opens files, `Shift+L` a folder: by the modifier, not
            // the text, which Caps Lock also capitalises.
            let folder = ev.mods & mods::SHIFT != 0;
            open_picker(s, ui, folder, true);
            return Handled::Yes;
        }
        match ev.text.as_str() {
            "z" | "Z" => skip(s, ui, false),
            "x" | "X" => play(s, ui),
            "c" | "C" => pause(s, ui),
            "v" | "V" => stop(s, ui),
            "b" | "B" => skip(s, ui, true),
            "s" | "S" => {
                let on = !s.playlist.shuffle();
                s.playlist.set_shuffle(on);
                if let Some(ids) = s.ids {
                    set_checkbox(ui, ids.shuffle, on);
                }
            }
            "r" | "R" => {
                let on = !s.playlist.repeat();
                s.playlist.set_repeat(on);
                if let Some(ids) = s.ids {
                    set_checkbox(ui, ids.repeat, on);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    });
}

/// Move every control to where the state's settings say.
fn sync_controls(s: &Amp, ui: &mut Ui<Amp>, ids: Ids) {
    let sliders = [
        (ids.volume, s.volume * 100.0),
        (ids.balance, s.balance * 100.0),
        (ids.preamp, s.eq.preamp),
    ];
    for (id, v) in sliders
        .into_iter()
        .chain(ids.bands.iter().copied().zip(s.eq.bands))
    {
        if let Ok(mut sl) = ui.widget_mut::<Slider<Amp>>(id) {
            sl.set_value(v);
        }
    }
    set_checkbox(ui, ids.eq_on, s.eq.enabled);
    set_checkbox(ui, ids.shuffle, s.playlist.shuffle());
    set_checkbox(ui, ids.repeat, s.playlist.repeat());
}

/// Connect, open the window and run until the app quits.
///
/// `paths` are the command line: named files, folders and playlists
/// replace the saved playlist and start playing.
///
/// # Errors
/// Any connection, wire or `epoll` failure, or the audio thread not
/// starting.
pub fn run(paths: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let mut amp = Amp::new(Config::detect())?;
    if !paths.is_empty() {
        amp.playlist.clear();
        amp.add_paths(paths);
        amp.start_on_open();
    }
    App::new(APP_NAME)?.title("nitro-amp").run(amp, build)?;
    Ok(())
}
