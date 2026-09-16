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
Everything below is from one such ledger, checked in as
`docs/bench-72ef50b.jsonl`: 54
scenario runs plus one bandwidth measurement, sha `72ef50b`, host
`ubuntu`, taken 2026-09-16 in a single sitting at 1920×1080@60.

**Headline: the same bouncing ball costs 20 875 µs of client CPU per
frame as a fullscreen pixel buffer and 83.5 µs as a moved sprite node,
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
then run frame-paced against `Frame` callbacks — eight seconds for every
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
`docs/bench-72ef50b.jsonl`, so every number in this document can be
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

<!-- generated by `nitro-bench report`: 54 runs -->

### rects

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rects n=10 640x480 | 60.0 | 60.0 | 10.0 | 812.5 | 62.5 | 394.0/475.0 | 162.0 | 119201 | 194 | 0 | ok |
| rects n=100 640x480 | 60.0 | 60.0 | 100.0 | 3187.5 | 83.3 | 2407.0/2470.0 | 390.0 | 322861 | 1724 | 0 | ok |
| rects n=500 640x480 | 60.0 | 60.0 | 500.0 | 11187.5 | 125.0 | 9888.0/10642.0 | 404.0 | 334841 | 8524 | 0 | ok |
| rects n=1000 640x480 | 59.9 | 60.0 | 1000.0 | 11920.7 | 187.9 | 10763.0/11244.0 | 231.0 | 334841 | 17024 | 0 | ok |
| rects n=2000 640x480 | 59.7 | 59.9 | 2000.0 | 12364.0 | 146.4 | 11183.0/11322.0 | 177.0 | 334841 | 34024 | 0 | ok |
| rects n=500 640x480 | 60.0 | 60.0 | 500.0 | 9833.3 | 111.1 | 8674.0/9057.0 | 399.0 | 336435 | 8524 | 9 | ok |

### rects-move

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rects-move n=10 640x480 | 60.0 | 60.0 | 10.0 | 833.3 | 62.5 | 419.0/649.0 | 151.0 | 120811 | 304 | 0 | ok |
| rects-move n=100 640x480 | 60.0 | 60.0 | 100.0 | 3354.2 | 83.3 | 2550.0/2611.0 | 397.0 | 325141 | 2824 | 0 | ok |
| rects-move n=500 640x480 | 59.9 | 60.0 | 500.0 | 11649.3 | 146.1 | 10310.0/10584.0 | 399.0 | 337161 | 14024 | 0 | ok |
| rects-move n=1000 640x480 | 59.9 | 60.0 | 1000.0 | 11941.5 | 167.0 | 10784.0/11145.0 | 229.0 | 337161 | 28024 | 0 | ok |
| rects-move n=2000 640x480 | 57.2 | 57.4 | 2000.0 | 13122.3 | 196.5 | 11558.0/11676.0 | 176.0 | 337161 | 56024 | 0 | ok |

### text

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| text n=10 640x480 | 59.9 | 60.0 | 10.0 | 2254.7 | 125.3 | 83.0/88.0 | 8.0 | 8160 | 474 | 0 | ok |
| text n=10 640x480 | 60.0 | 60.0 | 10.0 | 2416.7 | 125.0 | 213.0/483.0 | 49.0 | 39168 | 474 | 0 | ok |
| text n=100 640x480 | 60.0 | 60.0 | 100.0 | 6937.5 | 229.2 | 666.0/698.0 | 103.0 | 84240 | 4524 | 0 | ok |
| text n=100 640x480 | 60.0 | 60.0 | 100.0 | 8270.8 | 229.2 | 1647.0/1947.0 | 350.0 | 271296 | 4524 | 0 | ok |
| text n=500 640x480 | 60.0 | 60.0 | 500.0 | 12125.0 | 291.7 | 1502.0/1724.0 | 175.0 | 293904 | 22524 | 0 | ok |
| text n=500 640x480 | 60.0 | 60.1 | 500.0 | 12041.7 | 291.7 | 2338.0/2573.0 | 197.0 | 271296 | 22524 | 0 | ok |

### text-static

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| text-static n=10 640x480 | 59.9 | 60.0 | 10.0 | 334.0 | 41.8 | 83.0/85.0 | 8.0 | 8398 | 304 | 0 | ok |
| text-static n=100 640x480 | 60.0 | 60.0 | 100.0 | 1208.3 | 62.5 | 660.0/755.0 | 108.0 | 86130 | 2824 | 0 | ok |
| text-static n=500 640x480 | 60.0 | 60.0 | 500.0 | 4604.2 | 125.0 | 3052.0/3366.0 | 371.0 | 300498 | 14024 | 0 | ok |

### putimage

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| putimage 640x480 | 60.0 | 60.0 | 1.0 | 395.8 | 666.7 | 72.0/74.0 | 10.0 | 10000 | 56 | 0 | ok |
| putimage 640x480 | 60.0 | 60.0 | 1.0 | 770.8 | 3875.0 | 302.0/332.0 | 85.0 | 62500 | 56 | 0 | ok |
| putimage 640x480 | 59.9 | 60.0 | 1.0 | 1565.8 | 9540.7 | 753.0/977.0 | 232.0 | 250010 | 56 | 0 | ok |
| putimage 640x480 | 30.1 | 30.0 | 1.0 | 5892.1 | 27219.9 | 1192.0/4261.0 | 859.0 | 764640 | 56 | 0 | **slow** |

### scroll

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| scroll n=500 640x480 | 59.9 | 60.0 | 1.0 | 1586.6 | 41.8 | 815.0/896.0 | 373.0 | 304768 | 52 | 0 | ok |

