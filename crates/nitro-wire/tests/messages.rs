//! Message-level tests: exhaustive round-trip, every-prefix truncation and
//! a deterministic garbage fuzz.
//!
//! These three together are the crate's contract: *whatever* bytes arrive,
//! decoding either produces the message that was encoded or a
//! `DecodeError` — never a panic, never a silent misread.

use nitro_core::{Color, IRect, Point, Rect, Size, Transform};
use nitro_wire::codec::{FdQueue, Writer};
use nitro_wire::msg::{
    BufferDamage, ClientMsg, Closed, Commit, Configure, CreateBuffer, CreateNode, CreateWindow,
    DestroyBuffer, DestroyNode, Error as ErrorMsg, Fill, Focus, Frame, Hello, Key, PointerAxis,
    PointerButton, PointerEnter, PointerLeave, PointerMotion, Presented, Reparent, RequestFrame,
    ServerMsg, SetBorder, SetBounds, SetClip, SetCorners, SetFill, SetImage, SetOpacity,
    SetTransform, SetVisible, SetWindowTitle, Touch, Welcome,
};
use nitro_wire::types::{
    AxisSource, BufferId, ButtonState, ErrorCode, Layer, NodeId, NodeKind, TouchPhase, caps,
    format, window_flags,
};
use nitro_wire::{DecodeError, VERSION, header};

mod common;
use common::{Xorshift, memfd};

/// Every client message, with deliberately non-trivial field values: no
/// zeros, no defaults, distinct per field so a swapped pair shows up.
#[allow(clippy::too_many_lines)]
fn client_messages() -> Vec<ClientMsg> {
    vec![
        Hello {
            version: VERSION,
            name: "nitro-bar \u{1f600}".to_owned(),
        }
        .into(),
        Commit {
            serial: 0xdead_beef,
        }
        .into(),
        CreateWindow {
            id: NodeId(0x0102_0304),
            size: Size::new(1280.5, 720.25),
            layer: Layer::Overlay,
            flags: window_flags::UNDECORATED | window_flags::OPAQUE,
            title: "Tîtle — ünicode".to_owned(),
        }
        .into(),
        SetWindowTitle {
            window: NodeId(9),
            title: String::new(),
        }
        .into(),
        RequestFrame {
            window: NodeId(0xffff_fffe),
        }
        .into(),
        CreateNode {
            id: NodeId(11),
            kind: NodeKind::Image,
            parent: NodeId(12),
            before: NodeId(13),
        }
        .into(),
        DestroyNode { id: NodeId(14) }.into(),
        Reparent {
            id: NodeId(15),
            parent: NodeId(16),
            before: NodeId::NONE,
        }
        .into(),
        SetBounds {
            id: NodeId(17),
            rect: Rect::new(-1.5, 2.5, 300.75, 400.125),
        }
        .into(),
        SetTransform {
            id: NodeId(18),
            transform: Transform {
                a: 1.5,
                b: 2.5,
                c: 3.5,
                d: 4.5,
                e: 5.5,
                f: 6.5,
            },
        }
        .into(),
        SetVisible {
            id: NodeId(19),
            visible: true,
        }
        .into(),
        SetOpacity {
            id: NodeId(20),
            opacity: 0.375,
        }
        .into(),
        SetClip {
            id: NodeId(21),
            clip: false,
        }
        .into(),
        SetFill {
            id: NodeId(22),
            fill: Fill::None,
        }
        .into(),
        SetFill {
            id: NodeId(23),
            fill: Fill::Solid(Color::rgba(0x11, 0x22, 0x33, 0x44)),
        }
        .into(),
        SetFill {
            id: NodeId(24),
            fill: Fill::Linear {
                start: Point::new(1.0, 2.0),
                end: Point::new(3.0, 4.0),
                c0: Color::rgba(1, 2, 3, 4),
                c1: Color::rgba(5, 6, 7, 8),
            },
        }
        .into(),
        SetCorners {
            id: NodeId(25),
            radius: 8.5,
        }
        .into(),
        SetBorder {
            id: NodeId(26),
            width: 2.25,
            color: Color::rgba(9, 8, 7, 6),
        }
        .into(),
        CreateBuffer {
            id: BufferId(27),
            width: 640,
            height: 480,
            stride: 2560,
            format: format::AR24,
            size: 2560 * 480,
            fd: memfd("round-trip", 4096),
        }
        .into(),
        DestroyBuffer { id: BufferId(28) }.into(),
        BufferDamage {
            id: BufferId(29),
            rects: vec![
                IRect::new(0, 0, 16, 16),
                IRect::new(-4, -8, 100, 200),
                IRect::new(i32::MIN, i32::MAX, 1, 1),
            ],
        }
        .into(),
        BufferDamage {
            id: BufferId(30),
            rects: Vec::new(),
        }
        .into(),
        SetImage {
            id: NodeId(31),
            buffer: BufferId(32),
            src: IRect::new(1, 2, 3, 4),
        }
        .into(),
    ]
}

