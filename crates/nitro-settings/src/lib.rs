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
//! │ HDMI-A-1 1920×1080 @ 119.98 Hz [==o===] 2 ☑ primary   │  two lines per output
//! │    position [0   ] [0   ]  also 120 · 85 · 60 · 50 Hz │
//! │ VGA-1    1280×1024 @ 60 Hz     [o=====] 1 ☐ primary   │
//! │    position [1920] [0   ]  also 75 Hz                 │
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
//! │ Appearance                                           │
//! │ Colour scheme ☐ Dark                                  │
//! │ The scheme is saved and applied at once; …            │
//! │ ────────────────────────────────────────────────────  │
//! │ [Apply] [Revert]                            applied   │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! # Why a display row is two lines
//!
//! Because one was not enough, and the way it failed is the reason the
//! layout rules in `docs/ui.md` are what they are. The row held seven
//! widgets; #3718 then put the connector's whole alternative-rate list
//! into the mode label, and since #561 a label cannot be laid out below
//! its own text. So the row did not narrow — it grew to ~700 px in a
//! 560-px window, and its last four widgets were painted **on the
//! desktop**, outside the frame.
//!
//! Line 1 is what the monitor is and the two controls you reach for;
//! line 2, indented under the connector, is where it sits and what else
//! it could run. Both of the dim labels **elide**, so a television with
//! a dozen rates shortens its line rather than pushing the row out.
//! (And the toolkit now clips a window's content to the window, so even
//! a row that did overflow would be cut off rather than spilled.)
//!
//! # Why the window is the size it is
//!
//! [`WINDOW_SIZE`] is **measured** from the tree rather than chosen, and
//! its doc comment carries the table — because the previous value was
//! not, and a window smaller than its own tree does not clip, it
//! **shrinks**: a flex container hands its overflow back to its children
//! weighted by size, so every heading, note and row in this dialog was
//! laid out smaller than it had measured. Nothing measured wrong, which
//! is why it survived eighteen passing tests and was found by looking at
//! the screen.
//!
//! Two rules used to keep it fixed, and both are now the toolkit's job
//! (#561): a child is never laid out below the size it measured unless
//! it says it can, and a container's measured size already sums its
//! children, so a column cannot end before the rows inside it. What was
//! `shrink(0.0)` on the headings, notes, captions and on the `displays`
//! and `keyboard` columns is now the default, and this file no longer
//! spells it. `no_widget_is_laid_out_smaller_than_it_measures`
//! and `two_outputs_fit_the_window_and_a_third_clips_rather_than_overlaps`
//! in `tests/settings.rs` pin both, unchanged across that move.
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
    checkbox, column, icon, label, row, separator, slider, spacer, text_field,
};
use nitro_ui::{App, ColorRole, CrossAlign, Error, Scheme, Size, Ui, WidgetId};

use audio::Backend;
use conf::{Conf, KeyboardConf};

/// The name the app registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-settings";

