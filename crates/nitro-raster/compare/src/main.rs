// nitro-raster-compare / vello-bench
//
// A standalone, criterion-free benchmark harness that renders 5 fixed scenes
// with `vello_cpu` 0.2, so the numbers can be compared 1:1 against nitro's own
// CPU rasterizer. Anything nitro's harness needs to mirror is documented here.
//
// ---------------------------------------------------------------------------
// TARGET / PIXEL FORMAT
// ---------------------------------------------------------------------------
// Render target: 1920x1080, premultiplied RGBA8 (`vello_cpu::Pixmap`, which is
// `PremulRgba8`; `PixelFormat::Rgba8` is the only format vello_cpu 0.2 offers).
// Rasterization uses `RenderMode::OptimizeSpeed` (u8/u16 pipeline) and
// `CompositeMode::Replace`, which is what vello_cpu documents as the fast path.
// Single-threaded: the `multithreading` cargo feature is deliberately OFF, so
// `RenderSettings::num_threads == 0` and the single-threaded dispatcher is used.
// SIMD level is auto-detected by vello_cpu (`Level::try_detect()`).
//
// ---------------------------------------------------------------------------
// WHAT IS INSIDE THE TIMED REGION
// ---------------------------------------------------------------------------
// INSIDE (per iteration):
//   * `RenderContext::reset()`             - drop last frame's scene
//   * building the whole draw list         - path construction, PRNG, paints,
//                                            clips, culling decisions
//   * `RenderContext::flush()`             - finish coarse rasterization
//   * `RenderContext::render(&mut pixmap)` - fine rasterization + compositing
//                                            of final pixels into the pixmap
// OUTSIDE (one-time setup):
//   * `Pixmap` allocation (1920x1080)
//   * `RenderContext` allocation
//   * `Resources` allocation
//   * the 64x64 source image for scene (d)
//   * the gradient *stop list* is rebuilt per iteration (it is part of the
//     draw list), but the source pixmap for (d) is not.
// Rebuilding the scene every iteration is intentional: a real compositor
// rebuilds the damaged region's draw list every frame.
//
// ---------------------------------------------------------------------------
// PRNG (must match nitro's harness exactly)
// ---------------------------------------------------------------------------
// xorshift64, seeded with the constant 0x2545F491_4F6CDD1D. Step:
//     x ^= x << 13; x ^= x >> 7; x ^= x << 17;   (all on u64, wrapping)
// and the *post-step* value of x is the output word. Each scene re-seeds with
// the same constant at the start of every iteration, so every iteration draws
// the byte-identical scene and iterations are directly comparable.
// Helpers: `next_u32()` = low 32 bits of the output word;
//          `next_range(n)` = `next_u32() % n` (biased, but deterministic and
//          trivially reproducible in any language).
//
// ---------------------------------------------------------------------------
// SCENES
// ---------------------------------------------------------------------------
// (a) solid_fill   : fill the full 1920x1080 with opaque #202428.
//
// (b) rrects_alpha : opaque #202428 background, then 1000 rounded rects, each
//                    64x32 with corner radius 6, alpha 128/255 (0.5), at
//                    pseudo-random positions fully inside 1920x1080, with
//                    pseudo-random RGB.
//
// (c) gradient     : full-screen vertical linear gradient, opaque, from
//                    (0,0) #101820 to (0,1080) #E0D8C0, Extend::Pad.
//
// (d) blits        : a 64x64 straight-alpha ARGB source image is built once
//                    outside the timed loop (checkerboard-ish pattern with a
//                    varying alpha ramp, so alpha actually matters). Per
//                    iteration: fill the background opaque once, then draw the
//                    image 200 times scaled 1.5x (destination 96x96) at
//                    pseudo-random positions, with bilinear filtering
//                    (`ImageQuality::Medium` == bilinear in vello_cpu).
//
// (e) ui_frame     : 20 damage rectangles of 200x150 spread over the screen.
//                    One iteration == one full frame == all 20 damage rects.
//                    For each damage rect: push it as a clip path, then draw a
//                    "window" scene of 20 windows. Each window is
//                      - 1 opaque background rounded rect, 600x400, radius 8,
//                        at a pseudo-random position,
//                      - 30 small rounded rects 64x32 radius 6, alpha 128,
//                        at random positions inside the window,
//                      - 10 "borders": stroked rounded rects, 1.5px stroke,
//                        inset inside the window shape.
//                    CULLING: yes. Every primitive's bounding box is tested
//                    against the current damage rect and skipped if disjoint
//                    (whole windows are culled before their children are even
//                    generated -- but the PRNG is still advanced identically,
//                    so the *scene* is culling-independent). This is what a
//                    compositor does.
//
// ---------------------------------------------------------------------------
// OUTPUT
// ---------------------------------------------------------------------------
// One line per scene:
//   scene a  solid_fill  min=1.234ms  median=1.250ms  iters=100
// With `--json`, one JSON object per line:
//   {"scene":"a","name":"solid_fill","iters":100,"min_us":1234.5,"median_us":...}
// Iteration count: per-scene default (calibrated so each scene runs ~1-3 s),
// overridden by `--iters N` / first positional arg / env `BENCH_ITERS`.
// Warmup is always 3 iterations and is not recorded.

