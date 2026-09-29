//! The nitro GPU helper, minus the GPU.
//!
//! The helper is a **separate process** that composites what the display
//! planes cannot take — client dma-bufs (video, GPU windows) under the
//! server's shadow buffer — in one damage-clipped render pass of textured
//! quads, and replies at once with a `sync_file` the server hands to KMS
//! as `IN_FENCE_FD`. Design: `docs/surfaces.md` § GPU helper.
//!
//! This crate is everything about it that is not a GPU API, and it has no
//! `unsafe`:
//!
//! - [`proto`]: the server ↔ helper messages and their codec (on
//!   `nitro-wire` framing), used by both ends;
//! - [`client`]: the server's end of the socket;
//! - [`backend`]: the narrow trait a GPU API implements (no GPU types);
//! - [`event_loop`]: the helper's loop — validation, replies, fence
//!   polling, deferred release, idle exit;
//! - [`ring`]: buffer-age damage for the output ring;
//! - [`validate`]: hostile-input checks on every request;
//! - [`lifetime`]: texture references held by in-flight frames;
//! - [`sandbox`]: privilege drop and the render-node-only fd check;
//! - [`stats`]: counters, `VmRSS`, and DRM fdinfo driver memory;
//! - [`fake`]: a GPU-less backend for tests.
//!
//! The Vulkan backend is the `nitro-gpu-vulkan` binary, the only crate
//! with GPU bindings (and their `unsafe`).

#![forbid(unsafe_code)]

pub mod backend;
pub mod client;
pub mod event_loop;
pub mod fake;
pub mod lifetime;
pub mod proto;
pub mod ring;
pub mod sandbox;
pub mod stats;
pub mod validate;

/// A `poll` timeout from a `Duration` (saturating).
pub(crate) fn timespec(d: std::time::Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(d.subsec_nanos()),
    }
}

/// A pixel extent as `i32` (saturating; the protocol caps edges at
/// [`proto::MAX_EDGE`]).
pub(crate) fn px(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

pub use backend::{Backend, BackendError, Readback, Ring, RingRequest};
pub use client::Conn;
pub use event_loop::{Config, Exit, run};
pub use proto::{FromHelper, Message, ToHelper};
