//! `nitro-settings` — displays, keyboard and audio, in one window.
//!
//! It is an ordinary decorated `nitro-ui` app: a state struct, a tree
//! built once, a callback per widget. What makes it worth reading is that
//! it is the first app that **writes** something the compositor reads
//! back — `server.conf` — and so the first that has to answer "did that
//! work?" honestly.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ Displays                                             │
//! │ HDMI-A-1 1920×1080 @ 60 Hz [==o===] 2 ☑ primary 0   0 │  one row per output
//! │ VGA-1    1280×1024 @ 60 Hz [o=====] 1 ☐ primary 1920 0│
//! │ Positions are typed; drag-arrange is not in M4.       │
//! │ ────────────────────────────────────────────────────  │
//! │ Keyboard                                             │
//! │ Layout [de    ] Variant [      ] Options [ctrl:nocaps]│
//! │ Test here [                                         ] │
//! │ ────────────────────────────────────────────────────  │
//! │ Audio                                                │
//! │ Volume [======o===] 65 %  ☐ mute                      │
//! │ via wpctl                                            │
//! │ ────────────────────────────────────────────────────  │
//! │ [Apply] [Revert]                            applied   │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! # Driving it with `hey`
//!
//! Every widget carries a `.name()`, so the whole dialog is drivable from
//! a shell with no cooperation from this code. A display row is named by
//! its **connector** — the name the server, the file and the monitor's
//! own EDID all use — and its controls hang under it:
//!
//! ```text
//! hey nitro-settings list                                 # the whole tree
//! hey nitro-settings do displays/HDMI-A-1/primary click    # make it primary
//! hey nitro-settings set displays/HDMI-A-1/scale value 2
//! hey nitro-settings set displays/HDMI-A-1/x value 0
//! hey nitro-settings get displays/HDMI-A-1/scale_value value   # 2
//! hey nitro-settings set keyboard/layout value de
//! hey nitro-settings do apply click
//! hey nitro-settings get status value                      # applied
//! ```
//!
//! The names are published as [`names`] constants, the way `nitro-bar`
//! publishes its sections, so a script, a test and the tree cannot drift
//! apart.
//!
//! # Where the output list comes from
//!
//! From the **shell socket**. [`App::shell`] connects to `shell.sock`,
//! [`Ui::outputs`] subscribes, and each output arrives as a
//! `ShellEvent::Output`. That is a subscription, not a poll: a monitor
//! plugged in while the window is open grows a row, and nothing wakes up
//! in between.
//!
//! Note what this app does *not* do with that connection — it never calls
//! `App::surface`, so the window is an ordinary decorated one. A shell
//! connection with no shell surface is exactly that: the socket grants
//! the capability, and a `CreateWindow` on the `Normal` layer with no
//! flags is the same window `App::new` would have opened. The privilege
//! buys one thing here, the output list, and costs nothing else. (It was
//! checked rather than assumed — `a_shell_connection_still_opens_an_ordinary_window`
//! in `tests/settings.rs` asserts the window is decorated and focusable.)
//!
//! When the shell socket is not there at all — an older server, or a
//! client started outside the session — [`run`] falls back to
//! [`App::new`] and the rows are built from `server.conf` alone. That is
//! a genuinely poorer dialog (no resolutions, and no output the file has
//! never heard of), so it **says so in a label**,
//! [`names::DISPLAYS_NOTE`], rather than quietly showing less.
//!
//! # The file is rewritten wholesale
//!
//! Apply renders the whole of `server.conf` from the widgets and renames
//! it into place ([`conf::write`]). Comments somebody typed, and keys
//! this app has never heard of, are **lost**. That is the one real
//! limitation of the design, it is in the README too, and the rule it
//! implies is: edit the file or use the app, not both.
//!
//! What it does *not* do is write down opinions nobody expressed. A
//! connector the file said nothing about gets no `scale` line unless its
//! slider actually moves — see `Row::seeded_scale`: the slider is seeded
//! from the live scale, and writing that back would pin today's EDID
//! answer into the file and make a `NITRO_SCALE` dev override permanent.
//!
//! # What "applied" means
//!
//! Validation belongs to the compositor — it owns xkbcommon and it owns
//! the scale bounds — so this app does not second-guess it. Apply writes
//! the file and then *asks*: the server's control socket publishes a
//! `config_reloads` counter, and the counter moving is the server saying
//! "I read it and took it". It not moving within
//! [`control::RELOAD_WAIT`] is the server saying nothing, and the status
//! line then points at the log rather than inventing a reason.
//!
//! # Audio is a remote control, not a mixer
//!
//! There is no audio in nitro and there is not going to be. The audio
//! section shells out to `wpctl`, falls back to `pactl`, and says
//! "no audio backend found" when neither is installed ([`audio`]). It
//! reads the volume **once**, when the window opens, and writes when the
//! user moves the slider: no daemon, no D-Bus, no polling timer. A volume
//! changed by a media key while the window is open is stale until Revert,
//! which is what the idle contract costs and is worth saying out loud.

pub mod audio;
pub mod conf;
pub mod control;

use std::path::PathBuf;
use std::time::Duration;

use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::shell::{Layer, OutputInfo, ShellEvent, Surface};
use nitro_ui::widgets::{
    Checkbox, FlexBuilder, Label, LabelBuilder, Slider, TextField, TextFieldBuilder, button,
    checkbox, column, label, row, separator, slider, spacer, text_field,
};
use nitro_ui::{App, Color, CrossAlign, Error, Size, Ui, WidgetId};

