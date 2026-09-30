//! The **quick-settings menu**: the status pill at the bar's right end
//! and the drop-down it opens.
//!
//! ```text
//!                                    ╭──────────────────────────────────╮
//!  … 09:41 …          ( 🔊 87% ) ◀── │ 87%                  (⚙)(🔒)(⏻) │  Main
//!                     status pill    │ ╭──────────────────────────────╮ │
//!                                    │ │ Sound                        │ │
//!                                    │ │ (🔊) ━━━━━━━━●─────────  (🎧) │ │
//!                                    │ │ Built-in Audio Analog Stereo │ │
//!                                    │ ╰──────────────────────────────╯ │
//!                                    │ ╭─────────────╮                  │
//!                                    │ │ (◐) Dark    │                  │
//!                                    │ │     Style   │                  │
//!                                    │ ╰─────────────╯                  │
//!                                    ╰──────────────────────────────────╯
//! ```
//!
//! GNOME's structure — one status pill, a panel of round buttons, a card,
//! a two-column tile grid — with macOS's Sound card and round accent
//! badges, built from `nitro_ui::quick`. The **Outputs** and **Power**
//! views are drill-downs: a header with a back arrow replacing the panel's
//! content, at the same width.
//!
//! The menu is a server **popup** with the pointer grab
//! (`docs/wm.md` §Popups): a press outside it or Escape dismisses it and
//! is consumed, which is also why a second click on the pill *closes* the
//! menu rather than re-opening it — the pill never sees that press.
//! Popups are fixed-size, so a view switch opens the new popup and
//! removes the old one in the same turn, one commit.
//!
//! This is a multi-view panel (sliders, tiles, drill-downs), not a list
//! of items, so it does not use `nitro_ui::menu`; a plain item menu
//! behind an icon button should use `nitro_ui::menu::menu_button`.
//!
//! # Idle
//!
//! While the menu is closed this module schedules **nothing**: no timer,
//! no subprocess. The volume is read once at start-up (for the pill's
//! icon), when the menu opens, and after the menu changes something. A
//! volume changed elsewhere (a media key, another mixer) is therefore
//! stale in the pill until the menu next opens — the same trade
//! `nitro-settings` makes, for the same reason.

use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use nitro_system::audio::{Backend, Sink, Volume};
use nitro_system::session::{self, Action};
use nitro_system::{audio, conf};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::quick::{
    CHOICE_H, QS_GAP, QS_PAD, QS_RADIUS, QS_WIDTH, RoundButton, StatusPill, Tile, choice_row,
    drill_header, round_button, section_card, tile,
};
use nitro_ui::widgets::{Icon, column, label, panel, row, scroll, separator, slider, spacer};
use nitro_ui::{ColorRole, CrossAlign, PopupPlacement, Rect, Scheme, Ui, WidgetId, WindowId};

use crate::Bar;

/// The `hey`-addressable names of the pill and of the menu's controls.
/// The menu is a popup, so its widgets are at `window[N]/<name>`.
pub mod names {
    /// The status pill on every panel.
    pub const STATUS: &str = "status";
    /// The pill's volume icon.
    pub const STATUS_VOLUME: &str = "status_volume";
    /// The menu's root.
    pub const MENU: &str = "quick";
    /// The dim status line top-left (the battery reading).
    pub const STATUS_TEXT: &str = "status_text";
    /// The settings button.
    pub const SETTINGS: &str = "settings";
    /// The lock button.
    pub const LOCK: &str = "lock";
    /// The power button, which opens the Power view.
    pub const POWER: &str = "power";
    /// The Sound card.
    pub const SOUND: &str = "sound";
    /// The mute toggle.
    pub const MUTE: &str = "mute";
    /// The volume slider.
    pub const VOLUME: &str = "volume";
    /// The output button, which opens the Outputs view.
    pub const OUTPUT: &str = "output";
    /// The current sink's name under the slider.
    pub const SINK: &str = "sink";
    /// The Dark Style tile.
    pub const DARK: &str = "dark";
    /// The error / notice line at the bottom.
    pub const MESSAGE: &str = "message";
    /// A drill-down's back button (from `nitro_ui::quick::drill_header`).
    pub const BACK: &str = "back";
    /// The Power view's rows.
    pub const SUSPEND: &str = "suspend";
    /// Restart.
    pub const REBOOT: &str = "reboot";
    /// Power off.
    pub const POWEROFF: &str = "poweroff";
    /// Log out.
    pub const LOGOUT: &str = "logout";
    /// The Outputs view's `N`th sink row.
    #[must_use]
    pub fn sink_row(n: usize) -> String {
        format!("sink{n}")
    }
}

