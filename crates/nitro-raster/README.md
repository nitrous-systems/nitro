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
| a `solid_fill` | **0.17 ms** | 0.33 ms | **1.85 ms** | 2.21 ms | 1.19× |
| b `rrects_alpha` | **5.20 ms** | 3.55 ms | **11.79 ms** | 8.02 ms | 0.68× |
| c `gradient` | **0.18 ms** | 1.84 ms | **1.85 ms** | 5.48 ms | 2.96× |
| d `blits` | **27.26 ms** | 9.29 ms | **38.08 ms** | 21.19 ms | 0.56× |
| e `ui_frame` | **3.02 ms** | 14.68 ms | **5.22 ms** | 27.23 ms | 5.22× |

Only (e) changed by design — see [the thin-border fast path](#the-thin-border-fast-path).
(a), (c) and (d) are untouched code; (b) drifted down ~4 % on dev with the
same source, which is the scale of this machine's run-to-run variation.

Raw output:

```
# dev, --iters 40 (min of 3 runs; run-to-run spread < 0.5 %)
scene a  solid_fill    min=0.171ms  median=0.172ms
scene b  rrects_alpha  min=5.195ms  median=5.218ms
scene c  gradient      min=0.175ms  median=0.176ms
scene d  blits         min=27.258ms median=27.303ms
scene e  ui_frame      min=3.016ms  median=3.020ms

# box (ssh kaspar@192.168.1.204), --iters 25 (min of 3 runs)
scene a  solid_fill    min=1.854ms  median=1.892ms
scene b  rrects_alpha  min=11.791ms median=11.851ms
scene c  gradient      min=1.851ms  median=1.898ms
scene d  blits         min=38.079ms median=38.144ms
scene e  ui_frame      min=5.211ms  median=5.218ms
```

Before this round of work, scene (e) measured **5.18 ms on dev and 9.36 ms on
the box**.

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
the writes. That is deliberate, and it is why they survive on the server's
write-combined framebuffer where the blit split did not — see
[the benchmark lies about blits](#the-benchmark-lies-about-blits).

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
later round added **(d) < 10 ms**.

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

- **(e) is 5.22 ms — missed by 1.3×, down from 9.36 ms (1.79× faster).** The
  scene is deliberately brutal: after culling it still paints **1.77 M pixels
  per frame**, i.e. 85 % of a full screen spread over 20 clip rectangles,
  most of it 1.5 px-wide anti-aliased stroke bands. A realistic UI frame
  redraws far less. What is left is per-pixel blend throughput, not setup:
  see [the thin-border fast path](#the-thin-border-fast-path) for the two
  levers that produced the 1.79× and the measurement that says the remaining
  gap is not another dispatch trick.

- **(d) is 38.08 ms — unchanged, and the 10 ms target is not reachable in
  scalar code.** A row split *was* implemented and measured a solid win on
  the benchmark (28.47 → 24.21 ms dev, 38.08 → 35.90 box) — and was then
  **reverted, because it made the real server slower**. That story is in
  [the benchmark lies about blits](#the-benchmark-lies-about-blits) below; it
  is the most useful thing this round produced.

  Even had it stood, the target was out of reach. An instrumented breakdown
  of one 96×96 bilinear blit, dev, 200 blits:

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
  constraint costs us, by about 1.8× against vello".

### The benchmark lies about blits

This benchmark paints into a `Vec<u8>` — ordinary cached heap memory. The
server paints into a **DRM dumb buffer, which is write-combined**: writes are
cheap and coalesced, but every *read* is uncached. Source-over is
read-modify-write, so the two destinations have materially different cost
models, and a change can win here and lose there.

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
what ships. Note also the scale: **~87 % of `paint_us` is framebuffer traffic**,
not raster arithmetic (758 vs 5883 µs for identical work), so `paint_us_mean`
is largely a memory-bandwidth instrument — its noise floor is ~±100 µs, which
cannot resolve the ~90 µs the stroke work saves.

The rules that follow, for anyone optimizing this crate:

- **`cargo bench` is necessary, not sufficient.** It measures CPU cost
  faithfully and destination cost not at all. Anything that changes *how*
  pixels are written — order, width, how many times a line is revisited —
  must also be checked against the server on hardware.
- **Write each destination pixel once, sequentially.** It is the one rule
  that holds on both memory types, and it is why the surviving stroke work is
  safe: it changes the arithmetic per pixel, never the write pattern.
- The 16-byte-store and `copy_within` entries in the rejected list below are
  the same lesson found earlier from the other direction.

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
  **1.8× faster on scaled bilinear blits (d)**. Both are pure per-pixel
  throughput, which is exactly what its hand-written SIMD kernels buy and
  what our autovectorized scalar loops cannot match on a no-AVX2 CPU.

**Is our rasterizer > 2× slower than `vello_cpu` on (b) or (e)?** No. On (e)
we are 5.2× *faster*. On (b) we are 1.47× slower (11.79 vs 8.02 ms) — the
spec's "more than 2× slower" threshold is not crossed. The only scene where
we lose badly in absolute terms is (d), blits, at 1.8× slower.

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
3. It is ~880 lines of `unsafe`-free code (plus ~1460 lines of tests) with
   an exhaustively-tested blend and an analytic AA whose error is measured
   against a supersampled reference. That is the "contained complexity behind
   a tiny surface" goal 3 asks for.
4. Where we lose — per-pixel throughput on alpha fills and bilinear blits —
   the gap is explained (their SIMD kernels vs our autovectorized scalar
   loops) and is *bounded*: the blit breakdown above says even an alpha-free
   bilinear would not reach the 10 ms target on this CPU, so this is the
   no-SIMD constraint's price, not a missing optimization.

The honest caveats: **vello_cpu is the better renderer** in the abstract —
paths, rotation, text, strokes with joins and caps, and faster raw pixel
throughput. The moment the scene needs arbitrary paths or rotation, this
crate does not cover it and that decision should be revisited rather than
grown into a path rasterizer by accident. And scene (e) still misses its 4 ms
target (5.22 ms, from 9.36) even though it now beats vello by 5.2× — the
thin-border fast path was the named follow-up and it is done; what remains is
per-pixel blend throughput on 1.77 M painted pixels, which is the same
no-SIMD ceiling scene (d) runs into.

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
- **Splitting the blit row into edge/interior runs.** Tried twice. The first
  attempt split on *coverage* alone and kept the per-pixel clamp and the
  early-`continue`s, so the interior loop still could not be vectorized:
  slower. The second split on coverage **and** on "the texel pair needs no
  clamp" (a contiguous column range from `texels_in_range`, two divisions
  instead of a comparison per pixel) and dropped the early-outs inside the
  run — skipping a fully transparent texel and blending it write the same
  bytes, so the branch bought nothing and cost the vectorizer everything.
  That version **won the benchmark and lost the server**: 28.47 → 24.21 ms on
  dev, but `paint_us_mean` on the box 5883 → 6230 µs. Reverted; the full
  measurement is in [the benchmark lies about blits](#the-benchmark-lies-about-blits).
  Two lessons, not one: a run split only pays if the run is
  *unconditionally* uniform, **and** a blit win measured only against heap
  memory is not a win.
- **A separable (two-pass) bilinear resample for blits**, with source rows
  premultiplied and horizontally filtered once into a fixed-size stack
  scratch, so each destination row is a contiguous two-tap vertical lerp:
  **24.21 → 25.34 ms, i.e. slower**, and it stayed slower at every chunk
  width from 64 to 512 columns. A standalone probe of the same algorithm
  writing a tight 96×96 buffer said it *should* win (24.7 → 18.7 ms); the win
  vanished against a real 1920-pixel destination stride, where the second
  pass re-walks destination rows that have already fallen out of L1. Reverted
  — and a reminder that a kernel probe on a small buffer is not a measurement
  of the same kernel in the frame. (In hindsight this was the same
  destination-memory effect that later killed the run split, seen first.)
- **Premultiplying the source once per blit** (the whole 64×64 image into a
  scratch, or two rows at a time): **22.9 / 21.8 ms against 23.7 ms** for the
  current per-pixel fold in an isolated probe — a real but small win that did
  not survive being put behind the same destination-stride effect as the
  separable path. The per-pixel alpha fold folds each texel's alpha into its
  bilinear *weight*, which costs one multiply per texel and no extra pass, so
  there is less to save here than the phrase "premultiply once" suggests.
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
