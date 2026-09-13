//! The M2 widget set and the introspection socket, through the harness:
//! a real server, a real client, real input and real pixels.

use nitro_core::{Color, Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{
    Checkbox, Scroll, Separator, Slider, TextField, button, checkbox, column, label, panel, scroll,
    separator, slider, text_field,
};
use nitro_ui::{Ui, WidgetId};

/// Exact float equality, spelled out.
///
/// These are not computed approximations: a stepped slider's value is a
/// number the widget snapped, and a scroll offset is one it clamped, so
/// "is it the value we asked for" really is a bit comparison — the same
/// rule `wire.rs` uses to decide whether a property changed.
#[track_caller]
fn same(got: f32, want: f32) {
    assert_eq!(got.to_bits(), want.to_bits(), "got {got}, want {want}");
}

/// The two children of a root built by the closure below.
fn kids<S: 'static>(h: &mut Harness<S>) -> Vec<WidgetId> {
    let root = h.ui().root().unwrap();
    h.ui().children(root)
}

#[test]
fn a_text_field_types_deletes_and_reports_its_value() {
    struct S {
        seen: Vec<String>,
    }
    let mut h = Harness::sized(
        "field",
        S { seen: Vec::new() },
        Size::new(260.0, 60.0),
        |ui: &mut Ui<S>| {
            let field = ui.build(
                text_field("ab")
                    .name("input")
                    .placeholder("type here")
                    .on_change(|s: &mut S, _ui: &mut Ui<S>, t: &str| s.seen.push(t.to_owned())),
            );
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            ui.attach(root, field).unwrap();
            root
        },
    );
    let field = kids(&mut h)[0];
    assert_eq!(h.widget::<TextField<S>>(field).text(), "ab");
    // The caret starts at the end, which is where a click-free `set_text`
    // leaves it.
    assert_eq!(h.widget::<TextField<S>>(field).cursor(), 2);

    h.click(field);
    assert_eq!(h.ui().focused(), Some(field), "a click focuses the field");

    // Backspace removes the character before the caret.
    h.key(key::BACKSPACE);
    assert_eq!(h.widget::<TextField<S>>(field).text(), "a");
    assert_eq!(
        h.state().seen,
        ["a"],
        "on_change fired once, with the new text"
    );

    // Home, then Delete removes the first character.
    h.key(key::HOME);
    assert_eq!(h.widget::<TextField<S>>(field).cursor(), 0);
    h.key(key::DELETE);
    assert_eq!(h.widget::<TextField<S>>(field).text(), "");
    assert_eq!(h.state().seen, ["a", ""]);

    // Deleting at the end is a no-op, not a panic and not an event.
    h.key(key::DELETE);
    h.key(key::BACKSPACE);
    assert_eq!(h.state().seen.len(), 2, "no-op edits report nothing");
}

#[test]
fn shift_arrows_select_and_ctrl_a_selects_everything() {
    let mut h = Harness::sized("select", (), Size::new(260.0, 60.0), |ui: &mut Ui<()>| {
        let field = ui.build(text_field("hello"));
        let root = ui.build(panel().background(Color::WHITE).padding(8.0));
        ui.attach(root, field).unwrap();
        root
    });
    let field = kids(&mut h)[0];
    h.click(field);
    h.key(key::END);
    assert_eq!(h.widget::<TextField<()>>(field).selection(), (5, 5));

    // Shift-Left twice selects the last two characters.
    h.key_down(key::LEFT_SHIFT);
    h.key(key::LEFT);
    h.key(key::LEFT);
    h.key_up(key::LEFT_SHIFT);
    assert_eq!(h.widget::<TextField<()>>(field).selection(), (3, 5));
    assert_eq!(h.widget::<TextField<()>>(field).selected_text(), "lo");

    // Backspace over a selection removes the selection, not one char.
    h.key(key::BACKSPACE);
    assert_eq!(h.widget::<TextField<()>>(field).text(), "hel");

    // Ctrl-A selects everything, and typing over it replaces the lot.
    h.key_down(key::LEFT_CTRL);
    h.key(key::A);
    h.key_up(key::LEFT_CTRL);
    assert_eq!(h.widget::<TextField<()>>(field).selection(), (0, 3));
}