use audio::Backend;
use conf::{Conf, KeyboardConf};

/// The name the app registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-settings";

/// The window's size when it opens.
///
/// Explicit, unlike `nitro-calc`'s measured window: the display section's
/// height depends on how many monitors are plugged in, so a window sized
/// to its tree would open differently on every machine — and resize
/// itself when one was unplugged, which is the one thing a settings
/// dialog must not do while you are using it.
pub const WINDOW_SIZE: Size = Size::new(440.0, 320.0);

/// The "surface" an **ordinary** window is.
///
/// A shell connection does not have to open a shell surface, and this app
/// does not: `Normal` layer, no flags, no anchor and no zone is precisely
/// the window [`App::new`] opens, which is why [`run`] simply omits
/// `App::surface`. The constant exists for the test harness, whose
/// `Harness::shell` requires a `Surface` because a bar always has one —
/// so this is how a test says "a shell connection, an ordinary window",
/// which is the combination the binary actually ships.
pub const ORDINARY_WINDOW: Surface = Surface {
    layer: Layer::Normal,
    flags: 0,
    anchor: None,
    zone: None,
};

/// Smallest scale the slider offers.
const SCALE_MIN: f32 = 1.0;

/// Largest scale the slider offers.
///
/// The *file* allows 0.5..=8 and the server enforces that; the slider
/// offers 1..3, because that is the range of panels that exist and a knob
/// whose useful travel is its leftmost eighth is a worse control than one
/// that cannot express a scale nobody has. A file outside the range is
/// clamped into it when it is loaded — visibly, and reversibly by not
/// pressing Apply — because the alternative is a widget that lies about
/// what Apply is going to write.
const SCALE_MAX: f32 = 3.0;

/// Slider step, and so the set of scales this app can write.
const SCALE_STEP: f32 = 0.25;

/// Volume slider step: 5 %, which is what one press of a volume key is
/// worth on most keyboards.
const VOLUME_STEP: f32 = 0.05;

/// Font size of a section heading.
const HEADING_SIZE: f32 = 15.0;
/// Font size of everything else.
const TEXT_SIZE: f32 = 13.0;
/// Gap between rows, and between the widgets inside one.
const GAP: f32 = 6.0;
/// Padding around the whole window.
const PAD: f32 = 10.0;
/// Height of one control row.
const ROW_HEIGHT: f32 = 26.0;
/// Width of a position field: five digits and a minus sign.
const POS_WIDTH: f32 = 54.0;

/// What the displays note says when the shell socket gave us the list.
const NOTE_LIVE: &str = "Positions are typed, in desktop pixels — drag-arrange is not in M4.";

/// What it says when it did not.
///
/// Visible rather than merely logged, because the spec requires it and
/// because it is right: a dialog showing two of four monitors with no
/// explanation is worse than one that admits it is working from the file.
const NOTE_NO_SHELL: &str =
    "No shell socket: outputs come from server.conf alone, with no resolutions.";

/// What it says when the server has outputs but told us about none.
const NOTE_NO_OUTPUTS: &str = "The server reported no outputs.";

/// The `hey`-addressable names of every widget in the dialog.
///
/// Published as constants for the reason `nitro-bar` publishes its own:
/// the README documents `hey` commands, the tests assert the same paths,
/// and a rename that broke one would otherwise only be found on the box.
///
/// A display row is addressed `displays/<connector>/<field>`, so the leaf
/// names below repeat once per output — which is exactly what a path
/// syntax is for, and why the row's own name is the connector rather than
/// an index.
pub mod names {
    /// The column holding one row per output.
    pub const DISPLAYS: &str = "displays";
    /// The line under the display rows saying what is and is not possible.
    pub const DISPLAYS_NOTE: &str = "displays_note";
    /// In a display row: the connector name.
    pub const OUTPUT_NAME: &str = "name";
    /// In a display row: the resolution and refresh rate.
    pub const OUTPUT_MODE: &str = "mode";
    /// In a display row: the scale slider.
    pub const SCALE: &str = "scale";
    /// In a display row: the label showing the slider's value.
    pub const SCALE_VALUE: &str = "scale_value";
    /// In a display row: the primary checkbox.
    pub const PRIMARY: &str = "primary";
    /// In a display row: the x position field.
    pub const POS_X: &str = "x";
    /// In a display row: the y position field.
    pub const POS_Y: &str = "y";

    /// The keyboard section's container.
    pub const KEYBOARD: &str = "keyboard";
    /// `keyboard.layout`.
    pub const LAYOUT: &str = "layout";
    /// `keyboard.variant`.
    pub const VARIANT: &str = "variant";
    /// `keyboard.options`.
    pub const OPTIONS: &str = "options";
    /// The scratch field, to type in after Apply and see the new layout.
    pub const TEST: &str = "test";

    /// The audio section's row.
    pub const AUDIO: &str = "audio";
    /// The volume slider.
    pub const VOLUME: &str = "volume";
    /// The label showing the volume as a percentage.
    pub const VOLUME_VALUE: &str = "volume_value";
    /// The mute checkbox.
    pub const MUTE: &str = "mute";
    /// Which backend was found, or that none was.
    pub const AUDIO_STATUS: &str = "audio_status";

    /// The Apply button.
    pub const APPLY: &str = "apply";
    /// The Revert button.
    pub const REVERT: &str = "revert";
    /// The line that says what Apply did.
    pub const STATUS: &str = "status";
}

