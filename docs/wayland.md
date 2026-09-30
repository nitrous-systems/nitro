# nitro-wayland: the Wayland adapter

A design record, not a status report. Nothing on this page is built yet.
It decides how `nitro-wayland` is built, so that the work items that
implement it (tag `wayland` on the task board, #3993–#3998) all work from
the same picture. Numbers are marked **measured**, with the machine they
were measured on, or **estimate**. The design is laid out in the same
order the decisions were taken: dependency, process model, buffers,
protocol subset, tests, work plan.

`DESIGN.md` § Adapters states the premise. `nitro-wayland` is a
**separate process**. It speaks Wayland to legacy clients and turns each
surface into ordinary nitro wire objects, so the server never learns what
a `wl_surface` is. Everything below keeps to that. Where the wire cannot
express something Wayland needs, it is listed as a **wire gap** (§ 7) and
is not smuggled into the adapter as a workaround.

## Decisions in one screen

| # | question | decision |
|---|---|---|
| 1 | protocol core | **`wayland-server` 0.31 with its pure-Rust backend** (`wayland-backend` `rs/`), `default-features = false`, plus `wayland-protocols` for xdg/wp/zwp. +8 external names, linked only into `nitro-wayland`. `server_system` (libwayland) and a hand-rolled core are both rejected. |
| 2 | process model | **one adapter per session, one nitro connection per Wayland client.** `nitro-session` binds `$XDG_RUNTIME_DIR/wayland-N` itself and hands the listening socket to the adapter as its stdin. `WAYLAND_DISPLAY` is set for every piece except the server and the adapter. |
| 3 | shm buffers | **copied, never mapped.** `pread` of the damaged rows from the client's pool into the adapter's own sealed memfd, which becomes an **`Image`** node. The adapter maps no client memory, so it has no `SIGBUS` exposure and is `#![forbid(unsafe_code)]`. Measured on box1: 2.7 ms for a full 1080p frame, 19 µs for a 1920×20 terminal line. |
| 4 | dma-buf buffers | a **`Surface`** node, `CreateDmabufBuffer` + `PresentSurface` with **implicit sync** (the server snapshots the fence). `wp_linux_drm_syncobj` is not advertised. |
| 5 | serials | the adapter mints them and never checks them. Every request that carries one is forwarded, and the server's focus checks (`wire.md` L3833) decide. |
| 6 | subsurfaces | nodes in the parent window's tree. Basic support moves **into W1**. |
| 7 | decorations | server-side whenever the client negotiates it (`xdg-decoration`). Otherwise the window is `UNDECORATED`, and the adapter tells the client it is tiled on all four edges so that CSD toolkits drop their shadows. |
| 8 | out of scope | Xwayland (§ 4.12), text-input/IME, pointer constraints, layer-shell, session-lock. |

---

## 1. Dependency

### What was measured

A scratch crate outside the workspace (`[workspace]` table of its own,
removed afterwards) depended on `wayland-server = { version = "0.31",
default-features = false }` and `wayland-protocols = { version = "0.32",
default-features = false, features = ["server", "staging", "unstable"] }`.
It registered eight globals: compositor, shm, seat, output,
subcompositor, data device, `xdg_wm_base` and `zwp_linux_dmabuf_v1`. The
dev box is 128-core EPYC 7B13, rustc 1.97, and the release profile was
the workspace's own (`lto = "fat"`, `codegen-units = 1`,
`panic = "abort"`, `strip`).

| | measured |
|---|---|
| resolved | `wayland-server` 0.31.14, `wayland-backend` 0.3.17, `wayland-protocols` 0.32.13, `wayland-scanner` 0.31.11, `wayland-sys` 0.31.11 |
| **net-new external names** against the current tree (`cargo tree -e normal`) | **8**: `wayland-server`, `wayland-backend`, `wayland-sys`, `wayland-scanner`, `wayland-protocols`, `quick-xml`, `downcast-rs`, `smallvec`. Already present: `rustix`, `bitflags`, `linux-raw-sys`, `memchr`, `proc-macro2`, `quote`, `unicode-ident` |
| build-only additions | `cc` (+ `shlex`, `find-msvc-tools`): an **unconditional** build-dependency of `wayland-backend`, whose script compiles a C log shim **only** with the `log` feature. With that feature off, the script runs, compiles nothing and links nothing. `pkg-config` (for `wayland-sys`) is already in `Cargo.lock` and probes nothing unless a `client`/`server` feature is on. |
| `Cargo.lock` | +11 entries (the 8, plus `cc`, `shlex`, `find-msvc-tools`); +1 more with the dev-dependency below |
| dev-dependency | **`wayland-client`** 0.31.15, the only extra name (it shares `wayland-backend`) |
| workspace `cargo tree -e normal --prefix none \| sort -u` | 105 lines / 44 external names today → **52 names**. The line count is an estimate of about +12, and W1 measures it when the crate lands |
| scratch binary, stripped | **541 528 B**, dynamically linked against libc and libgcc_s only. `ldd` shows no libwayland |
| scratch clean release build | 14.25 s wall, 19.1 s user, for all of it including the dependencies |

**`unsafe` that ships.** `wayland-backend`'s `rs/` backend has **10**
`unsafe` sites. Most are `BorrowedFd::borrow_raw` on descriptors it just
received, plus one slice cast of `&[RawFd]` to `&[BorrowedFd]`. Two
`kevent` calls are BSD-only and not compiled here. The crate's other
~200 sites are in `sys/`, the libwayland backend, which is not built.
`wayland-server` has 2. `wayland-sys` is compiled for its type
definitions only: its FFI modules are behind the `server`/`client`
features. `smallvec` has 83. It is the most-used small-vector crate in
the ecosystem, and it enters our tree here for the first time.
`quick-xml` and `wayland-scanner` run **at compile time only**: the
scanner is a proc-macro that turns the protocol XML into Rust, and
neither of them is in the binary.

### Rejected: `server_system` (libwayland-server)

This is the same crate with the C backend. It would make libwayland the
tree's **third deliberate C dependency** after libseat and libpam,
probed through `pkg-config` at build time. Every callback crosses FFI,
and those are the ~200 `unsafe` sites in `sys/`. It buys only
bug-for-bug compatibility with the reference implementation. The real-app
matrix (§ 5.2) tests against libwayland *clients* anyway, which is where
that compatibility matters.

### Rejected: a hand-rolled protocol core

The wire format really is simple. It has an 8-byte header (object id,
then opcode and size), 32-bit arguments, length-prefixed strings and
arrays, and fds in `SCM_RIGHTS`. **Measured**, the protocol files the
subset below uses (`wayland.xml`, xdg-shell, xdg-decoration, xdg-output,
viewporter, fractional-scale, cursor-shape, primary-selection,
linux-dmabuf, presentation-time, xdg-activation, idle-inhibit,
single-pixel-buffer) come to **52 interfaces, 163 requests, 99 events and
377 arguments**. Transcribing them is **~2 000 lines, estimate**. The
codec, object map and fd queue add ~800 more. That puts it around the
size of `nitro-png`, which was the right call in #3711. It is the wrong
call here, for three reasons:

