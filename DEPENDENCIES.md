# Dependencies

Every external crate and every `unsafe` exception is listed here with the
reason it earns its place. Adding one is a design decision, not a
convenience. `cargo tree -e normal --prefix none | sort -u | wc -l` is the
number we watch.

## Crates

| crate | used by | why | cost / notes |
|---|---|---|---|
| `rustix` | seat, kms, server, wire, demo | Safe Linux syscalls (epoll, mmap, sockets + `SCM_RIGHTS`, timerfd, netlink) with no libc. The one crate that lets the rest of the tree be `unsafe`-free. | + `bitflags`, `linux-raw-sys` |
| `zerocopy` (+ `zerocopy-derive`) | wire | The wire format *is* `#[repr(C)]` layout: `U32<LittleEndian>`/`F32<LE>`/… give guaranteed little-endian fields, `Unaligned` lets a payload be decoded in place from any `&[u8]`, and `ref_from_bytes`/`as_bytes` replace the pointer casts we would otherwise write by hand. Validated, total, and `unsafe`-free in our tree. | +3 crates: `zerocopy`, `zerocopy-derive`, and **`syn` 2.x**. Note `drm` → `bytemuck_derive` pins `syn` **3.x**, so the two do *not* share a build: syn is compiled twice. Revisit if compile time hurts. |
| `drm` (+ `drm-ffi`, `drm-sys`, `drm-fourcc`) | kms | Safe wrappers over the ~30 DRM/KMS ioctls (atomic commit, dumb buffers, AddFB2, properties, events). Hand-rolling them is precisely the `unsafe` we forbid. | pulls `bytemuck` + `bytemuck_derive` → `syn` (proc-macro, compile time). Revisit if it hurts. |
| `signal-hook` (+ `signal-hook-registry`) | server, demo | SIGTERM/SIGINT → self-pipe without `unsafe` in our tree: `sigaction` and an async-signal-safe handler are exactly the shim we would otherwise have to write ourselves. `default-features = false` (no iterator/channel). The demo uses it so Ctrl-C prints its latency summary instead of killing the process mid-histogram. | + `libc` (already pulled by `libseat`). Only `low_level::pipe::register` is used. |
| `libseat` (+ `libseat-sys`) | seat | Bindings to the C libseat: one interface over logind / seatd / raw VT for DRM master + input fds without root. The single deliberate C dependency. | + `errno`, `libc`, `log`. `default-features = false`: the `custom_logger` feature builds a C shim (`cc`) to route libseat's log lines through `log`; we do not log. |
| `input` (+ `input-sys`) | server | Bindings to libinput, which is the only sane way to read evdev: tap detection, pointer acceleration, scroll-source classification and touchpad state are thousands of lines of hard-won device quirks we are not going to re-derive. `default-features = false, features = ["libinput_1_21"]` — the `udev` feature is **off**, so `libudev` never enters the tree: the server finds devices by reading `/dev/input` and opens them through `nitro-seat`. | + `libc` (already there via `libseat`). The FFI `unsafe` lives in the dependency; `LibinputInterface` is a safe trait we implement. Input-device hotplug is M3: it needs the netlink uevent socket `nitro-kms` already has, plus a directory diff. |
| `xkbcommon` | server | Keycode → keysym → UTF-8 with the user's own layout, dead keys, levels and modifier semantics. The alternative is shipping a keymap format and a compose engine, which is a project, not a dependency. It reads `XKB_DEFAULT_*`, so it honours whatever the user already configured. | + `xkeysym`, `memmap2`. The FFI `unsafe` (and the `mmap` of the keymap file) lives **inside the `xkbcommon` crate**, not in ours; our tree stays `unsafe`-free. |
| `swash` | text | OpenType shaping, scaling and hinted glyph rasterization in one pure-Rust crate. Text is the one part of a display server nobody should write twice: the shaper alone is the OpenType GSUB/GPOS state machines, script itemization and mark attachment. Clients never see any of it — the server shapes, so the wire carries strings. | **+7 net crates** (`swash`, `skrifa`, `read-fonts`, `font-types`, `yazi`, `zeno`, `once_cell`); the other five of its seventeen (`bytemuck`, `syn`, `proc-macro2`, `quote`, `unicode-ident`) are already in our tree. **The one place untrusted bytes are parsed by a dependency** — see below. |

### Why `swash` and not the alternatives

`parley` is the obvious candidate and was measured first: it costs roughly
thirty crates (fontique, peniko, kurbo, the unicode tables, …) against
seven, and what it buys over its own shaper is bidi, font fallback and
rich text — none of which M2 needs. **Its shaper *is* swash**, so parley
remains a layer we can add on top later without re-deciding anything: the
door stays open, we just have not paid for it yet.

`rustybuzz` + `ab_glyph_rasterizer` lands at a similar crate count and
gives up hinting and colour bitmaps to get there, which is the wrong trade
for 13-px UI text on a 96-dpi panel.

Font *discovery* is ours, not fontconfig's: `nitro-text` scans
`NITRO_FONT_DIRS` (three sensible defaults) and reads family, weight and
style straight out of each face with swash's own `FontRef`. Fontconfig
would be a C dependency, a config language and a cache format to solve a
`read_dir` and three alias tables.

### The untrusted-bytes note

