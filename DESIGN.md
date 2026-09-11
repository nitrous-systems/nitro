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
  opacity}`, `Rect{rrect, fill, border}`, `Text{run_id, pos}`, `Image{buf,
  src_rect}`, `Surface{external dma-buf}`. Each node caches its
  world-space bounds; a mutation marks the old and new bounds damaged.
  Windows are just top-level groups with a client owner and a z-order.
- **raster** — CPU 2D rasterizer producing exactly the damaged region into
  the back buffer. Anti-aliased rects/rounded-rects, solid/linear fills,
  glyph blitting from a server-side atlas, image scaling. Candidate: own
  minimal rasterizer; measure against `vello_cpu` before deciding. Text
  shaping/layout (`parley` + `skrifa`) lives here so clients send text
  *runs*, not glyph pixels — this is what keeps the remote link thin.
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
`nitro-term`, `nitro-calc`, `nitro-files`. Shell-only privileges (place a
window on the top layer, receive global hotkeys, read the window list) are
granted per socket by the server on connect (socket path / peer creds), not
by protocol extensions.

Session policy that talks to `logind` over D-Bus (suspend, power off,
lock) lives in one side daemon, `nitro-sessiond`, and is the only place a
D-Bus client is allowed.

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
- **M1** — Scene graph, damage, CPU raster of rects and text; one client
  drawing a moving box via mutations; measured input-to-photon latency.
- **M2** — `nitro-ui` with arena, passes, `WidgetMut`, six widgets, layout,
  introspection socket, `hey`-style CLI. `nitro-calc` as the first app.
- **M3** — Shell: bar, launcher, window management (focus, move, resize,
  z-order), keyboard layouts, multi-output.
- **M4** — Terminal, settings (display/audio), file manager; remote view;
  phone build.
- **M5** — Wayland adapter; GPU backend.

## What we take from the old repo

Inspiration and dev goodies only: the KMS/libseat session handling (as a
reference for the ioctl choreography and drop-order lessons), the
screenshot-over-SSH workflow, the tracing/bench harness ideas, the
logind-via-side-daemon split, and the app list. No code is copied without
being re-read against the goals above.

## Open questions

- Own rasterizer vs `vello_cpu`: decide by benchmark at M1.
- Text: `parley`+`skrifa` is the plan; how much of parley we actually need
  (bidi, rich text) determines whether a smaller shaper suffices.
- Scene-graph vocabulary: how rich before it stops being "primitive"? Rule
  of thumb: if a client would need more than ~10 nodes for a button, add a
  node kind; if a node needs per-frame updates to animate, add a property.
- Client-side vs server-side glyph cache for the remote case.
- Security model between clients (who may introspect whom).
