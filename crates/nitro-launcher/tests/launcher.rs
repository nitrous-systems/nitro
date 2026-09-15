//! The launcher, driven through a real server on the shell socket.
//!
//! Every test here builds the tree the binary builds
//! ([`nitro_launcher::build`]) and drives it the way the desktop does: a
//! real `Overlay` window on `shell.sock`, real key events travelling
//! evdev → server → grab → widget, and a real `fork`/`exec` at the end of
//! the launch path. Nothing pokes the state directly to set up a case the
//! wire would not produce.
//!
//! The two things that *are* faked are the `.desktop` search path
//! (`Launcher::with_dirs`, pointed at a fixture directory) and the
//! built-in entries (`Launcher::with_builtins`) — because the alternative
//! is a test that asserts on whatever applications the machine running it
//! happens to have installed, which is neither the claim nor reliably
//! true.

use std::path::PathBuf;

use nitro_kms::Image;
use nitro_launcher::desktop::{Entry, Source};
use nitro_launcher::spawn::Children;
use nitro_launcher::{Launcher, build, names};
use nitro_ui::event::key;
use nitro_ui::shell::Surface;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{Button, Label, TextField, column};
use nitro_ui::{Size, Ui, WidgetId};
use nitro_wire::client::Connection;
use nitro_wire::types::{Layer, NodeId};

/// Evdev keycode of the left Super key: the launcher's trigger.
const KEY_LEFTMETA: u32 = 125;

/// A launcher whose search path is `dirs` and whose built-ins are
/// `builtins`.
fn launcher(dirs: Vec<PathBuf>, builtins: Vec<Entry>) -> Harness<Launcher> {
    Harness::shell(
        "nitro-launcher",
        Launcher::new().with_dirs(dirs).with_builtins(builtins),
        Surface::overlay(),
        // Smaller than the real 600×400: the harness runs a 320×240
        // output, and a window bigger than the output is cropped by
        // `shot` and clicked at coordinates the server clamps. The tree
        // is the binary's either way — this is the awkward case, not a
        // soft one.
        Some(Size::new(300.0, 220.0)),
        build,
    )
}

/// A launcher over a fresh fixture directory of `.desktop` files.
fn with_files(name: &str, files: &[(&str, &str)]) -> (Harness<Launcher>, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "nitro-launcher-test-{}-{name}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    for (file, text) in files {
        std::fs::write(dir.join(file), text).expect("fixture file");
    }
    let mut h = launcher(vec![dir.clone()], Vec::new());
    h.settle();
    (h, dir)
}

/// A `.desktop` file's text.
fn entry_text(name: &str, exec: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n")
}

/// The three applications most tests use.
fn three() -> Vec<(&'static str, String)> {
    vec![
        ("calc.desktop", entry_text("Calculator", "/bin/true calc")),
        ("mail.desktop", entry_text("Mail", "/bin/true mail")),
        ("term.desktop", entry_text("Terminal", "/bin/true term")),
    ]
}

/// A harness over [`three`].
fn harness() -> (Harness<Launcher>, PathBuf) {
    let owned = three();
    let files: Vec<(&str, &str)> = owned.iter().map(|(f, t)| (*f, t.as_str())).collect();
    with_files("three", &files)
}

/// The widget named `path`, found the way `hey` finds it.
fn named(h: &mut Harness<Launcher>, path: &str) -> Option<WidgetId> {
    nitro_ui::introspect::resolve(h.ui(), &format!("window/{path}"))
}

/// The label of result row `n`, as `hey get results/<n> value` would show
/// it.
fn row(h: &mut Harness<Launcher>, n: usize) -> String {
    let id = named(h, &format!("{}/{n}", names::RESULTS)).unwrap_or_else(|| panic!("no row {n}"));
    h.widget::<Button<Launcher>>(id).text().to_owned()
}

/// Open the launcher with a real bare-Super tap: press and release with
/// nothing in between, which is what the server's tap state machine is
/// looking for.
fn super_tap(h: &mut Harness<Launcher>) {
    h.key_down(KEY_LEFTMETA);
    h.key_up(KEY_LEFTMETA);
    h.settle();
}

/// Open an ordinary client window on the harness's **wire** socket.
///
/// A real second client, so the focus it takes is the focus a desktop
/// would give it — which is the only way to tell "the launcher got the
/// key through its grab" from "the launcher happened to be focused".
fn open_window(h: &Harness<Launcher>, title: &str, size: Size) -> Connection {
    let mut conn = Connection::connect(h.server().wire_path(), title).expect("wire connect");
    conn.tx()
        .create_window(NodeId(1), title, size, Layer::Normal)
        .create_rect(
            NodeId(2),
            NodeId(1),
            nitro_core::Rect::new(0.0, 0.0, size.w, size.h),
        )
        .fill_solid(NodeId(2), nitro_core::Color::rgb(0x40, 0x80, 0xC0))
        .set_app_id(NodeId(1), title)
        .commit(1)
        .expect("commit");
    while !conn.flush().expect("flush") {}
    conn
}

