//! `nitro-raster` benchmark — criterion-free.
//!
//! Five scenes, mirroring `crates/nitro-raster/compare/src/main.rs` exactly
//! (same PRNG, same geometry, same colours, same culling policy) so the two
//! numbers are comparable. Run with:
//!
//! ```text
//! cargo bench -p nitro-raster              # or: cargo run --release --bench raster
//! BENCH_ITERS=50 ./target/release/deps/raster-*  --json
//! ```
//!
//! # Target / pixel format
//!
//! 1920×1080 XRGB8888 (`[B, G, R, X]`), stride = width × 4, single-threaded,
//! no explicit SIMD (the autovectorizer sees plain `chunks_exact_mut(4)`
//! loops). Unlike the vello harness there is no intermediate premultiplied
//! RGBA8 pixmap: we paint straight into what would be the scanout buffer, so
//! no conversion pass is hidden outside the timed region.
//!
//! # What is inside the timed region
//!
//! INSIDE (per iteration): the whole frame — every `fill_rect` /
//! `stroke_rect_inside` / `blit` call, the PRNG, the culling decisions and
//! the per-damage-rect clip loop.
//!
//! OUTSIDE (one-time): the 1920×1080 back buffer, the 64×64 source image for
//! scene (d). There is no scene/draw-list object to build: the rasterizer is
//! immediate-mode, so "building the draw list" *is* the drawing.
//!
//! # PRNG
//!
//! xorshift64 seeded with `0x2545_F491_4F6C_DD1D`; step
//! `x ^= x << 13; x ^= x >> 7; x ^= x << 17`, output = post-step `x`.
//! `next_range(n) = (x as u32) % n`. Re-seeded at the start of every scene
//! iteration, so every iteration draws the identical frame.

use std::time::{Duration, Instant};

use nitro_core::{Color, IRect, Point, Rect};
use nitro_raster::{Canvas, Fill, Image, Mask, PixelFormat};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const WIDTH_F: f32 = WIDTH as f32;
const HEIGHT_F: f32 = HEIGHT as f32;
const STRIDE: u32 = WIDTH * 4;

const BG: Color = Color::rgb(0x20, 0x24, 0x28);

// ---------------------------------------------------------------------------
// PRNG
// ---------------------------------------------------------------------------

const SEED: u64 = 0x2545_F491_4F6C_DD1D;

struct Rng(u64);

impl Rng {
    fn new() -> Self {
        Self(SEED)
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x as u32
    }

    #[inline]
    fn next_range(&mut self, n: u32) -> u32 {
        self.next_u32() % n
    }

    #[inline]
    fn next_f32(&mut self, n: f32) -> f32 {
        self.next_range(n.max(1.0) as u32) as f32
    }
}

#[inline]
fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

#[allow(clippy::cast_possible_wrap)] // 1920x1080, nowhere near i32::MAX
fn full_clip() -> IRect {
    IRect::new(0, 0, WIDTH as i32, HEIGHT as i32)
}

/// (a) fill the full screen with one opaque colour.
fn scene_solid_fill(c: &mut Canvas<'_>) {
    let clip = full_clip();
    c.fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F),
        &Fill::Solid(BG),
        0.0,
        1.0,
    );
}

const RRECT_COUNT: u32 = 1000;
const RRECT_W: f32 = 64.0;
const RRECT_H: f32 = 32.0;
const RRECT_R: f32 = 6.0;

/// (b) opaque background + 1000 alpha rounded rects.
fn scene_rrects_alpha(c: &mut Canvas<'_>) {
    let clip = full_clip();
    c.fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F),
        &Fill::Solid(BG),
        0.0,
        1.0,
    );
    let mut rng = Rng::new();
    for _ in 0..RRECT_COUNT {
        let x = rng.next_f32(WIDTH_F - RRECT_W);
        let y = rng.next_f32(HEIGHT_F - RRECT_H);
        let red = rng.next_range(256) as u8;
        let green = rng.next_range(256) as u8;
        let blue = rng.next_range(256) as u8;
        c.fill_rect(
            &clip,
            &Rect::new(x, y, RRECT_W, RRECT_H),
            &Fill::Solid(Color::rgba(red, green, blue, 128)),
            RRECT_R,
            1.0,
        );
    }
}

