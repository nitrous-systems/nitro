//! The system clipboard through `nitro-ui`: a `TextField`'s copy, cut and
//! paste keys, end to end through the in-process server — including the
//! server round trip for a paste inside one app, a second (raw wire)
//! client on either side of a transfer, a hostile owner whose descriptor
//! never reaches EOF, and the app-local fallback on a remote link.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
use nitro_ui::clipboard::{PLAIN_MIME, TEXT_MIME};
use nitro_ui::event::key;
use nitro_ui::test::Harness;
use nitro_ui::widgets::{TextField, column, panel, text_field};
use nitro_ui::{Ui, WidgetId};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{DataSource, Layer, NodeId, caps, window_flags};

#[derive(Default)]
struct S {
    /// Every value field `b`'s `on_change` reported.
    b_changes: Vec<String>,
}

/// Two plain fields `a` (holding `hello world`) and `b`, and a secret one.
fn build(ui: &mut Ui<S>) -> WidgetId {
    let root = ui.build(panel().background(Color::WHITE).padding(8.0));
    let col = ui.build(
        column()
            .gap(6.0)
            .child(text_field("hello world").name("a"))
            .child(
                text_field("")
                    .name("b")
                    .on_change(|s: &mut S, _ui: &mut Ui<S>, t: &str| {
                        s.b_changes.push(t.to_owned());
                    }),
            )
            .child(text_field("").name("pw").secret()),
    );
    ui.attach(root, col).unwrap();
    root
}

struct Fields {
    a: WidgetId,
    b: WidgetId,
    pw: WidgetId,
}

fn harness(name: &str) -> (Harness<S>, Fields) {
    let h = Harness::sized(name, S::default(), Size::new(260.0, 140.0), build);
    finish(h)
}

fn finish(mut h: Harness<S>) -> (Harness<S>, Fields) {
    let root = h.ui().root().unwrap();
    let col = h.ui().children(root)[0];
    let kids = h.ui().children(col);
    (
        h,
        Fields {
            a: kids[0],
            b: kids[1],
            pw: kids[2],
        },
    )
}

fn text(h: &Harness<S>, id: WidgetId) -> String {
    h.widget::<TextField<S>>(id).text().to_owned()
}

fn ctrl(h: &mut Harness<S>, k: u32) {
    h.key_with(key::LEFT_CTRL, k);
}

/// Select everything in `id` and copy it with Ctrl+C.
fn copy_all(h: &mut Harness<S>, id: WidgetId) {
    h.click(id);
    ctrl(h, key::A);
    ctrl(h, key::C);
}

fn paste_into(h: &mut Harness<S>, id: WidgetId, want: &str) {
    h.click(id);
    ctrl(h, key::V);
    h.wait_for("the paste", |h| text(h, id) == want);
    h.settle();
}

/// A raw wire client, the other side of a transfer.
struct Peer {
    conn: Connection,
    seen: Vec<ServerMsg>,
}

impl Peer {
    fn new(h: &Harness<S>, name: &str) -> Self {
        let mut conn = Connection::connect(h.server().wire_path(), name).expect("connect");
        conn.client_caps(caps::DATA).unwrap();
        conn.flush().unwrap();
        Self {
            conn,
            seen: Vec::new(),
        }
    }

