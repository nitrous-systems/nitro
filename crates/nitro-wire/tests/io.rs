//! Socket-level tests: `SCM_RIGHTS` round-trip over a `socketpair`, and
//! the partial-write path with a deliberately tiny send buffer.

use nitro_core::Rect;
use nitro_wire::codec::{FdQueue, Writer};
use nitro_wire::io::{Socket, pair};
use nitro_wire::msg::{BufferDamage, ClientMsg, Commit, CreateBuffer, SetBounds};
use nitro_wire::types::{BufferId, NodeId, format};
use nitro_wire::{Framer, MAX_PAYLOAD};

mod common;
use common::{identity, memfd};

use nitro_core::IRect;

/// Write everything in `w` to `a`, reading from `b` whenever the socket
/// fills, until the writer is empty; returns every message that arrived.
fn pump(a: &mut Socket, b: &mut Socket, w: &mut Writer) -> Vec<ClientMsg> {
    let mut framer = Framer::new();
    let mut out = Vec::new();
    let mut rounds = 0;
    loop {
        let done = a.send_all(w).expect("send");
        // Drain whatever is readable.
        while b.recv_into(&mut framer).expect("recv").is_some() {}
        while let Some(frame) = framer.next_frame().expect("frame") {
            let mut q = FdQueue::from_vec(frame.fds);
            out.push(ClientMsg::decode(frame.op, &frame.payload, &mut q).expect("decode"));
        }
        if done {
            // One last read pass for anything still in flight.
            while b.recv_into(&mut framer).expect("recv").is_some() {}
            while let Some(frame) = framer.next_frame().expect("frame") {
                let mut q = FdQueue::from_vec(frame.fds);
                out.push(ClientMsg::decode(frame.op, &frame.payload, &mut q).expect("decode"));
            }
            return out;
        }
        rounds += 1;
        assert!(rounds < 100_000, "socket made no progress");
    }
}

#[test]
fn a_memfd_survives_the_round_trip() {
    let (mut a, mut b) = pair().expect("socketpair");
    let fd = memfd("io-round-trip", 16384);
    let want = identity(&fd);

    let mut w = Writer::new();
    ClientMsg::from(CreateBuffer {
        id: BufferId(1),
        width: 64,
        height: 64,
        stride: 256,
        format: format::XR24,
        size: 16384,
        fd,
    })
    .encode(&mut w)
    .unwrap();

    let got = pump(&mut a, &mut b, &mut w);
    assert_eq!(got.len(), 1);
    let ClientMsg::CreateBuffer(buf) = &got[0] else {
        panic!("expected CreateBuffer");
    };
    assert_eq!(buf.width, 64);
    assert_eq!(buf.size, 16384);
    assert_eq!(
        identity(&buf.fd),
        want,
        "the received fd refers to the same file"
    );
}

#[test]
fn several_fds_land_on_their_own_messages() {
    let (mut a, mut b) = pair().expect("socketpair");
    let fds: Vec<_> = (0..4)
        .map(|i| memfd(&format!("multi-{i}"), 4096 * (i + 1)))
        .collect();
    let ids: Vec<_> = fds.iter().map(identity).collect();

    let mut w = Writer::new();
    for (i, fd) in fds.into_iter().enumerate() {
        // Interleave fd-carrying frames with plain ones.
        ClientMsg::from(Commit { serial: i as u32 })
            .encode(&mut w)
            .unwrap();
        ClientMsg::from(CreateBuffer {
            id: BufferId(i as u32 + 1),
            width: 1,
            height: 1,
            stride: 4,
            format: format::XR24,
            size: 4096 * (i as u32 + 1),
            fd,
        })
        .encode(&mut w)
        .unwrap();
    }

    let got = pump(&mut a, &mut b, &mut w);
    assert_eq!(got.len(), 8);
    let mut seen = 0;
    for msg in &got {
        if let ClientMsg::CreateBuffer(buf) = msg {
            assert_eq!(identity(&buf.fd), ids[seen], "buffer {seen} got its own fd");
            seen += 1;
        }
    }
    assert_eq!(seen, 4);
}

#[test]
fn a_big_message_survives_a_tiny_send_buffer() {
    let (mut a, mut b) = pair().expect("socketpair");
    // Force many short writes: the kernel doubles what we ask for and
    // enforces a floor, but this is far below the message size either way.
    rustix::net::sockopt::set_socket_send_buffer_size(a.as_fd(), 4096).expect("SO_SNDBUF");
    rustix::net::sockopt::set_socket_recv_buffer_size(b.as_fd(), 4096).expect("SO_RCVBUF");

    // ~2 MiB of damage rectangles in one message.
    let rects: Vec<IRect> = (0..128 * 1024)
        .map(|i| IRect::new(i, i + 1, i + 2, i + 3))
        .collect();
    let msg: ClientMsg = BufferDamage {
        id: BufferId(9),
        rects,
    }
    .into();
    let mut w = Writer::new();
    msg.encode(&mut w).unwrap();
    assert!(w.len() > 2 * 1024 * 1024, "message is big enough to matter");

    let got = pump(&mut a, &mut b, &mut w);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], msg);
}

