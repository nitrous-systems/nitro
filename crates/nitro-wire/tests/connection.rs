//! End-to-end tests over a real Unix socket: handshake, a transaction,
//! events back, and the fatal paths (version mismatch, message before
//! `Hello`).
//!
//! The socket lives in the OS temp directory rather than the worktree
//! because `sun_path` is limited to 108 bytes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use nitro_core::{Color, Point, Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::io::Socket;
use nitro_wire::msg::{ClientMsg, Commit, Configure, Hello, Presented, ServerMsg};
use nitro_wire::server::{ClientStream, Listener};
use nitro_wire::types::{ErrorCode, Layer, NodeId, caps};
use nitro_wire::{Error, VERSION};

mod common;

/// A unique temp directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = rustix::process::getpid().as_raw_nonzero();
        let dir = std::env::temp_dir().join(format!("nitro-wire-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("wire.sock")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Block until `fd` is readable, or five seconds pass.
fn wait_readable(fd: rustix::fd::BorrowedFd<'_>) {
    let mut fds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    let _ = rustix::event::poll(&mut fds, Some(&timeout));
}

/// Block until the listener has one connection, with a bounded wait.
fn accept_blocking(listener: &Listener) -> ClientStream {
    for _ in 0..10_000 {
        if let Some(c) = listener.accept().expect("accept") {
            return c;
        }
        wait_readable(listener.as_fd());
    }
    panic!("no connection arrived");
}

/// Read until at least one message is available, with a bounded wait.
fn next_blocking(client: &mut ClientStream) -> ClientMsg {
    for _ in 0..10_000 {
        if let Some(m) = client.next_msg().expect("decode") {
            return m;
        }
        client.read().expect("read");
        if let Some(m) = client.next_msg().expect("decode") {
            return m;
        }
        wait_readable(client.as_fd());
    }
    panic!("no message arrived");
}

#[test]
fn handshake_and_a_transaction() {
    let dir = TempDir::new("hs");
    let path = dir.socket();
    let listener = Listener::bind_default_at(&path);

    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        let hello = next_blocking(&mut client);
        let ClientMsg::Hello(h) = hello else {
            panic!("expected Hello, got {hello:?}");
        };
        assert_eq!(h.version, VERSION);
        assert_eq!(h.name, "test-client");
        client
            .welcome("nitro-test", caps::DIRECT_SCANOUT)
            .expect("welcome");
        assert!(client.flush().expect("flush"));

        // Collect the transaction up to and including its Commit.
        let mut got = Vec::new();
        loop {
            let m = next_blocking(&mut client);
            let done = matches!(m, ClientMsg::Commit(_));
            got.push(m);
            if done {
                break;
            }
        }

        // Answer with a Configure and a Presented for that serial.
        client
            .send(&ServerMsg::Configure(Configure {
                window: NodeId(1),
                size: Size::new(400.0, 300.0),
                position: Point::new(10.0, 20.0),
                scale: 2.0,
                output: 0,
            }))
            .unwrap();
        client
            .send(&ServerMsg::Presented(Presented {
                serial: 7,
                output: 0,
                time_ns: 123_456,
                seq: 9,
            }))
            .unwrap();
        assert!(client.flush().expect("flush"));
        got
    });

    let mut conn = Connection::connect(&path, "test-client").expect("connect");
    assert_eq!(conn.server_name(), "nitro-test");
    assert!(conn.has_caps(caps::DIRECT_SCANOUT));
    assert!(!conn.has_caps(caps::TEXT));

    let win = NodeId(1);
    let boxy = NodeId(2);
    conn.tx()
        .create_window(win, "demo", Size::new(400.0, 300.0), Layer::Normal)
        .create_rect(boxy, win, Rect::new(10.0, 10.0, 100.0, 50.0))
        .fill_solid(boxy, Color::rgb(0x33, 0x88, 0xff))
        .corners(boxy, 4.0)
        .request_frame(win)
        .commit(7)
        .expect("build transaction");
    assert!(conn.flush().expect("flush"));

    let sent = server.join().expect("server thread");
    // create_window, create_rect (= CreateNode + SetBounds), fill_solid,
    // corners, request_frame, Commit.
    assert_eq!(sent.len(), 7, "{sent:#?}");
    assert!(matches!(sent[0], ClientMsg::CreateWindow(_)));
    assert!(matches!(
        sent.last(),
        Some(ClientMsg::Commit(Commit { serial: 7 }))
    ));

    // The two events come back. Wait on the fd rather than spinning: the
    // server thread has already exited, so everything it sent is in the
    // socket buffer, but "already sent" is not "already readable".
    let mut events = Vec::new();
    while events.len() < 2 {
        wait_readable(conn.as_fd());
        conn.poll(&mut events).expect("poll");
    }
    assert_eq!(events.len(), 2, "{events:#?}");
    assert!(matches!(events[0], ServerMsg::Configure(_)));
    assert!(matches!(
        events[1],
        ServerMsg::Presented(Presented { serial: 7, .. })
    ));
}

