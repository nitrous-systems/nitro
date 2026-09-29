//! Real-kernel tests for the scanout-buffer mapping and the
//! `DMA_BUF_IOCTL_SYNC` bracket (#3914).
//!
//! The memfd half — which is also what the fake KMS backend exports — and
//! the `ENOTTY` path run everywhere; see `only_a_dmabuf_skips_the_seals`
//! for why a real dma-buf is not created here.

use std::os::fd::{AsFd, OwnedFd};

use nitro_shm::{
    DmaBufMapping, MapError, Mapping, SealError, SyncAccess, create_sealed, is_dmabuf, sync_end,
    sync_start,
};
use rustix::fs::{MemfdFlags, SealFlags};

fn unsealed(len: u64) -> OwnedFd {
    let fd =
        rustix::fs::memfd_create("t", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
    rustix::fs::ftruncate(&fd, len).unwrap();
    fd
}

#[test]
fn a_sealed_memfd_maps_and_writes_through() {
    let fd = create_sealed("t", 4096).unwrap();
    assert!(!is_dmabuf(&fd));
    let mut m = DmaBufMapping::map(fd.as_fd(), 4096).unwrap();
    assert_eq!(m.len(), 4096);
    m.as_bytes_mut()[..4].copy_from_slice(b"nv12");
    m.as_bytes_mut()[4095] = 7;
    // Another mapping of the same file sees the writes: `MAP_SHARED`.
    let dup = rustix::io::dup(&fd).unwrap();
    let r = Mapping::map(dup, 4096).unwrap();
    assert_eq!(&r.as_bytes()[..4], b"nv12");
    assert_eq!(r.as_bytes()[4095], 7);
    // And so does the server's read-only dma-buf entry point.
    let r2 = Mapping::map_dmabuf(rustix::io::dup(&fd).unwrap(), 4096).unwrap();
    assert_eq!(&r2.as_bytes()[..4], b"nv12");
}

#[test]
fn an_unsealed_memfd_is_refused() {
    let fd = unsealed(4096);
    assert!(matches!(
        DmaBufMapping::map(fd.as_fd(), 4096),
        Err(MapError::Seals(SealError::Missing(_)))
    ));
    assert!(matches!(
        Mapping::map_dmabuf(fd, 4096),
        Err(MapError::Seals(SealError::Missing(_)))
    ));
}

#[test]
fn a_partly_sealed_memfd_is_refused() {
    let fd = unsealed(4096);
    rustix::fs::fcntl_add_seals(&fd, SealFlags::GROW | SealFlags::SEAL).unwrap();
    assert!(matches!(
        DmaBufMapping::map(fd.as_fd(), 4096),
        Err(MapError::Seals(SealError::Missing(m))) if m == SealFlags::SHRINK
    ));
}

#[test]
fn a_regular_file_is_refused() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("nitro-shm-dmabuf-{}", std::process::id()));
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(4096).unwrap();
    let _ = std::fs::remove_file(&path);
    let fd = OwnedFd::from(f);
    assert!(!is_dmabuf(&fd));
    assert!(matches!(
        DmaBufMapping::map(fd.as_fd(), 4096),
        Err(MapError::Seals(SealError::Unsealable(_)))
    ));
}

#[test]
fn too_long_a_mapping_is_refused() {
    let fd = create_sealed("t", 4096).unwrap();
    assert_eq!(
        DmaBufMapping::map(fd.as_fd(), 4097).unwrap_err(),
        MapError::TooShort {
            file: 4096,
            need: 4097
        }
    );
    assert!(matches!(
        DmaBufMapping::map(fd.as_fd(), 0),
        Err(MapError::Os(_))
    ));
}

#[test]
fn sync_on_a_memfd_is_enotty_and_means_no_sync_needed() {
    let fd = create_sealed("t", 4096).unwrap();
    for a in [SyncAccess::Read, SyncAccess::Write, SyncAccess::ReadWrite] {
        assert_eq!(sync_start(&fd, a), Ok(false));
        assert_eq!(sync_end(&fd, a), Ok(false));
    }
}

#[test]
fn sync_on_a_socket_is_enotty_too() {
    let (r, _w) = std::os::unix::net::UnixStream::pair().unwrap();
    let r = OwnedFd::from(r);
    assert_eq!(sync_start(&r, SyncAccess::Write), Ok(false));
}

/// A real dma-buf needs an exporter: `/dev/udmabuf` (rarely loaded, and
/// its `UDMABUF_CREATE` would need an `unsafe` ioctl in this test, widening
/// the exception the crate is careful to scope) or a DRM device. So the
/// dma-buf half of the argument is exercised where a DRM device exists:
/// the server's scanout-buffer path on hardware (`docs/surfaces.md`,
/// box1). What *can* be pinned here is that nothing but a dma-buf skips
/// the seal check: every non-dma-buf kind above reports
/// `is_dmabuf == false` and goes through the seals.
#[test]
fn only_a_dmabuf_skips_the_seals() {
    let (r, _w) = std::os::unix::net::UnixStream::pair().unwrap();
    let r = OwnedFd::from(r);
    assert!(!is_dmabuf(&r));
    assert!(matches!(
        DmaBufMapping::map(r.as_fd(), 4096),
        Err(MapError::Seals(SealError::Unsealable(_)))
    ));
}
