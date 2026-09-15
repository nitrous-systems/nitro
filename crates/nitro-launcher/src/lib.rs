//! `nitro-launcher` — the desktop's application launcher: a Super-tap
//! overlay that searches `.desktop` files and starts what you pick.
//!
//! ```text
//!                        ┌────────────────────────────────┐
//!                        │ calc                           │  ← query
//!                        ├────────────────────────────────┤
//!                        │ ▸ Calculator                   │  ← results/0
//!                        │   KCalc                        │     results/1
//!                        │   Calendar                     │     results/2
//!                        └────────────────────────────────┘
//! ```
//!
//! Like [`nitro-bar`](../nitro_bar/index.html) it is a `nitro-ui` app
//! that happens to connect to `shell.sock`, which is what lets it live on
//! the `Overlay` layer and read the keyboard without taking focus. Three
//! things make it a *launcher* rather than a window with a text field in
//! it, and each is a decision worth stating.
//!
//! # It is never rebuilt, only hidden
//!
//! The window, the text field and the result rows are built **once**, at
//! start-up, and showing or hiding the launcher is one `SetVisible` on
//! the window. That is not a micro-optimisation: a launcher that opened a
//! window on the Super tap would pay a `CreateWindow`, a round trip per
//! string it measures and a first paint *while the user is already
//! typing*, and the visible result is a launcher that misses the first
//! keystroke. Hidden is also how the server's own rules want it — a
//! window that stops **showing** drops its keyboard grab and its
//! exclusive zone (`docs/shell.md`), so hiding is a complete release and
//! there is no second message to forget.
//!
//! `showing_and_hiding_is_one_mutation_each` asserts the cost from the
//! outside, by counting mutations.
//!
//! # The trigger is the server's, not a key handler
//!
//! `Super` tapped alone, and `Super+Space`, are both
//! [`bind_key`](nitro_ui::Ui::bind_key) bindings. A bound chord is *not*
//! delivered to the focused client at all, so the launcher's trigger
//! cannot be swallowed by whatever the user is typing into — and the bare
//! tap is a server-side state machine (`docs/shell.md` §The bare-modifier
//! tap) because "Super pressed and released with nothing in between" is
//! defined by what did *not* happen, which a client cannot see.
//!
//! The same binding closes it, which is the reason the grab deliberately
//! does not outrank the bindings: a launcher opened by a tap has to be
//! closable by a second tap *while it holds the keyboard*.
//!
//! # Everything it does is addressable
//!
//! ```console
//! $ hey nitro-launcher set query value calc
//! $ hey nitro-launcher do results/0 click        # launches Calculator
//! $ hey nitro-launcher get results/0 value
//! ```
//!
//! That is the agentic path, and it is the same path a key press takes:
//! `set query value` runs the field's `on_change`, which re-ranks and
//! rewrites the rows, and `do results/0 click` runs the row button's
//! callback, which spawns. Nothing in this crate special-cases being
//! driven from outside, which is the only way a scripted path stays
//! honest.
//!
//! # What it is not
//!
//! No icons (an icon *theme* lookup is a whole feature, and half of one
//! is worse than none), no calculator/web/command modes, no frecency, no
//! history. The README lists each with the reason.

pub mod desktop;
pub mod search;
pub mod spawn;

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::shell::{ShellEvent, Surface, WindowInfo};
use nitro_ui::widgets::{
    Button, Label, TextField, button as button_widget, column, label, scroll, text_field,
};
use nitro_ui::{App, ColorRole, Error, Size, Ui, WidgetId};

use desktop::{Entry, Source};

/// The name the launcher registers under, and so the first argument to
/// `hey`.
pub const APP_NAME: &str = "nitro-launcher";

/// Logical width of the overlay.
pub const WIDTH: f32 = 600.0;

/// Logical height of the overlay.
pub const HEIGHT: f32 = 400.0;

/// How many matches are shown.
///
/// Twenty is the spec's number and a defensible one: past the first
/// handful a user refines the query rather than scrolling, and the rows
/// are real widgets — a cap is what keeps "type one character" from
/// building three hundred buttons on a machine with a full
/// `/usr/share/applications`.
pub const MAX_RESULTS: usize = 20;

/// Hotkey id for the bare-Super tap.
pub const HOTKEY_TAP: u32 = 1;

/// Hotkey id for `Super+Space`.
///
/// A second trigger, because the tap is a gesture and a chord is not:
/// a user whose Super key is also their window-management modifier ends
/// up cancelling the tap constantly, and `Super+Space` always works.
pub const HOTKEY_CHORD: u32 = 2;

/// X11 keysym for `space`, which is what [`nitro_ui::Ui::bind_key`]
/// takes.
///
/// Named here rather than pulled from a keysym crate: this is the only
/// keysym the launcher needs, and the value has been stable since X11R1.
const XK_SPACE: u32 = 0x0020;