/// Every server message, same principle.
#[allow(clippy::too_many_lines)]
fn server_messages() -> Vec<ServerMsg> {
    vec![
        Welcome {
            version: VERSION,
            caps: caps::DIRECT_SCANOUT | caps::REMOTE,
            name: "nitro".to_owned(),
        }
        .into(),
        ErrorMsg {
            serial: 42,
            code: ErrorCode::BadBuffer,
            msg: "stride 100 < width 640 * 4".to_owned(),
        }
        .into(),
        Presented {
            serial: 7,
            output: 1,
            time_ns: 0x0123_4567_89ab_cdef,
            seq: 0xfedc_ba98_7654_3210,
        }
        .into(),
        Configure {
            window: NodeId(1),
            size: Size::new(1920.0, 1080.0),
            scale: 1.5,
            output: 2,
        }
        .into(),
        Frame {
            window: NodeId(2),
            deadline_ns: 1_234_567_890,
            refresh_ns: 16_666_666,
        }
        .into(),
        Focus {
            window: NodeId(3),
            focused: true,
        }
        .into(),
        Closed { window: NodeId(4) }.into(),
        PointerEnter {
            window: NodeId(5),
            node: NodeId(6),
            pos: Point::new(10.5, 20.25),
            time_ns: 99,
        }
        .into(),
        PointerLeave {
            window: NodeId(7),
            time_ns: 100,
        }
        .into(),
        PointerMotion {
            window: NodeId(8),
            node: NodeId::NONE,
            pos: Point::new(-1.0, -2.0),
            time_ns: 101,
        }
        .into(),
        PointerButton {
            window: NodeId(9),
            button: 0x110,
            state: ButtonState::Pressed,
            time_ns: 102,
        }
        .into(),
        PointerAxis {
            window: NodeId(10),
            dx: 0.0,
            dy: -53.5,
            source: AxisSource::Finger,
            time_ns: 103,
        }
        .into(),
        Key {
            window: NodeId(11),
            keycode: 30,
            state: ButtonState::Released,
            mods: 0b0101,
            keysym: 0x0061,
            time_ns: 104,
            utf8: "ä".to_owned(),
        }
        .into(),
        Key {
            window: NodeId(12),
            keycode: 1,
            state: ButtonState::Pressed,
            mods: 0,
            keysym: 0xff1b,
            time_ns: 105,
            utf8: String::new(),
        }
        .into(),
        Touch {
            window: NodeId(13),
            id: -7,
            phase: TouchPhase::Cancel,
            pos: Point::new(5.0, 6.0),
            time_ns: 106,
        }
        .into(),
    ]
}

/// Encode one message and split it into (op, payload, fds).
fn encode(msg_op: u16, w: &Writer) -> (u16, Vec<u8>, usize) {
    let bytes = w.bytes();
    let h = header::decode(bytes).expect("our own header decodes");
    assert_eq!(h.op, msg_op);
    assert_eq!(bytes.len(), header::SIZE + h.len as usize);
    (h.op, bytes[header::SIZE..].to_vec(), h.fds as usize)
}

