//! `nitro-demo`: the client that exercises the whole M1 path and measures
//! it.
//!
//! `hello_client` proves a client can draw. This one is the measurement
//! instrument: it draws the kind of scene a toolkit will (a gradient
//! backdrop, a row of bordered rounded rects, a memfd-backed image), it
//! follows the pointer the way a widget under the cursor would, and it
//! keeps the books on **input-to-photon latency from the client's side of
//! the socket**.
//!
//! # Why the client measures it too
//!
//! The server already reports `i2p_*` over its control socket, but that
//! number is the server marking its own homework: it is the interval
//! between an input timestamp the server read and a vblank the server saw,
//! with the client's work nowhere in it. The client's view spans the same
//! two instants *through* the round trip — `PointerMotion` out, `Commit`
//! back, the frame that carries it — so it includes the client's reaction,
//! the server's read of the transaction and the compositing pass. If the
//! two disagree by more than a frame, one of them is lying, and
//! `--stats` prints both so the test can say which.
//!
//! Both timestamps come from the *server* (`PointerMotion.time_ns` and
//! `Presented.time_ns`, both `CLOCK_MONOTONIC`), so the subtraction is
//! immune to any clock skew between the two processes and the client never
//! has to trust its own clock for the headline number.
//!
//! # Modules
//!
//! * [`args`] — the command line.
//! * [`latency`] — the histogram, the percentiles, and the
//!   serial-to-input-time ledger.
//! * [`geom`] — what a follower move damages, and the outlines that make
//!   that visible in a screenshot.
//! * [`scene`] — node ids, layout and the mutation batches; no sockets, so
//!   a test can build the same scene the binary does.
//! * [`control`] — the v0 control socket, for the server's own view.
//! * [`png`] — a size-conscious PNG writer for `--save-small`.
//! * [`app`] — the event loop that ties them together.
//!
//! The split is what lets `tests/against_server.rs` drive the real server
//! loop in-process with the real demo scene, rather than a reimplementation
//! of it that could drift.

#![forbid(unsafe_code)]

pub mod app;
pub mod args;
pub mod control;
pub mod geom;
pub mod latency;
pub mod png;
pub mod scene;

/// `CLOCK_MONOTONIC` nanoseconds, the clock every timestamp on the wire
/// uses.
#[must_use]
pub fn monotonic_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(t.tv_sec).unwrap_or(0) * 1_000_000_000 + u64::try_from(t.tv_nsec).unwrap_or(0)
}