### create

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| create n=50 640x480 | 60.1 | 60.1 | 153.0 | 686.1 | 83.2 | 246.0/277.0 | 30.0 | 29281 | 3385 | 0 | ok |

### plasma

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| plasma 640x480 | 59.9 | 60.0 | 1.0 | 1711.9 | 9686.8 | 821.0/927.0 | 256.0 | 307200 | 56 | 0 | ok |
| plasma 1920x1080 | 15.0 | 15.0 | 1.0 | 14416.7 | 49833.3 | 3601.0/7249.0 | 2114.0 | 2073600 | 56 | 16668 | **dropped** |

### fire

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fire 640x480 | 60.0 | 60.0 | 1.0 | 2812.5 | 9354.2 | 1439.0/1532.0 | 411.0 | 307205 | 56 | 0 | ok |
| fire 1920x1080 | 30.1 | 30.0 | 1.0 | 16556.0 | 21327.8 | 3757.0/9388.0 | 3027.0 | 2073600 | 56 | 0 | **slow** |

### rotozoom

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rotozoom 640x480 | 59.9 | 60.0 | 1.0 | 2839.2 | 4258.9 | 1440.0/1572.0 | 428.0 | 307200 | 56 | 0 | ok |
| rotozoom 1920x1080 | 59.9 | 60.0 | 1.0 | 16743.2 | 12463.5 | 11297.0/11475.0 | 2659.0 | 2073600 | 56 | 0 | ok |

### boing

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing 640x480 | 60.0 | 60.0 | 1.0 | 3083.3 | 7020.8 | 1721.0/1803.0 | 408.0 | 307200 | 56 | 0 | ok |
| boing 1920x1080 | 30.0 | 30.0 | 1.0 | 20500.0 | 20875.0 | 6066.0/12195.0 | 2650.0 | 2073600 | 56 | 0 | **slow** |
| boing 640x480 | 60.0 | 60.0 | 1.0 | 2958.3 | 6875.0 | 1583.0/3407.0 | 411.0 | 307200 | 56 | 0 | ok |
| boing 1920x1080 | 30.0 | 30.0 | 1.0 | 20333.3 | 20750.0 | 6062.0/12262.0 | 2613.0 | 2073600 | 56 | 0 | **slow** |

### boing-node

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing-node 640x480 | 59.9 | 60.0 | 1.0 | 2296.5 | 62.6 | 2014.0/2099.0 | 42.0 | 33097 | 52 | 0 | ok |
| boing-node 1920x1080 | 59.9 | 60.0 | 1.0 | 10041.8 | 83.5 | 9659.0/9749.0 | 208.0 | 170014 | 52 | 0 | ok |

### starfield

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield n=100 640x480 | 59.9 | 60.0 | 1.0 | 2860.1 | 1169.1 | 1536.0/1659.0 | 397.0 | 307205 | 56 | 0 | ok |
| starfield n=100 1920x1080 | 60.0 | 60.1 | 1.0 | 16708.3 | 10083.3 | 11697.0/11806.0 | 2179.0 | 2073600 | 56 | 0 | ok |
| starfield n=500 640x480 | 59.9 | 60.0 | 1.0 | 2860.1 | 1231.7 | 1511.0/1883.0 | 398.0 | 307210 | 56 | 0 | ok |
| starfield n=500 1920x1080 | 60.0 | 60.0 | 1.0 | 16645.8 | 9979.2 | 11668.0/11772.0 | 2182.0 | 2073600 | 56 | 0 | ok |
| starfield n=2000 640x480 | 59.9 | 60.0 | 1.0 | 2839.2 | 1336.1 | 1510.0/1539.0 | 406.0 | 307200 | 56 | 0 | ok |
| starfield n=2000 1920x1080 | 59.8 | 60.0 | 1.0 | 16701.5 | 10041.8 | 11707.0/11862.0 | 2184.0 | 2073600 | 56 | 0 | ok |

### starfield-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield-nodes n=100 640x480 | 60.0 | 60.0 | 100.0 | 1416.7 | 83.3 | 643.0/705.0 | 370.0 | 296298 | 2824 | 0 | ok |
| starfield-nodes n=100 1920x1080 | 59.9 | 60.0 | 100.0 | 7473.9 | 104.4 | 4500.0/4958.0 | 2556.0 | 1995560 | 2824 | 0 | ok |
| starfield-nodes n=500 640x480 | 59.9 | 60.0 | 500.0 | 2505.2 | 146.1 | 1179.0/1194.0 | 387.0 | 309444 | 14024 | 0 | ok |
| starfield-nodes n=500 1920x1080 | 59.9 | 60.0 | 500.0 | 8997.9 | 146.1 | 5361.0/5451.0 | 2642.0 | 2073600 | 14024 | 0 | ok |
| starfield-nodes n=2000 640x480 | 59.9 | 60.0 | 2000.0 | 6158.7 | 354.9 | 3084.0/3153.0 | 423.0 | 309444 | 56024 | 0 | ok |
| starfield-nodes n=2000 1920x1080 | 60.0 | 60.0 | 2000.0 | 11041.7 | 291.7 | 6593.0/6986.0 | 2402.0 | 2073600 | 56024 | 0 | ok |

### balls

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls n=32 640x480 | 60.0 | 60.0 | 1.0 | 2895.8 | 1458.3 | 1499.0/1666.0 | 399.0 | 307200 | 56 | 0 | ok |
| balls n=32 1920x1080 | 59.9 | 59.9 | 1.0 | 16638.8 | 10208.8 | 11682.0/11745.0 | 2140.0 | 2073600 | 56 | 0 | ok |