/// The window's size when it opens, and its **minimum** thereafter.
///
/// Explicit, unlike `nitro-calc`'s measured window: the display section's
/// height depends on how many monitors are plugged in, so a window sized
/// to its tree would open differently on every machine — and resize
/// itself when one was unplugged, which is the one thing a settings
/// dialog must not do while you are using it.
///
/// That rationale stands; what it used to omit is that an explicit size
/// still has to **fit the tree it contains**. It did not. At 440×320 the
/// root column's intrinsic height was ~400 px, and a flex container hands
/// an overflow back to its children as `flex_shrink` — so every direct
/// child of the root was scaled down to make the sum fit: section
/// headings 17.5 → 11.8 px (descenders gone: the `p` in "Displays", the
/// `y` in "Keyboard"), the two-line notes 30 → 20 px, `audio_status`
/// 15.1 → 10.2, and every `control_row` 26 → 17.5. Horizontally the same
/// arithmetic ate the keyboard captions — "Layout" rendered in 26 px as
/// "Layc" — and pushed the display row's `y` field out past the window's
/// right edge. Nothing about it was a *text measurement* bug, which is
/// why eighteen passing tests never saw it: every widget measured
/// correctly and was then laid out smaller than it measured.
///
/// So the number is now derived from the tree rather than chosen, and
/// both axes are **measured** rather than reasoned about.
///
/// # The width, and why the row it was measured for changed shape
///
/// 540 px of inner width was what the display row needed as one line:
/// 76 name + 136 mode + 64 slider + 30 value + 79 checkbox + 2 × 54
/// position + 6 × 6 gaps ≈ 529. That row was already full when #3718
/// appended the connector's alternative rates to the mode label
/// (`1920×1080 @ 119.98 Hz (also 60, 84.904, 59.94, 50, 24, 23.976)`),
/// which took the mode from 136 px to ~300 — and since #561 a label
/// cannot be laid out below its text, so the row did not narrow. It
/// **grew**, to ~700 px in this 560-px window, and the slider, the scale
/// value, the checkbox and both position fields were painted on the
/// desktop beside the frame. (Two bugs, and #3725 fixed both: the
/// toolkit now clips a window's content to the window, so the worst case
/// is cut off rather than spilled.)
///
/// The width did not move, because the row did: it is two lines now (see
/// `add_row`), and the width is still what its *first* line needs.
///
/// # The height, which did move
///
/// Measured from the built tree at that width with the height unbounded.
/// A display row is 2 × `ROW_HEIGHT` + `GAP` = 58 px of content, so each
/// output after the first costs **64 px** (`58 + GAP`) rather than the
/// 32 it cost as one line:
///
/// | outputs | tree needs | fits in 500 |
/// |---|---|---|
/// | 1 | 423.2 | yes, 77 px spare |
/// | 2 | 487.2 | yes, 13 px spare |
/// | 3 | 551.2 | **no** — 51 px short |
///
/// 440 → **500**, and the reason is the second line: 440 held two
/// one-line rows with 17 px spare and holds two two-line rows 47 px
/// short. 500 is the smallest round number that holds the two-monitor
/// case, which is what a laptop-plus-screen desktop actually is, and the
/// table is here rather than in prose because the first draft of the
/// *previous* fix claimed two outputs fit at 400 and was wrong by 23 px.
/// `two_outputs_fit_the_window_and_a_third_clips_rather_than_overlaps`
/// asserts the two-output row rather than restating it.
///
/// Widening or heightening it further is free; narrowing it is what
/// produced the bug, which is why [`build`] also pins it as the window's
/// **minimum** through `Ui::set_window_limits`.
///
/// It does **not** grow for a third monitor. The window has no way to ask
/// the server to resize it — `Ui::resize` only re-lays the client's own
/// tree out inside whatever the server gave, verified on pixels rather
/// than from the docs: the frame does not move. So a three-monitor
/// machine gets a window it must drag taller once, which is the
/// limitation `docs/settings.md` records; the alternative is a
/// client-initiated resize op that does not exist and that this task is
/// not the place to add.
///
/// Past that point the degradation is **clipping, not overlap** — and
/// since #561 that is the toolkit's guarantee rather than this file's
/// care. A child is never laid out below what it measured, and a
/// container's measured size already sums its children, so a column
/// cannot end before the rows inside it. Before that landed, the
/// `displays` and `keyboard` sub-columns needed an explicit
/// `shrink(0.0)` for exactly this: without it a column was laid out
/// shorter than the rows it contained and the last row was drawn over
/// `displays_note` — 0.8 px of overlap at two outputs, 15.6 px at
/// three.
pub const WINDOW_SIZE: Size = Size::new(560.0, 500.0);

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
///
/// The layout constants below are **public** for one reason: the test
/// that pins "no widget is laid out smaller than it measures" has to ask
/// the font engine what a heading of this size measures, and a test that
/// hard-coded 15.0 would stop testing the tree the moment somebody
/// changed the constant. Same for the rest of them.
pub const HEADING_SIZE: f32 = 15.0;
/// Font size of everything else.
pub const TEXT_SIZE: f32 = 13.0;
/// Gap between rows, and between the widgets inside one.
pub const GAP: f32 = 6.0;
/// Padding around the whole window.
pub const PAD: f32 = 10.0;
/// Height of one control row.
pub const ROW_HEIGHT: f32 = 26.0;
/// Width of a position field: five digits and a minus sign.
pub const POS_WIDTH: f32 = 54.0;
/// How far a display row's second line is indented under its first.
///
/// The width of the connector-name column (`min_width(76)`) would line
/// the second line's first widget up *with* the mode rather than under
/// the connector, which reads as a second output rather than as a
/// continuation. `GAP * 2` is the smallest indent that is visibly an
/// indent and costs the second line almost none of its width — the line
/// it sits on is the one with the two position fields, which have no
/// give.
pub const ROW_INDENT: f32 = GAP * 2.0;
/// The side of a section heading's icon, in logical pixels.
///
/// 16, not `HEADING_SIZE`: the artwork is drawn on a 16-unit grid, so a
/// 16 px box puts every stroke on a whole pixel at scale 1 and on a whole
/// pair at scale 2. It is also slightly taller than the 15 px heading,
/// which is what makes the pair read as an icon with a label rather than
/// as two words.
pub const ICON_PX: f32 = 16.0;

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

