# Input-to-photon latency

Measured on the test box (Pentium G3240, i915, HDMI-A-1 1920×1080@60,
`docs/testbox.md`) with `nitro-demo --follow`, which is the M1 exit
criterion: an input-to-photon number that was *measured*, not assumed.

**Headline: median 9.3 ms, p95 17.1 ms, min 1.3 ms, max 19.2 ms over 202
samples** — inside one refresh period, against a budget of one. The
budget is **met**.

It was not, when this document was first written. The original
measurement is kept below in full, because the diagnosis it made is the
reason the number moved and a before/after is worth more than an
after: **median 25.2 ms, p95 33.4 ms, min 17.5 ms** — one and a half
refreshes, missed by exactly one frame, with 0.3 ms of work in it. The
cause was a cursor-only flip going out before the client under the
pointer could answer (section 3.2), and the fix was to defer that flip
(issue #529, now closed). Section 4 has the before/after and the one
methodological trap that hid it.

**This dossier measures latency and nothing else.** Its companion is
[`bench.md`](bench.md), the *throughput* dossier: how many mutations per
frame the server sustains before it misses a vblank, and what a presented
frame costs in CPU on both sides of the socket — `x11perf`'s 1988
operations and the classic demo effects, ported to the wire by
`nitro-bench` and run on this same box. The two questions are
independent, and a system can pass either while failing the other: a
compositor that answers one pointer move in 9 ms may still be unable to
repaint a full screen sixty times a second, which is exactly what
`bench.md` finds.

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

and, from a second shell, 200 paced pointer moves:

```sh
ydotool mousemove -- -10000 -10000; sleep 0.3   # slam to a known corner
ydotool mousemove -- 200 150; sleep 0.5         # into the window
for i in $(seq 100); do
  ydotool mousemove -- 4 3;  sleep 0.03
  ydotool mousemove -- -4 -3; sleep 0.03
done
```

Four things about that recipe are load-bearing:

- **The corner slam.** libinput's pointer acceleration is non-linear for
  `ydotool` (`docs/testbox.md`), so a relative move of *n* does not travel
  *n* pixels. Slamming into the corner first makes the position known
  regardless; from there the moves only have to be large enough to move
  the follower, and the magnitude does not matter.
- **The pacing.** An unpaced `for` loop fires faster than the server can
  flip and measures queueing delay rather than latency. The first run of
  this experiment did exactly that and reported a mean of 25.7 ms with
  moves backlogged three deep — the same number for the wrong reason.
- **`sleep 0.03`, not `sleep 0.01`.** This changed, and it is the single
  most important line in the file. `sleep 0.01` plus `ydotool`'s own
  overhead comes out at ~65 moves/s, and each move costs **two** flips
  under the age-2 cursor rule, so it demands ~130 flips/s from a display
  that can retire 60. The server pins at exactly 60.0 flips/s and the
  number stops being a latency at all. Measured, at that pacing:

  | pacing | build | flips/s | client median |
  |---|---|---|---|
  | `sleep 0.01` (65 moves/s) | before the fix | 60.0 | 25.0 ms |
  | `sleep 0.01` (65 moves/s) | after the fix | 59.9 | 24.9 ms |
  | `sleep 0.03` (28 moves/s) | before the fix | 60.0 | 25.1 ms |
  | `sleep 0.03` (28 moves/s) | after the fix | 56.1 | **9.3 ms** |

  Both builds are saturated on the first two rows, and they agree there
  — which is the proof that the row is measuring the queue and not the
  pipeline. The original headline in this document was taken at
  `sleep 0.01`, and it was only luck that the mechanism it diagnosed was
  real: the number it quoted was one a saturated display would have
  produced anyway. **A run whose `flips/s` is 60.0 is measuring the
  display, not the compositor.** `nitro-shot --stats` reports `frames`;
  divide by the wall time of the loop.
- **Moves must land inside a window.** A pointer over the desktop damages
  only the cursor, so the *server* records i2p samples and the *client*
  records none. A run that reports `i2p[5s] no samples yet` next to a
  healthy `server i2p:` line is measuring nothing: the pointer missed.

## 3. The numbers, as they were before the fix

Everything in this section is the **original** measurement, kept because
its diagnosis is what section 4 acts on. The after-numbers are in
section 4.

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

### 3.2 Why the floor is a whole frame

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

This count is the load-bearing evidence, and unlike the headline number it
could not have been produced by a saturated display: the moves are one per
second. It is also the assertion the fake-backend integration test now
makes — 4 answered motions cost 8 flips with the deferral and 12 without,
i.e. exactly this 2-vs-3 — so the regression is caught in CI rather than
only on the box.

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

And that offset is what section 4 closes: with the flip deferred, the
client's median follows the server's down to ~9 ms at every one of these
rates. (The `~50 Hz` row is above the saturation threshold of section
4.4 — 100 moves/s of demand against 60 flips/s — which is why its client
figure does not move even after the fix.)

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
| idle, 5 s *after* a deferred flip | **0.0 %** (0 frames, `voluntary_ctxt_switches` flat) | **0.0 %** |
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

## 4. Verdict against the budget, and the fix

`DESIGN.md` sets "input → photon within one refresh at 60 Hz". The
measured median is **9.3 ms, or 0.56 refreshes. The budget is met.**

It was 25.2 ms when section 3 was written. What changed is one scheduling
decision, described below; nothing was made faster, and the 0.3 ms of
work per frame is the same 0.3 ms.

### 4.1 Before and after

Both runs: 202 samples, one window, `nitro-demo --follow --stats`, pointer
paced at 28 moves/s (section 2 explains why not faster). `sha256sum` of
both binaries verified immediately before *and* after each run, in the
same ssh session. The `nitro-demo` binary is byte-identical across the
two — same instrument, one variable.

| view | | min | median | p95 | max | mean |
|---|---|---|---|---|---|---|
| **client (input → photon)** | before | 17 502 | **25 132** | 33 119 | 34 060 | 25 466 |
| | after | 1 287 | **9 290** | 17 134 | 19 216 | 9 594 |
| server (`i2p_*`) | before | 11 869 | — | — | 31 849 | 23 057 |
| | after | 1 287 | — | — | 17 788 | 9 459 |

All microseconds. The distribution moved down by very close to one whole
refresh period at every point of it — median −15.8 ms, p95 −16.0 ms,
min −16.2 ms against a 16.67 ms frame — which is the signature of a
structural flip being removed rather than of anything getting faster. The
**minimum is the tell**: it was 17.5 ms, a whole frame, which no fast path
produces by accident; it is now 1.3 ms.

The two views also agree now to 0.2 ms rather than differing by a frame,
because the thing that separated them was exactly the extra flip.

### 4.2 The change

**Do not start a flip for cursor-only damage while a client has been told
about the input and has not yet answered.** The client's commit takes
0.12 ms to arrive and comes back within one wakeup, so cursor and content
ride the *same* flip instead of consecutive ones.

The opposite failure — a client that never answers stalling the cursor —
is bounded by a deadline, `frame::frame_deadline`, the same next-vblank
minus margin a frame callback is given. That is implemented as one
`CLOCK_MONOTONIC` timerfd in the epoll set, armed only while a flip is
actually held. The full rule, and what each of its four conditions is
protecting, is in `crates/nitro-server/README.md` under "Deferring a
cursor-only flip"; the mechanism is `crates/nitro-server/src/defer.rs`.

Two `stats` keys report it: `flips_deferred` and `defer_timeouts`.

### 4.3 The other three properties, re-measured

The fix must not have bought latency with anything else, so:

| property | measured |
|---|---|
| **Cursor over the bare desktop** | 16 flips for 8 isolated moves = **2 per move, unchanged**, and `flips_deferred` 0. Nobody to wait for, so the fast path is not even entered. |
| **A wedged client** (`nitro-demo` under `SIGSTOP`, hovered) | cursor keeps moving at **60.0 flips/s** — 183 flips in 3.05 s — and `defer_timeouts` climbs 0 → 96. The deadline is doing exactly its job. |
| **Idle** | 5 s after a deferral: **0 frames, 0 CPU ticks of 500, `voluntary_ctxt_switches` 205 → 205**. The timer is disarmed when nothing is held, so "idle is zero wakeups" survives adding a timer to the loop. |

### 4.4 The rate sweep, and the trap in the old recipe

The fix is invisible at the pacing the original headline used, and that is
a fact about the *measurement*, not the fix. Each pointer move costs two
flips (the age-2 cursor rule), so *m* moves/s demands 2*m* flips/s from a
display that retires 60.

| gap | moves/s | flips demanded | before | after | deferred / timed out |
|---|---|---|---|---|---|
| 0.01 | 65 | 130 | 25.0 ms | 24.9 ms | 3 / 0 |
| 0.02 | 41 | 82 | 25.6 ms | 24.7 ms | 5 / 0 |
| 0.03 | 28 | 56 | 25.1 ms | **9.3 ms** | 202 / 3 |
| 0.04 | 21 | 42 | — | **8.7 ms** | 82 / 0 |
| 0.05 | 17 | 34 | 26.8 ms | **9.6 ms** | 124 / 0 |
| 0.10 | 10 | 20 | 25.6 ms | **9.8 ms** | 86 / 0 |

The threshold is sharp and it is where the arithmetic says it should be:
between 41 and 28 moves/s, i.e. where demand crosses 60 flips/s. Above it
the server is pinned at the hardware cap — measured 60.0 flips/s before
and 59.9 after — every input arrives with a flip in flight, and the
deferral has nothing to defer (3 episodes in a whole run, against 202
below the threshold). The number that comes out is queue depth.

The control that makes this airtight is that **both builds report the same
~25 ms there**. If the old recipe had been measuring the pipeline, the
fixed build would have moved; it does not, because at 65 moves/s neither
build is measuring the pipeline.

Which means the original section 3 headline was, strictly, a saturated
number that happened to agree with the unsaturated one — the extra flip
was real, and section 3.2's flip count proved it directly, but the 25.2 ms
figure could not have distinguished the two causes. The rate experiment in
section 3 was already pointing at this: it showed the *server's* i2p
dropping to [1.2, 17.6] ms as the rate fell while the *client's* stayed at
26 ms. That gap was the extra flip, and it is now closed at every rate the
display can service.

Three further things, unchanged and re-confirmed:

- **Idle really is zero.** Not "low" — zero frames in five seconds with a
  client connected and visible, and now also with a timerfd in the loop.
- **Frame pacing is exact.** 16 666 µs mean, min 16 654, max 16 675.
- **The two views of latency agree**, now to 0.2 ms.

## 4.5 Where the paint time went: the heap shadow buffer (#539)

Everything above is about *scheduling* — which vblank a frame catches.
This section is about the other half, the work inside the frame, and it
is the larger number of the two: until #539 the server spent **6.2 ms**
rasterizing a frame on the box, i.e. more than a third of a refresh
period, against a scene that costs 0.4 ms of actual arithmetic.

The diagnosis came out of #3693's A/B rounds and is stated in full in
`crates/nitro-raster/README.md`: the same server, same scene, same
damage, painting into a DRM dumb buffer took 5883 µs and into heap memory
758 µs. **~87 % of `paint_us` was framebuffer traffic, not raster work.**
The mechanism is that a dumb buffer is mapped write-combined — writes are
cheap and coalesced, reads are uncached — and source-over is
read-modify-write: the rasterizer reads every destination pixel it
blends.

The fix is the one every CPU compositor arrives at. Each output owns a
heap-resident **shadow buffer** of the same format and stride as the
scanout buffer; the rasterizer paints into that, and the damage rects are
then streamed into the dumb buffer with sequential, write-only
`copy_from_slice` row copies. Nothing reads write-combined memory any
more. `frame.rs`'s module documentation has the design; the one
non-obvious consequence is that the age-2 union moves *off* the paint — a
shadow is never stale, so the rasterizer is given `damage(n)` alone and
only the copy needs `damage(n) ∪ damage(n-1)`.

### Measured

Same binary both ways, `NITRO_SHADOW=0` against the default, through a
systemd drop-in; #3693's protocol (30 `hello_client` cycles filling the
120-frame window), six interleaved pairs with the order flipped for pairs
4–6, every run valid at `frames=122 ∧ damage_px_mean=275418`.

