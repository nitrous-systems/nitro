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
`docs/bench-329faf3.jsonl`: 44
scenario runs plus one bandwidth measurement, sha `329faf3`, host
`ubuntu`, taken 2026-09-15 in a single sitting at 1920×1080@60.

**Headline: the same bouncing ball costs 16 370 µs of client CPU per
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
**twice**: once as a fullscreen pixel buffer re-uploaded every frame
(`crates/nitro-bench/src/pixels.rs`) and once as scene-graph mutations
(`nodes.rs`), driving the *same* simulation out of `effects.rs`. The ball
is in the same place on the same frame in both arms, so any difference
between them is a property of the path and not of the benchmark.

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
number: **between 1000 and 2000 rect mutations per frame at 60 Hz**.

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
small ones (`text-static`'s 62.6 µs/frame, the node arms' 83.5) should be
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
fullscreen variants next to a VGA-sized one is half the point.

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
| **copy** (`dst[i] = src[i]`, one read + one write per byte) | **3.61** | **434.9** |
| **write** (`dst[i] = v`, no read) | **6.67** | 804.0 |
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
survivable and is why §7.10's fire and rotozoom hold 60 Hz. At 120 Hz it
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
`docs/bench-329faf3.jsonl`, so every number in this document can be
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
a flip gap — the client being the limit — and `ok` otherwise.

<!-- generated by `nitro-bench report`: 44 runs -->

### rects

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rects n=10 | 60.0 | 60.0 | 10.0 | 750.0 | 62.5 | 376.0/409.0 | 163.0 | 119201 | 194 | 0 | ok |
| rects n=100 | 60.0 | 60.0 | 100.0 | 3062.5 | 83.3 | 2251.0/2323.0 | 393.0 | 322861 | 1724 | 0 | ok |
| rects n=500 | 60.0 | 60.0 | 500.0 | 10250.0 | 145.8 | 8918.0/9006.0 | 406.0 | 334841 | 8524 | 0 | ok |
| rects n=1000 | 59.9 | 59.9 | 1000.0 | 11795.4 | 208.8 | 10593.0/11010.0 | 253.0 | 334841 | 17024 | 0 | ok |
| rects n=2000 | 43.6 | 43.7 | 2000.0 | 15616.0 | 171.9 | 10921.0/11165.0 | 193.0 | 334841 | 34024 | 8 | **dropped** |
| rects n=500 | 60.0 | 60.0 | 500.0 | 10000.0 | 111.1 | 8764.0/9120.0 | 395.0 | 336435 | 8524 | 15 | ok |

### rects-move

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rects-move n=10 | 59.9 | 60.0 | 10.0 | 751.6 | 62.6 | 366.0/611.0 | 145.0 | 101007 | 304 | 0 | ok |
| rects-move n=100 | 60.0 | 60.0 | 100.0 | 2937.5 | 83.3 | 2114.0/2223.0 | 456.0 | 325952 | 2824 | 0 | ok |
| rects-move n=500 | 60.0 | 60.0 | 500.0 | 8979.2 | 125.0 | 7662.0/7850.0 | 481.0 | 338002 | 14024 | 0 | ok |
| rects-move n=1000 | 60.0 | 60.0 | 1000.0 | 11458.3 | 208.3 | 10201.0/11288.0 | 375.0 | 338002 | 28024 | 0 | ok |
| rects-move n=2000 | 50.1 | 50.2 | 2000.0 | 13840.4 | 149.6 | 10865.0/11143.0 | 303.0 | 338002 | 56024 | 4 | **dropped** |

### text

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| text n=10 | 60.0 | 60.0 | 10.0 | 2291.7 | 125.0 | 84.0/89.0 | 8.0 | 8160 | 474 | 0 | ok |
| text n=10 size=24 | 60.0 | 60.0 | 10.0 | 2437.5 | 125.0 | 221.0/484.0 | 42.0 | 39168 | 474 | 0 | ok |
| text n=100 | 60.0 | 60.0 | 100.0 | 6875.0 | 229.2 | 670.0/771.0 | 113.0 | 84240 | 4524 | 0 | ok |
| text n=100 size=24 | 59.9 | 60.0 | 100.0 | 8267.2 | 229.6 | 1645.0/1979.0 | 349.0 | 271296 | 4524 | 0 | ok |
| text n=500 | 59.9 | 60.0 | 500.0 | 12087.7 | 313.2 | 1488.0/1721.0 | 228.0 | 293904 | 22524 | 0 | ok |
| text n=500 size=24 | 59.9 | 60.0 | 500.0 | 12066.8 | 271.4 | 2344.0/2975.0 | 196.0 | 271296 | 22524 | 0 | ok |

