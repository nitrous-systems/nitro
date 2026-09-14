//! `nitro-bar` — the desktop's top bar, and the first program written
//! against the **shell socket**.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │ ☰  Calculator │ hello-dialog          09:41          87%+ 0.42 1.2/3.3G │
//! └──────────────────────────────────────────────────────────────────┘
//!    launcher  ── windows ──            clock        battery load  mem
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
//! hey nitro-bar do windows/win3 click        # focus that window
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
//! * the sensors poll every 5 s but write to the tree **only when the
//!   formatted string differs** — and a `Label`'s setter returns early on
//!   an unchanged string, so an unchanged reading costs no mutation, no
//!   commit and no wakeup beyond the timer itself.
//!
//! `a_settled_bar_is_silent_between_clock_ticks` in `tests/bar.rs`
//! asserts it from the outside, by counting commits over a window in
//! which the sensors are polled several times.
//!
//! # One bar, on the primary output
//!
//! The spec asked for one bar per output, following hotplug. **That is
//! not implementable on this protocol**, and the bar deliberately does
//! not fake it: a client cannot choose which output its window opens on,
//! `SetAnchor` anchors to whichever output the window is already on, and
//! nothing moves a window between outputs but a user's drag. N bar
//! windows would therefore all land on the primary output — N overlapping
//! bars and N×32 px of zone on one screen, which is worse than one bar.
//!
//! `docs/shell.md` §Deferred already records the gap ("Per-output shell
//! surfaces"), and the fix is an `output` field on `SetAnchor`. The bar
//! is structured so that adding it is small: everything below is per-bar
//! state reached through one [`Bar`] and one tree, so a second output
//! means a second `Ui` (one window each — `Ui` owns exactly one window)
//! rather than any change to the layout, the window list or the sensors.
//! The crate README says the same thing to a reader who is not in the
//! source.

pub mod clock;
pub mod sensors;

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::shell::{ShellEvent, Surface, WindowInfo, WindowRef};
use nitro_ui::widgets::{Button, Label, button as button_widget, label, row, spacer};
use nitro_ui::{App, Error, Size, Ui, WidgetId};

/// The name the bar registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-bar";

/// Logical height of the bar, and the space it reserves. Overridden by
/// `NITRO_BAR_HEIGHT`.
pub const DEFAULT_HEIGHT: f32 = 32.0;

/// How often the battery, load and memory readouts are re-read.
///
/// Five seconds is the spec's number and a defensible one: these are
/// three small virtual files, and a user reading a load average does not
/// need it fresher than the kernel's own 1-minute smoothing.
pub const POLL_MS: u64 = 5_000;

/// Font size of the bar's text. Smaller than the toolkit default, because
/// a 32 px strip has to fit a line of text with air around it.
const TEXT_SIZE: f32 = 13.0;

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
    /// The battery readout.
    pub const BATTERY: &str = "battery";
    /// The CPU load readout.
    pub const LOAD: &str = "load";
    /// The memory readout.
    pub const MEM: &str = "mem";
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
    /// Whether it is drawn as focused.
    focused: bool,
    /// The button widget.
    id: WidgetId,
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
    /// The window list, in the order the server reports it (by window
    /// identity, so the list does not reshuffle when a window is raised).
    entries: Vec<Entry>,
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
    /// The last readings pushed into the three labels, so a poll that
    /// finds the same numbers can be seen to have changed nothing.
    last: Readings,
}