/// Which view the menu shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Buttons, the Sound card, the tiles.
    Main,
    /// The output-device list.
    Outputs,
    /// Suspend / restart / power off / log out: the confirm step.
    Power,
}

/// Width of one grid tile: two per row, one gap between them.
const TILE_W: f32 = (QS_WIDTH - 2.0 * QS_PAD - QS_GAP) / 2.0;

/// At most this many sink rows show before the list scrolls, so the
/// Outputs view's height stays bounded.
const MAX_SINK_ROWS: f32 = 6.0;

/// The menu's widget ids that outlive one callback: set when a view is
/// built, used to sync them afterwards.
#[derive(Debug, Clone, Copy, Default)]
struct MenuIds {
    mute: Option<WidgetId>,
    dark: Option<WidgetId>,
}

/// The menu's state, inside [`Bar`].
pub struct Quick {
    /// The open popup, if any.
    popup: Option<WindowId>,
    /// What it shows.
    view: View,
    /// The panel window whose pill opened it.
    parent: WindowId,
    /// The pill's rectangle in that window.
    anchor: Rect,
    /// Where `wpctl`/`pactl` are looked for.
    pub(crate) audio_dirs: Vec<PathBuf>,
    audio: Option<Backend>,
    volume: Option<Volume>,
    sinks: Vec<Sink>,
    /// `session.sock`.
    pub(crate) session_socket: Option<PathBuf>,
    /// `server.conf`, for the Dark Style tile.
    pub(crate) config_path: Option<PathBuf>,
    /// What the settings button runs.
    pub(crate) settings_cmd: Vec<String>,
    /// Settings processes launched, reaped when the menu next opens.
    children: Vec<Child>,
    dark: bool,
    message: Option<String>,
    /// The pill icon last pushed, so an unchanged one costs nothing.
    icon: String,
    ids: MenuIds,
    opens: u64,
}

impl Quick {
    /// The real machine's paths.
    pub(crate) fn new() -> Self {
        Self {
            popup: None,
            view: View::Main,
            parent: WindowId::MAIN,
            anchor: Rect::new(0.0, 0.0, 0.0, 0.0),
            audio_dirs: audio::path_dirs(),
            audio: None,
            volume: None,
            sinks: Vec::new(),
            session_socket: session::default_socket_path(),
            config_path: conf::path(),
            settings_cmd: vec!["nitro-settings".to_owned()],
            children: Vec::new(),
            dark: false,
            message: None,
            icon: String::new(),
            ids: MenuIds::default(),
            opens: 0,
        }
    }

    /// The open popup window.
    #[must_use]
    pub fn popup(&self) -> Option<WindowId> {
        self.popup
    }

    /// The view shown (or last shown).
    #[must_use]
    pub fn view(&self) -> View {
        self.view
    }

    /// The notice line, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The pill's current volume icon.
    #[must_use]
    pub fn icon(&self) -> &str {
        &self.icon
    }

    /// How many times the menu has been opened.
    #[must_use]
    pub fn opens(&self) -> u64 {
        self.opens
    }
}

/// The pill's icon for a reading: the speaker level, crossed out when
/// muted, and the sliders glyph when there is no mixer at all (so the
/// pill still reads as "settings live here").
#[must_use]
pub fn volume_icon(have_mixer: bool, v: Option<Volume>) -> &'static str {
    match (have_mixer, v) {
        (false, _) => "sliders",
        (true, None) => "speaker",
        (true, Some(v)) if v.muted || v.level <= 0.0 => "volume-mute",
        (true, Some(v)) if v.level < 0.5 => "volume-down",
        (true, Some(_)) => "volume-up",
    }
}

/// Start-up: one mixer read, for the pill's icon. Off the idle path — it
/// runs once, from a zero-delay timer, before the first frame.
pub(crate) fn init(s: &mut Bar, ui: &mut Ui<Bar>) {
    let q = &mut s.quick;
    q.audio = Backend::detect_in(&q.audio_dirs);
    q.volume = q.audio.as_ref().and_then(Backend::volume);
    sync(s, ui);
}

