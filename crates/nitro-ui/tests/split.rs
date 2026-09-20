//! The split-view blueprint, through the harness: a real server, real
//! input and real pixels.
//!
//! The claims worth pinning are the ones an app relies on without
//! saying so: a selection change is two fills, a hover is one, a page
//! switch is a handful of mutations and no relayout, a card paints one
//! separator fewer than its rows, and a settled view sends nothing.

use nitro_core::{Palette, Point, Rect, Size};
use nitro_ui::build::{ContainerBuilder as _, IntoWidget as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::introspect::{get_prop, resolve};
use nitro_ui::split::{
    self, CARD_ROW_PAD_X, Card, Pages, SIDEBAR_MIN_WIDTH, SIDEBAR_ROW_INSET, SidebarRow, Switch,
    card, card_row, content_column, footnote, group_caption, pages, sidebar_row, sidebar_section,
    sidebar_separator, split_view, switch,
};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Label, button, text_field};
use nitro_ui::{ColorRole, Role, Ui, WidgetId};

/// The app state: which category is selected, and what was clicked.
#[derive(Default)]
struct S {
    selected: usize,
    clicks: Vec<String>,
    toggles: Vec<bool>,
}

const CATEGORIES: [(&str, &str); 3] = [
    ("display", "Displays"),
    ("keyboard", "Keyboard"),
    ("speaker", "Audio"),
];

/// Select category `i`: the row that was clicked is out of its slot
/// while its callback runs, so the selection is deferred — the rule
/// `docs/ui.md` documents.
fn select(ui: &mut Ui<S>, i: usize) {
    ui.defer(move |s: &mut S, ui: &mut Ui<S>| {
        s.selected = i;
        let sidebar = resolve(ui, "sidebar").expect("the sidebar");
        for (k, row) in ui.children(sidebar).into_iter().enumerate() {
            if let Ok(mut r) = ui.widget_mut::<SidebarRow<S>>(row) {
                r.set_selected(k == i);
            }
        }
        if let Some(p) = resolve(ui, "pages")
            && let Ok(mut p) = ui.widget_mut::<Pages>(p)
        {
            p.show(i);
        }
        if let Some(t) = resolve(ui, "title")
            && let Ok(mut t) = ui.widget_mut::<Label>(t)
        {
            t.set_text(CATEGORIES[i].1);
        }
    });
}

fn a_page(name: &str, i: usize) -> nitro_ui::Built<S> {
    content_column()
        .name(format!("page_{name}"))
        .child(group_caption("Group"))
        .child(
            card().name(format!("card_{name}")).child(
                card_row("Option").trailing(
                    switch("")
                        .name(format!("switch_{name}"))
                        .on_toggle(|s: &mut S, _ui: &mut Ui<S>, on: bool| s.toggles.push(on)),
                ),
            ),
        )
        .child(footnote(format!("Page {i}.")))
        .into_widget()
}

fn tree(ui: &mut Ui<S>) -> WidgetId {
    let mut v = split_view()
        .sidebar_header("Settings")
        .content_header(CATEGORIES[0].1)
        .content(
            pages().name("pages").children(
                CATEGORIES
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| a_page(n, i)),
            ),
        );
    for (i, (icon, text)) in CATEGORIES.iter().enumerate() {
        v = v.sidebar_child(
            sidebar_row(*icon, *text)
                .name(format!("nav_{icon}"))
                .icon_name(format!("icon_{icon}"))
                .selected(i == 0)
                .on_click(move |s: &mut S, ui: &mut Ui<S>| {
                    s.clicks.push((*text).to_owned());
                    select(ui, i);
                }),
        );
    }
    v.build(ui).root
}

fn harness() -> Harness<S> {
    let mut h = Harness::sized("split", S::default(), Size::new(320.0, 240.0), tree);
    h.settle();
    h
}

