# nitro-kms

The display backend of the nitro server: one trait, two implementations.

- `DrmBackend` — atomic KMS over an already-open DRM fd. Two CPU-writable
  dumb buffers per output, `NONBLOCK` page flips with vblank events,
  `FB_DAMAGE_CLIPS`, hotplug via a raw kernel uevent netlink socket.
  Dependencies: `drm` (safe ioctl wrappers) and `rustix`. No `unsafe`, no
  libdrm, no libudev, no libseat (the fd comes from outside).
- `FakeBackend` — the same contract over plain memory with a timerfd
  standing in for vblank, so the server and its tests run headless.

The server codes against `Backend` and never sees a DRM type.

## The contract

Everything is single-threaded and non-blocking. A backend hands out the fds
it wants watched (`poll_fds()`); when one is readable the caller invokes
`dispatch(&mut events)`, which never blocks and appends `Event`s to the
caller's vector (caller-provided so the frame path does not allocate).

Pixels are `XRGB8888` or `ARGB8888`, which are the same bytes: one
little-endian `u32` per pixel, `0xAARRGGBB`. Byte 3 is premultiplied
alpha when the output scans out ARGB (see "ARGB scanout (#3898)"
below) and is ignored otherwise. `Image::pixel` masks it off, and
`Image::alpha` reads it. Buffers have a `stride` in bytes that is *not* necessarily
`width * 4` (the fake pads to 64 bytes on purpose so stride bugs show up).

### Back-buffer borrow rules

Per output the backend owns two buffers. Exactly one is **front** — the most
recently committed one — and the other is **back**.

```text
                 back_buffer()        commit()              Flipped
   writable  ───────────────────►  in flight  ─────────────────────►  writable
  (back = B)   borrow &mut B      front := B, pending     pending := false,
                                   (neither writable)      back := old front
```

- `back_buffer(id)` borrows the back buffer mutably. The borrow ends when
  the `BufferMut` is dropped; nothing is committed until `commit`.
- `commit(id, damage)` presents the back buffer. It completes
  asynchronously: the output is `flip_pending` until `dispatch` yields
  `Event::Flipped { output: id, .. }`.
- While a flip is pending **neither buffer may be written**: the old front
  is still being scanned out and the new front is queued. `back_buffer` and
  `commit` return `Error::FlipPending`. Check `flip_pending(id)` before
  rendering, or just try and skip the frame.
- After `Flipped`, the back buffer is the one that was on screen *two*
  frames ago. Callers must either repaint fully or accumulate damage over
  two frames (the usual "age = 2" scheme). The backend does not copy
  between buffers.
- `damage` is in output pixels, clipped to the output; an empty slice means
  "everything changed". It is passed to the kernel as `FB_DAMAGE_CLIPS`
  when the primary plane exposes that property and silently dropped
  otherwise. It is a hint for the display path, not a promise that pixels
  outside it are preserved.
- The dumb-buffer memory is write-combined: write rows sequentially, never
  read from it. `read_front(id)` returns a tightly packed copy of the
  front buffer (the most recently committed one, whether or not its flip
  has completed) for screenshots.

### `pause` / `resume`

`pause()` is called when the session goes inactive (VT switch). The
backend stops accepting commits (`Error::Paused`) and keeps all state. A
flip already in flight completes and is still reported by `dispatch`.