/// Pump until `f` holds, so the server's notifications have time to
/// arrive.
fn until(h: &mut Harness<Launcher>, what: &str, f: impl Fn(&Harness<Launcher>) -> bool) {
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
fn a_bare_super_tap_shows_the_launcher_and_a_second_hides_it() {
    // The headline. The trigger is the *server's* — a `BindKey` with
    // `keysym: 0` — so what this really asserts is that the binding
    // landed, that the tap fired once on the release, and that the same
    // binding closes it while the launcher holds the keyboard.
    let (mut h, dir) = harness();
    assert!(!h.state().is_visible(), "hidden until something asks");
    assert_eq!(h.server().stat("hotkeys"), 2, "the tap and the chord");

    super_tap(&mut h);
    until(&mut h, "the launcher to show", |h| h.state().is_visible());
    assert_eq!(h.state().shows(), 1);
    assert_eq!(h.server().stat("grabbed"), 1, "and it took the keyboard");

    super_tap(&mut h);
    until(&mut h, "the launcher to hide", |h| !h.state().is_visible());

    // The grab goes with the window — the server drops a grab whose
    // window stops **showing**, so hiding is a complete release and the
    // launcher sends no second message. It is dropped **lazily**, on the
    // next key rather than on the commit that hid the window
    // (`docs/shell.md` §Keyboard grabs), so the statistic is read after
    // one: a test that waited for it to fall on its own would wait for
    // ever, which is what the first version of this did.
    h.key(46); // c
    h.settle();
    assert_eq!(
        h.server().stat("grabbed"),
        0,
        "hiding released the grab, with no message from the launcher"
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn the_grab_is_what_delivers_keys_and_hiding_gives_it_back() {
    // The launcher is `NO_FOCUS`, so keys reach it *only* through the
    // grab. That cannot be seen with the launcher alone — the harness
    // focuses its single window so ordinary key tests work at all — so
    // this opens a second, ordinary client to hold the focus. Then the
    // question is sharp: with somebody else focused, does the launcher
    // still get the keys while it is shown, and stop when it is hidden?
    let (mut h, dir) = harness();
    let conn = open_window(&h, "victim", Size::new(120.0, 90.0));
    // A newly placed window takes focus, so the launcher is definitely
    // not the focused window from here on.
    until(&mut h, "the other window", |h| {
        h.server().stat("windows") >= 2
    });
    h.settle();

    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.server().stat("grabbed"), 1);
    h.key(46); // c
    h.settle();
    assert_eq!(
        h.state().query(),
        "c",
        "the grab delivered the key past the focused window"
    );

    // And the focused window never saw it: a global overlay that also
    // leaked keystrokes into the window behind it would be a keylogger.
    h.key(key::ESC);
    until(&mut h, "the hide", |h| !h.state().is_visible());
    h.key(48); // b
    h.settle();
    assert_eq!(h.state().query(), "c", "a hidden launcher receives nothing");
    assert_eq!(h.server().stat("grabbed"), 0, "and the grab went with it");

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn another_window_taking_focus_hides_the_launcher() {
    // The spec's "focus-loss hides it", which has to be written
    // backwards: a `NO_FOCUS` overlay cannot *lose* focus because it
    // never had any, so the observable event is somebody else gaining
    // it. Without this the launcher sits on screen holding the keyboard
    // grab until Escape or a second tap.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.server().stat("grabbed"), 1);
    assert_eq!(h.state().focus_hides(), 0);

    // A real second client opening a real window, which takes focus when
    // the server places it — exactly what happens when a user starts
    // something, or clicks another window.
    let conn = open_window(&h, "interloper", Size::new(120.0, 90.0));
    until(&mut h, "the launcher to get out of the way", |h| {
        !h.state().is_visible()
    });
    assert_eq!(h.state().focus_hides(), 1, "hidden by the focus change");

    // And the grab went with it, so the new window's own keys reach the
    // new window: the launcher must not keep swallowing the keyboard.
    h.key(46); // c
    h.settle();
    assert_eq!(h.state().query(), "", "a hidden launcher receives nothing");
    assert_eq!(h.server().stat("grabbed"), 0, "and the grab was released");

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn the_focused_window_retitling_itself_is_not_a_focus_change() {
    // The server sends a `WindowInfo` whenever *anything* about a window
    // changes, and `focused` in it reports the current truth rather than
    // a transition. `clients.rs` pushes to `relisted` on `SetWindowTitle`
    // and `SetAppId`, so the already-focused window merely changing its
    // title produces `WindowInfo { focused: true }`.
    //
    // Treating that as "somebody took focus" makes the launcher vanish
    // while the user is typing into it, for no visible reason — and it is
    // not exotic: a shell sets its terminal's title on every prompt, a
    // browser on every page load, a clock-in-title app on a timer.
    let (mut h, dir) = harness();
    let mut conn = open_window(&h, "victim", Size::new(120.0, 90.0));
    until(&mut h, "the other window", |h| {
        h.server().stat("windows") >= 2
    });
    h.settle();

    // The launcher opens over it. The other window keeps the focus: a
    // hotkey does not move it, which is the whole point of `NO_FOCUS`.
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    h.key(46); // c
    h.settle();
    assert_eq!(h.state().query(), "c", "typing into the launcher");

    // Now the focused window retitles itself, repeatedly, the way a
    // terminal does on every prompt.
    for (serial, title) in [(10u32, "victim: ls"), (11, "victim: vim"), (12, "victim")] {
        conn.tx()
            .set_window_title(NodeId(1), title)
            .commit(serial)
            .expect("retitle");
        while !conn.flush().expect("flush") {}
        h.settle();
    }
    // Give the notifications every chance to arrive and be mishandled.
    for _ in 0..20 {
        h.settle();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert_eq!(
        h.state().focus_hides(),
        0,
        "a retitle is not a focus change"
    );
    assert!(
        h.state().is_visible(),
        "the launcher stayed up while the focused window retitled itself"
    );
    assert_eq!(h.state().query(), "c", "and kept what was typed");

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn focus_is_tracked_while_hidden_so_the_next_show_is_not_stale() {
    // The ordering inside `focus_moved`: the bookkeeping runs *before*
    // the "are we visible?" return. If it did not, focus changes that
    // happened while the launcher was hidden would never be recorded,
    // and the first change after the next show would be compared against
    // a stale window id — so the launcher would either hide immediately
    // (stale id differs) or, worse, fail to hide for the one window that
    // happened to match.
    let (mut h, dir) = harness();
    assert!(!h.state().is_visible());
    assert_eq!(h.state().focused_window(), None, "nothing focused yet");

    // Two windows open and take focus in turn, all while hidden.
    let a = open_window(&h, "first", Size::new(120.0, 90.0));
    until(&mut h, "the first window", |h| {
        h.state().focused_window().is_some()
    });
    let first = h.state().focused_window();
    let b = open_window(&h, "second", Size::new(120.0, 90.0));
    until(&mut h, "the second window to take focus", |h| {
        h.state().focused_window() != first
    });
    let second = h.state().focused_window();
    assert_ne!(
        second, first,
        "the launcher tracked the change while hidden"
    );
    assert_eq!(h.state().focus_hides(), 0, "and hid nothing doing it");

    // Now show it: the currently focused window is already known, so the
    // launcher stays up rather than hiding on a restatement of it.
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    for _ in 0..10 {
        h.settle();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        h.state().is_visible(),
        "it stayed up over the settled focus"
    );
    assert_eq!(h.state().focus_hides(), 0);

    drop((a, b));
    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn an_ordinary_focus_change_costs_a_hidden_launcher_nothing() {
    // The other half of the rule, and the one that would make a desktop
    // unusable if it were wrong: while the launcher is hidden, every
    // focus change on the machine arrives as a `WindowInfo` and must do
    // nothing at all — no mutation, no commit, no wakeup beyond the
    // message itself.
    let (mut h, dir) = harness();
    assert!(!h.state().is_visible());
    let a = open_window(&h, "first", Size::new(120.0, 90.0));
    until(&mut h, "the first window", |h| {
        h.server().stat("windows") >= 2
    });
    h.settle();

    h.tap();
    h.clear_tap();
    let commits = h.commits();
    // A second window opening takes focus from the first: two focus
    // changes, both reported, neither of interest.
    let b = open_window(&h, "second", Size::new(120.0, 90.0));
    until(&mut h, "the second window", |h| {
        h.server().stat("windows") >= 3
    });
    h.settle();
    assert_eq!(h.state().focus_hides(), 0, "nothing to hide");
    assert_eq!(
        h.commits(),
        commits,
        "a focus change under a hidden launcher costs no commit: {:?}",
        h.mutations()
    );

    drop((a, b));
    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn showing_and_hiding_is_one_mutation_each() {
    // The cost claim the whole design rests on: the tree is built once,
    // so coming and going is one `SetVisible` and not a rebuild. A
    // launcher that re-created its window would pay a `CreateWindow`, a
    // measurement round trip per string and a first paint while the user
    // is already typing.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the first show", |h| h.state().is_visible());
    // The first show is the expensive one (it clears the query and
    // re-ranks), so measure the *second*, which is the steady state.
    super_tap(&mut h);
    until(&mut h, "the hide", |h| !h.state().is_visible());

    h.tap();
    h.clear_tap();
    super_tap(&mut h);
    until(&mut h, "the second show", |h| h.state().is_visible());
    let visibles = h
        .mutations()
        .iter()
        .filter(|m| m.op == "SetVisible")
        .count();
    assert_eq!(visibles, 1, "one SetVisible: {:?}", h.mutations());
    assert!(
        !h.mutations().iter().any(|m| m.op == "CreateWindow"),
        "nothing was rebuilt: {:?}",
        h.mutations()
    );

    h.clear_tap();
    super_tap(&mut h);
    until(&mut h, "the second hide", |h| !h.state().is_visible());
    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert_eq!(
        ops,
        vec!["SetVisible", "Commit"],
        "hiding is exactly one mutation and its commit"
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn escape_hides_the_launcher() {
    // Escape is nobody's chord, which is why the launcher can have it:
    // it arrives as an ordinary key through the grab, and the app-level
    // handler takes it because the focused text field declined it.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());

    h.key(key::ESC);
    until(&mut h, "the hide", |h| !h.state().is_visible());
    until(&mut h, "the grab to go", |h| {
        h.server().stat("grabbed") == 0
    });

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn the_launcher_never_takes_focus() {
    // `NO_FOCUS` is not decoration: a launcher that took focus would make
    // the window behind it look inactive and would move the MRU order
    // every time the user glanced at it. So the keyboard arrives through
    // the *grab* instead, which is what the test above exercises — this
    // one pins down that focus really did not move.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(
        h.server().stat("grabbed"),
        1,
        "it reads the keyboard through a grab"
    );
    // The overlay is on the Overlay layer and reserves nothing: a
    // launcher that reserved space would shrink the desktop every time it
    // opened.
    assert_eq!(h.server().stat("exclusive_zones"), 0);

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn typing_narrows_the_list_and_the_best_match_is_first() {
    // The search path end to end: real key presses into the field, the
    // field's `on_change`, the ranking, and the rows the ranking
    // produced.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(
        h.state().match_count(),
        3,
        "an empty query shows everything: {:?}",
        h.state().match_names()
    );

    // `KEY_C`, `KEY_A`, `KEY_L`: evdev codes for c, a, l.
    for code in [46u32, 30, 38] {
        h.key(code);
    }
    h.settle();
    assert_eq!(h.state().query(), "cal");
    assert_eq!(h.state().match_names(), vec!["Calculator".to_owned()]);
    assert!(row(&mut h, 0).contains("Calculator"));
    assert!(named(&mut h, "results/1").is_none(), "the other rows went");

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn arrows_move_the_selection_and_wrap() {
    // ↑/↓ are app-level handlers rather than the field's, because the
    // field has its own use for Left/Right and none for Up/Down, and the
    // selection is a property of the *list*.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.state().selected(), 0);
    assert!(row(&mut h, 0).starts_with('▸'), "the first is marked");

    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.state().selected(), 1);
    assert!(row(&mut h, 1).starts_with('▸'));
    assert!(!row(&mut h, 0).starts_with('▸'), "and the first is not");

    // Up from the top wraps to the bottom: the list is short, and a user
    // pressing ↑ at the top means "the last one" far more often than they
    // mean "do nothing".
    h.key(key::UP);
    h.key(key::UP);
    h.settle();
    assert_eq!(h.state().selected(), h.state().match_count() - 1);

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_desktop_row_asks_for_a_coloured_icon_and_a_builtin_asks_for_a_symbolic_one() {
    // The rule `row_icon` implements, and the reason it is a rule rather
    // than a string: the server's two icon sets are separate namespaces
    // (`docs/icons.md`), so something has to say which a name belongs
    // to, and the entry's **source** already knows. A `.desktop` file's
    // `Icon=` is a claim about the machine's icon theme; a built-in's is
    // a shape compiled into the server, which is exactly why a built-in
    // exists — it is the entry for a box that has no theme at all.
    let dir = std::env::temp_dir().join(format!(
        "nitro-launcher-icons-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    std::fs::write(
        dir.join("browser.desktop"),
        "[Desktop Entry]\nType=Application\nName=Browser\nExec=/bin/true\nIcon=firefox\n",
    )
    .expect("fixture file");
    std::fs::write(
        dir.join("plain.desktop"),
        "[Desktop Entry]\nType=Application\nName=Plain\nExec=/bin/true\n",
    )
    .expect("fixture file");
    let mut h = launcher(
        vec![dir.clone()],
        // A program of its own: a built-in whose `argv[0]` file name
        // matched a `.desktop` entry's would be dropped as a duplicate,
        // which is `rescan`'s job and not what this test is about.
        vec![Entry {
            name: "Zebra".to_owned(),
            argv: vec!["/bin/echo".to_owned()],
            terminal: false,
            icon: Some("calculator".to_owned()),
            source: Source::Builtin,
        }],
    );
    h.settle();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    // Sorted by name: Browser, Plain, Zebra.
    assert_eq!(
        h.state().match_names(),
        vec!["Browser".to_owned(), "Plain".to_owned(), "Zebra".to_owned()]
    );

    // Row 0, the `.desktop` entry: a coloured application icon with the
    // symbolic `window` behind it. On a box whose theme has no
    // `firefox` the fallback has already fired by now, which is the
    // feature working rather than a failure — so both outcomes are
    // asserted, the way `nitro-bar`'s equivalent test does it.
    let id = named(&mut h, "results/0").expect("row 0");
    let b = h.widget::<Button<Launcher>>(id);
    if b.icon_fell_back() {
        assert_eq!(b.icon(), Some(nitro_launcher::FALLBACK_ICON));
    } else {
        assert_eq!(b.icon(), Some("firefox"));
        assert!(b.is_icon_coloured(), "a .desktop Icon= names the theme");
        assert_eq!(b.icon_fallback(), Some(nitro_launcher::FALLBACK_ICON));
    }
    assert!(
        b.icon_size()
            .is_some_and(|px| (px - nitro_launcher::ROW_ICON_PX).abs() < 0.01),
        "24 px, not the label's size: got {:?}",
        b.icon_size()
    );
    // The label is still there: the icon is in front of the name, not
    // instead of it, so `hey get results/0 value` is unaffected.
    assert!(b.text().contains("Browser"), "{}", b.text());

    // Row 1, a `.desktop` entry naming no icon: the generic symbolic one
    // rather than a gap, because a column where three names have a
    // picture and two do not looks broken.
    let id = named(&mut h, "results/1").expect("row 1");
    let b = h.widget::<Button<Launcher>>(id);
    assert_eq!(b.icon(), Some(nitro_launcher::FALLBACK_ICON));
    assert!(
        !b.is_icon_coloured(),
        "the generic icon is the symbolic one"
    );

    // Row 2, the built-in: symbolic, and with **no** fallback, because a
    // name from the compiled-in set cannot be missing.
    let id = named(&mut h, "results/2").expect("row 2");
    let b = h.widget::<Button<Launcher>>(id);
    assert_eq!(b.icon(), Some("calculator"));
    assert!(
        !b.is_icon_coloured(),
        "a built-in's icon is the server's own artwork"
    );
    assert!(!b.icon_fell_back(), "and it did not have to fall back");

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_keystroke_costs_nothing_for_a_row_that_did_not_change() {
    // "Work is proportional to what changed", on the keystroke path.
    //
    // Typing `c` narrows Calculator/Mail/Terminal to Calculator alone:
    // row 0 already said "▸ Calculator" and must cost **no** `SetText`
    // at all, while the other two rows are destroyed. The only string
    // that moved is the query field's own.
    //
    // Worth stating what this does *not* prove, because the obvious
    // reading is wrong: writing a row's label twice between flushes is
    // also one `SetText`, since the paint slot caches the last value
    // sent. The cost of a redundant write is a `String`, a mark and a
    // walk — not bytes. What is asserted here is the stronger and more
    // useful property: a row whose text is unchanged sends nothing.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.state().match_count(), 3);
    assert_eq!(row(&mut h, 0), "▸ Calculator");

    h.tap();
    h.clear_tap();
    h.key(46); // KEY_C
    h.settle();
    assert_eq!(h.state().match_names(), vec!["Calculator".to_owned()]);
    assert_eq!(row(&mut h, 0), "▸ Calculator", "row 0 is unchanged");

    let texts: Vec<&nitro_ui::Mutation> =
        h.mutations().iter().filter(|m| m.op == "SetText").collect();
    assert_eq!(
        texts.len(),
        1,
        "only the query field's own text moved; the unchanged row sent \
         nothing: {:?}",
        h.mutations()
    );
    // And the two rows that went were destroyed rather than rewritten.
    assert!(named(&mut h, "results/1").is_none());

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn moving_the_selection_costs_two_set_texts() {
    // The marker is in the label rather than in a colour, so a move is
    // two strings: the row that lost it and the row that gained it. A
    // restyle that rewrote every row would be O(list) per arrow press.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());

    assert!(row(&mut h, 0).starts_with('▸'));

    h.tap();
    h.clear_tap();
    h.key(key::DOWN);
    h.settle();
    assert_eq!(h.state().selected(), 1);
    let texts = h.mutations().iter().filter(|m| m.op == "SetText").count();
    assert_eq!(
        texts,
        2,
        "two rows changed, not the whole list: {:?}",
        h.mutations()
    );
    // And the strings really moved, which is what the count would not
    // catch on its own: an earlier toolkit bug had `mark` stop the
    // `SUB_PAINT` walk at an ancestor that already carried `SUB_LAYOUT`,
    // so the tree said "▸ Mail" and the screen still said "▸ Calculator".
    assert!(row(&mut h, 1).starts_with('▸'));
    assert!(!row(&mut h, 0).starts_with('▸'));

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn enter_launches_the_selected_entry_and_hides() {
    // The keyboard path all the way to a real process: the marker file is
    // what "the application started" means, and a launch that silently
    // did nothing would pass every assertion about the tree.
    let dir = std::env::temp_dir().join(format!("nitro-launcher-enter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let marker = dir.join("launched");
    std::fs::write(
        dir.join("stub.desktop"),
        entry_text(
            "Stub",
            &format!("/bin/sh -c \"echo ok > {}\"", marker.display()),
        ),
    )
    .expect("fixture");

    let mut h = launcher(vec![dir.clone()], Vec::new());
    h.settle();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.state().match_count(), 1, "the stub is the only entry");

    h.key(key::ENTER);
    until(&mut h, "the launch", |h| h.state().launches() == 1);
    assert!(
        !h.state().is_visible(),
        "and the launcher got out of the way"
    );

    until(&mut h, "the process to run", |_| marker.exists());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap_or_default().trim(),
        "ok"
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn clicking_a_row_launches_that_row() {
    // The pointer path, and the one bug a launcher must not have: row 0
    // shows a different application after every keystroke, so its
    // *callback* has to be replaced along with its label.
    let dir = std::env::temp_dir().join(format!("nitro-launcher-click-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let alpha = dir.join("alpha-ran");
    let beta = dir.join("beta-ran");
    std::fs::write(
        dir.join("alpha.desktop"),
        entry_text(
            "Alpha",
            &format!("/bin/sh -c \"echo a > {}\"", alpha.display()),
        ),
    )
    .expect("fixture");
    std::fs::write(
        dir.join("beta.desktop"),
        entry_text(
            "Beta",
            &format!("/bin/sh -c \"echo b > {}\"", beta.display()),
        ),
    )
    .expect("fixture");

    let mut h = launcher(vec![dir.clone()], Vec::new());
    h.settle();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(
        h.state().match_names(),
        vec!["Alpha".to_owned(), "Beta".to_owned()]
    );

    // Narrow to Beta, so row 0 is now a *different* application than it
    // was a moment ago, and click it. `KEY_B` is 48.
    h.key(48);
    h.settle();
    assert_eq!(h.state().match_names(), vec!["Beta".to_owned()]);

    let id = named(&mut h, "results/0").expect("row 0");
    h.click(id);
    until(&mut h, "the launch", |h| h.state().launches() == 1);
    assert_eq!(h.state().last_launch(), Some("/bin/sh"));
    until(&mut h, "the process", |_| beta.exists());
    assert!(!alpha.exists(), "row 0's callback followed its label");

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_query_that_matches_nothing_says_so() {
    // An empty list with no explanation looks exactly like a launcher
    // that has broken.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());

    // `KEY_Z` is 44.
    h.key(44);
    h.key(44);
    h.settle();
    assert_eq!(h.state().match_count(), 0);
    let id = named(&mut h, names::EMPTY).expect("the empty label");
    let text = h.widget::<Label>(id).text().to_owned();
    assert!(text.contains("No matches"), "{text:?}");
    assert!(
        text.contains("zz"),
        "and it says what was searched: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_terminal_entry_is_shown_marked_and_refused() {
    // M3 has no terminal to run one in. Showing it and refusing is the
    // honest failure; hiding it would be "htop is missing", and running
    // it would be a process the user can neither see nor type at.
    let text = format!(
        "{}Terminal=true\n",
        entry_text("Monitor", "/bin/true monitor")
    );
    let (mut h, dir) = with_files("terminal", &[("mon.desktop", &text)]);
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert!(row(&mut h, 0).contains("(terminal)"), "{}", row(&mut h, 0));

    h.key(key::ENTER);
    h.settle();
    assert_eq!(h.state().launches(), 0, "nothing was started");
    assert!(
        h.state()
            .last_error()
            .is_some_and(|e| e.contains("terminal")),
        "and the reason is on screen: {:?}",
        h.state().last_error()
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_launch_that_fails_brings_the_launcher_back_with_the_reason() {
    // A launch that failed silently is indistinguishable from a launcher
    // that is broken — and the program not being on `PATH` is the common
    // case, because a `.desktop` file can name anything.
    let text = entry_text("Ghost", "nitro-no-such-binary-ever");
    let (mut h, dir) = with_files("missing", &[("ghost.desktop", &text)]);
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());

    h.key(key::ENTER);
    until(&mut h, "the failure", |h| h.state().last_error().is_some());
    assert_eq!(h.state().launches(), 0);
    assert!(h.state().is_visible(), "it came back rather than vanishing");
    let id = named(&mut h, names::EMPTY).expect("the empty label");
    assert!(
        h.widget::<Label>(id).text().contains("Ghost"),
        "the reason names the entry: {:?}",
        h.widget::<Label>(id).text()
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn opening_the_launcher_clears_the_previous_query() {
    // A launcher that came back showing the last search would need the
    // user to clear it before typing, every single time.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    h.key(46); // c
    h.settle();
    assert_eq!(h.state().query(), "c");

    super_tap(&mut h);
    until(&mut h, "the hide", |h| !h.state().is_visible());
    super_tap(&mut h);
    until(&mut h, "the second show", |h| h.state().is_visible());
    assert_eq!(h.state().query(), "", "the query was cleared");
    let id = named(&mut h, names::QUERY).expect("the field");
    assert_eq!(
        h.widget::<TextField<Launcher>>(id).text(),
        "",
        "and so was the field, not just the state"
    );
    assert_eq!(h.state().match_count(), 3, "the whole list is back");

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn an_application_installed_since_start_up_appears_on_the_next_show() {
    // The rescan-on-show rule. Re-reading every `.desktop` file on each
    // keystroke would be a few hundred syscalls per character; never
    // re-reading them would mean restarting the launcher after every
    // install.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.state().match_count(), 3);
    super_tap(&mut h);
    until(&mut h, "the hide", |h| !h.state().is_visible());

    // The directory mtime has to move, and the clock's resolution is not
    // the filesystem's.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        dir.join("new.desktop"),
        entry_text("Newcomer", "/bin/true new"),
    )
    .expect("install");

    super_tap(&mut h);
    until(&mut h, "the rescan", |h| h.state().match_count() == 4);
    assert!(
        h.state().match_names().contains(&"Newcomer".to_owned()),
        "{:?}",
        h.state().match_names()
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_builtin_is_launchable_with_no_desktop_files_at_all() {
    // The test box's state: a freshly rsynced `~/nitro-bin` and nothing
    // in `/usr/share/applications`. The launcher lists its own siblings
    // so the box works anyway.
    let dir = std::env::temp_dir().join(format!("nitro-launcher-builtin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let marker = dir.join("ran");
    std::fs::create_dir_all(&dir).expect("dir");
    let mut h = launcher(
        vec![dir.join("no-such-applications")],
        vec![Entry {
            name: "Calculator".to_owned(),
            argv: vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("echo ok > {}", marker.display()),
            ],
            terminal: false,
            icon: Some("calculator".to_owned()),
            source: Source::Builtin,
        }],
    );
    h.settle();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    assert_eq!(h.state().match_names(), vec!["Calculator".to_owned()]);

    h.key(key::ENTER);
    until(&mut h, "the launch", |h| h.state().launches() == 1);
    until(&mut h, "the process", |_| marker.exists());

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn every_part_is_addressable_for_hey() {
    // `hey nitro-launcher list` is the smoke test, and it can only find
    // what is named. `results/0` in particular is the agentic path's
    // whole address.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    for path in [names::QUERY, names::RESULTS, names::EMPTY, "results/0"] {
        assert!(named(&mut h, path).is_some(), "no widget at {path}");
    }

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn a_hidden_launcher_is_silent_while_idle() {
    // The toolkit's idle contract, which a launcher has to keep too: it
    // sits in `epoll_wait` between taps, and the hotkey that wakes it is
    // the server's rather than a poll of anything.
    let (mut h, dir) = harness();
    assert!(!h.state().is_visible());
    assert_eq!(h.next_timeout(), None, "nothing is armed");
    h.assert_idle(200);

    // And a *shown* launcher is idle too: the tree settles and then
    // nothing moves until a key arrives.
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    h.assert_idle(200);

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn the_overlay_paints_where_the_anchor_put_it() {
    // A centred anchor is the absence of both edges on both axes, and the
    // launcher is the only surface in the tree that uses it. The window
    // keeps its own size — unlike a bar, which is resized by spanning —
    // so what this asserts is that the anchor did not silently span.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    let size = h.ui().window_size();
    assert!(
        (size.w - 300.0).abs() < 1.0 && (size.h - 220.0).abs() < 1.0,
        "a centred anchor does not resize: {size:?}"
    );
    // And there really are pixels there: the panel, the field and the
    // rows, against the theme's background.
    let bg = h.ui().theme().background.to_u32() >> 8;
    assert!(
        h.has_ink(nitro_core::Rect::new(0.0, 0.0, size.w, size.h), bg),
        "the overlay painted something"
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

#[test]
fn the_result_rows_are_painted() {
    // The defect this test exists for: `hey list` showed
    // `results/container[0]/0  button  0  ▸ Calculator  12,71,576,28` —
    // a row with a rect, enabled, clickable, launching the right thing —
    // and the screenshot showed an empty panel below the query field.
    // The model was right and the pixels were missing, so the assertion
    // has to be about pixels: ink **inside the first row's rect**, not
    // ink somewhere in the window.
    let (mut h, dir) = harness();
    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());

    // `KEY_C`, `KEY_A`, `KEY_L`: one match, so row 0 is Calculator and
    // there is nothing else in the list to confuse a rect with.
    for code in [46u32, 30, 38] {
        h.key(code);
    }
    h.settle();
    assert!(row(&mut h, 0).contains("Calculator"), "{}", row(&mut h, 0));

    let id = named(&mut h, "results/0").expect("row 0");
    let r = h.bounds(id);
    assert!(r.w > 1.0 && r.h > 1.0, "the row has a rect: {r:?}");

    // Two separate questions, and they need two different reference
    // colours. "Is the row there at all" is the row's fill against the
    // panel behind it; "is its label there" is the glyphs against the
    // row's own fill.
    //
    // The row is painted **most** of the rect, not a hairline: a button
    // clipped away to nothing still leaves a sliver of border, so a bare
    // `> 0` would pass on the bug this test exists for.
    let bg = h.ui().theme().background.to_u32() >> 8;
    let painted = h.ink_count(r, bg);
    let area = (r.w * r.h) as usize;
    assert!(
        painted > area / 2,
        "the result row is clipped away inside {r:?}: {painted} of {area} \
         pixels differ from the panel background {bg:06x}"
    );

    // And the label. The reference is `theme.button` — the enabled,
    // unhovered, unpressed face a result row actually paints — **not**
    // `theme.surface`: they are different colours (0xe4e4e8 vs 0xffffff),
    // and measuring against `surface` would count every pixel of a
    // glyphless fill as ink, making this a weaker restatement of the
    // assertion above rather than a test of the text.
    //
    // Skipped on a server with no fonts, which draws no glyphs at all;
    // everything above still holds there.
    if h.has_text() {
        let face = h.ui().theme().button.to_u32() >> 8;
        let glyphs = h.ink_count(r, face);
        assert!(
            glyphs > 0,
            "the result row's text painted nothing inside {r:?} \
             (row face {face:06x})"
        );
        assert!(
            glyphs < area,
            "every pixel differs from the row's own face {face:06x}, so \
             {glyphs} is the fill rather than the glyphs"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

/// How many pixels of two same-sized screenshots differ.
///
/// The comparison `the_launcher_is_not_on_screen_before_the_first_tap`
/// is built on: "does the screen look like this other screen", which is
/// the only question that can be asked about a compositor whose backdrop
/// is a gradient (no single colour is "the background").
fn differing(a: &Image, b: &Image) -> usize {
    assert_eq!((a.width, a.height), (b.width, b.height));
    let mut n = 0;
    for y in 0..a.height {
        for x in 0..a.width {
            if a.pixel(x, y) & 0x00ff_ffff != b.pixel(x, y) & 0x00ff_ffff {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn the_launcher_is_not_on_screen_before_the_first_tap() {
    // Found on the test box during the M3-E acceptance run: with the
    // session starting all three shell pieces, the launcher panel was on
    // screen from boot, over the wallpaper, before anything had tapped
    // Super. `hey nitro-launcher list` showed the rows, `is_visible()`
    // said false, and the screen said otherwise.
    //
    // That gap is why this assertion is about **pixels on the output**
    // and not about the launcher's own bool. Every other hide/show test
    // here asserts `!h.state().is_visible()`, which was true the whole
    // time: the bool starts `false`, `hide()` used to return early when
    // it was already `false`, and so the start-up hide never sent the
    // `SetVisible(false)` the window needed. The launcher believed it
    // was hidden and the server had never been told.
    //
    // The comparison is **differential** rather than against a
    // reference colour, because what is behind the overlay is the
    // compositor's gradient: no single colour is "the background", so a
    // `!= background` count would report the whole rectangle as ink
    // whether the launcher was there or not (it did, at 300x220 = 66000,
    // which is what sent this test through three wrong versions).
    //
    // So the question is asked as: **does the screen at boot look like
    // the screen with the launcher definitely hidden?** The reference is
    // produced by a show and a hide, which is the path that certainly
    // sends both mutations.
    let (mut h, dir) = harness();
    for _ in 0..20 {
        h.settle();
    }
    assert!(
        !h.state().is_visible(),
        "the launcher believes it is hidden"
    );
    let at_boot = h.output_shot();

    super_tap(&mut h);
    until(&mut h, "the show", |h| h.state().is_visible());
    let shown = h.output_shot();

    super_tap(&mut h);
    until(&mut h, "the hide", |h| !h.state().is_visible());
    let hidden = h.output_shot();

    // The positive control first, because it is what makes the real
    // assertion mean anything: showing the launcher *does* change the
    // screen. Without it, a launcher whose window had been deleted
    // outright would satisfy everything below.
    let shown_vs_hidden = differing(&shown, &hidden);
    assert!(
        shown_vs_hidden > 1000,
        "showing the overlay changed only {shown_vs_hidden} pixels; \
         this test cannot see the launcher at all"
    );

    // And the claim: at boot the screen already looked like this.
    let boot_vs_hidden = differing(&at_boot, &hidden);
    assert_eq!(
        boot_vs_hidden, 0,
        "the launcher is on screen before anybody asked for it: {boot_vs_hidden} \
         pixels differ from the same desktop with the overlay explicitly hidden"
    );

    let _ = std::fs::remove_dir_all(&dir);
    h.quit();
}

/// The state for the reaping test: a `Children` and nothing else, which
/// is all the hook the launcher registers ever touches.
struct Reaper {
    children: Children,
}

#[test]
fn a_launched_process_is_reaped_the_moment_it_exits() {
    // Issue #555: before this, a launched program stayed a zombie until
    // the user launched something else, because the only `try_wait` was
    // the one at the start of the next spawn. Now every child carries a
    // pidfd, the pidfd is registered with the loop, and the exit is a
    // wakeup like any other.
    //
    // The harness does not run the real `epoll` loop, so **only the
    // wakeup is faked**: the test polls the pidfd itself, exactly as the
    // loop's `epoll_wait` would, and then calls `Ui::run_fd` with the
    // token — which is precisely the call the app loop makes when
    // `epoll` names that descriptor. Everything after that point is the
    // shipped dispatch path.
    let mut h = Harness::new(
        "reap",
        Reaper {
            children: Children::new(),
        },
        |ui: &mut Ui<Reaper>| ui.build(column()),
    );

    let (ui, state) = h.parts();
    state
        .children
        .spawn(&["/bin/sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()])
        .expect("spawn");
    assert_eq!(state.children.len(), 1, "the child is remembered");
    state.children.watch(ui, |s: &mut Reaper| &mut s.children);
    let watched = state.children.watched();
    assert_eq!(watched.len(), 1, "and its pidfd joined the loop");
    let (fd, token) = watched[0];

    // The wakeup the loop would have had. A timeout rather than a
    // blocking poll, so a launcher that never opened a usable pidfd
    // fails this test instead of hanging it.
    let mut fds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    let ts = rustix::event::Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    let n = rustix::event::poll(&mut fds, Some(&ts)).expect("poll");
    assert_eq!(n, 1, "the pidfd became readable when the child exited");

    // And the dispatch, with no second spawn anywhere: this is the whole
    // fix.
    let (ui, state) = h.parts();
    ui.run_fd(state, token);
    assert_eq!(
        h.state().children.len(),
        0,
        "the exited child was reaped on its own exit, not at the next launch"
    );

    // The other half of the fix, and the regression that would cost a
    // core: the hook must have been *removed*. `epoll` is
    // level-triggered and an exited process's pidfd stays readable for
    // ever, so a hook left registered would be dispatched on every turn
    // of the loop and spin the launcher at 100 % CPU. A second `run_fd`
    // with the same token is the observable form of that: with the hook
    // gone it is a harmless no-op.
    let (ui, state) = h.parts();
    ui.run_fd(state, token);
    assert_eq!(
        h.state().children.len(),
        0,
        "a second wakeup on the same token does nothing"
    );
    assert!(
        h.state().children.watched().is_empty(),
        "and no hook is left watching a descriptor that is readable for ever"
    );

    h.quit();
}