#[test]
fn every_client_message_round_trips() {
    for msg in client_messages() {
        let mut w = Writer::new();
        msg.encode(&mut w).expect("encodes");
        let (op, payload, fd_count) = encode(msg.op(), &w);
        let (_, fds) = w.take();
        assert_eq!(fds.len(), fd_count, "{} fd count", msg.name());
        let mut q = FdQueue::from_vec(fds);
        let back = ClientMsg::decode(op, &payload, &mut q)
            .unwrap_or_else(|e| panic!("{} failed to decode: {e}", msg.name()));
        assert_eq!(back, msg, "{} round trip", msg.name());
    }
}

#[test]
fn every_server_message_round_trips() {
    for msg in server_messages() {
        let mut w = Writer::new();
        msg.encode(&mut w).expect("encodes");
        let (op, payload, fd_count) = encode(msg.op(), &w);
        assert_eq!(fd_count, 0, "no server message carries fds in v1");
        let mut q = FdQueue::new();
        let back = ServerMsg::decode(op, &payload, &mut q)
            .unwrap_or_else(|e| panic!("{} failed to decode: {e}", msg.name()));
        assert_eq!(back, msg, "{} round trip", msg.name());
    }
}

#[test]
fn every_proper_prefix_fails_to_decode() {
    for msg in client_messages() {
        let mut w = Writer::new();
        msg.encode(&mut w).expect("encodes");
        let (op, payload, _) = encode(msg.op(), &w);
        let (_, fds) = w.take();
        for cut in 0..payload.len() {
            // Give the fd back every time: a missing fd must not be what
            // makes the truncated decode fail.
            let mut q = FdQueue::from_vec(
                fds.iter()
                    .map(|fd| rustix::io::dup(fd).expect("dup for the truncation test"))
                    .collect(),
            );
            let r = ClientMsg::decode(op, &payload[..cut], &mut q);
            assert!(
                r.is_err(),
                "{} decoded from a {cut}-byte prefix of {} bytes",
                msg.name(),
                payload.len()
            );
        }
    }
    for msg in server_messages() {
        let mut w = Writer::new();
        msg.encode(&mut w).expect("encodes");
        let (op, payload, _) = encode(msg.op(), &w);
        for cut in 0..payload.len() {
            let mut q = FdQueue::new();
            assert!(
                ServerMsg::decode(op, &payload[..cut], &mut q).is_err(),
                "{} decoded from a {cut}-byte prefix",
                msg.name()
            );
        }
    }
}

#[test]
fn every_extra_byte_fails_to_decode() {
    for msg in client_messages() {
        let mut w = Writer::new();
        msg.encode(&mut w).expect("encodes");
        let (op, mut payload, _) = encode(msg.op(), &w);
        let (_, fds) = w.take();
        payload.push(0x5a);
        let mut q = FdQueue::from_vec(fds);
        assert_eq!(
            ClientMsg::decode(op, &payload, &mut q),
            Err(DecodeError::Trailing),
            "{} accepted a trailing byte",
            msg.name()
        );
    }
}

#[test]
fn an_unclaimed_fd_is_an_error() {
    let mut w = Writer::new();
    let msg: ClientMsg = Commit { serial: 1 }.into();
    msg.encode(&mut w).unwrap();
    let (op, payload, _) = encode(msg.op(), &w);
    let mut q = FdQueue::from_vec(vec![memfd("stray", 64)]);
    assert_eq!(
        ClientMsg::decode(op, &payload, &mut q),
        Err(DecodeError::UnexpectedFd)
    );
}

#[test]
fn a_missing_fd_is_an_error() {
    let mut w = Writer::new();
    let msg: ClientMsg = CreateBuffer {
        id: BufferId(1),
        width: 1,
        height: 1,
        stride: 4,
        format: format::XR24,
        size: 4,
        fd: memfd("missing", 4),
    }
    .into();
    msg.encode(&mut w).unwrap();
    let (op, payload, _) = encode(msg.op(), &w);
    let mut q = FdQueue::new();
    assert_eq!(
        ClientMsg::decode(op, &payload, &mut q),
        Err(DecodeError::MissingFd)
    );
}