/// Push the icon to every panel's pill and the menu's mute button,
/// touching nothing that has not changed.
pub(crate) fn sync(s: &mut Bar, ui: &mut Ui<Bar>) {
    let q = &s.quick;
    let name = volume_icon(q.audio.is_some(), q.volume);
    if s.quick.icon != name {
        name.clone_into(&mut s.quick.icon);
        for p in &s.panels {
            if let Ok(mut i) = ui.widget_mut::<Icon>(p.ids.volume_icon) {
                i.set_icon(name);
            }
        }
    }
    if let Some(id) = s.quick.ids.mute
        && let Ok(mut b) = ui.widget_mut::<RoundButton<Bar>>(id)
    {
        b.set_icon(name);
        let muted = s.quick.volume.is_some_and(|v| v.muted);
        b.set_label(if muted { "Unmute" } else { "Mute" });
    }
}

/// A click on a pill: open the menu under it, or close it if it is open
/// (only reachable from `hey`: a real press outside the popup is the
/// server's to consume, and dismisses it).
pub(crate) fn toggle(s: &mut Bar, ui: &mut Ui<Bar>, pill: WidgetId) {
    if s.quick.popup.is_some() {
        close(s, ui);
        return;
    }
    let Some(parent) = ui.window_of(pill) else {
        return;
    };
    let r = ui.window_bounds(pill);
    s.quick.parent = parent;
    // A few pixels of air between the bar and the panel.
    s.quick.anchor = Rect::new(r.x, r.y, r.w, r.h + 4.0);
    s.quick.message = None;
    s.quick.opens += 1;
    refresh(&mut s.quick);
    sync(s, ui);
    show(s, ui, View::Main);
}

/// Re-read everything the menu shows: the menu opening is the one moment
/// the bar asks, because it never polls.
fn refresh(q: &mut Quick) {
    q.children
        .retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
    q.audio = Backend::detect_in(&q.audio_dirs);
    q.volume = q.audio.as_ref().and_then(Backend::volume);
    q.sinks = q
        .audio
        .as_ref()
        .and_then(Backend::sinks)
        .unwrap_or_default();
    q.dark = q
        .config_path
        .as_deref()
        .is_some_and(|p| conf::load(p).theme.scheme == Some(Scheme::Dark));
}

/// Close the menu, if it is open.
pub(crate) fn close(s: &mut Bar, ui: &mut Ui<Bar>) {
    if let Some(p) = s.quick.popup.take() {
        s.quick.view = View::Main;
        s.quick.ids = MenuIds::default();
        let _ = ui.remove_window(s, p);
    }
}

/// Show `view`: a fresh popup with the view's tree, and the old one (if
/// any) removed in the same turn — popups are fixed-size, and the swap
/// is one commit.
fn show(s: &mut Bar, ui: &mut Ui<Bar>, view: View) {
    s.quick.ids = MenuIds::default();
    let root = match view {
        View::Main => build_main(s, ui),
        View::Outputs => build_outputs(s, ui),
        View::Power => build_power(s, ui),
    };
    let placement = PopupPlacement::below(s.quick.anchor);
    match ui.add_popup(s.quick.parent, placement, None, root) {
        Ok(win) => {
            let old = s.quick.popup.replace(win);
            s.quick.view = view;
            ui.on_window_closed(win, move |s: &mut Bar, _ui: &mut Ui<Bar>| {
                // Only the live one: the popup a view switch replaced
                // closes too, and must not reset the menu's state.
                if s.quick.popup == Some(win) {
                    s.quick.popup = None;
                    s.quick.view = View::Main;
                    s.quick.ids = MenuIds::default();
                }
            });
            // Tab / Enter / Space inside the menu. Escape is the server's.
            if ui.is_shell() {
                let _ = ui.grab_keyboard_of(win, true);
            }
            if let Some(old) = old {
                let _ = ui.remove_window(s, old);
            }
        }
        Err(e) => {
            eprintln!("nitro-bar: quick settings: {e}");
            let _ = ui.remove(root);
        }
    }
}

/// Switch view from inside a callback of the current popup, whose widget
/// is out of the tree while it runs: deferred to the end of the turn,
/// still before the flush, so still one commit.
fn switch(ui: &mut Ui<Bar>, view: View) {
    ui.defer(move |s: &mut Bar, ui: &mut Ui<Bar>| show(s, ui, view));
}

