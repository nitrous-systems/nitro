//! App-level key handlers and shortcuts, through the harness.
//!
//! The thing under test is the *ordering*: a handler registered with
//! `Ui::on_key` sees every press the focused chain declined, and nothing
//! else. So each test here puts a real widget in the way — a focused text
//! field that eats its characters, a focused button that does not eat
//! Escape — and asserts which side got the key.
//!
//! Issue #535: the documented pattern (a zero-sized widget hung off the
//! root) could never work, because keys bubble *upward* from the focused
//! widget and a sibling of the root's children is on nobody's ancestor
//! chain.

use nitro_core::{Color, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::{Handled, KeyEvent, key, mods};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Label, TextField, button, column, label, panel, row, spacer, text_field};
use nitro_ui::{Ui, WidgetId};

/// The evdev keycode of `q`, which is what a global shortcut most often
/// wants and what `hello_dialog` documents.
const Q: u32 = key::Q;

/// What a test's handlers recorded, and the tree they were registered on.
#[derive(Default)]
struct Seen {
    keys: Vec<String>,
    order: Vec<u8>,
    field: Option<WidgetId>,
}

#[test]
fn an_unfocused_tree_offers_q_to_the_app_handler() {
    // The exact shape of #535: nothing focused, so the key event starts
    // at the root and bubbles up from there — off the top of the tree.
    // The app handler is the only thing left, and it must fire.
    let mut h = Harness::sized(
        "unfocused",
        Seen::default(),
        Size::new(200.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            let text = ui.build(label("nothing focusable here"));
            ui.attach(root, text).unwrap();
            ui.on_key(|s: &mut Seen, _ui: &mut Ui<Seen>, k: &KeyEvent| {
                s.keys.push(k.text.clone());
                Handled::Yes
            });
            root
        },
    );
    assert_eq!(h.ui().focused(), None, "nothing is focusable in this tree");
    assert_eq!(h.ui().key_handler_count(), 1);

    h.key(Q);
    assert_eq!(h.state().keys, ["q"], "the app handler saw the key");
    h.quit();
}

#[test]
fn a_focused_text_field_keeps_its_q() {
    // The reason app handlers run *after* the focused chain rather than
    // before it: a shortcut must not steal a character from a widget
    // that is typing.
    let mut h = Harness::sized(
        "field-wins",
        Seen::default(),
        Size::new(260.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let field = ui.build(text_field("a"));
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            ui.attach(root, field).unwrap();
            ui.on_key(|s: &mut Seen, ui: &mut Ui<Seen>, k: &KeyEvent| {
                s.keys.push(k.text.clone());
                ui.quit();
                Handled::Yes
            });
            root
        },
    );
    let root = h.ui().root().unwrap();
    let field = h.ui().children(root)[0];
    h.state_mut().field = Some(field);
    h.click(field);
    assert_eq!(h.ui().focused(), Some(field));

    h.key(Q);
    assert_eq!(
        h.widget::<TextField<Seen>>(field).text(),
        "aq",
        "the field typed the character"
    );
    assert!(
        h.state().keys.is_empty(),
        "and the app handler was never offered it"
    );
    assert!(!h.ui().should_quit(), "so the app did not quit");
    h.quit();
}

#[test]
fn escape_reaches_the_app_handler_past_a_focused_button() {
    // A button consumes Space and Enter and declines everything else, so
    // Escape travels the whole chain and lands on the shortcut — with the
    // focus sitting on a widget, not nowhere.
    let mut h = Harness::sized(
        "escape",
        Seen::default(),
        Size::new(220.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let ok = ui.build(button("OK").on_click(|_: &mut Seen, _: &mut Ui<Seen>| {}));
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            ui.attach(root, ok).unwrap();
            ui.set_shortcut(mods::NONE, key::ESC, |_: &mut Seen, ui: &mut Ui<Seen>| {
                ui.quit();
            });
            root
        },
    );
    let root = h.ui().root().unwrap();
    let ok = h.ui().children(root)[0];
    h.ui().focus(ok);
    h.settle();
    assert_eq!(h.ui().focused(), Some(ok));

    // Space is the button's, and the shortcut must not see it fire.
    h.key(key::SPACE);
    assert!(!h.ui().should_quit(), "the button consumed Space");

    h.key(key::ESC);
    assert!(h.ui().should_quit(), "Escape reached the app shortcut");
    h.quit();
}

#[test]
fn a_shortcut_matches_its_modifiers_exactly() {
    // `mods::NONE` means *no* modifier: a chord is a different shortcut,
    // not the same one with extra bits. The mask leaves Lock and friends
    // out, so Caps Lock cannot disable an app's keys.
    let mut h = Harness::sized(
        "modifiers",
        Seen::default(),
        Size::new(200.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            let text = ui.build(label("."));
            ui.attach(root, text).unwrap();
            ui.set_shortcut(mods::NONE, Q, |s: &mut Seen, _: &mut Ui<Seen>| {
                s.order.push(0);
            });
            ui.set_shortcut(mods::CTRL, Q, |s: &mut Seen, _: &mut Ui<Seen>| {
                s.order.push(1);
            });
            root
        },
    );
    h.key(Q);
    assert_eq!(h.state().order, [0], "plain q ran the plain shortcut");

    h.key_with(key::LEFT_CTRL, Q);
    assert_eq!(
        h.state().order,
        [0, 1],
        "and Ctrl-Q ran the Ctrl one, not the plain one again"
    );
    h.quit();
}