/// One display row: the connector it is for, and the widgets in it.
///
/// The ids are kept because Apply reads them and Revert writes them. They
/// are gathered as the row is built rather than looked up by name
/// afterwards: addressing is a user-facing convenience, and making the
/// app's own behaviour depend on it would mean a rename could break the
/// dialog rather than just a documented command.
#[derive(Debug, Clone)]
struct Row {
    /// The connector, e.g. `HDMI-A-1`. Also the row's addressing name.
    connector: String,
    /// The server's output id, which is what `OutputGone` names. `None`
    /// for a row built from the file, which no hotplug can retire.
    output: Option<u32>,
    /// The scale this row was seeded with when the file said nothing about
    /// it — i.e. the live scale the server reported, which is the EDID
    /// default or a `NITRO_SCALE` dev override.
    ///
    /// Apply skips the `scale` line for a row still sitting on this value,
    /// so opening the app and pressing Apply does not silently pin a scale
    /// the file never had. Without it an untouched row would freeze
    /// today's EDID answer into the config — so a replaced monitor would
    /// no longer be measured — and, worse, persist a `NITRO_SCALE=…` meant
    /// for one `just fake` run into the user's permanent file.
    ///
    /// `None` when the file *did* carry a scale for this connector: the
    /// user has an explicit value, and it is written back unconditionally.
    seeded_scale: Option<f32>,
    /// The row container, removed when the output goes away.
    container: WidgetId,
    /// The scale slider.
    scale: WidgetId,
    /// The label beside it.
    scale_value: WidgetId,
    /// The primary checkbox.
    primary: WidgetId,
    /// The x position field.
    x: WidgetId,
    /// The y position field.
    y: WidgetId,
}

/// The app's state.
///
/// This is the `S` of `Ui<S>` — a plain struct handed to every callback
/// as `&mut S` alongside `&mut Ui<S>`. No `Rc`, no `RefCell`, no observer
/// list: a callback that has both of those does not need one.
pub struct Settings {
    /// Where `server.conf` is, once [`init`] has resolved it.
    path: Option<PathBuf>,
    /// Where the server's control socket is. Taken from the `Ui` at
    /// start-up, so a test harness's own server is the one asked rather
    /// than whatever `$NITRO_CONTROL` names in the test runner.
    control: Option<PathBuf>,
    /// How long Apply waits for `config_reloads` to move.
    reload_wait: Duration,
    /// Directories the audio backend is looked for in.
    ///
    /// Injected rather than read from `PATH` inside the callback, so a
    /// test can point it at a fake `wpctl` without `std::env::set_var` —
    /// which is `unsafe`, process-global, and would be seen by every
    /// other test running in the same binary. The same reasoning makes
    /// `nitro-bar` inject its sensor source.
    audio_dirs: Vec<PathBuf>,
    /// The backend found there, once [`init`] has looked.
    audio: Option<Backend>,
    /// One per display row, in the order they are drawn.
    rows: Vec<Row>,
    /// Whether the output list came from the shell socket.
    shell: bool,
    /// How many times Apply has run, for the tests and for
    /// `hey nitro-settings get window value`.
    applies: u64,
    /// How many times Revert has run.
    reverts: u64,
    /// What the status line last said, so a test can read the verdict
    /// without going through a widget lookup.
    status: String,
}

impl Settings {
    /// A dialog that has not read anything yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: None,
            control: None,
            reload_wait: control::RELOAD_WAIT,
            audio_dirs: audio::path_dirs(),
            audio: None,
            rows: Vec::new(),
            shell: false,
            applies: 0,
            reverts: 0,
            status: String::new(),
        }
    }

    /// Read and write `server.conf` at `path` rather than wherever the
    /// environment says.
    ///
    /// For the tests, and for `$NITRO_CONFIG` — which is how one dialog
    /// configures a second compositor on the same machine.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Ask this control socket about reloads, rather than the one the
    /// `Ui` names.
    #[must_use]
    pub fn with_control_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.control = Some(path.into());
        self
    }

    /// Wait `wait` for the reload counter instead of
    /// [`control::RELOAD_WAIT`].
    ///
    /// Only the tests shorten it, and only the ones about the *rejected*
    /// verdict: that verdict is defined as "the counter did not move
    /// before the deadline", so the deadline is the test's entire
    /// runtime, and a test suite must not spend seconds proving a
    /// timeout it can prove in milliseconds.
    #[must_use]
    pub fn with_reload_wait(mut self, wait: Duration) -> Self {
        self.reload_wait = wait;
        self
    }

    /// Look for `wpctl`/`pactl` in `dirs` instead of on `PATH`.
    #[must_use]
    pub fn with_audio_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.audio_dirs = dirs;
        self
    }

    /// Which audio backend was found, if any.
    #[must_use]
    pub fn audio(&self) -> Option<&Backend> {
        self.audio.as_ref()
    }

    /// The connectors that have a row, in order.
    #[must_use]
    pub fn connectors(&self) -> Vec<String> {
        self.rows.iter().map(|r| r.connector.clone()).collect()
    }

    /// Whether the output list came from the shell socket.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.shell
    }

    /// How many times Apply has run.
    #[must_use]
    pub fn applies(&self) -> u64 {
        self.applies
    }

    /// How many times Revert has run.
    #[must_use]
    pub fn reverts(&self) -> u64 {
        self.reverts
    }

    /// What the status line last said.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