/// A colour as the harness's pixel probes want it: `0x00RRGGBB`.
fn rgb(c: nitro_core::Color) -> u32 {
    c.to_u32() >> 8
}

fn ops<T: 'static>(h: &Harness<T>) -> Vec<&'static str> {
    h.mutations().iter().map(|m| m.op).collect()
}

fn count<T: 'static>(h: &Harness<T>, op: &str) -> usize {
    h.mutations().iter().filter(|m| m.op == op).count()
}

fn id(h: &mut Harness<S>, path: &str) -> WidgetId {
    resolve(h.ui(), path).unwrap_or_else(|| panic!("{path} resolves"))
}

#[test]
fn sidebar_rows_keep_the_names_hey_addresses_them_by() {
    let mut h = harness();
    let row = id(&mut h, "window/sidebar/nav_keyboard");
    assert_eq!(h.ui().role(row).unwrap(), Role::Button);
    let access = h.ui().accessible(row).unwrap();
    assert_eq!(access.value.as_deref(), Some("Keyboard"));
    assert_eq!(access.name.as_deref(), Some("Keyboard"));
    // The icon is a named child of its own.
    let icon = id(&mut h, "sidebar/nav_keyboard/icon_keyboard");
    assert_eq!(h.ui().role(icon).unwrap(), Role::Icon);
    // Every slot the blueprint names is there.
    let root = h.ui().root().unwrap();
    assert_eq!(
        h.ui().address_name(root).as_deref(),
        Some(split::names::SPLIT)
    );
    for name in [
        split::names::SIDEBAR,
        split::names::SIDEBAR_HEADER,
        split::names::CONTENT,
        split::names::CONTENT_HEADER,
        split::names::TITLE,
        "pages",
        "page_display",
        "card_display",
        "switch_display",
    ] {
        assert!(resolve(h.ui(), name).is_some(), "{name} resolves");
    }
    h.quit();
}

#[test]
fn a_sidebar_row_click_selects_and_repaints_only_that_row() {
    let mut h = harness();
    let first = id(&mut h, "nav_display");
    let second = id(&mut h, "nav_keyboard");
    assert!(h.widget::<SidebarRow<S>>(first).is_selected());

    h.tap();
    h.clear_tap();
    h.click(second);
    h.settle();
    assert_eq!(h.state().clicks, ["Keyboard"]);
    assert_eq!(h.state().selected, 1);
    assert!(!h.widget::<SidebarRow<S>>(first).is_selected());
    assert!(h.widget::<SidebarRow<S>>(second).is_selected());
    assert_eq!(h.ui().focused(), Some(second), "a click focuses the row");
    // A page switch: the two rows' faces, the page flip, the title, and
    // nothing laid out again — no `SetBounds`, no `SetText` beyond the
    // title's.
    assert_eq!(count(&h, "SetBounds"), 0, "nothing moved: {:?}", ops(&h));
    assert_eq!(count(&h, "SetVisible"), 2, "one page out, one in");
    assert_eq!(count(&h, "SetText"), 1, "the title");
    assert!(count(&h, "SetFill") >= 2, "the two rows' faces");
    let title = id(&mut h, "title");
    assert_eq!(
        h.widget::<Label>(title).text(),
        "Keyboard",
        "the header follows"
    );
    h.quit();
}

#[test]
fn hovering_a_sidebar_row_repaints_one_widget() {
    let mut h = harness();
    let row = id(&mut h, "nav_speaker");
    let b = h.bounds(row);
    let bg = rgb(h.ui().color(ColorRole::SidebarBackground));
    // Unselected and unhovered: the pill is transparent, so the row's
    // own rect shows the sidebar through it (the icon and label are ink,
    // so look at the pill's left inset corner).
    let corner = Rect::new(b.x + 1.0, b.y + b.h / 2.0 - 1.0, 4.0, 2.0);
    assert!(
        !h.has_ink(corner, bg),
        "a plain row is the sidebar's colour"
    );

    h.tap();
    h.clear_tap();
    h.move_pointer(Point::new(b.x + b.w / 2.0, b.y + b.h / 2.0));
    let fills = count(&h, "SetFill");
    assert_eq!(fills, 1, "hover is one fill: {:?}", ops(&h));
    assert!(h.has_ink(corner, bg), "the hover face is painted");

    h.clear_tap();
    h.move_pointer(Point::new(2.0, 200.0));
    assert_eq!(count(&h, "SetFill"), 1, "and one fill back");
    assert!(!h.has_ink(corner, bg));
    h.quit();
}

