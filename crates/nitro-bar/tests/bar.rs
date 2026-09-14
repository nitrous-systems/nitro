//! The bar, driven through a real server on the shell socket.
//!
//! Every test here builds the tree the binary builds ([`nitro_bar::build`])
//! and drives it the way the desktop does: the bar connects to
//! `shell.sock`, reserves its strip, and learns about windows from the
//! server's own `WindowInfo` events. Nothing pokes the bar's state
//! directly to set up a case that the wire would not produce — a window
//! list assembled by hand would be a second bar, and the bugs worth
//! catching live in the path between the two.
//!
//! The one thing that *is* faked is the wall clock
//! (`Bar::set_fake_time_ms`), because the alternative is a test that
//! takes a minute to find out whether the clock ticks once.

use nitro_bar::{Bar, build, names};
use nitro_core::Rect;
use nitro_ui::event::button;
use nitro_ui::shell::Surface;
use nitro_ui::test::Harness;
use nitro_ui::widgets::Label;
use nitro_ui::{Size, WidgetId};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{Layer, NodeId, WindowState};

/// The bar's height, and so the strip it reserves.
const BAR_H: f32 = 32.0;

/// A bar on the shell socket, spanning the harness's output.
fn harness() -> Harness<Bar> {
    bar(Bar::new())
}

/// A bar built around an already-configured state.
fn bar(state: Bar) -> Harness<Bar> {
    Harness::shell(
        "nitro-bar",
        state,
        Surface::bar(BAR_H as u32),
        Some(Size::new(320.0, BAR_H)),
        build,
    )
}

/// The widget named `name`, found the way `hey` finds it.
fn named(h: &mut Harness<Bar>, name: &str) -> Option<WidgetId> {
    nitro_ui::introspect::resolve(h.ui(), &format!("window/{name}"))
}

/// The text of a named label.
fn label_text(h: &mut Harness<Bar>, name: &str) -> String {
    let id = named(h, name).unwrap_or_else(|| panic!("no widget named {name}"));
    h.widget::<Label>(id).text().to_owned()
}

/// Open an ordinary client window on the harness's **wire** socket, and
/// return the connection plus its node id. It is a real second client, so
/// the bar learns about it exactly as it would on a desktop.
fn open_window(h: &Harness<Bar>, title: &str, size: Size) -> Connection {
    let mut conn = Connection::connect(h.server().wire_path(), title).expect("wire connect");
    conn.tx()
        .create_window(NodeId(1), title, size, Layer::Normal)
        .create_rect(NodeId(2), NodeId(1), Rect::new(0.0, 0.0, size.w, size.h))
        .fill_solid(NodeId(2), nitro_core::Color::rgb(0x40, 0x80, 0xC0))
        .set_app_id(NodeId(1), title)
        .commit(1)
        .expect("commit");
    while !conn.flush().expect("flush") {}
    conn
}