/// What the appearance note says.
///
/// It states the one thing about this section that differs from every
/// other control in the window — no Apply — because a user who ticked it
/// and then pressed Apply out of habit should not wonder whether the
/// first action took.
const NOTE_APPEARANCE: &str =
    "The scheme is saved and applied at once; per-colour overrides live in server.conf.";

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
    /// In a display row: the resolution and refresh rate in force.
    pub const OUTPUT_MODE: &str = "mode";
    /// In a display row: the other rates the connector offers at that
    /// size, on the second line. Absent from a row whose connector
    /// offers nothing else — a row says only what it has to say.
    pub const OUTPUT_MODES: &str = "modes";
    /// In a display row: the caption in front of the position fields.
    pub const POSITION: &str = "position";
    /// In a display row: the first line — connector, mode, scale,
    /// primary.
    pub const ROW_TOP: &str = "top";
    /// In a display row: the second line — position, and the rates.
    pub const ROW_BOTTOM: &str = "bottom";
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

    /// The appearance section's row.
    pub const APPEARANCE: &str = "appearance";
    /// The dark-scheme checkbox: `theme.scheme`.
    pub const DARK: &str = "dark";
    /// The line saying which scheme is in force and that it is live.
    pub const APPEARANCE_NOTE: &str = "appearance_note";

    /// The Apply button.
    pub const APPLY: &str = "apply";
    /// The Revert button.
    pub const REVERT: &str = "revert";
    /// The line that says what Apply did.
    pub const STATUS: &str = "status";
    /// The row holding Apply, Revert and the status line.
    ///
    /// Named rather than left as `window/container[N]`, and the reason is
    /// worth recording: it *was* an index, and adding the four heading
    /// rows renumbered it. A path built out of a sibling count is a path
    /// that changes whenever the tree above it does, which makes every
    /// script and every test that used it quietly wrong rather than
    /// loudly broken.
    pub const BUTTONS: &str = "buttons";

    /// The icon beside the Displays heading.
    pub const DISPLAYS_ICON: &str = "displays_icon";
    /// The icon beside the Keyboard heading.
    pub const KEYBOARD_ICON: &str = "keyboard_icon";
    /// The icon beside the Audio heading.
    pub const AUDIO_ICON: &str = "audio_icon";
    /// The icon beside the Appearance heading.
    pub const APPEARANCE_ICON: &str = "appearance_icon";
}

/// The icons each section heading carries, named in one place so a
/// rename is one edit and the tests assert on the same constants the
/// tree is built from.
pub mod icons {
    /// Beside "Displays".
    pub const DISPLAYS: &str = "display";
    /// Beside "Keyboard".
    pub const KEYBOARD: &str = "keyboard";
    /// Beside "Audio".
    pub const AUDIO: &str = "speaker";
    /// Beside "Appearance".
    pub const APPEARANCE: &str = "palette";
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
    ///
    /// A **column** of two lines since #3725, not a `control_row`: one
    /// row could not hold the connector, the mode, the alternatives
    /// #3718 added, the slider, its value, the checkbox and two position
    /// fields, and the toolkit's content floor turns "cannot hold" into
    /// "runs past the window". The `hey` path is unchanged —
    /// `displays/<connector>/scale` still resolves, because a segment
    /// that names no direct child is looked for by name in the subtree.
    container: WidgetId,
    /// The label carrying the mode in force, on line 1.
    mode: WidgetId,
    /// The dim label listing the connector's other rates, on line 2, or
    /// `None` for a connector that offers none.
    modes: Option<WidgetId>,
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
    /// How many times the scheme checkbox has written the file.
    ///
    /// Counted for the same reason `applies` is: the appearance section
    /// writes outside the Apply path, so a test needs a way to say "that
    /// click really did reach the disk" that is not "read the file and
    /// hope".
    scheme_writes: u64,
    /// Every mode each connector offers, from the server's `modes`
    /// command, as `(connector, mode)` in the order it listed them.
    ///
    /// Read once at start-up and not refreshed: it is a monitor's
    /// capability list, which does not change while the dialog is open
    /// unless the cable does — and a hotplug rebuilds the rows anyway.
    /// Empty when there is no server to ask, which is exactly when the
    /// row should say nothing extra.
    modes: Vec<(String, String)>,
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
            scheme_writes: 0,
            modes: Vec::new(),
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

