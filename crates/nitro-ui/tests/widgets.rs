//! The M2 widget set and the introspection socket, through the harness:
//! a real server, a real client, real input and real pixels.

use nitro_core::{Color, Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{
    Checkbox, Label, Scroll, Separator, Slider, TextField, button, checkbox, column, label, panel,
    row, scroll, separator, slider, text_field,
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
fn a_vertical_slider_has_its_minimum_at_the_bottom() {
    let mut h = Harness::sized("fader", (), Size::new(80.0, 200.0), |ui: &mut Ui<()>| {
        let s = ui.build(
            slider(0.0)
                .name("band")
                .range(-12.0, 12.0)
                .step(1.0)
                .vertical()
                .height(160.0),
        );
        let root = ui.build(panel().background(Color::WHITE).padding(8.0));
        ui.attach(root, s).unwrap();
        root
    });
    let id = kids(&mut h)[0];
    let b = h.bounds(id);
    // Taller than wide: the measured default turned with the track.
    assert!(b.h > b.w, "a vertical slider is laid out tall, got {b:?}");

    // Near the top is near the maximum; near the bottom, the minimum.
    h.click_at(Point::new(b.x + b.w / 2.0, b.y + 2.0));
    same(h.widget::<Slider<()>>(id).value(), 12.0);
    h.click_at(Point::new(b.x + b.w / 2.0, b.y + b.h - 2.0));
    same(h.widget::<Slider<()>>(id).value(), -12.0);

    // Up raises it, as it does on a horizontal one.
    h.key(key::UP);
    same(h.widget::<Slider<()>>(id).value(), -11.0);
    h.key(key::DOWN);
    same(h.widget::<Slider<()>>(id).value(), -12.0);
}

#[test]
fn a_collapsed_section_gives_its_space_back() {
    let mut h = Harness::sized("fold", (), Size::new(200.0, 200.0), |ui: &mut Ui<()>| {
        ui.build(
            column()
                .gap(10.0)
                .child(label("top").name("top"))
                .child(
                    column()
                        .name("section")
                        .child(label("one"))
                        .child(label("two")),
                )
                .child(label("bottom").name("bottom")),
        )
    });
    let [top, section, bottom] = kids(&mut h)[..] else {
        panic!("three children");
    };
    let open = h.bounds(bottom).y;
    assert!(h.bounds(section).h > 0.0);

    h.ui().set_collapsed(section, true);
    h.settle();
    assert!(h.ui().is_collapsed(section));
    assert!(!h.ui().is_visible(section), "a collapsed subtree is hidden");
    // The section and one of the two gaps around it are gone.
    let shut = h.bounds(bottom).y;
    let t = h.bounds(top);
    assert!(
        (shut - (t.y + t.h + 10.0)).abs() < 0.01,
        "bottom sits one gap below top, got {shut}"
    );
    assert!(shut < open);

    h.ui().set_collapsed(section, false);
    h.settle();
    assert!(h.ui().is_visible(section));
    assert!((h.bounds(bottom).y - open).abs() < 0.01);
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
fn a_scroll_actually_paints_its_children() {
    // The M3 launcher bug, at the level it really lived: the rows were
    // in the tree, had bounds, were enabled and launched the right
    // application when clicked — and the screen below the query field
    // was blank.
    //
    // A `Scroll` is a viewport, so it asks the scene to clip its content
    // group. A group is *created* at `Rect::EMPTY` and a bare group
    // paints nothing, so nothing ever sent its bounds — and a clip to an
    // empty rectangle clips every child away. Everything downstream of
    // the clip was right, which is exactly why `hey list` and a
    // screenshot disagreed.
    //
    // Pixels are the only assertion that would have caught it: the model
    // was correct throughout.
    let mut h =
        Harness::sized(
            "scrollink",
            (),
            Size::new(200.0, 120.0),
            |ui: &mut Ui<()>| {
                let inner = ui.build(column().gap(2.0).width_percent(1.0).children((0..3).map(
                    |i| {
                        button::<()>(format!("row {i}"))
                            .name(format!("r{i}"))
                            .height(28.0)
                            .width_percent(1.0)
                    },
                )));
                let view = ui.build(scroll().name("view").grow(1.0).width_percent(1.0));
                ui.attach(view, inner).unwrap();
                let root = ui.build(panel().background(Color::WHITE).padding(8.0));
                ui.attach(root, view).unwrap();
                root
            },
        );
    let view = kids(&mut h)[0];
    let inner = h.ui().children(view)[0];
    let rows = h.ui().children(inner);
    assert_eq!(rows.len(), 3);

    let white = Color::WHITE.to_u32() >> 8;
    for (i, row) in rows.iter().enumerate() {
        let r = h.bounds(*row);
        assert!(r.w > 1.0 && r.h > 1.0, "row {i} has a rect: {r:?}");
        assert!(
            h.ink_count(r, white) > 0,
            "row {i} is in the tree at {r:?} and painted nothing"
        );
    }
}

#[test]
fn a_child_added_to_a_scroll_later_paints_too() {
    // The launcher builds its tree once and fills the list afterwards —
    // the rows arrive on a timer, then on every keystroke. So the clip
    // has to be right when the content group is created *after* the
    // clip was asked for, which is the order an empty `Scroll` produces:
    // `paint` asks for the clip on the first flush, when there is no
    // content group yet to put it on, and the group appears on the flush
    // that attaches the first child.
    //
    // So the viewport is built with **no children at all** — attaching
    // one in the builder would create the group early and quietly test
    // the other order.
    let mut view: Option<WidgetId> = None;
    let mut h = Harness::sized(
        "scrolllate",
        (),
        Size::new(200.0, 200.0),
        |ui: &mut Ui<()>| {
            let v = ui.build(scroll().name("view").height(100.0).width_percent(1.0));
            let root = ui.build(panel().background(Color::WHITE).padding(0.0));
            ui.attach(root, v).unwrap();
            view = Some(v);
            root
        },
    );
    h.settle();
    let view = view.expect("the viewport");

    let inner = h.ui().build(column().gap(0.0).width_percent(1.0));
    h.ui().attach(view, inner).unwrap();
    let row = h.ui().build(
        button::<()>("late")
            .name("late")
            .height(28.0)
            .width_percent(1.0),
    );
    h.ui().attach(inner, row).unwrap();
    h.settle();

    let r = h.bounds(row);
    // Against a reference: a row clipped to an empty rectangle still
    // shows a hairline of border, so "some ink" is not enough — it has
    // to be most of the row.
    let white = Color::WHITE.to_u32() >> 8;
    let painted = h.ink_count(r, white);
    assert!(
        painted > (r.w * r.h) as usize / 2,
        "a row added after the first flush is clipped away at {r:?}: {painted} pixels"
    );
}

#[test]
fn a_viewport_clips_to_the_size_it_has_now() {
    // The clip rectangle is the widget's own, so it has to follow the
    // widget: a viewport that grew would otherwise go on clipping to the
    // rectangle it had when the clip was first asked for, and the rows
    // that arrived in the new space would be invisible in exactly the
    // way the launcher's were.
    //
    // The window is bigger than the viewport throughout, so the crop
    // `Harness::shot` takes is not itself doing the clipping — without
    // that the assertion would pass for the wrong reason.
    let mut h =
        Harness::sized(
            "scrollresize",
            (),
            Size::new(200.0, 200.0),
            |ui: &mut Ui<()>| {
                let inner = ui.build(column().gap(0.0).width_percent(1.0).children((0..4).map(
                    |i| {
                        button::<()>(format!("row {i}"))
                            .name(format!("r{i}"))
                            .height(28.0)
                            .width_percent(1.0)
                    },
                )));
                let view = ui.build(scroll().name("view").height(60.0).width_percent(1.0));
                ui.attach(view, inner).unwrap();
                let root = ui.build(panel().background(Color::WHITE).padding(0.0));
                ui.attach(root, view).unwrap();
                root
            },
        );
    let view = kids(&mut h)[0];
    let inner = h.ui().children(view)[0];
    let rows = h.ui().children(inner);
    let white = Color::WHITE.to_u32() >> 8;

    // 60px of viewport over 4×28px of content: the last row is entirely
    // past the fold, and a viewport that clips shows none of it.
    let last = h.bounds(rows[3]);
    assert!(last.y >= 60.0, "the last row is past the fold: {last:?}");
    let clipped = h.ink_count(last, white);
    let full = h.ink_count(h.bounds(rows[0]), white);
    assert!(
        clipped * 8 < full,
        "the row past the fold is clipped: {clipped} of {full} pixels"
    );

    // Grow the viewport past the content. The clip has to grow with it,
    // or the row stays as invisible as it was.
    let mut style = h.ui().style(view);
    style.height = nitro_ui::layout::Length::Px(140.0);
    h.ui().set_style(view, style);
    h.settle();
    let last = h.bounds(rows[3]);
    assert!(
        h.ink_count(last, white) >= full,
        "a row inside the grown viewport is still clipped at {last:?}"
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

/// Assert that what a label **paints** fits the box it is painted in.
///
/// The property `.elide(true)` actually sells, and the one no test made
/// until #3725's review found it missing. Eliding to the width the label
/// was *offered* rather than the width it was *given* passes every
/// assertion about `painted_text()` — it really does end in `…` — while
/// the scene clips that longer string at the label's own rect, mid-glyph
/// and ellipsis included, which is the hard truncation the ellipsis
/// exists to replace. Measured before the fix: a 122-px box painting a
/// 205-px string.
///
/// So the honest question is not "does it end in an ellipsis" but "does
/// the server say this string fits that rectangle", and this asks the
/// server.
#[track_caller]
fn assert_painted_fits<S: 'static>(h: &mut Harness<S>, id: WidgetId, px: f32) {
    let painted = h.widget::<Label>(id).painted_text().to_owned();
    let theme = nitro_ui::Theme::default();
    let style = nitro_ui::TextStyle::new(theme.font_family.clone(), px);
    let want = h
        .ui()
        .measure_text(&painted, &style, 0.0)
        .expect("measure what is painted")
        .width;
    let box_w = h.bounds(id).w;
    assert!(
        want <= box_w + 0.5,
        "the label paints {painted:?}, which measures {want}, into a \
         {box_w}-px box: the scene clips a run to its item's bounds, so \
         those {:.1} px are cut off mid-glyph and the ellipsis with them",
        want - box_w,
    );
}

#[test]
fn an_eliding_label_shortens_itself_instead_of_overflowing_the_row() {
    // A label is the widget with no smaller honest version of itself,
    // which is why the toolkit's default floor is its measured size — a
    // row of full-width labels overflows rather than squashing them.
    // `.elide(true)` is the opt-in for the label that *does* have one:
    // it drops characters from the end and says so with an ellipsis.
    //
    // The row here is the settings dialog's display row with everything
    // but the shape removed: a long mode string beside a fixed-width
    // control, in a window narrower than the two together. Without
    // elision the row runs past the window (that is #561 working as
    // designed, and what the human saw on the box). With it the label
    // gives the space up, the fixed sibling keeps its width, and nothing
    // leaves the window.
    let long = "1920×1080 @ 119.98 Hz (also 60, 84.904, 59.94, 50, 24, 23.976)";
    let mut h = Harness::sized("elide", (), Size::new(220.0, 60.0), |ui: &mut Ui<()>| {
        let mode = ui.build(label(long).name("mode").size(13.0).elide(true));
        let fixed = ui.build(label("☐ primary").name("fixed").size(13.0).width(80.0));
        let root = ui.build(row().gap(6.0).padding(6.0).width_percent(1.0));
        ui.attach(root, mode).unwrap();
        ui.attach(root, fixed).unwrap();
        root
    });
    if !h.has_text() {
        h.quit();
        return;
    }
    let root = h.ui().root().unwrap();
    let (mode, fixed) = {
        let k = h.ui().children(root);
        (k[0], k[1])
    };
    let size = h.ui().window_size();

    // 1. The row fits: both children end inside the window.
    for (name, id) in [("mode", mode), ("fixed", fixed)] {
        let b = h.bounds(id);
        assert!(
            b.right() <= size.w + 0.01,
            "{name} ends at x={} in a {}-wide window",
            b.right(),
            size.w,
        );
    }

    // 2. It is the *label* that gave way, not the fixed sibling.
    assert!(
        (h.bounds(fixed).w - 80.0).abs() < 0.01,
        "the fixed sibling kept its width: {:?}",
        h.bounds(fixed),
    );

    // 3. And it gave way by eliding rather than by being squashed: what
    //    is painted is a prefix of the text plus `…`, and it really is
    //    shorter than what the whole string measures.
    let painted = h.widget::<Label>(mode).painted_text().to_owned();
    assert!(
        h.widget::<Label>(mode).is_elided(),
        "the label knows it is showing less than it has: {painted:?}"
    );
    assert!(
        painted.ends_with('…') && long.starts_with(painted.trim_end_matches('…')),
        "the painted text is a prefix of the original plus an ellipsis: {painted:?}"
    );
    // `text()` is still the whole string — an accessibility client reads
    // the label, not the width of its box.
    assert_eq!(h.widget::<Label>(mode).text(), long);

    // 4. The prefix that survives is the part that names the thing: the
    //    resolution, not three characters of it.
    assert!(
        painted.starts_with("1920×1080"),
        "the important prefix survives: {painted:?}"
    );

    // 5. And what is painted fits the box it is painted in — the
    //    property the four assertions above do *not* make, and the one
    //    the first version of this feature failed: it elided against the
    //    width the label was offered (208 px, the row's inner width)
    //    rather than the width it was given (~122 px after the solver
    //    took the overflow out of it), so a 205-px string went into a
    //    122-px box and the scene cut it mid-glyph, ellipsis and all.
    assert_painted_fits(&mut h, mode, 13.0);
    h.quit();
}

#[test]
fn an_eliding_label_keeps_an_ellipsis_and_three_characters_at_its_narrowest() {
    // The floor. An eliding label takes part in shrink like a
    // `shrink_to_zero` widget, which on its own would let a crowded row
    // narrow it to nothing — and a label of zero width is not a smaller
    // honest version of itself, it is an absent one. So it reports a
    // floor of `…` plus its first three characters: enough to tell
    // `HDMI-A-1` from `VGA-1`, which is the question a user asks of a
    // truncated label, and few enough that the row still gets most of
    // the space back.
    //
    // The floor is measured through the server's own font engine here,
    // exactly as the widget computes it, so the assertion follows the
    // theme and the box's fonts instead of freezing today's metrics.
    let text = "HDMI-A-1 1920×1080 @ 119.98 Hz";
    let mut h = Harness::sized(
        "elidefloor",
        (),
        Size::new(120.0, 60.0),
        |ui: &mut Ui<()>| {
            let mode = ui.build(label(text).name("mode").size(13.0).elide(true));
            // A sibling far too wide for the window, with no give at all:
            // whatever the row cannot take out of the label it must leave as
            // overflow, which is what puts the label on its floor.
            let hog = ui.build(label("X".repeat(40)).name("hog").size(13.0));
            let root = ui.build(row().gap(6.0).width_percent(1.0));
            ui.attach(root, mode).unwrap();
            ui.attach(root, hog).unwrap();
            root
        },
    );
    if !h.has_text() {
        h.quit();
        return;
    }
    let root = h.ui().root().unwrap();
    let mode = h.ui().children(root)[0];

    let theme = nitro_ui::Theme::default();
    let style = nitro_ui::TextStyle::new(theme.font_family.clone(), 13.0);
    let floor = h
        .ui()
        .measure_text("HDM…", &style, 0.0)
        .expect("measure the remnant")
        .width;
    let got = h.bounds(mode).w;
    assert!(
        (got - floor).abs() < 0.51,
        "the label was narrowed to {got} but its floor is `HDM…` = {floor}: \
         an eliding label gives space up, but not the ellipsis and the first \
         three characters"
    );
    assert!(
        h.widget::<Label>(mode).painted_text().ends_with('…'),
        "and what is left is elided, not clipped: {:?}",
        h.widget::<Label>(mode).painted_text(),
    );
    // And the remnant really fits the floor-width box, which is the
    // assertion that makes the two above mean something: before #3725's
    // review the label elided against the ~120 px it was *offered* and
    // was then laid out at this ~28 px floor, so `painted_text()` ended
    // in an ellipsis that was itself off the end of the box.
    assert_painted_fits(&mut h, mode, 13.0);
    h.quit();
}

#[test]
fn re_eliding_happens_on_a_width_change_and_on_nothing_else() {
    // Work ∝ change. The elision search is `log(len)` measurements, and a
    // label that re-ran it whenever it was asked for its size would put
    // them in the layout path of every frame — the same mistake as
    // re-shaping a title on every pointer motion (`retitle`,
    // `docs/wm.md`).
    //
    // Two things about the census are worth recording, because the two
    // obvious versions of this test both pass vacuously.
    //
    // *Repaints* are not the interesting event: `paint` never measures,
    // so ten repaints re-elide nothing whatever the code does. They are
    // still asserted below, because that is the property the spec names
    // and a future `paint` that measured would break it — but the
    // load-bearing half is the ten **relayouts**, which do call
    // `measure`, at an unchanged width.
    //
    // And the counter is the label's own rather than the server's
    // `text_layouts`, because measurements are cached client-side by
    // `(text, style, max_width)`: a search re-run at a width it has
    // already been run at is answered entirely out of that cache, with
    // no wire traffic and no server-side layout pass. A `text_layouts`
    // census reads zero whether or not the memo exists — checked by
    // deleting the memo and watching it still pass.
    let long = "1920×1080 @ 119.98 Hz (also 60, 84.904, 59.94, 50, 24, 23.976)";
    let mut h = Harness::sized("elidework", (), Size::new(220.0, 60.0), |ui: &mut Ui<()>| {
        let mode = ui.build(label(long).name("mode").size(13.0).elide(true));
        let root = ui.build(row().gap(6.0).padding(6.0).width_percent(1.0));
        ui.attach(root, mode).unwrap();
        root
    });
    if !h.has_text() {
        h.quit();
        return;
    }
    let root = h.ui().root().unwrap();
    let mode = h.ui().children(root)[0];
    h.settle();
    assert!(
        h.widget::<Label>(mode).is_elided(),
        "the label really is eliding, so there is a search to count"
    );

    // What the settled count *is*, stated rather than merely held
    // constant below. Since #3725's review the search runs from `layout`
    // and nowhere else, so bringing this label up costs exactly **one**
    // — the first time it learns its own width. A version that also
    // searched from `measure` would read two, and a version that
    // searched on every pass would read more; pinning the number here is
    // what makes the equalities below say "and it stayed at one".
    let before = h.widget::<Label>(mode).elisions();
    assert_eq!(
        before, 1,
        "a settled eliding label has searched once: the offered width is \
         not a width it elides against, only the width it is given"
    );
    let layouts = h.server().stat("text_layouts");
    for _ in 0..10 {
        h.ui().mark(mode, nitro_ui::Dirty::PAINT);
        h.flush();
        h.settle();
    }
    assert_eq!(
        h.widget::<Label>(mode).elisions(),
        before,
        "ten repaints re-elided nothing"
    );
    for _ in 0..10 {
        h.ui()
            .mark(mode, nitro_ui::Dirty::LAYOUT | nitro_ui::Dirty::PAINT);
        h.flush();
        h.settle();
    }
    assert_eq!(
        h.widget::<Label>(mode).elisions(),
        before,
        "ten *relayouts* at an unchanged width re-elided nothing either: \
         the search is memoized on the width it was run at"
    );
    // And none of it cost the server any text work, which is what the
    // memo is ultimately for.
    assert_eq!(
        h.server().stat("text_layouts"),
        layouts,
        "twenty passes at an unchanged width shaped nothing"
    );

    // The control: a narrower window is a new width, and that does cost
    // a search — so the equalities above are the memo and not a counter
    // that never moves.
    h.configure(Size::new(160.0, 60.0));
    h.settle();
    assert!(
        h.widget::<Label>(mode).elisions() > before,
        "a width change re-elides: the counter is live"
    );
    h.quit();
}
