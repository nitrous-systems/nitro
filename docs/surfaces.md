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
capability bits `DIRECT_SCANOUT` 0 and `DMABUF` 2) is the **single
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

## Fallback chain for one Surface

1. its own plane, or direct scanout;
2. underlay with a hole in the shadow buffer;
3. GPU helper composite;
4. CPU convert + scale into the shadow buffer.

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

## Protocol needs

| need | status |
|---|---|
| buffer release | exists: `BufferReleased` (`RELEASE` cap) |
| acquire fences | new |
| latch the newest *ready* frame at vblank — video updates skip the transaction round-trip | new |
| colour metadata: BT.601 / 709 / 2020, full / limited range → plane `COLOR_ENCODING` / `COLOR_RANGE` | new |
| format / modifier feedback, so producers allocate scanout-capable buffers | new (`DMABUF` cap) |

## GPU helper: `nitro-gpu` (feature-gated)

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

## Video decode belongs to clients

Decoding lives in clients (a future `nitro-media` library), not in the
server: software (dav1d, openh264) first, **VA-API on Intel**, V4L2 M2M
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
([`budget.md`](budget.md)). If it is unavailable the overview snaps
instead of animating.

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
| overview thumbnail atlas | always (allocated when the output appears, pre-faulted) | **measured** (#3902): exactly `w × h × 4` = 8 294 400 bytes at 1080p (`stats overview_atlas_bytes`) |
| `nitro-gpu` helper, process RSS | always, by default (on-demand config: only while compositing) | **measured** with the #3903 probe (no helper yet), vendor ICD only, start → first submit: +8.7 MB RSS / +6.6 MB PSS (HSW), +10.9 / +8.4 MB (KBL); the full NV12 chain adds +9.4 / +12.3 MB PSS. Confirm with the real helper in #3901 |
| `nitro-gpu` helper, driver memory (Vulkan instance/device, command pools, pipelines; system RAM on iGPUs, not counted in RSS) | same | unmeasured; measure in #3901 |
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
   #3900.
7. Overview thumbnail atlas — #3902.
8. `nitro-gpu` helper, always-on by default — #3901 (held; design first).

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
- Chromium Ozone GPU rendering — #3905.
- `nitro-video` player — #3906.
- `just footprint` budget gate — #3907.
