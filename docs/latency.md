# Input-to-photon latency

Measured on the test box (Pentium G3240, i915, HDMI-A-1 1920×1080@60,
`docs/testbox.md`) with `nitro-demo --follow`, which is the M1 exit
criterion: an input-to-photon number that was *measured*, not assumed.

**Headline: median 25.2 ms, p95 33.4 ms, min 17.5 ms, max 34.2 ms over 202
samples** — between one and two refresh periods, against a budget of one.
The budget is **missed, by one frame**, and the cause is understood,
localised to four lines of the server, and not a performance problem: the
machine spends 0.3 ms of the 25 working. Section 4 has the verdict and
section 3 the diagnosis.

## 1. What the numbers mean

Two independent views of the same interval, deliberately, because a
measurement that only the server takes is the server marking its own
homework.

**The server's view** (`nitro-shot --stats`, keys `i2p_*`): from the
libinput event timestamp to the vblank of the frame that consumed it.
Everything between is the server's own work — routing, scene update,
raster, page flip. The client is nowhere in it.

**The client's view** (`nitro-demo`, the `i2p[...]` lines): from the same
libinput timestamp, carried to the client on `PointerMotion.time_ns`, to
`Presented.time_ns` for the commit that answered that motion. It therefore
spans the whole round trip — socket out, the client's own reaction, socket
back, the server's read of the transaction, the compositing pass, the
flip.

Both endpoints are stamped by the **server**, from `CLOCK_MONOTONIC`, so
the subtraction is immune to any clock skew between the two processes and
the client never has to trust its own clock for the headline figure. The
one exception is the `delivery` line — libinput timestamp to the moment
the client's `poll` returned — which is the client's own clock and is
reported separately for exactly that reason.

The client's figure is the honest one to quote: it is what a user's finger
actually waits for. It is also necessarily the larger of the two, and the
gap between them is the point of taking both.

### What is *not* in either number

- **USB polling.** A 125 Hz mouse adds up to 8 ms before the kernel has an
  event at all. libinput's timestamp is taken when the kernel reads the
  report, so everything upstream of that is invisible here.
- **Panel latency.** The flip completes when the scanout engine has the
  buffer; the display's own processing and pixel response are after that,
  and are typically another 5–20 ms on a cheap HDMI monitor.
- **The compositor's own cursor**, in the client's view: the server draws
  the hardware-less software cursor itself, and that happens before the
  client hears anything. It is in the server's number.

So the real finger-to-photon figure is this plus roughly 10–25 ms of
hardware neither process can see or control. That is normal, and it is why
the metric is defined at the timestamps it is defined at.

## 2. Method

`just deploy` (which now carries `nitro-demo`), then on the box:

```sh
XDG_RUNTIME_DIR=/run/user/1000 nitro-demo --follow --stats --seconds 22
```

and, from a second shell, 200 paced pointer moves at ~100 Hz:

```sh
ydotool mousemove -- -10000 -10000; sleep 0.2   # slam to a known corner
ydotool mousemove -- 200 150; sleep 0.4         # into the window
for i in $(seq 100); do
  ydotool mousemove -- 4 3;  sleep 0.01
  ydotool mousemove -- -4 -3; sleep 0.01
done
```

Three things about that recipe are load-bearing:

- **The corner slam.** libinput's pointer acceleration is non-linear for
  `ydotool` (`docs/testbox.md`), so a relative move of *n* does not travel
  *n* pixels. Slamming into the corner first makes the position known
  regardless; from there the moves only have to be large enough to move
  the follower, and the magnitude does not matter.
- **The pacing.** An unpaced `for` loop fires faster than the server can
  flip and measures queueing delay rather than latency. The first run of
  this experiment did exactly that and reported a mean of 25.7 ms with
  moves backlogged three deep — the same number for the wrong reason.
  `sleep 0.01` keeps every input independent.
- **Moves must land inside a window.** A pointer over the desktop damages
  only the cursor, so the *server* records i2p samples and the *client*
  records none. A run that reports `i2p[5s] no samples yet` next to a
  healthy `server i2p:` line is measuring nothing: the pointer missed.

## 3. The numbers

### `--follow`, 202 samples, one window

| view | count | min | median | p95 | max | mean |
|---|---|---|---|---|---|---|
| **client (input → photon)** | 202 | 17 499 | **25 213** | 33 394 | 34 193 | 25 355 |
| server (`i2p_*`, 100-sample window) | — | 17 421 | — | — | 32 035 | 24 165 |
| delivery (libinput → client) | 201 | 58 | 116 | 308 | 940 | 138 |