#[test]
fn a_card_paints_one_separator_fewer_than_its_rows() {
    let mut h = Harness::sized("card", S::default(), Size::new(320.0, 240.0), |ui| {
        ui.build(
            content_column().child(
                card()
                    .name("card")
                    .child(card_row("One").value("1"))
                    .child(card_row("Two").value("2"))
                    .child(card_row("Three").value("3")),
            ),
        )
    });
    h.settle();
    let c = id(&mut h, "card");
    assert_eq!(h.widget::<Card>(c).separators().len(), 2);
    // Rect nodes created by the card itself: the ring and two lines.
    let rows = h.ui().children(c);
    let ys: Vec<f32> = rows.iter().skip(1).map(|r| h.ui().bounds(*r).y).collect();
    assert_eq!(h.widget::<Card>(c).separators(), ys.as_slice());
    // The line is inset from the left, so the pixel just inside the
    // ring at a separator's y is surface, and one at the inset is not.
    let cb = h.bounds(c);
    let surface = rgb(h.ui().color(ColorRole::Surface));
    let y = cb.y + ys[0];
    assert!(
        !h.has_ink(Rect::new(cb.x + 3.0, y, CARD_ROW_PAD_X - 5.0, 1.0), surface),
        "no line in the inset"
    );
    assert!(
        h.has_ink(
            Rect::new(cb.x + CARD_ROW_PAD_X + 2.0, y, 20.0, 1.0),
            surface
        ),
        "a line past it"
    );

    // Adding a row later adds one separator, and a settled card is idle.
    h.ui().add_child(c, card_row("Four").value("4")).unwrap();
    h.settle();
    assert_eq!(h.widget::<Card>(c).separators().len(), 3);
    h.assert_idle(100);
    h.quit();
}

#[test]
fn a_split_view_fits_its_minimum_window() {
    let mut h = Harness::sized(
        "small",
        S::default(),
        Size::new(SIDEBAR_MIN_WIDTH + 100.0, 160.0),
        tree,
    );
    h.settle();
    let size = h.ui().window_size();
    let mut nodes = Vec::new();
    h.ui().introspect(&mut nodes);
    // What a scroll holds is *meant* to be taller than its viewport;
    // the clip is what keeps it inside. Widths still have to fit.
    let scrolled: std::collections::HashSet<WidgetId> = nodes
        .iter()
        .filter(|n| n.role == Role::Scroll)
        .flat_map(|n| n.children.iter().copied())
        .collect();
    let mut under_scroll = scrolled.clone();
    for n in &nodes {
        if under_scroll.contains(&n.id) {
            under_scroll.extend(n.children.iter().copied());
        }
    }
    for n in nodes {
        if !n.visible || n.bounds.w <= 0.0 || n.bounds.h <= 0.0 {
            continue;
        }
        let fits_h = under_scroll.contains(&n.id) || n.bounds.y + n.bounds.h <= size.h + 0.01;
        assert!(
            n.bounds.x + n.bounds.w <= size.w + 0.01 && fits_h,
            "{:?} ({:?}) overhangs a {}x{} window at {:?}",
            n.access.name,
            n.role,
            size.w,
            size.h,
            n.bounds
        );
    }
    // The rows are inset from the sidebar's edge.
    let sidebar = id(&mut h, "sidebar");
    let row = id(&mut h, "nav_display");
    let inset = h.bounds(row).x - h.bounds(sidebar).x;
    assert!((inset - SIDEBAR_ROW_INSET).abs() < 0.01, "inset is {inset}");
    h.quit();
}

