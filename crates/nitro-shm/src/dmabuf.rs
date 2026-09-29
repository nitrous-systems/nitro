//! Scanout buffers the server allocated (#3914): the client's mapping of
//! a dma-buf, and the `DMA_BUF_IOCTL_SYNC` bracket around its writes.
//!
//! **`unsafe` exception** (the tree's third; task 3914, granted by the
//! project's human in ask#430): two `unsafe` ioctl blocks, both for the
//! one `DMA_BUF_IOCTL_SYNC` request. Recorded under "`unsafe` exceptions" in `DEPENDENCIES.md`, with
//! the narrative in this crate's `README.md`. Like `map.rs`, this file and
//! only this file (besides `map.rs`) carries `#![allow(unsafe_code)]`; the
//! workspace's `unsafe_code = "deny"` covers every other line of the crate.
//!
//! The **mapping** adds no `unsafe` of its own. [`DmaBufMapping`] is a
//! wrapper over `map.rs`'s `RawMap`, exactly like `Mapping` and
//! `MappingMut`, so the tree still has one `mmap` and one `munmap`, and
//! the SAFETY arguments for them — including the dma-buf case of the
//! size argument — live there, next to the code they justify.
//!
//! # Why a dma-buf cannot `SIGBUS` the way an unsealed memfd can
//!
//! A dma-buf's size is set by its exporter (here: the DRM driver's PRIME
//! export of a dumb buffer) when the file is created. The dma-buf file has
//! no truncate and no `fallocate`, so neither the server, the client nor
//! anyone they pass the fd to can make it shorter than a mapping of it.
//! The seals exist to give a memfd that property; a dma-buf has it by type.
//! `RawMap::map` recognises a dma-buf by asking the kernel which
//! filesystem the inode is on (`fstatfs` → `DMA_BUF_MAGIC`), which a
//! client cannot fake; anything else must pass the seal check as before —
//! and a sealed memfd is exactly what the fake KMS backend exports.
//!
//! # The residual
//!
//! The same as `map.rs`'s, from the other side: the **server reads these
//! pages** (to paint them on the CPU path, or the display engine scans
//! them out) while the client may be writing them. A client that writes
//! outside a [`sync_start`]/[`sync_end`] bracket, or after `PresentSurface`
//! and before `BufferReleased`, sees its own frame **tear** — nothing
//! worse, because every reader indexes by geometry only (the raster
//! argument in `map.rs`, part (d)) and every bit pattern is a valid `u8`.
//! The bracket is a cache-coherency hint to the exporter, not a lock: on
//! i915 with a linear dumb buffer on x86 the CPU mapping is coherent and
//! the ioctl is close to a no-op, which is why the server does not bracket
//! its own reads (see `docs/surfaces.md`).

#![allow(unsafe_code)]

use std::os::fd::{AsFd, BorrowedFd};

use rustix::io::Errno;
use rustix::ioctl::{Opcode, Setter, opcode};
use rustix::mm::ProtFlags;

use crate::MapError;
use crate::map::{Accept, RawMap};

/// A read/write mapping of a server-allocated scanout buffer: the
/// client's view of a dma-buf from `SurfaceBufferAllocated`, so it can
/// decode straight into the pages the server scans out.
///
/// Accepts a real dma-buf, or a memfd carrying
/// [`REQUIRED_SEALS`](crate::REQUIRED_SEALS) (the fake backend's export).
/// Borrows the fd: the client keeps it for [`sync_start`]/[`sync_end`].
#[derive(Debug)]
pub struct DmaBufMapping {
    raw: RawMap,
}

impl DmaBufMapping {
    /// Map the first `len` bytes of `fd` read/write, `MAP_SHARED`.
    ///
    /// # Errors
    /// [`MapError::Seals`] if `fd` is neither a dma-buf nor a sealed
    /// memfd; [`MapError::TooShort`] if the file is shorter than `len`;
    /// [`MapError::Os`] if `fstat` or `mmap` fails, or `len == 0`.
    pub fn map(fd: BorrowedFd<'_>, len: usize) -> Result<Self, MapError> {
        let raw = RawMap::map(
            fd,
            len,
            ProtFlags::READ | ProtFlags::WRITE,
            Accept::DmaBufOrSealed,
        )?;
        Ok(Self { raw })
    }