#[test]
fn a_handler_can_take_tab_before_focus_traversal() {
    let mut h = Harness::sized(
        "handled-tab",
        Seen::default(),
        Size::new(200.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let first = ui.build(button("first").on_click(|_: &mut Seen, _: &mut Ui<Seen>| {}));
            let second = ui.build(button("second").on_click(|_: &mut Seen, _: &mut Ui<Seen>| {}));
            let root = ui.build(row().gap(8.0));
            ui.attach(root, first).unwrap();
            ui.attach(root, second).unwrap();
            ui.on_key(|s: &mut Seen, _: &mut Ui<Seen>, k: &KeyEvent| {
                if k.keycode == key::TAB {
                    s.keys.push("tab".into());
                    Handled::Yes
                } else {
                    Handled::No
                }
            });
            root
        },
    );
    let root = h.ui().root().unwrap();
    let first = h.ui().children(root)[0];
    let second = h.ui().children(root)[1];
    h.ui().focus(first);
    h.settle();

    h.key(key::TAB);

    assert_eq!(h.state().keys, ["tab"]);
    assert_eq!(
        h.ui().focused(),
        Some(first),
        "handled Tab must not traverse focus"
    );
    assert_ne!(h.ui().focused(), Some(second));
    h.quit();
}

#[test]
fn handlers_run_in_registration_order_until_one_takes_the_key() {
    let mut h = Harness::sized(
        "order",
        Seen::default(),
        Size::new(200.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            let text = ui.build(label("."));
            ui.attach(root, text).unwrap();
            // First: declines, so the offer continues.
            ui.on_key(|s: &mut Seen, _: &mut Ui<Seen>, _: &KeyEvent| {
                s.order.push(0);
                Handled::No
            });
            // Second: takes it.
            ui.on_key(|s: &mut Seen, _: &mut Ui<Seen>, _: &KeyEvent| {
                s.order.push(1);
                Handled::Yes
            });
            // Third: never runs, because the second one answered.
            ui.on_key(|s: &mut Seen, _: &mut Ui<Seen>, _: &KeyEvent| {
                s.order.push(2);
                Handled::Yes
            });
            root
        },
    );
    assert_eq!(h.ui().key_handler_count(), 3);

    h.key(Q);
    assert_eq!(
        h.state().order,
        [0, 1],
        "in order, stopping at the first Yes"
    );

    // One press is one offer: a release does not run the handlers again,
    // which is what would double-fire a quit.
    h.key(Q);
    assert_eq!(h.state().order, [0, 1, 0, 1], "exactly one round per press");
    h.quit();
}

#[test]
fn a_handler_can_reach_the_whole_tree() {
    // The signature's promise: a handler is handed `&mut S` and `&mut
    // Ui<S>`, exactly like a button's `on_click`, so it can edit widgets
    // rather than only set a flag.
    let mut h = Harness::sized(
        "tree-access",
        Seen::default(),
        Size::new(220.0, 80.0),
        |ui: &mut Ui<Seen>| {
            let text = ui.build(label("before"));
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            ui.attach(root, text).unwrap();
            ui.on_key(move |s: &mut Seen, ui: &mut Ui<Seen>, k: &KeyEvent| {
                s.keys.push(k.text.clone());
                ui.widget_mut::<Label>(text).unwrap().set_text("after");
                Handled::Yes
            });
            root
        },
    );
    let root = h.ui().root().unwrap();
    let text = h.ui().children(root)[0];

    h.key(Q);
    h.settle();
    assert_eq!(h.widget::<Label>(text).text(), "after");
    h.quit();
}

#[test]
fn the_hello_dialog_example_quits_on_q_and_on_escape() {
    /// The same tree and the same shortcuts `examples/hello_dialog.rs`
    /// registers. The example's `q` was dead for the whole of #535
    /// because nothing tested it; this is that test.
    struct State {
        ok: bool,
    }
    fn dialog(ui: &mut Ui<State>) -> WidgetId {
        let message = ui.build(label("Nothing has happened yet.").name("message"));
        let ok = ui.build(button("OK").name("ok").on_click(
            move |s: &mut State, ui: &mut Ui<State>| {
                s.ok = !s.ok;
                let text = if s.ok { "OK pressed." } else { "Toggled back." };
                ui.widget_mut::<Label>(message).unwrap().set_text(text);
            },
        ));
        let root = ui.build(
            column()
                .gap(12.0)
                .padding(16.0)
                .child(label("Hello, nitro").size(20.0).weight(700)),
        );
        let buttons = ui.build(
            row().gap(8.0).child(spacer()).child(
                button("Cancel")
                    .name("cancel")
                    .on_click(|_: &mut State, ui: &mut Ui<State>| ui.quit()),
            ),
        );
        ui.attach(buttons, ok).unwrap();
        ui.attach(root, message).unwrap();
        ui.attach(root, buttons).unwrap();
        ui.set_shortcut(mods::NONE, key::ESC, |_: &mut State, ui: &mut Ui<State>| {
            ui.quit();
        });
        ui.on_key(|_: &mut State, ui: &mut Ui<State>, k: &KeyEvent| {
            if k.text == "q" {
                ui.quit();
                return Handled::Yes;
            }
            Handled::No
        });
        root
    }

    // `q` with nothing focused.
    let mut h = Harness::sized(
        "dialog-q",
        State { ok: false },
        Size::new(300.0, 160.0),
        dialog,
    );
    assert!(!h.ui().should_quit());
    h.key(Q);
    assert!(h.ui().should_quit(), "q quits the dialog");
    h.quit();

    // And Escape with the OK button focused, which is the other half of
    // what the example's doc comment claims.
    let mut h = Harness::sized(
        "dialog-esc",
        State { ok: false },
        Size::new(300.0, 160.0),
        dialog,
    );
    let order = h.ui().focus_order();
    h.ui().focus(order[0]);
    h.settle();
    assert!(h.ui().focused().is_some(), "a button has the focus");
    h.key(key::ESC);
    assert!(h.ui().should_quit(), "Escape quits too");
    h.quit();
}