/// Font size of the query field and the rows.
const TEXT_SIZE: f32 = 16.0;

/// Padding inside the overlay panel.
const PAD: f32 = 12.0;

/// Gap between the query field and the list.
const GAP: f32 = 8.0;

/// Height of one result row.
const ROW_H: f32 = 28.0;

/// The `hey`-addressable names of the launcher's parts.
pub mod names {
    /// The query text field.
    pub const QUERY: &str = "query";
    /// The scrolling list of matches.
    pub const RESULTS: &str = "results";
    /// The "nothing matched" label.
    pub const EMPTY: &str = "empty";
}

/// The `hey`-addressable name of result row `i`: `results/<i>`.
///
/// Named by **position**, not by the application: a script asks for "the
/// first match", which is what a user pressing Enter gets, and a row
/// named after the program it currently shows would change its path every
/// time the query changed.
#[must_use]
pub fn row_name(i: usize) -> String {
    i.to_string()
}

/// The launcher's state.
pub struct Launcher {
    /// Everything launchable, as the last scan found it.
    entries: Vec<Entry>,
    /// Indices into `entries`, best match first, at most [`MAX_RESULTS`].
    matches: Vec<usize>,
    /// Which match is selected; always a valid index into `matches` when
    /// it is non-empty.
    selected: usize,
    /// The current query, mirrored from the text field so the ranking
    /// does not have to reach into the tree for it.
    query: String,
    /// Whether the overlay is on screen.
    visible: bool,
    /// How many times another window taking focus has hidden the
    /// overlay, for the tests and for `hey`.
    focus_hides: u64,
    /// Which window currently holds focus, so a `WindowInfo` that merely
    /// restates it — a retitle, a state change, an app id — is not
    /// mistaken for a focus change. See [`focus_moved`].
    focused: Option<nitro_ui::shell::WindowRef>,
    /// The directory mtimes the last scan saw; a change means rescan.
    fingerprint: (u64, usize),
    /// Launched processes, so they can be reaped.
    children: spawn::Children,
    /// How many launches have been made, for the tests and for `hey`.
    launches: u64,
    /// The last thing launched, for the tests and for `hey`.
    last_launch: Option<String>,
    /// The last launch failure, shown in place of the list.
    last_error: Option<String>,
    /// How many times the overlay has been shown.
    shows: u64,
    /// Where `.desktop` files are looked for. Overridable for the tests,
    /// which must not depend on what the machine running them has
    /// installed.
    dirs: Vec<std::path::PathBuf>,
    /// Extra entries merged into every scan: the nitro binaries next to
    /// the launcher, so a box with no desktop files still works.
    builtins: Vec<Entry>,
    /// The ids of the tree, filled in by [`build`].
    ids: Option<Ids>,
}

