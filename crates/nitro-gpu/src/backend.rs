//! The narrow interface a GPU API implements. No GPU types appear here:
//! a backend sees validated descriptors, owned fds and pixel rects, and
//! hands back opaque textures, dma-bufs and `sync_file`s.
//!
//! Everything a backend receives has already passed [`crate::validate`]:
//! ids are known, rects are in bounds, fourcc/modifier pairs were
//! advertised by [`Backend::info`], the fence count matches. A backend
//! still returns errors (the driver can refuse anything), and those go
//! back to the server as [`ErrorCode::Backend`] (or whatever code the
//! backend picks) — never a panic.

use std::fmt;
use std::os::fd::OwnedFd;

use nitro_core::IRect;

use crate::proto::{DeviceInfo, DmabufDesc, ErrorCode, Layer, ShadowDesc, ShadowPath, SlotLayout};

/// A refused backend operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    /// Code reported to the server.
    pub code: ErrorCode,
    /// Detail for the log and the `Error` message.
    pub msg: String,
}

impl BackendError {
    /// An [`ErrorCode::Backend`] error.
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Backend,
            msg: msg.into(),
        }
    }

    /// An error with a specific code.
    pub fn with_code(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.msg)
    }
}

impl std::error::Error for BackendError {}

/// An output-ring allocation request, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingRequest {
    /// Slot count.
    pub n: usize,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// Fourcc.
    pub fourcc: u32,
    /// Acceptable modifiers, in preference order.
    pub modifiers: Vec<u64>,
}

/// An allocated ring: the chosen modifier and one exported dma-buf per slot.
#[derive(Debug)]
pub struct Ring {
    /// Modifier the driver picked.
    pub modifier: u64,
    /// Per-slot layout and dma-buf fd.
    pub slots: Vec<(SlotLayout, OwnedFd)>,
}

/// A slot's pixels, linear BGRA, as a sealed memfd.
#[derive(Debug)]
pub struct Readback {
    /// Sealed memfd of `stride * h` bytes.
    pub memfd: OwnedFd,
    /// Row stride in bytes.
    pub stride: u32,
}

/// A GPU API. See the module docs.
pub trait Backend {
    /// A texture: an imported dma-buf or shadow buffer.
    type Tex;

    /// Device name, driver, and the format/modifier tables.
    fn info(&self) -> DeviceInfo;

    /// How the shadow reaches the GPU (for stats).
    fn shadow_path(&self) -> ShadowPath {
        ShadowPath::Unknown
    }

    /// Import a dma-buf; `fds` has one fd per plane.
    ///
    /// # Errors
    /// The driver refused the buffer.
    fn import_dmabuf(
        &mut self,
        d: &DmabufDesc,
        fds: Vec<OwnedFd>,
    ) -> Result<Self::Tex, BackendError>;

    /// Import the shadow buffer: a sealed memfd (seals and size already
    /// checked).
    ///
    /// # Errors
    /// The driver refused the buffer.
    fn import_shadow(&mut self, d: &ShadowDesc, memfd: OwnedFd) -> Result<Self::Tex, BackendError>;

    /// The shadow's pixels changed in `rects` (in bounds, non-empty).
    /// Takes effect with the next [`Backend::composite`].
    ///
    /// # Errors
    /// The copy could not be recorded.
    fn upload_damage(&mut self, t: &mut Self::Tex, rects: &[IRect]) -> Result<(), BackendError>;

    /// (Re)allocate the output ring. A backend may wait for the GPU to go
    /// idle here: a reallocation is a mode change, not a frame.
    ///
    /// # Errors
    /// No modifier in the list works, or allocation/export failed.
    fn alloc_output_ring(&mut self, req: &RingRequest) -> Result<Ring, BackendError>;

    /// Draw `layers` (bottom first) into slot `out_idx`, restricted to
    /// `clip` (non-empty rects inside the output; an empty `clip` draws
    /// nothing but still submits). Wait for every `acquire` `sync_file`
    /// before sampling. Return the completion `sync_file` **without
    /// waiting** for the GPU. The event loop guarantees the slot's
    /// previous frame has signalled.
    ///
    /// # Errors
    /// Recording or submission failed.
    fn composite(
        &mut self,
        out_idx: usize,
        clip: &[IRect],
        layers: &[(&Self::Tex, Layer)],
        acquire: Vec<OwnedFd>,
    ) -> Result<OwnedFd, BackendError>;

    /// Free a texture. Every frame that sampled it has signalled.
    fn release(&mut self, t: Self::Tex);

    /// Debug/test: copy slot `out_idx` to linear BGRA. May block on the GPU.
    ///
    /// # Errors
    /// The copy failed.
    fn readback(&mut self, out_idx: usize) -> Result<Readback, BackendError>;

    /// A screenshot (#3962): draw `layers` (bottom first, each already
    /// validated against a `w`×`h` target) into a temporary target and
    /// return it as linear BGRX. Blocks until the GPU is done, like
    /// [`Backend::readback`]; everything it allocates is freed before it
    /// returns. Pixels no layer covers are undefined.
    ///
    /// # Errors
    /// Allocation, recording or the copy failed.
    fn capture(
        &mut self,
        w: u32,
        h: u32,
        layers: &[(&Self::Tex, Layer)],
    ) -> Result<Readback, BackendError>;
}