/// The ids [`build`] gathers and the callbacks need.
///
/// `Copy`, and captured into every closure, exactly as `nitro-calc`'s
/// `Screen` and `nitro-bar`'s `Ids` are: `build` has the tree but not the
/// state, and the callbacks need the ids before the state exists.
#[derive(Debug, Clone, Copy)]
struct Ids {
    displays: WidgetId,
    displays_note: WidgetId,
    layout: WidgetId,
    variant: WidgetId,
    options: WidgetId,
    volume: WidgetId,
    volume_value: WidgetId,
    mute: WidgetId,
    audio_status: WidgetId,
    status: WidgetId,
}

/// Build the whole tree and return its root.
///
/// Public because the tests build the tree the binary builds: a test that
/// built its own would be testing a second dialog.
///
/// # Panics
/// Never in practice — every `attach` names an id this function has just
/// created, and a fresh id cannot be stale.
// One function because it *is* one tree, and the order the widgets are
// created in is the order they are read in. Splitting it into
// `build_displays`/`build_keyboard`/`build_audio` would scatter that
// order over four signatures passing ids between them, to satisfy a line
// count — `nitro-server`'s `config::parse` takes the same allow for the
// same reason.
#[allow(clippy::too_many_lines)]
pub fn build(ui: &mut Ui<Settings>) -> WidgetId {
    let dim = ui.theme().text_disabled;
    let ink = ui.theme().text;

    // -- displays ------------------------------------------------------
    //
    // An empty column: the rows arrive from the shell socket, or from the
    // file, on the first turn of the loop — `build` has no state to put
    // them in and no answer from the connection yet.
    let displays = ui.build(column().name(names::DISPLAYS).gap(GAP).width_percent(1.0));
    let displays_note = ui.build(
        label(NOTE_LIVE)
            .name(names::DISPLAYS_NOTE)
            .size(TEXT_SIZE)
            .color(dim),
    );

    // -- keyboard ------------------------------------------------------
    let layout = ui.build(field(names::LAYOUT, "us").grow(1.0));
    let variant = ui.build(field(names::VARIANT, "nodeadkeys").grow(1.0));
    let options = ui.build(field(names::OPTIONS, "ctrl:nocaps").grow(1.0));
    let test = ui.build(field(names::TEST, "type here after Apply").grow(1.0));
    let keyboard = ui.build(column().name(names::KEYBOARD).gap(GAP).width_percent(1.0));
    let kb_row = ui.build(control_row());
    for (caption_text, id) in [
        ("Layout", layout),
        ("Variant", variant),
        ("Options", options),
    ] {
        let c = ui.build(caption(caption_text, dim));
        ui.attach(kb_row, c).unwrap();
        ui.attach(kb_row, id).unwrap();
    }
    let test_row = ui.build(control_row());
    let test_caption = ui.build(caption("Test here", dim));
    ui.attach(test_row, test_caption).unwrap();
    ui.attach(test_row, test).unwrap();
    ui.attach(keyboard, kb_row).unwrap();
    ui.attach(keyboard, test_row).unwrap();

    // -- audio ---------------------------------------------------------
    //
    // The two labels the audio callbacks write to are built first, so the
    // slider's `on_change` can capture their ids: a builder's callback is
    // installed once, when the widget is made, and there is no setter for
    // a `Slider`'s handler afterwards.
    let volume_value = ui.build(
        label("")
            .name(names::VOLUME_VALUE)
            .size(TEXT_SIZE)
            .width(48.0),
    );
    let audio_status = ui.build(
        label("")
            .name(names::AUDIO_STATUS)
            .size(TEXT_SIZE)
            .color(dim),
    );
    let volume = ui.build(
        slider(0.0)
            .name(names::VOLUME)
            .range(0.0, 1.0)
            .step(VOLUME_STEP)
            .grow(1.0)
            .min_width(80.0)
            .on_change(move |s: &mut Settings, ui: &mut Ui<Settings>, v: f32| {
                // The label first: it is what the user is looking at
                // while dragging, and it has to be right even when the
                // mixer refuses the new value.
                set_label(ui, volume_value, &percent(v));
                if let Some(b) = s.audio.clone()
                    && let Err(e) = b.set_volume(v)
                {
                    set_label(ui, audio_status, &e);
                }
            }),
    );
    let mute = ui.build(checkbox("mute").name(names::MUTE).on_toggle(
        move |s: &mut Settings, ui: &mut Ui<Settings>, on: bool| {
            if let Some(b) = s.audio.clone()
                && let Err(e) = b.set_muted(on)
            {
                set_label(ui, audio_status, &e);
            }
        },
    ));
    let audio = ui.build(control_row().name(names::AUDIO));
    let volume_caption = ui.build(caption("Volume", dim));
    for child in [volume_caption, volume, volume_value, mute] {
        ui.attach(audio, child).unwrap();
    }

    // -- apply / revert ------------------------------------------------
    let status = ui.build(label("").name(names::STATUS).size(TEXT_SIZE).color(dim));
    let ids = Ids {
        displays,
        displays_note,
        layout,
        variant,
        options,
        volume,
        volume_value,
        mute,
        audio_status,
        status,
    };
    let apply_button = ui.build(
        button("Apply")
            .name(names::APPLY)
            .size(TEXT_SIZE)
            .on_click(move |s: &mut Settings, ui: &mut Ui<Settings>| apply(s, ui, ids)),
    );
    let revert_button = ui.build(
        button("Revert")
            .name(names::REVERT)
            .size(TEXT_SIZE)
            .on_click(move |s: &mut Settings, ui: &mut Ui<Settings>| revert(s, ui, ids)),
    );
    let gap = ui.build(spacer().grow(1.0));
    let buttons = ui.build(control_row());
    for child in [apply_button, revert_button, gap, status] {
        ui.attach(buttons, child).unwrap();
    }

    // -- the window ----------------------------------------------------
    let root = ui.build(column().gap(GAP).padding(PAD).width_percent(1.0));
    let h_displays = ui.build(heading("Displays", ink));
    let h_keyboard = ui.build(heading("Keyboard", ink));
    let h_audio = ui.build(heading("Audio", ink));
    let sep_a = ui.build(separator().width_percent(1.0));
    let sep_b = ui.build(separator().width_percent(1.0));
    let sep_c = ui.build(separator().width_percent(1.0));
    for child in [
        h_displays,
        displays,
        displays_note,
        sep_a,
        h_keyboard,
        keyboard,
        sep_b,
        h_audio,
        audio,
        audio_status,
        sep_c,
        buttons,
    ] {
        ui.attach(root, child).unwrap();
    }

    install(ui, ids);
    root
}