use std::sync::Arc;
use std::time::{Duration, Instant};

use vello_cpu::color::{AlphaColor, PremulRgba8, Srgb};
use vello_cpu::kurbo::{Affine, BezPath, Point, Rect, RoundedRect, Shape, Stroke};
use vello_cpu::peniko::{
    ColorStop, ColorStops, Extend, Gradient, ImageQuality, ImageSampler, LinearGradientPosition,
};
use vello_cpu::{Image, ImageSource, Pixmap, RenderContext, Resources};

const WIDTH: u16 = 1920;
const HEIGHT: u16 = 1080;
const WIDTH_F: f64 = WIDTH as f64;
const HEIGHT_F: f64 = HEIGHT as f64;

/// Tolerance used when converting shapes to paths (kurbo flattening accuracy).
const PATH_TOL: f64 = 0.1;

/// Background colour used by several scenes: #202428, opaque.
const BG: AlphaColor<Srgb> = AlphaColor::from_rgba8(0x20, 0x24, 0x28, 0xFF);

// ---------------------------------------------------------------------------
// Deterministic PRNG: xorshift64.
// ---------------------------------------------------------------------------

const SEED: u64 = 0x2545_F491_4F6C_DD1D;

struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Self(SEED)
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    /// Uniform-ish in `[0, n)`. Modulo bias is accepted: determinism and
    /// trivial reimplementability matter more than distribution purity here.
    #[inline]
    fn next_range(&mut self, n: u32) -> u32 {
        self.next_u32() % n
    }

    #[inline]
    fn next_f64(&mut self, n: f64) -> f64 {
        f64::from(self.next_range(n.max(1.0) as u32))
    }
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

#[inline]
fn rrect_path(x: f64, y: f64, w: f64, h: f64, r: f64) -> BezPath {
    RoundedRect::new(x, y, x + w, y + h, r).to_path(PATH_TOL)
}

/// Bounding-box overlap test used for damage-rect culling.
#[inline]
fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1
}

// ---------------------------------------------------------------------------
// Scene (a): solid_fill
// ---------------------------------------------------------------------------

fn scene_solid_fill(ctx: &mut RenderContext) {
    ctx.set_paint(BG);
    ctx.fill_rect(&Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F));
}

// ---------------------------------------------------------------------------
// Scene (b): rrects_alpha
// ---------------------------------------------------------------------------

const RRECT_COUNT: u32 = 1000;
const RRECT_W: f64 = 64.0;
const RRECT_H: f64 = 32.0;
const RRECT_R: f64 = 6.0;

fn scene_rrects_alpha(ctx: &mut RenderContext) {
    ctx.set_paint(BG);
    ctx.fill_rect(&Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F));

    let mut rng = Rng::new();
    for _ in 0..RRECT_COUNT {
        let x = rng.next_f64(WIDTH_F - RRECT_W);
        let y = rng.next_f64(HEIGHT_F - RRECT_H);
        let r = rng.next_range(256) as u8;
        let g = rng.next_range(256) as u8;
        let b = rng.next_range(256) as u8;
        ctx.set_paint(AlphaColor::<Srgb>::from_rgba8(r, g, b, 128));
        ctx.fill_path(&rrect_path(x, y, RRECT_W, RRECT_H, RRECT_R));
    }
}