Font files are the one input where a dependency parses bytes we did not
produce. Everything else in the tree that touches hostile input — the
wire decoder, client buffers — is code we wrote and bounded ourselves;
here the parsing lives in `read-fonts`/`skrifa` and the rasterization in
`zeno`. Two things limit the exposure. The files come from the system
font directories, so reading a hostile one already implies an attacker who
can write to `/usr/share/fonts` — and a client cannot make the server open
a file at all: it names a *family*, and an unknown family falls back to a
face already in the index. And swash is pure safe Rust, so a malformed
table is a panic or a wrong glyph, not memory corruption. Revisit if the
server ever accepts a font over the wire, which it should not.

Crate count: `cargo tree -e normal --prefix none | sort -u | wc -l` = **61**.
`input` and `xkbcommon` cost five of those between them (themselves plus
`input-sys`, `xkeysym`, `memmap2`); `swash` costs seven more (M2 text);
the rest of the rise since M0 is the server now depending on every other
nitro crate.

`nitro-demo` (M1's measurement client) adds no external dependency at
all: it uses `nitro-wire`, `nitro-core`, `rustix` and `signal-hook`, all
already here. Its `--save-small` PNG writer is its own small deflate
encoder rather than the `png` crate, for the same reason `nitro-shot` has
one.

`nitro-ui` (M2's toolkit) is the same story: `nitro-core`, `nitro-wire`
and `rustix`, all already here, plus an *optional* `nitro-server` behind
its `test-support` feature, which only its own test harness turns on — an
app binary links no compositor. It moved the figure from 60 to 61, and
that line is `nitro-ui` itself.

`nitro-hey` (M2's CLI for the introspection socket) is `std` and `rustix`
and nothing else — not even `nitro-ui`, whose `introspect` module it
could have shared a path resolver and an escaping function with. It is
the tool you reach for when something is already wrong, so it should
build and run when as little as possible is working; two dozen lines
duplicated is the price, and the tests on both sides pin the shared
format. Its PNG writer is a copy of `nitro-shot`'s for the reason the
next paragraph gives.

`nitro-calc` (M2's first application) adds **zero** external
dependencies, and that is the number the milestone was after: a complete
app — widget tree, state machine, formatter, scriptable socket — whose
entire dependency list is `nitro-ui`. It is the demonstration that
writing an app on this stack costs an app author nothing beyond the
toolkit. The four lines the tree count rose by are its own `nitro-calc`
and `nitro-ui (*)` entries, not new crates.

The M1 rise from 58 to 60 was the same kind of non-event: one line is
`nitro-demo`, a workspace crate, and the other is a second
`signal-hook v0.4.4 (*)` line — cargo's marker for a subtree it has
already printed — which `sort -u` counts as distinct from the first.
Counting distinct external crate *names* gives **35** throughout. The line
count is still the number we watch, because it is cheap and moves when
something real is added; it just wants reading with that caveat whenever a
new workspace crate reuses an existing dependency.

Planned (M3+): nothing currently. `parley` sits behind swash as the
upgrade path if bidi, font fallback or rich text ever become requirements.
Rejected: `serde` (hand-written wire), `png` (own stored-deflate encoder in
`nitro-shot`, copied verbatim into `nitro-hey` — moving it into
`nitro-core` would put a PNG encoder in the dependency graph of the
server, the toolkit and every app, to save 120 lines of pure arithmetic
that two CLIs use; revisit at a third consumer), `tokio`/`async-*`
(single-threaded epoll loop), `winit`,
`wgpu`, `smithay`, `libudev` (a `read_dir` and a netlink socket do what we
need of it), `fontconfig` (a `read_dir` and three alias tables do what we
need of it), `parley` and `rustybuzz` (see above).

## `rustix` features by crate

The feature set is per-crate, not workspace-wide, so each pays only for
the syscall families it uses.

| crate | features | used for |
|---|---|---|
| `nitro-wire` | `event`, `fs`, `net`, `process` | `poll` for the blocking handshake; `memfd_create`/`fstat`/`ftruncate` (tests) and `unlinkat`/`mkdir` for the socket path; `socket`/`bind`/`listen`/`accept`/`sendmsg`/`recvmsg` + `SCM_RIGHTS`; `getuid` for the `/tmp` fallback path |
| `nitro-server` | `event`, `fs`, `net`, `process`, `time` | epoll loop, control socket, signals, timers; `pread` to copy client buffers out of their memfds, and `eventfd` for the test input source |
| `nitro-kms` | `event`, `fs`, `mm`, `net`, `time` | DRM fds, `mmap` of dumb buffers, udev netlink |
| `nitro-demo` | `event`, `fs`, `process`, `time` | `poll` for the event loop; `memfd_create`/`ftruncate`/`pwrite` for the image buffer; `getuid` for the `/tmp` fallback of the control-socket path; `clock_gettime` for the delivery-leg breakdown |
| `nitro-ui` | `event`, `fs`, `process`, `time` | `epoll` for the app loop, `poll` for the synchronous text measurement, `Timespec` for `ui.set_timer`; `memfd_create`/`ftruncate`/`pwrite` for an `Image` widget's pixel buffer; `getuid`/`getpid` for the introspection socket's path |
| `nitro-hey` | `process` | `getuid` for the `/tmp` fallback of the app-socket directory |

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