/// A row of controls: the shape every labelled line in this dialog has.
fn control_row() -> FlexBuilder<Settings> {
    row()
        .gap(GAP)
        .height(ROW_HEIGHT)
        .width_percent(1.0)
        .cross_align(CrossAlign::Center)
}

/// A section heading.
fn heading(text: &str, color: Color) -> LabelBuilder<Settings> {
    label(text).size(HEADING_SIZE).weight(600).color(color)
}

/// A label in front of a control.
///
/// Deliberately **unnamed**: it is furniture, and naming it would put a
/// second `layout` in the keyboard row for `hey` to be ambiguous about.
fn caption(text: &str, color: Color) -> LabelBuilder<Settings> {
    label(text).size(TEXT_SIZE).color(color)
}

/// A named text field with a placeholder.
fn field(name: &str, placeholder: &str) -> TextFieldBuilder<Settings> {
    text_field("")
        .name(name)
        .placeholder(placeholder)
        .size(TEXT_SIZE)
}

/// Wire the tree up: subscribe to the outputs, and arm the one-shot that
/// reads the file, the outputs and the mixer.
///
/// Everything the dialog *does* is registered here, and none of it is a
/// poll: the outputs are a subscription, the file is read once, and the
/// volume is read once. After the first turn of the loop there is no
/// timer left armed at all, which is what the idle test asserts.
fn install(ui: &mut Ui<Settings>, ids: Ids) {
    ui.on_shell(
        move |s: &mut Settings, ui: &mut Ui<Settings>, ev: &ShellEvent| match ev {
            ShellEvent::Output(info) => upsert_output(s, ui, ids, info),
            ShellEvent::OutputGone(id) => remove_output(s, ui, *id),
            ShellEvent::OutputsEnd => outputs_end(s, ui, ids),
            _ => {}
        },
    );
    // A zero-delay timer rather than a direct call, because the state is
    // not reachable from `build` — which has only the tree. The app loop
    // runs due timers on its first turn, so this happens before the first
    // frame is presented. It is the trick `nitro-bar`'s clock uses.
    ui.set_timer(0, move |s: &mut Settings, ui: &mut Ui<Settings>| {
        init(s, ui, ids);
    });
}

/// Read everything the dialog shows, once.
///
/// Called from a zero-delay timer rather than from [`build`], because the
/// state does not exist while the tree is being built and all three of
/// these readings need it: the config path, the audio search path and the
/// control socket all live in [`Settings`].
fn init(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids) {
    if s.path.is_none() {
        s.path = conf::path();
    }
    if s.control.is_none() {
        s.control = Some(ui.control_path());
    }
    s.audio = Backend::detect_in(&s.audio_dirs);
    load_audio(s, ui, ids);

    let conf = load_conf(s);
    fill_keyboard(ui, ids, &conf.keyboard);

    // Asking for the outputs *subscribes*, so this is the only request
    // the dialog ever makes about them: everything after it arrives
    // unasked. An unprivileged connection would be **disconnected** for
    // sending it, so the capability is checked rather than assumed.
    if ui.is_shell() && ui.outputs().is_ok() {
        s.shell = true;
        return;
    }
    // No shell socket: the file is all there is. Say so, and build a row
    // for every connector it mentions.
    set_label(ui, ids.displays_note, NOTE_NO_SHELL);
    for connector in conf.outputs.keys().cloned().collect::<Vec<_>>() {
        add_row(s, ui, ids, &connector, None, &conf);
    }
}

/// The file as it is on disk right now, or an empty one.
fn load_conf(s: &Settings) -> Conf {
    s.path.as_deref().map_or_else(Conf::new, conf::load)
}

/// Read the mixer and fill the audio section.
fn load_audio(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids) {
    let Some(backend) = s.audio.clone() else {
        // The exact words the spec asks for. A section that cannot work
        // says why, rather than showing a slider that silently does
        // nothing.
        set_label(ui, ids.audio_status, "no audio backend found");
        set_audio_enabled(ui, ids, false);
        return;
    };
    let Some(v) = backend.volume() else {
        set_label(
            ui,
            ids.audio_status,
            &format!("{}: could not read the volume", backend.name()),
        );
        set_audio_enabled(ui, ids, false);
        return;
    };
    set_label(ui, ids.audio_status, &format!("via {}", backend.name()));
    set_audio_enabled(ui, ids, true);
    if let Ok(mut sl) = ui.widget_mut::<Slider<Settings>>(ids.volume) {
        sl.set_value(v.level);
    }
    if let Ok(mut c) = ui.widget_mut::<Checkbox<Settings>>(ids.mute) {
        c.set_checked(v.muted);
    }
    set_label(ui, ids.volume_value, &percent(v.level));
}

