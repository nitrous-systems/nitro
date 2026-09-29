//! `nitro-bar` — the desktop's top bar, and the first program written
//! against the **shell socket**.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ ≣  Calculator │ hello-dialog     09:41      87%+ ⚙0.4 ▤1.2/3.3G │
//! └──────────────────────────────────────────────────────────────────┘
//!    launcher  ── windows ──        clock      battery load  mem
//! ```
//!
//! It is a `nitro-ui` app like any other — a state struct, a tree built
//! once, a callback per button — with exactly one difference: it connects
//! to `shell.sock` rather than `wire.sock`, which is what lets it live on
//! the `Top` layer, span the top edge of its output and reserve 32 px of
//! screen space the rest of the desktop may not use. The socket *is* the
//! capability; see `docs/shell.md`.
//!
//! Every section carries a `.name()`, so the whole bar is drivable from a
//! shell with no cooperation from this code:
//!
//! ```text
//! hey nitro-bar list
//! hey nitro-bar do windows/win3 click        # focus it, restore it, or put it away
//! hey nitro-bar get clock value              # 09:41
//! ```
//!
//! # Idle costs nothing, and that is the headline
//!
//! With nothing changing, the bar puts **zero bytes on the wire between
//! clock ticks**, and the clock ticks once a minute. Three mechanisms,
//! each of which had to be right:
//!
//! * the window list is **subscribed, never polled** — `WindowList`
//!   answers once and then the server sends a `WindowInfo` when something
//!   actually changes;
//! * the clock's timer is **aligned to the minute boundary**, so it fires
//!   at :00 rather than every second to check whether the minute rolled
//!   over ([`clock::ms_to_next_minute`]);
//! * the sensors poll every 30 s and write to the tree **only when the
//!   formatted string differs** — the poll's [`Readings`] are compared
//!   against the last ones before the tree is touched at all, so an
//!   unchanged reading costs no mutation, no commit and no wakeup beyond
//!   the timer itself.
//!
//! The sensors' half of that is a rule, not an accident: **a sensor may
//! only repaint when its *rendered* string changes, and no sensor renders
//! more often than every 30 s**. Both halves are needed. The load average
//! moves on almost every read — 0.17, 0.23, 0.21 — so rendering it
//! verbatim at 5 s flipped the display six times per ten idle seconds on
//! a box whose CPU was doing nothing; one decimal ([`sensors::format_load`])
//! turns most of that motion back into the same string, and 30 s caps
//! what is left at two repaints a minute.
//!
//! `a_settled_bar_is_silent_while_nothing_changes` in `tests/bar.rs`
//! asserts it from the outside, by counting commits over a window in
//! which the sensors are polled several times.
//!
//! # One bar per output
//!
//! One process, **one shell connection**, one [`Ui`] — and one *panel*
//! (a window with its own copy of the tree) per connected output. The
//! main window is the panel on whichever output the server placed it;
//! every other output gets a panel opened with
//! [`Ui::add_surface_window`] and an anchor that names it
//! ([`Anchor::on`]), which is what `SetAnchor { output }` is for: the
//! server moves the window there before anchoring it, and the zone comes
//! off *that* output's work area.
//!
//! The set of panels follows hotplug by one rule, [`reconcile`]: **the
//! extra panels' outputs are exactly the last `Outputs` snapshot minus
//! the main window's output**. It is re-run at every `OutputsEnd` and
//! whenever the server re-places the main window
//! ([`Ui::on_window_placed`]), and it is idempotent, so the order those
//! two arrive in does not matter. An unplug is handled by the server
//! first — it migrates the orphaned panel to the primary output — and
//! by the rule second, which closes it because its output is gone; the
//! two bars on one screen that leaves last one commit.
//!
//! Every panel shows the same thing. The window list, the clock and the
//! sensors write to *every* panel's labels, and the idle contract above
//! holds per label: an unchanged string costs nothing, on one panel or
//! on four. `hey nitro-bar` reaches the main panel at `window/...` and
//! the others at `window[N]/...`.

pub mod clock;
pub mod quick;
pub mod sensors;

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::shell::{Anchor, Layer, ShellEvent, Surface, WindowInfo, WindowRef, WindowState};
use nitro_ui::widgets::{Button, Label, button as button_widget, icon, label, row, spacer};
use nitro_ui::{App, ColorRole, Error, IconTint, Size, Ui, WidgetId, WindowId, status_pill};

/// The name the bar registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-bar";

/// Logical height of the bar, and the space it reserves. Overridden by
/// `NITRO_BAR_HEIGHT`.
pub const DEFAULT_HEIGHT: f32 = 32.0;

/// How often the battery, load and memory readouts are re-read, and so
/// the fastest any of them can repaint.
///
/// Thirty seconds, not the spec's five. Reading the files is free; what
/// is not free is what a *changed* reading costs — a `SetText`, a commit
/// and a server-side repaint of that label's region. The load average
/// changes on nearly every read, so a 5 s poll repainted the bar six
/// times per ten seconds on an idle desktop: motion the user did not ask
/// for, in the widget least worth watching, and the thing that would
/// defeat panel self-refresh later.
///
/// Thirty seconds also loses nothing real: the kernel's own 1-minute
/// smoothing means a load average read twice a minute is as fresh as the
/// number can be, and memory in tenths of a GiB does not move faster than
/// that either.
pub const POLL_MS: u64 = 30_000;

/// Font size of the bar's text. Smaller than the toolkit default, because
/// a 32 px strip has to fit a line of text with air around it.
const TEXT_SIZE: f32 = 13.0;

/// The side of the bar's icons, in logical pixels.
///
/// 16 rather than `TEXT_SIZE`: the artwork is drawn on a 16-unit grid, so
/// a 16 px box puts every stroke on a whole pixel at scale 1 and on a
/// whole pair at scale 2. A 13 px icon would be legible and slightly
/// soft, for no gain — the bar is 32 px tall and has the room.
const ICON_PX: f32 = 16.0;

/// Gap between the sections, and between the buttons inside them.
const GAP: f32 = 6.0;

/// Horizontal padding at each end of the bar.
const PAD: f32 = 8.0;

/// Widest a window-list button may get before its label is elided.
///
/// A title list is only useful if the titles are readable, and a browser
/// window whose title is a whole headline would otherwise push every
/// other entry off the bar.
const MAX_BUTTON_W: f32 = 180.0;

/// Longest window label, in characters, before it is elided with `…`.
const MAX_LABEL_CHARS: usize = 22;