| per frame | `NITRO_SHADOW=0` | shadow (default) |
|---|---|---|
| `paint_us_mean` | 6233 µs | **418 µs** |
| `paint_us_min` | 119 µs | 0 µs |
| `paint_us_max` | 15 815 µs | 4349 µs |
| `copy_us_mean` | — (no copy) | 251 µs |
| **paint + copy, mean** | **6233 µs** | **669 µs** |

**9.3× on the whole frame path.** The paired difference on `paint_us` is
5815 µs with a standard deviation of 73 µs across the six pairs
(t(5) = 196): this is not a measurement that needs statistics, it needs
reporting. The two halves are counted separately on purpose —
`paint_us` is now CPU and cached memory, `copy_us` is the write-combined
mapping — because they respond to completely different changes, and
folding them back together would re-hide exactly what #3693 spent a round
digging out.

Note `paint_us_min` = 0: with the shadow the age-2 carry frame
rasterizes *nothing at all*. It has no new damage, the shadow already
holds the previous frame's, and all that is left is the copy. Before,
every such frame repainted the union.

### What it does not buy

**Input-to-photon is unchanged in the middle.** Three pairs of 60 paced
`nitro-calc` keypresses, the server's own `i2p_*` window:

| | shadow | `NITRO_SHADOW=0` |
|---|---|---|
| `i2p_mean_us` | 12 854 / 12 412 / 13 073 | 12 022 / 13 936 / 13 452 |
| `i2p_max_us` | 21 375 / 22 450 / 22 398 | 36 717 / 39 163 / 22 214 |

