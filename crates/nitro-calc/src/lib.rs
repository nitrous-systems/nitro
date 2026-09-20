//! `nitro-calc` — a calculator, and the first application written
//! against `nitro-ui`.
//!
//! It exists to answer one question with a running program rather than a
//! claim: *what does an app on nitro actually look like?* The answer is
//! this file — a state struct, a tree of widgets built once, and a
//! callback per button — plus [`engine`], which holds the arithmetic and
//! has never heard of a widget.
//!
//! ```text
//! ┌───────────────────────┐
//! │                 7 + 8 │  history  (a Label, named `history`)
//! │                    15 │  display  (a Label, named `display`)
//! ├─────┬─────┬─────┬─────┤
//! │  C  │  ⌫  │  ±  │  ÷  │
//! │  7  │  8  │  9  │  ×  │
//! │  4  │  5  │  6  │  −  │
//! │  1  │  2  │  3  │  +  │
//! │  0  │  .  │  =        │
//! └─────┴─────┴─────┴─────┘
//! ```
//!
//! Every button carries a `.name()`, so the whole calculator is drivable
//! from a shell with no cooperation from this code:
//!
//! ```text
//! hey nitro-calc do window/7 click
//! hey nitro-calc do window/plus click
//! hey nitro-calc do window/8 click
//! hey nitro-calc do window/equals click
//! hey nitro-calc get window/display value     # 15
//! ```
//!
//! The keyboard mirrors the buttons exactly (`0-9 . + - * / = Enter
//! Backspace Escape`, and `q` to quit), because both go through the same
//! [`engine::Key`] — a key and a button that disagreed would be two
//! implementations of the same calculator.
//!
//! # One keypress is one `SetText`
//!
//! Pressing a digit changes one thing: the display's string. What that
//! costs on the wire is **one `SetText` and the `Commit` that carries
//! it** — no button repaints, and nothing is re-measured but the label.
//! The test `one_keypress_is_one_set_text` asserts it from outside by
//! counting mutations, because a cost claim nothing checks stops being
//! true.
//!
//! # The window has a floor
//!
//! The keypad is a real grid — every cell the same width, `=` spanning
//! two of them — and the window declares a minimum size so it cannot be
//! dragged down until the fields vanish. Both numbers are *derived* from
//! [`KEYPAD`] and from text measured through the font engine rather than
//! written down, so a key with a wider glyph moves them on its own. See
//! [`min_window`].

pub mod engine;

use engine::{Engine, Key, Op};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{Handled, KeyEvent, key, mods};
use nitro_ui::widgets::{Label, button, column, label, row};
use nitro_ui::{Align, App, ColorRole, Error, Size, TextStyle, Ui, WidgetId};

/// The app's state: the calculator, and nothing else.
///
/// This is the `S` of `Ui<S>` — a plain struct handed to every callback
/// as `&mut S` alongside `&mut Ui<S>`. There is no `Rc`, no `RefCell` and
/// no observer list anywhere in this app, because a callback that has
/// both of those does not need one.
pub struct Calc {
    /// The state machine and the formatter; see [`engine`].
    engine: Engine,
    /// How many keys have been applied, however they arrived. Not used by
    /// the UI: it is here because `hey nitro-calc get window value` makes
    /// a claim about the app's own state easy to check from outside.
    presses: u64,
}

impl Calc {
    /// A calculator showing zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: Engine::new(),
            presses: 0,
        }
    }

    /// The state machine, for the tests.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// How many keys have been applied.
    #[must_use]
    pub fn presses(&self) -> u64 {
        self.presses
    }
}

impl Default for Calc {
    fn default() -> Self {
        Self::new()
    }
}

/// The two labels a key press writes to.
///
/// `Copy`, so the same handle goes into every button's `on_click` *and*
/// into the app-level key handlers — which is how a typed `7` and a
/// clicked `7` end up running the identical three lines of
/// [`Screen::press`]. Keeping the ids here rather than in [`Calc`] is
/// what lets the state be built before the tree it will drive.
#[derive(Debug, Clone, Copy)]
struct Screen {
    /// The big right-aligned number.
    display: WidgetId,
    /// The dimmer line above it.
    history: WidgetId,
}