/// Enable or disable the two audio controls together.
///
/// Disabled rather than hidden: a control that is there and greyed out
/// says "this machine has no mixer", where one that is absent says
/// "settings has no audio section", and only the first is true.
fn set_audio_enabled(ui: &mut Ui<Settings>, ids: Ids, on: bool) {
    if let Ok(mut sl) = ui.widget_mut::<Slider<Settings>>(ids.volume) {
        sl.set_enabled(on);
    }
    if let Ok(mut c) = ui.widget_mut::<Checkbox<Settings>>(ids.mute) {
        c.set_enabled(on);
    }
}

/// A linear volume as the percentage the label shows.
#[must_use]
pub fn percent(level: f32) -> String {
    format!("{} %", (level.clamp(0.0, 1.0) * 100.0).round() as u32)
}

/// Insert or update one output's row.
///
/// There is no separate "added" path, for the reason `nitro-bar` has
/// none: the server sends the same `OutputInfo` for the snapshot and for
/// every later change, so keying on the connector and upserting is one
/// code path where "added versus changed" would be two that must agree.
fn upsert_output(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids, info: &OutputInfo) {
    if let Some(r) = s.rows.iter_mut().find(|r| r.connector == info.name) {
        r.output = Some(info.id);
        let container = r.container;
        // A mode change is the only thing a re-sent `OutputInfo` may
        // move here. The scale and the position are deliberately *not*
        // re-read: they are what the user is editing, and a hotplug
        // elsewhere on the desktop must not throw away a typed number.
        set_named_label(ui, container, names::OUTPUT_MODE, &mode_text(info));
        return;
    }
    let conf = load_conf(s);
    let connector = info.name.clone();
    add_row(s, ui, ids, &connector, Some(info), &conf);
}

/// The end of the output snapshot: if it was empty, say so.
fn outputs_end(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids) {
    if s.rows.is_empty() {
        set_label(ui, ids.displays_note, NOTE_NO_OUTPUTS);
    }
}

/// Drop an unplugged output's row.
fn remove_output(s: &mut Settings, ui: &mut Ui<Settings>, output: u32) {
    let Some(i) = s.rows.iter().position(|r| r.output == Some(output)) else {
        return;
    };
    let r = s.rows.remove(i);
    // One `DestroyNode` on the row's group frees the subtree
    // server-side; a stale id afterwards is an error, never a panic.
    let _ = ui.remove(r.container);
}

/// Build one display row and record it.
///
/// The values come from the **file** when it mentions this connector and
/// from the live output otherwise, which is the precedence the server
/// itself applies: the file is the user's explicit answer to the EDID's
/// guess.
///
/// # Panics
/// Never in practice — every `attach` names an id created just above.
fn add_row(
    s: &mut Settings,
    ui: &mut Ui<Settings>,
    ids: Ids,
    connector: &str,
    info: Option<&OutputInfo>,
    conf: &Conf,
) {
    let dim = ui.theme().text_disabled;
    let saved = conf.output(connector);
    let scale = saved
        .and_then(|o| o.scale)
        .or_else(|| info.map(|i| i.scale))
        .unwrap_or(1.0)
        .clamp(SCALE_MIN, SCALE_MAX);
    let position = saved.and_then(|o| o.position).or_else(|| {
        // The server reports an output's origin in **device** pixels and
        // the file is in desktop logical ones; they agree at scale 1 and
        // diverge above it, so dividing by the scale is the closest
        // honest conversion a client can make. The file's own value wins
        // whenever there is one, so this only ever seeds a row for an
        // output nobody has configured yet.
        info.map(|i| {
            let s = if i.scale > 0.0 { i.scale } else { 1.0 };
            (
                (i.x as f32 / s).round() as i32,
                (i.y as f32 / s).round() as i32,
            )
        })
    });

    let name_label = ui.build(
        label(connector)
            .name(names::OUTPUT_NAME)
            .size(TEXT_SIZE)
            .min_width(76.0),
    );
    let mode_label = ui.build(
        label(info.map_or_else(|| "—".to_owned(), mode_text))
            .name(names::OUTPUT_MODE)
            .size(TEXT_SIZE)
            .color(dim)
            .min_width(96.0),
    );
    let scale_value = ui.build(
        label(conf::format_scale(scale))
            .name(names::SCALE_VALUE)
            .size(TEXT_SIZE)
            .width(30.0),
    );
    let scale_slider = ui.build(
        slider(scale)
            .name(names::SCALE)
            .range(SCALE_MIN, SCALE_MAX)
            .step(SCALE_STEP)
            .grow(1.0)
            .min_width(64.0)
            // The one thing a scale change costs: the label beside it.
            // Nothing else in the tree depends on the value until Apply
            // reads it, which is what keeps a slider step to a single
            // `SetText` plus the slider's own repaint.
            .on_change(move |_s: &mut Settings, ui: &mut Ui<Settings>, v: f32| {
                set_label(ui, scale_value, &conf::format_scale(v));
            }),
    );
    let mine = connector.to_owned();
    let primary = ui.build(
        checkbox("primary")
            .name(names::PRIMARY)
            .checked(saved.is_some_and(|o| o.primary))
            .on_toggle(move |s: &mut Settings, ui: &mut Ui<Settings>, on: bool| {
                if on {
                    make_primary(s, ui, &mine);
                }
            }),
    );
    let x = ui.build(field(names::POS_X, "x").width(POS_WIDTH));
    let y = ui.build(field(names::POS_Y, "y").width(POS_WIDTH));
    if let Some((px, py)) = position {
        set_field(ui, x, &px.to_string());
        set_field(ui, y, &py.to_string());
    }

    let container = ui.build(control_row().name(connector));
    for child in [
        name_label,
        mode_label,
        scale_slider,
        scale_value,
        primary,
        x,
        y,
    ] {
        ui.attach(container, child).unwrap();
    }
    if ui.attach(ids.displays, container).is_err() {
        return;
    }
    s.rows.push(Row {
        connector: connector.to_owned(),
        output: info.map(|i| i.id),
        // Only when the file said nothing: a file that names a scale gives
        // the user an explicit value, which Apply always writes back.
        seeded_scale: match saved.and_then(|o| o.scale) {
            Some(_) => None,
            None => Some(scale),
        },
        container,
        scale: scale_slider,
        scale_value,
        primary,
        x,
        y,
    });
}

