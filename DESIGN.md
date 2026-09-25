# nitro — architecture sketch

Status: draft v0. Everything here is up for argument; nothing is implemented.

## Goals (in priority order)

1. **Snappy.** Work is proportional to what changed, never to what is on
   screen. Idle means zero CPU. Input-to-photon latency is measured, not
   assumed.
2. **Small.** Low memory, few dependencies, short build. A dependency has
   to earn its place and is listed in `DEPENDENCIES.md` with the reason.
3. **Simple architecture.** Few components, narrow interfaces. Complexity
   may live *inside* a component (a rasterizer, a KMS backend); it may not
   leak *between* them.
4. **Remote and mobile from the same code.** The client/server wire is the
   remote protocol; the layout system knows about small screens, touch and
   rotation.
5. **Introspectable.** Every widget is addressable from outside the process
   (BeOS `hey`, accessibility, agents) through one mechanism.
6. **Rust.** Exceptions need a reason (kernel ABI shims, libseat).

Wayland is a later adapter, not a design input. BeOS is the spiritual
ancestor; Masonry/Xilem (Linebender) is the closest modern Rust reference
for the toolkit layer.

## The one-paragraph version

A single **server** process owns the seat (libseat), the display (KMS) and
input. It holds a **retained scene graph** of cheap primitive nodes — rect,
text run, image, clip, transform. **Clients** (apps, the bar, the launcher)
are separate processes that connect over a Unix socket and send *mutations*
to their part of the scene. The server computes damage, rasterizes only
what changed, assigns buffers to hardware planes where possible and commits
atomically. The **toolkit** in the client is a retained widget tree that
maps widgets to scene nodes and exposes the same tree over the same socket
family for scripting and accessibility. Remote = the same mutation stream
over TCP/SSH. Wayland = an adapter process that turns surfaces into image
leaves.

```
                 ┌───────────────────────────────────────────────┐
  clients        │ app (toolkit: widgets → scene mutations)       │  × N
                 └───────────────┬────────────────┬──────────────┘
                     nitro wire  │ (unix socket,  │ introspection
                                 │  fd passing)   │ (same framing)
                 ┌───────────────▼────────────────▼──────────────┐
  server         │  scene graph  →  damage  →  raster  →  planes  │
                 │  input routing ←  libinput                      │
                 │  seat (libseat)   KMS (drm)   [gpu: vulkan opt] │
                 └───────────────────────────────────────────────┘
                                 kernel: DRM/KMS, evdev
```

## Components

### `nitro-wire` — protocol

The only thing every other component depends on. A small binary framing
over `SOCK_SEQPACKET`/stream Unix sockets with `SCM_RIGHTS` fd passing;
framed identically over TCP for remote (fds become inline blobs or
out-of-band shared memory where available).

Design rules:

- Messages are **mutations on a tree of nodes with integer ids** allocated
  by the client (client id space is namespaced by the server). No object
  lifetimes to negotiate, no round-trips to create things.
- One **transaction** = a batch of mutations applied atomically at the next
  frame. Clients never see a torn frame; the server never re-lays out in
  the middle of a batch.
- Server → client: input events, frame-done with presentation timestamp,
  resize/scale/orientation, seat pause/resume, and *introspection*
  requests forwarded from third parties.
- The same node/mutation vocabulary is reused one level up by the toolkit's
  introspection tree (widgets instead of primitives), so there is one
  serializer, one debugger, one recording format.

Hand-written encoding, no serde in the hot path. `nitro-wire` must have
zero non-std dependencies.

### `nitro-server` — compositor

Single process, single-threaded event loop (epoll over: seat fd, DRM fd,
libinput fds, client sockets, timers). One thread is enough for a display
server if nothing blocks; rasterization is fanned out to a small worker
pool only when a frame's damage is large.

Sub-modules, each a contained blob behind a narrow trait:

- **seat** — `libseat`: opens `/dev/dri/cardN` and `/dev/input/*`, handles
  VT switch (`Deactivate` → stop flips, ack; `Activate` → re-modeset).
  Opened first, dropped last; every fd-holding object is its child. This
  is the one deliberate C dependency.
- **kms** — atomic modesetting via `drm` ioctls: connectors, CRTCs, planes,
  `FB_DAMAGE_CLIPS`, `IN_FENCE_FD`/`OUT_FENCE_PTR`, hotplug via udev
  netlink (raw socket, no libudev). Buffers are **dumb buffers** by default
  (works everywhere, zero Mesa); a `gpu` feature adds Vulkan (`ash`) images
  exported as dma-bufs behind the same `Buffer + Fence → Plane` interface.
- **scene** — the retained tree. Node kinds: `Group{transform, clip,
  opacity}`, `Rect{rrect, fill, border}`, `Text{run, colour, align}`, `Image{buf,

  src_rect}`, `Surface{external dma-buf}`. Each node caches its
  world-space bounds; a mutation marks the old and new bounds damaged.
  Windows are just top-level groups with a client owner and a z-order.
- **raster** — CPU 2D rasterizer producing exactly the damaged region into
  the back buffer. Anti-aliased rects/rounded-rects, solid/linear fills,
  glyph blitting from a server-side atlas, image scaling. Candidate: own
  minimal rasterizer; measure against `vello_cpu` before deciding. Text
  shaping/layout (`swash`, in `nitro-text`) lives server-side so clients
  send *strings*, not glyph pixels — this is what keeps the remote link
  thin and every app binary small.
- **planes** — decides per frame whether a node can be scanned out directly
  (fullscreen surface, video, cursor) instead of composited. Zero-copy
  scanout is the single largest power win on phones.
- **input** — `libinput` for devices; routing by hit-testing the scene;
  keyboard via `xkbcommon`. Pointer/touch/pen/keyboard unified into one
  small event enum. Gestures (pinch, swipe) are recognised in the server so
  phones and desktops behave identically.
- **clients** — socket accept, per-client id namespace, transaction
  application, frame scheduling (present-time driven: clients get a
  `frame` event with the deadline for the next flip).

### `nitro-ui` — toolkit (client library)

Retained widget tree, Masonry-shaped:

- Widgets live in an **arena indexed by id**; children are ids. No `Rc`, no
  parent pointers.
- **Passes**: `event`, `update`, `layout`, `paint`, `introspect`. Each a
  plain tree walk driven by dirty flags. `paint` emits scene mutations for
  dirty widgets only; `introspect` emits the accessibility/scripting tree
  for the same widgets.
- **`WidgetMut<'_, W>`**: the *only* way to mutate a widget; carries the
  context so `set_text()` marks layout/paint dirty automatically. This is
  what makes retained mode safe.
