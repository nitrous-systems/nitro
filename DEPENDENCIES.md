# Dependencies

Every external crate and every `unsafe` exception is listed here with the
reason it earns its place. Adding one is a design decision, not a
convenience. `cargo tree -e normal --prefix none | sort -u | wc -l` is the
number we watch.

## Crates

| crate | used by | why | cost / notes |
|---|---|---|---|
| `rustix` | seat, kms, server, wire, demo, session, launcher, files | Safe Linux syscalls (epoll, mmap, sockets + `SCM_RIGHTS`, timerfd, netlink) with no libc. The one crate that lets the rest of the tree be `unsafe`-free. | + `bitflags`, `linux-raw-sys` |
| `zerocopy` (+ `zerocopy-derive`) | wire | The wire format *is* `#[repr(C)]` layout: `U32<LittleEndian>`/`F32<LE>`/… give guaranteed little-endian fields, `Unaligned` lets a payload be decoded in place from any `&[u8]`, and `ref_from_bytes`/`as_bytes` replace the pointer casts we would otherwise write by hand. Validated, total, and `unsafe`-free in our tree. | +3 crates: `zerocopy`, `zerocopy-derive`, and **`syn` 2.x**. Note `drm` → `bytemuck_derive` pins `syn` **3.x**, so the two do *not* share a build: syn is compiled twice. **Measured and left alone** — see "`syn` is compiled twice" below; deduplicating it makes the wall-clock build *slower* on a many-core box. |
| `drm` (+ `drm-ffi`, `drm-sys`, `drm-fourcc`) | kms | Safe wrappers over the ~30 DRM/KMS ioctls (atomic commit, dumb buffers, AddFB2, properties, events). Hand-rolling them is precisely the `unsafe` we forbid. | pulls `bytemuck` + `bytemuck_derive` → `syn` 3.x (proc-macro, compile time). `bytemuck_derive` is a *direct* dependency of `drm`, so `default-features = false` cannot drop it. Measured: see "`syn` is compiled twice" below. |
| `signal-hook` (+ `signal-hook-registry`) | server, demo, session | SIGTERM/SIGINT → self-pipe without `unsafe` in our tree: `sigaction` and an async-signal-safe handler are exactly the shim we would otherwise have to write ourselves. `default-features = false` (no iterator/channel). The demo uses it so Ctrl-C prints its latency summary instead of killing the process mid-histogram. `nitro-session` uses it for the same reason the server does, and it is the crate's third consumer rather than a new dependency. | + `libc` (already pulled by `libseat`). Only `low_level::pipe::register` is used. |
| `libseat` (+ `libseat-sys`) | seat | Bindings to the C libseat: one interface over logind / seatd / raw VT for DRM master + input fds without root. The single deliberate C dependency. | + `errno`, `libc`, `log`. `default-features = false`: the `custom_logger` feature builds a C shim (`cc`) to route libseat's log lines through `log`; we do not log. |
| `input` (+ `input-sys`) | server | Bindings to libinput, which is the only sane way to read evdev: tap detection, pointer acceleration, scroll-source classification and touchpad state are thousands of lines of hard-won device quirks we are not going to re-derive. `default-features = false, features = ["libinput_1_21"]` — the `udev` feature is **off**, so `libudev` never enters the tree: the server finds devices by reading `/dev/input` and opens them through `nitro-seat`. | + `libc` (already there via `libseat`). The FFI `unsafe` lives in the dependency; `LibinputInterface` is a safe trait we implement. Input-device hotplug is M3: it needs the netlink uevent socket `nitro-kms` already has, plus a directory diff. |
| `xkbcommon` | server | Keycode → keysym → UTF-8 with the user's own layout, dead keys, levels and modifier semantics. The alternative is shipping a keymap format and a compose engine, which is a project, not a dependency. It reads `XKB_DEFAULT_*`, so it honours whatever the user already configured. | + `xkeysym`, `memmap2`. The FFI `unsafe` (and the `mmap` of the keymap file) lives **inside the `xkbcommon` crate**, not in ours; our tree stays `unsafe`-free. |
| `vte` | term | The VT/ANSI escape-sequence **state machine** (Paul Williams' DEC parser), which is a table of transitions nobody should transcribe twice: C0, CSI with its parameters and intermediates, OSC with both terminators, DCS, and UTF-8 decode across buffer boundaries. Crucially it assigns **no meaning** — it hands back `print`/`execute`/`csi_dispatch`/`osc_dispatch` and every escape sequence's *effect* is ours, in `nitro-term`'s own `vt.rs`, where it is tested. No `serde`, no allocator tricks, `default-features = false`. | **+2 crates** (`arrayvec`, `memchr`); `memchr` was already in the tree via nothing else, so it is genuinely two. The alternative is roughly 600 lines of state table and the bugs that come with hand-rolling one — and the failure mode of a wrong transition is a terminal that garbles output on a rare sequence, months later. |
| `swash` | text | OpenType shaping, scaling and hinted glyph rasterization in one pure-Rust crate. Text is the one part of a display server nobody should write twice: the shaper alone is the OpenType GSUB/GPOS state machines, script itemization and mark attachment. Clients never see any of it — the server shapes, so the wire carries strings. | **+7 net crates** (`swash`, `skrifa`, `read-fonts`, `font-types`, `yazi`, `zeno`, `once_cell`); the other five of its seventeen (`bytemuck`, `syn`, `proc-macro2`, `quote`, `unicode-ident`) are already in our tree. **The one place untrusted bytes are parsed by a dependency** — see below. |

### Why `vte`, and where the line is drawn

It is worth being precise about what this dependency does and does not
buy, because "a terminal library" would be the wrong thing to take.

`vte` is the **lexer**, not the terminal. It does not know what `CSI 2 J`
means, has no grid, no scrollback, no colours and no cursor; it turns a
byte stream into dispatch callbacks and stops. Everything that makes a
terminal a terminal — the cell model, the damage tracking, the scroll
region, the alternate screen, the SGR colour table, the key encodings —
is `nitro-term`'s, which is why `grid.rs` and `vt.rs` carry a hundred
unit tests between them.

That split is the reason it passes the bar. The parser is a
specification transcribed into a table: there is one right answer, it is
tedious, and getting a transition wrong produces a terminal that garbles
rare sequences long after anyone would connect the two. The semantics are
the opposite — they are what this application *is*, and vendoring
somebody's opinion about them (`alacritty_terminal`, say, which is what
sits above `vte` in Alacritty) would be taking the app rather than a
dependency.

