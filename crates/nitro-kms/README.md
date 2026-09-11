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

Pixels are always `XRGB8888`: one little-endian `u32` per pixel,
`0x00RRGGBB`. Buffers have a `stride` in bytes that is *not* necessarily
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
`rescan()` may be called while paused; new outputs are then modeset by
`resume()`.

### Hotplug

`Event::Hotplug` says "some connector changed". Call `rescan()`: it
re-probes connectors and returns whether `outputs()` changed. Outputs whose
connector vanished (or whose preferred mode changed) release their buffers
and their `OutputId` is never reused; new connectors get an id, buffers and
an immediate modeset (unless paused). Re-query `poll_fds()` after a rescan.

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
   loses); give each a CRTC from the union of its encoders'
   `possible_crtcs` (preferring the one already driving it) and a primary
   plane whose `possible_crtcs` includes that CRTC. Allocate two
   `XRGB8888` dumb buffers, `AddFB2` them, map them for the output's
   lifetime, create the mode blob.
3. Initial modeset: **one** blocking `ALLOW_MODESET` commit describing the
   complete state — our connectors get `CRTC_ID`, our CRTCs `MODE_ID` +
   `ACTIVE=1`, our planes `FB_ID`/`CRTC_ID`/`SRC_*` (16.16)/`CRTC_*`; every
   other connector gets `CRTC_ID=0`, every other CRTC `MODE_ID=0`,
   `ACTIVE=0`, every other primary plane `FB_ID=0`, `CRTC_ID=0`. Stating
   the whole picture is what keeps the kernel from rejecting the commit
   because fbcon or a previous master left a CRTC attached elsewhere.
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