- **Reactivity** is property-level: an app holds plain `State`, callbacks
  receive `&mut State` (routed by widget id path, no `Rc<RefCell>`), and
  updating state pushes into widget properties through `WidgetMut`. No
  per-frame re-render, no virtual tree. A Xilem-style declarative layer can
  be added on top later; the core never depends on it.
- **Construction API** is builder-style and reads like gpui —
  `column().gap(8).child(button("OK").on_click(|s: &mut State| ..))` — but
  builds nodes once.
- **Layout**: flexbox subset (row/column/wrap, grow/shrink, min/max,
  gap, padding). Responsive breakpoints are a first-class widget
  (`Adaptive`) so phone vs desktop is a layout decision, not a fork.
- **Introspection**: every widget has a stable id, a role, a name, a value
  and a set of actions. Exposed over a per-app Unix socket using the
  `nitro-wire` framing: `list`, `get`, `set`, `invoke`, `subscribe`. The
  same tree feeds an AT-SPI bridge later. This is goal 5 and is in from
  the first widget.

### Shell and apps

Ordinary clients, each a small binary:

`nitro-bar`, `nitro-launcher`, `nitro-settings` (display/audio/system),
`nitro-term`, `nitro-calc`, `nitro-files`, `nitro-amp` (a Winamp-style
music player). Shell-only privileges (place a
window on the top layer, receive global hotkeys, read the window list) are
granted per socket by the server on connect (socket path / peer creds), not
by protocol extensions.

Session policy that talks to `logind` over D-Bus (suspend, power off,
lock) lives in one side daemon, `nitro-sessiond`, and is the only place a
D-Bus client is allowed.

Login is not ours to run as root. `greetd` (a system package, not a
crate) owns PAM and the VT; `nitro-greeter` is an ordinary nitro-ui
client that speaks its four-message socket protocol, hosted by
`nitro-session --greeter`. Sketch and reasoning: `docs/greeter.md`.

### Adapters (later)

- `nitro-remote`: the server accepts the wire over TCP/SSH; a thin
  `nitro-view` client renders a remote scene locally. No pixels cross the
  link unless a node is an `Image`.
- `nitro-wayland`: a separate process speaking Wayland to legacy clients
  and forwarding each surface as a `Surface` node with its dma-buf. Keeps
  Wayland's object model out of the server.

## Cross-cutting rules

- **Damage everywhere.** Every layer produces and consumes damage regions:
  widget dirty → scene bounds → raster region → `FB_DAMAGE_CLIPS` /
  remote packet. A frame with no damage produces no flip.
- **Present-time scheduling.** The server tells clients when the next flip
  is; clients aim for it. No free-running render loops anywhere.
- **Budget per frame.** Input → photon within one refresh at 60 Hz on the
  workstation is the target; the tracing harness records where every
  millisecond went.
- **Everything headless.** The server runs against a fake KMS device and
  writes PNGs; the toolkit has a test harness that builds a tree, injects
  events, and snapshots. This is also the screenshot-over-SSH path:
  `nitro-shot` asks the live server for a readback of the front buffer.
- **No global allocator churn in the frame path.** Scene and arena nodes
  are pooled; transactions are parsed in place.
- **No `unsafe`.** `unsafe_code = "deny"` workspace-wide. Exceptions are
  explicit `#[allow(unsafe_code)]` on the smallest possible item, carry a
  `// SAFETY:` comment, and are confined to the ABI shims (ioctls, fd
  passing, mmap). The rest of the tree — scene, raster, toolkit, apps — is
  100% safe Rust.
- **Clippy pedantic, deny.** Workspace lints in `Cargo.toml`; every crate
  sets `[lints] workspace = true`. Per-lint allows are workspace-level with
  a stated reason, not sprinkled through the code.
- **A green test is evidence only about what it looked at.** A test can
  check something *adjacent* to the claim in its name — the other file,
  the other parity, the second frame — and stay green while the claim is
  false. Assert over every artefact the claim covers, and make a checker
  that finds nothing say so. `docs/bench.md` §11.1, with the worked
  example in `crates/nitro-shm/tests/seals.rs`.

## Milestones

- **M0** — **done.** `nitro-server` boots on KMS via libseat, shows a
  gradient + frame + vblank-paced bar, takes a screenshot over SSH
  (`nitro-shot`, own PNG encoder), survives VT switch. Headless fake-KMS
  backend and PNG output in CI. (`nitro-wire` moved to M1 — the M0 control
  socket is a throwaway line protocol.) Measured on the test box
  (Pentium G3240, i915, 1920×1080@60): idle CPU 0.0 % with no context
  switches, RSS 3.4 MB, flip interval 16.666 ms mean (16.65–16.68 ms),
  moving-bar mode 6–7 % CPU, 10 VT round-trips clean, `systemctl stop`
  exits 0 and returns tty1.