### text-static

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| text-static n=10 | 60.0 | 60.0 | 10.0 | 333.3 | 62.5 | 71.0/90.0 | 7.0 | 8398 | 304 | 0 | ok |
| text-static n=100 | 59.9 | 60.0 | 100.0 | 1210.9 | 62.6 | 679.0/693.0 | 102.0 | 86130 | 2824 | 0 | ok |
| text-static n=500 | 59.9 | 60.0 | 500.0 | 4592.9 | 146.1 | 3044.0/3363.0 | 367.0 | 300498 | 14024 | 0 | ok |

### putimage

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| putimage size=100 | 59.9 | 60.0 | 1.0 | 375.8 | 668.1 | 80.0/166.0 | 10.0 | 10000 | 56 | 0 | ok |
| putimage size=250 | 59.9 | 60.0 | 1.0 | 793.3 | 3883.1 | 298.0/332.0 | 87.0 | 62500 | 56 | 0 | ok |
| putimage size=500 | 59.9 | 60.0 | 1.0 | 1565.8 | 9352.8 | 763.0/1788.0 | 177.0 | 250005 | 56 | 0 | ok |
| putimage size=1080 | 30.0 | 30.0 | 1.0 | 5875.0 | 27416.7 | 1198.0/4255.0 | 892.0 | 764640 | 56 | 0 | **slow** |

### scroll

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| scroll n=500 | 59.9 | 60.0 | 1.0 | 3507.3 | 62.6 | 2131.0/2261.0 | 899.0 | 675696 | 52 | 0 | ok |

### create

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| create n=50 | 60.1 | 60.1 | 153.0 | 686.1 | 62.4 | 240.0/266.0 | 29.0 | 29281 | 3385 | 0 | ok |

### plasma

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| plasma size=640 | 60.0 | 60.0 | 1.0 | 1708.3 | 9520.8 | 808.0/929.0 | 206.0 | 307200 | 56 | 0 | ok |
| plasma size=1920 | 15.0 | 15.0 | 1.0 | 14333.3 | 50083.3 | 3518.0/7109.0 | 2094.0 | 2073600 | 56 | 16661 | **dropped** |

### fire

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| fire size=640 | 59.8 | 59.9 | 1.0 | 2776.6 | 9185.8 | 1437.0/1557.0 | 429.0 | 307200 | 56 | 0 | ok |
| fire size=1920 | 59.9 | 60.0 | 1.0 | 14196.2 | 7139.9 | 9186.0/9251.0 | 2142.0 | 2073600 | 56 | 0 | ok |

### rotozoom

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| rotozoom size=640 | 60.1 | 60.1 | 1.0 | 2848.2 | 4449.1 | 1441.0/1529.0 | 449.0 | 307200 | 56 | 0 | ok |
| rotozoom size=1920 | 59.7 | 59.9 | 1.0 | 16631.8 | 12845.2 | 10796.0/11044.0 | 2998.0 | 2073600 | 56 | 0 | ok |

### boing

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing size=640 | 59.9 | 60.0 | 1.0 | 3110.6 | 7056.4 | 1728.0/3414.0 | 389.0 | 307205 | 56 | 0 | ok |
| boing size=1920 | 33.7 | 33.9 | 1.0 | 19185.2 | 16370.4 | 12594.0/12975.0 | 3685.0 | 2073600 | 56 | 0 | **slow** |
| boing size=1920 | 32.3 | 32.5 | 1.0 | 19536.7 | 16602.3 | 11378.0/13096.0 | 3489.0 | 2073600 | 56 | 0 | **slow** |

### boing-node

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| boing-node size=389 | 59.9 | 60.0 | 1.0 | 10083.5 | 83.5 | 9661.0/9817.0 | 213.0 | 170014 | 52 | 0 | ok |

### starfield

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield n=100 size=1920 | 59.9 | 60.0 | 1.0 | 16492.7 | 9749.5 | 11529.0/11603.0 | 2137.0 | 2073600 | 56 | 0 | ok |
| starfield n=500 size=1920 | 60.0 | 60.1 | 1.0 | 16500.0 | 9791.7 | 11562.0/11641.0 | 2140.0 | 2073600 | 56 | 0 | ok |
| starfield n=2000 size=1920 | 59.5 | 59.6 | 1.0 | 16533.6 | 9852.9 | 11608.0/11757.0 | 2140.0 | 2073600 | 56 | 0 | ok |

### starfield-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| starfield-nodes n=100 | 59.9 | 60.0 | 100.0 | 7202.5 | 104.4 | 4313.0/4766.0 | 2439.0 | 1995560 | 2824 | 0 | ok |
| starfield-nodes n=500 | 59.9 | 60.0 | 500.0 | 8747.4 | 146.1 | 5152.0/5433.0 | 2505.0 | 2073600 | 14024 | 0 | ok |
| starfield-nodes n=2000 | 59.9 | 60.0 | 2000.0 | 11085.6 | 334.0 | 6369.0/6883.0 | 2307.0 | 2073600 | 56024 | 0 | ok |