/// [`close`], deferred for the same reason as [`switch`].
fn close_later(ui: &mut Ui<Bar>) {
    ui.defer(close);
}

/// The menu's rounded panel, fixed width.
fn menu_panel(ui: &mut Ui<Bar>) -> WidgetId {
    ui.build(
        panel()
            .name(names::MENU)
            .background_role(ColorRole::WindowBackground)
            .border_role(1.0, ColorRole::Hairline)
            .radius(QS_RADIUS)
            .padding(QS_PAD)
            .gap(QS_GAP)
            .width(QS_WIDTH)
            .cross_align(CrossAlign::Stretch),
    )
}

fn attach(ui: &mut Ui<Bar>, parent: WidgetId, child: WidgetId) {
    if ui.attach(parent, child).is_err() {
        let _ = ui.remove(child);
    }
}

/// The notice line, when there is one.
fn message_line(s: &Bar, ui: &mut Ui<Bar>, root: WidgetId) {
    if let Some(m) = &s.quick.message {
        let l = ui.build(
            label(m.clone())
                .name(names::MESSAGE)
                .size(12.0)
                .color_role(ColorRole::TextDim)
                .elide(true),
        );
        attach(ui, root, l);
    }
}

fn build_main(s: &mut Bar, ui: &mut Ui<Bar>) -> WidgetId {
    let root = menu_panel(ui);

    // -- top row: status text, then settings / lock / power -------------
    let lock_ok = Action::Lock.available();
    let lock = round_button("lock").name(names::LOCK).label(if lock_ok {
        "Lock"
    } else {
        "Lock (not available)"
    });
    let lock = if lock_ok {
        lock.on_click(|s: &mut Bar, ui: &mut Ui<Bar>| run_session(s, ui, Action::Lock))
    } else {
        lock.disabled()
    };
    let top = ui.build(
        row()
            .gap(8.0)
            .cross_align(CrossAlign::Center)
            .child(
                label(s.last.battery.clone().unwrap_or_default())
                    .name(names::STATUS_TEXT)
                    .size(13.0)
                    .color_role(ColorRole::TextDim)
                    .grow(1.0)
                    .shrink_to_zero(),
            )
            .child(
                round_button("gear")
                    .name(names::SETTINGS)
                    .label("Settings")
                    .on_click(|s: &mut Bar, ui: &mut Ui<Bar>| {
                        launch_settings(s);
                        close_later(ui);
                    }),
            )
            .child(lock)
            .child(
                round_button("power")
                    .name(names::POWER)
                    .label("Power Off / Log Out")
                    .on_click(|_s: &mut Bar, ui: &mut Ui<Bar>| switch(ui, View::Power)),
            ),
    );
    attach(ui, root, top);

    let card = build_sound(s, ui);
    attach(ui, root, card);

    // -- the tile grid --------------------------------------------------
    // One entry per tile: Wi-Fi, VPN and Bluetooth are one line each here
    // when their backends exist.
    let dark = s.quick.dark;
    let dark_tile = ui.build(
        tile("Dark Style", "circle-half")
            .name(names::DARK)
            .on(dark)
            .subtitle(if dark { "On" } else { "Off" })
            .width(TILE_W)
            .on_toggle(set_dark),
    );
    s.quick.ids.dark = Some(dark_tile);
    grid(ui, root, &[dark_tile]);

    message_line(s, ui, root);
    root
}