/// (c) full-screen vertical gradient.
fn scene_gradient(c: &mut Canvas<'_>) {
    let clip = full_clip();
    c.fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F),
        &Fill::Linear {
            start: Point::new(0.0, 0.0),
            end: Point::new(0.0, HEIGHT_F),
            c0: Color::rgb(0x10, 0x18, 0x20),
            c1: Color::rgb(0xE0, 0xD8, 0xC0),
        },
        0.0,
        1.0,
    );
}

const SRC_SIZE: u32 = 64;
const SRC_STRIDE: u32 = SRC_SIZE * 4;
const BLIT_COUNT: u32 = 200;
const BLIT_SCALE: f32 = 1.5;
const BLIT_DST: f32 = SRC_SIZE as f32 * BLIT_SCALE;

/// The 64×64 straight-alpha ARGB source: 8×8 checkerboard with a horizontal
/// alpha ramp. Built once, outside the timed loop.
fn make_source_image() -> Vec<u8> {
    let mut out = vec![0u8; (SRC_STRIDE * SRC_SIZE) as usize];
    for y in 0..SRC_SIZE {
        for x in 0..SRC_SIZE {
            let o = (y * SRC_STRIDE + x * 4) as usize;
            let checker = ((x / 8) + (y / 8)) % 2 == 0;
            let (red, green, blue) = if checker {
                (0xE0u8, 0x50u8, 0x30u8)
            } else {
                (0x30u8, 0x80u8, 0xE0u8)
            };
            out[o] = blue;
            out[o + 1] = green;
            out[o + 2] = red;
            out[o + 3] = (x * 255 / (SRC_SIZE - 1)) as u8;
        }
    }
    out
}

/// (d) opaque background + 200 blits of a 64×64 ARGB image scaled 1.5×.
fn scene_blits(c: &mut Canvas<'_>, img: &Image<'_>) {
    let clip = full_clip();
    c.fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, WIDTH_F, HEIGHT_F),
        &Fill::Solid(BG),
        0.0,
        1.0,
    );
    let mut rng = Rng::new();
    for _ in 0..BLIT_COUNT {
        let x = rng.next_f32(WIDTH_F - BLIT_DST);
        let y = rng.next_f32(HEIGHT_F - BLIT_DST);
        c.blit(
            &clip,
            &Rect::new(x, y, BLIT_DST, BLIT_DST),
            img,
            &img.bounds(),
            1.0,
        );
    }
}

const GLYPH_W: u32 = 8;
const GLYPH_H: u32 = 12;
const GLYPHS_PER_RUN: i32 = 50;
const GLYPH_RUNS: u32 = 40;
const TEXT_COLOR: Color = Color::rgb(0xE0, 0xE4, 0xEC);

/// One 8×12 A8 glyph-ish mask: a rounded blob with anti-aliased edges, so the
/// coverage bytes are a realistic mix of 0, 255 and partials. Built once,
/// outside the timed loop.
fn make_glyph_mask() -> Vec<u8> {
    let mut out = vec![0u8; (GLYPH_W * GLYPH_H) as usize];
    for y in 0..GLYPH_H {
        for x in 0..GLYPH_W {
            // Distance from a vertical bar plus a bowl: enough structure to
            // give interior 255s, edge partials and exterior 0s.
            let fx = x as f32 + 0.5;
            let fy = y as f32 + 0.5;
            let bar = (fx - 2.0).abs();
            let bowl = ((fx - 4.5).powi(2) + (fy - 6.0).powi(2)).sqrt() - 3.0;
            let d = bar.min(bowl.abs());
            let cov = (1.5 - d).clamp(0.0, 1.0);
            out[(y * GLYPH_W + x) as usize] = (cov * 255.0 + 0.5) as u8;
        }
    }
    out
}

