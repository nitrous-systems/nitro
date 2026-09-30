# Surfaces, planes and the GPU helper

A design record, not a status report. Nothing on this page is built yet.
It records the architecture agreed in a design discussion with the human
for foreign pixel content, so the work items that implement it (tag
`surface` on the task board) share one picture. Numbers are marked as
**measured** (with where), or as **estimates**; unmarked hardware figures
do not appear.

## Goal

Top-level goals, as set by the human, in order:

1. GPU-accelerated Chromium;
2. a GPU-accelerated video player;
3. small binaries and minimal memory.

What that needs is first-class external pixel content:

- video players — fullscreen with a controls overlay, and windowed;
- Chromium / Wayland clients rendering into GPU buffers;
- games and other 3D content;
- later, a video-call app (several live feeds at once).

Priorities, in order: **speed, low memory, low latency.** Animations
around this content are optional and never a reason to spend memory.

## The `Surface` node

`Surface` (`NodeKind` 5, reserved in [`wire.md`](wire.md), together with
capability bits `DIRECT_SCANOUT` 0 and `DMABUF` 2 — the latter
advertised since #3918) is the **single
abstraction for foreign pixels**. There is no second mechanism for video,
another for GPU windows and a third for fullscreen games.

- Clients always send N positioned Surfaces inside their ordinary scene.
- The **server** decides, per frame, how each one reaches the screen.
- Clients never learn plane counts, plane formats or which path was taken.
  The only hardware-shaped information they get is format/modifier
  feedback (below), so producers can allocate buffers that *can* be
  scanned out.

### A Surface is a hole in the shadow buffer

The shadow buffer becomes **ARGB**. Where a Surface is visible, the
rasterizer writes alpha 0 — a *clear* op, not a blend. Nitro content above
the Surface (popups, player controls, labels, the software cursor) is
drawn over the hole normally, so it keeps its own alpha.

Consequences:

- A new Surface buffer causes **no shadow damage and no raster**. The
  CPU rasterizer runs only when nitro content changes, exactly as today.
- The shadow buffer with holes can go straight onto the primary plane,
  with the Surfaces on planes *below* it (underlay), or be the top layer
  of a composite.
- Initially **every Surface is treated as opaque**. Translucent Surfaces
  (a blended hole) are a later problem and nothing here depends on them.

As built (#3898): the shadow holds **premultiplied** ARGB and every raster
op composites byte 3 as a fourth channel with source value 255, so alpha
stays 255 over opaque pixels and content over a hole comes out as a
correct premultiplied pixel. `Scene::set_surface_on_plane` flags a Surface,
which then paints `PaintKind::Hole` (`Canvas::clear_irect`). The server
switches the primary's framebuffer to AR24 (same dumb buffer, second
AddFB2) only while `Scene::has_holes` is true and the plane lists ARGB8888;
the test box's primary does not, so it stays XR24. `shot` fills holes with
`frame::HOLE_PLACEHOLDER` grey until Surfaces carry CPU-readable buffers
(#3897).

## Per-output modes: the `planes` module

Mode selection lives in a `planes` module **inside `nitro-server`** —
not a separate crate. It is pure logic: it takes the output's hardware
description and a `TEST_ONLY`-commit callback, so it is unit-testable
against the fake KMS backend. `nitro-kms` reports the hardware (planes,
formats, modifiers, zpos, scalers) and performs the commits; it does not
decide.

| mode | when | what goes where |
|---|---|---|
| 0 | no visible Surface | today's path, unchanged |
| 1 | every Surface fits a plane | each Surface on a plane **below** the primary (underlay); the primary is the dumb buffer with holes |
| 2 | some Surfaces don't fit | planes take what fits; the rest are composited — by the GPU helper, or on the CPU when the buffers are linear and CPU-readable |
| 3 | one opaque Surface covers the output | direct scanout of the client buffer on the primary; nitro content above it on an overlay or cursor plane |

Underlay on hardware with fixed zpos (Intel) is achieved by assignment,
not by reordering: the video goes on the lowest plane and the UI on a
plane above it. On the Haswell test box that works only for RGB video: its
primary lists no YUV format, so YUV Surfaces there can only be overlays
*above* the UI (measured, #3895; see "Plane counts").

The module is **driven by each plane's `IN_FORMATS`** (formats ×
modifiers) and never by assumptions about the hardware. The two test boxes
differ exactly where it matters (#3903): Kaby Lake scans out VA's Y-tiled
NV12 on its primary and sprite planes, while Haswell's sprite takes only
packed 4:2:2 (YUYV family) or XRGB, LINEAR/X_TILED, and refuses NV12 at
AddFB2. On Haswell a video Surface therefore reaches a plane only as
YUYV/UYVY or as helper-converted XRGB.

- Candidate configurations are validated with **`TEST_ONLY` atomic
  commits** — advertised capability is not the truth, the kernel's answer
  is.
- Results are **cached per layout signature** (Surface count, sizes,
  formats, positions relevant to plane constraints), so a steady-state
  frame costs no test commit.
- **Hysteresis** keeps a transient overlap (a tooltip, a menu) from
  flipping modes back and forth.
- A mode switch loses the age-2 buffer history, so it costs **one full
  repaint**. That cost is why hysteresis matters.

### As built (#3899)

`planes::Planner::decide` runs in `Server::paint` before anything is
rasterized. Its inputs:

- candidates from the paint list: Surfaces whose buffer has a KMS
  framebuffer (server-allocated, or a KMS-imported dma-buf), of an opaque
  format, axis-aligned, at opacity 1;
- for each candidate, whether any later item or the software cursor
  touches its visible rect (*obscured*).

Strategies, in order, each pre-filtered by `IN_FORMATS`, the scale rule
(never below 0.94×) and the plane's `COLOR_ENCODING`/`COLOR_RANGE`, then
`TEST_ONLY`'d:

1. **Direct (mode 3):** an unobscured Surface covering the output goes on
   the primary, else on an overlay with the primary off (HSW).
2. Greedy from the topmost Surface:
   - **overlay-above** (unobscured only; no alpha needed);
   - **underlay by mutable zpos** (AMD-like);
   - **underlay by primary swap**: the Surface on the primary, the UI as
     AR24 on an overlay (KBL).
3. **Composite (mode 0).**

Placed Surfaces are flagged `on_plane` and paint as holes. While a
Surface is flagged, the scene ignores its buffer changes: they damage
nothing. The output scans out AR24 only for underlays.

**Cache and hysteresis.** Decisions are cached per shape signature
(buffer ids excluded; ≤ 8 per output), so steady state asks nothing of
the kernel. Fewer planes applies at once. More planes needs the same
shape for 15 decisions **and** 250 ms. A shape change invalidates the
output (a full repaint); a layout that fails at commit time falls back
to composite and is pinned there.

**Release rule.** A `BufferReleased` for a buffer whose framebuffer a
committed layout reads is held (`plane_releases_held`) until
`take_released_buffers` reports it, i.e. until the replacing flip lands.
A modeset (resume, rescan, `set_modes`, another output lighting) resets
every decision.

**IN_FENCE_FD** (#3938). A frame whose acquire fence is still pending
**latches early** when its node is placed on a plane that has
`IN_FENCE_FD` and lists the buffer's format and modifier
(`Server::early_latch_nodes`; `Latch::latch_ready` then takes the newest
queued frame, fenced or not). The fence leaves the epoll set
(`FenceSet::take`) and is kept per output by node; just before either
commit (`paint` → `commit`, `flip_planes` → `commit_planes`)
`stage_plane_fences` hands it to `Backend::set_plane_fence` for the plane
the current decision gives that node. The kernel waits on it, the server
never does. An early-latched buffer gets no `DMA_BUF_IOCTL_SYNC` read
bracket (it would block on the fence), and no matching end. Stats:
`plane_fence_latches` (frames latched early), `plane_fences` (fences
handed over).

Residual: if the planner un-places the node between the latch and the
commit, the fence is dropped and the frame is composited. A linear,
CPU-readable buffer could then be read before its writer finishes, which
tears that one frame. A tiled one shows the placeholder anyway.

On testhost2, VA-API frames do not take this path: the export's
`vaSyncSurface` completes each frame before `PresentSurface`, so the
implicit fence has already signalled at receipt (`fence_waits` 0,
`plane_fences` 0). The path is for producers that present unfinished
work, such as a GPU client with `PresentSurfaceFenced`, and is covered on
the fake backend (`tests/dmabuf.rs`).

**Client dma-bufs on planes** (#3938). Every dma-buf is imported as a KMS
framebuffer when it is registered (AddFB2 with the modifier; VA's Y-tiled
NV12 passes on KBL). The framebuffer is cached in `HeldBuffer.scanout`
for the buffer's life and is a plane candidate like a server-allocated
buffer. The planner pre-filters on each plane's `IN_FORMATS`. A dma-buf
the planner cannot place and the CPU cannot read keeps the grey
placeholder until the GPU helper (#3922) composites it.

**SurfaceHint** reports YUYV where a plane lists YUYV but none lists
NV12 (HSW), and NV12 otherwise. `AllocSurfaceBuffers` with format 0
takes `planes::alloc_format`: NV12, else YUYV, else XR24.

Stats: `planes_mode` (max over outputs), `planes_in_use`,
`planes_candidates`, `planes_obscured`, `planes_tests`,
`planes_cache_hits`, `planes_fallbacks`, `planes_switches`,
`plane_flips`, `plane_releases_held`, `plane_fences`,
`plane_fence_latches`, `plane_reject_scale`, `planes_scale_limited` (#3956). Numbers:
[`budget.md`](budget.md) "Planes (#3899)".

## Fallback chain for one Surface

1. its own plane, or direct scanout;
2. underlay with a hole in the shadow buffer;
3. GPU helper composite;
4. CPU convert + scale into the shadow buffer.

A Surface that drops out of step 1 **because of the plane scaling
limit** is not a silent fallback (#3956). The server logs it once per
Surface per state change ("off planes: 1920x1080 -> 1601x900 is 83%
(planes take >= 94%); shown by GPU helper | CPU scaled blend |
placeholder", and "scale limit cleared: back on a plane | no longer
scale-limited | gone"). It counts `plane_reject_scale` (entries) and
`planes_scale_limited` (a gauge), and tells the producer through
`SurfacePlaneHint`'s `SCALE_LIMITED` flag. See § As built: the plane
size hint.

Step 4 works everywhere — simpledrm, VMs, machines with no Mesa — and is
**built first**. Every other step is an optimisation that falls back to it.

## The no-GPU video path (first milestone)

Two halves, neither needs a GPU driver:

**(a) CPU path.** memfd/shm Surface buffers in NV12 or XRGB, converted and
scaled into the shadow buffer by a dedicated **fused NV12→XRGB + scale
fast path** in `nitro-raster`. The generic scaled blit is too slow for
this: **measured** at ~12 ns/px (XRGB, release, 1080p destination) against
~0.2 ns/px for the 1:1 blit ([`wm.md`](wm.md), "The number that constrains the whole
design"). At that rate a 1080p frame would be ~25 ms (**estimate**, simple
multiplication), over a 60 Hz budget before anything else is drawn.

**(b) Plane path.** The server allocates dumb buffers, exports them as
PRIME dma-bufs and hands them to the client as output buffers. The client
memcpys decoded frames in; the server puts them on overlay planes. The
display engine does YUV→RGB and scaling — zero CPU compositing.

The display controller is **not** the GPU. Planes work with no GPU driver
loaded at all; that is what makes (b) available on the same machines as
(a).

### As built: server-allocated scanout buffers (#3914)

The allocation half of (b) exists. `AllocSurfaceBuffers` (wire op
`0x0312`, `docs/wire.md` § Server-allocated scanout buffers) asks for 1–4
buffers for a Surface node; the server allocates each with
`Backend::alloc_buffer` (a linear dumb buffer on DRM), reads its layout
with `buffer_info`, exports it with `export_buffer` (PRIME, `O_RDWR`) and
sends the fd in a `SurfaceBufferAllocated`. The client maps it
(`nitro_shm::DmaBufMapping`), writes each frame inside a
`DMA_BUF_IOCTL_SYNC` bracket and presents it with the ordinary
`PresentSurface`.

- **The server's CPU path is unchanged.** It maps its own duplicate of
  the export read-only (`Mapping::map_dmabuf`) and registers the buffer in
  the scene as an ordinary surface buffer (`ScanoutPixels`); the
  `HeldBuffer` records the KMS `BufferId` for the planes module (#3899)
  to find. Mapping the export was chosen over the kms dumb mapping: it
  needs no new kms API and is the same code on the fake backend (whose
  export is a sealed memfd).
- **Defaults (v1).** Format 0 → `NV12` if a plane on the node's output
  lists linear NV12, else `YUYV`, else `XR24`
  (`surface::default_scanout_format`, which #3899 replaces). Size 0 → the
  `SurfaceHint` size, rounded up to even.
- **Accounting.** The buffers count against the per-client buffer and
  byte caps like memfds; `stats` reports `scanout_buffers` and
  `scanout_buffer_bytes`. `DestroyBuffer` and disconnect free them with
  `free_buffer` after the scene dropped its mapping; nitro-kms defers the
  free while the buffer is on screen.
- **No server-side sync bracket.** The server reads without
  `DMA_BUF_IOCTL_SYNC`. For a linear i915 dumb buffer on x86 the CPU
  mapping is coherent (write-combined), so the bracket is a hint there and
  the client's half is what matters. Residual: on an exporter whose CPU
  mapping is *not* coherent, the CPU path could read stale lines of a
  frame — a torn picture, the same class as a client writing while
  presenting, and never more than its own window. The plane path does not
  read the pixels on the CPU at all.
- **Reading cost: measured, none.** Dumb-buffer mappings are
  write-combined, so CPU reads from them were expected to be slow. On box1
  (HSW) the CPU path's `paint_us` is the same within noise for the same
  YUYV frame from a memfd and from a dumb buffer: 716 vs 719 µs at 720p,
  1 491 vs 1 475 µs at 1080p fullscreen (`docs/budget.md` §
  "Server-allocated scanout buffers (#3914)"). The converter is
  compute-bound. So #3899 can composite these buffers without a
  penalty whenever a plane is refused. On HSW an NV12 allocation is
  refused by the kernel, and the server's default format there is YUYV.
- **Client.** `nitro-demo --video --scanout [--format nv12|yuyv|xr24]`
  allocates its ring this way (falling back to memfds on
  `AllocSurfaceBuffersFailed`) and draws NV12, YUYV (HSW's only
  overlay YUV format) or XR24 at the kernel's padded pitch.

### As built: client dma-bufs (#3918)

Clients hand over dma-bufs they allocated (`CreateDmabufBuffer`, one fd
per plane, format + modifier; `docs/wire.md` § Client dma-bufs). Server
side: `crates/nitro-server/src/dmabuf.rs`.

- **Import.** Validated at receipt against the default feedback's
  `IMPORT` set and the buffer caps. Linear buffers in a CPU format, all
  planes in one inode, are mapped read-only (`Mapping::map_dmabuf`) and
  painted by the ordinary CPU path. Anything else gets a store that is
  not CPU-readable (`PixelStore::cpu_readable`), and `frame::paint_surface`
  fills its rect with `HOLE_PLACEHOLDER` grey
  (`dmabuf_placeholder_paints`). When the output backend reports planes
  the buffer is also imported at commit with `Backend::import_buffer`
  (PRIME import + AddFB2 with modifiers). The result goes into
  `HeldBuffer.scanout` (`dmabuf_kms_imported` / `dmabuf_kms_refused`),
  and `DestroyBuffer` or disconnect frees it with `free_buffer`.
- **Fences.** A frame is latched only once its acquire fence has
  signalled. The latch queue holds up to 4 frames per node; the newest
  ready one wins and older ones are superseded. The fence is `poll`ed
  once at receipt; if it is still pending it goes into the server's epoll
  (`FenceSet`, tokens from `1 << 36`) and never blocks the loop.
  - Explicit fences come from `PresentSurfaceFenced`.
  - Implicit fences are a `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` snapshot taken
    at `PresentSurface` (a third ioctl in nitro-shm's `unsafe`
    exception). On kernels older than 6.0 the server polls the dma-buf
    itself instead.
  - On the CPU path the server brackets its reads with
    `DMA_BUF_IOCTL_SYNC` `READ`: it starts the bracket at latch and ends
    it when the buffer is replaced. This is correct on non-LLC hardware
    too.
- **Feedback.** `DmabufFeedback` is a per-output (or default, union) list
  of format + modifier pairs, flagged `SCANOUT` (from `IN_FORMATS`, non-cursor
  planes), `CPU` (linear CPU formats), `IMPORT`. It also carries
  `main_device` (`Backend::device_id`) and the output size as
  `max_width`/`max_height`, meaning "render at display size or smaller".
- **On planes (#3899, #3938).**
  - `HeldBuffer.scanout` holds the imported KMS framebuffer, and it is a
    plane candidate.
  - A placed frame latches early and hands its fence to
    `Backend::set_plane_fence` as `IN_FENCE_FD` (§Planes above).
  - `dmabuf::direct_scanout(planes)` is true when some non-cursor plane
    lists a real format/modifier pair: exactly the pairs the feedback
    flags `SCANOUT`. It sets the `DIRECT_SCANOUT` cap.
  - Releases happen after the replacing flip, through
    `take_released_buffers`.
- **Not in v1.** `SetSurface` with a dma-buf is refused, because readiness
  is defined only at the latch. The server does not check that a fence
  fd is a `sync_file`: a fence that never signals stalls only the
  sender's surface.

### As built: the plane size hint (#3956)

KBL planes downscale only to ~0.94×, and HSW planes do not scale. So a
Surface whose buffer is much larger than its on-screen rect cannot go on
a plane. The default nitro-video window showing a 1080p clip at scale
1.25 is 0.83×. Before #3956 the server fell back silently: mode 2 if the
helper ran, else the placeholder (tiled) or a CPU scaled blend (linear).

- **Wire.** `SurfaceHint` already carried the node's device size. What
  was missing was the plane's floor. `SurfacePlaneHint` (0x830c, behind
  the new cap `PLANE_HINT`, bit 18) carries the size, `min_scale_pct`
  (`planes::min_scale_pct`: 94 / 100 / 0) and the informational
  `SCALE_LIMITED` flag. It is sent on first size and on any change.
  Producers decide from size and ratio only, so there is no feedback
  loop.
- **Server.** `planes::check` gives the reason a plane refuses a
  candidate (`Reject::{Format, Scale, Color}`). `planes::scale_limited`
  is true when no non-cursor plane takes the candidate and some plane
  refuses it for scale alone. After each decision `plan_planes` records
  the set per output (`Output.scale_limited`), logs entries and exits
  once, counts them, and pushes the plane hints only when the set
  changed (it never re-sends `DmabufFeedback`).
- **nitro-video.** With VA-API it scales on the video engine (VPP) to
  the hinted size, and only when the plane could not take the stream as
  it is. It uses hysteresis and a 250 ms debounce. Measured on testhost2
  with the helper off: `planes_mode 1`, a real picture, server 2.3 % CPU
  (placeholder: 25.5 %; software: 49 %). Software decode is not scaled
  (measured: no net win), see `crates/nitro-video/README.md`.
- **The /2 idea, not built.** The idea is to have the GPU helper produce
  a half-size intermediate for a plane when a producer ignores the hint.
  It would still wake the 3D engine every frame, which is exactly mode
  2's cost (3.0 % server CPU on testhost2 for this clip, #3953). The
  only saving would be a smaller render target than mode 2's
  damage-clipped composite. It would also lose detail: the 1080p frame
  would show as 960 px upscaled to 1600. With producers rendering to the
  hint, the plane path is 2.3 % with no helper at all, and mode 2 stays
  the fallback for producers that ignore it. /2 does not beat plain
  mode 2 by enough to justify a second composite path.
- **box1 (HSW).** No plane scaler and no plane lists Y-tiled NV12, so
  VA-API video is software/shm there and composited whatever its size.
  The hint buys nothing for nitro-video on box1. A producer of
  plane-compatible buffers (linear YUYV) gets `min_scale_pct 100` and
  renders exactly at the node's size.

## Protocol needs

| need | status |
|---|---|
| buffer release | exists: `BufferReleased` (`RELEASE` cap) |
| acquire fences | **done** (#3918): `PresentSurfaceFenced` (explicit), `EXPORT_SYNC_FILE` snapshot on `PresentSurface` (implicit) |
| latch the newest *ready* frame at vblank — video updates skip the transaction round-trip | **done** (#3897; fence-aware queue #3918) |
| colour metadata: BT.601 / 709 / 2020, full / limited range → plane `COLOR_ENCODING` / `COLOR_RANGE` | new |
| format / modifier feedback, so producers allocate scanout-capable buffers | **done** (#3918): `DmabufFeedback`, `DMABUF` cap |

## GPU helper: `nitro-gpu` (feature-gated)

**As built (#3920), standalone:** `crates/nitro-gpu` (protocol, event
loop, validation, lifetimes, buffer-age ring damage, sandbox, stats; no
`unsafe`, no GPU API) and `crates/nitro-gpu-vulkan` (the helper binary,
the only crate with `ash`). The backend trait has no Vulkan types, so a
GLES backend can be added without touching the server. Tested headless
by readback on both boxes (9/9 on hasvk and anv; `just box-gpu-test`).
The shadow reaches the GPU by **memfd → udmabuf** (zero copy) when
`/dev/udmabuf` opens, else by damage-rect copies through a staging
buffer. seccomp (and `no_new_privs`) is deferred. Wired into nitro-server as mode 2 by #3922 (below).
Measured footprint: [`budget.md`](budget.md) § GPU helper.

A **separate process**, started with the session and **running by default**.
The first Surface that needs compositing (say, opening a video window)
must not pay process start plus Vulkan device creation, which would show
up as a visibly late first frame. Measured in #3903: with the vendor ICD
only, instance + device + first submit take ~10–16 ms warm; cold with the
default ICD set, instance creation alone takes 169 ms on the test box.
Cold start with a restricted ICD set, pipeline creation and shader-cache
misses together are **estimated** at 50–200 ms, which is not measured.
**On-demand spawn with idle exit** is an optional config
for memory-tight devices (phones, kiosks): they accept that first-frame
delay to save the helper's steady memory (see [Budget](#budget)).

Why a process rather than a module. Running it always-on changes none of
these:

- a userspace driver crash or a GPU reset kills the helper, not the
  display; the server restarts it and uses the CPU path meanwhile;
- least privilege: it holds a render node only and can be sandboxed — no
  DRM master, no input fds;
- Vulkan's `unsafe` stays out of the server;
- all driver memory is reclaimed when it exits or is restarted.

Protocol, server → helper: the output-buffer index, the damage, and a
layer list — the Surfaces plus the shadow buffer (exported via memfd →
udmabuf) as the top layer. The helper does **one damage-clipped render
pass of textured quads** (YUV→RGB, scaling) and replies immediately with a
`sync_file`. The server passes it as `IN_FENCE_FD` and **never waits on
the GPU**. Planes still take whatever fits; the helper only handles the
remainder.

- API: **Vulkan** via `ash` with `libvulkan` dlopen'd. There is no
  GLES/EGL backend, and `wgpu` is rejected on dependency count. Measured
  in #3903 ([`research/gpu-testbox.md`](research/gpu-testbox.md)): the
  helper minimum (dma-buf import with modifiers, SYNC_FD semaphore
  export/import, NV12 sampling via `VkSamplerYcbcrConversion`, a render
  pass into an exported scanout-capable image) is **met on both test
  boxes**, Haswell `hasvk` and Kaby Lake `anv`. hasvk is non-conformant,
  so the helper keeps to that verified narrow slice. KBL/anv is the
  development target. Cost to first submit is **8–11 MB RSS / 7–8 MB
  PSS** (9–12 MB PSS for the full NV12 chain), against 41–67 MB RSS /
  38–65 MB PSS for EGL/GLES (crocus/iris). Instance + device take about
  8 ms warm.
- **ICD restriction is mandatory.** The helper picks the vendor ICD
  from the render node's kernel driver (`VK_LOADER_DRIVERS_SELECT` /
  `VK_DRIVER_FILES`, or dlopen the ICD). With default discovery,
  llvmpipe maps libLLVM and the instance alone costs 59 MB PSS on the
  test box (measured).
- The render target's modifier list is the target plane's `IN_FORMATS`
  modifiers (X_TILED/LINEAR on Haswell), so the output stays
  scanout-capable.
- The CPU rasterizer remains **the only renderer of nitro content**. The
  GPU only combines finished buffers.
- It is needed mainly for overlapping GPU windows from different clients
  (a desktop of Wayland/Chromium windows) once planes run out. A single
  video or a fullscreen game should never need it.

### As built: mode 2 in the server (#3922)

`crates/nitro-server/src/gpu.rs` supervises the helper and
`planes.rs` decides mode 2 (`Mode::Gpu`, `planes_mode 2`):

- **Which Surfaces.** A visible, opaque, axis-aligned Surface whose buffer
  is a dma-buf the helper samples: a client `CreateDmabufBuffer`, or a
  server-allocated scanout buffer (`export_buffer`). The planner takes
  planes first (overlay-above only in mode 2); what is left goes to the
  helper. shm/memfd Surfaces stay on the CPU path (importing them through
  udmabuf is a follow-up).
- **Layout.** The primary shows a slot of the helper's ring (XR24, one
  `AddFB2` per slot at allocation) with the frame's `sync_file` as
  `IN_FENCE_FD`; overlays above it may still carry unobscured Surfaces.
  The helper draws the composited Surfaces bottom-first as opaque quads
  and the shadow on top (`PremulOver`); those Surfaces paint as holes in
  the shadow. The dumb output buffer is not on screen.
- **Never waits.** `paint` rasterizes into the shadow, sends
  `UploadDamage` + `Composite{slot, damage, layers}` and returns. The
  commit happens when `Composited` arrives (or at the next `Flipped` if a
  flip is pending). The damage is the raster region plus every helper
  layer that changed or moved; the helper adds buffer age. With no free
  slot the damage is kept and retried; nothing blocks.
- **Borrowed buffers.** A `BufferReleased` for a buffer an unsignalled
  helper frame samples is held until that frame's fence signals (a dup is
  in epoll). Textures are imported lazily on first use and `Release`d when
  the buffer goes. An import the helper refused marks the buffer
  CPU/placeholder.
- **Hysteresis.** Mode 2 goes through the same rules. Entering it with more
  planes in use waits 15 decisions and 250 ms; leaving it for a plane
  layout is an upgrade too, and waits the same way. A switch in or out
  invalidates the output (full repaint).
- **Supervision.** Spawned with the socket on fd 0. `Hello` must be
  answered in 2 s, and a `Composite` within 250 ms, or the helper is
  killed. Restarts back off 100 ms doubling to 30 s (reset after 60 s
  healthy); after 5 crashes in 5 min the server gives up until a VT
  resume or SIGHUP. Death in mode 2 falls back at once, skipping the
  hysteresis: planes for what fits, CPU for linear buffers, the
  placeholder otherwise, with a full repaint. A VT switch stops the
  helper; the resume restarts it. `gpu.helper = on-demand` spawns on the
  first mode-2 decision and releases everything when leaving mode 2, so
  the helper can idle-exit.
- **Limits.** The protocol has one output ring, so only one output is in
  mode 2 at a time (the others use planes/CPU). The ring lives as long as
  the helper. Entering mode 2 moves that output's shadow into a sealed
  memfd: the same 8 MB, counted as RssShmem instead of RssAnon.

## Video decode belongs to clients

Decoding lives in clients (a future `nitro-media` library), not in the
server. `nitro-video` v1 (#3906) decodes in software through the system
FFmpeg, and on VA-API through FFmpeg's hwaccel since #3923, the first
client dma-buf producer: its VA surfaces are registered with
`CreateDmabufBuffer` when the feedback says the server shows them, else it
decodes in software. In general: software (dav1d, openh264) first, **VA-API on Intel**, V4L2 M2M
on phones, and Vulkan Video on newer desktops.

VA-API is the Intel decode API because neither test box has Vulkan Video:
anv has none on Gen9 and hasvk has none at all (measured, #3903). The
test box (i965) decodes H.264, MPEG-2 and VC-1; KBL (iHD) adds VP8, VP9
and HEVC. AV1 is software on both. A 1080p30 H.264 decode costs ≈5% of one
core with VA-API, against ≈60% in software, on the test box. VA exports
NV12 as a Y-tiled DRM_PRIME_2 dma-buf, which the helper imports directly.
KBL's planes scan it out; Haswell's cannot (see "Per-output modes").

Decode usually runs on fixed-function blocks, not the 3D engine. GPU
compositing wakes the 3D engine and costs power — another reason planes
come before the helper.

Group-video layouts (video calls) pack several feeds into one shared
"video plane" buffer, updated by damage-driven copies (2D blitter or
memcpy), so N feeds cost one plane. Later work.

## Overview memory rule

**Pressing Super must allocate nothing.** Thumbnails of all windows
together fit on one output, so the thumbnail cache is **one fixed,
output-sized atlas** — 8 294 400 bytes at 1080p (1920 × 1080 × 4; built in #3902),
allocated once, when the output appears, and counted in the budget
([`budget.md`](budget.md)). Since #3916 it exists only with
`overview.animate = true`; by default, or if it is unavailable, the
overview snaps instead of animating and nothing is allocated.

Anything whose memory scales with window count must be optional. On
discrete GPUs, retained per-window textures may live in VRAM while the
helper runs; on iGPUs and phones "GPU memory" is system RAM (just not
RSS) and is budgeted like dumb buffers.

## Budget

Steady costs this design adds, to be carried into [`budget.md`](budget.md)
once measured. Every figure below is an **estimate** unless marked
**measured**.

| item | when paid | cost |
|---|---|---|
| overview thumbnail atlas | only with `overview.animate = true` (default off, #3916); then allocated when the output appears or the setting turns on, pre-faulted | **measured** (#3902): exactly `w × h × 4` = 8 294 400 bytes at 1080p (`stats overview_atlas_bytes`) |
| `nitro-gpu` helper, process RSS | always, by default (on-demand config: only while compositing) | **measured** with the #3903 probe (no helper yet), vendor ICD only, start → first submit: +8.7 MB RSS / +6.6 MB PSS (HSW), +10.9 / +8.4 MB (KBL); the full NV12 chain adds +9.4 / +12.3 MB PSS. **Real helper (#3920)**, idle after init: 10.8 MB RSS / 7.1 MB PSS (HSW), 12.6 / 7.2 (KBL); + 1080p ring + shadow + first frame: 11.4 / 7.5, 13.2 / 7.6 |
| `nitro-gpu` helper, driver memory (Vulkan instance/device, command pools, pipelines; system RAM on iGPUs, not counted in RSS) | same | **measured (#3920)**, DRM fdinfo: 1.0 MB (HSW) / 8.1 MB (KBL) idle; + 3-slot 1080p ring 24.9 / 31.8 MB; + shadow + first frame 32.8 / 41.8 MB total |
| server-allocated NV12 dumb buffers | per plane-placed Surface | 1.5 bytes/px × buffer count (~3 MB per 1080p buffer) |

The helper's two rows are what the on-demand config buys back. They must
be measured on the test box (RSS from `/proc`, driver memory from the
DRM fdinfo `drm-total-*` / `drm-resident-*` keys) before the always-on
default is considered settled.


## Plane counts

**Measured on the test boxes** (#3895, #3911; the full inventories and
`TEST_ONLY` results are in [`crates/nitro-kms/README.md`](../crates/nitro-kms/README.md#planes-discovery-test_only-and-the-hsw-gt1-inventory-measured)):

| hardware | planes per CRTC (measured) |
|---|---|
| Intel Haswell GT1 (test box) | primary + **1 overlay** + cursor, all fixed zpos (0/1/2). Overlay: packed YUV 4:2:2 (YUYV…) and XRGB, **no NV12** (refused at AddFB2), **no ARGB**, no scaling accepted with linear buffers. The primary must cover the CRTC and cannot scale. |
| Intel Kaby Lake R, UHD 620, Gen9 (testbox2, #3911; #3903) | primary + **1 overlay** + cursor per pipe, all fixed zpos (0/1/2); 2 scalers on pipes A/B, 1 on C. Primary *and* overlay take NV12 (linear, X, Y, Yf), XYUV, YUYV family, XR24/AR24 (+CCS), have `pixel blend mode` and `alpha`; the primary can be a window and can scale. **Linear NV12 accepted** 1:1, windowed, at odd positions/sizes. **Upscaling accepted** (NV12/YUYV/RGB, and primary + overlay scaled together); **downscaling only to ~0.94×** — 0.75× and 0.5× rejected at this panel's 337.5 MHz cdclk. AR24 on the overlay accepted. Pipe C has no NV12 (#3903). VA's Y-tiled NV12 passes AddFB2 (#3903). |

For comparison, **from memory, unverified**:

| hardware | usable planes (rough) |
|---|---|
| Intel Gen11+ | 5–7 |
| AMD DCN | primary + 1 overlay |
| Rockchip / Qualcomm | 3–6 |
| Allwinner | 1 video plane |

Advertised ≠ usable. `TEST_ONLY` is the truth.

## Work plan

In order; tasks carry the `surface` tag on the task board.

1. `nitro-kms` plane discovery + `TEST_ONLY` + a probe tool — #3895.
2. NV12 fast path in `nitro-raster` — #3896.
3. Surface v1: shm `Surface` node, CPU path, vblank latch, colour
   metadata, test client — #3897. **Built**: `SURFACE` cap (bit 16),
   `CreateSurfaceBuffer` (NV12, YUYV, UYVY, XR24, AR24), `SetSurface`,
   `PresentSurface` (latch semantics in `docs/wire.md` § Surfaces) and
   `SurfaceHint` (preferred format + device size; NV12 on the CPU path,
   the planes module will answer e.g. YUYV on Haswell). `nitro-demo
   --video`; box numbers in `docs/budget.md` § Surface CPU path.
4. ARGB shadow buffer + hole primitive — #3898.
5. `planes` module: underlay / overlay / direct scanout, driven by
   `IN_FORMATS`, with server-allocated dumb buffers in a format the
   plane accepts (NV12 on Gen9+, YUYV on Haswell) — #3899.
6. Client dma-buf import + fences + format feedback (`DMABUF` cap) —
   #3918 (refiled from #3900). **Built**; see "As built: client
   dma-bufs" above.
7. Overview thumbnail atlas — #3902.
8. `nitro-gpu` helper, always-on by default — #3901 → **#3920 built standalone** (crates + headless pixel tests); **#3922 integrated as mode 2** (above; fake-helper tests, hardware verification pending).

Alongside, and feeding into the items above:

- Test-box GPU/media capability spike — #3903, done. Outcome: the Vulkan
  helper minimum is met on both boxes, ICD restriction is mandatory, and
  VA-API is the Intel decode API
  ([`research/gpu-testbox.md`](research/gpu-testbox.md)).
- Cross-client Surface sharing for Chromium's out-of-process GPU — #3904,
  done. Token attach (option B): the owner's `ExportSurface` mints a bearer
  token, and a same-uid local connection's `ImportSurface` lets it
  `PresentSurface` into the node through this latch. The newest frame wins
  whoever sent it; releases and `Presented` go to the presenter. Contract:
  `wire.md` § Surface sharing; server side: `crates/nitro-server/src/share.rs`.
  Why B over C: [`chromium.md`](chromium.md#out-of-process-gpu-b-vs-c-3904).
- Chromium Ozone GPU rendering — #3921, **built**: Chromium's GPU process is a dma-buf producer. ANGLE-Vulkan renders into GBM buffers (nitro's `IMPORT` modifiers; linear for translucent windows) that it presents with `CreateDmabufBuffer` + `PresentSurfaceFenced` into the imported window Surface; `AR24` dma-bufs are premultiplied. See [`chromium.md`](chromium.md#gpu-rendering-via-dma-buf-3921). Video overlays: #3944.
- `nitro-video` player — #3906, **built**: system FFmpeg (libavformat/
  libavcodec, dynamic) on a decode thread → 4-buffer NV12 memfd ring →
  `PresentSurface`, paced by frame callbacks; nitro-ui controls overlay
  (`SurfaceView`), fullscreen. See `crates/nitro-video/README.md`.
- `just footprint` budget gate — #3907.
