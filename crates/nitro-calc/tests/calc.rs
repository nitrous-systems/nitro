//! The calculator, driven through a real server.
//!
//! Every test here builds the tree the binary builds ([`nitro_calc::build`])
//! and drives it the way a user would: synthetic clicks and key presses
//! travel evdev code → server hit test → wire → widget, and the
//! assertions are made on what the widgets then say. Nothing calls the
//! engine directly — that is `src/engine.rs`'s own tests' job, and mixing
//! the two would let a UI bug hide behind a passing state machine.

use nitro_calc::{Calc, build, min_window};
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
///
/// The width is the window's own declared minimum (277 px; see
/// [`nitro_calc::min_window`]), because since #3783 a narrower window is
/// one the server would refuse to let the user drag to — a test opening
/// it would be testing a state that no longer exists. The *height* is
/// still well short of what the tree wants, which is the awkwardness
/// this harness is here for.
fn harness() -> Harness<Calc> {
    Harness::sized("calc", Calc::new(), Size::new(277.0, 228.0), build)
}

/// Every keypad button's addressing name, in tree order.
///
/// The grid assertions want *all* of them rather than a sample: a button
/// that was the odd one out is exactly the one a sample would miss.
const BUTTONS: [&str; 19] = [
    "clear",
    "backspace",
    "negate",
    "divide",
    "7",
    "8",
    "9",
    "times",
    "4",
    "5",
    "6",
    "minus",
    "1",
    "2",
    "3",
    "plus",
    "0",
    "point",
    "equals",
];

/// The gap between buttons, and half the window's padding. Mirrors the
/// `GAP` the tree is built with; a copy because it is private there, and
/// the assertions below would be circular if they read it from the code
/// under test.
const GAP: f32 = 6.0;

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