That is the right answer and worth stating plainly: **latency is set by
which vblank you catch, not by how much of the interval you use.** 6.2 ms
of paint fits inside a 16.7 ms refresh, so removing 5.5 ms of it does not
move a median that is quantised to the refresh in the first place. What
it moves is the *tail* — two of the three `NITRO_SHADOW=0` runs show a
max near 37–39 ms, a missed frame, and none of the shadow runs does.
That is what 5.5 ms of headroom is for: the frames that used to be one
slow client or one scheduler hiccup away from missing their vblank now
have a whole refresh period of margin instead of two thirds of one.

The honest summary is that #539 buys **margin, not median**. The median
was already inside budget (section 4.1); the margin is what keeps it
there when the scene gets more expensive, which is the direction every
remaining milestone points.

**Idle is still exactly zero** — 0 frames and 0 CPU ticks over 5 s with
two decorated windows, both variants. The shadow is touched only on a
frame, so a server with nothing to do does nothing with it either.

**It costs 8 MB per output**, resident: 7.7 MB RSS → 15.9 MB on the box,
which is `1920 × 1080 × 4` to the byte. `docs/budget.md` records it as
the deliberate trade it is.

## 5. Remote: the same measurement over TCP (M4-E1)

An app on the dev box, its window on the test box, over a 1 Gb LAN
(`docs/remote.md` has the model and the security model). The question
this section answers is narrow: **what does putting the wire on TCP cost
the input-to-photon figure?**

