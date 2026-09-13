//! The introspection socket end to end: a real app on the harness, a
//! real client on the other end of a real Unix socket, driven by the
//! `hey` binary where the protocol allows it and by raw lines where a
//! test needs to see the bytes.
//!
//! The point of every test here is that the *outside* can do it. No test
//! calls a widget's method directly to make the thing happen; it sends a
//! request, and then asserts on the tree the app actually has.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;

use nitro_core::Size;
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::test::Harness;
use nitro_ui::widgets::{
    Checkbox, Label, Slider, TextField, button, checkbox, column, label, row, slider, text_field,
};
use nitro_ui::{Ui, WidgetId};

/// The app under test: a dialog with one of everything, named so paths
/// are stable.
struct S {
    clicks: u32,
    typed: String,
    message: Option<WidgetId>,
}

fn build(ui: &mut Ui<S>) -> WidgetId {
    let message = ui.build(label("nothing yet").name("message"));
    let ok = ui.build(
        button("OK")
            .name("ok")
            .on_click(|s: &mut S, ui: &mut Ui<S>| {
                s.clicks += 1;
                let text = format!("clicked {}", s.clicks);
                if let Some(m) = s.message
                    && let Ok(mut l) = ui.widget_mut::<Label>(m)
                {
                    l.set_text(text);
                }
            }),
    );
    let root = ui.build(
        column()
            .gap(6.0)
            .padding(8.0)
            .child(label("Title"))
            .child(
                text_field("")
                    .name("input")
                    .placeholder("type")
                    .on_change(|s: &mut S, _ui: &mut Ui<S>, t: &str| t.clone_into(&mut s.typed)),
            )
            .child(checkbox("Flag").name("flag"))
            .child(
                slider(0.0)
                    .name("level")
                    .range(0.0, 100.0)
                    .step(1.0)
                    .width(120.0),
            ),
    );
    let buttons = ui.build(row().gap(6.0).child(button("Cancel").name("cancel")));
    ui.attach(buttons, ok).unwrap();
    ui.attach(root, message).unwrap();
    ui.attach(root, buttons).unwrap();
    root
}

fn harness() -> (Harness<S>, std::path::PathBuf) {
    let mut h = Harness::with(
        "dialog",
        S {
            clicks: 0,
            typed: String::new(),
            message: None,
        },
        // The window has to fit the harness's 320x240 output *with its
        // decorations on*: since M3 the server frames a window with a 28 px
        // title bar and a 1 px border, so a 280x220 window is a 282x249
        // thing on screen and its bottom rows fall off the output --
        // which would make `shot` return a clipped crop rather than the
        // whole window.
        Some(Size::new(280.0, 200.0)),
        nitro_ui::Theme::default(),
        build,
    );
    // The builder closure cannot write to the state, so the label's id
    // is recovered afterwards — the same trick `tests/ui.rs` uses.
    let root = h.ui().root().unwrap();
    let message = h.ui().children(root)[4];
    h.state_mut().message = Some(message);
    let path = h.open_socket("dialog");
    h.settle();
    (h, path)
}

/// A client of the socket, on its own thread: the app is served by the
/// test thread, so a blocking request from the test thread would
/// deadlock.
struct Client {
    tx: Option<std::sync::mpsc::Sender<String>>,
    rx: std::sync::mpsc::Receiver<Reply>,
    /// Our own handle on the socket, so `Drop` can shut it down: a
    /// watcher's thread is parked in `read_line` for ever otherwise, and
    /// joining it would hang the test rather than fail it.
    sock: UnixStream,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// A parsed reply: the status line and the body lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    status: String,
    body: Vec<String>,
    /// Pixel bytes, for `shot`.
    pixels: Vec<u8>,
}