impl Launcher {
    /// A launcher that scans the real search path.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            query: String::new(),
            visible: false,
            focus_hides: 0,
            focused: None,
            fingerprint: (0, 0),
            children: spawn::Children::new(),
            launches: 0,
            last_launch: None,
            last_error: None,
            shows: 0,
            dirs: desktop::search_dirs(),
            builtins: builtins(),
            ids: None,
        }
    }

    /// Look for `.desktop` files in `dirs` instead of the real search
    /// path.
    ///
    /// For the tests, and consumed **before** the tree is built: the
    /// first scan happens as the tree is built, so a knob turned
    /// afterwards would not be read until the next rescan.
    #[must_use]
    pub fn with_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.dirs = dirs;
        self
    }

    /// Replace the built-in entries (the nitro binaries next to the
    /// launcher). For the tests: `current_exe` in a test binary is the
    /// test harness, whose siblings are whatever cargo last built.
    #[must_use]
    pub fn with_builtins(mut self, builtins: Vec<Entry>) -> Self {
        self.builtins = builtins;
        self
    }

    /// Whether the overlay is on screen.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// How many times the overlay has been shown.
    #[must_use]
    pub fn shows(&self) -> u64 {
        self.shows
    }

    /// How many times another window taking focus has hidden it.
    #[must_use]
    pub fn focus_hides(&self) -> u64 {
        self.focus_hides
    }

    /// Which window the launcher believes holds focus. For the tests.
    #[must_use]
    pub fn focused_window(&self) -> Option<nitro_ui::shell::WindowRef> {
        self.focused
    }

    /// Everything the launcher could launch, in scan order.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The names of the current matches, best first.
    #[must_use]
    pub fn match_names(&self) -> Vec<String> {
        self.matches
            .iter()
            .filter_map(|i| self.entries.get(*i))
            .map(|e| e.name.clone())
            .collect()
    }

    /// How many matches the query produced.
    #[must_use]
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Which match is selected.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The current query.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// How many launches have been made.
    #[must_use]
    pub fn launches(&self) -> u64 {
        self.launches
    }

    /// The program of the last successful launch.
    #[must_use]
    pub fn last_launch(&self) -> Option<&str> {
        self.last_launch.as_deref()
    }

    /// Why the last launch failed, if it did.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Re-read the search path, merging in the built-ins.
    ///
    /// The built-ins come **first** so a real `.desktop` file for the
    /// same program replaces them: a packaged `nitro-calc.desktop` has a
    /// better name and a better command than "the binary is called
    /// nitro-calc", and the fallback should lose to it rather than
    /// duplicate it.
    pub fn rescan(&mut self) {
        let scanned = desktop::scan(&self.dirs);
        // A built-in is dropped when a real `.desktop` file names the
        // same program: a packaged entry has a better name and a better
        // command than "the binary is called nitro-calc", and the
        // fallback should lose to it rather than appear beside it.
        //
        // The comparison is on the program's **file name**, not its full
        // path — a built-in names an absolute path in the launcher's own
        // directory and a packaged entry names one in `/usr/bin`, so
        // comparing the strings would never find the duplicate it exists
        // to remove. And it is one-directional: two *scanned* entries
        // that happen to run the same program (a browser and its
        // safe-mode entry, three wrappers around `/bin/sh`) are different
        // applications and both belong in the list. An earlier version
        // deduplicated everything against everything and silently
        // collapsed them, which the harness caught as "an empty query
        // shows one of three applications".
        let mut entries: Vec<Entry> = self
            .builtins
            .iter()
            .filter(|b| {
                !scanned
                    .iter()
                    .any(|e| program_file_name(e) == program_file_name(b))
            })
            .cloned()
            .collect();
        entries.extend(scanned);
        entries.sort_by_key(|e| e.name.to_lowercase());
        self.entries = entries;
        self.fingerprint = desktop::fingerprint(&self.dirs);
    }

    /// Rescan only if a search directory changed since the last one.
    ///
    /// Called on every show: an application installed since login must
    /// appear without restarting the launcher, and re-reading a few
    /// hundred files on every *keystroke* is the cost this avoids. See
    /// [`desktop::fingerprint`] for what the check does and does not
    /// catch.
    pub fn rescan_if_stale(&mut self) {
        if self.entries.is_empty() || desktop::fingerprint(&self.dirs) != self.fingerprint {
            self.rescan();
        }
    }

    /// Re-rank the entries against the current query.
    fn rerank(&mut self) {
        self.matches = search::rank(
            self.entries.iter().map(|e| e.name.as_str()),
            &self.query,
            MAX_RESULTS,
        );
        self.selected = 0;
    }
}

impl Default for Launcher {
    fn default() -> Self {
        Self::new()
    }
}

/// The file name of an entry's program, for the duplicate check in
/// [`Launcher::rescan`].
fn program_file_name(entry: &Entry) -> &str {
    let p = entry.program();
    p.rsplit('/').next().unwrap_or(p)
}

/// The nitro binaries that sit next to the launcher.
///
/// A box with no `.desktop` files at all — a freshly rsynced
/// `~/nitro-bin` — still has something to launch, which is what makes the
/// hardware test possible at all. The list is fixed rather than a
/// directory scan: `~/nitro-bin` also holds `nitro-server`, `hey` and
/// `shell_probe`, and offering the user a button that starts a second
/// compositor would be worse than offering nothing.
///
/// The third column is a **symbolic** icon name, not a theme one, and
/// that is the point: a built-in exists precisely on the box that has no
/// icon theme installed, so its icon has to come from the set compiled
/// into the server. The packaged `.desktop` files under `deploy/` name
/// the same shapes, so a box with both looks the same either way.
#[must_use]
pub fn builtins() -> Vec<Entry> {
    const KNOWN: [(&str, &str, &str); 6] = [
        ("nitro-calc", "Calculator", "calculator"),
        ("nitro-term", "Terminal", "terminal"),
        ("nitro-settings", "Settings", "gear"),
        ("nitro-files", "Files", "folder-fill"),
        ("hello_dialog", "Hello Dialog", "window"),
        ("nitro-demo", "Nitro Demo", "palette"),
    ];
    let Some(dir) = spawn::exe_dir() else {
        return Vec::new();
    };
    KNOWN
        .iter()
        .filter(|(bin, _, _)| dir.join(bin).is_file())
        .map(|(bin, name, icon)| Entry {
            name: (*name).to_owned(),
            // The absolute path, not the bare name: `~/nitro-bin` is not
            // on the compositor unit's `PATH`, and a launcher that only
            // worked when its own directory happened to be on `PATH`
            // would fail exactly on the box it was written for.
            argv: vec![dir.join(bin).to_string_lossy().into_owned()],
            terminal: false,
            icon: Some((*icon).to_owned()),
            source: Source::Builtin,
        })
        .collect()
}

