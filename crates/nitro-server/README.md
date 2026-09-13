# nitro-server

The display server. M1 shape: one thread, one epoll, a seat
(`nitro-seat`), a KMS backend (`nitro-kms`), a real scene graph
(`nitro-scene`) painted with `nitro-raster`, clients over `nitro-wire`,
and input from libinput and xkbcommon. `lib.rs` exposes `run(Config)`;
`main.rs` only turns environment variables into a `Config`, so the
integration tests drive the whole loop in-process on the fake backend with
a fake input source — no seat, no DRM device and no evdev node anywhere.
The M0 demo is gone: there is no moving bar, and the only pixels the
server paints for itself are the desktop background under the clients.

## Sockets

Two, and they do different jobs.

The **wire socket** is the one clients use: `nitro-wire` v1, at
`$XDG_RUNTIME_DIR/nitro/wire.sock` unless `NITRO_SOCKET` says otherwise
(`/tmp/nitro-<uid>/wire.sock` when the runtime directory is unset or
relative). Client and server resolve that path through the *same*
function in `nitro-wire`, so a client started in the server's environment
finds it without being configured. A connection opens with `Hello`
(version plus a name for the logs) and is answered with `Welcome`
(version, capability bits, server name). The capability word is **0** in
v1, deliberately: direct scanout, `Text` nodes and dma-buf surfaces are
M2/M5 work, and a zero bit is the protocol's way of saying "this does not
exist yet" — a client that asks for a `Text` or `Surface` node gets
`WrongKind` and the connection closes, rather than discovering at runtime
that the feature silently did nothing.

The **control socket** is the v0 line protocol and stays as the server's
own test and debug channel — it is what `nitro-shot` and the integration
tests speak, and it deliberately has nothing to do with the client
protocol. Path: `NITRO_CONTROL`, else
`$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock`
with a warning. One request per line; the reply is a status line and an
optional body. Parsing and formatting live in `protocol.rs` with unit
tests.

| request              | reply                                                                 |
|----------------------|-----------------------------------------------------------------------|
| `shot [output-name]` | `ok <width> <height> <stride>\n` + `stride*height` bytes `XRGB8888` (`read_front`) |
| `outputs`            | `ok\n`, one `name WxH@refresh_mhz\n` per output, blank line            |
| `stats`              | `ok\n`, one `key value` line per statistic (see below), blank line     |
| `quit`               | `ok\n`, then orderly shutdown                                          |
| `plug WxH`           | `ok\n`; fake backend only — hotplugs an output in, so a test can drive the "no output yet" state. Refused on DRM, where an output exists because a connector says so. |
| anything else        | `err <message>\n`                                                     |

Several requests per connection are fine; a request line longer than 256
bytes without a newline drops the client. Both socket files are unlinked
on shutdown.

## Environment

| variable          | values                           | default                        |
|-------------------|----------------------------------|--------------------------------|
| `NITRO_BACKEND`   | `drm`, `fake`                    | `drm`                          |
| `NITRO_DRM_CARD`  | `/dev/dri/cardN`                 | first card with a connected output, else first that opens |
| `NITRO_FAKE_SIZE` | `WxH` (fake only)                | `1280x720`                     |
| `NITRO_CONTROL`   | socket path                      | `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock` (with a warning) |
| `NITRO_SOCKET`    | socket path                      | `$XDG_RUNTIME_DIR/nitro/wire.sock` (resolved by `nitro-wire`, so clients agree) |
| `NITRO_INPUT`     | `off`                            | input enabled                  |
| `NITRO_INPUT_DIR` | directory scanned for `event*`   | `/dev/input`                   |
| `NITRO_LOG`       | `error`, `warn`, `info`, `debug` | `info`                         |

The keyboard layout comes from the `XKB_DEFAULT_{RULES,MODEL,LAYOUT,VARIANT,OPTIONS}`
variables — the same ones every other libinput-based compositor and
`setxkbmap` use — with a fallback to the `us` layout.

`fake` needs no seat at all and never opens input devices: `just fake`
runs it locally, `just fake-shot` grabs a PNG from it.

## Event loop

