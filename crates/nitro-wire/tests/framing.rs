//! Framer tests: partial feeds, fd binding, limits.

use nitro_wire::codec::{FdQueue, Writer};
use nitro_wire::msg::{ClientMsg, Commit, CreateBuffer, SetBounds};
use nitro_wire::types::{BufferId, NodeId, format};
use nitro_wire::{DecodeError, Framer, MAX_PAYLOAD, MAX_PENDING_FDS, header};

mod common;
use common::{identity, memfd};

use nitro_core::Rect;

#[test]
fn byte_at_a_time_yields_the_same_messages() {
    let mut w = Writer::new();
    let msgs: Vec<ClientMsg> = vec![
        Commit { serial: 1 }.into(),
        SetBounds {
            id: NodeId(2),
            rect: Rect::new(1.0, 2.0, 3.0, 4.0),
        }
        .into(),
        Commit { serial: 3 }.into(),
    ];
    for m in &msgs {
        m.encode(&mut w).unwrap();
    }
    let (bytes, _) = w.take();

    let mut f = Framer::new();
    let mut got = Vec::new();
    for b in &bytes {
        f.feed(std::slice::from_ref(b), []);
        while let Some(frame) = f.next_frame().unwrap() {
            let mut q = FdQueue::from_vec(frame.fds);
            got.push(ClientMsg::decode(frame.op, &frame.payload, &mut q).unwrap());
        }
    }
    assert_eq!(got, msgs);
    assert!(f.next_frame().unwrap().is_none());
}

#[test]
fn a_split_header_is_not_a_frame_yet() {
    let mut w = Writer::new();
    ClientMsg::from(Commit { serial: 7 })
        .encode(&mut w)
        .unwrap();
    let (bytes, _) = w.take();

    let mut f = Framer::new();
    f.feed(&bytes[..3], []);
    assert!(f.next_frame().unwrap().is_none());
    f.feed(&bytes[3..7], []);
    assert!(f.next_frame().unwrap().is_none());
    f.feed(&bytes[7..], []);
    let frame = f.next_frame().unwrap().expect("complete now");
    assert_eq!(frame.op, Commit::OP);
}

#[test]
fn oversize_len_is_rejected_before_any_allocation() {
    let mut f = Framer::new();
    f.feed(&header::encode(MAX_PAYLOAD as u32 + 1, Commit::OP, 0), []);
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::TooLarge);
}

#[test]
fn a_reserved_flag_is_rejected() {
    let mut h = header::encode(0, Commit::OP, 0);
    h[7] = 0x80;
    let mut f = Framer::new();
    f.feed(&h, []);
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::BadFlags);
}

#[test]
fn fds_go_to_the_frame_whose_header_arrived_with_them() {
    // Stream: Commit, CreateBuffer(fd A), Commit, CreateBuffer(fd B).
    let first_fd = memfd("a", 4096);
    let second_fd = memfd("b", 8192);
    let (id_a, id_b) = (identity(&first_fd), identity(&second_fd));
    assert_ne!(id_a, id_b);

    let mut w = Writer::new();
    ClientMsg::from(Commit { serial: 1 })
        .encode(&mut w)
        .unwrap();
    ClientMsg::from(CreateBuffer {
        id: BufferId(1),
        width: 32,
        height: 32,
        stride: 128,
        format: format::XR24,
        size: 4096,
        fd: first_fd,
    })
    .encode(&mut w)
    .unwrap();
    ClientMsg::from(Commit { serial: 2 })
        .encode(&mut w)
        .unwrap();
    ClientMsg::from(CreateBuffer {
        id: BufferId(2),
        width: 64,
        height: 32,
        stride: 256,
        format: format::AR24,
        size: 8192,
        fd: second_fd,
    })
    .encode(&mut w)
    .unwrap();
    let (bytes, fds) = w.take();
    assert_eq!(fds.len(), 2);
    let mut fds = fds.into_iter();
    let (fd_a, fd_b) = (fds.next().unwrap(), fds.next().unwrap());

    // Feed as the socket layer would: each fd with the chunk starting at
    // its frame's header.
    let frames = split_frames(&bytes);
    let mut f = Framer::new();
    let mut out = Vec::new();
    for (i, chunk) in frames.iter().enumerate() {
        match i {
            1 => f.feed(chunk, [rustix::io::dup(&fd_a).unwrap()]),
            3 => f.feed(chunk, [rustix::io::dup(&fd_b).unwrap()]),
            _ => f.feed(chunk, []),
        }
        while let Some(frame) = f.next_frame().unwrap() {
            let mut q = FdQueue::from_vec(frame.fds);
            out.push(ClientMsg::decode(frame.op, &frame.payload, &mut q).unwrap());
        }
    }
    assert_eq!(out.len(), 4);
    match (&out[1], &out[3]) {
        (ClientMsg::CreateBuffer(first), ClientMsg::CreateBuffer(second)) => {
            assert_eq!(identity(&first.fd), id_a, "first buffer got fd A");
            assert_eq!(identity(&second.fd), id_b, "second buffer got fd B");
        }
        _ => panic!("expected two CreateBuffer messages"),
    }
}