The method is section 2's, unchanged — `nitro-demo --follow`, 120 real
`ydotool` pointer moves on the box, the client's own `i2p[total]` line
— with `NITRO_SOCKET=tcp://192.168.1.204:7700` and
`remote.listen = 0.0.0.0:7700` on the server. The local baseline was
taken **the same evening, on the same box**, because this box's numbers
drift day to day.

| | i2p median | p95 |
|---|---|---|
| **local** (box, same evening) | **8 664 µs** | 17 272 |
| **remote**, `TCP_NODELAY` on, five runs | 9 232 / 9 496 / 9 802 / 10 205 / 10 629 µs | 15.6–18.8 ms |
| **remote**, Nagle on, four runs | 10 253 / 10 300 / 10 790 / 11 424 µs | 15.6–17.6 ms |

So a remote window costs roughly **1–2 ms of median latency**, on a
figure whose budget is one 16.7 ms refresh period. It is still inside
the budget, which is the headline: remote apps are not a degraded mode.

### The `TCP_NODELAY` A/B is a negative result

The option is a latency claim, and a latency claim with no measurement
behind it is a comment. `NITRO_TCP_NODELAY=0` turns it off so the A/B is
the **same binary, same window, same machine pair, one sockopt flipped**
— not "TCP vs Unix", which differs in far more than one option, and this
is the shape of control section 4's history argues for.