Level-triggered epoll, no timers. An idle server never wakes: with
nothing changing there is no damage, so no frame is painted, so no flip
completes, so nothing becomes readable. `top` shows 0.0 % and
`voluntary_ctxt_switches` stops counting — including with a client
connected and its window on screen.

| fd                        | on readable                                                              |
|---------------------------|--------------------------------------------------------------------------|
| seat                      | `Seat::dispatch`; `Disable` → suspend input, `backend.pause()`, `ack_disable()`; `Enable` → resume backend and input, reset xkb, full repaint |
| backend `poll_fds()`      | `Backend::dispatch`; `Flipped` → `Presented`/`Frame` to clients, then paint the next frame; `Hotplug` → `rescan()`, re-register fds, place unplaced windows, repaint |
| libinput                  | dispatch, convert to `InputEvent`, route, then update the scene and paint if anything moved |
| signal self-pipe          | SIGTERM/SIGINT → orderly shutdown (`signal-hook`'s `low_level::pipe` on a `UnixDatagram` pair) |
| control listener          | accept, register the client                                              |
| wire listener             | accept, allocate a `ClientId`, register the client                       |
| wire client               | read, decode, buffer mutations, apply on `Commit`; `OUT` interest only while bytes are queued |
| control client            | read lines, answer, drop on hangup; `OUT` interest only while a reply is queued |

Every wakeup does a bounded amount of work — a client read is capped by
`nitro-wire`'s read budget, a frame's paint is proportional to its damage,
nothing blocks — which is why one thread is still enough. Fanning
rasterization out to workers is a decision for the day a frame's damage
stops fitting in a vblank, and that day has not arrived (see the measured
numbers below).

`epoll_wait` is retried on `EINTR` (the signal handler interrupts it and
the kernel never restarts `epoll_wait`); the self-pipe is what ends the
loop.

Startup on DRM: open the seat, `dispatch()` once (libseat queues the
initial `Enable` inside `open_seat` without making the fd readable), wait
for `is_active()`, then open the card through the seat. The backend gets a
`dup` of the seat's fd (same open file description, hence DRM master) so it
can be `'static`; the seat closes its own fd after the backend is gone.

Shutdown order is the `Server` struct's field order: wire clients, control
clients, sockets, input (whose device fds belong to the seat), backend
(destroys FBs/dumb buffers, releases master with the fd), the seat's
`Device`, the seat. Exit code 0.

## Clients

**Id translation.** A client allocates its own `NodeId`s and `BufferId`s
and never learns the scene's keys. The server keeps a map per client in
*both* directions: ids to keys, to look a mutation up, and keys back to
ids, to name a node in an input event. The forward map is also the
isolation mechanism — a client simply cannot name another client's node,
which is cheaper and harder to get wrong than an ownership check on every
call. The reverse map is safe because scene keys are generational: a
stale key can never be confused with a recycled one.

**Transactions.** Nothing a client sends touches the scene until its
`Commit`. Mutations are buffered in arrival order and applied in one
pass, so a frame never shows half a batch. The one exception is
`CreateBuffer`, whose pixels are read when the message arrives rather than
at commit: the client may legitimately reuse the memfd for its next frame
as soon as it has sent the message, and reading late would tear its own
image. The descriptors ride along with the transaction and move into the
server's buffer map only once the scene has minted keys for them.

**Errors are fatal.** A mutation the scene refuses aborts the whole batch:
the client gets one `Error` carrying the commit serial and a code, and the
connection closes. A client that named a bad id has lost track of its own
tree, and everything it sends afterwards is guesswork. Disconnect — for
any reason, including the socket simply going away — destroys everything
the client owned: its windows (and with them their node trees) and its
buffers. The client's own maps are the authority, so that is a walk of
one hash map rather than a search of the scene, and the scene damages the
pixels those windows covered as it removes them, so the area repaints
without them on the next frame.

**The buffer copy path.** `CreateBuffer` validates the description
(format, stride, a 64 MiB cap that fits a 4K ARGB frame with room to
spare) and `pread`s the pixels out of the client's memfd into scene-owned
memory. They are **copied, not mapped**, and that is the M1 answer rather
than an oversight: a client can shrink a memfd under a live mapping and
turn the server's next read into SIGBUS, so a safe mapping needs either
enforced `F_SEAL_SHRINK` or a signal handler — and `mmap` is `unsafe`,
which this tree does not use. The copy costs one pass over the pixels per
update, and `BufferDamage` keeps that pass proportional to what actually
changed: only the damaged *rows* are re-read, one `pread` per row band,
because a row is contiguous and damage rectangles are usually wide.
Revisit with sealing when a client pushes video.

`DestroyBuffer` releases the descriptor as well as the pixels. The scene
forgets the bytes on its own, but the fd is the server's, and a client
that cycles buffers — create, damage, destroy, once per frame, which is
the obvious way to push changing images — would otherwise leak one
descriptor per frame until it hit the process limit.

**Placement.** A new window is cascaded onto the primary output —
`CASCADE_STEP` pixels right and down from the previous one, wrapping, and
never so far that its top-left corner leaves the output. The placement is
arithmetic in the window's index rather than stateful, so it is
predictable in a test and identical after a restart. The client is then
told what it got with `Configure` (size, scale, output); a later resize
the server or the client's own `SetBounds` decides on produces another.

A window created while there is *no* output — every connector unplugged,
or a hotplug still in flight — is held in a pending list instead. It is a
real window owning real nodes; it simply has nowhere to be, and it is
placed and configured the moment an output appears. No `Configure` is
sent before then: an unplaced window has no size, scale or output to
report, and naming output 0 for it would be a lie the client cannot
detect.

**Presentation and frame callbacks.** A commit is reported with
`Presented` when the frame carrying it reaches the screen, and a
`RequestFrame` is answered with `Frame` *after* the flip, because the
deadline it carries is only meaningful once the vblank it is extrapolated
from has actually happened.

Both of those need a second path, though, and the reason is the design
itself: **the server only flips when something changed**, so "wait for the
next flip" is not a promise it can keep to a client that changed nothing.
Two cases would otherwise hang:

* A commit that damages no pixel — a hidden subtree, a property set to the
  value it already had, a bare `RequestFrame`. The serial is the client's
  flow control, so it is acknowledged from the last vblank we saw rather
  than held for a frame that will never carry it. Holding it would stall
  any client that waits for `Presented` before sending the next frame, and
  would grow the pending list without bound. There are two shapes of this:
  a client with no *placed* window — nothing it commits can reach a screen
  — is answered inline at commit time, and one whose windows are placed
  but whose transaction produced no damage is answered once the update
  pass has confirmed there is nothing to paint.
* A `RequestFrame` from a quiescent desktop — which is exactly how a
  client *starts* an animation. If the answer waited for a flip, and the
  flip waited for damage, and the damage was going to be the client's
  response to the answer, nothing would ever happen. It is answered
  immediately from the extrapolated vblank clock, which
  `frame::frame_deadline` computes with or without a flip having happened.

A commit is stamped only onto the outputs its own windows are on, so the
same serial is never reported twice on a multi-output desktop, and frame
callbacks are answered only by the vblank of the output the requesting
window is actually on.

## Frame path

One pass, for each output, every time an event could have changed
anything:

1. `scene.update()` runs the scene's layout/damage pass and folds each
   output's damage (global device pixels, shifted back to the output's own
   origin) into that output's `OutputState`. Any `Configure` it produces
   is sent to the owning client.
