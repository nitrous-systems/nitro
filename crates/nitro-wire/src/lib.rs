//! The nitro wire protocol: the contract between clients and the display
//! server.
//!
//! Everything above this crate — toolkit, apps, the remote view, the
//! Wayland adapter — is layered on it, so it is deliberately small,
//! boring and versioned. Hand-written framing, `repr(C)` message bodies,
//! no `unsafe`, and exactly two dependencies (`rustix` for the kernel ABI,
//! `zerocopy` for validated layout).
//!
//! The full specification, including the byte layout of every message, is
//! in `docs/wire.md`.
//!
//! # Shape of the protocol
//!
//! * A **frame** is an 8-byte header ([`header::FrameHeader`]) plus a
//!   payload. Descriptors ride on the `sendmsg` that carries the header.
//! * Clients send **mutations** on a tree of nodes with **client-allocated
//!   ids**; nothing is visible until [`Commit`](msg::Commit), which applies
//!   the batch atomically.
//! * The server sends configuration, input and frame timing back.
//! * Errors are **fatal**: the server sends [`msg::Error`] and closes.
//!
//! # Client
//!
//! ```no_run
//! use nitro_core::{Color, Rect, Size};
//! use nitro_wire::client::Connection;
//! use nitro_wire::types::{Layer, NodeId};
//!
//! # fn main() -> Result<(), nitro_wire::Error> {
//! let mut conn = Connection::connect_default("demo")?;
//! let win = NodeId(1);
//! let box_ = NodeId(2);
//! conn.tx()
//!     .create_window(win, "demo", Size::new(400.0, 300.0), Layer::Normal)
//!     .create_rect(box_, win, Rect::new(10.0, 10.0, 100.0, 50.0))
//!     .fill_solid(box_, Color::rgb(0x33, 0x88, 0xff))
//!     .commit(1)?;
//! conn.flush()?;
//! # Ok(()) }
//! ```
//!
//! # Server
//!
//! ```no_run
//! use nitro_wire::server::{ClientStream, Listener};
//!
//! # fn main() -> Result<(), nitro_wire::Error> {
//! let listener = Listener::bind_default()?;
//! // ... register `listener.as_fd()` with epoll ...
//! while let Some(mut client) = listener.accept()? {
//!     client.read()?;
//!     while let Some(msg) = client.next_msg()? {
//!         if nitro_wire::server::is_hello(&msg).is_some() {
//!             client.welcome("nitro", 0)?;
//!         }
//!         // ... apply the mutation ...
//!     }
//!     client.flush()?;
//! }
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod client;
pub mod codec;
pub mod error;
pub mod framing;
pub mod io;
pub mod msg;
pub mod server;
pub mod types;
pub mod wire;

pub use codec::{FdQueue, Reader, Writer};
pub use error::{DecodeError, EncodeError, Error};
pub use framing::{Frame, Framer, Head, header};
pub use io::Socket;
pub use msg::{ClientMsg, Fill, ServerMsg};
pub use types::{BufferId, Edge, ErrorCode, Layer, NodeId, NodeKind, WindowRef};

/// Protocol version. Bumped only for an incompatible change; v1 is frozen
/// at M2 and grows only through new ops guarded by capability bits.
pub const VERSION: u32 = 1;

/// Largest payload of one frame, header excluded: 16 MiB.
///
/// Sized so a 2048×2048 `AR24` image *cannot* be sent inline — pixels go
/// through `SCM_RIGHTS` buffers, never through the stream.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// Largest number of descriptors on one frame.
///
/// v1 messages carry at most one; the extra room is for batching later
/// and bounds the receiver's ancillary buffer.
pub const MAX_FDS: usize = 8;