**Five interleaved pairs, mixed signs.** The first pair had Nagle
*faster*. Reported as "not resolvable in this workload", and the
mechanism says why it never could be: `--follow` commits about 4.7 times
a second, so every write is alone on the wire with nothing to coalesce
and the previous one long since acknowledged. Nagle holds a *second*
small write pending an ACK; a protocol that sends one and waits never
meets it.

The option stays set. It costs nothing, and the one burst in the demo's
life — the handshake and first transaction, which *are* back-to-back
small writes — does show it:

| | connect → first `Presented` |
|---|---|
| `TCP_NODELAY` on | **8.7, 9.2, 12.3, 21.6, 31.3 ms** |
| Nagle on | 18.8, 19.0, 20.1, 25.7, 31.3 ms |

Same direction in four of five pairs.

### Two cautions specific to a remote run

- **The `delivery` line is meaningless across machines.** It is the one
  figure taken against the *client's* own clock (section 1), so over a
  remote link it carries the full `CLOCK_MONOTONIC` skew between two
  machines — it reported `median=1496007485384 µs`, i.e. 17 days. The
  headline `i2p` figure is immune, because both its endpoints are
  stamped by the server. That is not a new caveat: `App::delivery`'s own
  doc comment predicted it, and this is the run that made it true.
- **Do not compare `seq 1 1000000` wall times.** The VT parsing happens
  on whichever machine the *app* runs on, so remote (0.35 s, 128-core
  dev box) against local (0.55 s, Pentium G3240) measures the two CPUs
  and says nothing about the link. The figure that *is* about the link
  is bytes on the wire: **43 185 B for 6 888 896 B of terminal output,
  0.63 %**, taken from the server socket's own `ss -ti` counters either
  side of the run.

## 6. Reproducing

```sh
just deploy                                   # includes nitro-demo
ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-demo --follow --stats --seconds 22'
# in another shell: the ydotool loop from section 2 (mind the pacing)
just size                                     # sizes and RSS (docs/budget.md)
```

`nitro-demo --help` lists the modes. `--stats` adds the server's own view
and the cross-check; `--damage` outlines the damage rects; `--save-small
FILE` writes the downscaled PNG used above, with no ImageMagick needed on
the box.

Three cautions learned here, each of which produced a wrong number first:

- **Check which binary you measured.** Another task deploys to this box
  too. The headline run above verifies `sha256sum` of `nitro-server` and
  `nitro-demo` immediately before *and* after the measurement, in the same
  session; an earlier run of these numbers was taken minutes before the
  binary was replaced underneath it. For a before/after, check that the
  *demo* binary is byte-identical across the pair as well: it is the
  instrument, and a differing one makes the comparison meaningless.
- **Check the client got samples.** `i2p[total] no samples` next to a
  populated `server i2p:` means the pointer never entered a window and the
  run measured the cursor, not the pipeline.
- **Check the display was not saturated** (section 2, and section 4.4 for
  what it costs). Divide `frames` from `nitro-shot --stats` by the wall
  time of the input loop: 60.0 flips/s on a 60 Hz panel means the queue
  was full and the run measured backlog, not latency. This one is the
  worst of the three, because it produces a plausible number that is
  stable across runs and even across builds — it hid a 16 ms improvement
  completely.

A before/after wants both builds measured in the same sitting, alternating
if possible. The box is shared and its thermal and scheduler state drift;
two runs a day apart are not a controlled comparison.

For a **remote** run (section 5), the client is on the other machine and
the `NITRO_SOCKET` goes with it:

```sh
box$  echo 'remote.listen = 127.0.0.1:7700' >> ~/.config/nitro/server.conf
dev$  ssh -L 7700:127.0.0.1:7700 box -N &
dev$  NITRO_SOCKET=tcp://127.0.0.1:7700 ./target/release/nitro-demo --follow --seconds 26
# and the ydotool loop on the *box*, as ever: the input is the box's
NITRO_TCP_NODELAY=0 ...   # the same run with Nagle, for the A/B
```

`--stats` is **not** useful remotely: it reads the server's control
socket, which is a Unix socket on the other machine. Read the server's
own figures there instead, with `nitro-shot --stats` on the box.