/// The launcher's widget ids, gathered by [`build`].
///
/// The `Ids`-and-`install` shape is the bar's and the calculator's: the
/// callbacks need the ids, the state does not exist when the tree is
/// built, so the ids are `Copy` and captured into the closures and
/// written into the state on the first turn of the loop.
#[derive(Debug, Clone, Copy)]
struct Ids {
    query: WidgetId,
    /// The column *inside* the scroller, which is what rows are attached
    /// to. The scroller itself is what `results` names.
    list: WidgetId,
    empty: WidgetId,
}

/// Build the whole tree and return its root.
///
/// Public because the tests build the tree the binary builds: a test that
/// built its own would be testing a second launcher.
///
/// # Panics
/// Never in practice — every `attach` names an id this function has just
/// created, and a fresh id cannot be stale.
pub fn build(ui: &mut Ui<Launcher>) -> WidgetId {
    let query = ui.build(
        text_field("")
            .name(names::QUERY)
            .placeholder("Search applications…")
            .size(TEXT_SIZE)
            .width_percent(1.0)
            .on_change(|s: &mut Launcher, ui: &mut Ui<Launcher>, text: &str| {
                // The same path a `hey set query value` takes, because it
                // *is* that path: the introspection socket routes `set`
                // through the widget's setter, which fires `on_change`.
                text.clone_into(&mut s.query);
                s.rerank();
                refresh(s, ui);
            })
            .on_submit(|s: &mut Launcher, ui: &mut Ui<Launcher>, _t: &str| {
                launch_selected(s, ui);
            }),
    );

    // The rows live in a column inside a scroller: `Scroll` is a viewport
    // over **one** taller child, so the column is that child and the rows
    // are the column's.
    let list = ui.build(column().gap(2.0).width_percent(1.0));
    let empty = ui.build(
        label("No matches")
            .name(names::EMPTY)
            .size(TEXT_SIZE)
            .color_role(ColorRole::TextDim),
    );
    let results = ui.build(scroll().name(names::RESULTS).grow(1.0).width_percent(1.0));
    ui.attach(results, list).unwrap();

    let root = ui.build(column().gap(GAP).padding(PAD).width(WIDTH).height(HEIGHT));
    for child in [query, empty, results] {
        ui.attach(root, child).unwrap();
    }

    install(ui, Ids { query, list, empty });
    root
}

/// Wire the tree up: stash the ids, bind the triggers, do the first scan
/// and hide the window.
fn install(ui: &mut Ui<Launcher>, ids: Ids) {
    // The triggers. Checked rather than assumed: a shell op on an
    // unprivileged connection is a *fatal* protocol error, so a launcher
    // that binds blindly on the wire socket is disconnected rather than
    // merely unbound.
    if ui.is_shell() {
        for (id, mods, keysym) in [
            (HOTKEY_TAP, nitro_ui::shell::mod_mask::SUPER, 0),
            (HOTKEY_CHORD, nitro_ui::shell::mod_mask::SUPER, XK_SPACE),
        ] {
            if let Err(e) = ui.bind_key(id, mods, keysym) {
                // Not fatal: a launcher with one trigger is still a
                // launcher, and `hey` can open it regardless.
                eprintln!("nitro-launcher: bind {id}: {e}");
            }
        }
    }

    ui.on_shell(
        move |s: &mut Launcher, ui: &mut Ui<Launcher>, ev: &ShellEvent| match ev {
            // The tap arrives **once**, on the release, with
            // `pressed: false` — there is no press to report, because
            // until the release the server cannot know it was a tap
            // rather than the start of a chord. A chord arrives twice, so
            // the release is ignored or the launcher would toggle twice
            // per press.
            ShellEvent::HotKey { id, pressed } if *id == HOTKEY_TAP && !*pressed => {
                toggle(s, ui);
            }
            ShellEvent::HotKey { id, pressed } if *id == HOTKEY_CHORD && *pressed => {
                toggle(s, ui);
            }
            // Focus loss, as near as a `NO_FOCUS` overlay can observe it.
            // See [`focus_moved`] for why it is somebody *else* taking
            // focus rather than this window losing it.
            ShellEvent::Window(info) if info.focused => focus_moved(s, ui, info),
            ShellEvent::WindowGone(w) => focus_gone(s, *w),
            _ => {}
        },
    );

    // The window list, which is what makes the rule above possible. It is
    // a subscription, not a poll: asking once is the only request the
    // launcher ever makes about windows, and everything after it arrives
    // unasked. A launcher on an unprivileged connection would be
    // *disconnected* for sending this, so the capability is checked
    // rather than assumed.
    if ui.is_shell()
        && let Err(e) = ui.window_list()
    {
        // Not fatal: a launcher that cannot watch the window list still
        // opens, searches and launches — it just keeps the overlay up
        // until Escape or a second tap. Dying here would trade a small
        // missing behaviour for no launcher at all.
        eprintln!("nitro-launcher: window list: {e}");
    }

    // Escape hides. An app-level handler rather than a widget's, because
    // the focused widget is the text field and a field that consumed
    // Escape would swallow it; `on_key` is offered exactly the presses no
    // widget took, which is the definition of what this wants.
    ui.on_key(
        move |s: &mut Launcher, ui: &mut Ui<Launcher>, k: &nitro_ui::KeyEvent| match k.keycode {
            nitro_ui::event::key::ESC => {
                hide(s, ui);
                nitro_ui::Handled::Yes
            }
            nitro_ui::event::key::UP => {
                move_selection(s, ui, -1);
                nitro_ui::Handled::Yes
            }
            nitro_ui::event::key::DOWN => {
                move_selection(s, ui, 1);
                nitro_ui::Handled::Yes
            }
            _ => nitro_ui::Handled::No,
        },
    );

    // The first scan and the first ranking, so the tree is complete
    // before the window is ever shown: the whole point of building once
    // is that the first Super tap does no work beyond one mutation.
    //
    // A zero-delay timer rather than a direct call, because the state is
    // not reachable from here — `install` runs inside `build`, which has
    // only the tree.
    ui.set_timer(0, move |s: &mut Launcher, ui: &mut Ui<Launcher>| {
        s.ids = Some(ids);
        s.rescan();
        s.rerank();
        refresh(s, ui);
        // Hidden until something asks for it. The window is created
        // visible (there is no flag for "create hidden"), so this is the
        // one frame in which it could be seen — which is why it happens
        // on the loop's first turn, before anything is presented.
        hide(s, ui);
    });
}