impl Client {
    fn connect(path: &Path) -> Self {
        let (tx, req_rx) = std::sync::mpsc::channel::<String>();
        let (rep_tx, rx) = std::sync::mpsc::channel::<Reply>();
        let sock = UnixStream::connect(path).expect("connect to the app socket");
        let mine = sock.try_clone().expect("dup the socket");
        let handle = std::thread::spawn(move || {
            let mut conn = BufReader::new(sock);
            while let Ok(req) = req_rx.recv() {
                if conn.get_mut().write_all(req.as_bytes()).is_err() {
                    return;
                }
                let mut status = String::new();
                if conn.read_line(&mut status).unwrap_or(0) == 0 {
                    return;
                }
                let status = status.trim_end_matches('\n').to_owned();
                // `shot` answers with pixels, not lines.
                let mut pixels = Vec::new();
                let mut body = Vec::new();
                if req.starts_with("shot") && status.starts_with("ok ") {
                    let f: Vec<u32> = status["ok ".len()..]
                        .split_whitespace()
                        .map(|x| x.parse().unwrap())
                        .collect();
                    pixels = vec![0u8; (f[2] * f[1]) as usize];
                    conn.read_exact(&mut pixels).expect("pixels");
                } else if !req.starts_with("watch") {
                    loop {
                        let mut line = String::new();
                        if conn.read_line(&mut line).unwrap_or(0) == 0 || line == "\n" {
                            break;
                        }
                        body.push(line.trim_end_matches('\n').to_owned());
                    }
                }
                if rep_tx
                    .send(Reply {
                        status,
                        body,
                        pixels,
                    })
                    .is_err()
                {
                    return;
                }
                if req.starts_with("watch") {
                    // A watcher streams events; each is one line and is
                    // reported as its own reply.
                    loop {
                        let mut line = String::new();
                        if conn.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if rep_tx
                            .send(Reply {
                                status: line.trim_end_matches('\n').to_owned(),
                                body: Vec::new(),
                                pixels: Vec::new(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            tx: Some(tx),
            rx,
            sock: mine,
            handle: Some(handle),
        }
    }

    /// Send a request and pump the app until the reply arrives.
    fn ask<A: 'static>(&self, h: &mut Harness<A>, request: &str) -> Reply {
        self.tx
            .as_ref()
            .expect("live")
            .send(format!("{request}\n"))
            .expect("send");
        let mut got = None;
        h.pump_socket_until(request, |_| match self.rx.try_recv() {
            Ok(r) => {
                got = Some(r);
                true
            }
            Err(_) => false,
        });
        got.expect("a reply")
    }

    /// The next streamed event, pumping until it arrives.
    fn next_event<A: 'static>(&self, h: &mut Harness<A>) -> String {
        let mut got = None;
        h.pump_socket_until("an event", |_| match self.rx.try_recv() {
            Ok(r) => {
                got = Some(r.status);
                true
            }
            Err(_) => false,
        });
        got.expect("an event")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Dropping the sender ends the request loop, and shutting the
        // socket down unparks a watcher thread blocked reading events.
        self.tx.take();
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Split a `list` line into its six fields.
fn fields(line: &str) -> Vec<&str> {
    line.split('\t').collect()
}

#[test]
fn list_names_every_widget_with_a_stable_path() {
    let (mut h, path) = harness();
    let c = Client::connect(&path);
    let r = c.ask(&mut h, "list");
    assert_eq!(r.status, "ok");
    let paths: Vec<&str> = r.body.iter().map(|l| fields(l)[0]).collect();
    assert_eq!(
        paths,
        [
            "window",
            "window/label[0]",
            "window/input",
            "window/flag",
            "window/level",
            "window/message",
            "window/container[0]",
            "window/container[0]/cancel",
            "window/container[0]/ok",
        ],
        "named widgets by name, unnamed by role[i] among same-role siblings"
    );
    // The fields are path, role, name, value, bounds, flags.
    let ok = r
        .body
        .iter()
        .find(|l| fields(l)[0].ends_with("/ok"))
        .unwrap();
    let f = fields(ok);
    assert_eq!(f.len(), 6);
    assert_eq!(f[1], "button");
    assert_eq!(f[2], "ok");
    assert!(
        f[4].split(',').count() == 4,
        "bounds are x,y,w,h: {:?}",
        f[4]
    );
    assert!(f[5].contains("enabled"), "flags: {:?}", f[5]);

    // A subtree lists only itself and its descendants.
    let sub = c.ask(&mut h, "list window/container[0]");
    let paths: Vec<&str> = sub.body.iter().map(|l| fields(l)[0]).collect();
    assert_eq!(
        paths,
        [
            "window/container[0]",
            "window/container[0]/cancel",
            "window/container[0]/ok"
        ]
    );

    assert_eq!(
        c.ask(&mut h, "list window/nope").status,
        "err no such widget"
    );
}

#[test]
fn a_unique_name_resolves_without_naming_the_layout() {
    // `ok` lives inside a row, so its canonical path is
    // `window/container[0]/ok` — a path that names the *layout* and that
    // rearranging the dialog would break. A name unique in the subtree
    // is therefore also addressed directly from above it.
    let (mut h, path) = harness();
    let c = Client::connect(&path);

    let short = c.ask(&mut h, "get window/ok value");
    assert_eq!(short.status, "ok");
    assert_eq!(short.body, ["OK"]);

    // It is the same widget the spelled-out path names, and the
    // canonical path is what `get path` still reports: the shortcut is
    // an addressing convenience, not a second identity.
    let full = c.ask(&mut h, "get window/container[0]/ok value");
    assert_eq!(full.body, short.body);
    assert_eq!(
        c.ask(&mut h, "get window/ok path").body,
        ["window/container[0]/ok"]
    );

    // And it drives the real callback, which is the point of having it.
    assert_eq!(c.ask(&mut h, "do window/ok click").status, "ok");
    h.settle();
    assert_eq!(h.state().clicks, 1);

    // A name that is not in the subtree is still an error rather than a
    // guess, and an explicit `role[i]` segment is matched before any
    // search: nothing that resolved before resolves differently.
    assert_eq!(
        c.ask(&mut h, "get window/nowhere").status,
        "err no such widget"
    );
    assert_eq!(
        c.ask(&mut h, "get window/container[0] path").body,
        ["window/container[0]"]
    );
}

#[test]
fn get_reports_properties_and_one_of_them() {
    let (mut harness, path) = harness();
    let client = Client::connect(&path);
    let reply = client.ask(&mut harness, "get window/level");
    assert_eq!(reply.status, "ok");
    let props: std::collections::HashMap<&str, &str> = reply
        .body
        .iter()
        .map(|line| line.split_once('\t').expect("prop<TAB>value"))
        .collect();
    assert_eq!(props["role"], "slider");
    assert_eq!(props["name"], "level");
    assert_eq!(props["value"], "0");
    assert_eq!(props["enabled"], "true");
    assert_eq!(props["focusable"], "true");
    assert_eq!(props["focused"], "false");
    assert!(props["actions"].contains("set_value"));

    // One property is the bare value, which is what makes it shell-usable.
    let one = client.ask(&mut harness, "get window/level value");
    assert_eq!(one.body, ["0"]);

    assert_eq!(
        client.ask(&mut harness, "get window/level frob").status,
        "err unknown property `frob`"
    );
    assert_eq!(
        client.ask(&mut harness, "get").status,
        "err get needs a path"
    );
}

#[test]
fn do_click_runs_the_real_callback_and_the_app_state_changes() {
    let (mut h, path) = harness();
    let c = Client::connect(&path);
    assert_eq!(h.state().clicks, 0);

    let r = c.ask(&mut h, "do window/container[0]/ok click");
    assert_eq!(r.status, "ok");
    h.settle();
    assert_eq!(
        h.state().clicks,
        1,
        "the app's own `&mut S` was handed over"
    );
    // And the label the callback edited really changed — read back
    // through the same socket, which is the whole point.
    assert_eq!(
        c.ask(&mut h, "get window/message value").body,
        ["clicked 1"]
    );

    // An unknown action is an error value, not a panic.
    assert_eq!(
        c.ask(&mut h, "do window/message frobnicate").status,
        "err unknown action `frobnicate`"
    );
    assert_eq!(
        c.ask(&mut h, "do window/nowhere click").status,
        "err no such widget"
    );
}

#[test]
fn do_drives_every_role_through_its_actions() {
    let (mut h, path) = harness();
    let c = Client::connect(&path);

    assert_eq!(c.ask(&mut h, "do window/flag toggle").status, "ok");
    h.settle();
    let root = h.ui().root().unwrap();
    let flag = h.ui().children(root)[2];
    assert!(h.widget::<Checkbox<S>>(flag).is_checked());
    assert_eq!(c.ask(&mut h, "get window/flag value").body, ["true"]);

    assert_eq!(c.ask(&mut h, "do window/level set_value 42").status, "ok");
    h.settle();
    let level = h.ui().children(root)[3];
    assert_eq!(
        h.widget::<Slider<S>>(level).value().to_bits(),
        42.0f32.to_bits()
    );

    assert_eq!(
        c.ask(&mut h, "do window/input set_value hello world")
            .status,
        "ok",
        "a value with a space in it arrives whole"
    );
    h.settle();
    let input = h.ui().children(root)[1];
    assert_eq!(h.widget::<TextField<S>>(input).text(), "hello world");
    assert_eq!(h.state().typed, "hello world", "on_change fired");

    // `focus` works on anything focusable and really moves the focus.
    assert_eq!(c.ask(&mut h, "do window/flag focus").status, "ok");
    h.settle();
    assert_eq!(h.ui().focused(), Some(flag));
    assert_eq!(c.ask(&mut h, "get window/flag focused").body, ["true"]);
}

#[test]
fn set_goes_through_the_same_setters_the_app_uses() {
    let (mut h, path) = harness();
    let c = Client::connect(&path);

    assert_eq!(c.ask(&mut h, "set window/input text typed in").status, "ok");
    h.settle();
    let root = h.ui().root().unwrap();
    let input = h.ui().children(root)[1];
    assert_eq!(h.widget::<TextField<S>>(input).text(), "typed in");

    assert_eq!(c.ask(&mut h, "set window/level value 75").status, "ok");
    h.settle();
    assert_eq!(c.ask(&mut h, "get window/level value").body, ["75"]);

    // A read-only property says so rather than silently doing nothing.
    assert_eq!(
        c.ask(&mut h, "set window/level bounds 1,2,3,4").status,
        "err `bounds` is read-only"
    );
    assert_eq!(
        c.ask(&mut h, "set window/level frob 1").status,
        "err no settable property `frob`"
    );
    assert_eq!(
        c.ask(&mut h, "set window/input").status,
        "err set needs a path, a property and a value"
    );

    // Renaming a widget renames its path, which is the point of names.
    assert_eq!(c.ask(&mut h, "set window/flag name toggle").status, "ok");
    h.settle();
    assert_eq!(c.ask(&mut h, "get window/toggle role").body, ["checkbox"]);
}

#[test]
fn watch_streams_changes_however_they_were_made() {
    let (mut h, path) = harness();
    let driver = Client::connect(&path);
    let watcher = Client::connect(&path);
    assert_eq!(watcher.ask(&mut h, "watch *").status, "ok");

    // A change made through the socket is reported…
    assert_eq!(driver.ask(&mut h, "set window/level value 30").status, "ok");
    let ev = watcher.next_event(&mut h);
    assert_eq!(ev, "event window/level value 30");

    // …and so is one the app's own callback made. The click itself is
    // reported first (the widget announces it), then the value change
    // the callback caused — one batch, two events.
    assert_eq!(
        driver.ask(&mut h, "do window/container[0]/ok click").status,
        "ok"
    );
    assert_eq!(
        watcher.next_event(&mut h),
        "event window/container[0]/ok click "
    );
    assert_eq!(
        watcher.next_event(&mut h),
        "event window/message value clicked 1",
        "a change made by the app's own code is reported identically"
    );

    // Focus changes are reported too.
    assert_eq!(driver.ask(&mut h, "do window/flag focus").status, "ok");
    let ev = watcher.next_event(&mut h);
    assert_eq!(ev, "event window/flag focus true");
}

#[test]
fn watch_reports_a_click_which_no_value_diff_could_show() {
    // A button that runs its callback leaves no trace in the tree, so a
    // snapshot diff cannot see it. `click` is reported because the
    // widget says so, which is why `EventCx::report_activation` exists.
    let (mut h, path) = harness();
    let driver = Client::connect(&path);
    let watcher = Client::connect(&path);
    assert_eq!(watcher.ask(&mut h, "watch *").status, "ok");

    assert_eq!(
        driver
            .ask(&mut h, "do window/container[0]/cancel click")
            .status,
        "ok",
        "Cancel has no callback and changes nothing"
    );
    assert_eq!(
        watcher.next_event(&mut h),
        "event window/container[0]/cancel click ",
        "the click is reported even though nothing in the tree changed"
    );
}

#[test]
fn watch_on_a_path_ignores_the_rest_of_the_tree() {
    let (mut h, path) = harness();
    let driver = Client::connect(&path);
    let watcher = Client::connect(&path);
    assert_eq!(watcher.ask(&mut h, "watch window/level").status, "ok");
    // A change elsewhere produces nothing…
    assert_eq!(driver.ask(&mut h, "set window/input text x").status, "ok");
    h.settle();
    h.serve_socket();
    assert!(
        watcher.rx.try_recv().is_err(),
        "a watcher on one widget saw another widget's change"
    );
    // …and a change inside the watched path does.
    assert_eq!(driver.ask(&mut h, "set window/level value 9").status, "ok");
    assert_eq!(watcher.next_event(&mut h), "event window/level value 9");
    assert_eq!(
        driver.ask(&mut h, "watch window/nowhere").status,
        "err no such widget"
    );
}

#[test]
fn shot_returns_exactly_this_window() {
    let (mut harness, path) = harness();
    let client = Client::connect(&path);
    let reply = client.ask(&mut harness, "shot");
    let header: Vec<u32> = reply.status["ok ".len()..]
        .split_whitespace()
        .map(|n| n.parse().expect("a number"))
        .collect();
    let (width, height, stride) = (header[0], header[1], header[2]);
    let size = harness.ui().window_size();
    assert_eq!(
        (width, height),
        (size.w as u32, size.h as u32),
        "cropped to the window, not the whole output"
    );
    assert_eq!(stride, width * 4);
    assert_eq!(reply.pixels.len(), (stride * height) as usize);
    // The window has a background, so the shot is not all zeroes.
    assert!(
        reply.pixels.chunks_exact(4).any(|px| px[..3] != [0, 0, 0]),
        "the screenshot is blank"
    );
}

#[test]
fn a_malformed_request_is_an_error_not_a_dropped_connection() {
    let (mut h, path) = harness();
    let c = Client::connect(&path);
    assert_eq!(
        c.ask(&mut h, "frobnicate").status,
        "err unknown request `frobnicate`"
    );
    // And the connection still works afterwards, which is the point.
    assert_eq!(c.ask(&mut h, "get window role").body, ["container"]);
}

#[test]
fn the_socket_is_removed_when_the_app_goes_away() {
    let (h, path) = harness();
    assert!(path.exists(), "the socket is there while the app runs");
    let dir = h.socket_dir().unwrap();
    assert_eq!(
        nitro_ui::introspect::list_apps(&dir).len(),
        1,
        "and `hey` can find it by reading the directory"
    );
    drop(h);
    assert!(!path.exists(), "and gone when the app is");
}

#[test]
fn paths_survive_a_label_changing_its_text() {
    // A label's *accessible* name is its text, which is not addressable
    // (it has spaces). Its path must therefore not change when the text
    // does — that is the whole reason `.name()` is separate.
    let (mut h, path) = harness();
    let c = Client::connect(&path);
    let before = c.ask(&mut h, "list").body;
    assert_eq!(
        c.ask(&mut h, "do window/container[0]/ok click").status,
        "ok"
    );
    h.settle();
    let after = c.ask(&mut h, "list").body;
    let paths =
        |b: &[String]| -> Vec<String> { b.iter().map(|l| fields(l)[0].to_owned()).collect() };
    assert_eq!(
        paths(&before),
        paths(&after),
        "paths are stable across a text change"
    );
}

#[test]
fn a_widget_with_no_name_is_addressed_by_role_and_index() {
    let mut h = Harness::sized("indexed", (), Size::new(200.0, 160.0), |ui: &mut Ui<()>| {
        ui.build(
            column()
                .padding(6.0)
                .gap(4.0)
                .child(button("one"))
                .child(label("between"))
                .child(button("two"))
                .child(button("three")),
        )
    });
    let path = h.open_socket("indexed");
    h.settle();
    let c = Client::connect(&path);
    let r = c.ask(&mut h, "list");
    let paths: Vec<&str> = r.body.iter().map(|l| fields(l)[0]).collect();
    assert_eq!(
        paths,
        [
            "window",
            "window/button[0]",
            "window/label[0]",
            "window/button[1]",
            "window/button[2]"
        ],
        "the index counts siblings of the same role, so a label between \
         two buttons does not renumber them"
    );
    // And the index addresses the right one: a button's *value* is its
    // label, while its *name* column is the addressing name it was never
    // given — which is the distinction `.name()` exists to make.
    assert_eq!(c.ask(&mut h, "get window/button[2] value").body, ["three"]);
    assert_eq!(c.ask(&mut h, "get window/button[0] value").body, ["one"]);
    assert_eq!(c.ask(&mut h, "get window/button[2] name").body, ["-"]);
}
