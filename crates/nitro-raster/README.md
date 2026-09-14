# `nitro-raster`

The CPU 2-D rasterizer the nitro server paints damaged rects with.

One type does the work: `Canvas`, a mutable borrow of an XRGB8888 back
buffer. Every call takes a `clip: &IRect` — the damage rect — and never
writes a byte outside it. No allocation, no `unsafe`, one dependency
(`nitro-core`, itself dependency-free).

```rust
use nitro_core::{Color, IRect, Rect};
use nitro_raster::{Canvas, Fill};

let mut buf = vec![0u8; 1920 * 1080 * 4];
let mut canvas = Canvas::new(&mut buf, 1920, 1080, 1920 * 4);
let damage = IRect::new(40, 30, 200, 150);

canvas.fill_rect(
    &damage,
    &Rect::new(64.0, 48.0, 600.0, 400.0),
    &Fill::Solid(Color::rgb(0x2C, 0x30, 0x36)),
    8.0,   // corner radius
    1.0,   // opacity
);
```

## API contract

### Coordinate space

Everything is **device pixels**. Pixel `(x, y)` covers the square
`[x, x+1) × [y, y+1)`; its centre is `(x+0.5, y+0.5)`. The scene applies
its transforms *before* calling: in M1 transforms are axis-aligned
(translate + scale), so shapes arrive as plain `Rect`s in device space.

### Clip

`clip` is intersected with the surface bounds, and nothing outside the
result is ever written — not one byte, not for a shape that extends past
it, not for an anti-aliased edge. The server passes each damage rect as the
clip and relies on this. The test suite fills the canvas with a sentinel
colour and asserts every pixel outside the clip still holds it, for fills,
strokes and blits.

### Blending

Source-over with straight-alpha inputs, converted to premultiplied at the
pixel:

```text
out = round((src * a + dst * (255 - a)) / 255)
a   = round(src.a * coverage * opacity / 255²)
```

The division by 255 is the exact `(t + (t >> 8)) >> 8` trick with
`t = x + 128`, which equals `round(x / 255)` for every `x <= 65_535`; our
numerators are convex combinations scaled by 255, so they never exceed
`255 * 255 = 65_025`. A `div255_is_exact_rounding` test checks the whole
range exhaustively, and `solid_blending_matches_float_reference` /
`opacity_blending_matches_float_reference` check the composite against a
float reference on 3000 random colour/alpha combinations — agreement is
within ±1 everywhere.

### Anti-aliasing

**Analytic coverage**: a pixel's coverage is the area of the pixel square
inside the shape.

- **Straight edges are exact.** A row of a shape is reduced to a horizontal
  interval `[l, r]` with a vertical weight, and the coverage of pixel `px`
  is `w * clamp(min(r, px+1) - max(l, px), 0, 1)`. A rect at `x = 0.5` gives
  exactly 50 % on the edge column; at `x = 0.25`, 75 %.
- **Corner arcs** integrate x exactly over 4 or 8 sub-scanlines of y (8
  above radius 10, where an arc sweeps more x per row; 4 below, which is
  half the work at the same accuracy class). The error is therefore confined
  to the `r × r` corner boxes, and measured against a 256×-supersampled
  reference it stays under 0.05 (13/255) for every radius — see
  `corner_coverage_is_close_to_a_supersampled_reference`. A quarter disc's
  summed coverage lands within 0.2 px² of `πr²/4`.
- Corners are **exactly symmetric**: the four corners of a rounded rect are
  pixel-identical under mirroring, asserted in
  `rounded_corners_are_symmetric_in_pixels`.