All microseconds. The two views agree to 1.2 ms, well inside the one-frame
tolerance the demo cross-checks against — which is the check that says
neither is lying.

Re-run after `nitro-text` (M2-pre) landed and this branch was rebased onto
it, since that work changed the server's paint path: median 25 430 µs, p95
32 880 µs, mean 25 601 µs over 202 samples, with `shape_us_mean 0` (the
demo draws no text). Within noise of the table above, so the conclusion
below is unaffected by it.

The distribution is the diagnosis. Latency is spread almost uniformly over
**[17.4 ms, 34.2 ms]** — that is exactly [1 frame, 2 frames] at 16.67 ms,
and a *floor* of one full frame is not something a fast path produces by
accident.

Meanwhile the work itself is nothing:

| stage | time |
|---|---|
| delivery, libinput → client (median) | 0.12 ms |
| server paint per frame (mean / max) | 0.19 ms / 0.32 ms |
| damage per frame | 1 560 px of 2 073 600 |

0.3 ms of work inside a 25 ms interval. **~98.8 % of the latency is
waiting for a vblank, not computing.** No amount of making the rasterizer
faster will move this number.

### Why the floor is a whole frame

The server paints only when a flip completes (`Server::paint` returns
early while `backend.flip_pending(id)`), and a pointer move damages the
**cursor** immediately, before the client has heard anything. So an
isolated move goes:

1. Motion arrives. The cursor moved, so there is damage *now*: the server
   paints a cursor-only frame and flips it. The client's `PointerMotion`
   goes out on the same wakeup.
2. The client answers with a commit — but a flip is already in flight, so
   the server cannot paint it. It waits.
3. That flip completes; now the client's content is painted and flipped.

Two flips for the client's pixels, and the second one is what `Presented`
reports. A direct count confirms it, one isolated move per second so
nothing is queued:

| | flips per isolated pointer move |
|---|---|
| cursor only, no client connected | **2** |
| with `nitro-demo --follow` connected | **3** |

(Two rather than one even for the bare cursor because of the age-2 rule:
the old and new cursor rects are damaged, and the back buffer is two
frames stale, so the region is repainted into both buffers.)

The rate experiment is the control. Same 200 moves, only the spacing
changes:

| input rate | server i2p (min–mean–max) | client i2p (median) |
|---|---|---|
| ~50 Hz | 17.5 / 24.4 / 31.8 ms | 26.0 ms |
| ~10 Hz | 1.2 / 9.3 / 17.6 ms | 26.7 ms |
| ~2 Hz | 1.2 / 9.3 / 17.7 ms | 26.6 ms |

The server's own number drops to **[0, 1] frame** as soon as inputs stop
colliding with in-flight flips — its fast path is genuinely fast, and
this is the proof. The client's stays at ~26 ms regardless. That constant
one-frame offset between the two, invariant under input rate, is the
signature of a structural extra flip rather than of load, queueing or
backpressure.

### `--animate`, 12 s

One commit per `Frame` callback, which is the contract present-time
scheduling is supposed to enforce:

| | |
|---|---|
| commits / frame callbacks / presented | 722 / 719 / 720 over 12.0 s |
| rate | 60.2 commits/s, 60.0 presented/s |
| flip interval | mean 16 666 µs (min 16 652, max 16 681) |
| paint | mean 59 µs |
| damage | 1 917 px/frame |
| CPU | server 1.5 %, client 0.3 % |

Commits, callbacks and flips all track 60 Hz and never diverge: the demo
never commits twice for one flip. The three-message spread between the
counters is structural, not drift — the build transaction is a commit no
callback asked for, and there is always exactly one `RequestFrame` in
flight. `nitro-demo` measures pacing on the *increments* between marks for
that reason, and warns only if commits outrun callbacks in an interval.

### CPU and memory

| state | server CPU | client CPU |
|---|---|---|
| idle, no client | **0.0 %** | — |
| idle, demo connected, no input | **0.0 %** (0 frames in 5 s) | **0.0 %** |
| `--animate` (60 Hz) | 1.5 % | 0.3 % |
| `--follow` under 100 Hz input | 2.2 % | 0.5 % |

The idle row is the one that matters and it is exact: with a client
connected and its window on screen, five seconds pass and the frame
counter does not move. `--follow` commits only in response to input, so a
demo nobody is touching costs literally nothing — the client-side half of
the property the whole design is built around.

