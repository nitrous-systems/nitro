# nitro-shm

Sealed memfds, and the one sanctioned mapping of them. This is the crate
whose whole point is an argument, so the argument is here rather than only
in the code.

A client's pixel buffer is a memfd. Before #569 the client `pwrite`
its frame into that memfd and the server `pread` it back out, so every
frame crossed memory **three times** before the rasterizer touched it:
effect → memfd → server heap. On the test box a fullscreen 1080p frame
spent 4–6 ms of its 16.7 ms budget on those two copies alone (`docs/bench.md`,
§"#569"). Mapping the file removes two of the three passes. Mapping a file
another process controls is the one thing in this tree that needs `unsafe`.

## Layout

| file | `unsafe`? | what |
|---|---|---|
| `src/lib.rs` | no | `create_sealed`, `memfd_with`, `check_seals`, `sealed_len`, the error types |
| `src/map.rs` | **yes**, scoped | `Mapping` (read-only), `MappingMut` (read/write), and the four `unsafe` blocks with their proofs |
| `src/dmabuf.rs` | **yes**, scoped (#3914) | `DmaBufMapping` (a `RawMap` wrapper, no `unsafe` of its own), `sync_start`/`sync_end`, and the two `unsafe` ioctl blocks for `DMA_BUF_IOCTL_SYNC` |

The workspace lint is `unsafe_code = "deny"`; `src/map.rs` and
`src/dmabuf.rs` carry `#![allow(unsafe_code)]` and nothing else in the
crate does, so `unsafe`
anywhere else in it fails the build. (`Cargo.toml` must keep
`[lints] workspace = true`, or the `deny` never applies and the `allow`
allows nothing.) There are exactly four `unsafe` blocks in `map.rs`
(`mmap`, `slice::from_raw_parts`, `slice::from_raw_parts_mut`, `munmap`),
two `unsafe` ioctl blocks in `dmabuf.rs`, and exactly one `mmap` and one
`munmap` in the whole tree.

## The seals, and what each one is for

The hazard a mapping has and a `pread` does not: if the file is made
shorter than the mapping, touching a page past the new end raises
`SIGBUS`. A hostile client can `ftruncate` its own memfd whenever it
likes, so a read-only mapping of an **unsealed** fd is a way for any
client to kill the display server. That is not a hypothetical the seals
were bolted on to answer — it is the reason the old code copied, and it is
the thing the sealing has to actually close.

- **`F_SEAL_SHRINK`** — `ftruncate` to a smaller size fails with `EPERM`.
  The load-bearing one: it is what makes "every byte of the mapping is
  inside the file" true for the mapping's whole life rather than at the
  instant it was checked.
- **`F_SEAL_SEAL`** — no further seals can be added. What this buys is not
  "the client cannot unseal" (it never could: Linux has no operation that
  *removes* a seal) but "the seal set is final" — a client cannot add
  `F_SEAL_WRITE` after the server mapped the buffer and turn its own later
  writes into `EPERM`s. Without it the checked state is not the permanent
  state and the argument has a hole.
- **`F_SEAL_GROW`** — `ftruncate` to a larger size fails with `EPERM`.
  **Not** load-bearing for `SIGBUS`, and not here for symmetry: a longer
  file does not invalidate a fixed-length mapping of its start. It is
  required so that the size observed at check time is the file's size for
  its whole life, which is what lets a caller check a client's declared
  size once and rely on it; and because the protocol has no legitimate use
  for growing a buffer whose geometry was fixed at `CreateBuffer`.

Deliberately **not** included: `F_SEAL_WRITE`/`F_SEAL_FUTURE_WRITE`. The
whole point is that the client keeps writing the next frame into the same
pages.

## What the server does with an unsealed buffer: refuse it

`check_seals` asks the **kernel** (`F_GET_SEALS`) what is enforced on the
inode behind the fd, not the client what it did. Seals are a property of
the inode, so the answer is the same whichever process and whichever
descriptor asks — which is what makes it a proof rather than a courtesy.

There is no `pread` fallback. A fallback would make the mapped path's
safety argument untestable from outside (which path did the server take?)
and leave the copying path rotting unexercised. An unsealed buffer is a
`BadBuffer` protocol error, like any other bad buffer.

## The residual — what sealing does *not* close

Stated plainly, because an honest "this remains possible, and here is why
it is acceptable" is worth more than a false total:

1. **Another process writes the pages while the server reads them.** No
   seal closes this without also forbidding the client's writes, and it is
   inherent to every shared-memory buffer protocol (`wl_shm` has the same
   contract). Formally, Rust's `&[u8]` promises the bytes do not change; a
   foreign process is outside the abstract machine. What keeps the
   consequence bounded to *a torn or stale pixel* rather than memory
   unsafety is the consumer: the server's only readers are the raster
   blits, which load each source byte and use the loaded value in
   arithmetic. No bounds or index computation depends on a pixel's value
   (checked against `nitro-raster/src/canvas.rs`: `blit_1to1`,
   `blit_scaled` and `bilinear` index by geometry only; there is no
   palette or LUT keyed by a source byte), and every bit pattern is a
   valid `u8`. The one value-dependent *branch* is the `alpha == 0` skip,
   whose arms are both plain arithmetic. So a hostile client can corrupt
   its own window's pixels and nothing else.
2. **`fallocate(PUNCH_HOLE)`** is not blocked by these seals. Punched
   pages read back as zeros (verified on the box's kernel), not `SIGBUS` —
   same class as (1).
3. **Address space.** A client can ask for many 64 MiB buffers;
   `mmap` failing with `ENOMEM` is a protocol error and a disconnect, not
   unsoundness. A cap now bounds it: `MAX_BUFFERS_PER_CLIENT` and
   `MAX_MAPPED_BYTES_PER_CLIENT` in `nitro-server/src/clients.rs` refuse
   a client holding too many buffers or bytes with `ErrorCode::Limit`,
   and `MAX_MAPPED_BYTES_ALL_CLIENTS` bounds the server-wide total. The
   residual is a client exhausting *its own* budget and being dropped.
4. **Memory pressure** on shmem faults is an OOM kill, not a signal on our
   thread, and is outside what seals can address.

## Scanout buffers: dma-bufs (#3914)

The server can hand a client a **dma-buf** — a PRIME export of a linear
dumb buffer it allocated through KMS — for the client to decode into
(`AllocSurfaceBuffers` in `docs/wire.md`). Both ends map it through the
same `RawMap`: the client with `DmaBufMapping` (read/write), the server
with `Mapping::map_dmabuf` (read-only, like every other buffer it reads).

**Why no seals.** A dma-buf cannot be sealed, and does not need to be:
its size is set by the exporter when the file is created, and the dma-buf
file has no truncate and no `fallocate`, so nobody can make it shorter
than a mapping of it — the property `F_SEAL_SHRINK` gives a memfd, here
by the file's type. `RawMap` recognises a dma-buf by asking the kernel
which filesystem the inode lives on (`fstatfs` → `DMA_BUF_MAGIC`); a
client cannot fake that. Anything that is not a dma-buf goes through the
seal check as before, and the fake KMS backend exports exactly such a
sealed memfd. The size check uses `fstat`, not `lseek(SEEK_END)`: the
file offset is shared with the client's duplicate and a check has no
business moving it.

**The sync bracket.** `sync_start(fd, access)` / `sync_end(fd, access)`
issue `DMA_BUF_IOCTL_SYNC` (`_IOW('b', 0, u64)`, flags `START`=0,
`END`=4, `READ`=1, `WRITE`=2) — the crate's two `unsafe` ioctl blocks,
with their argument in `dmabuf.rs`. `ENOTTY` (a memfd, a socket) returns
`Ok(false)`, "no sync needed"; `EINTR`/`EAGAIN` are retried. It is a
cache-coherency hint to the exporter, not a lock.

**The residual.** The same as (1) above, from the other side: a client
that writes outside the bracket, or after `PresentSurface` and before
`BufferReleased`, tears its own frame. The server's readers index by
geometry only, so nothing worse follows.

## Tests

`tests/seals.rs`, all real-kernel, 25 of them, and `tests/dmabuf.rs`
(#3914: the memfd and `ENOTTY` paths of the dma-buf mapping and sync; a
real dma-buf needs a DRM or udmabuf exporter, exercised on hardware). The ones that carry the
argument:

- **per-seal negatives** — each required seal missing on its own is
  refused and *named*; a plain `memfd_create` without `MFD_ALLOW_SEALING`
  is refused; a regular file is `Unsealable`.
- **the hostile case** — `ftruncate` smaller on a sealed fd returns
  `EPERM`, and the mapping is still fully readable afterwards. This is the
  positive proof the seal is *in force* rather than merely reported, which
  is the difference the whole safety argument turns on. Growing and adding
  seals are `EPERM` too.
- **the golden** — mapped bytes equal a `pread` copy byte for byte, and a
  write *after* mapping is visible with no re-read.
- **no leak** — the `/proc/self/maps` entry appears on map and is gone
  after `drop`; 64 map/drop cycles leave nothing behind.
- **fd lifetime** — the descriptor is closed once mapped and the bytes are
  still readable, because the mapping pins the inode.

**Miri cannot run any of it**: `memfd_create`, `F_ADD_SEALS`,
`F_GET_SEALS` and file-backed `mmap` have no Miri shims, and rustix's
`linux_raw` backend issues syscalls through inline asm Miri cannot
execute. That is why these are real-kernel tests asserting kernel
behaviour rather than a model of it.
