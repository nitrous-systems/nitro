//! Throwaway shell probe for the M3-B shell socket — the hardware smoke
//! test for the privileged path.
//!
//! It connects to **`shell.sock`**, not the wire socket, so the `Welcome`
//! carries `caps::SHELL` and the shell ops are accepted. Then it does the
//! three things a real bar, launcher and wallpaper do between them:
//!
//! * opens one `Top`-layer, `UNDECORATED`, `NO_FOCUS` window, anchors it to
//!   `TOP|LEFT|RIGHT` (so it spans the edge whatever the output is) and
//!   reserves a 32-px **exclusive zone** on the top edge. A maximized
//!   client should end up 32 px shorter and 32 px lower;
//! * asks for the `WindowList` and the `Outputs` list, and prints every
//!   `WindowInfo`, `WindowGone`, `OutputInfo` and `OutputGone` as it
//!   arrives, so the test can watch a window's title, state, focus and
//!   output change live;
//! * binds `Super+Return` and the **bare-Super tap**, and prints each
//!   `HotKey` — the two triggers the launcher needs.
//!
//! After the first commit it sits in a `poll(2)` on the connection fd, so
//! an idle probe costs nothing and `stats` should read 0.0 % with it
//! connected.
//!
//! No signal handling, on purpose: Ctrl-C kills the process, the kernel
//! closes the socket, and the server releases the zone, the bindings and
//! the window. That release path is itself part of what this probe tests —
//! after Ctrl-C, a maximized window should get its 32 px back.
//!
//! Run it on the test box against a running nitro server:
//!
//! ```text
//! ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/shell_probe'
//! ```
//!
//! `NITRO_SHELL_SOCKET` overrides the socket path; otherwise it is
//! `$XDG_RUNTIME_DIR/nitro/shell.sock`.

use std::io::Write as _;
use std::os::fd::BorrowedFd;

use nitro_core::{Color, Point, Rect, Size};
use nitro_wire::Error as WireError;
use nitro_wire::client::Connection;
use nitro_wire::msg::{Fill, ServerMsg};
use nitro_wire::types::{Edge, Layer, NodeId, anchor, caps, mod_mask, window_flags};
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;

/// The bar's root group.
const BAR: NodeId = NodeId(1);
/// The rect filling it.
const BAR_FILL: NodeId = NodeId(2);

/// Logical height of the bar, and the space it reserves.
const BAR_H: f32 = 32.0;
/// A width the anchor will immediately override; the server decides the
/// real one, which is the point of anchoring.
const BAR_W: f32 = 400.0;

/// Our id for the `Super+Return` binding.
const HOTKEY_LAUNCH: u32 = 1;
/// Our id for the bare-Super tap.
const HOTKEY_TAP: u32 = 2;
/// X11 `Return`.
const XK_RETURN: u32 = 0xff0d;

