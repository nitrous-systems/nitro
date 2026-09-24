//! A secret `TextField`: the text never leaves the process.
//!
//! "Never leaves" is checked where it can be checked completely: every
//! byte the client wrote to the server socket, recorded at the one
//! place all of them pass (`Connection::flush`). A plain field typed in
//! the same window is the control: it proves the check can find typed
//! text in that stream, so the secret field's absence means something.

use nitro_core::{Color, Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{SECRET_MASK, TextField, column, panel, text_field};
use nitro_ui::{Ui, WidgetId};

/// evdev keycodes for the letters the tests type. The harness's server
/// runs a real xkb keymap, so these arrive as `Text` events exactly as a
/// keyboard's would.
mod letter {
    pub const Q: u32 = 16;
    pub const W: u32 = 17;
    pub const T: u32 = 20;
    pub const Y: u32 = 21;
    pub const J: u32 = 36;
    pub const K: u32 = 37;
    pub const Z: u32 = 44;
    pub const X: u32 = 45;
    pub const M: u32 = 50;
}

/// Typed into the secret field: `qwzxty`.
const SECRET_KEYS: [u32; 6] = [
    letter::Q,
    letter::W,
    letter::Z,
    letter::X,
    letter::T,
    letter::Y,
];
const SECRET: &str = "qwzxty";

/// Typed into the plain control field: `mjk`, which shares no letter
/// with the secret, so neither can be mistaken for the other.
const PLAIN_KEYS: [u32; 3] = [letter::M, letter::J, letter::K];
const PLAIN: &str = "mjk";

#[derive(Default)]
struct S {
    /// Every value the secret field's `on_change` reported.
    changes: Vec<String>,
    /// What `on_submit` was handed.
    submitted: Option<String>,
}

/// A window with a secret field named `pw` above a plain one named
/// `plain`.
fn harness(name: &str) -> (Harness<S>, WidgetId, WidgetId) {
    let mut h = Harness::sized(
        name,
        S::default(),
        Size::new(260.0, 120.0),
        |ui: &mut Ui<S>| {
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            let col = ui.build(
                column()
                    .gap(6.0)
                    .child(
                        text_field("")
                            .name("pw")
                            .secret()
                            .on_change(|s: &mut S, _ui: &mut Ui<S>, t: &str| {
                                s.changes.push(t.to_owned());
                            })
                            .on_submit(|s: &mut S, _ui: &mut Ui<S>, t: &str| {
                                s.submitted = Some(t.to_owned());
                            }),
                    )
                    .child(text_field("").name("plain")),
            );
            ui.attach(root, col).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let col = h.ui().children(root)[0];
    let kids = h.ui().children(col);
    (h, kids[0], kids[1])
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn mask(n: usize) -> String {
    std::iter::repeat_n(SECRET_MASK, n).collect()
}

#[test]
fn typed_secret_text_never_reaches_the_server() {
    let (mut h, pw, plain) = harness("secret-wire");
    h.tap();

    h.click(plain);
    for k in PLAIN_KEYS {
        h.key(k);
    }
    h.click(pw);
    for k in SECRET_KEYS {
        h.key(k);
    }
    h.key(key::ENTER);
    h.settle();

    // The app has the text, and so do its callbacks.
    assert_eq!(h.widget::<TextField<S>>(pw).text(), SECRET);
    assert_eq!(h.widget::<TextField<S>>(plain).text(), PLAIN);
    assert_eq!(h.state().changes.last().map(String::as_str), Some(SECRET));
    assert_eq!(h.state().submitted.as_deref(), Some(SECRET));

    let sent = h.sent_bytes();
    // The control: typed text does show up in this stream.
    assert!(
        contains(sent, PLAIN.as_bytes()),
        "the plain field's text is not in the recorded bytes, so the \
         recording is not seeing what the client sends ({} bytes)",
        sent.len()
    );
    // The field's mask did go out, which is what stands in for it.
    assert!(
        contains(sent, mask(SECRET.len()).as_bytes()),
        "the secret field painted nothing the server saw"
    );
    // And no part of the secret did: not the whole, and not any three
    // consecutive characters of it (a partially typed prefix is sent
    // too, one keystroke at a time).
    for part in SECRET.as_bytes().windows(3) {
        assert!(
            !contains(sent, part),
            "{:?} of the secret reached the server",
            String::from_utf8_lossy(part)
        );
    }
    h.quit();
}

#[test]
fn introspection_reports_the_mask_and_can_write_but_not_unmask() {
    let (mut h, pw, _) = harness("secret-hey");
    let path = introspect::path_of(h.ui(), pw).expect("the field is addressable");

    // `set_value` is how a test (or `nitro-hey`) types a password.
    let (ui, state) = h.parts();
    introspect::invoke(ui, state, &path, "set_value", Some("s3cr3t!")).unwrap();
    h.settle();
    assert_eq!(h.widget::<TextField<S>>(pw).text(), "s3cr3t!");

    // `get … value` and `get … text` are the mask, one per character.
    assert_eq!(
        introspect::get_prop(h.ui(), &path, "value").unwrap(),
        mask(7)
    );
    assert_eq!(
        introspect::get_prop(h.ui(), &path, "text").unwrap(),
        mask(7)
    );
    // So is the walk `list` and `watch` read from.
    let mut nodes = Vec::new();
    h.ui().introspect(&mut nodes);
    let node = nodes
        .iter()
        .find(|n| n.id == pw)
        .expect("the field is in the walk");
    assert_eq!(node.access.value.as_deref(), Some(mask(7).as_str()));
    for n in &nodes {
        assert!(
            !n.access
                .value
                .as_deref()
                .unwrap_or_default()
                .contains("s3cr3t"),
            "{:?} exposes the secret",
            n.id
        );
    }

    // There is no action that turns the mode off.
    let (ui, state) = h.parts();
    assert_eq!(
        introspect::invoke(ui, state, &path, "set_secret", Some("false")),
        Err("unknown action `set_secret`".to_owned())
    );
    assert_eq!(
        introspect::get_prop(h.ui(), &path, "value").unwrap(),
        mask(7)
    );

    // `clear` empties it.
    let (ui, state) = h.parts();
    introspect::invoke(ui, state, &path, "clear", None).unwrap();
    assert_eq!(h.widget::<TextField<S>>(pw).text(), "");
    h.quit();
}

#[test]
fn debug_output_does_not_print_the_text() {
    let (mut h, pw, _) = harness("secret-debug");
    h.ui()
        .widget_mut::<TextField<S>>(pw)
        .unwrap()
        .set_text("hunter2");
    let dbg = format!("{:?}", h.widget::<TextField<S>>(pw));
    assert!(!dbg.contains("hunter2"), "{dbg}");
    assert!(dbg.contains("<secret, 7 chars>"), "{dbg}");
    h.quit();
}

#[test]
fn editing_keys_work_on_characters_not_on_mask_bytes() {
    // The mask is three bytes per character and the text is not, so
    // every caret position has to be translated between the two. Mixed
    // widths make a wrong translation visible: `ä` is two bytes, `b`
    // one, `€` three.
    let (mut h, pw, _) = harness("secret-edit");
    h.click(pw);
    h.ui()
        .widget_mut::<TextField<S>>(pw)
        .unwrap()
        .set_text("äb€");
    h.settle();
    assert_eq!(h.widget::<TextField<S>>(pw).cursor(), "äb€".len());

    h.key(key::LEFT);
    assert_eq!(h.widget::<TextField<S>>(pw).cursor(), "äb".len());
    h.key(key::BACKSPACE);
    assert_eq!(h.widget::<TextField<S>>(pw).text(), "ä€");
    assert_eq!(h.widget::<TextField<S>>(pw).cursor(), "ä".len());
    h.key(key::DELETE);
    assert_eq!(h.widget::<TextField<S>>(pw).text(), "ä");

    // Click-to-place goes through the server's cursor table for the
    // mask and back: the far left is offset 0, the far right the end.
    h.ui()
        .widget_mut::<TextField<S>>(pw)
        .unwrap()
        .set_text("äb€");
    h.settle();
    let b = h.bounds(pw);
    h.click_at(Point::new(b.x + 2.0, b.y + b.h / 2.0));
    assert_eq!(h.widget::<TextField<S>>(pw).cursor(), 0);
    h.click_at(Point::new(b.x + b.w - 2.0, b.y + b.h / 2.0));
    assert_eq!(h.widget::<TextField<S>>(pw).cursor(), "äb€".len());
    h.quit();
}

#[test]
fn a_click_between_two_characters_lands_between_them() {
    // The one path where the translation shows: the server's cursor
    // table is for the mask (three bytes per character), and a click
    // resolves to a mask offset that has to become a text offset. Aimed
    // at every boundary, it must land on the text's own boundaries,
    // `[0, 2, 3, 6]` for `äb€`, and not on the mask's `[0, 3, 6, 9]`.
    let (mut h, pw, _) = harness("secret-click");
    h.ui()
        .widget_mut::<TextField<S>>(pw)
        .unwrap()
        .set_text("äb€");
    h.settle();
    let style = nitro_ui::theme::TextStyle::from_theme(h.ui().theme());
    let pad = h.ui().theme().button_padding.0;
    let table = h.ui().cursor_positions(&mask(3), &style).unwrap();
    let xs: Vec<f32> = table.iter().map(|&(_, x)| x).collect();
    assert_eq!(
        xs.len(),
        4,
        "a boundary per character plus the start: {table:?}"
    );

    let b = h.bounds(pw);
    let want = [0, "ä".len(), "äb".len(), "äb€".len()];
    for (x, want) in xs.iter().zip(want) {
        h.click_at(Point::new(b.x + pad + x, b.y + b.h / 2.0));
        assert_eq!(
            h.widget::<TextField<S>>(pw).cursor(),
            want,
            "click at mask x {x}"
        );
    }
    h.quit();
}

#[test]
fn the_caret_scrolls_by_the_mask_not_by_the_text() {
    // The other direction of the translation: a caret offset in the text
    // must be found in the mask's cursor table. `End` on a secret longer
    // than the field scrolls the view so the caret sits at the right
    // edge, which is where the *mask* ends, three bytes per character
    // later than the text does.
    let (mut h, pw, _) = harness("secret-scroll");
    let long = "q".repeat(60);
    h.click(pw);
    h.ui()
        .widget_mut::<TextField<S>>(pw)
        .unwrap()
        .set_text(long.as_str());
    h.settle();
    h.key(key::HOME);
    h.key(key::END);

    let style = nitro_ui::theme::TextStyle::from_theme(h.ui().theme());
    let pad = h.ui().theme().button_padding.0;
    let table = h.ui().cursor_positions(&mask(60), &style).unwrap();
    let end_x = table.last().expect("a non-empty table").1;
    let view = h.bounds(pw).w - pad * 2.0;
    assert!(
        end_x > view,
        "the secret must overflow the field for this test"
    );
    let got = h.widget::<TextField<S>>(pw).scroll_offset();
    assert!(
        (got - (end_x - view)).abs() < 0.01,
        "scrolled {got}, want {}",
        end_x - view
    );
    h.quit();
}

#[test]
fn set_secret_switches_the_same_field_between_questions() {
    // A login conversation asks for a name (shown), then a password
    // (masked), in one field.
    let (mut h, pw, _) = harness("secret-switch");
    let path = introspect::path_of(h.ui(), pw).unwrap();
    {
        let mut f = h.ui().widget_mut::<TextField<S>>(pw).unwrap();
        f.set_secret(false);
        f.set_text("alice");
    }
    h.settle();
    assert!(!h.widget::<TextField<S>>(pw).is_secret());
    assert_eq!(
        introspect::get_prop(h.ui(), &path, "value").unwrap(),
        "alice"
    );

    h.tap();
    {
        let mut f = h.ui().widget_mut::<TextField<S>>(pw).unwrap();
        f.set_text("");
        f.set_secret(true);
        f.set_text("qwzxty");
    }
    h.settle();
    assert!(h.widget::<TextField<S>>(pw).is_secret());
    assert_eq!(
        introspect::get_prop(h.ui(), &path, "value").unwrap(),
        mask(6)
    );
    let sent = h.sent_bytes();
    assert!(contains(sent, mask(6).as_bytes()), "the mask went out");
    assert!(!contains(sent, b"qwzxty"), "and the text did not");
    h.quit();
}
