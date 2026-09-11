# `vello_cpu` comparison harness — results

Standalone harness (`nitro-raster-compare`, binary `vello-bench`) that renders 5
fixed scenes with [`vello_cpu`](https://crates.io/crates/vello_cpu) so the numbers
can be compared against nitro's own CPU rasterizer. Scene semantics are documented
in the header comment of `src/main.rs` — that comment is the normative spec our own
harness must mirror.

## Crate version and dependency footprint

* Resolved crate: **`vello_cpu` v0.2.0** (crates.io), plus `vello_common` v0.2.0.
* Features: default (`png`, `std`, `text`, `u8_pipeline`). **`multithreading` is OFF**,
  so `RenderSettings::num_threads == 0` and the single-threaded dispatcher is used —
  we compare single-threaded CPU rasterization.
* Dependency count: `cargo tree -e normal --prefix none | sort -u | wc -l` → **49**
  (unique `name vX.Y.Z` lines, including the root package and proc-macro build deps).

Trimmed `cargo tree -e normal`:

```
nitro-raster-compare v0.0.1
└── vello_cpu v0.2.0
    ├── bytemuck v1.25.2
    │   └── bytemuck_derive v1.12.0 (proc-macro)
    │       ├── proc-macro2 v1.0.107 → unicode-ident v1.0.24
    │       ├── quote v1.0.47
    │       └── syn v3.0.5
    ├── glifo v0.3.0                       # glyph rasterization / atlas
    │   ├── foldhash v0.2.0
    │   ├── hashbrown v0.17.1
    │   ├── log v0.4.34
    │   ├── peniko v0.6.1
    │   │   ├── color v0.3.3
    │   │   ├── kurbo v0.13.1
    │   │   │   ├── arrayvec v0.7.8
    │   │   │   ├── polycool v0.4.0
    │   │   │   └── smallvec v1.16.1
    │   │   └── linebender_resource_handle v0.1.1
    │   ├── png v0.18.1
    │   │   ├── bitflags v2.13.2
    │   │   ├── crc32fast v1.5.1 → cfg-if v1.0.4
    │   │   ├── fdeflate v0.3.7 → simd-adler32 v0.3.10
    │   │   ├── flate2 v1.1.10 → miniz_oxide v0.9.1 → adler2 v2.0.1
    │   │   └── miniz_oxide v0.8.9
    │   ├── skrifa v0.44.0
    │   │   └── read-fonts v0.41.0
    │   │       ├── font-types v0.12.5
    │   │       └── once_cell v1.21.4
    │   └── vello_common v0.2.0
    │       ├── fearless_simd v0.4.1       # SIMD abstraction
    │       ├── guillotiere v0.7.0 → euclid v0.22.14 → num-traits v0.2.19
    │       └── thiserror v2.0.20 → thiserror-impl v2.0.20 (proc-macro)
    ├── hashbrown v0.17.1 (*)
    ├── png v0.18.1 (*)
    └── vello_common v0.2.0 (*)
```

Notable: even with only the default features, a font stack (`glifo`, `skrifa`,
`read-fonts`, `font-types`), a PNG codec (`png`, `flate2`, `miniz_oxide`, `fdeflate`)
and a texture-atlas allocator (`guillotiere`, `euclid`) are pulled in. `text`/`png` can
be turned off, but they are on by default.

## Clean release build time

`cargo clean && /usr/bin/time -f '%e' cargo build --release` on the dev machine
(profile: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3` — matching nitro's
release profile as closely as reasonable):

**33.66 s wall clock** (warm cargo registry/cache; 128-core machine, so dependency
compilation is heavily parallel — on the 2-core box this would be far longer).

## Machines

| | dev | test box |
|---|---|---|
| CPU (`lscpu \| grep "Model name"`) | AMD EPYC 7B13 64-Core Processor | Intel(R) Pentium(R) CPU G3240 @ 3.10GHz |
| cores used | 1 (single-threaded) | 1 of 2 (single-threaded) |
| SIMD available | AVX2 (+AVX-512 subset) | **SSE4.2 only, no AVX/AVX2** |
| RAM | 125 GB | 3.3 GB, no swap |
| glibc | 2.43 | 2.43 (binary ran as-is, no rebuild needed) |

`vello_cpu` auto-detects the SIMD level (`Level::try_detect()`), so the box runs the
SSE4.2 path and dev runs the AVX2 path. Part of the dev/box gap is clock + IPC, part
is vector width — keep that in mind when comparing against our own rasterizer, which
should be measured on both machines too.

## Benchmark results

Target 1920x1080, premultiplied RGBA8, `RenderMode::OptimizeSpeed`,
`CompositeMode::Replace`, warmup 3 iterations, per-scene iteration counts below.
Timed region per iteration = `ctx.reset()` + full draw-list construction +
`ctx.flush()` + `ctx.render(&mut pixmap)` (i.e. coarse + flatten + fine rasterization
into the CPU pixmap). Pixmap/context/resource allocation and the 64x64 source image
for scene (d) are outside the timed region.

| scene | iters | dev min | dev median | box min | box median | box/dev |
|---|---|---|---|---|---|---|
| a `solid_fill` | 4000 | 0.327 ms | 0.329 ms | 2.211 ms | 2.273 ms | 6.9x |
| b `rrects_alpha` | 300 | 3.545 ms | 3.554 ms | 8.016 ms | 8.054 ms | 2.3x |
| c `gradient` | 800 | 1.838 ms | 1.849 ms | 5.477 ms | 5.511 ms | 3.0x |
| d `blits` | 200 | 9.291 ms | 9.306 ms | 21.186 ms | 21.276 ms | 2.3x |
| e `ui_frame` | 100 | 14.675 ms | 14.703 ms | 27.232 ms | 27.279 ms | 1.9x |

Raw dev output:

```
vello_cpu 0.2 bench  target=1920x1080 RGBA8(premul)  single-threaded  warmup=3
scene a  solid_fill    min=0.327ms  median=0.329ms  iters=4000
scene b  rrects_alpha  min=3.545ms  median=3.554ms  iters=300
scene c  gradient      min=1.838ms  median=1.849ms  iters=800
scene d  blits         min=9.291ms  median=9.306ms  iters=200
scene e  ui_frame      min=14.675ms  median=14.703ms  iters=100
```

Raw box output (`ssh kaspar@192.168.1.204 '~/nitro-bin/vello-bench'`, one run, nothing
else running, single-threaded, memory footprint a few tens of MB — well inside the
3.3 GB / no-swap budget):

```
vello_cpu 0.2 bench  target=1920x1080 RGBA8(premul)  single-threaded  warmup=3
scene a  solid_fill    min=2.211ms  median=2.273ms  iters=4000
scene b  rrects_alpha  min=8.016ms  median=8.054ms  iters=300
scene c  gradient      min=5.477ms  median=5.511ms  iters=800
scene d  blits         min=21.186ms  median=21.276ms  iters=200
scene e  ui_frame      min=27.232ms  median=27.279ms  iters=100
```

Min and median are within ~1 % of each other everywhere, i.e. the measurements are
very stable; there is no meaningful outlier tail on either machine.

### Deployment notes

`rsync -az target/release/vello-bench kaspar@192.168.1.204:nitro-bin/` and running it
worked **first try, no troubleshooting needed**: both machines are on glibc 2.43 and
the binary is a plain x86-64 ELF with no SIMD baseline above the default target
(vello_cpu dispatches on runtime feature detection rather than requiring `-C
target-cpu`). No rebuild with the box toolchain (cargo 1.95) was necessary — in fact
the box has no cargo installed at all.

## Observations / caveats for the recommendation

1. **Bilinear filtering: supported.** `peniko::ImageQuality::Medium` is bilinear in
   vello_cpu 0.2 (`Low` = nearest, `High` = bicubic), and there is a dedicated bilinear
   path in `fine/mod.rs`. Scene (d) uses `Medium`, so the numbers are true bilinear.
   Note though that scene (d) is the second-slowest scene: 200 scaled 96x96 blits cost
   ~9.3 ms on dev / ~21 ms on the box. Subtracting the one background fill each
   iteration pays for (scene a), that is **~45 µs per 96x96 bilinear blit on dev and
   ~95 µs on the box** — i.e. ~5 ns/px on dev, ~10 ns/px on the box.
   That is very expensive for what a compositor does constantly. Images are consumed as
   a *paint* (`set_paint(Image)` + `set_paint_transform` + `fill_rect`), so every blit
   goes through the generic paint pipeline; there is no fast "blit this pixmap here"
   path. `ImageSource::OpaqueId` exists (registered images, no per-draw `Arc` clone) but
   the upstream example explicitly says *"only `ImageSource::Pixmap` is currently
   supported. Don't use `ImageSource::OpaqueId`"*, so the cheaper handle path is not
   usable in 0.2.

2. **Clipped / damaged rendering is the weak spot.** `push_clip_path` takes an arbitrary
   `BezPath` and pushes it onto a clip stack — there is **no rectangular damage-region
   fast path and no "render only this rect of the scene" entry point that skips work
   proportionally**. `render_with`'s `offset` + a smaller pixmap can cut out a
   sub-rectangle, but the scene is still built and coarse-rasterized at full size, and
   it only gives you *one* rectangle per `render` call, not 20. Concretely: scene (e)
   touches 20 × 200×150 px = 600 000 px, about 29 % of the screen, yet a frame costs
   ~14.7 ms on dev — more than 4x a full-screen 1000-rounded-rect frame (scene b,
   3.5 ms). Even with aggressive bounding-box culling (see below) the per-damage-rect
   clip push plus re-walking the window list dominates. **A damage-driven compositor is
   exactly the workload vello_cpu 0.2 handles worst.**

3. **Culling is done by the harness, not the library.** Scene (e) culls every primitive
   against the current damage rect by bounding box before submitting it (documented in
   the source). This is what a compositor does and it is *already* included in the
   numbers above — so 14.7 ms is the post-culling cost. Without culling it is far worse.
   The PRNG is advanced identically whether a primitive is culled or not, so the logical
   scene is culling-independent and reproducible.

4. **Solid fill is fast, gradients are not free.** A full-screen opaque fill is 0.33 ms
   on dev — 1920x1080x4 B = 8.3 MB of pixel writes, so ≈ 25 GB/s. That is above what a
   single EPYC core sustains to DRAM, which is expected: the 8.3 MB pixmap is re-written
   every iteration and largely stays resident in L3, so this scene measures the
   rasterizer's fill path rather than memory bandwidth. On the box the same fill is
   2.21 ms ≈ 3.8 GB/s, much closer to that machine's real memory bandwidth. A full-screen
   vertical linear gradient is 1.85 ms, ~5.6x the solid fill, so gradient evaluation is
   not folded into a cheap per-scanline span on this path.

5. **Stroking** (`set_stroke` + `stroke_path`) works and is used by scene (e)'s borders
   (1.5 px); no API friction, strokes are flattened like fills.

6. **API friction encountered:** none serious, no scene had to be simplified.
   Minor points: `set_paint` takes the paint by value so each of the 200 blits in scene
   (d) clones an `Image` (an `Arc` clone — cheap, and inside the timed region on
   purpose, since a real user pays it); premultiplication of the straight-alpha source
   image is done once at construction time, outside the timed loop; and the pixel format
   is fixed to premultiplied RGBA8 (`PixelFormat::Rgba8` is the only variant), so if our
   scanout buffers are BGRA/XRGB a conversion pass would be needed on top of these
   numbers — that cost is **not** included here.

7. **Memory:** the working set is modest (one 1920x1080 RGBA8 pixmap ≈ 8 MB plus
   internal strip/tile buffers); the box ran everything comfortably without swap.

## Reproducing

```sh
cd crates/nitro-raster/compare
cargo build --release
./target/release/vello-bench            # default per-scene iteration counts
BENCH_ITERS=50 ./target/release/vello-bench
./target/release/vello-bench --json     # one JSON object per scene per line
./target/release/vello-bench --dump 20  # also write scene_<x>.png for visual checking
```

This directory is its own workspace (empty `[workspace]` table in `Cargo.toml`) and is
excluded from the parent workspace; it does not inherit workspace lints or dependencies.
