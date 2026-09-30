//! `menu_button`: an icon button whose menu is a grabbing popup, driven
//! against the real server on the in-process harness.

use nitro_core::{Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{button, column, row, spacer};
use nitro_ui::{MenuButton, MenuItem, Surface, Ui, WidgetId, WindowId, menu_button};

#[derive(Default)]
struct St {
    picked: Vec<String>,
    other_clicks: u32,
}

fn menu() -> nitro_ui::menu::MenuButtonBuilder<St> {
    menu_button("gear")
        .name("menu")
        .label("Session")
        .item(MenuItem::new("nitro", "Nitro").radio(true))
        .item(MenuItem::new("console", "Text console").radio(false))
        .separator()
        .item(MenuItem::new("off", "Power off").icon("power").disabled())
        .item(MenuItem::new("reboot", "Restart").icon("bootstrap-reboot"))
        .on_select(|s: &mut St, _ui: &mut Ui<St>, id: &str| s.picked.push(id.to_owned()))
}

fn tree(ui: &mut Ui<St>) -> WidgetId {
    ui.build(
        column()
            .padding(8.0)
            .gap(8.0)
            // Above the menu button, so the menu never covers it.
            .child(
                button("Other")
                    .name("other")
                    .on_click(|s: &mut St, _ui: &mut Ui<St>| s.other_clicks += 1),
            )
            .child(row().child(menu()).child(spacer().grow(1.0))),
    )
}

fn harness() -> Harness<St> {
    Harness::sized("menu", St::default(), Size::new(240.0, 200.0), tree)
}

fn named(h: &mut Harness<St>, path: &str) -> WidgetId {
    introspect::resolve(h.ui(), path).unwrap_or_else(|| panic!("no widget at {path}"))
}

fn popup_of(h: &mut Harness<St>, b: WidgetId) -> Option<WindowId> {
    h.ui().widget::<MenuButton<St>>(b).unwrap().popup()
}

#[test]
fn a_click_opens_the_menu_under_the_button() {
    let mut h = harness();
    let b = named(&mut h, "window/menu");
    h.click(b);
    let pop = popup_of(&mut h, b).expect("open");
    assert!(h.ui().has_window(pop));
    let console = named(&mut h, "window[1]/console");
    assert_eq!(h.ui().window_of(console), Some(pop));
    let a = h.ui().window_bounds(b);
    let main = h.ui().window_position_of(WindowId::MAIN);
    let p = h.ui().window_position_of(pop);
    assert!(
        (p.y - (main.y + a.bottom()).ceil()).abs() <= 1.0,
        "top at the button's bottom: popup {p:?} button {a:?} main {main:?}"
    );
    assert!(
        (p.x - (main.x + a.x).floor()).abs() <= 1.0,
        "left edges aligned"
    );
    let size = h.ui().window_size_of(pop);
    assert!(size.w >= nitro_ui::menu::MENU_MIN_W, "{size:?}");
    assert_eq!(
        introspect::resolve(h.ui(), "window[1]/console")
            .and_then(|id| h.ui().accessible(id).ok())
            .and_then(|a| a.value),
        Some("false".to_owned())
    );
    h.quit();
}

#[test]
fn keys_on_the_parent_navigate_and_activate() {
    let mut h = harness();
    let b = named(&mut h, "window/menu");
    h.ui().focus(b);
    h.settle();
    // Down opens with the checked item (nitro) highlighted; Down moves to
    // console; Down skips the separator and the disabled item.
    h.key(key::DOWN);
    let pop = popup_of(&mut h, b).expect("Down opens");
    h.key(key::DOWN);
    h.key(key::DOWN);
    h.key(key::ENTER);
    assert_eq!(h.state().picked, vec!["reboot".to_owned()]);
    assert!(!h.ui().has_window(pop), "activation closes");
    assert!(popup_of(&mut h, b).is_none());
    assert!(h.ui().is_focused(b), "focus is back on the button");
    // And it reopens; Up wraps to the last item from the checked one.
    h.key(key::ENTER);
    assert!(popup_of(&mut h, b).is_some());
    h.key(key::UP);
    h.key(key::UP);
    h.key(key::SPACE);
    assert_eq!(
        h.state().picked,
        vec!["reboot".to_owned(), "console".to_owned()]
    );
    h.quit();
}

#[test]
fn hover_and_click_activate_but_a_disabled_row_does_nothing() {
    let mut h = harness();
    let b = named(&mut h, "window/menu");
    h.click(b);
    let pop = popup_of(&mut h, b).unwrap();
    let off = named(&mut h, "window[1]/off");
    h.click(off);
    assert!(h.state().picked.is_empty());
    assert!(h.ui().has_window(pop), "still open");
    let console = named(&mut h, "window[1]/console");
    let cb = h.bounds(console);
    h.move_pointer_in(pop, Point::new(cb.x + 20.0, cb.y + cb.h / 2.0));
    let list = named(&mut h, "window[1]");
    assert_eq!(
        h.ui().accessible(list).unwrap().value,
        Some("console".to_owned()),
        "hover highlights"
    );
    h.click(console);
    assert_eq!(h.state().picked, vec!["console".to_owned()]);
    assert!(!h.ui().has_window(pop));
    h.quit();
}

#[test]
fn an_outside_click_dismisses_and_the_menu_reopens() {
    let mut h = harness();
    let b = named(&mut h, "window/menu");
    let other = named(&mut h, "window/other");
    h.click(b);
    let pop = popup_of(&mut h, b).unwrap();
    h.click(other);
    assert!(!h.ui().has_window(pop), "dismissed");
    assert_eq!(h.state().other_clicks, 0, "the press was consumed");
    assert!(popup_of(&mut h, b).is_none());
    assert!(h.state().picked.is_empty());
    h.click(b);
    assert!(popup_of(&mut h, b).is_some(), "reopens");
    h.quit();
}

#[test]
fn escape_dismisses_and_returns_focus() {
    let mut h = harness();
    let b = named(&mut h, "window/menu");
    h.click(b);
    let pop = popup_of(&mut h, b).unwrap();
    h.key(key::ESC);
    assert!(!h.ui().has_window(pop));
    assert!(popup_of(&mut h, b).is_none());
    assert!(h.ui().is_focused(b));
    assert!(h.state().picked.is_empty());
    h.quit();
}

#[test]
fn set_checked_moves_the_radio_mark() {
    let mut h = harness();
    let b = named(&mut h, "window/menu");
    h.click(b);
    h.ui()
        .widget_mut::<MenuButton<St>>(b)
        .unwrap()
        .set_checked("console", true);
    h.settle();
    let value = |h: &mut Harness<St>, path: &str| {
        let id = named(h, path);
        h.ui().accessible(id).unwrap().value
    };
    assert_eq!(value(&mut h, "window[1]/console"), Some("true".to_owned()));
    assert_eq!(value(&mut h, "window[1]/nitro"), Some("false".to_owned()));
    h.quit();
}

/// A greeter: a fullscreen lock surface is the only client, and the
/// button sits in its bottom-right corner.
fn corner(ui: &mut Ui<St>) -> WidgetId {
    ui.build(
        column()
            .width_percent(1.0)
            .height_percent(1.0)
            .padding(8.0)
            .child(spacer().grow(1.0))
            .child(
                row()
                    .width_percent(1.0)
                    .child(spacer().grow(1.0))
                    .child(menu().align_right(true)),
            ),
    )
}

#[test]
fn a_menu_in_a_screen_corner_flips_above_its_button() {
    let mut h = Harness::shell("greeter", St::default(), Surface::lock(), None, corner);
    h.settle();
    let btn = named(&mut h, "window/menu");
    h.click(btn);
    let pop = popup_of(&mut h, btn).expect("open");
    let main = h.ui().window_position_of(WindowId::MAIN);
    let anchor = h.ui().window_bounds(btn);
    let pos = h.ui().window_position_of(pop);
    let size = h.ui().window_size_of(pop);
    assert!(
        pos.y + size.h <= main.y + anchor.y + 1.0,
        "above the button: popup {pos:?} {size:?} button {anchor:?}"
    );
    let (ow, _) = nitro_ui::test::OUTPUT;
    assert!(
        pos.x >= 0.0 && pos.x + size.w <= ow as f32 + 0.5,
        "on the output: {pos:?} {size:?}"
    );
    // Keys addressed to the popup (a keyboard grab) navigate too. A
    // pointer-opened menu starts with nothing highlighted.
    h.key_in(pop, key::DOWN);
    h.key_in(pop, key::DOWN);
    h.key_in(pop, key::ENTER);
    assert_eq!(h.state().picked, vec!["console".to_owned()]);
    assert!(!h.ui().has_window(pop));
    h.quit();
}