    fn take<T>(
        &mut self,
        h: &mut Harness<S>,
        what: &str,
        f: impl Fn(&ServerMsg) -> bool,
        g: impl FnOnce(ServerMsg) -> T,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(i) = self.seen.iter().position(&f) {
                return g(self.seen.remove(i));
            }
            assert!(Instant::now() < deadline, "no {what}; got {:?}", self.seen);
            self.conn.flush().unwrap();
            self.conn.poll(&mut self.seen).unwrap();
            // The harness is on this thread: give it its turns too.
            h.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Offer `mimes` from a short-lived focused window, then hand focus
    /// back to the harness's window.
    fn copy(&mut self, h: &mut Harness<S>, mimes: &[&str]) {
        let root = NodeId(1);
        self.conn
            .tx()
            .create_window_with(
                root,
                "peer",
                Size::new(40.0, 30.0),
                Layer::Normal,
                window_flags::UNDECORATED,
            )
            .create_rect(NodeId(2), root, Rect::new(0.0, 0.0, 40.0, 30.0))
            .fill_solid(NodeId(2), Color::rgb(0x40, 0x40, 0x40))
            .commit(1)
            .unwrap();
        self.take(
            h,
            "Focus",
            |m| matches!(m, ServerMsg::Focus(f) if f.window == root && f.focused),
            |_| (),
        );
        let mimes: Vec<String> = mimes.iter().map(|s| (*s).to_owned()).collect();
        self.conn.set_selection(&mimes).unwrap();
        self.conn.tx().destroy_node(root).commit(2).unwrap();
        self.conn.flush().unwrap();
        h.wait_for("the peer's offer", |h| h.ui().clipboard_mimes() == mimes);
        h.server().focus_window();
        h.settle();
        assert!(h.ui().has_keyboard_focus());
    }

    /// The next `SelectionRequest`: (server id, mime).
    fn asked(&mut self, h: &mut Harness<S>) -> (u32, String) {
        self.take(
            h,
            "SelectionRequest",
            |m| matches!(m, ServerMsg::SelectionRequest(_)),
            |m| match m {
                ServerMsg::SelectionRequest(r) => (r.request, r.mime),
                _ => unreachable!(),
            },
        )
    }

    fn answer(&mut self, id: u32, fd: OwnedFd) {
        self.conn.send_selection(id, fd).unwrap();
        self.conn.flush().unwrap();
    }

    /// Read the selection in `mime`, to EOF.
    fn paste(&mut self, h: &mut Harness<S>, request: u32, mime: &str) -> Vec<u8> {
        self.conn
            .request_selection(request, DataSource::Clipboard, mime)
            .unwrap();
        self.conn.flush().unwrap();
        let fd = self.take(
            h,
            "SelectionData",
            |m| matches!(m, ServerMsg::SelectionData(d) if d.request == request),
            |m| match m {
                ServerMsg::SelectionData(d) => d.fd,
                _ => unreachable!(),
            },
        );
        let flags = rustix::fs::fcntl_getfl(&fd).unwrap();
        rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match rustix::io::read(&fd, &mut buf) {
                Ok(0) => return out,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(rustix::io::Errno::AGAIN) => {
                    assert!(Instant::now() < deadline, "no EOF");
                    h.pump();
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(e) => panic!("read: {e}"),
            }
        }
    }
}

fn memfd(bytes: &[u8]) -> OwnedFd {
    let fd = rustix::fs::memfd_create("peer", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::io::write(&fd, bytes).unwrap();
    rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0)).unwrap();
    fd
}

#[test]
fn copy_in_one_field_and_paste_in_another_through_the_server() {
    let (mut h, f) = harness("clip-roundtrip");
    assert!(h.ui().has_clipboard(), "a local link lists DATA");
    copy_all(&mut h, f.a);
    h.wait_for("our own offer", |h| {
        h.ui().clipboard_mimes() == [TEXT_MIME, PLAIN_MIME]
    });
    paste_into(&mut h, f.b, "hello world");
    assert_eq!(
        h.state().b_changes.last().map(String::as_str),
        Some("hello world"),
        "on_change fired with the pasted text"
    );
    assert_eq!(text(&h, f.a), "hello world", "copy left the source alone");
    assert_eq!(h.ui().clipboard_reads_pending(), 0);
}

#[test]
fn cut_removes_the_selection_and_it_pastes_back() {
    let (mut h, f) = harness("clip-cut");
    h.click(f.a);
    ctrl(&mut h, key::A);
    ctrl(&mut h, key::X);
    assert_eq!(text(&h, f.a), "");
    paste_into(&mut h, f.a, "hello world");
    // And again, into the other field: a paste does not consume it.
    paste_into(&mut h, f.b, "hello world");
}

#[test]
fn ctrl_insert_copies_and_shift_insert_pastes() {
    let (mut h, f) = harness("clip-insert");
    h.click(f.a);
    ctrl(&mut h, key::A);
    ctrl(&mut h, key::INSERT);
    h.click(f.b);
    h.key_with(key::LEFT_SHIFT, key::INSERT);
    h.wait_for("the paste", |h| text(h, f.b) == "hello world");
}

#[test]
fn a_secret_field_does_not_copy_or_cut() {
    let (mut h, f) = harness("clip-secret");
    copy_all(&mut h, f.a);
    h.settle();
    let offer = h.ui().clipboard_mimes().to_vec();
    h.click(f.pw);
    let (ui, s) = h.parts();
    ui.action(s, f.pw, "set_value", Some("hunter2")).unwrap();
    h.settle();
    ctrl(&mut h, key::A);
    ctrl(&mut h, key::C);
    ctrl(&mut h, key::X);
    assert_eq!(text(&h, f.pw), "hunter2", "cut did nothing");
    assert_eq!(h.ui().clipboard_mimes(), offer.as_slice());
    // The clipboard still holds the plain field's text.
    paste_into(&mut h, f.b, "hello world");
    // A secret field may be pasted into.
    h.click(f.pw);
    ctrl(&mut h, key::A);
    ctrl(&mut h, key::V);
    h.wait_for("the paste", |h| text(h, f.pw) == "hello world");
}

