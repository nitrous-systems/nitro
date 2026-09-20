# Throughput: x11perf and the demos, on the nitro wire

This is the throughput dossier. `docs/latency.md` is the other one: it
measures how long a pointer move takes to become a photon, which is the
number a desktop is judged by and the M1 exit criterion. It says nothing
about **how much drawing per frame the server sustains, and what a frame
costs** — and a compositor can meet a 16.7 ms latency budget while
burning a whole core to do it.

`crates/nitro-bench` is the second instrument. It ports the oldest
benchmarks in the field — `x11perf`'s operation micro-benchmarks and six
demoscene effects — to a retained scene graph, runs them on the test box
with the real desktop up, and writes one JSON object per run to a ledger.
**Four ledgers are checked in, and which one a number came from decides
what it means:**

| ledger | runs | what it is |
|---|---|---|
| `docs/bench-72ef50b.jsonl` | 54 | The first sitting, 60 Hz only. Kept for history. |
| **`docs/bench-1f35491.jsonl`** | **140** | sha `1f35491`, 2026-09-16, one sitting **swept over three refresh rates** — 1920×1080@60, 1920×1080@120 and 1280×720@240 (§9). **§6–§9 are generated from and argue from this ledger**, and it is the only one with a 120 Hz arm. It is **pre-#3728**, so its fullscreen paint figures are superseded by §7.10b — they are left as measured rather than rewritten. |
| `docs/bench-49d023b.jsonl` | 16 | The #569 pair (§7.10a): `28b6fdd` → `49d023b`, both arms in one sitting, fullscreen 1080p@60 only. |
| **`docs/bench-5e4b02e.jsonl`** | **86** | sha `5e4b02e`, 2026-09-20, **post-#3728** — two arms, 1920×1080@60 and 1280×720@240, **no 120 Hz arm**. The current fullscreen numbers (§7.10b), and what closed #568. |

Everything in §6–§9 below is from `1f35491` unless a section says
otherwise; §7.10a and §7.10b say otherwise and name their own ledgers.

**Headline: the same bouncing ball costs 20 833 µs of client CPU per
frame as a fullscreen pixel buffer and 111 µs as a moved sprite node,
and only the second one holds 60 Hz.** That is `DESIGN.md`'s first goal —
"work is proportional to what changed, never to what is on screen" —
reduced to a pair of numbers taken in the same minute on the same
machine. The rest of this document is the other fourteen scenarios, the
arithmetic that says which of them are bandwidth-bound, and a careful
account of what none of it shows.

The box is a Pentium G3240: Haswell, **2 cores, SSE4.2, no AVX2**, 3.3 GB
and no swap, Intel HD HSW GT1 on `i915`, HDMI-A-1 at 1920×1080@60
(`docs/testbox.md`). Deliberately weak: if it is snappy here it is snappy
anywhere. Every run in the ledger had the real desktop up —
`nitro-session` with the server, wallpaper, bar and launcher — because
that is the realistic baseline and a benchmark against a bare server
would be measuring a machine nobody has.

## 1. What each benchmark is

These names are not decoration. Each of them is the oldest, most quoted
statement of a particular kind of load, and using the original names is
what lets a reader who has never seen nitro know what was measured.

| benchmark | origin | what it loads |
|---|---|---|
| **x11perf** | Joel McCormack and Keith Packard, DEC/MIT, **1988** | the X11 server benchmark. `-rect100`, `-ftext`, `-putimage500`, `-scroll500`, `-create` are still the vocabulary people use for 2D performance, thirty-eight years on |
| **Boing ball** | Amiga, **1984**, Dale Luck and R.J. Mical, CES | the demo that sold the Amiga. Note what it actually was: not a redraw but a *bob* — a sprite moved with a cycling palette. That is exactly the point this benchmark makes, and §7.7 makes it with numbers |
| **plasma** | demoscene staple | sine-sum over every pixel: a full-surface rewrite, the worst case for a shared-memory upload |
| **fire** | the **PlayStation** Doom, **1997** | cellular fire, a full rewrite with a serial dependency between rows |
| **rotozoom** | Amiga/PC demoscene | texture rotate + zoom: a full rewrite with a per-pixel gather |
| **starfield** | everywhere, always | N small moving things on a black field — sparse load |
| **bouncing balls** | every 90s toolkit demo | a handful of moving, rounded, coloured boxes: the shape a real UI animation actually takes |

The three sparse effects — boing, starfield, balls — are implemented
**twice**: once as a pixel buffer covering the window, re-uploaded every
frame (`crates/nitro-bench/src/pixels.rs`) and once as scene-graph
mutations (`nodes.rs`), driving the *same* simulation out of
`effects.rs`, and each pair is run at **both** 640×480 and fullscreen.
The ball is in the same place on the same frame in both arms, so any
difference between them is a property of the path and not of the
benchmark.

`nitro-bench list` prints the whole set with the x11perf operation each
one ports.

## 2. Why the port cannot be literal

This is the central argument of the crate and every number below
inherits it.

x11perf works by issuing **one immediate-mode operation in a tight loop**
and reporting operations per second. `XFillRectangle` draws *now*, so
drawing it a million times is a meaningful thing to time. Nitro has no
such operation: a client mutates a retained tree and the server paints
once per vblank. Send a million `SetFill`s and the server will coalesce
them into a single frame — correctly, by design, because that is what
"work proportional to what changed" means when what changed is the same
node a million times. The resulting "operations per second" would be a
measurement of the socket and the client's own `write` loop: a large
number, reproducible, and about nothing.

The honest transposition is **mutations per frame sustained at the
output's refresh rate**. Each scenario has a sweep parameter N; the
question is how large N can get before the server misses a vblank or the
presented rate falls off the refresh. That preserves what x11perf was
actually asking — *how much 2D work per unit time* — while being a
question this architecture can answer. §7.1 gives the answer for rects,
which is the closest thing in this document to a classical x11perf
number: **at least 2000 rect mutations per frame at 60 Hz**, which is as
far as the sweep goes before it runs out of *damage* rather than out of
mutations.

There is a second problem, and it is the reason for the headline column.
A 60 Hz cap makes frames-per-second stop discriminating the moment the
client is fast enough: a scenario that could do 1000 frames a second and
one that could do 61 both present 60, and on a retained scene graph most
scenarios are in that regime. What does *not* saturate is **how much CPU
was burned to put those 60 frames on the glass** — 300 µs and 9 000 µs a
frame look identical on the display and are a factor of thirty apart in
battery, heat and headroom. So the headline column of every table is
**microseconds of CPU per presented frame**, from `/proc/<pid>/stat`
`utime+stime` deltas of *both* the server and the client
(`crates/nitro-bench/src/cpu.rs`). Both, because a cheap server that made
the client expensive has not made the system faster — §7.4 and §7.9 are
exactly those cases. The denominator is `presented`, not commits: work
thrown away before it was seen should not be amortised over frames that
were.

One consequence of reading `/proc` deserves stating: the kernel reports
CPU in ticks, and on this box a tick is 10 ms, so every CPU figure in the
ledger is a multiple of 10 000 µs. Over an eight-second run that is about
±10 µs per frame — invisible on the large numbers, and the reason the
small ones (`text-static`'s 62.5 µs/frame, the node arms' 83.5) should be
read as "under a hundred microseconds" rather than as three significant
figures.

## 3. The operations nitro does not have

A real finding, and the kind a benchmark suite usually hides by
substituting something. **`-line`, `-circle`, `-ellipse`, `-poly`: there
is no line, arc or polygon operation on the nitro wire at all.** The
primitive set is `Rect` / `Text` / `Image` / `Icon` plus groups, and that
is the whole of it. This crate does not implement those scenarios, and it
specifically does not fake a line with a thin rotated rect and report a
number for it, because the number would answer a question nobody asked.

What it means in practice is that a **chart widget** — axes, a polyline,
a scatter — cannot be built from the current primitives without either a
path op on the wire or a client-side pixel buffer, and a **drawing app**
is the same answer with a larger buffer. The pixel-buffer route is §7.4's
cost model, affordable at anything below fullscreen (`putimage` at
500×500 costs the server **1 566 µs/frame**) and §5's bandwidth problem
above it. Note also that a **rounded rect with `corners = d/2` is an
antialiased circle** and the server rasterises it well (§7.9), so
"circle" is not actually missing; "line" and "arbitrary path" are.
Knowing precisely which is missing is worth more than a synthetic ops/s
figure for a primitive that does not exist. If a path primitive is ever
added, this section is where its benchmark goes.

**`-move` ("flying windows") is missing too, and for a sharper reason.**
x11perf's version creates N small decorated windows and moves them every
frame; it is the classic exercise of the window manager, the frame hit
test and the damage union. On this wire it is **not expressible at all**:
no client message carries a window position. `CreateWindow` has `size`,
`layer`, `flags` and `title` and no origin, and `position` occurs exactly
once in the entire protocol — on the server's `Configure`, travelling the
other way. Placement is the window manager's decision and a client is
*told* where it ended up. Nor is it a privilege question: the `SHELL`
block can focus, restack, anchor and reserve screen space, and it cannot
move a window either.

So the honest statement is that **a client cannot animate its own window
across the screen**, and a benchmark of that operation would have to
drive the pointer through the server's own drag path rather than send
anything. `rects-move` is emphatically *not* a substitute and is not
offered as one: it moves undecorated `Rect` nodes inside a single window
and touches no window-manager code. Whether the gap matters is a real
question — a tiling desktop never needs it, a floating one gets dragging
from the server for free, and the case it actually blocks is a client
animating its own window (a slide-in panel, a tear-off palette). It is
recorded here because the spec asked for the scenario and the reason it
is absent is a fact about the protocol rather than about the benchmark.

## 4. The shape of a run

One process per row, which is load-bearing rather than convenient: the
CPU columns are `/proc` deltas, so a second scenario in the same process
would inherit the first one's heap, page cache and warmed socket, and the
server's `stats` windows are over the last N frames, so back-to-back runs
would bleed one scenario's paint times into the next one's
`paint_us_mean`. `deploy/bench.sh` forks a fresh `nitro-bench` per row
and appends its JSON line to the ledger.

Each run: connect, open one **undecorated** window (decoration would be a
constant every row paid), let the server `Configure` it, build the scene,
then run frame-paced against `Frame` callbacks — six seconds for every
row here, three for the control arm — sending exactly one transaction per
callback. The scenarios that push pixels time their own effect and their
own `pwrite` separately, so the report can subtract them: `frame = effect
+ upload + server pread + server paint + server copy`. Without that
split, a fullscreen plasma at 50 ms per frame is an indictment of the
compositor when 47 ms of it was the sine loop (§7.10).

Non-fullscreen rows run at **640×480**, which is period-correct: it is
what every effect in this crate originally ran at, and putting the
fullscreen variants next to a VGA-sized one is half the point. **Every
pixel/node pair now runs at both sizes**, which is what lets §7.7–§7.9
separate "the retained arm wins" from "the pixel arm hit the bandwidth
wall at 1080p" — two different claims, of which only the first is about
the architecture.

### A defect behind these runs, and why the numbers survive it

Every pixel-path row below (`putimage`, `plasma`, `fire`, `rotozoom`,
`boing`, `starfield`, `balls`) was measured while the benchmark was
displaying a **frozen frame 0**. `PixelScenario::build` called
`memfd_create` twice where it meant to `dup` once, so the descriptor the
client wrote every frame and the descriptor the server read from were two
unrelated files that merely started with the same bytes. Nobody saw it
because the picture was a correctly rendered plasma or starfield — just
the same one every frame. Issue #584, fixed; the node-path rows were
never affected.

**The cost columns stand and the box sweep was not re-run.** Every byte
of per-frame work still happened: the effect really computed a new frame
(`compute_us`), the client really `pwrite` a whole buffer (`upload_us`),
the server really re-`pread` the full damaged rect and really blitted it
(`paint_us`, `damage_px`). Only the pixel *values* were stale, and
copying a byte does not cost more or less depending on what the byte is.
Measured either side of the fix, `plasma --fullscreen` agrees within this
box's noise. A reader is entitled to know what the screen was showing
while these numbers were taken; they are not entitled to a different
number, because there isn't one.

## 5. Ceilings: what the machine can move

A benchmark result without a denominator is a number, not a finding.
"A fullscreen putimage costs 8 ms" becomes a finding only next to what
the machine can actually move. `nitro-bench bandwidth` measures it with
64 MB buffers — far past this box's 3 MB last-level cache, so the figure
is memory and not L3. Three loops and not one, because the server's
`pread` of a client buffer and its copy into scanout are both *copies*,
an effect filling its own surface is a *write* (faster, so quoting the
copy number for it would understate the headroom), and the read row
exists because the gap between read and write bandwidth on a
write-combined mapping is the entire reason the shadow buffer exists
(`docs/latency.md` §4.5).

| | GB/s | 1080p BGRA frames/s |
|---|---|---|
| **copy** (`dst[i] = src[i]`, one read + one write per byte) | **3.61** | **435.1** |
| **write** (`dst[i] = v`, no read) | **6.60** | 795.9 |
| **read** (sum every byte) | **8.50** | 1024.5 |

The frame arithmetic follows from one number, `1920 × 1080 × 4 =
**8 294 400 bytes** per 1080p BGRA frame`. At 60 Hz that is **497.7 MB/s
written** by the client. The server then reads the same bytes back —
`pread`, because it copies rather than maps; a mapping would let a client
`ftruncate` a memfd smaller underneath the server and turn a read into
`SIGBUS` — paints them, and copies the damaged region into
write-combined scanout memory. Call it **~3 passes** over the frame, an
approximation in one direction only: the third pass is
damage-proportional while the first two are not, so for a fullscreen
effect it is exact and for anything smaller an over-estimate.

| | bytes/s through memory | share of the box's 3.61 GB/s copy bandwidth |
|---|---|---|
| fullscreen 1080p at **60 Hz**, ~3 passes | **1.49 GB/s** | **41 %** |
| fullscreen 1080p at **120 Hz**, ~3 passes | **2.99 GB/s** | **83 %** |

**That is the single most useful predictive statement in this document.**
At 60 Hz the fullscreen pixel path spends two fifths of the machine's
memory bandwidth before the compositor has done anything clever, which is
survivable and is why §7.10's rotozoom still holds 60 Hz at 1080p, while
fire beside it and §7.7's boing, whose effects are dearer, fall to 30. At 120 Hz it
wants 83 %, which is not survivable alongside a client that also has to
*compute* the pixels: **the fullscreen pixel path at 120 Hz is
bandwidth-bound before it is CPU-bound**. No amount of making the
rasteriser faster changes that; only sending fewer bytes does, which is
what §7.7 through §7.9 measure — the retained arms are on the other side
of this line by five orders of magnitude, `boing-node` sending **52 bytes
per frame**.

## 6. The tables

Generated by `nitro-bench report`, pasted verbatim, grouped by scenario
exactly as that tool groups them. The ledger is checked in as
`docs/bench-1f35491.jsonl`, so every number in this document can be
recomputed from the raw counters without access to the box — the same
reason `docs/*.png` is checked in against the tree's own "screenshots are
build output" rule. On the box `deploy/bench.sh` writes
`~/tmp/bench/<sha>.jsonl` and `just
bench` fetches it to `tmp/bench/box.jsonl`. A table nobody regenerates is
a table somebody stopped updating in March, so this one is generated and
its provenance line says how many records it summarises.

Column notes, once, for all of them. **server / client µs/frame** are the
`/proc` CPU deltas over presented frames of §2, the headline pair.
**paint µs mean/max** and **copy µs mean** are the server's own counters,
which is why they are smaller than the CPU column — the CPU figure also
contains protocol handling, damage computation and the `pread`. **damage
px** is `damage_px_mean`, the server's mean damaged area per frame, of
which 2 073 600 is the whole screen. **bytes/frame** is wire bytes per
*commit*, the number that says whether a scenario is sending a scene
graph or a framebuffer. **flip rise µs** is the *rise* in the server's
all-time worst flip interval during this run, not its value, printed so
the verdict can be checked rather than taken (§8). And **verdict** is
`**dropped**` when this run's own worst flip interval exceeded 1.5 frame
budgets, `**slow**` when it presented below 85 % of the refresh *without*
a flip gap — the client being the limit — and `ok` otherwise. A run's
label carries its geometry: `WxH` whenever the surface is not square, so
`plasma 1920x1080` cannot be misread as a 1920² buffer.

<!-- generated by `nitro-bench report`: 140 runs -->

### rects

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rects n=10 640x480 | 59.8 | 60.0 | 10.0 | 779.9 | 55.7 | 388.0/417.0 | 152.0 | 119201 | 194 | 2 | ok |
| rects n=100 640x480 | 59.8 | 60.0 | 100.0 | 3091.9 | 83.6 | 2267.0/2418.0 | 392.0 | 322871 | 1724 | 1 | ok |
| rects n=500 640x480 | 59.8 | 60.0 | 500.0 | 10222.8 | 111.4 | 8904.0/9030.0 | 404.0 | 334841 | 8524 | 0 | ok |
| rects n=1000 640x480 | 59.7 | 59.8 | 1000.0 | 11843.6 | 167.6 | 10550.0/11006.0 | 245.0 | 334841 | 17024 | 16649 | **dropped** |
| rects n=2000 640x480 | 57.3 | 57.5 | 2000.0 | 12616.3 | 145.3 | 10953.0/11115.0 | 183.0 | 334841 | 34024 | 12 | **dropped** |
| rects n=500 640x480 | 60.0 | 60.0 | 500.0 | 10000.0 | 111.1 | 8824.0/9385.0 | 393.0 | 335093 | 8524 | 0 | ok |
| rects n=10 640x480 | 119.8 | 120.0 | 10.0 | 806.7 | 55.6 | 380.0/402.0 | 159.0 | 119201 | 194 | 0 | ok |
| rects n=100 640x480 | 119.8 | 120.0 | 100.0 | 3073.7 | 83.4 | 2272.0/2353.0 | 388.0 | 322861 | 1724 | 0 | ok |
| rects n=500 640x480 | 60.5 | 60.5 | 500.0 | 10275.5 | 110.2 | 8991.0/9181.0 | 414.0 | 334841 | 8524 | 3 | **dropped** |
| rects n=1000 640x480 | 60.0 | 60.2 | 1000.0 | 11916.7 | 194.4 | 10578.0/11148.0 | 252.0 | 334841 | 17024 | 0 | **slow** |
| rects n=2000 640x480 | 59.7 | 59.7 | 2000.0 | 12430.2 | 139.7 | 11056.0/11217.0 | 194.0 | 334841 | 34024 | 8334 | **dropped** |
| rects n=500 640x480 | 101.0 | 101.3 | 500.0 | 6699.7 | 66.0 | 5166.0/5616.0 | 236.0 | 334841 | 8524 | 0 | **slow** |
| rects n=100 640x480 | 240.0 | 240.0 | 100.0 | 2937.5 | 83.3 | 2160.0/2227.0 | 365.0 | 299500 | 1724 | 1 | ok |
| rects n=500 640x480 | 114.5 | 114.7 | 500.0 | 6550.2 | 87.3 | 5396.0/5533.0 | 252.0 | 334841 | 8524 | 8332 | **dropped** |
| rects n=2000 640x480 | 79.8 | 79.8 | 2000.0 | 10000.0 | 146.1 | 8881.0/8950.0 | 184.0 | 333044 | 34024 | 4161 | **dropped** |
| rects n=500 640x480 | 201.3 | 201.7 | 500.0 | 3758.3 | 33.1 | 2662.0/2694.0 | 163.0 | 292312 | 8524 | 0 | **slow** |

### rects-move

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rects-move n=10 640x480 | 59.8 | 60.0 | 10.0 | 779.9 | 55.7 | 392.0/513.0 | 151.0 | 120801 | 304 | 3 | ok |
| rects-move n=100 640x480 | 59.8 | 60.0 | 100.0 | 3119.8 | 83.6 | 2319.0/2664.0 | 393.0 | 325146 | 2824 | 0 | ok |
| rects-move n=500 640x480 | 60.0 | 60.0 | 500.0 | 10250.0 | 111.1 | 8959.0/9105.0 | 378.0 | 324540 | 14024 | 1 | ok |
| rects-move n=1000 640x480 | 59.8 | 60.0 | 1000.0 | 11671.3 | 167.1 | 10401.0/11163.0 | 244.0 | 337161 | 28024 | 0 | ok |
| rects-move n=2000 640x480 | 59.7 | 59.8 | 2000.0 | 12402.2 | 139.7 | 11155.0/11374.0 | 184.0 | 337161 | 56024 | 0 | ok |
| rects-move n=10 640x480 | 119.8 | 120.0 | 10.0 | 765.0 | 55.6 | 388.0/411.0 | 156.0 | 120801 | 304 | 0 | ok |
| rects-move n=100 640x480 | 119.8 | 120.0 | 100.0 | 3157.2 | 83.4 | 2349.0/2868.0 | 396.0 | 325146 | 2824 | 0 | ok |
| rects-move n=500 640x480 | 119.5 | 119.5 | 500.0 | 6053.0 | 69.7 | 5212.0/5282.0 | 223.0 | 324540 | 14024 | 0 | ok |
| rects-move n=1000 640x480 | 60.2 | 60.3 | 1000.0 | 11385.0 | 193.9 | 10202.0/11352.0 | 246.0 | 337161 | 28024 | 0 | **slow** |
| rects-move n=2000 640x480 | 59.7 | 59.8 | 2000.0 | 12486.0 | 139.7 | 11119.0/11384.0 | 196.0 | 337161 | 56024 | 0 | **slow** |

### text

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| text n=10 640x480 | 59.8 | 60.0 | 10.0 | 2367.7 | 139.3 | 83.0/88.0 | 8.0 | 8160 | 474 | 0 | ok |
| text n=10 640x480 | 59.8 | 60.0 | 10.0 | 2507.0 | 139.3 | 218.0/486.0 | 41.0 | 39168 | 474 | 0 | ok |
| text n=100 640x480 | 60.0 | 60.2 | 100.0 | 7000.0 | 222.2 | 658.0/743.0 | 112.0 | 84240 | 4524 | 0 | ok |
| text n=100 640x480 | 59.8 | 60.0 | 100.0 | 8300.8 | 222.8 | 1581.0/1644.0 | 347.0 | 271296 | 4524 | 0 | ok |
| text n=500 640x480 | 59.8 | 60.0 | 500.0 | 12144.8 | 306.4 | 1449.0/1620.0 | 230.0 | 293909 | 22524 | 0 | ok |
| text n=500 640x480 | 59.8 | 60.0 | 500.0 | 12061.3 | 250.7 | 2276.0/2560.0 | 197.0 | 271296 | 22524 | 0 | ok |
| text n=10 640x480 | 120.0 | 120.0 | 10.0 | 2250.0 | 125.0 | 84.0/110.0 | 8.0 | 8160 | 474 | 0 | ok |
| text n=10 640x480 | 119.8 | 120.0 | 10.0 | 2433.9 | 125.2 | 216.0/476.0 | 44.0 | 39168 | 474 | 0 | ok |
| text n=100 640x480 | 119.8 | 119.8 | 100.0 | 6203.1 | 208.6 | 577.0/610.0 | 95.0 | 84240 | 4524 | 0 | ok |
| text n=100 640x480 | 119.8 | 120.0 | 100.0 | 6161.3 | 180.8 | 1206.0/1252.0 | 271.0 | 271296 | 4524 | 0 | ok |
| text n=500 640x480 | 119.5 | 119.7 | 500.0 | 7963.7 | 223.2 | 1059.0/1104.0 | 232.0 | 293904 | 22524 | 0 | ok |
| text n=500 640x480 | 116.0 | 116.0 | 500.0 | 8577.6 | 258.6 | 1772.0/2195.0 | 201.0 | 271296 | 22524 | 0 | ok |
| text n=100 640x480 | 239.8 | 240.0 | 100.0 | 3203.6 | 118.1 | 312.0/329.0 | 45.0 | 84240 | 4524 | 0 | ok |

