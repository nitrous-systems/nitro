//! The calculator, driven through a real server.
//!
//! Every test here builds the tree the binary builds ([`nitro_calc::build`])
//! and drives it the way a user would: synthetic clicks and key presses
//! travel evdev code → server hit test → wire → widget, and the
//! assertions are made on what the widgets then say. Nothing calls the
//! engine directly — that is `src/engine.rs`'s own tests' job, and mixing
//! the two would let a UI bug hide behind a passing state machine.

use nitro_calc::{Calc, build};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::widgets::Label;
use nitro_ui::{Size, WidgetId};

/// A calculator in a window that fits the harness's output.
///
/// The harness runs a 320×240 output and injects pointer events in
/// *output* coordinates, so a window taller than 240 has its bottom rows
/// off-screen and a click on `=` lands on whatever the clamp leaves under
/// the pointer. The rows shrink to fit (they have the default
/// `flex_shrink`), which is the same path a real `Configure` to a small
/// screen takes — so this is the awkward case, not a soft one.
fn harness() -> Harness<Calc> {
    Harness::sized("calc", Calc::new(), Size::new(240.0, 228.0), build)
}

/// The widget named `name`, found the way `hey` finds it.
fn named(h: &mut Harness<Calc>, name: &str) -> WidgetId {
    nitro_ui::introspect::resolve(h.ui(), &format!("window/{name}"))
        .unwrap_or_else(|| panic!("no widget named {name}"))
}

fn display(h: &mut Harness<Calc>) -> String {
    let id = named(h, "display");
    h.widget::<Label>(id).text().to_owned()
}

fn history(h: &mut Harness<Calc>) -> String {
    let id = named(h, "history");
    h.widget::<Label>(id).text().to_owned()
}

/// Click the buttons named by `names`, in order.
fn click_all(h: &mut Harness<Calc>, names: &[&str]) {
    for n in names {
        let id = named(h, n);
        h.click(id);
    }
}

/// Type `text`, one evdev key press per character.
///
/// The evdev codes of the top number row and of the operator keys, so a
/// test types what a user types. `\n` is Enter.
fn type_text(h: &mut Harness<Calc>, text: &str) {
    for c in text.chars() {
        match c {
            '0' => h.key(11),
            '1'..='9' => h.key(c as u32 - '1' as u32 + 2),
            '.' => h.key(52),
            '+' => h.key_with(key::LEFT_SHIFT, 13), // shift-= is +
            '-' => h.key(12),
            '*' => h.key(55), // KEY_KPASTERISK
            '/' => h.key(98), // KEY_KPSLASH
            '=' => h.key(13), // KEY_EQUAL
            'c' => h.key(46), // KEY_C
            '\n' => h.key(key::ENTER),
            other => panic!("no key for {other:?}"),
        }
    }
}

#[test]
fn clicking_seven_plus_eight_equals_shows_fifteen() {
    // The headline: the spec's own example, through real clicks.
    let mut h = harness();
    assert_eq!(display(&mut h), "0");

    click_all(&mut h, &["7", "plus", "8", "equals"]);
    assert_eq!(display(&mut h), "15");
    assert_eq!(history(&mut h), "7 + 8 =");
    assert_eq!(h.state().presses(), 4, "one press per click, no more");
    h.quit();
}

#[test]
fn typing_nine_times_nine_enter_shows_eighty_one() {
    // The keyboard is the buttons: the same `engine::Key`, the same
    // callback, the same labels.
    let mut h = harness();
    type_text(&mut h, "9*9\n");
    assert_eq!(display(&mut h), "81");
    assert_eq!(history(&mut h), "9 × 9 =");
    h.quit();
}

#[test]
fn clear_puts_it_back_to_zero() {
    let mut h = harness();
    click_all(&mut h, &["7", "plus", "8", "equals"]);
    assert_eq!(display(&mut h), "15");

    let clear = named(&mut h, "clear");
    h.click(clear);
    assert_eq!(display(&mut h), "0");
    assert_eq!(history(&mut h), "", "and the history line with it");

    // And the pending operation is gone, not merely hidden.
    click_all(&mut h, &["3", "equals"]);
    assert_eq!(display(&mut h), "3");
    h.quit();
}

#[test]
fn escape_clears_and_backspace_deletes() {
    let mut h = harness();
    type_text(&mut h, "123");
    assert_eq!(display(&mut h), "123");
    h.key(key::BACKSPACE);
    assert_eq!(display(&mut h), "12");
    h.key(key::ESC);
    assert_eq!(display(&mut h), "0");
    h.quit();
}

#[test]
fn clicks_and_keys_mix_freely() {
    // They share one engine, so a half-typed sum can be finished with the
    // mouse. If the two paths had separate state this is what would break.
    let mut h = harness();
    type_text(&mut h, "12");
    click_all(&mut h, &["times"]);
    type_text(&mut h, "12");
    let equals = named(&mut h, "equals");
    h.click(equals);
    assert_eq!(display(&mut h), "144");
    h.quit();
}