### balls

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls n=32 size=1920 | 59.9 | 60.0 | 1.0 | 16492.7 | 9770.4 | 11547.0/11621.0 | 2136.0 | 2073600 | 56 | 0 | ok |

### balls-nodes

| run | presented/s | commits/s | mutations/frame | server µs/frame | client µs/frame | paint µs mean/max | copy µs mean | damage px | bytes/frame | flip rise µs | verdict |
|---|---|---|---|---|---|---|---|---|---|---|---|
| balls-nodes n=32 | 59.9 | 60.0 | 32.0 | 5595.0 | 83.5 | 3166.0/3760.0 | 611.0 | 508737 | 920 | 0 | ok |

The second `rects n=500` row is the **control**: the same scenario with
the shell clients killed, so a reader can see what the bar and the
launcher cost every other row. It is 10 000 µs/frame against 10 250 with
the shell up — **2.4 % of the server's per-frame CPU**, small enough that
no verdict here turns on it. Read it with the caveat the ledger's own
note records: `nitro-session` supervises its children and restarts them
within about a second, so the control arm is three seconds long and the
shell was back up for most of it. It is an upper bound on the shell's
cost, probably well above the truth; the way to tighten it would be to
stop the session rather than kill the processes.

## 7. Verdict per scenario

### 7.1 `rects` — the x11perf number, transposed

`x11perf -rect100`, as N nodes that recolour every frame — all of them,
because a node that does not change costs the server nothing, which is
the property under test. The sweep scales cleanly to **n=1000 at 59.9
presented/s, 11 795 µs/frame of server CPU**; at n=2000 it breaks, at
**43.6 presented/s** and verdict `**dropped**`, the run's own worst flip
interval rising 8 µs past a previous all-time maximum of 33 336 µs.

**So this box sustains between 1000 and 2000 rect mutations per frame at
60 Hz.** That is the closest thing in this document to a classical
x11perf result, and the sentence §2 promised: not ops/s, but how much
retained mutation fits in a frame.

The cost is nearly linear in N up to the cliff and then is not: 3 062
µs/frame at n=100, 10 250 at n=500, 11 795 at n=1000 — only **15 % more
for twice the nodes** between those last two, because by n=500 the grid
already covers 334 841 damage pixels and stops growing, after which the
marginal cost is per-node bookkeeping against a fixed rasterised area. At
n=2000 `paint_us_mean` is 10 921 µs against n=1000's 10 593 — the paint
barely moved — and the extra ~3 800 µs/frame of server CPU is protocol
and scene-update work on 34 024 bytes per frame of incoming mutations.
The client is never the limit: **208.8 µs/frame at n=1000**, some fifty
times below the server. Sending a thousand mutations is cheap; painting
them is not.

### 7.2 `rects-move` vs `rects` — damage union is not the bottleneck

The pair exists because recolouring dirties each node's own bounds while
*moving* dirties the union of the old and the new, and a benchmark that
did only one of them would have missed the more expensive half. It turns
out not to be the more expensive half:

| n | rects (recolour) µs/frame | rects-move µs/frame | rects damage px | rects-move damage px |
|---|---|---|---|---|
| 100 | 3 062.5 | 2 937.5 | 322 861 | 325 952 |
| 500 | 10 250.0 | **8 979.2** | 334 841 | 338 002 |
| 1000 | 11 795.4 | 11 458.3 | 334 841 | 338 002 |
| 2000 | 15 616.0 | **13 840.4** | 334 841 | 338 002 |

At every sweep point the moving arm costs the server *less*, and at
n=2000, where both fall off the refresh, it degrades less as well:
**50.1 presented/s against 43.6**. Both are marked `**dropped**`, so
neither held 60 Hz; the difference is how far they fell.

The honest reading is that **damage union is not the bottleneck;
rasterization is.** The moving arm dirties about 1 % more pixels (338 002
against 334 841) and pays for it, but it also sends a `SetBounds` where
the other sends a `SetFill`, and a recolour forces every rect to be
re-rasterised with a new source colour where a move leaves more of the
raster work coherent. Whatever the precise mechanism, the direction is
consistent across four sweep points and is the opposite of what the
scenario was written expecting — worth stating plainly, because "moving
things is expensive, recolouring is cheap" is a folk belief this table
does not support.

### 7.3 `text` vs `text-static` — the retained-text result

The one x11perf could not have run. X11 has no retained text: a moved
string is a redraw, so it costs exactly what a new one costs. Here the
string is unchanged, so the server re-uses its layout and its glyph tiles
and does nothing but composite.