/// The `hey`-addressable names of the bar's six sections.
pub mod names {
    /// The launcher button.
    pub const LAUNCHER: &str = "launcher";
    /// The window list's container.
    pub const WINDOWS: &str = "windows";
    /// The clock.
    pub const CLOCK: &str = "clock";
    /// The battery readout, inside the status pill.
    pub const BATTERY: &str = "battery";
    /// The status pill that opens the quick-settings menu.
    pub const STATUS: &str = crate::quick::names::STATUS;
    /// The CPU load readout.
    pub const LOAD: &str = "load";
    /// The icon in front of the load readout.
    pub const LOAD_ICON: &str = "load_icon";
    /// The memory readout.
    pub const MEM: &str = "mem";
    /// The icon in front of the memory readout.
    pub const MEM_ICON: &str = "mem_icon";
}

/// The icons the bar names, in one place so a rename is one edit and a
/// test can assert on the same constants the tree is built from.
pub mod icons {
    /// The launcher button's hamburger.
    pub const LAUNCHER: &str = "list";
    /// In front of the load average.
    pub const LOAD: &str = "cpu";
    /// In front of the memory readout.
    pub const MEM: &str = "memory";
    /// The window-list button of an application whose icon the box does
    /// not have.
    ///
    /// From the **symbolic** set, which is the point: the window list's
    /// icons are named by app id and looked up in the machine's icon
    /// theme, so the fallback has to come from the set that is compiled
    /// into the server and therefore cannot be missing. On a box with no
    /// icon theme at all — the test box — every button shows this, which
    /// is a task list that still reads rather than a row of gaps.
    pub const WINDOW: &str = "window";
}

/// One entry in the window list: the server's id, what it currently says,
/// and the button showing it.
#[derive(Debug, Clone)]
struct Entry {
    /// Server-global window id. Never reused, so a stale one names
    /// nothing rather than somebody else's window.
    window: WindowRef,
    /// The label last pushed into the button, so an unchanged
    /// `WindowInfo` costs nothing.
    text: String,
    /// The icon name last pushed into the button — the window's app id.
    ///
    /// Held for the same reason `text` is, and it matters more: the bar's
    /// idle contract is "no `SetIcon` after the first paint for an
    /// unchanged list", so something has to know what was already sent.
    icon: String,
    /// Whether it is drawn as focused.
    focused: bool,
    /// Whether the window is minimized, which both dims the row and
    /// decides what a click on it does.
    ///
    /// Held here rather than asked of the server per click because the
    /// bar already has it — every `WindowInfo` carries the state — and
    /// because the alternative would be a round trip on the click path to
    /// learn something the last event already said.
    minimized: bool,
    /// The button widget.
    id: WidgetId,
}

/// One panel: a bar window on one output, with its own copy of the tree.
///
/// Index 0 of [`Bar::panels`] is always the main window; the rest were
/// opened by [`open_panel`] for the other outputs.
struct Panel {
    /// The output the window is on, from its `Configure`. `None` for the
    /// main panel until the server has placed it; always known for the
    /// others, because they are opened *for* an output.
    output: Option<u32>,
    /// The window.
    win: WindowId,
    /// This panel's widget ids.
    ids: Ids,
    /// This panel's window-list buttons, in the order the server reports
    /// the windows (by window identity, so the list does not reshuffle
    /// when a window is raised).
    entries: Vec<Entry>,
}

/// The three readouts, as one poll produced them.
///
/// `None` is "nothing to show" — an empty widget rather than a zero that
/// is not true. A desktop has no battery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readings {
    /// Battery percentage and charge state, e.g. `87%+`.
    pub battery: Option<String>,
    /// The 1-minute load average.
    pub load: Option<String>,
    /// Memory used/total.
    pub mem: Option<String>,
}

/// Read all three sensors: what the bar does outside the tests.
#[must_use]
pub fn read_sensors() -> Readings {
    Readings {
        battery: sensors::battery(),
        load: sensors::load(),
        mem: sensors::memory(),
    }
}

/// Where a bar's readouts come from.
///
/// Injectable for one reason, and it is the idle test: the bar's contract
/// is "a poll that finds the same numbers costs nothing on the wire", and
/// that cannot be checked against the real `/proc`, whose load average
/// and free memory genuinely move while the test runs. A test that polled
/// the machine would be asserting that the machine was quiet, which is
/// not the claim and is not reliably true.
type SensorSource = Box<dyn Fn() -> Readings>;

/// The bar's state.
///
/// This is the `S` of `Ui<S>`: a plain struct handed to every callback as
/// `&mut S` alongside `&mut Ui<S>`. No `Rc`, no `RefCell`, no observer
/// list — a callback that has both of those does not need one.
pub struct Bar {
    /// The panels, main window first. See [`Panel`] and [`reconcile`].
    panels: Vec<Panel>,
    /// The last `WindowInfo` for every window the list shows: the source
    /// of truth a new panel's buttons are built from, so a panel opened
    /// after the snapshot lists the same windows as the first one.
    infos: Vec<WindowInfo>,
    /// The output ids of the last complete `Outputs` snapshot.
    outputs: Vec<u32>,
    /// The snapshot being received: ids accumulate here between the
    /// `Output` events and the `OutputsEnd` that makes them `outputs`.
    pending_outputs: Vec<u32>,
    /// The local time zone, read once at start-up.
    ///
    /// Re-reading `/etc/localtime` every minute would be three syscalls a
    /// minute to notice a change that happens twice a year; a session
    /// that changes time zone can restart the bar.
    zone: clock::Zone,
    /// Wall-clock override for the tests, in milliseconds since the
    /// epoch: `NITRO_BAR_FAKE_TIME`. See [`Bar::now_ms`].
    fake_time_ms: Option<i64>,
    /// What the clock label last showed, so a minute that produced the
    /// same string sends nothing.
    clock_text: String,
    /// Counts every wall-clock tick the bar has applied, for the tests
    /// and for `hey nitro-bar get window value`.
    ticks: u64,
    /// How many times the launcher button has been pressed.
    launcher_presses: u64,
    /// How many sensor polls have run, for the idle test.
    polls: u64,
    /// How long between sensor polls; [`POLL_MS`] outside the tests.
    poll_ms: u64,
    /// Where the readouts come from; the real `/proc` and `/sys` outside
    /// the tests. See [`SensorSource`].
    source: SensorSource,
    /// The last readings pushed into the three labels. Load-bearing:
    /// [`poll_sensors`] compares against it and leaves the tree alone
    /// when nothing moved, so "an unchanged poll costs nothing" is
    /// visible here rather than resting on a setter in another crate.
    last: Readings,
    /// The quick-settings menu. See [`quick`].
    quick: quick::Quick,
}