/// Show the launcher if it is hidden, hide it if it is shown.
pub fn toggle(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    if s.visible {
        hide(s, ui);
    } else {
        show(s, ui);
    }
}

/// Somebody else's window took focus: get out of the way.
///
/// This is the spec's "focus-loss hides it", and it has to be written
/// backwards because a `NO_FOCUS` overlay **cannot lose focus** — it never
/// had any. There is no `Focus { focused: false }` coming for this
/// window, and waiting for one is how the first version of this ended up
/// holding the keyboard grab until Escape.
///
/// So the observable event is somebody *else* gaining focus. The server
/// reports that as a `WindowInfo` with `focused: true` — but it reports a
/// `WindowInfo` whenever **anything** about a window changes, with
/// `focused` carrying the *current truth* rather than a transition.
/// `clients.rs` relists a window on `SetWindowTitle` and `SetAppId`,
/// `announce_state` on a minimize or maximize, and placement on a new
/// window. So the already-focused window merely changing its title also
/// arrives here saying `focused: true`.
///
/// Acting on that is a launcher that vanishes mid-word for no visible
/// reason, and it is not an exotic case: a shell sets its terminal's
/// title on every prompt, a browser on every page load, a clock-in-title
/// app on a timer. **So the trigger is a change of
/// [`WindowRef`](nitro_ui::shell::WindowRef) identity**, not the flag —
/// `the_focused_window_retitling_itself_is_not_a_focus_change` is the
/// regression.
///
/// Two more windows are ignored:
///
/// * **our own**, because the server may report the overlay itself and
///   hiding on that would close the launcher the moment it opened;
/// * anything while we are already hidden, which is every ordinary focus
///   change on the desktop and must cost nothing.
///
/// The bookkeeping happens **before** both of those returns, and that
/// ordering is load-bearing: a focus change observed while hidden still
/// has to be recorded, or the first change after the next show would be
/// compared against a stale value and be missed.
///
/// The launcher is opened by a hotkey rather than by a click, so this
/// does not race its own opening: the tap does not move focus, and the
/// window that had focus before the tap still has it afterwards.
fn focus_moved(s: &mut Launcher, ui: &mut Ui<Launcher>, info: &WindowInfo) {
    if s.focused == Some(info.window) {
        // The same window, saying something else about itself changed.
        return;
    }
    s.focused = Some(info.window);
    if !s.visible || info.app_id == APP_NAME {
        return;
    }
    s.focus_hides += 1;
    hide(s, ui);
}

/// A window is gone: forget it if it was the focused one.
///
/// Belt and braces rather than a fix for anything observable — a
/// `WindowRef` is retired and never reused (`docs/shell.md`), so a stale
/// one cannot come back and match. But "the focused window closed" is a
/// real state, and leaving the id behind would mean the launcher
/// believed something dead still held focus.
fn focus_gone(s: &mut Launcher, window: nitro_ui::shell::WindowRef) {
    if s.focused == Some(window) {
        s.focused = None;
    }
}

