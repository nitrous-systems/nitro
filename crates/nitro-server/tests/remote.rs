//! The remote listener, end to end: `remote.listen` in a real
//! `server.conf`, a real TCP client over `tcp://`, and the promises that
//! go with it — `caps::REMOTE|WM|TEXT` and never `SHELL`, a refused
//! buffer with the client still connected, pixels identical to the same
//! app over the Unix socket, a listener that appears and disappears on
//! reload, and a closed socket that takes the window with it.
//!
//! Every listener is `127.0.0.1:0` and every test reads the port back out
//! of `stats remote_listen`, so the suite never picks a number and never
//! races another developer on the same box.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_kms::Image;
use nitro_server::input::FakeInput;
use nitro_server::{Config, run};
use nitro_wire::client::Connection;
use nitro_wire::msg::{ClientMsg, CreateBuffer, ServerMsg};
use nitro_wire::types::{BufferId, Layer, NodeId, caps, format};
use nitro_wire::{Endpoint, Error as WireError};

const OUT: (u32, u32) = (320, 240);
const WIN: Size = Size::new(160.0, 100.0);
const RED: Color = Color::rgb(0xFF, 0x00, 0x00);

/// Wait for a condition, polling. Every wait in this file has a deadline:
/// a test that hangs tells you nothing.
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Harness {
    dir: PathBuf,
    control: PathBuf,
    wire_path: PathBuf,
    config_dir: PathBuf,
    config_path: PathBuf,
    thread: Option<JoinHandle<Result<(), nitro_server::Error>>>,
}

impl Harness {
    /// Start a server whose `server.conf` holds `conf`.
    fn start(name: &str, conf: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nitro-remote-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let control = dir.join("nitro").join("control.sock");
        let config_dir = dir.join("config");
        let config_path = config_dir.join("server.conf");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(&config_path, conf).expect("write server.conf");

        let mut config = Config::fake(OUT.0, OUT.1, &control);
        config.config_path = Some(config_path.clone());
        config.fake_input = Some(FakeInput::new().expect("eventfd"));
        let wire_path = config.wire_path.clone();
        let thread = std::thread::spawn(move || run(config));
        let h = Self {
            dir,
            control,
            wire_path,
            config_dir,
            config_path,
            thread: Some(thread),
        };
        wait_for("the control socket", || {
            UnixStream::connect(&h.control).is_ok()
        });
        wait_for("the wire socket", || h.wire_path.exists());
        h
    }

    fn connect(&self) -> BufReader<UnixStream> {
        let s = UnixStream::connect(&self.control).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        BufReader::new(s)
    }

