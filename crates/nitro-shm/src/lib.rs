//! Sealed memfds, and the one sanctioned mapping of them.
//!
//! A client's pixel buffer is a memfd. Before this crate the server copied
//! it out with `pread` and the client copied it in with `pwrite`, so every
//! frame crossed memory three times before the rasterizer touched it — the
//! measured cost is in `docs/bench.md` (#569). Mapping the file removes two
//! of the three passes, and mapping a file another process controls is the
//! one thing in this tree that needs `unsafe`.
//!
//! The crate is split so that the `unsafe` can be audited in one sitting:
//!
//! - this module is safe Rust: creating a memfd with the seals the mapping
//!   relies on ([`create_sealed`], [`memfd_with`]), and checking that an fd
//!   someone *else* handed us carries them ([`check_seals`]);
//! - [`map`] holds the four `unsafe` blocks (`mmap`, `from_raw_parts`,
//!   `from_raw_parts_mut`, `munmap`) and the `SAFETY` proofs for each. It
//!   is the only module in
//!   the workspace with `#![allow(unsafe_code)]` besides one function in
//!   `nitro-seat`; the exception is scoped to that file and recorded in
//!   `DEPENDENCIES.md`.
//!
//! # The seals, and what each one is for
//!
//! The hazard a mapping has and a `pread` does not: if the file is made
//! shorter than the mapping, touching a page past the new end raises
//! `SIGBUS`. A hostile client can `ftruncate` its memfd whenever it likes,
//! so a read-only mapping of an *unsealed* fd is a way for any client to
//! kill the display server. The seals close that:
//!
//! - **`F_SEAL_SHRINK`** — `ftruncate` to a smaller size fails with
//!   `EPERM`. This is the load-bearing one: it is what makes "every byte of
//!   the mapping is inside the file" a fact for the mapping's whole life.
//! - **`F_SEAL_SEAL`** — no further seals can be added. There is no
//!   operation that *removes* a seal at all, so what this buys is not
//!   "the client cannot unseal" (it never could) but "the set of seals is
//!   final": a client cannot, say, add `F_SEAL_WRITE` after the server
//!   mapped the buffer and turn its own later writes into `EPERM`s it then
//!   blames on us. Required so that what [`check_seals`] observed is what
//!   holds for ever, with no later state to reason about.
//! - **`F_SEAL_GROW`** — `ftruncate` to a larger size fails with `EPERM`.
//!   Not needed to prevent `SIGBUS` (a longer file does not invalidate a
//!   fixed-length mapping), and it is not here for symmetry. With it the
//!   file's size at check time is its size for its whole life, so
//!   [`Mapping::file_len`] is a constant rather than a lower bound, and the
//!   client's declared size can be checked against reality once. The
//!   protocol has no legitimate use for growing a buffer whose geometry was
//!   fixed at `CreateBuffer`.
//!
//! What the seals deliberately do **not** include is `F_SEAL_WRITE` or
//! `F_SEAL_FUTURE_WRITE`: the whole point is that the client keeps writing
//! the next frame into the same pages. The consequences of that — bytes
//! changing under the server's `&[u8]` — are the residual the `map` module
//! states plainly rather than pretends away.
//!
//! # Miri
//!
//! None of this is reachable under Miri: `memfd_create`, `F_ADD_SEALS`,
//! `F_GET_SEALS` and a file-backed `mmap` have no Miri shims, and rustix's
//! default `linux_raw` backend issues syscalls through inline assembly that
//! Miri cannot execute. The tests in `tests/seals.rs` are real-kernel tests
//! and assert the seals are *in force* (the kernel says `EPERM`), not merely
//! set.

// No `#![forbid(unsafe_code)]` here, on purpose: a `forbid` cannot be
// overridden by the `#![allow(unsafe_code)]` in `map.rs`. The workspace's
// `unsafe_code = "deny"` applies to every other line of this crate.

mod map;

use std::fmt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

pub use map::{Mapping, MappingMut};
use rustix::fs::{MemfdFlags, SealFlags};
pub use rustix::io::Errno;

/// The seals a buffer must carry before it is mapped: `F_SEAL_SHRINK |
/// F_SEAL_GROW | F_SEAL_SEAL`. See the crate docs for each one's role.
pub const REQUIRED_SEALS: SealFlags = SealFlags::SHRINK
    .union(SealFlags::GROW)
    .union(SealFlags::SEAL);