impl Screen {
    /// Apply one key and push the result into the two labels.
    ///
    /// The only place this app writes to the tree. A `set_text` that
    /// changes nothing costs nothing — [`Label`]'s setter returns early —
    /// which is what keeps a digit to one `SetText` even though both
    /// labels are offered a new string.
    fn press(self, s: &mut Calc, ui: &mut Ui<Calc>, k: Key) {
        s.engine.press(k);
        s.presses += 1;
        if let Ok(mut l) = ui.widget_mut::<Label>(self.display) {
            l.set_text(s.engine.display());
        }
        if let Ok(mut l) = ui.widget_mut::<Label>(self.history) {
            l.set_text(s.engine.history());
        }
    }
}

/// Font size of the display, in logical pixels.
const DISPLAY_SIZE: f32 = 28.0;
/// Font size of the history line.
const HISTORY_SIZE: f32 = 13.0;
/// Font size of a button's label.
const BUTTON_SIZE: f32 = 18.0;
/// Height of one keypad row.
const ROW_HEIGHT: f32 = 44.0;
/// Smallest a keypad row may be squeezed to.
///
/// The rows opt out of the toolkit's content shrink floor (see
/// [`build`]), which on its own means a floor of *zero*: a short enough
/// window would collapse the keypad to nothing rather than to "small but
/// still tappable". This bounds it at the button's own line box, so a
/// squeezed key is still a key with its glyph inside it, and below that
/// the keypad overflows and clips like anything else.
///
/// It is also a *term* of [`min_window`], which is the other half of the
/// story: this says how small a row may honestly go, and the window
/// minimum says the window may not be dragged below the sum of them.
/// Neither is sufficient alone — a floor with no window minimum still
/// loses its bottom row off the edge (issue #614).
///
/// `BUTTON_SIZE` rather than the measured line height because a constant
/// cannot call the font engine; 18 px of text measures ~21 px of line
/// box in the default theme, so this is the conservative side of it.
const ROW_MIN_HEIGHT: f32 = BUTTON_SIZE;
/// Gap between buttons; the window's padding is twice it.
const GAP: f32 = 6.0;

/// The keypad as it is drawn: `(label, addressing name, key)`.
///
/// One table rather than twenty builder calls, so the layout and the
/// behaviour cannot drift apart. The names are what `hey` addresses: a
/// digit is its own digit, and a symbol gets a word, because `window/+`
/// is not a path anyone wants to quote in a shell.
///
/// An empty entry is not a gap: it means **the previous key spans this
/// cell**. `=` is two cells wide, and [`span_of`] reads that off the
/// table, so the keypad stays a grid whose every cell is the same size
/// rather than a last row with three children dividing four cells'
/// worth of width.
const KEYPAD: [[(&str, &str, Key); 4]; 5] = [
    [
        ("C", "clear", Key::Clear),
        ("⌫", "backspace", Key::Backspace),
        ("±", "negate", Key::Negate),
        ("÷", "divide", Key::Op(Op::Div)),
    ],
    [
        ("7", "7", Key::Digit(7)),
        ("8", "8", Key::Digit(8)),
        ("9", "9", Key::Digit(9)),
        ("×", "times", Key::Op(Op::Mul)),
    ],
    [
        ("4", "4", Key::Digit(4)),
        ("5", "5", Key::Digit(5)),
        ("6", "6", Key::Digit(6)),
        ("−", "minus", Key::Op(Op::Sub)),
    ],
    [
        ("1", "1", Key::Digit(1)),
        ("2", "2", Key::Digit(2)),
        ("3", "3", Key::Digit(3)),
        ("+", "plus", Key::Op(Op::Add)),
    ],
    [
        ("0", "0", Key::Digit(0)),
        (".", "point", Key::Dot),
        ("=", "equals", Key::Equals),
        ("", "", Key::Equals),
    ],
];