Writing our own parser was the alternative considered, and it is a real
option: about 600 lines. It was refused for two crates because the UTF-8
handling is the part that would bite — a multi-byte character split
across two `read`s has to resume, not reset, and that is exactly the bug
that only shows up under load.

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
Counting distinct external crate *names* gives **34** throughout. The line
count is still the number we watch, because it is cheap and moves when
something real is added; it just wants reading with that caveat whenever a
new workspace crate reuses an existing dependency.

**Count the names with `awk 'NF'`, not a bare `awk '{print $1}'`:**
`cargo tree` separates each root's subtree with a blank line, the bare
form turns every blank into an empty string, and `sort -u` keeps one — so
it reports 35 for 34 crates. That off-by-one is where the "35" this file
and `docs/budget.md` both used to carry came from; the figure was never
35 (#530). A `Cargo.lock` count is a third number again, **37**, because
the lock file also carries `pkg-config`, `windows-sys` and
`windows-link` — one build dependency and two `cfg(windows)` entries no
Linux build ever compiles. `docs/budget.md` §"Dependency count" has the
per-milestone decomposition.

M3's shell crates add **zero** external dependencies between them, and
that is the number worth reporting. `nitro-bar` is `nitro-ui` plus
`rustix` (the wall clock and `/sys`); `nitro-launcher` and
`nitro-wallpaper` are `nitro-ui` and `std` and nothing else, like
`nitro-calc`. The launcher's spec allowed `rustix` for `fork`/`setsid`
in its spawn path and it turned out not to be needed:
`std::process::Command::process_group(0)` is the safe half of `setsid`
and the half a launcher actually needs (`crates/nitro-launcher/src/spawn.rs`
argues the other half is a controlling terminal neither process has).
`cargo tree -e normal --prefix none | sort -u | wc -l` is unchanged at
**67** across the three of them.

`nitro-session` (M3-E) adds **zero** too, and it is the one where the
temptation was real. It is the "one place a D-Bus client is allowed"
that `DESIGN.md` names — and it still does not speak D-Bus, because
`zbus` is **~40 crates against a tree of 34** and `systemctl suspend`
*is* a logind call with the same polkit check and the same inhibitor
handling. What D-Bus would buy over a fork/exec is *events*
(`PrepareForSleep`, `Lock`/`Unlock`, an inhibitor fd held across a
suspend), and the one action that needs them — `lock` — is the one
action M3 does not implement. So the permission is still unspent, and
`crates/nitro-session/README.md` records what would spend it.

`nitro-term` (M4-A) is the first workspace crate since M0 to add an
external dependency, and it adds **three crate names**: `vte`, plus
`arrayvec` and `memchr` behind it. The whole-workspace figure moves
**70 → 74** lines and **34 → 37 distinct external crate names** — and
unlike every rise since M0, this one moves the *name* count too, which
is the figure that means something. (`docs/budget.md` decomposes the
line count and explains why 34 rather than the 35 this file used to
carry: a bare `awk '{print $1}'` turns `cargo tree`'s blank separator
line into an empty string that `sort -u` keeps.) The argument for spending it is above; what is
worth recording here is the shape of the app around it, because it is the
shape `nitro-calc` established. `nitro-term` is `nitro-ui` + `rustix` +
`vte`, and everything that makes a terminal a terminal — the grid, the
damage tracking, the colour table, the key encodings — is *in* the crate
rather than under it. A 650 KB binary containing a VT parser, a cell
model and a scrollback ring, against `nitro-calc`'s 560 KB for a
calculator, is what that costs: 90 KB, and no third-party terminal.

`nitro-files` (M4-D) adds **zero** external crates, and it is the one
where the temptation was a whole family of them. A file manager wants a
timezone database for its date column, a MIME database, a glob engine, an
image decoder for thumbnails and an icon-theme loader; it takes none of
them. The dates are arithmetic (Howard Hinnant's days-from-civil, in
UTC, and the limitation is stated in `docs/files.md`), the MIME table is
the system's own `globs2` with a thirty-line built-in fallback for a bare
rootfs, the glob engine is refused by keeping only the `*.ext` shape of
rule, and there are no thumbnails and no icon theme for the reason
`nitro-wallpaper` reads P6 PPM only: a decoder is a parser for untrusted
bytes in a long-lived process. What it *does* take is two workspace
crates — `nitro-ui`, and `nitro-launcher` for the `.desktop` parser and
the spawn path, which is reuse rather than a dependency in the sense this
file counts. `cargo tree -e normal -p nitro-files --prefix none | sort -u`
is `rustix` (+ `bitflags`, `linux-raw-sys`), `zerocopy` (+ its derive and
the `syn` chain) and the nitro crates: every one of them already in the
tree. The whole-workspace figure moves **75 → 77** lines and stays at
**41 distinct external crate names** — the two new lines are
`nitro-files` itself and a second `nitro-launcher (*)`, cargo's marker
for a subtree it has already printed.


The whole-workspace count with the session in is **70** lines and still
**34 distinct external crate names** — the rise from 67 is three
`(*)`/workspace lines, not three crates.

The session is `rustix` + `nitro-wire` + `signal-hook`. Its spec allowed
only the first two; `signal-hook` is the addition, and it adds no crate
to the tree. The alternative inside `rustix` is
`runtime::kernel_sigaction`, which is `unsafe`, `doc(hidden)` and
documented as unusable in a process that has a libc — which this one
does, transitively. Its logging module is a copy of the server's rather
than a dependency on `nitro-server`, which would link libseat, libinput,
xkbcommon and the whole compositor into the process whose job is to
`fork`, `exec` and `poll`.

The wallpaper is where a dependency would have been easiest to justify
and was still refused: it reads images, and it reads **P6 PPM only**
rather than linking a PNG or JPEG decoder. A decoder is a parser for
untrusted bytes in a process that runs for the whole session, which is
exactly the exposure the untrusted-bytes note below is careful about for
fonts; `magick in.png out.ppm` is the answer, and the decoder it avoids
is seventy readable lines. Same reasoning as `png` under Rejected.

`nitro-settings` (M4-C) adds **zero** external dependencies, and the
place that was tested is its own configuration file. The obvious move for
`server.conf` is a TOML crate; it was refused because the format has no
tables, no arrays and no types beyond a float, an integer pair and a
string — `key = value` with `#` comments is a 40-line parser, and the key
*is* the path a table would have spelled. The server's copy lives in
`crates/nitro-server/src/config.rs` and the app has its own renderer and
parser in `crates/nitro-settings/src/conf.rs`, because an app must not
link the compositor to write nine lines of text; a round-trip test feeds
this crate's output to the server's own parser, so the two copies cannot
drift without a test failing.

The inotify watch is `rustix::fs::inotify` — already a dependency, and
the feature (`fs`) was already enabled. Audio shells out to `wpctl` with
a `pactl` fallback rather than linking a PipeWire or D-Bus client: the
whole interaction is three commands and their output, and a sound-server
client library would be a permanent dependency for a section that is idle
whenever nobody is dragging the volume slider. `nitro-settings` is
`nitro-ui` and `std`, like `nitro-calc`.

The whole-workspace figure is **74** lines and **37 distinct external
crate names** with M4-C in — both unchanged from M4-A, which is the
number this section exists to report.

## M4-E1: remote apps over TCP

**Zero new crates, and that was the expectation going in.** A remote
client is the *same* wire over a different socket, so the whole addition
is a TCP socket, four socket options and an address parser:

- `rustix::net` was **already** a `nitro-wire` feature (Unix sockets,
  `SCM_RIGHTS`, `sendmsg`/`recvmsg`), and it covers `AF_INET`/`AF_INET6`
  and `sockopt` — `set_tcp_nodelay`, `set_socket_keepalive`,
  `set_tcp_keepidle`/`keepintvl`/`keepcnt`, `set_socket_reuseaddr` —
  with no new feature flag and no new crate. `nitro-server` likewise
  already had `net` for the control socket.
- Name resolution is `std::net::ToSocketAddrs`, i.e. the platform
  resolver. A DNS crate would be a permanent dependency for one call
  that runs at most once per process start, and it would answer a
  *different* question from the one the rest of the system answers — a
  client that resolves a name differently from `ssh` to the same host is
  a support case nobody can reproduce.
- No TLS. There is no authentication and the documented model is
  loopback plus an SSH forward (`docs/remote.md`), precisely so that a
  display server does not grow a key exchange, a cipher negotiation and
  an account model — three attack surfaces with no reviewers. `rustls`
  alone would roughly double the external crate count of the workspace.

The figure is therefore still **74** lines and **37 distinct external
crate names** with M4-E1 in.

## `syn` is compiled twice, and it stays that way (#521)

Two proc-macro crates in the tree want different major versions of `syn`:

- `zerocopy-derive` 0.8 → **`syn` 2.x** (`nitro-wire`, the wire format)
- `bytemuck_derive` 1.12 → **`syn` 3.x** (`bytemuck` → `drm` → `nitro-kms`)

So `syn` is built twice on a cold build. Proc-macro only, so there is no
runtime or binary-size cost; the question was purely compile time.

**Measured, and closed as not worth doing.** Neither upstream offers a
version that resolves it: `drm` 0.15 (latest) requires
`bytemuck/derive` unconditionally — `bytemuck_derive` is a *direct*
dependency of `drm`, not an optional feature, so `default-features = false`
cannot reach it — and the only `zerocopy` on syn 3 is `0.9.0-alpha.0`,
which is not something the wire format should sit on.

The one lever that *does* work from our side is pinning
`bytemuck_derive` down to **1.10.2**, the last release on syn 2; that
collapses the two builds into one, keeps the workspace at 37 distinct
external crates, and the whole suite passes. It was measured and
**rejected**. Cold `cargo build --release --workspace`, alternating the two
lockfiles so both see the same machine state, on a 128-core box:

| | wall | user CPU |
|---|---|---|
| syn twice (as shipped) | 29.53 / 29.40 / 29.22 s | 121.46 / 121.69 s |
| syn once (`bytemuck_derive` 1.10.2) | 29.66 / 29.72 / 29.72 s | 120.08 / 120.54 s |

**Wall time is 0.3–0.5 s *worse*, in all three pairs, and the direction is
not noise** (the two clusters do not overlap). The ~1.2 s of user CPU the
second `syn` really does cost is **off the critical path**: the two
proc-macro chains build in parallel with each other and with the rest of
the graph, so removing one shortens no path and the extra scheduling makes
the build marginally longer. The issue's "~10–20 s of build time" estimate
assumed the work was serial. On a 2-core machine the trade would look
different — and that is worth re-measuring if anyone builds this on one.

It is also the wrong *kind* of change: pinning a transitive proc-macro
backwards is a lock-file pin that any `cargo update` silently undoes, so
the repo would carry a constraint nothing enforces, to buy a regression in
the number it was supposed to improve.

**The upstream condition that resolves it for free:** `zerocopy-derive`
reaching syn 3 in a stable release (0.9 is heading there), or `drm` making
`bytemuck`'s `derive` optional. Either removes the duplicate with no pin
and no measurement needed — whichever lands first, the duplicate
disappears on the next `cargo update`.

Planned (M3+): nothing currently. `parley` sits behind swash as the
upgrade path if bidi, font fallback or rich text ever become requirements.
Rejected: `serde` (hand-written wire), **`png`** — encoding *and* decoding,
now measured rather than assumed; see "`png` versus our own decoder" below
— `tokio`/`async-*`
(single-threaded epoll loop), `winit`,
`wgpu`, `smithay`, `libudev` (a `read_dir` and a netlink socket do what we
need of it), `fontconfig` (a `read_dir` and three alias tables do what we
need of it), `parley` and `rustybuzz` (see above).

## `png` versus our own decoder, measured (#3711)

Icons are coming, XDG icon themes are largely PNG, and the server will need
a decoder. `png` was already under Rejected for the *encoder* side, on the
argument that a stored-deflate writer is 120 lines of arithmetic. Decoding
is a real parser, so the rejection was re-opened with the only question
that settles it: **"are we sure a png crate saves code/ram/compile time?
Measure!"**

Both routes were built. `crates/nitro-png` is the one that landed; the
`png`-crate harness lived in a scratch consumer and is gone. Everything
below is measured, not estimated.

### The table

| | `crates/nitro-png` | `png` 0.18 |
|---|---|---|
| **new external crates** | **0** | **8**: `png`, `flate2`, `miniz_oxide` (×2, 0.8 *and* 0.9), `fdeflate`, `crc32fast`, `simd-adler32`, `adler2`, `cfg-if` |
| **`nitro-server` binary** | 2 282 776 B (**+50 712**, +2.3 %) | 2 361 872 B (**+129 808**, +5.8 %) |
| **clean build, wall** | 29.46 / 29.61 / 29.50 s | 29.97 / 30.14 / 29.98 s |
| **clean build, user CPU** | 62.10 / 62.14 / 62.30 s | 65.55 / 65.72 / 65.54 s |
| **incremental, wall** | 19.11 / 19.17 / 19.17 s | 19.64 / 19.57 / 19.65 s |
| **decode, 16×16** | 10.2 µs | 6.8 µs |
| **decode, 48×48** | 18.3 µs | 14.9 µs |
| **decode, 128×128** | 619 µs | 324 µs |
| **decode, 256×256** | 2178 µs | 823 µs |
| **decode, 512×512** | 13 327 µs | 5872 µs |
| **peak RSS over baseline, 512×512** | **+3.8 MB** | **+1.5 MB** |
| **lines: code we own** | **999** non-test code lines | 0 |
| **lines: code we ship** | 999 | **~28 500** across the nine crates |
| **`unsafe` in the dependency** | none | `flate2` 37 sites, `miniz_oxide` 4, `png` 1, `fdeflate` 1 |

Baseline is `nitro-server` on main: 2 232 064 B, 29.17 / 29.15 / 29.12 s
clean, 18.83 / 18.78 / 18.75 s incremental. Binary and compile figures are
the dev box (128 cores, `-p nitro-server`, release, the profile's own
`strip = true`, `docs/budget.md`'s methodology); each configuration is the
same tree with one dependency added and a hidden `--decode-png FILE` flag
in `main` that prints the decoded size and a checksum, so LTO cannot drop
the decode. **Decode speed and RSS are the test box** (Pentium G3240,
Haswell, no AVX2 — the CPU that matters), median of 21, release.

**Compile time is real but small, and in the crate's disfavour**: the two
clusters do not overlap in any of the three pairs, but the whole difference
is ~0.5 s wall against a 29 s build. Under the rule this file already
applies to `syn`, half a second is close enough to noise that it decides
nothing; the ~3.5 s of *user* CPU the crate route costs is the more honest
figure, and it is off the critical path on a many-core box for exactly the
reason the `syn` section gives.

### The crate is faster, and it does not matter at icon sizes

`png` wins on speed at every size, by 1.2× to 2.7×, and the gap grows with
the image. That is a real result and it is not a tuning miss that one more
afternoon would close: `simd-adler32` and `crc32fast` dispatch on runtime
CPU features, `fdeflate` has a specialised fast path for the
filter-0/RGBA8 shape most icons are, and `miniz_oxide`'s inflate is
heavily tuned. Ours already has a 9-bit Huffman lookup table, an 8-byte
bulk bit refill and `const`-generic unrolled filter rows; the remaining
gap is theirs to keep.

What decides it is the absolute number. **At the sizes a theme is made of
— 16 to 48 px — the difference is 3–4 µs per icon**, about the cost of a
syscall, against a first paint that is already tens of milliseconds. At
256×256 it is 1.4 ms, which is visible; at 512×512, 7.5 ms, which is a
frame. An icon loader does not decode 512×512 images, and if one ever does
— a thumbnailer, say — that is the moment to re-open this, with the
numbers already in hand.

### Peak RSS is the one place ours is worse in kind

+3.8 MB against +1.5 MB decoding a 512×512, over a 2.2 MB baseline, on the
box. Ours holds the whole filtered raster *and* the output BGRA at once;
`png` unfilters row by row into a one-row scratch. For a 48×48 icon that
is 9 kB of transient and irrelevant, but it is a design property rather
than an oversight, and on a 3.3 GB box with no swap it is the number that
would matter first if the decoder ever met a large image. The fix, if it
is ever needed, is a streaming unfilter — not a dependency.

### Why ours wins anyway

Eight crates and ~28 500 lines, to save 999 lines and 3 µs per icon. The
second `miniz_oxide` is the detail that makes the point: `flate2` wants
0.9, `png` wants 0.8, so the tree compiles two copies of the same inflate
implementation — and neither is the one that decodes our PNGs, because
`png` reaches for `fdeflate` first. That is four inflate implementations
in the dependency graph of a display server, to decode a 48×48 icon.

And 80 KB of `nitro-server` is not nothing against a binary this file's
sibling `docs/budget.md` watches to the byte.

The `unsafe` row is worth naming separately. This tree's one deliberate
exception is eleven lines in `nitro-seat`; the crate route adds 43 sites
across three dependencies, all of them in code that parses **untrusted
bytes**. `DEPENDENCIES.md` already flags font files as "the one input where
a dependency parses bytes we did not produce" and is careful that swash is
pure safe Rust. Icons come from the same place fonts do — system
directories — so the exposure is comparable, but the mitigation that made
fonts acceptable (a malformed table is a panic, not memory corruption)
does not hold for `flate2`, which wraps a C-shaped API even in its Rust
backend. Ours is `unsafe_code = "deny"` with no exception, and
`tests/fuzz.rs` asserts no input panics.

### What the comparison itself bought

The corpus diff is the part worth keeping in a document about
dependencies. Both routes decoded **6455 PNGs** (`/usr/share` and `/opt` on
the dev box — icon themes here are SVG, so `/usr/share/icons` has only 67
files and the box 13) and the pixels were compared **byte for byte**, not
"both returned Ok". That found a real bug in ours: `tRNS` colour-key
entries are two bytes big-endian **at every bit depth**, and the decoder
read the high byte — so at depth 8 every key compared as zero and the
black pixels of colour-keyed images silently went transparent. 59 files
disagreed; zero do now.

A test written by the same author from the same misreading of the
specification would not have caught it. That is the argument for measuring
against a mature crate even when the decision is to write your own — and
the reason the harness is recorded in the task thread rather than kept: it
requires the dependency it exists to reject.

The survey the interlace refusal rests on came out of the same run: **0 of
6455 files are Adam7-interlaced**, which `crates/nitro-png/tests/corpus.rs`
re-checks on whatever machine runs it rather than trusting this sentence.

### The conditions that would re-open it

A consumer that decodes images much larger than an icon (RSS, then speed);
an icon path measured to be decode-bound, which at 3–4 µs it will not be;
or a need for Adam7, APNG frames past the first, or colour management,
each of which is a feature we would be writing rather than a bug we would
be fixing.

The workspace figure is **77** lines and **37 distinct external crate
names** with `nitro-png` in — the name count unchanged, which is the number
this whole section exists to hold still.


## `rustix` features by crate

The feature set is per-crate, not workspace-wide, so each pays only for
the syscall families it uses.

| crate | features | used for |
|---|---|---|
| `nitro-wire` | `event`, `fs`, `net`, `process` | `poll` for the blocking handshake; `memfd_create`/`fstat`/`ftruncate` (tests) and `unlinkat`/`mkdir` for the socket path; `socket`/`bind`/`listen`/`accept`/`sendmsg`/`recvmsg` + `SCM_RIGHTS`; **TCP sockets and `sockopt` (`TCP_NODELAY`, `SO_KEEPALIVE` + the three keepalive timers, `SO_REUSEADDR`) for the remote transport — the same feature, not a new one**; `getuid` for the `/tmp` fallback path |
| `nitro-server` | `event`, `fs`, `net`, `process`, `time` | epoll loop, control socket, signals, timers; `pread` to copy client buffers out of their memfds, and `eventfd` for the test input source |
| `nitro-kms` | `event`, `fs`, `mm`, `net`, `time` | DRM fds, `mmap` of dumb buffers, udev netlink |
| `nitro-demo` | `event`, `fs`, `process`, `time` | `poll` for the event loop; `memfd_create`/`ftruncate`/`pwrite` for the image buffer; `getuid` for the `/tmp` fallback of the control-socket path; `clock_gettime` for the delivery-leg breakdown |
| `nitro-ui` | `event`, `fs`, `process`, `time` | `epoll` for the app loop, `poll` for the synchronous text measurement, `Timespec` for `ui.set_timer`; `memfd_create`/`ftruncate`/`pwrite` for an `Image` widget's pixel buffer; `getuid`/`getpid` for the introspection socket's path |
| `nitro-hey` | `process` | `getuid` for the `/tmp` fallback of the app-socket directory |
| `nitro-bar` | `time` | `clock_gettime` for the wall clock |
| `nitro-launcher` | `process` | `pidfd_open` so a launched child's exit is a descriptor the app loop can wait on rather than a thing noticed at the next spawn (#555), and `getpgrp` in the test that checks a launched process left the launcher's process group |
| `nitro-files` | `fs`, `pipe`, `event`, `process` | `inotify` for the live refresh of the directory on screen; `pipe` for the background scan's doorbell descriptor; `poll` for draining it and for the scan's own tests; `getuid`/`getpid` for the temporary-path fallbacks |
| `nitro-session` | `event`, `process` | `poll` over the pidfds, the session socket and the signal pipe; `pidfd_open` so a child's exit is a descriptor rather than a timer tick, `kill_process_group` for teardown, `getuid` for the `/tmp` fallback of the socket path |
| `nitro-term` | `pty`, `termios`, `process`, `fs`, `stdio` | `openpt`/`grantpt`/`unlockpt`/`ptsname` for the pseudoterminal; `tcsetwinsize` (`TIOCSWINSZ`) so a resize reaches the child as `SIGWINCH`; `kill_process_group`/`waitpid` to take the shell down with the window; `open` for the slave and `fcntl_setfl` to make the master non-blocking |

## `unsafe` exceptions

One, in `nitro-seat`: `close_device_fd` reclaims a device descriptor with
`OwnedFd::from_raw_fd` so the `OwnedFd`'s own `Drop` closes it. libseat
hands out a raw fd from `libseat_open_device` and `libseat_close_device`
does **not** close it (measured on libseat 0.9, logind and `noop`
backends); the `libseat` crate's `Device` has no `Drop` and consumes the
fd on close, so the caller owns it and the server leaked one fd per input
device per VT round trip. There is no safe way to turn a `RawFd` we did
not open into an owned one, and `rustix::io::close` is `unsafe` too. The
safety condition is narrow and local: the fd came from
`libseat_open_device`, libseat has just been told to close the device,
and the `libseat::Device` naming it has been consumed — so this is the
only owner. `tests/noop_backend.rs` asserts the process fd count is flat
across 16 open/close and 16 open/drop cycles.

The FFI-binding crates above (`libseat-sys`, `drm-ffi`,
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

A second deliberate non-use, and the one that cost a **binary**
dependency rather than a crate. A terminal's child needs its own session
and the pty as its controlling terminal, which are two syscalls that can
only happen between the `fork` and the `exec` — i.e. in
`CommandExt::pre_exec`, which is `unsafe` because the closure runs in a
forked child where almost nothing is async-signal-safe. `nitro-term`
therefore spawns **`setsid --ctty $SHELL`** (util-linux) and lets a
program that already does those two syscalls do them. The cost is honest
and is documented in `crates/nitro-term/README.md`: `setsid` is a
run-time dependency, and without it the terminal runs without job
control and says so.

It is worth noting beside `nitro-launcher`, which faced the same question
and answered it differently: `std`'s `process_group(0)` is the safe half
of `setsid`, and a launcher needs only that half because neither it nor
what it starts has a controlling terminal to worry about
(`crates/nitro-launcher/src/spawn.rs`). A terminal is precisely the case
where the other half matters, so it is the one place that pays.