    /// Use this mode table instead of asking the control socket for one.
    ///
    /// `(connector, mode)` pairs in the spelling the server's `modes`
    /// command uses, e.g. `("HDMI-A-1", "1920x1080@120")`.
    ///
    /// Injected for the same reason the config path and the audio search
    /// path are — a test must not reach for `std::env::set_var` — and for
    /// one more: the harness's fake output offers exactly **one** mode,
    /// so a row built against its server never gets a second-line rates
    /// label, and the label would have no test at all. A non-empty list
    /// here is taken as given and the socket is not asked.
    #[must_use]
    pub fn with_modes(mut self, modes: Vec<(String, String)>) -> Self {
        self.modes = modes;
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

    /// How many times the scheme checkbox has written `server.conf`.
    ///
    /// The appearance section saves outside the Apply path, so this is
    /// how a test says "that click reached the disk" — and, more
    /// usefully, "that one did *not*".
    #[must_use]
    pub fn scheme_writes(&self) -> u64 {
        self.scheme_writes
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
    dark: WidgetId,
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
    // -- displays ------------------------------------------------------
    //
    // An empty column: the rows arrive from the shell socket, or from the
    // file, on the first turn of the loop — `build` has no state to put
    // them in and no answer from the connection yet.
    //
    // No `shrink(0.0)` here any more: since #561 a container's measured
    // size is its floor, and a container's measured size already sums
    // its children — so a column cannot be laid out shorter than the
    // rows it holds. It used to be able to, and the last row was drawn
    // on top of `displays_note`: 0.8 px of overlap with two outputs,
    // 15.6 px with three.
    let displays = ui.build(column().name(names::DISPLAYS).gap(GAP).width_percent(1.0));
    let displays_note = ui.build(note(NOTE_LIVE).name(names::DISPLAYS_NOTE));

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
        let c = ui.build(caption(caption_text));
        ui.attach(kb_row, c).unwrap();
        ui.attach(kb_row, id).unwrap();
    }
    let test_row = ui.build(control_row());
    let test_caption = ui.build(caption("Test here"));
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
    let audio_status = ui.build(note("").name(names::AUDIO_STATUS));
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
    let volume_caption = ui.build(caption("Volume"));
    for child in [volume_caption, volume, volume_value, mute] {
        ui.attach(audio, child).unwrap();
    }

    // -- appearance ----------------------------------------------------
    //
    // One checkbox, because there are two schemes. A pair of radio
    // buttons would say the same thing in twice the space, and a
    // drop-down would promise a list that does not exist.
    //
    // Unlike every other control in this window, this one **does not
    // wait for Apply**: it writes `server.conf` on the spot. A colour
    // scheme is the one setting whose result you judge by looking at it,
    // and the server pushes the new palette to every client within a
    // frame — so "tick it and watch the desktop change" is the whole
    // interaction. Making the user press Apply afterwards would be
    // asking them to confirm something they can already see.
    //
    // The status label is built *before* the checkbox because the
    // toggle's callback writes to it, and a builder's callback can only
    // capture ids that already exist — the same ordering the audio
    // section uses for `audio_status`.
    let status = ui.build(
        // Not a `note`: this one lives **inside** a row, where
        // `flex_shrink` governs width rather than height, and a status
        // line that refused to narrow would take the space out of the
        // Apply and Revert buttons beside it. A long verdict is better
        // clipped than a button is.
        label("")
            .name(names::STATUS)
            .size(TEXT_SIZE)
            .color_role(ColorRole::TextDim),
    );
    let dark = ui.build(checkbox("Dark").name(names::DARK).on_toggle(
        move |s: &mut Settings, ui: &mut Ui<Settings>, on: bool| {
            set_scheme(s, ui, status, on);
        },
    ));
    let appearance_note = ui.build(note(NOTE_APPEARANCE).name(names::APPEARANCE_NOTE));

    // -- apply / revert ------------------------------------------------
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
        dark,
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
    let buttons = ui.build(control_row().name(names::BUTTONS));
    for child in [apply_button, revert_button, gap, status] {
        ui.attach(buttons, child).unwrap();
    }

    // -- the window ----------------------------------------------------
    let root = ui.build(column().gap(GAP).padding(PAD).width_percent(1.0));
    let h_displays = heading_row(ui, names::DISPLAYS_ICON, icons::DISPLAYS, "Displays");
    let h_keyboard = heading_row(ui, names::KEYBOARD_ICON, icons::KEYBOARD, "Keyboard");
    let h_audio = heading_row(ui, names::AUDIO_ICON, icons::AUDIO, "Audio");
    let h_appearance = heading_row(ui, names::APPEARANCE_ICON, icons::APPEARANCE, "Appearance");
    let appearance = ui.build(control_row().name(names::APPEARANCE));
    let scheme_caption = ui.build(caption("Colour scheme"));
    for child in [scheme_caption, dark] {
        ui.attach(appearance, child).unwrap();
    }
    let sep_a = ui.build(separator().width_percent(1.0));
    let sep_b = ui.build(separator().width_percent(1.0));
    let sep_c = ui.build(separator().width_percent(1.0));
    let sep_d = ui.build(separator().width_percent(1.0));
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
        h_appearance,
        appearance,
        appearance_note,
        sep_d,
        buttons,
    ] {
        ui.attach(root, child).unwrap();
    }

    install(ui, ids);
    root
}

/// A row of controls: the shape every labelled line in this dialog has.
///
/// `height(ROW_HEIGHT)` is now enough on its own. It used **not** to be:
/// an explicit length is folded into the constraints a child is
/// *measured* with, and the flex solver then took a container's overflow
/// back out of its children weighted by `flex_shrink`, clamping only to
/// `min_height`/`max_height` afterwards — so a root column with more
/// tree than window squashed every row to 17.5 px while each one went on
/// reporting that it had asked for 26. Since #561 the measured size is
/// itself the floor, so the row keeps the 26 it asked for. The
/// `min_height` stays because it is what a row that ends up in a
/// `Zero`-floor container would still be held up by, and it is why
/// `every_row_is_exactly_row_height` can assert an equality rather than
/// a tolerance.
fn control_row() -> FlexBuilder<Settings> {
    row()
        .gap(GAP)
        .height(ROW_HEIGHT)
        .min_height(ROW_HEIGHT)
        .width_percent(1.0)
        .cross_align(CrossAlign::Center)
}

/// A section heading's **label**.
///
/// A heading is one line of text at a fixed size, so there is no smaller
/// honest version of it — and since #561 that is the toolkit's default
/// rather than something this file asks for. Letting the root column
/// reclaim its overflow here cost the headings their descenders: 17.5 px
/// of measured text laid out in 11.8, which is what "Displays" with no
/// tail on the `p` looked like on the box.
fn heading(text: &str) -> LabelBuilder<Settings> {
    label(text)
        .size(HEADING_SIZE)
        .weight(600)
        .color_role(ColorRole::Text)
}

