//! Throwaway demo client for the nitro wire protocol — the M1 hardware
//! smoke test, grown a text section for M2.
//!
//! It opens one 640x400 `Normal`-layer window titled "hello" containing:
//!
//! * a background rect filling the window, painted with a vertical
//!   `Fill::Linear` gradient (vertical because the M1 rasterizer projects a
//!   gradient axis onto its dominant component — a diagonal axis would not
//!   be drawn as asked),
//! * three rounded rects with solid fills and a translucent white border,
//! * a 64x64 `Image` node showing a checkerboard with a radial alpha blob,
//!   uploaded as an `AR24` client buffer backed by a memfd,
//! * and, when the server reports the `TEXT` capability: a `sans` 16 px
//!   title, a button-like rounded rect with a centred `sans` 14 px "OK",
//!   and a `mono` 13 px line. The client sends *strings*, never glyphs —
//!   shaping, the font and the atlas are all the server's.
//!
//! Before it commits the scene it also fires one `MeasureText` for the
//! button's label and prints the answer, which is the protocol's one
//! request/response pair: a `TextMeasured` arrives without a commit ever
//! being sent, and a real toolkit uses exactly that to size a button around
//! its text.
//!
//! After the first commit it sits in a `poll(2)` on the connection fd — 0%
//! CPU while idle — and prints every `ServerMsg` it receives as one compact
//! line, flushed, so the hardware test can grep the output for
//! `Configure`, `Focus`, `Presented`, `PointerEnter`, `PointerMotion`,
//! `Key`, `TextMeasured` and `TextMetrics`. A `Configure` is answered by
//! resizing the background rect (and restretching its gradient) and
//! committing again.
//!
//! There is no signal handling on purpose: SIGINT keeps its default
//! disposition, so Ctrl-C simply kills the process and the kernel closes
//! the socket — the server cleans up the client's nodes and buffers. That
//! is also why no `signal-hook` self-pipe appears here. The process exits
//! with status 0 when the server closes the connection or the window.
//!
//! Run it on the test box against a running nitro server:
//!
//! ```text
//! ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/hello_client'
//! ```
//!
//! `NITRO_SOCKET` overrides the socket path; otherwise it is
//! `$XDG_RUNTIME_DIR/nitro/wire.sock`.

use std::io::Write as _;
use std::os::fd::{BorrowedFd, OwnedFd};

use nitro_core::{Color, IRect, Point, Rect, Size};
use nitro_wire::Error as WireError;
use nitro_wire::client::{Connection, Transaction};
use nitro_wire::msg::{CreateBuffer, Fill, MeasureText, ServerMsg, SetText};
use nitro_wire::types::{Align, BufferId, Layer, NodeId, NodeKind, caps, format};
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;

/// Root group of the one window we create.
const WINDOW: NodeId = NodeId(1);
/// Rect covering the whole window, holding the gradient.
const BACKGROUND: NodeId = NodeId(2);
/// The three rounded rects.
const CARDS: [NodeId; 3] = [NodeId(3), NodeId(4), NodeId(5)];
/// Image node sampling the memfd buffer.
const IMAGE: NodeId = NodeId(6);
/// Title label across the top.
const TITLE: NodeId = NodeId(7);
/// The button-like rect and the label centred in it.
const BUTTON: NodeId = NodeId(8);
/// The button's label.
const BUTTON_LABEL: NodeId = NodeId(9);
/// A monospaced line, to prove the `mono` alias resolves to a different
/// face than `sans`.
const MONO_LINE: NodeId = NodeId(10);
/// Id of the one client buffer.
const BUFFER: BufferId = BufferId(1);
/// Request id of the one `MeasureText` we send.
const MEASURE_REQUEST: u32 = 0x4f4b;

/// The button's bounds; its label is centred inside them.
const BUTTON_RECT: Rect = Rect::new(40.0, 300.0, 140.0, 44.0);
/// Font size of the title.
const TITLE_PX: f32 = 22.0;
/// Font size of the button label.
const LABEL_PX: f32 = 14.0;
/// Font size of the monospaced line.
const MONO_PX: f32 = 13.0;

