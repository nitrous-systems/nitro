//! The nitro GPU helper's Vulkan backend.
//!
//! This is the **only** crate in the workspace with GPU bindings, and the
//! crate-wide `unsafe` exception that comes with them (DEPENDENCIES.md,
//! "`unsafe` exceptions"): Vulkan calls through `ash`, the `UDMABUF_CREATE`
//! and `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` ioctls, and the one
//! `env::set_var` that restricts the loader to the vendor ICD. Every
//! `unsafe` block carries a `// SAFETY:` comment; clippy enforces it.
//!
//! - [`icd`]: render node → kernel driver → vendor ICD manifest;
//! - `device`: instance, device, format/modifier tables;
//! - `pipeline`: render pass, samplers (YCbCr conversions), pipelines;
//! - [`backend`]: [`VkBackend`], the `nitro_gpu::Backend` implementation;
//! - [`sys`]: the two kernel ioctls.
//!
//! The protocol, event loop, validation and sandbox are `nitro-gpu`'s.

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod backend;
mod device;
pub mod icd;
mod pipeline;
pub mod sys;

pub use backend::VkBackend;

/// A pixel extent as `i32` (saturating).
pub(crate) fn px(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}