/// Tick `connector`'s primary box and clear every other.
///
/// Exactly one output is primary, and the server resolves a file naming
/// two by taking the alphabetically first — so a dialog that let two
/// boxes be ticked would be offering a control whose effect depends on
/// connector names. Unticking the others costs nothing when they are
/// already clear: the setter returns early on an unchanged value.
fn make_primary(s: &mut Settings, ui: &mut Ui<Settings>, connector: &str) {
    let others: Vec<WidgetId> = s
        .rows
        .iter()
        .filter(|r| r.connector != connector)
        .map(|r| r.primary)
        .collect();
    for id in others {
        if let Ok(mut c) = ui.widget_mut::<Checkbox<Settings>>(id) {
            c.set_checked(false);
        }
    }
}

/// `1920×1080 @ 60 Hz`, from an output.
#[must_use]
pub fn mode_text(info: &OutputInfo) -> String {
    format!("{}×{} @ {}", info.w, info.h, format_hz(info.refresh_mhz))
}

/// Millihertz as a refresh rate a person reads.
///
/// A whole rate loses its decimals (`60 Hz`, not `60.00 Hz`) and anything
/// else keeps two, because `59.94` is a mode people recognise by name and
/// `59.9` is not.
#[must_use]
pub fn format_hz(mhz: u32) -> String {
    if mhz.is_multiple_of(1000) {
        format!("{} Hz", mhz / 1000)
    } else {
        format!("{:.2} Hz", mhz as f32 / 1000.0)
    }
}

/// Read the widgets into a [`Conf`], with the rows it could not read.
///
/// The one place the tree is read rather than written, and so the shape
/// of what Apply writes. A position whose fields are both empty writes
/// **no** position line — "say nothing about it" is a real instruction to
/// the server, distinct from `0,0` — and one that is not a number is
/// skipped, with the connector's name returned so the status line can say
/// which, rather than written as garbage the compositor would warn about.
fn collect(s: &Settings, ui: &Ui<Settings>, ids: Ids) -> (Conf, Vec<String>) {
    let mut conf = Conf::new();
    let mut unreadable = Vec::new();
    for r in &s.rows {
        let x = field_text(ui, r.x);
        let y = field_text(ui, r.y);
        let position = if x.is_empty() && y.is_empty() {
            None
        } else if let Some(p) = parse_pair(&x, &y) {
            Some(p)
        } else {
            unreadable.push(r.connector.clone());
            None
        };
        let value = ui
            .widget::<Slider<Settings>>(r.scale)
            .map(Slider::value)
            .ok();
        // A row the user never moved, on a connector the file never
        // mentioned, writes no `scale` line at all: persisting the live
        // value would pin today's EDID answer (so a replaced monitor stops
        // being measured) and would bake a `NITRO_SCALE` dev override into
        // the user's permanent file. Moving the slider off the seeded
        // value is what makes it the user's opinion.
        let scale = match (value, r.seeded_scale) {
            (Some(v), Some(seed)) if v.to_bits() == seed.to_bits() => None,
            (v, _) => v,
        };
        let primary = ui
            .widget::<Checkbox<Settings>>(r.primary)
            .is_ok_and(Checkbox::is_checked);
        let out = conf.output_mut(&r.connector);
        out.scale = scale;
        out.position = position;
        out.primary = primary;
    }
    conf.keyboard = KeyboardConf {
        layout: field_text(ui, ids.layout),
        variant: field_text(ui, ids.variant),
        options: field_text(ui, ids.options),
    };
    (conf, unreadable)
}

/// Parse a pair of position fields, treating an empty half as zero.
///
/// An empty `y` beside an `x` of `1920` means "on the same row", which is
/// what somebody typing one number into a two-field control means. Both
/// empty is handled by the caller, because it means something else
/// entirely: no position line at all.
fn parse_pair(x: &str, y: &str) -> Option<(i32, i32)> {
    let one = |t: &str| -> Option<i32> {
        if t.is_empty() {
            Some(0)
        } else {
            t.parse().ok()
        }
    };
    Some((one(x)?, one(y)?))
}