| n | `text` server µs/frame | `text-static` server µs/frame | ratio | `text` layouts shaped | `text-static` layouts shaped |
|---|---|---|---|---|---|
| 10 | 2 291.7 | 333.3 | 6.9× | 4 800 | 1 |
| 100 | **6 875.0** | **1 210.9** | **5.7×** | 48 000 | **0** |
| 500 | 12 087.7 | 4 592.9 | 2.6× | 240 000 | **0** |

The ratios are `text` ÷ `text-static` at the same n, and the layout
counts are the server's own `text_layouts` counter differenced across the
run: `text` at n=100 shapes **100 layouts per frame, 48 000 over 480
commits**, and `text-static` at the same n shapes **zero**. Not "few" —
zero, over eight seconds and 479 presented frames.

`glyph_renders` tells the same story and is the stronger claim, because
it is the expensive half: rasterising a glyph into the atlas. It does not
move at all in the n=100 and n=500 static arms (123 → 123 both times). In
the n=10 arm it rises by 4 — and so does the `rects n=10` arm's, by 5, in
a scenario that draws no text whatsoever. That is the **bar's clock**
redrawing behind the benchmark window, which is what "the real desktop
was up" costs a counter, and why the larger sweep points are the ones to
quote.

**A moved label re-uses its layout and its glyph tiles.** The
microseconds are corroboration; the two pinned counters are the result.
`crates/nitro-bench/tests/against_server.rs` asserts the `text_layouts`
half against a real server on the fake backend, so a regression is caught
in CI and not only on the box.

The 24-pixel rows are the `-f24text` sweep and say something smaller: at
n=100, doubling the font size costs **8 267 against 6 875 µs/frame**, a
20 % increase for 3.2× the damaged area (271 296 against 84 240 px).
Shaping dominates rasterisation at this size.

### 7.4 `putimage` — where the client becomes the limit

`x11perf -putimage100/-putimage500`: a client buffer rewritten and
re-uploaded every frame. Damage scales as the square of the edge, which
is the check that the scenario is measuring what it claims:

| size | damage px | expected (size²) | server µs/frame | client µs/frame | presented/s |
|---|---|---|---|---|---|
| 100 | 10 000 | 10 000 | 375.8 | 668.1 | 59.9 |
| 250 | 62 500 | 62 500 | 793.3 | 3 883.1 | 59.9 |
| 500 | 250 005 | 250 000 | 1 565.8 | 9 352.8 | 59.9 |
| 1080 | 764 640 | — | 5 875.0 | **27 416.7** | **30.0** |

The three small points land on size² to within five pixels (`boing` at
640 shows the same consistent +5, an artefact of the counter and not of
the scenario). The 1080 row does not, and the reason is worth recording:
the buffer is 1080×1080 in a 640×480 window, so it overflows and is
**clipped by the output's bottom edge** — 764 640 is exactly 1080 × 708.
Damage is what is on the screen, not what the client wrote.

At 1080 the run presents **30.0/s and is client-bound**: the client's own
effect plus upload is **27 417 µs/frame against the server's 5 875**, and
the verdict says `**slow**` rather than `**dropped**` — the server
flipped on time every time, there was simply nothing new to flip on half
of them. The split the crate measures makes the blame precise: of the
client's 27 478 µs per commit, **26 091 is the effect and 1 386 is the
`pwrite`**. The upload is 5 % of the client's cost, so making the wire
faster would buy this scenario almost nothing.

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
has to be a repaint. So the scenario is a clipping group holding a column
of 500 rects taller than the window, whose offset moves one 16-pixel row
per frame. The result is **one mutation per frame, 52 bytes on the
wire**, whatever the content height — a thousand rows would send the same
52 bytes — for **3 507 µs/frame of server CPU**, 675 696 damage pixels
and 59.9 presented/s.

675 696 pixels for a scroll whose newly exposed row is 640×16 — 10 240
pixels — is a factor of **66**. State it plainly as the finding it is:
**this server repaints the viewport rather than blitting it, so the cost
of a scroll is the size of the viewport and not the size of the exposed
line.** It is in fact 2.2 viewports (675 696 ÷ 307 200), because
`damage_px` is the age-2 union `damage(n) ∪ damage(n-1)` the back buffer
must be brought up to date over, and a scrolling viewport dirties a
different region each frame; the remaining 0.2 is the shell redrawing
behind the window. The factor to quote is the one against the exposed
line, and it is large either way.