impl Bar {
    /// A bar with no widgets yet; [`build`] fills in the ids.
    #[must_use]
    pub fn new() -> Self {
        Self {
            panels: Vec::new(),
            infos: Vec::new(),
            outputs: Vec::new(),
            pending_outputs: Vec::new(),
            zone: clock::Zone::local(),
            fake_time_ms: fake_time_ms(),
            clock_text: String::new(),
            ticks: 0,
            launcher_presses: 0,
            polls: 0,
            poll_ms: POLL_MS,
            source: Box::new(read_sensors),
            last: Readings::default(),
            quick: quick::Quick::new(),
        }
    }

    /// Look for `wpctl`/`pactl` in `dirs` instead of on `PATH`.
    #[must_use]
    pub fn with_audio_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.quick.audio_dirs = dirs;
        self
    }

    /// Talk to this `session.sock` instead of `$XDG_RUNTIME_DIR`'s.
    #[must_use]
    pub fn with_session_socket(mut self, path: std::path::PathBuf) -> Self {
        self.quick.session_socket = Some(path);
        self
    }

    /// Read and write this `server.conf` for the Dark Style tile.
    #[must_use]
    pub fn with_config_path(mut self, path: std::path::PathBuf) -> Self {
        self.quick.config_path = Some(path);
        self
    }

    /// What the settings button runs (default `nitro-settings`).
    #[must_use]
    pub fn with_settings_command(mut self, cmd: Vec<String>) -> Self {
        self.quick.settings_cmd = cmd;
        self
    }

    /// The quick-settings menu's state.
    #[must_use]
    pub fn quick(&self) -> &quick::Quick {
        &self.quick
    }

    /// Take the readouts from `source` instead of from `/proc` and
    /// `/sys`. See [`SensorSource`] for why this exists.
    #[must_use]
    pub fn with_sensors(mut self, source: impl Fn() -> Readings + 'static) -> Self {
        self.source = Box::new(source);
        self
    }

    /// Poll the sensors every `ms` instead of every [`POLL_MS`].
    ///
    /// For the tests: the idle test has to watch several polls happen and
    /// still find the wire silent, and it cannot spend five seconds per
    /// poll doing it. Consumed **before** the tree is built, because the
    /// first poll is armed as the tree is built and a knob turned
    /// afterwards would not be read until the poll after next.
    #[must_use]
    pub fn with_poll_ms(mut self, ms: u64) -> Self {
        self.poll_ms = ms;
        self
    }

    /// Pin the wall clock to `ms` since the epoch.
    ///
    /// For the tests, and the same rule as [`Bar::with_poll_ms`]: the
    /// clock's first timer is armed from this, so it has to be in place
    /// before the tree is built.
    #[must_use]
    pub fn with_fake_time_ms(mut self, ms: i64) -> Self {
        self.fake_time_ms = Some(ms);
        self
    }

    /// The main panel's entries; empty before the tree exists.
    fn entries(&self) -> &[Entry] {
        self.panels.first().map_or(&[], |p| p.entries.as_slice())
    }

    /// The listed windows, in order. For the tests.
    #[must_use]
    pub fn windows(&self) -> Vec<WindowRef> {
        self.entries().iter().map(|e| e.window).collect()
    }

    /// How many panels are open, the main window included.
    #[must_use]
    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    /// The output of every panel, main window first; 0 for a panel the
    /// server has not placed yet. For the tests.
    #[must_use]
    pub fn panel_outputs(&self) -> Vec<u32> {
        self.panels.iter().map(|p| p.output.unwrap_or(0)).collect()
    }

    /// The output the main window is on, once the server has said.
    #[must_use]
    pub fn main_output(&self) -> Option<u32> {
        self.panels.first().and_then(|p| p.output)
    }

    /// The output ids of the last `Outputs` snapshot. For the tests.
    #[must_use]
    pub fn outputs(&self) -> &[u32] {
        &self.outputs
    }

    /// How many sensor polls have run.
    #[must_use]
    pub fn polls(&self) -> u64 {
        self.polls
    }

    /// How many times the launcher button has been pressed.
    #[must_use]
    pub fn launcher_presses(&self) -> u64 {
        self.launcher_presses
    }

    /// How many windows the list currently holds.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.entries().len()
    }

    /// The labels of the window list, in order. For the tests.
    #[must_use]
    pub fn window_labels(&self) -> Vec<String> {
        self.entries().iter().map(|e| e.text.clone()).collect()
    }

    /// Which window the list draws as focused, if any.
    #[must_use]
    pub fn focused_window(&self) -> Option<WindowRef> {
        self.entries().iter().find(|e| e.focused).map(|e| e.window)
    }

    /// The windows the list draws as minimized. For the tests, and for
    /// the same reason `window_labels` is: the dimming is a *rendered*
    /// property, and a test that only read the label would pass on a bar
    /// that marked the row and forgot to tint it.
    #[must_use]
    pub fn minimized_windows(&self) -> Vec<WindowRef> {
        self.entries()
            .iter()
            .filter(|e| e.minimized)
            .map(|e| e.window)
            .collect()
    }

    /// What the clock currently shows.
    #[must_use]
    pub fn clock_text(&self) -> &str {
        &self.clock_text
    }

    /// How many clock ticks have been applied.
    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    /// The current wall clock in milliseconds since the epoch.
    ///
    /// `NITRO_BAR_FAKE_TIME` overrides it, which is how a test asserts
    /// that the clock updates *exactly once* at a minute boundary without
    /// waiting a minute for it. The override is read once, at start-up:
    /// a test sets it before the bar is built, and a clock whose source
    /// could change under it would not be a clock.
    #[must_use]
    pub fn now_ms(&self) -> i64 {
        self.fake_time_ms.unwrap_or_else(now_ms)
    }

    /// Pin the wall clock, for the tests.
    ///
    /// The clock's timer is armed from the same source, so a test moves
    /// time forward and runs the timers rather than waiting a minute for
    /// a minute to pass.
    pub fn set_fake_time_ms(&mut self, ms: i64) {
        self.fake_time_ms = Some(ms);
    }
}

impl Default for Bar {
    fn default() -> Self {
        Self::new()
    }
}

/// Milliseconds since the epoch, from the system's real-time clock.
fn now_ms() -> i64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Realtime);
    t.tv_sec
        .saturating_mul(1_000)
        .saturating_add(t.tv_nsec / 1_000_000)
}