### balls-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls-nodes n=32 640x480 | 60.0 | 60.0 | 32.0 | 1687.5 | 62.5 | 1111.0/1332.0 | 52.0 | 46274 | 920 | 0 | ok |
| balls-nodes n=32 1920x1080 | 59.9 | 60.0 | 32.0 | 5595.0 | 83.5 | 3147.0/3742.0 | 613.0 | 508737 | 920 | 0 | ok |

The second `rects n=500` row is the **control**: the same scenario with
the shell clients killed, so a reader can see what the bar and the
launcher cost every other row. It is 9 833 µs/frame against 11 188 with
the shell up — **1 355 µs/frame, 12 % of the server's per-frame CPU**,
and the same gap is visible in the server's own paint counter (8 674
against 9 888). No verdict here turns on it. Read it with the caveat the
ledger's own note records: `nitro-session` supervises its children and
restarts them within about a second, so the control arm is three seconds
long and the shell was back up for most of it — so the control arm still
paid most of the shell's cost, and 1 355 µs is therefore a *floor* under
it rather than a ceiling. The way to tighten it would be to stop the
session rather than kill the processes.

## 7. Verdict per scenario

### 7.1 `rects` — the x11perf number, transposed

`x11perf -rect100`, as N nodes that recolour every frame — all of them,
because a node that does not change costs the server nothing, which is
the property under test. The sweep scales cleanly to the end: **n=2000 at
59.7 presented/s, 12 364 µs/frame of server CPU**, verdict `ok`, with no
rise at all in the server's worst flip interval. Nothing here broke.

**So this box sustains at least 2000 rect mutations per frame at
60 Hz** — and the sweep stopped because it ran out of *damage*, not
because it ran out of headroom in mutations. That is the closest thing in
this document to a classical x11perf result, and the sentence §2
promised: not ops/s, but how much retained mutation fits in a frame.
(A previous version of this document reported a cliff between n=1000 and
n=2000, from a run that dropped to 43.6/s. It does not reproduce on the
fixed build.)

The cost is nearly linear in N and then flattens hard: 3 188 µs/frame at
n=100, 11 188 at n=500, 11 921 at n=1000, 12 364 at n=2000 — **11 % more
for four times the nodes** across those last three, because by n=500 the
grid already covers 334 841 damage pixels and stops growing, after which
the marginal cost is per-node bookkeeping against a fixed rasterised
area. `paint_us_mean` says the same thing more directly: 9 888 at n=500
against 11 183 at n=2000, 1 295 µs more for 1 500 more nodes, while the
wire traffic quadruples from 8 524 to 34 024 bytes per frame. The client
is never the limit: **187.9 µs/frame at n=1000**, some sixty times below
the server (11 920.7 ÷ 187.9 = 63×). Sending two thousand mutations is
cheap; painting them is not — and past n=500 this window cannot get any
dirtier, so finding the true mutation ceiling needs a bigger window or
smaller rects, which is the sweep to run next.

### 7.2 `rects-move` vs `rects` — the damage union costs very little

The pair exists because recolouring dirties each node's own bounds while
*moving* dirties the union of the old and the new, and a benchmark that
did only one of them would have missed the more expensive half. Moving
**is** the more expensive half, by a small and fairly consistent margin:

| n | `rects` (recolour) µs/frame | `rects-move` µs/frame | rects damage px | rects-move damage px |
|---|---|---|---|---|
| 10 | 812.5 | 833.3 | 119 201 | 120 811 |
| 100 | 3 187.5 | 3 354.2 | 322 861 | 325 141 |
| 500 | 11 187.5 | 11 649.3 | 334 841 | 337 161 |
| 1000 | 11 920.7 | 11 941.5 | 334 841 | 337 161 |
| 2000 | 12 364.0 | **13 122.3** | 334 841 | 337 161 |

The moving arm costs the server **2.6 / 5.2 / 4.1 / 0.2 / 6.1 % more**
(the ratio of the two server columns at each n) and damages **2 320 more
pixels** from n=500 on. Both hold the refresh until the very end, where
the moving arm is the one that gives: **57.2 presented/s against 59.7**
at n=2000. Neither is marked `**dropped**` — no flip gap on either — so
the move arm's loss is 57.2/60, a client-side shortfall in commits
(57.4/s) rather than a missed vblank.

So: **the damage union costs something, and very little.** Two thousand
rects moving two pixels each cost about 2 300 extra damage pixels and
under 800 µs of server CPU a frame over recolouring the same two
thousand. The reason is visible in the damage column and is worth more
than the microseconds: from n=500 onward **both arms saturate at ~335 000
damaged pixels**, against a 640×480 window of 307 200. The rects tile the
window; past a certain density everything is dirty either way, and the
old∪new union of a two-pixel move adds only a rim. A union is only
expensive when the thing that moved is *sparse*, which is the boing ball
(§7.7) and not this.

**The previous version of this document reported the opposite** — the
moving arm cheaper at every sweep point, written up as "damage union is
not the bottleneck" — and it was an artefact of the benchmark. The move
arm cycled eight phases that collapsed to four positions in consecutive
pairs, and `Scene::set_bounds` early-returns on unchanged bounds, so
**half its `SetBounds` were server-side no-ops** while the recolour arm
it was being compared against changed every node on every frame. The arm
was doing half the work and the table dutifully reported it. The orbit is
now four phases in which consecutive frames always differ on at least one
axis, pinned by `every_moving_frame_actually_moves_every_rect` (and its
twin on the recolour side, because a repeated colour is an equally silent
no-op). The folk belief the old text set out to refute — "moving things
is expensive, recolouring is cheap" — turns out to be mildly true and
mostly irrelevant at this density.

### 7.3 `text` vs `text-static` — the retained-text result