#[test]
fn fds_survive_a_chunk_that_carries_several_frames() {
    // One recvmsg delivering `Commit + CreateBuffer` together with the fd:
    // the fd belongs to the CreateBuffer, not to the Commit.
    let fd = memfd("shared-chunk", 4096);
    let id = identity(&fd);
    let mut w = Writer::new();
    ClientMsg::from(Commit { serial: 5 })
        .encode(&mut w)
        .unwrap();
    ClientMsg::from(CreateBuffer {
        id: BufferId(3),
        width: 1,
        height: 1,
        stride: 4,
        format: format::XR24,
        size: 4,
        fd,
    })
    .encode(&mut w)
    .unwrap();
    let (bytes, fds) = w.take();

    let mut f = Framer::new();
    f.feed(&bytes, fds);
    let first = f.next_frame().unwrap().unwrap();
    assert_eq!(first.op, Commit::OP);
    assert!(first.fds.is_empty(), "the Commit must not steal the fd");
    let second = f.next_frame().unwrap().unwrap();
    assert_eq!(second.fds.len(), 1);
    assert_eq!(identity(&second.fds[0]), id);
}

#[test]
fn a_frame_declaring_an_fd_that_never_arrived_is_an_error() {
    let mut w = Writer::new();
    ClientMsg::from(CreateBuffer {
        id: BufferId(1),
        width: 1,
        height: 1,
        stride: 4,
        format: format::XR24,
        size: 4,
        fd: memfd("dropped", 4),
    })
    .encode(&mut w)
    .unwrap();
    let (bytes, _fds) = w.take(); // fds deliberately dropped
    let mut f = Framer::new();
    f.feed(&bytes, []);
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::MissingFd);
}

#[test]
fn an_empty_payload_frames_cleanly() {
    // No v1 message is zero-length, but the framer must handle it.
    let mut f = Framer::new();
    f.feed(&header::encode(0, 0x0999, 0), []);
    let frame = f.next_frame().unwrap().unwrap();
    assert_eq!(frame.op, 0x0999);
    assert!(frame.payload.is_empty());
}

/// Split a well-formed stream into its frames.
fn split_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut off = 0;
    while off < bytes.len() {
        let h = header::decode(&bytes[off..]).unwrap();
        let end = off + header::SIZE + h.len as usize;
        out.push(bytes[off..end].to_vec());
        off = end;
    }
    out
}

#[test]
fn unclaimed_fds_cannot_accumulate_without_bound() {
    // The DoS: a peer attaches a descriptor to every `sendmsg` but never
    // declares one in a header. The frames decode fine, so nothing ever
    // errors — without a cap the receiver parks one open fd per call and
    // eventually hits EMFILE, taking every other client down with it.
    let mut f = Framer::new();
    let mut framed = 0;
    let mut err = None;

    for i in 0..(MAX_PENDING_FDS * 2) {
        let mut w = Writer::new();
        ClientMsg::from(Commit { serial: i as u32 })
            .encode(&mut w)
            .unwrap();
        let (bytes, _) = w.take();
        // One unclaimed fd per chunk, exactly as a hostile peer would.
        f.feed(&bytes, [memfd("flood", 64)]);
        match f.next_frame() {
            Ok(Some(_)) => framed += 1,
            Ok(None) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
        assert!(
            f.pending_fds() <= MAX_PENDING_FDS,
            "pending fds grew past the cap at iteration {i}"
        );
    }

    assert_eq!(
        err,
        Some(DecodeError::UnexpectedFd),
        "the flood must be rejected, not absorbed (framed {framed} frames)"
    );
    // And the error is latched: the connection is finished.
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::UnexpectedFd);
}

#[test]
fn a_legitimate_burst_of_fds_is_not_mistaken_for_a_flood() {
    // Several fd-carrying frames in one chunk is normal and must pass.
    let mut w = Writer::new();
    let count = 8;
    let mut ids = Vec::new();
    for i in 0..count {
        let fd = memfd("burst", u64::from(i + 1) * 4096);
        ids.push(identity(&fd));
        ClientMsg::from(CreateBuffer {
            id: BufferId(i + 1),
            width: 1,
            height: 1,
            stride: 4,
            format: format::XR24,
            size: (i + 1) * 4096,
            fd,
        })
        .encode(&mut w)
        .unwrap();
    }
    let (bytes, fds) = w.take();
    assert_eq!(fds.len() as u32, count);

    let mut f = Framer::new();
    f.feed(&bytes, fds);
    for (i, want) in ids.iter().enumerate() {
        let frame = f.next_frame().unwrap().expect("frame");
        let mut q = FdQueue::from_vec(frame.fds);
        let msg = ClientMsg::decode(frame.op, &frame.payload, &mut q).unwrap();
        let ClientMsg::CreateBuffer(buf) = msg else {
            panic!("expected CreateBuffer");
        };
        assert_eq!(identity(&buf.fd), *want, "buffer {i} kept its own fd");
    }
}

#[test]
fn the_poison_error_is_the_original_one() {
    // A latched framer must report what actually went wrong, not a
    // generic `Truncated`, or the server logs the wrong cause.
    let mut f = Framer::new();
    let mut h = header::encode(0, Commit::OP, 0);
    h[7] = 0x40; // reserved flag
    f.feed(&h, []);
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::BadFlags);
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::BadFlags);
    assert_eq!(f.next_frame().unwrap_err(), DecodeError::BadFlags);
}