/// `NITRO_BAR_FAKE_TIME`, in milliseconds since the epoch.
fn fake_time_ms() -> Option<i64> {
    std::env::var("NITRO_BAR_FAKE_TIME")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// One panel's widget ids, gathered by [`build_panel`].
///
/// `build` only has the tree — the state does not exist yet when the
/// callbacks are written, and the callbacks need the ids. So the ids are
/// `Copy` and captured into the closures, and every callback starts with
/// [`ensure_main`], which writes the same set into [`Bar`] as its first
/// [`Panel`] the first time any of them runs. That is the calculator's
/// `Screen` trick, and it is what lets the state be built before the tree
/// it will drive.
#[derive(Debug, Clone, Copy)]
struct Ids {
    windows: WidgetId,
    clock: WidgetId,
    battery: WidgetId,
    load: WidgetId,
    mem: WidgetId,
    volume_icon: WidgetId,
}

/// The bar's height: `NITRO_BAR_HEIGHT`, else [`DEFAULT_HEIGHT`].
///
/// Clamped rather than trusted: a zero or negative height would reserve
/// nothing and draw nothing, and a huge one would reserve the screen —
/// both of which look like the bar has crashed.
#[must_use]
pub fn height() -> f32 {
    std::env::var("NITRO_BAR_HEIGHT")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|h| h.is_finite())
        .map_or(DEFAULT_HEIGHT, |h| h.clamp(8.0, 256.0))
}

/// The icon name a window-list button asks for: the window's **app id**.
///
/// Rule (a) of the two `docs/shell.md` sets out, and the whole of it. An
/// application's desktop file is conventionally named after its app id
/// and its `Icon=` conventionally matches, so the app id is usually the
/// icon's name as well — `firefox` is all three. The bar therefore does
/// no `.desktop` parsing, holds no index and reads no files: it sends one
/// string and the server looks it up in the machine's icon theme.
///
/// It is honestly limited, and the limit is worth stating where a reader
/// will hit it: an application whose app id and icon name differ
/// (`org.gnome.Nautilus` with `Icon=nautilus`) gets the fallback icon and
/// nothing says why. The alternative — the server resolving
/// `app_id → .desktop → Icon=` — is more correct and much bigger; the
/// reasoning is in `docs/shell.md`.
///
/// A window with no app id at all gets [`icons::WINDOW`] directly rather
/// than an empty name, because an empty name *clears* an icon node on the
/// wire and would leave a gap where every other button has a picture.
#[must_use]
pub fn entry_icon(info: &WindowInfo) -> String {
    let id = info.app_id.trim();
    if id.is_empty() {
        return icons::WINDOW.to_owned();
    }
    id.to_owned()
}

/// Shorten a window label to something a bar can show.
///
/// Prefers the title and falls back to the app id, because the title is
/// what the user recognises ("Inbox — Mail") and the app id is what the
/// program calls itself. A window with neither gets a placeholder rather
/// than an empty button nothing can be clicked on.
#[must_use]
pub fn entry_label(info: &WindowInfo) -> String {
    let raw = if info.title.trim().is_empty() {
        info.app_id.trim()
    } else {
        info.title.trim()
    };
    if raw.is_empty() {
        return "(untitled)".to_owned();
    }
    elide(raw, MAX_LABEL_CHARS)
}

/// Cut `s` to `max` characters, marking the cut with `…`.
///
/// Counts **characters**, not bytes, so a title in a script that is not
/// Latin is elided at the same visual length rather than mid-codepoint —
/// and slicing by byte would panic on exactly that input.
#[must_use]
pub fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    // `max - 1` so the ellipsis replaces a character rather than adding
    // one: the result is never wider than `max`.
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Build the main panel's tree, wire the bar up, and return the root.
///
/// Public because the tests build the tree the binary builds: a test that
/// built its own would be testing a second bar.
///
/// # Panics
/// Never in practice — every `attach` names an id this function has just
/// created, and a fresh id cannot be stale.
pub fn build(ui: &mut Ui<Bar>) -> WidgetId {
    let (root, ids) = build_panel(ui);
    install(ui, ids);
    root
}