#[test]
fn a_multi_line_paste_becomes_one_line() {
    let (mut h, f) = harness("clip-lines");
    h.click(f.a);
    assert!(h.ui().set_clipboard_text("one\ntwo\r\nthree\tfour\u{7}").unwrap());
    paste_into(&mut h, f.b, "one two three four");
}

#[test]
fn a_peer_offers_text_and_ctrl_v_pastes_it() {
    let (mut h, f) = harness("clip-peer-in");
    let mut peer = Peer::new(&h, "peer");
    peer.copy(&mut h, &[PLAIN_MIME]);
    h.click(f.b);
    ctrl(&mut h, key::V);
    let (id, mime) = peer.asked(&mut h);
    assert_eq!(mime, PLAIN_MIME, "the first wanted type that is on offer");
    peer.answer(id, memfd("from the peer".as_bytes()));
    h.wait_for("the paste", |h| text(h, f.b) == "from the peer");
    assert_eq!(h.ui().clipboard_reads_pending(), 0);
}

#[test]
fn a_peer_reads_what_the_app_copied() {
    let (mut h, f) = harness("clip-peer-out");
    let mut peer = Peer::new(&h, "peer");
    copy_all(&mut h, f.a);
    assert_eq!(peer.paste(&mut h, 1, TEXT_MIME), b"hello world");
    assert_eq!(peer.paste(&mut h, 2, PLAIN_MIME), b"hello world");
    // A type that was not offered is EOF, not a hang.
    assert_eq!(peer.paste(&mut h, 3, "image/png"), b"");
}

#[test]
fn a_hostile_owner_that_never_reaches_eof_times_out() {
    let (mut h, f) = harness("clip-hostile");
    let mut peer = Peer::new(&h, "peer");
    peer.copy(&mut h, &[TEXT_MIME]);
    h.click(f.b);
    ctrl(&mut h, key::V);
    let (id, _) = peer.asked(&mut h);
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    rustix::io::write(&ours, b"partial").unwrap();
    peer.answer(id, theirs.into());
    h.settle();
    assert_eq!(h.ui().clipboard_reads_pending(), 1, "still reading");
    assert_eq!(text(&h, f.b), "");
    h.advance_timers(nitro_ui::clipboard::READ_TIMEOUT_MS + 1);
    h.settle();
    assert_eq!(h.ui().clipboard_reads_pending(), 0, "gave up");
    assert_eq!(text(&h, f.b), "", "a partial transfer is not pasted");
    drop(ours);
    // Still alive and still pasting.
    copy_all(&mut h, f.a);
    paste_into(&mut h, f.b, "hello world");
}

#[test]
fn a_remote_app_has_an_app_local_clipboard() {
    let (mut h, f) = finish(Harness::remote("clip-remote", S::default(), build));
    assert!(!h.ui().has_clipboard());
    copy_all(&mut h, f.a);
    assert_eq!(h.ui().clipboard_mimes(), [TEXT_MIME, PLAIN_MIME]);
    paste_into(&mut h, f.b, "hello world");
}

#[test]
fn set_clipboard_without_keyboard_focus_is_refused_locally() {
    let (mut h, _f) = harness("clip-unfocused");
    let mut peer = Peer::new(&h, "peer");
    let root = NodeId(1);
    peer.conn
        .tx()
        .create_window_with(
            root,
            "peer",
            Size::new(40.0, 30.0),
            Layer::Normal,
            window_flags::UNDECORATED,
        )
        .create_rect(NodeId(2), root, Rect::new(0.0, 0.0, 40.0, 30.0))
        .fill_solid(NodeId(2), Color::rgb(0x40, 0x40, 0x40))
        .commit(1)
        .unwrap();
    peer.conn.flush().unwrap();
    h.wait_for("focus to move away", |h| !h.ui().has_keyboard_focus());
    assert!(!h.ui().set_clipboard_text("nope").unwrap());
    h.settle();
    assert!(!h.ui().should_quit(), "the connection survived");
    assert!(h.ui().clipboard_mimes().is_empty());
}
