//! The mapping: four `unsafe` blocks and their proofs.
//!
//! **`unsafe` exception**: task 3754 / issue #569, granted on the condition
//! that every `SAFETY` comment below is an argument, not a restatement.
//! Recorded under "`unsafe` exceptions" in `DEPENDENCIES.md`, with the
//! narrative version in this crate's `README.md`. The workspace lint is
//! `unsafe_code = "deny"`; this file, and only this file, allows it.
//! Nothing outside it may write `unsafe` — and that is enforced by the
//! compiler rather than by this sentence: the allow below is an inner
//! attribute scoped to this module, so an `unsafe` block anywhere else in
//! the crate fails the build. (`Cargo.toml` must keep
//! `[lints] workspace = true` for the deny to apply at all;
//! `the_lint_that_scopes_this_exception_is_still_in_place` in
//! `tests/seals.rs` is the regression test for someone deleting it.)
//!
//! The threat model throughout is a **hostile client**: the process that
//! created the memfd, still holds a descriptor to it, and will do whatever
//! the kernel lets it do to the file while the server has it mapped. Every
//! precondition is discharged from what the kernel enforces on *this* file
//! as observed through *this* fd, never from what the client claims.
//!
//! Exactly one `mmap` and one `munmap` exist in the tree; both are in
//! [`RawMap`]. [`Mapping`] (read-only, the server's) and [`MappingMut`]
//! (read/write, the client's) are thin wrappers that fix the protection
//! flags and expose slices.
//!
//! # Miri cannot check any of this, and here is why
//!
//! Stated here rather than only in the crate docs, because this is the
//! file a reader arrives at asking "was this verified by a tool?". It was
//! not, and it cannot be: `memfd_create`, `F_ADD_SEALS`, `F_GET_SEALS` and
//! a **file-backed** `mmap` have no Miri shims (Miri models anonymous
//! memory only), and rustix's default `linux_raw` backend issues syscalls
//! through inline assembly, which Miri refuses outright. So there is no
//! `cargo miri test` result to point at for this module.
//!
//! What stands in for it is that every load-bearing claim below is pinned
//! by a **real-kernel** test in `tests/seals.rs` that asserts the kernel's
//! own behaviour rather than a model of it — in particular
//! `a_sealed_memfd_cannot_be_shrunk`, which checks `ftruncate` returns
//! `EPERM` *and* that the mapping is still readable to its last byte. That
//! is the difference between "the seal was set" and "the seal is in
//! force", and it is the hinge the whole argument turns on.

#![allow(unsafe_code)]

use std::fmt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::ptr::NonNull;

use rustix::mm::{MapFlags, ProtFlags};

use crate::{MapError, check_seals};

/// A live `MAP_SHARED` mapping of the first `len` bytes of a sealed file.
///
/// Owns its `munmap`. Private: the two public wrappers choose the
/// protection and the slice types, and neither can be built without going
/// through [`RawMap::map`], which is where the preconditions are checked.
struct RawMap {
    /// Page-aligned start of the mapping, as `mmap` returned it.
    ptr: NonNull<u8>,
    /// Length in bytes as passed to `mmap`; also what `munmap` gets.
    len: usize,
    /// The file's size when it was mapped. Constant for the file's life
    /// because `F_SEAL_GROW | F_SEAL_SHRINK` were verified before mapping.
    file_len: u64,
}