/// Build one panel's tree: the same tree for the main window and for
/// every other output's panel, so they cannot drift apart.
fn build_panel(ui: &mut Ui<Bar>) -> (WidgetId, Ids) {
    let h = height();

    // -- left: the launcher button and the window list ----------------
    //
    // The launcher button does not open the launcher itself: it asks the
    // server for the overview (`SetOverview(Toggle)`), the same request
    // the launcher sends on a bare-Super tap, so there is one trigger path
    // rather than two that must agree. The server answers every overview
    // watcher, the launcher among them, which is what shows it.
    //
    // The glyph is an **icon**, not a character. `☰` used to be a
    // codepoint in the label, which meant it came from whatever font on
    // the box happened to have U+2630 — a different weight and a
    // different optical size from everything beside it, and nothing at
    // all on a box whose fonts lack it. `icon("list")` is the desktop's
    // own artwork, rasterised by the server at the output's scale and
    // tinted from the palette. The button's *text* is still "Menu", so
    // `hey nitro-bar list` and a screen reader are unaffected.
    let launcher = ui.build(
        button_widget("Menu")
            .icon(icons::LAUNCHER)
            .name(names::LAUNCHER)
            .size(TEXT_SIZE)
            .height_percent(1.0)
            .on_click(|s: &mut Bar, ui: &mut Ui<Bar>| {
                // Counted as well as sent: the count is what
                // `hey nitro-bar do launcher click` can assert on without
                // a server in the loop.
                s.launcher_presses += 1;
                // A request, fire and forget. The bar does not render
                // anything for the overview; the server answers it an
                // `OverviewState`, which it ignores. Checked, because a
                // shell op on the wire socket is fatal.
                if ui.is_shell()
                    && let Err(e) = ui.set_overview(nitro_ui::shell::OverviewRequest::Toggle)
                {
                    eprintln!("nitro-bar: overview: {e}");
                }
            }),
    );
    // `shrink_to_zero` on the window-list **row**, and nowhere else in
    // this file.
    //
    // Every other section of the bar is a fixed string that must keep
    // the width it measured — which is the toolkit's default since #561,
    // and is why the clock and the sensor labels need no opt-out here.
    // The window list is the one part whose content count has no
    // ceiling: each button is already elided to `MAX_LABEL_CHARS` and
    // capped at `MAX_BUTTON_W`, but the *number* of them is not capped
    // at all.
    //
    // Measured on the box at 1920, which corrected a guess made from the
    // 320 px test harness: **twelve** windows need no shrinking at all
    // (buttons at their natural 104 px, the row ending at 1379). The
    // cliff is around twenty. At twenty-four the row is squeezed to 1708
    // and the buttons fall to 65 px, with the clock, battery, load and
    // memory all still on the strip — whereas with the content floor the
    // row would be laid out at its intrinsic ~2700 px and push all four
    // clean off the end of the bar, including the clock that is supposed
    // to be centred *on the bar*.
    //
    // So this row keeps the elastic behaviour: the buttons divide
    // whatever is left over, and a title too narrow to read is the
    // honest signal that there are too many windows for the bar. That is
    // the better failure, because the sections it would otherwise
    // displace are the ones the user did not open and cannot close.
    let windows = ui.build(
        row()
            .name(names::WINDOWS)
            .gap(GAP)
            .shrink_to_zero()
            .height_percent(1.0),
    );

    // -- centre: the clock --------------------------------------------
    let clock_id = ui.build(
        label("")
            .name(names::CLOCK)
            .size(TEXT_SIZE)
            .align(nitro_ui::Align::Center),
    );

    // -- right: battery, load, memory ---------------------------------
    // The status pill: the volume icon and the battery reading in one
    // clickable group that opens the quick-settings menu. The icon is
    // set by `quick::sync`, and only when its name changes.
    let volume_icon = ui.build(
        icon("sliders")
            .name(quick::names::STATUS_VOLUME)
            .size(ICON_PX)
            .color_role(ColorRole::Text),
    );
    let battery = ui.build(
        label("")
            .name(names::BATTERY)
            .size(TEXT_SIZE)
            .color_role(ColorRole::Text),
    );
    let status = ui.build(
        status_pill("Quick Settings")
            .name(names::STATUS)
            .height(h - 6.0),
    );
    ui.attach(status, volume_icon).unwrap();
    ui.attach(status, battery).unwrap();
    quick::hook_pill(ui, status);
    // The two readouts that are *numbers with no units* get an icon in
    // front of them, because `0.4  1.2/3.3G` says nothing about which is
    // which. The icons are static — they are painted once and never
    // again, however many sensor polls go past, which is what keeps the
    // bar's idle contract intact (`icons_are_painted_once_and_never_again`
    // in `tests/bar.rs`).
    //
    // Battery and wifi deliberately get none: the bar has a battery
    // *sensor* and no wifi one, and an icon in front of `87%+` would be
    // redundant where an icon in front of `0.4` is the only thing that
    // makes it readable.
    let load_icon = ui.build(
        icon(icons::LOAD)
            .name(names::LOAD_ICON)
            .size(ICON_PX)
            .color_role(ColorRole::TextDim),
    );
    let load = ui.build(
        label("")
            .name(names::LOAD)
            .size(TEXT_SIZE)
            .color_role(ColorRole::TextDim),
    );
    let mem_icon = ui.build(
        icon(icons::MEM)
            .name(names::MEM_ICON)
            .size(ICON_PX)
            .color_role(ColorRole::TextDim),
    );
    let mem = ui.build(
        label("")
            .name(names::MEM)
            .size(TEXT_SIZE)
            .color_role(ColorRole::TextDim),
    );

    // One row, with a spacer on each side of the clock: that is what
    // keeps the clock centred *on the bar* rather than centred in
    // whatever the window list happens to leave over.
    let root = ui.build(
        row()
            .gap(GAP)
            .padding_xy(PAD, 0.0)
            .height(h)
            .width_percent(1.0)
            .cross_align(nitro_ui::CrossAlign::Center),
    );
    let left_pad = ui.build(spacer().grow(1.0));
    let right_pad = ui.build(spacer().grow(1.0));
    for child in [
        launcher, windows, left_pad, clock_id, right_pad, load_icon, load, mem_icon, mem, status,
    ] {
        ui.attach(root, child).unwrap();
    }

    (
        root,
        Ids {
            windows,
            clock: clock_id,
            battery,
            load,
            mem,
            volume_icon,
        },
    )
}

/// Wire the tree up: subscribe to the window list and the outputs, watch
/// where the main window lands, and arm the clock and sensor timers.
///
/// Everything the bar *does* is registered here, and all of it is
/// event-driven: a subscription rather than a poll for the window list
/// and the outputs, a minute-aligned timer for the clock, and a 30 s
/// timer for the sensors that writes to the tree only when a string
/// actually changed.
fn install(ui: &mut Ui<Bar>, ids: Ids) {
    ui.on_shell(move |s: &mut Bar, ui: &mut Ui<Bar>, ev: &ShellEvent| {
        ensure_main(s, ids);
        match ev {
            ShellEvent::Window(info) => upsert(s, ui, info),
            ShellEvent::WindowGone(w) => remove(s, ui, *w),
            // The output snapshot's end *is* the moment: a hotplug re-sends
            // the whole list, and the panels are reconciled against the
            // complete list rather than against each output as it arrives.
            ShellEvent::Output(info) => s.pending_outputs.push(info.id),
            ShellEvent::OutputsEnd => {
                s.outputs = std::mem::take(&mut s.pending_outputs);
                reconcile(s, ui);
            }
            // The snapshot that follows would close it too; closing it
            // here shortens the moment the server's migration leaves two
            // bars on the primary output.
            ShellEvent::OutputGone(id) => {
                let gone: Vec<WindowId> = s
                    .panels
                    .iter()
                    .skip(1)
                    .filter(|p| p.output == Some(*id))
                    .map(|p| p.win)
                    .collect();
                close_panels(s, ui, &gone);
            }
            // The window snapshot's end needs no special case: every
            // window in it arrived as an ordinary `Window` and was
            // upserted. Keying on it would be a second code path that has
            // to agree with the first.
            ShellEvent::WindowListEnd | ShellEvent::HotKey { .. } | ShellEvent::Overview { .. } => {
            }
        }
    });

    // Where the main window landed. `Outputs` is answered before the main
    // window's first `Configure`, so this is what tells the bar which
    // output it must *not* open a second panel on; a resize says nothing
    // about that, which is why it is not `on_resize`.
    ui.on_window_placed(WindowId::MAIN, move |s: &mut Bar, ui: &mut Ui<Bar>, out| {
        ensure_main(s, ids);
        s.panels[0].output = Some(out);
        reconcile(s, ui);
    });

    // The window list and the outputs. Asking subscribes, so these are the
    // only requests the bar ever makes about either: everything after
    // them arrives unasked. A bar on an unprivileged connection would be
    // *disconnected* for sending them, so the capability is checked
    // rather than assumed.
    if ui.is_shell() {
        // Not fatal: a bar with no window list is still a clock and
        // three readouts, and dying here would take the whole panel
        // off the screen over one failed request.
        if let Err(e) = ui.window_list() {
            eprintln!("nitro-bar: window list: {e}");
        }
        if let Err(e) = ui.outputs() {
            eprintln!("nitro-bar: outputs: {e}");
        }
    }

    tick_clock(ui, ids);
    arm_first_sensor_poll(ui, ids);
    // One mixer read for the pill's icon, before the first frame. Not a
    // poll: nothing re-arms it.
    ui.set_timer(0, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        ensure_main(s, ids);
        quick::init(s, ui);
    });
}