The one x11perf could not have run. X11 has no retained text: a moved
string is a redraw, so it costs exactly what a new one costs. Here the
string is unchanged, so the server re-uses its layout and its glyph tiles
and does nothing but composite.

| n | `text` server µs/frame | `text-static` server µs/frame | ratio | `text` layouts shaped | `text-static` layouts shaped |
|---|---|---|---|---|---|
| 10 | 2 254.7 | 334.0 | 6.75× | 4 800 | 3 |
| 100 | **6 937.5** | **1 208.3** | **5.74×** | 48 000 | **2** |
| 500 | 12 125.0 | 4 604.2 | 2.63× | 240 000 | **0** |

The ratios are `text` ÷ `text-static` at the same n, on the server
µs/frame column (itself `server_cpu_us ÷ presented`), and the layout
counts are the server's own `text_layouts` counter differenced across the
run: `text` at n=100 shapes **100 layouts per frame, 48 000 over 480
commits**, and `text-static` at the same n shapes **two**, over eight
seconds and 480 presented frames — and at n=500, zero. The handful in the
static arms is the shell, not the scenario; see the glyph paragraph
below.

`glyph_renders` tells the same story and is the stronger claim, because
it is the expensive half: rasterising a glyph into the atlas. It does not
move at all in the n=100 and n=500 static arms (123 → 124 and 124 → 124).
In the n=10 arm it rises by 4 — and the `rects n=10` arm's rises by 6, in
a scenario that draws no text whatsoever. That is the **bar's clock**
redrawing behind the benchmark window, which is what "the real desktop
was up" costs a counter, and why the larger sweep points are the ones to
quote.

**A moved label re-uses its layout and its glyph tiles.** The
microseconds are corroboration; the two pinned counters are the result.
`crates/nitro-bench/tests/against_server.rs` asserts the `text_layouts`
half against a real server on the fake backend, so a regression is caught
in CI and not only on the box.

The 24-pixel rows are the `-f24text` sweep — the second row of each pair
in §6's `text` table, which the label no longer distinguishes, because a
non-square run now prints its geometry instead of its `size=`. They say
something smaller: at n=100, doubling the font size costs **8 271 against
6 938 µs/frame**, a 19 % increase for 3.2× the damaged area (271 296
against 84 240 px). Shaping dominates rasterisation at this size.

### 7.4 `putimage` — where the client becomes the limit

`x11perf -putimage100/-putimage500`: a client buffer rewritten and
re-uploaded every frame. Damage scales as the square of the edge, which
is the check that the scenario is measuring what it claims:

| size | damage px | expected (size²) | server µs/frame | client µs/frame | presented/s |
|---|---|---|---|---|---|
| 100 | 10 000 | 10 000 | 395.8 | 666.7 | 60.0 |
| 250 | 62 500 | 62 500 | 770.8 | 3 875.0 | 60.0 |
| 500 | 250 010 | 250 000 | 1 565.8 | 9 540.7 | 59.9 |
| 1080 | 764 640 | — | 5 892.1 | **27 219.9** | **30.1** |

The three small points land on size² to within ten pixels (`starfield` at
640 shows the same consistent +5, an artefact of the counter and not of
the scenario). The 1080 row does not, and the reason is worth recording:
the buffer is 1080×1080 in a 640×480 window, so it overflows and is
**clipped by the output's bottom edge** — 764 640 is exactly 1080 × 708.
Damage is what is on the screen, not what the client wrote.

At 1080 the run presents **30.1/s and is client-bound**: the client's own
effect plus upload is **27 276 µs/frame against the server's 5 892**, and
the verdict says `**slow**` rather than `**dropped**` — the server
flipped on time every time, there was simply nothing new to flip on half
of them. The split the crate measures makes the blame precise: of the
client's 27 276 µs per commit, **25 890 is the effect and 1 386 is the
`pwrite`** (`compute_us ÷ commits` and `upload_us ÷ commits`). The upload
is 5 % of the client's cost, so making the wire faster would buy this
scenario almost nothing.

That upload column is itself a finding. The tree writes `pwrite` rather
than `mmap` because mapping needs `unsafe`, which this workspace denies —
so the upload column is a syscall per frame that a mapped buffer would
not pay. At 1080p it is **1 386 µs/frame**, about 8 % of a 60 Hz budget.
That is the price of the `#![forbid(unsafe_code)]` rule, measured, and it
is small.

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
height — a thousand rows would send the same 52 bytes — for **1 587
µs/frame of server CPU**, 304 768 damage pixels and 59.9 presented/s, of
which the server's own counters account for 815 µs of paint and 373 of
copy.

The window is 640 × 480 = 307 200 pixels, so **a one-row scroll damages
0.99 of the viewport**: essentially exactly one full viewport repaint,
every frame, to move the content up sixteen pixels. State it plainly as
the finding it is: **this server repaints the viewport rather than
blitting it, so the cost of a scroll is the size of the viewport and not
the size of the exposed line.** X11's `CopyArea` moved the existing
pixels and repainted only the newly exposed row — 640 × 16 = 10 240
pixels. 304 768 ÷ 10 240 is a factor of **~30**, and it is the factor to
quote.

A previous version of this document reported 675 696 px, "2.2
viewports", and explained the excess as the age-2 damage union plus the
shell. That was the scenario's own defect, not the server's: its single
group was set to `(0, offset, w, h + content)` every frame, so the clip
rectangle was taller than the window from frame 0 and **clipped nothing
at all**, leaving the rows to damage area outside the viewport. With a
clipper that actually clips, the number is a clean one-viewport repaint
and the finding *survives in a stronger form* — there is no longer a
residual to explain away.