// ---------------------------------------------------------------------------
// Scene (c): gradient
// ---------------------------------------------------------------------------

fn scene_gradient(ctx: &mut RenderContext) {
    let grad = Gradient {
        kind: LinearGradientPosition {
            start: Point::new(0.0, 0.0),
            end: Point::new(0.0, HEIGHT_F),
        }
        .into(),
        stops: ColorStops::from(
            [
                ColorStop::from((0.0, AlphaColor::<Srgb>::from_rgba8(0x10, 0x18, 0x20, 0xFF))),
                ColorStop::from((1.0, AlphaColor::<Srgb>::from_rgba8(0xE0, 0xD8, 0xC0, 0xFF))),
            ]
            .as_slice(),
        ),
        extend: Extend::Pad,
        ..Default::default()
    };
    ctx.set_paint(grad);
    ctx.fill_rect(&Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F));
}

// ---------------------------------------------------------------------------
// Scene (d): blits
// ---------------------------------------------------------------------------

const SRC_SIZE: u16 = 64;
const BLIT_COUNT: u32 = 200;
const BLIT_SCALE: f64 = 1.5;
const BLIT_DST: f64 = SRC_SIZE as f64 * BLIT_SCALE; // 96.0

/// Build the 64x64 source image once, outside the timed loop.
///
/// Conceptually straight-alpha ARGB: an 8x8 checkerboard of two colours with a
/// left-to-right alpha ramp (0..=255). vello_cpu's `Pixmap` stores premultiplied
/// RGBA8, so we premultiply here at construction time (outside the timed loop),
/// which is exactly what an upload/import step would do.
fn make_source_image() -> Image {
    let mut px = Vec::with_capacity(usize::from(SRC_SIZE) * usize::from(SRC_SIZE));
    for y in 0..SRC_SIZE {
        for x in 0..SRC_SIZE {
            let checker = ((x / 8) + (y / 8)) % 2 == 0;
            let (r, g, b) = if checker {
                (0xE0_u8, 0x50_u8, 0x30_u8)
            } else {
                (0x30_u8, 0x80_u8, 0xE0_u8)
            };
            // Alpha ramp across the width: straight alpha 0..=255.
            let a = ((u32::from(x) * 255) / u32::from(SRC_SIZE - 1)) as u8;
            let m = |c: u8| ((u16::from(c) * u16::from(a)) / 255) as u8;
            px.push(PremulRgba8 {
                r: m(r),
                g: m(g),
                b: m(b),
                a,
            });
        }
    }
    let pixmap = Pixmap::from_parts_with_opacity(px, SRC_SIZE, SRC_SIZE, true);

    Image {
        image: ImageSource::Pixmap(Arc::new(pixmap)),
        sampler: ImageSampler {
            x_extend: Extend::Pad,
            y_extend: Extend::Pad,
            // Medium == bilinear in vello_cpu 0.2 (Low == nearest,
            // High == bicubic). Bilinear filtering is supported.
            quality: ImageQuality::Medium,
            alpha: 1.0,
        },
    }
}

fn scene_blits(ctx: &mut RenderContext, img: &Image) {
    ctx.set_paint(BG);
    ctx.fill_rect(&Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F));

    let mut rng = Rng::new();
    for _ in 0..BLIT_COUNT {
        let x = rng.next_f64(WIDTH_F - BLIT_DST);
        let y = rng.next_f64(HEIGHT_F - BLIT_DST);
        ctx.set_paint(img.clone());
        // The paint transform maps image space -> device space: scale by 1.5
        // then translate to the destination corner.
        ctx.set_paint_transform(Affine::translate((x, y)) * Affine::scale(BLIT_SCALE));
        ctx.fill_rect(&Rect::new(x, y, x + BLIT_DST, y + BLIT_DST));
    }
    ctx.reset_paint_transform();
}

// ---------------------------------------------------------------------------
// Scene (e): ui_frame
// ---------------------------------------------------------------------------