/// Make the main window the first [`Panel`], if it is not yet.
///
/// Called at the top of every callback rather than once from `build`,
/// because `build` has no state to write into; see [`Ids`].
fn ensure_main(s: &mut Bar, ids: Ids) {
    if s.panels.is_empty() {
        s.panels.push(Panel {
            output: None,
            win: WindowId::MAIN,
            ids,
            entries: Vec::new(),
        });
    }
}

/// Make the panels match the outputs: one on every output in the last
/// snapshot, the main window's excepted.
///
/// Idempotent, and safe in any order: before the main window is placed
/// nothing is known about which output to skip, so nothing is done; before
/// the first `OutputsEnd` there are no outputs and no extra panels. Also
/// closes a panel whose output the main window was migrated *onto* — the
/// server re-homes an orphaned window to the primary, and a bar that
/// followed only the output list would be two bars on that screen.
fn reconcile(s: &mut Bar, ui: &mut Ui<Bar>) {
    let Some(main) = s.main_output() else {
        return;
    };
    let stale: Vec<WindowId> = s
        .panels
        .iter()
        .skip(1)
        .filter(|p| {
            p.output
                .is_none_or(|o| o == main || !s.outputs.contains(&o))
        })
        .map(|p| p.win)
        .collect();
    close_panels(s, ui, &stale);
    for out in s.outputs.clone() {
        if out != main && !s.panels.iter().any(|p| p.output == Some(out)) {
            open_panel(s, ui, out);
        }
    }
}

/// Close the given extra panels' windows and forget them.
fn close_panels(s: &mut Bar, ui: &mut Ui<Bar>, wins: &[WindowId]) {
    for win in wins {
        if *win == WindowId::MAIN {
            continue;
        }
        // The window's close handler forgets the panel; this is the same
        // forgetting for a `remove_window` that failed, which only means
        // the server had closed it already.
        let _ = ui.remove_window(s, *win);
        s.panels.retain(|p| p.win != *win);
    }
}

/// Open a panel on `output`: the same tree as the main window, anchored
/// to that output, listing the same windows and showing the same clock
/// and readouts.
fn open_panel(s: &mut Bar, ui: &mut Ui<Bar>, output: u32) {
    let h = height();
    let (root, ids) = build_panel(ui);
    let surface = Surface::bar(h as u32).anchored(Anchor::top().on(output));
    // A width the anchor immediately overrides, as in `run`.
    let win = match ui.add_surface_window("nitro-bar", Some(Size::new(640.0, h)), root, surface) {
        Ok(win) => win,
        Err(e) => {
            // Not fatal: the primary output still has its bar. The tree
            // built above would otherwise sit in the arena unreachable.
            eprintln!("nitro-bar: panel on output {output}: {e}");
            let _ = ui.remove(root);
            return;
        }
    };
    // Should the server close it (it does not, today), forget it rather
    // than keep a panel whose window is gone.
    ui.on_window_closed(win, move |s: &mut Bar, _ui: &mut Ui<Bar>| {
        s.panels.retain(|p| p.win != win);
    });
    s.panels.push(Panel {
        output: Some(output),
        win,
        ids,
        entries: Vec::new(),
    });
    if !s.quick.icon().is_empty()
        && let Ok(mut i) = ui.widget_mut::<nitro_ui::widgets::Icon>(ids.volume_icon)
    {
        i.set_icon(s.quick.icon().to_owned());
    }
    let idx = s.panels.len() - 1;
    for info in s.infos.clone() {
        upsert_in(s, ui, idx, &info);
    }
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.clock) {
        l.set_text(s.clock_text.clone());
    }
    for (id, reading) in [
        (ids.battery, &s.last.battery),
        (ids.load, &s.last.load),
        (ids.mem, &s.last.mem),
    ] {
        if let Ok(mut l) = ui.widget_mut::<Label>(id) {
            l.set_text(reading.clone().unwrap_or_default());
        }
    }
}

/// The label a window-list button shows: the window's label, with a
/// marker when it holds focus and a bracket when it is minimized.
///
/// Focus is shown *in the label* rather than by disabling the other
/// buttons, which was the first attempt and was plainly wrong: a disabled
/// button ignores clicks, so dimming the unfocused entries made every row
/// but one unclickable — in the one widget whose entire purpose is to be
/// clicked. It is also the form a script can read, since `hey` prints a
/// button's label as its value.
///
/// A **minimized** window is marked the same way, and for the same
/// reason: `[Calculator]` rather than a state a script cannot see. The
/// brackets are the convention every taskbar since twm has used for "put
/// away", they survive a palette a user has made low-contrast, and they
/// are paired with — not replaced by — the dimmed tint the button takes
/// (see [`entry_text_role`]): colour alone is an affordance a
/// colour-blind user does not get, and a marker alone is one that is easy
/// to miss in a row of eight.
#[must_use]
pub fn button_text(label: &str, focused: bool, minimized: bool) -> String {
    if minimized {
        // Never both markers: a minimized window does not hold focus (the
        // server hands focus on when it minimizes one), so `▸ [x]` would
        // be a state that cannot happen.
        return format!("[{label}]");
    }
    if focused {
        format!("▸ {label}")
    } else {
        label.to_owned()
    }
}

/// The palette role a window-list button's label takes.
///
/// A minimized window's entry is **dimmed**: it is still a row you can
/// click — that is the whole of #3724's second half — but it is not a
/// window that is on screen, and a task list in which the eight rows for
/// eight windows look identical says nothing about which of them you can
/// actually see. `TextDim` is the role for exactly that ("a hint, a units
/// suffix, a disabled label", `docs/theme.md`) and it is contrast-checked
/// against the window background in both schemes by the palette's own
/// tests, so the dimmed row stays readable rather than becoming a row
/// nobody can make out.
///
/// Not `set_enabled(false)`, which would be the obvious way to grey a
/// button and is the one thing that must not happen here: a disabled
/// button ignores clicks, and clicking a minimized entry is precisely
/// what has to work.
///
/// **The label only, never the icon.** The button's primary icon is the
/// window's app id resolved in the machine's icon theme, and dimming it
/// would not dim it — it would destroy it. See the argument at the
/// `.icon_coloured(…)` call in [`upsert`], and
/// `a_minimized_entry_dims_its_label_and_leaves_its_icon_alone`.
#[must_use]
pub fn entry_text_role(minimized: bool) -> ColorRole {
    if minimized {
        ColorRole::TextDim
    } else {
        ColorRole::ButtonText
    }
}