### text-static

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| text-static n=10 640x480 | 59.8 | 60.0 | 10.0 | 306.4 | 55.7 | 91.0/123.0 | 8.0 | 8398 | 304 | 0 | ok |
| text-static n=100 640x480 | 60.0 | 60.0 | 100.0 | 1166.7 | 83.3 | 676.0/698.0 | 102.0 | 86130 | 2824 | 0 | ok |
| text-static n=500 640x480 | 60.0 | 60.2 | 500.0 | 4555.6 | 111.1 | 3021.0/3092.0 | 368.0 | 300498 | 14024 | 0 | ok |
| text-static n=10 640x480 | 119.7 | 119.8 | 10.0 | 320.3 | 55.7 | 86.0/118.0 | 8.0 | 8398 | 304 | 0 | ok |
| text-static n=100 640x480 | 120.0 | 120.0 | 100.0 | 1180.6 | 83.3 | 674.0/703.0 | 104.0 | 86130 | 2824 | 0 | ok |
| text-static n=500 640x480 | 119.8 | 120.0 | 500.0 | 4548.0 | 139.1 | 3041.0/3125.0 | 375.0 | 300498 | 14024 | 0 | ok |
| text-static n=100 640x480 | 239.8 | 240.0 | 100.0 | 1195.3 | 76.4 | 672.0/693.0 | 104.0 | 86130 | 2824 | 0 | ok |

### putimage

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| putimage 640x480 | 59.8 | 60.0 | 1.0 | 390.0 | 668.5 | 73.0/100.0 | 10.0 | 10010 | 56 | 0 | ok |
| putimage 640x480 | 59.8 | 60.0 | 1.0 | 779.9 | 4039.0 | 314.0/552.0 | 81.0 | 62502 | 56 | 0 | ok |
| putimage 640x480 | 59.8 | 60.0 | 1.0 | 1476.3 | 8189.4 | 712.0/929.0 | 161.0 | 250000 | 56 | 0 | ok |
| putimage 640x480 | 30.0 | 30.0 | 1.0 | 5166.7 | 27111.1 | 887.0/1828.0 | 784.0 | 613440 | 56 | 0 | **slow** |
| putimage 640x480 | 119.8 | 120.0 | 1.0 | 375.5 | 625.9 | 74.0/104.0 | 10.0 | 10000 | 56 | 0 | ok |
| putimage 640x480 | 119.8 | 120.0 | 1.0 | 792.8 | 3866.5 | 299.0/327.0 | 84.0 | 62500 | 56 | 0 | ok |
| putimage 640x480 | 119.8 | 119.8 | 1.0 | 1307.4 | 4937.4 | 561.0/609.0 | 163.0 | 250000 | 56 | 0 | ok |
| putimage 640x480 | 29.6 | 29.6 | 1.0 | 4719.1 | 27359.6 | 860.0/1762.0 | 547.0 | 613440 | 56 | 0 | **slow** |
| putimage 640x480 | 239.8 | 240.0 | 1.0 | 382.2 | 667.1 | 71.0/96.0 | 11.0 | 10000 | 56 | 0 | ok |
| putimage 640x480 | 119.9 | 120.1 | 1.0 | 1472.2 | 5055.6 | 263.0/552.0 | 202.0 | 250000 | 56 | 0 | **slow** |
| putimage 640x480 | 54.0 | 53.8 | 1.0 | 2469.1 | 13611.1 | 437.0/1007.0 | 297.0 | 380160 | 56 | 2 | **dropped** |

### scroll

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| scroll n=500 640x480 | 59.8 | 60.0 | 1.0 | 1615.6 | 55.7 | 834.0/901.0 | 361.0 | 306560 | 52 | 0 | ok |
| scroll n=500 640x480 | 119.8 | 120.0 | 1.0 | 1599.4 | 55.6 | 840.0/867.0 | 366.0 | 306560 | 52 | 0 | ok |
| scroll n=500 640x480 | 239.8 | 240.0 | 1.0 | 1521.9 | 69.5 | 677.0/1042.0 | 315.0 | 306560 | 52 | 0 | ok |

### create

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| create n=50 640x480 | 60.0 | 60.2 | 153.0 | 694.4 | 55.6 | 243.0/262.0 | 29.0 | 29281 | 3385 | 0 | ok |
| create n=50 640x480 | 120.2 | 120.2 | 153.0 | 693.5 | 83.2 | 241.0/274.0 | 32.0 | 29281 | 3385 | 0 | ok |

### plasma

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| plasma 640x480 | 60.0 | 60.0 | 1.0 | 1638.9 | 9000.0 | 779.0/886.0 | 205.0 | 307200 | 56 | 0 | ok |
| plasma 1920x1080 | 15.0 | 15.0 | 1.0 | 14444.4 | 49333.3 | 3511.0/7080.0 | 2094.0 | 2073600 | 56 | 16666 | **dropped** |
| plasma 640x480 | 103.7 | 103.8 | 1.0 | 1623.8 | 6623.8 | 657.0/691.0 | 209.0 | 307200 | 56 | 0 | ok |
| plasma 1920x1080 | 15.0 | 15.0 | 1.0 | 15111.1 | 49555.6 | 3705.0/7515.0 | 2243.0 | 2073600 | 56 | 0 | **slow** |
| plasma 640x480 | 80.0 | 80.0 | 1.0 | 1958.3 | 8291.7 | 400.0/847.0 | 240.0 | 307200 | 56 | 0 | **slow** |
| plasma 1280x720 | 34.3 | 34.3 | 1.0 | 6116.5 | 21310.7 | 1372.0/2847.0 | 875.0 | 921600 | 56 | 0 | **slow** |

### fire

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fire 640x480 | 59.9 | 60.1 | 1.0 | 2722.2 | 9583.3 | 1371.0/1494.0 | 383.0 | 307200 | 56 | 0 | ok |
| fire 1920x1080 | 30.0 | 30.0 | 1.0 | 16666.7 | 22555.6 | 3697.0/9270.0 | 3128.0 | 2073600 | 56 | 0 | **slow** |
| fire 640x480 | 120.0 | 120.0 | 1.0 | 1777.8 | 4722.2 | 859.0/943.0 | 265.0 | 307200 | 56 | 0 | ok |
| fire 1920x1080 | 24.2 | 24.0 | 1.0 | 15103.4 | 25793.1 | 3743.0/7580.0 | 2211.0 | 2073600 | 56 | 0 | **slow** |
| fire 640x480 | 238.5 | 238.7 | 1.0 | 1614.3 | 3004.9 | 770.0/826.0 | 255.0 | 307200 | 56 | 0 | ok |
| fire 1280x720 | 48.0 | 48.0 | 1.0 | 6250.0 | 14652.8 | 1405.0/2946.0 | 878.0 | 921600 | 56 | 0 | **slow** |

### rotozoom

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rotozoom 640x480 | 59.8 | 60.0 | 1.0 | 2896.9 | 4261.8 | 1441.0/1751.0 | 408.0 | 309910 | 56 | 0 | ok |
| rotozoom 1920x1080 | 59.0 | 59.2 | 1.0 | 16525.4 | 12570.6 | 10963.0/11199.0 | 2793.0 | 2073600 | 56 | 0 | ok |
| rotozoom 640x480 | 119.7 | 119.8 | 1.0 | 2729.8 | 3941.5 | 1302.0/1574.0 | 338.0 | 307200 | 56 | 0 | ok |
| rotozoom 1920x1080 | 23.3 | 23.3 | 1.0 | 19428.6 | 22785.7 | 4946.0/10268.0 | 2543.0 | 2073600 | 56 | 0 | **slow** |
| rotozoom 640x480 | 240.0 | 240.2 | 1.0 | 1861.1 | 1826.4 | 891.0/1239.0 | 309.0 | 307210 | 56 | 0 | ok |
| rotozoom 1280x720 | 119.9 | 120.1 | 1.0 | 7500.0 | 5861.1 | 4542.0/4694.0 | 1537.0 | 921600 | 56 | 0 | **slow** |

### boing

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing 640x480 | 59.8 | 60.0 | 1.0 | 2924.8 | 6796.7 | 1533.0/1649.0 | 406.0 | 307200 | 56 | 0 | ok |
| boing 1920x1080 | 30.0 | 30.0 | 1.0 | 20277.8 | 21055.6 | 5912.0/11950.0 | 2702.0 | 2073600 | 56 | 0 | **slow** |
| boing 640x480 | 59.8 | 60.0 | 1.0 | 3119.8 | 6991.6 | 1707.0/2514.0 | 414.0 | 307205 | 56 | 0 | ok |
| boing 1920x1080 | 30.0 | 30.0 | 1.0 | 20500.0 | 20833.3 | 6041.0/12134.0 | 2630.0 | 2073600 | 56 | 0 | **slow** |
| boing 640x480 | 120.0 | 120.0 | 1.0 | 2291.7 | 4611.1 | 1223.0/1406.0 | 312.0 | 307200 | 56 | 0 | ok |
| boing 1920x1080 | 24.7 | 24.7 | 1.0 | 18648.6 | 22364.9 | 4207.0/9916.0 | 3483.0 | 2073600 | 56 | 1 | **dropped** |
| boing 640x480 | 119.8 | 120.0 | 1.0 | 2267.0 | 4631.4 | 1191.0/1404.0 | 311.0 | 307200 | 56 | 0 | ok |
| boing 1920x1080 | 24.3 | 24.3 | 1.0 | 18698.6 | 23972.6 | 4052.0/8896.0 | 3347.0 | 2073600 | 56 | 0 | **slow** |
| boing 640x480 | 240.1 | 240.1 | 1.0 | 1880.6 | 2435.8 | 1034.0/1082.0 | 263.0 | 307200 | 56 | 0 | ok |
| boing 1280x720 | 42.2 | 42.2 | 1.0 | 8853.8 | 14505.9 | 1963.0/4490.0 | 1449.0 | 921600 | 56 | 2 | **dropped** |

### boing-node

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing-node 640x480 | 59.8 | 60.0 | 1.0 | 2228.4 | 55.7 | 2021.0/2036.0 | 42.0 | 32885 | 52 | 0 | ok |
| boing-node 1920x1080 | 59.8 | 60.0 | 1.0 | 10000.0 | 111.4 | 9572.0/9775.0 | 205.0 | 168868 | 52 | 0 | ok |
| boing-node 640x480 | 119.7 | 119.8 | 1.0 | 2284.1 | 55.7 | 2009.0/2099.0 | 42.0 | 33354 | 52 | 0 | ok |
| boing-node 1920x1080 | 60.3 | 60.5 | 1.0 | 10110.5 | 55.2 | 9671.0/9780.0 | 209.0 | 168868 | 52 | 0 | **slow** |
| boing-node 640x480 | 240.0 | 240.2 | 1.0 | 2270.8 | 62.5 | 2014.0/2038.0 | 42.0 | 33078 | 52 | 0 | ok |
| boing-node 1280x720 | 120.8 | 121.0 | 1.0 | 4675.9 | 69.0 | 4374.0/4462.0 | 97.0 | 76509 | 52 | 0 | **slow** |

### starfield

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield n=100 640x480 | 59.8 | 60.0 | 1.0 | 2841.2 | 1169.9 | 1508.0/1548.0 | 397.0 | 307200 | 56 | 0 | ok |
| starfield n=100 1920x1080 | 60.0 | 60.2 | 1.0 | 16500.0 | 9888.9 | 11553.0/11644.0 | 2141.0 | 2073600 | 56 | 0 | ok |
| starfield n=500 640x480 | 59.8 | 60.0 | 1.0 | 3203.3 | 1504.2 | 1814.0/1884.0 | 420.0 | 307200 | 56 | 0 | ok |
| starfield n=500 1920x1080 | 59.0 | 59.0 | 1.0 | 16355.9 | 9604.5 | 11517.0/11579.0 | 2144.0 | 2073600 | 56 | 0 | ok |
| starfield n=2000 640x480 | 60.0 | 60.0 | 1.0 | 2833.3 | 1277.8 | 1520.0/1630.0 | 397.0 | 307200 | 56 | 0 | ok |
| starfield n=2000 1920x1080 | 59.8 | 60.0 | 1.0 | 16740.9 | 10195.0 | 11757.0/11865.0 | 2182.0 | 2073600 | 56 | 0 | ok |
| starfield n=100 640x480 | 119.8 | 120.0 | 1.0 | 2948.5 | 1251.7 | 1570.0/1624.0 | 411.0 | 307200 | 56 | 0 | ok |
| starfield n=100 1920x1080 | 53.4 | 53.4 | 1.0 | 17476.6 | 10436.1 | 12247.0/12365.0 | 2269.0 | 2073600 | 56 | 0 | **slow** |
| starfield n=500 640x480 | 119.8 | 120.0 | 1.0 | 2934.6 | 1251.7 | 1577.0/1635.0 | 407.0 | 307200 | 56 | 0 | ok |
| starfield n=500 1920x1080 | 53.3 | 53.4 | 1.0 | 17593.8 | 10500.0 | 12260.0/12422.0 | 2259.0 | 2073600 | 56 | 0 | **slow** |
| starfield n=2000 640x480 | 119.8 | 120.0 | 1.0 | 3045.9 | 1460.4 | 1647.0/1889.0 | 411.0 | 307200 | 56 | 0 | ok |
| starfield n=2000 1920x1080 | 53.3 | 53.4 | 1.0 | 17531.2 | 10500.0 | 12267.0/12383.0 | 2265.0 | 2073600 | 56 | 0 | **slow** |
| starfield n=100 640x480 | 240.0 | 240.2 | 1.0 | 2895.8 | 1215.3 | 1544.0/1572.0 | 405.0 | 307200 | 56 | 0 | ok |
| starfield n=100 1280x720 | 120.0 | 120.2 | 1.0 | 7527.8 | 4444.4 | 5157.0/5260.0 | 929.0 | 921600 | 56 | 0 | **slow** |
| starfield n=500 640x480 | 240.0 | 240.0 | 1.0 | 2895.8 | 1229.2 | 1549.0/1579.0 | 407.0 | 307200 | 56 | 0 | ok |
| starfield n=500 1280x720 | 120.0 | 120.2 | 1.0 | 7527.8 | 4513.9 | 5177.0/5251.0 | 933.0 | 921600 | 56 | 0 | **slow** |
| starfield n=2000 640x480 | 239.8 | 240.0 | 1.0 | 2911.7 | 1369.0 | 1542.0/1576.0 | 407.0 | 307200 | 56 | 0 | ok |
| starfield n=2000 1280x720 | 120.0 | 120.2 | 1.0 | 7569.4 | 4527.8 | 5199.0/5291.0 | 928.0 | 921600 | 56 | 0 | **slow** |

### starfield-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield-nodes n=100 640x480 | 59.8 | 60.0 | 100.0 | 1420.6 | 83.6 | 671.0/728.0 | 364.0 | 303278 | 2824 | 0 | ok |
| starfield-nodes n=100 1920x1080 | 59.8 | 60.0 | 100.0 | 7186.6 | 111.4 | 4459.0/4773.0 | 2486.0 | 2032190 | 2824 | 0 | ok |
| starfield-nodes n=500 640x480 | 59.8 | 60.0 | 500.0 | 2507.0 | 139.3 | 1178.0/1202.0 | 391.0 | 309444 | 14024 | 0 | ok |
| starfield-nodes n=500 1920x1080 | 60.0 | 60.2 | 500.0 | 8750.0 | 166.7 | 5202.0/5276.0 | 2512.0 | 2073600 | 14024 | 0 | ok |
| starfield-nodes n=2000 640x480 | 59.8 | 60.0 | 2000.0 | 6156.0 | 362.1 | 3052.0/3144.0 | 417.0 | 309444 | 56024 | 0 | ok |
| starfield-nodes n=2000 1920x1080 | 60.0 | 60.0 | 2000.0 | 11361.1 | 333.3 | 6581.0/6868.0 | 2353.0 | 2073600 | 56024 | 0 | ok |
| starfield-nodes n=100 640x480 | 119.8 | 120.0 | 100.0 | 1432.5 | 69.5 | 659.0/720.0 | 367.0 | 303399 | 2824 | 0 | ok |
| starfield-nodes n=100 1920x1080 | 120.0 | 120.0 | 100.0 | 6291.7 | 69.4 | 3667.0/4020.0 | 2318.0 | 2021933 | 2824 | 0 | ok |
| starfield-nodes n=500 640x480 | 119.7 | 119.8 | 500.0 | 2520.9 | 139.3 | 1207.0/1231.0 | 369.0 | 309444 | 14024 | 0 | ok |
| starfield-nodes n=500 1920x1080 | 119.8 | 120.0 | 500.0 | 7009.7 | 83.4 | 4177.0/4266.0 | 2376.0 | 2073600 | 14024 | 0 | ok |
| starfield-nodes n=2000 640x480 | 119.8 | 120.0 | 2000.0 | 6189.2 | 361.6 | 3097.0/3191.0 | 435.0 | 309444 | 56024 | 0 | ok |
| starfield-nodes n=2000 1920x1080 | 119.8 | 120.0 | 2000.0 | 8303.2 | 180.8 | 4973.0/5082.0 | 2357.0 | 2073600 | 56024 | 0 | ok |
| starfield-nodes n=100 640x480 | 240.0 | 240.2 | 100.0 | 1437.5 | 83.3 | 621.0/699.0 | 368.0 | 287877 | 2824 | 0 | ok |
| starfield-nodes n=100 1280x720 | 238.0 | 238.2 | 100.0 | 3032.2 | 84.0 | 1609.0/1853.0 | 944.0 | 867916 | 2824 | 0 | ok |
| starfield-nodes n=500 640x480 | 239.8 | 240.0 | 500.0 | 2487.8 | 145.9 | 1180.0/1203.0 | 401.0 | 309444 | 14024 | 0 | ok |
| starfield-nodes n=500 1280x720 | 240.0 | 240.2 | 500.0 | 3173.6 | 111.1 | 1694.0/1752.0 | 808.0 | 921600 | 14024 | 0 | ok |
| starfield-nodes n=2000 640x480 | 240.0 | 240.2 | 2000.0 | 3152.8 | 215.3 | 1556.0/1630.0 | 213.0 | 309444 | 56024 | 0 | ok |
| starfield-nodes n=2000 1280x720 | 235.5 | 235.7 | 2000.0 | 4012.7 | 219.4 | 2200.0/2277.0 | 815.0 | 921600 | 56024 | 0 | ok |

### balls

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls n=32 640x480 | 59.8 | 60.0 | 1.0 | 2896.9 | 1448.5 | 1490.0/1592.0 | 402.0 | 307304 | 56 | 0 | ok |
| balls n=32 1920x1080 | 59.8 | 60.0 | 1.0 | 16657.4 | 10195.0 | 11663.0/11773.0 | 2139.0 | 2073600 | 56 | 0 | ok |
| balls n=32 640x480 | 119.8 | 120.0 | 1.0 | 2934.6 | 1502.1 | 1553.0/1582.0 | 414.0 | 307200 | 56 | 0 | ok |
| balls n=32 1920x1080 | 51.3 | 51.5 | 1.0 | 17694.8 | 10876.6 | 12401.0/12530.0 | 2275.0 | 2073600 | 56 | 0 | **slow** |

### balls-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls-nodes n=32 640x480 | 60.0 | 60.0 | 32.0 | 1722.2 | 55.6 | 1194.0/1354.0 | 80.0 | 61003 | 920 | 0 | ok |
| balls-nodes n=32 1920x1080 | 59.8 | 60.0 | 32.0 | 6044.6 | 83.6 | 3743.0/4424.0 | 1013.0 | 835574 | 920 | 0 | ok |
| balls-nodes n=32 640x480 | 119.7 | 119.8 | 32.0 | 1587.7 | 69.6 | 1079.0/1328.0 | 47.0 | 41931 | 920 | 0 | ok |
| balls-nodes n=32 1920x1080 | 119.8 | 120.0 | 32.0 | 4687.1 | 69.5 | 2694.0/2994.0 | 310.0 | 273781 | 920 | 0 | ok |

The last `rects n=500` row of each rate is the **control**: the same
scenario with the shell clients killed, so a reader can see what the bar
and the launcher cost every other row. At 60 Hz it is 10 000 µs/frame
against 10 223 with the shell up — **223 µs/frame, 2 % of the server's
per-frame CPU** — and the control is now taken *once per rate*, because
what the shell costs per frame is exactly the sort of figure this
document's §9 asks whether the rate moves. Read it with the caveat the
ledger's own note records: `nitro-session` supervises its children and
restarts them within about a second, so the control arm is three seconds
long and the shell was back up for most of it — so the control arm still
paid most of the shell's cost, and the gap is therefore a *floor* under
it rather than a ceiling. The way to tighten it would be to stop the
session rather than kill the processes.

The control is also a row the report has to work to keep separate, and
the reason is worth a sentence here because it nearly cost this document
a table. It is byte-for-byte the same sweep point as the row it controls
— `rects`, n=500, 640×480, same rate — so any grouping keyed on those
fields alone treats the two as one run, and "the last run wins" then
**silently replaces every measured row with its own control**. The table
would report 10 000 µs where the measurement is 10 223: a plausible
number, in the right units, in the right cell, describing a desktop with
no shell on it. `Record::is_control` reads the ledger's note so the two
stay apart, and §9's rate tables label the control row in words.

## 7. Verdict per scenario

### 7.1 `rects` — the x11perf number, transposed

`x11perf -rect100`, as N nodes that recolour every frame — all of them,
because a node that does not change costs the server nothing, which is
the property under test. The sweep runs to the end: **n=2000 at
57.3 presented/s, 12 616 µs/frame of server CPU**. Nothing broke, but the
top of the sweep is no longer comfortable — see the verdict note below.

**So this box sustains about 2000 rect mutations per frame at
60 Hz** — and the sweep stopped because it ran out of *damage*, not
because it ran out of headroom in mutations. That is the closest thing in
this document to a classical x11perf result, and the sentence §2
promised: not ops/s, but how much retained mutation fits in a frame.
§9.3 asks the same question of the other two rates and answers "between
100 and 500" at both 8.3 ms and 4.2 ms.

