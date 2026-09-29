//! M5-H: the clipboard, end to end on the fake backend, through real
//! pipes and memfds. `docs/wire.md` § Data transfer is the contract; the
//! bookkeeping is unit-tested in `src/data.rs`, and this file is about the
//! wiring: focus authorization, the descriptor relay, the EOF answers, the
//! capability gate and descriptor accounting.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_server::{BackendKind, Config, MAX_PENDING_SELECTIONS, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{DataSource, ErrorCode, Layer, NodeId, caps, window_flags};

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Harness {
    dir: PathBuf,
    path: PathBuf,
    wire_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    /// Start a server. Callers hold [`shared`] (or the write lock) first.
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-data-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nitro").join("control.sock");
        let mut config = Config::fake(1, 1, &path);
        config.backend = BackendKind::Fake {
            width: 640,
            height: 480,
        };
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            path,
            wire_path,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.path).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        h
    }

    fn request_text(&self, req: &str) -> Vec<String> {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
        c.get_mut().write_all(req.as_bytes()).unwrap();
        let mut lines = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = c.read_line(&mut line).unwrap();
            assert!(n > 0, "connection closed mid-reply");
            let l = line.trim_end_matches('\n').to_owned();
            if l.is_empty() {
                break;
            }
            lines.push(l);
        }
        lines
    }

    fn stat(&self, key: &str) -> u64 {
        let lines = self.request_text("stats\n");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
            .parse()
            .unwrap()
    }

    /// A plain client that did not opt into anything.
    fn plain(&self, name: &str) -> Client {
        Client {
            conn: Connection::connect(&self.wire_path, name).expect("wire connect"),
            seen: Vec::new(),
        }
    }

    /// A client that listed `DATA`, the way every conformant one must.
    fn client(&self, name: &str) -> Client {
        let mut c = self.plain(name);
        assert!(c.conn.has_caps(caps::DATA), "a local link advertises DATA");
        c.conn.client_caps(caps::DATA).unwrap();
        c.conn.flush().unwrap();
        c
    }

    fn quit(mut self) {
        let s = UnixStream::connect(&self.path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = BufReader::new(s);
        c.get_mut().write_all(b"quit\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
        let t = self.thread.take().unwrap();
        wait_for("the server thread to stop", || t.is_finished());
        t.join().unwrap().expect("server returned an error");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Client {
    conn: Connection,
    seen: Vec<ServerMsg>,
}

impl Client {
    fn pump(&mut self) -> bool {
        let _ = self.conn.flush();
        self.conn.poll(&mut self.seen).is_ok()
    }

    /// Wait until some received message matches `f`, and remove it.
    fn take<T>(
        &mut self,
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
            self.conn
                .poll(&mut self.seen)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// An undecorated window; returns once it holds keyboard focus.
    fn window(&mut self, id: u32, serial: u32) -> NodeId {
        let root = NodeId(id);
        let size = Size::new(100.0, 80.0);
        self.conn
            .tx()
            .create_window_with(root, "w", size, Layer::Normal, window_flags::UNDECORATED)
            .create_rect(NodeId(id + 1), root, Rect::new(0.0, 0.0, size.w, size.h))
            .fill_solid(NodeId(id + 1), Color::rgb(0x40, 0x40, 0x40))
            .commit(serial)
            .unwrap();
        self.take(
            "Focus",
            |m| matches!(m, ServerMsg::Focus(f) if f.window == root && f.focused),
            |_| (),
        );
        root
    }

    fn copy(&mut self, mimes: &[&str]) {
        let mimes: Vec<String> = mimes.iter().map(|s| (*s).to_owned()).collect();
        self.conn.set_selection(&mimes).unwrap();
        self.conn.flush().unwrap();
    }

    fn paste(&mut self, request: u32, mime: &str) {
        self.conn
            .request_selection(request, DataSource::Clipboard, mime)
            .unwrap();
        self.conn.flush().unwrap();
    }

    fn offer(&mut self) -> Vec<String> {
        self.take(
            "SelectionOffer",
            |m| matches!(m, ServerMsg::SelectionOffer(_)),
            |m| match m {
                ServerMsg::SelectionOffer(o) => o.mimes,
                _ => unreachable!(),
            },
        )
    }

    /// The next `SelectionRequest`: (server id, mime).
    fn asked(&mut self) -> (u32, String) {
        self.take(
            "SelectionRequest",
            |m| matches!(m, ServerMsg::SelectionRequest(_)),
            |m| match m {
                ServerMsg::SelectionRequest(r) => {
                    assert_eq!(r.source, DataSource::Clipboard);
                    (r.request, r.mime)
                }
                _ => unreachable!(),
            },
        )
    }

    fn data(&mut self, request: u32) -> OwnedFd {
        self.take(
            "SelectionData",
            |m| matches!(m, ServerMsg::SelectionData(d) if d.request == request),
            |m| match m {
                ServerMsg::SelectionData(d) => d.fd,
                _ => unreachable!(),
            },
        )
    }

    fn error(&mut self) -> (ErrorCode, String) {
        self.take(
            "Error",
            |m| matches!(m, ServerMsg::Error(_)),
            |m| match m {
                ServerMsg::Error(e) => (e.code, e.msg),
                _ => unreachable!(),
            },
        )
    }

    /// Answer a request with a pipe carrying `bytes`.
    fn answer_pipe(&mut self, id: u32, bytes: &[u8]) {
        let (r, w) = rustix::pipe::pipe().unwrap();
        self.conn.send_selection(id, r).unwrap();
        self.conn.flush().unwrap();
        rustix::io::write(&w, bytes).unwrap();
        drop(w);
    }

    fn selection_data_count(&self) -> usize {
        self.seen
            .iter()
            .filter(|m| matches!(m, ServerMsg::SelectionData(_)))
            .count()
    }
}

/// Read a descriptor to EOF, non-blocking with a deadline, as
/// `docs/wire.md` tells a client to.
fn read_all(fd: &OwnedFd) -> Vec<u8> {
    let flags = rustix::fs::fcntl_getfl(fd).unwrap();
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(rustix::io::Errno::AGAIN) => {
                assert!(Instant::now() < deadline, "no EOF on the selection fd");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("read: {e}"),
        }
    }
}

/// Every test holds this for reading, and the descriptor-accounting test
/// for writing: `/proc/self/fd` counts the whole test process, so that one
/// measurement is exact only while no other test's server is running.
static FDS: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
    FDS.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn two_clients_copy_and_paste_through_a_real_pipe() {
    let _fds = shared();
    let h = Harness::start("copy");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain", "text/html"]);
    assert_eq!(b.offer(), ["text/plain", "text/html"]);
    assert_eq!(
        a.offer(),
        ["text/plain", "text/html"],
        "everyone, owner included"
    );

    b.paste(7, "text/plain");
    let (id, mime) = a.asked();
    assert_eq!(mime, "text/plain");
    a.answer_pipe(id, b"hello, clipboard");
    assert_eq!(read_all(&b.data(7)), b"hello, clipboard");

    // And a sealed-style memfd: the owner already has the bytes.
    b.paste(8, "text/html");
    let (id, mime) = a.asked();
    assert_eq!(mime, "text/html");
    let fd = rustix::fs::memfd_create("sel", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::io::write(&fd, b"<b>hi</b>").unwrap();
    rustix::fs::seek(&fd, rustix::fs::SeekFrom::Start(0)).unwrap();
    a.conn.send_selection(id, fd).unwrap();
    a.conn.flush().unwrap();
    assert_eq!(read_all(&b.data(8)), b"<b>hi</b>");

    assert_eq!(h.stat("selection_transfers"), 2);
    assert_eq!(h.stat("selection_eof"), 0);
    assert_eq!(h.stat("selections_pending"), 0);
    h.quit();
}

#[test]
fn a_client_pasting_its_own_selection() {
    let _fds = shared();
    let h = Harness::start("self");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    a.offer();
    a.paste(3, "text/plain");
    let (id, _) = a.asked();
    a.answer_pipe(id, b"mine");
    assert_eq!(read_all(&a.data(3)), b"mine");
    h.quit();
}

#[test]
fn a_client_without_keyboard_focus_cannot_take_the_selection() {
    let _fds = shared();
    let h = Harness::start("focus");
    let mut b = h.client("b");
    b.window(10, 1);
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    assert_eq!(b.offer(), ["text/plain"]);
    assert_eq!(a.offer(), ["text/plain"]);
    // B's window lost focus to A's. Its SetSelection is the race a client
    // cannot avoid (the Focus was in flight), so it is dropped, not fatal.
    b.copy(&["image/png"]);
    // A round trip on B proves the server has processed the copy.
    b.paste(5, "image/png");
    assert!(read_all(&b.data(5)).is_empty(), "image/png is not on offer");
    assert!(b.pump(), "B is still connected");
    assert!(
        !b.seen.iter().any(|m| matches!(m, ServerMsg::Error(_))),
        "no Error: {:?}",
        b.seen
    );
    assert!(
        !b.seen
            .iter()
            .chain(a.seen.iter())
            .any(|m| matches!(m, ServerMsg::SelectionOffer(_))),
        "no offer, echo or otherwise"
    );
    assert_eq!(h.stat("windows"), 2);
    // A still owns the selection, and serves it.
    let mut c = h.client("c");
    assert_eq!(
        c.offer(),
        ["text/plain"],
        "the opt-in snapshot is still A's"
    );
    c.paste(1, "text/plain");
    let (id, _) = a.asked();
    a.answer_pipe(id, b"still a");
    assert_eq!(read_all(&c.data(1)), b"still a");
    h.quit();
}

#[test]
fn focus_is_per_client_not_per_window() {
    let _fds = shared();
    let h = Harness::start("perclient");
    let mut a = h.client("a");
    a.window(1, 1);
    let mut b = h.client("b");
    b.window(10, 1);
    // A's second window takes focus; its first stays unfocused.
    a.window(3, 2);
    a.copy(&["text/plain"]);
    assert_eq!(b.offer(), ["text/plain"]);
    assert!(a.pump(), "A survived");
    h.quit();
}

#[test]
fn the_owner_disconnecting_mid_transfer_gives_the_requester_eof() {
    let _fds = shared();
    let h = Harness::start("ownergone");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    b.offer();
    b.paste(5, "text/plain");
    a.asked();
    drop(a);
    assert!(read_all(&b.data(5)).is_empty());
    // And the selection went with its owner.
    assert!(b.offer().is_empty());
    std::thread::sleep(Duration::from_millis(50));
    b.pump();
    assert_eq!(b.selection_data_count(), 0, "exactly one answer");
    assert_eq!(h.stat("selection_eof"), 1);
    h.quit();
}

#[test]
fn the_requester_disappearing_does_not_harm_the_owner() {
    let _fds = shared();
    let srv = Harness::start("requestergone");
    let mut req = srv.client("b");
    let mut own = srv.client("a");
    own.window(1, 1);
    own.copy(&["text/plain"]);
    req.offer();
    req.paste(5, "text/plain");
    let (id, _) = own.asked();
    drop(req);
    wait_for("the parked request to go", || {
        srv.stat("selections_pending") == 0
    });
    // The owner answers into the void: its write may get EPIPE, in its own
    // process, and nothing else happens.
    let (rd, wr) = rustix::pipe::pipe().unwrap();
    own.conn.send_selection(id, rd).unwrap();
    own.conn.flush().unwrap();
    wait_for("the stale answer to be dropped", || {
        matches!(rustix::io::write(&wr, b"x"), Err(rustix::io::Errno::PIPE))
    });
    // A is still connected and still paints.
    own.conn
        .tx()
        .fill_solid(NodeId(2), Color::rgb(0xFF, 0, 0))
        .commit(2)
        .unwrap();
    own.conn.flush().unwrap();
    own.take(
        "Presented",
        |m| matches!(m, ServerMsg::Presented(p) if p.serial == 2),
        |_| (),
    );
    assert_eq!(srv.stat("windows"), 1);
    assert_eq!(srv.stat("selection_transfers"), 0);
    srv.quit();
}

#[test]
fn a_new_selection_supersedes_the_old_and_re_offers() {
    let _fds = shared();
    let srv = Harness::start("supersede");
    let mut other = srv.client("c");
    let mut own = srv.client("a");
    own.window(1, 1);
    own.copy(&["text/plain"]);
    assert_eq!(other.offer(), ["text/plain"]);
    other.paste(9, "text/plain");
    own.asked();
    let mut next = srv.client("b");
    next.window(10, 1);
    next.copy(&["image/png"]);
    // The request parked against A is answered at EOF, not left waiting.
    assert!(read_all(&other.data(9)).is_empty());
    for who in [&mut own, &mut next, &mut other] {
        let last = loop {
            let got = who.offer();
            if got == ["image/png"] {
                break got;
            }
        };
        assert_eq!(last, ["image/png"]);
    }
    // The new owner answers new requests; the old one is never asked.
    other.paste(10, "image/png");
    let (id, mime) = next.asked();
    assert_eq!(mime, "image/png");
    next.answer_pipe(id, b"PNG");
    assert_eq!(read_all(&other.data(10)), b"PNG");
    own.pump();
    assert!(
        !own.seen
            .iter()
            .any(|m| matches!(m, ServerMsg::SelectionRequest(_)))
    );
    srv.quit();
}

#[test]
fn a_stale_send_selection_is_dropped_not_fatal() {
    let _fds = shared();
    let h = Harness::start("stale");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    b.offer();
    b.paste(4, "text/plain");
    let (id, _) = a.asked();
    // The same owner replaces its own selection: a new data source, so the
    // old request is cancelled (docs/wire.md § What the server does).
    a.copy(&["text/plain", "text/uri-list"]);
    assert!(read_all(&b.data(4)).is_empty());
    // The late answer to the cancelled request is dropped quietly.
    a.answer_pipe(id, b"late");
    std::thread::sleep(Duration::from_millis(50));
    assert!(a.pump(), "A survived its stale answer");
    b.pump();
    assert_eq!(b.selection_data_count(), 0, "nothing beyond the one EOF");
    assert_eq!(h.stat("selection_transfers"), 0);
    assert_eq!(h.stat("selection_eof"), 1);
    h.quit();
}

#[test]
fn clearing_the_selection_offers_an_empty_list() {
    let _fds = shared();
    let h = Harness::start("clear");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    assert_eq!(b.offer(), ["text/plain"]);
    a.copy(&[]);
    assert!(b.offer().is_empty());
    assert_eq!(a.offer(), ["text/plain"]);
    assert!(a.offer().is_empty());
    b.paste(1, "text/plain");
    assert!(read_all(&b.data(1)).is_empty());
    // A MIME type outside a live offer is answered the same way, and the
    // owner is not woken for it.
    a.copy(&["text/plain"]);
    b.paste(2, "image/png");
    assert!(read_all(&b.data(2)).is_empty());
    a.pump();
    assert!(
        !a.seen
            .iter()
            .any(|m| matches!(m, ServerMsg::SelectionRequest(_)))
    );
    h.quit();
}

#[test]
fn no_descriptors_leak_across_many_transfers() {
    let _only = FDS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let h = Harness::start("leak");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    b.offer();
    // Baseline after one full transfer, so every lazily-opened descriptor
    // (fonts, icons, the connections themselves) already exists.
    b.paste(1_000, "text/plain");
    let (id, _) = a.asked();
    a.answer_pipe(id, b"warm");
    assert_eq!(read_all(&b.data(1_000)), b"warm");
    let base_transfers = h.stat("selection_transfers");
    let base = open_fds();

    for i in 0..100u32 {
        b.paste(i, "text/plain");
        let (id, _) = a.asked();
        a.answer_pipe(id, format!("n{i}").as_bytes());
        assert_eq!(read_all(&b.data(i)), format!("n{i}").as_bytes());
        if i % 5 == 0 {
            // A failure answer: a type nobody offered.
            b.paste(500 + i, "image/png");
            assert!(read_all(&b.data(500 + i)).is_empty());
        }
    }
    assert_eq!(h.stat("selection_transfers") - base_transfers, 100);
    assert_eq!(h.stat("selection_eof"), 20);
    assert_eq!(h.stat("selections_pending"), 0);
    wait_for("the descriptor count to return to baseline", || {
        open_fds() == base
    });
    h.quit();
}

#[test]
fn the_seventeenth_outstanding_request_is_answered_with_eof() {
    let _fds = shared();
    let h = Harness::start("cap");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    b.offer();
    let n = u32::try_from(MAX_PENDING_SELECTIONS).unwrap();
    for i in 0..=n {
        b.paste(i, "text/plain");
    }
    // The first sixteen park; the seventeenth comes straight back at EOF.
    assert!(read_all(&b.data(n)).is_empty());
    wait_for("sixteen parked", || h.stat("selections_pending") == 16);
    b.pump();
    assert_eq!(b.selection_data_count(), 0);
    assert!(b.pump(), "B is still connected");
    // The owner going away answers all sixteen.
    drop(a);
    for i in 0..n {
        assert!(read_all(&b.data(i)).is_empty());
    }
    assert_eq!(h.stat("selection_eof"), 17);
    h.quit();
}

#[test]
fn reusing_an_outstanding_request_id_is_fatal() {
    let _fds = shared();
    let h = Harness::start("reuse");
    let mut b = h.client("b");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    b.offer();
    b.paste(3, "text/plain");
    b.paste(3, "text/plain");
    let (code, msg) = b.error();
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("outstanding"), "{msg}");
    h.quit();
}

#[test]
fn a_drag_request_outside_a_drag_is_fatal() {
    let _fds = shared();
    let h = Harness::start("drag");
    let mut b = h.client("b");
    b.conn
        .request_selection(1, DataSource::Drag, "text/plain")
        .unwrap();
    b.conn.flush().unwrap();
    assert_eq!(b.error().0, ErrorCode::Protocol);
    h.quit();
}

#[test]
fn the_data_ops_need_the_capability_to_be_listed() {
    let _fds = shared();
    let h = Harness::start("nocaps");
    // A pre-M5 client (the bar, the terminal): never sent `ClientCaps`.
    let mut bar = h.plain("bar");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    a.offer();
    a.copy(&[]);
    a.offer();
    std::thread::sleep(Duration::from_millis(50));
    assert!(bar.pump(), "the bar survived the copy");
    assert!(
        !bar.seen
            .iter()
            .any(|m| matches!(m, ServerMsg::SelectionOffer(_))),
        "a client that did not list DATA is never pushed an offer: {:?}",
        bar.seen
    );

    // And one that sends a DATA op without listing the bit is refused.
    let mut rude = h.plain("rude");
    rude.window(20, 1);
    rude.copy(&["text/plain"]);
    let (code, msg) = rude.error();
    assert_eq!(code, ErrorCode::Protocol);
    assert!(msg.contains("ClientCaps"), "{msg}");
    h.quit();
}

#[test]
fn mime_lists_are_bounded_and_ascii() {
    let _fds = shared();
    let h = Harness::start("mimes");
    let cases: [(Vec<String>, ErrorCode); 4] = [
        (vec!["a/b".to_owned(); 65], ErrorCode::Limit),
        (vec!["x".repeat(257)], ErrorCode::Limit),
        (vec!["text/plain; ü".to_owned()], ErrorCode::Protocol),
        (vec![String::new()], ErrorCode::Protocol),
    ];
    for (i, (mimes, want)) in cases.into_iter().enumerate() {
        let mut a = h.client("a");
        let id = u32::try_from(i).unwrap() * 10 + 1;
        a.window(id, 1);
        a.conn.set_selection(&mimes).unwrap();
        a.conn.flush().unwrap();
        assert_eq!(a.error().0, want, "case {i}");
    }
    h.quit();
}

#[test]
fn a_client_opting_into_data_late_is_told_about_the_current_selection() {
    let _fds = shared();
    let h = Harness::start("late");
    let mut a = h.client("a");
    a.window(1, 1);
    a.copy(&["text/plain"]);
    a.offer();
    // C connects after the copy; listing DATA is enough to hear about it.
    let mut c = h.client("c");
    assert_eq!(c.offer(), ["text/plain"]);
    // A second `ClientCaps` that keeps the bit does not repeat it.
    c.conn.client_caps(caps::DATA).unwrap();
    c.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    c.pump();
    assert!(
        !c.seen
            .iter()
            .any(|m| matches!(m, ServerMsg::SelectionOffer(_)))
    );
    h.quit();
}