impl RawMap {
    /// Map `[0, len)` of the file behind `fd` with `prot`.
    ///
    /// The order of the checks matters for the proof: seals first, size
    /// second, `mmap` last. Only once the seals are known to be in force
    /// does the size become a permanent fact rather than a snapshot.
    fn map(fd: BorrowedFd<'_>, len: usize, prot: ProtFlags) -> Result<Self, MapError> {
        // (1) The kernel enforces `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`
        //     on the inode behind `fd`, or this returns `Err`.
        check_seals(fd)?;
        // (2) The file is at least `len` bytes long. Because of (1) it will
        //     be exactly this long for as long as the inode exists.
        let st = rustix::fs::fstat(fd).map_err(MapError::Os)?;
        let file_len = u64::try_from(st.st_size).unwrap_or(0);
        let need = len as u64;
        if file_len < need {
            return Err(MapError::TooShort {
                file: file_len,
                need,
            });
        }
        // (3) `len` is non-zero: `mmap(len = 0)` is `EINVAL`, and a
        //     zero-length slice from a dangling pointer would be sound but
        //     the `NonNull` below would be a lie. Refuse it instead.
        if len == 0 {
            return Err(MapError::Os(rustix::io::Errno::INVAL));
        }

        // SAFETY (mmap): rustix marks `mmap` unsafe because a mapping can
        // alias or replace existing memory and because the caller must not
        // use it beyond what the file backs. Each precondition, and who
        // establishes it:
        //
        // * Address: `ptr` is null and `MAP_FIXED` is not set, so the
        //   kernel chooses a fresh, page-aligned range that overlaps no
        //   existing mapping. Nothing in this process is displaced, so no
        //   pointer anywhere else becomes dangling.
        //
        // * Length vs. file size, now: `fstat` at (2) reported
        //   `st_size >= len`, so every byte of `[0, len)` is inside the
        //   file at this moment and no page of the mapping is past EOF.
        //
        // * Length vs. file size, for the mapping's whole lifetime — the
        //   hazard the old `pread` path documented: a shorter file turns
        //   a load from the mapped range into `SIGBUS`. `check_seals` at
        //   (1) had the kernel report `F_GET_SEALS` on this fd and
        //   confirmed `F_SEAL_SHRINK` is among them. Seals are a property
        //   of the inode, not of the descriptor or of the asking process,
        //   so what was observed through our fd is what the kernel enforces
        //   against the client's fd too: its `ftruncate` to anything
        //   smaller fails with `EPERM` (asserted by `tests/seals.rs`
        //   `a_sealed_memfd_cannot_be_shrunk`, which is the positive proof
        //   the seal is in force and not merely set). There is no syscall
        //   that removes a seal, and `F_SEAL_SEAL` — also confirmed at (1)
        //   — forbids adding any, so the seal set observed is the seal set
        //   for ever. The mapping itself holds a reference to the inode
        //   (`close` on the last fd does not tear down a mapping; the
        //   kernel keeps the file alive for the mapping), so "for as long
        //   as the inode exists" covers the mapping's whole life. Hence:
        //   for as long as this `RawMap` exists, `st_size >= len` holds and
        //   no load through it can fault for "page beyond EOF".
        //
        // * `F_SEAL_GROW`: not load-bearing for `SIGBUS` — a longer file
        //   does not invalidate a fixed-length mapping of its start. It is
        //   required so that `file_len` is the file's size for its whole
        //   life rather than a lower bound, which is what lets a caller
        //   check a client's declared size once and rely on it, and
        //   because the protocol has no legitimate use for growing a
        //   buffer whose geometry was fixed at `CreateBuffer`.
        //
        // * Hostility: the client is assumed to lie about everything it
        //   sends. Nothing above trusts it — (1) and (2) are what the
        //   kernel says about the file, and a client that skipped sealing
        //   was refused at (1) before this call, so no unsealed inode is
        //   ever mapped by this process.
        //
        // * Offset 0 is page-aligned trivially; `MAP_SHARED` because the
        //   client's later writes must be visible (a private mapping would
        //   copy-on-write the very pages we want to see change).
        let raw =
            unsafe { rustix::mm::mmap(std::ptr::null_mut(), len, prot, MapFlags::SHARED, fd, 0) }
                .map_err(MapError::Os)?;
        // A successful `mmap` never returns null (`MAP_FAILED` is `-1` and
        // rustix has already turned it into `Err`). `expect` rather than an
        // unchecked construction: if a kernel ever did return null, a panic
        // here is the right outcome and the `munmap` in `Drop` is not
        // reached because `Self` was never built.
        let ptr = NonNull::new(raw.cast::<u8>()).expect("mmap returned a null mapping");
        Ok(Self { ptr, len, file_len })
    }