The cost is nearly linear in N and then flattens hard: 3 092 µs/frame at
n=100, 10 223 at n=500, 11 844 at n=1000, 12 616 at n=2000 — **23 % more
for four times the nodes** across those last three, because by n=500 the
grid already covers 334 841 damage pixels and stops growing, after which
the marginal cost is per-node bookkeeping against a fixed rasterised
area. `paint_us_mean` says the same thing more directly: 8 904 at n=500
against 10 953 at n=2000, 2 049 µs more for 1 500 more nodes, while the
wire traffic quadruples from 8 524 to 34 024 bytes per frame. The client
is never the limit: **167.6 µs/frame at n=1000**, some seventy times below
the server (11 843.6 ÷ 167.6 = 71×). Sending two thousand mutations is
cheap; painting them is not — and past n=500 this window cannot get any
dirtier, so finding the true mutation ceiling needs a bigger window or
smaller rects, which is the sweep to run next.

**Two rows at the top of this sweep carry a `**dropped**` verdict in this
ledger where the previous one had `ok`**, and the honest reading is that
they are marginal rather than that something regressed. `rects n=1000`
presents 59.7/s — indistinguishable from the 59.9 it managed before — but
its `flip_interval_max_us` rose by 16 649 µs during the run, one extra
refresh period, so §8's rise test attributes one missed vblank to it. At
n=2000 the rise is 12 µs and the verdict comes from the rate instead
(57.3/s, outside the 5 % tolerance). A single dropped frame in a
six-second run is what "at the edge of the budget" looks like at 12 616
µs against 16 667, and the column is doing its job by saying so rather
than rounding it to `ok`.

### 7.2 `rects-move` vs `rects` — the damage union costs very little

The pair exists because recolouring dirties each node's own bounds while
*moving* dirties the union of the old and the new, and a benchmark that
did only one of them would have missed the more expensive half.

**In this ledger the two are indistinguishable**, and that is a third
answer for a comparison this document has now given three:

| n | `rects` (recolour) µs/frame | `rects-move` µs/frame | move ÷ recolour | rects damage px | rects-move damage px |
|---|---|---|---|---|---|
| 10 | 779.9 | 779.9 | 1.000 | 119 201 | 120 801 |
| 100 | 3 091.9 | 3 119.8 | 1.009 | 322 871 | 325 146 |
| 500 | 10 222.8 | 10 250.0 | 1.003 | 334 841 | 324 540 |
| 1000 | 11 843.6 | 11 671.3 | **0.985** | 334 841 | 337 161 |
| 2000 | 12 616.3 | 12 402.2 | **0.983** | 334 841 | 337 161 |

Every ratio is within **±1.7 %**, the sign changes across the sweep, and
this box's own day-to-day drift is "a few percent" (§11). **There is no
effect here to report.** The previous ledger measured the move arm 2–6 %
dearer at every point and this document drew a finding from it; at six
seconds a row rather than ten, the same comparison gives a coin toss.
Reporting "moving is slightly dearer" from the earlier run was reading
a signal out of a gap that is the same size as the noise, and saying so
is more useful than a fourth revision of the sign.

What survives, and is the part that was never marginal, is the *reason*
both arms are the same: from n=500 onward **both saturate at ~335 000
damaged pixels**, against a 640×480 window of 307 200. The rects tile the
window; past a certain density everything is dirty either way, and the
old∪new union of a two-pixel move adds only a rim — at most 2 320 pixels,
0.7 %, which is exactly the size of the CPU difference that cannot be
resolved above. **A union is only expensive when the thing that moved is
*sparse***, which is the boing ball (§7.7) and not this. The folk belief
the original text set out to refute — "moving things is expensive,
recolouring is cheap" — is simply not measurable at this density, which
is a cleaner refutation than a 3 % win in either direction.

**This row has now reversed twice, and the second reversal is the
instructive one.** The first was a benchmark defect: the move arm cycled
eight phases that collapsed to four positions in consecutive pairs, and
`Scene::set_bounds` early-returns on unchanged bounds, so **half its
`SetBounds` were server-side no-ops** while the recolour arm changed
every node on every frame. The arm was doing half the work and the table
dutifully reported it as cheaper. That is fixed — the orbit is four
phases in which consecutive frames always differ on at least one axis,
pinned by `every_moving_frame_actually_moves_every_rect` and its twin on
the recolour side — and the fix is what made the arms comparable.

But the *second* reversal, from "consistently 2–6 % dearer" to "within
noise", had no defect behind it at all. Same code, same box, shorter
runs. A difference that small was never a finding; it was a measurement
reported to more precision than it had. The lesson is narrower than §8's
and worth its own sentence: **an instrument that is correct can still be
quoted past its resolution, and a comparison whose effect size is
smaller than the run-to-run drift needs repeats to be a result at all.**
Two arms, taken back to back once each, cannot distinguish 3 % on this
box.

### 7.3 `text` vs `text-static` — the retained-text result

The one x11perf could not have run. X11 has no retained text: a moved
string is a redraw, so it costs exactly what a new one costs. Here the
string is unchanged, so the server re-uses its layout and its glyph tiles
and does nothing but composite.

| n | `text` server µs/frame | `text-static` server µs/frame | ratio | `text` layouts shaped | `text-static` layouts shaped |
|---|---|---|---|---|---|
| 10 | 2 367.7 | 306.4 | 7.73× | 3 601 | 2 |
| 100 | **7 000.0** | **1 166.7** | **6.00×** | 36 100 | **0** |
| 500 | 12 144.8 | 4 555.6 | 2.67× | 180 002 | **0** |

The ratios are `text` ÷ `text-static` at the same n, on the server
µs/frame column (itself `server_cpu_us ÷ presented`), and the layout
counts are the server's own `text_layouts` counter differenced across the
run: `text` at n=100 shapes **100 layouts per frame, 36 100 over 361
commits** of a six-second run, and `text-static` at the same n shapes
**none at all** — and at n=500, none. The handful in the smallest static
arm is the shell, not the scenario; see the glyph paragraph below.
§9.5 takes the same pair to 120 and 240 Hz, where `text-static` holds
1 167 / 1 181 / 1 195 µs and `text` is the arm that falls over.

`glyph_renders` tells the same story and is the stronger claim, because
it is the expensive half: rasterising a glyph into the atlas. **It does
not move at all in the n=100 and n=500 arms — zero, on both sides of the
comparison** — because by then every glyph the scenario uses is already
in the atlas and re-shaping a layout is not re-rasterising its glyphs.
In the n=10 arms it rises by 23 and 5, which is the **bar's clock**
redrawing behind the benchmark window: that is what "the real desktop was
up" costs a counter, and why the larger sweep points are the ones to
quote.

**A moved label re-uses its layout and its glyph tiles.** The
microseconds are corroboration; the two pinned counters are the result.
`crates/nitro-bench/tests/against_server.rs` asserts the `text_layouts`
half against a real server on the fake backend, so a regression is caught
in CI and not only on the box.

The 24-pixel rows are the `-f24text` sweep — the second row of each pair
in §6's `text` table, which the label no longer distinguishes, because a
non-square run now prints its geometry instead of its `size=`. They say
something smaller: at n=100, doubling the font size costs **8 301 against
7 000 µs/frame**, a 19 % increase for 3.2× the damaged area (271 296
against 84 240 px) — the same 19 % the previous ledger measured, which is
the kind of agreement across two sittings that makes a ratio quotable.
Shaping dominates rasterisation at this size.

### 7.4 `putimage` — where the client becomes the limit

`x11perf -putimage100/-putimage500`: a client buffer rewritten and
re-uploaded every frame. Damage scales as the square of the edge, which
is the check that the scenario is measuring what it claims:

| size | damage px | expected (size²) | server µs/frame | client µs/frame | presented/s |
|---|---|---|---|---|---|
| 100 | 10 010 | 10 000 | 390.0 | 668.5 | 59.8 |
| 250 | 62 502 | 62 500 | 779.9 | 4 039.0 | 59.8 |
| 500 | 250 000 | 250 000 | 1 476.3 | 8 189.4 | 59.8 |
| 1080 | 613 440 | — | 5 166.7 | **27 111.1** | **30.0** |

The three small points land on size² to within ten pixels (`starfield` at
640 shows the same consistent +5, an artefact of the counter and not of
the scenario). The 1080 row does not, and the reason is worth recording:
the buffer is 1080×1080 in a 640×480 window, so it overflows and is
**clipped by the output's bottom edge** — 613 440 is exactly 1080 × 568.
Damage is what is on the screen, not what the client wrote. (The previous
ledger recorded 764 640 = 1080 × 708 for the same row: the window is the
same, but the benchmark window's *position* under the bar differs between
sittings, so how much of an oversized buffer survives the clip is a
property of where the window landed. The arithmetic is exact either way,
which is the point of checking it.)

At 1080 the run presents **30.0/s and is client-bound**: the client's own
effect plus upload is **27 028 µs/frame against the server's 5 167**, and
the verdict says `**slow**` rather than `**dropped**` — the server
flipped on time every time, there was simply nothing new to flip on half
of them. The split the crate measures makes the blame precise: of the
client's per-commit cost, **25 640 is the effect and 1 388 is the
`pwrite`** (`compute_us ÷ commits` and `upload_us ÷ commits`). The upload
is 5 % of the client's cost, so making the wire faster would buy this
scenario almost nothing.

That upload column was itself a finding, and it is now a **fixed** one.
The tree used to write `pwrite` rather than `mmap` because mapping needs
`unsafe`, which this workspace denies — so the upload column was a syscall
per frame that a mapped buffer would not pay. At 1080p it was **1 388
µs/frame** here, about 8 % of a 60 Hz budget, and 2 570–6 044 µs on the
fullscreen effect rows where the buffer is four times the size.

#569 took it to **zero**: the client renders into a mapping of a *sealed*
memfd and the server maps the same file instead of `pread`ing it. The
numbers in this section predate that change — they are the 1f35491 ledger
and are left exactly as measured — and §7.10a carries the before/after.
What made it possible was not lifting the `unsafe` rule but earning an
exception: `F_SEAL_SHRINK`, `F_SEAL_GROW` and `F_SEAL_SEAL` on the
client's memfd, verified by the server with `F_GET_SEALS` before it maps
anything, which is what stops a hostile client shrinking the file into a
`SIGBUS` under the server's mapping.

### 7.5 `scroll` — a retained scroll is one mutation

X11's `-scroll500` is a `CopyArea` of the window onto itself: the classic
terminal scroll, where the server blits the existing pixels up and
repaints only the newly exposed line. Nitro has no `CopyArea`, and asking
for one would miss the point — a retained scroll is a **transform on a
group**, and the server decides for itself whether that can be a copy or
has to be a repaint. So the scenario is a **fixed clipper at the
viewport** with a content group of 500 rects inside it, taller than the
window, whose offset moves one 16-pixel row per frame. The result is
**one mutation per frame, 52 bytes on the wire**, whatever the content
height — a thousand rows would send the same 52 bytes — for **1 616
µs/frame of server CPU**, 306 560 damage pixels and 59.8 presented/s, of
which the server's own counters account for 834 µs of paint and 361 of
copy.

The window is 640 × 480 = 307 200 pixels, so **a one-row scroll damages
0.998 of the viewport**. The obvious reading of that — and the one this
document carried, and the one issue #570 was filed on — is that the
server repaints a viewport where `CopyArea` repainted a 640 × 16 = 10 240
pixel line, a factor of **~30** waiting to be reclaimed. **That reading
is wrong, and the measurement that retires it is a different measurement
from the one above.**

The question the damage figure cannot answer on its own is *how many
viewport pixels genuinely differ across a one-row scroll*. Sampling the
paint list per scanline either side of the mutation and comparing the
resolved top colour:

| | px |
|---|---|
| viewport rows that genuinely change | **450 of 480** |
| genuinely-changed pixels | **288 000** |
| `damage_px` reported | **306 560** |
| the "exposed line" the ~30× compared against | 10 240 |

**450 of 480 rows change**, and the 30 that do not are the scenario's own
1-px inter-row gaps (`row_h - 1.0`, one per 16 px, 480 ÷ 16 = 30), which
stay gaps under a 16-px shift. With solid content — a terminal, a file
list — it is 480 of 480. This is obvious once stated: scrolling a column
of *differently coloured* rows past a fixed viewport gives every pixel
its neighbour's colour. A probe at the dead centre of the viewport, 240
px from either edge, shows it directly — the row painting `(320, 240)`
has fill 15 before the mutation and fill 16 after.

So the finding, stated correctly: **the server's damage for a scroll is
306 560 px against a true minimum of 288 000 — 1.06×, within 6 % of
minimal.** The excess is `Damage`'s documented rect-merge policy in
`crates/nitro-core/src/damage.rs` collapsing the region to its bounding
box, which is a trade that crate makes on purpose. There is no 30× of
damage to reclaim, because there is no 30× of damage. **What `CopyArea`
bought was never less damage — it was cheaper pixels: the same area,
moved instead of re-rasterized.**

That also disposes of the fix #570 recommended ("damage the symmetric
difference of a pure translation, not the union"). It is true of a
framebuffer that has already been blitted and false of a scene graph that
has not: damaging the two thin bands would leave **267 520 px stale** —
the whole viewport interior frozen at the previous frame with only the
top and bottom edges animating. It is not a smaller correct answer, it is
an incorrect one, and §8 D records what that would have done to a test.

§9 adds that the figure does not move with the refresh rate — 306 560 px
at 60, 120 and 240 Hz alike, for 1 616 / 1 599 / 1 522 µs/frame — so a
scroll costs the same per frame however often frames happen.

A previous version of this document reported 675 696 px, "2.2
viewports", and explained the excess as the age-2 damage union plus the
shell. That was the scenario's own defect, not the server's: its single
group was set to `(0, offset, w, h + content)` every frame, so the clip
rectangle was taller than the window from frame 0 and **clipped nothing
at all**, leaving the rows to damage area outside the viewport. With a
clipper that actually clips, the number is a clean one-viewport repaint
with no residual to explain away. **What survived that correction was
the measurement, not the inference drawn from it** — worth stating
precisely, because an earlier version of this paragraph said "the
finding *survives in a stronger form*" and, read today, its nearest
referent is the ~30× claim the paragraphs above retire. The clean figure
is what made the *reading* checkable, and checking it is what retired
it.

**The recoverable factor is ~2× on paint, not ~30× on damage**, and it is
filed as **#592** with its ceiling attached. This section's own run puts
`paint_us` at 834 µs and `copy_us` at 361 (#592 quotes 815 and 373 from a
neighbouring sitting; the difference is this box's run-to-run drift, not
a disagreement). A `memmove` of 640 × 464 px XRGB8888 is 27 µs here, so
the paint side is compressible to ~27 µs plus one 640 × 16 exposed band.
The copy side is *not*, without per-buffer translation accumulators: the
scanout buffer handed out at frame *n* was on screen at *n−2*, so serving
it by translation needs a two-frame (32 px) delta plus repair. And
`damage_px_mean` will not move at all — it is `region_area` of
`repaint_region`, which is `damage(n) ∪ damage(n−1)`, the region the
age-2 back buffer is behind by (`crates/nitro-server/src/frame.rs`).
Those pixels genuinely differ between that buffer and the screen and must
be written whatever the scene graph concludes. So roughly 1 616 → ~800
µs/frame: real, worth having, and not 30×.

**One claim in the issue does need narrowing, and it is the issue's best
paragraph.** "A 500-row list and a 50 000-row list scroll for the same
client cost" is true on the wire — one `SetBounds`, 52 bytes, whatever
the height — and false in the server, where the update walk is
proportional to *content* rows rather than visible ones:

| rows | `visited_nodes` | update walk |
|---|---|---|
| 500 | 503 | 22 µs |
| 5 000 | 5 003 | 215 µs |
| 50 000 | 50 003 | **2 138 µs** |

At 50 000 rows the scene walk alone is 2.1 ms/frame, and ~470 of every
500 rows are entirely outside the clip rectangle both before and after.
**No client in this tree pays it**, and that is the load-bearing part:
`nitro-term` materialises screen rows only (slots are `row *
SLOTS_PER_ROW` for `row in 0..grid().rows()`; scrollback lives in the
model, never in the scene), `nitro_ui::List` is explicitly `visible + 2`
"whether the model holds a hundred rows or a hundred thousand", and the
one unvirtualised tall-child shape, `nitro_ui::Scroll`, has a single
in-tree user bounded at `MAX_RESULTS = 20`. The cliff is real and it is
unoccupied; `crates/nitro-scene/src/lib.rs` records it, and
`a_tall_clipped_column_costs_a_walk_per_content_row` in
`crates/nitro-scene/tests/stress.rs` pins the present behaviour so it
fails if anyone makes it worse. Culling the walk was prototyped and
measured during this task and **not taken** — the reasoning is with the
test.

### 7.6 `create` — menus and tooltips are cheap

`x11perf -create/-map`: the one scenario whose *units* survive the
transposition unchanged, because creating a subtree is not a per-frame
paint and "how many per second" really is the question. A retained client
creating and destroying a subtree is the toolkit operation behind opening
a menu, a tooltip or a dialog — where a user notices latency most and
where a benchmark almost never looks.

**153 mutations per frame** — one `DestroyNode`, one group, and 50 rects
with bounds and fills — at **694.4 µs/frame of server CPU**, 60.0
presented/s, 29 281 damage pixels, 3 385 bytes/frame. Creating and
destroying fifty nodes sixty times a second costs the server about 4 % of
a frame budget (694.4 ÷ 16 667): menus, tooltips and dialogs are cheap.
And the cost per frame does not move with the rate — 694.4 µs at 60 Hz
and 693.0 at 120 (§9.8) — so at 120 Hz the same menu costs 8 % of a
budget rather than 4 %, which is the §9.2 result applied to the one
scenario a user meets by clicking something.
The `nodes` counter stands at 96 before the run and 147 during it — the
fifty-one-node subtree, live. And the invariant the scenario really
exists for: **the server's `nodes` count returns to baseline** once the
client disconnects. Every record carries `stats_before` and `stats_after` so this
is checkable on the box, and
`creating_and_destroying_nodes_leaves_the_count_where_it_was` in
`crates/nitro-bench/tests/against_server.rs` pins it against a real
server in CI. One leaked node per opened menu is invisible for an hour
and fatal for a session.

### 7.7 `boing` vs `boing-node` — the headline

The same simulation, the same ball in the same place on the same frame,
run at **both** sizes. One arm recomputes the sphere into the window's
whole buffer every frame — 8 294 400 bytes at 1080p; the other uploads
the sprite **once** and then sends nothing but a new rectangle.

| | `boing` 640×480 | `boing-node` 640×480 | `boing` 1920×1080 | `boing-node` 1920×1080 |
|---|---|---|---|---|
| presented/s | 59.8 | 59.8 | **30.0** | **59.8** |
| client µs/frame | 6 991.6 | **55.7** | **20 833.3** | **111.4** |
| — of which effect | 6 293 | — | 16 903 | — |
| — of which upload (`pwrite`) | 614 | — | 3 880 | — |
| server µs/frame | 3 119.8 | **2 228.4** | 20 500.0 | **10 000.0** |
| damage px | 307 205 | **32 885** | 2 073 600 | **168 868** |
| bytes/frame | 56 | **52** | 56 | **52** |
| verdict | `ok` | `ok` | `**slow**` | `ok` |

At 1080p: **187× less client CPU** (20 833.3 ÷ 111.4), **12.3× less
damage** (2 073 600 ÷ 168 868), **2.1× less server CPU** — and, the part
that is not a ratio, the retained arm **holds 60 Hz where the pixel arm
cannot**. Both arms were run twice, back to back, and reproduced
themselves: 30.0 presented/s both times at 1080p, 20 500.0 and 20 277.8
µs/frame of server CPU; 3 119.8 and 2 924.8 at VGA. (The table quotes the
first of each pair; the second is the within-sitting twin §10 asks for.)
The client ratio is the one to treat as approximate: 111.4 µs is eleven
10 ms CPU ticks over a six-second run (§2), so it means "about a tenth of
a millisecond" and the previous ledger's 83.5 is the same measurement
with one tick's difference. **The ratio is two orders of magnitude and
its third digit is noise.**
This is `DESIGN.md` goal 1, measured. The ball's 389×389
bounding box is **7.3 % of the screen** and the retained arm damages
**8.1 %** of it — the extra being the trailing edge it left behind. The
pixel arm damages 100 %, every frame, for the same ball.

**And the retained arm wins at VGA too**, which is the more careful
claim: 2 228.4 against 3 119.8 µs of server CPU (1.40×), 55.7 against
6 991.6 of client (126×), 32 885 damage pixels against the full 307 205
(9.3×). The 173-pixel ball is 9.7 % of a 640×480 window and the node arm
damages 10.7 % of it — the same shape of result, one sixth the size. So
the pixel arm's defeat at 1080p is not merely "it hit the bandwidth
wall": it loses on every column at a resolution where there is no wall to
hit, and 1080p only widens the margin.

Two honest qualifications. The retained arm's server cost is only
**2.1×** better at 1080p, not 187×: the server still rasterises and
blends a 389×389 alpha sprite into the scene twice per frame (old
position and new), and at 9 572 µs of mean paint that is not nothing. The
dramatic factor is the *client's*, which is the half a battery notices.
And the bytes/frame columns are 56 and 52, which look identical — because
the pixel arm's 8 MB never crosses the socket either; it goes through a
memfd. The wire is not where the pixel path is expensive. Memory is.

And the historical note, which belongs here rather than in a commit
message. **The first version of `boing-node` was four times *worse* than
the pixel arm**, because of a bug in the benchmark. It took `--size` as
the sprite edge and let the simulation pick the ball's radius from the
window; on a fullscreen 1080p run those disagreed — a 128-pixel sprite
drawn into 172-pixel bounds — so **the server resampled the image on
every frame**: 9.7 ms of paint to move one ball. The scenario was
measuring image scaling, and it would have been written up as a finding
about nitro's retained path rather than as the benchmark's own defect.
The box run caught it, because the number was implausible in a direction
the author cared about. The same class of bug hit `putimage`: the first
version stretched every buffer across the whole window, so 100, 250 and
500 px all reported the same ~11 ms of paint and the same 307 200 damage
pixels — a four-point sweep measuring one thing.

The fix in both cases was to make the geometry an *output* of the
scenario rather than an input: `boing-node` now sets its sprite edge from
the simulation's own radius and records the edge it actually uploaded, so
the ledger's `"size":389` (and `"size":173` for the VGA arm) cannot lie
about which one ran — the table itself now prints the window's `WxH`,
since a `size=` on a non-square run was its own small lie. The lesson is
the one this project keeps relearning, and the `nitro-testbox` room has
the best phrasing of it: **the instrument agreed with the code because it
was measuring the layer below the broken one.**

### 7.8 `starfield` vs `starfield-nodes` — the crossover, measured

This pair is in the suite because it is the case where the retained arm
could plausibly lose. Two thousand stars is two thousand `SetBounds` per
frame and two thousand damage rectangles for the server to union; the
buffer arm is one memcpy of the whole screen. Reporting only the case
that flatters the design would be advocacy.

At 1080p it does not lose:

| n | pixels server µs/frame | nodes server µs/frame | ratio | pixels client | nodes client | nodes bytes/frame |
|---|---|---|---|---|---|---|
| 100 | 16 500.0 | **7 186.6** | 2.30× | 9 888.9 | 111.4 | 2 824 |
| 500 | 16 355.9 | **8 750.0** | 1.87× | 9 604.5 | 166.7 | 14 024 |
| 2000 | 16 740.9 | **11 361.1** | 1.47× | 10 195.0 | 333.3 | 56 024 |