2. `repaint_region()` computes the region to paint. The backend owns two
   buffers per output and alternates them strictly, so the buffer handed
   out at frame `n` is the one that was on screen at frame `n - 2`.
   Painting only this frame's damage would leave the previous frame's
   changes stale in it, so the region painted is **`damage(n) ∪
   damage(n-1)`** — the *age-2 rule*. What is carried forward is the
   previous frame's **damage**, not the region it painted: the painted
   region was itself a union of two frames' damage, and feeding it back
   would make every frame at least as large as the one before and never
   shrink again. One consequence worth stating: after a resume, a modeset
   or a new output, `invalidate()` simply sets the damage to the whole
   output, and the rule produces the two full frames both unknown buffers
   need, with no separate "repaint fully for N frames" counter to get
   subtly wrong when a client commits between them.
3. For each rect of that region: the server's background first — the
   vertical gradient inside its 4-px frame, one `fill_irect` per row —
   unless an opaque client rect covers the whole clip, which the scene
   promises conservatively; then the scene's paint list clipped to the
   rect, one `nitro-raster` call per item; then the software cursor last.
   The rasterizer never writes outside the clip it was given, so one rect
   cannot smear into another.
4. `commit` with the same region as `FB_DAMAGE_CLIPS` — it is exactly the
   set of pixels that differ between what this buffer holds and what must
   be on screen — and the paint time and damage area go into the rolling
   statistics.