/// The laid-out box of the widget named `name`.
fn box_of(h: &mut Harness<Calc>, name: &str) -> nitro_ui::Rect {
    let id = named(h, name);
    h.bounds(id)
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
fn a_bare_q_does_not_quit_and_ctrl_q_does() {
    // A bare letter that closes the app is one stray keystroke away from
    // throwing a sum away, and keys really do land in the wrong window:
    // a launcher trigger races the grab it asks for, so a query typed
    // fast enough after the tap goes to whatever was focused. That is how
    // a real calculator exited on its own (#3713). So `q` is inert, and
    // quitting is spelled `Ctrl+Q`.
    let mut h = harness();
    type_text(&mut h, "12");
    h.key(key::Q);
    assert!(!h.ui().should_quit(), "a bare q must not quit");
    assert_eq!(display(&mut h), "12", "and must not touch the entry either");

    h.key_with(key::LEFT_CTRL, key::Q);
    assert!(h.ui().should_quit(), "Ctrl+Q quits");
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
    // The cost claim, asserted from outside by counting mutations.
    //
    // A *keyboard* digit is the clean measurement: it changes the
    // display's string and touches nothing else, where a click also
    // repaints the button it hit (pressed, released, hovered), which is
    // the button's own business rather than the app's.
    let mut h = harness();
    type_text(&mut h, "7");

    h.tap();
    h.clear_tap();
    let commits = h.commits();
    type_text(&mut h, "5");
    h.settle();

    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert_eq!(display(&mut h), "75");
    assert_eq!(
        ops,
        ["SetText", "Commit"],
        "a keypress is one SetText and the commit that carries it"
    );
    assert_eq!(h.commits() - commits, 1, "and exactly one commit");

    // And the converse: `=` here settles `75` to `75`, so the *display*
    // is the label that does not move and only the history line is sent.
    // One `SetText`, not two — which is the no-op setter earning its
    // keep, since `press` offers both labels a string every time.
    h.clear_tap();
    let equals = named(&mut h, "equals");
    h.click(equals);
    h.settle();
    let set_texts = h.mutations().iter().filter(|m| m.op == "SetText").count();
    assert_eq!(display(&mut h), "75", "the display did not change");
    assert_eq!(history(&mut h), "75 =", "but the history did");
    assert_eq!(
        set_texts,
        1,
        "only the label whose text actually changed was sent: {:?}",
        h.mutations()
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

#[test]
fn every_field_survives_the_minimum_window() {
    // Issue #614: the calculator could be dragged small enough that
    // fields vanished. The window now declares a minimum, so the
    // interesting question is whether *that* size is actually habitable
    // — a minimum the tree does not fit in is a lie, not a fix.
    let mut h = harness();
    let min = min_window(h.ui());
    h.configure(min);
    h.settle();

    let check = |h: &mut Harness<Calc>, when: &str| {
        for name in BUTTONS.iter().copied().chain(["display", "history"]) {
            let b = box_of(h, name);
            assert!(
                b.w > 0.0 && b.h > 0.0,
                "{name} has no box at the declared minimum {min:?} ({when}): {b:?}"
            );
        }
        // And the bottom row is *inside* the window, not merely non-zero:
        // a keypad pushed past the edge is the same bug with a box.
        let e = box_of(h, "equals");
        assert!(
            e.y + e.h <= min.h,
            "the last keypad row bottoms out inside the window ({when}): \
             {e:?} in {min:?}"
        );
    };
    check(&mut h, "empty");

    // The half that fails without eliding labels. Fifteen digits is what
    // the engine allows, and a label that wrapped them to a second line
    // would make the *column* taller — pushing the keypad's bottom row
    // out of a window the user cannot make any bigger by shrinking. A
    // minimum computed from one-line labels does not close that; the
    // labels being one line by construction does.
    type_text(&mut h, "123456789012345");
    h.settle();
    assert_eq!(display(&mut h), "123456789012345");
    check(&mut h, "after a fifteen-digit entry");

    // And with the history line at its longest too: two full entries and
    // an operator is the widest string this calculator can put up there.
    type_text(&mut h, "*123456789012345=");
    h.settle();
    assert_eq!(history(&mut h), "123456789012345 × 123456789012345 =");
    check(&mut h, "with a full history line");
    h.quit();
}

#[test]
fn the_keypad_is_a_grid() {
    // "The button sizes are non-uniform, they should all have the same
    // size" — the other half of #614.
    //
    // `grow` divides the *leftover* space equally; it does not equalise
    // sizes. Left at their natural basis the buttons stayed apart by
    // exactly the difference between their glyphs (`⌫` measures twice
    // what `C` does), and the last row had three children dividing what
    // the others split four ways. So: one explicit width for every cell,
    // and `=` a real two-cell span rather than a row with a different
    // child count.
    //
    // Checked at several sizes, not just the default: the equal basis is
    // what makes them equal at rest, and `grow` proportional to the span
    // is what keeps them equal under a `Configure`. A fix that only did
    // the first would pass at one width and fail at every other.
    let mut h = harness();
    let min = min_window(h.ui());
    for size in [
        min,
        Size::new(320.0, 240.0),
        Size::new(500.0, 400.0),
        Size::new(300.0, 700.0),
    ] {
        h.configure(size);
        h.settle();

        let cell = box_of(&mut h, "7");
        for name in BUTTONS {
            if name == "equals" {
                continue;
            }
            let b = box_of(&mut h, name);
            assert!(
                (b.w - cell.w).abs() < 0.01 && (b.h - cell.h).abs() < 0.01,
                "at {size:?} the {name} button is {b:?}, not the {cell:?} \
                 every other cell is"
            );
        }

        // `=` is two cells and the gap it bridges — not "a bit wider",
        // and not a third of a row.
        let eq = box_of(&mut h, "equals");
        let want = cell.w * 2.0 + GAP;
        assert!(
            (eq.w - want).abs() < 0.01,
            "at {size:?} `=` spans {:?}, not two cells plus the gap ({want})",
            eq.w
        );
        assert!(
            (eq.h - cell.h).abs() < 0.01,
            "at {size:?} `=` is {:?} tall, not a cell's {:?}",
            eq.h,
            cell.h
        );

        // And the row it is in ends where the others do: a span that
        // overshot would be uniform and still wrong.
        let divide = box_of(&mut h, "divide");
        assert!(
            (eq.x + eq.w - (divide.x + divide.w)).abs() < 0.01,
            "at {size:?} the last row ends at {} and the first at {}",
            eq.x + eq.w,
            divide.x + divide.w
        );
    }
    h.quit();
}

#[test]
fn the_window_declares_its_tree_as_its_minimum() {
    // The fix for "it can be resized until the fields vanish" is a
    // minimum the *server* enforces: a client that merely clamped its
    // own layout would draw a letterbox inside a window the user is
    // still shrinking.
    //
    // The assertion leans on a property of `Ui::set_window_limits` that
    // makes it a real test rather than a restatement: **re-declaring the
    // same limits sends nothing.** So a second call with exactly
    // `(min_window, no maximum)` producing no message is proof that
    // those are the limits already on the wire — it cannot pass if the
    // calculator declared a smaller minimum, a different maximum, or
    // none at all. A control follows: different limits *do* produce a
    // message, so the silence above is the de-duplication and not a dead
    // tap.
    //
    // What this does not claim is that the server honours them; that is
    // `a_resize_respects_the_limits_the_client_declared` in
    // `crates/nitro-server/tests/wm.rs`, which drives a real edge drag.
    let mut h = harness();
    h.settle();
    let min = min_window(h.ui());

    // No maximum: a calculator is happy as big as the screen allows.
    let no_max = Size::ZERO;
    h.tap();
    h.ui()
        .set_window_limits(min, no_max)
        .expect("re-declare the same limits");
    h.flush();
    assert!(
        !h.mutations().iter().any(|m| m.op == "SetWindowLimits"),
        "re-declaring the same limits sends nothing, so the calculator \
         had already declared min={min:?} max={no_max:?}; it sent {:?}",
        h.mutations(),
    );

    // The control: the tap is live and this path does emit.
    h.ui()
        .set_window_limits(Size::new(100.0, 100.0), no_max)
        .expect("declare different limits");
    h.flush();
    assert!(
        h.mutations().iter().any(|m| m.op == "SetWindowLimits"),
        "a *different* minimum does reach the wire, so the silence above \
         was de-duplication rather than a dead tap: {:?}",
        h.mutations(),
    );
    // Put the real limits back: a test that leaves the control's 100×100
    // installed is a trap for whoever adds an assertion after it.
    h.ui()
        .set_window_limits(min, no_max)
        .expect("restore the calculator's own limits");
    h.flush();
    h.ui().tap(false);

    // And the minimum is derived, not declared: it is at least as wide
    // as the keypad's own cells and as tall as its rows' floors, so
    // there is no size at which the tree is asked to fit in less than it
    // measures. Checking it against the *laid-out* keypad is what makes
    // this catch a keypad that grew a row while the number stayed put.
    h.configure(min);
    h.settle();
    let clear = box_of(&mut h, "clear");
    let equals = box_of(&mut h, "equals");
    assert!(
        equals.x + equals.w + GAP * 2.0 <= min.w + 0.01,
        "the minimum width holds the whole keypad: {equals:?} in {min:?}"
    );
    assert!(
        clear.h >= 18.0,
        "and a row at the minimum is still a tappable height: {clear:?}"
    );
    h.quit();
}