/// Largest number of *unclaimed* descriptors a [`Framer`] will hold.
///
/// Descriptors arrive out of band and are claimed only when the frame that
/// declares them is decoded, so the real quantity this bounds is **how
/// many descriptors can arrive between two drains** — not how many fit in
/// one `recvmsg`. The kernel does not coalesce skbs carrying `SCM_RIGHTS`,
/// so each `recvmsg` yields one `sendmsg`'s worth; a receiver that reads
/// repeatedly without decoding accumulates one per call regardless of how
/// few bytes each carries.
///
/// That makes this constant **coupled to the receive loop**: a loop that
/// reads without draining must yield when it reaches this cap (see
/// [`server::ClientStream::read`]) rather than let [`Framer::feed`] hit
/// it, or a legitimate batch of buffers is killed as a flood. Tune one and
/// you must look at the other.
///
/// Yielding alone is not enough, though: yielding is progress only while
/// the caller has something to drain. At the cap with *nothing decodable
/// left* the pending descriptors belong to no frame and never will, so a
/// loop that kept yielding would spin at 100% CPU and never report the
/// attack. Both halves are needed, and together they are the real test:
/// **do pending descriptors survive a drain?**
///
/// Without the cap, a peer attaching a descriptor to every `sendmsg` while
/// declaring `fds: 0` in every header would park one open fd per call in
/// the receiver forever: the frames decode fine, nothing errors, and the
/// process walks into `EMFILE` — on a server, taking every other client
/// with it. The honest distinction between that and a large legitimate
/// batch is whether the pending count survives a drain: legitimate traffic
/// drops to zero as soon as frames are decoded, the flood never does.
/// Exceeding the cap — or sitting at it with nothing left to decode — is a
/// fatal [`DecodeError::UnexpectedFd`](crate::DecodeError::UnexpectedFd).
pub const MAX_PENDING_FDS: usize = 64;

/// Environment variable overriding the socket path.
pub const SOCKET_ENV: &str = "NITRO_SOCKET";

/// Environment variable overriding the **shell** socket path.
pub const SHELL_SOCKET_ENV: &str = "NITRO_SHELL_SOCKET";

/// Subdirectory of `$XDG_RUNTIME_DIR` holding the socket.
pub const SOCKET_SUBDIR: &str = "nitro";

/// Default socket file name.
pub const DEFAULT_SOCKET_NAME: &str = "wire.sock";

/// Default **shell** socket file name; see [`shell_socket_path`].
pub const DEFAULT_SHELL_SOCKET_NAME: &str = "shell.sock";

/// Where the wire socket lives.
///
/// `NITRO_SOCKET` overrides the whole path; otherwise
/// `$XDG_RUNTIME_DIR/nitro/wire.sock`, falling back to
/// `/tmp/nitro-<uid>/wire.sock` when the runtime directory is unset or
/// relative.
///
/// Client and server both resolve through this one function — they must
/// agree, so there is deliberately only one copy. It is re-exported as
/// [`client::socket_path`] and [`server::socket_path`].
#[must_use]
pub fn socket_path() -> std::path::PathBuf {
    resolve_socket(SOCKET_ENV, DEFAULT_SOCKET_NAME)
}

/// Where the **shell** socket lives.
///
/// `NITRO_SHELL_SOCKET` overrides the whole path; otherwise
/// `$XDG_RUNTIME_DIR/nitro/shell.sock`, falling back to
/// `/tmp/nitro-<uid>/shell.sock` on the same terms as [`socket_path`].
///
/// This second socket **is** the privilege: a client that can connect to it
/// is granted [`caps::SHELL`](types::caps::SHELL) and may send the shell
/// ops. The directory is `0700`, so the grant is "the user's own
/// processes" and nothing finer; per-app allow lists are deferred (see
/// `docs/shell.md`).
#[must_use]
pub fn shell_socket_path() -> std::path::PathBuf {
    resolve_socket(SHELL_SOCKET_ENV, DEFAULT_SHELL_SOCKET_NAME)
}

/// The shared resolution both socket paths use: an explicit override, else
/// the runtime directory, else a per-uid directory under `/tmp`.
fn resolve_socket(env: &str, name: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(p) = std::env::var_os(env) {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir.join(SOCKET_SUBDIR).join(name);
        }
    }
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/nitro-{uid}")).join(name)
}