/// A section heading: its icon and its label, in one row.
///
/// The row is **not** a `control_row`: a `control_row` is
/// `ROW_HEIGHT`-tall by contract, and a heading is as tall as its own
/// text (17.5 px at `HEADING_SIZE`). Pinning the heading to 26 would add
/// 8.5 px per section — 34 px over four sections — to a window whose
/// height is measured from its tree. So the row takes its height from
/// its children, which is what `HEADING_ROW_H` below records and why
/// `WINDOW_SIZE` did not move.
///
/// `ColorRole::Text`, the same role the label takes, so the pair is one
/// visual unit that follows the scheme together.
fn heading_row(ui: &mut Ui<Settings>, name: &str, icon_name: &str, text: &str) -> WidgetId {
    let container = ui.build(
        row()
            .gap(GAP)
            .width_percent(1.0)
            .cross_align(CrossAlign::Center),
    );
    let glyph = ui.build(
        icon(icon_name)
            .name(name)
            .size(ICON_PX)
            .color_role(ColorRole::Text),
    );
    let text = ui.build(heading(text));
    for child in [glyph, text] {
        ui.attach(container, child).unwrap();
    }
    container
}

/// A line of explanatory text under a section.
///
/// Wrapped, so its height depends on the width it is given — and the
/// height it computes is the height it gets. Before #561 the two-line
/// notes were laid out in 20 px of a measured 30 and the second line was
/// sliced through the middle.
fn note(text: &str) -> LabelBuilder<Settings> {
    label(text).size(TEXT_SIZE).color_role(ColorRole::TextDim)
}