#[test]
fn a_checkbox_toggles_by_click_and_by_space_and_paints_differently() {
    struct S {
        toggles: Vec<bool>,
    }
    let mut h = Harness::sized(
        "check",
        S {
            toggles: Vec::new(),
        },
        Size::new(200.0, 60.0),
        |ui: &mut Ui<S>| {
            let box_ = ui.build(
                checkbox("Enabled")
                    .name("flag")
                    .on_toggle(|s: &mut S, _ui: &mut Ui<S>, v: bool| s.toggles.push(v)),
            );
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            ui.attach(root, box_).unwrap();
            root
        },
    );
    let id = kids(&mut h)[0];
    assert!(!h.widget::<Checkbox<S>>(id).is_checked());
    let bounds = h.bounds(id);
    let before = h.ink_count(bounds, 0x00ff_ffff);

    h.click(id);
    assert!(h.widget::<Checkbox<S>>(id).is_checked());
    assert_eq!(h.state().toggles, [true]);
    let after = h.ink_count(bounds, 0x00ff_ffff);
    assert_ne!(before, after, "a checked box looks different");

    // Space toggles the focused box back.
    h.key(key::SPACE);
    assert!(!h.widget::<Checkbox<S>>(id).is_checked());
    assert_eq!(h.state().toggles, [true, false]);
}

#[test]
fn a_slider_follows_the_pointer_and_the_arrow_keys() {
    struct S {
        values: Vec<f32>,
    }
    let mut h = Harness::sized(
        "slider",
        S { values: Vec::new() },
        Size::new(220.0, 60.0),
        |ui: &mut Ui<S>| {
            let s = ui.build(
                slider(0.0)
                    .name("volume")
                    .range(0.0, 100.0)
                    .step(1.0)
                    .width(160.0)
                    .on_change(|s: &mut S, _ui: &mut Ui<S>, v: f32| s.values.push(v)),
            );
            let root = ui.build(panel().background(Color::WHITE).padding(8.0));
            ui.attach(root, s).unwrap();
            root
        },
    );
    let id = kids(&mut h)[0];
    same(h.widget::<Slider<S>>(id).value(), 0.0);

    // Clicking the middle of the track puts it near the middle of the
    // range; the exact value depends on the knob width, so the assertion
    // is a band rather than a number.
    let b = h.bounds(id);
    h.click_at(Point::new(b.x + b.w / 2.0, b.y + b.h / 2.0));
    let mid = h.widget::<Slider<S>>(id).value();
    assert!(
        (40.0..=60.0).contains(&mid),
        "clicked the middle, got {mid}"
    );
    assert_eq!(h.state().values.len(), 1);

    // The step snaps it to whole numbers.
    same(mid.fract(), 0.0);

    // Arrow keys move by one step; Home and End go to the ends.
    h.key(key::RIGHT);
    same(h.widget::<Slider<S>>(id).value(), mid + 1.0);
    h.key(key::LEFT);
    same(h.widget::<Slider<S>>(id).value(), mid);
    h.key(key::END);
    same(h.widget::<Slider<S>>(id).value(), 100.0);
    h.key(key::HOME);
    same(h.widget::<Slider<S>>(id).value(), 0.0);
}