/// Pump the harness until `f` holds, letting the other client's window
/// and the server's notifications arrive.
fn until(h: &mut Harness<Bar>, what: &str, f: impl Fn(&Harness<Bar>) -> bool) {
    for _ in 0..400 {
        if f(h) {
            return;
        }
        h.settle();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn the_bar_reserves_its_strip_and_a_maximized_window_lands_below_it() {
    // The headline: the bar's exclusive zone is what makes it a bar
    // rather than a window that happens to be on top. A maximized client
    // must come back 32 px shorter and 32 px lower.
    let mut h = harness();
    h.settle();
    assert_eq!(
        h.server().stat("exclusive_zones"),
        1,
        "the bar reserves exactly one zone"
    );
    assert_eq!(
        h.server().stat("shell_clients"),
        1,
        "and it is on the privileged socket"
    );

    let mut conn = open_window(&h, "victim", Size::new(160.0, 120.0));
    conn.tx()
        .set_window_state(NodeId(1), WindowState::Maximized)
        .commit(2)
        .expect("maximize");
    while !conn.flush().expect("flush") {}

    // The last `Configure` the client is given is the maximized geometry,
    // measured against the work area the zone has already shrunk.
    let mut last = None;
    for _ in 0..400 {
        h.settle();
        let mut batch = Vec::new();
        let _ = conn.poll(&mut batch);
        for m in batch {
            if let ServerMsg::Configure(c) = m {
                last = Some(c);
            }
        }
        if last.as_ref().is_some_and(|c| c.position.y >= BAR_H) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let c = last.expect("the maximized window was configured");
    // The *frame* starts at the bar's edge; the client is told where its
    // content landed, which is one decoration inset further in. So the
    // assertion is against the unmaximized placement (a centred cascade,
    // far down the output) and against the work area's height, not
    // against 32 exactly — the zone's effect is that the whole thing
    // moved up to the top of what is left of the desktop.
    assert!(
        c.position.y > BAR_H,
        "a maximized window starts below the bar: {}",
        c.position.y
    );
    assert!(
        c.position.y < BAR_H + 32.0,
        "and immediately below it, not cascaded down the screen: {}",
        c.position.y
    );
    // The height is the output minus the bar's strip, minus the frame.
    // Without the zone it would be the full 240 minus the frame, so the
    // difference the zone makes is the thing being asserted.
    let without_zone = 240.0 - (c.size.h + c.position.y);
    assert!(
        without_zone.abs() < 4.0,
        "the window fills the work area the zone left: {c:?}"
    );
    assert!(
        c.size.h < 240.0 - BAR_H,
        "and is shorter than the output by at least the bar: {}",
        c.size.h
    );

    drop(conn);
    h.quit();
}

#[test]
fn the_window_list_tracks_windows_appearing_retitling_and_closing() {
    // One button per window, and the label follows the title: the three
    // events a task list is made of, through the real subscription.
    let mut h = harness();
    h.settle();
    assert_eq!(h.state().window_count(), 0, "nothing open yet");

    let mut conn = open_window(&h, "first", Size::new(120.0, 90.0));
    until(&mut h, "the window to be listed", |h| {
        h.state().window_count() == 1
    });
    assert_eq!(h.state().window_labels(), vec!["first".to_owned()]);
    // And it is addressable by the server's own window id, so a script's
    // path is stable for the window's whole life.
    let name = nitro_bar::entry_name(
        h.state()
            .focused_window()
            .or_else(|| h.state().windows().first().copied())
            .expect("a listed window"),
    );
    assert!(named(&mut h, &name).is_some(), "addressable as {name}");

    // A retitle is a `WindowInfo` with a new title, not a new entry.
    conn.tx()
        .set_window_title(NodeId(1), "renamed")
        .commit(3)
        .expect("retitle");
    while !conn.flush().expect("flush") {}
    until(&mut h, "the retitle", |h| {
        h.state().window_labels() == vec!["renamed".to_owned()]
    });
    assert_eq!(h.state().window_count(), 1, "a retitle is not a new entry");

    // A second window is a second button.
    let conn2 = open_window(&h, "second", Size::new(120.0, 90.0));
    until(&mut h, "the second window", |h| {
        h.state().window_count() == 2
    });

    // And closing one takes its button with it.
    drop(conn2);
    until(&mut h, "the close", |h| h.state().window_count() == 1);
    assert_eq!(h.state().window_labels(), vec!["renamed".to_owned()]);

    drop(conn);
    h.quit();
}

#[test]
fn the_bar_does_not_list_itself() {
    // The bar is a window like any other as far as the server is
    // concerned. Listing itself would give the user a button that focuses
    // a NO_FOCUS panel — a row that does nothing.
    let mut h = harness();
    h.settle();
    let conn = open_window(&h, "only-one", Size::new(120.0, 90.0));
    until(&mut h, "the window", |h| h.state().window_count() == 1);
    assert_eq!(h.state().window_labels(), vec!["only-one".to_owned()]);
    drop(conn);
    h.quit();
}

#[test]
fn clicking_a_window_list_button_focuses_that_window() {
    // The click path end to end: a real click on the bar's button sends
    // `FocusWindow`, and the server focuses the other client's window —
    // which comes back to the bar as a `WindowInfo` with `focused: true`.
    let mut h = harness();
    h.settle();
    let a = open_window(&h, "alpha", Size::new(120.0, 90.0));
    until(&mut h, "alpha", |h| h.state().window_count() == 1);
    let b = open_window(&h, "beta", Size::new(120.0, 90.0));
    until(&mut h, "beta", |h| h.state().window_count() == 2);

    // The newest window took focus when it was placed, so focusing the
    // *first* one is a real change rather than a no-op.
    let first = h.state().windows()[0];
    let id = named(&mut h, &nitro_bar::entry_name(first)).expect("alpha's button");
    h.click(id);
    until(&mut h, "the focus to move", |h| {
        h.state().focused_window() == Some(first)
    });

    drop((a, b));
    h.quit();
}

#[test]
fn middle_clicking_a_window_list_button_asks_that_window_to_close() {
    // `CloseWindow` is a *request*: the owning client is told and
    // decides. So the assertion is that the client got `Closed`, not that
    // the window vanished — a client with unsaved work may refuse.
    let mut h = harness();
    h.settle();
    let mut conn = open_window(&h, "closeable", Size::new(120.0, 90.0));
    until(&mut h, "the window", |h| h.state().window_count() == 1);

    let win = h.state().windows()[0];
    let id = named(&mut h, &nitro_bar::entry_name(win)).expect("its button");
    let b = h.bounds(id);
    h.move_pointer(nitro_core::Point::new(b.x + b.w / 2.0, b.y + b.h / 2.0));
    h.press(button::MIDDLE);
    h.release(button::MIDDLE);

    let mut closed = false;
    for _ in 0..400 {
        h.settle();
        let mut batch = Vec::new();
        let _ = conn.poll(&mut batch);
        if batch.iter().any(|m| matches!(m, ServerMsg::Closed(_))) {
            closed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(closed, "the owning client was asked to close");

    drop(conn);
    h.quit();
}

#[test]
fn the_clock_updates_exactly_once_at_the_minute_boundary() {
    // The idle claim's other half: the clock costs one `SetText` a
    // minute, and the timer is aligned to the boundary rather than
    // polling every second to notice it.
    //
    // 1970-01-01 09:41:30 UTC, so the boundary is 30 s away. Pinned
    // before the tree is built, because the clock's first timer is armed
    // from it as the tree is built.
    let mut h = bar(Bar::new().with_fake_time_ms((9 * 3600 + 41 * 60 + 30) * 1000));
    h.settle();
    let before = h.state().ticks();
    let shown = h.state().clock_text().to_owned();
    assert_eq!(shown.len(), 5, "HH:MM, e.g. 09:41: {shown:?}");
    assert_eq!(
        label_text(&mut h, names::CLOCK),
        shown,
        "the label shows what the state says"
    );

    // Half a minute later, still the same minute: the clock's timer is
    // not even due, and running the loop must change nothing at all.
    h.state_mut()
        .set_fake_time_ms((9 * 3600 + 41 * 60 + 59) * 1000);
    h.tap();
    h.advance_timers(29_000);
    h.run_timers();
    h.settle();
    assert_eq!(h.state().ticks(), before, "no tick inside the minute");
    assert_eq!(
        h.state().clock_text(),
        shown,
        "and the string did not change"
    );

    // Over the boundary: exactly one tick, and exactly one `SetText`.
    h.state_mut().set_fake_time_ms((9 * 3600 + 42 * 60) * 1000);
    h.clear_tap();
    h.advance_timers(1_001);
    h.run_timers();
    h.settle();
    assert_eq!(h.state().ticks(), before + 1, "exactly one tick");
    assert_ne!(h.state().clock_text(), shown, "showing the new minute");
    let texts = h.mutations().iter().filter(|m| m.op == "SetText").count();
    assert_eq!(
        texts,
        1,
        "one SetText, not one per widget: {:?}",
        h.mutations()
    );

    h.quit();
}

#[test]
fn a_settled_bar_is_silent_while_nothing_changes() {
    // The contract: with nothing changing, zero wire traffic between
    // clock ticks — even though the sensors are polled throughout. The
    // poll happens, the strings do not change, and an unchanged `Label`
    // sends nothing.
    //
    // The fake clock is pinned inside a minute, so no tick is due; the
    // sensors are polled every 20 ms so that 300 ms of "idle" contains a
    // dozen polls; and the readings are **fixed**, because the claim
    // under test is "a poll that finds the same numbers costs nothing".
    // Polling the real `/proc` would be asserting that this machine's
    // load average held still for 300 ms, which is neither the claim nor
    // reliably true — it is what made an earlier version of this test
    // fail about one run in six.
    let readings = nitro_bar::Readings {
        battery: Some("87%".to_owned()),
        load: Some("0.42".to_owned()),
        mem: Some("1.2/3.3G".to_owned()),
    };
    let mut h = bar(Bar::new()
        .with_fake_time_ms((9 * 3600 + 41 * 60 + 5) * 1000)
        .with_poll_ms(20)
        .with_sensors(move || readings.clone()));
    h.settle();
    let polls = h.state().polls();
    // The readings really are on screen, so what follows is "unchanged",
    // not "never arrived".
    assert_eq!(label_text(&mut h, names::LOAD), "0.42");
    assert_eq!(label_text(&mut h, names::MEM), "1.2/3.3G");
    assert_eq!(label_text(&mut h, names::BATTERY), "87%");

    // `assert_idle` runs the timers on every turn, so this really is the
    // app loop's own idle.
    h.assert_idle(300);
    assert!(
        h.state().polls() > polls,
        "the sensors really were polled while idle: {} then {}",
        polls,
        h.state().polls()
    );

    h.quit();
}

#[test]
fn a_sensor_that_changes_costs_one_set_text() {
    // The other half of the idle claim: when a reading *does* change, it
    // costs exactly one `SetText` — the one label that moved — and not a
    // repaint of the bar.
    use std::cell::Cell;
    use std::rc::Rc;

    let load = Rc::new(Cell::new(0));
    let seen = Rc::clone(&load);
    let mut h = bar(Bar::new()
        .with_fake_time_ms((9 * 3600 + 41 * 60 + 5) * 1000)
        .with_poll_ms(20)
        .with_sensors(move || nitro_bar::Readings {
            battery: Some("87%".to_owned()),
            load: Some(format!("0.{:02}", seen.get())),
            mem: Some("1.2/3.3G".to_owned()),
        }));
    h.settle();
    assert_eq!(label_text(&mut h, names::LOAD), "0.00");

    load.set(42);
    h.tap();
    h.advance_timers(21);
    h.run_timers();
    h.settle();
    assert_eq!(label_text(&mut h, names::LOAD), "0.42");
    let texts = h.mutations().iter().filter(|m| m.op == "SetText").count();
    assert_eq!(
        texts,
        1,
        "one SetText for the one changed readout: {:?}",
        h.mutations()
    );

    h.quit();
}

#[test]
fn every_section_is_addressable_for_hey() {
    // `hey nitro-bar list` is the smoke test, and it can only find what
    // is named. A section that lost its name would still draw and would
    // be invisible to every script.
    let mut h = harness();
    h.settle();
    for name in [
        names::LAUNCHER,
        names::WINDOWS,
        names::CLOCK,
        names::BATTERY,
        names::LOAD,
        names::MEM,
    ] {
        assert!(named(&mut h, name).is_some(), "no widget named {name}");
    }
    h.quit();
}

#[test]
fn the_launcher_button_is_clickable_from_a_script() {
    // The launcher itself is #3691; what this pins down is that the
    // button is wired to the same callback a real click runs, through the
    // action a script uses.
    let mut h = harness();
    h.settle();
    let id = named(&mut h, names::LAUNCHER).expect("the launcher button");
    h.click(id);
    h.settle();
    assert_eq!(h.state().launcher_presses(), 1);
    h.quit();
}

#[test]
fn the_bar_paints_across_its_whole_output() {
    // The probe's second finding, as a regression: a bar that ignores the
    // `Configure` its own anchor produces paints its original width and
    // the desktop shows through the rest of the strip. The window was
    // asked for at 320 wide against a 320 output, so what this really
    // checks is that the anchor's configure was honoured at all.
    let mut h = harness();
    h.settle();
    assert!(
        (h.ui().window_size().w - 320.0).abs() < 1.0,
        "the anchor spanned the output: {:?}",
        h.ui().window_size()
    );
    assert!(
        (h.ui().window_size().h - BAR_H).abs() < 1.0,
        "and kept the bar's height: {:?}",
        h.ui().window_size()
    );
    h.quit();
}
