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
use nitro_ui::event::{Event, Handled, key};
use nitro_ui::widgets::{Label, button, label, row};
use nitro_ui::{
    Align, App, Built, Constraints, Error, EventCx, FlexItem, MeasureCx, Role, Size, Ui, Widget,
    WidgetId,
};

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
/// into the [`Keyboard`] widget — which is how a typed `7` and a clicked
/// `7` end up running the identical three lines of [`Screen::press`].
/// Keeping the ids here rather than in [`Calc`] is what lets the state be
/// built before the tree it will drive.
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
    let dim = ui.theme().text_disabled;
    let history = ui.build(
        label("")
            .name("history")
            .size(HISTORY_SIZE)
            .color(dim)
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

    // The root *is* the keyboard handler; see [`Keypad`].
    let mut root = Built::new(Keypad { screen });
    {
        let style = &mut root.state_mut().style;
        style.gap = GAP;
        style.padding = nitro_ui::Edges::all(GAP * 2.0);
    }
    let root = ui.build(root);
    ui.attach(root, history).unwrap();
    ui.attach(root, display).unwrap();

    for line in KEYPAD {
        let r = ui.build(row().gap(GAP).height(ROW_HEIGHT).width_percent(1.0));
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
    root
}

/// The root: a column that also reads the keyboard.
///
/// It holds the same [`Screen`] the buttons hold, so `7` on the keyboard
/// and `7` on the screen run the same code with the same invalidation.
///
/// **Why the root and not an invisible child.** A key goes to the focused
/// widget and bubbles *upward* to the root, so the last widget to see an
/// unclaimed key is the root itself — a sibling of the root's other
/// children is never on that path and would never be offered anything.
/// (`docs/ui.md` and `examples/hello_dialog.rs` show the shortcut as a
/// zero-sized child instead; that is issue-worthy and is filed, but a
/// calculator whose digits do not type is not the place to find out.)
///
/// Handling keys here rather than filtering them is also what leaves the
/// toolkit's own bindings alone: `Tab` never reaches a widget at all, and
/// `Space` on a focused button is consumed by the button long before it
/// bubbles this far.
struct Keypad {
    screen: Screen,
}

impl Widget<Calc> for Keypad {
    /// A column's intrinsic size: the flex solver's own arithmetic over
    /// the children, which is exactly what [`Flex`](nitro_ui::widgets::Flex)
    /// does. The default `layout` already lays children out with the
    /// solver, so this is the only pass a container has to write.
    fn measure(&mut self, cx: &mut MeasureCx<'_, Calc>, constraints: Constraints) -> Size {
        let style = cx.ui.style(cx.id);
        let inner = constraints.loosen().deflate(style.padding);
        let mut items = Vec::new();
        for c in cx.children() {
            let cstyle = cx.ui.style(c);
            let basis = cx.measure_child(c, inner.deflate(cstyle.margin));
            items.push(FlexItem::new(cstyle, basis));
        }
        let main = nitro_ui::layout::intrinsic_main(&style, &items);
        let cross = nitro_ui::layout::intrinsic_cross(&style, &items);
        let size = style.direction.size(main, cross);
        constraints.constrain(Size::new(
            size.w + style.padding.horizontal(),
            size.h + style.padding.vertical(),
        ))
    }

    fn event(&mut self, cx: &mut EventCx<'_, Calc>, ev: &Event) -> Handled {
        match ev {
            // A printing character comes back as `Text` once no widget
            // took the `KeyDown`, which is how one arm covers every
            // keyboard layout: the server has already applied the keymap.
            Event::Text { text } => {
                if text == "q" {
                    cx.ui.quit();
                    return Handled::Yes;
                }
                match Key::from_text(text) {
                    Some(k) => {
                        self.screen.press(cx.state, cx.ui, k);
                        Handled::Yes
                    }
                    None => Handled::No,
                }
            }
            // Enter, Backspace and Escape produce no text worth matching
            // on, so they go by keycode.
            Event::KeyDown(k) => {
                let pressed = match k.keycode {
                    key::ENTER => Key::Equals,
                    key::BACKSPACE => Key::Backspace,
                    key::ESC => Key::Clear,
                    _ => return Handled::No,
                };
                self.screen.press(cx.state, cx.ui, pressed);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Container
    }
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