That is a real difference from X11's model and not obviously the wrong
choice: a blit-based scroll needs the server to prove nothing else
changed in the region, and at 1.6 ms a viewport repaint fits inside a
16.7 ms budget ten times over. But it is the number to point at if
`nitro-term` ever wants a fast full-screen scroll, and the number a
copy-based optimisation would have to beat.

### 7.6 `create` — menus and tooltips are cheap

`x11perf -create/-map`: the one scenario whose *units* survive the
transposition unchanged, because creating a subtree is not a per-frame
paint and "how many per second" really is the question. A retained client
creating and destroying a subtree is the toolkit operation behind opening
a menu, a tooltip or a dialog — where a user notices latency most and
where a benchmark almost never looks.

**153 mutations per frame** — one `DestroyNode`, one group, and 50 rects
with bounds and fills — at **686.1 µs/frame of server CPU**, 60.1
presented/s, 29 281 damage pixels, 3 385 bytes/frame. Creating and
destroying fifty nodes sixty times a second costs the server about 4 % of
a frame budget (686.1 ÷ 16 667): menus, tooltips and dialogs are cheap.
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
| presented/s | 60.0 | 59.9 | **30.0** | **59.9** |
| client µs/frame | 7 020.8 | **62.6** | **20 875.0** | **83.5** |
| — of which effect | 6 373.2 | — | 16 926.0 | — |
| — of which upload (`pwrite`) | 598.8 | — | 3 904.2 | — |
| server µs/frame | 3 083.3 | **2 296.5** | 20 500.0 | **10 041.8** |
| damage px | 307 200 | **33 097** | 2 073 600 | **170 014** |
| bytes/frame | 56 | **52** | 56 | **52** |
| verdict | `ok` | `ok` | `**slow**` | `ok` |

At 1080p: **250× less client CPU** (20 875.0 ÷ 83.5), **12.2× less
damage** (2 073 600 ÷ 170 014), **2.0× less server CPU** — and, the part
that is not a ratio, the retained arm **holds 60 Hz where the pixel arm
cannot**. Both arms were run twice, back to back, and reproduced
themselves: 30.0 presented/s both times at 1080p, 20 500.0 and 20 333.3
µs/frame of server CPU; 3 083.3 and 2 958.3 at VGA. (The table quotes the
first of each pair; the second is the within-sitting twin §10 asks for.)
This is `DESIGN.md` goal 1, measured. The ball's 389×389
bounding box is **7.3 % of the screen** and the retained arm damages
**8.2 %** of it — the extra being the trailing edge it left behind. The
pixel arm damages 100 %, every frame, for the same ball.

**And the retained arm wins at VGA too**, which is the more careful
claim: 2 296.5 against 3 083.3 µs of server CPU (1.34×), 62.6 against
7 020.8 of client (112×), 33 097 damage pixels against the full 307 200
(9.3×). The 173-pixel ball is 9.7 % of a 640×480 window and the node arm
damages 10.8 % of it — the same shape of result, one sixth the size. So
the pixel arm's defeat at 1080p is not merely "it hit the bandwidth
wall": it loses on every column at a resolution where there is no wall to
hit, and 1080p only widens the margin.

Two honest qualifications. The retained arm's server cost is only
**2.0×** better at 1080p, not 250×: the server still rasterises and
blends a 389×389 alpha sprite into the scene twice per frame (old
position and new), and at 9 659 µs of mean paint that is not nothing. The
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
| 100 | 16 708.3 | **7 473.9** | 2.24× | 10 083.3 | 104.4 | 2 824 |
| 500 | 16 645.8 | **8 997.9** | 1.85× | 9 979.2 | 146.1 | 14 024 |
| 2000 | 16 701.5 | **11 041.7** | 1.51× | 10 041.8 | 291.7 | 56 024 |

The node arm wins at every N measured, and on the client side it is not
close: **292 µs/frame against 10 042 at n=2000**, a factor of 34. Both
arms hold ~60 Hz throughout, so this is entirely a CPU-per-frame story,
which is what §2 said the headline column was for.

**At 640×480 it does lose, and that is the interesting half.** Every pair
now runs at both sizes, and the VGA column has the answer the previous
version of this section could only extrapolate towards:

| n | pixels server µs/frame | nodes server µs/frame | ratio | pixels client | nodes client |
|---|---|---|---|---|---|
| 100 | 2 860.1 | **1 416.7** | 2.02× | 1 169.1 | 83.3 |
| 500 | 2 860.1 | **2 505.2** | 1.14× | 1 231.7 | 146.1 |
| 2000 | 2 839.2 | 6 158.7 | **0.46×** | 1 336.1 | 354.9 |

**The crossover is observed, between n=500 and n=2000 at VGA.** The node
arm is 2× cheaper at a hundred stars, barely ahead at five hundred, and
**2.2× more expensive at two thousand** (6 158.7 ÷ 2 839.2). Interpolating
linearly between the two node points either side — (500, 2 505.2) and
(2000, 6 158.7), 2.44 µs per star — against the pixel arm's flat 2 853 µs
mean puts the crossing at about **n ≈ 640**; the least-squares line
through all three node points gives 2.48 µs/star and n ≈ 660. Both are
interpolations between measured points rather than extrapolations past
them, which is why they can be quoted at all.

