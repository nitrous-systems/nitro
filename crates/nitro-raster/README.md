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
`stroke_does_not_double_blend_translucently`.

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
| a `solid_fill` | **0.17 ms** | 0.33 ms | **1.86 ms** | 2.21 ms | 1.19× |
| b `rrects_alpha` | **5.44 ms** | 3.55 ms | **11.88 ms** | 8.02 ms | 0.67× |
| c `gradient` | **0.18 ms** | 1.84 ms | **1.86 ms** | 5.48 ms | 2.95× |
| d `blits` | **28.47 ms** | 9.29 ms | **38.08 ms** | 21.19 ms | 0.56× |
| e `ui_frame` | **5.18 ms** | 14.68 ms | **9.36 ms** | 27.23 ms | 2.91× |

Raw output:

```
# dev, --iters 60
scene a  solid_fill    min=0.169ms  median=0.172ms
scene b  rrects_alpha  min=5.438ms  median=5.452ms
scene c  gradient      min=0.177ms  median=0.177ms
scene d  blits         min=28.473ms median=28.503ms
scene e  ui_frame      min=5.179ms  median=5.187ms

# box (ssh kaspar@192.168.1.204), --iters 30
scene a  solid_fill    min=1.862ms  median=1.896ms
scene b  rrects_alpha  min=11.881ms median=12.006ms
scene c  gradient      min=1.857ms  median=1.944ms
scene d  blits         min=38.077ms median=38.187ms
scene e  ui_frame      min=9.364ms  median=9.374ms
```

### Against the targets

The spec set two targets on the box: **(a) < 1.5 ms** and **(e) < 4 ms**.

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

- **(e) is 9.36 ms — missed by 2.3×.** This is a genuine gap, though the
  scene is deliberately brutal: after culling, it still paints **1.77 M
  pixels per frame**, i.e. 85 % of a full screen spread over 20 clip
  rectangles, most of it 1.5 px-wide anti-aliased stroke bands. A realistic
  UI frame redraws far less. The work is dominated by stroke rows
  (instrumentation: removing the borders takes the scene from 5.2 ms to
  1.3 ms on dev), and each stroke row paints only a couple of pixels per
  band, so per-row setup — not per-pixel blending — is the cost. The obvious
  next lever is a dedicated "thin axis-aligned border" path that emits four
  straight bands instead of two rrect coverage walks; it was left out of this
  task as scope.

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

- `nitro-raster` is **2.9× faster on the damage-rect UI frame (e)** and
  **2.9× faster on gradients (c)**, and slightly faster on solid fill (a).
  Scene (e) is *the* compositor workload, and it is the one vello_cpu handles
  worst — 27 ms for a frame that touches 29 % of the screen, more than 3× its
  own full-screen 1000-rrect frame, even with the harness doing the culling.
- `vello_cpu` is **1.5× faster on 1000 alpha rounded rects (b)** and
  **1.8× faster on scaled bilinear blits (d)**. Both are pure per-pixel
  throughput, which is exactly what its hand-written SIMD kernels buy and
  what our autovectorized scalar loops cannot match on a no-AVX2 CPU.

**Is our rasterizer > 2× slower than `vello_cpu` on (b) or (e)?** No. On (e)
we are 2.9× *faster*. On (b) we are 1.48× slower (11.88 vs 8.02 ms) — the
spec's "more than 2× slower" threshold is not crossed. The only scene where
we lose badly in absolute terms is (d), blits, at 1.8× slower.

### Recommendation

**Keep the own rasterizer.** The decision is the creator's; this is the
recommendation on the numbers:

1. It wins on the scene that actually describes the server's job. Damage-rect
   painting is the whole architecture (goal 1: work proportional to what
   changed), and vello_cpu 0.2 has no mechanism for it — 20 damage rects
   means 20 clip-path pushes over a full-size scene, and it shows: 27 ms vs
   our 9 ms.
2. The dependency cost is 49 crates versus 0, including a font stack and a
   PNG codec we do not want, and a 34 s release build (on 128 cores) versus
   0.34 s. Goal 2 says a dependency has to earn its place; on the one
   workload that matters it is slower, so it does not.
3. It is ~720 lines of `unsafe`-free code (plus ~1050 lines of tests) with
   an exhaustively-tested blend and an analytic AA whose error is measured
   against a supersampled reference. That is the "contained complexity behind
   a tiny surface" goal 3 asks for.
4. Where we lose — per-pixel throughput on alpha fills and bilinear blits —
   the gap is explained (their SIMD kernels vs our autovectorized scalar
   loops) and is addressable incrementally without changing any API, if
   profiling of real frames ever says it matters.

The honest caveats: **vello_cpu is the better renderer** in the abstract —
paths, rotation, text, strokes with joins and caps, and faster raw pixel
throughput. The moment the scene needs arbitrary paths or rotation, this
crate does not cover it and that decision should be revisited rather than
grown into a path rasterizer by accident. And scene (e) misses its 4 ms
target even though it beats vello — the thin-border fast path is the
follow-up.

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
- Splitting the blit row into edge/interior runs: slower, the extra branching
  costs more than the coverage calls it skips.
- **Run-detection in the opaque mask path** (scan `cov` for maximal runs of
  255 and `store_solid` each one, instead of a per-pixel store): **35 % slower**
  on bench scene (f) — 0.312 vs 0.231 ms. A glyph is a few pixels wide with
  anti-aliased edges, so the runs are 1–3 px long and the scan costs more than
  the wide store saves. The per-pixel store stayed.