/// Columns in the keypad, read off the table rather than declared.
const COLS: usize = KEYPAD[0].len();
/// Rows in the keypad.
const ROWS: usize = KEYPAD.len();

/// How many cells the key at `(row, col)` spans.
///
/// One, plus every empty entry that follows it — that is what an empty
/// entry in [`KEYPAD`] *means*. So `=` is two cells, and a keypad that
/// grew a triple-width key would need no change here.
fn span_of(row: usize, col: usize) -> usize {
    let mut n = 1;
    while col + n < COLS && KEYPAD[row][col + n].1.is_empty() {
        n += 1;
    }
    n
}

/// The width of one keypad cell: the widest glyph in the table, plus the
/// padding a button puts around its label.
///
/// This is what makes the buttons *uniform*. `grow` divides the
/// **leftover** space equally; it does not equalise sizes, so buttons
/// left at their natural basis stay apart by exactly the difference
/// between their glyphs — `⌫` measures twice what `C` does. Giving every
/// button the same basis is the only way a row of them is a grid.
///
/// Measured from [`KEYPAD`] itself rather than written down, so a key
/// with a wider glyph widens every cell automatically and there is no
/// constant to drift.
fn cell_size<S: 'static>(ui: &mut Ui<S>) -> f32 {
    let style = button_style(ui);
    let mut widest: f32 = 0.0;
    for line in KEYPAD {
        for (text, name, _) in line {
            if name.is_empty() {
                continue;
            }
            let m = ui.measure_text(text, &style, 0.0).unwrap_or_default();
            widest = widest.max(m.width);
        }
    }
    let pad = ui.theme().button_padding.0 * 2.0;
    (widest + pad).ceil()
}

/// The text style a keypad button draws its label in.
fn button_style<S: 'static>(ui: &mut Ui<S>) -> TextStyle {
    let mut style = TextStyle::from_theme(ui.theme());
    style.size_px = BUTTON_SIZE;
    style
}

/// The text style the display draws its number in.
///
/// The same family, size and weight `build` gives that label. Two places
/// rather than one because the builder takes them as separate setters;
/// a measurement in a different style would be a floor for a string
/// nobody draws.
fn display_style() -> TextStyle {
    TextStyle {
        family: "mono".to_owned(),
        size_px: DISPLAY_SIZE,
        weight: 600,
        italic: false,
    }
}

/// The narrowest the display may be and still show a whole *fixed*
/// result: [`engine::DIGITS`] zeros in the display's own style.
///
/// Fifteen significant digits is what the engine documents it will print
/// without inventing precision, and what the entry caps at, so a window
/// this wide never elides a number the user typed or a fixed-notation
/// result.
///
/// An **exponential** result (`-2.32305722891176e+56`, 22 characters) is
/// wider than that and does elide at the minimum — deliberately. Sizing
/// the floor to the widest string the formatter can ever emit would put
/// it near 400 px for a case the user fixes by dragging the window, and
/// an ellipsis is the honest signal the toolkit provides for exactly
/// this. Nothing scriptable loses by it either: `Label::accessible`
/// reports the whole text regardless of what is painted, so `hey
/// nitro-calc get window/display value` still answers in full.
fn display_min_width<S: 'static>(ui: &mut Ui<S>) -> f32 {
    let zeros = "0".repeat(engine::DIGITS);
    ui.measure_text(&zeros, &display_style(), 0.0)
        .unwrap_or_default()
        .width
        .ceil()
}