impl Bar {
    /// A bar with no widgets yet; [`build`] fills in the ids.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            zone: clock::Zone::local(),
            fake_time_ms: fake_time_ms(),
            clock_text: String::new(),
            ticks: 0,
            launcher_presses: 0,
            polls: 0,
            poll_ms: POLL_MS,
            source: Box::new(read_sensors),
            last: Readings::default(),
        }
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

    /// The listed windows, in order. For the tests.
    #[must_use]
    pub fn windows(&self) -> Vec<WindowRef> {
        self.entries.iter().map(|e| e.window).collect()
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
        self.entries.len()
    }

    /// The labels of the window list, in order. For the tests.
    #[must_use]
    pub fn window_labels(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.text.clone()).collect()
    }

    /// Which window the list draws as focused, if any.
    #[must_use]
    pub fn focused_window(&self) -> Option<WindowRef> {
        self.entries.iter().find(|e| e.focused).map(|e| e.window)
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

/// The bar's widget ids, gathered by [`build`].
///
/// `build` only has the tree — the state does not exist yet when the
/// callbacks are written, and the callbacks need the ids. So the ids are
/// `Copy` and captured into the closures, and [`install`] writes the same
/// set into [`Bar`] on the first turn of the loop. That is the
/// calculator's `Screen` trick, and it is what lets the state be built
/// before the tree it will drive.
#[derive(Debug, Clone, Copy)]
struct Ids {
    windows: WidgetId,
    clock: WidgetId,
    battery: WidgetId,
    load: WidgetId,
    mem: WidgetId,
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

/// Build the whole tree and return its root.
///
/// Public because the tests build the tree the binary builds: a test that
/// built its own would be testing a second bar.
///
/// # Panics
/// Never in practice — every `attach` names an id this function has just
/// created, and a fresh id cannot be stale.
pub fn build(ui: &mut Ui<Bar>) -> WidgetId {
    let h = height();
    let dim = ui.theme().text_disabled;

    // -- left: the launcher button and the window list ----------------
    //
    // The launcher button does not open the launcher itself: it fires the
    // same bare-Super path the launcher already listens on (#3691), so
    // there is one trigger rather than two that must agree.
    let launcher = ui.build(
        button_widget("☰")
            .name(names::LAUNCHER)
            .size(TEXT_SIZE)
            .height_percent(1.0)
            .on_click(|s: &mut Bar, _ui: &mut Ui<Bar>| {
                // Counted rather than acted on: the process that owns the
                // launcher is #3691, and wiring this to a binary that is
                // not there yet would be a half-feature. The count is
                // what `hey nitro-bar do launcher click` can assert on.
                s.launcher_presses += 1;
            }),
    );
    let windows = ui.build(row().name(names::WINDOWS).gap(GAP).height_percent(1.0));

    // -- centre: the clock --------------------------------------------
    let clock_id = ui.build(
        label("")
            .name(names::CLOCK)
            .size(TEXT_SIZE)
            .align(nitro_ui::Align::Center),
    );

    // -- right: battery, load, memory ---------------------------------
    let battery = ui.build(label("").name(names::BATTERY).size(TEXT_SIZE).color(dim));
    let load = ui.build(label("").name(names::LOAD).size(TEXT_SIZE).color(dim));
    let mem = ui.build(label("").name(names::MEM).size(TEXT_SIZE).color(dim));

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
        launcher, windows, left_pad, clock_id, right_pad, battery, load, mem,
    ] {
        ui.attach(root, child).unwrap();
    }

    install(
        ui,
        Ids {
            windows,
            clock: clock_id,
            battery,
            load,
            mem,
        },
    );
    root
}

/// Wire the tree up: stash the ids, subscribe to the window list, and
/// arm the clock and sensor timers.
///
/// Everything the bar *does* is registered here, and all of it is
/// event-driven: a subscription rather than a poll for the window list, a
/// minute-aligned timer for the clock, and a 5 s timer for the sensors
/// that writes to the tree only when a string actually changed.
fn install(ui: &mut Ui<Bar>, ids: Ids) {
    ui.on_shell(
        move |s: &mut Bar, ui: &mut Ui<Bar>, ev: &ShellEvent| match ev {
            ShellEvent::Window(info) => upsert(s, ui, ids, info),
            ShellEvent::WindowGone(w) => remove(s, ui, *w),
            // The snapshot's end needs no special case: every window in it
            // arrived as an ordinary `Window` and was upserted. Keying on it
            // would be a second code path that has to agree with the first.
            _ => {}
        },
    );

    // The window list. Asking subscribes, so this is the only request the
    // bar ever makes about windows: everything after it arrives unasked.
    // A bar on an unprivileged connection would be *disconnected* for
    // sending this, so the capability is checked rather than assumed.
    if ui.is_shell()
        && let Err(e) = ui.window_list()
    {
        // Not fatal: a bar with no window list is still a clock and
        // three readouts, and dying here would take the whole panel
        // off the screen over one failed request.
        eprintln!("nitro-bar: window list: {e}");
    }

    tick_clock(ui, ids);
    arm_sensors(ui, ids, true);
}

/// The label a window-list button shows: the window's label, with a
/// marker when it holds focus.
///
/// Focus is shown *in the label* rather than by disabling the other
/// buttons, which was the first attempt and was plainly wrong: a disabled
/// button ignores clicks, so dimming the unfocused entries made every row
/// but one unclickable — in the one widget whose entire purpose is to be
/// clicked. It is also the form a script can read, since `hey` prints a
/// button's label as its value.
#[must_use]
pub fn button_text(label: &str, focused: bool) -> String {
    if focused {
        format!("▸ {label}")
    } else {
        label.to_owned()
    }
}

/// Insert or update one window's entry.
///
/// There is no separate "added" path: the server sends the same
/// `WindowInfo` for the snapshot and for every later change, so keying on
/// the id and upserting is one code path where "added vs changed" would
/// be two that must agree.
///
/// The bar's own window is **skipped**. It is a window like any other as
/// far as the server is concerned, and listing itself would give the user
/// a button that focuses a `NO_FOCUS` panel — a row that does nothing,
/// which is worse than no row.
fn upsert(s: &mut Bar, ui: &mut Ui<Bar>, ids: Ids, info: &WindowInfo) {
    if info.app_id == APP_NAME {
        return;
    }
    let text = entry_label(info);
    let window = info.window;
    if let Some(e) = s.entries.iter_mut().find(|e| e.window == window) {
        let id = e.id;
        e.text.clone_from(&text);
        e.focused = info.focused;
        // One setter for both changes, because both are the same string.
        // It returns early when the string is unchanged, so a
        // `WindowInfo` that changed nothing we draw costs no mutation and
        // no commit — which matters, because a focus change sends one for
        // *both* windows involved.
        if let Ok(mut b) = ui.widget_mut::<Button<Bar>>(id) {
            b.set_text(button_text(&text, info.focused));
        }
        return;
    }
    let id = ui.build(
        button_widget(button_text(&text, info.focused))
            // Named by the server's own window id, so the path a script
            // uses (`windows/win7`) is stable for the window's whole life
            // and names the same window the server does.
            .name(entry_name(window))
            .size(TEXT_SIZE)
            .max_width(MAX_BUTTON_W)
            .height_percent(1.0)
            .on_click(move |_s: &mut Bar, ui: &mut Ui<Bar>| {
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
    if ui.attach(ids.windows, id).is_err() {
        return;
    }
    s.entries.push(Entry {
        window,
        text,
        focused: info.focused,
        id,
    });
}

/// Drop a window's entry and its button.
fn remove(s: &mut Bar, ui: &mut Ui<Bar>, window: WindowRef) {
    let Some(i) = s.entries.iter().position(|e| e.window == window) else {
        return;
    };
    let e = s.entries.remove(i);
    // One `DestroyNode` on the button's group frees the subtree
    // server-side; a stale id afterwards is an error, never a panic.
    let _ = ui.remove(e.id);
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
    let now = s.now_ms();
    let text = clock::format_hm(now.div_euclid(1_000), &s.zone);
    if text != s.clock_text {
        s.clock_text.clone_from(&text);
        s.ticks += 1;
        if let Ok(mut l) = ui.widget_mut::<Label>(ids.clock) {
            l.set_text(text);
        }
    }
    let ms = clock::ms_to_next_minute(now);
    ui.set_timer(ms, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        apply_clock(s, ui, ids);
    });
}

/// Arm the sensor poll.
///
/// The first poll is immediate — a bar that came up with three blank
/// readouts for five seconds would look broken — and the interval comes
/// from the state so a test can shorten it.
fn arm_sensors(ui: &mut Ui<Bar>, ids: Ids, first: bool) {
    if first {
        ui.set_timer(0, move |s: &mut Bar, ui: &mut Ui<Bar>| {
            poll_sensors(s, ui, ids);
        });
        return;
    }
    ui.set_timer(POLL_MS, move |s: &mut Bar, ui: &mut Ui<Bar>| {
        poll_sensors(s, ui, ids);
    });
}

/// Re-read the three sensors and push whatever changed.
///
/// The readings are compared as **strings**, not as numbers: the string
/// is what is drawn, so two loads that round to `0.42` are the same
/// reading as far as the bar is concerned, and a `Label`'s setter returns
/// early on an unchanged string. That is the whole reason a 5 s poll
/// costs nothing on the wire — the poll happens, the tree does not move,
/// and `flush` sends no commit.
fn poll_sensors(s: &mut Bar, ui: &mut Ui<Bar>, ids: Ids) {
    let now = (s.source)();
    for (id, reading) in [
        (ids.battery, &now.battery),
        (ids.load, &now.load),
        (ids.mem, &now.mem),
    ] {
        let text = reading.clone().unwrap_or_default();
        if let Ok(mut l) = ui.widget_mut::<Label>(id) {
            l.set_text(text);
        }
    }
    s.last = now;
    s.polls += 1;
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