/// The bar's gradient, top and bottom.
const BAR_TOP: Color = Color::rgb(0x2C, 0x3E, 0x55);
const BAR_BOTTOM: Color = Color::rgb(0x1A, 0x22, 0x30);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = Connection::connect_shell("shell-probe")?;
    emit(&format!(
        "Connected server={:?} caps={:#x} shell={}",
        conn.server_name(),
        conn.caps(),
        conn.has_caps(caps::SHELL)
    ))?;
    if !conn.has_caps(caps::SHELL) {
        // Reached only if the server bound the shell socket but answered
        // without the bit, which would be a server bug worth failing on
        // rather than limping past.
        emit("no SHELL capability on the shell socket: server bug")?;
        return Ok(());
    }

    // The bar: one window, one rect, `Top` layer so no normal window can
    // cover it, `NO_FOCUS` so it never steals the keyboard.
    conn.tx()
        .create_window_with(
            BAR,
            "shell-probe bar",
            Size::new(BAR_W, BAR_H),
            Layer::Top,
            window_flags::UNDECORATED | window_flags::NO_FOCUS,
        )
        .create_rect(BAR_FILL, BAR, Rect::new(0.0, 0.0, BAR_W, BAR_H))
        .fill(
            BAR_FILL,
            Fill::Linear {
                start: Point::new(0.0, 0.0),
                end: Point::new(0.0, BAR_H),
                c0: BAR_TOP,
                c1: BAR_BOTTOM,
            },
        )
        // Span the top edge, and reserve the strip. Anchor *and* zone,
        // because they answer different questions: the anchor says where
        // the bar is, the zone says what the rest of the desktop may use.
        .set_anchor(BAR, anchor::TOP | anchor::LEFT | anchor::RIGHT, 0)
        .set_exclusive_zone(BAR, Edge::Top, BAR_H as u32)
        .commit(1)?;
    // The serial is the client's own counter; `Configure`s are answered with
    // later ones below.
    let mut serial: u32 = 1;
    flush_blocking(&mut conn)?;

    // The window list and the output list. Both are answered on receipt and
    // both subscribe, so everything after this arrives unasked.
    conn.window_list()?;
    conn.outputs()?;
    // The launcher's two triggers.
    conn.bind_key(HOTKEY_LAUNCH, mod_mask::SUPER, XK_RETURN)?;
    conn.bind_key(HOTKEY_TAP, mod_mask::SUPER, 0)?;
    flush_blocking(&mut conn)?;
    emit("asked for the window list and the outputs; bound Super+Return and the Super tap")?;

    let mut events = Vec::new();
    loop {
        // Block in the kernel rather than spinning on the non-blocking
        // socket: an idle probe must cost nothing on the test box.
        wait(conn.as_fd(), PollFlags::IN)?;
        events.clear();
        if let Err(e) = conn.poll(&mut events) {
            return match e {
                // The server hung up: a clean end of the probe.
                WireError::Closed => {
                    emit("server closed the connection")?;
                    Ok(())
                }
                other => Err(other.into()),
            };
        }
        for msg in &events {
            emit(&describe(msg))?;
            match msg {
                // Answer the anchor's `Configure` the way a real bar must:
                // the server decided the window's size, and the client's own
                // nodes do not resize themselves. Without this the bar keeps
                // painting its original width and the desktop shows through
                // the rest of the strip — which is exactly what the first
                // run of this probe showed.
                ServerMsg::Configure(c) if c.window == BAR => {
                    serial += 1;
                    conn.tx()
                        .bounds(BAR_FILL, Rect::new(0.0, 0.0, c.size.w, c.size.h))
                        .fill(
                            BAR_FILL,
                            Fill::Linear {
                                start: Point::new(0.0, 0.0),
                                end: Point::new(0.0, c.size.h),
                                c0: BAR_TOP,
                                c1: BAR_BOTTOM,
                            },
                        )
                        .commit(serial)?;
                }
                ServerMsg::Closed(_) => {
                    emit("the bar was closed; exiting")?;
                    return Ok(());
                }
                _ => {}
            }
        }
        flush_blocking(&mut conn)?;
        if conn.is_closed() {
            emit("server closed the connection")?;
            return Ok(());
        }
    }
}

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
        ServerMsg::Welcome(m) => format!("Welcome caps={:#x} name={:?}", m.caps, m.name),
        ServerMsg::Error(m) => format!(
            "Error serial={} code={:?} msg={:?}",
            m.serial, m.code, m.msg
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
        ServerMsg::Presented(m) => format!("Presented serial={} output={}", m.serial, m.output),
        ServerMsg::Closed(m) => format!("Closed window={}", m.window.raw()),
        ServerMsg::HotKey(m) => format!(
            "HotKey id={} pressed={} ({})",
            m.id,
            m.pressed,
            match m.id {
                HOTKEY_LAUNCH => "Super+Return",
                HOTKEY_TAP => "bare Super tap",
                _ => "?",
            }
        ),
        ServerMsg::WindowInfo(m) => format!(
            "WindowInfo window={} state={:?} focused={} output={} layer={:?} app_id={:?} title={:?}",
            m.window.raw(),
            m.state,
            m.focused,
            // `u32::MAX` is \"on no output\": created before any output
            // existed, or its output was unplugged.
            if m.output == u32::MAX {
                "-".to_owned()
            } else {
                m.output.to_string()
            },
            // Only `Normal` is an application; a task list filters on this.
            m.layer,
            m.app_id,
            m.title
        ),
        ServerMsg::WindowListEnd(_) => "WindowListEnd".to_owned(),
        ServerMsg::WindowGone(m) => format!("WindowGone window={}", m.window.raw()),
        ServerMsg::OutputInfo(m) => format!(
            "OutputInfo id={} {}x{}@{}.{:03}Hz scale={} at {},{} name={:?}",
            m.id,
            m.w,
            m.h,
            m.refresh_mhz / 1000,
            m.refresh_mhz % 1000,
            m.scale,
            m.x,
            m.y,
            m.name
        ),
        ServerMsg::OutputsEnd(_) => "OutputsEnd".to_owned(),
        ServerMsg::OutputGone(m) => format!("OutputGone id={}", m.id),
        other => other.name().to_owned(),
    }
}

fn emit(line: &str) -> std::io::Result<()> {
    println!("{line}");
    std::io::stdout().flush()
}