/// Write the file, then ask the server whether it took it.
///
/// The order is the reason this is one function: the reload counter has
/// to be read **before** the rename, or a server that reloaded for some
/// other reason a moment earlier would be read as having accepted this
/// file.
fn apply(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids) {
    s.applies += 1;
    let (conf, unreadable) = collect(s, ui, ids);
    let Some(path) = s.path.clone() else {
        set_status(s, ui, ids, "no config path: set $HOME or $NITRO_CONFIG");
        return;
    };
    let control = s.control.clone().unwrap_or_else(|| ui.control_path());
    let before = control::config_reloads_at(&control);
    if let Err(e) = conf::write(&path, &conf) {
        set_status(s, ui, ids, &format!("could not save: {e}"));
        return;
    }
    let verdict = match control::wait_for_reload(&control, before, s.reload_wait) {
        control::Applied::Reloaded => "applied".to_owned(),
        control::Applied::Rejected => "server rejected: see log".to_owned(),
        // Written and correct; nothing confirmed it. A different thing
        // from a refusal, and it must not be drawn as one.
        control::Applied::Unknown => format!("saved to {}", path.display()),
    };
    let text = if unreadable.is_empty() {
        verdict
    } else {
        format!(
            "{verdict} — ignored unreadable position: {}",
            unreadable.join(", ")
        )
    };
    set_status(s, ui, ids, &text);
}

/// Re-read the file and put every widget back.
fn revert(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids) {
    s.reverts += 1;
    let conf = load_conf(s);
    fill_keyboard(ui, ids, &conf.keyboard);
    for r in s.rows.clone() {
        let saved = conf.output(&r.connector);
        let scale = saved
            .and_then(|o| o.scale)
            .unwrap_or(1.0)
            .clamp(SCALE_MIN, SCALE_MAX);
        if let Ok(mut sl) = ui.widget_mut::<Slider<Settings>>(r.scale) {
            sl.set_value(scale);
        }
        set_label(ui, r.scale_value, &conf::format_scale(scale));
        if let Ok(mut c) = ui.widget_mut::<Checkbox<Settings>>(r.primary) {
            c.set_checked(saved.is_some_and(|o| o.primary));
        }
        let (x, y) = saved.and_then(|o| o.position).map_or_else(
            || (String::new(), String::new()),
            |(x, y)| (x.to_string(), y.to_string()),
        );
        set_field(ui, r.x, &x);
        set_field(ui, r.y, &y);
    }
    // The audio section is re-read here too, deliberately: Revert means
    // "show me what is actually true", and the volume is the one value in
    // this window that something else can have changed behind its back.
    load_audio(s, ui, ids);
    set_status(s, ui, ids, "reverted");
}

/// Put the keyboard section back to what the file says.
fn fill_keyboard(ui: &mut Ui<Settings>, ids: Ids, k: &KeyboardConf) {
    set_field(ui, ids.layout, &k.layout);
    set_field(ui, ids.variant, &k.variant);
    set_field(ui, ids.options, &k.options);
}

/// Write the status line and remember what it says.
fn set_status(s: &mut Settings, ui: &mut Ui<Settings>, ids: Ids, text: &str) {
    text.clone_into(&mut s.status);
    set_label(ui, ids.status, text);
}

/// Set a label's text, ignoring a stale id.
fn set_label(ui: &mut Ui<Settings>, id: WidgetId, text: &str) {
    if let Ok(mut l) = ui.widget_mut::<Label>(id) {
        l.set_text(text);
    }
}

/// Set the text of the label named `name` among `parent`'s children.
fn set_named_label(ui: &mut Ui<Settings>, parent: WidgetId, name: &str, text: &str) {
    let Some(id) = ui
        .children(parent)
        .into_iter()
        .find(|id| ui.address_name(*id).as_deref() == Some(name))
    else {
        return;
    };
    set_label(ui, id, text);
}

/// Set a text field's contents, ignoring a stale id.
fn set_field(ui: &mut Ui<Settings>, id: WidgetId, text: &str) {
    if let Ok(mut f) = ui.widget_mut::<TextField<Settings>>(id) {
        f.set_text(text);
    }
}

/// A text field's trimmed contents.
fn field_text(ui: &Ui<Settings>, id: WidgetId) -> String {
    ui.widget::<TextField<Settings>>(id)
        .map(|f| f.text().trim().to_owned())
        .unwrap_or_default()
}

/// Connect, open the window and run until the app quits.
///
/// The binary is this one call; everything else in the crate is a library
/// so the tests can build the *same* tree rather than a copy of it.
///
/// The connection is the **shell** socket, for the output list, and no
/// `surface` is asked for — so the window is an ordinary decorated one. A
/// server too old for the shell socket, or a client started outside the
/// session, falls back to the ordinary socket and to a dialog that works
/// from the file alone, which it says in [`names::DISPLAYS_NOTE`].
///
/// # Errors
/// Any connection, wire or `epoll` failure. They are all fatal.
pub fn run() -> Result<(), Error> {
    let app = match App::shell(APP_NAME) {
        Ok(app) => app,
        Err(e) => {
            // Not fatal, and not silent: the dialog still configures the
            // keyboard and the audio, and it will say in its own window
            // that the display list is from the file. Falling back
            // quietly is what would be wrong.
            eprintln!("{APP_NAME}: no shell socket ({e}); outputs from server.conf only");
            App::new(APP_NAME)?
        }
    };
    app.title("Settings")
        .size(WINDOW_SIZE)
        .run(Settings::new(), build)
}