/// Why an fd is not acceptable as a sealed buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// `F_GET_SEALS` failed: the fd is not a memfd (or anything else that
    /// supports sealing), so nothing can be promised about its size. A
    /// regular file on disk answers `EINVAL` here.
    Unsealable(Errno),
    /// The fd supports sealing but lacks these required seals.
    Missing(SealFlags),
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsealable(e) => write!(f, "fd does not support sealing ({e})"),
            Self::Missing(missing) => {
                f.write_str("fd lacks ")?;
                let mut first = true;
                for (bit, name) in [
                    (SealFlags::SHRINK, "F_SEAL_SHRINK"),
                    (SealFlags::GROW, "F_SEAL_GROW"),
                    (SealFlags::SEAL, "F_SEAL_SEAL"),
                ] {
                    if missing.contains(bit) {
                        if !first {
                            f.write_str(", ")?;
                        }
                        f.write_str(name)?;
                        first = false;
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SealError {}

/// Why a buffer could not be mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The fd is not sealed the way the mapping's safety argument needs.
    Seals(SealError),
    /// The file is shorter than the length asked for. With the seals in
    /// place this is a permanent fact, so the buffer is refused rather
    /// than waited on.
    TooShort {
        /// The file's size, from `fstat`.
        file: u64,
        /// The length the caller wanted mapped.
        need: u64,
    },
    /// `fstat` or `mmap` failed. `ENOMEM` (address space) and `ENODEV`
    /// (a file that cannot be mapped) are the ones a hostile client can
    /// provoke; all are refusals, none are unsound.
    Os(Errno),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seals(e) => write!(f, "{e}"),
            Self::TooShort { file, need } => {
                write!(f, "file is {file} bytes, {need} are needed")
            }
            Self::Os(e) => write!(f, "mapping the fd: {e}"),
        }
    }
}

impl std::error::Error for MapError {}

impl From<SealError> for MapError {
    fn from(e: SealError) -> Self {
        Self::Seals(e)
    }
}

/// Check that `fd` carries every seal in [`REQUIRED_SEALS`].
///
/// This asks the **kernel** what is enforced on the file behind `fd`, not
/// the client what it did: seals are a property of the inode, and the
/// answer is the same whichever process, and whichever of its descriptors,
/// asks. A client that never sealed its memfd fails here; so does one that
/// sent a descriptor to something that is not a memfd.
///
/// # Errors
/// [`SealError::Unsealable`] if the fd does not support sealing at all,
/// [`SealError::Missing`] naming exactly the required bits that are absent.
pub fn check_seals(fd: BorrowedFd<'_>) -> Result<(), SealError> {
    let seals = rustix::fs::fcntl_get_seals(fd).map_err(SealError::Unsealable)?;
    let missing = REQUIRED_SEALS.difference(seals);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SealError::Missing(missing))
    }
}

/// A fresh memfd of `len` bytes, zero-filled, sealed with
/// [`REQUIRED_SEALS`].
///
/// The seals do not restrict writing: the caller can `pwrite` into it or
/// map it with [`MappingMut`] before or after handing a duplicate to the
/// server. What they fix is the *size*, which is exactly what a mapping
/// needs to stay valid.
///
/// # Errors
/// Any `memfd_create`/`ftruncate`/`F_ADD_SEALS` failure. `EPERM` from the
/// sealing step cannot happen on a memfd this function just created with
/// `MFD_ALLOW_SEALING`; it is still reported rather than ignored.
pub fn create_sealed(name: &str, len: u64) -> Result<OwnedFd, Errno> {
    let fd = rustix::fs::memfd_create(name, MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING)?;
    rustix::fs::ftruncate(&fd, len)?;
    rustix::fs::fcntl_add_seals(&fd, REQUIRED_SEALS)?;
    Ok(fd)
}

/// Put `pixels` in a fresh sealed memfd and hand back its descriptor.
///
/// The write-once shape every image-carrying client in the tree wants
/// (the toolkit's `Image` widget, the demo, `hello_client`, the bench's
/// sprite): [`create_sealed`] then one `pwrite` loop. A client that
/// rewrites its buffer every frame should map it instead — see
/// [`MappingMut`].
///
/// # Errors
/// Any `memfd_create`/`ftruncate`/`F_ADD_SEALS`/`pwrite` failure. A
/// zero-length `pwrite` is reported as `EIO` rather than spun on: a memfd
/// that accepts nothing will go on accepting nothing.
pub fn memfd_with(name: &str, pixels: &[u8]) -> Result<OwnedFd, Errno> {
    let fd = create_sealed(name, pixels.len() as u64)?;
    let mut done = 0usize;
    while done < pixels.len() {
        match rustix::io::pwrite(&fd, &pixels[done..], done as u64) {
            Ok(0) => return Err(Errno::IO),
            Ok(n) => done += n,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(fd)
}

/// The size of the file behind a **sealed** fd.
///
/// Checks the seals first, because only with `F_SEAL_SHRINK` and
/// `F_SEAL_GROW` in force is the answer a fact rather than a snapshot.
///
/// # Errors
/// [`MapError::Seals`] if the fd is not sealed, [`MapError::Os`] if
/// `fstat` fails.
pub fn sealed_len(fd: impl AsFd) -> Result<u64, MapError> {
    let fd = fd.as_fd();
    check_seals(fd)?;
    let st = rustix::fs::fstat(fd).map_err(MapError::Os)?;
    // `st_size` is an `off_t`; a negative one is not something a memfd can
    // report, and treating it as "empty" is the safe direction (the map
    // refuses a too-short file).
    Ok(u64::try_from(st.st_size).unwrap_or(0))
}