/// Size we ask for; the server answers with a `Configure`.
const INITIAL: Size = Size::new(640.0, 400.0);
/// Edge length of the square demo buffer, in pixels.
const IMG_EDGE: u32 = 64;
/// Bytes per buffer row: `AR24` is 4 bytes per pixel.
const IMG_STRIDE: u32 = IMG_EDGE * 4;
/// Total buffer size in bytes.
const IMG_BYTES: u32 = IMG_STRIDE * IMG_EDGE;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = Connection::connect_default("hello")?;
    emit(&format!(
        "Connected server={:?} caps={:#x}",
        conn.server_name(),
        conn.caps()
    ))?;

    // Serials are the client's own counter; the server echoes them back in
    // `Presented`, which is how the test sees a frame reach the screen.
    let mut serial: u32 = 1;
    // One `MeasureText` before anything is committed. The answer comes back
    // on its own — no commit, no frame — which is the whole point of the
    // op: this is how a toolkit sizes a button around its label.
    let text_ok = conn.has_caps(caps::TEXT);
    if text_ok {
        conn.measure_text(MeasureText {
            request: MEASURE_REQUEST,
            size_px: LABEL_PX,
            weight: 600,
            italic: false,
            max_width: 0.0,
            wrap: false,
            family: "sans".to_owned(),
            text: "OK".to_owned(),
        })?;
    } else {
        emit("no TEXT capability: the server found no fonts, skipping labels")?;
    }
    build_scene(&mut conn, shared_buffer(&checker_blob())?, text_ok, serial)?;
    flush_blocking(&mut conn)?;

    let mut events = Vec::new();
    loop {
        // Block in the kernel rather than spinning on the non-blocking
        // socket: an idle client must cost nothing on the test box.
        wait(conn.as_fd(), PollFlags::IN)?;
        events.clear();
        if let Err(e) = conn.poll(&mut events) {
            return match e {
                // The server hung up: that is a clean end of the demo.
                WireError::Closed => {
                    emit("Disconnected")?;
                    Ok(())
                }
                other => Err(other.into()),
            };
        }

        // Print first, act second, so the log shows what the server said
        // even if answering it fails.
        let mut configured = None;
        let mut done = false;
        for msg in &events {
            emit(&describe(msg))?;
            match msg {
                // Only the last size matters if several arrive in one read.
                ServerMsg::Configure(c) => configured = Some(c.size),
                ServerMsg::Closed(_) => done = true,
                _ => {}
            }
        }

        if let Some(size) = configured {
            serial += 1;
            reconfigure(&mut conn, size, serial)?;
            flush_blocking(&mut conn)?;
        }
        if done {
            return Ok(());
        }
    }
}

/// Build the whole scene in one transaction and commit it.
fn build_scene(
    conn: &mut Connection,
    fd: OwnedFd,
    text: bool,
    serial: u32,
) -> Result<(), WireError> {
    let mut tx = conn
        .tx()
        .create_window(WINDOW, "hello", INITIAL, Layer::Normal)
        .create_rect(
            BACKGROUND,
            WINDOW,
            Rect::new(0.0, 0.0, INITIAL.w, INITIAL.h),
        )
        .fill(BACKGROUND, backdrop(INITIAL));
    // The builder is a by-value chain, so a loop just rebinds it.
    for (node, rect, color) in cards() {
        tx = tx
            .create_rect(node, WINDOW, rect)
            .fill_solid(node, color)
            .corners(node, 18.0)
            .border(node, 2.0, Color::rgba(255, 255, 255, 96));
    }
    tx = tx
        .create_image(IMAGE, WINDOW, Rect::new(452.0, 232.0, 128.0, 128.0))
        .create_buffer(CreateBuffer {
            id: BUFFER,
            width: IMG_EDGE,
            height: IMG_EDGE,
            stride: IMG_STRIDE,
            format: format::AR24,
            size: IMG_BYTES,
            fd,
        })
        .image(
            IMAGE,
            BUFFER,
            IRect::new(0, 0, IMG_EDGE.cast_signed(), IMG_EDGE.cast_signed()),
        );
    if text {
        tx = add_labels(tx);
    }
    tx.commit(serial)
}