#[test]
fn pages_show_one_child_and_hide_the_rest() {
    let mut h = harness();
    let p = id(&mut h, "pages");
    let kids = h.ui().children(p);
    assert_eq!(h.widget::<Pages>(p).current(), 0);
    // Every page is laid out at the full body, and only one is visible.
    for (i, k) in kids.iter().enumerate() {
        assert_eq!(h.bounds(*k), h.bounds(p), "page {i} fills the stack");
        assert_eq!(h.ui().is_visible(*k), i == 0);
    }
    // The hidden pages' switches are not Tab-reachable...
    let order = h.ui().focus_order();
    assert!(order.contains(&id(&mut h, "switch_display")));
    assert!(!order.contains(&id(&mut h, "switch_keyboard")));
    // ...but still resolve, and answer a script.
    assert_eq!(
        get_prop(h.ui(), "switch_keyboard", "visible").as_deref(),
        Ok("false")
    );

    // A click at the switch's spot reaches the *visible* page's switch.
    let visible_switch = id(&mut h, "switch_display");
    h.click(visible_switch);
    h.settle();
    assert_eq!(h.state().toggles, [true]);
    assert!(h.widget::<Switch<S>>(visible_switch).is_checked());
    let hidden_switch = id(&mut h, "switch_keyboard");
    assert!(!h.widget::<Switch<S>>(hidden_switch).is_checked());

    // Switch pages: one SetVisible each way, and the same click now
    // reaches page 1's switch. The click above focused page 0's switch;
    // a scripted switch moves no pointer, so `show` drops the focus
    // rather than leaving keystrokes in a widget nobody can see.
    assert_eq!(h.ui().focused(), Some(visible_switch));
    h.tap();
    h.clear_tap();
    h.ui().widget_mut::<Pages>(p).unwrap().show(1);
    h.settle();
    assert_eq!(count(&h, "SetVisible"), 2);
    assert_eq!(count(&h, "SetBounds"), 0);
    assert_eq!(
        h.ui().focused(),
        None,
        "the hidden page's switch lost the focus"
    );
    h.key(key::SPACE);
    h.settle();
    assert_eq!(h.state().toggles, [true], "and Space reaches nothing");
    // The pointer is parked on the switch from the click above; a click
    // without a move is routed to the chain of the last move, so step
    // off and back on as a person would.
    h.move_pointer(Point::new(2.0, 2.0));
    h.click(hidden_switch);
    h.settle();
    assert_eq!(h.state().toggles, [true, true]);
    assert!(h.widget::<Switch<S>>(hidden_switch).is_checked());
    let order = h.ui().focus_order();
    assert!(order.contains(&id(&mut h, "switch_keyboard")));
    assert!(!order.contains(&id(&mut h, "switch_display")));
    h.quit();
}