    /// The mapped bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.raw.bytes()
    }

    /// The mapped bytes, writable. Bracket writes with [`sync_start`] and
    /// [`sync_end`].
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.raw.bytes_mut()
    }

    /// Length of the mapping in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len
    }

    /// Always `false`: a zero-length mapping is refused at [`map`](Self::map).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.len == 0
    }
}

/// Whether `fd` is a dma-buf (the kernel reports `DMA_BUF_MAGIC` for its
/// filesystem). `false` for a memfd, which is what the fake backend
/// exports.
#[must_use]
pub fn is_dmabuf(fd: impl AsFd) -> bool {
    crate::map::is_dmabuf(fd.as_fd())
}

/// What a CPU access bracket covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAccess {
    /// The CPU will read.
    Read,
    /// The CPU will write.
    Write,
    /// Both.
    ReadWrite,
}

impl SyncAccess {
    fn bits(self) -> u64 {
        match self {
            Self::Read => SYNC_READ,
            Self::Write => SYNC_WRITE,
            Self::ReadWrite => SYNC_READ | SYNC_WRITE,
        }
    }
}

/// `DMA_BUF_SYNC_READ`.
const SYNC_READ: u64 = 1;
/// `DMA_BUF_SYNC_WRITE`.
const SYNC_WRITE: u64 = 2;
/// `DMA_BUF_SYNC_START` (zero: the absence of `END`).
const SYNC_START: u64 = 0;
/// `DMA_BUF_SYNC_END`.
const SYNC_END: u64 = 4;

/// `DMA_BUF_IOCTL_SYNC`: `_IOW('b', 0, struct dma_buf_sync)`, where the
/// struct is one `__u64 flags`.
const DMA_BUF_IOCTL_SYNC: Opcode = opcode::write::<u64>(b'b', 0);

/// Begin a CPU access to the dma-buf behind `fd`.
///
/// Returns `Ok(true)` when the kernel took the bracket, `Ok(false)` when
/// `fd` is not a dma-buf (`ENOTTY`: the fake backend's memfd), which
/// needs no sync. `EINTR`/`EAGAIN` are retried.
///
/// # Errors
/// Any other errno from the ioctl.
pub fn sync_start(fd: impl AsFd, access: SyncAccess) -> Result<bool, Errno> {
    sync(fd.as_fd(), SYNC_START | access.bits())
}

/// End a CPU access begun with [`sync_start`]; the same `access`.
///
/// # Errors
/// As [`sync_start`].
pub fn sync_end(fd: impl AsFd, access: SyncAccess) -> Result<bool, Errno> {
    sync(fd.as_fd(), SYNC_END | access.bits())
}

fn sync(fd: BorrowedFd<'_>, flags: u64) -> Result<bool, Errno> {
    loop {
        // SAFETY (Setter::new): the two preconditions are that the opcode
        // is valid and that `u64` is the type the kernel expects for it.
        // `DMA_BUF_IOCTL_SYNC` is `_IOW('b', 0, struct dma_buf_sync)` in
        // `<linux/dma-buf.h>`, and `struct dma_buf_sync` is exactly one
        // `__u64 flags` — size 8, the size `opcode::write::<u64>` encodes,
        // so the opcode computed here is bit-for-bit the header's. The
        // kernel only *reads* the argument (`copy_from_user` of 8 bytes);
        // `Setter` passes a pointer to its own `u64`, valid for 8 bytes of
        // reads for the whole call.
        let op = unsafe { Setter::<DMA_BUF_IOCTL_SYNC, u64>::new(flags) };
        // SAFETY (ioctl): rustix marks `ioctl` unsafe because the pattern
        // object must describe the request truthfully — established by
        // the `Setter::new` argument above — and because an arbitrary
        // request on an arbitrary fd could do anything. On a dma-buf this
        // request only flushes/invalidates CPU caches for the buffer, with
        // no effect on this process's memory beyond the 8-byte read. On
        // any other file the `b`/0 request is not that file's ioctl:
        // memfds, pipes and sockets answer `ENOTTY`; a device whose own
        // ioctl table happened to use `'b'`/0 could interpret it, which is
        // why callers pass only fds from `SurfaceBufferAllocated` (a
        // dma-buf, or the fake's memfd) — and the argument is still only
        // a read of our 8 bytes, so no such device could write into this
        // process through it.
        match unsafe { rustix::ioctl::ioctl(fd, op) } {
            Ok(()) => return Ok(true),
            Err(Errno::NOTTY) => return Ok(false),
            Err(Errno::INTR | Errno::AGAIN) => {}
            Err(e) => return Err(e),
        }
    }
}