/// Put the overlay on screen: rescan if anything changed, clear the
/// query, take the keyboard, and make the window visible.
///
/// The order matters. The query is cleared and the list re-ranked
/// *before* the window is shown, so the user never sees the previous
/// search for a frame; and the grab rides the same commit as the
/// `SetVisible`, because the server drops a grab on a window that is not
/// showing — asking for one in an earlier transaction would be asking for
/// something that is immediately taken away.
pub fn show(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    let Some(ids) = s.ids else { return };
    s.rescan_if_stale();
    s.query.clear();
    s.last_error = None;
    if let Ok(mut f) = ui.widget_mut::<TextField<Launcher>>(ids.query) {
        f.set_text("");
    }
    s.rerank();
    refresh(s, ui);
    ui.focus(ids.query);
    s.visible = true;
    s.shows += 1;
    let _ = ui.set_window_visible(true);
    // `NO_FOCUS` means the server will never give this window keyboard
    // focus, so the grab is the *only* way keys arrive. It is not an
    // optimisation and there is no fallback path.
    if ui.is_shell() {
        let _ = ui.grab_keyboard(true);
    }
}

/// Take the overlay off screen.
///
/// One `SetVisible`, and the grab and the window state go with it: the
/// server releases a grab whose window stops showing, so there is no
/// second message and nothing to forget. The tree is left exactly as it
/// is — the next show is one mutation, not a rebuild.
///
/// **Unconditional**, and that is the whole of issue "launcher on screen
/// at boot" (found on the box during the M3-E acceptance run). The
/// obvious early return — `if !s.visible { return }` — is wrong for
/// exactly one caller, and it is the most important one: the start-up
/// hide in [`install`]. A window is created **visible** (the protocol has
/// no "create hidden" flag), so at that moment `s.visible` is `false`
/// while the window is on screen: the bool and the server disagree, and
/// the early return resolved the disagreement in favour of the bool. The
/// launcher then sat over the wallpaper for the whole session, believing
/// it was hidden, and every test passed because they all asked the bool.
///
/// Sending the mutation unconditionally costs a `SetVisible` on a hide
/// that was already a hide — 4 bytes, once, on a path a user reaches by
/// pressing Escape at an already-closed launcher. Tracking "what does the
/// server think?" precisely enough to skip it would be a second copy of
/// a fact the server already owns, and this is what the second copy
/// drifting looks like.
pub fn hide(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    s.visible = false;
    let _ = ui.set_window_visible(false);
}

/// Move the selection by `delta`, wrapping.
///
/// Wrapping rather than clamping because the list is short and a user
/// pressing ↑ at the top means "the last one" far more often than they
/// mean "do nothing".
pub fn move_selection(s: &mut Launcher, ui: &mut Ui<Launcher>, delta: isize) {
    if s.matches.is_empty() {
        return;
    }
    let n = s.matches.len().cast_signed();
    let next = (s.selected.cast_signed() + delta)
        .rem_euclid(n)
        .cast_unsigned();
    if next == s.selected {
        return;
    }
    s.selected = next;
    restyle_rows(s, ui);
}

/// Launch whatever is selected, and hide.
fn launch_selected(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    let Some(index) = s.matches.get(s.selected).copied() else {
        return;
    };
    launch_index(s, ui, index);
}

/// Launch `entries[index]`, and hide on success.
///
/// The launcher hides **before** the spawn is attempted so the user sees
/// it go immediately, and comes back on failure with the reason in place
/// of the list. A launcher that stayed up while a program started would
/// cover the window it just opened.
fn launch_index(s: &mut Launcher, ui: &mut Ui<Launcher>, index: usize) {
    let Some(entry) = s.entries.get(index).cloned() else {
        return;
    };
    if entry.terminal {
        // Deliberately refused rather than run: with no terminal
        // emulator, spawning `htop` with its stdio on /dev/null produces
        // a process the user can neither see nor type at, which looks
        // exactly like a launcher that did nothing. Saying so is the
        // honest failure.
        s.last_error = Some(format!("{}: needs a terminal", entry.name));
        show_error(s, ui);
        return;
    }
    hide(s, ui);
    match s.children.spawn(&entry.argv) {
        Ok(_) => {
            // The child's pidfd joins the loop, so its exit is reaped the
            // moment it happens rather than at the next launch.
            s.children.watch(ui, |s: &mut Launcher| &mut s.children);
            s.launches += 1;
            s.last_launch = Some(entry.program().to_owned());
            s.last_error = None;
        }
        Err(e) => {
            // Back on screen *first*, and the reason set afterwards:
            // `show` clears `last_error`, because a launcher reopened by
            // a fresh tap should not still be showing the last failure.
            // Setting it before the show would therefore wipe exactly the
            // message this branch exists to display.
            show(s, ui);
            s.last_error = Some(format!("{}: {e}", entry.name));
            show_error(s, ui);
        }
    }
}