That is a real difference from X11's model and not obviously the wrong
choice: a blit-based scroll needs the server to prove nothing else
changed in the region, and at 3.5 ms a viewport repaint fits inside a
16.7 ms budget five times over. But it is the number to point at if
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
a frame budget: menus, tooltips and dialogs are cheap. And the invariant
the scenario really exists for: **the server's `nodes` count returns to
baseline.** Every record carries `stats_before` and `stats_after` so this
is checkable on the box, and
`creating_and_destroying_nodes_leaves_the_count_where_it_was` in
`crates/nitro-bench/tests/against_server.rs` pins it against a real
server in CI. One leaked node per opened menu is invisible for an hour
and fatal for a session.

### 7.7 `boing` vs `boing-node` — the headline

Fullscreen 1080p, the same simulation, the same ball in the same place on
the same frame. One arm recomputes the sphere into an 8 294 400-byte
buffer every frame; the other uploads the sprite **once** and then sends
nothing but a new rectangle.

| | `boing` (pixels) | `boing-node` (retained) |
|---|---|---|
| presented/s | **33.7** | **59.9** |
| client µs/frame | **16 370.4** | **83.5** |
| — of which effect | 10 844.0 | — |
| — of which upload (`pwrite`) | 5 426.5 | — |
| server µs/frame | 19 185.2 | 10 083.5 |
| damage px | 2 073 600 | 170 014 |
| bytes/frame | 56 | **52** |
| verdict | `**slow**` | `ok` |

**196× less client CPU** (16 370.4 ÷ 83.5), **12.2× less damage**
(2 073 600 ÷ 170 014), **1.9× less server CPU** — and, the part that is
not a ratio, the retained arm **holds 60 Hz where the pixel arm cannot**.
The pixel arm was run twice, back to back, and reproduced itself: 33.7
and 32.3 presented/s. This is `DESIGN.md` goal 1, measured. The ball's
389×389 bounding box is **7.3 % of the screen** and the retained arm
damages **8.2 %** of it — the extra being the trailing edge it left
behind. The pixel arm damages 100 %, every frame, for the same ball.

Two honest qualifications. The retained arm's server cost is only
**1.9×** better, not 196×: the server still rasterises and blends a
389×389 alpha sprite into the scene twice per frame (old position and
new), and at 9 661 µs of mean paint that is not nothing. The dramatic
factor is the *client's*, which is the half a battery notices. And the
bytes/frame columns are 56 and 52, which look identical — because the
pixel arm's 8 MB never crosses the socket either; it goes through a
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
the `size=389` in the table cannot lie about which one ran. The lesson is
the one this project keeps relearning, and the `nitro-testbox` room has
the best phrasing of it: **the instrument agreed with the code because it
was measuring the layer below the broken one.**

### 7.8 `starfield` vs `starfield-nodes` — the arm that might have lost

This pair is in the suite because it is the case where the retained arm
could plausibly lose. Two thousand stars is two thousand `SetBounds` per
frame and two thousand damage rectangles for the server to union; the
buffer arm is one memcpy of the whole screen. Reporting only the case
that flatters the design would be advocacy.

It does not lose:

| n | pixels server µs/frame | nodes server µs/frame | ratio | pixels client | nodes client | nodes bytes/frame |
|---|---|---|---|---|---|---|
| 100 | 16 492.7 | **7 202.5** | 2.29× | 9 749.5 | 104.4 | 2 824 |
| 500 | 16 500.0 | **8 747.4** | 1.89× | 9 791.7 | 146.1 | 14 024 |
| 2000 | 16 533.6 | **11 085.6** | 1.49× | 9 852.9 | 334.0 | 56 024 |

The node arm wins at every N measured, and on the client side it is not
close: **334 µs/frame against 9 853 at n=2000**, a factor of 29. Both
arms hold ~60 Hz throughout, so this is entirely a CPU-per-frame story,
which is what §2 said the headline column was for. Now the honest part,
which is three separate qualifications.

**The buffer arm's cost is flat and the node arm's is not.** 16 493 →
16 534 µs across a twenty-fold increase in N, against 7 203 → 11 086. The
buffer arm pays for the screen and does not care what is on it; the node
arm pays for the stars. The curves are converging.

**Where they cross is beyond what was measured.** A least-squares line
through the three node points gives 1.91 µs per star plus 7 356 µs of
fixed cost, meeting the buffer arm's ~16 500 µs at about **n ≈ 4 800**;
the outer two points alone (500 and 2000) give 1.56 µs/star and a
crossing near **n ≈ 5 500**. Both are extrapolations more than twice past
the last measurement, and the curve is visibly not a line — the step from
100 to 500 costs 3.9 µs/star and the step from 500 to 2000 costs 1.6 —
so the right statement is "somewhere in the thousands, and nobody has
measured it". If that number matters to a design decision, run
`starfield-nodes --n 4000` and `--n 8000` and put the answer here.