There is no hardware cursor plane. A KMS cursor is composited by the
display engine rather than into the framebuffer, so it does not appear in
a screenshot taken by dumping the back buffer; every visual test run over
SSH would lose the pointer exactly when it matters. The cursor is a 24×24
ARGB image blitted like anything else, inside the frame's damage clip, and
a pointer move damages the old and the new cursor rect.

It is drawn only once a pointer device has actually reported something,
not merely because an output exists. Whether there is a pointer on this
desk is a question libinput's capability bits answer badly — a machine can
have a disabled touchpad, or a "pointer" that is really a lid switch — and
the honest test is whether one has moved. A keyboard-only box therefore
shows no arrow, which is also what a headless server's screenshots should
show.

A commit that fails keeps its damage and sets a retry flag, so the next
event repaints the same region instead of stranding the output until the
next resume or hotplug (issue #519). The paint itself is not wasted — the
back buffer holds it — but the buffers did not swap, so the next paint
must cover the same region again. The matching fix on the other side is
that `nitro-kms::resume()` now clears flip-pending for every output
(issue #520): a flip abandoned by a VT switch is never completed, and an
output that still believed a flip was in flight would never paint again.

## Input

libinput is created with `new_from_path`, not `new_with_udev`: the udev
backend would pull `libudev` into the dependency tree for something a
`read_dir` of `/dev/input` does (`event*` nodes only — `mouse*` and `js*`
are legacy interfaces for the same hardware, sorted numerically so
`event10` follows `event9`). Devices are opened through `nitro-seat`,
which is the only thing in the process allowed to open `/dev/input/*`, and
that is what lets the server run without root. They are also **closed back
through the seat**, which matters more than it looks: libinput hands back
only the raw descriptor, so the `Device` it belongs to must be findable
from that number, and libseat refuses to reopen a device it still has
open — leaving one behind is exactly what makes `Libinput::resume` fail
after a VT switch. Input-device hotplug is M3; the uevent socket already
carries the notifications, but acting on them means re-scanning and
diffing, which is not worth the code before the shell exists.

Everything above `input.rs` speaks `InputEvent`, a small enum with no
libinput in it. That `InputSource` seam is what makes the whole input
path — hit-testing, focus, enter/leave bookkeeping, the input-to-photon
clock — testable on the fake backend: `FakeSource` is a queue a test
pushes into, woken through an `eventfd` so the loop sees the same "fd
readable, then dispatch" shape it sees from libinput.

Keys are translated by xkbcommon against the `XKB_DEFAULT_*` keymap, with
the evdev-to-XKB `+ 8` offset applied at every boundary and each event
resolved against the state *before* it is applied (so pressing Shift does
not retroactively shift itself). If no keymap compiles the server logs a
warning and runs on: the evdev keycode still reaches the focused client,
without keysym or text, which is a far better failure mode for a display
server than refusing to start. Two chords never reach a client, because
they are the compositor's: **Ctrl+Alt+Backspace** quits, and
**Ctrl+Alt+F1..F12** switches VT through the seat.

Routing:

* The pointer has one position in device pixels, clamped to the **union**
  of the outputs, so it can cross a gap between them in a single motion.
  Acceleration is whatever libinput applied. Absolute devices report in
  their own unit square and are scaled onto the first output; mapping a
  tablet or touchscreen to the right output is a settings question, and
  settings are M4.
* Every motion hit-tests the scene. The window under the pointer gets
  `PointerMotion` with **window-local** coordinates (not node-local: a
  client compares the position against the layout it sent, whatever
  transforms the intervening groups apply) and the node that was hit; a
  change of window sends `PointerLeave` to the old one and `PointerEnter`
  to the new.
* Buttons and scroll go to the window the pointer is **over**, not the
  focused one — focus follows the click, not the other way round. A left
  press raises that window (within the `Normal` layer only, so a click
  cannot pull a panel out from under a menu) and focuses it; a press on
  the desktop drops focus, which is how a client learns it stopped
  receiving keys.
* A touch sequence belongs to the window it started on: the finger may
  wander off while dragging and the client still owns the gesture until it
  lifts.

Every input event's timestamp is remembered, and handed to whichever
outputs are about to paint at the *next scene update* — which is the first
moment the damage it caused is visible to the server. That indirection
matters: a pointer move damages the cursor immediately, but a click or a
key does not. The pixels answering those are the client's, and they arrive
in a later wakeup as a commit. Stamping only what was already dirty would
quietly reduce the histogram to a cursor-motion histogram; carrying the
timestamp until a frame actually consumes it measures the thing the metric
is named after, click-to-photon included.

The carry is bounded to 200 ms. An input nothing ever responds to must not
sit waiting to be reported as a multi-second latency by some unrelated
frame later on — which is precisely what an earlier version did, once
reporting 33 seconds.

## Statistics

`stats` returns these keys, in this order. The paint and latency windows
are short on purpose: an average over an hour hides the stutter a test is
looking for.

| key                      | meaning                                                                 |
|--------------------------|-------------------------------------------------------------------------|
| `frames`                 | Page flips completed since startup.                                      |
| `flips_pending`          | Outputs with a flip in flight right now (0 when idle).                   |
| `uptime_ms`              | Milliseconds since `run()` started.                                      |
| `active`                 | 1 unless the session is paused by a VT switch.                           |
| `flip_interval_mean_us`  | Mean interval between flips, in microseconds.                            |
| `flip_interval_min_us`   | Shortest interval seen.                                                  |
| `flip_interval_max_us`   | Longest interval seen. Intervals longer than four refresh periods are **not counted**: an idle server deliberately stops flipping, and that gap is the absence of frames, not a slow one — counting it would measure how long the desktop sat still. |
| `paint_us_min`           | Rasterization time per frame, over the last 120 frames (two seconds at 60 Hz). Paint only, not the commit. |
| `paint_us_mean`          | Mean of the same window.                                                 |
| `paint_us_max`           | Max of the same window — in practice a full-screen repaint.              |
| `damage_px_mean`         | Mean damaged device pixels repainted per frame, same 120-frame window.   |
| `i2p_min_us`             | Input-to-photon latency, over the last 100 samples: from the libinput event timestamp to the vblank of the frame carrying its effect. |
| `i2p_mean_us`            | Mean of the same window.                                                 |
| `i2p_max_us`             | Max of the same window.                                                  |
| `clients`                | Connected wire clients.                                                  |
| `windows`                | Windows in the scene.                                                    |
| `nodes`                  | Nodes in the scene, across every window.                                 |

The key naming is inconsistent on purpose — `paint_us_min` but
`i2p_min_us` — because that is what the protocol spec says, and the wire
format outranks tidiness. There is no histogram and no percentile:
percentiles over 120 samples are mostly noise, and a real latency
distribution needs a real sampling story, which is a later decision.

## VT-switch contract

On `SeatEvent::Disable`, in this order: **input is suspended first**
(libinput must have let go of its device fds before the ack, or the switch
hangs), then the backend is paused, then `ack_disable()` — libseat holds
the switch until that ack. On `Enable`: `resume()` the backend (one
blocking `ALLOW_MODESET` commit, which also clears any flip that was
abandoned), resume input, **reset the xkb state** (key releases that
happened on the other VT were never seen, so the modifier state is a
guess and dropping it is the only honest option), invalidate every output
and repaint fully. Three round trips on the test box with a client
connected: clean, and input is still routed to the client afterwards.

One known wart: each round trip leaks one descriptor per input device,
because `libseat_close_device` reports success without closing the fd it
handed out. The fix belongs in `nitro-seat`, which owns that value; issue
#525 tracks it. Bounded and slow — five fds per switch against a default
limit of 1024 — but real.

## Testing

- Unit tests per module: protocol parsing and replies, the `OutputState`
  age-2 rule and deadline maths, the statistics windows, control
  buffering, cascade placement, buffer validation, pointer clamping and
  hit-testing, keyboard resolution and hotkeys, the cursor bitmap,
  logging, signals (skipped when the sandbox blocks SIGTERM).
- `tests/fake_loop.rs` runs `run(Config::fake(..))` on a thread with a
  fake input source — the real event loop, no seat and no evdev node —
  and covers fourteen things:
  1. `outputs`, exact `shot` pixels against `render::background_color`,
     `shot` by name and the error for an unknown one, `err` for a bad
     request, that an idle server stops flipping entirely, and that `quit`
     stops the thread and removes both socket files.
  2. A client window: `Configure` with the size and scale it got,
     `Presented` for its commit serial, the window's pixels where the
     cascade put it (the first at the origin, the second one
     `CASCADE_STEP` down and right), and non-zero `paint_us`/`damage_px`.
  3. Pointer motion: `PointerEnter` naming the rect node with
     window-local coordinates, a second move inside producing
     `PointerMotion` rather than another enter, `PointerLeave` on the way
     out, the software cursor visible in the screenshot, and a non-zero
     `i2p_max_us`.
  4. A left click on the lower of two overlapping windows: `Focus` and
     `PointerButton` to that client, and the overlap repainted in its
     colour because the click raised it.
  5. Disconnect: windows and nodes drop to zero and the area the client
     covered is repainted with the desktop underneath.
  6. A bad message (a node whose parent does not exist — something only
     the server's id map can catch) gets `Error` with the commit serial
     and `UnknownNode`, and the connection closes.
  7. A client buffer written to a memfd, uploaded with `CreateBuffer`,
     shown by an `Image` node, and checked pixel-for-pixel in a
     screenshot.
  8. Eight control clients sending partial lines, and two requests on one
     connection.
  9. The desktop frame still painted under everything.
  10. A client cycling 64 buffers (create, damage, destroy) leaks no
      descriptors, counted from `/proc/self/fd` — the server runs on a
      thread of the test process, so its leaks are the test's to see.
  11. A commit that changes no pixel is still `Presented`, four times in a
      row, so a client using the serial as flow control cannot stall.
  12. A `RequestFrame` from a settled, idle server is answered with a
      `Frame` carrying a usable deadline, without waiting for a flip that
      would never come.
  13. A window created before any output exists: its commit is still
      acknowledged with `Presented` (nothing it drew can reach a screen,
      so no frame will ever carry that serial), it gets no `Configure`
      while there is nowhere to put it, and it is placed and configured
      when an output is hotplugged in (`plug WxH`), appearing at the
      cascade's first position.
  14. No cursor is drawn until a pointer device reports something: a bare
      desktop is background everywhere, and the arrow appears on the first
      motion.
- Hardware: `just deploy`, `just shot`, `just box-chvt 1|2`, `just
  box-stop` (see `docs/testbox.md`), plus the demo client:
  `ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/hello_client'`,
  which opens a gradient-and-rounded-rects window with an image node and
  prints every `ServerMsg` it receives.

## Measured on the test box

Pentium G3240, i915, 1920×1080@60, with `hello_client` connected:

| what                     | value                                                     |
|--------------------------|-----------------------------------------------------------|
| idle CPU                 | 0.0 %, zero voluntary context switches over 10 s          |
| RSS                      | server 7.5 MB, `hello_client` 2.9 MB                      |
| flip interval            | mean 16 665 µs, min 16 659, max 16 673                    |
| `paint_us`               | min 50, mean 3 663, max 15 791                            |
| `damage_px_mean`         | 611 406 of 2 073 600                                      |
| input-to-photon          | min 2 223 µs, mean 8 745 µs, max 17 328 µs                |
| VT switches              | 3 round trips with a client connected: clean, input still routed |

The paint figures need reading with the workload in mind: the maximum is a
full-screen repaint, while a pointer move costs tens of microseconds, and
the damage mean is dominated by the startup full repaints rather than by
steady state. Input-to-photon averaging under one 16.7 ms frame is the
number that matters, and it is what the age-2 rule plus "paint only on
`Flipped`" is supposed to buy.