/// The smallest this calculator can honestly be.
///
/// Derived from the same constants the tree is built from — the keypad
/// table's own shape, [`ROW_MIN_HEIGHT`], [`GAP`], and the two labels
/// measured through the font engine — so it cannot drift out of step
/// with the layout the way a written-down pair would.
///
/// Declared to the server in [`build`], because only the server can
/// refuse the drag: a client that merely clamped its own layout would
/// draw a letterbox inside a window the user is still shrinking.
///
/// Public because the tests assert against the app's *own* number rather
/// than a copy of it — a copy is a constant that drifts.
#[must_use]
pub fn min_window<S: 'static>(ui: &mut Ui<S>) -> Size {
    let cell = cell_size(ui);
    let pad = GAP * 4.0; // the root's padding, both sides
    let cols = COLS as f32;
    let keypad_w = cell * cols + GAP * (cols - 1.0);
    let w = keypad_w.max(display_min_width(ui)) + pad;

    let history_h = ui
        .measure_text(
            "0",
            &TextStyle::new(ui.theme().font_family.clone(), HISTORY_SIZE),
            0.0,
        )
        .unwrap_or_default()
        .height;
    let display_h = ui
        .measure_text("0", &display_style(), 0.0)
        .unwrap_or_default()
        .height;
    let rows = ROWS as f32;
    // `ROWS + 2` children in the root column, so `ROWS + 1` gaps.
    let h = pad + history_h + display_h + rows * ROW_MIN_HEIGHT + GAP * (rows + 1.0);
    Size::new(w.ceil(), h.ceil())
}

/// Build the whole tree and return its root.
///
/// Public because the tests build the tree the binary builds: a test that
/// built its own would be testing a second calculator.
///
/// # Panics
/// Never in practice — every `attach` names an id this function has just
/// created, and a fresh id cannot be stale.
pub fn build(ui: &mut Ui<Calc>) -> WidgetId {
    let cell = cell_size(ui);
    let dmin = display_min_width(ui);
    let history = ui.build(
        label("")
            .name("history")
            .size(HISTORY_SIZE)
            .color_role(ColorRole::TextDim)
            // Eliding, not wrapping. A non-eliding label given less
            // width than its text takes **more height** — it wraps to a
            // second line and pushes the keypad's bottom row out of the
            // window, which is issue #614 arriving by a second route
            // that a window minimum computed from one-line labels does
            // not close. An eliding label is one line by definition.
            .elide(true)
            .align(Align::Right)
            .width_percent(1.0),
    );
    let display = ui.build(
        label("0")
            .name("display")
            .family("mono")
            .size(DISPLAY_SIZE)
            .weight(600)
            .elide(true)
            // Eliding opts a label out of the content floor, so say what
            // the floor *is*: fifteen digits, the number the engine
            // promises. See `display_min_width`.
            .min_width(dmin)
            .align(Align::Right)
            .width_percent(1.0),
    );
    let screen = Screen { display, history };

    // A plain `column()` root: the keyboard is handled by app-level key
    // handlers below, not by a widget, so nothing here reimplements a
    // container's `measure`.
    let root = ui.build(column().gap(GAP).padding(GAP * 2.0));
    ui.attach(root, history).unwrap();
    ui.attach(root, display).unwrap();

    for (y, line) in KEYPAD.into_iter().enumerate() {
        // `shrink_to_zero` on the **row**, matching the `grow` on the
        // buttons inside it: this keypad is elastic by construction. The
        // buttons divide whatever width there is, and `ROW_HEIGHT` is
        // the height a comfortable tap target wants rather than the
        // height its glyphs need — 44 px around an 18 px label. So a
        // keypad squeezed into a short window is honestly a smaller
        // keypad, which is what the toolkit's default floor (never
        // smaller than measured; overflow and clip instead) assumes it
        // is not. A row of text would keep the floor; a grid of tap
        // targets gives it up, and the buttons follow the row because
        // their height is a percentage of it.
        //
        // `min_height` puts the floor back at a *defensible* place
        // rather than at zero: opting out of the content floor means
        // "smaller is honest", not "arbitrarily small is honest". It is
        // a floor per *row*, though, and a floor no window respects is
        // not a floor at all — the window minimum `build` declares
        // (see `min_window`) is what stops the window being dragged
        // below the sum of these. See `ROW_MIN_HEIGHT`.
        let r = ui.build(
            row()
                .gap(GAP)
                .height(ROW_HEIGHT)
                .shrink_to_zero()
                .min_height(ROW_MIN_HEIGHT)
                .width_percent(1.0),
        );
        for (x, (text, name, k)) in line.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            let span = span_of(y, x);
            let span_f = span as f32;
            let b = ui.build(
                button(text)
                    .name(name)
                    .size(BUTTON_SIZE)
                    // The two halves of "a grid", and both are needed.
                    //
                    // The explicit `width` gives every button the *same*
                    // basis, which its own glyph would not: `grow`
                    // divides the leftover, so buttons that start
                    // different stay different by exactly that much.
                    //
                    // `grow` proportional to the span is what honours a
                    // `Configure`: the buttons divide whatever width the
                    // window has, so a resize stretches the keypad
                    // instead of leaving a gap — and a two-cell key
                    // takes exactly two cells' worth of the stretch, so
                    // `=` stays `2 * cell + GAP` at every size.
                    .width(cell * span_f + GAP * (span_f - 1.0))
                    .grow(span_f)
                    .height_percent(1.0)
                    .on_click(move |s: &mut Calc, ui: &mut Ui<Calc>| screen.press(s, ui, k)),
            );
            ui.attach(r, b).unwrap();
        }
        ui.attach(root, r).unwrap();
    }

    // Only the server can refuse a drag, so the minimum has to be
    // *declared* rather than clamped locally. It rides the window's
    // first commit: `build` runs before the window exists, which is the
    // case `Ui::set_window_limits` documents. Zero maximum — no upper
    // limit, a calculator is happy as big as you like.
    let min = min_window(ui);
    let _ = ui.set_window_limits(min, Size::ZERO);
    install_keyboard(ui, screen);
    root
}