#[test]
fn a_version_mismatch_is_rejected_with_an_error() {
    let dir = TempDir::new("ver");
    let path = dir.socket();
    let listener = Listener::bind_default_at(&path);

    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        // Read until the `Hello` is complete; it must be rejected.
        loop {
            match client.next_msg() {
                Ok(Some(m)) => panic!("accepted {m:?} at version 999"),
                Ok(None) => {
                    wait_readable(client.as_fd());
                    if let Err(Error::Closed) = client.read() {
                        panic!("client closed before saying Hello");
                    }
                }
                Err(Error::Version { ours, theirs }) => {
                    assert_eq!(ours, VERSION);
                    assert_eq!(theirs, 999);
                    nitro_wire::server::reject_version(&mut client, theirs);
                    return;
                }
                Err(e) => panic!("expected a version error, got {e:?}"),
            }
        }
    });

    // A hand-rolled client speaking v999.
    let socket = Socket::connect(&path).expect("connect");
    let mut conn_out = nitro_wire::Writer::new();
    ClientMsg::from(Hello {
        version: 999,
        name: "from-the-future".to_owned(),
    })
    .encode(&mut conn_out)
    .unwrap();
    let mut socket = socket;
    while !socket.send_all(&mut conn_out).expect("send") {
        std::thread::yield_now();
    }

    let mut framer = nitro_wire::Framer::new();
    let mut msg = None;
    loop {
        if let Some(frame) = framer.next_frame().expect("frame") {
            let mut q = nitro_wire::FdQueue::from_vec(frame.fds);
            msg = Some(ServerMsg::decode(frame.op, &frame.payload, &mut q).expect("decode"));
            break;
        }
        match socket.recv_into(&mut framer) {
            Ok(Some(_)) => {}
            Ok(None) => wait_readable(socket.as_fd()),
            Err(Error::Closed) => break,
            Err(e) => panic!("recv: {e:?}"),
        }
    }
    server.join().expect("server thread");

    match msg {
        Some(ServerMsg::Error(e)) => assert_eq!(e.code, ErrorCode::Version),
        other => panic!("expected a version Error, got {other:?}"),
    }
}

#[test]
fn a_message_before_hello_is_fatal() {
    let dir = TempDir::new("prehello");
    let path = dir.socket();
    let listener = Listener::bind_default_at(&path);

    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        loop {
            match client.next_msg() {
                Ok(Some(m)) => panic!("accepted {m:?} before Hello"),
                Ok(None) => {
                    wait_readable(client.as_fd());
                    if let Err(e) = client.read() {
                        return e;
                    }
                }
                Err(e) => return e,
            }
        }
    });

    let mut socket = Socket::connect(&path).expect("connect");
    let mut out = nitro_wire::Writer::new();
    ClientMsg::from(Commit { serial: 1 })
        .encode(&mut out)
        .unwrap();
    while !socket.send_all(&mut out).expect("send") {
        std::thread::yield_now();
    }

    let err = server.join().expect("server thread");
    assert!(
        matches!(err, Error::Unexpected("message before Hello")),
        "got {err:?}"
    );
}

#[test]
fn the_listener_unlinks_its_socket() {
    let dir = TempDir::new("unlink");
    let path = dir.socket();
    {
        let _l = Listener::bind(&path).expect("bind");
        assert!(path.exists());
    }
    assert!(!path.exists(), "the socket file is removed on drop");

    // And binding twice in a row works (a stale file is unlinked).
    let first = Listener::bind(&path).expect("first bind");
    drop(first);
    let second = Listener::bind(&path).expect("second bind");
    drop(second);
}

/// `Listener::bind`, panicking — the tests always want the socket.
trait BindAt {
    fn bind_default_at(path: &std::path::Path) -> Listener;
}

impl BindAt for Listener {
    fn bind_default_at(path: &std::path::Path) -> Listener {
        Listener::bind(path).expect("bind")
    }
}