Memory, 1920×1080 on the box:

| | VmRSS | VmHWM |
|---|---|---|
| `nitro-server`, 1 window | 7 468 kB | 7 468 kB |
| `nitro-server`, 5 windows | 7 592 kB | 7 592 kB |
| `nitro-demo`, 1 window | 3 168 kB | 3 164 kB |

Five windows cost the server 124 kB over one. Full budget table in
`docs/budget.md`.

### Five windows, damage

With `--windows 5` overlapping and the pointer moving in the topmost,
damage stays bounded — the point of the exercise:

| | 1 window | 5 windows |
|---|---|---|
| `damage_px_mean` (steady state) | 1 560 | 1 560 |
| `paint_us_mean` | 189 | 294 |
| nodes in the scene | 18 | 90 |

Steady-state damage does not grow with window count, because a pointer
move damages two follower rects and nothing else regardless of how many
windows are stacked behind. `paint_us_mean` rises by ~100 µs from walking
five windows' paint lists rather than one. The `paint_us_max` of 64 ms at
five windows is the startup full-screen repaint of five 800×500 windows
with gradients, not a steady-state frame.

![nitro-demo with damage outlines](demo-damage.png)

`nitro-demo --follow --damage` on the box, downscaled 2× by the demo's own
`--save-small`. The red outlines are the two rectangles the client told the
server it damaged: the one the follower left and the one it arrived at.
They are the *client's* claim, drawn from the numbers it sent, so a
mismatch between them and what the server actually repainted would show up
as a smear outside an outline.

## 4. Verdict against the budget

`DESIGN.md` sets "input → photon within one refresh at 60 Hz". The
measured median is **25.2 ms, or 1.5 refreshes. The budget is missed by
one frame.**

It is worth being precise about what is and is not wrong, because the two
halves point in opposite directions:

- **The compositor meets its own budget.** The server's i2p drops to
  [1.2, 17.6] ms — inside one refresh — the moment inputs stop colliding
  with in-flight flips. Paint is 0.19 ms on a 2014 Pentium with no AVX2
  and damage is 1 560 px. Nothing here is slow.
- **The pipeline does not**, because a client's response is structurally
  one flip behind the cursor that provoked it. The server paints the
  cursor as soon as the pointer moves, and that flip blocks the frame the
  client's answer wants.

So this is a scheduling bug, not a performance one, and the fix is a
scheduling change: **do not start a flip for cursor-only damage while a
client has been told about the input and has not yet answered.** Waiting
the few hundred microseconds for the client's commit (it takes 0.12 ms to
reach it and it answers within one wakeup) would let cursor and content
ride the *same* flip and put the median at roughly 9 ms, comfortably
inside one refresh. The risk is the opposite failure — a client that never
answers must not stall the cursor — so it needs a deadline, which is
precisely what `frame::frame_deadline` already computes.

That is a change to the server's frame scheduler, not to the demo, and it
wants its own task and its own before/after numbers from this same
harness. M1's deliverable is the measurement and the diagnosis; the fix
belongs to whoever owns the scheduler next. Filed as **issue #529** rather
than smuggled into this task.

Three further things this run establishes, none of which were in doubt but
all of which are now measured rather than assumed:

- **Idle really is zero.** Not "low" — zero frames in five seconds with a
  client connected and visible.
- **Frame pacing is exact.** 16 666 µs mean over 719 callbacks, one commit
  each, no double-commits.
- **The two views of latency agree**, to 1.2 ms. Neither instrument is
  lying, which is what makes the diagnosis above trustworthy.

## 5. Reproducing

```sh
just deploy                                   # includes nitro-demo
ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-demo --follow --stats --seconds 22'
# in another shell: the ydotool loop from section 2
just size                                     # sizes and RSS (docs/budget.md)
```

`nitro-demo --help` lists the modes. `--stats` adds the server's own view
and the cross-check; `--damage` outlines the damage rects; `--save-small
FILE` writes the downscaled PNG used above, with no ImageMagick needed on
the box.

Two cautions learned here, both of which produced a wrong number first:

- **Check which binary you measured.** Another task deploys to this box
  too. The headline run above verifies `sha256sum` of `nitro-server` and
  `nitro-demo` immediately before *and* after the measurement, in the same
  session; an earlier run of these numbers was taken minutes before the
  binary was replaced underneath it.
- **Check the client got samples.** `i2p[total] no samples` next to a
  populated `server i2p:` means the pointer never entered a window and the
  run measured the cursor, not the pipeline.