    /// The mapped bytes.
    fn bytes(&self) -> &[u8] {
        // SAFETY (from_raw_parts): the preconditions of `slice::from_raw_parts`
        // are (a) `ptr` is valid for reads of `len` bytes, (b) it is aligned
        // for `u8`, (c) the bytes are initialised `u8`s, (d) nothing mutates
        // them for the lifetime of the slice, (e) `len <= isize::MAX`.
        //
        // (a) `ptr..ptr + len` is exactly the range `mmap` returned for
        //     `len` bytes, mapped readable: both wrappers pass `PROT_READ`.
        //     The range stays mapped until `Drop` (the only `munmap`), and
        //     `Drop` takes `&mut self`, which the borrow checker will not
        //     grant while a `&[u8]` derived from `&self` is alive. That is
        //     the structural rule the compiler does not know about
        //     `munmap`: the mapping owns its unmap, the slice borrows from
        //     the mapping, so the unmap cannot happen under the slice. And
        //     by the `mmap` proof above, no page of the range is past EOF
        //     for the mapping's life, so a read faults only as memory
        //     pressure would make any load fault — an OOM kill, not a
        //     signal on this thread, and outside what seals address.
        // (b) `u8` has alignment 1; every address is aligned. (`ptr` is in
        //     fact page-aligned, which is more than needed.)
        // (c) Every bit pattern is a valid `u8`, so there is no such thing
        //     as an invalid value to read — the validity half is trivial
        //     for `u8` and holds regardless of what the client wrote. A
        //     never-written shmem page reads as zeros, so nothing here is
        //     uninitialised in the sense the kernel exposes.
        // (d) Within this process: `Mapping` hands out only `&[u8]` and is
        //     `PROT_READ`; `MappingMut` hands out `&mut [u8]` only through
        //     `&mut self`, so a `&[u8]` and a `&mut [u8]` from the same
        //     mapping cannot coexist. Neither wrapper is `Clone`, and no
        //     other code in this process maps or writes the file (the
        //     server closes its fd after mapping; the client's `RawMap`
        //     is the only writer it has).
        //     Across processes — **the residual**, stated plainly: the
        //     client (or anyone it gave the fd to) can write these pages
        //     while this slice is live. No seal closes that without also
        //     forbidding the client's own writes, which is the point of
        //     the buffer; every shared-memory window protocol has the same
        //     contract. Formally a `&[u8]` promises immutability and a
        //     foreign process is outside the abstract machine. What keeps
        //     the consequence bounded to "a torn or stale pixel" rather
        //     than memory unsafety is the *consumer*: the server's only
        //     readers are the raster blits, which load each source byte
        //     and use the loaded value in arithmetic — no bounds or index
        //     computation depends on a pixel's value (verified by reading
        //     `nitro-raster/src/canvas.rs`: `blit_1to1`, `blit_scaled`
        //     and `bilinear` index by geometry only; the `u64` widening
        //     copy masks the value; alpha feeds `over_*`/`div255`
        //     multiplies, and there is no palette or LUT keyed by a source
        //     byte). The one value-dependent *branch* is the `alpha == 0`
        //     skip in `blit_1to1`/`bilinear`/`blend_texel`; both arms are
        //     plain arithmetic on the destination, so a torn alpha picks
        //     a different sound arm, not an out-of-range access. Every
        //     loaded value is a valid `u8` by (c). A
        //     hostile client can therefore corrupt its own window's pixels
        //     and nothing else. Well-behaved clients (the bench, the
        //     toolkit) write only between `Frame` and `Commit`, so they
        //     never race the paint. Two further cases of the same class:
        //     `fallocate(PUNCH_HOLE)` is not blocked by these seals and
        //     makes the punched pages read as zeros (verified; no fault);
        //     and a process that maps the same file twice has two virtual
        //     aliases of one physical page, which is another foreign-write
        //     alias as far as either slice can tell.
        // (e) `len` is a `usize` that `mmap` accepted; the kernel refuses
        //     lengths past the address-space limit, which is below
        //     `isize::MAX` on every Linux target.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The mapped bytes, writable. Only [`MappingMut`] calls this, and it
    /// is the wrapper that mapped with `PROT_WRITE`.
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY (from_raw_parts_mut): (a), (b), (c) and (e) exactly as in
        // `bytes` above, plus the pages are mapped writable — the only
        // caller is `MappingMut::as_bytes_mut`, and `MappingMut::map_mut`
        // is the only constructor of a `RawMap` with `PROT_WRITE`.
        // Uniqueness within this process: this method takes `&mut self`,
        // so no `&[u8]` from `bytes` and no other `&mut [u8]` from here can
        // be alive at the same time; and the same `Drop`-takes-`&mut self`
        // argument keeps the range mapped while the slice lives. The
        // cross-process residual is the same as for `bytes`, in the other
        // direction: the server reads these pages (read-only) while the
        // client writes them, and a torn read on the server's side is the
        // server's residual, not a hazard to this slice.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for RawMap {
    fn drop(&mut self) {
        // SAFETY (munmap): `ptr` and `len` are exactly what `mmap` returned
        // and was given in `RawMap::map`, the only place a `RawMap` is
        // built, and they have not changed since (both fields are private
        // and never assigned after construction). The range is unmapped
        // exactly once: `Drop` runs at most once per value, the type is
        // not `Clone` or `Copy`, and this is the tree's only `munmap`. No
        // reference into the range can be live: `bytes`/`bytes_mut` borrow
        // `self`, and `drop` takes `&mut self`, which excludes them. The
        // `Drop` cannot be skipped by any path that also keeps the value:
        // only `mem::forget` (or a leak of a `Box`) would leave the mapping
        // in place, and nothing in the tree calls either on a mapping —
        // `tests/seals.rs` `dropping_a_mapping_unmaps_it` checks that the
        // `/proc/self/maps` entry goes away.
        //
        // The result is ignored on purpose: `munmap` fails only for an
        // unaligned or unmapped range, neither of which this can be, and
        // there is nothing a destructor could do with the error anyway.
        let _ = unsafe { rustix::mm::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

impl fmt::Debug for RawMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawMap")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .field("file_len", &self.file_len)
            .finish()
    }
}

/// A read-only mapping of a sealed buffer: the server's view of a client's
/// pixels.
///
/// Consumes the fd. The mapping holds its own reference to the file, so
/// the descriptor is closed as soon as the map succeeds — one fewer fd
/// per buffer than the `pread` path kept, and nothing for the server to
/// remember to release on `DestroyBuffer` beyond dropping this.
///
/// The bytes are **live**: what the client writes into the file after
/// this is what [`as_bytes`](Self::as_bytes) sees, with no re-read. The
/// concurrency contract that implies is stated in the crate docs and in
/// `docs/wire.md` under `CreateBuffer`.
#[derive(Debug)]
pub struct Mapping {
    raw: RawMap,
}

impl Mapping {
    /// Map the first `len` bytes of `fd` read-only.
    ///
    /// # Errors
    /// [`MapError::Seals`] if the fd does not carry
    /// [`REQUIRED_SEALS`](crate::REQUIRED_SEALS) — checked with the kernel,
    /// so a client that sealed nothing is refused however honest its
    /// message looked; [`MapError::TooShort`] if the file is shorter than
    /// `len`; [`MapError::Os`] if `fstat` or `mmap` fails, or `len == 0`.
    pub fn map(fd: OwnedFd, len: usize) -> Result<Self, MapError> {
        let raw = RawMap::map(fd.as_fd(), len, ProtFlags::READ)?;
        // `fd` drops here: the mapping does not need it. Checked by
        // `tests/seals.rs` `the_fd_can_be_closed_once_mapped`.
        drop(fd);
        Ok(Self { raw })
    }

    /// The mapped bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.raw.bytes()
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

    /// The file's size, which the seals make a constant.
    #[must_use]
    pub fn file_len(&self) -> u64 {
        self.raw.file_len
    }
}

/// A read/write mapping of a sealed buffer: the client's own view, so it
/// can render straight into the pages the server will read.
///
/// Borrows the fd rather than consuming it, because the client typically
/// maps first and then moves the descriptor into `CreateBuffer`. The
/// mapping outlives the descriptor for the same reason [`Mapping`] can
/// close it.
///
/// The seals are required here too, not only on the server's side: the
/// argument that a load through the mapping cannot `SIGBUS` is the same
/// argument, and a client that mapped an unsealed file would be one
/// `ftruncate` (its own, or that of whoever it gave the fd to) from a
/// crash. [`create_sealed`](crate::create_sealed) is the intended source.
#[derive(Debug)]
pub struct MappingMut {
    raw: RawMap,
}

impl MappingMut {
    /// Map the first `len` bytes of `fd` read/write.
    ///
    /// # Errors
    /// As [`Mapping::map`].
    pub fn map_mut(fd: BorrowedFd<'_>, len: usize) -> Result<Self, MapError> {
        let raw = RawMap::map(fd, len, ProtFlags::READ | ProtFlags::WRITE)?;
        Ok(Self { raw })
    }

    /// The mapped bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.raw.bytes()
    }

    /// The mapped bytes, writable. Writes land in the file at once; the
    /// server sees them at its next paint, with no upload.
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.raw.bytes_mut()
    }

    /// Length of the mapping in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len
    }

    /// Always `false`: a zero-length mapping is refused at
    /// [`map_mut`](Self::map_mut).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.len == 0
    }
}