The node arm wins at every N measured, and on the client side it is not
close: **333 µs/frame against 10 195 at n=2000**, a factor of 31. Both
arms hold ~60 Hz throughout, so this is entirely a CPU-per-frame story,
which is what §2 said the headline column was for.

**At 640×480 it does lose, and that is the interesting half.** Every pair
now runs at both sizes, and the VGA column has the answer the previous
version of this section could only extrapolate towards:

| n | pixels server µs/frame | nodes server µs/frame | ratio | pixels client | nodes client |
|---|---|---|---|---|---|
| 100 | 2 841.2 | **1 420.6** | 2.00× | 1 169.9 | 83.6 |
| 500 | 3 203.3 | **2 507.0** | 1.28× | 1 504.2 | 139.3 |
| 2000 | 2 833.3 | 6 156.0 | **0.46×** | 1 277.8 | 362.1 |

**The crossover is observed, between n=500 and n=2000 at VGA.** The node
arm is 2× cheaper at a hundred stars, ahead at five hundred, and
**2.2× more expensive at two thousand** (6 156.0 ÷ 2 833.3). Interpolating
linearly between the two node points either side — (500, 2 507.0) and
(2000, 6 156.0), 2.43 µs per star — against the pixel arm's flat 2 959 µs
mean puts the crossing at about **n ≈ 690**; the least-squares line
through all three node points gives 2.48 µs/star and n ≈ 700. Both are
interpolations between measured points rather than extrapolations past
them, which is why they can be quoted at all. (The previous ledger put
the same crossing at n ≈ 640–660 from 2.44–2.48 µs/star — the *slope*
reproduces to within 2 % across two sittings, which is what makes this a
measurement rather than a single evening's number.)

At 1080p the same arithmetic puts the crossing far out of reach: 2.07
µs/star and 7 303 µs of fixed cost from the three node points, meeting
the buffer arm's ~16 532 µs at **n ≈ 4 500**, and the outer two points
alone (500 and 2000) give 1.74 µs/star and n ≈ 5 000. Those *are*
extrapolations, more than twice past the last measurement, so the honest
statement at 1080p remains "somewhere in the low thousands".
`starfield-nodes --n 4000` and `--n 8000` at 1080p would settle it.

**And §9.4 adds a third axis this section did not have: the crossing
moves with the refresh rate.** At 240 Hz the VGA node arm's cost at
n=2000 halves (6 156 → 3 153 µs) while the buffer arm's does not move, so
the two converge to within 8 % and the crossing moves out past n = 2000 —
because a faster display gives a retained scene *less* accumulated damage
per frame. At 720p the node arm does not lose at any N measured.

**The answer depends on resolution in exactly the way the bandwidth
argument predicts**, and that is the finding. The buffer arm's cost is
flat in N and proportional to the *screen*: 2 853 µs at VGA and 16 685 at
1080p, a factor of 5.8 for 6.75× the pixels, while what is on the screen
changes it by under 1 %. The node arm's cost is proportional to the
*stars*, with a floor set by the damage they union to. Shrink the screen
and the buffer arm gets cheap enough to beat; enlarge it and the node arm
wins by more. A widget author's version: **the retained path is the right
one until you have of order a thousand independently moving things in a
small window** — measured at n ≈ 640 for a 640×480 viewport, and n ≈ 5 000
for a 1080p one — and "small" is doing real work in that sentence.

**Damage is not where the win comes from in this pair.** The node arm's
`damage_px_mean` is 2 032 190 at n=100 and the full 2 073 600 at n=500
and above at 1080p, and 303 278 → 309 444 at VGA against a 307 200-pixel
window — essentially the whole screen either way, because a thousand
scattered stars union to the screen almost immediately. (At n=500 and
above the VGA node arm damages *more* than the window holds: the age-2
union again, plus the shell.) So the node arm is not winning by damaging
less; it is winning because **no whole-screen buffer is written by the
client, `pread` back by the server, and composited**. That matters for
predicting a different workload: a retained arm whose motion is
*localised* (the boing ball) wins on damage as well, and wins much
bigger.

The wire cost is the node arm's one real disadvantage and belongs in the
same breath: **56 024 bytes per frame at n=2000 against the buffer arm's
56**. That is 3.4 MB/s of socket traffic at 60 Hz, nothing locally, and a
fact worth remembering for `docs/remote.md`'s TCP path, where two
thousand animated nodes would be 27 Mb/s on the wire — **and 107 Mb/s at
240 Hz**, since the wire cost is per frame and the frames are what the
rate multiplies. That is the one column where a high refresh rate is
straightforwardly bad news for the retained path.

### 7.9 `balls` vs `balls-nodes` — antialiased circles, cheaply

Thirty-two bouncing circles, at both sizes. A rounded rect with
`corners = d/2` **is** an antialiased circle, so the node arm is asking
the server's rounded-rect path for the most expensive per-pixel work in
the whole rect family, thirty-two times a frame.

| | `balls` 640×480 | `balls-nodes` 640×480 | ratio | `balls` 1920×1080 | `balls-nodes` 1920×1080 | ratio |
|---|---|---|---|---|---|---|
| server µs/frame | 2 896.9 | **1 722.2** | **1.68×** | 16 657.4 | **6 044.6** | **2.76×** |
| client µs/frame | 1 448.5 | **55.6** | 26× | 10 195.0 | **83.6** | 122× |
| damage px | 307 304 | **61 003** | 5.04× | 2 073 600 | **835 574** | 2.48× |
| bytes/frame | 56 | 920 | 0.06× | 56 | 920 | 0.06× |
| paint µs mean | 1 490 | 1 194 | 1.25× | 11 663 | 3 743 | 3.12× |

**The server draws 32 antialiased circles, and their motion damage, for
6 045 µs at 1080p — just over a third of what it costs to take one memcpy
of the screen through the pixel path.** Read the damage figure alongside
it: 835 574 pixels, **40.3 % of the screen**, which is what "work
proportional to what changed" looks like when the change is localised but
not *small* — compare the starfield, where the same claim was true of CPU
and not of damage at all. 920 bytes per frame (32 `SetBounds` and the
commit) is the entire wire cost of the animation, at either size.

The damage figures are the ones that moved most between the two ledgers
(508 737 → 835 574 at 1080p, 46 274 → 61 003 at VGA) and the reason is
worth stating rather than smoothing over: `damage_px_mean` includes the
age-2 union of two consecutive frames (§11), so for thirty-two
independently bouncing balls it depends on *where the balls happened to
be* during the sampled window — a spread-out frame unions to far more
than a clustered one. The ratios survive the change in the same direction
and the CPU columns barely move (1 687 → 1 722, 5 595 → 6 045), which is
the right way round: the durable result is the server cost, and the
damage figure is the noisier instrument.

The VGA column is the same result with the bandwidth argument removed:
**1 722 µs against 2 897**, and damage down to 61 003 px — **19.9 % of a
640×480 window**, a *better* proportion than at 1080p because thirty-two
balls in a small window overlap less of it than their 1080p twins cover
of the screen. Unlike the starfield, this pair has no crossover in sight
at either size: thirty-two is nowhere near the density where per-node
cost catches the per-screen one.

### 7.10 `plasma`, `fire`, `rotozoom` — client-bound at 1080p

The three full-surface rewrites: every pixel recomputed every frame.
These are the workloads a compositor built around "work proportional to
change" is worst at, which is precisely why they are in the suite.

| | 640×480 presented/s | 1920×1080 presented/s | 1080p client µs/frame | — of which effect | 1080p server µs/frame |
|---|---|---|---|---|---|
| plasma | 60.0 | **15.0** | **49 333** | **46 815** | 14 444 |
| fire | 59.9 | **30.0** | 22 556 | **19 259** | 16 667 |
| rotozoom | 59.8 | **59.0** | 12 571 | 7 093 | 16 525 |

At 640×480 all three hold 60 Hz comfortably and the server's share is
1 639 / 2 722 / 2 897 µs per frame: the period-correct resolution is a
solved problem on hardware two decades younger than the effects. Their
effects cost 8 647 / 9 007 / 3 609 µs and their uploads 271 / 509 / 572,
so even at VGA these three are already client-dominated — the compositor
is the cheap part of a demoscene effect at every size measured.

At 1080p, **plasma presents 15.0/s and it is not the compositor's
fault**: the client burns 49 333 µs per frame of which **46 815 is the
sine loop**, while the server's 14 444 µs would have fitted in the budget
with 2.2 ms to spare. Its verdict is `**dropped**` rather than `**slow**`
because the server's own worst flip interval rose by 16 666 µs during the
run — one whole extra refresh period — which is what a client presenting
at a quarter rate does to the flip cadence.

**Fire falls to 30.0/s at 1080p**, and it is the row whose story changed
most across ledgers. Its effect is **19 259 µs/frame** — 2.14× its VGA
cost of 9 007 for 6.75× the pixels, which is the sublinear-but-real
scaling a cellular automaton with a serial row dependency should show —
and its `pwrite` is another 3 325, for 22 584 µs of client work against a
16 667 µs budget. The server's 16 667 is no help, but the verdict is
`**slow**` and not `**dropped**`: the flip cadence never broke (rise 0),
there was simply nothing new to show on every other vblank. A previous
version of this document had fire holding 59.9/s with an effect of only
2 933 µs — *less* than the same effect at VGA, for four times the pixels
— and printed that impossible pair without remarking on it. §8 records
why.

Rotozoom is again the only one of the three that holds ~60 Hz at 1080p,
and it does so with no margin at all. Its server cost is **16 525 µs
against a 16 667 µs budget** — 99.1 % of the frame — and it is marked
`ok` because the server never actually missed a flip (rise 0) and
presented 59.0/s. §9.1 is what happens to that row when the budget
halves: it falls to **23.3 presented/s at 120 Hz**, the largest loss in
the sweep. That row is the best single illustration of why this document
exists: fps says "fine", and CPU per frame says "one more window on that
screen and it is not".

> **Superseded 2026-09-20 (#3728):** this row's margin is no longer
> nonexistent. Re-measured on the box after #3728, rotozoom fullscreen
> at 1080p@60 costs **6 889 µs/frame against the 16 667 budget — 41 %,
> not 99.1 %** — with paint 10 963 → 3 951 µs. §7.10b has the table. The
> paragraph is left as measured because the *reasoning* is the durable
> part and §9.1's 120 Hz column is `1f35491`'s; what changed is the
> number, not the lesson.

It gets there by having the cheapest effect of the
three (6 881 µs, a per-pixel gather rather than a transcendental) and
paying the most for its upload (5 411 µs, 43 % of its client frame).

The honest reading of all three: **at 1080p the fullscreen pixel path is
limited by the client's own effect on this CPU, not by nitro** — and that
is itself the argument for the retained path. Two of the three now miss
the refresh, and the one that does not spends its whole server budget to
manage it. The §5 bandwidth wall is visible in the upload column, which
runs 2 570–5 411 µs/frame for these three at 1080p against 283–555 at
VGA: five to ten times the cost for 6.75× the bytes, on a path that
computes nothing. Fire and rotozoom are the rows that will break first at
120 Hz — fire because it already has, and rotozoom because it has nothing
left to give, and §9.1 measures exactly that: fire 30.0 → 24.2/s and
rotozoom 59.0 → 23.3/s at 120 Hz.

### 7.10a #569 follow-up — mapping the buffer, measured either side

Everything above this line is the 1f35491 ledger and is left as measured.
This section is a **separate, paired run** for #569, and its numbers come
from `docs/bench-49d023b.jsonl` — not from editing the tables above.

**What changed.** A client frame used to cross memory three times before
the rasterizer touched it: the effect wrote a heap `Surface`, the client
`pwrite` it into a memfd, and the server `pread` it back into scene-owned
memory. Now the client renders **straight into a mapping of its own
memfd** and the server **maps the same file read-only**, so the frame
crosses once. The enabling condition is sealing: the client creates the
memfd with `MFD_ALLOW_SEALING` and applies `F_SEAL_SHRINK`, `F_SEAL_GROW`
and `F_SEAL_SEAL`, and the server verifies that with `F_GET_SEALS` and
refuses the buffer otherwise — without which a client could shrink the
file under the server's mapping and `SIGBUS` the compositor. The argument
is in `crates/nitro-shm/README.md`.

**How it was measured.** Both arms in **one sitting**, 1920×1080@60, 6 s
per run, `before` = 28b6fdd (main), `after` = 49d023b. Each arm was
deployed and then **md5-verified against the running binary** before its
rows were taken — the §8 lesson about instruments, and the #3704 rule.
`before` ran first, so any warming drift over the sitting counts against
the change rather than for it.

| fullscreen 1080p | upload µs/f | client µs/f | server µs/f | presented/s |
|---|---|---|---|---|
| `balls n=32` | 5 519 → **0** | 9 750 → **4 972** | 12 417 → **8 139** | 60 → 60 |
| `starfield n=500` | 5 848 → **0** | 9 750 → **3 889** | 12 361 → **8 056** | 60 → 60 |
| `rotozoom` | 2 527 → **0** | 10 639 → **9 944** | 9 722 → **6 750** | 60 → 60 |
| `fire` | 3 512 → **0** | 22 167 → **18 191** | 12 389 → **8 404** | 30 → 31 |
| `boing` | 4 399 → **0** | 20 667 → **15 667** | 16 556 → **9 444** | **30 → 60** |
| `plasma` | 2 583 → **0** | 49 333 → **47 333** | 10 333 → **7 667** | 15 → 15 |
| `putimage 1080` | 1 425 → **0** | 27 056 → **25 833** | 3 111 → **1 556** | 30 → 30 |
| `boing-node` *(control)* | 0 → 0 | 83 → 83 | 9 889 → 10 000 | 60 → 60 |

**The shape is the one predicted: two of the three passes disappear.**

- **`upload_us` is 0 on every row**, because there is no upload left to
  time — not a cheaper copy, no copy. The column is kept rather than
  removed so older ledgers go on parsing and so the report shows the cost
  as *gone* rather than silently dropping it.
- **Server CPU per frame falls 13–50 %**, which is the `pread` leaving.
  The biggest proportional win is `putimage` (3 111 → 1 556, a halving):
  it damages a small buffer, so the `pread` was most of what the server
  did. `boing` falls furthest in absolute terms (16 556 → 9 444).
- **Client CPU per frame falls by roughly the old upload**, as it must:
  `balls` 9 750 → 4 972 against an upload of 5 519, `starfield`
  9 750 → 3 889 against 5 848.
- **`boing` doubles its frame rate, 30 → 60/s.** That is the one row where
  the saving crosses a threshold rather than just showing up in a column:
  its client frame cost was 20 634 µs against the 16 667 µs a 60 Hz frame
  allows, so it missed every second vsync; at 15 614 µs it fits. This is
  also why its `paint_us_mean` *rises* (3 089 → 5 486) — the server is
  painting twice as many frames, not painting more slowly.
- **`boing-node` is the control and does not move** (9 889 → 10 000 µs,
  within this box's run-to-run noise). The retained path never paid the
  upload, so a change to the pixel escape hatch must leave it alone — and
  it does.

**What did not change.** `damage_px_mean` is identical on every row
(2 073 600 for the fullscreen arm), so the scenarios are doing the same
work and being measured the same way; the pixels are unchanged, which is
what `crates/nitro-server/tests/shadow.rs`'s screenshot-equality tests
assert in CI. `plasma` at 15/s and `putimage` at 30/s are effect-bound
(47 333 and 25 833 µs/frame of pure compute) and stay exactly where they
were: removing the upload cannot help a client that spends fifty
milliseconds in its own sine loop. #568 is untouched by *this* change and
was the largest remaining term on these rows — until #3728, which is
§7.10b, the next section.

### 7.10b #568 re-measured after #3728 — the 11.5 ms headline is dead, and ns/px fell at both sizes

Everything above §7.10a is the `1f35491` ledger and is left as measured.
This section is a **third, separate sitting**, and its numbers come from
`docs/bench-5e4b02e.jsonl` — not from editing the tables above. The
reason it is added rather than substituted is stated plainly in §7.10b.6.

**Why the re-measure.** #3728 (`7509d73`, "Skip occluded passes and widen
the 1:1 opaque blit store") landed two levers: skipping passes fully
occluded by an opaque 1:1 XR24 image, and an 8-byte widened store in the
1:1 opaque blit. Its commit message records a **dev-machine** figure —
`starfield --n 500 --fullscreen` at 976 µs against ~2 400 — on the *fake*
backend, with no scanout, no bar and no launcher behind it. The box sweep
was deliberately not re-run at the time, correctly, because this
document's provenance rule forbids rewriting tables from a ledger that
does not exist. The consequence was that §8a still argued from
`paint_us_mean` **11 757 µs / 71 % of the frame** — honest, sourced, and
**stale by one lever**.

**Provenance.** 86 scenario runs plus one bandwidth probe, sha
`5e4b02e`, host `ubuntu`, 2026-09-20, one sitting, 6 s per run — the same
run length as the `1f35491` sitting, so run length is not a variable
between the two. Two arms: **1920×1080@60** (54 runs) and **1280×720@240**
(32 runs, the reduced matrix). The ledger carries
`# binaries unchanged across the whole run: c0bb1f5d… 50870c57…` at both
ends, and those md5s were compared against the **local** `target/release`
binaries before the run started — the script's own fingerprint pair
answers "was it mine throughout", and only the local↔box comparison
answers "is it the tree I think it is", which `deploy/bench.sh` cannot
check because it runs on the box. **No `# SKIPPED` arm, no `# FAILED`
row, no `# INVALID` line**; every record's `sha` is `5e4b02e` and none
carries the `-box-checkout` fallback suffix.

**There is no 120 Hz arm in this ledger.** §9's three-rate argument rests
on `1f35491` and is untouched by this section.

#### 7.10b.1 The headline: 11 517 → 5 424 µs, and 69 % → 33 % of the frame

Fullscreen 1920×1080@60, `paint_us_mean` µs. **Pre** is `1f35491`
(pre-#3728); **now** is `5e4b02e`. Both columns are fullscreen rows at
60 Hz in their own ledger — the rate is in the key deliberately, see
§7.10b.7.

| fullscreen 1080p@60 | paint pre | paint **now** | × | copy pre → now | server µs/f pre → now | presented/s pre → now | % of 16 667 µs frame |
|---|---|---|---|---|---|---|---|
| `starfield n=100` | 11 553 | **5 328** | 2.17 | 2 141 → 2 500 | 16 500 → **8 189** | 60.0 → 59.8 | 69 % → **32 %** |
| `starfield n=500` | 11 517 | **5 424** | 2.12 | 2 144 → 2 524 | 16 356 → **8 273** | 59.0 → 59.8 | 69 % → **33 %** |
| `starfield n=2000` | 11 757 | **5 391** | 2.18 | 2 182 → 2 518 | 16 741 → **8 245** | 59.8 → 59.8 | 71 % → **32 %** |
| `balls n=32` | 11 663 | **5 365** | 2.17 | 2 139 → 2 538 | 16 657 → **8 194** | 59.8 → 60.0 | 70 % → **32 %** |
| `rotozoom` | 10 963 | **3 951** | 2.77 | 2 793 → 2 802 | 16 525 → **6 889** | 59.0 → 60.0 | 66 % → **24 %** |
| `fire` | 3 697 | **2 245** | 1.65 | 3 128 → 2 451 | 16 667 → **8 068** | 30.0 → 34.4 | 22 % → **13 %** |
| `plasma` | 3 511 | **1 576** | 2.23 | 2 094 → 2 106 | 14 444 → **7 582** | 15.0 → 15.2 | 21 % → **9 %** |
| `boing` \* | 5 976 | **5 587** | 1.07 | 2 666 → 3 820 | 20 389 → **9 765** | 30.0 → **55.4** | 36 % → **34 %** |
| `boing-node` *(control)* | 9 572 | 9 637 | 0.99 | 205 → 202 | 10 000 → 10 084 | 59.8 → 59.8 | 57 % → 58 % |
| `starfield-nodes n=500` *(control)* | 5 202 | 5 220 | 1.00 | 2 512 → 2 523 | 8 750 → 8 806 | 60.0 → 60.0 | 31 % → 31 % |
| `balls-nodes n=32` *(control)* | 3 743 | 3 778 | 0.99 | 1 013 → 1 019 | 6 045 → 6 100 | 59.8 → 59.8 | 22 % → 23 % |

\* `boing --fullscreen` runs **twice per arm** in both ledgers — once in
the effects loop and once as the pixel arm of the boing pair — so its row
is the mean of the two (pre 5 912/6 041, now 5 659/5 515). §9.6's table
quoted the single 6 041 run. Picking one silently would be the same class
of error as §7.10b.7's.

**So the answer to the headline question is: neither.** Not the 11.5 ms
this document had been claiming, and not the 976 µs the dev box
suggested. **≈5 400 µs**, a 2.1–2.8× improvement on the exact metric #568
was filed against, and paint is now **a third of the 60 Hz frame rather
than 71 %**. The dev figure was never going to transfer: it is a fake
backend with no scanout and no shell clients, and this box runs every row
with the bar, the launcher and the wallpaper up.

**The three retained-path rows are controls and do not move** (0.99–1.00×).
#3728's levers are about an opaque 1:1 image blit, which the node path
never takes, so a change that moved them would have been a change doing
something other than advertised.

**`copy_us_mean` does not improve, and on the damage-saturated rows it
rises slightly** (2 141 → 2 500 on `starfield n=100`). That is the right
shape: #3728 removed *paint* passes, and the shadow→scanout copy is the
same number of bytes over the same write-combined path either way. It is
also why the frame's total is not down by the full paint saving.

#### 7.10b.2 ns/px at both sizes — the question that survives the fix

This is what the task existed to settle. §9.6 established that the
per-pixel excess is **proportional to area rather than fixed per frame**,
so lever A removing *redundant passes* would not, on its own, make the
remaining pass cheaper per pixel. If ns/px had fallen at 1080p only, the
gap would still be open.

`ns/px = paint_us_mean × 1000 ÷ damage_px_mean`, per row, in each row's
own ledger. **The 720p column is at 240 Hz in both ledgers** — that is the
only 720p arm either sitting has, so the size comparison holds the rate
fixed at a rate that differs from the 1080p column. §9.6 carries the same
caveat and it is unchanged here.

| fullscreen | 1080p@60 ns/px pre → **now** | × | 720p@240 ns/px pre → **now** | × |
|---|---|---|---|---|
| `plasma` | 1.69 → **0.76** | 2.23 | 1.49 → **0.82** | 1.81 |
| `fire` | 1.78 → **1.08** | 1.65 | 1.52 → **0.82** | 1.87 |
| `boing` | 2.88 → **2.69** | 1.07 | 2.13 → **1.18** | 1.80 |
| `rotozoom` | 5.29 → **1.91** | 2.77 | 4.93 → **1.86** | 2.65 |
| `starfield n=100` | 5.57 → **2.57** | 2.17 | 5.60 → **2.15** | 2.60 |
| `starfield n=500` | 5.55 → **2.62** | 2.12 | 5.62 → **2.16** | 2.59 |
| `starfield n=2000` | 5.67 → **2.60** | 2.18 | 5.64 → **2.19** | 2.58 |
| `balls n=32` | 5.62 → **2.59** | 2.17 | *(1080p only — no `balls` in the reduced 720p matrix)* | — |
| `boing-node` *(control)* | 56.68 → 57.07 | 0.99 | 57.81 → **38.39** | 1.51 |
| `starfield-nodes n=500` *(control)* | 2.51 → 2.52 | 1.00 | 1.84 → 1.80 | 1.02 |

**ns/px fell at both sizes, on every pixel-arm scenario.** 1.07–2.77× at
1080p and 1.80–2.65× at 720p. The fall is not smaller at the smaller size
— for `boing`, `fire` and the starfields it is *larger* — which is the
shape that distinguishes "the remaining pass got cheaper per pixel" from
"redundant whole-frame passes were removed at the larger frame only". Had
#3728 only skipped passes, the 720p column would have moved less than the
1080p one, not more.

`balls n=32` has no 720p counterpart: the reduced `matrix_720p240` drops
the balls pair. That is a row the 720p arm never ran, not a blank
measurement — saying which is the point of §11.1.

`boing-node` at 720p is the one control that moves (57.81 → 38.39), and
it is not a paint improvement: it damages 75 667 px there, the smallest
damage in the matrix, and it went from 120.8 to 240.0 presented/s between
the sittings. Its ns/px is dominated by per-frame overhead divided by a
tiny area, which is exactly the regime §9.6 says ns/px is the wrong
statistic for. At 1080p, where it damages 168 868 px, it is flat (0.99×).

#### 7.10b.3 The `putimage` fit, recomputed — and why the old one cannot be reused

#568's argument was an extrapolation: fit `putimage`'s damage/paint
points, extrapolate to 2 073 600 px, and compare against what fullscreen
measures. Three different numbers for that extrapolation are in
circulation (the issue's ~3 250 µs, §8a/§9.6's ~3 060 µs, and a
least-squares fit over `1f35491`'s four points giving 2 780 µs), from
different ledgers and different methods.

**The fit has to be recomputed from this ledger, not quoted**, for two
reasons. The first is #3728 itself: `putimage`'s buffer is an opaque 1:1
XR24 image, so lever A speeds the *fit* up too, and comparing a new
fullscreen number against the old fit would be exactly the error this
document is about. The second is that **`putimage`'s damage changed
between the two ledgers for an unrelated reason**: #3726 made the server
clip a window's content to the window, so the sweep's oversized buffers
are now cut at the 640×480 window edge — `size 500` damages 500×480 =
240 000 px where it damaged 250 000, and `size 1080` damages 640×480 =
307 200 px where `1f35491` recorded 613 440. The new arm's lever arm is
therefore **shorter**, and an extrapolation to 2 073 600 px from it
reaches 6.7× beyond its largest point.

Method: ordinary least squares `paint_us = a + b·px` over the `putimage`
rows of one arm, stated with the points used.

| fit | points (px) | `paint_us = a + b·px` | → at 2 073 600 px | fullscreen `starfield n=500` measures | residual |
|---|---|---|---|---|---|
| `1f35491` 60 Hz, all 4 | 10 010 / 62 502 / 250 000 / 613 440 | 206 + 1.241 µs/1000px | 2 780 µs | 11 517 | **4.14×** |
| `5e4b02e` 60 Hz, all 4 | 10 000 / 62 500 / 240 000 / 307 200 | 11 + 0.919 µs/1000px | **1 916 µs** | 5 424 | **2.83×** |
| `1f35491` 60 Hz, 3 small | 10 010 / 62 502 / 250 000 | 96 + 2.520 µs/1000px | 5 320 µs | 11 517 | 2.16× |
| `5e4b02e` 60 Hz, 3 small | 10 000 / 62 500 / 240 000 | 9 + 0.948 µs/1000px | 1 975 µs | 5 424 | 2.75× |

And at 720p (target 921 600 px), where the arm has three points and tops
out at `size 720`:

| fit | points (px) | → at 921 600 px | fullscreen measures | residual |
|---|---|---|---|---|
| `1f35491` 240 Hz | 10 000 / 250 000 / 380 160 | 941 µs | 5 177 | **5.50×** |
| `5e4b02e` 240 Hz | 10 000 / 240 000 / 307 200 | **425 µs** | 1 995 | **4.69×** |

**Read these as bounds, not as a single number.** The residual is
sensitive to which points enter the fit — 4.14× vs 2.16× on the *same*
pre-fix ledger, purely from dropping the largest point — which is the
honest reason #568's "~3.8×" and §8a's "~3 060 µs" and the issue's
"~3 250 µs" never agreed. The slope is the durable part, and it is
unambiguous: **1.241 → 0.919 µs/1000 px at 60 Hz (1.35× cheaper per
pixel), 0.800 → 0.230 at 240 Hz**, measured on rows whose damage the
window clip *shrank*, which if anything understates the gain.

A residual of 2.8× remains at 1080p. It is smaller than the 4.1× the same
method gave pre-fix, and it is measured against a fit whose largest point
is now 307 200 px, so the extrapolation is six times beyond its data. **It
is not evidence of a further 2.8× lever**; it is what a long extrapolation
from a short lever arm looks like when the two regimes differ (a 640×480
window's buffer versus a full-surface composite). Closing that question
properly needs a `putimage` sweep whose points reach fullscreen area —
which this matrix cannot produce, because #3726 now clips them to the
window.

#### 7.10b.4 Verdict on #568

**The gap is closed and the issue dies.**

The two things #568 asked for are both answered. The headline —
`paint_us_mean` 11 757 µs, 71 % of the 60 Hz frame — is **5 328–5 424 µs
and 32–33 %**, a 2.1–2.8× improvement on the exact metric. And the
per-pixel question that survived the fix is answered the way a real fix
answers it: **ns/px fell at 1080p *and* at 720p, on every pixel-arm
scenario**, 1.07–2.77× and 1.80–2.65× respectively. Lever A did not
merely remove redundant passes at the larger frame; the remaining pass is
genuinely cheaper per pixel.

What remains is a *measurement* limitation rather than a known cost: the
residual between the `putimage` fit and the fullscreen rows is 2.8× at
1080p, down from 4.1× by the same method, and the fit's lever arm is now
too short to carry an extrapolation that far. That is not the 3.8×
per-pixel gap #568 was filed about, and re-filing it would be inventing a
finding out of an artefact of the fitting method. It is recorded here and
in §9.6 so a future sweep can pick it up if it ever matters.

Attribution, stated because the pre-ledger predates more than one lever:
`1f35491` is 55 commits before `5e4b02e`, and two of them touch the pixel
path — #3728 and #3754 (mapping sealed client buffers, `1bce188`). The
`49d023b` ledger brackets them, its `28b6fdd` arm being post-#3728 and
pre-#3754, so the two are separable and the answer is unambiguous:

| fullscreen 1080p@60, `paint_us_mean` | `1f35491` pre-#3728 | `28b6fdd` +#3728 | `49d023b` +#3754 | `5e4b02e` now |
|---|---|---|---|---|
| `starfield n=500` | 11 517 | **5 874** | 5 264 | 5 424 |
| `balls n=32` | 11 663 | **5 815** | 5 345 | 5 365 |
| `rotozoom` | 10 963 | **3 984** | 3 850 | 3 951 |
| `plasma` | 3 511 | **1 523** | 1 571 | 1 576 |

**The entire drop happens at #3728**, and #3754 leaves `paint_us_mean`
flat — as it must, since it removed the client's upload and the server's
`pread`, neither of which is paint. The credit for this section's headline
belongs to #3728 alone.

#### 7.10b.5 What this sitting does not show

- **No 120 Hz arm.** §9's rate argument is `1f35491`'s and is not
  re-measured here. A fullscreen 1080p paint of 5 424 µs now fits inside
  a 8 333 µs 120 Hz frame where 11 517 did not, but that is arithmetic
  against a budget, **not a measurement at 120 Hz**, and §9.1's
  bandwidth argument (three passes over the frame at 2.98 GB/s of an
  available 3.56) is a separate ceiling this section does not touch.
- **`balls` has no 720p row**, as above.
- **The two sittings are four days apart** (2026-09-16 and 2026-09-20) on
  a box whose numbers drift. The `boing`/`fire` presented/s differences
  are partly #3754's client-side saving, not paint. The within-sitting
  controls are what carry the claim: three retained-path rows flat to
  within 1 %.
- Standing caveat, unchanged: 2-core Haswell, SSE4.2, **no AVX2**. The
  *ratio* travels; the absolute microseconds are this box's. Measured
  `copy` bandwidth this sitting was 3.56 GB/s against 3.43 on the
  `1f35491` sitting — a 4 % drift that bounds how much of any small
  difference above is real.

#### 7.10b.6 Why this is a new section and not a regeneration

`just bench-report docs/bench-5e4b02e.jsonl` reproduces every table in
this section, and the tables in §7.10b.8 are that tool's output pasted
verbatim. They are **added** rather than swapped into §6/§9.8 for two
reasons:

1. **This ledger has no 120 Hz arm.** Regenerating §6 and §9.8 from it
   would *delete* the column §9.1, §9.2, §9.3 and §9.5 argue from —
   replacing one stale claim with four newly-unsupported ones.
2. **Several hundred prose numbers in §7.1–§7.10 are quoted from
   `1f35491` rows.** Regenerating the tables beneath them would leave
   every one of those paragraphs silently contradicting the table above
   it. An honestly-labelled stale table beats a document that disagrees
   with itself.

§7.10a set the precedent one lever earlier and states it in its first
line. This is the same shape.

#### 7.10b.7 An instrument bug, in the §9.7 tradition — committed in the analysis, not the run

The first version of the script that produced §7.10b.1 keyed its
before/after rows on `(scenario, n, width, height)` and **silently
reported the 120 Hz numbers as the "pre" column**. `1f35491` holds
fullscreen 1080p rows at *both* 60 and 120 Hz; with rate absent from the
key, the 120 Hz arm — which is read second — overwrote the 60 Hz one.
Every "pre" figure was plausible, internally consistent, and about the
wrong arm: `balls n=32` read 12 401 instead of 11 663, and `rotozoom`
4 946 instead of 10 963, which would have turned a 2.77× win into a
1.25× one.

**This is `report.rs`'s `rate_key` collision, reproduced in the analysis
of the ledger it warns about** (`report.rs:164` drops `size` for
fullscreen runs and keys on rounded hertz; `:214` is last-run-wins). It
was caught by adding rate to the key and asserting the key is unique —
at which point the assert fired again, on `boing --fullscreen`, which
legitimately runs twice per arm. Both are now handled explicitly and the
duplicate is disclosed in §7.10b.1's footnote rather than resolved by
whichever row happened to be last.

The same trap decided the run's shape: a `1280x720@60` arm was **not**
added to this ledger, because a 720p@60 fullscreen row and a 1080p@60
fullscreen row are the same `rate_key`, and `just bench-report` would
have replaced every 1080p row in §7.10b.8 with its 720p twin — producing
a complete, plausible, entirely wrong table. The 720p arm is at 240 Hz,
which is also the only rate `1f35491` has 720p rows at.

#### 7.10b.8 The generated tables for this ledger

From `cargo run -q -p nitro-bench -- report docs/bench-5e4b02e.jsonl`,
pasted verbatim; the fullscreen-relevant scenarios only. **Two arms: 60 Hz
at 1920×1080 and 240 Hz at 1280×720.** Rows labelled `640x480` are the
period-correct arm and run at both rates; rows labelled `1920x1080` are
the 60 Hz arm's fullscreen and `1280x720` the 240 Hz arm's.

#### putimage

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| putimage 640x480 | 59.8 | 60.0 | 1.0 | 306.4 | 640.7 | 16.0/19.0 | 10.0 | 10000 | 56 | 0 | ok |
| putimage 640x480 | 59.8 | 60.0 | 1.0 | 445.7 | 3788.3 | 72.0/94.0 | 84.0 | 62500 | 56 | 0 | ok |
| putimage 640x480 | 60.0 | 60.0 | 1.0 | 694.4 | 9750.0 | 236.0/271.0 | 205.0 | 240000 | 56 | 0 | ok |
| putimage 640x480 | 29.8 | 30.0 | 1.0 | 1508.4 | 26033.5 | 289.0/606.0 | 372.0 | 307200 | 56 | 0 | **slow** |
| putimage 640x480 | 240.0 | 240.0 | 1.0 | 298.6 | 645.8 | 16.0/18.0 | 10.0 | 10000 | 56 | 0 | ok |
| putimage 640x480 | 120.0 | 120.0 | 1.0 | 625.0 | 4777.8 | 69.0/303.0 | 172.0 | 240000 | 56 | 0 | **slow** |
| putimage 640x480 | 60.0 | 60.0 | 1.0 | 1052.6 | 12382.3 | 176.0/377.0 | 265.0 | 307200 | 56 | 0 | **slow** |

#### plasma

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| plasma 640x480 | 59.8 | 60.0 | 1.0 | 640.7 | 8607.2 | 234.0/266.0 | 269.0 | 307200 | 56 | 0 | ok |
| plasma 1920x1080 | 15.2 | 15.0 | 1.0 | 7582.4 | 46703.3 | 1576.0/3219.0 | 2106.0 | 2073600 | 56 | 16666 | **dropped** |
| plasma 640x480 | 120.1 | 119.9 | 1.0 | 873.8 | 5936.2 | 120.0/268.0 | 245.0 | 307200 | 56 | 0 | **slow** |
| plasma 1280x720 | 40.0 | 40.0 | 1.0 | 3541.7 | 20166.7 | 760.0/1583.0 | 903.0 | 921600 | 56 | 0 | **slow** |

#### fire

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fire 640x480 | 59.7 | 59.9 | 1.0 | 1197.8 | 8969.4 | 434.0/522.0 | 431.0 | 307200 | 56 | 0 | ok |
| fire 1920x1080 | 34.4 | 34.4 | 1.0 | 8067.6 | 18357.5 | 2245.0/4928.0 | 2451.0 | 2073600 | 56 | 0 | **slow** |
| fire 640x480 | 240.0 | 240.0 | 1.0 | 659.7 | 2465.3 | 258.0/281.0 | 273.0 | 307200 | 56 | 0 | ok |
| fire 1280x720 | 73.2 | 73.0 | 1.0 | 3553.5 | 9430.5 | 753.0/1576.0 | 891.0 | 921600 | 56 | 0 | **slow** |

#### rotozoom

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rotozoom 640x480 | 59.8 | 60.0 | 1.0 | 1142.1 | 3649.0 | 416.0/459.0 | 418.0 | 307200 | 56 | 0 | ok |
| rotozoom 1920x1080 | 60.0 | 60.0 | 1.0 | 6888.9 | 9250.0 | 3951.0/6013.0 | 2802.0 | 2073600 | 56 | 0 | ok |
| rotozoom 640x480 | 240.0 | 240.0 | 1.0 | 715.3 | 1618.1 | 247.0/277.0 | 293.0 | 307200 | 56 | 0 | ok |
| rotozoom 1280x720 | 235.9 | 235.9 | 1.0 | 3241.5 | 3170.9 | 1713.0/1749.0 | 1356.0 | 921600 | 56 | 0 | ok |

#### boing

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing 640x480 | 59.8 | 60.0 | 1.0 | 1420.6 | 6378.8 | 668.0/1045.0 | 448.0 | 307200 | 56 | 0 | ok |
| boing 1920x1080 | 59.2 | 59.2 | 1.0 | 9691.0 | 15814.6 | 5659.0/5765.0 | 3826.0 | 2073600 | 56 | 0 | ok |
| boing 640x480 | 59.8 | 59.9 | 1.0 | 1309.2 | 6267.4 | 561.0/949.0 | 429.0 | 307205 | 56 | 0 | ok |
| boing 1920x1080 | 51.5 | 51.5 | 1.0 | 9838.7 | 16161.3 | 5515.0/5660.0 | 3814.0 | 2073600 | 56 | 0 | ok |
| boing 640x480 | 239.8 | 239.9 | 1.0 | 681.0 | 2223.8 | 257.0/382.0 | 278.0 | 307200 | 56 | 0 | ok |
| boing 1280x720 | 120.2 | 120.0 | 1.0 | 4771.2 | 6352.3 | 1088.0/2250.0 | 1215.0 | 921600 | 56 | 0 | **slow** |

#### boing-node

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing-node 640x480 | 60.0 | 60.0 | 1.0 | 2277.8 | 55.6 | 2021.0/2097.0 | 43.0 | 32880 | 52 | 0 | ok |
| boing-node 1920x1080 | 59.8 | 60.0 | 1.0 | 10083.6 | 55.7 | 9637.0/9731.0 | 202.0 | 168868 | 52 | 0 | ok |
| boing-node 640x480 | 240.0 | 240.0 | 1.0 | 2256.9 | 62.5 | 2014.0/2045.0 | 43.0 | 33061 | 52 | 0 | ok |
| boing-node 1280x720 | 240.0 | 240.0 | 1.0 | 3104.2 | 48.6 | 2905.0/2964.0 | 63.0 | 75667 | 52 | 0 | ok |

#### starfield

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield n=100 640x480 | 59.8 | 60.0 | 1.0 | 1337.0 | 473.5 | 663.0/1317.0 | 430.0 | 307200 | 56 | 0 | ok |
| starfield n=100 1920x1080 | 59.8 | 60.0 | 1.0 | 8189.4 | 3899.7 | 5328.0/5631.0 | 2500.0 | 2073600 | 56 | 0 | ok |
| starfield n=500 640x480 | 59.8 | 60.0 | 1.0 | 1197.8 | 445.7 | 511.0/1008.0 | 419.0 | 307200 | 56 | 0 | ok |
| starfield n=500 1920x1080 | 59.8 | 60.0 | 1.0 | 8273.0 | 4094.7 | 5424.0/5787.0 | 2524.0 | 2073600 | 56 | 0 | ok |
| starfield n=2000 640x480 | 60.0 | 60.0 | 1.0 | 1305.6 | 611.1 | 603.0/1086.0 | 443.0 | 307200 | 56 | 0 | ok |
| starfield n=2000 1920x1080 | 59.8 | 60.0 | 1.0 | 8245.1 | 4178.3 | 5391.0/5736.0 | 2518.0 | 2073600 | 56 | 0 | ok |
| starfield n=100 640x480 | 240.0 | 240.0 | 1.0 | 1215.3 | 423.6 | 519.0/839.0 | 427.0 | 307200 | 56 | 0 | ok |
| starfield n=100 1280x720 | 239.8 | 240.0 | 1.0 | 3224.5 | 1633.1 | 1986.0/2045.0 | 1040.0 | 921600 | 56 | 0 | ok |
| starfield n=500 640x480 | 240.2 | 240.2 | 1.0 | 1214.4 | 451.1 | 528.0/685.0 | 427.0 | 307200 | 56 | 0 | ok |
| starfield n=500 1280x720 | 240.2 | 240.2 | 1.0 | 3220.0 | 1644.7 | 1995.0/2056.0 | 1042.0 | 921600 | 56 | 0 | ok |
| starfield n=2000 640x480 | 240.2 | 240.2 | 1.0 | 1242.2 | 576.0 | 573.0/944.0 | 430.0 | 307200 | 56 | 0 | ok |
| starfield n=2000 1280x720 | 240.0 | 240.0 | 1.0 | 3236.1 | 1715.3 | 2015.0/2054.0 | 1049.0 | 921600 | 56 | 0 | ok |

#### starfield-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield-nodes n=100 640x480 | 59.8 | 60.0 | 100.0 | 1392.8 | 55.7 | 646.0/700.0 | 362.0 | 301192 | 2824 | 0 | ok |
| starfield-nodes n=100 1920x1080 | 59.8 | 60.0 | 100.0 | 7130.9 | 111.4 | 4464.0/4777.0 | 2498.0 | 2032190 | 2824 | 0 | ok |
| starfield-nodes n=500 640x480 | 60.0 | 60.0 | 500.0 | 2500.0 | 138.9 | 1164.0/1244.0 | 380.0 | 307200 | 14024 | 0 | ok |
| starfield-nodes n=500 1920x1080 | 60.0 | 60.0 | 500.0 | 8805.6 | 166.7 | 5220.0/5381.0 | 2523.0 | 2073600 | 14024 | 0 | ok |
| starfield-nodes n=2000 640x480 | 59.8 | 60.0 | 2000.0 | 6100.3 | 362.1 | 3012.0/3094.0 | 362.0 | 307200 | 56024 | 0 | ok |
| starfield-nodes n=2000 1920x1080 | 60.0 | 60.2 | 2000.0 | 11416.7 | 333.3 | 6541.0/6912.0 | 2340.0 | 2073600 | 56024 | 0 | ok |
| starfield-nodes n=100 640x480 | 240.0 | 240.0 | 100.0 | 1416.7 | 90.3 | 620.0/703.0 | 354.0 | 285845 | 2824 | 0 | ok |
| starfield-nodes n=100 1280x720 | 240.0 | 240.0 | 100.0 | 3111.1 | 97.2 | 1640.0/2101.0 | 958.0 | 859774 | 2824 | 0 | ok |
| starfield-nodes n=500 640x480 | 240.2 | 240.2 | 500.0 | 2463.6 | 152.7 | 1169.0/1190.0 | 379.0 | 307200 | 14024 | 0 | ok |
| starfield-nodes n=500 1280x720 | 240.0 | 240.0 | 500.0 | 3145.8 | 111.1 | 1661.0/1716.0 | 810.0 | 921600 | 14024 | 0 | ok |
| starfield-nodes n=2000 640x480 | 239.7 | 239.8 | 2000.0 | 3108.5 | 187.8 | 1535.0/1571.0 | 278.0 | 307200 | 56024 | 0 | ok |
| starfield-nodes n=2000 1280x720 | 233.8 | 234.0 | 2000.0 | 4048.5 | 206.7 | 2201.0/2527.0 | 827.0 | 921600 | 56024 | 0 | ok |

#### balls

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls n=32 640x480 | 59.8 | 60.0 | 1.0 | 1281.3 | 752.1 | 626.0/1070.0 | 420.0 | 307200 | 56 | 0 | ok |
| balls n=32 1920x1080 | 60.0 | 60.0 | 1.0 | 8194.4 | 5027.8 | 5365.0/5668.0 | 2538.0 | 2073600 | 56 | 0 | ok |

#### balls-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls-nodes n=32 640x480 | 59.8 | 60.0 | 32.0 | 1754.9 | 55.7 | 1207.0/1370.0 | 79.0 | 61003 | 920 | 0 | ok |
| balls-nodes n=32 1920x1080 | 59.8 | 60.0 | 32.0 | 6100.3 | 83.6 | 3778.0/4447.0 | 1019.0 | 835574 | 920 | 0 | ok |

## 8. The `flip rise` column, and a lesson about instruments

`flip_interval_max_us` is the server's **cumulative, all-time maximum**
flip interval. It never decays, it is not windowed, and it is not reset
between clients, so its *value* says nothing about any particular run.

The first version of the verdict logic read `stats_after` and compared it
to 1.5 frame budgets. It marked **25 of 45 runs as having dropped
frames** while they presented a flat 59.9/s with a mean flip interval of
17 ms. It was not measuring those runs at all: it was measuring which
scenario had run *earlier* in the same server process and left a 33 ms
spike behind. In this final ledger the effect is starker still — 53 of
the 54 rows carry an all-time maximum above the 1.5-budget threshold,
inherited from a handful of genuinely bad frames, while only **two** rows
have a non-zero rise at all (`plasma 1920x1080` +16 668, and the control
`rects n=500` +9 after a server restart reset the counter to 16 666).

The test is now on the **rise**: `flip_interval_max_us` after minus
before, so a run is only blamed for a gap it actually caused. And the
column prints the rise, so a reader who wants to argue with the 1.5
threshold has the measurement it was applied to rather than a verdict to
take on trust. A `0` there means something precise and slightly awkward —
"this run's worst interval was no worse than something earlier in the
session" — which is a weaker claim than "it was fine", and saying so is
the point of printing it.

The same session produced a second instrument failure of the same family.
The bandwidth probe's first version fed each round's result into a
checksum, on the theory that a store whose value is observed cannot be
deleted. It was not enough: LLVM proved the copy loop's source was a
constant fill, folded the whole loop away, and the probe reported **zero
microseconds — `inf GB/s`**, a spectacular result for no work at all. The
fix is `std::hint::black_box` around the buffers inside the loop, the
only thing in stable Rust that actually promises this, with the checksum
kept as corroboration. `Bandwidth::bytes_per_second` now returns 0.0
rather than an infinity for a zero-microsecond measurement, because
`0.00 GB/s` next to a working `read` row makes a reader ask the right
question and `inf` makes them scroll past.

**And then a reviewer found three more, in the scenarios themselves, and
one of them reversed a finding in this document.** They are the same
family again, and they are the reason §7.1, §7.2, §7.5 and §7.10 read
differently than they did.

- **A — a fullscreen effect simulating the wrong size.** A `--fullscreen`
  pixel run constructs its effect before it knows the screen: the real
  size arrives only with the server's `Configure`, and nothing rebuilt
  the effect when it did. So the buffer was 1920×1080 and correct, and
  the *simulation inside it* was the command line's default 640×480 —
  fire burning in the top-left quadrant of an 8 MB buffer with the rest
  left at the zero-fill, boing and the sparse pairs bouncing in a VGA
  world while their retained twins used the whole screen. `Effect` now
  has a `resize` hook that `build` calls with the configured size, and
  the test asserts on *ink in the far corner* rather than on the
  surface's dimensions — the dimensions were right all along, which is
  exactly why the existing test passed. **The evidence was in the
  published ledger and the author read past it: fullscreen fire reported
  2 933 µs of compute against VGA fire's 8 592 — four times the pixels
  for a third of the cost, which cannot be true of any effect.** It took
  a reviewer to look at a number that had been printed, shipped and
  quoted in prose. (Both figures are from the *previous* ledger; in this
  one VGA fire computes 8 751 µs.) Fullscreen fire now computes 17 127 µs
  and presents 30.1/s.
- **B — a "moving" arm that moved on half its frames.** `rects-move`
  cycled eight offsets that collapsed to four positions in consecutive
  pairs, and `Scene::set_bounds` early-returns on unchanged bounds, so
  half its `SetBounds` were server-side no-ops — against a recolour arm
  that changed every node on every frame. **§7.2's finding was drawn from
  that gap and was the exact opposite of the truth**: the document
  reported moving as *cheaper* than recolouring at every sweep point,
  and with a genuine four-corner orbit it is consistently slightly dearer.
  A published, confidently argued, plausible finding, reversed by a
  four-line fix to the instrument. There is no stronger statement of why
  the instrument must be checked than that, and it is why this section
  exists.
- **C — a clipping group that clipped nothing.** `scroll`'s per-frame
  mutation resized its single group to `(0, offset, w, h + content)`, so
  from frame 0 the clip rectangle was taller than the window. The rows
  were bounded only by the output, and the scenario measured a scene
  nobody had described: 675 696 damage pixels, "2.2 viewports", with the
  excess explained away in prose as the age-2 union and the shell. A
  fixed clipper at the viewport with a moving content group inside it
  gives 304 768 — 0.99 of the viewport — so the **number** came out
  cleaner, with nothing left to explain away (§7.5). The reading built on
  that number did not survive, and D below is where it is retired: a
  clean measurement is what made the inference checkable, not evidence
  that the inference was right.
- **D — an instrument that was right, and a reading of it that was not.**
  The three above are instrument defects: a scenario measuring a
  different scene from the one its name described. This one is not, and
  that is why it is worth the space. `scroll`'s 306 560 damage pixels
  were **correct** — they reproduce exactly, at every content size and
  every refresh rate. What was wrong was the inference drawn from them:
  that a viewport's worth of damage to move content by one row meant the
  server was repainting ~30× more than it needed to, because `CopyArea`
  repainted only the exposed 640 × 16 line. Nobody checked the quantity
  that claim rests on — *how many viewport pixels genuinely differ* — and
  when it is measured it is **450 of 480 rows, 288 000 px**, against
  306 560 damaged. The server was within 6 % of minimal the whole time,
  and 10 240 px is what `CopyArea` *repainted*, not what changed; the
  ~30× compared two different quantities (§7.5).

  **The part worth keeping is what the proposed proof would have done.**
  #570 named its own acceptance test: damage the symmetric difference of
  the translation instead of the union, and assert `damage_px_mean` falls
  from ~304 768 to ~20 000. That test would have **passed on a visibly
  corrupt screen** — 267 520 px of viewport interior frozen at the
  previous frame, only the top and bottom bands animating — because the
  metric it asserts on is a proxy for "the server is not doing
  unnecessary work", and a server that does not do *necessary* work
  satisfies it beautifully. A test that certifies the bug is worse than
  no test.

  That is the third time this batch, and the sibling cases make the rule
  rather than the anecdote: **#566**'s parity-dependent bound, and
  **#584**'s one-screenshot test that proved *a* frame arrived but not
  that frames *keep* arriving. All three assert that a proxy metric
  moved, rather than the property the metric was standing in for. The
  rule, and the better probe in each case:

  > **Assert the property, not the proxy.** If the assertion can be
  > satisfied by a system that is broken in the direction the metric does
  > not look, it is not a test of the property.

  For #584 the property is *frames keep arriving*, so the probe is two
  screenshots separated in time, not one. For #566 it is the bound
  holding at both parities, not at the one sampled. For #570 the property
  is *the walk does not touch what it cannot paint*, and the probe is
  `visited_nodes` (`UpdateStats`, `crates/nitro-scene/src/scene.rs`) —
  which, unlike `damage_px_mean`, **cannot be satisfied by a corrupt
  screen**, because it counts work done rather than pixels claimed.

  This rule has a sibling one level out. Here the assertion is on the
  wrong *quantity* — a proxy for the property. It can also be on exactly
  the right quantity and read the wrong *artefact*: the other file, the
  other parity, the second frame. That is §11.1.


Four properties, four regression tests, and one rule they share: each
test asserts the property the bad number violated, not the shape of the
code that produced it. Note also what none of them were: a wrong
formula, a mis-parsed field, a unit error. A, B and C were each a
scenario quietly measuring a *different scene* from the one its name and
this document described, while every counter around it stayed
self-consistent. **D is the one that was not an instrument defect at
all** — the counter was right, the scene was the one described, and the
error was entirely in the sentence the number was turned into. It is the
harder failure to catch, because there is nothing in the code to find.

**Those five** — the flip-interval verdict, the folded bandwidth loop, and
A, B and C — are one lesson, and it is the one this project keeps
relearning: from `docs/latency.md`'s saturated-display trap, from the
`boing-node` resampling bug in §7.7, and from #3711:

> **The instrument agreed with the code because it was measuring the
> layer below the broken one.**

An eliminated loop does not report an error. It reports a great result.
Neither does a fire burning in a corner of its buffer, or a rect that
never moved: they report a number, and the number goes in a table, and
the table goes in a document like this one.

**D is the exception that makes the rule precise, and it is the more
uncomfortable case.** There the instrument did *not* agree with the code
by measuring the layer below the broken one — there was no broken layer.
The number was right, and it was turned into a sentence that did not
follow from it, and the sentence was quoted onward into an issue, a table
row and a recommended fix. Checking the instrument would not have caught
it; only measuring the quantity the claim actually rested on did. So the
two halves are worth stating together: **verify that the instrument
measures the scene you described, and then verify that the conclusion
measures the quantity it names.**

## 8a. The three worst numbers, filed

A benchmark that ends in a document is half a benchmark. The three
findings below were the worst numbers in the matrix and each was filed as
an issue, so that a future run has something to close rather than a
paragraph to re-read. **All three are now closed, and they closed in
three different ways**: #569 by a change that made the cost go away,
#570 by a measurement that showed the finding was a misreading of a
correct number, and #568 by a fix (#3728) whose effect had to be
**re-measured on the box** before it could be believed — the dev-machine
number in its commit message was a fake backend with no scanout, and the
row below carried a stale headline for four days because this document
will not rewrite a table from a ledger that does not exist. §7.10b is
that ledger. The second and third kinds are worth filing for too.

| issue | the number | what it is |
|---|---|---|
| **#568** | ~~`paint_us_mean` **11 757 µs** for a fullscreen 1080p repaint — **71 %** of the 60 Hz frame~~ → **5 328–5 424 µs, 32–33 % of the frame** | **Fixed by #3728, re-measured on the box in §7.10b** (`docs/bench-5e4b02e.jsonl`). The finding was real and is stated below as it was measured; the numbers it rests on are the pre-#3728 ledger and the row above is the correction. Original: fullscreen rows across three scenarios — rotozoom, starfield at all three N, balls — do genuinely different work per frame (a per-pixel gather, two thousand moving stars, thirty-two circles) and reported near-identical server costs: **16 525–16 741 µs, paint 10 963–11 757**, because the server's work is a function of damaged area alone. A least-squares fit through `putimage`'s four damage/paint points extrapolated to ~3 060 µs at 2 073 600 px against the 11 000–11 800 measured, so **a fullscreen repaint was ~3.8× more expensive per pixel than a large partial one** — and §9.6 added the constraint that the excess is *proportional to area rather than fixed per frame*. **What closed it:** #3728 skipped passes fully occluded by an opaque 1:1 XR24 image and widened the 1:1 opaque blit's store, which is precisely a per-pixel-path change at full-surface damage — the thing §9.6 said to profile. Paint fell **2.1–2.8×** at 1080p, and the question that survived the fix is answered too: **ns/px fell at *both* sizes** — 1.07–2.77× at 1080p and 1.80–2.65× at 720p — so the remaining pass is genuinely cheaper per pixel rather than merely run fewer times. The residual against a recomputed `putimage` fit is 2.8×, down from 4.1× by the same method, and is now dominated by the fit's lever arm (#3726 clips the sweep's buffers to the window, so its largest point is 307 200 px); §7.10b.3 says why that is a fitting artefact and not a further lever. |
| **#569** | ~~`upload_us` **2 570–6 044 µs/frame** at 1080p~~ → **0**, and the server's `pread` with it | **Fixed.** The client now renders straight into a mapping of its own sealed memfd and the server maps the same file read-only, so two of the frame's three passes over the pixels are gone. Measured either side in one sitting: `docs/bench-49d023b.jsonl` and §7.10 below. `upload_us` is 0 on every row, server CPU/frame falls 13–50 %, and `boing` crosses the 60 Hz budget and doubles to 60 fps. The seal check (`F_SEAL_SHRINK`, `F_SEAL_GROW`, `F_SEAL_SEAL`, verified with `F_GET_SEALS`) is what makes the mapping sound against a hostile client; `crates/nitro-shm/README.md` carries the argument and the residuals. Scope was always the escape hatch: the retained path never paid this (§7.7's 83.5 µs) and is unchanged — `boing-node` is the control row and does not move. |
| **#570** | ~~a one-row scroll damages **304 768 px = 0.99× the viewport**, a factor of ~30 against `CopyArea`~~ → **damage is within 6 % of minimal; the finding was a misreading** | **Retired, and the measurement is the interesting part.** The number was right and the reasoning was wrong. A one-row scroll of heterogeneous content genuinely changes **450 of 480 viewport rows = 288 000 px** (the 30 that do not are the scenario's own 1-px inter-row gaps; with solid content it is 480 of 480), because shifting differently-coloured rows past a fixed viewport gives every pixel its neighbour's colour. Reported damage is **306 560 px against a true minimum of 288 000 — 1.06×**, the excess being `Damage`'s documented rect-merge policy (`crates/nitro-core/src/damage.rs`). So there is no 30× to reclaim: **what `CopyArea` bought was not less damage but cheaper pixels**, and the recommended fix (damage the symmetric difference) would have left 267 520 px stale — see §8 D, because the test it proposed as proof would have certified the bug. The real prize is ~2× on `paint_us`, filed as **#592** with its ceiling attached, and `damage_px_mean` cannot move at all: it is `damage(n) ∪ damage(n−1)`, pixels that genuinely differ from the age-2 back buffer. §7.5 carries the full argument. (The figure was 675 696 px in an earlier ledger, from a clipping group that clipped nothing.) |

**#568 was about the server's cost being proportional to damaged area**,
and that is a statement about the *rasterizer's* per-pixel path at full‑
surface damage, not about the scene graph's arithmetic. Reading it that
way is what made it fixable: #3728 changed the per-pixel path at
full-surface damage and the cost fell 2.1–2.8× (§7.10b). An earlier
version of this paragraph generalised it to two of the three — "the
damage it computes is larger than the area that actually changed" — on
the strength of #570. That generalisation does not survive #570's
retirement: its damage is 1.06× the area that actually changed, which is
the scene graph getting the granularity **right**. The sentence is
narrowed rather than deleted because it is exactly the kind of
plausible‑sounding summary that outlives the finding it was drawn from.

## 9. Refresh rate: 60 Hz, 120 Hz and 720p@240, measured

The previous version of this section was titled "60 Hz only, and why",
and the why was that nitro had no mode-selection key. #3718 added one
(`output.<connector>.mode = WxH@Hz`, plus `modeline` for timings a
monitor never advertised), so this section is now three columns of
measurement rather than one column and an argument.

**The three arms, all in one sitting, all on the same binaries:**

| arm | `outputs` reports | frame budget | rows |
|---|---|---|---|
| 1920×1080@60 | `1920x1080@60000` | 16 667 µs | 54 |
| 1920×1080@120 | `1920x1080@**119982**` | 8 335 µs | 54 |
| 1280×720@240 (modeline) | `1280x720@**239840** (custom)` | 4 169 µs | 32 |

Every arm confirmed itself twice, from two instruments that do not share
a code path: `deploy/bench.sh` parses the server's `outputs` reply before
running a scenario, and each record's own `refresh_mhz` is the *client's*
reading of `refresh_ns` from its `Frame` callbacks. The two agree per arm
in all 140 rows. That pairing is the whole reason the 120 Hz column can
be believed: a `mode` line that matches nothing is a **warning plus the
default**, so a fallen-back arm comes back happily at 60 Hz and produces
a full, plausible, internally consistent "120 Hz" column that is a second
60 Hz column — and "120 Hz bought nothing" is exactly what that looks
like.

**The 240 Hz arm is 720p and that is arithmetic, not choice.** HDMI 1.4
on Haswell caps the TMDS clock near 300 MHz; 1080p@240 needs 606.5 MHz,
@165 needs 401.0 and @144 needs 346.5, so none of them fit and the
connector's own list stops at 1080p@119.982. 1280×720@240 with CVT-RB
timing is 279.75 MHz, which does fit — and #3718 confirmed the picture on
the glass with the one instrument that can answer that question, a person
looking at the monitor. The 240 Hz arm is also a **reduced** matrix (32
rows, not 54): it puts the human's own screen at 720p, so it is short by
design and hands the panel back inside ten minutes.

**Read every 240 Hz cell with its pixel count in hand.** That column
changes two variables at once — the rate doubles *and* the screen drops
from 2 073 600 to 921 600 pixels, 44.4 % — because on this box the two
cannot be separated. Where that matters the text says which of the two is
doing the work, and the VGA rows are the control that separates them: a
640×480 run is the *same scene at the same size* in all three arms, so
its column is a pure rate comparison.

### 9.1 The fullscreen pixel path at 120 Hz — the §5 prediction, checked

> **Read with §7.10b, 2026-09-20.** Everything in §9 is the `1f35491`
> ledger and is **pre-#3728**, which cut fullscreen paint 2.1–2.8×. Two
> of this section's load-bearing figures moved: rotozoom's 99.1 % of the
> 60 Hz budget is now 41 %, and §9.6's ns/px table has a post-fix pair
> at the end. **The 120 Hz arm has not been re-measured** — the post-fix
> ledger has only 60 Hz and 240 Hz arms — so this section's 120 Hz
> conclusions stand as the only measurement there is, and the bandwidth
> argument below is a ceiling that #3728 does not move: it removes
> *passes over the frame* that paint makes, not the damage-to-scanout
> copy, and `copy_us_mean` is flat or slightly up post-fix. What a
> re-run at 120 Hz would settle is whether a 5 400 µs paint now fits
> the 8 333 µs budget in practice; arithmetic says it does, and
> arithmetic against a budget is not a measurement.

§5 predicts that three passes over a fullscreen 1080p frame at 120 Hz
wants 2.99 GB/s against this box's measured copy bandwidth, i.e. **83 %
of it**, and concludes that the fullscreen pixel path at 120 Hz is
bandwidth-bound before it is CPU-bound. This ledger measures the box at
**copy 3.43 GB/s, write 6.36, read 8.05** (it was 3.61 in the previous
run — the same box, a few percent slower on the day, which is the drift
§11 warns about), so the prediction's threshold is 87 % rather than 83 %.

The prediction holds, and **the way it fails is more interesting than
the fact that it fails**:

| fullscreen 1080p | 60 Hz | 120 Hz | what happened |
|---|---|---|---|
| `rotozoom` | **59.0**/s | **23.3**/s | held 60, lost more than half at 120 |
| `starfield` n=2000 | **59.8**/s | **53.3**/s | the only pixel arm still near its cap |
| `balls` n=32 | 59.8/s | **51.3**/s | |
| `boing` | 30.0/s | **24.3**/s | already halved at 60 |
| `fire` | 30.0/s | **24.2**/s | |
| `plasma` | 15.0/s | 15.0/s | client-bound at both; the rate is irrelevant to it |

**Not one fullscreen pixel scenario gained a single presented frame from
doubling the refresh rate, and five of six lost frames.** Rotozoom is the
headline: it was the one effect holding ~60 Hz at 1080p, at **99.1 % of
its frame budget** (16 525 µs against 16 667 — §7.10's figure), and at
120 Hz it presents **23.3/s, 39 % of what it managed at half the
refresh**. Offering it twice as many vblanks made it slower in absolute
terms.

The mechanism is visible in the server's counters, and it is **not** the
rasteriser:

| fullscreen 1080p | `paint_us_mean` 60 → 120 | `copy_us_mean` 60 → 120 | server µs/frame 60 → 120 |
|---|---|---|---|
| `rotozoom` | 10 963 → **4 946** | 2 793 → **2 543** | 16 525 → 19 429 |
| `starfield` n=2000 | 11 757 → 12 267 | 2 182 → 2 265 | 16 741 → 17 531 |
| `boing` | 6 041 → 4 052 | 2 630 → **3 347** | 20 500 → 18 699 |

Rotozoom's *paint* halved while its server CPU per frame went **up** by
18 %. The server is painting less per frame and spending more CPU per
frame, which is what a path that is waiting on memory rather than
computing looks like: the work per frame did not grow, the frames simply
stopped arriving. `copy_us_mean` — the damage-to-scanout copy, the purest
bandwidth term in the suite — stays flat or rises slightly at 120 Hz
while the frame rate falls, so the per-frame copy did not get cheaper for
having less time available.

**The honest statement is therefore narrower than §5's, and it is the one
the data supports:** at 1080p the fullscreen pixel path is already at the
edge of its limit at 60 Hz — rotozoom at **99.1 %** of budget with
nothing left over, three of the six already below 60/s — and 120 Hz does
not move that limit, it only halves the budget each frame is measured
against. The bandwidth arithmetic predicts
*that there is a wall around here* and the wall is where it said; what the
measurement adds is that at 1080p the path was already against it at 60,
so the 120 Hz column does not show a cliff so much as the absence of a
gain. §5's sentence should be read as "no amount of making the rasteriser
faster changes this" — which the paint column now demonstrates directly,
since paint went *down* and throughput went down with it.

### 9.2 Does the retained path keep per-frame cost flat across rates?

This is #3718's claim — "120 Hz costs twice the frames, not twice the
work" — carried from a pointer move to a loaded scene, and it is the
cleanest result in this document.

| retained, 640×480 | server µs **per frame** | server CPU **per second** |
|---|---|---|
| `boing-node` | **2 228 / 2 284 / 2 271** | 133 330 → 273 327 → **544 981** |
| `text-static` n=100 | **1 167 / 1 181 / 1 195** | 69 999 → 141 664 → **286 662** |
| `starfield-nodes` n=500 | **2 507 / 2 521 / 2 488** | 149 997 → 301 660 → **596 657** |

(60 / 120 / 240 Hz, in that order.)

**Per-frame cost is flat to within 2.4 % across a fourfold change in
refresh rate, while CPU per second goes up by almost exactly four.**
`boing-node` costs 2 228 µs to composite at 60 Hz and 2 271 µs at 240 Hz
— a 1.9 % difference, well inside this box's day-to-day drift — and burns
4.09× the CPU per second doing it 4× as often. `text-static` is 1 167 →
1 195 µs, 2.4 %, at 4.10× the CPU/s. That is the claim, measured, on a
scene with a hundred shaped text nodes or a bouncing sprite rather than a
cursor.

It is worth being precise about what this does and does not say. It says
the *per-frame* cost of the retained path is a property of the scene and
not of the clock, so a display running at 4× the rate asks for 4× the
work in total and not 4× the work per frame. It does not say the retained
path is free at 240 Hz: 2 271 µs against a 4 169 µs budget is **54 % of
the frame**, where the same scene at 60 Hz was 13 %. The headroom is what
the rate spends, and that is the sentence a budget cares about — the same
shape as #3718's finding that 120 Hz buys wall-clock latency and spends
frame headroom.

The fullscreen retained rows show the other half. `boing-node` fullscreen
holds **59.8/s at 60 Hz** and **60.3/s at 120 Hz** — it does not get
faster, because at 10 000 µs/frame it is over the 8 335 µs budget and
presents every other vblank. Its per-frame cost is again flat (10 000 vs
10 110), so what changed is only which budget it is compared against. At
720p@240 the same scene costs **4 676 µs** and presents **120.8/s**: the
cost fell by 53 % with a 55.6 % cut in pixels, so this arm's saving is the
screen, not the rate.

### 9.3 `rects`: how many mutations fit in 8.3 ms and 4.2 ms?

#3717 found ≥ 2000 mutations/frame inside a 16.7 ms budget at 60 Hz, with
damage saturating before the mutation count did. Per rate:

| `rects` 640×480 | 60 Hz | 120 Hz | 240 Hz |
|---|---|---|---|
| n=100 | 59.8/s, 3 092 µs | **119.8/s**, 3 074 µs | **240.0/s**, 2 938 µs |
| n=500 | 59.8/s, 10 223 µs | 60.5/s, 10 275 µs | 114.5/s, 6 550 µs |
| n=1000 | 59.7/s, 11 844 µs | 60.0/s, 11 917 µs | — |
| n=2000 | 57.3/s, 12 616 µs | 59.7/s, 12 430 µs | 79.8/s, 10 000 µs |

**The answer is between 100 and 500 at both 8.3 ms and 4.2 ms**, and the
per-frame cost column says why the bracket is that wide: n=100 costs
~3 000 µs and n=500 costs ~10 200, so the budget is crossed somewhere in
a stretch where cost is still climbing steeply with N. At 120 Hz n=100
holds a full 119.8/s and n=500 collapses to 60.5/s — precisely "one frame
in two", the signature of a scene that costs between one and two budgets.
At 240 Hz n=100 holds 240.0/s outright.

Note the per-frame cost is **the same at every rate** for a given N
(3 092 / 3 074 / 2 938 at n=100; 10 223 / 10 275 at n=500 for the two
1080p arms) — §9.2's result again, now in the x11perf family: mutations
cost what they cost, and the rate decides how many of those frames fit in
a second.

### 9.4 The starfield crossover at 720p@240 — §7.8's prediction, checked

§7.8 bracketed the crossover at **n ≈ 640 for a 640×480 viewport** and
**n ≈ 5 000 for a 1080p one**, and predicted that if the crossing scales
with screen area it lands near **n ≈ 2 000 at 720p**.

**The prediction is wrong in an interesting direction: at 720p the node
arm wins at every N measured, and the crossover is not reached.**

| 720p@240, fullscreen | pixels µs/frame | nodes µs/frame | ratio | pixels /s | nodes /s |
|---|---|---|---|---|---|
| n=100 | 7 528 | **3 032** | 0.40× | 120.0 | **238.0** |
| n=500 | 7 528 | **3 174** | 0.42× | 120.0 | **240.0** |
| n=2000 | 7 569 | **4 013** | 0.53× | 120.0 | **235.5** |

The node arm is 2.5× cheaper at a hundred stars and still 1.9× cheaper at
two thousand, and — the part that matters for a desktop — **it is the
only arm that holds 240 Hz**. The buffer arm sits at exactly 120.0/s in
all three rows: one frame in two, every time, which is what a scene
costing between one and two 4 169 µs budgets does.

The crossover *did* move, though, and the VGA column is where it shows:

| 640×480 (same scene at all three rates) | 60 Hz | 120 Hz | 240 Hz |
|---|---|---|---|
| n=2000, pixels µs/frame | 2 833 | 3 046 | 2 912 |
| n=2000, nodes µs/frame | **6 156** | **6 189** | **3 153** |
| ratio | 2.17× | 2.03× | **1.08×** |

At VGA the node arm loses at n=2000 — §7.8's finding, reproduced at 60 and
120 Hz — but at 240 Hz its cost **halves** (6 156 → 3 153 µs) while the
buffer arm's does not move, and the two arms converge to within 8 %.

**I cannot say from this ledger why**, and the honest form of that is
worth more than a plausible mechanism. What is established:

- Both arms kept up. The n=2000 node arm presented **1 440 of 1 441
  commits** at 240 Hz and 359 of 360 at 60, so this is not a scene being
  measured over frames it failed to draw.
- The scene is identical. `mutations ÷ presented` is 2 005.6 / 2 002.8 /
  2 001.4 across the three rates, and the stars move by frame *index*
  rather than by elapsed time, so each frame displaces them equally at
  every rate.
- `damage_px_mean` is **309 444 px at all three rates**, and
  `paint_us_mean` fell from 3 052 to 1 556 µs against it. `copy_us_mean`
  fell the same way, 417 → 213, over a region the counter says did not
  change.
- The fall is consistent rather than an outlier: paint min/mean/max at
  240 Hz is 1 518 / 1 556 / 1 630 µs, a tight distribution well clear of
  the 60 Hz row's 3 024 / 3 052 / 3 144.

A caution about reading the damage column against the paint column, which
this section originally got wrong: **they are not measured over the same
region.** `paint_us` times `rasterize_region()` — this frame's damage
alone, because the shadow already holds everything older — while
`damage_px` reports `repaint_region()`, the age-2 union
`damage(n) ∪ damage(n-1)` that the copy is done over
(`crates/nitro-server/src/frame.rs`). So a constant `damage_px` does not
by itself mean the rasterizer's input was constant, and an earlier draft
of this paragraph used it to argue a mechanism it cannot support.

The candidate that survives partway is the one §9.6 measures: the 240 Hz
arm runs at 720p, so the server's shadow buffer is **3 686 400 bytes
against 8 294 400**, and the same 309 444-pixel copy out of a smaller
buffer is cheaper per pixel on a box with 3 MB of L3. That would explain
`copy_us` halving at a constant copy region, and paint shares the same
destination. **But it does not explain the selectivity**: `boing-node`
(2 021 → 2 014 µs) and `starfield-nodes` n=500 (1 178 → 1 180) are
perfectly flat at 240 Hz against the same smaller shadow, and a locality
effect should have moved them too. Until something distinguishes those
rows from this one, "paint halved at constant reported damage, cause not
identified" is the whole of what this ledger supports. The measurement to
run is `starfield-nodes --n 2000` at 720p@**60** — the same screen, the
old rate — which separates the shadow's size from the refresh rate in one
arm. This sweep could not: on this box 240 Hz only exists at 720p.

So the widget author's rule from §7.8 — "the retained path is right until
you have of order a thousand independently moving things in a small
window" — survives, with a footnote that is an observation rather than an
explanation: **at 240 Hz the measured crossing moves out past n = 2000
even at VGA**, and at 720p the node arm never loses at all.

### 9.5 The x11perf headline, and `text` vs `text-static`, per rate

The reshape ratio, at n=100, 640×480, the same scene in all three arms:

| | 60 Hz | 120 Hz | 240 Hz |
|---|---|---|---|
| `text` (reshapes every frame) | 7 000 µs | 6 203 µs | 3 204 µs |
| `text-static` (never reshapes) | **1 167 µs** | **1 181 µs** | **1 195 µs** |
| **ratio** | **6.0×** | **5.3×** | **2.7×** |
| `text` layouts over the run | 36 100 | 71 900 | 144 002 |
| `text-static` layouts over the run | **0** | **0** | **1** |

`text-static` is the flat row of §9.2 — 1 167 / 1 181 / 1 195 µs, a 2.4 %
spread — and `text` is the one that falls, because its cost per frame is
dominated by shaping and at 240 Hz the scene had 6 s × 240 × 100 = 144 002
layouts to do and only 4 169 µs a frame to do them in. **The retained arm
shapes zero glyphs at every rate**, which is the claim, and it is the only
one of the two whose cost a designer can predict without knowing the
refresh rate.

The ratio *narrowing* with rate is not the retained win eroding: it is
`text` failing to keep up and so being measured over fewer, cheaper
frames. The absolute numbers are the ones to read.

### 9.6 #568's fixed per-frame cost at 4.2 ms — and what 720p says about it

> **This section is the pre-#3728 measurement and is left as measured.**
> It is the analysis that decided what to profile, and it was right: the
> excess is proportional to area, so the per-pixel path at full-surface
> damage was the thing to change, and #3728 changed it. The post-fix
> numbers are in §7.10b and the closing pair is restated at the end of
> this section. Every figure between here and there is from
> `docs/bench-1f35491.jsonl`.


#568 records that a fullscreen 1080p repaint costs `paint_us_mean`
11 300–11 700 µs regardless of what is being drawn, and that a
least-squares fit through `putimage`'s damage/paint points extrapolates
to ~3 060 µs at that area — so **a fullscreen repaint is ~3.8× more
expensive per pixel than a large partial one**, and there is a fixed
per-frame cost that is not the linear part.

At 720p the frame is 921 600 px against 2 073 600 — **2.25× fewer
pixels** — so if the cost were purely linear in area, paint should fall
by 55.6 %. Per-pixel cost, which is the form that makes the question
answerable:

| fullscreen (**pre-#3728**, `1f35491`) | 1080p `paint_us_mean` | ns/px | 720p `paint_us_mean` | ns/px |
|---|---|---|---|---|
| `plasma` | 3 511 | 1.69 | **1 372** | **1.49** |
| `boing` | 6 041 | 2.91 | **1 963** | **2.13** |
| `rotozoom` | 10 963 | 5.29 | **4 542** | **4.93** |
| `starfield` n=2000 | 11 757 | 5.67 | **5 199** | **5.64** |

**Paint scales with pixels, and it scales slightly better than
linearly** — every scenario's ns/px *falls* at the smaller size, by 1 %
(starfield) to 27 % (boing). So the fixed part #568 is looking for does
not grow as a share when the screen shrinks; if anything the smaller
frame is marginally more efficient per pixel, which is what a
cache-friendlier working set looks like on a box with 3 MB of L3 and an
8.3 MB 1080p frame against a 3.7 MB 720p one.

That is a real constraint on #568's hypothesis. A large *fixed* per-frame
cost would show up as ns/px rising sharply at the smaller size — half the
pixels carrying the same constant is twice the per-pixel cost — and it
does not. Whatever makes a fullscreen repaint 3.8× dearer per pixel than
a large partial one, **it is proportional to area rather than constant per
frame**, so the thing to profile is the per-pixel path at full-surface
damage (a different loop, a different access pattern, a different
blend path) and not a fixed setup cost.

And the answer to "what do the fullscreen scenarios actually get at
720p@240": **120.0/s for starfield, rotozoom and the other
damage-saturated rows, 48.0/s for fire, 42.2/s for boing, 34.3/s for
plasma.** A 4 169 µs budget against a 5 199 µs paint is one frame in two
before the client has computed anything, which is exactly the 120.0/s
those rows report. The numbers are on #568.

**What happened next, and it is the part this section got right.** The
prescription above — *profile the per-pixel path at full-surface damage,
not a fixed setup cost* — is what #3728 acted on, and the re-measurement
(§7.10b, `docs/bench-5e4b02e.jsonl`) closes the question this table
posed. The closing pair, ns/px at both sizes, same instrument, same box:

| fullscreen | 1080p@60 ns/px pre → **post** | 720p@240 ns/px pre → **post** |
|---|---|---|
| `plasma` | 1.69 → **0.76** | 1.49 → **0.82** |
| `boing` | 2.88 → **2.69** | 2.13 → **1.18** |
| `rotozoom` | 5.29 → **1.91** | 4.93 → **1.86** |
| `starfield` n=2000 | 5.67 → **2.60** | 5.64 → **2.19** |

**ns/px fell at both sizes**, 1.07–2.77× at 1080p and 1.80–2.65× at 720p,
which is the shape that distinguishes a cheaper per-pixel path from
merely running fewer whole-frame passes — had it been only the latter,
the 720p column would have moved *less* than the 1080p one, and for
`boing`, `fire` and the starfields it moved more. The `boing` 1080p
figure is 2.88 rather than this table's 2.91 because §7.10b averages the
two `boing --fullscreen` runs each arm contains; this table quoted the
second.

And the frame rates the last paragraph reports are transformed: at
720p@240 the damage-saturated rows now present at **235.9–240.2/s**
against the 120.0/s above, because a 1 995 µs paint fits inside the
4 169 µs budget where 5 199 µs took two frames. `fire` is 73.2/s against
48.0, `plasma` 40.0 against 34.3, `boing` 120.2 against 42.2.

### 9.7 What the rate sweep cost in instrument bugs

Three, all found on the box, all of which produced *plausible or empty*
output rather than an error, and none of which is reachable with a
single-rate ledger. They are recorded here rather than in §8 because they
are specifically the failure modes a *comparison across configurations*
introduces.

- **A 141-run sweep measured binaries someone else deployed.** Another
  task ran `just deploy` six minutes after this one rsynced its build and
  six minutes before the first arm. Every other instrument agreed:
  `outputs` reported the right mode at every arm, the client's
  `refresh_mhz` agreed with it, the ledger's `sha` column faithfully
  named the tree this task built from, and the numbers were plausible.
  The run measured a different build for forty minutes and said so
  nowhere. **`md5` before a run answers "is this mine now"; a
  forty-minute run has to answer "was it mine throughout", and only a
  pair of fingerprints answers that.** `deploy/bench.sh` now fingerprints
  `nitro-server` and `nitro-bench` at both ends, writes both into the
  ledger, and fails loudly with `# INVALID: the binaries changed DURING
  this run`. The clean ledger carries the positive form: `# binaries
  unchanged across the whole run`.
- **Every row was stamped with the wrong sha**, from the same family. The
  script read `git -C ~/src/ai/nitro rev-parse HEAD` — the *box's clone*,
  which is whatever `just box-push` last pushed there and was seventeen
  commits behind the binaries in `~/nitro-bin`. The sha now comes from
  the caller, which is the machine that ran `cargo build`, and the clone
  fallback stamps `<sha>-box-checkout` and warns, because a wrong sha is
  worse than `unknown`: `unknown` sends a reader to ask, a plausible sha
  sends them to read the wrong diff.
- **The rate table's row key used two fields that are not sweep points.**
  A fullscreen run records `size` as an *output* — the screen's width
  (1920, 1920, **1280**) for the pixel scenarios, the ball's on-screen
  diameter (389 vs **260**) for `boing-node` — so keying on it
  re-introduced the geometry through a field that does not have `width`
  in its name and scattered every fullscreen row across three keys. The
  entire 240 Hz column printed as `?`, which looks exactly like "the
  240 Hz arm never ran". And the control arm, being byte-for-byte the
  same sweep point as the row it controls, **silently replaced every
  measured `rects n=500` with its own shell-less control** under the
  "last run wins" rule — a plausible number, right units, right cell,
  describing a desktop with no shell on it.

All three are one sentence: **a provenance or identity field derived from
something other than the artefact it claims to describe.** The sha came
from the box's clone instead of the build; the row identity came from a
measured output instead of the sweep point; the control's identity came
from its parameters instead of its condition. Each now has a regression
test asserting the property, and the binary check is the one that will
matter to somebody else, because the box is shared and four tasks deploy
to it in an evening.

### 9.8 The generated rate tables

Per scenario, a column per rate, from the same ledger as §6 and printed
by the same tool (`nitro-bench report`). Each table states the frame
budget its verdicts were drawn against. A scenario measured at only one
rate is omitted rather than shown as a row of `?`, and a `?` cell is a
genuine gap in the sweep — the 240 Hz arm is the reduced matrix of the
table at the top of this section.

#### rects across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| rects n=10 640x480 | 59.8 | 779.9 | 55.7 | 388.0 | ok | 119.8 | 806.7 | 55.6 | 380.0 | ok | ? | ? | ? | ? | ? |
| rects n=100 640x480 | 59.8 | 3091.9 | 83.6 | 2267.0 | ok | 119.8 | 3073.7 | 83.4 | 2272.0 | ok | 240.0 | 2937.5 | 83.3 | 2160.0 | ok |
| rects n=500 640x480 | 59.8 | 10222.8 | 111.4 | 8904.0 | ok | 60.5 | 10275.5 | 110.2 | 8991.0 | **dropped** | 114.5 | 6550.2 | 87.3 | 5396.0 | **dropped** |
| rects n=1000 640x480 | 59.7 | 11843.6 | 167.6 | 10550.0 | **dropped** | 60.0 | 11916.7 | 194.4 | 10578.0 | **slow** | ? | ? | ? | ? | ? |
| rects n=2000 640x480 | 57.3 | 12616.3 | 145.3 | 10953.0 | **dropped** | 59.7 | 12430.2 | 139.7 | 11056.0 | **dropped** | 79.8 | 10000.0 | 146.1 | 8881.0 | **dropped** |
| rects n=500 640x480 (control: shell down) | 60.0 | 10000.0 | 111.1 | 8824.0 | ok | 101.0 | 6699.7 | 66.0 | 5166.0 | **slow** | 201.3 | 3758.3 | 33.1 | 2662.0 | **slow** |

#### rects-move across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| rects-move n=10 640x480 | 59.8 | 779.9 | 55.7 | 392.0 | ok | 119.8 | 765.0 | 55.6 | 388.0 | ok |
| rects-move n=100 640x480 | 59.8 | 3119.8 | 83.6 | 2319.0 | ok | 119.8 | 3157.2 | 83.4 | 2349.0 | ok |
| rects-move n=500 640x480 | 60.0 | 10250.0 | 111.1 | 8959.0 | ok | 119.5 | 6053.0 | 69.7 | 5212.0 | ok |
| rects-move n=1000 640x480 | 59.8 | 11671.3 | 167.1 | 10401.0 | ok | 60.2 | 11385.0 | 193.9 | 10202.0 | **slow** |
| rects-move n=2000 640x480 | 59.7 | 12402.2 | 139.7 | 11155.0 | ok | 59.7 | 12486.0 | 139.7 | 11119.0 | **slow** |

#### text across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| text n=10 640x480 | 59.8 | 2367.7 | 139.3 | 83.0 | ok | 120.0 | 2250.0 | 125.0 | 84.0 | ok | ? | ? | ? | ? | ? |
| text n=10 size=24 640x480 | 59.8 | 2507.0 | 139.3 | 218.0 | ok | 119.8 | 2433.9 | 125.2 | 216.0 | ok | ? | ? | ? | ? | ? |
| text n=100 640x480 | 60.0 | 7000.0 | 222.2 | 658.0 | ok | 119.8 | 6203.1 | 208.6 | 577.0 | ok | 239.8 | 3203.6 | 118.1 | 312.0 | ok |
| text n=100 size=24 640x480 | 59.8 | 8300.8 | 222.8 | 1581.0 | ok | 119.8 | 6161.3 | 180.8 | 1206.0 | ok | ? | ? | ? | ? | ? |
| text n=500 640x480 | 59.8 | 12144.8 | 306.4 | 1449.0 | ok | 119.5 | 7963.7 | 223.2 | 1059.0 | ok | ? | ? | ? | ? | ? |
| text n=500 size=24 640x480 | 59.8 | 12061.3 | 250.7 | 2276.0 | ok | 116.0 | 8577.6 | 258.6 | 1772.0 | ok | ? | ? | ? | ? | ? |

#### text-static across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| text-static n=10 640x480 | 59.8 | 306.4 | 55.7 | 91.0 | ok | 119.7 | 320.3 | 55.7 | 86.0 | ok | ? | ? | ? | ? | ? |
| text-static n=100 640x480 | 60.0 | 1166.7 | 83.3 | 676.0 | ok | 120.0 | 1180.6 | 83.3 | 674.0 | ok | 239.8 | 1195.3 | 76.4 | 672.0 | ok |
| text-static n=500 640x480 | 60.0 | 4555.6 | 111.1 | 3021.0 | ok | 119.8 | 4548.0 | 139.1 | 3041.0 | ok | ? | ? | ? | ? | ? |

#### putimage across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| putimage size=100 640x480 | 59.8 | 390.0 | 668.5 | 73.0 | ok | 119.8 | 375.5 | 625.9 | 74.0 | ok | 239.8 | 382.2 | 667.1 | 71.0 | ok |
| putimage size=250 640x480 | 59.8 | 779.9 | 4039.0 | 314.0 | ok | 119.8 | 792.8 | 3866.5 | 299.0 | ok | ? | ? | ? | ? | ? |
| putimage size=500 640x480 | 59.8 | 1476.3 | 8189.4 | 712.0 | ok | 119.8 | 1307.4 | 4937.4 | 561.0 | ok | 119.9 | 1472.2 | 5055.6 | 263.0 | **slow** |
| putimage size=1080 640x480 | 30.0 | 5166.7 | 27111.1 | 887.0 | **slow** | 29.6 | 4719.1 | 27359.6 | 860.0 | **slow** | ? | ? | ? | ? | ? |

#### scroll across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| scroll n=500 640x480 | 59.8 | 1615.6 | 55.7 | 834.0 | ok | 119.8 | 1599.4 | 55.6 | 840.0 | ok | 239.8 | 1521.9 | 69.5 | 677.0 | ok |

#### create across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| create n=50 640x480 | 60.0 | 694.4 | 55.6 | 243.0 | ok | 120.2 | 693.5 | 83.2 | 241.0 | ok |

#### plasma across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| plasma size=640 640x480 | 60.0 | 1638.9 | 9000.0 | 779.0 | ok | 103.7 | 1623.8 | 6623.8 | 657.0 | ok | 80.0 | 1958.3 | 8291.7 | 400.0 | **slow** |
| plasma fullscreen | 15.0 | 14444.4 | 49333.3 | 3511.0 | **dropped** | 15.0 | 15111.1 | 49555.6 | 3705.0 | **slow** | 34.3 | 6116.5 | 21310.7 | 1372.0 | **slow** |

#### fire across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| fire size=640 640x480 | 59.9 | 2722.2 | 9583.3 | 1371.0 | ok | 120.0 | 1777.8 | 4722.2 | 859.0 | ok | 238.5 | 1614.3 | 3004.9 | 770.0 | ok |
| fire fullscreen | 30.0 | 16666.7 | 22555.6 | 3697.0 | **slow** | 24.2 | 15103.4 | 25793.1 | 3743.0 | **slow** | 48.0 | 6250.0 | 14652.8 | 1405.0 | **slow** |

#### rotozoom across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| rotozoom size=640 640x480 | 59.8 | 2896.9 | 4261.8 | 1441.0 | ok | 119.7 | 2729.8 | 3941.5 | 1302.0 | ok | 240.0 | 1861.1 | 1826.4 | 891.0 | ok |
| rotozoom fullscreen | 59.0 | 16525.4 | 12570.6 | 10963.0 | ok | 23.3 | 19428.6 | 22785.7 | 4946.0 | **slow** | 119.9 | 7500.0 | 5861.1 | 4542.0 | **slow** |

#### boing across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| boing size=640 640x480 | 59.8 | 3119.8 | 6991.6 | 1707.0 | ok | 119.8 | 2267.0 | 4631.4 | 1191.0 | ok | 240.1 | 1880.6 | 2435.8 | 1034.0 | ok |
| boing fullscreen | 30.0 | 20500.0 | 20833.3 | 6041.0 | **slow** | 24.3 | 18698.6 | 23972.6 | 4052.0 | **slow** | 42.2 | 8853.8 | 14505.9 | 1963.0 | **dropped** |

#### boing-node across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| boing-node size=173 640x480 | 59.8 | 2228.4 | 55.7 | 2021.0 | ok | 119.7 | 2284.1 | 55.7 | 2009.0 | ok | 240.0 | 2270.8 | 62.5 | 2014.0 | ok |
| boing-node fullscreen | 59.8 | 10000.0 | 111.4 | 9572.0 | ok | 60.3 | 10110.5 | 55.2 | 9671.0 | **slow** | 120.8 | 4675.9 | 69.0 | 4374.0 | **slow** |

#### starfield across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield n=100 size=640 640x480 | 59.8 | 2841.2 | 1169.9 | 1508.0 | ok | 119.8 | 2948.5 | 1251.7 | 1570.0 | ok | 240.0 | 2895.8 | 1215.3 | 1544.0 | ok |
| starfield n=100 fullscreen | 60.0 | 16500.0 | 9888.9 | 11553.0 | ok | 53.4 | 17476.6 | 10436.1 | 12247.0 | **slow** | 120.0 | 7527.8 | 4444.4 | 5157.0 | **slow** |
| starfield n=500 size=640 640x480 | 59.8 | 3203.3 | 1504.2 | 1814.0 | ok | 119.8 | 2934.6 | 1251.7 | 1577.0 | ok | 240.0 | 2895.8 | 1229.2 | 1549.0 | ok |
| starfield n=500 fullscreen | 59.0 | 16355.9 | 9604.5 | 11517.0 | ok | 53.3 | 17593.8 | 10500.0 | 12260.0 | **slow** | 120.0 | 7527.8 | 4513.9 | 5177.0 | **slow** |
| starfield n=2000 size=640 640x480 | 60.0 | 2833.3 | 1277.8 | 1520.0 | ok | 119.8 | 3045.9 | 1460.4 | 1647.0 | ok | 239.8 | 2911.7 | 1369.0 | 1542.0 | ok |
| starfield n=2000 fullscreen | 59.8 | 16740.9 | 10195.0 | 11757.0 | ok | 53.3 | 17531.2 | 10500.0 | 12267.0 | **slow** | 120.0 | 7569.4 | 4527.8 | 5199.0 | **slow** |

#### starfield-nodes across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs, 240 Hz = 4169 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict | 240 presented/s | 240 server µs | 240 client µs | 240 paint µs | 240 verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield-nodes n=100 640x480 | 59.8 | 1420.6 | 83.6 | 671.0 | ok | 119.8 | 1432.5 | 69.5 | 659.0 | ok | 240.0 | 1437.5 | 83.3 | 621.0 | ok |
| starfield-nodes n=100 fullscreen | 59.8 | 7186.6 | 111.4 | 4459.0 | ok | 120.0 | 6291.7 | 69.4 | 3667.0 | ok | 238.0 | 3032.2 | 84.0 | 1609.0 | ok |
| starfield-nodes n=500 640x480 | 59.8 | 2507.0 | 139.3 | 1178.0 | ok | 119.7 | 2520.9 | 139.3 | 1207.0 | ok | 239.8 | 2487.8 | 145.9 | 1180.0 | ok |
| starfield-nodes n=500 fullscreen | 60.0 | 8750.0 | 166.7 | 5202.0 | ok | 119.8 | 7009.7 | 83.4 | 4177.0 | ok | 240.0 | 3173.6 | 111.1 | 1694.0 | ok |
| starfield-nodes n=2000 640x480 | 59.8 | 6156.0 | 362.1 | 3052.0 | ok | 119.8 | 6189.2 | 361.6 | 3097.0 | ok | 240.0 | 3152.8 | 215.3 | 1556.0 | ok |
| starfield-nodes n=2000 fullscreen | 60.0 | 11361.1 | 333.3 | 6581.0 | ok | 119.8 | 8303.2 | 180.8 | 4973.0 | ok | 235.5 | 4012.7 | 219.4 | 2200.0 | ok |

#### balls across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| balls n=32 size=640 640x480 | 59.8 | 2896.9 | 1448.5 | 1490.0 | ok | 119.8 | 2934.6 | 1502.1 | 1553.0 | ok |
| balls n=32 fullscreen | 59.8 | 16657.4 | 10195.0 | 11663.0 | ok | 51.3 | 17694.8 | 10876.6 | 12401.0 | **slow** |

#### balls-nodes across rates

Frame budget: 60 Hz = 16667 µs, 120 Hz = 8335 µs.

| run | 60 presented/s | 60 server µs | 60 client µs | 60 paint µs | 60 verdict | 120 presented/s | 120 server µs | 120 client µs | 120 paint µs | 120 verdict |
|---|---|---|---|---|---|---|---|---|---|---|
| balls-nodes n=32 640x480 | 60.0 | 1722.2 | 55.6 | 1194.0 | ok | 119.7 | 1587.7 | 69.6 | 1079.0 | ok |
| balls-nodes n=32 fullscreen | 59.8 | 6044.6 | 83.6 | 3743.0 | ok | 119.8 | 4687.1 | 69.5 | 2694.0 | ok |

### refresh pivot

| run | 60 Hz server µs/frame | 60 Hz presented/s | 60 Hz verdict | 120 Hz server µs/frame | 120 Hz presented/s | 120 Hz verdict | 240 Hz server µs/frame | 240 Hz presented/s | 240 Hz verdict |
|---|---|---|---|---|---|---|---|---|---|
| rects n=10 640x480 | 779.9 | 59.8 | ok | 806.7 | 119.8 | ok | ? | ? | ? |
| rects-move n=10 640x480 | 779.9 | 59.8 | ok | 765.0 | 119.8 | ok | ? | ? | ? |
| rects n=100 640x480 | 3091.9 | 59.8 | ok | 3073.7 | 119.8 | ok | 2937.5 | 240.0 | ok |
| rects-move n=100 640x480 | 3119.8 | 59.8 | ok | 3157.2 | 119.8 | ok | ? | ? | ? |
| rects n=500 640x480 | 10222.8 | 59.8 | ok | 10275.5 | 60.5 | **dropped** | 6550.2 | 114.5 | **dropped** |
| rects-move n=500 640x480 | 10250.0 | 60.0 | ok | 6053.0 | 119.5 | ok | ? | ? | ? |
| rects n=1000 640x480 | 11843.6 | 59.7 | **dropped** | 11916.7 | 60.0 | **slow** | ? | ? | ? |
| rects-move n=1000 640x480 | 11671.3 | 59.8 | ok | 11385.0 | 60.2 | **slow** | ? | ? | ? |
| rects n=2000 640x480 | 12616.3 | 57.3 | **dropped** | 12430.2 | 59.7 | **dropped** | 10000.0 | 79.8 | **dropped** |
| rects-move n=2000 640x480 | 12402.2 | 59.7 | ok | 12486.0 | 59.7 | **slow** | ? | ? | ? |
| text n=10 640x480 | 2367.7 | 59.8 | ok | 2250.0 | 120.0 | ok | ? | ? | ? |
| text-static n=10 640x480 | 306.4 | 59.8 | ok | 320.3 | 119.7 | ok | ? | ? | ? |
| text n=10 size=24 640x480 | 2507.0 | 59.8 | ok | 2433.9 | 119.8 | ok | ? | ? | ? |
| text n=100 640x480 | 7000.0 | 60.0 | ok | 6203.1 | 119.8 | ok | 3203.6 | 239.8 | ok |
| text-static n=100 640x480 | 1166.7 | 60.0 | ok | 1180.6 | 120.0 | ok | 1195.3 | 239.8 | ok |
| text n=100 size=24 640x480 | 8300.8 | 59.8 | ok | 6161.3 | 119.8 | ok | ? | ? | ? |
| text n=500 640x480 | 12144.8 | 59.8 | ok | 7963.7 | 119.5 | ok | ? | ? | ? |
| text-static n=500 640x480 | 4555.6 | 60.0 | ok | 4548.0 | 119.8 | ok | ? | ? | ? |
| text n=500 size=24 640x480 | 12061.3 | 59.8 | ok | 8577.6 | 116.0 | ok | ? | ? | ? |
| putimage size=100 640x480 | 390.0 | 59.8 | ok | 375.5 | 119.8 | ok | 382.2 | 239.8 | ok |
| putimage size=250 640x480 | 779.9 | 59.8 | ok | 792.8 | 119.8 | ok | ? | ? | ? |
| putimage size=500 640x480 | 1476.3 | 59.8 | ok | 1307.4 | 119.8 | ok | 1472.2 | 119.9 | **slow** |
| putimage size=1080 640x480 | 5166.7 | 30.0 | **slow** | 4719.1 | 29.6 | **slow** | ? | ? | ? |
| scroll n=500 640x480 | 1615.6 | 59.8 | ok | 1599.4 | 119.8 | ok | 1521.9 | 239.8 | ok |
| create n=50 640x480 | 694.4 | 60.0 | ok | 693.5 | 120.2 | ok | ? | ? | ? |
| plasma size=640 640x480 | 1638.9 | 60.0 | ok | 1623.8 | 103.7 | ok | 1958.3 | 80.0 | **slow** |
| plasma fullscreen | 14444.4 | 15.0 | **dropped** | 15111.1 | 15.0 | **slow** | 6116.5 | 34.3 | **slow** |
| fire size=640 640x480 | 2722.2 | 59.9 | ok | 1777.8 | 120.0 | ok | 1614.3 | 238.5 | ok |
| fire fullscreen | 16666.7 | 30.0 | **slow** | 15103.4 | 24.2 | **slow** | 6250.0 | 48.0 | **slow** |
| rotozoom size=640 640x480 | 2896.9 | 59.8 | ok | 2729.8 | 119.7 | ok | 1861.1 | 240.0 | ok |
| rotozoom fullscreen | 16525.4 | 59.0 | ok | 19428.6 | 23.3 | **slow** | 7500.0 | 119.9 | **slow** |
| boing size=640 640x480 | 3119.8 | 59.8 | ok | 2267.0 | 119.8 | ok | 1880.6 | 240.1 | ok |
| boing fullscreen | 20500.0 | 30.0 | **slow** | 18698.6 | 24.3 | **slow** | 8853.8 | 42.2 | **dropped** |
| boing-node size=173 640x480 | 2228.4 | 59.8 | ok | 2284.1 | 119.7 | ok | 2270.8 | 240.0 | ok |
| boing-node fullscreen | 10000.0 | 59.8 | ok | 10110.5 | 60.3 | **slow** | 4675.9 | 120.8 | **slow** |
| starfield n=100 size=640 640x480 | 2841.2 | 59.8 | ok | 2948.5 | 119.8 | ok | 2895.8 | 240.0 | ok |
| starfield-nodes n=100 640x480 | 1420.6 | 59.8 | ok | 1432.5 | 119.8 | ok | 1437.5 | 240.0 | ok |
| starfield n=100 fullscreen | 16500.0 | 60.0 | ok | 17476.6 | 53.4 | **slow** | 7527.8 | 120.0 | **slow** |
| starfield-nodes n=100 fullscreen | 7186.6 | 59.8 | ok | 6291.7 | 120.0 | ok | 3032.2 | 238.0 | ok |
| starfield n=500 size=640 640x480 | 3203.3 | 59.8 | ok | 2934.6 | 119.8 | ok | 2895.8 | 240.0 | ok |
| starfield-nodes n=500 640x480 | 2507.0 | 59.8 | ok | 2520.9 | 119.7 | ok | 2487.8 | 239.8 | ok |
| starfield n=500 fullscreen | 16355.9 | 59.0 | ok | 17593.8 | 53.3 | **slow** | 7527.8 | 120.0 | **slow** |
| starfield-nodes n=500 fullscreen | 8750.0 | 60.0 | ok | 7009.7 | 119.8 | ok | 3173.6 | 240.0 | ok |
| starfield n=2000 size=640 640x480 | 2833.3 | 60.0 | ok | 3045.9 | 119.8 | ok | 2911.7 | 239.8 | ok |
| starfield-nodes n=2000 640x480 | 6156.0 | 59.8 | ok | 6189.2 | 119.8 | ok | 3152.8 | 240.0 | ok |
| starfield n=2000 fullscreen | 16740.9 | 59.8 | ok | 17531.2 | 53.3 | **slow** | 7569.4 | 120.0 | **slow** |
| starfield-nodes n=2000 fullscreen | 11361.1 | 60.0 | ok | 8303.2 | 119.8 | ok | 4012.7 | 235.5 | ok |
| balls n=32 size=640 640x480 | 2896.9 | 59.8 | ok | 2934.6 | 119.8 | ok | ? | ? | ? |
| balls-nodes n=32 640x480 | 1722.2 | 60.0 | ok | 1587.7 | 119.7 | ok | ? | ? | ? |
| balls n=32 fullscreen | 16657.4 | 59.8 | ok | 17694.8 | 51.3 | **slow** | ? | ? | ? |
| balls-nodes n=32 fullscreen | 6044.6 | 59.8 | ok | 4687.1 | 119.8 | ok | ? | ? | ? |
| rects n=500 640x480 (control: shell down) | 10000.0 | 60.0 | ok | 6699.7 | 101.0 | **slow** | 3758.3 | 201.3 | **slow** |

## 10. Reproducing

```sh
just bench                       # the whole matrix once at the box's current mode (~12 min, 54 runs plus a bandwidth probe)
just bench '1920x1080@60\ 1920x1080@120\ 720p240' 6   # the three-rate sweep behind §9 (~17 min, 140 runs)
just bench '1920x1080@60\ 720p240' 6                  # the two-arm sweep behind §7.10b (~11 min, 86 runs)
just bench-report                # tmp/bench/box.jsonl → the markdown above
just bench-bandwidth box         # the box's memcpy rate: copy 3.43 GB/s (3.56 on the §7.10b sitting)
```

**The backslashes are load-bearing and are not a typo.** A multi-word
`--modes` argument written the obvious way — `just bench "60 120"` —
**exits 2 with `unknown argument 120`**. `just` passes the string to
`ssh` as one argv element, but ssh joins its argv with spaces into a
single command string and the *remote* shell re-splits it, so every arm
after the first arrives at `deploy/bench.sh` as a stray positional. The
escaped space survives the second split. This section documented the
double-quoted spelling from its first version and **that spelling has
never worked**; the 140-run `1f35491` ledger reached the box some other
way. Filed as **#618**, which owns the `justfile` fix and the other two
doc sites; this line is corrected here because a command that exits 2 in
a section headed "Reproducing" is the exact failure this document is
about.

and, for one scenario at a time, on the box:

```sh
XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench rects --n 1000 --seconds 10 --json
XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench boing --fullscreen --seconds 6 --json
~/nitro-bin/nitro-bench list     # every scenario, with the x11perf op it ports
~/nitro-bin/nitro-bench report ~/tmp/bench/5e4b02e.jsonl
```

The ledger is `~/tmp/bench/<sha>.jsonl` on the box, fetched to
`tmp/bench/box.jsonl` here. **Delete the box-side file once fetched**:
`just bench` fetches with `cat ~/tmp/bench/*.jsonl`, a glob, so a ledger
left there is concatenated into the *next* task's run — and
`deploy/bench.sh` **appends**, so a second attempt at the same sha
doubles every row rather than replacing it. The runs behind this document
are **checked in** — `docs/bench-1f35491.jsonl` (363 KB) for §6–§9 and
`docs/bench-5e4b02e.jsonl` (231 KB) for §7.10b — so a reviewer can
re-derive every figure with

```sh
cargo run -q -p nitro-bench -- report docs/bench-1f35491.jsonl   # §6 and §9.8
cargo run -q -p nitro-bench -- report docs/bench-5e4b02e.jsonl   # §7.10b.8
```

and get those tables back byte for byte. That is deliberate: a
document whose evidence lives only in a gitignored `tmp/` is a document
asking to be trusted, and this one would rather be checked. Lines are
append-only and self-describing,
so two runs on two machines concatenate with `cat` and a reader can
always tell which sha and which host a row came from.

The box protocol, which matters because the box is shared. **Announce in
the `nitro-testbox` room before starting**: the three-rate sweep takes
the machine for about twenty minutes, restarts `nitro-dev` between arms,
and **puts the human's screen at 1280×720 for the 240 Hz arm** — which is
why that arm is a reduced matrix and why the script says in the log when
it enters and leaves it. The human's `~/.config/nitro/server.conf` is
**backed up and restored** — a refresh sweep writes a `mode` or
`modeline` line into it and must hand back exactly what it found,
including his own `mode = 1920x1080@120` — and the deployed binaries are
restored byte-for-byte after a run.

**Check which binaries you measured, at both ends of the run.** Other
tasks deploy to this box and they do not read the room: this document's
own sweep was run once against binaries a different task deployed six
minutes before the first arm, and nothing in the output said so
(§9.7). An `md5sum` before the run answers "is this mine now"; a
long run has to answer "was it mine *throughout*", and only a pair of
fingerprints does. `deploy/bench.sh` now takes both, writes them into the
ledger, and refuses to call the result a measurement if they differ. A
before/after pair should also check the *benchmark's own* binary is
identical across the two, since it is the instrument —
`docs/latency.md` §6 lost a measurement to exactly that. Finally,
**take both arms of a pair in the same sitting**: this box's numbers
drift by the day, so `deploy/bench.sh` runs boing/boing-node,
starfield/starfield-nodes, balls/balls-nodes and text/text-static back to
back — and now at **both sizes**, so a VGA pair and a fullscreen pair are
each within-sitting. Every ratio in §7 is within-sitting for that reason.
Two runs a day apart are not a comparison.

## 11. What this does not measure

Rigour here is worth more than any table above, because the failure mode
of a benchmark document is a reader who takes it for more than it is.

- **No latency.** Not one number here is an input-to-photon figure.
  Throughput and latency are different properties and a system can be
  excellent at one and terrible at the other; `docs/latency.md` is the
  other dossier and the one a desktop is judged by.
- **One client.** Every run is a single benchmark window plus the shell.
  Nothing here says what happens with five animating clients, which is a
  materially different scheduling problem — the server's paint is serial
  and its damage union is global.
- **No GPU path.** Everything measured is the CPU rasteriser writing into
  a heap shadow buffer and copying damage into a dumb buffer
  (`docs/latency.md` §4.5). A Vulkan or KMS-plane path would change every
  server column and none of the client ones.
- **One output**, scale 1: no multi-monitor, no mixed scales, no hotplug
  during a run.
- **Three resolutions**, 640×480, 1280×720 and 1920×1080 — but not
  evenly. Every pair is measured at 640×480 and fullscreen (§4), which is
  what makes §7.8's crossover a bracket rather than an extrapolation;
  720p appears only in the 240 Hz arm and only for the reduced matrix,
  so it is a third point for the scenarios in that arm and absent for
  the rest.
- **Three refresh rates** (§9), and the third one changes two things at
  once: the 240 Hz arm is 720p, so its column moves the rate *and* the
  pixel count together and cannot separate them. The 640×480 rows are
  the control that can — identical scenes at all three rates — and they
  are what §9.2's flat-per-frame result rests on.
- **One box, and now three sittings and three shas** on `ubuntu`:
  `1f35491` (2026-09-16, three rates, §6–§9 and every prose number in
  §7.1–§7.10), `49d023b`/`28b6fdd` (2026-09-20, the #569 pair, §7.10a)
  and `5e4b02e` (2026-09-20, two arms, §7.10b and the current fullscreen
  figures). **Within a sitting the ratios are the durable part**; across
  sittings this box's numbers drift day to day, and the absolute
  microsecond figures should be expected to move by a few percent on a
  re-run. The measured `copy` bandwidth moved 3.43 → 3.56 GB/s between
  the first and third sittings, which is roughly the size of drift to
  expect. §7.10b's cross-sitting comparison carries three retained-path
  control rows, flat to within 1 %, for exactly this reason.
- **CPU is quantised to 10 ms ticks** (§2): the sub-100 µs client figures
  mean "under a hundred microseconds", not three significant figures.
  And **`damage_px_mean` includes the age-2 union** `damage(n) ∪
  damage(n-1)`, the region the back buffer must be brought up to date
  over — so it over-counts the newly-dirtied area by however much two
  consecutive frames fail to overlap: a rim for a slow sprite
  (`boing-node`'s 170 014 against a 151 321-pixel ball) and up to a
  factor of two for something that jumps. Comparable *between* rows,
  which is how it is used, and not against a naive "pixels the client
  changed" count.

- **Every effect's own compute cost is a property of this CPU.** A
  Pentium G3240 has two Haswell cores, SSE4.2 and **no AVX2**, and every
  fullscreen effect in §7.10 is a tight scalar loop over 2 073 600 pixels
  that a machine with AVX2 would run two to four times faster. Plasma's
  46 815 µs/frame is a statement about this silicon. That is precisely
  why the crate reports `compute_us` and `upload_us` separately from the
  server's CPU: **the server's share is separable, and it is the only
  share that is about nitro.** Read the server columns as the product's
  numbers and the client columns as the box's.
- **Nothing here is an idle measurement.** "Idle means zero CPU" is goal
  1's other half and is measured in `docs/latency.md` §4.3 — 0 frames and
  0 CPU ticks over five seconds with clients connected. A throughput
  suite by construction never idles.

### 11.1 A test that checks something adjacent to the claim

The limits above are about a reader taking this document for more than it
is. A green test is the same transaction with a machine, and it fails the
same way:

> **A green test is evidence only about what it looked at.** If the
> assertion never reads the artefact the claim is about — the other file,
> the other parity, the second frame — it is a test of something adjacent
> to the claim, and it stays green while the claim is false.

This is worse than a missing test. A missing test is visible; this one is
silent by construction, and the tick is read as coverage of the claim in
the test's name. §8 D is its near neighbour and a different rule: there,
the assertion is on a *proxy quantity* and a system broken in the
direction the metric does not look satisfies it. Here the predicate can be
exactly right and still be evaluated over the wrong *artefact*.

Four instances in one round of work:

- **#566** — a bound that held at one parity, so half the inputs were
  never really constrained (§8 D; `crates/nitro-ui/tests/list.rs`).
- **#584** — one screenshot proved *a* picture reached the screen, not a
  *moving* one; the defect survived the crate's entire history behind a
  comment asserting the correct design (§4, §8 D;
  `crates/nitro-bench/tests/against_server.rs`).
- **#570** — the acceptance test it proposed would have measured the
  instrument rather than the claim, and passed on a visibly corrupt
  screen (§8 D).
- **#569, twice** — and this is the one that earns the entry. The guard
  `the_unsafe_surface_is_exactly_what_the_docs_claim` counted this
  crate's `unsafe` blocks in `map.rs` and never read the documents
  quoting that count, so it could not catch the stale figure in
  `DEPENDENCIES.md` — exactly the drift it existed to prevent. When that
  was fixed, the replacement matcher judged only the word immediately
  before "block(s)" and was blind to most of the files it named,
  because those files write the count with a modifier in between. It
  passed on clean text and caught the one defect it was tested with.

The fourth is the point: **the guard built because of the first three was
itself subject to the pattern, twice** — and the second time it was
verified by breaking a document, by the author and independently by the
reviewer, both of whom concluded it was sound.

Two sub-rules came out of that, and they are the reusable part:

> **Breaking one input is evidence only for the input you broke.** A guard
> over N sites needs N breaks.

The matcher was verified against the single file whose phrasing it
happened to fit. What resolved it was running the matcher across *all* its
inputs, not testing one input harder.

> **A checker that finds nothing must say so.** "No findings" and "cannot
> look" are indistinguishable from outside, and the harmless-looking one
> is usually wrong.

The remedy is the assertion in
`the_doc_count_guard_catches_drift_in_every_phrasing`: every named input
must yield at least one *judged* site, which turns silent blindness into a
loud failure. That assertion is the general fix — correcting the matcher
alone would have left the method intact.

The worked example is `crates/nitro-shm/tests/seals.rs` (commits
`c025a5e`, `5921684`). Its comments record all four matcher attempts and
why two of them would have shipped green, and `DOCS_QUOTING_THE_COUNT`'s
doc comment states the guard's own remaining limit rather than implying
it. Read it there rather than re-deriving it here.