**Damage is not where the win comes from, at least not at these N.** The
node arm's `damage_px_mean` is 1 995 560 at n=100 and the full 2 073 600
at n=500 and above — essentially the whole screen either way, because a
thousand scattered stars union to the screen almost immediately. So the
node arm is not winning by damaging less; it is winning because **no 8 MB
buffer is written by the client, `pread` back by the server, and
composited**. That matters for predicting a different workload: a
retained arm whose motion is *localised* (the boing ball) wins on damage
as well, and wins much bigger.

The wire cost is the node arm's one real disadvantage and belongs in the
same breath: **56 024 bytes per frame at n=2000 against the buffer arm's
56**. That is 3.4 MB/s of socket traffic, nothing locally, and a fact
worth remembering for `docs/remote.md`'s TCP path, where two thousand
animated nodes would be 27 Mb/s on the wire.

### 7.9 `balls` vs `balls-nodes` — antialiased circles, cheaply

Thirty-two bouncing circles, fullscreen. A rounded rect with
`corners = d/2` **is** an antialiased circle, so the node arm is asking
the server's rounded-rect path for the most expensive per-pixel work in
the whole rect family, thirty-two times a frame.

| | `balls` (pixels) | `balls-nodes` (retained) | ratio |
|---|---|---|---|
| server µs/frame | 16 492.7 | **5 595.0** | **2.95×** |
| client µs/frame | 9 770.4 | **83.5** | 117× |
| damage px | 2 073 600 | **508 737** | 4.08× |
| bytes/frame | 56 | 920 | 0.06× |
| paint µs mean | 11 547 | 3 166 | 3.65× |

**The server draws 32 antialiased circles, and their motion damage, for
5 595 µs — a third of what it costs to take one memcpy of the screen
through the pixel path.** Read the damage figure alongside it: 508 737
pixels, **24.5 % of the screen**, which is what "work proportional to
what changed" looks like when the change is genuinely localised — compare
the starfield, where the same claim was true of CPU and not of damage.
920 bytes per frame (32 `SetBounds` and the commit) is the entire wire
cost of the animation.

### 7.10 `plasma`, `fire`, `rotozoom` — client-bound at 1080p

The three full-surface rewrites: every pixel recomputed every frame.
These are the workloads a compositor built around "work proportional to
change" is worst at, which is precisely why they are in the suite.

| | 640×480 presented/s | 1920×1080 presented/s | 1080p client µs/frame | — of which effect | 1080p server µs/frame |
|---|---|---|---|---|---|
| plasma | 60.0 | **15.0** | **50 083.3** | **47 430.1** | 14 333.3 |
| fire | 59.8 | **59.9** | 7 139.9 | 2 933.0 | 14 196.2 |
| rotozoom | 60.1 | **59.7** | 12 845.2 | 7 284.4 | 16 631.8 |

At 640×480 all three hold 60 Hz comfortably and the server's share is
1 708 / 2 777 / 2 848 µs per frame: the period-correct resolution is a
solved problem on hardware two decades younger than the effects.

At 1080p, **plasma presents 15.0/s and it is not the compositor's
fault**: the client burns 50 083 µs per frame of which **47 430 is the
sine loop**, while the server's 14 333 µs would have fitted in the budget
with 2.3 ms to spare. Its verdict is `**dropped**` rather than `**slow**`
because at 15 fps the server's own worst flip interval rose to 50 009 µs
— three refresh periods — which is what a client presenting at a quarter
rate does to the flip cadence.

Fire and rotozoom hold ~60 Hz at 1080p, with a margin the §5 arithmetic
predicts exactly. Rotozoom's server cost is **16 632 µs against a 16 667
µs budget** — 99.8 % of the frame — and it is still marked `ok` because
the server never actually missed a flip (rise 0) and presented 59.7/s.
That row is the best single illustration of why this document exists:
fps says "fine", and CPU per frame says "one more window on that screen
and it is not".

The honest reading of all three: **at 1080p the fullscreen pixel path is
limited by the client's own effect on this CPU, not by nitro** — and that
is itself the argument for the retained path. The fire row makes it
sharply: its effect is only 2 933 µs/frame (a cellular automaton is
cheap) but its `pwrite` is 4 095 and the server's paint is 9 186, so fire
is the one effect here dominated by *moving* the frame rather than by
computing it. That is the §5 bandwidth wall visible in a single row, and
it is the row that will break first at 120 Hz.

## 8. The `flip rise` column, and a lesson about instruments

`flip_interval_max_us` is the server's **cumulative, all-time maximum**
flip interval. It never decays, it is not windowed, and it is not reset
between clients, so its *value* says nothing about any particular run.