`stroke_rect_inside` paints the difference of two analytic coverages (outer
rrect minus inset inner rrect), so the band is anti-aliased on *both* sides
and never double-blends where the two edges meet — a translucent border is
exactly one blend per pixel, asserted in
`stroke_does_not_double_blend_translucently`. Most rows of most borders take
a fast path that skips the coverage walk entirely; see
[the thin-border fast path](#the-thin-border-fast-path).

### Drawing operations

| call | what it paints |
|---|---|
| `fill_irect` | an integer rect in one colour; opaque colours are stored, not blended |
| `fill_rect` | a rounded rect, anti-aliased, `Fill::Solid` or `Fill::Linear` |
| `stroke_rect_inside` | a rounded-rect border lying entirely inside the rect |
| `blit` | an `Image` (XRGB8888 or straight-alpha ARGB8888), 1:1 or bilinear-scaled |
| `blit_mask` | an A8 coverage `Mask` tinted with one colour, source-over |
| `blit_masks` | a batch of masks sharing a colour and an opacity |
| `blend_pixel_at` | one pixel; for tests and debug markers |

### Glyph masks

Text is drawn as **A8 coverage masks tinted with one colour** — shaping, glyph
rasterization and the atlas live outside this crate, which keeps the font
dependency out of it.

```rust
use nitro_core::{Color, IRect};
use nitro_raster::{Canvas, Mask};

let mut buf = vec![0u8; 256 * 64 * 4];
let mut canvas = Canvas::new(&mut buf, 256, 64, 256 * 4);
let damage = IRect::new(0, 0, 256, 64);

// A glyph somewhere inside a bigger atlas page: `stride` is the page's
// pitch, so the sub-rectangle is blitted without a copy.
let page = vec![0u8; 512 * 128];
let glyph = Mask {
    data: &page[(7 * 512 + 3)..], // top-left of the glyph in the page
    w: 8,
    h: 12,
    stride: 512,
};

canvas.blit_mask(&damage, 40, 20, &glyph, Color::rgb(0xE0, 0xE4, 0xEC), 1.0);

// A whole run of glyphs sharing a colour:
let run = [(40, 20, glyph), (49, 20, glyph), (58, 20, glyph)];
canvas.blit_masks(&damage, Color::rgb(0xE0, 0xE4, 0xEC), 1.0, &run);
```

`(x, y)` is the device pixel the mask's top-left corner lands on — the caller
has already applied the glyph's placement offsets. There is **no scaling and
no filtering**: a glyph mask is rasterized at its final size by the atlas.
Coverage combines with the tint's alpha and the opacity through the same
`a = round(color.a * coverage * opacity / 255²)` as every other call, so the
blend is the crate's one blend; `mask_blend_matches_float_reference` checks
it against the float reference on random coverage/colour/opacity.

An empty or invalid mask, a transparent colour, `opacity <= 0` and an empty
clip are all no-ops. Coverage 0 skips the pixel; with an opaque colour at
opacity 1, coverage 255 *stores* instead of blending.

`Mask::is_valid()` wants a non-zero extent, `stride >= w`, and
`(h - 1) * stride + w` bytes — **not** `stride * h`. That difference is the
point of the type. A mask is a strided *view* into somebody else's buffer,
and a glyph packed flush against the bottom of an atlas page has no bytes at
all after its last pixel: a full final row exists only if the glyph is not
at the page's edge. Demanding one rejected exactly those glyphs, and
rejected them silently — `blit_mask` returned early and they were never
drawn, with no error and no counter moving. `(h - 1) * stride + w` is the
last byte the blit loop can touch, so it is the honest requirement;
`mask_at_the_bottom_right_of_a_page_is_valid_and_blits` pins it.

`blit_masks` is exactly `blit_mask` in a loop (asserted byte-identical in
`mask_batch_equals_a_loop_of_single_blits`) with the per-call setup — the
clip∩surface intersection, the effective source alpha, the opaque-path
decision — hoisted out. It is a **small** win, and honestly so: on a tight
micro-benchmark of 50 8×12 glyphs it saves ~3 % (2.44 → 2.36 µs per run,
`mask_batch_and_loop_timing`), and on the full-screen bench scenes (f) vs (g)
it is inside the noise (0.235 vs 0.234 ms). The per-call setup is a handful
of integer ops; the batch exists because a run of glyphs is the natural call
shape for the text painter, not because it unlocks a faster inner loop.

### Colour space

sRGB **bytes are blended as-is, with no linearization**. This is a
deliberate M1 simplification: it is what most toolkits do, it costs nothing,
and it keeps the inner loops integer-only. Gamma-correct blending would need
a 512-entry LUT in and a 4096-entry LUT out, or f32 pixels. Revisit when
there is a reason — the blend is in one module (`blend.rs`) and the change
would be local.

### Allocation and scratch

**Zero allocation**, per call and per frame. There is also no `Scratch`
parameter: coverage is computed on the fly from a fixed-size `RowSpans`
value that lives on the stack, so there is nothing for the caller to own.
The spec allowed for a `&mut Scratch`; it turned out not to be needed, which
is a strictly smaller surface.

### `unsafe` and SIMD

None of either. The loops slice a row once and then run
`chunks_exact_mut(4)`, which the compiler autovectorizes — a release build
contains a few hundred packed-integer SSE instructions across these loops.
Nothing is `target_feature`-gated, so the same binary runs on the
SSE4.2-only test box.

## Limitations (M1)

- **No rotation or shear.** Axis-aligned rects only. A rotated node needs a
  path rasterizer; that is a later decision, not a gap in this one.
- **No gamma-correct blending** (see above).
- **Linear gradients are axis-aligned.** A gradient whose axis is diagonal
  is projected onto its dominant component. The scene never asks for one.
- **No text layout, shaping or font handling.** Glyphs arrive as A8 coverage
  masks from a server-side atlas and are drawn with `blit_mask` /
  `blit_masks`; everything upstream of the mask lives in another crate.
- No radial/sweep gradients, no blur, no blend modes other than source-over.

## Benchmark

`benches/raster.rs` — criterion-free: `std::time::Instant`, 3 warm-up
iterations, then N timed ones, reporting min and median. Run it with

```sh
cargo bench -p nitro-raster              # default per-scene iteration counts
cargo bench -p nitro-raster -- --iters 50 --json
```

The five scenes mirror `compare/src/main.rs` (the `vello_cpu` harness)
exactly: same xorshift64 PRNG and seed, same geometry, same colours, same
bounding-box culling policy. The timed region is the whole frame; the back
buffer and the source image are allocated outside it.

| | scene |
|---|---|
| **a** `solid_fill` | 1920×1080 opaque solid fill |
| **b** `rrects_alpha` | opaque background + 1000 rounded rects 64×32 r=6, alpha 128 |
| **c** `gradient` | full-screen vertical linear gradient, opaque |
| **d** `blits` | background + 200 blits of a 64×64 straight-alpha ARGB image scaled 1.5× (bilinear) |
| **e** `ui_frame` | 20 damage rects of 200×150; per rect, 20 windows × (rounded-rect background + 30 alpha rrects + 10 stroked borders), bbox-culled |
| **f** `glyphs_loop` | 40 runs × 50 8×12 A8 glyph masks, one `blit_mask` per glyph |
| **g** `glyphs_batch` | identical work to (f), one `blit_masks` per 50-glyph run |

Scenes (f) and (g) are new with the mask work and are not part of the
vello comparison below (which predates them); on this dev box both run at
**0.235 ms / 0.234 ms** for 2000 glyphs per frame.

### Results

Both rasterizers, both machines, single-threaded, 1920×1080. Dev: AMD EPYC
7B13 (AVX2). Box: Pentium G3240, Haswell, 2 cores, **SSE4.2, no AVX2**
(`docs/testbox.md`). Min of ≥25 iterations; median is within ~1 % of min
everywhere on both machines, so only min is tabulated for `nitro-raster` —
full output below.

| scene | nitro dev | vello dev | nitro **box** | vello **box** | box speedup |
|---|---|---|---|---|---|
| a `solid_fill` | **0.17 ms** | 0.33 ms | **1.83 ms** | 2.21 ms | 1.21× |
| b `rrects_alpha` | **5.45 ms** | 3.55 ms | **11.77 ms** | 8.02 ms | 0.68× |
| c `gradient` | **0.18 ms** | 1.84 ms | **1.84 ms** | 5.48 ms | 2.98× |
| d `blits` | **24.17 ms** | 9.29 ms | **36.09 ms** | 21.19 ms | 0.59× |
| e `ui_frame` | **2.97 ms** | 14.68 ms | **5.22 ms** | 27.23 ms | 5.22× |

Only (d) changed by design — see [the blit row split](#the-blit-row-split),
re-taken now that the server paints into a heap shadow. (a), (b), (c), (e)
and the glyph scenes are untouched code.

Those untouched scenes are worth a warning. On dev, (b) reads 5.45 ms here
against 5.20 before — and **the split cannot touch scene (b), which contains
no blit call at all.** It is **code layout, not work**: the split adds ~170
lines of blit code to the binary and shifts everything after it. That is not a
guess — a control binary containing the split but never taking it reproduces
the movement exactly, while (d) stays at the baseline. The effect is not even
stable across machines or days: on the box the same scenes moved 1–7 % in the
*other* direction in one session and not at all in the next. See [measuring
this crate](#measuring-this-crate).

Raw output:

```
# dev, --iters 40 (min of 3 runs; run-to-run spread < 0.5 %)
scene a  solid_fill    min=0.170ms  median=0.171ms
scene b  rrects_alpha  min=5.450ms  median=5.473ms
scene c  gradient      min=0.176ms  median=0.178ms
scene d  blits         min=24.169ms median=24.272ms
scene e  ui_frame      min=2.971ms  median=2.978ms
scene f  glyphs_loop   min=0.227ms  median=0.229ms
scene g  glyphs_batch  min=0.225ms  median=0.227ms

# box (ssh kaspar@192.168.1.204), --iters 25 (min of 3 runs)
scene a  solid_fill    min=1.834ms  median=1.887ms
scene b  rrects_alpha  min=11.776ms median=11.829ms
scene c  gradient      min=1.841ms  median=1.887ms
scene d  blits         min=36.299ms median=36.415ms
scene e  ui_frame      min=5.215ms  median=5.226ms
scene f  glyphs_loop   min=0.314ms  median=0.315ms
scene g  glyphs_batch  min=0.316ms  median=0.317ms
```

(d) on the box is the noisiest number here, and the number that moved most
under scrutiny. Seven interleaved pairs, order flipped from pair 4, all seven
the same sign: baseline 38.05 ms against 36.09 (−5.2 %). One baseline run came
in at 36.46 against a 37.98–38.31 cluster; including it the delta is −4.5 %,
and it is reported rather than dropped.

An earlier measurement of this same lever said −7.4 %. It was taken against a
build with the `alpha == 0` bug described in [the guard that looks
redundant](#the-guard-that-looks-redundant), and about two of those seven
points were the bug skipping work that has to happen. Dev did not notice the
fix (24.17 vs 24.21 ms); the box, which has no AVX2, did.

Before this round of work, scene (e) measured **5.18 ms on dev and 9.36 ms on
the box**, and scene (d) **27.26 / 38.08 ms**.

### The thin-border fast path

Most of a UI frame's stroke work is 1–2 px window chrome, and the general
path charged a lot for it: two `RowSpans` decompositions and a coverage
subtraction per pixel, on rows whose geometry is *identical* to the row above
and below. Two observations fixed that, and together they took scene (e) from
**9.36 to 5.22 ms on the box** (5.18 → 3.02 on dev).

**1. The corner-free rows are the same row, over and over.** Between the
corner arcs a stroke is two vertical bands, and their columns and coverages
do not depend on `y` at all. So the blend is resolved once, all the way down
to its operands — not just the coverage but the premultiplied source
`[b,g,r] * a + 128` and the `255 - a` — into a small fixed-size table
(`Band`, at most 16 columns, on the stack). Each of those rows then costs
three multiply-adds per column and nothing else. Resolving only the
*coverage* and calling the generic per-pixel blend was measured first and is
worth having, but resolving the whole blend is another 0.17 ms on dev.

**2. The top and bottom bands are long constant-coverage runs.** Counting the
work on the benchmark's chrome: the vertical bands are 60 200 rows carrying
61 258 pixels — about **one pixel per row** — while the horizontal ones are
3 620 rows carrying 134 746, i.e. **37 pixels per row**. The pixels are in
the rows the first lever does *not* cover. Those runs have a single coverage
across their whole width (every sub-scanline covers them, and the inner shape
does not reach the row), so they go to `blend_solid` — the same vectorized
multiply-add loop a solid fill uses — instead of a coverage evaluation per
column. That is the larger of the two wins: 3.81 → 3.16 ms on dev, measured
with the blit split still in place (it was reverted afterwards, which is why
the absolute figures here do not match the final table — the *deltas* are the
point).

Both levers change only the arithmetic per pixel, never the order or width of
the writes. That was deliberate, and it is why they survived on the server's
write-combined framebuffer when the blit split did not — see [the benchmark
lies about blits](#the-benchmark-lies-about-blits) for that episode, and [the
blit row split](#the-blit-row-split) for the lever's return once the
destination stopped being write-combined.

The fast path is a pure optimization, and the tests hold it to that:
`stroke_fast_path_is_byte_identical_to_the_general_walk` runs 1000 geometry
combinations — fractional origins, five radii from sharp to clamped, widths
from 0.5 to 9 px, two opacities, four clips including ones that cut the bands
in half — and asserts the two paths produce **byte-identical** buffers, using
a test-only `stroke_rect_inside_general` that forces the general walk. Exact
equality is the right bar, not approximate: a border that shifted by one
level between its corner rows and its straight rows would be a visible seam.
`stroke_fast_path_still_blends_each_pixel_once` pins the no-double-blend
contract on the fast rows specifically.

A band wider than 16 columns, or one that would overlap its partner, falls
back to the general walk rather than growing the table — a thick border is
not the case this exists for, and the fallback keeps the fast path's
preconditions trivially true.


### Against the targets

The spec set two targets on the box: **(a) < 1.5 ms** and **(e) < 4 ms**; a
later round added **(d) < 10 ms**. Two of the three were missed, and after
#3693 and #3702 had exhausted the dispatch-level levers the remaining gap was
shown to be per-pixel blend throughput — a SIMD or pixel-format decision.

**That decision was taken (2026-09-14, issue #522): accept the measured
numbers and retarget.** The targets on this page are now

| scene | target | measured (box) |
|---|---|---|
| (a) `solid_fill` | at memory speed | 1.83 ms (within 1 % of `memset`-class floor) |
| (e) `ui_frame` | **≤ 5.3 ms** | 5.22 ms |
| (d) `blits` | **≤ 37 ms** | 36.09 ms |

The reasoning, in full, because a retarget that is not argued is just a
missed target with the evidence deleted:

1. **The running compositor no longer pays this.** Since the shadow buffer
   (#3695) the server paints roughly **0.4 ms of damage per frame**. `ui_frame`
   is a *full-frame synthetic* — 1.77 M pixels over 20 clip rects, 85 % of the
   screen — and the 4 ms target predates the lever that made a full-frame
   repaint stop happening. The target was calibrated against a world where the
   worst case was the common case.
2. **The only remaining lever needs `unsafe` or nightly.** Explicit SIMD means
   `std::arch` intrinsics (an `unsafe` exception in a crate whose entire point
   is `#![forbid(unsafe_code)]`) or `std::simd` (nightly; the tree is on
   stable). The packed-`u32` two-channels-at-a-time blend is the one
   `unsafe`-free trick left, and the instrumented breakdown below says it does
   not reach 4 ms on (e) and gets nowhere near 10 ms on (d).
3. **Scene (d) stresses a path the desktop barely uses.** The instrumented
   server does six scaled blits per run — **0.288 % of painted pixels** reach
   `blit_scaled`. Of the two, (e) is the one that would matter if either did.
4. **There is a natural time to revisit.** M5's Wayland adapter and dma-buf
   import decide whether full-screen image blits go through the CPU rasterizer
   at all. If they do not, this crate's blit path stops being on the hot path
   for good; if they do, the decision is re-made with the real workload in
   hand rather than against a synthetic.

The measured history below is unchanged — the *target* line moved, the
measurements did not.


- **(a) is 1.86 ms — missed, but it is at the hardware floor.** A
  micro-benchmark on the box (`tmp/fillbench.rs`, not committed) writing the
  same 8.3 MB buffer:

  | | box | dev |
  |---|---|---|
  | `buf.fill(byte)` (`memset`) | 0.834 ms (9.95 GB/s) | 0.097 ms |
  | `chunks_exact_mut(4)` per row | 1.857 ms (4.47 GB/s) | 0.148 ms |
  | `chunks_exact_mut(8)` per row | 1.855 ms | 0.148 ms |
  | `align_to_mut::<u32>().fill()` | 1.845 ms | 0.148 ms |

  Every way of writing a **4-byte repeating pattern** costs ~1.85 ms on that
  machine — `memset` is twice as fast only because a 1-byte pattern hits a
  different microcode path. `nitro-raster` hits 1.86 ms, i.e. **within 1 % of
  the fastest possible way to fill that buffer in Rust on that CPU**. The
  1.5 ms target is not reachable without changing the pixel format or not
  touching every pixel; it should be restated as "at memory speed", which it
  is. (Also worth noting: the real compositor rarely repaints the full screen
  — that is what the damage rects are for.)

- **(e) is 5.22 ms, down from 9.36 ms (1.79× faster), and is now the
  target.** The scene is deliberately brutal: after culling it still paints
  **1.77 M pixels per frame**, i.e. 85 % of a full screen spread over 20 clip
  rectangles, most of it 1.5 px-wide anti-aliased stroke bands. A realistic UI
  frame redraws far less, and since #3695 the server's actual per-frame paint
  is ~0.4 ms. What is left is per-pixel blend throughput, not setup: see [the
  thin-border fast path](#the-thin-border-fast-path) for the two levers that
  produced the 1.79× and the measurement that says the remaining gap is not
  another dispatch trick. The original 4 ms would need SIMD; see the retarget
  rationale above.

- **(d) is 36.09 ms on the box, down from 38.08 — and the old 10 ms target was
  never reachable in scalar code, which is why it is now 37 ms.** The row
  split that #3693 measured and
  then reverted has been **re-taken**: 27.31 → 24.17 ms dev, 38.05 → 36.09 ms
  box. It was reverted because it lost on the write-combined DRM dumb buffer;
  #539 moved the rasterizer's destination to a heap shadow, which removed that
  objection. See [the blit row split](#the-blit-row-split).

  The old target was out of reach by a margin the split cannot close.
  An instrumented breakdown of one 96×96 bilinear blit, dev, 200 blits:

  | inner loop | |
  |---|---|
  | full bilinear + straight-alpha compositing (what we do) | 25.8 ms |
  | bilinear RGB with no alpha work at all | 11.4 ms |
  | nearest-neighbour + full source-over blend | 11.3 ms |
  | nearest-neighbour straight copy (the memory floor) | 2.2 ms |

  **Alpha handling alone is 14 of the 25.8 ms**, and even an alpha-free
  bilinear — wrong output, quoted only as a bound — is 11.4 ms, i.e. above
  the 10 ms target *on the faster machine*. The 200 blits cover 1.8 M
  destination pixels each needing four texels weighted per channel: 4× the
  arithmetic of a fill at the same pixel count, which is exactly what
  `vello_cpu`'s hand-written SIMD kernels do 4 or 8 lanes at a time and our
  `chunks_exact_mut(4)` loops do one. The 10 ms target implies a ~3.6×
  speedup over a loop already within 2× of a plain nearest-neighbour blend;
  **on an SSE4.2-only CPU with no explicit SIMD that is not available**, and
  the honest restatement is "blits are the one scene where the no-SIMD
  constraint costs us, by about 1.7× against vello" — which is what the 37 ms
  target now says.

### The benchmark lies about blits

**Superseded as of #539 — it no longer does, and the reason is worth
keeping.** Everything below was true and load-bearing when the server
rasterized straight into the DRM dumb buffer. It now paints into a
heap-resident *shadow buffer* and streams the damage rects out to the
dumb buffer with write-only row copies (`crates/nitro-server/src/frame.rs`,
`docs/latency.md` §4.5), so **the destination the rasterizer writes is
ordinary cached heap memory — exactly what this benchmark measures.** The
benchmark and the server agree again.

So the rules at the end of this section have changed weight: the first
one has been replaced outright (`cargo bench` is the instrument now; see
[measuring this crate](#measuring-this-crate)), and the second is a good
idea rather than a law. The data is kept in full, because it is the evidence
that produced the shadow buffer, and because it is the cleanest example
in this repository of a benchmark being right about the CPU and wrong
about the machine.

---

This benchmark paints into a `Vec<u8>` — ordinary cached heap memory. The
server *used to* paint into a **DRM dumb buffer, which is
write-combined**: writes are cheap and coalesced, but every *read* is
uncached. Source-over is read-modify-write, so the two destinations had
materially different cost models, and a change could win here and lose
there.

That is not hypothetical. The blit row split — hoisting the per-pixel clamp
and the early-`continue`s out of the interior run so it vectorizes — was
measured, kept, and then reverted when the end-to-end number disagreed:

| | dev bench | box bench | server `paint_us_mean` on the box |
|---|---|---|---|
| without the split | 28.47 ms | 38.08 ms | **5883 µs** |
| with the split | **24.21 ms** | **35.90 ms** | **6230 µs** (+5.9 %) |

Four interleaved A/B pairs, redeploying between every run, all four the same
sign and non-overlapping — not drift. Bisected to the split specifically:
disabling the *stroke* fast path left the regression in place (+25 µs, noise),
while restoring the original `blit_scaled` removed essentially all of it.

The mechanism is the destination. The split trades arithmetic for a less
sequential write pattern, which is free in cached RAM and expensive on
write-combined memory. The same server, same scene, same 256 000 px damage,
switching only the backend:

| backend | without split | with split |
|---|---|---|
| DRM dumb buffer (write-combined) | 5883 µs | 6230 µs |
| `NITRO_BACKEND=fake` (heap) | 758 µs | 658 µs |

On the heap the split is 13 % **faster** — the benchmark was right about the
CPU work. On the real framebuffer it is 6 % slower, and the framebuffer is
what ships. Note also the scale: **~87 % of `paint_us` was framebuffer
traffic**, not raster arithmetic (758 vs 5883 µs for identical work), so
`paint_us_mean` was largely a memory-bandwidth instrument — its noise floor
was ~±100 µs, which could not resolve the ~90 µs the stroke work saves.

That last row is what issue #539 was filed on, and it is why the split was
the *right* revert at the time and the wrong lesson to generalise from.
The problem was never the write pattern; it was that the destination was
the wrong kind of memory to be reading at all. With the shadow buffer in
place the same measurement on the same box reads:

| per frame, same scene and damage | before #539 | after #539 |
|---|---|---|
| `paint_us_mean` (rasterizer) | 6233 µs | **418 µs** |
| `copy_us_mean` (stream to the dumb buffer) | — | 251 µs |
| total | 6233 µs | **669 µs**, 9.3× |

Six interleaved pairs, same binary, `NITRO_SHADOW=0` against the default;
`docs/latency.md` §4.5 has the full run. `paint_us_mean` is now within a
factor of the fake backend's 758 µs, which is the point: **it is no longer
mostly framebuffer traffic**.

It is tempting to conclude from that last table that `paint_us_mean` became a
rasterizer instrument. **It did not**, and the sentence that used to stand
here — "its noise floor is small enough to resolve the ~90 µs of stroke work
that used to disappear into it" — was wrong. Dropping from 6233 to ~300 µs
removed the bandwidth floor and exposed a *different* one: at that scale the
measurement resolves binary code layout, which moves it by ±35 µs for code
that never runs. [Measuring this crate](#measuring-this-crate) has the
control that shows it.

The rules that follow, for anyone optimizing this crate:

- **`cargo bench` is now representative — and it is the instrument.** The
  destination is heap on both sides, so a change that wins here wins there.
  The old advice in this slot was "check `paint_us_mean` on hardware anyway";
  that has been tried properly and **`paint_us_mean` can no longer resolve a
  change to this crate** — see [measuring this
  crate](#measuring-this-crate) for the measurement that retired it.
- **Write each destination pixel once, sequentially.** It is the one rule
  that held on both memory types even when they disagreed about everything
  else, and it is why the stroke work is safe: it changes the arithmetic per
  pixel, never the write pattern. It is also, now, the rule the
  shadow-to-framebuffer copy is built out of. The blit row split obeys it
  too — it splits a row into runs but still writes each pixel once, left to
  right.
- **The blit row split has been re-taken.** It was 13 % faster on the heap
  and was reverted for a destination the server no longer has. It is back,
  with the byte-identity test it never had: [the blit row
  split](#the-blit-row-split).
- The 16-byte-store and `copy_within` entries in the rejected list below
  were the same lesson found earlier from the other direction; they were
  rejected on the heap benchmark too, so they stay rejected.

### The blit row split

The scaled blit walks one destination row at a time. The original loop did,
per pixel: a bounds test on the texel pair, a coverage evaluation, and three
early-`continue`s (transparent texel, zero coverage, zero alpha). None of
that vectorizes — a loop the compiler cannot prove uniform stays scalar.

The split hoists the conditions out of the pixel and into the *run*. Two
facts make a contiguous interior run possible:

- **Coverage is constant across the interior.** Only the first and last
  column of a destination row can be partially covered, so between
  `full_start()` and `full_end()` the coverage is exactly 1 (on a
  full-height row) and the whole `effective_alpha` fold collapses to one
  value hoisted out of the loop.
- **The clamp is a range, not a per-pixel test.** The source x mapping is
  monotonic, so the columns whose texel pair `[ix, ix+1]` lies inside the
  source rect form **one contiguous run**, and its ends are two divisions
  (`texels_in_range`) instead of a comparison per pixel.

Intersect the two and the row becomes at most three runs: a leading edge, an
interior, a trailing edge. The edges keep the fully general loop; the
interior (`blit_run_inner`) is branch-free — loads, multiplies, one 4-byte
store — which is the shape the autovectorizer wants.

Two of the early-`continue`s are deliberately **not** carried into the
interior. Skipping a fully transparent texel and blending it write the same
bytes, so those branches bought nothing and cost the vectorizer everything.
That was the lesson of the *first* attempt at this split, which kept them and
was slower.

**The third one is not like the other two, and dropping it was a bug.** See
[the guard that looks redundant](#the-guard-that-looks-redundant) — it is the
most useful thing this round produced after the measurement section, because
of how nearly it shipped.

| | dev | box |
|---|---|---|
| without the split | 27.31 ms | 38.05 ms |
| with the split | **24.17 ms** | **36.09 ms** |
| | −11.5 % | −5.2 % |

Four interleaved pairs on dev, seven on the box (order flipped from pair 4),
all the same sign on both.

**It is held to byte identity**, the same bar as the thin-border fast path:
`blit_split_is_byte_identical_to_the_general_walk` runs 320 geometry
combinations — scales above and below 1:1 and non-integer ones, fractional
destination origins (which move the coverage's partial columns and the 16.16
phase independently), sub-rects that put the source-clamp boundary inside the
row, both pixel formats, and clips that cut a row down to two columns so it is
*all* edge and has no interior at all — against a test-only `blit_general`
that forces every column through the edge run. Exact equality is the right
bar: an interior that rounded differently from its edges would be a seam down
both sides of every scaled image.

**That test is necessary and it is not sufficient** — see below.

This is the lever #3693 measured, kept, and then reverted because it lost on
the write-combined DRM dumb buffer. #539 moved the rasterizer's destination to
a heap shadow, which removed the reason for the revert; re-taking it was this
round's job.

### The guard that looks redundant

The interior run drops the general loop's three early-`continue`s. Two of
them — a fully transparent texel, a zero coverage — really are redundant:
blending them writes the destination's own bytes back. The third is
`alpha == 0`, and it is **load bearing**:

```rust
over_premul(src_premul, dst, 0) == src_premul + dst
```

With zero alpha the destination is not preserved — the premultiplied source
channel is *added* to it. That is harmless only if a zero alpha implies zero
channels, and it does not always, because the channel and the alpha are
**rounded separately**. `bilinear` can return `t.b > t.a` by one step, and
then `div255(t.a * extra)` is 0 while `div255(t.b * extra)` is 1. There are
637 such `(t.a, t.b, extra)` triples.

The visible effect was **one pixel, one channel, off by one**, in a sweep of
3600 blit configurations. A blit that brightens a single pixel by 1/255 does
not get reported as a bug and does not get found by looking.

**Why the byte-identity test did not catch it.**
`blit_split_is_byte_identical_to_the_general_walk` compares the split against
`blit_general` — but `blit_general` forces every column through the *edge*
run, and the edge run is part of the same new code. Both sides of the
comparison dropped the guard together, so both agreed, byte for byte, on the
wrong answer. **Byte-identity to yourself is not correctness**, and that is
the trap: the test is a genuinely strong check on the split *boundary*, and it
is worth nothing against a mistake in the code the two paths share.

What found it was comparing against **the previous revision** rather than
against a sibling path: hash the output of a blit sweep through the public API
on this branch and on `329faf3`, diff the hashes, bisect to the case, dump the
pixels. `blit_output_matches_the_golden_hash` now pins that value permanently,
so the next change to this code is checked against what shipped before it, not
against its own reflection.

**Restoring the guard was not free**, which is why the shape of the fix is
worth recording. Putting it back per pixel cost **2.1 ms of the 3.1 ms the
split saves** — it sits in the dependency chain of all three channels and
stops the loop vectorizing. But `extra` is *constant across a run*, and the
guard is provably dead when `extra >= 128`: above that threshold
`div255(t.a * extra)` is zero only when `t.a` is zero, and `bilinear` returns
an all-zero texel in that case. (128 is exact, not a round number — at 127,
`t.a = 1` still rounds to zero.) So it is resolved **once per run**, and the
faint-`extra` loop is `#[cold]` and `#[inline(never)]`, because merely letting
it share a function with the hot loop cost **1.6 ms even when it never ran**.
Scene (d) is back to 24.17 ms on dev and 36.09 on the box, and the sweep hash
matches `329faf3` exactly.

Three tests pin the reasoning rather than just the outcome: the `alpha == 0`
sweep over every `(t.a, t.b, extra)`, the threshold being exactly 128, and
`bilinear` zeroing its channels when its alpha is zero. If any of those three
facts stops being true, the fast path is unsound, and one of them is in
another function — which is exactly the sort of coupling that rots silently.

### Measuring this crate

**Use `cargo bench`. Do not use the server's `paint_us_mean`.** That is a
change from the previous advice, and it was established by measurement rather
than by preference.

The temptation is obvious: #539 took `paint_us_mean` from 6233 µs to ~300 µs,
so it stopped being a memory-bandwidth instrument. It did not thereby become
a rasterizer instrument. Re-taking the blit row split was the test case.

First, the server barely does the thing at all. Instrumented, the whole
desktop — wallpaper, bar, launcher, a client cycling 30 times — performs
**6 scaled blits per run**: the 128×128 wallpaper from a 64×64 source, 98 304
pixels against ~34 M painted per run. **0.288 % of painted pixels reach
`blit_scaled`**, the only function the split touches. Ceiling on the effect:
**~0.07 µs** of a ~300 µs mean.

Measured anyway, on the box, with the shadow on, six interleaved pairs and the
order flipped mid-series:

| | `paint_us_mean` |
|---|---|
| without the split | 316.5 µs (sd 15.1) |
| with the split | **291.0 µs** (sd 15.1) |
| paired diff | **−25.5 µs**, t = 3.38, all six pairs the same sign |

A textbook-significant result — and **350× larger than the mechanism
permits**. So a third binary was built: the split code compiled in, but never
taken (`blit_impl(split=false)`), which executes byte-identical instructions
to the baseline on every scene.

| | `paint_us_mean` | vs baseline |
|---|---|---|
| baseline | 306.7 µs | — |
| **control — split code present, never executed** | **341.6 µs** | **+34.9 µs** |
| split | 291.0 µs | −15.7 µs |

**The control moved more than the change did, and in the opposite
direction.** At ~0.3 ms, `paint_us_mean` resolves *binary layout* — code
alignment, branch-predictor aliasing, i-cache placement — not sub-µs raster
work. `copy_us_mean` stayed flat throughout (169.5 vs 168.7 µs), which is the
control that *should* not move: the split changes arithmetic, not the write
pattern.

The same effect is visible in the benchmark table above, which is why it is
called out there: scenes with **no blit calls at all** move by up to 5 % when
blit code is added to the binary, downward on the box and upward on dev.

So, concretely:

- **Build the never-taken control.** A variant containing your code on a path
  that is never entered costs one binary and separates "my change is faster"
  from "my change moved the code". Six same-sign interleaved pairs at t = 3.4
  were not enough without it.
- **Check the mechanism against the size of the claim.** Work out what
  fraction of the work your change can possibly touch *before* measuring. A
  delta 350× larger than the mechanism allows is a bug in the measurement, no
  matter how good its statistics are.
- **`paint_us_mean` still answers server-level questions** — it is how #539
  was measured, a 15× effect. It is the wrong instrument for a 1 % change in
  one rasterizer function, and it will produce a confident, reproducible,
  wrong answer if asked.


### vello_cpu comparison

Full detail in `compare/RESULTS.md`; the harness is
`compare/` (a standalone Cargo project with an empty `[workspace]` table, so
it is not part of ours, and the workspace `Cargo.toml` already excludes it).

| | `nitro-raster` | `vello_cpu` 0.2.0 |
|---|---|---|
| dependencies (`cargo tree -e normal \| sort -u \| wc -l`) | **2** (self + `nitro-core`) | **49** |
| what they are | nothing | a font stack (`glifo`, `skrifa`, `read-fonts`), a PNG codec (`png`, `flate2`, `miniz_oxide`), a SIMD abstraction (`fearless_simd`), an atlas allocator (`guillotiere`, `euclid`), `bytemuck_derive` → `syn` |
| clean release build | **0.34 s** (`cargo clean && cargo build --release -p nitro-raster`, `lto=fat`, `cgu=1`) | **33.7 s** on a 128-core machine |
| `unsafe` in our tree | none | none (but plenty in theirs) |
| destination format | XRGB8888 — what KMS dumb buffers want | premultiplied RGBA8 only; **a conversion pass to BGRA/XRGB scanout is not in their numbers** |
| damage-rect rendering | native: `clip` on every call, cost proportional to the clip | no rectangular damage path; `push_clip_path` takes an arbitrary `BezPath`, and the scene is built and coarse-rasterized full-size regardless |
| rotation / paths / text | not supported | supported |

**Where each one wins on the box:**

- `nitro-raster` is **5.2× faster on the damage-rect UI frame (e)** and
  **2.8× faster on gradients (c)**, and slightly faster on solid fill (a).
  Scene (e) is *the* compositor workload, and it is the one vello_cpu handles
  worst — 27 ms for a frame that touches 29 % of the screen, more than 3× its
  own full-screen 1000-rrect frame, even with the harness doing the culling.
- `vello_cpu` is **1.5× faster on 1000 alpha rounded rects (b)** and
  **1.7× faster on scaled bilinear blits (d)**. Both are pure per-pixel
  throughput, which is exactly what its hand-written SIMD kernels buy and
  what our autovectorized scalar loops cannot match on a no-AVX2 CPU.

**Is our rasterizer > 2× slower than `vello_cpu` on (b) or (e)?** No. On (e)
we are 5.2× *faster*. On (b) we are 1.47× slower (11.77 vs 8.02 ms) — the
spec's "more than 2× slower" threshold is not crossed. The only scene where
we lose badly in absolute terms is (d), blits, at 1.7× slower (36.09 vs
21.19 ms) — down from 1.8× with the row split re-taken.

### Recommendation

**Keep the own rasterizer.** The decision is the creator's; this is the
recommendation on the numbers:

1. It wins on the scene that actually describes the server's job. Damage-rect
   painting is the whole architecture (goal 1: work proportional to what
   changed), and vello_cpu 0.2 has no mechanism for it — 20 damage rects
   means 20 clip-path pushes over a full-size scene, and it shows: 27 ms vs
   our 5.2 ms.
2. The dependency cost is 49 crates versus 0, including a font stack and a
   PNG codec we do not want, and a 34 s release build (on 128 cores) versus
   0.34 s. Goal 2 says a dependency has to earn its place; on the one
   workload that matters it is slower, so it does not.
3. It is ~1050 lines of `unsafe`-free code (plus ~1670 lines of tests) with
   an exhaustively-tested blend and an analytic AA whose error is measured
   against a supersampled reference. That is the "contained complexity behind
   a tiny surface" goal 3 asks for.
4. Where we lose — per-pixel throughput on alpha fills and bilinear blits —
   the gap is explained (their SIMD kernels vs our autovectorized scalar
   loops) and is *bounded*: the blit breakdown above says even an alpha-free
   bilinear would not reach the old 10 ms target on this CPU, so this is the
   no-SIMD constraint's price, not a missing optimization.

The honest caveats: **vello_cpu is the better renderer** in the abstract —
paths, rotation, text, strokes with joins and caps, and faster raw pixel
throughput. The moment the scene needs arbitrary paths or rotation, this
crate does not cover it and that decision should be revisited rather than
grown into a path rasterizer by accident. And scene (e) missed its original
4 ms target (5.22 ms, from 9.36) even though it now beats vello by 5.2× — the
thin-border fast path was the named follow-up and it is done; what remains is
per-pixel blend throughput on 1.77 M painted pixels, which is the same
no-SIMD ceiling scene (d) runs into. That is the gap issue #522 closed by
retargeting rather than by adding SIMD; see [against the
targets](#against-the-targets).

### Things measured and rejected

- **`zerocopy` 0.8 for a `&mut [u32]` pixel view** (offered by the creator as
  an approved dependency): implemented with
  `<[u32]>::mut_from_bytes(row)` and measured — **0.173 ms vs 0.173 ms** on
  the solid fill, identical on every other scene. The `chunks_exact_mut(4)`
  loop already compiles to the same stores. Reverted; **no dependency added**,
  so `DEPENDENCIES.md` is unchanged.
- 16-byte stores in the solid path: **2× slower on the box** (3.09 vs
  1.86 ms) — the wide `copy_from_slice` defeats the autovectorizer.
- `copy_within` prefix-doubling (memmove) for solid fills: no change on dev,
  slightly worse on the box.
- Caching the per-row column extents in `RowSpans`: slower — the struct is
  copied per row and got bigger.

  Those three, plus `zerocopy` above, were **re-checked this round** against
  the question "was this rejected because the destination was
  write-combined?". None of them was: every one was measured on this
  benchmark, i.e. against heap memory, which is what the server now paints
  into. #539 gives no reason to revisit them, and they stay rejected on their
  original numbers. The blit row split was the only entry in this list whose
  rejection turned on the destination's memory type — the premultiply-caching
  entry below reads as if it might be a second one, and is not: it was
  rejected on a heap measurement too.
- **Splitting the blit row into edge/interior runs** — **re-taken, no longer
  rejected.** Tried twice in #3693. The first attempt split on *coverage*
  alone and kept the per-pixel clamp and the early-`continue`s, so the
  interior loop still could not be vectorized: slower, and that version stays
  rejected. The second split on coverage **and** on "the texel pair needs no
  clamp" and dropped the early-outs inside the run; it won the benchmark
  (28.47 → 24.21 ms dev) and lost the server (`paint_us_mean` 5883 → 6230 µs
  on the write-combined DRM dumb buffer), and was reverted for it. Since #539
  the rasterizer's destination is a heap shadow, so the reason for the revert
  is gone and the lever is back: see [the blit row
  split](#the-blit-row-split) for the current numbers and the byte-identity
  test it now carries. The surviving lesson is the first one only — **a run
  split only pays if the run is *unconditionally* uniform**. The second
  lesson ("a blit win measured only against heap memory is not a win") was
  true of a server that read back from write-combined memory and is not true
  of this one; what replaced it is in [measuring this
  crate](#measuring-this-crate).
- **A separable (two-pass) bilinear resample for blits**, with source rows
  premultiplied and horizontally filtered into a fixed-size stack scratch, so
  each destination row is a contiguous two-tap vertical lerp. **Re-measured
  this round under the re-taken split, and it splits in two:**
  - *Filtering both source rows per destination row* (the form #3693 tried):
    **24.21 → 34.05 ms, far slower**, and slower at every chunk width from 64
    to 512 columns (34.7 / 34.1 / 34.7 / 36.0). The original rejection is
    confirmed, and the margin is much larger than the 25.34 ms first
    recorded — the horizontal pass is redone for every destination row, so at
    a scale near 1:1 it nearly doubles the filtering work.
  - *Caching the filtered rows across destination rows* (keyed on the source
    row index and the 16.16 phase, so each source row is filtered once per
    blit) is the strongest form of the lever, and it **does win: 24.22 →
    23.23 ms, −4.1 %**, with a never-taken control sitting exactly on
    baseline (24.22 ms), so the win is real work and not code layout.
    **Not taken**, for two reasons that outweigh 4 %: it is **not
    byte-identical** to the exact path (the two-pass rounding differs by ±1 on
    2.5 % of bytes — diffuse, not a seam, but still a second answer to the
    same question), and it needs a **16 KB stack scratch** in a crate whose
    stated contract is zero allocation and small fixed-size stack state. A 4 %
    scene-(d) win is not worth giving up exact equality with the general walk;
    revisit if blits ever dominate a real frame, which [measuring this
    crate](#measuring-this-crate) shows they do not.
- **Premultiplying the source once per blit** (the whole 64×64 image into a
  scratch, or two rows at a time): **22.9 / 21.8 ms against 23.7 ms** for the
  current per-pixel fold in an isolated probe — a real but small win that did
  not survive being put behind the same destination-stride effect as the
  separable path. The per-pixel alpha fold folds each texel's alpha into its
  bilinear *weight*, which costs one multiply per texel and no extra pass, so
  there is less to save here than the phrase "premultiply once" suggests.
  (This was rejected on the heap benchmark, not on the framebuffer, so #539
  does not reopen it; the cached separable above is the same idea taken
  further and measured again, and it is the one that wins — by 4 %.)
- **Specialising the one-column stroke band** (hoisting the single pixel out
  of the band's inner loop, since a 1–2 px border makes `n == 1` the typical
  case): **2.98 → 3.10 ms on scene (e), slower.** The generic loop over a
  `[u8; 4]` slice was already what the compiler wanted; the special case only
  added a branch on the hot path.
- **Run-detection in the opaque mask path** (scan `cov` for maximal runs of
  255 and `store_solid` each one, instead of a per-pixel store): **35 % slower**
  on bench scene (f) — 0.312 vs 0.231 ms. A glyph is a few pixels wide with
  anti-aliased edges, so the runs are 1–3 px long and the scan costs more than
  the wide store saves. The per-pixel store stayed.
- **Nearest-neighbour for integer scales** was specified as a blit lever and
  is *not* implemented, because the benchmark's 1.5× is not an integer scale
  and nothing in the scene hits it. The 1:1 case — the one the compositor
  actually generates, for an unscaled window — already has its own exact path
  (`blit_1to1`), and it is 10× faster than the bilinear one (2.8 vs 26.3 ms
  for 200 blits). Adding a third path for 2×/3× would be speculative: no
  caller asks for it. Worth revisiting if HiDPI scaling ever lands.

  The instrumentation in [measuring this crate](#measuring-this-crate)
  sharpens this: the running desktop performs **6 scaled blits per run**, all
  of them the wallpaper, and **0.288 % of painted pixels** reach
  `blit_scaled` at all. Everything else is `blit_1to1`, fills, strokes and
  glyph masks. Scene (d) is a stress test of a path the compositor barely
  uses — worth keeping honest, not worth contorting the code for.