    fn request_text(&self, req: &str) -> Vec<String> {
        let mut c = self.connect();
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

    /// One `stats` line's value, as text.
    fn stat_text(&self, key: &str) -> String {
        let lines = self.request_text("stats\n");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or_else(|| panic!("no `{key}` in {lines:?}"))
            .to_owned()
    }

    fn stat(&self, key: &str) -> u64 {
        self.stat_text(key).parse().expect("a numeric statistic")
    }

    /// The address `remote_listen` reports, or `None` when it is `off`.
    fn remote_listen(&self) -> Option<String> {
        match self.stat_text("remote_listen").as_str() {
            "off" => None,
            addr => Some(addr.to_owned()),
        }
    }

    /// A client over the Unix socket.
    fn local_client(&self, name: &str) -> Connection {
        Connection::connect(&self.wire_path, name).expect("wire connect")
    }

    /// A client over TCP, through the listener the server reports.
    fn remote_client(&self, name: &str) -> Connection {
        let addr = self
            .remote_listen()
            .expect("the remote listener is supposed to be up");
        let endpoint = Endpoint::parse(&format!("tcp://{addr}")).expect("parse");
        Connection::connect_endpoint(&endpoint, name).expect("tcp connect")
    }

    fn shot(&self) -> Image {
        let mut c = self.connect();
        c.get_mut().write_all(b"shot\n").unwrap();
        let mut header = String::new();
        c.read_line(&mut header).unwrap();
        let fields: Vec<u32> = header
            .trim_end()
            .strip_prefix("ok ")
            .expect("ok header")
            .split(' ')
            .map(|f| f.parse().unwrap())
            .collect();
        let (width, height, stride) = (fields[0], fields[1], fields[2]);
        let mut data = vec![0u8; (stride * height) as usize];
        c.read_exact(&mut data).unwrap();
        Image {
            width,
            height,
            stride,
            data,
        }
    }

    /// Overwrite `server.conf` atomically, the way a settings app does.
    fn rewrite_config(&self, conf: &str) {
        let tmp = self.config_dir.join("server.conf.tmp");
        std::fs::write(&tmp, conf).expect("write temp");
        std::fs::rename(&tmp, &self.config_path).expect("rename into place");
    }

    /// Ask for a reload and wait for the counter to move: an inotify
    /// event is asynchronous, and a test that slept would be a test that
    /// is flaky on a loaded box.
    fn reload(&self) {
        let before = self.stat("config_reloads");
        let mut c = self.connect();
        c.get_mut().write_all(b"reload\n").unwrap();
        let mut line = String::new();
        c.read_line(&mut line).unwrap();
        assert_eq!(line, "ok\n");
        wait_for("the reload to complete", || {
            self.stat("config_reloads") > before
        });
    }

    fn settle(&self) {
        let mut stable = 0;
        let mut last = u64::MAX;
        wait_for("the server to go quiet", || {
            let lines = self.request_text("stats\n");
            let value = |key: &str| -> u64 {
                lines
                    .iter()
                    .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            let frames = value("frames");
            if value("flips_pending") == 0 && frames == last {
                stable += 1;
            } else {
                stable = 0;
            }
            last = frames;
            std::thread::sleep(Duration::from_millis(8));
            stable >= 3
        });
    }

    fn quit(mut self) {
        let mut c = self.connect();
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

impl Drop for Harness {
    fn drop(&mut self) {
        // A test that panicked still has to stop its server.
        let Some(t) = self.thread.take() else {
            return;
        };
        if let Ok(c) = UnixStream::connect(&self.control) {
            let mut c = BufReader::new(c);
            if c.get_mut().write_all(b"quit\n").is_ok() {
                let mut line = String::new();
                let _ = c.read_line(&mut line);
            }
        }
        let _ = t.join();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Drain a client's socket until `f` matches, or time out.
fn expect<T>(
    conn: &mut Connection,
    seen: &mut Vec<ServerMsg>,
    what: &str,
    f: impl Fn(&ServerMsg) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = seen.iter().rev().find_map(&f) {
            return found;
        }
        assert!(Instant::now() < deadline, "no {what}; got {seen:?}");
        conn.flush().unwrap();
        conn.poll(seen).unwrap_or_else(|e| panic!("{what}: {e}"));
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Open a window with one red rect and a label, and wait for its first
/// `Configure`.
fn make_window(conn: &mut Connection, seen: &mut Vec<ServerMsg>, id: u32, serial: u32) -> NodeId {
    let root = NodeId(id);
    let rect = NodeId(id + 1);
    let text = NodeId(id + 2);
    conn.tx()
        .create_window_with(root, "remote", WIN, Layer::Normal, 0)
        .create_rect(rect, root, Rect::new(0.0, 0.0, WIN.w, WIN.h))
        .fill_solid(rect, RED)
        .create_node(text, nitro_wire::types::NodeKind::Text, root)
        .bounds(text, Rect::new(8.0, 8.0, WIN.w - 16.0, 24.0))
        .set_text(text, "sans", 16.0, Color::WHITE, "remote")
        .commit(serial)
        .unwrap();
    conn.flush().unwrap();
    expect(conn, seen, "the first Configure", |m| match m {
        ServerMsg::Configure(c) if c.window == root => Some(()),
        _ => None,
    });
    root
}

#[test]
fn a_remote_client_gets_remote_wm_and_text_but_never_shell() {
    let h = Harness::start("caps", "remote.listen = 127.0.0.1:0\n");
    let addr = h.remote_listen().expect("a listener");
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");
    assert_ne!(
        addr, "127.0.0.1:0",
        "the kernel's port is reported, not `0`"
    );

    let conn = h.remote_client("remote-caps");
    assert!(conn.has_caps(caps::REMOTE), "caps = {:#x}", conn.caps());
    assert!(conn.has_caps(caps::WM));
    assert!(
        !conn.has_caps(caps::SHELL),
        "a TCP port is not a 0700 path: caps = {:#x}",
        conn.caps()
    );
    assert!(conn.is_remote());
    // `TEXT` follows the font scan, exactly as it does locally — a box
    // with no fonts is a legitimate configuration, so the assertion is
    // that the two agree rather than that the bit is set.
    let local = h.local_client("local-caps");
    assert_eq!(
        conn.has_caps(caps::TEXT),
        local.has_caps(caps::TEXT),
        "the TEXT bit does not depend on the transport"
    );
    assert!(!local.has_caps(caps::REMOTE), "a Unix client is not remote");

    wait_for("both clients to be counted", || {
        h.stat("remote_clients") == 1 && h.stat("clients") == 2
    });
    drop(local);
    drop(conn);
    h.quit();
}

#[test]
fn a_shell_op_from_a_remote_client_is_refused() {
    let h = Harness::start("shellop", "remote.listen = 127.0.0.1:0\n");
    let mut conn = h.remote_client("remote-shell");
    let mut seen = Vec::new();
    // `WindowList` needs `caps::SHELL`, which a remote client never has.
    conn.window_list().unwrap();
    conn.flush().unwrap();
    let msg = expect(&mut conn, &mut seen, "the refusal", |m| match m {
        ServerMsg::Error(e) => Some(e.msg.clone()),
        _ => None,
    });
    assert!(msg.contains("caps::SHELL"), "{msg}");
    h.quit();
}

#[test]
fn a_buffer_from_a_remote_client_is_refused_and_the_client_survives() {
    let h = Harness::start("buffer", "remote.listen = 127.0.0.1:0\n");
    let mut conn = h.remote_client("remote-buffer");
    let mut seen = Vec::new();
    let root = make_window(&mut conn, &mut seen, 1, 1);
    h.settle();
    assert_eq!(h.stat("windows"), 1);

    // The client-side refusal: the toolkit's `Image` path goes through
    // this, so this is the error an app sees.
    let fd =
        rustix::fs::memfd_create("remote-buffer", rustix::fs::MemfdFlags::CLOEXEC).expect("memfd");
    rustix::fs::ftruncate(&fd, 4096).expect("ftruncate");
    let err = conn
        .send(&ClientMsg::CreateBuffer(CreateBuffer {
            id: BufferId(1),
            width: 32,
            height: 32,
            stride: 128,
            format: format::XR24,
            size: 4096,
            fd,
        }))
        .expect_err("a buffer cannot go out over TCP");
    assert!(
        matches!(err, WireError::RemoteNoFds),
        "want RemoteNoFds, got {err:?}"
    );

    // The connection is still alive and still painting: the window is
    // there, and a further commit is honoured.
    conn.tx()
        .fill_solid(NodeId(2), Color::rgb(0x00, 0xFF, 0x00))
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    assert_eq!(h.stat("windows"), 1, "the client kept its window");
    assert_eq!(h.stat("remote_clients"), 1, "and its connection");
    let shot = h.shot();
    let px = pixel(&shot, OUT.0 / 2, OUT.1 / 2);
    assert_eq!(px, (0x00, 0xFF, 0x00), "the green fill was applied");
    let _ = root;
    h.quit();
}

/// A message that *declares* descriptors is the fatal case: the far end
/// would be waiting for bytes that can never arrive, so the connection
/// goes. A client built on `nitro-wire` cannot produce one (the sender
/// refuses first), so this drives the raw bytes.
#[test]
fn a_frame_declaring_descriptors_is_fatal_on_a_remote_link() {
    let h = Harness::start("declared", "remote.listen = 127.0.0.1:0\n");
    let addr = h.remote_listen().expect("a listener");
    let mut conn = h.remote_client("remote-liar");
    let mut seen = Vec::new();
    make_window(&mut conn, &mut seen, 1, 1);
    h.settle();

    // A second, raw connection: handshake by hand, then a header that
    // claims one descriptor.
    let mut raw = std::net::TcpStream::connect(&addr).expect("connect");
    raw.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // Hello { version, name }
    let mut hello = Vec::new();
    hello.extend_from_slice(&nitro_wire::VERSION.to_le_bytes());
    hello.extend_from_slice(&4u32.to_le_bytes());
    hello.extend_from_slice(b"liar");
    let mut frame = nitro_wire::header::encode(hello.len() as u32, 0x0001, 0).to_vec();
    frame.extend_from_slice(&hello);
    raw.write_all(&frame).expect("write Hello");
    // Read the Welcome, whatever it says; then lie about an fd.
    let mut buf = [0u8; 256];
    let _ = raw.read(&mut buf).expect("Welcome");
    let liar = nitro_wire::header::encode(0, 0x0001, 1);
    raw.write_all(&liar).expect("write the lying header");
    let _ = raw.flush();

    // The server drops that connection. Ours is untouched.
    wait_for("the liar to be disconnected", || {
        h.stat("remote_clients") == 1
    });
    h.settle();
    assert_eq!(h.stat("windows"), 1, "the honest client kept its window");
    h.quit();
}

/// The headline claim: the same app draws the same pixels, whichever
/// socket it came in on.
#[test]
fn a_window_over_tcp_renders_identically_to_one_over_the_unix_socket() {
    // Two servers, not one server twice. Window placement is a counter
    // (`WindowManager::next_placement`), so a second window on the same
    // server is deliberately offset from the first — comparing them would
    // be comparing two *placements*, not two transports. Two fresh
    // servers put the first window of each in the same place, which is
    // what makes the pixel comparison mean "the transport changed
    // nothing".
    let unix_side = Harness::start("pixels-unix", "");
    let tcp_side = Harness::start("pixels-tcp", "remote.listen = 127.0.0.1:0\n");

    let mut local = unix_side.local_client("pixels");
    let mut seen = Vec::new();
    make_window(&mut local, &mut seen, 1, 1);
    unix_side.settle();
    let over_unix = unix_side.shot();

    let mut remote = tcp_side.remote_client("pixels");
    let mut seen = Vec::new();
    make_window(&mut remote, &mut seen, 1, 1);
    tcp_side.settle();
    let over_tcp = tcp_side.shot();

    assert_eq!(over_tcp.width, over_unix.width);
    assert_eq!(over_tcp.height, over_unix.height);
    assert_eq!(over_tcp.stride, over_unix.stride);
    assert_eq!(
        over_tcp.data, over_unix.data,
        "a remote window is pixel-identical to a local one"
    );
    unix_side.quit();
    tcp_side.quit();
}

#[test]
fn the_listener_appears_and_disappears_on_reload() {
    // No key at all: no listener, which is the default and the state
    // every existing configuration is in.
    let h = Harness::start("reload", "keyboard.layout = us\n");
    assert_eq!(h.remote_listen(), None, "absent key means no listener");

    h.rewrite_config("keyboard.layout = us\nremote.listen = 127.0.0.1:0\n");
    h.reload();
    let addr = h.remote_listen().expect("the listener appeared");
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");

    // A client on it, with a window.
    let mut conn = h.remote_client("reload-client");
    let mut seen = Vec::new();
    make_window(&mut conn, &mut seen, 1, 1);
    h.settle();
    assert_eq!(h.stat("windows"), 1);

    // A reload that does not touch `remote.listen` must not rebind: the
    // port stays the same, and the client is undisturbed.
    h.rewrite_config("keyboard.layout = de\nremote.listen = 127.0.0.1:0\n");
    h.reload();
    assert_eq!(
        h.remote_listen().as_deref(),
        Some(addr.as_str()),
        "an unchanged remote.listen does not rebind"
    );

    // Remove the key: the listener goes, the connected client stays and
    // keeps painting.
    h.rewrite_config("keyboard.layout = de\n");
    h.reload();
    assert_eq!(h.remote_listen(), None, "the listener closed");
    assert_eq!(h.stat("remote_clients"), 1, "the client was not dropped");

    conn.tx()
        .fill_solid(NodeId(2), Color::rgb(0x00, 0x00, 0xFF))
        .commit(2)
        .unwrap();
    conn.flush().unwrap();
    h.settle();
    let shot = h.shot();
    assert_eq!(
        pixel(&shot, OUT.0 / 2, OUT.1 / 2),
        (0x00, 0x00, 0xFF),
        "the surviving remote client still paints"
    );

    // And nothing new can connect.
    let addr: std::net::SocketAddr = addr.parse().expect("literal");
    assert!(
        nitro_wire::io::Socket::connect_tcp(&[addr]).is_err(),
        "the port is closed"
    );
    h.quit();
}

#[test]
fn closing_the_tcp_socket_takes_the_window_with_it() {
    let h = Harness::start("close", "remote.listen = 127.0.0.1:0\n");
    let mut conn = h.remote_client("close-client");
    let mut seen = Vec::new();
    make_window(&mut conn, &mut seen, 1, 1);
    h.settle();
    assert_eq!(h.stat("windows"), 1);
    assert_eq!(h.stat("remote_clients"), 1);
    assert_eq!(h.stat("focused"), 1, "a new window takes focus");

    drop(conn);

    wait_for("the window to go with the connection", || {
        h.stat("windows") == 0 && h.stat("remote_clients") == 0
    });
    h.settle();
    assert_eq!(h.stat("focused"), 0, "focus moved off the dead window");
    assert_eq!(h.stat("clients"), 0);
    h.quit();
}

#[test]
fn a_bad_remote_listen_warns_and_leaves_the_server_running() {
    // A name rather than a literal: the parser refuses it, the server
    // logs it and comes up anyway. A typo in a config file must not cost
    // a person their desktop.
    let h = Harness::start("badaddr", "remote.listen = nitro.example.com:7700\n");
    assert_eq!(h.remote_listen(), None);
    // The Unix path is entirely unaffected.
    let mut conn = h.local_client("still-works");
    let mut seen = Vec::new();
    make_window(&mut conn, &mut seen, 1, 1);
    h.settle();
    assert_eq!(h.stat("windows"), 1);
    h.quit();
}

/// The `(r, g, b)` of one pixel of an `XRGB8888` screenshot.
fn pixel(img: &Image, x: u32, y: u32) -> (u8, u8, u8) {
    let i = (y * img.stride + x * 4) as usize;
    (img.data[i + 2], img.data[i + 1], img.data[i])
}