/// The Sound card: mute, the chunky volume slider, the output button,
/// and the current sink's name (or why there is no mixer).
fn build_sound(s: &mut Bar, ui: &mut Ui<Bar>) -> WidgetId {
    let card = ui.build(section_card("Sound").name(names::SOUND));
    let have = s.quick.audio.is_some();
    let vol = s.quick.volume;
    let icon = volume_icon(have, vol);
    let mut mute = round_button(icon)
        .name(names::MUTE)
        .label(if vol.is_some_and(|v| v.muted) {
            "Unmute"
        } else {
            "Mute"
        })
        .on_click(toggle_mute);
    let mut volume = slider(vol.map_or(0.0, |v| v.level))
        .chunky()
        .name(names::VOLUME)
        .grow(1.0)
        .shrink_to_zero()
        .on_change(set_volume);
    let mut output = round_button("headphones")
        .name(names::OUTPUT)
        .label("Sound Output")
        .on_click(|_s: &mut Bar, ui: &mut Ui<Bar>| switch(ui, View::Outputs));
    if vol.is_none() {
        mute = mute.disabled();
        volume = volume.disabled();
    }
    if !have {
        output = output.disabled();
    }
    let mute = ui.build(mute);
    s.quick.ids.mute = Some(mute);
    let volume = ui.build(volume);
    let output = ui.build(output);
    let volume_row = ui.build(row().gap(8.0).cross_align(CrossAlign::Center));
    for c in [mute, volume, output] {
        attach(ui, volume_row, c);
    }
    attach(ui, card, volume_row);
    let note = if have {
        s.quick
            .sinks
            .iter()
            .find(|k| k.default)
            .map_or_else(String::new, |k| k.name.clone())
    } else {
        "No wpctl or pactl found — install PipeWire or PulseAudio".to_owned()
    };
    if !note.is_empty() {
        let l = ui.build(
            label(note)
                .name(names::SINK)
                .size(12.0)
                .color_role(ColorRole::TextDim)
                .elide(true),
        );
        attach(ui, card, l);
    }
    card
}

/// Lay `tiles` out two to a row; an odd count leaves an empty half.
fn grid(ui: &mut Ui<Bar>, root: WidgetId, tiles: &[WidgetId]) {
    for pair in tiles.chunks(2) {
        let r = ui.build(row().gap(QS_GAP));
        for t in pair {
            attach(ui, r, *t);
        }
        if pair.len() == 1 {
            let gap = ui.build(spacer().width(TILE_W));
            attach(ui, r, gap);
        }
        attach(ui, root, r);
    }
}

fn build_outputs(s: &mut Bar, ui: &mut Ui<Bar>) -> WidgetId {
    let root = menu_panel(ui);
    let header = ui.build(drill_header(
        "Sound Output",
        |_s: &mut Bar, ui: &mut Ui<Bar>| switch(ui, View::Main),
    ));
    attach(ui, root, header);
    if s.quick.sinks.is_empty() {
        let l = ui.build(
            label("No output devices")
                .size(13.0)
                .color_role(ColorRole::TextDim),
        );
        attach(ui, root, l);
    } else {
        let list = ui.build(column().gap(2.0).width_percent(1.0));
        for (i, sink) in s.quick.sinks.iter().enumerate() {
            let target = sink.clone();
            let r = ui.build(
                choice_row(sink.name.clone(), sink.default)
                    .name(names::sink_row(i))
                    .on_click(move |s: &mut Bar, ui: &mut Ui<Bar>| pick_sink(s, ui, &target)),
            );
            attach(ui, list, r);
        }
        let rows = s.quick.sinks.len() as f32;
        let h = rows.min(MAX_SINK_ROWS) * CHOICE_H + (rows.min(MAX_SINK_ROWS) - 1.0) * 2.0;
        let sc = ui.build(scroll().height(h).width_percent(1.0));
        attach(ui, sc, list);
        attach(ui, root, sc);
    }
    message_line(s, ui, root);
    root
}

fn build_power(s: &mut Bar, ui: &mut Ui<Bar>) -> WidgetId {
    let root = menu_panel(ui);
    let header = ui.build(drill_header(
        "Power Off",
        |_s: &mut Bar, ui: &mut Ui<Bar>| switch(ui, View::Main),
    ));
    attach(ui, root, header);
    let list = ui.build(column().gap(2.0).width_percent(1.0));
    let rows = [
        (Action::Suspend, "Suspend", "moon", names::SUSPEND),
        (
            Action::Reboot,
            "Restart…",
            "bootstrap-reboot",
            names::REBOOT,
        ),
        (Action::Poweroff, "Power Off…", "power", names::POWEROFF),
    ];
    for (action, text, icon, name) in rows {
        let r = ui.build(
            choice_row(text, false)
                .icon(icon)
                .name(name)
                .on_click(move |s: &mut Bar, ui: &mut Ui<Bar>| run_session(s, ui, action)),
        );
        attach(ui, list, r);
    }
    let sep = ui.build(separator().color_role(ColorRole::Hairline));
    attach(ui, list, sep);
    let out = ui.build(
        choice_row("Log Out…", false)
            .icon("box-arrow-right")
            .name(names::LOGOUT)
            .on_click(|s: &mut Bar, ui: &mut Ui<Bar>| run_session(s, ui, Action::Logout)),
    );
    attach(ui, list, out);
    attach(ui, root, list);
    message_line(s, ui, root);
    root
}