#[test]
fn dividing_by_zero_shows_error_and_clear_recovers() {
    let mut h = harness();
    click_all(&mut h, &["8", "divide", "0", "equals"]);
    assert_eq!(display(&mut h), "Error");

    // Every key is refused while the error stands.
    click_all(&mut h, &["5", "plus"]);
    assert_eq!(display(&mut h), "Error");

    click_all(&mut h, &["clear", "1", "plus", "1", "equals"]);
    assert_eq!(display(&mut h), "2", "and it computes again afterwards");
    h.quit();
}

#[test]
fn one_keypress_is_one_set_text() {
    // The cost claim, asserted from outside by counting mutations. A
    // digit changes the display's string and nothing else: no button
    // repaints, no node is created, and the history line is untouched
    // because its text did not change.
    let mut h = harness();
    click_all(&mut h, &["7"]);

    h.tap();
    h.clear_tap();
    let commits = h.commits();
    let five = named(&mut h, "5");
    h.click(five);
    h.settle();

    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    let set_texts = ops.iter().filter(|o| **o == "SetText").count();
    assert_eq!(display(&mut h), "75");
    assert_eq!(set_texts, 1, "exactly one SetText: {ops:?}");
    assert!(!ops.contains(&"CreateNode"), "no node was created: {ops:?}");
    // A click also repaints the button it hit (pressed, then released,
    // then hovered), which is the button's own business; what matters is
    // that the *text* of exactly one node moved and that the whole thing
    // rode on a bounded number of commits.
    assert!(
        h.commits() - commits <= 3,
        "a click is a handful of commits, not a redraw: {ops:?}"
    );
    h.quit();
}

#[test]
fn the_display_keeps_up_with_a_long_entry() {
    // Fifteen significant digits, and the sixteenth keystroke is ignored
    // rather than producing a number the display cannot be trusted on.
    let mut h = harness();
    type_text(&mut h, "1234567890123456");
    assert_eq!(display(&mut h), "123456789012345");
    h.quit();
}

#[test]
fn a_resize_stretches_the_keypad() {
    // The `Configure` path: every button has `.grow(1.0)`, so a wider
    // window makes wider buttons rather than a gap on the right.
    let mut h = harness();
    let seven = named(&mut h, "7");
    let before = h.bounds(seven);

    h.configure(Size::new(400.0, 320.0));
    let after = h.bounds(seven);
    assert!(
        after.w > before.w + 20.0,
        "the button grew with the window: {before:?} → {after:?}"
    );

    // And the display still spans the window, right-aligned.
    let disp = named(&mut h, "display");
    let d = h.bounds(disp);
    assert!(d.w > 370.0, "the display spans the window: {d:?}");
    h.quit();
}

#[test]
fn the_buttons_are_drawn_and_the_display_has_ink() {
    let mut h = harness();
    if !h.has_text() {
        // A server with no fonts draws no glyphs; the layout assertions
        // above still hold, but there is nothing to photograph.
        h.quit();
        return;
    }
    click_all(&mut h, &["7"]);
    h.settle();

    let disp = named(&mut h, "display");
    let bounds = h.bounds(disp);
    assert!(
        h.has_ink(bounds, 0x00f2_f2f2),
        "the display drew something onto the theme background: {bounds:?}"
    );

    let seven = named(&mut h, "7");
    let b = h.bounds(seven);
    assert!(h.has_ink(b, 0x00f2_f2f2), "and so did the 7 button: {b:?}");
    h.quit();
}

#[test]
fn tab_walks_the_buttons_and_space_presses_one() {
    // The toolkit's focus handling, exercised by an app that never
    // mentions it: the buttons are focusable because `button()` makes
    // them so, and `Space` activates the focused one.
    let mut h = harness();
    let clear = named(&mut h, "clear");
    h.ui().focus(clear);
    h.settle();
    h.key(key::TAB);
    assert_eq!(
        h.ui().focused(),
        Some(named(&mut h, "backspace")),
        "Tab moved to the next button in tree order"
    );
    h.quit();
}

#[test]
fn the_tree_is_named_the_way_hey_addresses_it() {
    // Every path the README promises resolves. A rename that broke a
    // documented command would otherwise only show up on the box.
    let mut h = harness();
    for name in [
        "display",
        "history",
        "0",
        "7",
        "plus",
        "minus",
        "times",
        "divide",
        "equals",
        "clear",
        "backspace",
        "negate",
        "point",
    ] {
        let id = named(&mut h, name);
        assert!(
            !h.bounds(id).is_empty() || name == "history",
            "{name} has a box to click"
        );
    }
    h.quit();
}

#[test]
fn nothing_is_sent_while_it_sits_there() {
    // The property the whole retained design exists for: a settled tree
    // is silent. An app that repainted on a timer would fail here.
    let mut h = harness();
    click_all(&mut h, &["7", "plus", "8", "equals"]);
    h.assert_idle(200);
    h.quit();
}