At 1080p the same arithmetic puts the crossing far out of reach: 1.74
µs/star and 7 666 µs of fixed cost from the three node points, meeting
the buffer arm's ~16 685 µs at **n ≈ 5 200**, and the outer two points
alone (500 and 2000) give 1.36 µs/star and n ≈ 6 100. Those *are*
extrapolations, more than twice past the last measurement, and the curve
is visibly not a line — the step from 100 to 500 costs 3.8 µs/star and
the step from 500 to 2000 costs 1.4 — so the honest statement at 1080p
remains "somewhere in the thousands". `starfield-nodes --n 4000` and
`--n 8000` at 1080p would settle it.

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
`damage_px_mean` is 1 995 560 at n=100 and the full 2 073 600 at n=500
and above at 1080p, and 296 298 → 309 444 at VGA against a 307 200-pixel
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
56**. That is 3.4 MB/s of socket traffic, nothing locally, and a fact
worth remembering for `docs/remote.md`'s TCP path, where two thousand
animated nodes would be 27 Mb/s on the wire.

### 7.9 `balls` vs `balls-nodes` — antialiased circles, cheaply

Thirty-two bouncing circles, at both sizes. A rounded rect with
`corners = d/2` **is** an antialiased circle, so the node arm is asking
the server's rounded-rect path for the most expensive per-pixel work in
the whole rect family, thirty-two times a frame.

| | `balls` 640×480 | `balls-nodes` 640×480 | ratio | `balls` 1920×1080 | `balls-nodes` 1920×1080 | ratio |
|---|---|---|---|---|---|---|
| server µs/frame | 2 895.8 | **1 687.5** | **1.72×** | 16 638.8 | **5 595.0** | **2.97×** |
| client µs/frame | 1 458.3 | **62.5** | 23× | 10 208.8 | **83.5** | 122× |
| damage px | 307 200 | **46 274** | 6.64× | 2 073 600 | **508 737** | 4.08× |
| bytes/frame | 56 | 920 | 0.06× | 56 | 920 | 0.06× |
| paint µs mean | 1 499 | 1 111 | 1.35× | 11 682 | 3 147 | 3.71× |

**The server draws 32 antialiased circles, and their motion damage, for
5 595 µs at 1080p — a third of what it costs to take one memcpy of the
screen through the pixel path.** Read the damage figure alongside it:
508 737 pixels, **24.5 % of the screen**, which is what "work
proportional to what changed" looks like when the change is genuinely
localised — compare the starfield, where the same claim was true of CPU
and not of damage. 920 bytes per frame (32 `SetBounds` and the commit) is
the entire wire cost of the animation, at either size.

The VGA column is the same result with the bandwidth argument removed:
**1 688 µs against 2 896**, and damage down to 46 274 px — **15.1 % of a
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
| plasma | 59.9 | **15.0** | **49 833.3** | **47 194.2** | 14 416.7 |
| fire | 60.0 | **30.1** | 21 327.8 | **17 126.8** | 16 556.0 |
| rotozoom | 59.9 | **59.9** | 12 463.5 | 6 881.5 | 16 743.2 |

At 640×480 all three hold 60 Hz comfortably and the server's share is
1 712 / 2 813 / 2 839 µs per frame: the period-correct resolution is a
solved problem on hardware two decades younger than the effects. Their
effects cost 9 338 / 8 751 / 3 639 µs and their uploads 283 / 530 / 555,
so even at VGA these three are already client-dominated — the compositor
is the cheap part of a demoscene effect at every size measured.

At 1080p, **plasma presents 15.0/s and it is not the compositor's
fault**: the client burns 49 833 µs per frame of which **47 194 is the
sine loop**, while the server's 14 417 µs would have fitted in the budget
with 2.3 ms to spare. Its verdict is `**dropped**` rather than `**slow**`
because at 15 fps the server's own worst flip interval rose to 50 009 µs
— three refresh periods — which is what a client presenting at a quarter
rate does to the flip cadence.

**Fire falls to 30.1/s at 1080p**, and it is the row whose story changed
most. Its effect is **17 127 µs/frame** — 1.96× its VGA cost of 8 751 for
6.75× the pixels, which is the sublinear-but-real scaling a cellular
automaton with a serial row dependency should show — and its `pwrite` is
another 4 262, for 21 389 µs of client work against a 16 667 µs budget.
The server's 16 556 is no help, but the verdict is `**slow**` and not
`**dropped**`: the flip cadence never broke (rise 0), there was simply
nothing new to show on every other vblank. A previous version of this
document had fire holding 59.9/s with an effect of only 2 933 µs — *less*
than the same effect at VGA, for four times the pixels — and printed that
impossible pair without remarking on it. §8 records why.

Rotozoom is now the only one of the three that holds ~60 Hz at 1080p, and
it does so with no margin at all. Its server cost is **16 743 µs against
a 16 667 µs budget** — 100.5 % of the frame — and it is still marked `ok`
because the server never actually missed a flip (rise 0) and presented
59.9/s. That row is the best single illustration of why this document
exists: fps says "fine", and CPU per frame says "one more window on that
screen and it is not". It gets there by having the cheapest effect of the
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
left to give.

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
  gives 304 768 — 0.99 of the viewport — and the finding behind #570 came
  out *cleaner*, with nothing left to explain away (§7.5).

Three properties, three regression tests, and one rule they share: each
test asserts the property the bad number violated, not the shape of the
code that produced it. Note also what none of them were: a wrong
formula, a mis-parsed field, a unit error. Every one was a scenario
quietly measuring a *different scene* from the one its name and this
document described, while every counter around it stayed self-consistent.

All five are the same lesson, and it is the one this project keeps
relearning — from `docs/latency.md`'s saturated-display trap, from the
`boing-node` resampling bug in §7.7, and from #3711:

> **The instrument agreed with the code because it was measuring the
> layer below the broken one.**

An eliminated loop does not report an error. It reports a great result.
Neither does a fire burning in a corner of its buffer, or a rect that
never moved: they report a number, and the number goes in a table, and
the table goes in a document like this one.

