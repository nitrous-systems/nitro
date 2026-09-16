//! A client that deliberately paints outside its own window, for
//! verifying the compositor's containment on real hardware (#3726).
//!
//! It exists because **the box cannot verify this claim with any of our
//! own apps.** Every application on the test box is a `nitro-ui` app,
//! and since #3725 the toolkit sets `SetClip` on its root widget's
//! group — so a spill is contained by the *client* before the server
//! ever has to contain it, and a working compositor clip looks exactly
//! like a broken one. Same shape as #3724's overhang, which leaned on
//! clients painting their background and was verified with the one kind
//! of client that cannot exhibit the bug: a desktop staffed entirely by
//! your own well-behaved apps does not exercise a guarantee about
//! badly-behaved ones.
//!
//! So this is raw `nitro-wire`, with no toolkit anywhere, and it asks
//! for the spill three ways:
//!
//! * a **blue** rect starting inside the window and running 300 px past
//!   its right edge — `nitro-settings`' Displays row, with the
//!   arithmetic removed,
//! * a **magenta** rect at a negative `y`, over the title bar the server
//!   drew,
//! * and an explicit `SetClip { clip: false }` on its own window,
//!   trying to switch the containment off.
//!
//! The third is the interesting one on a *new* server: it is refused
//! (`BadParent`), which is fatal, so the client prints the error and
//! exits non-zero. Pass `--no-unclip` to skip it and leave the window up
//! for a screenshot. Against an **old** server the same run is accepted
//! and the two rects land on the desktop, which is the before-arm this
//! verification needs.
//!
//! ```text
//! ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/overflow_client --no-unclip'
//! ```
//!
//! `NITRO_SOCKET` overrides the socket path; otherwise it is
//! `$XDG_RUNTIME_DIR/nitro/wire.sock`.

use std::io::Write as _;

use nitro_core::{Color, Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{Layer, NodeId};

/// Root group of the one window we create: the *content* group.
const WINDOW: NodeId = NodeId(1);
/// Background filling the window, so "inside" is visibly the client's.
const BACKGROUND: NodeId = NodeId(2);
/// Runs 300 px past the window's right edge.
const SPILL_RIGHT: NodeId = NodeId(3);
/// Sits above the window's top edge, over the server's title bar.
const SPILL_UP: NodeId = NodeId(4);

/// The window's content size. 560x400 matches the settings dialog the
/// original defect was reported against.
const SIZE: Size = Size::new(560.0, 400.0);
/// The title bar's height in logical units (`wm::TITLE_H`), hard-coded
/// because a client is not told the frame's insets.
const TITLE_H: f32 = 28.0;

const GREEN: Color = Color::rgb(0x20, 0x80, 0x40);
const BLUE: Color = Color::rgb(0x00, 0x60, 0xFF);
const MAGENTA: Color = Color::rgb(0xFF, 0x00, 0xFF);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let unclip = !std::env::args().any(|a| a == "--no-unclip");
    let path = match std::env::var_os("NITRO_SOCKET") {
        Some(p) => std::path::PathBuf::from(p),
        None => std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?)
            .join("nitro")
            .join("wire.sock"),
    };
    let mut conn = Connection::connect(&path, "overflow")?;
    emit(&format!("connected: caps={:#x}", conn.caps()))?;

    conn.tx()
        .create_window(WINDOW, "overflow", SIZE, Layer::Normal)
        .create_rect(BACKGROUND, WINDOW, Rect::new(0.0, 0.0, SIZE.w, SIZE.h))
        .fill_solid(BACKGROUND, GREEN)
        // Starts 20 px inside the right edge and runs 300 px past it, so
        // a correct clip is visible as a *cut*: 20 px of blue survive.
        .create_rect(
            SPILL_RIGHT,
            WINDOW,
            Rect::new(SIZE.w - 20.0, 40.0, 300.0, 40.0),
        )
        .fill_solid(SPILL_RIGHT, BLUE)
        // And one over the title bar the server drew above us.
        .create_rect(SPILL_UP, WINDOW, Rect::new(20.0, -TITLE_H, 120.0, TITLE_H))
        .fill_solid(SPILL_UP, MAGENTA)
        .commit(1)?;
    conn.flush()?;
    emit("committed: background + 2 overflowing rects")?;

    if unclip {
        // The request a client would make to opt out of being contained.
        // Refused since #3726, and fatal; `--no-unclip` skips it.
        conn.tx().clip(WINDOW, false).commit(2)?;
        conn.flush()?;
        emit("sent SetClip{clip:false} on our own window")?;
    }

    // Report what the server says, so a run is self-describing: the
    // `Configure` gives the content rectangle every pixel claim is
    // measured against, an `Error` is the refusal, and a
    // `PointerButton` is a click that *reached us* — which is the hit
    // test's half of the containment claim, so it is printed for the
    // whole life of the process rather than for an opening window.
    //
    // The first cut printed for three seconds and then went quiet in an
    // idle loop that called `seen.clear()`, so a click injected later
    // produced no line at all. That reports "no click arrived" for a
    // click that did, which is indistinguishable from the clip working
    // — and it was caught only by the control arm (a click *inside* the
    // window, which must arrive) reporting zero too. A silent client is
    // not evidence of a silent compositor.
    let mut seen = Vec::new();
    let mut clicks = 0usize;
    loop {
        conn.flush()?;
        if conn.poll(&mut seen).is_err() {
            emit("the server closed the connection")?;
            return Ok(());
        }
        for m in seen.drain(..) {
            match m {
                ServerMsg::Configure(c) => emit(&format!(
                    "Configure pos={:.0},{:.0} size={:.0}x{:.0} \
                     -> spill_right reaches x={:.0}, {:.0} px past the edge",
                    c.position.x,
                    c.position.y,
                    c.size.w,
                    c.size.h,
                    // From the size the server actually gave us, not the
                    // size we asked for: a line that mixes the two would
                    // quietly describe a different window if the server
                    // ever configured us smaller.
                    c.position.x + c.size.w + 280.0,
                    280.0,
                ))?,
                ServerMsg::PointerButton(b) => {
                    clicks += 1;
                    emit(&format!(
                        "PointerButton #{clicks} REACHED US: button={:#x} state={:?}",
                        b.button, b.state
                    ))?;
                }
                ServerMsg::Error(e) => {
                    emit(&format!("Error {:?}: {}", e.code, e.msg))?;
                    emit("the compositor refused to be opted out of: contained")?;
                    return Ok(());
                }
                other => emit(other.name())?,
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Print one line and flush, so output survives a pipe (`ssh`, `grep`).
fn emit(line: &str) -> std::io::Result<()> {
    println!("{line}");
    std::io::stdout().flush()
}
