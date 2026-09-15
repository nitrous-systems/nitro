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

pub mod engine;

use engine::{Engine, Key, Op};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{Handled, KeyEvent, key, mods};
use nitro_ui::widgets::{Label, button, column, label, row};
use nitro_ui::{Align, App, ColorRole, Error, Ui, WidgetId};

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
/// Gap between buttons; the window's padding is twice it.
const GAP: f32 = 6.0;

/// The keypad as it is drawn: `(label, addressing name, key)`.
///
/// One table rather than twenty builder calls, so the layout and the
/// behaviour cannot drift apart. The names are what `hey` addresses: a
/// digit is its own digit, and a symbol gets a word, because `window/+`
/// is not a path anyone wants to quote in a shell. An empty name is the
/// hole the double-width `=` leaves in the last row.
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

/// Build the whole tree and return its root.
///
/// Public because the tests build the tree the binary builds: a test that
/// built its own would be testing a second calculator.
///
/// # Panics
/// Never in practice — every `attach` names an id this function has just
/// created, and a fresh id cannot be stale.
pub fn build(ui: &mut Ui<Calc>) -> WidgetId {
    let history = ui.build(
        label("")
            .name("history")
            .size(HISTORY_SIZE)
            .color_role(ColorRole::TextDim)
            .align(Align::Right)
            .width_percent(1.0),
    );
    let display = ui.build(
        label("0")
            .name("display")
            .family("mono")
            .size(DISPLAY_SIZE)
            .weight(600)
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

    for line in KEYPAD {
        // `shrink_to_zero` on the **row**, matching the `grow(1.0)` on the
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
        let r = ui.build(
            row()
                .gap(GAP)
                .height(ROW_HEIGHT)
                .shrink_to_zero()
                .width_percent(1.0),
        );
        for (text, name, k) in line {
            if name.is_empty() {
                continue;
            }
            let b = ui.build(
                button(text)
                    .name(name)
                    .size(BUTTON_SIZE)
                    // `grow` is what honours a `Configure`: the buttons
                    // divide whatever width the window has, so a resize
                    // stretches the keypad instead of leaving a gap.
                    .grow(1.0)
                    .height_percent(1.0)
                    .on_click(move |s: &mut Calc, ui: &mut Ui<Calc>| screen.press(s, ui, k)),
            );
            ui.attach(r, b).unwrap();
        }
        ui.attach(root, r).unwrap();
    }

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
    ui.on_key(move |s: &mut Calc, ui: &mut Ui<Calc>, ev: &KeyEvent| {
        if ev.text == "q" {
            ui.quit();
            return Handled::Yes;
        }
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
    // is no constant here to drift out of step with the layout.
    App::new(APP_NAME)?
        .title("Calculator")
        .run(Calc::new(), build)
}

/// The name the app registers under, and so the first argument to `hey`.
pub const APP_NAME: &str = "nitro-calc";