#[test]
fn a_switch_toggles_like_a_checkbox_and_answers_the_same_actions() {
    let mut h = Harness::sized("switch", S::default(), Size::new(200.0, 80.0), |ui| {
        ui.build(
            nitro_ui::widgets::column().padding(8.0).child(
                switch("Dark")
                    .name("dark")
                    .on_toggle(|s: &mut S, _ui: &mut Ui<S>, on: bool| s.toggles.push(on)),
            ),
        )
    });
    h.settle();
    let sw = id(&mut h, "dark");
    assert_eq!(h.ui().role(sw).unwrap(), Role::Checkbox);
    assert_eq!(
        h.ui().accessible(sw).unwrap().value.as_deref(),
        Some("false")
    );
    let track = Rect::new(
        h.bounds(sw).x + 2.0,
        h.bounds(sw).y + 4.0,
        split::SWITCH_SIZE.0 - 4.0,
        split::SWITCH_SIZE.1 - 8.0,
    );
    let accent = rgb(h.ui().color(ColorRole::Accent));
    let off = h.ink_count(track, accent);

    // A script sets it, exactly as `hey set dark value true` does.
    let (ui, s) = h.parts();
    nitro_ui::introspect::set(ui, s, "dark", "value", "true").unwrap();
    h.settle();
    assert!(h.widget::<Switch<S>>(sw).is_checked());
    assert_eq!(h.state().toggles, [true]);
    let on = h.ink_count(track, accent);
    assert!(on < off, "the track is the accent when on ({on} < {off})");

    // Space toggles it back; a click toggles it on again.
    h.ui().focus(sw);
    h.settle();
    h.key(key::SPACE);
    h.settle();
    assert!(!h.widget::<Switch<S>>(sw).is_checked());
    h.click(sw);
    h.settle();
    assert!(h.widget::<Switch<S>>(sw).is_checked());
    assert_eq!(h.state().toggles, [true, false, true]);

    // A setter does not fire the callback.
    h.ui()
        .widget_mut::<Switch<S>>(sw)
        .unwrap()
        .set_checked(false);
    h.settle();
    assert_eq!(h.state().toggles.len(), 3);
    h.quit();
}

#[test]
fn a_settled_split_view_sends_nothing() {
    let mut h = harness();
    let row = id(&mut h, "nav_keyboard");
    let b = h.bounds(row);
    h.move_pointer(Point::new(b.x + 20.0, b.y + b.h / 2.0));
    h.click(row);
    h.settle();
    h.assert_idle(300);
    h.quit();
}

#[test]
fn up_and_down_walk_the_sidebar() {
    let mut h = harness();
    let first = id(&mut h, "nav_display");
    h.click(first);
    h.settle();
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.state().selected, 1);
    assert_eq!(h.ui().focused(), Some(id(&mut h, "nav_keyboard")));
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.state().selected, 2);
    // The end is the end.
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.state().selected, 2);
    h.key(key::UP);
    h.settle();
    assert_eq!(h.state().selected, 1);
    h.quit();
}

#[test]
fn a_navigation_row_is_a_button_with_a_chevron_and_a_plain_one_is_not() {
    let mut h = Harness::sized("nav", S::default(), Size::new(320.0, 200.0), |ui| {
        ui.build(
            content_column().child(
                card()
                    .child(
                        card_row("Go somewhere")
                            .name("go")
                            .subtitle("A second line")
                            .on_click(|s: &mut S, _ui: &mut Ui<S>| s.clicks.push("go".into())),
                    )
                    .child(card_row("Plain").name("plain").trailing(button("Do"))),
            ),
        )
    });
    h.settle();
    let go = id(&mut h, "go");
    let plain = id(&mut h, "plain");
    assert_eq!(h.ui().role(go).unwrap(), Role::Button);
    assert_eq!(h.ui().role(plain).unwrap(), Role::Container);
    // The chevron is the last child of the navigation row.
    let last = *h.ui().children(go).last().unwrap();
    assert_eq!(h.ui().role(last).unwrap(), Role::Icon);
    assert_eq!(
        h.widget::<nitro_ui::widgets::Icon>(last).name(),
        "chevron-right"
    );
    h.click(go);
    h.settle();
    assert_eq!(h.state().clicks, ["go"]);
    let (ui, s) = h.parts();
    ui.action(s, go, "click", None).unwrap();
    assert_eq!(h.state().clicks, ["go", "go"]);
    // The subtitle made the row taller than the plain one.
    assert!(h.bounds(go).h > h.bounds(plain).h);
    h.quit();
}