- **M1** — **done.** Scene graph, exact damage, CPU raster of rects and
  images; `nitro-wire` v1; clients drawing through mutations; libinput and
  xkbcommon input routed by hit test; measured input-to-photon latency.
  The server is a compositor: clients connect on
  `$XDG_RUNTIME_DIR/nitro/wire.sock`, their transactions land in the scene
  at `Commit`, damage drives the rasterizer into the KMS back buffer under
  the age-2 rule (the region painted is `damage(n) ∪ damage(n-1)`, because
  the back buffer is two frames stale), and input is routed back to the
  window under the pointer with window-local coordinates. A software
  cursor keeps screenshots honest until the hardware plane arrives in M3.
  Measured on the test box (Pentium G3240, i915, 1920×1080@60) with a
  client connected: idle **0.0 % CPU with zero voluntary context
  switches**, RSS 7.5 MB (client 3.2 MB), flip interval 16.666 ms mean
  (16.653–16.680), paint 0.19 ms mean per pointer-move frame (a
  full-screen repaint is the 13 ms maximum). **Input-to-photon, measured
  end to end by `nitro-demo` over 202 samples: median 9.3 ms, p95
  17.1 ms, min 1.3 ms** — inside one refresh, so the "within one refresh
  at 60 Hz" budget is **met**. It was missed by exactly one frame
  (median 25.2 ms) until the frame scheduler stopped flipping cursor-only
  damage while the client under the pointer still owed an answer: the
  cursor and the client's content now ride the same flip, bounded by a
  timerfd deadline so a client that never answers cannot stall the arrow
  (issue #529). Full method, before/after, the rate sweep and the
  saturation trap that hid the improvement are in `docs/latency.md`;
  **throughput** — the other half of the question, how much drawing per
  frame the server sustains and what a frame costs in CPU — is in
  `docs/bench.md`, measured by `nitro-bench` against the 1988 `x11perf`
  operations and the classic demo effects;
  sizes and RSS in `docs/budget.md`. Three VT round trips with a client
  connected are clean and input still routes afterwards. Text was the one
  thing the milestone promised and did not deliver — M1 shipped rects and
  images only — and M2-pre below has since closed that gap.
- **M2-pre** — **done.** Text end to end. `nitro-text` (swash) does font
  discovery, shaping, layout, measurement and an A8 glyph atlas *in the
  server*; `nitro-wire` grows `SetText`/`MeasureText` and
  `TextMetrics`/`TextMeasured` behind the `TEXT` capability bit (v1 is not
  bumped — new ops, new bit, exactly as the versioning policy prescribes);
  `nitro-scene`'s `Text` kind becomes live, holding an opaque store handle
  rather than any font type; and `nitro-raster` learns `blit_mask`. A
  client sends a *string*, never a glyph, which is what keeps the remote
  link thin and every app binary small.
- **M2** — **done.** `nitro-ui` with arena, passes, `WidgetMut`, eleven
  widgets, layout, introspection socket, the `hey` CLI, and `nitro-calc`
  as the first app.
  The toolkit core is in: widgets in a generational arena, take-out
  dispatch so a callback gets `&mut State` *and* `&mut Ui`, `WidgetMut` as
  the only mutation door, TREE/LAYOUT/PAINT driven by dirty flags into one
  `Commit`, a flex subset with pure-function tests, `Flex`/`Panel`/
  `Label`/`Button`/`TextField`/`Checkbox`/`Slider`/`Scroll`/`Separator`/
  `Image`/`Spacer`, an epoll app loop, and a harness that runs a real
  server in-process and asserts on pixels *and* on the mutations sent.
  Every app opens an introspection socket and answers `list`/`get`/`set`/
  `do`/`watch`/`shot` on the app's own loop, between events — so `hey
  nitro-calc do window/7 click` runs the real callback with the real
  `&mut S`, and being scriptable costs neither a thread nor a lock.
  **`nitro-calc` is the exit criterion, measured on the box**: 489 lines
  of app source excluding tests, a **560 KB** stripped binary, **2 752 kB**
  RSS, one thread, **0.0 % idle CPU with zero context switches** in the
  app *and* the server, **one keypress is exactly two mutations
  (`SetText`, `Commit`)**, and keypress-to-photon of **1.9 ms min /
  ~12–14 ms mean** by the server's `i2p` counters. The mean sits above
  `nitro-demo`'s 9.3 ms pointer figure because every new digit is a new
  string, and text measurement is a synchronous round trip in M2
  (cached; `docs/ui.md`) — the first measurement to put a price on that
  decision, and the argument for making it async in M3. Numbers and
  method in `docs/budget.md`.
- **M3** — **done.** Shell: bar, launcher, wallpaper, window management
  (focus, move, resize, z-order), the privileged shell socket, and a
  session that starts and supervises all of it. **The exit criterion is
  a desktop on the test box**, and the measurement is the whole tree
  rather than the compositor alone:

  | | |
  |---|---|
  | processes / threads | **5 / 5** (session, server, wallpaper, bar, launcher), one thread each |
  | whole-desktop RSS | **28 968 kB** at the M3-E exit run. Re-measured later with the anon/file split and two more windows: **30 700 kB, of which 13 372 kB is private** — `docs/budget.md` §"The M3 desktop" carries that row, and it is the one to quote for a memory claim. |
  | idle CPU over 60 s | **0.00 %** for four of five; 0.02 % on the server, which is the bar's once-a-minute clock |
  | `nitro-session` | 2 792 kB RSS, **511 KB** binary, 0.00 % idle |
  | `nitro-server` | 17 840 kB, of which **8 208 kB is #539's shadow buffer** (9 536 kB with `NITRO_SHADOW=0`) |
  | **server budget, revised** (#538) | **`RssAnon` ≤ 2.5 MB + 100 kB per decorated window, plus one scanout-sized shadow buffer per output; `RssFile` ≤ 7.5 MB and flat in windows.** Measured: 2 424 kB floor, 3 980 kB with five windows, 7 252 kB file, 8 100 kB shadow. The old "≤ 8 MB RSS" line did not say anon or total, so it could not tell an allocation from a linked library. |
  | bar killed → back | **1.11 s** (budget 2 s), and it re-reads the window list on its own |
  | restart backoff, measured | 1 → 2 → 4 → 8 → 16 s, reset to 1 s by a 43.1 s run |
  | 3 × VT round trip | same pids throughout; input still routes, Super still opens the launcher |
  | `systemctl stop` | **0.49 s**, `Result=success`, exit 0, no processes left, tty1 back |

  The acceptance run drove the real thing with `ydotool`: Super tap →
  launcher → type `calc` → Enter → `nitro-calc` appears decorated;
  `hello_dialog` beside it; a titlebar drag moved a window by −400,−158
  against −400,−160 asked; a corner drag resized 223×334 → 454×508 with
  the top-left pinned; `Alt+Tab` cycled MRU focus both ways. Numbers,
  method and the picture are in `docs/budget.md` §"The M3 desktop".

  - **M3-A done.** Server-side window management: decorations (opt-out per
    window), server move/resize with zero client round-trips, focus and an
    MRU `Alt+Tab` cycle, `Normal`/`Maximized`/`Fullscreen`/`Minimized`
    states with size limits, a centred-cascade placement inside a per-output
    work area, multi-output layout with per-output scale, output and
    input-device hotplug. The wire grew the `WM` capability bit and four ops
    behind it; v1's byte layout is unchanged. Model in `docs/wm.md`.
  - **M3-B done.** The **shell socket**: a second `nitro-wire` listener
    whose connections carry `caps::SHELL`. *The socket is the
    capability* — a client is privileged because of where it connected,
    not because of anything it sent — and the whole check is one `if`
    against a token range. Layers, anchors, exclusive zones, keyboard
    grabs, the global hotkey table and the Super-tap state machine.
    `docs/shell.md`.
  - **M3-C done.** `nitro-bar`: window list, clock, battery/load/memory,
    on an exclusive zone the server subtracts from every other window's
    work area.
  - **M3-D done.** `nitro-launcher` (a Super-tap overlay that searches
    `.desktop` files and spawns what you pick) and `nitro-wallpaper`
    (the smallest possible shell client: one surface, painted once,
    silent thereafter).
  - **M3-E done.** `nitro-session`: starts the server, waits for it to
    really answer, starts the shell, supervises it, and answers the
    power actions on `$XDG_RUNTIME_DIR/nitro/session.sock`. A shell
    piece that dies is restarted with backoff; the *server* exiting ends
    the session with the server's code; `SIGTERM` tears down in reverse
    order inside one deadline. Power actions go through `systemctl`
    rather than D-Bus — `zbus` is ~40 crates against a tree of 34, and
    `systemctl suspend` *is* a logind call. `lock` is the one action
    that needs what only D-Bus provides (an inhibitor held across the
    suspend, so a lock screen can paint first), and it is the one action
    deferred to M4. Reasoning in `crates/nitro-session/README.md`.

  Two user-visible defects were found by running the thing rather than
  testing it, and both are worth recording because of *why* the tests
  missed them. A launcher whose result rows had bounds, labels and
  working clicks painted nothing (a clipping group sized `EMPTY`;
  #3697), and the launcher was **on screen from boot** because its
  start-up `hide()` returned early on a `visible` flag that was already
  `false` — the flag and the server disagreed, and every test asked the
  flag. Both now have pixel-level tests; the second one
  (`the_launcher_is_not_on_screen_before_the_first_tap`) compares whole
  screenshots, because the compositor's backdrop is a gradient and no
  single colour is "the background".
- **M4** — Terminal, settings (display/audio), file manager; remote view;
  phone build.

  - **M4-A done.** `nitro-term`: a terminal emulator on `nitro-ui` — a
    pty, a VT parser, a cell grid with scrollback and an alternate
    screen, and a widget that draws one `Text` node per same-style run.

    It is here because it is **the hardest test of goal 1**. Every app
    up to now changed a label or a clock; a terminal's output is
    produced by a program that has never heard of a display server, so
    "work proportional to change" either survives `yes | head -100000`
    or it was a claim about toy workloads. Three mechanisms keep it, and
    `docs/term.md` has the reasoning: a stable paint slot per run (so an
    unchanged run costs zero bytes), a per-row damage bit consulted
    *before* the row is split into runs (so a clean row is not walked at
    all), and `RequestFrame` pacing (so the scene is touched once per
    frame rather than once per line).

    Measured from outside by counting mutations: a keystroke into an
    existing row is **two** mutations — one `SetText` for the run, one
    `SetBounds` for the cursor — and twenty thousand lines of `seq`
    cost no more commits than there were frames. An idle terminal at a
    prompt schedules no timer, requests no frame and sends nothing.

    On the box, against the kernel's own terminal on tty1:

    | | nitro-term | Linux console |
    |---|---|---|
    | `seq 1 1000000` | **0.44 s** | 129.5 s |
    | `cat` 5 MB | **0.11 s** (47 MB/s) | 4.14 s |
    | keystroke i2p (mean / max) | **12.6 / 22.7 ms** | — |
    | idle, 30 s at a prompt | **0 frames, 0 ticks** | — |
    | RSS, 10 000 scrollback lines | **4.2 MB** (budget 6) | — |
    | binary | **650 KB** (budget 900) | — |

    The ~290× on `seq` is not cleverness in the terminal, it is the
    design: the console draws every line and nitro-term draws **thirty
    frames**, so 999 970 lines were parsed into the grid and overwritten
    without ever being rasterized. Read "2.26 M lines/s" as a
    parse-and-discard rate; what the user sees is thirty legible frames
    and a child that never waits.

    **A terminal barely moves the server** — 18.4 → 18.9 MB, and
    `atlas_pages` stays at 1 — which is the question the milestone
    actually asked of the per-run `SetText` design. The heaviest text
    client there is added 76 glyphs to the atlas, because a terminal
    draws the same ASCII over and over at one size in one family. The
    spec asked for a cell-grid wire op to be proposed if per-run text
    turned out to be the bottleneck; it measured 239 runs and ~1 ms of
    paint per frame under `htop`, 7 % of a refresh, so **no issue was
    filed and the wire is unchanged**.

    **Four defects were found by running it on the box and none by the
    test suite**, which is the M3 lesson repeating and is worth the same
    honesty. Two were memory, and the second hid behind the first:
    parking the cursor's paint slot at `Slot::MAX` made the framework's
    *dense* slot vector allocate 65 536 entries — 11.8 MB, present even
    with `--scrollback 0`, which is what finally cleared the scrollback
    of suspicion — and a scrollback row trimmed *in place* left the
    allocator a full-width hole the next blank row could not reuse, so
    every row was two cells long and RSS still grew by a full row per
    line. Only a measurement of the process could separate them, which
    is why `a_full_scrollback_costs_what_its_text_costs` reads
    `/proc/self/status`. Together: **29.4 MB → 4.2 MB**.

    The other two were scripting, and both made `hey` look like it
    worked: `get grid text` returned nothing because `Role::Terminal`
    was not in the set that property is derived for, and
    `set grid value 'ls\n'` wrapped its bytes in the bracketed-paste
    markers — whose entire purpose is to tell readline *not* to execute
    what arrives, so under bash 5.1+ every scripted command sat unrun on
    the prompt. A script driving a terminal is a keyboard, not a
    clipboard.

    Two more came from the same place. **Frame pacing has a trap**: "read the
    pty until `WouldBlock`, then take a frame" never comes back while
    the writer is faster than the reader, so `cat` of a 5 MB file was
    consumed in one drain and one commit — a perfect score by the letter
    of the claim, describing a terminal that showed nothing for two and
    a half seconds and then jumped to the end. A drain is now capped at
    256 KiB. And **a text node's box is sized to the row, not to its
    run**: the width is invisible (runs are left-aligned and unwrapped)
    but it is *diffed*, so a box sized to the run grew by a cell per
    typed character and put a `SetBounds` next to every `SetText`. That
    one is the difference between a keystroke costing two mutations and
    three.

    The toolkit gained only general things: frame callbacks
    (`request_frame`/`on_frame`), `SetWindowTitle`/`SetWindowLimits`, a
    `u16` paint-slot index, `PaintCx::keep` ("this slot is unchanged" —
    the third answer beside emit and omit, for a widget whose slots are
    its content rather than its parts), and `Role::Terminal`. All are in
    `docs/ui.md`.

    The one `unsafe` this app would have needed is bought with a
    dependency instead: a child needs `setsid` + `TIOCSCTTY` between
    fork and exec, which is `pre_exec`, so the shell is started as
    `setsid --ctty $SHELL` and the terminal degrades honestly (no job
    control, and it says so) where util-linux is missing. It is the
    mirror of `nitro-launcher`'s conclusion that `process_group(0)` is
    the half a launcher needs — same question, different answer, and a
    terminal is the case where the other half matters.

    `vte` is the first external dependency a workspace crate has added
    since M0: +2 crates, for the DEC parser state table and nothing
    else. It assigns no meaning — the grid, the damage, the colours and
    the key encodings are all `nitro-term`'s, which is why `grid.rs` and
    `vt.rs` carry a hundred unit tests. Argued in `DEPENDENCIES.md`.

  - **M4-C done.** `nitro-settings` and **`server.conf`**: the display
    server's configuration stopped being an environment variable.
    Per-connector scale, position and primary, plus the keyboard layout,
    now live in `$XDG_CONFIG_HOME/nitro/server.conf` — read at startup,
    watched with inotify, and re-applied without a restart. It is what
    `docs/wm.md` deferred in M3 under "a persistent output layout";
    `NITRO_SCALE` survives, demoted from stop-gap to dev override.

    The **file is the contract and the app is one editor for it**. That
    ordering decided the format: plain `key = value` lines with `#`
    comments, a 40-line parser and no TOML dependency, because there are
    no tables, no arrays and no types beyond a float, an integer pair and
    a string. It survives `sed`, a settings app, and a person with a
    broken desktop and a text console.

    **Nothing in that file can fail.** It is user input that arrives
    while the compositor is running, so a bad line is a logged warning
    and a skipped line — never a stopped desktop. Scales are clamped to
    0.5–8 rather than trusted: `scale = 20` would render the desktop at
    twenty times and leave nothing clickable with which to undo it,
    including the settings app. `keyboard.repeat` is refused **by name**
    rather than falling into "unknown key", because nothing in nitro
    repeats keys yet and a setting that appears to work and changes
    nothing costs a user an afternoon.

    A configured position moves the output in **both** spaces — the
    desktop layout windows are placed in and the device rectangle the
    pointer is clamped to. Half-doing it would make the pointer cross
    between screens somewhere other than a dragged window does.

    On the box: scale 2 and back, layout `de` → `y` types `z`, both by
    reload with no restart. `nitro-settings` is **2 872 kB RSS** (budget
    3.5 MB) and **0 CPU ticks over 30 s idle**, as is the server with the
    watch armed — an inotify fd with nothing queued is not readable, so
    it never wakes the loop. The binary is **719 240 B against a 700 KB
    budget, 2.7 % over**, and is recorded that way rather than rounded:
    three sections, a config renderer, a control-socket client and a
    subprocess audio backend against `nitro-calc`'s one keypad, on a
    budget set before any of them were specified.

    **The defect that mattered was found by running it, not by thirteen
    passing integration tests** — the M3 and M4-A lesson a third time.
    The inotify watch goes on the file's *parent directory*, because a
    crash-safe save is a rename and a rename replaces the inode. That
    directory has to exist for `inotify_add_watch`, and on a machine that
    has never been configured it does not — so the watch was never armed,
    never retried, and the one event guaranteed to be missed was the
    first file a settings app ever writes. Every fresh installation is in
    that state. The suite could not see it because **the harness created
    the config directory before starting the server**: the convenience
    was the hiding place, and the fix came with a constructor that leaves
    it out and a test that fails without it.

    Two measurement notes, in the discipline M4-B4 established. The scale
    claim is settled on **pixels** — the bar's strip measures 32 device
    rows at 1× and 64 at 2× — because `stats` and a config file would
    both be satisfied by a desktop that never re-laid out, and
    `hey get window bounds` reports the same numbers at either scale,
    logical geometry being scale-invariant by design. And the keyboard
    test carries a control: the same evdev keycode yields `z` under `de`
    and `y` under `us`, without which it could not tell "the layout
    applied" from "that key is z". The server's RSS cost for the config
    and the watch came out **below what a 3.3 GB box can resolve**
    (+68, +744, +36 kB over three interleaved pairs against a
    ~110–668 kB A-side spread), and is reported as that rather than as a
    mean implying a precision it does not have.

    Zero new dependencies.

  - **M4-D done.** `nitro-files`: a file manager on `nitro-ui` — a
    virtualised list over a directory, an editable path bar, a
    status-line confirm instead of dialogs, and the freedesktop trash,
    MIME and `.desktop` machinery under it (`dir`, `mime`, `trash`,
    `ops` — four modules with no widget in them, tested without a
    display server).

    It is here because it is **the app that made the toolkit's last
    "this is M3" deviation real work**. `docs/ui.md` had carried a
    bullet saying a list of ten thousand rows costs ten thousand widgets
    and that virtualisation is "a widget, not a new mechanism" — a
    comfortable thing to write while every app on the stack showed a
    calculator's worth of widgets. A directory is the only model whose
    size the user chooses and nothing bounds, so either the bullet was a
    plan or it was an excuse. **`nitro_ui::List` is that widget**: it
    materialises `visible + 2` rows whatever the model holds, a scroll
    inside the two spare rows is one `SetTransform`, a re-anchor re-emits
    only the rows that changed, and moving the selection is two
    `SetFill`s. Each of those three is asserted from outside by counting
    mutations, because a cost claim nothing checks stops being true.

    The second hard thing is the one with the wider consequence.
    Reading a directory is the first piece of work in this tree that is
    **unbounded and not ours** — `/usr/bin` is two thousand entries and a
    `stat` each, and a directory on a sleeping NFS mount returns in
    thirty seconds. A directory of more than 2 000 entries is therefore
    read on a **thread**, and the result arrives through a pipe
    registered with `Ui::add_fd`: the channel carries the payload, the
    pipe carries the fact that there is one, because a channel cannot
    wake `epoll_wait` and pushing a `Vec` through a descriptor would mean
    inventing a serialisation between two threads of one process. The
    toolkit gained no thread integration and needs none — a descriptor is
    already what the loop waits on — and `docs/ui.md` now records
    "long work off the loop" as the general pattern, with the
    level-triggered-`epoll` rule that a hook whose descriptor stays
    readable must be removed (`Ui::remove_fd`) or drained.

    Opening a file **reuses the launcher rather than copying it**:
    extension → `globs2`/built-in table → `mimeapps.list` then
    `mimeinfo.cache` → `.desktop` → `nitro_launcher::desktop::parse` →
    `nitro_launcher::spawn`. That path already detaches the child into
    its own process group, sends its stdio to `/dev/null`, **drops
    `NITRO_SHELL_SOCKET`** from its environment and reaps through a
    pidfd; a second implementation would be a second place for each of
    those to be forgotten, and the one most likely to be forgotten is the
    environment subtraction nobody can see. Reuse also fixed #555 for
    everyone: launched children are now reaped **when they exit**,
    through a pidfd registered with `Ui::add_fd`, rather than at the next
    spawn. Zero new external crates — `nitro-ui`, `nitro-launcher`,
    `rustix` (`fs`, `pipe`, `event`, `process`).

    Measured on the **test box** (Pentium G3240, no AVX2, 1920×1080),
    release, driven through `hey` and real `ydotool` input:

    | | nitro-files | budget |
    |---|---|---|
    | RSS, `/usr/bin` listed (1 860 entries) | **3 592 kB** | ≤ 4 MB |
    | binary, release, stripped | **817 352 bytes** | ≤ 800 KB — **2 % over** |
    | idle 60 s, `/usr/bin` listed, watch armed | **0 CPU ticks, 1 ctxt switch, +2 server frames** | 0 |
    | the same minute with the app killed (control) | **+2 server frames** | — |
    | Page Down over `/usr/share`: pixels changed | **704×408 = 287 232** | ≈ the list, not the 2 073 600 px screen |
    | `/usr/bin`, `set path value` → rows | **25 / 35 / 25 ms** | — |
    | 50 000 entries listed end to end (dev box) | **0.18 s** | < 1 s |
    | threads | **1** | — |

    Three of those are the milestone's actual claims, and each was
    taken with the instrument that could contradict it. **The damage is
    settled on pixels**, not on `stats`: `damage_px` is the server's own
    opinion about what it repainted, which is the thing under test, so
    the number above is the bounding box of the pixels that actually
    differ between two framebuffer readbacks either side of one Page
    Down — 704×408, the list's bounds exactly. `damage_px_mean` agrees to
    the pixel. **Idle is zero with inotify armed**, and the two frames
    the server did advance are the bar's minute clock rather than this
    app: the same minute with `nitro-files` killed advances the same
    two. **The UI answers `hey` while a 50 000-entry scan is in flight**,
    which is the whole point of doing the read off the loop. The binary
    is **17 KB over budget and the attribution is known**: the app links
    the toolkit's ~68 KB introspection protocol, monomorphised per
    app-state type, and the `dyn`-interface fix `docs/ui.md` already
    records would more than cover the overrun.

    **The box found three defects the whole suite could not**, and the
    first is the toolkit's. `Ui::add_fd`'s token *was* the raw fd of the
    toolkit's own `dup`; descriptor numbers are recycled the instant
    they are closed, so a hook removed and another added in the same
    turn took the same token and the app loop — which keys its
    already-registered list on it — never put the new one in the `epoll`
    set. A file manager that re-arms its watch on every navigation
    therefore refreshed the first directory and no directory
    afterwards, with nothing returning an error and every test passing,
    because the tests call `Ui::run_fd` directly and `run_fd` was fine:
    only the loop was wrong. `FdToken` is now an opaque monotonic `u64`
    (`a_re_armed_fd_hook_gets_a_fresh_token`). The other two were this
    app's: a `Delete` confirmation whose `y`/`n` was eaten by the list's
    type-ahead — so the answer depended on the file names in the
    directory, and a pending question now takes the keyboard — and
    `EXDEV` reaching the status line as "os error 18" instead of a
    sentence.

    **A fourth defect came out of the integration tests, and it is the
    toolkit's too.** Take-out dispatch means the one widget a callback
    cannot reach is itself — `widget_mut` answers `Error::Busy`, a value
    rather than a panic. That is the right design and it is fine for a
    button, whose `on_click` changes something else. It is not fine for
    a list, where "activate this row" means "show different rows
    **here**": `nitro-files` wrote the new rows with `if let Ok(mut l) =
    …`, the `Err` went into the `if let`, and the path bar updated while
    the rows on screen did not — with nothing anywhere returning an
    error anybody read, and a direct call to the same function working
    perfectly. `Ui::defer` queues such work until every widget is back
    in its slot, which is not a new mechanism but the one `Ui::focus`
    has always used for the identical reason. Every swallowed
    `Error::Busy` in the app is now a reported one.

    That is three bugs found by running the program and one by writing
    tests against it, none by the 78 that already passed — and all four
    share a shape worth naming: **the instrument agreed with the code
    because it was measuring the layer below the broken one.**
    `hey get path value` read the app's state, not the widget's; the
    tests called `Ui::run_fd` directly, not the loop that dispatches it.

    Limitations are recorded where a reader will find them
    (`docs/files.md`): no drag and drop and no cross-process clipboard
    (there is no clipboard protocol yet, so `Ctrl+C`/`Ctrl+V` are the
    app's own), no thumbnails, no mounts UI, no icon theme, UTC
    timestamps, symlinks not stat'ed through in the listing, and
    single selection by pointer — `PointerDown` carries no modifier
    mask, so multi-selection is keyboard-only.

  - **M4-F done.** **Colours are roles; the server is the switch.** One
    semantic palette in `nitro_core::palette` — `Role` names a *meaning*
    (`Accent`, `TextDim`, `TitleBarActive`, `Ansi1`), `Palette` answers
    with a `Color` — the server owns which palette is in force
    (`theme.scheme = light|dark` plus per-role overrides in
    `server.conf`), and it **pushes** it to every client over a new
    `Theme` op behind caps `THEME`. One line in a file re-colours the
    whole desktop in a frame, with nothing restarted.

    It is here because the alternative was already failing. `nitro-term`
    shipped its own sixteen ANSI colours, the window manager its own
    `BAR_ACTIVE`, the wallpaper its own gradient: three palettes, three
    opinions about "dark", and no place to put a switch — because a
    central colour setting needs a central place colours come *from*,
    and a `Theme` struct each app copies and tweaks is not one. A role
    fixes the three failure modes at once (drift, colours no setting can
    reach, and nowhere for the switch to live) because it is a name for
    a meaning rather than a value.

    `nitro_ui::Theme` survives as a **view** on the palette, so a
    `Button` needed no changes; what changed is that its colours are a
    projection and its metrics (font, radius, paddings) are not, so a
    scheme switch moves the first and leaves the second alone. A palette
    push costs **one commit** per client and the tree is silent
    afterwards — asserted from outside by counting commits, because a
    switch that repainted per widget would look identical and cost a
    commit per label.

    The claim is enforced rather than remembered: `deploy/lint-colors.sh`
    fails the build on a `Color::rgb(` or a bare `0xRRGGBB` outside a
    short allow-list, and it runs from `just clippy` and the default
    recipe. Two lines in the tree carry an explicit exemption, and both
    are the same honest case: a colour that is **the program's or the
    user's**, not the desktop's — a truecolor `SGR 38;2;r;g;b`, which is
    content, and `nitro-wallpaper --color`, which is an override. The
    256-colour cube stays xterm's arithmetic for the same reason.

    Two things the wire design had to get right. The colour **count is
    on the wire** (`vec<Color>`, not a fixed `N`), so appending a role is
    compatible in both directions — a short table leaves the rest at
    defaults, a long one has its tail ignored — which is what lets a
    desktop mid-upgrade render instead of refusing to start; roles are
    therefore only ever *appended*, since a role's position is its wire
    index. And the `Theme` arrives **immediately after `Welcome`**,
    before the first `Configure`, because a client's first paint happens
    before its first round trip and a palette one trip later means every
    app on the desktop flashes its built-in defaults for a frame.

    The tests hold both schemes to **WCAG AA** — 4.5:1 for every
    text/background pair, 3:1 for secondary text and for the twelve
    chromatic ANSI colours against their own terminal background — with
    the contrast formula implemented in twenty lines rather than
    eyeballed, so a "nicer" grey that quietly makes a label unreadable
    fails the build. The light scheme's ANSI colours are *darkened*,
    which is not taste: the familiar saturated sixteen are chosen for a
    dark background, and `ls --color`'s blue directory on paper is
    genuinely unreadable.

    **Two defects came out of writing the settings tests**, and both are
    the same shape as M4-D's: the instrument agreed with the code
    because the two parsers of one format disagreed quietly.
    `nitro-settings` has a second implementation of `server.conf` (so a
    settings dialog does not link libinput and drm), and `#` starts a
    comment exactly where a colour literal's `#` sits — so its parser ate
    `theme.accent = #6ca8f0`, and Apply, which rewrites the file
    wholesale, would have **silently deleted a colour the user
    hand-picked**. Both parsers now make the same narrow exception (a `#`
    followed by six or eight hex digits is a value) and both have a test
    pinning it. The second was Apply rendering the whole file from the
    widgets, which dropped the `theme.*` block entirely; it is now
    carried over from disk verbatim, overrides included — the one
    carve-out to "Apply rewrites the file wholesale", and it earns it.

    Folded in on the way through: **issue #558**. The config watch asked
    for `CLOSE_WRITE|MOVED_TO|CREATE` only, so `rm server.conf` was not
    an event at all and a `theme.scheme` or `keyboard.layout` from a file
    that no longer existed stayed in force until something else triggered
    a reload. Deleting the file is the documented way back to defaults,
    so it has to be one.

    Role table, config keys, wire op, lint rule and how to add a role:
    `docs/theme.md`.
  - **M4-E1 done.** **Remote apps: the same wire over TCP.** An app runs
    on machine B, connects to the server on machine A, and its window is
    on A's screen — decorated, focusable, `Alt+Tab`-able, driven by A's
    keyboard and pointer. `NITRO_SOCKET=tcp://host:port` on the client,
    `remote.listen = <addr>:<port>` in A's `server.conf`, and nothing
    else. `docs/remote.md`.

    The point of the milestone is how little code it is, and *why*.
    `docs/wire.md` has said since M1 that the framing does not depend on
    the socket being local, and `caps::REMOTE` — "the link is remote:
    buffers are expensive, text is cheap" — has been bit 3 since M1. This
    is the first time either claim was **tested**, and they held: the
    messages are little-endian `repr(C)` bodies behind an 8-byte header,
    so a stream socket is a stream socket. The endianness claim is now
    pinned against a byte literal rather than assumed, so a field that
    ever becomes native-endian fails on the machine that built it rather
    than on someone's ARM laptop.

    What did **not** come for free is the descriptor channel, and that is
    the whole design of the feature. An image's pixels ride on
    `SCM_RIGHTS`; TCP has no equivalent and there is no honest fallback
    (16 MiB of inline pixels on the one link that is slow). So a
    descriptor-carrying message is refused **before it is encoded**
    (`Error::RemoteNoFds`, nothing queued, connection untouched), refused
    again at the socket as a backstop so no half-frame can ever reach the
    wire, and answered by the server with an `Error` that **keeps the
    client connected** — the one error in the protocol that is not fatal,
    because a client that ignored the capability bit is better served by
    an explanation than a dead socket. All *three* buffer ops are refused
    on those terms, not just the one carrying the descriptor: an app
    sends `CreateBuffer`, then the `SetImage` naming it, then
    `BufferDamage`, and refusing only the first would hand the client a
    clear sentence and then disconnect it two messages later with `no
    buffer with id 1`.

    In the toolkit the refusal is **not an error value at all**, and that
    is the difference between a feature and a footgun: `upload_image`
    answers `None`, which an `Image` widget already handles by drawing
    nothing, so a remote app **carries on drawing everything else**.
    Routing it through the pass's error channel instead would fail the
    paint, and a failed paint ends `Ui::flush` and so `App::run` — a
    remote app with one image anywhere in its tree would exit on its
    first paint. "There is no buffer here" is a permanent property of
    the connection, not a fault, and is reported as such; a real failure
    (a memfd that will not open) still fails the pass.

    `nitro-calc`, `nitro-term`, `nitro-files` and `nitro-settings` run
    remotely **unmodified**; the wallpaper cannot and says so, because it
    *is* an image. The shell socket is never TCP: `caps::SHELL` is
    granted for having opened a `0700` path, and a port proves nothing of
    the sort.

    **There is no authentication, and the documentation leads with that.**
    The supported configuration is loopback plus `ssh -L`, which puts
    authentication in sshd — which already has an answer to who you are
    and what the transport encryption is. A non-loopback bind warns
    loudly on every bind, and is for measurement. `remote.listen` is
    absent by default, and absent means no socket at all: no bind, no
    epoll registration, no accept path.

    Measured on the box (Pentium G3240) as A, the dev box as B, 1 Gb LAN:

    | | measured |
    |---|---|
    | i2p median, remote over LAN | **9.2–10.6 ms** (local, same evening: 8.7 ms) |
    | `nitro-term` `seq 1 1000000`: **bytes on the wire** | **43 185 B** for **6 888 896 B** of output — **0.63 %** |
    | `nitro-files` first paint of `/usr/share` | remote **9.2 ms** (local on the box 10–23 ms) |
    | server RSS with the listener idle | **below this box's resolution** |
    | `nitro-server` binary | +19.8 KB (+0.92 %) |
    | remote app `SIGKILL`ed | window gone within one loop turn |
    | link blackholed (a yanked cable) | **17 s**, via `SO_KEEPALIVE` 10/5/3 |

    The wire-byte ratio is the number that says something about the link,
    and it is the design working: a million lines of terminal output
    become about thirty frames of `SetText`, because the mutation stream
    describes what the screen should look like and a screen overwritten
    33 000 times only has to be described once per frame. That is why
    this forwards usefully where pixels would not. The `seq` *wall* times
    are deliberately **not** quoted as a comparison — the VT parsing
    happens on whichever machine the app runs on, so they measure the two
    CPUs.

    **The `TCP_NODELAY` A/B is a negative result, and it is reported as
    one.** `NITRO_TCP_NODELAY=0` exists so the claim behind the sockopt
    could be measured with one option flipped rather than asserted. Five
    interleaved pairs came back with **mixed signs** — the first pair had
    Nagle *faster*, which is exactly how a 10 % "win" gets published off
    one run — and the mechanism says why: `--follow` commits 4.7 times a
    second, so every write is alone on the wire with nothing to coalesce.
    Nagle holds a *second* small write; a protocol that sends one and
    waits never meets it. The option stays on, because the one burst that
    does exist shows it: connect-to-first-`Presented` is 8.7–21.6 ms with
    it against 18.8–31.3 ms without, same direction in four of five pairs.

    Two traps from the box run are recorded because they generalise.
    Killing an ssh forward is **not** a yanked cable — it is a clean FIN
    and takes 0.6 s, i.e. the close path the RST test already covers; the
    17 s figure needed the packets actually *dropped*. And `nc -q1 -U`
    silently returns nothing for a large fraction of control requests on
    that box, which reads exactly like "the key is absent": the first
    reading after enabling the listener looked like a broken reload while
    `ss -ltnp` showed the port LISTENing. Two defects in `nitro-demo`
    came out of running it remotely, and the second is the transferable
    one: **dropping a node from a scene is two edits, not one.** The demo
    built its scene without the image and then died on the first
    `Configure` with `UnknownNode`, because the re-layout still laid out a
    node the build had never created — in a path that only runs when the
    window is resized, so it survived 61 passing tests. Zero new external
    crates: `rustix`'s `net` already covers TCP sockets and socket
    options.

  - **M4-G done.** **Symbolic icons, server-side by name.** A client
    sends `icon("gear")` — a name, a palette role and a square size —
    and the server rasterises the artwork it owns at that output's
    device scale in the colour that role currently names. `SetIcon`
    (0x0208) behind `caps::ICONS`, `crates/nitro-icons` (a vendored
    Bootstrap Icons subset, MIT, pinned to a commit), an `IconEngine`
    beside the text engine, an `icon()` widget, and the bar and settings
    as first consumers. `docs/icons.md`.

    It is the text argument a second time, and the same three reasons
    make it: **remote** (a buffer is a file descriptor and cannot cross
    TCP, so pixels are the one thing a remote app cannot send; a name
    is nine bytes and a string), **theme** (the node stores a *role*,
    resolved at paint time, so `theme.scheme = dark` recolours every
    icon in the same frame as the text — and because the cache holds
    coverage rather than tinted pixels, at **zero** re-rasterisations),
    and **scale** (rasterised at `round(size × output scale)`, so a 2×
    screen gets a real 2× icon rather than a doubled 16 px tile —
    settled on the count of anti-aliased edge pixels, which a 2× blit
    would multiply by exactly four and a real raster does not).

    Two decisions are worth recording. `Icon` is its **own node kind**
    rather than an `Image`: an `Image` *is* a region of a client
    buffer, so overloading it would have meant a node whose meaning
    depended on which setter ran last, with no defined answer for
    `SetImage` then `SetIcon`. And an unknown name is the protocol's
    **second non-fatal error** (`BadIcon`), on exactly the terms the
    remote buffer op earned the first: the node is cleared, the commit
    applies, the client is told and keeps running. A desktop must not
    lose an application because one widget named an icon a newer set
    has, and a client cannot check in advance since the set is the
    server's.

    The dependency is **zero new crates**: `zeno` was already in the
    tree via swash. It did not come free, though, and the trap is the
    transferable part — zeno 0.3.3 mishandles SVG's *implicit repeated
    arc argument sets*, which 13 of our 47 icons are written with, and
    **`render_into` reports no error when it does**. It silently draws
    the prefix it parsed. A mishandled icon would have shipped as a
    stray fragment with every return value green. So the importer
    converts arcs to cubics at import time and `set.rs` holds arc-free
    data, which makes the construct unwritable rather than merely
    unused; the conversion is checked against the 33 icons zeno does
    parse natively (worst edge-pixel delta 38/255, **zero** solid-pixel
    flips at 16/32/64/128 px).

    The cache never evicts, and unlike the glyph atlas that is not a
    deferral: the set is *closed*, so all 47 icons at all four
    recommended sizes is about **190 KB**, against a 2 MiB ceiling that
    refuses rather than evicts. An LRU here would be machinery
    answering a question that cannot be asked; `icon_refusals` in
    `stats` is how the server would say that reasoning was wrong.

- **M5** — Wayland adapter; GPU backend.

## What we take from the old repo

Inspiration and dev goodies only: the KMS/libseat session handling (as a
reference for the ioctl choreography and drop-order lessons), the
screenshot-over-SSH workflow, the tracing/bench harness ideas, the
logind-via-side-daemon split, and the app list. No code is copied without
being re-read against the goals above.

## Open questions

- Own rasterizer vs `vello_cpu`: **decided at M1 — our own.** It is 2.9×
  faster on the damage-rect UI frame, which is the scene that describes the
  server's job, and carries two dependencies against forty-nine. We lose on
  raw per-pixel throughput (1.5× on alpha rrects, 1.8× on scaled blits),
  which is what their hand-written SIMD buys on a no-AVX2 CPU. Full numbers
  in `crates/nitro-raster/compare/RESULTS.md`.
- Text: **decided at M2-pre — `swash`.** Shaping, scaling and hinted glyph
  rendering in one pure-Rust crate for seven net dependencies, against
  roughly thirty for `parley`, whose shaper *is* swash. What parley adds
  over it — bidi, font fallback, rich text — is not M2 work, and layering
  it on later costs nothing that has been decided here. Font discovery is
  ours (`NITRO_FONT_DIRS`, no fontconfig). Reasoning in `DEPENDENCIES.md`,
  limitations in `crates/nitro-text/README.md`.
- Scene-graph vocabulary: how rich before it stops being "primitive"? Rule
  of thumb: if a client would need more than ~10 nodes for a button, add a
  node kind; if a node needs per-frame updates to animate, add a property.
- Client-side vs server-side glyph cache for the remote case: **decided at
  M2-pre — server-side.** Clients send strings and a style and never see a
  glyph, so nothing about text depends on the link being local; the atlas
  is shared across clients, because a glyph is a glyph.
- Security model between clients (who may introspect whom).
