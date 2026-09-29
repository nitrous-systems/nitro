//! The two kernel ioctls the helper needs besides Vulkan: `UDMABUF_CREATE`
//! (memfd → dma-buf, for the zero-copy shadow) and
//! `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (attach a frame's completion fence to
//! the output dma-buf, so implicit-sync readers wait for it).

use std::ffi::c_void;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use rustix::io::Errno;
use rustix::ioctl::{Ioctl, IoctlOutput, Opcode, Setter, opcode};

/// `struct udmabuf_create` from `<linux/udmabuf.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

const _: () = assert!(std::mem::size_of::<UdmabufCreate>() == 24);

/// `UDMABUF_CREATE`: `_IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: Opcode = opcode::write::<UdmabufCreate>(b'u', 0x42);
/// `UDMABUF_FLAGS_CLOEXEC`.
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// The request; its output is the new dma-buf fd (the ioctl's return value).
struct Create(UdmabufCreate);

// SAFETY: `opcode` is `UDMABUF_CREATE`, whose argument is `struct
// udmabuf_create` — `UdmabufCreate` reproduces it field for field (size 24,
// asserted above, which is also the size the opcode encodes). `as_ptr`
// points at that struct, alive and exclusively borrowed for the call.
// `IS_MUTATING = false` is right: `_IOW` means the kernel only reads it.
// `output_from_ptr` (below) turns the return value into an fd.
unsafe impl Ioctl for Create {
    type Output = OwnedFd;
    const IS_MUTATING: bool = false;

    fn opcode(&self) -> Opcode {
        UDMABUF_CREATE
    }

    fn as_ptr(&mut self) -> *mut c_void {
        (&raw mut self.0).cast()
    }

    unsafe fn output_from_ptr(out: IoctlOutput, _: *mut c_void) -> rustix::io::Result<OwnedFd> {
        if out < 0 {
            return Err(Errno::BADF);
        }
        // SAFETY: a successful `UDMABUF_CREATE` returns a freshly installed
        // fd (the new dma-buf, `O_CLOEXEC`) that only this call knows
        // about, so this is its single owner. Negative values were refused
        // above.
        Ok(unsafe { OwnedFd::from_raw_fd(out) })
    }
}

/// Wrap `size` bytes of a memfd from `offset` as a dma-buf through
/// `/dev/udmabuf`. The memfd must carry `F_SEAL_SHRINK` and must not carry
/// `F_SEAL_WRITE`; `offset` and `size` must be page-aligned.
///
/// # Errors
/// `ENOENT`/`EACCES` from opening `/dev/udmabuf` (no module, no access),
/// `EINVAL` for a bad memfd/range, or any other errno.
pub fn udmabuf(memfd: impl AsFd, offset: u64, size: u64) -> Result<OwnedFd, Errno> {
    let dev = rustix::fs::open(
        "/dev/udmabuf",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let memfd =
        u32::try_from(rustix::fd::AsRawFd::as_raw_fd(&memfd.as_fd())).map_err(|_| Errno::BADF)?;
    let req = Create(UdmabufCreate {
        memfd,
        flags: UDMABUF_FLAGS_CLOEXEC,
        offset,
        size,
    });
    // SAFETY: `Create` describes `UDMABUF_CREATE` truthfully (see its
    // `Ioctl` impl). `dev` is `/dev/udmabuf`, whose only ioctls are
    // `UDMABUF_CREATE`/`_LIST`; the kernel reads our 24 bytes, pins the
    // memfd's pages and returns a new fd. It writes no memory of ours.
    unsafe { rustix::ioctl::ioctl(&dev, req) }
}

/// `struct dma_buf_import_sync_file` from `<linux/dma-buf.h>`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct ImportSyncFile {
    flags: u32,
    fd: i32,
}

const _: () = assert!(std::mem::size_of::<ImportSyncFile>() == 8);

/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`: `_IOW('b', 3, struct dma_buf_import_sync_file)`.
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: Opcode = opcode::write::<ImportSyncFile>(b'b', 3);
/// `DMA_BUF_SYNC_WRITE`: the fence is a write; readers must wait for it.
const DMA_BUF_SYNC_WRITE: u32 = 2;

/// Add `sync_file` as a **write** fence to the dma-buf `buf` (Linux 6.0+).
/// The `sync_file` fd is not consumed.
///
/// # Errors
/// `ENOTTY` on an older kernel, or any other errno.
pub fn import_sync_file(buf: impl AsFd, sync_file: impl AsFd) -> Result<(), Errno> {
    let arg = ImportSyncFile {
        flags: DMA_BUF_SYNC_WRITE,
        fd: rustix::fd::AsRawFd::as_raw_fd(&sync_file.as_fd()),
    };
    loop {
        // SAFETY: (Setter::new) the opcode is `_IOW('b', 3, struct
        // dma_buf_import_sync_file)` and `ImportSyncFile` is that struct
        // (`{ __u32 flags; __s32 fd; }`, size 8, asserted above).
        let op = unsafe { Setter::<DMA_BUF_IOCTL_IMPORT_SYNC_FILE, ImportSyncFile>::new(arg) };
        // SAFETY: (ioctl) `op` describes the request truthfully. Every
        // caller passes a dma-buf the helper exported itself, so the
        // request reaches the dma-buf ioctl table, where it only reads the
        // 8 bytes and adds the sync_file's fence to the reservation object.
        match unsafe { rustix::ioctl::ioctl(buf.as_fd(), op) } {
            Err(Errno::INTR | Errno::AGAIN) => {}
            r => return r,
        }
    }
}
