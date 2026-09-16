//! The M2 acceptance tests: a real server on the fake backend, a real
//! client, a real widget tree, and assertions on pixels and on the wire.
//!
//! Everything here goes through [`nitro_ui::test::Harness`], which runs
//! the server on a thread and the `Ui` on the test thread. A click really
//! travels evdev code → server hit test → wire → widget, and a repaint
//! really produces the mutations it claims to.

use nitro_core::{Point, Rect, Size};
use nitro_server::input::InputEvent;
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Button, Label, button, column, label, panel, row, spacer};
use nitro_ui::{Error, Ui, WidgetId};

/// The window's background: the harness's tree paints nothing behind the
/// root, so the desktop shows through. Ink assertions compare against
/// whatever is actually there, so they read the pixel first.
fn background_at<S: 'static>(h: &Harness<S>, x: u32, y: u32) -> u32 {
    h.shot().pixel(x, y)
}

#[test]
fn a_column_of_two_labels_lays_out_with_the_gap_and_draws_text() {
    struct S {
        first: Option<WidgetId>,
        second: Option<WidgetId>,
    }
    let mut h = Harness::sized(
        "labels",
        S {
            first: None,
            second: None,
        },
        Size::new(240.0, 120.0),
        |ui: &mut Ui<S>| {
            let first = ui.build(label("First line"));
            let second = ui.build(label("Second line"));
            let root = ui.build(
                panel()
                    .background(nitro_core::Color::WHITE)
                    .padding(10.0)
                    .gap(12.0),
            );
            ui.attach(root, first).unwrap();
            ui.attach(root, second).unwrap();
            root
        },
    );
    // Recover the ids: the builder closure cannot write to the state, so
    // walk the tree instead — which also checks the tree has the shape
    // the builder described.
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    assert_eq!(kids.len(), 2, "the panel has two children");
    h.state_mut().first = Some(kids[0]);
    h.state_mut().second = Some(kids[1]);

    let a = h.bounds(kids[0]);
    let b = h.bounds(kids[1]);
    assert!(a.w > 0.0 && a.h > 0.0, "the first label has a size: {a:?}");
    assert!(b.w > 0.0 && b.h > 0.0);
    // Padding 10, gap 12: the first starts at the padding, the second one
    // gap below the first's bottom.
    assert!((a.y - 10.0).abs() < 0.01, "padding places the first: {a:?}");
    assert!(
        (b.y - (a.bottom() + 12.0)).abs() < 0.01,
        "the gap separates them: {a:?} {b:?}"
    );
    assert!((a.x - 10.0).abs() < 0.01);

    if h.has_text() {
        // White panel, dark text: any pixel that is not the panel colour
        // is a glyph.
        let panel_px = 0x00ff_ffff;
        assert!(h.has_ink(a, panel_px), "the first label drew glyphs");
        assert!(h.has_ink(b, panel_px), "the second label drew glyphs");
        // And the gap between them did not.
        let gap = Rect::new(a.x, a.bottom() + 1.0, a.w, 10.0);
        assert!(!h.has_ink(gap, panel_px), "the gap is empty");
    }
    h.quit();
}