`resume()` re-modesets every output with one blocking `ALLOW_MODESET`
commit. DRM master may have been revoked and re-granted in between: dumb
buffers and framebuffer objects survive that, CRTC/plane state does not.
`rescan()` may be called while paused. Only lit outputs are restored: an
output with no `commit` yet has nothing of ours to restore, and `resume()`
leaves it to its first commit (see "The first picture is a finished
frame").

### Hotplug

`Event::Hotplug` says "some connector changed". Call `rescan()`: it
re-probes connectors and returns whether `outputs()` changed. Outputs whose
connector vanished (or whose preferred mode changed) release their buffers
and their `OutputId` is never reused; new connectors get an id, buffers and
lit at their first `commit`, like every output; the outputs already lit
are modeset again at once (unless paused).
Re-query `poll_fds()` after a rescan.

Documented-not-solved corner cases: more connected connectors than CRTCs
(the extra ones are skipped, retried at the next rescan); a monitor that is
replaced by another on the same connector between two rescans with the
same mode (kept as is); CRTC assignment is greedy, not a maximum matching.

## `DrmBackend` commit sequence

1. `open(fd, opts)`: set `O_NONBLOCK` on the fd; enable client caps
   `UNIVERSAL_PLANES` and `ATOMIC` (`Error::Unsupported` otherwise); cache
   the property ids of every connector (`CRTC_ID`), CRTC (`MODE_ID`,
   `ACTIVE`) and plane (`FB_ID`, `CRTC_ID`, `SRC_*`, `CRTC_*`, optional
   `FB_DAMAGE_CLIPS`, `type`) — a missing one is `Error::MissingProperty`
   naming object and property.
2. Enumerate: force-probe every connector; for connected ones pick the
   preferred mode (else largest area, then highest refresh; interlaced
   loses), or the configured one, then **keep the mode the CRTC is
   already showing when it is as good** (`select::keep_on_screen`, below);
   give each a CRTC from the union of its encoders'
   `possible_crtcs` (preferring the one already driving it) and a primary
   plane whose `possible_crtcs` includes that CRTC. Allocate two
   `XRGB8888` dumb buffers, `AddFB2` them, map them for the output's
   lifetime, create the mode blob.
3. Initial modeset, **per output, deferred to its first `commit`** and
   made with the frame that commit carries: one blocking `ALLOW_MODESET`
   commit for the outputs lit so far — their connectors get `CRTC_ID`,
   their CRTCs `MODE_ID` + `ACTIVE=1`, their planes `FB_ID`/`CRTC_ID`/
   `SRC_*` (16.16)/`CRTC_*`. An output not lit yet is left out of the
   request and keeps the previous picture, rather than showing a zeroed
   buffer until its own frame. The commit that lights the **last** output
   describes the complete state: every other connector gets
   `CRTC_ID=0`, every other CRTC `MODE_ID=0`, `ACTIVE=0`, every other
   primary plane `FB_ID=0`, `CRTC_ID=0`. Stating the whole picture is
   what keeps the kernel from rejecting the commit because fbcon or a
   previous master left a CRTC attached elsewhere. A partial commit the
   kernel refuses for that reason (our connector assigned a CRTC that
   still drives one of our unlit connectors) falls back to lighting every
   output at once. Each is followed by an ordinary flip to the same
   buffer, which is what delivers the output's first `Event::Flipped`.
4. `commit`: `NONBLOCK | PAGE_FLIP_EVENT` with the plane's `FB_ID` and,
   when supported, an `FB_DAMAGE_CLIPS` blob (`drm_mode_rect` x1,y1,x2,y2
   built in a reused `Vec<i32>`; the blob is destroyed right after the
   ioctl — the kernel holds its own reference). The only per-commit
   allocations are that blob and the clone of the small per-output request
   template that the `drm` crate's by-value `atomic_commit` forces.
5. `dispatch`: read `DRM_EVENT_FLIP_COMPLETE` records off the fd (the
   kernel's 32-bit sequence and `CLOCK_MONOTONIC` timestamp become
   `Event::Flipped`), then drain the netlink socket; any message with
   `SUBSYSTEM=drm` and `HOTPLUG=1` becomes one `Event::Hotplug`.
6. Drop: framebuffers, dumb buffers and blobs are destroyed. CRTC state is
   deliberately left alone — the kernel restores fbcon (or the next master
   sets its own) when master status goes away with the fd.

## The first picture is a finished frame

A server starting on a panel that is already lit (by fbcon, by the
greeter's compositor, by the previous session) should replace what is
there with its own first frame, with no black frame in between and,
where possible, without a monitor resync. Two rules, both about each
output's first commit:

- **No commit before there is a frame, per output.** `open` probes,
  allocates and picks modes, and leaves the CRTCs alone. Until an
  output's first `commit`, its panel keeps the previous picture. It is
  not replaced with the zeroed dumb buffer, which is what `open` used to
  commit, and with several outputs the first commit lights only its own:
  lighting them all at once would put the zeroed buffer on every panel
  but the one whose frame was ready. A `rescan` or `resume` leaves an
  unlit output alone, and commits nothing while no output is lit.
  A card that is opened, found to have no output and dropped (the
  server's multi-GPU probe) is now never modeset at all.
- **Keep the mode that is lit, when it is as good.** The kernel does a
  full modeset (and the monitor resyncs, one to three seconds of black
  on many HDMI panels) only when the new mode differs from the old one,
  by the `drm_mode_equal` comparison: timings and flags. So the probe
  reads the mode the connector's current CRTC is scanning out and
  prefers it over the pick when `select::keep_on_screen` says it answers
  the configuration as well. With nothing configured: the same size, and
  a refresh no more than half a hertz below. With a request: exactly the
  refresh the request resolved to. A modeline is never replaced. This
  applies **only before the connector's first commit**, while the lit
  mode is someone else's. After that it is our own earlier pick, and a
  configuration reload must be able to move away from it.

It is not seamless yet. The previous compositor's exit destroys its
framebuffers (so its planes go dark), and the kernel restores fbcon when
the last DRM fd closes. That is the gap between the previous server's
exit and this one's first commit, and `docs/greeter.md` sets out what
closing it would take. What this section guarantees is that the gap
ends in a finished frame, as early as the server can paint one, and
without a resync when the mode allows.

Verified by unit tests for the mode rule and for which objects a
lighting commit lists only. This container has no DRM device, so the
deferred modeset needs checking on the box: a cold start from fbcon,
`systemctl restart nitro-dev`, a VT round trip, and a start with two
monitors (each should go from the old picture straight to ours).

The fd is passed as `DrmFd::Owned` (closed on drop) or `DrmFd::Borrowed`
(never closed — for when the seat owns it and closes it itself after the
backend is gone). Hotplug needs the netlink socket, which some sandboxes
forbid; that failure is non-fatal and reported by `hotplug_error()`.

## What `FakeBackend` guarantees

- Same contract as above, including `FlipPending` and `Paused` errors.
- One or more outputs of configurable size/refresh/name; stride padded to
  64 bytes; buffers start black.
- The vblank is a one-shot timerfd armed by the first `commit` after an
  idle period; an idle fake causes no wakeups. Every output committed
  before it fires flips at that tick. `Flipped.time` is `CLOCK_MONOTONIC`,
  `sequence` counts per output from 1.
- `tick(&mut events)` flips synchronously for tests that do not poll.
- `damage_log()` records every `(output, damage)` passed to `commit`.
- `plug(spec)` / `unplug(id)` queue an `Event::Hotplug` (delivered by the
  next `dispatch` or `tick`) and take effect on `rescan`.
- `read_front` returns the front buffer; `write_ppm(id, path)` dumps it as
  P6 for eyeballing.
- Planes: `FakeOutputSpec::planes(vec![FakePlaneSpec::…])` sets an output's
  inventory. Without it the output gets one primary plane with linear
  XRGB8888 + ARGB8888 and no scaling. Plane ids are unique per backend.
  Each output is its own CRTC (`crtc_mask` bit `(id - 1) % 32`).
- ARGB scanout: `FakeOutputSpec::alpha` (default `true`; builder
  `.alpha(false)`) sets `scanout_alpha`. `set_scanout_alpha(id, true)` on
  a non-alpha output is `Error::Unsupported`; off is always `Ok`.
  `scanout_alpha_on(id)` reads the state back and
  `scanout_alpha_sets(id)` counts the successful calls.
- `alloc_buffer` / `free_buffer` only record `(format, w, h)`
  (`buffer(id)` reads it back). Odd NV12 sizes are refused.
- `test_layout` follows the DRM contract (`Paused`, `NotLit` before the
  first commit, `NoSuchObject` for a foreign plane or an unknown buffer).
  The verdict comes from a rule-based acceptor, which says `EINVAL` for:
  - a duplicate plane
  - a format that is not listed with LINEAR
  - `src` outside the buffer, or `dst` off the output
  - scaling on a non-scaling plane, or outside its `scale_limits`
  - zpos outside its range, an immutable zpos being moved, or two planes
    with equal mutable zpos
  - a rotation, `COLOR_*` value or in-fence the plane doesn't offer

  It says `ENOSPC` beyond `set_max_active_planes(n)`, which stands in
  for shared scalers and bandwidth. `set_test_hook` replaces the rules
  outright. `test_log()` records `(output, planes, verdict)` for every
  answered question.

## Hardware smoke test

```sh
cargo build --release --example kms_fill
rsync target/release/examples/kms_fill kaspar@192.168.1.204:nitro-bin/
ssh kaspar@192.168.1.204 'sudo ~/nitro-bin/kms_fill /dev/dri/card1'
```

Opens the card directly (no libseat; needs root or a VT with no other
master), modesets every output, shows a gradient with a bar moving one
column per flip for 3 s, prints flip-interval stats, then exercises
`rescan` and `pause`/`resume`. Expect ~16.7 ms mean at 60 Hz.

## Planes: discovery, `TEST_ONLY` and the HSW GT1 inventory (measured)

`Backend::planes(output)` lists every plane that can go on the output's
CRTC: primary first, then overlays, then cursors. For each plane it
reports:
- kind and `possible_crtcs`
- formats with their modifiers, parsed from the `IN_FORMATS` blob, or
  `formats()` + LINEAR when the plane has no such blob
- `zpos` (current value, range, and whether it is immutable)
- the `rotation` bits
- the `COLOR_ENCODING` / `COLOR_RANGE` / `pixel blend mode` names as the
  kernel spells them
- `alpha`, `FB_DAMAGE_CLIPS` and `IN_FENCE_FD`

Discovery runs once per `rescan`, never per frame, and a property that
cannot be read is reported as absent rather than failing. `scaling` is
`None` on DRM: no property says whether a plane scales, so only a test
commit can tell.

`alloc_buffer(format, w, h)` makes a linear dumb scanout buffer.
Supported formats are XRGB/ARGB/XBGR (32 bpp), YUYV/UYVY (16 bpp), and
NV12 (one dumb buffer of `h * 3/2` rows, with the CbCr plane at
`pitch * h`). i915 validates the format at `AddFB2`, so an unsupported
format fails here with `Error::Io`, before any test runs.

`test_layout(output, &[PlaneAssignment])` sends one atomic commit with
`DRM_MODE_ATOMIC_TEST_ONLY`. The contract:
- **The layout is the whole CRTC.** Every plane that can go on the CRTC
  and is not listed is disabled in the test, the primary included, so
  the answer doesn't depend on what is on screen.
- **No `ALLOW_MODESET`.** A layout that would need a modeset is
  rejected, which is the right answer for a decision made at flip time.
- **The output must be lit** (committed once): `Error::NotLit` before
  then, and `Error::Paused` while paused.
- `Ok(Verdict::Rejected(errno))` means the display engine said no. `Err`
  means the question could not be asked: a stale output, a plane that
  isn't this output's, or an unknown buffer.
- If a zpos, rotation, `COLOR_*` value or in-fence is requested on a
  plane without that property, or an immutable zpos is asked to move,
  the answer is `Rejected(EINVAL)` without a round trip, because that is
  what the kernel would say.
- `PlaneSource::OutputFront` is the output's current front buffer.
  The frame path (`commit`, `flip`, the modesets) is untouched; it still
  uses the primary plane only.

### Probe

```sh
cargo build --release -p nitro-kms --example planes_probe
rsync target/release/examples/planes_probe kaspar@192.168.1.204:tmp/
ssh kaspar@192.168.1.204 'sudo systemctl stop nitro-dev; sudo ~/tmp/planes_probe /dev/dri/card1; sudo systemctl start nitro-dev'
```

The probe lights each output with one black frame and prints the
inventory. It then test-commits a fixed set of layouts:
- XRGB on the primary
- NV12 and YUYV overlays at 1:1, as a window, scaled, and below the
  primary
- two overlays
- XRGB/ARGB overlays at 1:1, 2× and 0.5×
- cursor sizes

A buffer the kernel refuses at `AddFB2` shows as `SKIP`.

### Test box: Intel HD (Haswell GT1), i915, kernel 7.0.0-15-generic, 1920×1080 HDMI — 2026-09-29

```text
output#1 HDMI-A-1 1920x1080: 3 planes
  plane#34 primary crtcs=0b01 zpos=0 [0..0] immutable rotation=rotate-0|rotate-180
    COLOR_ENCODING: -; COLOR_RANGE: -
    blend: -; alpha=false damage_clips=false in_fence=true
    [I915_X_TILED, LINEAR]: C8   RG16 XR24 XB24 XR30 XB30 XB4H
  plane#39 overlay crtcs=0b01 zpos=1 [1..1] immutable rotation=rotate-0|rotate-180
    COLOR_ENCODING: ITU-R BT.601 YCbCr, ITU-R BT.709 YCbCr; COLOR_RANGE: YCbCr limited range, YCbCr full range
    blend: -; alpha=false damage_clips=false in_fence=true
    [I915_X_TILED, LINEAR]: XR24 XB24 XR30 XB30 XR4H XB4H YUYV YVYU UYVY VYUY
  plane#46 cursor  crtcs=0b01 zpos=2 [2..2] immutable rotation=rotate-0|rotate-180
    COLOR_ENCODING: -; COLOR_RANGE: -
    blend: -; alpha=false damage_clips=false in_fence=true
    [LINEAR]: AR24
  layouts (TEST_ONLY, no ALLOW_MODESET; unlisted planes on the CRTC disabled):
  (a) XRGB fullscreen on primary                             ACCEPT
  (a2) XRGB 1920x1080 buffer on primary                      ACCEPT
  (a3) XRGB primary scaled 1280x720 -> fullscreen            REJECT ERANGE
  (a4) XRGB primary as a 960x540 window                      REJECT EINVAL
  (b) NV12 1920x1080 overlay 1:1 above primary               SKIP: addfb NV12 1920x1080 failed (EINVAL)
  (b2) NV12 1920x1080 overlay 1:1 alone (primary off)        SKIP: addfb NV12 1920x1080 failed (EINVAL)
  (b3) NV12 960x540 overlay 1:1 window above primary         SKIP: addfb NV12 960x540 failed (EINVAL)
  (c) NV12 on primary, XRGB UI on overlay (fixed zpos)       SKIP: addfb NV12 1920x1080 failed (EINVAL)
  (d) NV12 1280x720 scaled to fullscreen                     SKIP: addfb NV12 1280x720 failed (EINVAL)
  (d2) NV12 1280x720 scaled to a 960x540 window              SKIP: addfb NV12 1280x720 failed (EINVAL)
  (e) two NV12 overlays                                      SKIP: one overlay plane
  (b) YUYV 1920x1080 overlay 1:1 above primary               ACCEPT
  (b2) YUYV 1920x1080 overlay 1:1 alone (primary off)        ACCEPT
  (b3) YUYV 960x540 overlay 1:1 window above primary         ACCEPT
  (c) YUYV on primary, XRGB UI on overlay (fixed zpos)       REJECT EINVAL
  (d) YUYV 1280x720 scaled to fullscreen                     REJECT ERANGE
  (d2) YUYV 1280x720 scaled to a 960x540 window              REJECT ERANGE
  (e) two YUYV overlays                                      SKIP: one overlay plane
  (g) XR24 960x540 overlay 1:1 above primary                 ACCEPT
  (g2) XR24 960x540 overlay 2x to fullscreen                 REJECT ERANGE
  (g3) XR24 1920x1080 overlay 0.5x to 960x540                REJECT ERANGE
  (g) AR24 960x540 overlay 1:1 above primary                 REJECT EINVAL
  (g2) AR24 960x540 overlay 2x to fullscreen                 REJECT EINVAL
  (g3) AR24 1920x1080 overlay 0.5x to 960x540                REJECT EINVAL
  (h) ARGB 64x64 on cursor + primary                         ACCEPT
  (h) ARGB 128x128 on cursor + primary                       ACCEPT
  (h) ARGB 256x256 on cursor + primary                       ACCEPT
  (h2) ARGB 64x64 cursor + YUYV overlay + primary            ACCEPT
```

What this means for the `planes` module (#3899) on this box:

- **One overlay plane per CRTC, plus the cursor.** The primary is at
  zpos 0, the sprite at 1 and the cursor at 2, and all three are
  immutable. Underlay is therefore impossible: nothing can go below the
  primary. The video can go *on* the primary only if it is RGB (the
  primary lists no YUV), and the UI would then sit on the overlay. So on
  HSW, "Surface on a plane" means **overlay above the UI, with the
  Surface's rectangle unobscured**. Anything else is composited.
- **No NV12 anywhere.** The overlay doesn't list it and `AddFB2` refuses
  NV12 framebuffers outright (EINVAL). Scanout-capable YUV on HSW is
  **packed 4:2:2 only** (YUYV/YVYU/UYVY/VYUY, BT.601/709, limited or
  full range, on the overlay). Server-allocated video buffers for this
  box must be YUYV, not NV12.
- **No scaling observed with linear buffers.** The overlay rejected every
  scaled layout with ERANGE: 2× up, 0.5× down, and 0.75×
  (1280×720 → 960×540). Only 1:1 was accepted, both fullscreen and as a
  window. The primary cannot scale (ERANGE) and must cover the whole
  CRTC (a 960×540 window gives EINVAL). Whether sprite scaling needs a
  tiled buffer or is off for another reason on this kernel is not
  established. Only linear dumb buffers were tried, because the API
  allocates nothing else. Until that is settled, treat a scaled Surface
  as composited on this box.
- **The overlay has no ARGB.** Only XR24 and friends are listed, and AR24
  is refused. An overlay is opaque; there is no per-pixel blending with
  what is below it.
- The cursor plane takes ARGB at 64, 128 and 256, alongside an active
  YUYV overlay. The 3-plane layout (primary + YUYV overlay + cursor) is
  accepted.
- No plane has `FB_DAMAGE_CLIPS` on this kernel. Every plane has
  `IN_FENCE_FD` and `rotate-0|rotate-180`. There is no `alpha` and no
  `pixel blend mode`.

## ARGB scanout (#3898)

`Backend::scanout_alpha(output)` says whether the primary plane can scan
out `ARGB8888`. `Backend::set_scanout_alpha(output, on)` switches it,
starting with the next commit or modeset. The bytes are the same either
way: byte 3 of every pixel is premultiplied alpha. The server writes 255
everywhere except in "holes", where it writes 0 so that a plane *below*
the primary (an underlay) shows through. On DRM the capability is "the
primary's formats list linear AR24". Enabling it `AddFB2`s a second
framebuffer, `ARGB8888`, on each of the output's two dumb buffers. It
uses the same handles, so nothing is reallocated or copied. The flip,
the modesets and `PlaneSource::OutputFront` all use that framebuffer
while alpha is on. When the primary has `pixel blend mode`, it is set to
`Pre-multiplied`, which is the kernel default anyway. A resize replaces
the output, which turns alpha off again.

**Verdict for the test box** (see the inventory above): the HSW GT1
primary lists C8, RGB565, XR24, XB24, XR30, XB30 and XB4H. That means
no ARGB8888 and no `pixel blend mode`. zpos is immutable: primary 0,
sprite 1, cursor 2. So underlays are impossible there, `scanout_alpha`
is false, and the server stays on XR24. The AR24 path has been verified
on `FakeBackend` only.
