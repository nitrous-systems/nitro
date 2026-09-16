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
    open_layer_window(h, title, size, Layer::Normal)
}

/// As [`open_window`], on a chosen layer.
///
/// A non-`Normal` layer needs the **shell** socket: the layer is part of
/// `CreateWindow`, and asking for a shell one on the ordinary wire socket
/// is a fatal protocol error rather than a refusal.
fn open_layer_window(h: &Harness<Bar>, title: &str, size: Size, layer: Layer) -> Connection {
    let path = if layer == Layer::Normal {
        h.server().wire_path()
    } else {
        h.server().shell_path()
    };
    let mut conn = Connection::connect(path, title).expect("connect");
    conn.tx()
        .create_window(NodeId(1), title, size, layer)
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
fn a_shell_surface_is_not_a_window_in_the_task_list() {
    // The M3 defect: with the wallpaper and the launcher running, the
    // bar's window list showed `nitro-wallpaper` and `nitro-launcher` as
    // entries. They *are* windows — the server makes no distinction — but
    // a task list lists applications, and every one of these is as
    // unfocusable as the bar itself.
    //
    // Filtering on the bar's own app id was not enough: it only ever hid
    // this bar. The layer is what separates furniture from applications,
    // so the server carries it in `WindowInfo` and the bar filters on it.
    let mut h = harness();
    h.settle();

    // One of each shell layer, and one real application.
    let wallpaper = open_layer_window(
        &h,
        "nitro-wallpaper",
        Size::new(320.0, 240.0),
        Layer::Background,
    );
    let launcher = open_layer_window(&h, "nitro-launcher", Size::new(120.0, 90.0), Layer::Overlay);
    let dock = open_layer_window(&h, "some-dock", Size::new(120.0, 24.0), Layer::Top);
    let app = open_window(&h, "an-application", Size::new(120.0, 90.0));

    until(&mut h, "the application", |h| h.state().window_count() == 1);
    // And it stays at one: settle well past the point where the three
    // shell surfaces' `WindowInfo`s have all arrived, or this would pass
    // by racing them.
    for _ in 0..20 {
        h.settle();
    }
    assert_eq!(
        h.state().window_labels(),
        vec!["an-application".to_owned()],
        "only the Normal-layer window is listed"
    );
    // Not just absent from the model — no button either.
    for name in ["nitro-wallpaper", "nitro-launcher", "some-dock"] {
        assert!(
            !h.state().window_labels().iter().any(|l| l == name),
            "{name} got a button"
        );
    }

    drop((wallpaper, launcher, dock, app));
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

    // And the button itself is **not** left focused. The bar's window is
    // `NO_FOCUS`, so a click acts without taking toolkit focus: a focus
    // ring on a surface the server will never give keys to is a lie, and
    // this is exactly what `hey nitro-bar list` reported as
    // `focused,hovered` before the toolkit gate. The path asserted here
    // is the one a script reads.
    let path = format!("window/{}/{}", names::WINDOWS, nitro_bar::entry_name(first));
    assert_eq!(
        nitro_ui::introspect::get_prop(h.ui(), &path, "focused").as_deref(),
        Ok("false"),
        "a clicked window-list button must not show a focus ring"
    );
    assert_eq!(h.ui().focused(), None, "nothing in the bar took focus");

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
    //
    // The sensors are pinned to a constant reading for the reason
    // `SensorSource` gives: this test advances the timers a cumulative
    // 30 001 ms, and since M4-B2 took `POLL_MS` from 5 s to 30 s that
    // crosses a sensor poll, which lands in the *same* `run_timers` batch
    // as the clock tick. With the real `/proc` behind it the load average
    // moves on a busy machine, the load label repaints, and the "exactly
    // one SetText" assertion below counts two — a failure that says only
    // that the test host was loaded. Reproduced on a 128-core box at
    // roughly 1 run in 12 with eight spinners running, on `main` as well
    // as here, which is what identified it as this test's bug rather than
    // a regression in whatever branch happened to hit it.
    let steady = nitro_bar::Readings {
        battery: Some("87%".to_owned()),
        load: Some("0.4".to_owned()),
        mem: Some("1.2/3.3G".to_owned()),
    };
    let mut h = bar(Bar::new()
        .with_fake_time_ms((9 * 3600 + 41 * 60 + 30) * 1000)
        .with_sensors(move || steady.clone()));
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
    // poll happens, the readings match the last ones, and the tree is
    // never touched.
    //
    // The fake clock is pinned inside a minute, so no tick is due; the
    // sensors are polled every 20 ms so that 300 ms of "idle" contains a
    // dozen polls (the real interval is 30 s — see the README's sensor
    // rule — and a test cannot spend half a minute per poll); and the
    // readings are **fixed**, because the claim under test is "a poll
    // that finds the same numbers costs nothing". Polling the real
    // `/proc` would be asserting that this machine's load average held
    // still for 300 ms, which is neither the claim nor reliably true — it
    // is what made an earlier version of this test fail about one run in
    // six.
    //
    // That is only half the rule. The other half — no sensor *renders*
    // more often than every 30 s, so what a real `/proc` does between
    // polls cannot reach the screen more than twice a minute — is
    // `POLL_MS` itself, and
    // `the_sensors_render_no_more_often_than_every_thirty_seconds`
    // asserts it.
    let readings = nitro_bar::Readings {
        battery: Some("87%".to_owned()),
        load: Some("0.4".to_owned()),
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
    assert_eq!(label_text(&mut h, names::LOAD), "0.4");
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
        names::LOAD_ICON,
        names::MEM,
        names::MEM_ICON,
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

#[test]
fn the_sensors_render_no_more_often_than_every_thirty_seconds() {
    // The second half of the bar's sensor rule, and the half the idle
    // test above cannot see: it shortens the interval to 20 ms so that
    // 300 ms contains a dozen polls, which means nothing in it would
    // notice the shipped interval going back to 5 s.
    //
    // 30 s, not 5, because the load average moves on nearly every read —
    // an idle box measured six painted frames per ten seconds, all of
    // them the load label flipping between values the user did not ask
    // for. With a minute-aligned clock, a 30 s cap is the coarsest budget
    // that still lets a reading appear within a clock tick of its change.
    assert_eq!(
        nitro_bar::POLL_MS,
        30_000,
        "no sensor may render more often than every 30 s"
    );

    // And the shipped interval is the one a bar that was not configured
    // by a test actually arms, rather than a constant nothing reads.
    let mut h = harness();
    h.settle();
    let polls = h.state().polls();
    assert_eq!(
        polls, 1,
        "the first poll is immediate, so the bar is never blank"
    );
    // Well short of 30 s: no second poll is due.
    h.advance_timers(5_000);
    h.run_timers();
    h.settle();
    assert_eq!(
        h.state().polls(),
        polls,
        "a 5 s interval would have polled again here"
    );
    // Past it: exactly one more.
    h.advance_timers(25_001);
    h.run_timers();
    h.settle();
    assert_eq!(h.state().polls(), polls + 1, "and one poll at 30 s");
    h.quit();
}

#[test]
fn a_poll_that_finds_the_same_readings_does_not_touch_the_tree() {
    // The bar's own half of "an unchanged poll costs nothing". The idle
    // test asserts it from the outside, by counting commits; this asserts
    // the mechanism, because the outside test would still pass if the bar
    // pushed every reading into every label and leaned entirely on
    // `Label::set_text`'s early return — a setter in another crate, which
    // is not where the bar's contract should live.
    let readings = nitro_bar::Readings {
        battery: Some("87%".to_owned()),
        load: Some("0.4".to_owned()),
        mem: Some("1.2/3.3G".to_owned()),
    };
    let mut h = bar(Bar::new()
        .with_fake_time_ms((9 * 3600 + 41 * 60 + 5) * 1000)
        .with_poll_ms(20)
        .with_sensors(move || readings.clone()));
    h.settle();
    assert_eq!(label_text(&mut h, names::LOAD), "0.4");

    let polls = h.state().polls();
    h.tap();
    h.clear_tap();
    h.advance_timers(21);
    h.run_timers();
    h.settle();
    assert!(h.state().polls() > polls, "the sensors were polled");
    assert_eq!(
        h.mutations().len(),
        0,
        "an unchanged poll produced no mutation at all: {:?}",
        h.mutations()
    );
    h.quit();
}

#[test]
fn the_icons_are_painted_once_and_never_again() {
    // The icon half of the idle contract, and the reason icons are sent
    // **by name**: an icon is static, so the two things that could
    // change it — the desktop's scheme and the output's scale — are the
    // *server's* to act on. The bar therefore sends one `SetIcon` per
    // icon on the first paint and none afterwards, however many sensor
    // polls go past.
    //
    // 120 s of ticks at the real 30 s interval is four polls; the poll
    // interval is compressed to 20 ms here for the same reason
    // `a_settled_bar_sends_nothing_while_its_sensors_tick` compresses
    // it, and the readings are fixed so what is being measured is
    // "nothing changed" rather than "this machine held still".
    let readings = nitro_bar::Readings {
        battery: Some("87%".to_owned()),
        load: Some("0.4".to_owned()),
        mem: Some("1.2/3.3G".to_owned()),
    };
    let mut h = bar(Bar::new()
        .with_fake_time_ms((9 * 3600 + 41 * 60 + 5) * 1000)
        .with_poll_ms(20)
        .with_sensors(move || readings.clone()));
    h.settle();

    // The icons really are in the tree, so what follows is "no second
    // SetIcon" rather than "no SetIcon at all".
    for name in [names::LOAD_ICON, names::MEM_ICON] {
        let id = named(&mut h, name).unwrap_or_else(|| panic!("no widget named {name}"));
        assert!((h.widget::<nitro_ui::widgets::Icon>(id).size() - 16.0).abs() < 0.01);
    }

    h.tap();
    // Four polls' worth of 30 s ticks, compressed.
    for _ in 0..4 {
        h.advance_timers(31);
        h.run_timers();
        h.settle();
    }
    let icons = h.mutations().iter().filter(|m| m.op == "SetIcon").count();
    assert_eq!(
        icons,
        0,
        "120 s of sensor ticks emitted {icons} SetIcon(s): {:?}",
        h.mutations()
    );

    h.quit();
}

#[test]
fn a_window_list_button_asks_for_its_app_id_as_a_coloured_icon() {
    // Rule (a) of `docs/shell.md`, asserted where it is implemented: the
    // bar sends the window's **app id** as the icon name, coloured, with
    // the symbolic `window` as the fallback. It parses no `.desktop`
    // file and reads nothing off disk, which is the whole reason the
    // rule is worth having — and the whole reason it is limited.
    let mut h = harness();
    h.settle();
    let conn = open_window(&h, "alpha", Size::new(120.0, 90.0));
    until(&mut h, "the window to be listed", |h| {
        h.state().window_count() == 1
    });
    h.settle();
    let win = h.state().windows().first().copied().expect("a window");
    let id = named(&mut h, &nitro_bar::entry_name(win)).expect("its button");
    let button = h.widget::<nitro_ui::widgets::Button<Bar>>(id);

    // **Two outcomes are correct here, and which one happens is a fact
    // about the machine running the test rather than about the bar.**
    // The icon the button asks for is the app id, `alpha`, and no icon
    // theme on earth has an application called that — so on any real box
    // the server answers `BadIcon` and the button has already spent its
    // one fallback by the time this runs. A test that demanded `alpha`
    // would therefore pass only on a machine with a hand-made theme, and
    // a test that demanded `window` would pass for the wrong reason on a
    // machine that happened to have one. Asserting the pair is the
    // honest shape: it is the app id and the fallback is still armed, or
    // it is the fallback *and the button says it fell back*.
    if button.icon_fell_back() {
        assert_eq!(button.icon(), Some(nitro_bar::icons::WINDOW));
        assert_eq!(
            button.icon_fallback(),
            None,
            "a spent fallback is taken, which is what makes it exactly once"
        );
    } else {
        assert_eq!(button.icon(), Some("alpha"));
        assert!(
            button.is_icon_coloured(),
            "an app id names the machine's icon theme, not the symbolic set"
        );
        assert_eq!(button.icon_fallback(), Some(nitro_bar::icons::WINDOW));
    }
    assert!(
        button
            .icon_size()
            .is_some_and(|px| (px - 16.0).abs() < 0.01),
        "16 px, like the bar's other icons: got {:?}",
        button.icon_size()
    );
    // The label is untouched — the icon is in front of the title, not
    // instead of it, so `hey` and a screen reader still see the words.
    assert!(button.text().contains("alpha"));

    // And the name that went out really was the app id rather than the
    // title, which the live widget can no longer tell us once it has
    // fallen back. `entry_icon` is the function that decides, so it is
    // the one to ask — with a `WindowInfo` whose title and app id
    // differ, which `open_window` deliberately does not produce.
    assert_eq!(
        nitro_bar::entry_icon(&nitro_ui::shell::WindowInfo {
            window: nitro_ui::shell::WindowRef(7),
            state: nitro_ui::shell::WindowState::Normal,
            focused: false,
            output: 0,
            layer: Layer::Normal,
            app_id: "firefox".to_owned(),
            title: "Inbox — Mail".to_owned(),
        }),
        "firefox"
    );

    drop(conn);
    h.quit();
}

#[test]
fn the_window_list_icons_are_painted_once_and_never_again() {
    // The idle contract extended to the window list, which is the part
    // that could plausibly break it: unlike the bar's static icons these
    // are created at run time and rewritten whenever a `WindowInfo`
    // arrives — and a focus change sends one for *both* windows
    // involved. An app id is fixed for a window's life, so an unchanged
    // list must cost zero `SetIcon`s however many polls and focus
    // changes go past.
    let readings = nitro_bar::Readings {
        battery: Some("87%".to_owned()),
        load: Some("0.4".to_owned()),
        mem: Some("1.2/3.3G".to_owned()),
    };
    let mut h = bar(Bar::new()
        .with_poll_ms(20)
        .with_sensors(move || readings.clone()));
    h.settle();
    let mut a = open_window(&h, "alpha", Size::new(120.0, 90.0));
    let b = open_window(&h, "beta", Size::new(120.0, 90.0));
    until(&mut h, "both windows to be listed", |h| {
        h.state().window_count() == 2
    });
    h.settle();

    // Armed *after* the list settled, so what follows is "no second
    // SetIcon" rather than "no SetIcon at all" — the icons are asserted
    // to be on the buttons by
    // `a_window_list_button_asks_for_its_app_id_as_a_coloured_icon`.
    h.tap();
    // Four polls' worth of 30 s ticks, compressed as the other idle
    // tests compress them.
    for _ in 0..4 {
        h.advance_timers(31);
        h.run_timers();
        h.settle();
    }
    // And a retitle, which is the `WindowInfo` a real desktop actually
    // produces while a list sits still: the label moves, the app id does
    // not, so the icon must not be re-sent.
    a.tx()
        .set_window_title(NodeId(1), "alpha renamed")
        .commit(3)
        .expect("retitle");
    while !a.flush().expect("flush") {}
    until(&mut h, "the retitle", |h| {
        h.state()
            .window_labels()
            .iter()
            .any(|l| l.contains("renamed"))
    });
    h.settle();

    let icons = h.mutations().iter().filter(|m| m.op == "SetIcon").count();
    assert_eq!(
        icons,
        0,
        "a settled window list emitted {icons} SetIcon(s): {:?}",
        h.mutations()
    );

    drop(a);
    drop(b);
    h.quit();
}

#[test]
fn a_window_with_no_app_id_still_gets_an_icon() {
    // An empty name **clears** an icon node on the wire, so passing an
    // absent app id straight through would leave a gap where every other
    // button has a picture. The generic symbolic icon is the honest
    // answer, and it is the same one the fallback uses.
    use nitro_ui::shell::{WindowInfo, WindowRef, WindowState};
    let info = |app_id: &str| WindowInfo {
        window: WindowRef(7),
        state: WindowState::Normal,
        focused: false,
        output: 0,
        layer: Layer::Normal,
        app_id: app_id.to_owned(),
        title: "Some Title".to_owned(),
    };
    // The title is *not* consulted: an icon name is an app id or it is
    // nothing, because a title is prose and prose is not an icon name.
    assert_eq!(nitro_bar::entry_icon(&info("")), nitro_bar::icons::WINDOW);
    assert_eq!(
        nitro_bar::entry_icon(&info("   ")),
        nitro_bar::icons::WINDOW
    );
}

#[test]
fn the_launcher_button_draws_an_icon_and_keeps_its_accessible_name() {
    // The bargain the `≡` character was traded for: the glyph is the
    // desktop's own artwork (so it is the right weight, at the output's
    // scale, in the palette's colour), and the *word* is still what a
    // script and a screen reader see. A button that lost its name would
    // still look right and would be unaddressable.
    let mut h = harness();
    h.settle();
    let id = named(&mut h, names::LAUNCHER).expect("the launcher button");
    let button = h.widget::<nitro_ui::widgets::Button<Bar>>(id);
    assert_eq!(button.icon(), Some(nitro_bar::icons::LAUNCHER));
    assert_eq!(button.text(), "Menu");
    // And it still fires the same callback a real click runs.
    h.click(id);
    h.settle();
    assert_eq!(h.state().launcher_presses(), 1);
    h.quit();
}

/// #3724's second report: "if a window is minimized, clicking it in the
/// bar should re-open it".
///
/// The whole toggle, through the real click path, on the server's own
/// counters: **focused → minimized → restored**, with `stats minimized`
/// going 0 → 1 → 0 and `focused` following it down and back up.
///
/// Both halves were broken, in different places. Clicking the *focused*
/// entry did nothing at all, because focusing an already-focused window
/// is a no-op — so the bar's row for the window you were looking at was
/// a button with no effect. And clicking a *minimized* entry did nothing
/// either, silently: `FocusWindow` went out, and the server's
/// `focusable()` excludes `Minimized`, so it was refused on the same
/// terms a `NO_FOCUS` overlay is. The refusal is still there for
/// `NO_FOCUS`; a minimized window is restored first.
#[test]
fn clicking_the_focused_entry_minimizes_it_and_clicking_it_again_restores_it() {
    let mut h = harness();
    h.settle();
    let conn = open_window(&h, "toggle", Size::new(120.0, 90.0));
    until(&mut h, "the window", |h| h.state().window_count() == 1);

    // A new window on the Normal layer takes focus when it is placed, so
    // the entry starts focused and on screen.
    let win = h.state().windows()[0];
    until(&mut h, "the focus", |h| {
        h.state().focused_window() == Some(win)
    });
    assert_eq!(h.server().stat("minimized"), 0, "nothing is put away yet");
    assert_eq!(h.server().stat("focused"), 1);

    let id = named(&mut h, &nitro_bar::entry_name(win)).expect("its button");

    // One: the focused, visible entry. A click puts the window away.
    h.click(id);
    until(&mut h, "the minimize", |h| {
        h.server().stat("minimized") == 1
    });
    until(&mut h, "the bar to hear about it", |h| {
        h.state().minimized_windows() == vec![win]
    });
    assert_eq!(
        h.server().stat("focused"),
        0,
        "the only window was put away, so nothing holds focus"
    );
    assert_eq!(
        h.state().focused_window(),
        None,
        "the bar's own view followed"
    );
    // And the row says so, in a form a script can read: `hey` prints a
    // button's label as its value, and this is what it prints.
    assert_eq!(
        h.widget::<nitro_ui::widgets::Button<Bar>>(id).text(),
        "[toggle]",
        "a minimized entry is marked, not merely dimmed"
    );
    // Dimmed as well as marked: colour alone is an affordance a
    // colour-blind user does not get, and a marker alone is easy to miss
    // in a row of eight, so the entry carries both.
    assert_eq!(
        h.widget::<nitro_ui::widgets::Button<Bar>>(id).text_role(),
        Some(nitro_ui::ColorRole::TextDim),
        "a minimized entry is not dimmed"
    );
    // But it is still **enabled**: a disabled button ignores clicks, and
    // the next thing this test does is click it.
    assert!(
        h.widget::<nitro_ui::widgets::Button<Bar>>(id).is_enabled(),
        "a minimized entry must stay clickable"
    );

    // Two: the minimized entry. A click brings it back and focuses it.
    h.click(id);
    until(&mut h, "the restore", |h| h.server().stat("minimized") == 0);
    until(&mut h, "the focus to come back", |h| {
        h.state().focused_window() == Some(win)
    });
    assert_eq!(h.server().stat("focused"), 1);
    assert!(
        h.state().minimized_windows().is_empty(),
        "the bar still thinks the window is put away"
    );
    assert_eq!(
        h.widget::<nitro_ui::widgets::Button<Bar>>(id).text(),
        "▸ toggle",
        "the restored entry is marked focused again"
    );
    assert_eq!(
        h.widget::<nitro_ui::widgets::Button<Bar>>(id).text_role(),
        Some(nitro_ui::ColorRole::ButtonText),
        "the restored entry is still dimmed"
    );

    drop(conn);
    h.quit();
}

/// An **unfocused, visible** entry is not a toggle: it focuses.
///
/// The other arm of the click rule, and the one that must not regress —
/// a task list whose rows minimized whatever you clicked would be
/// unusable. `clicking_a_window_list_button_focuses_that_window` covers
/// the focus half; this one is specifically that the window is *not* put
/// away on the way.
#[test]
fn clicking_an_unfocused_entry_focuses_it_rather_than_minimizing_it() {
    let mut h = harness();
    h.settle();
    let a = open_window(&h, "alpha", Size::new(120.0, 90.0));
    until(&mut h, "alpha", |h| h.state().window_count() == 1);
    let b = open_window(&h, "beta", Size::new(120.0, 90.0));
    until(&mut h, "beta", |h| h.state().window_count() == 2);

    // The newest took focus, so the first entry is unfocused and visible.
    let first = h.state().windows()[0];
    until(&mut h, "beta's focus", |h| {
        h.state().focused_window() != Some(first)
    });
    let id = named(&mut h, &nitro_bar::entry_name(first)).expect("alpha's button");
    h.click(id);
    until(&mut h, "the focus to move", |h| {
        h.state().focused_window() == Some(first)
    });
    assert_eq!(
        h.server().stat("minimized"),
        0,
        "clicking an unfocused row put a window away"
    );

    drop((a, b));
    h.quit();
}

/// A minimized entry dims its **label**, and does not touch its icon.
///
/// The review of #3724 caught this as a real defect, and it is worth a
/// test of its own because the failure is silent and permanent.
///
/// The button's primary icon is the window's **app id**, resolved in the
/// machine's icon theme, and the role byte a `SetIcon` carries is the
/// server's *selector* rather than a hint: a non-`AS_COLOURED` role means
/// the compiled-in symbolic set and nothing else. So dimming that icon
/// does not dim it — it asks for `firefox` in a set that has no
/// `firefox`, earns `BadIcon`, spends the toolkit's one fallback swapping
/// the name for `window`, and on restore asks `lookup_app("window")`,
/// which also fails, with the fallback now latched. The entry's icon is
/// gone for good, and `upsert` cannot repair it: `set_icon_coloured` only
/// fires `if icon_changed`, and the app id never changed.
///
/// **The assertion is that the toggle changes nothing about the icon**,
/// which is the machine-independent form. Naming a tint would not be: on
/// a box with a matching theme the icon is the app id and `Coloured`, on
/// a box without one it has already fallen back to the symbolic `window`
/// — and `a_window_list_button_asks_for_its_app_id_as_a_coloured_icon`
/// makes that argument at length. Either is fine; what must not happen is
/// that *minimizing* moves it.
#[test]
fn a_minimized_entry_dims_its_label_and_leaves_its_icon_alone() {
    let mut h = harness();
    h.settle();
    let conn = open_window(&h, "dimmable", Size::new(120.0, 90.0));
    until(&mut h, "the window", |h| h.state().window_count() == 1);
    h.settle();
    let win = h.state().windows()[0];
    until(&mut h, "the focus", |h| {
        h.state().focused_window() == Some(win)
    });
    let id = named(&mut h, &nitro_bar::entry_name(win)).expect("its button");

    let before = {
        let b = h.widget::<nitro_ui::widgets::Button<Bar>>(id);
        (
            b.icon().map(str::to_owned),
            b.icon_tint(),
            b.icon_fell_back(),
            b.text_role(),
        )
    };
    assert_eq!(
        before.3,
        Some(nitro_ui::ColorRole::ButtonText),
        "a visible entry's label is ordinary button text"
    );

    // Minimize it through the bar's own click, which is the path that
    // had the bug.
    h.click(id);
    until(&mut h, "the minimize", |h| {
        h.server().stat("minimized") == 1
    });
    until(&mut h, "the bar to hear", |h| {
        h.state().minimized_windows() == vec![win]
    });
    h.settle();

    {
        let b = h.widget::<nitro_ui::widgets::Button<Bar>>(id);
        assert_eq!(
            b.text_role(),
            Some(nitro_ui::ColorRole::TextDim),
            "a minimized entry's label is dimmed"
        );
        assert_eq!(
            (
                b.icon().map(str::to_owned),
                b.icon_tint(),
                b.icon_fell_back()
            ),
            (before.0.clone(), before.1, before.2),
            "minimizing moved the icon: name, tint or fallback state changed"
        );
    }

    // And back, which is where the damage used to become permanent.
    h.click(id);
    until(&mut h, "the restore", |h| h.server().stat("minimized") == 0);
    until(&mut h, "the focus back", |h| {
        h.state().focused_window() == Some(win)
    });
    h.settle();

    let b = h.widget::<nitro_ui::widgets::Button<Bar>>(id);
    assert_eq!(
        b.text_role(),
        Some(nitro_ui::ColorRole::ButtonText),
        "a restored entry's label is ordinary again"
    );
    assert_eq!(
        (
            b.icon().map(str::to_owned),
            b.icon_tint(),
            b.icon_fell_back()
        ),
        (before.0, before.1, before.2),
        "the icon did not survive a minimize/restore round trip"
    );
    // Whatever the machine resolved, the icon is still *an* icon: the
    // failure this test exists for ends with the node cleared and nothing
    // left to name.
    assert!(b.icon().is_some(), "the entry lost its icon entirely");

    drop(conn);
    h.quit();
}