The first version of the verdict logic read `stats_after` and compared it
to 1.5 frame budgets. It marked **25 of 45 runs as having dropped
frames** while they presented a flat 59.9/s with a mean flip interval of
17 ms. It was not measuring those runs at all: it was measuring which
scenario had run *earlier* in the same server process and left a 33 ms
spike behind. In this final ledger the effect is starker still — 43 of
the 44 rows carry an all-time maximum above the 1.5-budget threshold,
inherited from a handful of genuinely bad frames, while only **four**
rows have a non-zero rise at all (`rects n=2000` +8 µs, `rects-move
n=2000` +4, `plasma size=1920` +16 661, and the control `rects n=500`
+15 after a server restart reset the counter).

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

Both are the same lesson, and it is the one this project keeps
relearning — from `docs/latency.md`'s saturated-display trap, from the
`boing-node` resampling bug in §7.7, and from #3711:

> **The instrument agreed with the code because it was measuring the
> layer below the broken one.**

An eliminated loop does not report an error. It reports a great result.

## 8a. The three worst numbers, filed

A benchmark that ends in a document is half a benchmark. The three
findings below are the worst numbers in the matrix and each is an open
issue, so that a future run has something to close rather than a
paragraph to re-read.

| issue | the number | what it is |
|---|---|---|
| **#568** | `paint_us_mean` **11 529 µs** for a fullscreen 1080p repaint — **69 %** of the 60 Hz frame, and more than a whole 120 Hz frame | Four fullscreen scenarios with wildly different client costs report near-identical server costs, because the server's work is a function of damaged area alone. Extrapolating `putimage`'s damage/paint curve to 2 073 600 px predicts ~3 250 µs against the 9 000–11 500 measured, so **a fullscreen repaint is ~3× more expensive per pixel than a large partial one** — there is a fixed per-frame cost that is not the linear part, and that gap is what to profile. |
| **#569** | `upload_us` **4 095–5 800 µs/frame** at 1080p — for `fire` and `balls`, **more than the effect itself** | The `pwrite`/`pread` pair the buffer path pays because `mmap` is `unsafe` in this tree and a client can shrink a memfd into a SIGBUS under the server's mapping. `F_SEAL_SHRINK` is the fix that exists and is not taken; the price of not taking it is now measured at 33–60 % of the client's whole frame. Note the scope: the retained path never pays it (§7.7's 83.5 µs), so this is "make the escape hatch cheaper", not "the architecture is wrong". |
| **#570** | a one-row scroll damages **675 696 px = 2.2× the viewport** | The wire side is excellent — one mutation, 52 bytes, independent of content height. The server side repaints the viewport where `CopyArea` moved a line. Damaging the *symmetric difference* of a pure translation rather than the union would take this to ~20 000 px without touching the rasterizer, and `nitro-bench scroll` re-run is the proof it would land. It compounds with #568 for a fullscreen `nitro-term`. |

Two of the three are about the same underlying thing — **the server's cost
is proportional to damaged area, and the damage it computes is larger
than the area that actually changed.** That is not a contradiction of
`DESIGN.md` goal 1 so much as a statement of where the goal is currently
achieved at the wrong granularity: proportional to what the *scene graph*
thinks changed, rather than to what the pixels did.

## 9. Refresh rate: 60 Hz only, and why

Every row above was taken at **1920×1080@60**, and `refresh_mhz` is
60 000 in all 44 records. The panel also offers 1080p at **120, 85, 50
and 24 Hz**, and the 120 Hz column is the obvious follow-up — it is where
§5's arithmetic says the fullscreen pixel path stops keeping up, and
where §7.10's rotozoom row (99.8 % of a 60 Hz budget) becomes 200 % of a
120 Hz one.

It was not measured because **nitro had no mode-selection key when this
ran**. Task **#3718** adds `output.<connector>.mode = WxH@Hz`;
`deploy/bench.sh --modes "60 120"` already writes that line into the
human's `server.conf`, restarts the unit, and runs the whole matrix again
per mode. Two rules in that script are non-negotiable and both are scar
tissue. The human's `server.conf` is **backed up and restored**, never
rewritten from scratch — it carries his colour scheme and his keyboard
layout, and those are his. And after each restart the script **refuses
the arm unless the server's `outputs` reply reports `@120000`**: a mode
line that matches nothing is a *warning plus the default*, so the server
comes back happily at 60 Hz and an arm that silently fell back would
produce a full, plausible, internally consistent 120 Hz column that was
actually a second 60 Hz column — and "120 Hz bought nothing" is exactly
what that looks like. `nitro-bench report` has a refresh-pivot table
waiting for the data; it prints nothing today because there is nothing to
pivot.

