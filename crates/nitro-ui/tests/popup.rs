//! Popups: `Ui::add_popup` and the server's `PopupDone`.
//!
//! The server does the placement and the dismissal (`docs/wm.md`
//! §Popups); what these assert is the toolkit's half — that a popup is
//! an ordinary secondary window to the app, that the server's dismissal
//! runs the app's close handlers, and that swapping one popup for
//! another is a single commit.

use nitro_core::{Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{button, column, row, spacer};
use nitro_ui::{PopupPlacement, Ui, WidgetId, WindowId};

#[derive(Default)]
struct St {
    main_clicks: u32,
    item_clicks: u32,
    closed: u32,
    popup: Option<WindowId>,
}

fn main_tree(ui: &mut Ui<St>) -> WidgetId {
    ui.build(
        column().padding(8.0).gap(8.0).child(
            row().width_percent(1.0).child(spacer().grow(1.0)).child(
                button("Menu")
                    .name("menu")
                    .on_click(|s: &mut St, _ui: &mut Ui<St>| s.main_clicks += 1),
            ),
        ),
    )
}

fn menu_tree(ui: &mut Ui<St>, height: f32) -> WidgetId {
    ui.build(
        column().padding(6.0).height(height).child(
            button("Item")
                .name("item")
                .on_click(|s: &mut St, _ui: &mut Ui<St>| s.item_clicks += 1),
        ),
    )
}

fn named(h: &mut Harness<St>, path: &str) -> WidgetId {
    introspect::resolve(h.ui(), path).unwrap_or_else(|| panic!("no widget at {path}"))
}

/// Open the menu under the "menu" button, as a bar's status pill would.
fn open_menu(h: &mut Harness<St>, height: f32) -> WindowId {
    let anchor = named(h, "window/menu");
    // Window coordinates: the anchor rectangle is in the parent's space.
    let rect = h.ui().window_bounds(anchor);
    let root = menu_tree(h.ui(), height);
    let win = h
        .ui()
        .add_popup(
            WindowId::MAIN,
            PopupPlacement::below(rect),
            Some(Size::new(120.0, height)),
            root,
        )
        .expect("add popup");
    h.ui()
        .on_window_closed(win, |s: &mut St, _ui: &mut Ui<St>| {
            s.closed += 1;
            s.popup = None;
        });
    h.state_mut().popup = Some(win);
    h.settle();
    win
}

fn harness() -> Harness<St> {
    Harness::sized("popup", St::default(), Size::new(240.0, 160.0), main_tree)
}

#[test]
fn a_popup_hangs_under_its_anchor_right_edges_aligned() {
    let mut h = harness();
    let pop = open_menu(&mut h, 60.0);
    let anchor = named(&mut h, "window/menu");
    let a = h.ui().window_bounds(anchor);
    let main = h.ui().window_position_of(WindowId::MAIN);
    let p = h.ui().window_position_of(pop);
    assert_eq!(h.ui().window_size_of(pop), Size::new(120.0, 60.0));
    // Right edges aligned, top edge at the anchor's bottom.
    assert!(
        ((p.x + 120.0) - (main.x + a.right()).ceil()).abs() <= 1.0,
        "right edges: popup {p:?} anchor {a:?} main {main:?}"
    );
    assert!(
        (p.y - (main.y + a.bottom()).ceil()).abs() <= 1.0,
        "top at the anchor's bottom: popup {p:?} anchor {a:?}"
    );
    // It is a real window: its content is reachable and clickable.
    let item = named(&mut h, "window[1]/item");
    assert_eq!(h.ui().window_of(item), Some(pop));
    h.click(item);
    assert_eq!(h.state().item_clicks, 1);
    assert_eq!(h.state().closed, 0, "a click inside does not dismiss");
    h.quit();
}

#[test]
fn an_outside_press_dismisses_and_is_not_delivered() {
    let mut h = harness();
    let pop = open_menu(&mut h, 60.0);
    // The anchor button is outside the popup: the press dismisses and is
    // consumed, so the button's click never happens — which is what makes
    // a second click on a toggle button close its menu rather than
    // re-open it.
    let menu = named(&mut h, "window/menu");
    h.click(menu);
    assert_eq!(h.state().closed, 1, "the close handler ran");
    assert_eq!(h.state().popup, None);
    assert_eq!(h.state().main_clicks, 0, "the press was consumed");
    assert!(!h.ui().has_window(pop));
    // With the menu gone the same click reaches the button again.
    h.click(menu);
    assert_eq!(h.state().main_clicks, 1);
    h.quit();
}

#[test]
fn escape_dismisses_a_grabbing_popup() {
    let mut h = harness();
    let pop = open_menu(&mut h, 60.0);
    h.key(key::ESC);
    assert_eq!(h.state().closed, 1);
    assert!(!h.ui().has_window(pop));
    h.quit();
}

#[test]
fn swapping_a_popup_for_a_taller_one_is_one_commit() {
    let mut h = harness();
    let old = open_menu(&mut h, 40.0);
    let anchor = named(&mut h, "window/menu");
    let rect = h.ui().window_bounds(anchor);
    h.tap();
    let root = menu_tree(h.ui(), 90.0);
    let new = h
        .ui()
        .add_popup(
            WindowId::MAIN,
            PopupPlacement::below(rect),
            Some(Size::new(120.0, 90.0)),
            root,
        )
        .unwrap();
    let (ui, st) = h.parts();
    ui.remove_window(st, old).unwrap();
    assert!(h.flush(), "the swap commits");
    let commits = h.mutations().iter().filter(|m| m.op == "Commit").count();
    assert_eq!(commits, 1, "create + destroy in one transaction");
    let ops: Vec<_> = h.mutations().iter().map(|m| m.op).collect();
    assert!(ops.contains(&"CreatePopup"), "{ops:?}");
    assert!(ops.contains(&"DestroyNode"), "{ops:?}");
    h.settle();
    assert!(h.ui().has_window(new));
    assert_eq!(h.ui().window_size_of(new), Size::new(120.0, 90.0));
    // The old one's close handler ran (the app removed it); the new one is
    // live and still dismisses on an outside press.
    assert_eq!(h.state().closed, 1);
    h.click_at(Point::new(10.0, 150.0));
    assert!(!h.ui().has_window(new));
    h.quit();
}