#[test]
fn a_partial_write_leaves_the_rest_queued() {
    let (mut a, mut b) = pair().expect("socketpair");
    rustix::net::sockopt::set_socket_send_buffer_size(a.as_fd(), 4096).expect("SO_SNDBUF");

    let mut w = Writer::new();
    for i in 0..20_000u32 {
        ClientMsg::from(SetBounds {
            id: NodeId(i + 1),
            rect: Rect::new(i as f32, 0.0, 1.0, 1.0),
        })
        .encode(&mut w)
        .unwrap();
    }
    let total = w.len();

    // The first send_all must hit EAGAIN and report "not done".
    let done = a.send_all(&mut w).expect("send");
    assert!(!done, "the socket should have filled up");
    assert!(w.len() < total, "some bytes went out");
    assert!(!w.is_empty(), "the rest stayed queued");

    // Draining the reader and flushing repeatedly finishes it.
    let got = pump(&mut a, &mut b, &mut w);
    assert_eq!(got.len(), 20_000);
    assert!(w.is_empty());
}

#[test]
fn a_closed_peer_is_reported() {
    let (mut a, b) = pair().expect("socketpair");
    drop(b);
    let mut framer = Framer::new();
    let err = a.recv_into(&mut framer).expect_err("peer is gone");
    assert!(matches!(err, nitro_wire::Error::Closed), "got {err:?}");
}

#[test]
fn an_oversize_message_is_refused_without_touching_the_buffer() {
    let mut w = Writer::new();
    ClientMsg::from(Commit { serial: 1 })
        .encode(&mut w)
        .unwrap();
    let before = w.len();
    // A vec that encodes past MAX_PAYLOAD.
    let rects: Vec<IRect> = vec![IRect::new(0, 0, 1, 1); MAX_PAYLOAD / 16 + 2];
    let err = ClientMsg::from(BufferDamage {
        id: BufferId(1),
        rects,
    })
    .encode(&mut w)
    .expect_err("too large");
    assert_eq!(err, nitro_wire::EncodeError::TooLarge);
    assert_eq!(w.len(), before, "the buffer was rolled back");
}

#[test]
fn fds_reach_the_right_messages_under_write_pressure() {
    // The hard case: many fd-carrying frames interleaved with big fd-less
    // ones, over a socket too small to take them in one write. Every
    // `sendmsg` therefore stops somewhere arbitrary — often mid-header —
    // and each buffer must still arrive with its own descriptor.
    const N: u32 = 64;

    let (mut a, mut b) = pair().expect("socketpair");
    rustix::net::sockopt::set_socket_send_buffer_size(a.as_fd(), 4096).expect("SO_SNDBUF");
    rustix::net::sockopt::set_socket_recv_buffer_size(b.as_fd(), 4096).expect("SO_RCVBUF");

    let mut want = Vec::new();
    let mut w = Writer::new();
    for i in 0..N {
        // A chunky fd-less message to push the writes out of alignment.
        let rects: Vec<IRect> = (0..200)
            .map(|r| IRect::new(r, i.cast_signed(), 1, 1))
            .collect();
        ClientMsg::from(BufferDamage {
            id: BufferId(i + 1),
            rects,
        })
        .encode(&mut w)
        .unwrap();

        // Each memfd gets a distinct size, so its identity is checkable.
        let fd = memfd(&format!("pressure-{i}"), u64::from(i + 1) * 4096);
        want.push(identity(&fd));
        ClientMsg::from(CreateBuffer {
            id: BufferId(i + 1),
            width: i + 1,
            height: 1,
            stride: (i + 1) * 4,
            format: format::XR24,
            size: (i + 1) * 4096,
            fd,
        })
        .encode(&mut w)
        .unwrap();
    }

    let got = pump(&mut a, &mut b, &mut w);
    assert_eq!(got.len() as u32, N * 2);

    let mut seen = 0usize;
    for msg in &got {
        if let ClientMsg::CreateBuffer(buf) = msg {
            assert_eq!(buf.id, BufferId(seen as u32 + 1));
            assert_eq!(
                identity(&buf.fd),
                want[seen],
                "buffer {seen} received the wrong descriptor"
            );
            seen += 1;
        }
    }
    assert_eq!(seen as u32, N);
}