/// Insert or update one window's entry, on every panel.
///
/// There is no separate "added" path: the server sends the same
/// `WindowInfo` for the snapshot and for every later change, so keying on
/// the id and upserting is one code path where "added vs changed" would
/// be two that must agree.
///
/// Two kinds of window are **skipped**.
///
/// The bar's own, by app id. It is a window like any other as far as the
/// server is concerned, and listing itself would give the user a button
/// that focuses a `NO_FOCUS` panel — a row that does nothing, which is
/// worse than no row.
///
/// And every window that is not on the `Normal` layer. A task list lists
/// *applications*; the wallpaper (`Background`), a dock or another bar
/// (`Top`) and the launcher (`Overlay`) are furniture, and every one of
/// them is as unfocusable as the bar itself. Filtering on the app id
/// alone was not enough — it only ever hid *this* bar, so with the
/// wallpaper and the launcher running the list showed `nitro-wallpaper`
/// and `nitro-launcher` as windows.
fn upsert(s: &mut Bar, ui: &mut Ui<Bar>, info: &WindowInfo) {
    if info.app_id == APP_NAME || info.layer != Layer::Normal {
        // A window can change layer, so this is a *removal*, not just a
        // skip: an application that became a shell surface after it was
        // listed would otherwise keep its button forever.
        remove(s, ui, info.window);
        return;
    }
    // Remembered first, so a panel opened later lists it too.
    if let Some(i) = s.infos.iter_mut().find(|i| i.window == info.window) {
        i.clone_from(info);
    } else {
        s.infos.push(info.clone());
    }
    for idx in 0..s.panels.len() {
        upsert_in(s, ui, idx, info);
    }
}

/// [`upsert`] for one panel, whose `WindowInfo` has already passed the
/// filter above.
fn upsert_in(s: &mut Bar, ui: &mut Ui<Bar>, idx: usize, info: &WindowInfo) {
    let text = entry_label(info);
    let icon = entry_icon(info);
    let window = info.window;
    let minimized = info.state == WindowState::Minimized;
    let panel = &mut s.panels[idx];
    if let Some(e) = panel.entries.iter_mut().find(|e| e.window == window) {
        let id = e.id;
        e.text.clone_from(&text);
        e.focused = info.focused;
        let dim_changed = e.minimized != minimized;
        e.minimized = minimized;
        let icon_changed = e.icon != icon;
        e.icon.clone_from(&icon);
        // One setter for both changes, because both are the same string.
        // It returns early when the string is unchanged, so a
        // `WindowInfo` that changed nothing we draw costs no mutation and
        // no commit — which matters, because a focus change sends one for
        // *both* windows involved.
        if let Ok(mut b) = ui.widget_mut::<Button<Bar>>(id) {
            b.set_text(button_text(&text, info.focused, minimized));
            // The tint only when the state actually crossed the minimize
            // line, for the reason the icon is guarded below: an
            // unconditional setter would be a paint per `WindowInfo` and
            // the bar's idle claim is counted, not eyeballed.
            if dim_changed {
                b.set_text_role(Some(entry_text_role(minimized)));
            }
            // The icon only when the app id moved, which is almost never:
            // an app id is fixed for a window's life in every client we
            // ship, and the one thing that can change it — a late
            // `SetAppId` — is exactly when the icon should change too.
            // Guarding it here rather than leaning on the setter's own
            // early return is what makes the idle claim checkable: the
            // 120 s tick test asserts **zero** `SetIcon`s, not "no
            // visible change".
            if icon_changed {
                b.set_icon_coloured(icon.clone());
            }
        }
        return;
    }
    let id = ui.build(
        button_widget(button_text(&text, info.focused, minimized))
            // Named by the server's own window id, so the path a script
            // uses (`windows/win7`) is stable for the window's whole life
            // and names the same window the server does.
            .name(entry_name(window))
            .size(TEXT_SIZE)
            // The app id **as an icon name**, which is the freedesktop
            // convention: an application's desktop file is usually named
            // after its app id and its `Icon=` usually matches. It is a
            // claim about the box rather than a fact about it, so it is
            // paired with a symbolic fallback — see `docs/shell.md` for
            // the rule, its limitation, and the bigger alternative that
            // was not taken.
            .icon_size(ICON_PX)
            .icon_coloured(icon.clone())
            // **The icon's tint does not follow the dimming**, and that is
            // not an oversight — it is the one thing the minimized look
            // must not touch. The primary icon's name is the window's
            // **app id**, resolved in the machine's icon theme, and the
            // role byte is the server's *selector* rather than a hint: a
            // non-`AS_COLOURED` role means the compiled-in symbolic set
            // and nothing else (`clients.rs::icon_handle`). So asking for
            // `firefox` in `TextDim` is not "the Firefox logo, dimmed", it
            // is a lookup of `firefox` in a set that does not have it —
            // `BadIcon`, the node cleared, the toolkit's one rescue spent
            // swapping the name for `window`, and on restore a
            // `lookup_app("window")` that fails with the rescue already
            // latched. The entry's icon would be gone for good, and
            // `upsert` could not repair it: `set_icon_coloured` only fires
            // `if icon_changed`, and the app id has not changed.
            //
            // It is also the right *look*, which is what
            // `docs/wm.md` says about the frame's own app icon: somebody
            // else's artwork has no tint to change, and a minimized
            // Firefox is still the Firefox logo. The bracket and the
            // dimmed label carry the state.
            .icon_fallback_tinted(icons::WINDOW, IconTint::Role(ColorRole::ButtonText))
            .text_role(entry_text_role(minimized))
            .max_width(MAX_BUTTON_W)
            // The buttons shrink with their row, for the reason the row
            // does (see `build`): the window list is the one part of the
            // bar whose content has no ceiling, and a squeezed title is
            // a better failure than a button drawn over the clock. A
            // `Zero` floor on the row alone would not do it — the row
            // would be narrowed and its children would then overflow
            // *it* — which is the container-versus-child pair #561 is
            // about, read in the one direction where the child really is
            // the elastic one.
            .shrink_to_zero()
            .height_percent(1.0)
            .on_click(move |s: &mut Bar, ui: &mut Ui<Bar>| {
                // **The taskbar toggle**, and the whole of #3724's second
                // half. Three cases, one message each, decided from the
                // state the bar already holds:
                //
                // * unfocused, on screen → `FocusWindow`: focus and raise.
                // * minimized → `FocusWindow` as well. The *server*
                //   restores it first (`Server::focus_window_for_shell`),
                //   because "un-minimize then focus" is one act and a bar
                //   that sent two messages could have the second refused
                //   for the state the first had just changed.
                // * focused and on screen → minimize it. That is what
                //   every taskbar does, and it is the only way to *put a
                //   window away* from the bar: there was none before, so
                //   the focused row was a button that did nothing.
                //
                // No new wire message: minimizing goes out as the
                // `SetWindowStateFor` the bar could already send. Every
                // panel's entry for the window says the same, so the
                // first one found is the answer.
                let entry = s
                    .panels
                    .iter()
                    .flat_map(|p| p.entries.iter())
                    .find(|e| e.window == window);
                let put_away = entry.is_some_and(|e| e.focused && !e.minimized);
                if put_away {
                    let _ = ui.set_window_state_for(window, WindowState::Minimized);
                    return;
                }
                // Silently refused by the server when it cannot be
                // honoured, on the same terms a click on the window would
                // be; a task list must not be able to wedge the keyboard.
                let _ = ui.focus_window(window);
            })
            .on_alt_click(move |_s: &mut Bar, ui: &mut Ui<Bar>| {
                // A *request*: the owning client is told and decides, so
                // unsaved work survives a misclick in the task list.
                let _ = ui.close_window(window);
            }),
    );
    let panel = &mut s.panels[idx];
    if ui.attach(panel.ids.windows, id).is_err() {
        // Unreachable while `ids.windows` outlives the tree, which it
        // does — but a button left in the arena with no parent and
        // nothing pointing at it would be a leak that never announced
        // itself. The widget is dropped on the way out.
        let _ = ui.remove(id);
        return;
    }
    panel.entries.push(Entry {
        window,
        text,
        icon,
        focused: info.focused,
        minimized,
        id,
    });
}

