//! The same wire over **TCP**: handshake, a thousand frames, the refusal
//! of an fd-carrying frame, and the byte image that pins endianness.
//!
//! Everything binds `127.0.0.1:0` and asks the kernel which port it got,
//! so the suite never picks a number and never races another developer on
//! the same box.

use std::thread;

use nitro_core::{Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::codec::Writer;
use nitro_wire::io::Socket;
use nitro_wire::msg::{ClientMsg, Commit, CreateBuffer, SetBounds, SetIcon};
use nitro_wire::server::{ClientStream, TcpListener};
use nitro_wire::types::{BufferId, Layer, NodeId, caps, format};
use nitro_wire::{Endpoint, Error, header};

mod common;
use common::memfd;

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

/// Block until `fd` is writable, or five seconds pass.
fn wait_writable(fd: rustix::fd::BorrowedFd<'_>) {
    let mut fds = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::OUT,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: 5,
        tv_nsec: 0,
    };
    let _ = rustix::event::poll(&mut fds, Some(&timeout));
}

/// Block until the listener has one connection, with a bounded wait.
fn accept_blocking(listener: &TcpListener) -> ClientStream {
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
fn a_loopback_pair_handshakes_and_carries_a_thousand_frames() {
    const FRAMES: usize = 1000;

    let listener = TcpListener::bind("127.0.0.1:0".parse().expect("literal")).expect("bind");
    let addr = listener.addr();
    assert_ne!(addr.port(), 0, "the kernel resolved a real port");

    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        let hello = next_blocking(&mut client);
        assert!(matches!(hello, ClientMsg::Hello(_)), "{hello:?}");
        assert!(client.is_remote(), "a TCP client is a remote client");
        client
            .welcome("nitro-test", caps::WM | caps::TEXT | caps::REMOTE)
            .expect("welcome");
        assert!(client.flush().expect("flush"));
        let mut n = 0usize;
        let last;
        loop {
            match next_blocking(&mut client) {
                ClientMsg::SetBounds(_) => n += 1,
                ClientMsg::Commit(Commit { serial }) => {
                    last = serial;
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        (n, last)
    });

    let endpoint = Endpoint::parse(&format!("tcp://{addr}")).expect("parse");
    let mut conn = Connection::connect_endpoint(&endpoint, "tcp-client").expect("connect");
    assert_eq!(conn.server_name(), "nitro-test");
    assert!(conn.has_caps(caps::REMOTE));
    assert!(conn.is_remote());

    let mut tx = conn.tx();
    for i in 0..FRAMES {
        tx = tx.bounds(NodeId(1), Rect::new(i as f32, 0.0, 10.0, 10.0));
    }
    tx.commit(42).expect("build");
    // A thousand small frames will not fit one socket buffer; flush until
    // the kernel has taken them all, waiting for writability in between.
    while !conn.flush().expect("flush") {
        wait_writable(conn.as_fd());
    }

    let (n, serial) = server.join().expect("server thread");
    assert_eq!(n, FRAMES);
    assert_eq!(serial, 42);
}

#[test]
fn an_fd_frame_is_refused_before_a_byte_goes_out() {
    let listener = TcpListener::bind("127.0.0.1:0".parse().expect("literal")).expect("bind");
    let addr = listener.addr();

    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        let hello = next_blocking(&mut client);
        assert!(matches!(hello, ClientMsg::Hello(_)), "{hello:?}");
        client
            .welcome("nitro-test", caps::WM | caps::REMOTE)
            .expect("welcome");
        assert!(client.flush().expect("flush"));
        // Read whatever arrives for a moment, so the assertion below is
        // "nothing was sent" and not "nothing was read yet".
        let mut msgs = Vec::new();
        for _ in 0..20 {
            let _ = client.read();
            while let Some(m) = client.next_msg().expect("decode") {
                msgs.push(m);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        msgs
    });

    let endpoint = Endpoint::parse(&format!("tcp://{addr}")).expect("parse");
    let mut conn = Connection::connect_endpoint(&endpoint, "tcp-client").expect("connect");

    // A `SetBounds` first, so the stream is known to work, then the
    // buffer. The refusal must not be "the socket was broken anyway".
    conn.send(&ClientMsg::SetBounds(SetBounds {
        id: NodeId(1),
        rect: Rect::new(0.0, 0.0, 1.0, 1.0),
    }))
    .expect("queue");
    assert!(conn.flush().expect("flush"));

    let fd = memfd("tcp-refusal", 4096);
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
        .expect_err("an fd frame cannot go out over TCP");
    assert!(
        matches!(err, Error::RemoteNoFds),
        "want RemoteNoFds, got {err:?}"
    );
    // Nothing was queued, so the connection is exactly as it was: the
    // next flush is a no-op rather than a permanently wedged socket.
    assert!(!conn.has_pending_writes());
    assert!(conn.flush().expect("the connection still works"));

    let msgs = server.join().expect("server thread");
    assert_eq!(msgs.len(), 1, "only the fd-less message arrived: {msgs:#?}");
    assert!(matches!(msgs[0], ClientMsg::SetBounds(_)));
}

#[test]
fn a_unix_socket_still_carries_descriptors() {
    // The other half of the claim: the refusal is a property of the
    // *link*, not a new rule for everyone. A socketpair is local.
    let (mut a, _b) = nitro_wire::io::pair().expect("socketpair");
    assert!(!a.is_remote());
    let mut w = Writer::new();
    ClientMsg::from(CreateBuffer {
        id: BufferId(1),
        width: 32,
        height: 32,
        stride: 128,
        format: format::XR24,
        size: 4096,
        fd: memfd("local-ok", 4096),
    })
    .encode(&mut w)
    .expect("encode");
    assert!(a.send_all(&mut w).expect("a local socket takes fds"));
}

/// The byte image of one message, spelled out.
///
/// `docs/wire.md` claims a mixed x86-64/aarch64 pair is fine because every
/// field on the wire is an explicit little-endian type. That claim is
/// worth exactly as much as a test of it: this one compares a `SetBounds`
/// against a literal, so a field that ever becomes native-endian — or is
/// reordered, or padded — fails here on the machine that built it, rather
/// than on someone's ARM laptop talking to an x86 box.
#[test]
fn the_wire_image_is_little_endian_and_pinned() {
    let mut w = Writer::new();
    ClientMsg::from(SetBounds {
        id: NodeId(0x0102_0304),
        rect: Rect::new(1.0, 2.0, 3.0, 4.0),
    })
    .encode(&mut w)
    .expect("encode");

    #[rustfmt::skip]
    let want: &[u8] = &[
        // header: len = 20, op = SetBounds (0x0104), fds = 0, flags = 0
        20, 0, 0, 0,
        0x04, 0x01,
        0, 0,
        // NodeId(0x01020304), little-endian
        0x04, 0x03, 0x02, 0x01,
        // Rect: x, y, w, h as little-endian IEEE-754 f32
        0x00, 0x00, 0x80, 0x3f,
        0x00, 0x00, 0x00, 0x40,
        0x00, 0x00, 0x40, 0x40,
        0x00, 0x00, 0x80, 0x40,
    ];
    assert_eq!(w.bytes(), want, "the wire image moved");

    // And the op code in that literal really is `SetBounds`.
    let head = header::decode(w.bytes()).expect("header");
    assert_eq!(head.op, SetBounds::OP);
    assert_eq!(head.len, 20);
    assert_eq!(head.fds, 0);
}

#[test]
fn a_connect_to_nothing_fails_rather_than_hanging() {
    // Bind, learn the port, close: nothing is listening there now. The
    // client must come back with an error, not block forever.
    let addr = {
        let listener = TcpListener::bind("127.0.0.1:0".parse().expect("literal")).expect("bind");
        listener.addr()
    };
    let err = Socket::connect_tcp(&[addr]).expect_err("nothing is listening");
    assert!(matches!(err, Error::Io(_)), "{err:?}");
}

#[test]
fn a_window_over_tcp_looks_exactly_like_one_over_a_unix_socket() {
    // Same builder, same bytes: the transport is not allowed to change
    // what a transaction looks like.
    let mut w = Writer::new();
    ClientMsg::from(nitro_wire::msg::CreateWindow {
        id: NodeId(1),
        size: Size::new(400.0, 300.0),
        layer: Layer::Normal,
        flags: 0,
        title: "demo".to_owned(),
    })
    .encode(&mut w)
    .expect("encode");
    let over_unix = w.bytes().to_vec();

    let listener = TcpListener::bind("127.0.0.1:0".parse().expect("literal")).expect("bind");
    let addr = listener.addr();
    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        let hello = next_blocking(&mut client);
        assert!(matches!(hello, ClientMsg::Hello(_)), "{hello:?}");
        client.welcome("nitro-test", caps::REMOTE).expect("welcome");
        assert!(client.flush().expect("flush"));
        next_blocking(&mut client)
    });

    let endpoint = Endpoint::parse(&format!("tcp://{addr}")).expect("parse");
    let mut conn = Connection::connect_endpoint(&endpoint, "tcp-client").expect("connect");
    conn.tx()
        .create_window(NodeId(1), "demo", Size::new(400.0, 300.0), Layer::Normal)
        .finish()
        .expect("build");
    while !conn.flush().expect("flush") {}

    let got = server.join().expect("server thread");
    let ClientMsg::CreateWindow(cw) = got else {
        panic!("expected CreateWindow, got {got:?}");
    };
    let mut w2 = Writer::new();
    ClientMsg::from(cw).encode(&mut w2).expect("re-encode");
    assert_eq!(w2.bytes(), over_unix.as_slice());
}