#[test]
fn scrolling_is_one_set_transform_and_nothing_else() {
    // The design claim this checks: scrolling moves a group, it does not
    // re-lay-out or repaint anything. Counting mutations is the only way
    // to check that from the outside.
    let mut h = Harness::sized("scroll", (), Size::new(200.0, 80.0), |ui: &mut Ui<()>| {
        let inner = ui.build(
            column()
                .gap(4.0)
                .children((0..12).map(|i| label(format!("line {i}")))),
        );
        let view = ui.build(scroll().height(60.0));
        ui.attach(view, inner).unwrap();
        let root = ui.build(panel().background(Color::WHITE).padding(8.0));
        ui.attach(root, view).unwrap();
        root
    });
    let view = kids(&mut h)[0];
    same(h.widget::<Scroll>(view).offset(), 0.0);
    assert!(
        h.widget::<Scroll>(view).max_offset() > 0.0,
        "the content is taller than the viewport"
    );

    h.tap();
    h.clear_tap();
    h.ui().widget_mut::<Scroll>(view).unwrap().scroll_by(20.0);
    h.settle();
    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert_eq!(
        ops,
        ["SetTransform", "Commit"],
        "scrolling is one SetTransform and the commit that carries it"
    );
    same(h.widget::<Scroll>(view).offset(), 20.0);

    // And it is clamped: scrolling past the end stops at the end and
    // sends nothing more.
    let max = h.widget::<Scroll>(view).max_offset();
    h.ui().widget_mut::<Scroll>(view).unwrap().scroll_to(1e6);
    h.settle();
    same(h.widget::<Scroll>(view).offset(), max);
    h.clear_tap();
    h.ui().widget_mut::<Scroll>(view).unwrap().scroll_to(1e6);
    h.settle();
    assert!(
        h.mutations().is_empty(),
        "a scroll that changes nothing sends nothing: {:?}",
        h.mutations()
    );
}

#[test]
fn the_wheel_scrolls_the_viewport_under_the_pointer() {
    let mut h = Harness::sized("wheel", (), Size::new(200.0, 80.0), |ui: &mut Ui<()>| {
        let inner = ui.build(
            column()
                .gap(4.0)
                .children((0..12).map(|i| label(format!("row {i}")))),
        );
        let view = ui.build(scroll().height(60.0).speed(30.0));
        ui.attach(view, inner).unwrap();
        let root = ui.build(panel().background(Color::WHITE).padding(8.0));
        ui.attach(root, view).unwrap();
        root
    });
    let view = kids(&mut h)[0];
    let b = h.bounds(view);
    h.move_pointer(Point::new(b.x + b.w / 2.0, b.y + b.h / 2.0));
    h.wheel(-1.0);
    same(h.widget::<Scroll>(view).offset(), 30.0);
    // Scrolling back up past the top stops at zero.
    h.wheel(1.0);
    h.wheel(1.0);
    same(h.widget::<Scroll>(view).offset(), 0.0);
}

#[test]
fn a_separator_draws_a_line_across_its_container() {
    let mut h = Harness::sized("sep", (), Size::new(200.0, 80.0), |ui: &mut Ui<()>| {
        let line = ui.build(separator().color(Color::rgb(0, 0, 0)));
        let root = ui.build(panel().background(Color::WHITE).padding(8.0).gap(6.0));
        ui.attach(root, line).unwrap();
        root
    });
    let line = kids(&mut h)[0];
    let b = h.bounds(line);
    assert!(b.w > 100.0, "a separator spans its container: {b:?}");
    assert!(b.h > 0.0 && b.h <= 4.0, "and is thin: {b:?}");
    assert!(h.has_ink(b, 0x00ff_ffff), "it is actually drawn");
    assert_eq!(
        h.ui().role(line).unwrap(),
        nitro_ui::Role::Separator,
        "and says what it is"
    );
    let _ = h.widget::<Separator>(line);
}