/// Put the last error in the "no matches" label, where the eye already is.
fn show_error(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    let Some(ids) = s.ids else { return };
    let text = s.last_error.clone().unwrap_or_default();
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.empty) {
        l.set_text(text);
    }
}

/// Rebuild the result rows from the current matches.
///
/// Rows are **reused**: the i-th row is the i-th row whatever it shows,
/// so a query that narrows from five matches to four costs four
/// `SetText`s and one `DestroyNode` rather than nine of each. That is the
/// same reasoning as the bar's window list, and the reason the rows are
/// named by position.
fn refresh(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    let Some(ids) = s.ids else { return };
    let wanted: Vec<(usize, String)> = s
        .matches
        .iter()
        .filter_map(|i| s.entries.get(*i).map(|e| (*i, row_text(e))))
        .collect();
    let rows = ui.children(ids.list);

    for (n, (index, text)) in wanted.iter().enumerate() {
        // Decorated **here**, rather than written bare and then
        // overwritten by a `restyle_rows` pass immediately afterwards.
        //
        // What that costs is worth being exact about, because the
        // obvious claim is wrong: it is **not** a wire saving. Two
        // `set_text` calls on one widget between flushes produce **one**
        // `SetText`, because the paint slot caches the last value sent
        // and only the final value is ever painted (`docs/ui.md`). The
        // double write cost a redundant `String`, a redundant
        // layout+paint mark and a second walk of every row, and zero
        // extra bytes. Writing it once is simply the honest shape of
        // "the row's label is its name plus its marker".
        //
        // `rerank` resets the selection to 0, so the row selected here is
        // the one being written rather than a stale index.
        let text = decorate(text, n == s.selected);
        let index = *index;
        if let Some(id) = rows.get(n).copied() {
            if let Ok(mut b) = ui.widget_mut::<Button<Launcher>>(id) {
                b.set_text(text);
                // The callback has to be replaced too: row 0 shows a
                // different application after every keystroke, and a
                // button whose label moved but whose callback did not is
                // the worst bug a launcher can have.
                b.set_on_click(move |s: &mut Launcher, ui: &mut Ui<Launcher>| {
                    launch_index(s, ui, index);
                });
            }
            continue;
        }
        let id = ui.build(
            button_widget(text)
                .name(row_name(n))
                .size(TEXT_SIZE)
                .height(ROW_H)
                .width_percent(1.0)
                .on_click(move |s: &mut Launcher, ui: &mut Ui<Launcher>| {
                    launch_index(s, ui, index);
                }),
        );
        if ui.attach(ids.list, id).is_err() {
            return;
        }
    }
    // Anything left over is gone: one `DestroyNode` on each row's group.
    for id in rows.into_iter().skip(wanted.len()) {
        let _ = ui.remove(id);
    }

    // The "no matches" label doubles as the error line, so it says
    // whichever is true. An empty string is an empty widget, not a blank
    // row: `Label`'s setter returns early on an unchanged string, so a
    // keystroke that changes neither costs nothing.
    let note = if let Some(e) = &s.last_error {
        e.clone()
    } else if s.matches.is_empty() {
        if s.query.trim().is_empty() {
            "No applications found".to_owned()
        } else {
            format!("No matches for “{}”", s.query.trim())
        }
    } else {
        String::new()
    };
    if let Ok(mut l) = ui.widget_mut::<Label>(ids.empty) {
        l.set_text(note);
    }
    // Not `restyle_rows` here: every row above was written already
    // decorated, so a second pass would re-mark each one to produce the
    // string it already has. It stays the *selection-move* path's job,
    // where nothing else has rewritten the labels.
}

/// Mark the selected row, and only that one.
fn restyle_rows(s: &mut Launcher, ui: &mut Ui<Launcher>) {
    let Some(ids) = s.ids else { return };
    for (n, id) in ui.children(ids.list).into_iter().enumerate() {
        let Some(index) = s.matches.get(n).copied() else {
            continue;
        };
        let Some(entry) = s.entries.get(index) else {
            continue;
        };
        let text = decorate(&row_text(entry), n == s.selected);
        if let Ok(mut b) = ui.widget_mut::<Button<Launcher>>(id) {
            // One setter, and it returns early on an unchanged string, so
            // moving the selection costs exactly two `SetText`s — the row
            // that lost the marker and the one that gained it.
            b.set_text(text);
        }
    }
}

/// What a row shows: the name, and a marker for an entry that cannot be
/// run.
#[must_use]
pub fn row_text(entry: &Entry) -> String {
    if entry.terminal {
        format!("{} (terminal)", entry.name)
    } else {
        entry.name.clone()
    }
}