/// A label in front of a control.
///
/// Deliberately **unnamed**: it is furniture, and naming it would put a
/// second `layout` in the keyboard row for `hey` to be ambiguous about.
///
/// A caption is the one thing in a row that cannot usefully be narrowed:
/// the fields beside it degrade gracefully at any width, a six-letter
/// word does not. The keyboard row wants 683 px of its three captions
/// and three fields and gets 540; with every child shrinking by weight
/// the captions lost 40 % of their width and "Layout" in 26 px read
/// "Layc". The overflow now comes out of the fields, which say they are
/// viewports over their own text, and the caption keeps what it
/// measured without asking.
fn caption(text: &str) -> LabelBuilder<Settings> {
    label(text).size(TEXT_SIZE).color_role(ColorRole::TextDim)
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
    // The one thing that stops the bug coming back by the user's own
    // hand: the server refuses a drag below this, so the tree can never
    // again be asked to fit in less than it measures. Zero on the height
    // axis would mean "no limit", so both components are real numbers;
    // the maximum is unlimited, because a machine with four monitors
    // wants to make this window taller and nothing here should stop it.
    //
    // Sent from here rather than from `build` because `build` runs before
    // the window exists — `Ui::set_window_limits` records that and rides
    // the window's first commit, which is exactly what is wanted, but
    // `install` is where every other piece of wiring lives.
    let _ = ui.set_window_limits(WINDOW_SIZE, Size::ZERO);
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
    // The monitor's own capability list, for the read-only
    // `also 120 · 85 · … Hz` on each display row's second line. A server
    // that is not running, or one too old to know `modes`, leaves this
    // empty and the row says only the mode in force — and carries no
    // second-line rates label at all, which is what it did before this
    // existed. A list injected by a test wins: the harness's fake output
    // offers one mode, so asking its server would make the label
    // untestable.
    if s.modes.is_empty()
        && let Some(path) = s.control.as_deref()
    {
        s.modes = control::modes_at(path).unwrap_or_default();
    }
    s.audio = Backend::detect_in(&s.audio_dirs);
    load_audio(s, ui, ids);

    let conf = load_conf(s);
    fill_keyboard(ui, ids, &conf.keyboard);
    fill_appearance(ui, ids, &conf.theme);

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
    if let Some(r) = s.rows.iter().find(|r| r.connector == info.name) {
        let (mode, modes) = (r.mode, r.modes);
        if let Some(r) = s.rows.iter_mut().find(|r| r.connector == info.name) {
            r.output = Some(info.id);
        }
        // A mode change is the only thing a re-sent `OutputInfo` may
        // move here. The scale and the position are deliberately *not*
        // re-read: they are what the user is editing, and a hotplug
        // elsewhere on the desktop must not throw away a typed number.
        //
        // Both lines move together, because both are about the mode: the
        // one in force on line 1, and the ones it could be on line 2. A
        // connector that grew its list while the window was open keeps
        // whatever label it was built with, though — a row with no
        // `modes` label cannot gain one without a relayout of the whole
        // section, and a mode *change* does not change the list.
        set_label(ui, mode, &mode_text(info));
        if let Some(id) = modes
            && let Some(text) = alternatives_text(info, &s.modes)
        {
            set_label(ui, id, &text);
        }
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

    // A display row is **two lines** since #3725, and the reason is the
    // arithmetic rather than taste. One row held seven widgets — the
    // connector, the mode, the slider, its value, the `primary` box and
    // two position fields — and #3718 then put the connector's whole
    // alternative-rate list into the mode label
    // (`1920×1080 @ 119.98 Hz (also 60, 84.904, 59.94, 50, 24,
    // 23.976)`). Since #561 a label cannot be laid out below its text,
    // so the row did not narrow: it grew to ~700 px in a 560-px window
    // and the last four widgets were painted on the desktop beside it.
    //
    //     HDMI-A-1   1920×1080 @ 119.98 Hz   [──●──] 1   ☐ primary
    //       position [0] [0]   also 120 · 85 · 60 · 50 · 24 Hz
    //
    // Line 1 is what the monitor *is* and the two controls you reach for;
    // line 2, indented under the connector, is what it could be. The
    // alternatives are a dim secondary label rather than a picker: a
    // picker that changed the mode would blank the screen from a dialog
    // you might be reading on it, which is the control `docs/settings.md`
    // says this app deliberately does not offer. They **elide**, so a
    // television's dozen rates shorten rather than push the row out, and
    // the row carries no `modes` label at all on a connector that offers
    // nothing else.
    //
    // Nothing here spells `shrink(0.0)` except the position fields: since
    // #561 a widget's measured size is its own floor, and the two widgets
    // that say otherwise say so themselves — the slider (`Zero`, because
    // a narrower track is still a track and its value is in the label
    // beside it) and the eliding label (`Zero` with a floor of `…` plus
    // three characters). So an overflow lands on those two by
    // construction. The `min_width`s stay: they are what a *shorter*
    // string than today's would still reserve, and the slider's is what
    // says how narrow is still draggable.
    let name_label = ui.build(
        label(connector)
            .name(names::OUTPUT_NAME)
            .size(TEXT_SIZE)
            .min_width(76.0),
    );
    // Just the mode in force. The alternatives moved to line 2, which is
    // what stopped this label from being the thing that broke the row.
    let mode_label = ui.build(
        label(info.map_or_else(|| "—".to_owned(), mode_text))
            .name(names::OUTPUT_MODE)
            .size(TEXT_SIZE)
            .color_role(ColorRole::TextDim)
            .elide(true)
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
    let BottomLine {
        row: bottom,
        x,
        y,
        rates: rates_label,
    } = build_bottom_line(s, ui, info, position);

    let top = ui.build(control_row().name(names::ROW_TOP));
    for child in [name_label, mode_label, scale_slider, scale_value, primary] {
        ui.attach(top, child).unwrap();
    }
    let container = ui.build(column().name(connector).gap(GAP).width_percent(1.0));
    for child in [top, bottom] {
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
        mode: mode_label,
        modes: rates_label,
        scale: scale_slider,
        scale_value,
        primary,
        x,
        y,
    });
}

/// The widgets on a display row's **second** line, and the line itself.
///
/// Split out of [`add_row`] because the row is two lines and the function
/// was one: a reader who wants to know what the position fields do should
/// not have to walk past the slider's callback to find them.
struct BottomLine {
    /// The line itself, ready to attach.
    row: WidgetId,
    /// The x position field.
    x: WidgetId,
    /// The y position field.
    y: WidgetId,
    /// The dim label listing the connector's other rates, or `None` for a
    /// connector that offers none.
    rates: Option<WidgetId>,
}

/// Build a display row's second line: `position [x] [y]  also … Hz`.
///
/// Indented under the connector (`ROW_INDENT`), so it reads as a
/// continuation of the first line rather than as another output.
///
/// # Panics
/// Never in practice — every `attach` names an id created just above.
fn build_bottom_line(
    s: &Settings,
    ui: &mut Ui<Settings>,
    info: Option<&OutputInfo>,
    position: Option<(i32, i32)>,
) -> BottomLine {
    // The two position fields hold three or four digits, so they take an
    // explicit width rather than the field default of twenty characters.
    // `shrink(0.0)` is still spelled out here because a field is one of
    // the few widgets that *does* opt out of the content floor (it is a
    // viewport over its own text), so without it these two would be the
    // first thing a crowded row narrowed — and a position box too narrow
    // for "1920" is not a smaller version of itself.
    let x = ui.build(field(names::POS_X, "x").width(POS_WIDTH).shrink(0.0));
    let y = ui.build(field(names::POS_Y, "y").width(POS_WIDTH).shrink(0.0));
    if let Some((px, py)) = position {
        set_field(ui, x, &px.to_string());
        set_field(ui, y, &py.to_string());
    }
    let position_caption = ui.build(caption("position").name(names::POSITION));
    // Only when there is something to list. A row that said `also  Hz`,
    // or reserved space for a connector with one mode, would be spending
    // the window's scarcest axis on nothing.
    let rates = info
        .and_then(|i| alternatives_text(i, &s.modes))
        .map(|text| {
            ui.build(
                label(text)
                    .name(names::OUTPUT_MODES)
                    .size(TEXT_SIZE)
                    .color_role(ColorRole::TextDim)
                    .elide(true)
                    .grow(1.0)
                    .min_width(60.0),
            )
        });
    let row = ui.build(
        control_row()
            .name(names::ROW_BOTTOM)
            .padding_xy(ROW_INDENT, 0.0),
    );
    for child in [position_caption, x, y] {
        ui.attach(row, child).unwrap();
    }
    if let Some(m) = rates {
        ui.attach(row, m).unwrap();
    }
    BottomLine { row, x, y, rates }
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

/// `also 120 · 85 · 50 · 24 Hz`, or `None` when there is nothing to say.
///
/// The second line of a display row: the *other* rates this connector
/// offers at the size it is running, which is the one thing about a
/// monitor the file cannot tell you. Middle dots rather than commas
/// because the list is a set of alternatives rather than a sentence, and
/// one `Hz` at the end rather than six, because the row has no room for
/// the five it does not need.
///
/// Rates are printed as a **person** reads them — at most two decimals
/// and no trailing zeros, so `84.904` from the kernel's table is `84.9`
/// and `59.940` is `59.94`. The matching against the server's own
/// spelling happens before that, on the server's strings, which is why
/// `nitro_kms_hz` still exists: the exclusion of the current rate has
/// to agree with the server character for character, and the display
/// spelling deliberately does not.
#[must_use]
pub fn alternatives_text(info: &OutputInfo, modes: &[(String, String)]) -> Option<String> {
    let others = alternative_rates(info, modes);
    if others.is_empty() {
        return None;
    }
    let shown: Vec<String> = others.iter().map(|r| trim_rate(r)).collect();
    Some(format!("also {} Hz", shown.join(" · ")))
}

/// The other rates this connector offers at the size it is running, in
/// the server's own spelling and in the order `modes` listed them.
fn alternative_rates(info: &OutputInfo, modes: &[(String, String)]) -> Vec<String> {
    let size = format!("{}x{}@", info.w, info.h);
    let mut others: Vec<String> = Vec::new();
    for (_, m) in modes.iter().filter(|(n, _)| *n == info.name) {
        let Some(rate) = m.strip_prefix(&size) else {
            continue;
        };
        // The rate in force is what the line already says, and a table
        // that lists the same rate twice (60.000 and 59.940 both round to
        // "60" in this spelling) must not say it twice either.
        if rate == nitro_kms_hz(info.refresh_mhz) || others.iter().any(|o| o == rate) {
            continue;
        }
        others.push(rate.to_owned());
    }
    others
}

/// A rate from the server's table, as a person reads it: at most two
/// decimals, and no trailing zeros.
///
/// The server prints three decimals because that is what a millihertz
/// table has (`84.904`, `59.940`); a display row wants `84.9` and
/// `59.94`. Rounding rather than truncating, so `84.904` does not become
/// `84.90` and then `84.9` by a different route than `84.896` would.
fn trim_rate(rate: &str) -> String {
    let Ok(v) = rate.parse::<f64>() else {
        // Not a number we can re-print: show the server's own spelling
        // rather than nothing. A rate this app cannot parse is a rate the
        // user can still type into `output.<c>.mode`.
        return rate.to_owned();
    };
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_owned()
}

/// The rate in the spelling the `modes` command uses, so the current mode
/// can be matched against that list.
///
/// Not [`format_hz`], which is the spelling a *person* reads (`60 Hz`,
/// `59.94 Hz`): this one has to agree character for character with the
/// server's, or the line lists the rate it is already running as an
/// alternative to itself.
fn nitro_kms_hz(refresh_mhz: u32) -> String {
    if refresh_mhz.is_multiple_of(1000) {
        format!("{}", refresh_mhz / 1000)
    } else {
        let s = format!("{:.3}", f64::from(refresh_mhz) / 1000.0);
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
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
    // The colour section is carried over from the file rather than
    // rendered from the widgets, and it is the one place this dialog
    // does that. The scheme checkbox already wrote it (`set_scheme`
    // saves on the spot), and the per-role overrides have no widget at
    // all — so the only honest thing Apply can do with `theme.*` is hand
    // back what is on disk. Collecting it from the tree would mean Apply
    // deleting a user's hand-picked `theme.accent`, from a button that
    // promises to save.
    conf.theme = load_conf(s).theme;
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
    fill_appearance(ui, ids, &conf.theme);
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

/// Put the appearance section back to what the file says.
fn fill_appearance(ui: &mut Ui<Settings>, ids: Ids, t: &conf::ThemeConf) {
    let dark = t.scheme.unwrap_or_default() == Scheme::Dark;
    if let Ok(mut c) = ui.widget_mut::<Checkbox<Settings>>(ids.dark) {
        // `set_checked` does not fire `on_change`, which is what makes
        // this safe to call from Revert: a setter that re-entered
        // `set_scheme` would write the file back out on every Revert,
        // and a Revert that writes is not a revert.
        c.set_checked(dark);
    }
}

/// Write the scheme to `server.conf` and let the server push it.
///
/// The odd one out in this window: it saves immediately rather than
/// waiting for Apply. See the comment on the checkbox in [`build`].
///
/// Everything else in the file is preserved, because the rest of the
/// dialog is *not* collected here: the file is re-read, one key is
/// changed, and it goes back. Rendering from the widgets instead would
/// mean ticking "Dark" silently committed a half-edited keyboard layout
/// the user had not pressed Apply for.
fn set_scheme(s: &mut Settings, ui: &mut Ui<Settings>, status: WidgetId, dark: bool) {
    s.scheme_writes += 1;
    let say = |s: &mut Settings, ui: &mut Ui<Settings>, text: String| {
        text.clone_into(&mut s.status);
        set_label(ui, status, &text);
    };
    let Some(path) = s.path.clone() else {
        say(
            s,
            ui,
            "no config path: set $HOME or $NITRO_CONFIG".to_owned(),
        );
        return;
    };
    let mut conf = load_conf(s);
    conf.theme.scheme = Some(if dark { Scheme::Dark } else { Scheme::Light });
    if let Err(e) = conf::write(&path, &conf) {
        say(s, ui, format!("could not save the scheme: {e}"));
        return;
    }
    // No `wait_for_reload` here, unlike Apply. The confirmation a user
    // wants for a colour scheme is the screen changing colour, which
    // happens on its own within a frame; blocking the click for up to
    // `RELOAD_WAIT` to be able to write "applied" in a status line would
    // make the one instant control in the window the slowest.
    say(
        s,
        ui,
        format!("scheme: {}", if dark { "dark" } else { "light" }),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32, refresh_mhz: u32) -> OutputInfo {
        OutputInfo {
            id: 1,
            name: "HDMI-A-1".to_owned(),
            w,
            h,
            refresh_mhz,
            scale: 1.0,
            x: 0,
            y: 0,
        }
    }

    #[test]
    fn a_rate_a_person_reads() {
        assert_eq!(format_hz(60_000), "60 Hz");
        assert_eq!(format_hz(120_000), "120 Hz");
        assert_eq!(format_hz(59_940), "59.94 Hz");
    }

    #[test]
    fn the_second_line_names_the_other_rates_at_this_size() {
        // The box's connector, as `modes` reports it: several rates at
        // 1080p and a 4K mode that is a different *size*.
        let modes: Vec<(String, String)> = [
            "1920x1080@120",
            "1920x1080@85",
            "1920x1080@60",
            "1920x1080@50",
            "1920x1080@24",
            "3840x2160@30",
        ]
        .iter()
        .map(|m| ("HDMI-A-1".to_owned(), (*m).to_owned()))
        .collect();
        assert_eq!(
            alternatives_text(&info(1920, 1080, 60_000), &modes).as_deref(),
            Some("also 120 · 85 · 50 · 24 Hz"),
            "the rate in force is not listed as an alternative to itself, \
             and a different size is not an alternative at all"
        );
        // At 120 the same list reads the other way round, which is the
        // check that the exclusion is of the *current* rate and not of a
        // hard-coded 60.
        assert_eq!(
            alternatives_text(&info(1920, 1080, 120_000), &modes).as_deref(),
            Some("also 85 · 60 · 50 · 24 Hz")
        );
        // A size the connector lists nothing else at gets no second-line
        // label at all, rather than an empty one: `None` is what tells
        // `add_row` not to build the widget.
        assert_eq!(alternatives_text(&info(3840, 2160, 30_000), &modes), None);
        // No server to ask: nothing to say.
        assert_eq!(alternatives_text(&info(1920, 1080, 60_000), &[]), None);
        // And the line in force is its own label, with no list in it —
        // which is the whole of #3725's row fix in one assertion.
        assert_eq!(
            mode_text(&info(1920, 1080, 119_982)),
            "1920×1080 @ 119.98 Hz"
        );
    }

    #[test]
    fn a_rate_is_printed_with_at_most_two_decimals_and_no_trailing_zeros() {
        // The server's table is in millihertz and prints three decimals
        // (`84.904`, `59.940`, `120.000`); a display row wants what a
        // person reads. Rounded rather than truncated, so `84.904` and
        // `84.896` do not arrive at `84.9` by different routes.
        let modes: Vec<(String, String)> = ["1920x1080@84.904", "1920x1080@59.940", "1920x1080@50"]
            .iter()
            .map(|m| ("HDMI-A-1".to_owned(), (*m).to_owned()))
            .collect();
        assert_eq!(
            alternatives_text(&info(1920, 1080, 119_982), &modes).as_deref(),
            Some("also 84.9 · 59.94 · 50 Hz")
        );
        // A rate this app cannot parse is shown in the server's own
        // spelling rather than dropped: it is still a string the user can
        // type into `output.<c>.mode`.
        assert_eq!(trim_rate("weird"), "weird");
    }

    #[test]
    fn a_fractional_rate_matches_the_servers_spelling() {
        // The trap this exists for: the *display* spelling of the rate in
        // force is `59.94 Hz` and the server's is `59.940`, and the
        // exclusion has to be done on the server's, or the line offers
        // the rate it is already running as something else to try.
        let modes: Vec<(String, String)> = ["1920x1080@60", "1920x1080@59.94"]
            .iter()
            .map(|m| ("HDMI-A-1".to_owned(), (*m).to_owned()))
            .collect();
        assert_eq!(mode_text(&info(1920, 1080, 59_940)), "1920×1080 @ 59.94 Hz");
        assert_eq!(
            alternatives_text(&info(1920, 1080, 59_940), &modes).as_deref(),
            Some("also 60 Hz")
        );
    }

    #[test]
    fn another_connectors_modes_are_not_this_rows() {
        let modes = vec![
            ("DP-1".to_owned(), "1920x1080@144".to_owned()),
            ("HDMI-A-1".to_owned(), "1920x1080@120".to_owned()),
        ];
        assert_eq!(
            alternatives_text(&info(1920, 1080, 60_000), &modes).as_deref(),
            Some("also 120 Hz")
        );
    }
}