#[test]
fn an_image_draws_the_pixels_it_was_given() {
    // A 8x8 solid red block: the assertion is that red pixels appear
    // where the widget is, which needs no font and no rasterizer detail.
    let px: Vec<u8> = (0..8 * 8).flat_map(|_| [0u8, 0, 255, 255]).collect();
    let mut h = Harness::sized("image", (), Size::new(80.0, 80.0), |ui: &mut Ui<()>| {
        let img = ui.build(nitro_ui::widgets::image(8, 8, px.clone()));
        let root = ui.build(panel().background(Color::WHITE).padding(8.0));
        ui.attach(root, img).unwrap();
        root
    });
    let img = kids(&mut h)[0];
    let b = h.bounds(img);
    assert_eq!((b.w, b.h), (8.0, 8.0), "sized to its pixels");
    let shot = h.shot();
    let px = shot.pixel(b.x as u32 + 4, b.y as u32 + 4);
    assert_eq!(px & 0x00ff_ffff, 0x00ff_0000, "the red block is drawn");
}

#[test]
fn the_window_paints_the_theme_background_by_default() {
    // Issue #531: a root that paints nothing left the theme's dark text
    // on the desktop, where it was barely readable.
    let mut h = Harness::sized("backdrop", (), Size::new(120.0, 80.0), |ui: &mut Ui<()>| {
        // A bare column paints nothing of its own.
        ui.build(column())
    });
    h.settle();
    let theme = nitro_ui::Theme::default();
    let want = u32::from(theme.background.r) << 16
        | u32::from(theme.background.g) << 8
        | u32::from(theme.background.b);
    let shot = h.shot();
    for (x, y) in [(0u32, 0u32), (119, 0), (0, 79), (119, 79), (60, 40)] {
        assert_eq!(
            shot.pixel(x, y) & 0x00ff_ffff,
            want,
            "the window background covers ({x}, {y})"
        );
    }
}

#[test]
fn a_transparent_window_paints_no_background() {
    let mut h = Harness::with_options(
        "clear",
        (),
        Some(Size::new(120.0, 80.0)),
        nitro_ui::Theme::default(),
        false,
        |ui: &mut Ui<()>| ui.build(column()),
    );
    h.settle();
    let theme = nitro_ui::Theme::default();
    let background = u32::from(theme.background.r) << 16
        | u32::from(theme.background.g) << 8
        | u32::from(theme.background.b);
    let shot = h.shot();
    assert_ne!(
        shot.pixel(2, 2) & 0x00ff_ffff,
        background,
        "asked for transparent, got the theme's background"
    );
}

#[test]
fn tab_reaches_every_new_widget_and_focus_shows() {
    let mut h = Harness::sized("focus", (), Size::new(280.0, 200.0), |ui: &mut Ui<()>| {
        ui.build(
            column()
                .gap(6.0)
                .padding(8.0)
                .child(text_field("").name("f"))
                .child(checkbox("c").name("c"))
                .child(slider(0.5).name("s").width(120.0))
                .child(button("b").name("b")),
        )
    });
    h.settle();
    let order = h.ui().focus_order();
    assert_eq!(order.len(), 4, "every new widget is focusable");
    for expected in &order {
        h.key(key::TAB);
        assert_eq!(h.ui().focused(), Some(*expected));
    }
    // And it wraps.
    h.key(key::TAB);
    assert_eq!(h.ui().focused(), Some(order[0]));
}