const DAMAGE_COUNT: u32 = 20;
const DAMAGE_W: f64 = 200.0;
const DAMAGE_H: f64 = 150.0;
const WINDOW_COUNT: u32 = 20;
const WINDOW_W: f64 = 600.0;
const WINDOW_H: f64 = 400.0;
const WINDOW_R: f64 = 8.0;
const WIDGETS_PER_WINDOW: u32 = 30;
const BORDERS_PER_WINDOW: u32 = 10;

/// The 20 damage rects: a 5x4 grid, spread over the screen, 200x150 each.
fn damage_rects() -> Vec<Rect> {
    let mut out = Vec::with_capacity(DAMAGE_COUNT as usize);
    for row in 0..4 {
        for col in 0..5 {
            let x = 40.0 + f64::from(col) * ((WIDTH_F - 80.0 - DAMAGE_W) / 4.0);
            let y = 30.0 + f64::from(row) * ((HEIGHT_F - 60.0 - DAMAGE_H) / 3.0);
            out.push(Rect::new(x, y, x + DAMAGE_W, y + DAMAGE_H));
        }
    }
    out
}

fn scene_ui_frame(ctx: &mut RenderContext, damage: &[Rect]) {
    for dmg in damage {
        // Set the damage rect as the clip for everything drawn below it.
        ctx.push_clip_path(&dmg.to_path(PATH_TOL));

        // The PRNG is re-seeded per damage rect so that every damage rect sees
        // the *same* window layout -- the scene is a single UI, re-clipped.
        let mut rng = Rng::new();
        for _ in 0..WINDOW_COUNT {
            let wx = rng.next_f64(WIDTH_F - WINDOW_W);
            let wy = rng.next_f64(HEIGHT_F - WINDOW_H);
            let wr = Rect::new(wx, wy, wx + WINDOW_W, wy + WINDOW_H);

            // CULL: skip the whole window (and its children) when its bbox does
            // not touch the damage rect. The PRNG is still advanced by exactly
            // the same number of steps, so the logical scene is unchanged.
            let visible = intersects(&wr, dmg);

            if visible {
                ctx.set_paint(AlphaColor::<Srgb>::from_rgba8(0x2C, 0x30, 0x36, 0xFF));
                ctx.fill_path(&rrect_path(wx, wy, WINDOW_W, WINDOW_H, WINDOW_R));
            }

            // 30 small alpha widgets inside the window.
            for _ in 0..WIDGETS_PER_WINDOW {
                let ox = rng.next_f64(WINDOW_W - RRECT_W);
                let oy = rng.next_f64(WINDOW_H - RRECT_H);
                let r = rng.next_range(256) as u8;
                let g = rng.next_range(256) as u8;
                let b = rng.next_range(256) as u8;
                if !visible {
                    continue;
                }
                let x = wx + ox;
                let y = wy + oy;
                if !intersects(&Rect::new(x, y, x + RRECT_W, y + RRECT_H), dmg) {
                    continue;
                }
                ctx.set_paint(AlphaColor::<Srgb>::from_rgba8(r, g, b, 128));
                ctx.fill_path(&rrect_path(x, y, RRECT_W, RRECT_H, RRECT_R));
            }

            // 10 stroked "borders", 1.5px wide, inset inside the window shape.
            ctx.set_stroke(Stroke::new(1.5));
            for i in 0..BORDERS_PER_WINDOW {
                let inset = 4.0 + f64::from(i) * 6.0;
                let bx = wx + inset;
                let by = wy + inset;
                let bw = WINDOW_W - 2.0 * inset;
                let bh = WINDOW_H - 2.0 * inset;
                if !visible || bw <= 0.0 || bh <= 0.0 {
                    continue;
                }
                if !intersects(&Rect::new(bx, by, bx + bw, by + bh), dmg) {
                    continue;
                }
                ctx.set_paint(AlphaColor::<Srgb>::from_rgba8(0x90, 0x98, 0xA0, 0xC0));
                ctx.stroke_path(&rrect_path(bx, by, bw, bh, WINDOW_R));
            }
        }

        ctx.pop_clip_path();
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct SceneSpec {
    letter: char,
    name: &'static str,
    /// Default iteration count, calibrated so the scene runs roughly 1-3 s on a
    /// modern desktop core.
    default_iters: u32,
}

const SCENES: [SceneSpec; 5] = [
    SceneSpec {
        letter: 'a',
        name: "solid_fill",
        default_iters: 4000,
    },
    SceneSpec {
        letter: 'b',
        name: "rrects_alpha",
        default_iters: 300,
    },
    SceneSpec {
        letter: 'c',
        name: "gradient",
        default_iters: 800,
    },
    SceneSpec {
        letter: 'd',
        name: "blits",
        default_iters: 200,
    },
    SceneSpec {
        letter: 'e',
        name: "ui_frame",
        default_iters: 100,
    },
];

const WARMUP: u32 = 3;

fn main() {
    let mut json = false;
    let mut dump = false;
    let mut iters_override: Option<u32> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            // Sanity check: render each scene once and write scene_<x>.png.
            "--dump" => dump = true,
            "--iters" => {
                i += 1;
                iters_override = args.get(i).and_then(|s| s.parse().ok());
            }
            other => {
                if let Ok(n) = other.parse::<u32>() {
                    iters_override = Some(n);
                } else {
                    eprintln!("usage: vello-bench [--json] [--dump] [--iters N | N]");
                    std::process::exit(2);
                }
            }
        }
        i += 1;
    }
    if iters_override.is_none() {
        iters_override = std::env::var("BENCH_ITERS")
            .ok()
            .and_then(|s| s.parse().ok());
    }

    // ---- one-time setup, OUTSIDE every timed region ----
    let mut pixmap = Pixmap::new(WIDTH, HEIGHT);
    let mut ctx = RenderContext::new(WIDTH, HEIGHT);
    let mut resources = Resources::new();
    let src_image = make_source_image();
    let damage = damage_rects();

    if !json {
        println!(
            "vello_cpu 0.2 bench  target={WIDTH}x{HEIGHT} RGBA8(premul)  \
             single-threaded  warmup={WARMUP}"
        );
    }

    for spec in &SCENES {
        let iters = iters_override.unwrap_or(spec.default_iters).max(20);

        let mut run = |ctx: &mut RenderContext, pixmap: &mut Pixmap| {
            // ===== TIMED REGION starts here =====
            ctx.reset();
            match spec.letter {
                'a' => scene_solid_fill(ctx),
                'b' => scene_rrects_alpha(ctx),
                'c' => scene_gradient(ctx),
                'd' => scene_blits(ctx, &src_image),
                'e' => scene_ui_frame(ctx, &damage),
                _ => unreachable!(),
            }
            ctx.flush();
            ctx.render(&mut *pixmap, &mut resources);
            // ===== TIMED REGION ends here =====
        };

        for _ in 0..WARMUP {
            run(&mut ctx, &mut pixmap);
        }

        let mut samples: Vec<Duration> = Vec::with_capacity(iters as usize);
        for _ in 0..iters {
            let t0 = Instant::now();
            run(&mut ctx, &mut pixmap);
            samples.push(t0.elapsed());
        }

        // Touch the pixmap so nothing can be optimized away.
        std::hint::black_box(pixmap.data().first());

        if dump {
            let png = pixmap.clone().into_png().expect("png encode");
            std::fs::write(format!("scene_{}.png", spec.letter), png).expect("png write");
        }

        samples.sort_unstable();
        let min_us = samples[0].as_secs_f64() * 1e6;
        let median_us = {
            let n = samples.len();
            if n % 2 == 1 {
                samples[n / 2].as_secs_f64() * 1e6
            } else {
                (samples[n / 2 - 1].as_secs_f64() + samples[n / 2].as_secs_f64()) * 0.5 * 1e6
            }
        };

        if json {
            println!(
                "{{\"scene\":\"{}\",\"name\":\"{}\",\"iters\":{},\"min_us\":{:.1},\"median_us\":{:.1}}}",
                spec.letter, spec.name, iters, min_us, median_us
            );
        } else {
            println!(
                "scene {}  {:<13} min={:.3}ms  median={:.3}ms  iters={}",
                spec.letter,
                spec.name,
                min_us / 1000.0,
                median_us / 1000.0,
                iters
            );
        }
    }
}