1. **The hard part is not the codec, it is object lifecycle.** A client
   can send requests to an object the server has just destroyed, while
   the destructor event is still in flight. Ids are reused only after
   `wl_display.delete_id`. Every object carries a version, and events
   newer than that version must not be sent. `new_id` arguments with a
   dynamic interface (`wl_registry.bind`) and fds split across
   `sendmsg`s (wayland-rs caps a send at 28) are further traps. smithay,
   niri and cosmic have already paid for those bugs in `wayland-backend`.
2. **The png lesson points the other way here.** #3711 found its `tRNS`
   bug only by comparing against a mature decoder. If a hand-rolled server
   is tested with a hand-rolled client, both carry the same misreading of
   the spec. We would need `wayland-client` as the reference client
   anyway, which pays most of the dependency cost for the test half alone.
3. **It is binary-isolated.** As with `ash` in `nitro-gpu-vulkan`, no
   other binary links it. Every other nitro binary stays byte-identical,
   the server never sees a Wayland type, and removing the adapter would
   remove the dependency.

`smithay` stays under Rejected in `DEPENDENCIES.md`. It is a compositor
framework: renderer, backend, input and desktop abstractions. We already
have a compositor. We need only its bottom layer, and that layer is
`wayland-server`.

**Decision:** `wayland-server` + `wayland-protocols`, pure-Rust backend,
`default-features = false`, recorded in `DEPENDENCIES.md` as a
**proposed** row. It goes into `Cargo.toml` with W1 (#3993) and not
before. `wayland-client` + `wayland-protocols/client` are dev-dependencies
of `nitro-wayland` only.

---

## 2. Process model

### One adapter per session, one nitro connection per Wayland client

The adapter is **one process** that owns the Wayland socket. For each
Wayland client it accepts, it opens **one nitro wire connection** to
`wire.sock`. This is not a multiplexed connection, and the reasons are
the wire's own rules:

- **Authorization is per connection.** `SetCursor` is honoured for the
  client that holds pointer focus, and `SetSelection` for the one that
  holds keyboard focus (`wire.md` L3849ff). With one multiplexed
  connection, every Wayland client would share every other's focus. A
  background app could take the clipboard while a sibling had focus, and
  the server could not tell the two apart.
- **Errors are fatal per connection.** A malformed buffer from one app
  would close the shared connection and take every Wayland window down
  with it.
- **Limits are per connection:** 32 buffers and 128 MiB per client
  (`wire.md` L1338). Three browser windows multiplexed would reach the
  cap together.
- **Kill semantics come for free.** When a Wayland client disconnects,
  the adapter drops its nitro connection and the server reclaims every
  node and buffer. The adapter needs no per-app cleanup code.
- **Ids need no translation.** Each connection has its own node and
  buffer id space, allocated by the adapter from 1.

The cost is one socket and one server-side client struct per app. That
is a few kB, against apps that each hold megabytes of buffers.

`Hello.name` is `wayland:<comm>`, where `<comm>` is read from
`/proc/<pid>/comm` of the peer (`SO_PEERCRED`). That name is what the
server logs, `nitro-hey` and `stats` show. Each connection lists
`POPUP | CURSOR | DRAG | OUTPUTS | KEYMAP | RELEASE | DATA |
OPAQUE_REGION | SURFACE` in `ClientCaps` (+ `DMABUF` from W5). It does
not list `TEXT` or `ICONS`: a Wayland client draws its own. `KEYMAP`
matters for a second reason: it switches off the server's synthesized key
repeat for this connection (`wire.md` L1961). A Wayland client repeats on
its own, using `wl_keyboard.repeat_info`.

**Where the adapter keeps state.** The adapter has **one** event loop:
an epoll over the Wayland listening socket, every Wayland client and
every nitro connection. It also keeps one extra nitro connection of its
own, the **watcher**, which uses `ListOutputs` to maintain the output
list that every Wayland client's `wl_output` globals are built from. The
watcher opens no windows.

### Socket naming, and who binds it

**`nitro-session` binds the socket, not the adapter.** At session start
it goes through `$XDG_RUNTIME_DIR/wayland-N` for N = 1, 2, …, 32. For
each N it takes an `flock(LOCK_EX | LOCK_NB)` on `wayland-N.lock`, which
libwayland's convention and `wayland-server`'s `bind_auto` also use. The
first lock it gets decides the name. It unlinks a stale socket with that
name, binds, and listens. N starts at 1 because `wayland-0` is often held
by a desktop the developer is nested in. The session keeps the lock fd
and the listening socket **for its whole life**, and gives the listener
to the adapter as the adapter's **stdin**:
`Command::stdin(Stdio::from(owned_fd))`. That is safe std, and it is the
precedent `nitro-gpu` set with `dup2_stdin`. The adapter accepts from fd
0 and hands each stream to `DisplayHandle::insert_client`. It never
needs `ListeningSocket`.

Why the session and not the adapter:

- **`WAYLAND_DISPLAY` is known before anything is spawned.** The session
  sets it in the environment of every child it starts.
  `spawn_with_env`, `crates/nitro-session/src/child.rs:159`, already
  sets `PATH` and `XDG_CURRENT_DESKTOP` for children. The launcher
  inherits it, and so does every app the launcher starts. Nothing has to
  read back a name the adapter chose.
- **The name survives an adapter restart.** While the adapter is down,
  new connections wait in the listen backlog rather than failing with
  `ENOENT`. The restarted adapter accepts them, so an app launched during
  a restart simply starts late.
- **No second session can take the name.** The kernel drops the lock
  when the session exits, however it exits.

The adapter binds its own socket (`bind_auto`, `wayland-1..32`) and
prints the name only when it is run by hand, for development: when fd 0
is not a listening socket or `--display NAME` is given. That path uses the
same code, apart from where the listener comes from.

**Environment.** The session sets `WAYLAND_DISPLAY=wayland-N` for every
piece except `nitro-server` and `nitro-wayland`. When the session itself
runs nested under another Wayland desktop, an inherited `WAYLAND_DISPLAY`
is **overwritten**, never passed on. Otherwise an app would open on the
outer desktop. `DISPLAY` is left unset (there is no Xwayland), and that is
what makes GTK, Qt and Firefox choose their Wayland backends. W6 checks
whether Qt 6 also needs `XDG_SESSION_TYPE=wayland` or
`QT_QPA_PLATFORM=wayland` on the boxes. If it does, the session sets the
variable for apps only and records the reason next to it.

### Start order, crash and restart

- The adapter is a session piece listed right after `SERVER` in `PIECES`
  (`crates/nitro-session/src/pieces.rs:146`). It is not a shell piece:
  `NITRO_SESSION_PIECES` does not select it, and `--greeter` does not
  start it. It connects to the server like any client, after the session
  has seen `wire.sock` (`wait_for_sockets`,
  `crates/nitro-session/src/wait.rs:155`).
- **The adapter crashes.** Every Wayland client loses its connection, as
  it would if any compositor crashed. GTK and Qt apps exit, foot exits.
  Their nitro connections close with them, so their windows vanish.
  Native nitro clients are unaffected, which is the point of keeping
  Wayland out of the server. The session restarts the adapter with the
  shell backoff (`crate::backoff`). The socket and lock stay held, so
  the next launch works.
- **The server exits or crashes.** Every nitro connection closes, the
  adapter disconnects every Wayland client and exits 1, and the session
  ends as it does today.
- **A Wayland client misbehaves.** It gets a `wl_display.error` and is
  disconnected. Its nitro connection closes. Nobody else notices.
- **Nitro rejects something.** The error kills only that client's
  connection, and the adapter posts `wl_display.error`
  (`implementation`) on the Wayland side. Treat this as an adapter bug:
  the adapter validates everything the server would refuse before
  sending it (§ 3.4).

### The launcher

`NATIVE_PROGRAMS` (`crates/nitro-launcher/src/lib.rs:545`, #3842) and
`with_native_only` exist only because foreign `.desktop` entries could
not show a window. W6 removes the filter **when `WAYLAND_DISPLAY` is set
in the launcher's environment**, which is how the session says "an
adapter is running". It keeps the filter when the session runs without
the adapter piece. `xdg_toplevel.set_app_id` becomes `SetAppId`, so the
bar's task list and icon lookup work for Wayland apps just as they do
for native ones.

---

## 3. Buffers

### 3.1 wl_shm: copy with `pread`, into the adapter's own sealed memfds

The server refuses any buffer that is not a sealed memfd (`wire.md`
L1318–1372). The reason is `SIGBUS`: a client that shrinks a file under
the server's mapping would kill the compositor. `wl_shm` pools are
ordinary fds. Clients grow them (`wl_shm_pool.resize`) and nothing stops
them shrinking them. So a pool cannot be forwarded, and a copy is
needed somewhere. The options:

| option | verdict |
|---|---|
| **A. `pread` the damaged rows from the pool fd into the adapter's own sealed memfd** | **Chosen.** No mapping of client memory at all. A client that truncated its pool gets a short read, and the adapter answers with `wl_shm` `invalid_fd` / `wl_display.error`. That is a protocol error for that client and never a signal. The adapter stays `#![forbid(unsafe_code)]`. |
| B. `mmap` the pool and survive `SIGBUS` | Rejected. weston and wlroots install a `SIGBUS` handler that `mmap`s zero pages over the faulting range (`MAP_FIXED`) and flags the pool. That needs a signal handler touching per-pool state, `MAP_FIXED` from inside it, and a thread-local "which pool am I reading" set around every copy. That is a new `unsafe` exception, and one of the worst kind: async-signal context. The measurements below show that it buys nothing on the box. |
| C. Forward sealed pools as they are | Rejected. A pool is many buffers at offsets, `CreateBuffer` has no offset field, and a pool sealed against `GROW` could not be resized. No client seals one. |

**Measured** on box1 (Pentium G3240, Haswell, 2 cores, no AVX2, kernel
7.0). The source was a 1920×1080×4 file on tmpfs, the median of 41 runs,
and the numbers were repeated twice. "rows" copies `h` separate row
spans, as a damaged sub-rectangle needs. A full-width rect is one `pread`.

| damage | bytes | `pread` rows (box1) | `memcpy` from a mapping (box1) | dev box `memcpy` |
|---|---|---|---|---|
| full 1920×1080 | 8.1 MB | **2.7 ms** (3.1 GB/s) | 2.96 ms (2.8 GB/s) | 0.31 ms |
| 800×600 window | 1.9 MB | **0.85 ms** | 0.48–0.74 ms | 0.10 ms |
| terminal line 1920×20 | 150 KB | **0.019 ms** | 0.009 ms | 0.003 ms |
| cursor-blink 64×64 | 16 KB | **0.040 ms** | 0.001 ms | — |

At window sizes, memory bandwidth dominates on the box, and `pread`
**equals or beats** `memcpy` from a mapping. For small rects the cost is
the syscall per row, about 0.6 µs per row, so 64 rows cost 40 µs. That
is well under a millisecond, and it only matters for rects that are both
narrow and tall. W1 may coalesce such a rect into one `pread` covering the
whole row span when that is cheaper (bytes read < 2× the rect). A
fullscreen 1080p client that repaints everything costs **~2.7 ms of the
16.7 ms frame**, in the adapter process and on the second core. The
server does not pay it.

**Memory is the real cost, not CPU.** Each shm window holds a second copy
of its pixels in the adapter: **two** sealed memfds per surface
(§ 3.2), which is 16 MB for a maximized 1080p window and 3.8 MB for an
800×600 terminal. That counts against the per-client 128 MiB cap, which
is why the cap has to be per client (§ 2). `nitro-shm`'s
`create_sealed` + `MappingMut` (`crates/nitro-shm/src/map.rs:424`) are
the adapter's only mapping. They are the tree's existing, audited one,
and they map the adapter's **own** sealed file. A W6 `footprint` run
records the adapter's RSS with the matrix apps open.

### 3.2 Which node, and the release rule

An shm surface is an **`Image`** node. This is the transactional path:
`SetImage` + `BufferDamage` go inside the same `Commit` as the
surface's geometry and its subsurfaces. That is exactly
`wl_surface.commit`'s atomicity, and the swap rule is already "Wayland's
`attach` + `damage` semantics" (`wire.md` L1415ff). A `Surface` node
would bring the latch, which is not transactional. It is the right thing
for dma-bufs (§ 3.3) and the wrong thing here.

On each `wl_surface.commit` with an shm buffer attached:

1. Choose the adapter buffer to write. Every surface has **two** sealed
   memfds (`AR24` or `XR24`, sized to the client buffer). Take the one
   not currently shown, which has had its `BufferReleased`.
2. Copy into it the union of **this commit's damage and the damage of
   the commit before**. This is buffer age 2: the other buffer is one
   frame behind. `damage` (surface coordinates) and `damage_buffer`
   (buffer coordinates) are both converted to buffer pixels, and so is
   the scale.
3. **Send `wl_buffer.release` right away.** The adapter has its copy, so
   the client may redraw at once. That is better than any mapping
   compositor can do, and it lets a two-buffer client run without
   stalling.
4. `SetImage { id, buffer, src }` + `BufferDamage { this commit's rects }`
   in the commit batch, then `Commit`.

If neither adapter buffer has been released yet, meaning the server
still reads both, the adapter allocates a **third** buffer rather than
stalling the client. The cap is 3 per surface. At the cap it holds the
commit until a `BufferReleased` arrives, and only then sends the client's
release. This is the only case where nitro's `BufferReleased` reaches a
Wayland client, and only indirectly.

**Alpha.** `wl_shm` `ARGB8888` is **premultiplied**. `CreateBuffer`
`AR24` is **straight** (`wire.md` L569; dma-buf `AR24` is premultiplied
since #3921, but a memfd is not). The adapter un-premultiplies while it
copies. It skips alpha 0 and 255, and it skips anything inside the
declared opaque region. **Measured** on box1: 1.6 ms per 1080p frame when
every pixel is opaque (the check alone), and **9.9 ms** when every pixel
is translucent. Real windows are opaque apart from rounded corners and
shadows, so the common cost is the first figure, and in practice it is
paid only on damaged rects. The worst case is a translucent fullscreen
terminal, and it is why wire gap **G1** (premultiplied memfd `AR24`) is
listed: the server's blit already has an `Argb8888Premul` path for
dma-bufs (`crates/nitro-server/src/dmabuf.rs:104`). `XRGB8888` is copied
as it is. Those are the only two shm formats advertised, and both are
mandatory in `wl_shm`.

`wl_surface.set_opaque_region` → `SetOpaqueRegion`, converted from
surface-local logical rects to buffer pixels. This matters beyond
correctness: `docs/chromium.md` measured blend at ~10 ns/px against a
copy at ~1.8 ns/px.

### 3.3 dma-buf clients (W5)

A dma-buf surface is a **`Surface`** node. It has to be:
`SetSurface` naming a dma-buf is `BadBuffer`, and "a Wayland adapter maps
`wl_surface.commit` of a dma-buf to a present" (`wire.md` L2401).

| Wayland | nitro |
|---|---|
| `zwp_linux_dmabuf_v1` **v4** only: `get_default_feedback`, `get_surface_feedback`. v3's `format`/`modifier` events are not sent. | `DmabufFeedback` (`wire.md` L2457): default feedback on id 0, per-node feedback on the surface's `Surface` node |
| feedback `main_device` | `DmabufFeedback.main_device` (`dev_t`) |
| feedback `format_table` | a sealed memfd the adapter writes, 16 bytes per entry (`u32` format, `u32` pad, `u64` modifier), from the pairs flagged **`CPU` or `COMPOSITE`**. A pair that is only `IMPORT` shows the grey placeholder, so it is **not advertised**: a client would pick it and show grey |
| tranche 1: `scanout` flag, `target_device` = main | the advertised pairs that also carry `SCANOUT` |
| tranche 2 | the rest of the advertised pairs |
| `zwp_linux_buffer_params_v1.add` × planes, `create` / `create_immed` | `CreateDmabufBuffer` (`wire.md` L2409). The modifier must be the same on every plane: nitro takes one, and a mismatch is a `zwp_linux_buffer_params_v1` `invalid_format` error |
| `wl_surface.commit` with a dma-buf attached | the batch's other state (bounds, subsurfaces) in a `Commit`, **then** `PresentSurface { serial, src, damage }`. The present shares the commit serial space |
| `wl_buffer.release` | `BufferReleased` (`wire.md` L2076): only after another frame has latched, or the node let go. This is the one place the release is the server's rather than the adapter's |

**Sync: implicit only.** A plain `PresentSurface` on a dma-buf makes the
server snapshot its write fences (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`) and
wait on the snapshot without blocking (`wire.md` L2435ff). Mesa's EGL and
Vulkan WSI rely on implicit sync when `wp_linux_drm_syncobj_manager_v1`
is absent, and the adapter **does not advertise it**. Explicit sync would
need a DRM fd in the adapter plus the `SYNCOBJ` ioctls:
timeline-point → sync_file export for the acquire point, and a signal on
`BufferReleased` for the release point. Those are new `unsafe` ioctls,
bought for NVIDIA's proprietary driver, which neither box has.
`PresentSurfaceFenced` stays available for that later task, if it is ever
filed.

**Validate first.** A `CreateDmabufBuffer` the server refuses is fatal to
that client's nitro connection (`BadBuffer`). So the adapter first
checks the format and modifier against the feedback it advertised, the
plane count, and each plane's bounds against the fd's size (`lseek
SEEK_END` on a dma-buf is safe). Failures are answered on the Wayland
side: `failed` for `create`, and `invalid_format` / `out_of_bounds` for
`create_immed`. On the fake backend the server takes a sealed memfd as
the dma-buf stand-in, and the adapter's check allows exactly that, so
the headless tests reach this path (§ 5.1).

**A surface that changes buffer kind** (shm → dma-buf, e.g. Firefox after
a GPU reset) swaps its node: `DestroyNode` of the `Image` and
`CreateNode(Surface)` at the same position among its siblings (`before`),
in one commit.

### 3.4 What the adapter refuses before the server can

Buffer dimensions over 16384, a stride below `4·w`, and offsets outside
the pool (pool size from `fstat` at `create_buffer` and at `resize`) are
`wl_shm` `invalid_stride` / `invalid_fd`, posted to the client when the
buffer is created. A surface whose total adapter buffers would pass the
per-client cap is a `wl_display` `no_memory` error. Wayland has no
softer answer, and the alternative is a nitro `Limit` that kills the
client anyway, with a worse message.

---

## 4. Protocol subset, mapped to the wire

Versions are the maximum the adapter advertises. The phase is the task
that adds the global. "—" in the gap column means the wire already
covers it.

| global | ver | phase | wire | gap |
|---|---|---|---|---|
| `wl_compositor` (`wl_surface`, `wl_region`, `wl_callback`) | 6 | W1 | § 4.1 | — |
| `wl_subcompositor` | 1 | **W1** (sync, position, stacking) / W3 (desync) | § 4.2 | — |
| `wl_shm` | 1 | W1 | § 3.1–3.2 | G1 (perf only) |
| `xdg_wm_base` → `xdg_toplevel` | 6 | W1 | § 4.3 | G5 |
| `xdg_wm_base` → `xdg_popup`, `xdg_positioner` | 6 | W3 | § 4.4 | G12 (verify) |
| `zxdg_decoration_manager_v1` | 1 | W3 | § 4.5 | G4, G9, G11 |
| `wl_seat` (pointer, keyboard, touch) | 9 | W1 advertises it with no capabilities; W2 fills it | § 4.6 | G7 (verify) |
| `wp_cursor_shape_manager_v1` | 1 | W2 | `SetCursor`, a cast | — |
| `wl_output` | 4 | W1 (basic) / W3 (hotplug, scale) | § 4.7 | G8 (cosmetic) |
| `zxdg_output_manager_v1` | 3 | W3 | § 4.7 | — |
| `wp_viewporter` | 1 | W3 | § 4.8 | — |
| `wp_fractional_scale_manager_v1` | 1 | W3 | § 4.8 | G11 |
| `wp_single_pixel_buffer_manager_v1` | 1 | W3 | a `Rect` + `SetFill` instead of an `Image` | — |
| `wl_data_device_manager` | 3 | W4 | § 4.9 | — |
| `zwp_primary_selection_device_manager_v1` | 1 | W4 | kept inside the adapter (§ 4.9) | G2 (deferred) |
| `zwp_linux_dmabuf_v1` | 4 | W5 | § 3.3 | — |
| `wp_presentation` | 2 | W5 | § 4.10 | — |
| `zwp_idle_inhibit_manager_v1` | 1 | W5 | accepted, a no-op (§ 4.11) | G10 (later) |
| `xdg_activation_v1` | — | not advertised | § 4.11 | G6 |
| `zwp_text_input_manager_v3`, input-method | — | deferred | nitro has no IME at all | — |
| `wp_linux_drm_syncobj_manager_v1` | — | not advertised | § 3.3 | — |
| relative-pointer, pointer-constraints, keyboard-shortcuts-inhibit | — | deferred (games, VNC viewers) | — | — |
| layer-shell, session-lock, foreign-toplevel, screencopy | — | never: the shell is nitro's own | — | — |
| Xwayland | — | out of scope | § 4.12 | — |

### 4.1 `wl_surface` and the commit

- A surface is a **`Group`** node holding one content node, an `Image`
  or a `Surface`. Its subsurfaces are sibling groups under it (§ 4.2).
  Group bounds = surface position in its parent. Content bounds =
  buffer size / `buffer_scale`, or the viewport destination (§ 4.8).
- `commit` → one nitro `Commit`, carrying the surface's pending state
  and the cached state of its synchronized subsurfaces. Nitro batches
  are atomic, and so are Wayland commits. The mapping is exact, which is
  most of what makes subsurfaces cheap.
- A surface with no role, or not yet mapped, produces no nitro traffic.
  **The window is created at the first commit that has a buffer**
  (`CreateWindow` with the buffer's logical size). Nitro has no mapped-
  but-empty window, and neither does xdg-shell.
- `frame` → `wl_callback`. Every callback in a window's surface tree
  (subsurfaces included) hangs off one `RequestFrame { window }`, and the
  window's `Frame` fires them all with `done(CLOCK_MONOTONIC now, ms)`.
  A popup is a window of its own and has its own `RequestFrame`. W1 must
  verify that a minimized or hidden window still gets its `Frame`
  (G13). If it does not, clients that wait for the callback before
  drawing freeze until the window is shown again, which is a different
  answer from mutter's but a legitimate one. Record whichever the
  server does.
- `set_buffer_scale` / `preferred_buffer_scale` (v6) =
  `ceil(Configure.scale)`. `enter` / `leave(wl_output)` follow
  `Configure.output`.
- `set_input_region`: stored, and **not forwarded**. Nitro hit-tests by
  node bounds. With the tiled-state rule (§ 4.5) there is no shadow for
  an input region to exclude, so the loss is small.
- `attach(dx, dy)` / `offset` (v5): honoured only for drag icons
  (`SetDragIconOffset`). On a toplevel it would move the window, and
  placement is the server's.

### 4.2 Subsurfaces (basic support in W1)

GTK 4's graphics offload, Firefox's compositor layers and mpv's
`dmabuf-wayland` output all put content in subsurfaces. A toolkit that
cannot count on them falls back to paths that are slower or broken.
Supporting them is cheap because the node tree already *is* a subsurface
tree. That is why the basic part moves from W3 into W1.

| Wayland | nitro |
|---|---|
| `get_subsurface(surface, parent)` | the child's `Group` is `Reparent`ed under the parent's `Group`, above the parent's content node |
| `set_position(x, y)` | `SetBounds` on the child's group, applied on the **parent's** next commit, as the protocol requires |
| `place_above` / `place_below(sibling)` | `Reparent { before }`: node order is z-order. "Below the parent" means before the parent's content node |
| sync mode (default) | the child's commits are cached in the adapter and flushed into the parent's nitro `Commit` |
| desync mode (W3) | the child's commit becomes its own nitro `Commit` at once |
| a subsurface outside its parent | clipped at the window's content rectangle. The window's group always clips (`wire.md` L1128ff). Rare in practice, and recorded |

Pointer input names the innermost node under the pointer
(`PointerEnter.node`). The adapter maps that node to its `wl_surface`,
subtracts the surface's accumulated offset to get surface-local
coordinates, and synthesizes `wl_pointer.leave` / `enter` whenever the
node under the pointer changes surface.

### 4.3 `xdg_toplevel` (W1)

| Wayland | nitro |
|---|---|
| first commit with a buffer | `CreateWindow { size: window geometry, layer: Normal, flags }` + `SetAppId`. Flags: `UNDECORATED` unless server-side decoration was negotiated (§ 4.5) |
| initial commit (no buffer) | answered by the adapter with `xdg_toplevel.configure(0, 0, states)` + `xdg_surface.configure`. The client picks its size, and nitro's `Configure` for the real window follows |
| `set_window_geometry` | the window's content size. The `Image` is offset by `-geometry.xy`, so anything outside the geometry (a CSD shadow) is clipped |
| `Configure { size, … }` | `xdg_toplevel.configure(size, states)` + `xdg_surface.configure(serial)`. The adapter applies nothing until the client's `ack_configure` and its next commit |
| committed geometry ≠ configured size (foot rounds to whole cells) | **one** `SetBounds` on the window root (a resize request, `wire.md` L1090) per acked configure. The server answers with a `Configure` of that size, the adapter forwards it, and the client accepts its own size. That converges in one round trip. Never re-sent for the same size, so it cannot loop. |
| states | `maximized`/`fullscreen` from `WindowState`, `activated` from `Focus`, `tiled_*` (§ 4.5), `suspended` (v6) while `Minimized` |
| `configure_bounds` (v4) | the work area of the window's output (`OutputWorkArea`) |
| `wm_capabilities` (v5) | `maximize`, `fullscreen`, `minimize`. **Not** `window_menu` |
| `set_title`, `set_app_id` | `SetWindowTitle`, `SetAppId` |
| `set_min_size` / `set_max_size` | `SetWindowLimits` (0 = no limit, the same convention) |
| `set_maximized`, `unset_maximized`, `set_fullscreen(output)`, `unset_fullscreen`, `set_minimized` | `SetWindowState`. The `output` argument is ignored: the server picks the output the window is on |
| `move(seat, serial)` | `StartMove` |
| `resize(seat, serial, edges)` | `StartResize`. `xdg_toplevel.resize_edge` is a bitmask with the **same bits** as nitro's `resize_edges` (top 1, bottom 2, left 4, right 8; corners are sums), so it is a cast |
| `show_window_menu` | ignored: nitro has no window menu |
| `close` (event) | on `Closed`. The client's `destroy` → `DestroyNode` of the root |
| `set_parent` | stored, **not forwarded**. See G5: dialogs open as ordinary windows |

### 4.4 Popups and the positioner (W3)

The wire's popup was designed with this cast in mind (`wire.md` L835):

| `xdg_positioner` | `CreatePopup` / `RepositionPopup` |
|---|---|
| `set_anchor_rect` | `anchor_rect` (`IRect`, integer on both sides) |
| `set_anchor` (none 0, top 1, bottom 2, left 3, right 4, top_left 5, bottom_left 6, top_right 7, bottom_right 8) | `PopupAnchor`: **the same values** |
| `set_gravity` | `PopupGravity`: the same values |
| `set_constraint_adjustment` (slide_x 1, slide_y 2, flip_x 4, flip_y 8, resize_x 16, resize_y 32) | `constraint_adjust`: **the same bits** |
| `set_size` | `size` |
| `set_offset(x, y)` | **folded into `anchor_rect`**: the rect is translated by the offset. The unconstrained position is identical. A flipped popup mirrors the translated rect rather than negating the offset, which can differ by `2·offset`. Menus use offsets of 0–4 px |
| `set_reactive` (v3) | on every `Configure` of the parent, the adapter re-sends `RepositionPopup` with the stored positioner |
| `set_parent_size`, `set_parent_configure` | ignored: the server places against the parent it knows |
| `xdg_popup.grab(seat, serial)` | `flags: GRAB`. The serial is dropped (§ 4.6) |
| `xdg_popup.reposition(positioner, token)` | `RepositionPopup`. The adapter queues the token and sends `repositioned(token)` before the `configure` that answers it. Nitro has no token (`wire.md` L922ff), but the requests are answered in order, so a FIFO is exact |
| `configure(x, y, w, h)` (parent-relative) | from `Configure`: popup `position` − parent `position`, the subtraction `wire.md` L864 prescribes |
| `popup_done` | `PopupDone` |

A popup is created as a nitro window, so its surface tree gets its own
`RequestFrame`. **G12, to verify:** that keyboard input reaches a
grabbing popup's client while the popup is open. GTK menus need arrow
keys. The client is the same connection either way, so this is probably
already so, but W3 tests it.

### 4.5 Decorations (W3)

Nitro draws server-side frames by default, and that is what the adapter
offers:

- `zxdg_toplevel_decoration_v1.set_mode(any)` or `unset_mode` → configure
  **`server_side`**. The window is created **without** `UNDECORATED`.
  Qt 6 and foot negotiate this, and it gives them nitro's title bar,
  buttons and resize bands.
- `set_mode(client_side)` → honoured: `UNDECORATED`.
- **A client that never binds the manager** (GTK 4, which is CSD-only,
  and Firefox) → `UNDECORATED`. It draws its own title bar, which asks
  for `move`/`resize`, which become `StartMove`/`StartResize`.
- **The tiled-state rule.** Every `UNDECORATED` toplevel is configured
  with `tiled_left | tiled_right | tiled_top | tiled_bottom` (v2+). A
  GTK 4 or libadwaita window then drops its shadow and rounded corners
  and draws a flat rectangle that is exactly its window geometry. That
  is what makes § 4.3's "clip outside the geometry" invisible. The price
  is that GTK's own resize margins, which live in the shadow, go too.
  Such a window resizes with `Super`+right-drag, or by maximizing. G9
  records the alternative.
- The mode is fixed when the window is created, since the wire's flags
  are a `CreateWindow` argument. A mode change after map **recreates the
  window**: a new root with the same content, then destroy the old one.
  That happens once per window lifetime at most, when a client changes
  its mind. If W3 finds it flickers, G4 is the server op that fixes it.

### 4.6 Input (W2)

**Serials.** The adapter keeps one monotonic `u32` counter. It stamps
every event that carries a serial (`enter`, `leave`, `button`, `key`,
`keyboard.modifiers`, `touch.down/up`, configure) and **never checks
one**. `xdg_popup.grab`, `move`, `resize`, `set_selection`, `start_drag`
and `set_cursor` are forwarded as they are, and the server's
focus-and-button checks (`wire.md` L3849ff) authorize them. They are
stricter than a serial check, which is the wire's own argument for having
no serials. Failures are silent on both protocols, so a client can tell
no difference. Two cases need care:

- **A `set_selection` from an unfocused client.** Nitro drops it. Wayland
  compositors drop it too, because its serial is stale.
- **Keyboard focus.** Wayland sends `wl_keyboard.enter` with the pressed
  keys. Nitro's `Focus` carries none, so the array is empty, which is
  legal.

| Wayland | nitro |
|---|---|
| `wl_pointer.enter` / `leave` / `motion` | `PointerEnter` / `PointerLeave` / `PointerMotion`, mapped to surface-local coordinates through the node (§ 4.2). Time = `time_ns / 1e6` |
| `wl_pointer.button` | `PointerButton`. evdev codes on both sides, so no translation |
| `wl_pointer.axis` + `axis_source` + `axis_value120` (v8) + `frame` | `PointerAxis`. `value` = `dx`/`dy` (logical px on both sides). For `Wheel`/`WheelTilt`, `value120 = d / 15 · 120` (`WHEEL_PX_PER_NOTCH` = 15, `wire.md` L1893). Natural scroll is already applied by the server |
| `wl_pointer.axis_stop` | **G7, to verify.** Kinetic scrolling in GTK and Firefox needs "the fingers lifted". libinput reports that as a zero-delta finger scroll. W2 checks whether the server forwards it (`crates/nitro-server/src/input.rs:876`). If it does, zero `dx, dy` with `Finger` → `axis_stop`. If not, that is a small server item |
| `wl_keyboard.keymap` | `Keymap`: the same sealed memfd, NUL-terminated and sized with the NUL. The fd is passed through (`dup`), since the server built it to be shared (`F_SEAL_WRITE`) |
| `wl_keyboard.repeat_info` (v4) | `Keymap.rate_hz`, `delay_ms` |
| `wl_keyboard.key` / `modifiers` | `Key` (evdev keycode) / `Modifiers`, in that order, as the wire already sends them |
| `wl_keyboard.enter` / `leave` | `Focus { focused }`, to the window's main surface |
| `wl_touch.down/motion/up/cancel` + `frame` | `Touch`. The surface under the point comes from the node, as for the pointer |
| `wp_cursor_shape_device_v1.set_shape` | `SetCursor`: **a cast**. The values are verbatim (`wire.md` § Enumerations) |
| `wl_pointer.set_cursor(surface)` | `NULL` → `SetCursor(None)`. A surface → `SetCursor(Default)`. See the note below |

**Cursor images are not supported, deliberately.** The wire takes named
shapes only (`wire.md` L947ff). GTK 4 (≥ 4.16), Qt 6 and foot use
`wp_cursor_shape` when it is offered, and get the right cursor. A client
that only sends pixmap cursors (GTK 3 apps, **Firefox**, older SDL)
shows the arrow everywhere, including over text. W6's matrix records
which of the apps are affected. G3 is the wire change that would fix it,
and it reverses a decision the wire made on purpose, so it is left to
the human.

### 4.7 Outputs (W1 basic, W3 complete)

The watcher connection's `ListOutputs` snapshot gives one `wl_output`
global per `OutputInfo`. `OutputGone` removes it, and hotplug updates
it and sends `done`.

| `wl_output` | from `OutputInfo` (`wire.md` L3029) |
|---|---|
| `geometry(x, y, 0, 0, unknown, "nitro", name, normal)` | position in device px. Physical size **0 × 0**, which the protocol allows as "unknown". G8 |
| `mode(current, w, h, refresh_mhz)` | `w`, `h`, `refresh_mhz`: the same unit |
| `scale` (v2) | `ceil(scale)` |
| `name`, `description` (v4) | connector name |
| `zxdg_output_v1.logical_position` / `logical_size` (W3) | device ÷ `scale` |

### 4.8 Scale, viewporter, fractional scale (W3)

- `wp_fractional_scale_v1.preferred_scale` = `round(Configure.scale ·
  120)`. The client then renders at device resolution and sets a
  `wp_viewport` destination of the logical size.
- `wp_viewport.set_destination(w, h)` → content node bounds (logical).
  `set_source(rect)` → `SetImage.src` / `PresentSurface.src` in buffer
  pixels. The server scales anything it gets.
- For a window whose **content group is on whole device pixels** (an
  undecorated window at any scale, or any window at an integer scale), a
  device-sized buffer with bounds `px / scale` is a **1:1 blit** with
  exact sub-rect damage (`wire.md` L1399ff, #3940). Since #4003 that
  includes a **server-decorated** window at a fractional scale: the
  frame insets are snapped to whole device pixels per output scale, so
  SSD apps (mostly Qt) are 1:1 too. That was G11.

### 4.9 Clipboard, drag-and-drop, primary selection (W4)

The server brokers every transfer between clients, so a Wayland app and
a native nitro app exchange data in both directions without the adapter
seeing the other side.

| Wayland | nitro |
|---|---|
| `wl_data_device.set_selection(source)` | `SetSelection { mimes }` from `wl_data_source.offer`s. `NULL` → an empty list |
| `SelectionOffer { mimes }` | stored per connection. `wl_data_device.data_offer` + `offer`s + `selection` are sent on `wl_keyboard.enter`, and again whenever it changes while focused: Wayland shows the selection only to the focused client |
| `wl_data_offer.receive(mime, fd)` | `RequestSelection { source: Clipboard }`. The `SelectionData` fd (a read end) is **pumped** into the client's write fd: `splice` through the event loop when either end is a pipe, non-blocking `read`/`write` otherwise. This is the writer state machine the wire spared its own clients (`wire.md` L3148), and the adapter is where it lives |
| `SelectionRequest { request, mime }` | a pipe. `wl_data_source.send(mime, write end)` to the Wayland owner, `SendSelection(read end)` to the server. Nothing to pump |
| `start_drag(source, origin, icon, serial)` | the icon surface becomes a window of the same connection, `UNDECORATED \| NO_FOCUS`, with `SetDragIconOffset` from its `attach`/`offset`. Then `StartDrag { window, icon, actions, mimes }`. `wl_data_source.set_actions`: copy 1, move 2 → the same nitro bits. **ask 4 is dropped**: nitro's 4 is `LINK`, and Wayland has no link |
| `DragEnter` / `DragMotion` / `DragLeave` / `DragDrop` | `wl_data_device.enter` (with a new offer) / `motion` / `leave` / `drop` |
| `wl_data_offer.accept(mime)` + `set_actions` | `AcceptDrop { action, mime }` |
| `wl_data_offer.receive` during a drag | `RequestSelection { source: Drag }` |
| `wl_data_offer.finish` | `FinishDrag` (target) |
| `DragFinished { accepted, action }` | `wl_data_source.dnd_drop_performed` then `dnd_finished`, or `cancelled` if not accepted. Nitro does not tell the source when the drop happens, so both are sent at the finish. Clients handle the two back to back |
| source `destroy` during a drag | `FinishDrag` (source cancel) |

**Primary selection stays inside the adapter.** The wire has none
(`docs/chromium.md` L103). Middle-click paste is a habit of terminal and
GTK users, and those apps are Wayland apps here. So
`zwp_primary_selection_*` is implemented **between the adapter's own
clients**: the owner's source, every other client's offer, and a pipe
from one to the other. It involves no server op and no new wire bit.
A native nitro app neither sees it nor sets it, which is what nitro apps
already get today. G2 is the server version, and it waits until a nitro
app wants it.

### 4.10 Presentation time (W5)

`wp_presentation`: `clock_id` = `CLOCK_MONOTONIC`, the clock of
`Presented`. Each `feedback(surface)` is tied to the serial of the nitro
`Commit` (or `PresentSurface`) of that `wl_surface.commit`:

- `Presented { serial, output, time_ns, seq }` → `sync_output` (the
  `wl_output` of `output`), then `presented(time, refresh = 1e12 /
  refresh_mhz ns, seq, flags = vsync | hw_clock | hw_completion)`.
  `zero_copy` is not claimed: whether a frame went on a plane is not
  reported, on purpose (`docs/surfaces.md`).
- A feedback whose serial is older than a `Presented` that has already
  arrived, and has no `Presented` of its own, is `discarded`. Those are
  superseded latch frames (`wire.md` L2206).

### 4.11 Activation, idle inhibit

- **`xdg_activation_v1` is not advertised.** Asking for focus for another
  window (`FocusWindow`) is `SHELL`-only. The adapter could hold a shell
  connection, but `WindowInfo` cannot name another client's window by
  its `NodeId`. Apps work without it: GTK and Qt treat a missing global
  as "no activation". G6.
- **`zwp_idle_inhibit_manager_v1` is advertised and does nothing.** nitro
  has no idle blanking or DPMS today, so there is nothing to inhibit, and
  mpv and Firefox stop warning. G10 is the op the day nitro gains idle
  blanking.

### 4.12 Xwayland: out of scope, and what it would take

Not in W1–W6. The cheapest route is **`xwayland-satellite`**, run
unmodified as a session piece. It is a separate program that is an X11
window manager and Xwayland's compositor on one side, and an ordinary
Wayland client of `nitro-wayland` on the other. It needs `xdg_wm_base`,
`wl_compositor`/`wl_shm`, `wp_viewporter`, `linux-dmabuf` and `wl_seat`,
all of which W1–W5 provide. It works best with
relative-pointer/pointer-constraints for games, which are deferred here.
So Xwayland is a packaging-and-test task after W6 (set `DISPLAY` and add
it to the matrix), not adapter code. Writing our own X WM is not on the
table.

---

## 5. Test strategy

### 5.1 Headless: `crates/nitro-wayland/tests/`

Dev-dependencies: `nitro-server` with `test-support`, the same
arrangement as `nitro-ui`; `wayland-client`; and `wayland-protocols` with
`client`. Every test starts a `TestServer`
(`crates/nitro-server/src/test_support.rs:39`), starts the adapter
in-process on a thread with a socket in the test's own directory, and
connects `wayland-client` to it. Assertions go through the server's
`shot()`, `stat()` and `push_input()`, never through adapter internals.
The pixels on the fake output are what count.

| phase | tests |
|---|---|
| W1 | globals and versions listed; shm toplevel, red 200×100 → red in `shot()` at `Configure.position`; partial `damage` repaints only that rect, and the rest stays as it was; `wl_buffer.release` arrives before `frame.done`; frame callbacks fire; the `ack_configure` / resize-request round trip converges (commit a size ≠ configure, assert a single `SetBounds`); **a client truncates its pool under a live buffer → that client gets `wl_display.error`, the adapter and a second client carry on** (the `SIGBUS` regression test); premultiplied → straight alpha round trip on a 50 % pixel; subsurface above/below parent and sync-mode atomicity (the child's move invisible until the parent commits); two clients → two server clients (`stats`); disconnect frees the server's buffers |
| W2 | `push_input` motion → `wl_pointer.enter/motion` in surface-local coords, including over a subsurface; button + `move` → the window moves in `shot()`; `resize` edges cast; keymap fd readable and NUL-terminated; `Key` + `Modifiers` order; touch; `set_shape(Text)` → the server's cursor stat; axis `value120` for one notch = 120 |
| W3 | positioner → popup in `shot()` where an equivalent native `CreatePopup` puts it (the table in § 4.4 as a test); `flip_y` near the bottom edge; `popup_done` on outside click; server-side decoration → frame visible; client-side → tiled states in the configure; `wl_output` values from a two-output `TestServer`; `viewport` destination scaling; fractional preferred scale at `scale = 1.25` |
| W4 | clipboard Wayland → Wayland, Wayland → native (`nitro_wire::Connection`), native → Wayland; 8 MiB payload through a pipe; DnD between two Wayland clients with a finished `copy`; primary selection between two Wayland clients |
| W5 | dmabuf feedback table matches `DmabufFeedback` (`CPU`/`COMPOSITE` pairs only); `create_immed` with the fake backend's memfd stand-in → pixels in `shot()`; an out-of-bounds plane → `invalid_format` and **no** nitro error; presentation feedback `presented` with `CLOCK_MONOTONIC` times; `discarded` for a superseded frame |

`cargo test -p nitro-wayland` runs them all. They are in `just test` like
every other crate.

### 5.2 On the boxes: the real-app matrix (W6, with W5 for GPU apps)

These run against libwayland clients, the independent implementation
that § 1's argument leans on. New recipes in `deploy/dev.just`, box-aware
like the rest (`docs/testbox.md`):

- `just box-wayland app='foot'`: starts the app inside the running
  session with the session's `WAYLAND_DISPLAY`, waits for its window in
  the window list (`nitro-hey` / `WindowInfo` app id), and saves a
  `nitro-shot` to `tmp/wayland/<app>.png`.
- `just wayland-matrix`: every row below on the chosen box. It prints a
  pass/fail table and the adapter's RSS (`footprint`) with the apps open.

| app | proves | pass criteria |
|---|---|---|
| `weston-simple-shm` | shm, frame callbacks | animates; adapter CPU per frame logged |
| `foot` | SSD via xdg-decoration, keyboard, resize to cell size | types with `ydotool` (as `docs/wm.md` L1322 does); resize converges; clipboard to and from `nitro-term` |
| `weston-terminal` | CSD without xdg-decoration, pixmap cursors | window drawn and movable from its own title bar; the arrow-cursor limitation confirmed |
| `gtk4-widget-factory` | CSD + tiled states, popovers (xdg_popup), subsurfaces, cursor-shape, fractional scale | menus open where clicked; no clipped shadow; text cursor over entries |
| a Qt 6 app on the box (`qterminal`, `kate`, whichever is installed) | SSD negotiation, popups, fractional scale | nitro frame, not Qt's; menus |
| Firefox | CSD, subsurfaces, dma-buf (W5), clipboard, DnD, primary selection | page renders, scrolls (kinetic, G7), copy/paste both ways, a file dragged from `nitro-files` |
| `mpv --vo=dmabuf-wayland` | dma-buf in a subsurface, `Surface` node, presentation time, idle inhibit | 1080p H.264 plays at display rate; `stats` shows planes or composite, not placeholder |
| `mpv --vo=gpu` | EGL dma-buf with implicit sync | plays without tearing |
| `weston-simple-dmabuf-egl` (W5) | linux-dmabuf v4 feedback | runs; the modifier it picked is in the advertised table |

Both boxes: box1 for cost (Haswell, no AVX2, 2 cores) and testbox2 for
the Kaby Lake planes and GPU helper.

---

## 6. Work plan

The six tasks already on the board, confirmed or amended. None of them
is edited by this task: the amendments are for each planner to pick up.

| task | scope (amended) | depends on |
|---|---|---|
| **#3993 W1 core** | crate + dependency (the `DEPENDENCIES.md` row goes from proposed to real, with measured tree/binary numbers); socket from stdin or `bind_auto`; per-client nitro connection + watcher; `wl_compositor` v6; **basic `wl_subcompositor` (moved from W3)**; `wl_shm` with the `pread` copy, two buffers per surface and un-premultiply; `wl_region` / opaque region; `xdg_wm_base` toplevel as in § 4.3; frame callbacks; **basic `wl_output` (moved from W3)**, because clients choose scale from it; `wl_seat` advertised with no capabilities, because some clients refuse to start without one; the W1 tests in § 5.1 | #3992 |
| **#3994 W2 input** | as filed: seat pointer/keyboard/touch, keymap pass-through, focus, serials (§ 4.6), cursor shape + the `set_cursor` fallback, move/resize. Plus the G7 check | W1 |
| **#3995 W3 shell completeness** | popups + positioner (§ 4.4), decoration + the tiled-state rule (§ 4.5), `xdg-output` and output hotplug, viewporter, fractional scale, single-pixel-buffer, subsurface desync. Plus the G12 check. **Subsurfaces and basic `wl_output` move out to W1** | W1 (W2 for the grab tests) |
| **#3996 W4 data** | as filed, with primary selection **inside the adapter** (§ 4.9) rather than bridged | W2 (keyboard focus decides which client sees the selection) |
| **#3997 W5 GPU clients** | linux-dmabuf v4 + feedback, implicit sync only (**no `wp_linux_drm_syncobj`**), `Surface` node + `PresentSurface`, `wp_presentation`, idle-inhibit as a no-op; boxes | W1 |
| **#3998 W6 session** | the session binds `wayland-N` + lock and passes it as stdin (§ 2); `WAYLAND_DISPLAY` for pieces and apps (overwrite, never inherit); the launcher filter goes when `WAYLAND_DISPLAY` is set; the `just box-wayland` / `wayland-matrix` recipes and the matrix run; adapter footprint in `docs/budget.md` | W1 for the session half (can start early); W2–W5 for the full matrix |

### 7. Wire gaps: proposed server items

Each of these is a **proposed task**. None of them is filed, and none
blocks W1–W6: every row says what the adapter does without it.

| id | gap | without it | proposal |
|---|---|---|---|
| **G1** | memfd `AR24` is straight alpha; `wl_shm` is premultiplied | the adapter un-premultiplies: 1.6 ms (opaque) to 9.9 ms (all translucent) per 1080p frame on box1 | a premultiplied flag for `CreateBuffer`/`CreateSurfaceBuffer` behind a new cap bit. The raster already has `Argb8888Premul`. **Worth filing after W1** has real-app numbers |
| G2 | no primary selection on the wire | kept inside the adapter, Wayland ↔ Wayland only | `DataSource::Primary` behind a new bit, when a native app wants it |
| G3 | no bitmap cursors (deliberate, `wire.md` L947) | pixmap-cursor clients (Firefox, GTK 3) show the arrow | a `SetCursorImage` op. It reverses a wire decision, so it is **the human's call** after the W6 matrix |
| G4 | window flags are fixed at `CreateWindow` | a decoration-mode change after map recreates the window | `SetWindowFlags`, only if W3 sees a flicker |
| G5 | no transient parent / modal (`xdg_toplevel.set_parent`, `xdg-dialog`) | dialogs open as ordinary windows, placed like any window, and can fall behind their parent | a `SetParent` op: placement over the parent, stacking with it, and minimizing with it. It is useful to Chromium too (`set_parent_for_non_top_level_windows`) |
| G6 | a client cannot ask for focus with a token | `xdg_activation_v1` not advertised | an activation op authorized like the others (the requester holds focus, or the token came from the focused client) |
| G7 | "fingers lifted" may not reach clients | no `axis_stop`, so no kinetic scrolling | verify in W2; forward zero-delta finger scrolls if they are dropped |
| G8 | `OutputInfo` has no physical size / make / model | `wl_output.geometry` reports 0 × 0 and "nitro" | cosmetic; not proposed |
| G9 | no input margin outside an undecorated window | CSD resize-from-shadow is lost (tiled-state rule) | a "shadow inset" on `CreateWindow`, if users miss it |
| G10 | no idle inhibit | no-op global | an inhibit op on the day nitro blanks idle outputs |
| G11 | ~~server frame insets are not whole device pixels at fractional scales~~ **fixed (#4003)** | ~~SSD Wayland windows (Qt, foot) at 1.25 are resampled~~ | insets are snapped to whole device pixels per output scale (`wm::frame_insets_for`); native decorated apps benefit too |
| G12 | (verify) keyboard to a grabbing popup | — | W3 test |
| G13 | (verify) `Frame` for hidden/minimized windows | — | W1 test; record the behaviour |