**1080p@240 is not reachable on this box, and that is arithmetic rather
than pessimism.** HDMI 1.4 on Haswell caps the TMDS clock near 300 MHz.
1080p@120 is 285.5 MHz — the last mode the EDID offers, fitting with
about 5 % to spare. 1080p@144 needs 346.5 MHz, 1080p@165 needs 401.0 and
1080p@240 needs roughly 606 MHz: none of them fit, and no amount of
software gets them. **1280×720@240** with CVT-RB timing is about 280 MHz
and might, which is what #3718 is testing.

If 720p@240 lands, the predictions worth writing down in advance so they
can be wrong in public. The frame is **1 280 × 720 × 4 = 3 686 400
bytes**, 44 % of a 1080p frame, so the pixel path gets cheaper per frame
exactly as the budget drops — 4.17 ms at 240 Hz against 16.67 at 60 — and
three passes at 240 Hz over a 720p frame is **2.65 GB/s**, 74 % of this
box's copy bandwidth: marginally better than 1080p@120's 83 %, and still
the wall. The **retained arms have headroom**, with `balls-nodes` at
5 595 µs/frame and `boing-node` at 10 084 the two to watch — the first
fits a 4.17 ms budget only if its cost scales down with resolution
roughly as its damage does, which is a measurement and not an assumption.
The **fullscreen effects will be client-bound**, as they are here: a
2-core Haswell computing a plasma is a plasma-computing problem. And the
**starfield crossover** (§7.8) is the genuinely interesting measurement
at 720p, because the buffer arm's flat cost drops by 56 % while the node
arm's per-star cost barely moves. The crossing N should fall a long way,
and where it lands is the number a widget author actually wants.

## 10. Reproducing

```sh
just bench                       # the whole matrix on the box (~15 min, 44 scenario runs plus a bandwidth probe)
just bench "60 120"              # the same, swept over refresh rates (needs #3718)
just bench-report                # tmp/bench/box.jsonl → the markdown above
just bench-bandwidth box         # the box's memcpy rate: copy 3.61 GB/s
```

and, for one scenario at a time, on the box:

```sh
XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench rects --n 1000 --seconds 10 --json
XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-bench boing --fullscreen --seconds 8 --json
~/nitro-bin/nitro-bench list     # every scenario, with the x11perf op it ports
~/nitro-bin/nitro-bench report ~/tmp/bench/329faf3.jsonl
```

The ledger is `~/tmp/bench/<sha>.jsonl` on the box, fetched to
`tmp/bench/box.jsonl` here. The run behind this document is **checked in**
as `docs/bench-329faf3.jsonl` (108 KB), so a reviewer can re-derive every
figure above with

```sh
cargo run -q -p nitro-bench -- report docs/bench-329faf3.jsonl
```

and get the tables in §6 back byte for byte. That is deliberate: a
document whose evidence lives only in a gitignored `tmp/` is a document
asking to be trusted, and this one would rather be checked. Lines are
append-only and self-describing,
so two runs on two machines concatenate with `cat` and a reader can
always tell which sha and which host a row came from.

The box protocol, which matters because the box is shared. **Announce in
the `nitro-testbox` room before starting**: `just bench` takes the
machine for about fifteen minutes and restarts `nitro-dev`. The human's
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
back, and every ratio in §7 is within-sitting for that reason. Two runs a
day apart are not a comparison.

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
  during a run. And **60 Hz only** (§9) — the 120 Hz column is the known
  gap and §5's arithmetic about it is a prediction, not a measurement.
- **One box, one evening, one sha** (`329faf3` on `ubuntu`, 2026-09-15).
  This box's numbers drift day to day; the within-sitting ratios in §7
  are the durable part, and the absolute microsecond figures should be
  expected to move by a few percent on a re-run.
- **CPU is quantised to 10 ms ticks** (§2): the sub-100 µs client figures
  mean "under a hundred microseconds", not three significant figures.
  And **`damage_px_mean` includes the age-2 union**, so it is roughly
  twice the newly-dirtied area for a steady animation — comparable
  *between* rows, which is how it is used, and not against a naive
  "pixels the client changed" count.
- **Every effect's own compute cost is a property of this CPU.** A
  Pentium G3240 has two Haswell cores, SSE4.2 and **no AVX2**, and every
  fullscreen effect in §7.10 is a tight scalar loop over 2 073 600 pixels
  that a machine with AVX2 would run two to four times faster. Plasma's
  47 430 µs/frame is a statement about this silicon. That is precisely
  why the crate reports `compute_us` and `upload_us` separately from the
  server's CPU: **the server's share is separable, and it is the only
  share that is about nitro.** Read the server columns as the product's
  numbers and the client columns as the box's.
- **Nothing here is an idle measurement.** "Idle means zero CPU" is goal
  1's other half and is measured in `docs/latency.md` §4.3 — 0 frames and
  0 CPU ticks over five seconds with clients connected. A throughput
  suite by construction never idles.
