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
| `input` (+ `input-sys`) | server | Bindings to libinput, which is the only sane way to read evdev: tap detection, pointer acceleration, scroll-source classification and touchpad state are thousands of lines of hard-won device quirks we are not going to re-derive. `default-features = false, features = ["libinput_1_21"]` — the `udev` feature is **off**, so `libudev` never enters the tree: the server finds devices by reading `/dev/input` and opens them through `nitro-seat`. | + `libc` (already there via `libseat`). The FFI `unsafe` lives in the dependency; `LibinputInterface` is a safe trait we implement. Input-device hotplug is M3: it needs the netlink uevent socket `nitro-kms` already has, plus a directory diff. |
| `xkbcommon` | server | Keycode → keysym → UTF-8 with the user's own layout, dead keys, levels and modifier semantics. The alternative is shipping a keymap format and a compose engine, which is a project, not a dependency. It reads `XKB_DEFAULT_*`, so it honours whatever the user already configured. | + `xkeysym`, `memmap2`. The FFI `unsafe` (and the `mmap` of the keymap file) lives **inside the `xkbcommon` crate**, not in ours; our tree stays `unsafe`-free. |

Crate count: `cargo tree -e normal --prefix none | sort -u | wc -l` = **48**.
`input` and `xkbcommon` cost five of those between them (themselves plus
`input-sys`, `xkeysym`, `memmap2`); the rest of the rise since M0 is the
server now depending on every other nitro crate.

Planned (M2+): `parley` + `skrifa` (text shaping and layout).
Rejected: `serde` (hand-written wire), `png` (own stored-deflate encoder in
`nitro-shot`), `tokio`/`async-*` (single-threaded epoll loop), `winit`,
`wgpu`, `smithay`, `libudev` (a `read_dir` and a netlink socket do what we
need of it).

## `rustix` features by crate

The feature set is per-crate, not workspace-wide, so each pays only for
the syscall families it uses.

| crate | features | used for |
|---|---|---|
| `nitro-wire` | `event`, `fs`, `net`, `process` | `poll` for the blocking handshake; `memfd_create`/`fstat`/`ftruncate` (tests) and `unlinkat`/`mkdir` for the socket path; `socket`/`bind`/`listen`/`accept`/`sendmsg`/`recvmsg` + `SCM_RIGHTS`; `getuid` for the `/tmp` fallback path |
| `nitro-server` | `event`, `fs`, `net`, `process`, `time` | epoll loop, control socket, signals, timers; `pread` to copy client buffers out of their memfds, and `eventfd` for the test input source |
| `nitro-kms` | `event`, `fs`, `mm`, `net`, `time` | DRM fds, `mmap` of dumb buffers, udev netlink |

## `unsafe` exceptions

None in our code. The FFI-binding crates above (`libseat-sys`, `drm-ffi`,
`input-sys`, `xkbcommon`) contain their own, which is exactly why each is
listed here: it buys a kernel or C ABI we would otherwise have to write
`unsafe` ourselves to reach.

One deliberate *non*-use is worth recording. The server copies a client's
buffer out of its memfd with `pread` rather than mapping it. A mapping
would be one syscall and zero copies, but a client can shrink the memfd
under a live mapping and turn the server's next read into a SIGBUS — so a
safe mapping needs either enforced `F_SEAL_SHRINK` or a signal handler, and
`mmap` of a foreign fd is `unsafe` in our tree besides. `BufferDamage`
keeps the copy proportional to what actually changed. Revisit with sealing
when a client pushes video.