#[test]
fn clicking_a_button_runs_the_callback_and_the_repaint_is_one_commit() {
    struct S {
        clicks: u32,
        label: Option<WidgetId>,
    }
    let mut h = Harness::sized(
        "click",
        S {
            clicks: 0,
            label: None,
        },
        Size::new(240.0, 120.0),
        |ui: &mut Ui<S>| {
            let text = ui.build(label("before"));
            let btn = ui.build(button("Press").on_click(move |s: &mut S, ui: &mut Ui<S>| {
                s.clicks += 1;
                s.label = Some(text);
                ui.widget_mut::<Label>(text).unwrap().set_text("after");
            }));
            let root = ui.build(panel().padding(8.0).gap(8.0));
            ui.attach(root, text).unwrap();
            ui.attach(root, btn).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let (text, btn) = (kids[0], kids[1]);

    assert_eq!(h.state().clicks, 0);
    assert_eq!(h.widget::<Label>(text).text(), "before");
    assert!(h.widget::<Button<S>>(btn).is_enabled());

    h.click(btn);
    assert_eq!(h.state().clicks, 1, "the callback ran with `&mut S`");
    assert_eq!(h.state().label, Some(text), "and could mutate the state");
    assert_eq!(
        h.widget::<Label>(text).text(),
        "after",
        "and the label through `ui.widget_mut`"
    );

    // Now the interesting half: a *second* text change costs exactly one
    // commit, and touches only the label's node.
    h.tap();
    h.clear_tap();
    let commits_before = h.commits();
    h.ui().widget_mut::<Label>(text).unwrap().set_text("again");
    h.settle();
    assert_eq!(
        h.commits() - commits_before,
        1,
        "one text change is one commit; got {:?}",
        h.mutations()
    );
    let set_texts: Vec<_> = h.mutations().iter().filter(|m| m.op == "SetText").collect();
    assert_eq!(
        set_texts.len(),
        1,
        "exactly one SetText: {:?}",
        h.mutations()
    );
    // Nothing else was told to change its text, and no button node was
    // touched at all.
    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(
        !ops.contains(&"SetFill"),
        "the button did not repaint: {ops:?}"
    );
    assert!(
        !ops.contains(&"CreateNode"),
        "no node was recreated: {ops:?}"
    );
    h.quit();
}

#[test]
fn a_resize_handler_sees_the_new_size_and_a_move_fires_nothing() {
    // `on_resize` is for the work a resize implies that a re-layout does
    // not: an app whose content has its own units (a terminal's cells)
    // has to recompute how much of it fits, and may have to tell
    // something outside the process — `nitro-term` sends `TIOCSWINSZ`
    // from here.
    //
    // Two properties, and the second is why the hook filters rather than
    // forwarding every `Configure`: the handler sees the *new* size
    // already applied, and a `Configure` that only moved the window
    // fires nothing, so an app does not reflow because the user dragged
    // its titlebar.
    use std::cell::RefCell;
    use std::rc::Rc;

    let seen: Rc<RefCell<Vec<Size>>> = Rc::new(RefCell::new(Vec::new()));
    let mut h = Harness::sized("onresize", (), Size::new(200.0, 100.0), |ui: &mut Ui<()>| {
        ui.build(column().child(label("hi")))
    });
    {
        let seen = Rc::clone(&seen);
        h.ui()
            .on_resize(move |_s: &mut (), ui: &mut Ui<()>, size: Size| {
                // The size is both the argument and already in `window_size`.
                assert_eq!(ui.window_size(), size);
                seen.borrow_mut().push(size);
            });
    }
    assert_eq!(h.ui().resize_handler_count(), 1);

    h.configure(Size::new(320.0, 120.0));
    assert_eq!(
        seen.borrow().as_slice(),
        [Size::new(320.0, 120.0)],
        "a real resize reaches the handler, with the new size"
    );

    // The same size again is not a resize.
    h.configure(Size::new(320.0, 120.0));
    assert_eq!(
        seen.borrow().len(),
        1,
        "a Configure that changed nothing fires nothing"
    );
    h.quit();
}

#[test]
fn a_configure_relayouts_the_tree() {
    let mut h = Harness::sized("resize", (), Size::new(200.0, 100.0), |ui: &mut Ui<()>| {
        let left = ui.build(label("left"));
        let right = ui.build(label("right").grow(1.0));
        let root = ui.build(row().gap(4.0));
        ui.attach(root, left).unwrap();
        ui.attach(root, right).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let before = h.bounds(kids[1]);
    assert_eq!(h.ui().window_size(), Size::new(200.0, 100.0));

    h.configure(Size::new(320.0, 100.0));
    assert_eq!(h.ui().window_size(), Size::new(320.0, 100.0));
    let after = h.bounds(kids[1]);
    assert!(
        after.w > before.w + 100.0,
        "the growing child absorbed the extra 120: {before:?} → {after:?}"
    );
    assert!(
        (h.bounds(root).w - 320.0).abs() < 0.01,
        "the root fills the window"
    );
    h.quit();
}

#[test]
fn a_settled_tree_sends_nothing_while_idle() {
    let mut h = Harness::sized("idle", (), Size::new(160.0, 80.0), |ui: &mut Ui<()>| {
        let l = ui.build(label("static"));
        let root = ui.build(panel().padding(6.0));
        ui.attach(root, l).unwrap();
        root
    });
    h.settle();
    // The whole design's headline claim, checked from outside: with
    // nothing changing, not one commit goes out.
    h.assert_idle(200);
    h.quit();
}

#[test]
fn stale_ids_and_re_entrancy_are_errors_not_panics() {
    let mut h = Harness::sized("errors", (), Size::new(160.0, 80.0), |ui: &mut Ui<()>| {
        let l = ui.build(label("x"));
        let root = ui.build(column());
        ui.attach(root, l).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let child = h.ui().children(root)[0];

    // Wrong type.
    let e = h.ui().widget::<Button<()>>(child).unwrap_err();
    assert!(matches!(e, Error::WrongType { .. }), "{e}");

    // Busy: the widget is out of its slot while a `WidgetMut` is alive.
    {
        let mut m = h.ui().widget_mut::<Label>(child).unwrap();
        let again = m.ui().widget_mut::<Label>(child);
        assert!(matches!(again.err(), Some(Error::Busy)), "re-entrancy");
        // …but another widget is reachable, which is the whole point.
        assert!(m.ui().widget::<nitro_ui::widgets::Flex>(root).is_ok());
    }

    // Stale: the id outlives the widget.
    h.ui().remove(child).unwrap();
    let e = h.ui().widget::<Label>(child).unwrap_err();
    assert!(matches!(e, Error::StaleWidget), "{e}");
    assert!(matches!(
        h.ui().widget_mut::<Label>(child).err(),
        Some(Error::StaleWidget)
    ));
    assert!(matches!(
        h.ui().remove(child).err(),
        Some(Error::StaleWidget)
    ));
    assert!(matches!(
        h.ui().set_root(child).err(),
        Some(Error::StaleWidget)
    ));
    assert!(matches!(
        h.ui().attach(root, child).err(),
        Some(Error::StaleWidget)
    ));
    h.quit();
}

#[test]
fn tab_walks_the_focusable_widgets_in_order() {
    let mut h = Harness::sized("focus", 0u32, Size::new(300.0, 80.0), |ui: &mut Ui<u32>| {
        let a = ui.build(button("A").on_click(|s: &mut u32, _| *s += 1));
        let text = ui.build(label("not focusable"));
        let b = ui.build(button("B").on_click(|s: &mut u32, _| *s += 10));
        let c = ui.build(button("C").on_click(|s: &mut u32, _| *s += 100));
        let root = ui.build(row().gap(6.0).padding(6.0));
        for k in [a, text, b, c] {
            ui.attach(root, k).unwrap();
        }
        root
    });
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let (a, text, b, c) = (kids[0], kids[1], kids[2], kids[3]);

    assert_eq!(
        h.ui().focus_order(),
        vec![a, b, c],
        "the label is not in the Tab order"
    );
    assert!(h.ui().focused().is_none());

    h.key(key::TAB);
    assert_eq!(h.ui().focused(), Some(a));
    h.key(key::TAB);
    assert_eq!(h.ui().focused(), Some(b));
    h.key(key::TAB);
    assert_eq!(h.ui().focused(), Some(c));
    h.key(key::TAB);
    assert_eq!(h.ui().focused(), Some(a), "Tab wraps around");

    // Shift-Tab walks back. The server resolves the modifier, so the
    // harness holds the real Shift key down.
    h.key_with(key::LEFT_SHIFT, key::TAB);
    assert_eq!(h.ui().focused(), Some(c), "Shift-Tab goes backwards");

    // Space activates the focused button, and only that one.
    h.key(key::SPACE);
    assert_eq!(*h.state(), 100, "Space activated C");
    h.key(key::ENTER);
    assert_eq!(*h.state(), 200, "Enter activates too");
    let _ = text;
    h.quit();
}

#[test]
fn a_button_reacts_to_hover_and_press_and_a_disabled_one_does_not() {
    let mut h = Harness::sized("hover", 0u32, Size::new(200.0, 80.0), |ui: &mut Ui<u32>| {
        let b = ui.build(button("Hit me").on_click(|s: &mut u32, _| *s += 1));
        let root = ui.build(column().padding(10.0));
        ui.attach(root, b).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let btn = h.ui().children(root)[0];
    let bounds = h.bounds(btn);

    assert!(!h.ui().is_hovered(btn));
    h.move_pointer(Point::new(bounds.x + 2.0, bounds.y + 2.0));
    assert!(h.ui().is_hovered(btn), "the pointer is over it");
    assert!(!h.widget::<Button<u32>>(btn).is_pressed());

    h.press(nitro_ui::event::button::LEFT);
    assert!(h.widget::<Button<u32>>(btn).is_pressed());
    assert_eq!(*h.state(), 0, "the click fires on release, not press");
    h.release(nitro_ui::event::button::LEFT);
    assert!(!h.widget::<Button<u32>>(btn).is_pressed());
    assert_eq!(*h.state(), 1);

    // Disabling it stops everything.
    h.ui()
        .widget_mut::<Button<u32>>(btn)
        .unwrap()
        .set_enabled(false);
    h.settle();
    h.click(btn);
    assert_eq!(*h.state(), 1, "a disabled button does not fire");
    assert!(!h.widget::<Button<u32>>(btn).is_enabled());

    // Moving away clears the hover.
    h.move_pointer(Point::new(1.0, 79.0));
    assert!(!h.ui().is_hovered(btn));
    h.quit();
}

#[test]
fn a_spacer_pushes_its_siblings_apart() {
    let mut h = Harness::sized("spacer", (), Size::new(300.0, 60.0), |ui: &mut Ui<()>| {
        let left = ui.build(label("L"));
        let gap = ui.build(spacer());
        let right = ui.build(label("R"));
        let root = ui.build(row().padding(5.0));
        for k in [left, gap, right] {
            ui.attach(root, k).unwrap();
        }
        root
    });
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let left = h.bounds(kids[0]);
    let right = h.bounds(kids[2]);
    assert!(
        (right.right() - 295.0).abs() < 0.5,
        "the right label is at the right edge: {right:?}"
    );
    assert!(right.x > left.right() + 100.0, "{left:?} {right:?}");
    h.quit();
}

#[test]
fn widgets_report_their_role_and_accessible_record() {
    let mut h = Harness::sized("a11y", 0u32, Size::new(200.0, 80.0), |ui: &mut Ui<u32>| {
        let l = ui.build(label("Name:"));
        let b = ui.build(button("OK").on_click(|_: &mut u32, _| {}));
        let root = ui.build(row().gap(4.0).name("the row"));
        ui.attach(root, l).unwrap();
        ui.attach(root, b).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);

    assert_eq!(h.ui().role(root).unwrap(), nitro_ui::Role::Container);
    assert_eq!(h.ui().role(kids[0]).unwrap(), nitro_ui::Role::Label);
    assert_eq!(h.ui().role(kids[1]).unwrap(), nitro_ui::Role::Button);

    let a = h.ui().accessible(kids[1]).unwrap();
    assert_eq!(a.name.as_deref(), Some("OK"));
    // `click` first, because it is what the introspection protocol and
    // `hey` say; `activate` is the AT-SPI spelling of the same thing.
    assert_eq!(a.actions, ["click", "activate", "focus"]);
    let l = h.ui().accessible(kids[0]).unwrap();
    assert_eq!(l.value.as_deref(), Some("Name:"));
    // The container's name comes from the framework state, not the
    // widget, which is what `name()` on the builder sets.
    assert_eq!(
        h.ui().accessible(root).unwrap().name.as_deref(),
        Some("the row")
    );
    h.quit();
}

#[test]
fn a_panel_paints_its_background_and_a_colour_change_is_paint_only() {
    let mut h = Harness::sized("panel", (), Size::new(120.0, 60.0), |ui: &mut Ui<()>| {
        ui.build(
            panel()
                .background(nitro_core::Color::rgb(0x20, 0x80, 0x40))
                .radius(0.0)
                .border(0.0, nitro_core::Color::TRANSPARENT),
        )
    });
    h.settle();
    let px = background_at(&h, 60, 30);
    assert_eq!(
        px & 0x00ff_ffff,
        0x0020_8040,
        "the panel colour is on screen"
    );

    let root = h.ui().root().unwrap();
    h.tap();
    h.clear_tap();
    h.ui()
        .widget_mut::<nitro_ui::widgets::Panel>(root)
        .unwrap()
        .set_background(nitro_core::Color::rgb(0x80, 0x20, 0x40));
    h.settle();
    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(ops.contains(&"SetFill"), "the fill changed: {ops:?}");
    assert!(
        !ops.contains(&"SetBounds"),
        "a colour change moves nothing: {ops:?}"
    );
    assert_eq!(
        background_at(&h, 60, 30) & 0x00ff_ffff,
        0x0080_2040,
        "and the pixels followed"
    );
    h.quit();
}

#[test]
fn moving_a_widget_is_one_mutation_and_no_repaint() {
    let mut h = Harness::sized("move", (), Size::new(200.0, 200.0), |ui: &mut Ui<()>| {
        let inner = ui.build(panel().width(40.0).height(40.0));
        let root = ui.build(column().padding(0.0));
        ui.attach(root, inner).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let inner = h.ui().children(root)[0];
    h.settle();

    h.tap();
    h.clear_tap();
    // Push the child down by giving the container a top padding: a pure
    // move, no size change anywhere.
    let mut style = h.ui().style(root);
    style.padding.top = 60.0;
    h.ui().set_style(root, style);
    h.settle();

    let for_inner: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(
        for_inner.contains(&"SetBounds"),
        "the move is a SetBounds: {for_inner:?}"
    );
    assert!(
        !for_inner.contains(&"CreateNode"),
        "nothing was recreated: {for_inner:?}"
    );
    assert!(
        (h.bounds(inner).y - 60.0).abs() < 0.01,
        "{:?}",
        h.bounds(inner)
    );
    h.quit();
}

#[test]
fn the_pointer_enters_the_deepest_widget_and_bubbles_out() {
    let mut h = Harness::sized("hit", (), Size::new(200.0, 120.0), |ui: &mut Ui<()>| {
        let deep = ui.build(panel().width(50.0).height(50.0));
        let middle = ui.build(panel().padding(10.0));
        ui.attach(middle, deep).unwrap();
        let root = ui.build(column().padding(10.0));
        ui.attach(root, middle).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let middle = h.ui().children(root)[0];
    let deep = h.ui().children(middle)[0];
    let b = h.bounds(deep);

    h.move_pointer(Point::new(b.x + b.w / 2.0, b.y + b.h / 2.0));
    assert!(h.ui().is_hovered(deep), "the deepest widget is hovered");
    assert!(h.ui().is_hovered(middle), "and so is its ancestor");
    assert!(h.ui().is_hovered(root));

    // A point inside the middle's padding but outside the deep child.
    let m = h.bounds(middle);
    h.move_pointer(Point::new(m.x + 2.0, m.y + 2.0));
    assert!(!h.ui().is_hovered(deep), "the deep child was left");
    assert!(h.ui().is_hovered(middle));
    h.quit();
}

#[test]
fn quit_is_set_by_the_app_and_by_a_closed_window() {
    let mut h = Harness::sized("quit", (), Size::new(100.0, 50.0), |ui: &mut Ui<()>| {
        ui.build(column())
    });
    assert!(!h.ui().should_quit());
    h.ui().quit();
    assert!(h.ui().should_quit());
    h.quit();
}

#[test]
fn the_scroll_wheel_reaches_the_widget_under_the_pointer() {
    /// A widget that counts the scroll deltas it is given, so the test
    /// can check the axis event really routes.
    ///
    /// It paints a rectangle, and has to: the *server* decides which
    /// window the pointer is over by hit-testing painted scene content,
    /// so a window whose widgets all draw nothing is not under the
    /// pointer at all and receives no pointer events. See `docs/ui.md`.
    #[derive(Default)]
    struct Counter {
        dy: f32,
    }
    impl nitro_ui::Widget<()> for Counter {
        fn measure(
            &mut self,
            _cx: &mut nitro_ui::MeasureCx<'_, ()>,
            c: nitro_ui::Constraints,
        ) -> Size {
            c.constrain(Size::new(80.0, 80.0))
        }
        fn paint(&mut self, cx: &mut nitro_ui::PaintCx<'_, ()>) {
            let b = cx.bounds;
            cx.fill_rect(0, b, nitro_core::Color::rgb(0x30, 0x30, 0x30));
        }
        fn event(
            &mut self,
            _cx: &mut nitro_ui::EventCx<'_, ()>,
            ev: &nitro_ui::Event,
        ) -> nitro_ui::Handled {
            if let nitro_ui::Event::Scroll { dy, .. } = ev {
                self.dy += dy;
                return nitro_ui::Handled::Yes;
            }
            nitro_ui::Handled::No
        }
    }

    let mut h = Harness::sized("scroll", (), Size::new(120.0, 120.0), |ui: &mut Ui<()>| {
        let c = ui.build(nitro_ui::Built::new(Counter::default()));
        let root = ui.build(column());
        ui.attach(root, c).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let c = h.ui().children(root)[0];
    h.move_pointer(Point::new(20.0, 20.0));
    h.server().push_input(InputEvent::PointerAxis {
        dx: 0.0,
        dy: 12.0,
        source: nitro_wire::types::AxisSource::Wheel,
        time_ns: 50_000_000,
    });
    h.settle();
    assert!(
        (h.widget::<Counter>(c).dy - 12.0).abs() < 0.01,
        "the scroll reached the widget: {}",
        h.widget::<Counter>(c).dy
    );
    h.quit();
}

#[test]
fn a_timer_fires_and_can_be_cancelled() {
    let mut h = Harness::sized("timer", 0u32, Size::new(100.0, 50.0), |ui: &mut Ui<u32>| {
        ui.build(column())
    });
    let t = h.ui().set_timer(1, |s: &mut u32, _| *s += 1);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut state = 0u32;
    h.ui().run_timers(&mut state);
    assert_eq!(state, 1, "the timer fired");
    h.ui().run_timers(&mut state);
    assert_eq!(state, 1, "and only once");

    let t2 = h.ui().set_timer(10_000, |s: &mut u32, _| *s += 100);
    assert!(h.ui().next_timeout().is_some());
    h.ui().cancel_timer(&t2);
    assert!(h.ui().next_timeout().is_none(), "cancelled");
    let _ = t;
    h.quit();
}

#[test]
fn the_hello_dialog_example_toggles_its_label_and_quits() {
    /// The same tree `examples/hello_dialog.rs` builds, so the example
    /// cannot rot silently: an example that stops compiling is caught by
    /// `cargo build`, one that stops *working* needs this.
    struct State {
        ok: bool,
    }
    let mut h = Harness::sized(
        "dialog",
        State { ok: false },
        Size::new(300.0, 160.0),
        |ui: &mut Ui<State>| {
            let message = ui.build(label("Nothing has happened yet."));
            let root = ui.build(column().gap(12.0).padding(16.0));
            ui.attach(root, message).unwrap();
            let buttons = ui.build(
                row()
                    .gap(8.0)
                    .child(spacer())
                    .child(
                        button("Cancel").on_click(|_: &mut State, ui: &mut Ui<State>| {
                            ui.quit();
                        }),
                    )
                    .child(
                        button("OK").on_click(move |s: &mut State, ui: &mut Ui<State>| {
                            s.ok = !s.ok;
                            let text = if s.ok {
                                "You pressed OK."
                            } else {
                                "Toggled back."
                            };
                            ui.widget_mut::<Label>(message).unwrap().set_text(text);
                        }),
                    ),
            );
            ui.attach(root, buttons).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let message = h.ui().children(root)[0];
    let buttons = h.ui().children(root)[1];
    let kids = h.ui().children(buttons);
    let (cancel, ok) = (kids[1], kids[2]);

    h.click(ok);
    assert!(h.state().ok);
    assert_eq!(h.widget::<Label>(message).text(), "You pressed OK.");
    h.click(ok);
    assert!(!h.state().ok);
    assert_eq!(h.widget::<Label>(message).text(), "Toggled back.");

    assert!(!h.ui().should_quit());
    h.click(cancel);
    assert!(h.ui().should_quit(), "Cancel quits");
    h.quit();
}

#[test]
fn the_introspect_pass_walks_the_whole_tree() {
    let mut h = Harness::sized(
        "introspect",
        0u32,
        Size::new(240.0, 100.0),
        |ui: &mut Ui<u32>| {
            let title = ui.build(label("Title"));
            let ok = ui.build(button("OK").on_click(|s: &mut u32, _| *s += 1));
            let root = ui.build(panel().padding(8.0).gap(6.0).name("dialog"));
            ui.attach(root, title).unwrap();
            ui.attach(root, ok).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);

    let mut nodes = Vec::new();
    h.ui().introspect(&mut nodes);
    assert_eq!(nodes.len(), 3, "root, label, button");
    assert_eq!(nodes[0].id, root, "pre-order: the root comes first");
    assert_eq!(nodes[0].role, nitro_ui::Role::Container);
    assert_eq!(nodes[0].access.name.as_deref(), Some("dialog"));
    assert_eq!(nodes[0].children, kids);

    assert_eq!(nodes[1].role, nitro_ui::Role::Label);
    assert_eq!(nodes[1].access.value.as_deref(), Some("Title"));
    assert!(!nodes[1].focusable, "a label is not in the Tab order");

    assert_eq!(nodes[2].role, nitro_ui::Role::Button);
    assert_eq!(nodes[2].access.actions, ["click", "activate", "focus"]);
    assert!(nodes[2].focusable);
    assert!(!nodes[2].focused);
    // Bounds are in window coordinates, so an outside process can point
    // at a widget without knowing the tree's nesting.
    let b = nodes[2].bounds;
    assert!(b.x >= 8.0 && b.y >= 8.0 && b.w > 0.0, "{b:?}");
    assert_eq!(b, h.bounds(kids[1]));

    // Focus shows up in the next walk, which is what a subscriber needs.
    h.key(key::TAB);
    h.ui().introspect(&mut nodes);
    assert!(nodes[2].focused, "the button took the focus");

    // A stale id is an error, not a panic.
    let gone = kids[0];
    h.ui().remove(gone).unwrap();
    assert!(matches!(
        h.ui().introspect_node(gone).err(),
        Some(Error::StaleWidget)
    ));
    h.ui().introspect(&mut nodes);
    assert_eq!(nodes.len(), 2, "the removed label is gone from the tree");
    h.quit();
}

#[test]
fn an_app_owned_fd_gets_its_callback() {
    use std::io::Write as _;
    use std::os::fd::AsFd as _;

    let mut h = Harness::sized("fd", 0u32, Size::new(100.0, 50.0), |ui: &mut Ui<u32>| {
        ui.build(column())
    });
    let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
    let token = h
        .ui()
        .add_fd(read.as_fd(), |s: &mut u32, _| *s += 1)
        .unwrap();
    write.write_all(b"x").unwrap();

    let mut state = 0u32;
    h.ui().run_fd(&mut state, token);
    assert_eq!(state, 1, "the callback ran with `&mut S`");
    h.ui().run_fd(&mut state, token);
    assert_eq!(state, 2, "and is still registered afterwards");

    // The hook owns a `dup`, so the app may drop its own end; and an
    // unknown token is ignored rather than panicking.
    drop(read);
    h.ui()
        .run_fd(&mut state, nitro_ui::FdToken::from_raw(u64::MAX));
    assert_eq!(state, 2);

    h.ui().remove_fd(token);
    h.ui().run_fd(&mut state, token);
    assert_eq!(state, 2, "a removed hook does not fire");
    h.quit();
}

#[test]
fn a_child_added_to_a_settled_tree_is_painted() {
    // Regression: `add_child` marked only the parent, so the new slot's
    // own PAINT flag had no `SUB_PAINT` trail above it. On a settled tree
    // `pass_paint` stopped at the clean root and the child got a group
    // and bounds but was never painted — invisible until some unrelated
    // change happened to repaint an ancestor.
    let mut h = Harness::sized("addchild", (), Size::new(200.0, 120.0), |ui: &mut Ui<()>| {
        ui.build(column().padding(8.0).gap(4.0))
    });
    let root = h.ui().root().unwrap();
    h.settle();
    assert_eq!(h.ui().children(root).len(), 0);

    h.tap();
    h.clear_tap();
    let added = h
        .ui()
        .add_child(
            root,
            panel()
                .background(nitro_core::Color::rgb(0x20, 0x80, 0x40))
                .radius(0.0)
                .border(0.0, nitro_core::Color::TRANSPARENT)
                .width(60.0)
                .height(40.0),
        )
        .unwrap();
    h.settle();

    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(ops.contains(&"CreateNode"), "the child got a node: {ops:?}");
    assert!(
        ops.contains(&"SetFill"),
        "and it painted, which is the regression: {ops:?}"
    );
    let b = h.bounds(added);
    assert!(!b.is_empty(), "and it was laid out: {b:?}");
    assert_eq!(
        background_at(&h, b.x as u32 + 30, b.y as u32 + 20) & 0x00ff_ffff,
        0x0020_8040,
        "its pixels really reached the screen"
    );

    // A stale parent is still an error, not a panic.
    assert!(matches!(
        h.ui().add_child(added, label("x")).err(),
        None | Some(Error::StaleWidget)
    ));
    h.ui().remove(added).unwrap();
    assert!(matches!(
        h.ui().add_child(added, label("x")).err(),
        Some(Error::StaleWidget)
    ));
    h.quit();
}

#[test]
fn a_combined_mark_lights_every_sub_flag_on_the_way_up() {
    // Regression, found by the launcher (#3691). `Ui::mark` walks up
    // setting the matching `SUB_` flags and stops "as soon as one is
    // already lit" — but the stop tested whether *any* of the wanted bits
    // was present, not all of them. So a `LAYOUT | PAINT` mark on a
    // widget whose ancestor already carried `SUB_LAYOUT` (from an earlier
    // layout-only change) stopped there and never set `SUB_PAINT`, all
    // the way to the root.
    //
    // `pass_paint` then skipped the subtree, and the symptom was the
    // nastiest kind: the tree said one thing and the screen said another,
    // with no error anywhere. The launcher's selection marker moved in
    // `Button::text()` and never reached a pixel.
    //
    // The shape below is exactly that: two siblings deep in a tree, one
    // marked layout-only (which lights `SUB_LAYOUT` on every ancestor),
    // then the other's text changed — `Button::set_text` marks
    // `LAYOUT | PAINT`.
    struct S {
        a: Option<WidgetId>,
        b: Option<WidgetId>,
    }
    let mut h = Harness::sized(
        "submark",
        S { a: None, b: None },
        Size::new(240.0, 120.0),
        |ui: &mut Ui<S>| {
            let a = ui.build(button("aaa"));
            let b = ui.build(button("bbb"));
            // Three containers deep, so the `SUB_` walk has somewhere to
            // stop early.
            let inner = ui.build(row().gap(4.0));
            ui.attach(inner, a).unwrap();
            ui.attach(inner, b).unwrap();
            let middle = ui.build(column().gap(4.0));
            ui.attach(middle, inner).unwrap();
            let root = ui.build(column().padding(8.0));
            ui.attach(root, middle).unwrap();
            root
        },
    );
    // The ids, fished back out the way a test must: by walking, since the
    // builder closure cannot write to a state it does not have.
    let root = h.ui().root().unwrap();
    let middle = h.ui().children(root)[0];
    let inner = h.ui().children(middle)[0];
    let (a, b) = (h.ui().children(inner)[0], h.ui().children(inner)[1]);
    h.state_mut().a = Some(a);
    h.state_mut().b = Some(b);
    h.settle();

    // One widget marked LAYOUT-only: every ancestor now carries
    // `SUB_LAYOUT` and none carries `SUB_PAINT`.
    h.ui().mark(a, nitro_ui::Dirty::LAYOUT);
    // …and now the sibling's text changes, which is `LAYOUT | PAINT`.
    h.tap();
    h.clear_tap();
    h.ui().widget_mut::<Button<S>>(b).unwrap().set_text("zzz");
    h.settle();

    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(
        ops.contains(&"SetText"),
        "the repaint reached the wire: {ops:?}"
    );
    assert_eq!(
        h.widget::<Button<S>>(b).text(),
        "zzz",
        "and the tree agrees with it"
    );
    h.quit();
}

#[test]
fn a_label_is_painted_at_the_width_it_was_measured_at() {
    // Regression: `measure` asked for a wrap width but `paint` hardcoded
    // `max_width: 0.0, wrap: false`, so a label narrower than its string
    // reserved two lines of height and drew one overflowing line.
    let long = "wrapping is decided once and used twice";
    let mut h = Harness::sized("wrap", (), Size::new(120.0, 200.0), |ui: &mut Ui<()>| {
        let l = ui.build(label(long));
        let root = ui.build(
            panel()
                .background(nitro_core::Color::WHITE)
                .radius(0.0)
                .border(0.0, nitro_core::Color::TRANSPARENT)
                .padding(4.0),
        );
        ui.attach(root, l).unwrap();
        root
    });
    if !h.has_text() {
        h.quit();
        return;
    }
    let root = h.ui().root().unwrap();
    let l = h.ui().children(root)[0];
    let b = h.bounds(l);

    // The window is narrow, so the string must have wrapped: more than
    // one line of height, and no wider than the content box.
    assert!(b.w <= 112.01, "the label fits its parent: {b:?}");
    assert!(b.h > 20.0, "and wrapped to more than one line: {b:?}");

    // The painted run agrees: ink stays inside the measured box. If the
    // label were painted unwrapped it would be one long line escaping to
    // the right, and the server clips a run to its node bounds — so the
    // give-away is the *bottom* half of the box being empty.
    let lower = Rect::new(b.x, b.y + b.h / 2.0, b.w, b.h / 2.0);
    assert!(
        h.has_ink(lower, 0x00ff_ffff),
        "the second line was actually drawn: {lower:?}"
    );
    h.quit();
}

#[test]
fn a_button_does_not_stay_pressed_when_the_pointer_leaves() {
    // Regression: `pressed` was cleared only by `PointerUp`, which is
    // routed to the hover chain — so press, drag off, release left the
    // button painted active for ever (there is no pointer grab in M2).
    let mut h = Harness::sized(
        "stuck",
        0u32,
        Size::new(200.0, 120.0),
        |ui: &mut Ui<u32>| {
            let b = ui.build(button("Hold me").on_click(|s: &mut u32, _| *s += 1));
            let root = ui.build(column().padding(10.0));
            ui.attach(root, b).unwrap();
            root
        },
    );
    let root = h.ui().root().unwrap();
    let btn = h.ui().children(root)[0];
    let b = h.bounds(btn);

    h.move_pointer(Point::new(b.x + 2.0, b.y + 2.0));
    h.press(nitro_ui::event::button::LEFT);
    assert!(h.widget::<Button<u32>>(btn).is_pressed());

    // Drag off the button and release out there.
    h.move_pointer(Point::new(b.x + b.w + 20.0, b.y + b.h + 20.0));
    assert!(
        !h.widget::<Button<u32>>(btn).is_pressed(),
        "leaving clears the pressed state"
    );
    h.release(nitro_ui::event::button::LEFT);
    assert_eq!(*h.state(), 0, "and no click was delivered");
    assert!(!h.widget::<Button<u32>>(btn).is_pressed());

    // The button still works normally afterwards.
    h.click(btn);
    assert_eq!(*h.state(), 1);
    h.quit();
}

#[test]
fn focus_changes_are_reported_however_the_focus_moved() {
    /// Records the `FocusChanged` events it is given.
    #[derive(Default)]
    struct Watcher {
        events: Vec<bool>,
    }
    impl nitro_ui::Widget<()> for Watcher {
        fn measure(
            &mut self,
            _cx: &mut nitro_ui::MeasureCx<'_, ()>,
            c: nitro_ui::Constraints,
        ) -> Size {
            c.constrain(Size::new(60.0, 30.0))
        }
        fn paint(&mut self, cx: &mut nitro_ui::PaintCx<'_, ()>) {
            let b = cx.bounds;
            cx.fill_rect(0, b, nitro_core::Color::rgb(0x40, 0x40, 0x40));
        }
        fn event(
            &mut self,
            cx: &mut nitro_ui::EventCx<'_, ()>,
            ev: &nitro_ui::Event,
        ) -> nitro_ui::Handled {
            match ev {
                nitro_ui::Event::FocusChanged { focused } => {
                    self.events.push(*focused);
                    nitro_ui::Handled::No
                }
                // Taking focus on a click is what `request_focus` is for,
                // and it is the path that used to report nothing.
                nitro_ui::Event::PointerDown { .. } => {
                    cx.request_focus();
                    nitro_ui::Handled::Yes
                }
                _ => nitro_ui::Handled::No,
            }
        }
    }

    let mut h = Harness::sized("focusev", (), Size::new(200.0, 140.0), |ui: &mut Ui<()>| {
        let w = ui.build(nitro_ui::Built::new(Watcher::default()));
        let b = ui.build(button("B").on_click(|(), _| {}));
        let root = ui.build(column().padding(10.0).gap(6.0));
        ui.attach(root, w).unwrap();
        ui.attach(root, b).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let (watcher, btn) = (kids[0], kids[1]);
    assert!(h.widget::<Watcher>(watcher).events.is_empty());

    // Click it: `request_focus` runs inside the widget's own `event`, so
    // the notification has to be queued and delivered afterwards.
    h.click(watcher);
    assert!(h.ui().is_focused(watcher));
    assert_eq!(
        h.widget::<Watcher>(watcher).events,
        [true],
        "a click-taken focus is reported"
    );

    // Tab away: the watcher is told it lost the focus.
    h.key(key::TAB);
    assert!(h.ui().is_focused(btn));
    assert_eq!(
        h.widget::<Watcher>(watcher).events,
        [true, false],
        "and so is losing it"
    );
    h.quit();
}

#[test]
fn a_measurement_taken_while_handling_an_event_keeps_the_other_messages() {
    // Regression: `pump` did `self.wire.stray = batch` after draining it,
    // overwriting anything `measure_text`'s round trip had parked there
    // during dispatch — dropping real server messages on the floor.
    //
    // Making that deterministic needs a message guaranteed to arrive
    // *while* the handler is blocked, so the handler commits a text
    // change first: the server answers every node it reshaped with a
    // `TextMetrics`, which then lands in the round trip's poll and is
    // parked. With the bug it is dropped; with the fix the next `pump`
    // delivers it.
    struct S {
        label: Option<WidgetId>,
        keys: u32,
        measured: f32,
    }
    struct Measurer;
    impl nitro_ui::Widget<S> for Measurer {
        fn measure(
            &mut self,
            _cx: &mut nitro_ui::MeasureCx<'_, S>,
            c: nitro_ui::Constraints,
        ) -> Size {
            c.constrain(Size::new(80.0, 40.0))
        }
        fn paint(&mut self, cx: &mut nitro_ui::PaintCx<'_, S>) {
            let b = cx.bounds;
            cx.fill_rect(0, b, nitro_core::Color::rgb(0x50, 0x50, 0x50));
        }
        fn event(
            &mut self,
            cx: &mut nitro_ui::EventCx<'_, S>,
            ev: &nitro_ui::Event,
        ) -> nitro_ui::Handled {
            let nitro_ui::Event::KeyDown(_) = ev else {
                return nitro_ui::Handled::No;
            };
            cx.state.keys += 1;
            // Reshape a label and push it, so a `TextMetrics` is on its
            // way back to us...
            if let Some(l) = cx.state.label {
                let text = format!("reshaped {}", cx.state.keys);
                if let Ok(mut label) = cx.ui.widget_mut::<Label>(l) {
                    label.set_text(text);
                }
                let _ = cx.ui.flush();
            }
            // ...and then block on a measurement, whose round trip is
            // what picks that `TextMetrics` up and parks it.
            let style = nitro_ui::TextStyle::new("sans", 14.0);
            let unique = format!("measured mid-event {}", cx.state.keys);
            if let Ok(m) = cx.ui.measure_text(&unique, &style, 0.0) {
                cx.state.measured = m.width;
            }
            nitro_ui::Handled::Yes
        }
    }

    let mut h = Harness::sized(
        "straykeep",
        S {
            label: None,
            keys: 0,
            measured: 0.0,
        },
        Size::new(200.0, 120.0),
        |ui: &mut Ui<S>| {
            let m = ui.build(nitro_ui::Built::new(Measurer));
            let l = ui.build(label("before"));
            let root = ui.build(column().padding(6.0).gap(4.0));
            ui.attach(root, m).unwrap();
            ui.attach(root, l).unwrap();
            root
        },
    );
    if !h.has_text() {
        h.quit();
        return;
    }
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    let (measurer, text) = (kids[0], kids[1]);
    h.state_mut().label = Some(text);
    h.ui().focus(measurer);
    h.settle();

    // Deliver one key by hand so the parked message is still queued when
    // we look: `Harness::key` would settle and consume it.
    h.send_key(key::SPACE);
    h.wait_for("the key to be dispatched", |h| {
        h.pump();
        h.state().keys >= 1
    });
    assert_eq!(h.state().keys, 1, "the handler ran");
    assert!(h.state().measured > 0.0, "and its measurement came back");

    // The `TextMetrics` the handler's own commit provoked was parked
    // mid-round-trip. With the bug it was overwritten by the empty
    // drained buffer; with the fix the next pump still has it.
    assert!(
        h.pump() > 0,
        "the message parked during dispatch survived the pump"
    );

    // And the whole thing still works end to end.
    h.settle();
    assert_eq!(h.widget::<Label>(text).text(), "reshaped 1");
    h.key(key::SPACE);
    assert_eq!(h.state().keys, 2, "no keystroke was dropped");
    h.quit();
}

#[test]
fn a_click_in_a_no_focus_window_activates_without_taking_focus() {
    // Every shell surface is `NO_FOCUS`, and the server will never route
    // a key to one. So the focus a button takes on click buys nothing
    // there and costs something visible: a focus ring on whatever was
    // last clicked, and `hey … list` reporting it `focused` — a lie about
    // a window that cannot be focused.
    //
    // The claim is precisely "activates without focusing": the callback
    // still runs, and only the focus move is dropped.
    let mut h = Harness::shell(
        "nofocus-bar",
        0u32,
        nitro_ui::shell::Surface::bar(32),
        Some(Size::new(320.0, 32.0)),
        |ui: &mut Ui<u32>| {
            let b = ui.build(
                button("press")
                    .name("press")
                    .on_click(|s: &mut u32, _ui: &mut Ui<u32>| *s += 1),
            );
            let root = ui.build(row().padding(4.0));
            ui.attach(root, b).unwrap();
            root
        },
    );
    h.settle();
    let root = h.ui().root().unwrap();
    let b = h.ui().children(root)[0];

    h.click(b);
    h.settle();
    assert_eq!(*h.state(), 1, "the click still ran the callback");
    assert_eq!(h.ui().focused(), None, "and took no focus");
    assert!(!h.ui().is_focused(b));

    // What a script sees, which is where this was reported from: the
    // button must not come back `focused` in the introspection listing.
    let props = nitro_ui::introspect::get_prop(h.ui(), "window/press", "focused")
        .expect("the button is addressable");
    assert_eq!(props, "false", "hey would report a focus ring");

    // The suppression is about focus a *click* takes on the user's
    // behalf. An app that places focus deliberately still can — the
    // launcher is NO_FOCUS and focuses its query field, because it reads
    // the keyboard through a grab rather than through focus.
    h.ui().focus(b);
    h.settle();
    assert_eq!(
        h.ui().focused(),
        Some(b),
        "Ui::focus is not gated, only request_focus"
    );
    h.quit();
}

#[test]
fn a_click_in_an_ordinary_window_still_takes_focus() {
    // The other side of the rule, so the gate cannot be "no widget ever
    // focuses on click" by accident: the same tree in an ordinary window
    // focuses exactly as before.
    let mut h = Harness::sized(
        "focusable-app",
        0u32,
        Size::new(200.0, 60.0),
        |ui: &mut Ui<u32>| {
            let b = ui.build(button("press").on_click(|s: &mut u32, _ui: &mut Ui<u32>| *s += 1));
            let root = ui.build(row().padding(4.0));
            ui.attach(root, b).unwrap();
            root
        },
    );
    h.settle();
    let root = h.ui().root().unwrap();
    let b = h.ui().children(root)[0];
    h.click(b);
    h.settle();
    assert_eq!(*h.state(), 1);
    assert_eq!(
        h.ui().focused(),
        Some(b),
        "an ordinary window focuses on click"
    );
    h.quit();
}

#[test]
fn advancing_the_timers_past_the_clock_origin_saturates_rather_than_panicking() {
    // `advance_timers` is public test support, and `Instant` arithmetic
    // panics on underflow: a caller fast-forwarding a year on a process
    // that has been up for a second means "fire everything", not "abort".
    let mut h = Harness::sized(
        "saturate",
        0u32,
        Size::new(120.0, 40.0),
        |ui: &mut Ui<u32>| {
            ui.set_timer(60_000, |s: &mut u32, _ui: &mut Ui<u32>| *s += 1);
            ui.build(panel())
        },
    );
    h.settle();
    assert_eq!(*h.state(), 0, "not due yet");

    h.advance_timers(60 * 60 * 24 * 365 * 1000);
    h.run_timers();
    assert_eq!(*h.state(), 1, "a saturated deadline is simply due");
    h.quit();
}

#[test]
fn a_re_armed_fd_hook_gets_a_fresh_token() {
    // Regression, found on the box rather than here (#M4-D): a hook's
    // token used to be the raw descriptor number of the `Ui`'s own
    // duplicate. Descriptor numbers are recycled the instant they are
    // closed and the kernel hands back the lowest free one, so removing
    // one hook and adding another in the same turn produced the **same
    // token** — and the app loop, which keeps a list of what it has
    // already registered with `epoll` so it does not `epoll_ctl` every
    // wakeup, concluded the new hook was already in the set. It was not:
    // closing a descriptor removes it from every epoll set. The hook
    // existed, was never registered, and never fired.
    //
    // The symptom on the box was a file manager whose inotify watch
    // refreshed the first directory and no directory afterwards — it
    // re-arms the watch on every navigation — with nothing anywhere
    // returning an error. What makes it invisible from inside `Ui` is
    // that `run_fd` still worked; only the *loop* was wrong.
    use std::io::Write as _;
    use std::os::fd::AsFd as _;

    let mut h = Harness::sized("rearm", 0u32, Size::new(100.0, 50.0), |ui: &mut Ui<u32>| {
        ui.build(column())
    });
    let (read_a, _write_a) = std::os::unix::net::UnixStream::pair().unwrap();
    let first = h
        .ui()
        .add_fd(read_a.as_fd(), |s: &mut u32, _| *s += 1)
        .unwrap();

    // Drop the hook and its descriptor, then arm a new one — the shape
    // of "the watch moved to another directory". The kernel will hand
    // the new `dup` the number the old one just freed.
    h.ui().remove_fd(first);
    drop(read_a);
    let (read_b, mut write_b) = std::os::unix::net::UnixStream::pair().unwrap();
    let second = h
        .ui()
        .add_fd(read_b.as_fd(), |s: &mut u32, _| *s += 10)
        .unwrap();

    assert_ne!(
        first, second,
        "a re-armed hook must not reuse the retired hook's token"
    );

    // And the token the loop would read out of its epoll event still
    // finds the right hook.
    write_b.write_all(b"x").unwrap();
    let mut state = 0u32;
    h.ui()
        .run_fd(&mut state, nitro_ui::FdToken::from_raw(second.raw()));
    assert_eq!(state, 10, "the new hook fired, not the retired one");

    // The retired token is dead rather than aliasing the live hook.
    h.ui().run_fd(&mut state, first);
    assert_eq!(state, 10, "the retired token names nothing");
    h.quit();
}

#[test]
fn a_widget_callback_changes_that_widget_by_deferring() {
    // Take-out dispatch is what makes `Fn(&mut S, &mut Ui<S>)` possible,
    // and its one cost is that the widget running a callback is the one
    // widget that callback cannot reach: `widget_mut` answers
    // `Error::Busy`, a value rather than a panic.
    //
    // For most widgets that is the end of it, because a callback changes
    // something else. A list is the case where it is not — "activate
    // this row" means "show different rows *here*" — and `nitro-files`
    // shipped the bug this test exists to prevent: the write went into
    // an `Err` nobody read, the path bar updated, and the rows on screen
    // stayed as they were, with no error reported anywhere.
    //
    // `Ui::defer` is the answer, and it is the same one `Ui::focus` has
    // always used for the same reason.
    let mut h = Harness::sized("defer", (), Size::new(200.0, 80.0), |ui: &mut Ui<()>| {
        let root = ui.build(column());
        let label = ui.build(label("before").name("l"));
        ui.attach(root, label).unwrap();
        root
    });
    let root = h.ui().root().unwrap();
    let target = h.ui().children(root)[0];

    // First the negative, observed where it actually bites: a button
    // whose `on_click` tries to reach **itself**. The framework has that
    // button out of its slot for the duration of the call, so
    // `widget_mut` answers `Error::Busy` — a value, not a panic — and an
    // app that ignored it would write nothing and never know.
    let clicks: std::rc::Rc<std::cell::Cell<bool>> = std::rc::Rc::new(std::cell::Cell::new(false));
    let seen = std::rc::Rc::clone(&clicks);
    let btn = h.ui().build(
        button("b")
            .name("b")
            .on_click(move |_s: &mut (), ui: &mut Ui<()>| {
                let me = nitro_ui::introspect::resolve(ui, "b").expect("the button");
                seen.set(ui.widget_mut::<Button<()>>(me).is_err());
            }),
    );
    h.ui().attach(root, btn).unwrap();
    h.settle();
    h.click(btn);
    h.settle();
    assert!(
        clicks.get(),
        "a widget cannot reach itself from inside its own callback — `Error::Busy`, by design"
    );

    // And the cure, in the shape the app uses: the callback queues the
    // work instead, and it runs with the tree whole.
    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = std::rc::Rc::clone(&done);
    {
        let (ui, _state) = h.parts();
        ui.defer(move |_s: &mut (), ui: &mut Ui<()>| {
            ui.widget_mut::<Label>(target)
                .expect("the widget is back in its slot when a deferred callback runs")
                .set_text("after");
            flag.set(true);
        });
        assert_eq!(ui.deferred_count(), 1, "queued, not run");
    }
    {
        let (ui, state) = h.parts();
        ui.run_deferred(state);
    }
    h.settle();
    assert!(done.get(), "the deferred callback ran");
    assert_eq!(h.widget::<Label>(target).text(), "after");
    assert_eq!(h.ui().deferred_count(), 0, "the queue drained");

    // A deferred callback may defer again — a navigation that triggers a
    // re-listing is the real case — and both run in the same pass.
    {
        let (ui, _state) = h.parts();
        ui.defer(move |_s: &mut (), ui: &mut Ui<()>| {
            ui.defer(move |_s: &mut (), ui: &mut Ui<()>| {
                if let Ok(mut l) = ui.widget_mut::<Label>(target) {
                    l.set_text("nested");
                }
            });
        });
    }
    {
        let (ui, state) = h.parts();
        ui.run_deferred(state);
    }
    h.settle();
    assert_eq!(h.widget::<Label>(target).text(), "nested");
    h.quit();
}

#[test]
fn a_column_that_does_not_fit_clips_rather_than_squashing_its_labels() {
    // Issue #561, at the level the user meets it: three labels in a
    // window too short for them.
    //
    // Before the content shrink floor this was the *silent* bug — the
    // solver handed the overflow back to all three weighted by size, the
    // server painted each label's full glyphs into a box smaller than
    // the text it had measured, and every number `hey list` reported was
    // internally consistent. Run against the old solver these labels
    // come back at 11.5/11.5/11.5 px against a measured 15.1, which is
    // what clipped the descenders of "Displays" on the box.
    //
    // What the floor buys: every label keeps its measured height, the
    // rows never overlap, and the surplus runs off the bottom for the
    // window to clip.
    let mut h = Harness::sized("floor", (), Size::new(200.0, 60.0), |ui: &mut Ui<()>| {
        let root = ui.build(column().gap(6.0).padding(4.0));
        for text in ["Displays", "Keyboard", "Appearance"] {
            let l = ui.build(label(text).size(15.0).weight(600));
            ui.attach(root, l).unwrap();
        }
        root
    });
    if !h.has_text() {
        h.quit();
        return;
    }
    let root = h.ui().root().unwrap();
    let kids = h.ui().children(root);
    assert_eq!(kids.len(), 3);

    // The "want" is measured through the server's own font engine rather
    // than written down, so this follows the theme and the font the box
    // has rather than freezing today's metrics.
    let style = nitro_ui::TextStyle {
        weight: 600,
        ..nitro_ui::TextStyle::new("sans", 15.0)
    };
    for id in &kids {
        let text = h.widget::<Label>(*id).text().to_owned();
        let want = h
            .ui()
            .measure_text(&text, &style, 0.0)
            .expect("measure")
            .height;
        let got = h.bounds(*id).h;
        assert!(
            got >= want - 1e-3,
            "{text:?} is laid out {got} tall but measures {want}"
        );
    }

    // And they do not overlap: each starts at or after the one above it
    // ended. This is the half that `min_height` on the leaves alone did
    // not buy — the container used to be shrunk below the children it
    // now refuses to shrink, and they rendered on top of each other.
    for pair in kids.windows(2) {
        let (a, b) = (h.bounds(pair[0]), h.bounds(pair[1]));
        assert!(b.y >= a.bottom() - 1e-3, "rows overlap: {a:?} then {b:?}");
    }

    // The surplus is overflow, not squash: the tree is genuinely taller
    // than the 60 px window, which is the outcome the floor chooses.
    let last = h.bounds(kids[2]);
    assert!(
        last.bottom() > h.ui().window_size().h,
        "the third label runs past the window: {last:?}"
    );
    h.quit();
}

#[test]
fn a_child_moved_past_the_window_paints_nothing_on_the_desktop() {
    // The other half of #561's answer, and the one `nitro-settings`
    // needed: the content floor stops a row being *squashed*, and this
    // stops the overflow it produces instead from being painted on the
    // desktop.
    //
    // The settings dialog shipped without it. Its display row measured
    // ~700 px in a 560-px window, nothing in the row would shrink, and
    // the slider, the `primary` checkbox and both position fields were
    // painted outside the frame, over the wallpaper. `SetClip` is opt-in
    // on the wire and the toolkit never set it on its own root, so a
    // window's content was contained by nothing at all.
    //
    // The pin is a pixel census of the desktop strip immediately right
    // of the window, taken **before and after** the child moves out
    // there: moving a white 60×40 panel from inside the window to
    // x = width + 10 must change *zero* pixels of desktop. Before/after
    // rather than against a constant colour, because the desktop is a
    // gradient and "changed" is the honest question anyway. The census
    // runs on `output_shot`, not `shot` — the latter crops to the window
    // and would hide exactly the spill this is about.
    //
    // The child is moved by a container that overrides `layout`, because
    // no flex arrangement produces a child outside its parent on
    // purpose: the failure being pinned is the *paint* path, and the
    // layout that leads there is reproduced directly rather than through
    // an app's arithmetic.

    /// A container that places its one child either inside its own box
    /// or ten pixels past its right edge — an overflowing row, with the
    /// arithmetic removed.
    #[derive(Default)]
    struct Overflowing {
        outside: bool,
    }
    impl nitro_ui::Widget<()> for Overflowing {
        fn measure(
            &mut self,
            _cx: &mut nitro_ui::MeasureCx<'_, ()>,
            c: nitro_ui::Constraints,
        ) -> Size {
            c.max
        }
        fn layout(&mut self, cx: &mut nitro_ui::LayoutCx<'_, ()>, bounds: Rect) {
            let x = if self.outside { bounds.w + 10.0 } else { 10.0 };
            for child in cx.children() {
                cx.place_child(child, Rect::new(x, 10.0, 60.0, 40.0));
            }
        }
    }

    let mut h = Harness::sized("rootclip", (), Size::new(120.0, 80.0), |ui: &mut Ui<()>| {
        let far = ui.build(
            panel()
                .background(nitro_core::Color::WHITE)
                .name("far")
                .width(60.0)
                .height(40.0),
        );
        let mut built = nitro_ui::Built::new(Overflowing::default());
        built.state_mut().name = Some("root".to_owned());
        let root = ui.build(built);
        ui.attach(root, far).unwrap();
        root
    });
    let origin = h.ui().window_position();
    let size = h.ui().window_size();
    let far = nitro_ui::introspect::resolve(h.ui(), "window/far").expect("the child");
    assert!(
        h.bounds(far).right() <= size.w,
        "the child starts inside the window: {:?} in {size:?}",
        h.bounds(far),
    );

    // The desktop strip right of the window, in output coordinates.
    let before = h.output_shot();
    let strip_x = (origin.x + size.w).max(0.0) as u32;
    let strip_w = 60u32.min(before.width.saturating_sub(strip_x));
    let y0 = origin.y.max(0.0) as u32;
    let y1 = ((origin.y + size.h) as u32).min(before.height);
    assert!(
        strip_w > 0 && y1 > y0,
        "there is desktop right of the window to look at"
    );

    let root = h.ui().root().unwrap();
    {
        let mut w = h.ui().widget_mut::<Overflowing>(root).expect("the root");
        w.outside = true;
        w.request_layout();
    }
    h.settle();
    assert!(
        h.bounds(far).x > size.w,
        "the child really moved outside the window: {:?} in {size:?}",
        h.bounds(far),
    );

    let after = h.output_shot();
    let mut changed = 0usize;
    for y in y0..y1 {
        for x in strip_x..strip_x + strip_w {
            if before.pixel(x, y) != after.pixel(x, y) {
                changed += 1;
            }
        }
    }
    assert_eq!(
        changed, 0,
        "a widget laid out past the window's right edge changed {changed} pixels \
         in the {strip_w}-px desktop strip beside it: a window's content is \
         clipped to the window, so overflow is a layout bug you *see* as \
         cut-off, never as spill"
    );
    h.quit();
}