// -- actions ------------------------------------------------------------

fn set_volume(s: &mut Bar, ui: &mut Ui<Bar>, v: f32) {
    let Some(b) = s.quick.audio.clone() else {
        return;
    };
    match b.set_volume(v) {
        Ok(()) => {
            if let Some(vol) = &mut s.quick.volume {
                vol.level = v;
            }
        }
        Err(e) => s.quick.message = Some(e),
    }
    ui.defer(sync);
}

fn toggle_mute(s: &mut Bar, ui: &mut Ui<Bar>) {
    let (Some(b), Some(v)) = (s.quick.audio.clone(), s.quick.volume) else {
        return;
    };
    match b.set_muted(!v.muted) {
        Ok(()) => {
            if let Some(vol) = &mut s.quick.volume {
                vol.muted = !v.muted;
            }
        }
        Err(e) => {
            s.quick.message = Some(e);
            switch(ui, s.quick.view);
        }
    }
    ui.defer(sync);
}

fn pick_sink(s: &mut Bar, ui: &mut Ui<Bar>, sink: &Sink) {
    let Some(b) = s.quick.audio.clone() else {
        return;
    };
    match b.set_default_sink(sink) {
        Ok(()) => {
            s.quick.message = None;
            refresh(&mut s.quick);
            ui.defer(sync);
            switch(ui, View::Main);
        }
        Err(e) => {
            s.quick.message = Some(e);
            switch(ui, View::Outputs);
        }
    }
}

fn set_dark(s: &mut Bar, ui: &mut Ui<Bar>, on: bool) {
    let Some(path) = s.quick.config_path.clone() else {
        s.quick.message = Some("no config path ($XDG_CONFIG_HOME or $HOME)".to_owned());
        switch(ui, View::Main);
        return;
    };
    let scheme = if on { Scheme::Dark } else { Scheme::Light };
    // One key rewritten, everything else kept; the server's inotify
    // watch reloads it and pushes the new palette to every client, this
    // menu included.
    match conf::set_scheme(&path, scheme) {
        Ok(()) => {
            s.quick.dark = on;
            ui.defer(move |s: &mut Bar, ui: &mut Ui<Bar>| {
                if let Some(id) = s.quick.ids.dark
                    && let Ok(mut t) = ui.widget_mut::<Tile<Bar>>(id)
                {
                    t.set_subtitle(if on { "On" } else { "Off" });
                }
            });
        }
        Err(e) => {
            s.quick.message = Some(format!("{}: {e}", path.display()));
            switch(ui, View::Main);
        }
    }
}

/// Super+L: ask the session to lock, the same request as the Lock
/// button, without opening or touching the menu. A failure is printed:
/// there is no menu open to show it in.
pub(crate) fn lock_from_hotkey(s: &mut Bar) -> Result<(), String> {
    match &s.quick.session_socket {
        Some(p) => session::request(p, Action::Lock),
        None => Err("no session socket: is nitro-session running?".to_owned()),
    }
}

fn run_session(s: &mut Bar, ui: &mut Ui<Bar>, action: Action) {
    let result = match &s.quick.session_socket {
        Some(p) => session::request(p, action),
        None => Err("no session socket: is nitro-session running?".to_owned()),
    };
    match result {
        Ok(()) => {
            s.quick.message = None;
            close_later(ui);
        }
        Err(e) => {
            s.quick.message = Some(format!("{}: {e}", action.verb()));
            switch(ui, s.quick.view);
        }
    }
}

/// Start `nitro-settings`, detached into its own process group so it
/// outlives a bar restart; reaped when the menu next opens.
fn launch_settings(s: &mut Bar) {
    let Some((cmd, args)) = s.quick.settings_cmd.split_first() else {
        return;
    };
    match Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(c) => s.quick.children.push(c),
        Err(e) => eprintln!("nitro-bar: {cmd}: {e}"),
    }
}

/// Wire a freshly built pill's click to the menu.
pub(crate) fn hook_pill(ui: &mut Ui<Bar>, pill: WidgetId) {
    if let Ok(mut p) = ui.widget_mut::<StatusPill<Bar>>(pill) {
        p.set_on_click(move |s: &mut Bar, ui: &mut Ui<Bar>| toggle(s, ui, pill));
    }
}