#[test]
fn the_sidebar_and_cards_follow_a_scheme_switch() {
    let mut h =
        Harness::sized("scheme", S::default(), Size::new(320.0, 240.0), |ui| {
            split_view()
                .sidebar_child(sidebar_section("Places"))
                .sidebar_child(sidebar_row("house", "Home").name("home").selected(true))
                .sidebar_child(sidebar_separator())
                .sidebar_child(sidebar_row("hdd", "Root").name("root"))
                .content(content_column().child(
                    card().name("card").child(
                        card_row("Field").trailing(text_field("x").name("field").width(80.0)),
                    ),
                ))
                .build(ui)
                .root
        });
    h.settle();
    let sidebar_id = id(&mut h, "sidebar");
    let sidebar = h.bounds(sidebar_id);
    // The top-left corner of the pane, above the first section header
    // (the bottom of a window can fall off the output in the harness).
    let probe = Rect::new(sidebar.x + 1.0, sidebar.y + 1.0, 3.0, 3.0);
    let light = rgb(Palette::light().get(ColorRole::SidebarBackground));
    let dark = rgb(Palette::dark().get(ColorRole::SidebarBackground));
    assert!(!h.has_ink(probe, light), "the sidebar is the light role");
    let card_id = id(&mut h, "card");
    let card = h.bounds(card_id);
    let ring = Rect::new(card.x + card.w / 2.0, card.y, 4.0, 1.0);
    assert!(
        h.has_ink(ring, rgb(Palette::light().get(ColorRole::Surface))),
        "the card has a ring"
    );

    h.ui().set_palette(Palette::dark());
    h.settle();
    assert!(!h.has_ink(probe, dark), "and the dark one after the switch");
    assert!(h.has_ink(probe, light));
    assert!(
        h.has_ink(ring, rgb(Palette::dark().get(ColorRole::Surface))),
        "the ring moved with it"
    );
    h.assert_idle(100);
    h.quit();
}

#[test]
fn a_hidden_widget_is_not_hit_and_not_focusable() {
    // The framework half of `Pages`, on its own: `set_node_visible`
    // hides a subtree from the pointer and from Tab, keeps it in the
    // tree, and costs one `SetVisible` each way.
    let mut h = Harness::sized("hidden", S::default(), Size::new(200.0, 100.0), |ui| {
        ui.build(
            nitro_ui::widgets::column()
                .padding(8.0)
                .gap(4.0)
                .child(button("A").name("a"))
                .child(
                    nitro_ui::widgets::row()
                        .name("box")
                        .child(
                            button("B")
                                .name("b")
                                .on_click(|s: &mut S, _ui: &mut Ui<S>| {
                                    s.clicks.push("b".into());
                                }),
                        ),
                ),
        )
    });
    h.settle();
    let bx = id(&mut h, "box");
    let b = id(&mut h, "b");
    assert_eq!(h.ui().focus_order(), vec![id(&mut h, "a"), b]);

    h.tap();
    h.clear_tap();
    h.ui().set_node_visible(bx, false);
    h.settle();
    assert_eq!(ops(&h), ["SetVisible", "Commit"]);
    assert_eq!(h.ui().focus_order(), vec![id(&mut h, "a")]);
    assert!(resolve(h.ui(), "b").is_some(), "still in the tree");
    let n = h.ui().introspect_node(b).unwrap();
    assert!(!n.visible);
    assert!(!n.bounds.is_empty(), "and still laid out");
    h.click(b);
    h.settle();
    assert!(h.state().clicks.is_empty(), "the pointer does not enter it");
    assert_eq!(get_prop(h.ui(), "b", "visible").as_deref(), Ok("false"));

    h.clear_tap();
    h.ui().set_node_visible(bx, true);
    h.settle();
    assert_eq!(ops(&h), ["SetVisible", "Commit"]);
    h.ui().set_node_visible(bx, true);
    h.settle();
    h.assert_idle(50);
    h.move_pointer(Point::new(2.0, 2.0));
    h.click(b);
    h.settle();
    assert_eq!(h.state().clicks, ["b"]);
    h.quit();
}