/// The selection marker, in the label rather than in a colour.
///
/// The same reasoning as the bar's focus marker: it is what a script can
/// read, since `hey` prints a button's label as its value, and it works
/// on a server with no fonts where a colour change would be invisible.
#[must_use]
pub fn decorate(text: &str, selected: bool) -> String {
    if selected {
        format!("▸ {text}")
    } else {
        format!("  {text}")
    }
}

/// Connect, open the overlay and run the loop.
///
/// # Errors
/// Any connection, wire or `epoll` failure. They are all fatal — and a
/// failure to reach the **shell** socket is the loudest of them: a
/// launcher that fell back to the ordinary socket would come up looking
/// right, bind no hotkey, take no grab, and be killed by the first shell
/// op it sent.
pub fn run() -> Result<(), Error> {
    App::shell(APP_NAME)?
        .title("nitro-launcher")
        .surface(Surface::overlay())
        .size(Size::new(WIDTH, HEIGHT))
        .run(Launcher::new(), build)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_is_named_by_its_position() {
        // Named by position rather than by application: a script asks for
        // "the first match", and a row named after what it happens to
        // show would change its path on every keystroke.
        assert_eq!(row_name(0), "0");
        assert_eq!(row_name(19), "19");
    }

    #[test]
    fn the_selection_marker_keeps_the_name_readable() {
        assert_eq!(decorate("Calculator", true), "▸ Calculator");
        assert_eq!(decorate("Calculator", false), "  Calculator");
        // Both forms contain the name, which is what a `hey get … value`
        // assertion and a user's eye both rely on.
        for selected in [true, false] {
            assert!(decorate("Calculator", selected).contains("Calculator"));
        }
    }

    #[test]
    fn a_terminal_entry_says_so_in_its_row() {
        let e = Entry {
            name: "htop".to_owned(),
            argv: vec!["htop".to_owned()],
            terminal: true,
            icon: None,
            source: Source::Builtin,
        };
        assert_eq!(row_text(&e), "htop (terminal)");
    }

    #[test]
    fn a_desktop_file_replaces_the_builtin_for_the_same_program() {
        // The fallback should lose to a real entry rather than duplicate
        // it: a packaged `.desktop` has a better name and a better
        // command than "the binary is called nitro-calc".
        let dir = std::env::temp_dir().join(format!("nitro-launcher-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("calc.desktop"),
            "[Desktop Entry]\nName=Proper Calculator\nExec=/usr/bin/nitro-calc --scientific\n",
        )
        .unwrap();

        let mut l = Launcher::new()
            .with_dirs(vec![dir.clone()])
            .with_builtins(vec![Entry {
                name: "Calculator".to_owned(),
                argv: vec!["/opt/nitro-calc".to_owned()],
                terminal: false,
                icon: Some("calculator".to_owned()),
                source: Source::Builtin,
            }]);
        l.rescan();
        let names: Vec<&str> = l.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Proper Calculator"],
            "one entry, and it is the packaged one"
        );
        assert_eq!(
            l.entries()[0].source,
            Source::Desktop(dir.join("calc.desktop"))
        );
        assert_eq!(
            l.entries()[0].argv,
            vec!["/usr/bin/nitro-calc".to_owned(), "--scientific".to_owned()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_desktop_entries_running_the_same_program_are_both_listed() {
        // A browser and its safe-mode entry, or three wrappers around
        // `/bin/sh`, are different applications. An earlier version of
        // `rescan` deduplicated every entry against every other by
        // program name and silently collapsed them into one.
        let dir = std::env::temp_dir().join(format!("nitro-launcher-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, name) in [("a.desktop", "Alpha"), ("b.desktop", "Beta")] {
            std::fs::write(
                dir.join(file),
                format!("[Desktop Entry]\nName={name}\nExec=/bin/true\n"),
            )
            .unwrap();
        }
        let mut l = Launcher::new()
            .with_dirs(vec![dir.clone()])
            .with_builtins(Vec::new());
        l.rescan();
        let names: Vec<&str> = l.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "Beta"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_builtin_with_no_desktop_file_survives_the_scan() {
        // The test box's whole case: no `.desktop` files anywhere.
        let dir = std::env::temp_dir().join(format!("nitro-launcher-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut l = Launcher::new()
            .with_dirs(vec![dir])
            .with_builtins(vec![Entry {
                name: "Calculator".to_owned(),
                argv: vec!["/opt/nitro-calc".to_owned()],
                terminal: false,
                icon: Some("calculator".to_owned()),
                source: Source::Builtin,
            }]);
        l.rescan();
        assert_eq!(l.entries().len(), 1);
        assert_eq!(l.entries()[0].source, Source::Builtin);
    }
}