/// The positions of one run of glyphs, advancing like a line of text.
fn glyph_run_positions(run: u32) -> Vec<(i32, i32)> {
    let y = 20 + (run.cast_signed() % 20) * 24;
    (0..GLYPHS_PER_RUN).map(|i| (30 + i * 9, y)).collect()
}

/// (f) 40 runs × 50 glyphs, one [`Canvas::blit_mask`] call per glyph.
fn scene_glyphs_loop(c: &mut Canvas<'_>, mask: &Mask<'_>) {
    let clip = full_clip();
    for run in 0..GLYPH_RUNS {
        for (x, y) in glyph_run_positions(run) {
            c.blit_mask(&clip, x, y, mask, TEXT_COLOR, 1.0);
        }
    }
}

/// (g) the identical work as (f), one [`Canvas::blit_masks`] call per run.
fn scene_glyphs_batch(c: &mut Canvas<'_>, mask: &Mask<'_>) {
    let clip = full_clip();
    let mut entries: Vec<(i32, i32, Mask<'_>)> = Vec::with_capacity(GLYPHS_PER_RUN as usize);
    for run in 0..GLYPH_RUNS {
        entries.clear();
        entries.extend(
            glyph_run_positions(run)
                .into_iter()
                .map(|(x, y)| (x, y, *mask)),
        );
        c.blit_masks(&clip, TEXT_COLOR, 1.0, &entries);
    }
}

const DAMAGE_COUNT: usize = 20;
const DAMAGE_W: f32 = 200.0;
const DAMAGE_H: f32 = 150.0;
const WINDOW_COUNT: u32 = 20;
const WINDOW_W: f32 = 600.0;
const WINDOW_H: f32 = 400.0;
const WINDOW_R: f32 = 8.0;
const WIDGETS_PER_WINDOW: u32 = 30;
const BORDERS_PER_WINDOW: u32 = 10;

/// The 20 damage rects: a 5×4 grid of 200×150 rects, as in the vello harness.
fn damage_rects() -> Vec<IRect> {
    let mut out = Vec::with_capacity(DAMAGE_COUNT);
    for row in 0..4 {
        for col in 0..5 {
            let x = 40.0 + col as f32 * ((WIDTH_F - 80.0 - DAMAGE_W) / 4.0);
            let y = 30.0 + row as f32 * ((HEIGHT_F - 60.0 - DAMAGE_H) / 3.0);
            out.push(IRect::new(
                x as i32,
                y as i32,
                DAMAGE_W as i32,
                DAMAGE_H as i32,
            ));
        }
    }
    out
}

/// (e) one frame = 20 damage rects × (20 windows × (bg + 30 widgets + 10
/// borders)), with bounding-box culling against the damage rect. The PRNG
/// advances identically whether or not a primitive is culled.
fn scene_ui_frame(c: &mut Canvas<'_>, damage: &[IRect]) {
    for dmg in damage {
        let dmgf = dmg.to_rect();
        let mut rng = Rng::new();
        for _ in 0..WINDOW_COUNT {
            let wx = rng.next_f32(WIDTH_F - WINDOW_W);
            let wy = rng.next_f32(HEIGHT_F - WINDOW_H);
            let wr = Rect::new(wx, wy, WINDOW_W, WINDOW_H);
            let visible = intersects(&wr, &dmgf);
            if visible {
                c.fill_rect(
                    dmg,
                    &wr,
                    &Fill::Solid(Color::rgb(0x2C, 0x30, 0x36)),
                    WINDOW_R,
                    1.0,
                );
            }
            for _ in 0..WIDGETS_PER_WINDOW {
                let ox = rng.next_f32(WINDOW_W - RRECT_W);
                let oy = rng.next_f32(WINDOW_H - RRECT_H);
                let red = rng.next_range(256) as u8;
                let green = rng.next_range(256) as u8;
                let blue = rng.next_range(256) as u8;
                if !visible {
                    continue;
                }
                let rect = Rect::new(wx + ox, wy + oy, RRECT_W, RRECT_H);
                if !intersects(&rect, &dmgf) {
                    continue;
                }
                c.fill_rect(
                    dmg,
                    &rect,
                    &Fill::Solid(Color::rgba(red, green, blue, 128)),
                    RRECT_R,
                    1.0,
                );
            }
            for i in 0..BORDERS_PER_WINDOW {
                let inset = 4.0 + i as f32 * 6.0;
                let bw = WINDOW_W - 2.0 * inset;
                let bh = WINDOW_H - 2.0 * inset;
                if !visible || bw <= 0.0 || bh <= 0.0 {
                    continue;
                }
                let rect = Rect::new(wx + inset, wy + inset, bw, bh);
                if !intersects(&rect, &dmgf) {
                    continue;
                }
                c.stroke_rect_inside(
                    dmg,
                    &rect,
                    1.5,
                    Color::rgba(0x90, 0x98, 0xA0, 0xC0),
                    WINDOW_R,
                    1.0,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct SceneSpec {
    letter: char,
    name: &'static str,
    default_iters: u32,
}

const SCENES: [SceneSpec; 7] = [
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
    SceneSpec {
        letter: 'f',
        name: "glyphs_loop",
        default_iters: 2000,
    },
    SceneSpec {
        letter: 'g',
        name: "glyphs_batch",
        default_iters: 2000,
    },
];

const WARMUP: u32 = 3;

fn main() {
    let mut json = false;
    let mut iters_override: Option<u32> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--iters" => {
                i += 1;
                iters_override = args.get(i).and_then(|s| s.parse().ok());
            }
            // `cargo bench` passes these; ignore them.
            "--bench" | "--test" => {}
            other => {
                if let Ok(n) = other.parse::<u32>() {
                    iters_override = Some(n);
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
    let mut buf = vec![0u8; (STRIDE * HEIGHT) as usize];
    let src = make_source_image();
    let img = Image {
        data: &src,
        width: SRC_SIZE,
        height: SRC_SIZE,
        stride: SRC_STRIDE,
        format: PixelFormat::Argb8888,
    };
    let damage = damage_rects();
    let glyph = make_glyph_mask();
    let glyph_mask = Mask {
        data: &glyph,
        w: GLYPH_W,
        h: GLYPH_H,
        stride: GLYPH_W,
    };

    for spec in &SCENES {
        let iters = iters_override.unwrap_or(spec.default_iters).max(1);
        let mut times: Vec<Duration> = Vec::with_capacity(iters as usize);
        for n in 0..iters + WARMUP {
            let t0 = Instant::now();
            {
                let mut c = Canvas::new(&mut buf, WIDTH, HEIGHT, STRIDE);
                match spec.letter {
                    'a' => scene_solid_fill(&mut c),
                    'b' => scene_rrects_alpha(&mut c),
                    'c' => scene_gradient(&mut c),
                    'd' => scene_blits(&mut c, &img),
                    'e' => scene_ui_frame(&mut c, &damage),
                    'f' => scene_glyphs_loop(&mut c, &glyph_mask),
                    _ => scene_glyphs_batch(&mut c, &glyph_mask),
                }
            }
            let dt = t0.elapsed();
            if n >= WARMUP {
                times.push(dt);
            }
        }
        times.sort_unstable();
        let min = times[0].as_secs_f64() * 1e6;
        let med = times[times.len() / 2].as_secs_f64() * 1e6;
        if json {
            println!(
                "{{\"scene\":\"{}\",\"name\":\"{}\",\"iters\":{},\"min_us\":{:.1},\"median_us\":{:.1}}}",
                spec.letter, spec.name, iters, min, med
            );
        } else {
            println!(
                "scene {}  {:<13} min={:.3}ms  median={:.3}ms  iters={}",
                spec.letter,
                spec.name,
                min / 1000.0,
                med / 1000.0,
                iters
            );
        }
    }
}