/// The keyboard, as app-level handlers.
///
/// A key no widget took is offered to these, in registration order, so
/// the toolkit's own bindings are untouched: `Tab` never reaches a
/// handler at all, `Space` on a focused button is consumed by the button,
/// and a text field added to this tree later would keep its own digits.
///
/// The digits match on `ev.text` — the characters the press produced —
/// because the server has already applied the keymap, so one arm covers
/// every layout. Enter, Backspace and Escape produce no text worth
/// matching on, so they are [`Ui::set_shortcut`]s on their keycodes.
///
/// Quitting is `Ctrl+Q`, and it is deliberately *modified*: a bare letter
/// that closes the app is one stray keystroke away from throwing a sum
/// away, and keys do land in the wrong window (a launcher trigger races
/// its own grab). Escape is not a second quit — here it means `Clear`,
/// which is the stronger convention for a calculator and the one the
/// shortcut above already implements.
fn install_keyboard(ui: &mut Ui<Calc>, screen: Screen) {
    for (code, k) in [
        (key::ENTER, Key::Equals),
        (key::BACKSPACE, Key::Backspace),
        (key::ESC, Key::Clear),
    ] {
        ui.set_shortcut(mods::NONE, code, move |s: &mut Calc, ui: &mut Ui<Calc>| {
            screen.press(s, ui, k);
        });
    }
    ui.set_shortcut(mods::CTRL, key::Q, |_s: &mut Calc, ui: &mut Ui<Calc>| {
        ui.quit();
    });
    ui.on_key(move |s: &mut Calc, ui: &mut Ui<Calc>, ev: &KeyEvent| {
        match Key::from_text(&ev.text) {
            Some(k) => {
                screen.press(s, ui, k);
                Handled::Yes
            }
            None => Handled::No,
        }
    });
}

/// Connect, open the window and run until the app quits.
///
/// The binary is this one call; everything else in the crate is a
/// library so the tests can build the *same* tree rather than a copy of
/// it.
///
/// # Errors
/// Any connection, wire or `epoll` failure. They are all fatal.
pub fn run() -> Result<(), Error> {
    // No explicit size: the window is sized by the tree, so the keypad's
    // rows fix the height and the widest button row the width, and there
    // is no constant here to drift out of step with the layout. The
    // *minimum* is derived the same way rather than declared — see
    // `min_window`, which `build` sends to the server.
    App::new(APP_NAME)?
        .title("Calculator")
        .run(Calc::new(), build)
}

/// The name the app registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-calc";
