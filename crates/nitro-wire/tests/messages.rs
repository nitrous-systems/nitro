//! Message-level tests: exhaustive round-trip, every-prefix truncation and
//! a deterministic garbage fuzz.
//!
//! These three together are the crate's contract: *whatever* bytes arrive,
//! decoding either produces the message that was encoded or a
//! `DecodeError` — never a panic, never a silent misread.

use nitro_core::{Color, IRect, Palette, Point, Rect, Role, Size, Transform};
use nitro_wire::codec::{FdQueue, Writer};
use nitro_wire::msg::{
    BindKey, BufferDamage, ClientMsg, CloseWindow, Closed, Commit, Configure, CreateBuffer,
    CreateNode, CreateWindow, DestroyBuffer, DestroyNode, Error as ErrorMsg, Fill, Focus,
    FocusWindow, Frame, GrabKeyboard, Hello, HotKey, Key, MeasureText, OutputGone, OutputInfo,
    Outputs, OutputsEnd, PointerAxis, PointerButton, PointerEnter, PointerLeave, PointerMotion,
    Presented, Reparent, RequestFrame, ServerMsg, SetAnchor, SetAppId, SetBorder, SetBounds,
    SetClip, SetCorners, SetExclusiveZone, SetFill, SetImage, SetLayer, SetOpacity, SetText,
    SetTransform, SetVisible, SetWindowLimits, SetWindowState, SetWindowStateFor, SetWindowTitle,
    TextMeasured, TextMetrics, Theme, Touch, UnbindKey, Welcome, WindowGone, WindowInfo,
    WindowList, WindowListEnd, WindowState,
};
use nitro_wire::types::{
    Align, AxisSource, BufferId, ButtonState, CursorPos, Edge, ErrorCode, Layer, NodeId, NodeKind,
    TouchPhase, WindowRef, WindowState as WindowStateValue, anchor, caps, format, mod_mask,
    window_flags,
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
            flags: window_flags::UNDECORATED | window_flags::NO_FOCUS,
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
        SetWindowState {
            window: NodeId(0x0043_2100),
            state: WindowStateValue::Fullscreen,
        }
        .into(),
        SetWindowState {
            window: NodeId(37),
            state: WindowStateValue::Minimized,
        }
        .into(),
        SetWindowLimits {
            window: NodeId(38),
            min: Size::new(320.5, 240.25),
            max: Size::new(1920.75, 1080.125),
        }
        .into(),
        SetAppId {
            window: NodeId(39),
            app_id: "org.nitro.calc".to_owned(),
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
        SetText {
            node: NodeId(33),
            size_px: 14.5,
            weight: 700,
            italic: true,
            max_width: 320.25,
            wrap: true,
            align: Align::Center,
            color: Color::rgba(0x21, 0x43, 0x65, 0x87),
            family: "sans".to_owned(),
            text: "Grüße, wörld \u{1f600}".to_owned(),
        }
        .into(),
        SetText {
            node: NodeId(34),
            size_px: 11.0,
            weight: 400,
            italic: false,
            max_width: 0.0,
            wrap: false,
            align: Align::Right,
            color: Color::BLACK,
            family: String::new(),
            text: String::new(),
        }
        .into(),
        MeasureText {
            request: 0x00c0_ffee,
            size_px: 9.75,
            weight: 300,
            italic: true,
            max_width: 64.5,
            wrap: false,
            family: "mono".to_owned(),
            text: "mesuré ✓".to_owned(),
        }
        .into(),
        MeasureText {
            request: 1,
            size_px: 16.0,
            weight: 400,
            italic: false,
            max_width: 0.0,
            wrap: true,
            family: String::new(),
            text: String::new(),
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
        // Shell ops (caps::SHELL).
        SetLayer {
            window: NodeId(40),
            layer: Layer::Top,
        }
        .into(),
        SetLayer {
            window: NodeId(41),
            layer: Layer::Background,
        }
        .into(),
        SetExclusiveZone {
            window: NodeId(42),
            edge: Edge::Top,
            px: 32,
        }
        .into(),
        SetExclusiveZone {
            window: NodeId(43),
            edge: Edge::Right,
            px: 0,
        }
        .into(),
        SetAnchor {
            window: NodeId(44),
            edges: anchor::TOP | anchor::LEFT | anchor::RIGHT,
            margin: 6,
        }
        .into(),
        SetAnchor {
            window: NodeId(45),
            edges: 0,
            margin: 0,
        }
        .into(),
        BindKey {
            id: 0x0bad_f00d,
            mods: mod_mask::SUPER | mod_mask::SHIFT,
            keysym: 0xff0d,
        }
        .into(),
        BindKey {
            id: 2,
            mods: mod_mask::SUPER,
            keysym: 0,
        }
        .into(),
        UnbindKey { id: 0x0bad_f00d }.into(),
        GrabKeyboard {
            window: NodeId(46),
            on: true,
        }
        .into(),
        GrabKeyboard {
            window: NodeId(47),
            on: false,
        }
        .into(),
        WindowList.into(),
        Outputs.into(),
        FocusWindow {
            window: WindowRef(0x00de_0001),
        }
        .into(),
        CloseWindow {
            window: WindowRef::NONE,
        }
        .into(),
        SetWindowStateFor {
            window: WindowRef(0x00de_0002),
            state: WindowStateValue::Minimized,
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
            position: Point::new(64.0, 48.0),

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
        WindowState {
            window: NodeId(0x0055_00aa),
            state: WindowStateValue::Maximized,
        }
        .into(),
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
        TextMetrics {
            node: NodeId(14),
            width: 123.5,
            height: 34.25,
            ascent: 12.75,
            descent: -3.5,
            line_count: 3,
        }
        .into(),
        TextMeasured {
            request: 0x00c0_ffee,
            width: 200.5,
            height: 18.25,
            ascent: 14.0,
            descent: 4.25,
            line_count: 1,
            cursor_x: vec![
                CursorPos::new(0, 0.0),
                CursorPos::new(1, 8.5),
                CursorPos::new(4, 21.75),
                CursorPos::new(u32::MAX, -1.5),
            ],
        }
        .into(),
        TextMeasured {
            request: 2,
            width: 0.0,
            height: 0.0,
            ascent: 0.0,
            descent: 0.0,
            line_count: 0,
            cursor_x: Vec::new(),
        }
        .into(),
        // Shell events (caps::SHELL).
        HotKey {
            id: 0x0bad_f00d,
            pressed: true,
            time_ns: 0x0123_4567_89ab_cdef,
        }
        .into(),
        HotKey {
            id: 2,
            pressed: false,
            time_ns: 1,
        }
        .into(),
        WindowInfo {
            window: WindowRef(0x00de_0001),
            state: WindowStateValue::Maximized,
            focused: true,
            output: 3,
            layer: Layer::Normal,
            app_id: "org.nitro.calc".to_owned(),
            title: "Calculator — ünicode".to_owned(),
        }
        .into(),
        WindowInfo {
            window: WindowRef(7),
            state: WindowStateValue::Normal,
            focused: false,
            output: u32::MAX,
            layer: Layer::Background,
            app_id: String::new(),
            title: String::new(),
        }
        .into(),
        WindowListEnd.into(),
        WindowGone {
            window: WindowRef(0x00de_0002),
        }
        .into(),
        OutputInfo {
            id: 2,
            w: 2560,
            h: 1440,
            scale: 1.5,
            x: -1920,
            y: 0,
            refresh_mhz: 59_951,
            name: "HDMI-A-1".to_owned(),
        }
        .into(),
        OutputInfo {
            id: 0,
            w: 0,
            h: 0,
            scale: 0.0,
            x: 0,
            y: 0,
            refresh_mhz: 0,
            name: String::new(),
        }
        .into(),
        OutputsEnd.into(),
        OutputGone { id: 9 }.into(),
        // The palette, at a serial and a table length that are both
        // deliberately *not* the built-in ones: a `Theme` that only ever
        // carried `Role::COUNT` colours would hide a decoder that
        // ignored the count.
        Theme {
            serial: 0x0102_0304,
            colors: vec![
                Color::rgba(0x11, 0x22, 0x33, 0x44),
                Color::rgb(1, 2, 3),
                Color::TRANSPARENT,
            ],
        }
        .into(),
        Theme {
            serial: 0,
            colors: Vec::new(),
        }
        .into(),
        Theme::from_palette(7, &Palette::dark()).into(),
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
#[allow(clippy::too_many_lines)]
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

    let mut w = Writer::new();
    ClientMsg::from(SetWindowState {
        window: NodeId(0x0102_0304),
        state: WindowStateValue::Fullscreen,
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=5, op=0x0013, fds=0, flags=0
            0x05, 0x00, 0x00, 0x00, 0x13, 0x00, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // window
            0x02, // state Fullscreen
        ]
    );

    let mut w = Writer::new();
    ClientMsg::from(SetWindowLimits {
        window: NodeId(0x0102_0304),
        min: Size::new(1.0, 2.0),
        max: Size::new(3.0, 4.0),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=20, op=0x0014, fds=0, flags=0
            0x14, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // window
            0x00, 0x00, 0x80, 0x3f, // min.w 1.0
            0x00, 0x00, 0x00, 0x40, // min.h 2.0
            0x00, 0x00, 0x40, 0x40, // max.w 3.0
            0x00, 0x00, 0x80, 0x40, // max.h 4.0
        ]
    );

    let mut w = Writer::new();
    ClientMsg::from(SetText {
        node: NodeId(0x0102_0304),
        size_px: 16.0,
        weight: 700,
        italic: true,
        max_width: 320.0,
        wrap: true,
        align: Align::Center,
        color: Color::rgba(0x11, 0x22, 0x33, 0x44),
        family: "sans".to_owned(),
        text: "hi".to_owned(),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(w.bytes(), &GOLDEN_SET_TEXT);

    let mut w = Writer::new();
    ClientMsg::from(MeasureText {
        request: 0x0a0b_0c0d,
        size_px: 16.0,
        weight: 400,
        italic: false,
        max_width: 0.0,
        wrap: true,
        family: "mono".to_owned(),
        text: "hi".to_owned(),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(w.bytes(), &GOLDEN_MEASURE_TEXT);

    let mut w = Writer::new();
    ServerMsg::from(TextMetrics {
        node: NodeId(0x0102_0304),
        width: 1.0,
        height: 2.0,
        ascent: 3.0,
        descent: 4.0,
        line_count: 5,
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(w.bytes(), &GOLDEN_TEXT_METRICS);

    let mut w = Writer::new();
    ServerMsg::from(TextMeasured {
        request: 0x0a0b_0c0d,
        width: 1.0,
        height: 2.0,
        ascent: 3.0,
        descent: 4.0,
        line_count: 5,
        cursor_x: vec![CursorPos::new(0, 0.0), CursorPos::new(2, 1.0)],
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(w.bytes(), &GOLDEN_TEXT_MEASURED);

    let mut w = Writer::new();
    ClientMsg::from(SetExclusiveZone {
        window: NodeId(0x0102_0304),
        edge: Edge::Bottom,
        px: 32,
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=9, op=0x0402, fds=0, flags=0
            0x09, 0x00, 0x00, 0x00, 0x02, 0x04, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // window
            0x01, // edge Bottom
            0x20, 0x00, 0x00, 0x00, // px 32
        ]
    );

    let mut w = Writer::new();
    ServerMsg::from(Theme {
        serial: 0x0102_0304,
        colors: vec![Color::rgba(0x11, 0x22, 0x33, 0x44), Color::rgb(1, 2, 3)],
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=16, op=0x8004, fds=0, flags=0
            0x10, 0x00, 0x00, 0x00, 0x04, 0x80, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // serial
            0x02, 0x00, 0x00, 0x00, // colour count
            0x11, 0x22, 0x33, 0x44, // r, g, b, a
            0x01, 0x02, 0x03, 0xff, //
        ]
    );

    let mut w = Writer::new();
    ClientMsg::from(BindKey {
        id: 0x0102_0304,
        mods: mod_mask::SUPER,
        keysym: 0xff0d,
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=12, op=0x0404, fds=0, flags=0
            0x0c, 0x00, 0x00, 0x00, 0x04, 0x04, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // id
            0x08, 0x00, 0x00, 0x00, // mods SUPER
            0x0d, 0xff, 0x00, 0x00, // keysym Return
        ]
    );

    // The three empty-bodied shell requests are a bare header.
    for (msg, op) in [
        (ClientMsg::from(WindowList), 0x0407u16),
        (ClientMsg::from(Outputs), 0x040b),
    ] {
        let mut w = Writer::new();
        msg.encode(&mut w).unwrap();
        assert_eq!(
            w.bytes(),
            &[
                0x00,
                0x00,
                0x00,
                0x00,
                (op & 0xff) as u8,
                (op >> 8) as u8,
                0x00,
                0x00
            ]
        );
    }

    let mut w = Writer::new();
    ServerMsg::from(HotKey {
        id: 0x0102_0304,
        pressed: true,
        time_ns: 0x0102_0304_0506_0708,
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=13, op=0x8401, fds=0, flags=0
            0x0d, 0x00, 0x00, 0x00, 0x01, 0x84, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // id
            0x01, // pressed
            0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // time_ns
        ]
    );

    let mut w = Writer::new();
    // `layer` was added in place in M3 (task #3697), moving this head from
    // 10 to 11 bytes. That is a layout change without a `VERSION` bump,
    // which the comment at the top of this test would normally forbid:
    // the exemption is argued in `docs/wire.md` under Versioning policy
    // (`WindowInfo` is `SHELL`-gated, so no unprivileged client can
    // observe the layout). Anything in the unprivileged blocks still
    // needs the bump.
    ServerMsg::from(WindowInfo {
        window: WindowRef(0x0102_0304),
        state: WindowStateValue::Maximized,
        focused: true,
        output: 2,
        layer: Layer::Top,
        app_id: "ab".to_owned(),
        title: "cd".to_owned(),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=23, op=0x8402, fds=0, flags=0
            0x17, 0x00, 0x00, 0x00, 0x02, 0x84, 0x00, 0x00, //
            0x04, 0x03, 0x02, 0x01, // window
            0x01, // state Maximized
            0x01, // focused
            0x02, 0x00, 0x00, 0x00, // output
            0x02, // layer Top
            0x02, 0x00, 0x00, 0x00, b'a', b'b', // app_id
            0x02, 0x00, 0x00, 0x00, b'c', b'd', // title
        ]
    );

    let mut w = Writer::new();
    ServerMsg::from(OutputInfo {
        id: 1,
        w: 1920,
        h: 1080,
        scale: 2.0,
        x: -1,
        y: 0,
        refresh_mhz: 60_000,
        name: "ab".to_owned(),
    })
    .encode(&mut w)
    .unwrap();
    assert_eq!(
        w.bytes(),
        &[
            // header: len=34, op=0x8405, fds=0, flags=0
            0x22, 0x00, 0x00, 0x00, 0x05, 0x84, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00, // id
            0x80, 0x07, 0x00, 0x00, // w 1920
            0x38, 0x04, 0x00, 0x00, // h 1080
            0x00, 0x00, 0x00, 0x40, // scale 2.0
            0xff, 0xff, 0xff, 0xff, // x -1
            0x00, 0x00, 0x00, 0x00, // y 0
            0x60, 0xea, 0x00, 0x00, // refresh_mhz 60000
            0x02, 0x00, 0x00, 0x00, b'a', b'b', // name
        ]
    );
}

#[test]
fn a_bad_edge_byte_is_rejected() {
    let mut w = Writer::new();
    ClientMsg::from(SetExclusiveZone {
        window: NodeId(1),
        edge: Edge::Top,
        px: 1,
    })
    .encode(&mut w)
    .unwrap();
    let mut bytes = w.bytes()[header::SIZE..].to_vec();
    bytes[4] = 4; // Top/Bottom/Left/Right only
    let mut q = FdQueue::new();
    assert_eq!(
        ClientMsg::decode(SetExclusiveZone::OP, &bytes, &mut q),
        Err(DecodeError::BadValue)
    );
}

#[test]
fn the_shell_ops_live_in_their_own_block() {
    // 0x_4xx client, 0x84xx server: a block of its own, so the unprivileged
    // protocol can keep growing without colliding with it.
    for op in [
        SetLayer::OP,
        SetExclusiveZone::OP,
        SetAnchor::OP,
        BindKey::OP,
        UnbindKey::OP,
        GrabKeyboard::OP,
        WindowList::OP,
        FocusWindow::OP,
        CloseWindow::OP,
        SetWindowStateFor::OP,
        Outputs::OP,
    ] {
        assert_eq!(op & 0xff00, 0x0400, "client shell op {op:#06x}");
        assert!(ClientMsg::is_op(op));
        assert!(!ServerMsg::is_op(op));
    }
    for op in [
        HotKey::OP,
        WindowInfo::OP,
        WindowListEnd::OP,
        WindowGone::OP,
        OutputInfo::OP,
        OutputsEnd::OP,
        OutputGone::OP,
    ] {
        assert_eq!(op & 0xff00, 0x8400, "server shell op {op:#06x}");
        assert!(ServerMsg::is_op(op));
        assert!(!ClientMsg::is_op(op));
    }
}

#[test]
fn a_bad_align_byte_is_rejected() {
    // A `SetText` payload whose `align` byte is 3 (Left/Center/Right only).
    let mut bytes = GOLDEN_SET_TEXT[header::SIZE..].to_vec();
    bytes[ALIGN_OFFSET] = 3;
    let mut q = FdQueue::new();
    assert_eq!(
        ClientMsg::decode(SetText::OP, &bytes, &mut q),
        Err(DecodeError::BadValue)
    );
}

#[test]
fn a_bad_bool_byte_is_rejected() {
    let mut bytes = GOLDEN_SET_TEXT[header::SIZE..].to_vec();
    bytes[ITALIC_OFFSET] = 2;
    let mut q = FdQueue::new();
    assert_eq!(
        ClientMsg::decode(SetText::OP, &bytes, &mut q),
        Err(DecodeError::BadValue)
    );

    let mut bytes = GOLDEN_SET_TEXT[header::SIZE..].to_vec();
    bytes[WRAP_OFFSET] = 0xff;
    let mut q = FdQueue::new();
    assert_eq!(
        ClientMsg::decode(SetText::OP, &bytes, &mut q),
        Err(DecodeError::BadValue)
    );
}

#[test]
fn a_hostile_cursor_count_is_truncated_not_allocated() {
    // `TextMeasured` head (24 bytes) then a count of 2^32-1 cursors with
    // no items behind it: `Reader::get_vec` validates the count against
    // the bytes actually available *before* reserving anything.
    let mut bytes = GOLDEN_TEXT_MEASURED[header::SIZE..GOLDEN_TEXT_MEASURED.len() - 16].to_vec();
    assert_eq!(bytes.len(), 28, "24-byte head plus the u32 count");
    bytes[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut q = FdQueue::new();
    assert_eq!(
        ServerMsg::decode(TextMeasured::OP, &bytes, &mut q),
        Err(DecodeError::Truncated)
    );

    // One item short of the declared count is truncated too.
    let mut bytes = GOLDEN_TEXT_MEASURED[header::SIZE..].to_vec();
    bytes[24..28].copy_from_slice(&3u32.to_le_bytes());
    let mut q = FdQueue::new();
    assert_eq!(
        ServerMsg::decode(TextMeasured::OP, &bytes, &mut q),
        Err(DecodeError::Truncated)
    );
}

#[test]
fn a_theme_message_carries_a_whole_palette() {
    for palette in [Palette::light(), Palette::dark()] {
        let msg = Theme::from_palette(3, &palette);
        assert_eq!(msg.colors.len(), Role::COUNT);
        assert_eq!(msg.palette(), palette);
        // And through the bytes, which is the path that matters.
        let mut w = Writer::new();
        ServerMsg::from(msg).encode(&mut w).unwrap();
        let (op, payload, _) = encode(nitro_wire::msg::Theme::OP, &w);
        let back = ServerMsg::decode(op, &payload, &mut FdQueue::new()).expect("decodes");
        let ServerMsg::Theme(back) = back else {
            panic!("decoded as something else");
        };
        assert_eq!(back.serial, 3);
        assert_eq!(back.palette(), palette);
    }
}

#[test]
fn a_short_or_long_theme_table_is_not_an_error() {
    // The forward-compatibility rule the `N`-on-the-wire layout exists
    // for: a peer one release behind sends fewer roles, one release
    // ahead sends more. Neither may kill the connection.
    let short = Theme {
        serial: 1,
        colors: vec![Color::rgb(1, 2, 3)],
    };
    let p = short.palette();
    assert_eq!(
        p.get(Role::from_index(0).expect("role 0")),
        Color::rgb(1, 2, 3)
    );
    // Everything it did not carry keeps the built-in default.
    assert_eq!(p.get(Role::Ansi15), Palette::default().get(Role::Ansi15));

    let mut colors = Palette::dark().colors().to_vec();
    colors.push(Color::rgb(9, 9, 9));
    colors.push(Color::rgb(8, 8, 8));
    let long = Theme { serial: 2, colors };
    assert_eq!(long.palette(), Palette::dark());
    // And the long form survives the wire too, rather than being
    // rejected for trailing bytes.
    let mut w = Writer::new();
    ServerMsg::from(long).encode(&mut w).unwrap();
    let (op, payload, _) = encode(nitro_wire::msg::Theme::OP, &w);
    assert!(ServerMsg::decode(op, &payload, &mut FdQueue::new()).is_ok());
}

/// Offset of `SetText::italic` inside its payload: after `node` + `size_px`
/// + `weight`.
const ITALIC_OFFSET: usize = 4 + 4 + 2;
/// Offset of `SetText::wrap`: after `italic` + `max_width`.
const WRAP_OFFSET: usize = ITALIC_OFFSET + 1 + 4;
/// Offset of `SetText::align`: right after `wrap`.
const ALIGN_OFFSET: usize = WRAP_OFFSET + 1;

/// Golden frame for a fixed [`SetText`]; see `payload_layouts_are_frozen`.
const GOLDEN_SET_TEXT: [u8; 8 + 21 + 8 + 6] = [
    // header: len=35, op=0x0206, fds=0, flags=0
    0x23, 0x00, 0x00, 0x00, 0x06, 0x02, 0x00, 0x00, //
    0x04, 0x03, 0x02, 0x01, // node
    0x00, 0x00, 0x80, 0x41, // size_px 16.0
    0xbc, 0x02, // weight 700
    0x01, // italic
    0x00, 0x00, 0xa0, 0x43, // max_width 320.0
    0x01, // wrap
    0x01, // align Center
    0x11, 0x22, 0x33, 0x44, // color
    0x04, 0x00, 0x00, 0x00, b's', b'a', b'n', b's', // family
    0x02, 0x00, 0x00, 0x00, b'h', b'i', // text
];

/// Golden frame for a fixed [`MeasureText`].
const GOLDEN_MEASURE_TEXT: [u8; 8 + 16 + 8 + 6] = [
    // header: len=30, op=0x0207, fds=0, flags=0
    0x1e, 0x00, 0x00, 0x00, 0x07, 0x02, 0x00, 0x00, //
    0x0d, 0x0c, 0x0b, 0x0a, // request
    0x00, 0x00, 0x80, 0x41, // size_px 16.0
    0x90, 0x01, // weight 400
    0x00, // italic
    0x00, 0x00, 0x00, 0x00, // max_width 0.0
    0x01, // wrap
    0x04, 0x00, 0x00, 0x00, b'm', b'o', b'n', b'o', // family
    0x02, 0x00, 0x00, 0x00, b'h', b'i', // text
];

/// Golden frame for a fixed [`TextMetrics`].
const GOLDEN_TEXT_METRICS: [u8; 8 + 24] = [
    // header: len=24, op=0x8301, fds=0, flags=0
    0x18, 0x00, 0x00, 0x00, 0x01, 0x83, 0x00, 0x00, //
    0x04, 0x03, 0x02, 0x01, // node
    0x00, 0x00, 0x80, 0x3f, // width 1.0
    0x00, 0x00, 0x00, 0x40, // height 2.0
    0x00, 0x00, 0x40, 0x40, // ascent 3.0
    0x00, 0x00, 0x80, 0x40, // descent 4.0
    0x05, 0x00, 0x00, 0x00, // line_count 5
];

/// Golden frame for a fixed [`TextMeasured`] with two cursor positions.
const GOLDEN_TEXT_MEASURED: [u8; 8 + 24 + 4 + 16] = [
    // header: len=44, op=0x8302, fds=0, flags=0
    0x2c, 0x00, 0x00, 0x00, 0x02, 0x83, 0x00, 0x00, //
    0x0d, 0x0c, 0x0b, 0x0a, // request
    0x00, 0x00, 0x80, 0x3f, // width 1.0
    0x00, 0x00, 0x00, 0x40, // height 2.0
    0x00, 0x00, 0x40, 0x40, // ascent 3.0
    0x00, 0x00, 0x80, 0x40, // descent 4.0
    0x05, 0x00, 0x00, 0x00, // line_count 5
    0x02, 0x00, 0x00, 0x00, // cursor_x count
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // {0, 0.0}
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3f, // {2, 1.0}
];