/// The three text nodes: a title, a button with a centred label, and a
/// monospaced line.
///
/// Note what the client sends: a family name, a size and a string. No
/// glyph, no font file, no atlas — that is the whole reason this is a few
/// hundred bytes on the wire and works the same over SSH.
fn add_labels(tx: Transaction<'_>) -> Transaction<'_> {
    tx
        // Title: left-aligned across the top.
        .create_node(TITLE, NodeKind::Text, WINDOW)
        .bounds(TITLE, Rect::new(40.0, 8.0, 560.0, 30.0))
        .set_text(TITLE, "sans", TITLE_PX, Color::WHITE, "Hello, nitro")
        // A button-like rect …
        .create_rect(BUTTON, WINDOW, BUTTON_RECT)
        .fill_solid(BUTTON, Color::rgb(0x2b, 0x3b, 0x5c))
        .corners(BUTTON, 8.0)
        .border(BUTTON, 1.0, Color::rgba(255, 255, 255, 140))
        // … with its label centred in it. The label's bounds are the
        // button's, in window coordinates, and `Align::Center` is what puts
        // the block in the middle — the client never computes a glyph
        // position, because it does not know the metrics.
        .create_node(BUTTON_LABEL, NodeKind::Text, WINDOW)
        .bounds(
            BUTTON_LABEL,
            Rect::new(
                BUTTON_RECT.x,
                BUTTON_RECT.y + 13.0,
                BUTTON_RECT.w,
                LABEL_PX * 1.4,
            ),
        )
        .set_text_full(SetText {
            node: BUTTON_LABEL,
            size_px: LABEL_PX,
            weight: 600,
            italic: false,
            max_width: 0.0,
            wrap: false,
            align: Align::Center,
            color: Color::WHITE,
            family: "sans".to_owned(),
            text: "OK".to_owned(),
        })
        // A monospaced line, so a screenshot shows two different faces.
        .create_node(MONO_LINE, NodeKind::Text, WINDOW)
        .bounds(MONO_LINE, Rect::new(200.0, 310.0, 400.0, 20.0))
        .set_text(
            MONO_LINE,
            "mono",
            MONO_PX,
            Color::rgb(0x9f, 0xe8, 0xc0),
            "mono: 0123456789 il1 O0",
        )
}

/// Answer a `Configure`: restretch the background over the new size.
fn reconfigure(conn: &mut Connection, size: Size, serial: u32) -> Result<(), WireError> {
    conn.tx()
        .bounds(BACKGROUND, Rect::new(0.0, 0.0, size.w, size.h))
        // The gradient is expressed in the node's local space, so its end
        // point has to follow the new height or it would stop mid-window.
        .fill(BACKGROUND, backdrop(size))
        .commit(serial)
}

/// Vertical gradient covering a window of `size`.
fn backdrop(size: Size) -> Fill {
    Fill::Linear {
        start: Point::new(0.0, 0.0),
        end: Point::new(0.0, size.h),
        c0: Color::rgb(0x10, 0x18, 0x30),
        c1: Color::rgb(0x50, 0x28, 0x70),
    }
}

/// The three rounded rects: node, bounds, fill colour.
fn cards() -> [(NodeId, Rect, Color); 3] {
    [
        (
            CARDS[0],
            Rect::new(40.0, 48.0, 160.0, 120.0),
            Color::rgb(0x33, 0x88, 0xff),
        ),
        (
            CARDS[1],
            Rect::new(240.0, 96.0, 160.0, 120.0),
            Color::rgb(0xff, 0x9f, 0x43),
        ),
        (
            CARDS[2],
            Rect::new(440.0, 48.0, 160.0, 120.0),
            Color::rgba(0x2e, 0xd5, 0x73, 0xc0),
        ),
    ]
}

/// Paint the demo image: an 8-pixel checkerboard faded out radially.
///
/// `AR24` is little-endian `[b, g, r, a]` with straight alpha, so the
/// server compositing this over the gradient is visible proof that the
/// buffer path and the blend path both work.
fn checker_blob() -> Vec<u8> {
    /// Centre of a 64-pixel edge, in pixel-centre coordinates.
    const CENTRE: f32 = 31.5;
    /// Distance at which the blob is fully transparent.
    const RADIUS: f32 = 32.0;

    let mut px = vec![0u8; IMG_BYTES as usize];
    for y in 0..IMG_EDGE {
        for x in 0..IMG_EDGE {
            let dx = x as f32 - CENTRE;
            let dy = y as f32 - CENTRE;
            let fade = 1.0 - dx.hypot(dy) / RADIUS;
            let alpha = (fade.clamp(0.0, 1.0) * 255.0) as u8;
            let [red, green, blue] = if (x / 8 + y / 8) % 2 == 0 {
                [0xf5, 0xf5, 0xf5]
            } else {
                [0xe0, 0x3b, 0x8b]
            };
            let off = (y * IMG_STRIDE + x * 4) as usize;
            px[off..off + 4].copy_from_slice(&[blue, green, red, alpha]);
        }
    }
    px
}

/// Put `pixels` in a fresh memfd and hand back its descriptor.
///
/// Written with `pwrite` rather than `mmap`: mapping the buffer would need
/// `unsafe`, which this tree denies, and the demo writes its pixels exactly
/// once so a syscall per pass costs nothing.
fn shared_buffer(pixels: &[u8]) -> Result<OwnedFd, Errno> {
    let fd = rustix::fs::memfd_create("nitro-hello", rustix::fs::MemfdFlags::CLOEXEC)?;
    rustix::fs::ftruncate(&fd, pixels.len() as u64)?;
    let mut done = 0usize;
    while done < pixels.len() {
        // Short writes are legal; loop until the whole image is in.
        match rustix::io::pwrite(&fd, &pixels[done..], done as u64) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => done += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(fd)
}

/// Write every queued byte, waiting for writability as needed.
fn flush_blocking(conn: &mut Connection) -> Result<(), WireError> {
    while !conn.flush()? {
        wait(conn.as_fd(), PollFlags::OUT)?;
    }
    Ok(())
}

/// Block until `fd` is ready for `events`.
fn wait(fd: BorrowedFd<'_>, events: PollFlags) -> Result<(), Errno> {
    let mut fds = [PollFd::new(&fd, events)];
    loop {
        match rustix::event::poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            // SIGINT kills us outright (no handler is installed), so an
            // EINTR here came from something harmless: retry.
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
}

/// One compact line per message, for the hardware test to grep.
fn describe(msg: &ServerMsg) -> String {
    match msg {
        ServerMsg::Welcome(m) => {
            format!(
                "Welcome version={} caps={:#x} name={:?}",
                m.version, m.caps, m.name
            )
        }
        ServerMsg::Error(m) => {
            format!(
                "Error serial={} code={:?} msg={:?}",
                m.serial, m.code, m.msg
            )
        }
        ServerMsg::Presented(m) => format!(
            "Presented serial={} output={} seq={} t={}",
            m.serial, m.output, m.seq, m.time_ns
        ),
        ServerMsg::Configure(m) => format!(
            "Configure window={} size={}x{} pos={},{} scale={} output={}",
            m.window.raw(),
            m.size.w,
            m.size.h,
            m.position.x,
            m.position.y,
            m.scale,
            m.output
        ),
        ServerMsg::Frame(m) => format!(
            "Frame window={} deadline={} refresh={}",
            m.window.raw(),
            m.deadline_ns,
            m.refresh_ns
        ),
        ServerMsg::Focus(m) => format!("Focus window={} focused={}", m.window.raw(), m.focused),
        ServerMsg::Closed(m) => format!("Closed window={}", m.window.raw()),
        ServerMsg::TextMetrics(m) => format!(
            "TextMetrics node={} size={:.1}x{:.1} ascent={:.1} descent={:.1} lines={}",
            m.node.raw(),
            m.width,
            m.height,
            m.ascent,
            m.descent,
            m.line_count
        ),
        ServerMsg::TextMeasured(m) => format!(
            "TextMeasured request={:#x} size={:.1}x{:.1} ascent={:.1} lines={} cursors={}",
            m.request,
            m.width,
            m.height,
            m.ascent,
            m.line_count,
            m.cursor_x.len()
        ),
        // Split in two only to keep each function short enough for
        // clippy's `too_many_lines`.
        other => describe_input(other),
    }
}

/// One compact line per input message; see [`describe`].
fn describe_input(msg: &ServerMsg) -> String {
    match msg {
        ServerMsg::PointerEnter(m) => format!(
            "PointerEnter window={} node={} pos={:.1},{:.1} t={}",
            m.window.raw(),
            m.node.raw(),
            m.pos.x,
            m.pos.y,
            m.time_ns
        ),
        ServerMsg::PointerLeave(m) => {
            format!("PointerLeave window={} t={}", m.window.raw(), m.time_ns)
        }
        ServerMsg::PointerMotion(m) => format!(
            "PointerMotion window={} node={} pos={:.1},{:.1} t={}",
            m.window.raw(),
            m.node.raw(),
            m.pos.x,
            m.pos.y,
            m.time_ns
        ),
        ServerMsg::PointerButton(m) => format!(
            "PointerButton window={} button={:#x} state={:?} t={}",
            m.window.raw(),
            m.button,
            m.state,
            m.time_ns
        ),
        ServerMsg::PointerAxis(m) => format!(
            "PointerAxis window={} d={:.2},{:.2} source={:?} t={}",
            m.window.raw(),
            m.dx,
            m.dy,
            m.source,
            m.time_ns
        ),
        ServerMsg::Key(m) => format!(
            "Key window={} keycode={} state={:?} mods={:#x} keysym={:#x} utf8={:?} t={}",
            m.window.raw(),
            m.keycode,
            m.state,
            m.mods,
            m.keysym,
            m.utf8,
            m.time_ns
        ),
        ServerMsg::Touch(m) => format!(
            "Touch window={} id={} phase={:?} pos={:.1},{:.1} t={}",
            m.window.raw(),
            m.id,
            m.phase,
            m.pos.x,
            m.pos.y,
            m.time_ns
        ),
        // Unreachable: `describe` handles every other variant itself.
        other => other.name().to_owned(),
    }
}

/// Print one line and flush, so output survives a pipe (`ssh`, `grep`).
fn emit(line: &str) -> std::io::Result<()> {
    println!("{line}");
    std::io::stdout().flush()
}