#[test]
fn garbage_never_panics() {
    // 100k pseudo-random frames through both decoders. The PRNG is a
    // xorshift64* with a fixed seed: deterministic, zero dependencies.
    let mut rng = Xorshift::new(0x2545_f491_4f6c_dd1d);
    let client_ops: Vec<u16> = client_messages().iter().map(ClientMsg::op).collect();
    let server_ops: Vec<u16> = server_messages().iter().map(ServerMsg::op).collect();
    let mut payload = Vec::with_capacity(256);
    let mut decoded = 0u32;

    for i in 0..100_000u32 {
        payload.clear();
        let len = (rng.next_u32() % 96) as usize;
        for _ in 0..len {
            payload.push(rng.next_u32() as u8);
        }
        // Half the time use a real op so the field decoders get exercised
        // rather than bouncing off `UnknownOp`.
        let use_real = rng.next_u32() & 1 == 0;
        if i % 2 == 0 {
            let op = if use_real {
                client_ops[(rng.next_u32() as usize) % client_ops.len()]
            } else {
                rng.next_u32() as u16
            };
            let mut q = if rng.next_u32().is_multiple_of(4) {
                FdQueue::from_vec(vec![memfd("fuzz", 16)])
            } else {
                FdQueue::new()
            };
            if ClientMsg::decode(op, &payload, &mut q).is_ok() {
                decoded += 1;
            }
        } else {
            let op = if use_real {
                server_ops[(rng.next_u32() as usize) % server_ops.len()]
            } else {
                rng.next_u32() as u16
            };
            let mut q = FdQueue::new();
            if ServerMsg::decode(op, &payload, &mut q).is_ok() {
                decoded += 1;
            }
        }
    }
    // Some random payloads *are* valid messages (a `Commit` is four
    // arbitrary bytes); the point is that nothing panicked. Assert the
    // fuzz actually reached the decoders rather than erroring out early.
    assert!(decoded > 0, "the fuzz never produced a valid message");
}

#[test]
fn garbage_frames_never_panic_through_the_framer() {
    use nitro_wire::Framer;
    let mut rng = Xorshift::new(0x9e37_79b9_7f4a_7c15);
    for _ in 0..10_000 {
        let mut f = Framer::new();
        let len = (rng.next_u32() % 64) as usize;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            bytes.push(rng.next_u32() as u8);
        }
        f.feed(&bytes, []);
        while let Ok(Some(frame)) = f.next_frame() {
            let mut q = FdQueue::from_vec(frame.fds);
            let _ = ClientMsg::decode(frame.op, &frame.payload, &mut q);
        }
    }
}

#[test]
fn payload_layouts_are_frozen() {
    // Golden byte strings: if a field is reordered or resized, this breaks.
    // Update it only together with `docs/wire.md` and a version bump.
    let mut w = Writer::new();
    ClientMsg::from(SetBounds {
        id: NodeId(0x0102_0304),
        rect: Rect::new(1.0, 2.0, 3.0, 4.0),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=20, op=0x0104, fds=0, flags=0
            0x14, 0x00, 0x00, 0x00, 0x04, 0x01, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // id
            0x00, 0x00, 0x80, 0x3f, // 1.0
            0x00, 0x00, 0x00, 0x40, // 2.0
            0x00, 0x00, 0x40, 0x40, // 3.0
            0x00, 0x00, 0x80, 0x40, // 4.0
        ]
    );

    let mut w = Writer::new();
    ClientMsg::from(Hello {
        version: 1,
        name: "ab".to_owned(),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            0x0a, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // header
            0x01, 0x00, 0x00, 0x00, // version
            0x02, 0x00, 0x00, 0x00, // name length
            b'a', b'b',
        ]
    );
}
