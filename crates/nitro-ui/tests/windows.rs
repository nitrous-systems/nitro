//! Several windows per app: `Ui::add_window` and everything routed by
//! the `window` a server message names.
//!
//! The dialog under test is what a file picker is: a second window with
//! its own tree, focus, hover and size, opened by an app that keeps
//! running when the dialog closes.

use nitro_core::{Point, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::event::key;
use nitro_ui::introspect;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{button, column};
use nitro_ui::{Error, Ui, WidgetId, WindowId};
use nitro_wire::msg::{PointerMotion, ServerMsg};

#[derive(Default)]
struct St {
    main_clicks: u32,
    dialog_clicks: u32,
    dialog_resized: Vec<Size>,
    main_resized: u32,
    closed: u32,
}

fn main_tree(ui: &mut Ui<St>) -> WidgetId {
    ui.on_resize(|s: &mut St, _ui: &mut Ui<St>, _| s.main_resized += 1);
    ui.build(
        column().padding(8.0).child(
            button("Main")
                .name("main_ok")
                .on_click(|s: &mut St, _ui: &mut Ui<St>| s.main_clicks += 1),
        ),
    )
}

fn dialog_tree(ui: &mut Ui<St>) -> WidgetId {
    ui.build(
        column()
            .padding(8.0)
            .gap(4.0)
            .child(
                button("Open")
                    .name("open")
                    .on_click(|s: &mut St, _ui: &mut Ui<St>| s.dialog_clicks += 1),
            )
            .child(button("Cancel").name("cancel")),
    )
}

fn harness() -> (Harness<St>, WindowId) {
    let mut h = Harness::sized("windows", St::default(), Size::new(240.0, 160.0), main_tree);
    let dialog = h.open_window("Pick", Some(Size::new(140.0, 90.0)), dialog_tree);
    (h, dialog)
}

fn named(h: &mut Harness<St>, path: &str) -> WidgetId {
    introspect::resolve(h.ui(), path).unwrap_or_else(|| panic!("no widget at {path}"))
}

#[test]
fn a_second_window_is_a_real_window_and_the_server_focuses_it() {
    let (mut h, dialog) = harness();
    assert_eq!(h.server().stat("windows"), 2, "the server has both windows");
    assert_eq!(h.ui().windows(), vec![WindowId::MAIN, dialog]);
    assert_eq!(h.ui().window_size_of(dialog), Size::new(140.0, 90.0));
    assert_eq!(h.ui().window_title_of(dialog), "Pick");
    // The server focuses a newly placed window, and says so; that is
    // what makes it the one keys and `focused()` speak about, and what
    // a pointer-driven test below relies on (it is also on top).
    assert_eq!(h.ui().active_window(), dialog);

    // Both trees painted: each window's shot has more than one colour,
    // its backdrop and at least a button on it.
    let main = h.shot();
    let dlg = h.shot_window(dialog);
    assert_eq!((dlg.width, dlg.height), (140, 90));
    let colours = |img: &nitro_server::test_support::Image| {
        let mut seen = std::collections::HashSet::new();
        for y in 0..img.height {
            for x in 0..img.width {
                seen.insert(img.pixel(x, y) & 0x00ff_ffff);
            }
        }
        seen.len()
    };
    assert!(colours(&dlg) > 1, "the dialog's tree is on screen");
    assert!(colours(&main) > 1, "the main tree is on screen");
    h.quit();
}

#[test]
fn a_click_in_the_dialog_reaches_the_dialog_only() {
    let (mut h, dialog) = harness();
    let open = named(&mut h, "window[1]/open");
    assert_eq!(h.ui().window_of(open), Some(dialog));
    h.click(open);
    assert_eq!(h.state().dialog_clicks, 1);
    assert_eq!(h.state().main_clicks, 0, "the main window saw nothing");
    // The click focused the button inside the dialog, and only there.
    assert_eq!(h.ui().focused_in(dialog), Some(open));
    assert_eq!(h.ui().focused_in(WindowId::MAIN), None);
    h.quit();
}

#[test]
fn hover_is_per_window() {
    let (mut h, dialog) = harness();
    let main_ok = named(&mut h, "window/main_ok");
    let open = named(&mut h, "window[1]/open");
    let motion = |win: WindowId, pos: Point| {
        ServerMsg::PointerMotion(PointerMotion {
            window: win.raw(),
            node: nitro_wire::types::NodeId::NONE,
            pos,
            time_ns: 1,
        })
    };
    let (ui, st) = h.parts();
    let b = ui.window_bounds(main_ok);
    ui.dispatch(
        st,
        &motion(WindowId::MAIN, Point::new(b.x + 2.0, b.y + 2.0)),
    );
    let d = ui.window_bounds(open);
    ui.dispatch(st, &motion(dialog, Point::new(d.x + 2.0, d.y + 2.0)));
    // Hovering the dialog did not un-hover the main window's button: the
    // two hover chains are separate, and each is left by its own
    // `PointerLeave`.
    assert!(ui.is_hovered(main_ok));
    assert!(ui.is_hovered(open));
    ui.dispatch(
        st,
        &ServerMsg::PointerLeave(nitro_wire::msg::PointerLeave {
            window: dialog.raw(),
            time_ns: 2,
        }),
    );
    assert!(!ui.is_hovered(open));
    assert!(ui.is_hovered(main_ok));
    h.quit();
}

#[test]
fn keys_go_to_the_named_window_and_tab_stays_inside_it() {
    let (mut h, dialog) = harness();
    let open = named(&mut h, "window[1]/open");
    let cancel = named(&mut h, "window[1]/cancel");
    let main_ok = named(&mut h, "window/main_ok");

    h.key_in(dialog, key::TAB);
    assert_eq!(h.ui().focused_in(dialog), Some(open));
    h.key_in(dialog, key::TAB);
    assert_eq!(h.ui().focused_in(dialog), Some(cancel));
    h.key_in(dialog, key::TAB);
    assert_eq!(
        h.ui().focused_in(dialog),
        Some(open),
        "Tab wraps inside the dialog"
    );
    assert_eq!(h.ui().focused_in(WindowId::MAIN), None, "and never left it");

    // Tab in the main window moves the main window's focus, and leaves
    // the dialog's where it was.
    h.key_in(WindowId::MAIN, key::TAB);
    assert_eq!(h.ui().focused_in(WindowId::MAIN), Some(main_ok));
    assert_eq!(h.ui().focused_in(dialog), Some(open));

    // Return goes to the dialog's focused button.
    h.key_in(dialog, key::ENTER);
    assert_eq!(h.state().dialog_clicks, 1);
    assert_eq!(h.state().main_clicks, 0);

    // A real key goes through the server to the window it focused: the
    // dialog.
    h.key(key::ENTER);
    assert_eq!(h.state().dialog_clicks, 2);
    assert_eq!(h.state().main_clicks, 0);
    h.quit();
}

#[test]
fn a_configure_on_the_dialog_resizes_only_the_dialog() {
    let (mut h, dialog) = harness();
    h.ui()
        .on_window_resize(dialog, |s: &mut St, _ui: &mut Ui<St>, size| {
            s.dialog_resized.push(size);
        });
    let resized_before = h.state().main_resized;
    h.configure_window(dialog, Size::new(180.0, 100.0));
    assert_eq!(h.ui().window_size_of(dialog), Size::new(180.0, 100.0));
    assert_eq!(h.ui().window_size(), Size::new(240.0, 160.0));
    assert_eq!(h.state().dialog_resized, [Size::new(180.0, 100.0)]);
    assert_eq!(
        h.state().main_resized,
        resized_before,
        "on_resize is the main window's"
    );
    let root = h.ui().root_of(dialog).unwrap();
    assert_eq!(
        h.bounds(root).size(),
        Size::new(180.0, 100.0),
        "the dialog re-laid out"
    );
    h.quit();
}

#[test]
fn closing_the_dialog_from_the_server_runs_the_callback_and_does_not_quit() {
    let (mut h, dialog) = harness();
    let open = named(&mut h, "window[1]/open");
    h.ui()
        .on_window_closed(dialog, |s: &mut St, _ui: &mut Ui<St>| s.closed += 1);
    let before = h.ui().widget_count();

    h.close_from_server(dialog);
    assert_eq!(h.state().closed, 1);
    assert!(
        !h.ui().should_quit(),
        "a dialog closing is not the app closing"
    );
    assert!(!h.ui().has_window(dialog));
    assert_eq!(h.ui().windows(), vec![WindowId::MAIN]);
    assert!(
        h.ui()
            .widget(open)
            .map(|_: &nitro_ui::widgets::Button<St>| ())
            .is_err()
    );
    assert_eq!(
        h.ui().widget_count(),
        before - 3,
        "the dialog's widgets are gone"
    );
    // `Closed` is a request: the server drops the window when the client
    // destroys it, which the toolkit does on the way out.
    assert_eq!(
        h.server().stat("windows"),
        1,
        "the server dropped the dialog"
    );
    // The app keeps working.
    let main_ok = named(&mut h, "window/main_ok");
    h.click(main_ok);
    assert_eq!(h.state().main_clicks, 1);
    h.quit();
}

#[test]
fn remove_window_closes_it_on_the_server_and_does_not_quit() {
    let (mut h, dialog) = harness();
    h.ui()
        .on_window_closed(dialog, |s: &mut St, _ui: &mut Ui<St>| s.closed += 1);
    h.remove_window(dialog);
    assert_eq!(h.state().closed, 1);
    assert!(!h.ui().should_quit());
    assert_eq!(
        h.server().stat("windows"),
        1,
        "the server dropped the dialog"
    );
    // The server's `Closed` for a window we removed ourselves arrived
    // during settle and was not taken for the main window's.
    assert!(!h.ui().should_quit());
    assert!(matches!(
        h.ui().remove_window(&mut St::default(), dialog),
        Err(Error::NoWindow)
    ));

    // The main window still works and still paints.
    let main_ok = named(&mut h, "window/main_ok");
    h.click(main_ok);
    assert_eq!(h.state().main_clicks, 1);
    h.quit();
}

#[test]
fn closing_the_main_window_quits() {
    let (mut h, dialog) = harness();
    h.close_from_server(dialog);
    assert!(!h.ui().should_quit());
    h.close_from_server(WindowId::MAIN);
    assert!(h.ui().should_quit());
    h.quit();
}

#[test]
fn a_window_root_must_be_a_free_widget() {
    let (mut h, dialog) = harness();
    let ui = h.ui();
    let main_root = ui.root().unwrap();
    assert!(matches!(
        ui.add_window("x", None, main_root),
        Err(Error::NotRoot)
    ));
    let dialog_root = ui.root_of(dialog).unwrap();
    assert!(matches!(
        ui.add_window("x", None, dialog_root),
        Err(Error::NotRoot)
    ));
    let child = ui.children(main_root)[0];
    assert!(matches!(
        ui.add_window("x", None, child),
        Err(Error::NotRoot)
    ));
    h.quit();
}

#[test]
fn introspection_paths_name_the_window() {
    let (mut h, dialog) = harness();
    let open = named(&mut h, "window[1]/open");
    let ui = h.ui();
    let path = introspect::path_of(ui, open).unwrap();
    assert!(path.starts_with("window[1]/"), "{path}");
    assert_eq!(
        introspect::resolve(ui, &path),
        Some(open),
        "path_of round-trips"
    );
    assert_eq!(introspect::resolve(ui, "window[1]"), ui.root_of(dialog));
    assert_eq!(introspect::resolve(ui, "window[0]"), ui.root());
    assert_eq!(introspect::resolve(ui, "window[2]"), None);
    // The main window's paths are exactly what they were.
    let main_ok = introspect::resolve(ui, "window/main_ok").unwrap();
    assert!(
        introspect::path_of(ui, main_ok)
            .unwrap()
            .starts_with("window/")
    );
    // A dialog's widget is not reachable through the main window.
    assert_eq!(introspect::resolve(ui, "window/open"), None);
    h.quit();
}