## 8a. The three worst numbers, filed

A benchmark that ends in a document is half a benchmark. The three
findings below are the worst numbers in the matrix and each is an open
issue, so that a future run has something to close rather than a
paragraph to re-read.

| issue | the number | what it is |
|---|---|---|
| **#568** | `paint_us_mean` **11 707 µs** for a fullscreen 1080p repaint — **70 %** of the 60 Hz frame, and more than a whole 120 Hz frame | Five fullscreen rows across three scenarios — rotozoom, starfield at all three N, balls — do genuinely different work per frame (a per-pixel gather, two thousand moving stars, thirty-two circles) and report near-identical server costs: **16 639–16 743 µs, paint 11 297–11 707**, because the server's work is a function of damaged area alone. A least-squares fit through `putimage`'s four damage/paint points extrapolates to ~3 060 µs at 2 073 600 px against the 11 300–11 700 measured, so **a fullscreen repaint is ~3.8× more expensive per pixel than a large partial one** — there is a fixed per-frame cost that is not the linear part, and that gap is what to profile. |
| **#569** | `upload_us` **2 570–6 044 µs/frame** at 1080p — for `starfield` and `balls`, **more than the effect itself** | The `pwrite`/`pread` pair the buffer path pays because `mmap` is `unsafe` in this tree and a client can shrink a memfd into a SIGBUS under the server's mapping. `F_SEAL_SHRINK` is the fix that exists and is not taken; the price of not taking it is now measured at 5–60 % of the client's whole frame, and at the top of that range (starfield n=100 at 1080p: 6 044 µs of upload against 3 908 of effect) the client spends more time handing the frame over than making it. Note the scope: the retained path never pays it (§7.7's 83.5 µs), so this is "make the escape hatch cheaper", not "the architecture is wrong". |
| **#570** | a one-row scroll damages **304 768 px = 0.99× the viewport** | The wire side is excellent — one mutation, 52 bytes, independent of content height. The server side repaints the whole viewport where `CopyArea` moved a line and repainted only the exposed 640×16 = 10 240 px, a factor of ~30. Damaging the *symmetric difference* of a pure translation rather than the union would take this to ~20 000 px without touching the rasterizer, and `nitro-bench scroll` re-run is the proof it would land. It compounds with #568 for a fullscreen `nitro-term`. (The figure was 675 696 px in an earlier ledger, from a clipping group that clipped nothing; the issue is unchanged, its number is now honest — §7.5.) |

Two of the three are about the same underlying thing — **the server's cost
is proportional to damaged area, and the damage it computes is larger
than the area that actually changed.** That is not a contradiction of
`DESIGN.md` goal 1 so much as a statement of where the goal is currently
achieved at the wrong granularity: proportional to what the *scene graph*
thinks changed, rather than to what the pixels did.

## 9. Refresh rate: 60 Hz only, and why

Every row above was taken at **1920×1080@60**, and `refresh_mhz` is
60 000 in all 54 records. The panel also offers 1080p at **120, 85, 50
and 24 Hz**, and the 120 Hz column is the obvious follow-up — it is where
§5's arithmetic says the fullscreen pixel path stops keeping up, and
where §7.10's rotozoom row (100.5 % of a 60 Hz budget) becomes 200 % of a
120 Hz one.

It was not measured because **nitro had no mode-selection key when this
ran**. Task **#3718** adds `output.<connector>.mode = WxH@Hz`;
`deploy/bench.sh --modes "60 120"` already writes that line into the
human's `server.conf`, restarts the unit, and runs the whole matrix again
per mode. Two rules in that script are non-negotiable and both are scar
tissue. The human's `server.conf` is **backed up and restored**, never
rewritten from scratch — it carries his colour scheme and his keyboard
layout, and those are his. And after each restart the script **refuses
the arm unless the server's `outputs` reply confirms the mode**: a mode
line that matches nothing is a *warning plus the default*, so the server
comes back happily at 60 Hz and an arm that silently fell back would
produce a full, plausible, internally consistent 120 Hz column that was
actually a second 60 Hz column — and "120 Hz bought nothing" is exactly
what that looks like. `nitro-bench report` has a refresh-pivot table
waiting for the data; it prints nothing today because there is nothing to
pivot.

**That check parses the rate rather than matching a literal, and the
reason is worth a paragraph because the first version got it wrong.** A
real mode's refresh is almost never the round number you asked for. This
panel's "120 Hz" mode is clock 285 500 over a 2080×1144 total, which is
**119.982 Hz**, and `outputs` reports `@119982`; its 85 is 84.904. The
configuration line should still say `@120` — the key matches to the
nearest listed mode within half a hertz precisely so that a person writes
the round number — but a guard comparing the reply against the literal
`@120000` would **never match and would skip the 120 Hz arm for ever**,
silently, leaving a tidy `# SKIPPED` line in the ledger. That is the same
class of quiet failure the guard was written to catch, committed inside
the guard itself; #3718, who owns the key, spotted it. The check is now
"within 500 mHz of what was asked", which is the rule the server itself
applies, and the ledger's note records the rate `outputs` **reported**
rather than the one requested — a column labelled with the request would
be the request marking its own homework. Each record's `refresh_mhz` is
the client's independent reading of the same thing, from the `Frame`
callback's `refresh_ns`, so the two can be checked against each other.
`nitro-shot --modes` lists what a connector really offers, in the
spelling the key takes.