#[test]
fn a_set_icon_crosses_tcp_unchanged() {
    // The point of naming icons rather than sending them: a `SetIcon` is
    // a string and nine bytes, so unlike `SetImage` it needs no
    // descriptor and works over a remote link with no special case at
    // all. This is that claim, over a real loopback socket rather than
    // through the codec.
    let listener = TcpListener::bind("127.0.0.1:0".parse().expect("literal")).expect("bind");
    let addr = listener.addr();

    let server = thread::spawn(move || {
        let mut client = accept_blocking(&listener);
        let hello = next_blocking(&mut client);
        assert!(matches!(hello, ClientMsg::Hello(_)), "{hello:?}");
        assert!(client.is_remote());
        client
            .welcome("nitro-test", caps::WM | caps::ICONS | caps::REMOTE)
            .expect("welcome");
        assert!(client.flush().expect("flush"));
        let mut msgs = Vec::new();
        loop {
            match next_blocking(&mut client) {
                ClientMsg::Commit(_) => break,
                other => msgs.push(other),
            }
        }
        msgs
    });

    let endpoint = Endpoint::parse(&format!("tcp://{addr}")).expect("parse");
    let mut conn = Connection::connect_endpoint(&endpoint, "tcp-client").expect("connect");
    // The server said it has icons; a remote client may use them, which
    // is exactly what it may *not* do with buffers.
    assert!(conn.has_caps(caps::ICONS));
    assert!(conn.has_caps(caps::REMOTE));

    let sent = SetIcon {
        node: NodeId(7),
        size: 24.0,
        role: 4,
        name: "volume-mute".to_owned(),
    };
    conn.send(&ClientMsg::SetIcon(sent.clone())).expect("queue");
    conn.send(&ClientMsg::Commit(Commit { serial: 1 }))
        .expect("queue");
    while !conn.flush().expect("flush") {
        wait_writable(conn.as_fd());
    }

    let msgs = server.join().expect("server thread");
    assert_eq!(msgs.len(), 1, "{msgs:#?}");
    assert_eq!(msgs[0], ClientMsg::SetIcon(sent));
}
