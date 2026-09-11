# Dependencies

Every external crate and every `unsafe` exception is listed here with the
reason it earns its place. Adding one is a design decision, not a
convenience. `cargo tree -e normal --prefix none | sort -u | wc -l` is the
number we watch.

## Crates

| crate | used by | why | cost / notes |
|---|---|---|---|
| `rustix` | seat, kms, server, wire | Safe Linux syscalls (epoll, mmap, sockets + `SCM_RIGHTS`, timerfd, netlink) with no libc. The one crate that lets the rest of the tree be `unsafe`-free. | + `bitflags`, `linux-raw-sys` |
| `zerocopy` (+ `zerocopy-derive`) | wire | The wire format *is* `#[repr(C)]` layout: `U32<LittleEndian>`/`F32<LE>`/… give guaranteed little-endian fields, `Unaligned` lets a payload be decoded in place from any `&[u8]`, and `ref_from_bytes`/`as_bytes` replace the pointer casts we would otherwise write by hand. Validated, total, and `unsafe`-free in our tree. | +3 crates: `zerocopy`, `zerocopy-derive`, and **`syn` 2.x**. Note `drm` → `bytemuck_derive` pins `syn` **3.x**, so the two do *not* share a build: syn is compiled twice. Revisit if compile time hurts. |
| `drm` (+ `drm-ffi`, `drm-sys`, `drm-fourcc`) | kms | Safe wrappers over the ~30 DRM/KMS ioctls (atomic commit, dumb buffers, AddFB2, properties, events). Hand-rolling them is precisely the `unsafe` we forbid. | pulls `bytemuck` + `bytemuck_derive` → `syn` (proc-macro, compile time). Revisit if it hurts. |
| `signal-hook` (+ `signal-hook-registry`) | server | SIGTERM/SIGINT → self-pipe without `unsafe` in our tree: `sigaction` and an async-signal-safe handler are exactly the shim we would otherwise have to write ourselves. `default-features = false` (no iterator/channel). | + `libc` (already pulled by `libseat`). Only `low_level::pipe::register` is used. |
| `libseat` (+ `libseat-sys`) | seat | Bindings to the C libseat: one interface over logind / seatd / raw VT for DRM master + input fds without root. The single deliberate C dependency. | + `errno`, `libc`, `log`. `default-features = false`: the `custom_logger` feature builds a C shim (`cc`) to route libseat's log lines through `log`; we do not log. |

Planned (M1+): `input` (libinput), `xkbcommon`, `parley` + `skrifa` (text).
Rejected: `serde` (hand-written wire), `png` (own stored-deflate encoder in
`nitro-shot`), `tokio`/`async-*` (single-threaded epoll loop), `winit`,
`wgpu`, `smithay`.

## `rustix` features by crate

The feature set is per-crate, not workspace-wide, so each pays only for
the syscall families it uses.

| crate | features | used for |
|---|---|---|
| `nitro-wire` | `event`, `fs`, `net`, `process` | `poll` for the blocking handshake; `memfd_create`/`fstat`/`ftruncate` (tests) and `unlinkat`/`mkdir` for the socket path; `socket`/`bind`/`listen`/`accept`/`sendmsg`/`recvmsg` + `SCM_RIGHTS`; `getuid` for the `/tmp` fallback path |
| `nitro-server` | `event`, `fs`, `net`, `process`, `time` | epoll loop, control socket, signals, timers |
| `nitro-kms` | `event`, `fs`, `mm`, `net`, `time` | DRM fds, `mmap` of dumb buffers, udev netlink |

## `unsafe` exceptions

None in our code yet. FFI-binding crates above contain their own.