**1080p@240 is not reachable on this box, and that is arithmetic rather
than pessimism.** HDMI 1.4 on Haswell caps the TMDS clock near 300 MHz.
1080p@120 is 285.5 MHz — the last mode the EDID offers, fitting with
about 5 % to spare. 1080p@144 needs 346.5 MHz, 1080p@165 needs 401.0 and
1080p@240 needs roughly 606 MHz: none of them fit, and no amount of
software gets them. **1280×720@240** with CVT-RB timing is about 280 MHz
and might, which is what #3718 is testing.

**The link carries it; the panel is the open question.** #3718 set the
modeline (`279750 1280 1328 1360 1440 720 723 727 810 +hsync -vsync`) and
the kernel accepted it: the CRTC really is at 240, `outputs` reports
**`1280x720@239840 (custom)`** — CVT rounds the pixel clock down to a
0.25 MHz step, so "240" is 239.840 — and the server managed **219
flips/s** with a mean flip interval of **4 491 µs** against the 4 167 µs
period. Whether a photon reaches the glass is a question no instrument in
this repository can answer: `just shot` reads the *shadow* buffer and
returns a perfectly good 1280×720 frame whether or not the panel locked.
Only a human looking at the screen can settle it, which is the same rule
this project applies to every pixels claim, one layer further out.

If 720p@240 lands, the predictions worth writing down in advance so they
can be wrong in public. The frame is **1 280 × 720 × 4 = 3 686 400
bytes**, 44 % of a 1080p frame, so the pixel path gets cheaper per frame
exactly as the budget drops — 4.17 ms at 240 Hz against 16.67 at 60 — and
three passes at 240 Hz over a 720p frame is **2.65 GB/s**, 74 % of this
box's copy bandwidth: marginally better than 1080p@120's 83 %, and still
the wall. The **retained arms have headroom**, with `balls-nodes` at
5 595 µs/frame and `boing-node` at 10 042 the two to watch — and their
640×480 arms (1 688 and 2 297) are the first evidence that the cost does
scale down with resolution, though 720p is not VGA and 240 Hz is not 60.
The **fullscreen effects will be client-bound**, as they are here: a
2-core Haswell computing a plasma is a plasma-computing problem. And the
**starfield crossover** (§7.8) is the genuinely interesting measurement
at 720p, because it is now bracketed on both sides: **n ≈ 640 at VGA and
n ≈ 5 000 at 1080p**, with 720p's 921 600 pixels three times the first
and four ninths of the second. If the crossing scales with screen area it
lands near n ≈ 2 000 at 720p, which is a prediction this suite can check
in one afternoon — and the number a widget author actually wants.

## 10. Reproducing

```sh
just bench                       # the whole matrix on the box (~20 min, 54 scenario runs plus a bandwidth probe)
just bench "60 120"              # the same, swept over refresh rates (needs #3718)
just bench-report                # tmp/bench/box.jsonl → the markdown above
just bench-bandwidth box         # the box's memcpy rate: copy 3.61 GB/s
```

and, for one scenario at a time, on the box:

```sh
XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench rects --n 1000 --seconds 10 --json
XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench boing --fullscreen --seconds 8 --json
~/nitro-bin/nitro-bench list     # every scenario, with the x11perf op it ports
~/nitro-bin/nitro-bench report ~/tmp/bench/72ef50b.jsonl
```

The ledger is `~/tmp/bench/<sha>.jsonl` on the box, fetched to
`tmp/bench/box.jsonl` here. The run behind this document is **checked in**
as `docs/bench-72ef50b.jsonl` (132 KB), so a reviewer can re-derive every
figure above with

```sh
cargo run -q -p nitro-bench -- report docs/bench-72ef50b.jsonl
```

and get the tables in §6 back byte for byte. That is deliberate: a
document whose evidence lives only in a gitignored `tmp/` is a document
asking to be trusted, and this one would rather be checked. Lines are
append-only and self-describing,
so two runs on two machines concatenate with `cat` and a reader can
always tell which sha and which host a row came from.

The box protocol, which matters because the box is shared. **Announce in
the `nitro-testbox` room before starting**: `just bench` takes the
machine for about twenty minutes and restarts `nitro-dev`. The human's
`~/.config/nitro/server.conf` is **backed up and restored** — a refresh
sweep writes a `mode` line into it and must hand back exactly what it
found — and the deployed binaries are restored byte-for-byte after a run.
**Check which binaries you measured**, because other tasks deploy to this
box too: `docs/latency.md` §6 lost a measurement to a binary replaced
underneath it, and a before/after should also check the *benchmark's own*
binary is identical across the pair, since it is the instrument. Finally,
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
- **Two resolutions only**, 640×480 and 1920×1080. Every pair is measured
  at both (§4), which is what makes §7.8's crossover a bracket rather
  than an extrapolation — but a bracket with two points in it. Anything
  said about how these costs vary *with* resolution is a line through two
  measurements, and 720p (§9) is the obvious third.

  during a run. And **60 Hz only** (§9) — the 120 Hz column is the known
  gap and §5's arithmetic about it is a prediction, not a measurement.
- **One box, one evening, one sha** (`72ef50b` on `ubuntu`, 2026-09-16).
  This box's numbers drift day to day; the within-sitting ratios in §7
  are the durable part, and the absolute microsecond figures should be
  expected to move by a few percent on a re-run.
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
  47 194 µs/frame is a statement about this silicon. That is precisely
  why the crate reports `compute_us` and `upload_us` separately from the
  server's CPU: **the server's share is separable, and it is the only
  share that is about nitro.** Read the server columns as the product's
  numbers and the client columns as the box's.
- **Nothing here is an idle measurement.** "Idle means zero CPU" is goal
  1's other half and is measured in `docs/latency.md` §4.3 — 0 frames and
  0 CPU ticks over five seconds with clients connected. A throughput
  suite by construction never idles.