/// Drop a window's entry and its button, on every panel.
fn remove(s: &mut Bar, ui: &mut Ui<Bar>, window: WindowRef) {
    s.infos.retain(|i| i.window != window);
    for panel in &mut s.panels {
        let Some(i) = panel.entries.iter().position(|e| e.window == window) else {
            continue;
        };
        let e = panel.entries.remove(i);
        // One `DestroyNode` on the button's group frees the subtree
        // server-side; a stale id afterwards is an error, never a panic.
        let _ = ui.remove(e.id);
    }
}

/// The `hey`-addressable name of a window-list entry.
#[must_use]
pub fn entry_name(window: WindowRef) -> String {
    format!("win{}", window.raw())
}

/// Paint the clock immediately and arm its timer.
///
/// The first paint cannot wait for the timer: a bar that came up blank
/// until the next :00 would look broken for up to 59 seconds.
fn tick_clock(ui: &mut Ui<Bar>, ids: Ids) {
    // A zero-delay timer rather than a direct call, because the state is
    // not reachable from here — `install` runs inside `build`, which has
    // only the tree. The app loop runs due timers on its first turn, so
    // this fires before the first frame is presented.
    ui.set_timer(0, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        apply_clock(s, ui, ids);
    });
}

/// Write the current `HH:MM` into the clock label, if it changed, and
/// re-arm for the next boundary.
///
/// The timer is re-armed from *inside* the callback rather than being a
/// repeating one, because the interval is not constant: it is "however
/// long until the next :00", recomputed each time. A fixed 60 s repeat
/// would drift off the boundary over hours and eventually tick at :30.
fn apply_clock(s: &mut Bar, ui: &mut Ui<Bar>, ids: Ids) {
    ensure_main(s, ids);
    let now = s.now_ms();
    let text = clock::format_hm(now.div_euclid(1_000), &s.zone);
    if text != s.clock_text {
        s.clock_text.clone_from(&text);
        s.ticks += 1;
        for p in &s.panels {
            if let Ok(mut l) = ui.widget_mut::<Label>(p.ids.clock) {
                l.set_text(text.clone());
            }
        }
    }
    let ms = clock::ms_to_next_minute(now);
    ui.set_timer(ms, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        apply_clock(s, ui, ids);
    });
}

/// Arm the **first** sensor poll, which is immediate: a bar that came up
/// with three blank readouts for half a minute would look broken.
///
/// Every later poll is re-armed from inside [`poll_sensors`] itself,
/// where `s.poll_ms` is reachable — so a test that shortens the interval
/// is obeyed from the next poll on, and there is no second arming path
/// reading the [`POLL_MS`] constant behind the state's back.
fn arm_first_sensor_poll(ui: &mut Ui<Bar>, ids: Ids) {
    ui.set_timer(0, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        poll_sensors(s, ui, ids);
    });
}

/// Re-read the three sensors and push whatever changed.
///
/// The readings are compared as **strings**, not as numbers: the string
/// is what is drawn, so two loads that both render `0.4` are the same
/// reading as far as the bar is concerned. The comparison happens *here*,
/// against [`Bar::last`], before the tree is touched at all — a `Label`'s
/// setter also returns early on an unchanged string, but that is a second
/// line of defence in another crate, and the bar's own claim should be
/// legible in the bar's own code. That is the whole reason a poll costs
/// nothing on the wire: the poll happens, the tree does not move, and
/// `flush` sends no commit.
fn poll_sensors(s: &mut Bar, ui: &mut Ui<Bar>, ids: Ids) {
    ensure_main(s, ids);
    let now = (s.source)();
    s.polls += 1;
    if now != s.last {
        for p in &s.panels {
            for (id, reading) in [
                (p.ids.battery, &now.battery),
                (p.ids.load, &now.load),
                (p.ids.mem, &now.mem),
            ] {
                let text = reading.clone().unwrap_or_default();
                if let Ok(mut l) = ui.widget_mut::<Label>(id) {
                    l.set_text(text);
                }
            }
        }
        s.last = now;
    }
    // Re-armed from inside the callback with the *current* interval, so a
    // test that shortens it is obeyed from the next poll on.
    let ms = s.poll_ms;
    ui.set_timer(ms, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        poll_sensors(s, ui, ids);
    });
}

///
/// # Errors
/// Any connection, wire or `epoll` failure. They are all fatal — and a
/// failure to reach the **shell** socket is the loudest of them: a bar
/// that silently fell back to the ordinary socket would come up looking
/// right and die on its first shell op.
pub fn run() -> Result<(), Error> {
    let h = height();
    App::shell(APP_NAME)?
        .title("nitro-bar")
        .surface(Surface::bar(h as u32))
        // A width the anchor immediately overrides; the server decides
        // the real one, which is the point of anchoring.
        .size(Size::new(640.0, h))
        .run(Bar::new(), build)
}