#[test]
fn a_scrolled_child_is_clicked_and_reported_where_it_now_is() {
    // `Scroll` moves its children with a transform and never touches
    // their bounds — that is what makes scrolling one mutation. Hit
    // testing and `window_bounds` must therefore account for it, or a
    // scrolled button is clickable where it *used* to be and `hey` reports
    // a rectangle that is no longer on screen.
    struct S {
        hits: Vec<u32>,
    }
    let mut h = Harness::sized(
        "scrollhit",
        S { hits: Vec::new() },
        Size::new(200.0, 120.0),
        |ui: &mut Ui<S>| {
            let inner = ui.build(column().gap(0.0).children((0..6).map(|i| {
                button(format!("b{i}"))
                    .name(format!("b{i}"))
                    .height(20.0)
                    .on_click(move |s: &mut S, _ui: &mut Ui<S>| s.hits.push(i))
            })));
            let view = ui.build(scroll().height(60.0).name("view"));
            ui.attach(view, inner).unwrap();
            let root = ui.build(panel().background(Color::WHITE).padding(0.0));
            ui.attach(root, view).unwrap();
            root
        },
    );
    let view = kids(&mut h)[0];
    let inner = h.ui().children(view)[0];
    let rows = h.ui().children(inner);
    assert_eq!(rows.len(), 6);

    // Unscrolled: the top row is at the top of the viewport.
    let top = h.bounds(rows[0]);
    h.click_at(Point::new(top.x + 5.0, top.y + 5.0));
    assert_eq!(h.state().hits, [0], "unscrolled, the first row is on top");

    // Scroll by two rows. Row 2 is now where row 0 was.
    h.ui().widget_mut::<Scroll>(view).unwrap().scroll_to(40.0);
    h.settle();
    same(h.widget::<Scroll>(view).offset(), 40.0);

    assert_eq!(
        h.bounds(rows[2]),
        top,
        "a scrolled child reports the rectangle it is actually drawn at"
    );
    // The pointer has to actually move for the hover chain to be walked
    // again: scrolling moves the content under a stationary pointer, and
    // nothing re-hit-tests until the next pointer event. That is a real
    // limitation (recorded in docs/ui.md), not an artefact of the test —
    // without this line the click would still be routed to row 0.
    h.move_pointer(Point::new(top.x + 5.0, top.y + 15.0));
    h.click_at(Point::new(top.x + 5.0, top.y + 5.0));
    assert_eq!(
        h.state().hits,
        [0, 2],
        "the click lands on the row that is now under the pointer"
    );

    // And the row scrolled off the top is no longer hit at all.
    assert!(
        h.bounds(rows[0]).y < top.y,
        "row 0 moved up out of the viewport: {:?}",
        h.bounds(rows[0])
    );
}

#[test]
fn replacing_an_image_releases_the_buffer_it_replaced() {
    // Without the release, an app that updates one image per frame leaks
    // a server-side buffer per frame for its whole lifetime. Counting
    // mutations is how that claim is checked from outside.
    let red: Vec<u8> = (0..8 * 8).flat_map(|_| [0u8, 0, 255, 255]).collect();
    let blue: Vec<u8> = (0..8 * 8).flat_map(|_| [255u8, 0, 0, 255]).collect();
    let mut h = Harness::sized("imgswap", (), Size::new(80.0, 80.0), |ui: &mut Ui<()>| {
        let img = ui.build(nitro_ui::widgets::image(8, 8, red.clone()));
        let root = ui.build(panel().background(Color::WHITE).padding(8.0));
        ui.attach(root, img).unwrap();
        root
    });
    let img = kids(&mut h)[0];
    let first = h
        .widget::<nitro_ui::widgets::Image>(img)
        .buffer()
        .expect("the first buffer was uploaded");

    h.tap();
    h.clear_tap();
    h.ui()
        .widget_mut::<nitro_ui::widgets::Image>(img)
        .unwrap()
        .set_pixels(8, 8, blue.clone());
    h.settle();

    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(
        ops.contains(&"CreateBuffer"),
        "the new pixels were uploaded: {ops:?}"
    );
    assert!(
        ops.contains(&"DestroyBuffer"),
        "the replaced buffer was released: {ops:?}"
    );
    let second = h
        .widget::<nitro_ui::widgets::Image>(img)
        .buffer()
        .expect("the second buffer");
    assert_ne!(second, first, "a replacement is a new buffer id");

    // The new pixels really are on screen.
    let b = h.bounds(img);
    let px = h.shot().pixel(b.x as u32 + 4, b.y as u32 + 4);
    assert_eq!(px & 0x00ff_ffff, 0x0000_00ff, "the blue block is drawn");
}
