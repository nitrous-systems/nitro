//! Integration-style tests over the public surface.
//!
//! Conventions:
//! - Canvases are filled with a **sentinel** (`0xFF00FF`, magenta) so any
//!   write outside the clip is loud and obvious.
//! - The float reference implementations live here, not in the library.

use nitro_core::{Color, IRect, Point, Rect};

use crate::{BYTES_PER_PIXEL, Canvas, Fill, Image, Mask, PixelFormat};

const SENTINEL: u32 = 0x00FF_00FF;

/// `u32 -> i32` for test dimensions, which are tiny.
#[allow(clippy::cast_possible_wrap)] // test canvases are at most a few hundred px
fn iw(v: u32) -> i32 {
    v as i32
}

/// A test surface with a deliberately padded stride.
struct Surface {
    data: Vec<u8>,
    w: u32,
    h: u32,
    stride: u32,
}

impl Surface {
    fn new(w: u32, h: u32) -> Self {
        // Pad the stride so stride bugs surface, like the fake KMS backend.
        let stride = (w * BYTES_PER_PIXEL as u32).div_ceil(64) * 64;
        let mut s = Self {
            data: vec![0; (stride * h) as usize],
            w,
            h,
            stride,
        };
        s.fill_sentinel();
        s
    }

    fn fill_sentinel(&mut self) {
        let px = SENTINEL.to_le_bytes();
        for d in self.data.chunks_exact_mut(4) {
            d.copy_from_slice(&px);
        }
    }

    fn canvas(&mut self) -> Canvas<'_> {
        Canvas::new(&mut self.data, self.w, self.h, self.stride)
    }

    fn px(&self, x: i32, y: i32) -> u32 {
        let o = y as usize * self.stride as usize + x as usize * BYTES_PER_PIXEL;
        u32::from_le_bytes([self.data[o], self.data[o + 1], self.data[o + 2], 0])
    }

    /// `(b, g, r)` of the pixel.
    fn bgr(&self, x: i32, y: i32) -> (u8, u8, u8) {
        let v = self.px(x, y);
        (v as u8, (v >> 8) as u8, (v >> 16) as u8)
    }

    /// Assert nothing outside `clip` was written.
    fn assert_untouched_outside(&self, clip: &IRect) {
        for y in 0..iw(self.h) {
            for x in 0..iw(self.w) {
                if !clip.contains(x, y) {
                    assert_eq!(
                        self.px(x, y),
                        SENTINEL,
                        "pixel ({x}, {y}) outside clip {clip:?} was written"
                    );
                }
            }
        }
    }
}

fn rgb(c: Color) -> (u8, u8, u8) {
    (c.b, c.g, c.r)
}

// ---------------------------------------------------------------------------
// xorshift64 PRNG — same generator as the benchmark and the vello harness.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x as u32
    }
    fn byte(&mut self) -> u8 {
        (self.next_u32() % 256) as u8
    }
}

// ---------------------------------------------------------------------------
// fill_irect
// ---------------------------------------------------------------------------

#[test]
fn fill_irect_is_pixel_exact_and_clipped() {
    let mut s = Surface::new(32, 16);
    let clip = IRect::new(4, 2, 10, 8);
    let c = Color::rgb(0x12, 0x34, 0x56);
    s.canvas().fill_irect(&clip, &IRect::new(0, 0, 32, 16), c);
    for y in 0..16 {
        for x in 0..32 {
            if clip.contains(x, y) {
                assert_eq!(s.bgr(x, y), rgb(c), "({x},{y})");
            }
        }
    }
    s.assert_untouched_outside(&clip);
}

#[test]
fn fill_irect_translucent_blends() {
    let mut s = Surface::new(8, 4);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas()
        .fill_irect(&clip, &clip, Color::rgba(255, 255, 255, 128));
    // round((255*128 + 0*127)/255) = 128
    assert_eq!(s.bgr(0, 0), (128, 128, 128));
}

#[test]
fn fill_irect_ignores_transparent_and_empty() {
    let mut s = Surface::new(8, 4);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::TRANSPARENT);
    s.canvas()
        .fill_irect(&clip, &IRect::new(0, 0, 0, 4), Color::WHITE);
    s.canvas()
        .fill_irect(&IRect::new(100, 100, 4, 4), &clip, Color::WHITE);
    s.assert_untouched_outside(&IRect::EMPTY);
}

// ---------------------------------------------------------------------------
// fill_rect: interiors, AA, clipping
// ---------------------------------------------------------------------------

#[test]
fn opaque_fill_rect_interior_is_exact() {
    let mut s = Surface::new(24, 12);
    let clip = s.canvas().bounds();
    let c = Color::rgb(0xAB, 0xCD, 0xEF);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(4.0, 3.0, 10.0, 6.0),
        &Fill::Solid(c),
        0.0,
        1.0,
    );
    for y in 3..9 {
        for x in 4..14 {
            assert_eq!(s.bgr(x, y), rgb(c), "({x},{y})");
        }
    }
    // One pixel outside every edge is untouched.
    for y in 3..9 {
        assert_eq!(s.px(3, y), SENTINEL);
        assert_eq!(s.px(14, y), SENTINEL);
    }
    for x in 4..14 {
        assert_eq!(s.px(x, 2), SENTINEL);
        assert_eq!(s.px(x, 9), SENTINEL);
    }
}

#[test]
fn fill_rect_never_writes_outside_clip() {
    let mut s = Surface::new(40, 24);
    let clip = IRect::new(10, 6, 12, 9);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(-5.5, -3.25, 60.0, 40.0),
        &Fill::Solid(Color::rgba(0x10, 0x20, 0x30, 200)),
        7.0,
        0.75,
    );
    s.assert_untouched_outside(&clip);
    // And something inside actually got painted.
    assert_ne!(s.px(15, 10), SENTINEL);
}

#[test]
fn stroke_never_writes_outside_clip() {
    let mut s = Surface::new(40, 24);
    let clip = IRect::new(3, 3, 20, 14);
    s.canvas().stroke_rect_inside(
        &clip,
        &Rect::new(-2.0, -2.0, 60.0, 40.0),
        3.0,
        Color::rgba(0xFF, 0, 0, 128),
        5.0,
        1.0,
    );
    s.assert_untouched_outside(&clip);
}

#[test]
fn blit_never_writes_outside_clip() {
    let src = checker_image(16, 16, PixelFormat::Argb8888);
    let img = Image {
        data: &src,
        width: 16,
        height: 16,
        stride: 64,
        format: PixelFormat::Argb8888,
    };
    let mut s = Surface::new(40, 24);
    let clip = IRect::new(5, 5, 9, 7);
    s.canvas().blit(
        &clip,
        &Rect::new(-3.5, -1.5, 50.0, 30.0),
        &img,
        &img.bounds(),
        1.0,
    );
    s.assert_untouched_outside(&clip);
}

#[test]
fn half_pixel_edge_is_fifty_percent_coverage() {
    let mut s = Surface::new(16, 8);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    // Left edge at x = 0.5: column 0 gets 50 % of white.
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.5, 0.0, 8.0, 8.0),
        &Fill::Solid(Color::WHITE),
        0.0,
        1.0,
    );
    let (b, g, r) = s.bgr(0, 3);
    for v in [b, g, r] {
        assert!(v.abs_diff(128) <= 1, "edge column got {v}, want ~128");
    }
    assert_eq!(s.bgr(1, 3), (255, 255, 255));
    // Right edge at 8.5 -> column 8 is also half.
    let (b, _, _) = s.bgr(8, 3);
    assert!(b.abs_diff(128) <= 1, "right edge column got {b}");
    assert_eq!(s.px(9, 3), 0);
}

#[test]
fn quarter_pixel_edges_scale_linearly() {
    for (off, want) in [(0.25_f32, 191u8), (0.5, 128), (0.75, 64)] {
        let mut s = Surface::new(8, 4);
        let clip = s.canvas().bounds();
        s.canvas().fill_irect(&clip, &clip, Color::BLACK);
        s.canvas().fill_rect(
            &clip,
            &Rect::new(off, 0.0, 4.0, 4.0),
            &Fill::Solid(Color::WHITE),
            0.0,
            1.0,
        );
        let (b, _, _) = s.bgr(0, 1);
        assert!(b.abs_diff(want) <= 1, "off {off}: got {b}, want {want}");
    }
}

#[test]
fn rounded_rect_corner_is_empty_and_inside_is_full() {
    let mut s = Surface::new(48, 32);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 40.0, 24.0),
        &Fill::Solid(Color::WHITE),
        8.0,
        1.0,
    );
    // Extreme corner pixels are (almost) untouched by an r=8 corner.
    for (x, y) in [(0, 0), (39, 0), (0, 23), (39, 23)] {
        let (b, _, _) = s.bgr(x, y);
        assert!(b <= 4, "corner ({x},{y}) has coverage {b}");
    }
    // Inscribed area is fully covered.
    for (x, y) in [(20, 0), (20, 23), (0, 12), (39, 12), (8, 8)] {
        assert_eq!(s.bgr(x, y), (255, 255, 255), "({x},{y})");
    }
}

#[test]
fn rounded_corners_are_symmetric_in_pixels() {
    let mut s = Surface::new(64, 40);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 64.0, 40.0),
        &Fill::Solid(Color::WHITE),
        10.0,
        1.0,
    );
    for y in 0..10 {
        for x in 0..10 {
            let tl = s.bgr(x, y).0;
            let tr = s.bgr(63 - x, y).0;
            let bl = s.bgr(x, 39 - y).0;
            let br = s.bgr(63 - x, 39 - y).0;
            assert_eq!(tl, tr, "h mirror at ({x},{y})");
            assert_eq!(tl, bl, "v mirror at ({x},{y})");
            assert_eq!(tl, br, "diag mirror at ({x},{y})");
        }
    }
}

#[test]
fn zero_radius_matches_fill_irect_on_integer_bounds() {
    let c = Color::rgb(9, 88, 200);
    let mut a = Surface::new(20, 12);
    let mut b = Surface::new(20, 12);
    let clip = IRect::new(0, 0, 20, 12);
    a.canvas().fill_irect(&clip, &IRect::new(3, 2, 9, 7), c);
    b.canvas().fill_rect(
        &clip,
        &Rect::new(3.0, 2.0, 9.0, 7.0),
        &Fill::Solid(c),
        0.0,
        1.0,
    );
    assert_eq!(a.data, b.data);
}

#[test]
fn opacity_scales_the_fill() {
    let mut s = Surface::new(8, 4);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 8.0, 4.0),
        &Fill::Solid(Color::WHITE),
        0.0,
        0.5,
    );
    let (b, _, _) = s.bgr(4, 2);
    assert!(b.abs_diff(128) <= 1, "got {b}");
    // Zero opacity paints nothing.
    let mut s = Surface::new(8, 4);
    let clip = s.canvas().bounds();
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 8.0, 4.0),
        &Fill::Solid(Color::WHITE),
        0.0,
        0.0,
    );
    s.assert_untouched_outside(&IRect::EMPTY);
}

#[test]
fn degenerate_shapes_paint_nothing() {
    let mut s = Surface::new(8, 4);
    let clip = s.canvas().bounds();
    for r in [
        Rect::new(2.0, 2.0, 0.0, 3.0),
        Rect::new(2.0, 2.0, 3.0, 0.0),
        Rect::new(2.0, 2.0, -3.0, 3.0),
    ] {
        s.canvas()
            .fill_rect(&clip, &r, &Fill::Solid(Color::WHITE), 2.0, 1.0);
    }
    s.assert_untouched_outside(&IRect::EMPTY);
}

// ---------------------------------------------------------------------------
// Blending vs a float reference
// ---------------------------------------------------------------------------

/// Float source-over reference, straight alpha, no gamma.
fn reference_over(src: Color, dst: (u8, u8, u8), alpha_scale: f32) -> (u8, u8, u8) {
    let a = f32::from(src.a) / 255.0 * alpha_scale;
    let f = |s: u8, d: u8| {
        let v = f32::from(s) * a + f32::from(d) * (1.0 - a);
        v.round().clamp(0.0, 255.0) as u8
    };
    (f(src.b, dst.0), f(src.g, dst.1), f(src.r, dst.2))
}

#[test]
fn solid_blending_matches_float_reference() {
    let mut rng = Rng::new(0x2545_F491_4F6C_DD1D);
    for _ in 0..2000 {
        let dst = Color::rgb(rng.byte(), rng.byte(), rng.byte());
        let src = Color::rgba(rng.byte(), rng.byte(), rng.byte(), rng.byte());
        let mut s = Surface::new(4, 2);
        let clip = s.canvas().bounds();
        s.canvas().fill_irect(&clip, &clip, dst);
        s.canvas().fill_rect(
            &clip,
            &Rect::new(0.0, 0.0, 4.0, 2.0),
            &Fill::Solid(src),
            0.0,
            1.0,
        );
        let got = s.bgr(2, 1);
        let want = reference_over(src, rgb(dst), 1.0);
        for (g, w) in [(got.0, want.0), (got.1, want.1), (got.2, want.2)] {
            assert!(
                g.abs_diff(w) <= 1,
                "src {src:?} dst {dst:?}: got {got:?} want {want:?}"
            );
        }
    }
}

#[test]
fn opacity_blending_matches_float_reference() {
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    for _ in 0..1000 {
        let dst = Color::rgb(rng.byte(), rng.byte(), rng.byte());
        let src = Color::rgba(rng.byte(), rng.byte(), rng.byte(), rng.byte());
        let op_u8 = rng.byte();
        let op = f32::from(op_u8) / 255.0;
        let mut s = Surface::new(4, 2);
        let clip = s.canvas().bounds();
        s.canvas().fill_irect(&clip, &clip, dst);
        s.canvas().fill_rect(
            &clip,
            &Rect::new(0.0, 0.0, 4.0, 2.0),
            &Fill::Solid(src),
            0.0,
            op,
        );
        let got = s.bgr(2, 1);
        // The library quantises opacity to a byte first; do the same.
        let want = reference_over(src, rgb(dst), f32::from(super::blend::unit_u8(op)) / 255.0);
        for (g, w) in [(got.0, want.0), (got.1, want.1), (got.2, want.2)] {
            assert!(
                g.abs_diff(w) <= 1,
                "src {src:?} dst {dst:?} op {op_u8}: got {got:?} want {want:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Gradients
// ---------------------------------------------------------------------------

#[test]
fn vertical_gradient_endpoints_and_midpoint() {
    let h: u32 = 100;
    let mut s = Surface::new(4, h);
    let clip = s.canvas().bounds();
    let c0 = Color::rgb(0, 0, 0);
    let c1 = Color::rgb(200, 100, 50);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 4.0, h as f32),
        &Fill::Linear {
            start: Point::new(0.0, 0.0),
            end: Point::new(0.0, h as f32),
            c0,
            c1,
        },
        0.0,
        1.0,
    );
    // Row 0's centre is y=0.5 -> t = 0.005; row h-1 -> t = 0.995.
    let top = s.bgr(2, 0);
    let bot = s.bgr(2, iw(h) - 1);
    assert!(top.2 <= 2 && top.1 <= 1, "top {top:?}");
    assert!(bot.2 >= 198 && bot.1 >= 99, "bottom {bot:?}");
    // Midpoint (t = 0.5) is the exact average.
    let mid = s.bgr(2, 50);
    assert!(mid.2.abs_diff(100) <= 2, "mid r {}", mid.2);
    assert!(mid.1.abs_diff(50) <= 2, "mid g {}", mid.1);
    assert!(mid.0.abs_diff(25) <= 2, "mid b {}", mid.0);
}

#[test]
fn gradient_endpoints_are_exact_when_sampled_at_the_ends() {
    // Place the endpoints at pixel centres so t hits exactly 0 and 1.
    let mut s = Surface::new(8, 2);
    let clip = s.canvas().bounds();
    let c0 = Color::rgb(10, 20, 30);
    let c1 = Color::rgb(240, 230, 220);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 8.0, 2.0),
        &Fill::Linear {
            start: Point::new(0.5, 0.0),
            end: Point::new(7.5, 0.0),
            c0,
            c1,
        },
        0.0,
        1.0,
    );
    assert_eq!(s.bgr(0, 0), rgb(c0));
    assert_eq!(s.bgr(7, 0), rgb(c1));
    // Exact midpoint of the axis is column 3.5 -> columns 3 and 4 straddle it.
    let a = s.bgr(3, 0).2;
    let b = s.bgr(4, 0).2;
    assert!(a < 125 && b > 125, "straddle {a} {b}");
}

#[test]
fn gradient_pads_outside_the_axis() {
    let mut s = Surface::new(16, 2);
    let clip = s.canvas().bounds();
    let c0 = Color::rgb(0, 0, 0);
    let c1 = Color::WHITE;
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 16.0, 2.0),
        &Fill::Linear {
            start: Point::new(4.0, 0.0),
            end: Point::new(8.0, 0.0),
            c0,
            c1,
        },
        0.0,
        1.0,
    );
    assert_eq!(s.bgr(0, 0), (0, 0, 0));
    assert_eq!(s.bgr(15, 0), (255, 255, 255));
}

#[test]
fn translucent_gradient_blends() {
    let mut s = Surface::new(8, 2);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().fill_rect(
        &clip,
        &Rect::new(0.0, 0.0, 8.0, 2.0),
        &Fill::Linear {
            start: Point::new(0.0, 0.0),
            end: Point::new(0.0, 2.0),
            c0: Color::rgba(255, 255, 255, 128),
            c1: Color::rgba(255, 255, 255, 128),
        },
        0.0,
        1.0,
    );
    let (b, _, _) = s.bgr(4, 1);
    assert!(b.abs_diff(128) <= 1, "got {b}");
}

// ---------------------------------------------------------------------------
// Strokes
// ---------------------------------------------------------------------------

#[test]
fn stroke_stays_inside_and_leaves_a_hole() {
    let mut s = Surface::new(24, 16);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().stroke_rect_inside(
        &clip,
        &Rect::new(2.0, 2.0, 16.0, 10.0),
        2.0,
        Color::WHITE,
        0.0,
        1.0,
    );
    // Border band.
    assert_eq!(s.bgr(2, 2), (255, 255, 255));
    assert_eq!(s.bgr(3, 6), (255, 255, 255));
    // Interior hole untouched.
    assert_eq!(s.bgr(8, 6), (0, 0, 0));
    assert_eq!(s.bgr(4, 4), (0, 0, 0));
    // Outside the rect untouched.
    assert_eq!(s.bgr(1, 6), (0, 0, 0));
    assert_eq!(s.bgr(18, 6), (0, 0, 0));
}

#[test]
fn stroke_wider_than_the_rect_fills_it() {
    let mut s = Surface::new(16, 12);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().stroke_rect_inside(
        &clip,
        &Rect::new(2.0, 2.0, 8.0, 6.0),
        20.0,
        Color::WHITE,
        0.0,
        1.0,
    );
    for y in 2..8 {
        for x in 2..10 {
            assert_eq!(s.bgr(x, y), (255, 255, 255), "({x},{y})");
        }
    }
}

#[test]
fn stroke_does_not_double_blend_translucently() {
    let mut s = Surface::new(16, 12);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().stroke_rect_inside(
        &clip,
        &Rect::new(0.0, 0.0, 16.0, 12.0),
        3.0,
        Color::rgba(255, 255, 255, 128),
        4.0,
        1.0,
    );
    // Every pixel of the band is exactly one 50 % white over black.
    for x in 4..12 {
        let (b, _, _) = s.bgr(x, 1);
        assert!(b.abs_diff(128) <= 2, "col {x} got {b}");
    }
}

#[test]
fn stroke_hollow_early_out_is_exact() {
    // A thin border in a large rect: the early-out must skip only rows that
    // truly paint nothing. Compare against a reference that paints the same
    // ring one damage-rect row at a time (which takes a different path).
    let mut a = Surface::new(48, 40);
    let mut b = Surface::new(48, 40);
    let clip = IRect::new(0, 0, 48, 40);
    a.canvas().fill_irect(&clip, &clip, Color::BLACK);
    b.canvas().fill_irect(&clip, &clip, Color::BLACK);
    let rect = Rect::new(3.5, 2.5, 40.0, 33.0);
    a.canvas()
        .stroke_rect_inside(&clip, &rect, 1.5, Color::rgba(9, 200, 30, 190), 7.0, 1.0);
    for y in 0..40 {
        b.canvas().stroke_rect_inside(
            &IRect::new(0, y, 48, 1),
            &rect,
            1.5,
            Color::rgba(9, 200, 30, 190),
            7.0,
            1.0,
        );
    }
    assert_eq!(
        a.data, b.data,
        "row-clipped stroke differs from full stroke"
    );
    // And the hollow middle really is untouched.
    assert_eq!(a.bgr(24, 20), (0, 0, 0));
}

#[test]
fn stroke_fast_path_is_byte_identical_to_the_general_walk() {
    // The straight-row fast path resolves the two bands once instead of
    // walking two coverages per row. It must be *exactly* the general path,
    // not merely close: a border that shifts by one level between the corner
    // rows and the straight ones is a visible seam. Sweep geometry that
    // exercises fractional edges, fractional widths, every radius regime
    // (sharp, small, larger than the width, clamped) and clips that cut the
    // bands in half.
    let mut cases = 0;
    for x in [0.0_f32, 0.25, 0.5, 3.5, 7.75] {
        for radius in [0.0_f32, 1.0, 4.0, 7.5, 60.0] {
            for width in [0.5_f32, 1.0, 1.5, 3.0, 9.0] {
                for opacity in [1.0_f32, 0.35] {
                    for clip in [
                        IRect::new(0, 0, 48, 40),
                        IRect::new(6, 4, 20, 30),
                        IRect::new(0, 9, 48, 3),
                        IRect::new(30, 0, 5, 40),
                    ] {
                        let rect = Rect::new(x, x * 0.5, 38.0, 31.5);
                        let color = Color::rgba(9, 200, 30, 190);
                        let mut fast = Surface::new(48, 40);
                        let mut slow = Surface::new(48, 40);
                        let all = IRect::new(0, 0, 48, 40);
                        fast.canvas().fill_irect(&all, &all, Color::BLACK);
                        slow.canvas().fill_irect(&all, &all, Color::BLACK);
                        fast.canvas()
                            .stroke_rect_inside(&clip, &rect, width, color, radius, opacity);
                        slow.canvas().stroke_rect_inside_general(
                            &clip, &rect, width, color, radius, opacity,
                        );
                        assert_eq!(
                            fast.data, slow.data,
                            "x={x} r={radius} w={width} op={opacity} clip={clip:?}"
                        );
                        cases += 1;
                    }
                }
            }
        }
    }
    assert_eq!(cases, 5 * 5 * 5 * 2 * 4);
}

#[test]
fn stroke_fast_path_still_blends_each_pixel_once() {
    // The band pair must not overlap: the straight rows of a translucent
    // border are exactly one blend deep, same as the corner rows.
    let mut s = Surface::new(32, 24);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().stroke_rect_inside(
        &clip,
        &Rect::new(0.0, 0.0, 32.0, 24.0),
        2.0,
        Color::rgba(255, 255, 255, 128),
        6.0,
        1.0,
    );
    // Row 12 is a straight row (r = 6, so rows 6..18 are corner-free).
    for x in [0, 1, 30, 31] {
        let (b, _, _) = s.bgr(x, 12);
        assert!(b.abs_diff(128) <= 1, "col {x} got {b}, want one 50 % blend");
    }
    // And the hole between the bands is untouched.
    assert_eq!(s.bgr(16, 12), (0, 0, 0));
}

#[test]
fn zero_width_stroke_paints_nothing() {
    let mut s = Surface::new(8, 8);
    let clip = s.canvas().bounds();
    s.canvas().stroke_rect_inside(
        &clip,
        &Rect::new(1.0, 1.0, 6.0, 6.0),
        0.0,
        Color::WHITE,
        0.0,
        1.0,
    );
    s.assert_untouched_outside(&IRect::EMPTY);
}

// ---------------------------------------------------------------------------
// Blits
// ---------------------------------------------------------------------------

/// An `8×8`-checkered image with a horizontal alpha ramp, straight alpha.
fn checker_image(width: u32, height: u32, format: PixelFormat) -> Vec<u8> {
    let stride = (width * 4).div_ceil(64) * 64;
    let mut out = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let o = (y * stride + x * 4) as usize;
            let checker = ((x / 8) + (y / 8)) % 2 == 0;
            let (red, green, blue) = if checker {
                (0xE0, 0x50, 0x30)
            } else {
                (0x30, 0x80, 0xE0)
            };
            let alpha = if width > 1 {
                (x * 255 / (width - 1)) as u8
            } else {
                255
            };
            out[o] = blue;
            out[o + 1] = green;
            out[o + 2] = red;
            out[o + 3] = if format == PixelFormat::Argb8888 {
                alpha
            } else {
                0
            };
        }
    }
    out
}

fn image_of(data: &[u8], w: u32, h: u32, format: PixelFormat) -> Image<'_> {
    Image {
        data,
        width: w,
        height: h,
        stride: (w * 4).div_ceil(64) * 64,
        format,
    }
}

#[test]
fn blit_one_to_one_is_exact() {
    let data = checker_image(16, 16, PixelFormat::Xrgb8888);
    let img = image_of(&data, 16, 16, PixelFormat::Xrgb8888);
    let mut s = Surface::new(32, 32);
    let clip = s.canvas().bounds();
    s.canvas().blit(
        &clip,
        &Rect::new(4.0, 5.0, 16.0, 16.0),
        &img,
        &img.bounds(),
        1.0,
    );
    for y in 0..16 {
        for x in 0..16 {
            let o = (y * img.stride + x * 4) as usize;
            assert_eq!(
                s.bgr(4 + iw(x), 5 + iw(y)),
                (data[o], data[o + 1], data[o + 2]),
                "({x},{y})"
            );
        }
    }
    // Outside the destination: untouched.
    assert_eq!(s.px(3, 5), SENTINEL);
    assert_eq!(s.px(20, 5), SENTINEL);
}

#[test]
fn blit_sub_rect_one_to_one_is_exact() {
    let data = checker_image(16, 16, PixelFormat::Xrgb8888);
    let img = image_of(&data, 16, 16, PixelFormat::Xrgb8888);
    let mut s = Surface::new(24, 24);
    let clip = s.canvas().bounds();
    let sr = IRect::new(4, 6, 8, 5);
    s.canvas()
        .blit(&clip, &Rect::new(2.0, 3.0, 8.0, 5.0), &img, &sr, 1.0);
    for y in 0..5 {
        for x in 0..8 {
            let o = ((y + 6) * img.stride + (x + 4) * 4) as usize;
            assert_eq!(
                s.bgr(2 + iw(x), 3 + iw(y)),
                (data[o], data[o + 1], data[o + 2]),
                "({x},{y})"
            );
        }
    }
}

#[test]
fn blit_one_to_one_handles_odd_widths_and_offsets() {
    // The opaque 1:1 row loop stores two pixels at a time, so a row with an
    // odd pixel count (or an odd x offset, which shifts where the pairs
    // start) exercises the 4-byte tail. A miss here is silent corruption of
    // the last column, so pin it at several widths and offsets.
    let data = checker_image(16, 16, PixelFormat::Xrgb8888);
    let img = image_of(&data, 16, 16, PixelFormat::Xrgb8888);
    for (w, wf) in [
        (1_u32, 1.0_f32),
        (2, 2.0),
        (3, 3.0),
        (7, 7.0),
        (9, 9.0),
        (15, 15.0),
    ] {
        for (ox, oxf) in [(0_u32, 0.0_f32), (1, 1.0), (3, 3.0)] {
            if ox + w > img.width {
                continue; // the source rect must stay inside the image
            }

            let mut s = Surface::new(32, 8);
            let clip = s.canvas().bounds();
            let sr = IRect::new(iw(ox), 1, iw(w), 4);
            s.canvas()
                .blit(&clip, &Rect::new(oxf, 2.0, wf, 4.0), &img, &sr, 1.0);
            for y in 0..4 {
                for x in 0..w {
                    let o = ((y + 1) * img.stride + (x + ox) * 4) as usize;
                    assert_eq!(
                        s.bgr(iw(ox + x), 2 + iw(y)),
                        (data[o], data[o + 1], data[o + 2]),
                        "w={w} ox={ox} ({x},{y})"
                    );
                    // Byte 3 is always stored as 255, whatever the source held.
                    let d = (y as usize + 2) * s.stride as usize + (ox + x) as usize * 4 + 3;
                    assert_eq!(s.data[d], 255, "w={w} ox={ox} alpha at ({x},{y})");
                }
            }
            // The pixel just past the run is untouched.
            assert_eq!(s.px(iw(ox + w), 2), SENTINEL, "w={w} ox={ox}");
        }
    }
}

#[test]
fn blit_one_to_one_forces_alpha_255_for_an_xrgb_source() {
    // An XRGB source whose byte 3 is garbage must not leak it into the
    // canvas: an opaque store writes alpha 255 (#3898).
    let stride = 64_u32;
    let mut data = vec![0u8; (stride * 4) as usize];
    for (i, px) in data.chunks_exact_mut(4).enumerate() {
        px.copy_from_slice(&[i as u8, 0x40, 0x80, (i * 7) as u8]);
    }
    let img = Image {
        data: &data,
        width: 5,
        height: 4,
        stride,
        format: PixelFormat::Xrgb8888,
    };
    let mut s = Surface::new(8, 8);
    let clip = s.canvas().bounds();
    s.canvas().blit(
        &clip,
        &Rect::new(0.0, 0.0, 5.0, 4.0),
        &img,
        &img.bounds(),
        1.0,
    );
    for y in 0..4_usize {
        for x in 0..5_usize {
            let o = y * stride as usize + x * 4;
            let d = y * s.stride as usize + x * 4;
            assert_eq!(
                &s.data[d..d + 4],
                &[data[o], data[o + 1], data[o + 2], 255],
                "({x},{y})"
            );
        }
    }
}

#[test]
fn blit_straight_alpha_source_over() {
    // A 2x1 source: left fully transparent, right 50 % red.
    let mut data = vec![0u8; 64];
    data[0..4].copy_from_slice(&[0, 0, 255, 0]); // a = 0
    data[4..8].copy_from_slice(&[0, 0, 255, 128]); // a = 128
    let img = Image {
        data: &data,
        width: 2,
        height: 1,
        stride: 64,
        format: PixelFormat::Argb8888,
    };
    let mut s = Surface::new(4, 2);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().blit(
        &clip,
        &Rect::new(0.0, 0.0, 2.0, 1.0),
        &img,
        &img.bounds(),
        1.0,
    );
    assert_eq!(s.bgr(0, 0), (0, 0, 0), "transparent texel changed the dst");
    let (_, _, r) = s.bgr(1, 0);
    assert!(r.abs_diff(128) <= 1, "50 % red over black gave {r}");
}

#[test]
fn blit_opacity_scales() {
    let mut data = vec![0u8; 64];
    data[0..4].copy_from_slice(&[0, 0, 255, 255]);
    let img = Image {
        data: &data,
        width: 1,
        height: 1,
        stride: 64,
        format: PixelFormat::Argb8888,
    };
    let mut s = Surface::new(4, 2);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().blit(
        &clip,
        &Rect::new(0.0, 0.0, 1.0, 1.0),
        &img,
        &img.bounds(),
        0.5,
    );
    let (_, _, r) = s.bgr(0, 0);
    assert!(r.abs_diff(128) <= 1, "got {r}");
}

/// Float reference for a bilinear sample of a grey ramp image.
fn reference_bilinear_grey(vals: &[u8], width: usize, height: usize, sx: f32, sy: f32) -> f32 {
    let fx = sx.floor();
    let fy = sy.floor();
    let tx = sx - fx;
    let ty = sy - fy;
    let at = |px: i32, py: i32| {
        let cx = px.clamp(0, iw(width as u32) - 1) as usize;
        let cy = py.clamp(0, iw(height as u32) - 1) as usize;
        f32::from(vals[cy * width + cx])
    };
    let (x, y) = (fx as i32, fy as i32);
    let top = at(x, y) * (1.0 - tx) + at(x + 1, y) * tx;
    let bot = at(x, y + 1) * (1.0 - tx) + at(x + 1, y + 1) * tx;
    top * (1.0 - ty) + bot * ty
}

/// Build an opaque grey image from `vals` (row-major, `w * h`).
fn grey_image(vals: &[u8], width: u32, height: u32) -> Vec<u8> {
    let stride = (width * 4).div_ceil(64) * 64;
    let mut out = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let o = (y * stride + x * 4) as usize;
            let grey = vals[(y * width + x) as usize];
            out[o] = grey;
            out[o + 1] = grey;
            out[o + 2] = grey;
            out[o + 3] = 255;
        }
    }
    out
}

#[test]
fn blit_scaled_bilinear_samples_midpoints_exactly() {
    // At 1.5x, destination pixel centre x maps to source (x + 0.5) / 1.5 - 0.5,
    // so x = 1 lands on source 0.5: the exact midpoint between texels 0 and 1,
    // and x = 4 lands on 2.5. Those pixels must be the exact averages.
    let vals: [u8; 16] = [
        0, 100, 200, 40, //
        0, 100, 200, 40, //
        0, 100, 200, 40, //
        0, 100, 200, 40,
    ];
    let data = grey_image(&vals, 4, 4);
    let img = image_of(&data, 4, 4, PixelFormat::Argb8888);
    let mut s = Surface::new(16, 16);
    let clip = s.canvas().bounds();
    s.canvas().blit(
        &clip,
        &Rect::new(0.0, 0.0, 6.0, 6.0),
        &img,
        &img.bounds(),
        1.0,
    );
    // Row 2's centre maps to source y = 1.1666..., but every row is identical,
    // so vertical interpolation is a no-op and only x matters.
    let mid01 = s.bgr(1, 2).0; // (0 + 100) / 2
    let mid23 = s.bgr(4, 2).0; // (200 + 40) / 2
    assert!(mid01.abs_diff(50) <= 1, "midpoint 0-1 got {mid01}");
    assert!(mid23.abs_diff(120) <= 1, "midpoint 2-3 got {mid23}");
}

#[test]
fn blit_two_times_bilinear_matches_a_float_reference() {
    let vals: [u8; 16] = [
        0, 255, 90, 30, //
        255, 0, 10, 200, //
        60, 130, 255, 0, //
        20, 40, 80, 160,
    ];
    let data = grey_image(&vals, 4, 4);
    let img = image_of(&data, 4, 4, PixelFormat::Argb8888);
    let mut s = Surface::new(16, 16);
    let clip = s.canvas().bounds();
    s.canvas().blit(
        &clip,
        &Rect::new(0.0, 0.0, 8.0, 8.0),
        &img,
        &img.bounds(),
        1.0,
    );
    for y in 0..8 {
        for x in 0..8 {
            let sx = f32::midpoint(x as f32, 0.5) - 0.5;
            let sy = f32::midpoint(y as f32, 0.5) - 0.5;
            let want = reference_bilinear_grey(&vals, 4, 4, sx, sy);
            let got = f32::from(s.bgr(x, y).0);
            assert!(
                (got - want).abs() <= 2.0,
                "({x},{y}) got {got} want {want:.2}"
            );
        }
    }
    // Corners clamp to the corner texels.
    assert!(s.bgr(0, 0).0.abs_diff(vals[0]) <= 2);
    assert!(s.bgr(7, 7).0.abs_diff(vals[15]) <= 2);
}

#[test]
fn blit_scaled_of_a_flat_image_is_that_colour() {
    // Bilinear of a constant image must be exactly the constant everywhere.
    let mut data = vec![0u8; 64 * 8];
    for y in 0..8 {
        for x in 0..8 {
            let o = y * 64 + x * 4;
            data[o] = 0x30;
            data[o + 1] = 0x80;
            data[o + 2] = 0xE0;
            data[o + 3] = 0xFF;
        }
    }
    let img = Image {
        data: &data,
        width: 8,
        height: 8,
        stride: 64,
        format: PixelFormat::Argb8888,
    };
    let mut s = Surface::new(24, 24);
    let clip = s.canvas().bounds();
    s.canvas().blit(
        &clip,
        &Rect::new(1.0, 1.0, 12.0, 12.0),
        &img,
        &img.bounds(),
        1.0,
    );
    for y in 2..12 {
        for x in 2..12 {
            assert_eq!(s.bgr(x, y), (0x30, 0x80, 0xE0), "({x},{y})");
        }
    }
}

#[test]
fn image_texel_edge_extends() {
    let data = checker_image(16, 16, PixelFormat::Argb8888);
    let img = image_of(&data, 16, 16, PixelFormat::Argb8888);
    let quad = |t: super::canvas::Texel| (t.b, t.g, t.r, t.a);
    // In range.
    let inside = img.texel(3, 4);
    let off = (4 * img.stride + 3 * 4) as usize;
    assert_eq!(
        quad(inside),
        (
            u32::from(data[off]),
            u32::from(data[off + 1]),
            u32::from(data[off + 2]),
            u32::from(data[off + 3]),
        )
    );
    // Out of range clamps to the nearest edge texel.
    assert_eq!(quad(img.texel(0, 0)), quad(img.texel(-5, -9)));
    assert_eq!(quad(img.texel(15, 15)), quad(img.texel(99, 99)));
    // An Xrgb source reports opaque regardless of the stored byte.
    let xi = image_of(&data, 16, 16, PixelFormat::Xrgb8888);
    assert_eq!(xi.texel(0, 0).a, 255);
}

#[test]
fn blit_split_is_byte_identical_to_the_general_walk() {
    // The scaled blit splits each destination row into an interior run --
    // constant coverage, no texel pair needing a clamp -- and the leading and
    // trailing columns where one or both fail. The split is a pure
    // optimization, so it must be *exactly* the general walk: a blit whose
    // interior rounded differently from its edges would be a visible seam
    // down both sides of every scaled image.
    //
    // The sweep hits what the split's preconditions are made of: scales above
    // and below 1 and non-integer ones, fractional destination origins (which
    // move the coverage partial columns and the 16.16 phase independently),
    // sub-rects that put the source-clamp boundary inside the destination
    // row, both source formats, and clips that cut a row down to a couple of
    // columns -- i.e. rows that are *all* edge and have no interior at all.
    let argb = noisy_argb(16, 16, 0x9E37_79B9_7F4A_7C15);
    let mut cases = 0;
    for format in [PixelFormat::Argb8888, PixelFormat::Xrgb8888] {
        let img = image_of(&argb, 16, 16, format);
        for (dw, dh) in [
            (24.0_f32, 24.0_f32), // 1.5x up
            (40.0, 17.0),         // wide, non-integer both axes
            (9.0, 33.0),          // down in x, up in y
            (7.5, 7.5),           // below 1:1, fractional extent
            (32.0, 32.0),         // exact 2x
        ] {
            for (ox, oy) in [(0.0_f32, 0.0_f32), (0.3, 0.0), (2.5, 1.25), (-3.75, -2.5)] {
                for src_rect in [IRect::new(0, 0, 16, 16), IRect::new(3, 2, 9, 11)] {
                    for opacity in [1.0_f32, 0.45] {
                        for clip in [
                            IRect::new(0, 0, 48, 40),
                            IRect::new(5, 3, 30, 24),
                            IRect::new(11, 0, 2, 40), // two columns: no interior
                            IRect::new(0, 17, 48, 1),
                        ] {
                            let dst = Rect::new(ox, oy, dw, dh);
                            let mut fast = Surface::new(48, 40);
                            let mut slow = Surface::new(48, 40);
                            let all = IRect::new(0, 0, 48, 40);
                            // A non-uniform destination, so a wrong blend
                            // cannot hide in a flat background.
                            fast.canvas()
                                .fill_irect(&all, &all, Color::rgb(20, 90, 160));
                            slow.canvas()
                                .fill_irect(&all, &all, Color::rgb(20, 90, 160));
                            fast.canvas().blit(&clip, &dst, &img, &src_rect, opacity);
                            slow.canvas()
                                .blit_general(&clip, &dst, &img, &src_rect, opacity);
                            assert_eq!(
                                fast.data, slow.data,
                                "fmt={format:?} dst={dst:?} src={src_rect:?} \
                                 op={opacity} clip={clip:?}"
                            );
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 2 * 5 * 4 * 2 * 2 * 4);
}

#[test]
fn blit_split_never_writes_outside_clip() {
    // The split hands each run a sub-slice of the row it computed itself, so
    // the clip contract is re-asserted against the split specifically.
    let argb = noisy_argb(16, 16, 0x1234_5678_9ABC_DEF0);
    let img = image_of(&argb, 16, 16, PixelFormat::Argb8888);
    for clip in [
        IRect::new(7, 5, 19, 13),
        IRect::new(0, 0, 1, 40),
        IRect::new(40, 30, 8, 10),
    ] {
        let mut s = Surface::new(48, 40);
        s.canvas().blit(
            &clip,
            &Rect::new(-2.5, -1.25, 55.0, 47.0),
            &img,
            &img.bounds(),
            0.8,
        );
        s.assert_untouched_outside(&clip);
    }
}

#[test]
fn blit_rejects_invalid_images() {
    let data = [0u8; 16];
    let img = Image {
        data: &data,
        width: 100,
        height: 100,
        stride: 400,
        format: PixelFormat::Xrgb8888,
    };
    assert!(!img.is_valid());
    let mut s = Surface::new(8, 8);
    let clip = s.canvas().bounds();
    s.canvas().blit(
        &clip,
        &Rect::new(0.0, 0.0, 8.0, 8.0),
        &img,
        &img.bounds(),
        1.0,
    );
    s.assert_untouched_outside(&IRect::EMPTY);
}

// ---------------------------------------------------------------------------
// Misc surface behaviour
// ---------------------------------------------------------------------------

#[test]
fn canvas_accessors_and_pixel_blend() {
    let mut s = Surface::new(8, 4);
    {
        let mut c = s.canvas();
        assert_eq!(c.width(), 8);
        assert_eq!(c.height(), 4);
        assert_eq!(c.stride(), 64);
        assert_eq!(c.bounds(), IRect::new(0, 0, 8, 4));
        assert_eq!(c.data().len(), 256);
        assert_eq!(c.data_mut().len(), 256);
    }
    let clip = IRect::new(0, 0, 8, 4);
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas()
        .blend_pixel_at(&clip, 3, 2, Color::rgba(255, 255, 255, 128), 1.0);
    assert!(s.bgr(3, 2).0.abs_diff(128) <= 1);
    // Outside the clip: no-op.
    s.canvas()
        .blend_pixel_at(&IRect::new(0, 0, 2, 2), 3, 2, Color::WHITE, 1.0);
    assert!(s.bgr(3, 2).0.abs_diff(128) <= 1);
}

#[test]
fn fill_helpers_report_opacity() {
    assert!(Fill::Solid(Color::WHITE).is_opaque());
    assert!(!Fill::Solid(Color::rgba(1, 2, 3, 4)).is_opaque());
    assert!(Fill::Solid(Color::TRANSPARENT).is_transparent());
    let g = Fill::Linear {
        start: Point::ZERO,
        end: Point::new(0.0, 1.0),
        c0: Color::WHITE,
        c1: Color::rgba(0, 0, 0, 128),
    };
    assert!(!g.is_opaque());
    assert!(!g.is_transparent());
    assert!(PixelFormat::Xrgb8888.is_opaque());
    assert!(!PixelFormat::Argb8888.is_opaque());
}

#[test]
#[should_panic(expected = "stride")]
fn canvas_rejects_a_too_small_stride() {
    let mut d = vec![0u8; 64];
    let _ = Canvas::new(&mut d, 8, 2, 16);
}

#[test]
#[should_panic(expected = "too small")]
fn canvas_rejects_a_too_small_buffer() {
    let mut d = vec![0u8; 16];
    let _ = Canvas::new(&mut d, 8, 2, 32);
}

#[test]
fn painting_a_damage_rect_grid_covers_the_surface_exactly_once() {
    // The compositor pattern: one fill per damage rect, all disjoint.
    let mut s = Surface::new(40, 24);
    let c = Color::rgb(1, 2, 3);
    for gy in 0..3 {
        for gx in 0..4 {
            let clip = IRect::new(gx * 10, gy * 8, 10, 8);
            s.canvas().fill_rect(
                &clip,
                &Rect::new(0.0, 0.0, 40.0, 24.0),
                &Fill::Solid(c),
                0.0,
                1.0,
            );
        }
    }
    for y in 0..24 {
        for x in 0..40 {
            assert_eq!(s.bgr(x, y), rgb(c), "({x},{y})");
        }
    }
}

// ---------------------------------------------------------------------------
// Mask blits
// ---------------------------------------------------------------------------

/// A `w × h` mask of pseudo-random coverage, laid into a page of `stride`
/// bytes per row; the padding is filled with a poison value so a stride bug
/// shows up as a wrong pixel rather than a plausible one.
fn mask_page(w: u32, h: u32, stride: u32, rng: &mut Rng) -> Vec<u8> {
    let mut out = vec![0xAAu8; (stride * h) as usize];
    for y in 0..h {
        for x in 0..w {
            out[(y * stride + x) as usize] = rng.byte();
        }
    }
    out
}

/// A `w × h` mask of one coverage value, tightly packed.
fn mask_flat(w: u32, h: u32, cov: u8) -> Vec<u8> {
    vec![cov; (w * h) as usize]
}

fn mask_of(data: &[u8], w: u32, h: u32, stride: u32) -> Mask<'_> {
    Mask { data, w, h, stride }
}

#[test]
fn mask_blend_matches_float_reference() {
    let mut rng = Rng::new(0x1234_5678_9ABC_DEF1);
    let (w, h) = (5u32, 3u32);
    for _ in 0..400 {
        let cov = mask_page(w, h, w, &mut rng);
        let mask = mask_of(&cov, w, h, w);
        let color = Color::rgba(rng.byte(), rng.byte(), rng.byte(), rng.byte());
        let op_u8 = rng.byte();
        let opacity = f32::from(op_u8) / 255.0;
        // Random destination pixels, one per mask pixel.
        let mut dst = Vec::with_capacity((w * h) as usize);
        let mut s = Surface::new(w, h);
        let clip = s.canvas().bounds();
        for y in 0..iw(h) {
            for x in 0..iw(w) {
                let under = Color::rgb(rng.byte(), rng.byte(), rng.byte());
                dst.push(under);
                s.canvas().fill_irect(&clip, &IRect::new(x, y, 1, 1), under);
            }
        }
        s.canvas().blit_mask(&clip, 0, 0, &mask, color, opacity);
        for y in 0..iw(h) {
            for x in 0..iw(w) {
                let frac = f32::from(cov[(y * iw(w) + x) as usize]) / 255.0;
                // The library quantises opacity to a byte first; do the same.
                let op = f32::from(super::blend::unit_u8(opacity)) / 255.0;
                let under = dst[(y * iw(w) + x) as usize];
                let want = reference_over(color, rgb(under), frac * op);
                let got = s.bgr(x, y);
                for (g, wv) in [(got.0, want.0), (got.1, want.1), (got.2, want.2)] {
                    assert!(
                        g.abs_diff(wv) <= 1,
                        "({x},{y}) color {color:?} op {op_u8} cov {frac}: got {got:?} want {want:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn mask_never_writes_outside_clip() {
    let mut rng = Rng::new(0x0F0F_0F0F_1111_2222);
    let cov = mask_page(24, 20, 24, &mut rng);
    let mask = mask_of(&cov, 24, 20, 24);
    let color = Color::rgba(0x10, 0x20, 0x30, 200);
    // A clip that is strictly inside the surface, with the mask hanging off
    // all four of its edges.
    let mut s = Surface::new(40, 30);
    let clip = IRect::new(8, 6, 10, 9);
    for (x, y) in [(0, 0), (-6, -7), (14, 11), (-3, 10), (16, -5)] {
        s.canvas().blit_mask(&clip, x, y, &mask, color, 1.0);
    }
    s.assert_untouched_outside(&clip);
    assert_ne!(
        s.px(12, 10),
        SENTINEL,
        "nothing was painted inside the clip"
    );

    // A clip larger than the surface, with the mask hanging off every surface
    // edge: partial blits, no panic, nothing outside the surface (which the
    // borrow checker guarantees) and nothing outside the surface∩clip.
    let mut s = Surface::new(16, 12);
    let big = IRect::new(-100, -100, 1000, 1000);
    for (x, y) in [(-10, -8), (-10, 5), (10, -8), (10, 5), (-30, 0), (0, -40)] {
        s.canvas().blit_mask(&big, x, y, &mask, color, 1.0);
    }
    // Far off in both directions: pure no-ops.
    let mut s2 = Surface::new(16, 12);
    for (x, y) in [(1000, 0), (0, 1000), (-1000, 0), (0, -1000)] {
        s2.canvas().blit_mask(&big, x, y, &mask, color, 1.0);
    }
    s2.assert_untouched_outside(&IRect::EMPTY);
    // An empty clip paints nothing.
    let mut s3 = Surface::new(16, 12);
    s3.canvas()
        .blit_mask(&IRect::new(4, 4, 0, 8), 0, 0, &mask, color, 1.0);
    s3.canvas()
        .blit_mask(&IRect::new(100, 100, 8, 8), 0, 0, &mask, color, 1.0);
    s3.assert_untouched_outside(&IRect::EMPTY);
    // The clipped-off blits above did paint something in `s`.
    assert_ne!(s.px(0, 5), SENTINEL);
}

#[test]
fn mask_partial_blit_at_negative_coords_is_the_right_sub_rect() {
    let mut rng = Rng::new(0x5151_5151_7777_0001);
    let cov = mask_page(8, 6, 8, &mut rng);
    let mask = mask_of(&cov, 8, 6, 8);
    let color = Color::rgb(0xFF, 0xFF, 0xFF);
    let mut s = Surface::new(8, 6);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    // Place the mask at (-3, -2): device (0, 0) shows mask texel (3, 2).
    s.canvas().blit_mask(&clip, -3, -2, &mask, color, 1.0);
    for y in 0..4 {
        for x in 0..5 {
            let want = cov[((y + 2) * 8 + (x + 3)) as usize];
            assert_eq!(s.bgr(x, y).0, want, "({x},{y})");
        }
    }
    // Beyond the mask's extent: still black.
    assert_eq!(s.bgr(5, 0), (0, 0, 0));
    assert_eq!(s.bgr(0, 4), (0, 0, 0));
}

#[test]
fn mask_full_coverage_equals_fill_irect() {
    let c = Color::rgb(0x12, 0x34, 0x56);
    let cov = mask_flat(9, 7, 255);
    let mask = mask_of(&cov, 9, 7, 9);
    let mut a = Surface::new(20, 16);
    let mut b = Surface::new(20, 16);
    let clip = IRect::new(0, 0, 20, 16);
    a.canvas().blit_mask(&clip, 3, 2, &mask, c, 1.0);
    b.canvas().fill_irect(&clip, &IRect::new(3, 2, 9, 7), c);
    assert_eq!(a.data, b.data, "all-255 mask differs from fill_irect");
}

#[test]
fn mask_full_coverage_translucent_equals_fill_irect() {
    // The non-opaque path must agree with the blended fill too.
    let c = Color::rgba(0x12, 0x34, 0x56, 137);
    let cov = mask_flat(9, 7, 255);
    let mask = mask_of(&cov, 9, 7, 9);
    let mut a = Surface::new(20, 16);
    let mut b = Surface::new(20, 16);
    let clip = IRect::new(0, 0, 20, 16);
    a.canvas().fill_irect(&clip, &clip, Color::BLACK);
    b.canvas().fill_irect(&clip, &clip, Color::BLACK);
    a.canvas().blit_mask(&clip, 3, 2, &mask, c, 1.0);
    b.canvas().fill_irect(&clip, &IRect::new(3, 2, 9, 7), c);
    assert_eq!(a.data, b.data);
}

#[test]
fn mask_zero_coverage_changes_nothing() {
    let cov = mask_flat(9, 7, 0);
    let mask = mask_of(&cov, 9, 7, 9);
    let mut s = Surface::new(20, 16);
    let clip = IRect::new(0, 0, 20, 16);
    s.canvas().blit_mask(&clip, 3, 2, &mask, Color::WHITE, 1.0);
    s.assert_untouched_outside(&IRect::EMPTY);
}

#[test]
fn mask_sub_rect_of_a_page_uses_the_stride() {
    // A 32-byte-wide atlas page holding a 5×4 glyph at its top-left, with the
    // rest of the page poisoned; blitting w=5,h=4,stride=32 must read only the
    // glyph.
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_0001);
    let page = mask_page(5, 4, 32, &mut rng);
    let mask = mask_of(&page, 5, 4, 32);
    assert!(mask.is_valid());
    let mut s = Surface::new(12, 8);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().blit_mask(&clip, 2, 1, &mask, Color::WHITE, 1.0);
    for y in 0..4 {
        for x in 0..5 {
            let want = page[(y * 32 + x) as usize];
            assert_eq!(s.bgr(2 + x, 1 + y).0, want, "({x},{y})");
        }
    }
    // The poison bytes were not read: the surrounding pixels are still black.
    for x in 0..12 {
        assert_eq!(s.bgr(x, 0), (0, 0, 0), "row above ({x})");
        assert_eq!(s.bgr(x, 5), (0, 0, 0), "row below ({x})");
    }
    for y in 0..8 {
        assert_eq!(s.bgr(7, y), (0, 0, 0), "column right ({y})");
        assert_eq!(s.bgr(1, y), (0, 0, 0), "column left ({y})");
    }
}

#[test]
fn mask_sub_rect_in_the_middle_of_a_page() {
    // Blit the glyph at page offset (3, 2) by slicing `data` — the documented
    // way to address a sub-rectangle of an atlas page.
    let mut rng = Rng::new(0x1010_2020_3030_4040);
    let page = mask_page(16, 12, 16, &mut rng);
    let sub = Mask {
        data: &page[(2 * 16 + 3)..],
        w: 6,
        h: 5,
        stride: 16,
    };
    assert!(sub.is_valid());
    let mut s = Surface::new(16, 12);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().blit_mask(&clip, 0, 0, &sub, Color::WHITE, 1.0);
    for y in 0..5 {
        for x in 0..6 {
            let want = page[((y + 2) * 16 + (x + 3)) as usize];
            assert_eq!(s.bgr(x, y).0, want, "({x},{y})");
        }
    }
}

#[test]
fn mask_at_the_bottom_right_of_a_page_is_valid_and_blits() {
    // The regression this guards. A glyph packed onto an atlas page's last
    // shelf, at `y + h == PAGE` and `x > 0`, is followed by `PAGE * h - x`
    // bytes — fewer than `stride * h`. Demanding a full final row rejected
    // exactly those glyphs, and rejected them *silently*: `blit_mask`
    // returned early and they simply never appeared.
    const PAGE: u32 = 32;
    let mut rng = Rng::new(0x0BAD_5EED_0BAD_5EED);
    let page = mask_page(PAGE, PAGE, PAGE, &mut rng);
    let (gw, gh) = (5u32, 4u32);
    // Bottom-right corner: the last row of the glyph is the last row of the
    // page, and there is nothing at all after its last pixel.
    let (gx, gy) = (PAGE - gw, PAGE - gh);
    let tail = &page[(gy * PAGE + gx) as usize..];
    assert_eq!(
        tail.len() as u32,
        (gh - 1) * PAGE + gw,
        "the slice is exactly the documented minimum"
    );
    let mask = mask_of(tail, gw, gh, PAGE);
    assert!(
        mask.is_valid(),
        "a mask ending flush with the page must be valid"
    );

    let mut s = Surface::new(12, 8);
    let clip = s.canvas().bounds();
    s.canvas().fill_irect(&clip, &clip, Color::BLACK);
    s.canvas().blit_mask(&clip, 1, 1, &mask, Color::WHITE, 1.0);
    for y in 0..gh {
        for x in 0..gw {
            let want = page[((gy + y) * PAGE + gx + x) as usize];
            let (px, py) = ((1 + x).cast_signed(), (1 + y).cast_signed());
            assert_eq!(s.bgr(px, py).0, want, "({x},{y})");
        }
    }

    // One byte short of that minimum is still invalid, and still a no-op.
    let short = mask_of(&tail[..tail.len() - 1], gw, gh, PAGE);
    assert!(!short.is_valid());
    let mut t = Surface::new(12, 8);
    t.canvas().fill_irect(&clip, &clip, Color::BLACK);
    t.canvas().blit_mask(&clip, 1, 1, &short, Color::WHITE, 1.0);
    for y in 0..8 {
        for x in 0..12 {
            assert_eq!(
                t.bgr(x, y),
                (0, 0, 0),
                "a truncated mask must paint nothing ({x},{y})"
            );
        }
    }
}

#[test]
fn mask_degenerate_inputs_are_no_ops() {
    let data = mask_flat(8, 8, 255);
    let mut s = Surface::new(12, 8);
    let clip = s.canvas().bounds();
    let bad = [
        mask_of(&data, 0, 4, 4),  // zero width
        mask_of(&data, 4, 0, 4),  // zero height
        mask_of(&data, 8, 4, 4),  // stride < w
        mask_of(&data, 8, 20, 8), // data too short
        mask_of(&[], 4, 4, 4),    // no data at all
    ];
    for m in &bad {
        assert!(!m.is_valid());
        s.canvas().blit_mask(&clip, 0, 0, m, Color::WHITE, 1.0);
        s.canvas()
            .blit_masks(&clip, Color::WHITE, 1.0, &[(0, 0, *m)]);
    }
    // Valid mask, but nothing to paint with.
    let ok = mask_of(&data, 8, 8, 8);
    assert!(ok.is_valid());
    s.canvas()
        .blit_mask(&clip, 0, 0, &ok, Color::TRANSPARENT, 1.0);
    s.canvas().blit_mask(&clip, 0, 0, &ok, Color::WHITE, 0.0);
    s.canvas().blit_mask(&clip, 0, 0, &ok, Color::WHITE, -1.0);
    s.canvas()
        .blit_masks(&clip, Color::WHITE, 0.0, &[(0, 0, ok)]);
    // An empty batch.
    s.canvas().blit_masks(&clip, Color::WHITE, 1.0, &[]);
    s.assert_untouched_outside(&IRect::EMPTY);
}

/// `N` glyph-sized masks at pseudo-random positions — the batch workload.
fn glyph_run(
    rng: &mut Rng,
    count: usize,
    gw: u32,
    gh: u32,
    span: i32,
) -> (Vec<u8>, Vec<(i32, i32)>) {
    let cov = mask_page(gw, gh, gw, rng);
    let mut at = Vec::with_capacity(count);
    for _ in 0..count {
        let px = (rng.next_u32() % span.unsigned_abs()).cast_signed() - span / 3;
        let py = (rng.next_u32() % span.unsigned_abs()).cast_signed() - span / 3;
        at.push((px, py));
    }
    (cov, at)
}

#[test]
fn mask_batch_equals_a_loop_of_single_blits() {
    for (color, opacity) in [
        (Color::rgb(0xE0, 0xE4, 0xEC), 1.0_f32),
        (Color::rgba(0xE0, 0x40, 0x20, 190), 0.6),
    ] {
        let mut rng = Rng::new(0x7777_1111_2222_3333);
        let (cov, at) = glyph_run(&mut rng, 40, 8, 12, 48);
        let mask = mask_of(&cov, 8, 12, 8);
        let entries: Vec<(i32, i32, Mask<'_>)> = at.iter().map(|&(x, y)| (x, y, mask)).collect();

        let mut a = Surface::new(48, 32);
        let mut b = Surface::new(48, 32);
        let clip = IRect::new(2, 1, 40, 28);
        let surf = IRect::new(0, 0, 48, 32);
        a.canvas().fill_irect(&surf, &clip, Color::BLACK);
        b.canvas().fill_irect(&surf, &clip, Color::BLACK);
        for &(x, y) in &at {
            a.canvas().blit_mask(&clip, x, y, &mask, color, opacity);
        }
        b.canvas().blit_masks(&clip, color, opacity, &entries);
        assert_eq!(a.data, b.data, "batch differs from a loop of single blits");
    }
}

#[test]
fn mask_batch_and_loop_timing() {
    // Not an assertion about speed — a number for the README. Run with
    // `cargo test -p nitro-raster -- --nocapture mask_batch_and_loop_timing`.
    use std::time::Instant;

    const GLYPHS: usize = 50;

    let mut rng = Rng::new(0x2545_F491_4F6C_DD1D);
    let (cov, at) = glyph_run(&mut rng, GLYPHS, 8, 12, 300);
    let mask = mask_of(&cov, 8, 12, 8);
    let entries: Vec<(i32, i32, Mask<'_>)> = at.iter().map(|&(x, y)| (x, y, mask)).collect();
    let mut s = Surface::new(400, 64);
    let clip = s.canvas().bounds();
    let color = Color::rgb(0xE0, 0xE4, 0xEC);

    let reps = 2000;
    let mut loop_best = f64::MAX;
    let mut batch_best = f64::MAX;
    for _ in 0..5 {
        let t0 = Instant::now();
        for _ in 0..reps {
            for &(x, y) in &at {
                s.canvas().blit_mask(&clip, x, y, &mask, color, 1.0);
            }
        }
        loop_best = loop_best.min(t0.elapsed().as_secs_f64());
        let t1 = Instant::now();
        for _ in 0..reps {
            s.canvas().blit_masks(&clip, color, 1.0, &entries);
        }
        batch_best = batch_best.min(t1.elapsed().as_secs_f64());
    }
    let per = f64::from(reps);
    println!(
        "{GLYPHS} glyphs of 8x12: loop {:.2} us/run, batch {:.2} us/run ({:.1}% saved)",
        loop_best / per * 1e6,
        batch_best / per * 1e6,
        (loop_best - batch_best) / loop_best * 100.0,
    );
}

/// A noisy ARGB source: structure in every channel, and a deliberate excess of
/// alpha 0 and 255, so a blit sweep sees transparent, translucent and opaque
/// texels — including the transparent one, which is the case the split's
/// interior run deliberately stops branching on.
///
/// One generator for both the split's byte-identity sweep and the golden-hash
/// sweep (issue #552 folded the second copy in). **The byte-generation order
/// is load bearing**: `blit_output_matches_the_golden_hash` pins a hard-coded
/// hash of this generator's output run through the blit, so changing the
/// argument order, the `% 4` alpha distribution or the order of the `byte()`
/// calls moves the hash and fails that test.
fn noisy_argb(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let stride = (w * 4).div_ceil(64) * 64;
    let mut out = vec![0u8; (stride * h) as usize];
    let mut r = Rng::new(seed);
    for y in 0..h {
        for x in 0..w {
            let o = (y * stride + x * 4) as usize;
            out[o] = r.byte();
            out[o + 1] = r.byte();
            out[o + 2] = r.byte();
            out[o + 3] = match r.next_u32() % 4 {
                0 => 0,
                1 => 255,
                _ => r.byte(),
            };
        }
    }
    out
}

/// FNV-1a over every byte a blit sweep produces. Public API only, so the same
/// sweep can be run against another revision and the two hashes compared.
fn blit_sweep_hash() -> (u32, u64) {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let argb = noisy_argb(16, 16, 0x9E37_79B9_7F4A_7C15);
    let mut cases = 0;
    for format in [PixelFormat::Argb8888, PixelFormat::Xrgb8888] {
        let img = image_of(&argb, 16, 16, format);
        for (dw, dh) in [
            (24.0_f32, 24.0_f32),
            (40.0, 17.0),
            (9.0, 33.0),
            (7.5, 7.5),
            (32.0, 32.0),
            (61.0, 3.5),
        ] {
            for (ox, oy) in [
                (0.0_f32, 0.0_f32),
                (0.3, 0.0),
                (2.5, 1.25),
                (-3.75, -2.5),
                (17.125, 9.875),
            ] {
                for src_rect in [
                    IRect::new(0, 0, 16, 16),
                    IRect::new(3, 2, 9, 11),
                    IRect::new(0, 0, 1, 16),
                    IRect::new(11, 11, 5, 5),
                ] {
                    for opacity in [1.0_f32, 0.45, 0.02] {
                        for clip in [
                            IRect::new(0, 0, 48, 40),
                            IRect::new(5, 3, 30, 24),
                            IRect::new(11, 0, 2, 40),
                            IRect::new(0, 17, 48, 1),
                            IRect::new(46, 38, 2, 2),
                        ] {
                            let mut s = Surface::new(48, 40);
                            let all = IRect::new(0, 0, 48, 40);
                            s.canvas().fill_irect(&all, &all, Color::rgb(20, 90, 160));
                            s.canvas().blit(
                                &clip,
                                &Rect::new(ox, oy, dw, dh),
                                &img,
                                &src_rect,
                                opacity,
                            );
                            // Byte 3 is alpha since #3898 and must be 255
                            // over the opaque background; it is hashed as
                            // the 0 it used to be, so the pre-split hash
                            // still pins the colour bytes.
                            for (i, b) in s.data.iter().enumerate() {
                                let b = if i % 4 == 3 {
                                    assert_eq!(*b, 255, "alpha at byte {i}");
                                    0
                                } else {
                                    *b
                                };
                                hash ^= u64::from(b);
                                hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
                            }
                            cases += 1;
                        }
                    }
                }
            }
        }
    }
    (cases, hash)
}

#[test]
fn blit_output_matches_the_golden_hash() {
    // A pinned hash of 3600 blit configurations, through the public API only.
    //
    // This exists because the *other* blit test cannot catch a whole class of
    // bug. `blit_split_is_byte_identical_to_the_general_walk` compares the
    // split against `blit_general` -- but both run the same edge code, so a
    // mistake made in the code they share is invisible to it, and one was:
    // the split dropped the general loop's `alpha == 0` check, which is load
    // bearing (see `blend_texel`), and both sides of the comparison dropped
    // it together. Byte-identity to yourself is not correctness.
    //
    // The hash was taken from the pre-split implementation at 329faf3 by
    // running this same sweep there, so it pins this crate's blit output to
    // what shipped before the split, not to what the split happens to do.
    //
    // If a deliberate change to blit output lands, this value must be
    // updated -- and the update should be justified in the commit message,
    // because every other blit test passing does not mean the output is the
    // same.
    let (cases, hash) = blit_sweep_hash();
    assert_eq!(cases, 3600);
    assert_eq!(
        hash, 0x40b4_4566_be88_3bec,
        "blit output changed against the pre-split reference (329faf3)"
    );
}

// ---------------------------------------------------------------------------
// NV12 video blit
// ---------------------------------------------------------------------------

// Y/U/V/R/G/B, x/y/w/h: the domain's names.
#[allow(clippy::many_single_char_names, clippy::similar_names)]
mod nv12 {
    use super::{Rng, SENTINEL, Surface, iw};
    use crate::{Nv12, YuvEncoding, YuvMatrix, YuvRange};
    use nitro_core::IRect;

    const MATRICES: [YuvMatrix; 3] = [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020];
    const RANGES: [YuvRange; 2] = [YuvRange::Limited, YuvRange::Full];

    fn encodings() -> impl Iterator<Item = YuvEncoding> {
        MATRICES
            .into_iter()
            .flat_map(|m| RANGES.into_iter().map(move |r| YuvEncoding::new(m, r)))
    }

    // ---- float references --------------------------------------------------

    /// `(luma scale, chroma scale, luma offset)`.
    fn range_params(r: YuvRange) -> (f64, f64, f64) {
        match r {
            YuvRange::Limited => (255.0 / 219.0, 255.0 / 224.0, 16.0),
            YuvRange::Full => (1.0, 1.0, 0.0),
        }
    }

    /// Unrounded float RGB of (possibly fractional) YUV.
    fn yuv_to_rgb_f(enc: YuvEncoding, y: f64, u: f64, v: f64) -> [f64; 3] {
        let (kr, kb) = enc.matrix.kr_kb();
        let kg = 1.0 - kr - kb;
        let (ys, cs, yoff) = range_params(enc.range);
        let l = (y - yoff) * ys;
        let (u, v) = ((u - 128.0) * cs, (v - 128.0) * cs);
        [
            l + 2.0 * (1.0 - kr) * v,
            l - 2.0 * kb * (1.0 - kb) / kg * u - 2.0 * kr * (1.0 - kr) / kg * v,
            l + 2.0 * (1.0 - kb) * u,
        ]
    }

    fn to_u8(x: f64) -> u8 {
        x.round().clamp(0.0, 255.0) as u8
    }

    /// Rounded, clamped `(r, g, b)`.
    fn yuv_to_rgb(enc: YuvEncoding, y: f64, u: f64, v: f64) -> (u8, u8, u8) {
        let [r, g, b] = yuv_to_rgb_f(enc, y, u, v);
        (to_u8(r), to_u8(g), to_u8(b))
    }

    /// Forward: RGB bytes → rounded YUV bytes.
    fn rgb_to_yuv(enc: YuvEncoding, rgb: (u8, u8, u8)) -> (u8, u8, u8) {
        let (kr, kb) = enc.matrix.kr_kb();
        let kg = 1.0 - kr - kb;
        let r = f64::from(rgb.0) / 255.0;
        let g = f64::from(rgb.1) / 255.0;
        let b = f64::from(rgb.2) / 255.0;
        let y = kr * r + kg * g + kb * b;
        let pb = (b - y) / (2.0 * (1.0 - kb));
        let pr = (r - y) / (2.0 * (1.0 - kr));
        match enc.range {
            YuvRange::Limited => (
                to_u8(16.0 + 219.0 * y),
                to_u8(128.0 + 224.0 * pb),
                to_u8(128.0 + 224.0 * pr),
            ),
            YuvRange::Full => (
                to_u8(255.0 * y),
                to_u8(128.0 + 255.0 * pb),
                to_u8(128.0 + 255.0 * pr),
            ),
        }
    }

    // ---- an owned NV12 image -----------------------------------------------

    /// An owned NV12 frame with padded strides and *tight* last rows (the
    /// buffers end right after the last row's payload).
    struct Frame {
        y: Vec<u8>,
        uv: Vec<u8>,
        w: u32,
        h: u32,
        ys: u32,
        uvs: u32,
    }

    impl Frame {
        /// Luma from `fy(x, y)`, chroma pair `(j, k)` from `fc(j, k)`.
        fn new(
            w: u32,
            h: u32,
            fy: impl Fn(u32, u32) -> u8,
            fc: impl Fn(u32, u32) -> (u8, u8),
        ) -> Self {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let ys = w + 5;
            let uvs = 2 * cw + 6;
            let mut y = vec![0xEE; ((h - 1) * ys + w) as usize];
            let mut uv = vec![0xEE; ((ch - 1) * uvs + 2 * cw) as usize];
            for r in 0..h {
                for c in 0..w {
                    y[(r * ys + c) as usize] = fy(c, r);
                }
            }
            for k in 0..ch {
                for j in 0..cw {
                    let (u, v) = fc(j, k);
                    uv[(k * uvs + 2 * j) as usize] = u;
                    uv[(k * uvs + 2 * j + 1) as usize] = v;
                }
            }
            Self {
                y,
                uv,
                w,
                h,
                ys,
                uvs,
            }
        }

        fn nv12(&self) -> Nv12<'_> {
            Nv12 {
                y: &self.y,
                y_stride: self.ys,
                uv: &self.uv,
                uv_stride: self.uvs,
                width: self.w,
                height: self.h,
            }
        }

        fn luma(&self, x: i32, y: i32) -> f64 {
            f64::from(self.y[y as usize * self.ys as usize + x as usize])
        }

        fn chroma(&self, j: i32, k: i32) -> (f64, f64) {
            let o = k as usize * self.uvs as usize + 2 * j as usize;
            (f64::from(self.uv[o]), f64::from(self.uv[o + 1]))
        }

        /// Nearest-chroma reference for the 1:1 path, source pixel `(x, y)`.
        fn ref_1to1(&self, enc: YuvEncoding, x: i32, y: i32) -> (u8, u8, u8) {
            let (u, v) = self.chroma(x >> 1, y >> 1);
            yuv_to_rgb(enc, self.luma(x, y), u, v)
        }

        /// Float bilinear reference for the scaled path, with the documented
        /// siting and edge clamps, at destination pixel `(dx, dy)`.
        fn ref_scaled(
            &self,
            enc: YuvEncoding,
            dst: &IRect,
            sr: &IRect,
            dx: i32,
            dy: i32,
        ) -> [f64; 3] {
            let sx = (f64::from(dx - dst.x) + 0.5) * f64::from(sr.w) / f64::from(dst.w)
                + f64::from(sr.x)
                - 0.5;
            let sy = (f64::from(dy - dst.y) + 0.5) * f64::from(sr.h) / f64::from(dst.h)
                + f64::from(sr.y)
                - 0.5;
            let lerp2 = |pos: f64, first: i32, last: i32| {
                let f = pos.floor();
                let t = pos - f;
                let i = f as i32;
                (i.clamp(first, last), (i + 1).clamp(first, last), t)
            };
            let (x0, x1, tx) = lerp2(sx, sr.x, sr.right() - 1);
            let (y0, y1, ty) = lerp2(sy, sr.y, sr.bottom() - 1);
            let l = (self.luma(x0, y0) * (1.0 - tx) + self.luma(x1, y0) * tx) * (1.0 - ty)
                + (self.luma(x0, y1) * (1.0 - tx) + self.luma(x1, y1) * tx) * ty;
            let (j0, j1, tcx) = lerp2(sx / 2.0, sr.x >> 1, (sr.right() - 1) >> 1);
            let (k0, k1, tcy) = lerp2((sy - 0.5) / 2.0, sr.y >> 1, (sr.bottom() - 1) >> 1);
            let c = |j, k| self.chroma(j, k);
            let mix = |a: (f64, f64), b: (f64, f64), t: f64| {
                (a.0 * (1.0 - t) + b.0 * t, a.1 * (1.0 - t) + b.1 * t)
            };
            let top = mix(c(j0, k0), c(j1, k0), tcx);
            let bot = mix(c(j0, k1), c(j1, k1), tcx);
            let (u, v) = mix(top, bot, tcy);
            yuv_to_rgb_f(enc, l, u, v)
        }
    }

    fn smooth_frame(w: u32, h: u32) -> Frame {
        // Gentle gradients: the fixed-point position quantization (1/256 px)
        // then moves a sample by far less than one code value.
        Frame::new(
            w,
            h,
            |x, y| (30 + (x * 3 + y * 2) % 190) as u8,
            |j, k| {
                (
                    (60 + (j * 5 + k) % 130) as u8,
                    (200 - (j + 3 * k) % 140) as u8,
                )
            },
        )
    }

    fn assert_near(got: (u8, u8, u8), want: (u8, u8, u8), tol: i32, what: &str) {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs();
        assert!(
            d(got.0, want.0) <= tol && d(got.1, want.1) <= tol && d(got.2, want.2) <= tol,
            "{what}: got {got:?}, want {want:?} (±{tol})"
        );
    }

    /// `(r, g, b)` of a surface pixel.
    fn rgb_at(s: &Surface, x: i32, y: i32) -> (u8, u8, u8) {
        let (b, g, r) = s.bgr(x, y);
        (r, g, b)
    }

    /// Byte 3 of a surface pixel.
    fn x_byte(s: &Surface, x: i32, y: i32) -> u8 {
        s.data[y as usize * s.stride as usize + x as usize * 4 + 3]
    }

    fn blit(s: &mut Surface, clip: &IRect, dst: &IRect, f: &Frame, sr: &IRect, enc: YuvEncoding) {
        s.canvas().blit_nv12(clip, dst, &f.nv12(), sr, enc);
    }

    const BARS: [(u8, u8, u8); 8] = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
        (0, 0, 0),
    ];

    #[test]
    fn colour_bars_round_trip_for_every_matrix_and_range() {
        const BAR_W: u32 = 4;
        for enc in encodings() {
            let yuv: Vec<_> = BARS.iter().map(|&c| rgb_to_yuv(enc, c)).collect();
            let f = Frame::new(
                BAR_W * 8,
                6,
                |x, _| yuv[(x / BAR_W) as usize].0,
                |j, _| {
                    let b = yuv[(2 * j / BAR_W) as usize];
                    (b.1, b.2)
                },
            );
            let mut s = Surface::new(40, 10);
            let all = s.canvas().bounds();
            let dst = IRect::new(3, 2, iw(f.w), iw(f.h));
            blit(&mut s, &all, &dst, &f, &f.nv12().bounds(), enc);
            for (i, &bar) in (0..).zip(BARS.iter()) {
                for dx in 0..iw(BAR_W) {
                    let x = 3 + iw(BAR_W) * i + dx;
                    assert_near(rgb_at(&s, x, 4), bar, 1, &format!("{enc:?} bar {i}"));
                    assert_eq!(x_byte(&s, x, 4), 255);
                }
            }
            s.assert_untouched_outside(&dst);
        }
    }

    #[test]
    fn published_limited_range_values_decode() {
        let bt601 = YuvEncoding::new(YuvMatrix::Bt601, YuvRange::Limited);
        let bt709 = YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Limited);
        let cases = [
            (bt601, (81, 90, 240), (255, 0, 0)),
            (bt709, (63, 102, 240), (255, 0, 0)),
            (bt601, (235, 128, 128), (255, 255, 255)),
            (bt709, (235, 128, 128), (255, 255, 255)),
            (bt709, (16, 128, 128), (0, 0, 0)),
            // Super-white and super-black clamp.
            (bt709, (250, 128, 128), (255, 255, 255)),
            (bt709, (4, 128, 128), (0, 0, 0)),
        ];
        for (enc, (y, u, v), want) in cases {
            let f = Frame::new(2, 2, |_, _| y, |_, _| (u, v));
            let mut s = Surface::new(2, 2);
            let all = s.canvas().bounds();
            blit(&mut s, &all, &all, &f, &all, enc);
            assert_near(rgb_at(&s, 1, 1), want, 1, &format!("{enc:?} {y}/{u}/{v}"));
        }
    }

    #[test]
    fn one_to_one_matches_the_float_reference_on_random_input() {
        let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
        let (w, h) = (33, 20);
        let f = Frame::new(
            w,
            h,
            |x, y| (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40503)) as u8,
            |j, k| {
                (
                    (j.wrapping_mul(97) ^ k.wrapping_mul(7919)) as u8,
                    (j.wrapping_mul(193) + k.wrapping_mul(389)) as u8,
                )
            },
        );
        // And every (Y, U, V) corner and a random cloud through a 2x2 frame.
        let mut samples: Vec<(u8, u8, u8)> = Vec::new();
        for y in [0, 16, 128, 235, 255] {
            for u in [0, 16, 128, 240, 255] {
                for v in [0, 16, 128, 240, 255] {
                    samples.push((y, u, v));
                }
            }
        }
        for _ in 0..2000 {
            samples.push((rng.byte(), rng.byte(), rng.byte()));
        }
        for enc in encodings() {
            let mut s = Surface::new(w, h);
            let all = s.canvas().bounds();
            blit(&mut s, &all, &all, &f, &all, enc);
            for y in 0..iw(h) {
                for x in 0..iw(w) {
                    assert_near(rgb_at(&s, x, y), f.ref_1to1(enc, x, y), 1, "frame");
                }
            }
            for &(y, u, v) in &samples {
                let f = Frame::new(2, 2, |_, _| y, |_, _| (u, v));
                let mut s = Surface::new(1, 1);
                let one = IRect::new(0, 0, 1, 1);
                blit(&mut s, &one, &one, &f, &one, enc);
                let want = yuv_to_rgb(enc, f64::from(y), f64::from(u), f64::from(v));
                assert_near(rgb_at(&s, 0, 0), want, 1, &format!("{enc:?} {y}/{u}/{v}"));
            }
        }
    }

    #[test]
    fn a_flat_field_scales_to_itself_everywhere() {
        let f = Frame::new(17, 9, |_, _| 140, |_, _| (90, 170));
        let enc = YuvEncoding::default();
        let mut s = Surface::new(2, 2);
        let all = s.canvas().bounds();
        blit(&mut s, &all, &all, &f, &IRect::new(0, 0, 2, 2), enc);
        let want = rgb_at(&s, 0, 0);
        for (w, h) in [
            (40, 23),
            (5, 3),
            (17, 9),
            (1, 30),
            (31, 1),
            (1, 1),
            (26, 13),
        ] {
            let mut s = Surface::new(w + 4, h + 4);
            let all = s.canvas().bounds();
            let dst = IRect::new(2, 2, iw(w), iw(h));
            blit(&mut s, &all, &dst, &f, &IRect::new(1, 1, 15, 7), enc);
            for y in dst.y..dst.bottom() {
                for x in dst.x..dst.right() {
                    assert_eq!(rgb_at(&s, x, y), want, "{w}x{h} at ({x}, {y})");
                    assert_eq!(x_byte(&s, x, y), 255);
                }
            }
            s.assert_untouched_outside(&dst);
        }
    }

    #[test]
    fn never_writes_outside_clip_and_dst() {
        let f = smooth_frame(23, 15);
        let enc = YuvEncoding::new(YuvMatrix::Bt601, YuvRange::Full);
        let dsts = [
            IRect::new(-5, -3, 30, 20),
            IRect::new(10, 8, 40, 30),
            IRect::new(4, 4, 23, 15),
            IRect::new(30, 20, 23, 15),
        ];
        let clips = [
            IRect::new(0, 0, 64, 48),
            IRect::new(3, 5, 17, 9),
            IRect::new(11, 0, 1, 48),
            IRect::new(7, 13, 20, 1),
            IRect::new(-10, -10, 25, 21),
            IRect::new(50, 40, 40, 40),
        ];
        for dst in &dsts {
            for clip in &clips {
                for sr in [IRect::new(0, 0, 23, 15), IRect::new(3, 1, 9, 7)] {
                    let mut s = Surface::new(48, 36);
                    blit(&mut s, clip, dst, &f, &sr, enc);
                    let allowed = clip.intersect(dst);
                    s.assert_untouched_outside(&allowed);
                    let inside = allowed.intersect(&s.canvas().bounds());
                    for y in inside.y..inside.bottom() {
                        for x in inside.x..inside.right() {
                            assert_ne!(s.px(x, y), SENTINEL, "({x}, {y}) not painted");
                            assert_eq!(x_byte(&s, x, y), 255);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn clipped_is_byte_identical_to_unclipped() {
        let f = smooth_frame(37, 21);
        let mut rng = Rng::new(0xDEAD_BEEF_1234_5678);
        let cases = [
            (IRect::new(2, 1, 37, 21), IRect::new(0, 0, 37, 21)), // 1:1
            (IRect::new(1, 3, 29, 19), IRect::new(5, 3, 29, 19)), // 1:1, odd crop
            (IRect::new(-7, 2, 70, 41), IRect::new(0, 0, 37, 21)), // up
            (IRect::new(3, 3, 19, 9), IRect::new(1, 1, 35, 19)),  // down
            (IRect::new(0, 0, 51, 13), IRect::new(3, 2, 30, 17)), // mixed
        ];
        for enc in encodings() {
            for (dst, sr) in &cases {
                let mut full = Surface::new(64, 48);
                let all = full.canvas().bounds();
                blit(&mut full, &all, dst, &f, sr, enc);
                for _ in 0..12 {
                    let x = iw(rng.next_u32() % 64);
                    let y = iw(rng.next_u32() % 48);
                    let w = 1 + iw(rng.next_u32() % 40);
                    let h = 1 + iw(rng.next_u32() % 30);
                    let clip = IRect::new(x, y, w, h);
                    let mut part = Surface::new(64, 48);
                    blit(&mut part, &clip, dst, &f, sr, enc);
                    let inside = clip.intersect(dst).intersect(&all);
                    for py in inside.y..inside.bottom() {
                        for px in inside.x..inside.right() {
                            assert_eq!(
                                part.px(px, py),
                                full.px(px, py),
                                "{enc:?} dst {dst:?} clip {clip:?} at ({px}, {py})"
                            );
                        }
                    }
                    part.assert_untouched_outside(&inside);
                }
            }
        }
    }

    #[test]
    fn one_to_one_with_odd_sizes_and_crops_uses_the_covering_chroma() {
        // Distinct chroma per column and row, so a wrong pair is visible.
        let f = Frame::new(
            17,
            9,
            |x, y| (20 + x * 11 + y * 7) as u8,
            |j, k| ((40 + j * 19 + k * 3) as u8, (220 - j * 13 - k * 5) as u8),
        );
        let crops = [
            IRect::new(0, 0, 17, 9),
            IRect::new(1, 1, 16, 8),
            IRect::new(3, 0, 13, 9),
            IRect::new(3, 1, 5, 3),
            IRect::new(16, 8, 1, 1),
            IRect::new(1, 0, 1, 9),
            IRect::new(5, 2, 12, 7),
        ];
        for enc in encodings() {
            for sr in &crops {
                for (ox, oy) in [(0, 0), (1, 2), (4, 1)] {
                    let mut s = Surface::new(24, 14);
                    let all = s.canvas().bounds();
                    let dst = IRect::new(ox, oy, sr.w, sr.h);
                    blit(&mut s, &all, &dst, &f, sr, enc);
                    for y in 0..sr.h {
                        for x in 0..sr.w {
                            let want = f.ref_1to1(enc, sr.x + x, sr.y + y);
                            assert_near(
                                rgb_at(&s, ox + x, oy + y),
                                want,
                                1,
                                &format!("{enc:?} crop {sr:?} src ({}, {})", sr.x + x, sr.y + y),
                            );
                        }
                    }
                    s.assert_untouched_outside(&dst);
                }
            }
        }
    }

    #[test]
    fn scaled_matches_the_bilinear_reference_with_siting() {
        let frames = [
            smooth_frame(17, 9),
            smooth_frame(1, 1),
            smooth_frame(40, 31),
        ];
        let geoms = [
            (IRect::new(0, 0, 40, 23), None),
            (IRect::new(2, 1, 9, 5), None),
            (IRect::new(0, 0, 33, 7), Some(IRect::new(1, 1, 7, 5))),
            (IRect::new(1, 2, 13, 29), Some(IRect::new(3, 0, 5, 7))),
            (IRect::new(0, 0, 1, 1), None),
            (IRect::new(0, 0, 41, 17), None),
        ];
        for enc in encodings() {
            for f in &frames {
                for (dst, crop) in &geoms {
                    let sr = crop
                        .unwrap_or_else(|| f.nv12().bounds())
                        .intersect(&f.nv12().bounds());
                    if sr.is_empty() || (sr.w == dst.w && sr.h == dst.h) {
                        continue;
                    }
                    let mut s = Surface::new(48, 36);
                    let all = s.canvas().bounds();
                    blit(&mut s, &all, dst, f, &sr, enc);
                    for y in dst.y..dst.bottom() {
                        for x in dst.x..dst.right() {
                            let [r, g, b] = f.ref_scaled(enc, dst, &sr, x, y);
                            let want = (to_u8(r), to_u8(g), to_u8(b));
                            assert_near(
                                rgb_at(&s, x, y),
                                want,
                                2,
                                &format!("{enc:?} {}x{} {dst:?} {sr:?} ({x}, {y})", f.w, f.h),
                            );
                        }
                    }
                    s.assert_untouched_outside(dst);
                }
            }
        }
    }

    #[test]
    fn scaled_chroma_is_sited_between_rows_and_on_even_columns() {
        // Chroma rows alternate 64/192 in U; luma is flat. At 2x vertical
        // upscale of a 4x4 source, destination rows land at source
        // y = 0.25 + 0.5 k, so chroma y = (y - 0.5) / 2 lies between chroma
        // rows 0 and 1 for the middle rows — the siting is observable.
        let f = Frame::new(
            4,
            4,
            |_, _| 128,
            |_, k| (if k == 0 { 64 } else { 192 }, 128),
        );
        let enc = YuvEncoding::new(YuvMatrix::Bt709, YuvRange::Full);
        let mut s = Surface::new(8, 8);
        let all = s.canvas().bounds();
        blit(
            &mut s,
            &all,
            &IRect::new(0, 0, 4, 8),
            &f,
            &f.nv12().bounds(),
            enc,
        );
        for y in 0..8 {
            let [_, _, b] = f.ref_scaled(enc, &IRect::new(0, 0, 4, 8), &f.nv12().bounds(), 0, y);
            let got = i32::from(rgb_at(&s, 0, y).2);
            assert!(
                (got - i32::from(to_u8(b))).abs() <= 2,
                "row {y}: {got} vs {b}"
            );
        }
        // Destination row y samples luma y (y + 0.5) / 2 - 0.5 and chroma
        // row (luma - 0.5) / 2: rows 0 and 1 land at -0.375 and -0.125
        // (clamped to chroma row 0), rows 6 and 7 at 1.125 and 1.375
        // (clamped to row 1), and rows 2..=5 interpolate strictly between.
        // A chroma y of `luma / 2` (the wrong, co-sited siting) would start
        // interpolating one destination row earlier.
        assert_eq!(rgb_at(&s, 0, 0), rgb_at(&s, 0, 1));
        assert_eq!(rgb_at(&s, 0, 6), rgb_at(&s, 0, 7));
        let blue = |y| rgb_at(&s, 0, y).2;
        assert!(blue(1) < blue(2) && blue(2) < blue(3) && blue(3) < blue(4) && blue(5) < blue(6));
    }

    #[test]
    fn validity_uses_tight_last_rows_and_invalid_is_a_no_op() {
        let f = smooth_frame(7, 5);
        assert!(f.nv12().is_valid(), "tight buffers must be accepted");
        let short_y = Nv12 {
            y: &f.y[..f.y.len() - 1],
            ..f.nv12()
        };
        let short_uv = Nv12 {
            uv: &f.uv[..f.uv.len() - 1],
            ..f.nv12()
        };
        let bad_ys = Nv12 {
            y_stride: 6,
            ..f.nv12()
        };
        let bad_uvs = Nv12 {
            uv_stride: 7, // needs 2 * ceil(7 / 2) = 8
            ..f.nv12()
        };
        let empty = Nv12 {
            width: 0,
            ..f.nv12()
        };
        for bad in [short_y, short_uv, bad_ys, bad_uvs, empty] {
            assert!(!bad.is_valid(), "{bad:?}");
            let mut s = Surface::new(16, 16);
            let all = s.canvas().bounds();
            s.canvas().blit_nv12(
                &all,
                &all,
                &bad,
                &IRect::new(0, 0, 7, 5),
                YuvEncoding::default(),
            );
            s.assert_untouched_outside(&IRect::EMPTY);
        }
        // Empty crop, empty dst, empty clip, crop outside the source.
        let mut s = Surface::new(16, 16);
        let all = s.canvas().bounds();
        let src = f.nv12();
        let enc = YuvEncoding::default();
        s.canvas()
            .blit_nv12(&all, &all, &src, &IRect::new(0, 0, 0, 5), enc);
        s.canvas()
            .blit_nv12(&all, &IRect::new(0, 0, 0, 9), &src, &src.bounds(), enc);
        s.canvas()
            .blit_nv12(&IRect::EMPTY, &all, &src, &src.bounds(), enc);
        s.canvas()
            .blit_nv12(&all, &all, &src, &IRect::new(7, 0, 3, 3), enc);
        s.assert_untouched_outside(&IRect::EMPTY);
    }

    #[test]
    fn extreme_geometry_does_not_panic() {
        let f = smooth_frame(1, 1);
        let big = smooth_frame(64, 64);
        let enc = YuvEncoding::default();
        let mut s = Surface::new(16, 16);
        let all = s.canvas().bounds();
        let huge = IRect::new(-1_000_000_000, -1_000_000_000, 2_000_000_000, 2_000_000_000);
        s.canvas()
            .blit_nv12(&all, &huge, &f.nv12(), &f.nv12().bounds(), enc);
        s.canvas()
            .blit_nv12(&all, &huge, &big.nv12(), &big.nv12().bounds(), enc);
        let tiny = IRect::new(3, 3, 1, 1);
        s.canvas()
            .blit_nv12(&all, &tiny, &big.nv12(), &big.nv12().bounds(), enc);
        let far = IRect::new(1_000_000_000, 5, 1_000_000_000, 4);
        s.canvas()
            .blit_nv12(&all, &far, &big.nv12(), &big.nv12().bounds(), enc);
        assert_eq!(x_byte(&s, 3, 3), 255);
    }

    // ---- packed 4:2:2 (YUYV / UYVY) -----------------------------------------

    mod packed {
        use super::super::{Rng, Surface, iw};
        use super::{assert_near, encodings, rgb_at, to_u8, x_byte, yuv_to_rgb, yuv_to_rgb_f};
        use crate::{Packed422, Packed422Order, YuvEncoding, YuvMatrix, YuvRange};
        use nitro_core::IRect;

        /// An owned YUYV frame, padded stride, tight last row.
        struct Frame {
            data: Vec<u8>,
            w: u32,
            h: u32,
            stride: u32,
        }

        impl Frame {
            /// Luma from `fy(x, y)`, chroma of group `(j, y)` from `fc(j, y)`.
            fn new(
                w: u32,
                h: u32,
                fy: impl Fn(u32, u32) -> u8,
                fc: impl Fn(u32, u32) -> (u8, u8),
            ) -> Self {
                let stride = 2 * w + 6;
                let mut data = vec![0xEE; ((h - 1) * stride + 2 * w) as usize];
                for r in 0..h {
                    for j in 0..w / 2 {
                        let o = (r * stride + 4 * j) as usize;
                        let (u, v) = fc(j, r);
                        data[o..o + 4].copy_from_slice(&[fy(2 * j, r), u, fy(2 * j + 1, r), v]);
                    }
                }
                Self { data, w, h, stride }
            }

            fn src(&self) -> Packed422<'_> {
                Packed422 {
                    data: &self.data,
                    stride: self.stride,
                    width: self.w,
                    height: self.h,
                    order: Packed422Order::Yuyv,
                }
            }

            /// The same image in UYVY byte order.
            fn swapped(&self) -> Vec<u8> {
                let mut d = self.data.clone();
                for r in 0..self.h as usize {
                    let row = &mut d[r * self.stride as usize..][..2 * self.w as usize];
                    for g in row.chunks_exact_mut(4) {
                        g.swap(0, 1);
                        g.swap(2, 3);
                    }
                }
                d
            }

            fn luma(&self, x: i32, y: i32) -> f64 {
                f64::from(self.data[y as usize * self.stride as usize + 2 * x as usize])
            }

            fn chroma(&self, j: i32, y: i32) -> (f64, f64) {
                let o = y as usize * self.stride as usize + 4 * j as usize;
                (f64::from(self.data[o + 1]), f64::from(self.data[o + 3]))
            }

            fn ref_1to1(&self, enc: YuvEncoding, x: i32, y: i32) -> (u8, u8, u8) {
                let (u, v) = self.chroma(x >> 1, y);
                yuv_to_rgb(enc, self.luma(x, y), u, v)
            }

            /// Float bilinear reference: chroma at `x = lx / 2`, `y = ly`.
            fn ref_scaled(
                &self,
                enc: YuvEncoding,
                dst: &IRect,
                sr: &IRect,
                dx: i32,
                dy: i32,
            ) -> [f64; 3] {
                let sx = (f64::from(dx - dst.x) + 0.5) * f64::from(sr.w) / f64::from(dst.w)
                    + f64::from(sr.x)
                    - 0.5;
                let sy = (f64::from(dy - dst.y) + 0.5) * f64::from(sr.h) / f64::from(dst.h)
                    + f64::from(sr.y)
                    - 0.5;
                let lerp2 = |pos: f64, first: i32, last: i32| {
                    let f = pos.floor();
                    let i = f as i32;
                    (i.clamp(first, last), (i + 1).clamp(first, last), pos - f)
                };
                let (x0, x1, tx) = lerp2(sx, sr.x, sr.right() - 1);
                let (y0, y1, ty) = lerp2(sy, sr.y, sr.bottom() - 1);
                let l = (self.luma(x0, y0) * (1.0 - tx) + self.luma(x1, y0) * tx) * (1.0 - ty)
                    + (self.luma(x0, y1) * (1.0 - tx) + self.luma(x1, y1) * tx) * ty;
                let (j0, j1, tcx) = lerp2(sx / 2.0, sr.x >> 1, (sr.right() - 1) >> 1);
                let mix = |a: (f64, f64), b: (f64, f64), t: f64| {
                    (a.0 * (1.0 - t) + b.0 * t, a.1 * (1.0 - t) + b.1 * t)
                };
                let top = mix(self.chroma(j0, y0), self.chroma(j1, y0), tcx);
                let bot = mix(self.chroma(j0, y1), self.chroma(j1, y1), tcx);
                let (u, v) = mix(top, bot, ty);
                yuv_to_rgb_f(enc, l, u, v)
            }
        }

        fn smooth(w: u32, h: u32) -> Frame {
            Frame::new(
                w,
                h,
                |x, y| (30 + (x * 3 + y * 2) % 190) as u8,
                |j, y| {
                    (
                        (60 + (j * 5 + y) % 130) as u8,
                        (200 - (j + 3 * y) % 140) as u8,
                    )
                },
            )
        }

        fn noisy(w: u32, h: u32) -> Frame {
            Frame::new(
                w,
                h,
                |x, y| (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40503)) as u8,
                |j, k| {
                    (
                        (j.wrapping_mul(97) ^ k.wrapping_mul(7919)) as u8,
                        (j.wrapping_mul(193) + k.wrapping_mul(389)) as u8,
                    )
                },
            )
        }

        fn blit(
            s: &mut Surface,
            clip: &IRect,
            dst: &IRect,
            src: &Packed422<'_>,
            sr: &IRect,
            enc: YuvEncoding,
        ) {
            s.canvas().blit_yuyv(clip, dst, src, sr, enc);
        }

        #[test]
        fn one_to_one_matches_the_float_reference() {
            let f = noisy(34, 13);
            let mut rng = Rng::new(0x1234_5678_9ABC_DEF0);
            for enc in encodings() {
                let mut s = Surface::new(40, 16);
                let all = s.canvas().bounds();
                let dst = IRect::new(3, 2, 34, 13);
                blit(&mut s, &all, &dst, &f.src(), &f.src().bounds(), enc);
                for y in 0..13 {
                    for x in 0..34 {
                        let got = rgb_at(&s, 3 + x, 2 + y);
                        assert_near(
                            got,
                            f.ref_1to1(enc, x, y),
                            1,
                            &format!("{enc:?} ({x}, {y})"),
                        );
                        assert_eq!(x_byte(&s, 3 + x, 2 + y), 255);
                    }
                }
                s.assert_untouched_outside(&dst);
                // Random single triples through a 2x1 frame.
                for _ in 0..500 {
                    let (y, u, v) = (rng.byte(), rng.byte(), rng.byte());
                    let f = Frame::new(2, 1, |_, _| y, |_, _| (u, v));
                    let mut s = Surface::new(2, 1);
                    let all = s.canvas().bounds();
                    blit(&mut s, &all, &all, &f.src(), &all, enc);
                    let want = yuv_to_rgb(enc, f64::from(y), f64::from(u), f64::from(v));
                    assert_near(rgb_at(&s, 1, 0), want, 1, &format!("{enc:?} {y}/{u}/{v}"));
                }
            }
        }

        #[test]
        fn one_to_one_with_odd_crops_uses_the_covering_group() {
            let f = Frame::new(
                18,
                7,
                |x, y| (20 + x * 11 + y * 7) as u8,
                |j, y| ((40 + j * 19 + y * 3) as u8, (220 - j * 13 - y * 5) as u8),
            );
            let crops = [
                IRect::new(0, 0, 18, 7),
                IRect::new(1, 1, 17, 6),
                IRect::new(3, 0, 13, 7),
                IRect::new(3, 1, 5, 3),
                IRect::new(17, 6, 1, 1),
                IRect::new(1, 0, 1, 7),
                IRect::new(5, 2, 12, 5),
            ];
            for enc in encodings() {
                for sr in &crops {
                    for (ox, oy) in [(0, 0), (1, 2), (4, 1)] {
                        let mut s = Surface::new(24, 12);
                        let all = s.canvas().bounds();
                        let dst = IRect::new(ox, oy, sr.w, sr.h);
                        blit(&mut s, &all, &dst, &f.src(), sr, enc);
                        for y in 0..sr.h {
                            for x in 0..sr.w {
                                let want = f.ref_1to1(enc, sr.x + x, sr.y + y);
                                assert_near(
                                    rgb_at(&s, ox + x, oy + y),
                                    want,
                                    1,
                                    &format!("{enc:?} crop {sr:?} ({x}, {y})"),
                                );
                            }
                        }
                        s.assert_untouched_outside(&dst);
                    }
                }
            }
        }

        #[test]
        fn scaled_matches_the_bilinear_reference_with_siting() {
            let frames = [smooth(18, 9), smooth(2, 1), smooth(40, 31)];
            let geoms = [
                (IRect::new(0, 0, 40, 23), None),
                (IRect::new(2, 1, 9, 5), None),
                (IRect::new(0, 0, 33, 7), Some(IRect::new(1, 1, 7, 5))),
                (IRect::new(1, 2, 13, 29), Some(IRect::new(3, 0, 5, 7))),
                (IRect::new(0, 0, 1, 1), None),
                (IRect::new(0, 0, 41, 17), None),
            ];
            for enc in encodings() {
                for f in &frames {
                    for (dst, crop) in &geoms {
                        let b = f.src().bounds();
                        let sr = crop.unwrap_or(b).intersect(&b);
                        if sr.is_empty() || (sr.w == dst.w && sr.h == dst.h) {
                            continue;
                        }
                        let mut s = Surface::new(48, 36);
                        let all = s.canvas().bounds();
                        blit(&mut s, &all, dst, &f.src(), &sr, enc);
                        for y in dst.y..dst.bottom() {
                            for x in dst.x..dst.right() {
                                let [r, g, b] = f.ref_scaled(enc, dst, &sr, x, y);
                                assert_near(
                                    rgb_at(&s, x, y),
                                    (to_u8(r), to_u8(g), to_u8(b)),
                                    2,
                                    &format!("{enc:?} {}x{} {dst:?} {sr:?} ({x}, {y})", f.w, f.h),
                                );
                            }
                        }
                        s.assert_untouched_outside(dst);
                    }
                }
            }
        }

        #[test]
        fn clipped_is_byte_identical_to_unclipped() {
            let f = smooth(38, 21);
            let mut rng = Rng::new(0xDEAD_BEEF_1234_5678);
            let cases = [
                (IRect::new(2, 1, 38, 21), IRect::new(0, 0, 38, 21)), // 1:1
                (IRect::new(1, 3, 29, 19), IRect::new(5, 1, 29, 19)), // 1:1, odd crop
                (IRect::new(-7, 2, 70, 41), IRect::new(0, 0, 38, 21)), // up
                (IRect::new(3, 3, 19, 9), IRect::new(1, 1, 35, 19)),  // down
                (IRect::new(0, 0, 51, 13), IRect::new(3, 2, 30, 17)), // mixed
                (IRect::new(0, 0, 9, 40), IRect::new(0, 0, 38, 21)),  // strong down x
            ];
            for enc in encodings() {
                for (dst, sr) in &cases {
                    let mut full = Surface::new(64, 48);
                    let all = full.canvas().bounds();
                    blit(&mut full, &all, dst, &f.src(), sr, enc);
                    for _ in 0..12 {
                        let x = iw(rng.next_u32() % 64);
                        let y = iw(rng.next_u32() % 48);
                        let w = 1 + iw(rng.next_u32() % 40);
                        let h = 1 + iw(rng.next_u32() % 30);
                        let clip = IRect::new(x, y, w, h);
                        let mut part = Surface::new(64, 48);
                        blit(&mut part, &clip, dst, &f.src(), sr, enc);
                        let inside = clip.intersect(dst).intersect(&all);
                        for py in inside.y..inside.bottom() {
                            for px in inside.x..inside.right() {
                                assert_eq!(
                                    part.px(px, py),
                                    full.px(px, py),
                                    "{enc:?} dst {dst:?} clip {clip:?} at ({px}, {py})"
                                );
                            }
                        }
                        part.assert_untouched_outside(&inside);
                    }
                }
            }
        }

        #[test]
        fn uyvy_equals_yuyv_after_swapping_bytes() {
            let f = noisy(26, 11);
            let swapped = f.swapped();
            let uyvy = Packed422 {
                data: &swapped,
                order: Packed422Order::Uyvy,
                ..f.src()
            };
            let enc = YuvEncoding::new(YuvMatrix::Bt601, YuvRange::Limited);
            let geoms = [
                (IRect::new(1, 1, 26, 11), IRect::new(0, 0, 26, 11)),
                (IRect::new(0, 2, 13, 9), IRect::new(3, 1, 13, 9)),
                (IRect::new(0, 0, 40, 30), IRect::new(0, 0, 26, 11)),
                (IRect::new(2, 0, 11, 5), IRect::new(1, 1, 23, 9)),
            ];
            for (dst, sr) in &geoms {
                let mut a = Surface::new(44, 32);
                let mut b = Surface::new(44, 32);
                let clip = IRect::new(0, 1, 30, 25);
                blit(&mut a, &clip, dst, &f.src(), sr, enc);
                blit(&mut b, &clip, dst, &uyvy, sr, enc);
                assert_eq!(a.data, b.data, "{dst:?} {sr:?}");
            }
        }

        #[test]
        fn validity_and_no_ops() {
            let f = smooth(8, 5);
            assert!(f.src().is_valid(), "tight buffer must be accepted");
            let short = Packed422 {
                data: &f.data[..f.data.len() - 1],
                ..f.src()
            };
            let odd_w = Packed422 {
                width: 7,
                ..f.src()
            };
            let bad_stride = Packed422 {
                stride: 15,
                ..f.src()
            };
            let empty = Packed422 {
                height: 0,
                ..f.src()
            };
            for bad in [short, odd_w, bad_stride, empty] {
                assert!(!bad.is_valid(), "{bad:?}");
                let mut s = Surface::new(16, 16);
                let all = s.canvas().bounds();
                blit(
                    &mut s,
                    &all,
                    &all,
                    &bad,
                    &IRect::new(0, 0, 8, 5),
                    YuvEncoding::default(),
                );
                s.assert_untouched_outside(&IRect::EMPTY);
            }
            let mut s = Surface::new(16, 16);
            let all = s.canvas().bounds();
            let src = f.src();
            let enc = YuvEncoding::default();
            blit(&mut s, &all, &all, &src, &IRect::new(0, 0, 0, 5), enc);
            blit(
                &mut s,
                &all,
                &IRect::new(0, 0, 0, 9),
                &src,
                &src.bounds(),
                enc,
            );
            blit(&mut s, &IRect::EMPTY, &all, &src, &src.bounds(), enc);
            blit(&mut s, &all, &all, &src, &IRect::new(8, 0, 3, 3), enc);
            s.assert_untouched_outside(&IRect::EMPTY);
            // Extreme geometry does not panic.
            let huge = IRect::new(-1_000_000_000, -1_000_000_000, 2_000_000_000, 2_000_000_000);
            blit(&mut s, &all, &huge, &src, &src.bounds(), enc);
            blit(
                &mut s,
                &all,
                &IRect::new(3, 3, 1, 1),
                &src,
                &src.bounds(),
                enc,
            );
            let far = IRect::new(1_000_000_000, 5, 1_000_000_000, 4);
            blit(&mut s, &all, &far, &src, &src.bounds(), enc);
            assert_eq!(x_byte(&s, 3, 3), 255);
        }
    }
}

// ---------------------------------------------------------------------------
// blit_xrgb_scaled
// ---------------------------------------------------------------------------

#[allow(clippy::many_single_char_names)] // x/y/w/h and surface pairs
mod xrgb_scaled {
    use super::{Rng, Surface, iw};
    use crate::{Image, PixelFormat};
    use nitro_core::{IRect, Rect};

    /// A smooth opaque XRGB image with garbage in byte 3.
    fn smooth(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                v[o] = (20 + x * 4 + y) as u8;
                v[o + 1] = (200 - x * 2 - y * 3) as u8;
                v[o + 2] = (60 + (x + y) * 3) as u8;
                v[o + 3] = 0x5A;
            }
        }
        v
    }

    fn img(data: &[u8], w: u32, h: u32, format: PixelFormat) -> Image<'_> {
        Image {
            data,
            width: w,
            height: h,
            stride: w * 4,
            format,
        }
    }

    #[test]
    fn agrees_with_the_generic_blit() {
        let data = smooth(31, 19);
        let src = img(&data, 31, 19, PixelFormat::Xrgb8888);
        let geoms = [
            (IRect::new(0, 0, 60, 40), IRect::new(0, 0, 31, 19)),
            (IRect::new(3, 2, 17, 11), IRect::new(0, 0, 31, 19)),
            (IRect::new(1, 1, 31, 19), IRect::new(0, 0, 31, 19)),
            (IRect::new(0, 5, 45, 7), IRect::new(4, 3, 20, 9)),
        ];
        for (dst, sr) in &geoms {
            let mut a = Surface::new(64, 48);
            let mut b = Surface::new(64, 48);
            let all = a.canvas().bounds();
            a.canvas().blit_xrgb_scaled(&all, dst, &src, sr);
            let dstf = Rect::new(dst.x as f32, dst.y as f32, dst.w as f32, dst.h as f32);
            b.canvas().blit(&all, &dstf, &src, sr, 1.0);
            for y in dst.y..dst.bottom() {
                for x in dst.x..dst.right() {
                    let (p, q) = (a.bgr(x, y), b.bgr(x, y));
                    let d = |m: u8, n: u8| (i32::from(m) - i32::from(n)).abs();
                    assert!(
                        d(p.0, q.0) <= 1 && d(p.1, q.1) <= 1 && d(p.2, q.2) <= 1,
                        "{dst:?} ({x}, {y}): {p:?} vs {q:?}"
                    );
                    assert_eq!(
                        a.data[y as usize * a.stride as usize + x as usize * 4 + 3],
                        255
                    );
                }
            }
            a.assert_untouched_outside(dst);
        }
    }

    #[test]
    fn is_clip_invariant_and_ignores_argb() {
        let data = smooth(23, 17);
        let src = img(&data, 23, 17, PixelFormat::Xrgb8888);
        let mut rng = Rng::new(0x0123_4567_89AB_CDEF);
        for dst in [
            IRect::new(-3, 2, 57, 39),
            IRect::new(4, 4, 11, 7),
            IRect::new(0, 0, 23, 17),
        ] {
            let mut full = Surface::new(48, 40);
            let all = full.canvas().bounds();
            full.canvas()
                .blit_xrgb_scaled(&all, &dst, &src, &IRect::new(1, 1, 21, 15));
            for _ in 0..20 {
                let clip = IRect::new(
                    iw(rng.next_u32() % 48),
                    iw(rng.next_u32() % 40),
                    1 + iw(rng.next_u32() % 30),
                    1 + iw(rng.next_u32() % 30),
                );
                let mut part = Surface::new(48, 40);
                part.canvas()
                    .blit_xrgb_scaled(&clip, &dst, &src, &IRect::new(1, 1, 21, 15));
                let inside = clip.intersect(&dst).intersect(&all);
                for y in inside.y..inside.bottom() {
                    for x in inside.x..inside.right() {
                        assert_eq!(part.px(x, y), full.px(x, y), "{dst:?} {clip:?} ({x}, {y})");
                    }
                }
                part.assert_untouched_outside(&inside);
            }
        }
        let argb = img(&data, 23, 17, PixelFormat::Argb8888);
        let mut s = Surface::new(16, 16);
        let all = s.canvas().bounds();
        s.canvas()
            .blit_xrgb_scaled(&all, &all, &argb, &argb.bounds());
        s.assert_untouched_outside(&IRect::EMPTY);
        // 1x1 source to a huge rect, and a huge source into one pixel.
        let one = smooth(1, 1);
        let one = img(&one, 1, 1, PixelFormat::Xrgb8888);
        let huge = IRect::new(-1_000_000_000, -1_000_000_000, 2_000_000_000, 2_000_000_000);
        s.canvas()
            .blit_xrgb_scaled(&all, &huge, &one, &one.bounds());
        s.canvas()
            .blit_xrgb_scaled(&all, &huge, &src, &src.bounds());
        s.canvas()
            .blit_xrgb_scaled(&all, &IRect::new(2, 2, 1, 1), &src, &src.bounds());
    }
}

// ---------------------------------------------------------------------------
// Destination alpha (#3898)
// ---------------------------------------------------------------------------

mod dest_alpha {
    use super::{Surface, iw};
    use crate::{Canvas, Fill, Image, Mask, Nv12, PixelFormat, YuvEncoding};
    use nitro_core::{Color, IRect, Point, Rect};

    /// One named painting op.
    type Op = (&'static str, Box<dyn Fn(&mut Canvas<'_>)>);

    /// Every painting op the crate has, each run over the whole surface.
    #[allow(clippy::too_many_lines)] // a flat table of ops
    fn ops() -> Vec<Op> {
        let all = IRect::new(0, 0, 40, 24);
        let half = Color::rgba(200, 100, 40, 128);
        let mut v: Vec<Op> = Vec::new();
        v.push((
            "fill_irect opaque",
            Box::new(move |c| c.fill_irect(&all, &IRect::new(2, 2, 11, 9), Color::rgb(1, 2, 3))),
        ));
        v.push((
            "fill_irect translucent",
            Box::new(move |c| c.fill_irect(&all, &IRect::new(3, 1, 30, 20), half)),
        ));
        v.push((
            "fill_rect aa rounded",
            Box::new(move |c| {
                c.fill_rect(
                    &all,
                    &Rect::new(1.3, 2.6, 30.2, 17.1),
                    &Fill::Solid(half),
                    5.0,
                    0.8,
                );
            }),
        ));
        v.push((
            "fill_rect opaque aa",
            Box::new(move |c| {
                c.fill_rect(
                    &all,
                    &Rect::new(4.5, 3.5, 20.0, 10.0),
                    &Fill::Solid(Color::rgb(9, 200, 90)),
                    3.0,
                    1.0,
                );
            }),
        ));
        v.push((
            "fill_rect linear",
            Box::new(move |c| {
                c.fill_rect(
                    &all,
                    &Rect::new(0.0, 0.0, 40.0, 24.0),
                    &Fill::Linear {
                        start: Point::new(0.0, 0.0),
                        end: Point::new(40.0, 0.0),
                        c0: Color::rgba(255, 0, 0, 30),
                        c1: Color::rgba(0, 0, 255, 250),
                    },
                    0.0,
                    1.0,
                );
            }),
        ));
        v.push((
            "fill_rect linear opaque",
            Box::new(move |c| {
                c.fill_rect(
                    &all,
                    &Rect::new(0.0, 0.0, 40.0, 24.0),
                    &Fill::Linear {
                        start: Point::new(0.0, 0.0),
                        end: Point::new(0.0, 24.0),
                        c0: Color::rgb(255, 0, 0),
                        c1: Color::rgb(0, 0, 255),
                    },
                    0.0,
                    1.0,
                );
            }),
        ));
        v.push((
            "stroke",
            Box::new(move |c| {
                c.stroke_rect_inside(
                    &all,
                    &Rect::new(2.25, 1.75, 33.5, 19.0),
                    1.5,
                    Color::rgba(10, 250, 60, 200),
                    0.0,
                    1.0,
                );
            }),
        ));
        v.push((
            "stroke rounded",
            Box::new(move |c| {
                c.stroke_rect_inside(
                    &all,
                    &Rect::new(2.25, 1.75, 33.5, 19.0),
                    2.0,
                    Color::rgb(10, 250, 60),
                    6.0,
                    0.7,
                );
            }),
        ));
        v.push((
            "blend_pixel_at",
            Box::new(move |c| c.blend_pixel_at(&all, 5, 5, half, 1.0)),
        ));
        v.push((
            "mask",
            Box::new(move |c| {
                let cov: Vec<u8> = (0..16 * 8).map(|i| (i * 29 % 256) as u8).collect();
                let m = Mask {
                    data: &cov,
                    w: 16,
                    h: 8,
                    stride: 16,
                };
                c.blit_mask(&all, 3, 4, &m, Color::rgb(250, 250, 250), 1.0);
                c.blit_mask(&all, 20, 10, &m, half, 1.0);
            }),
        ));
        for (name, format, scale, opacity) in [
            ("blit argb 1:1", PixelFormat::Argb8888, 1.0, 1.0),
            ("blit argb 1:1 faded", PixelFormat::Argb8888, 1.0, 0.6),
            ("blit argb scaled", PixelFormat::Argb8888, 1.7, 1.0),
            ("blit xrgb 1:1", PixelFormat::Xrgb8888, 1.0, 1.0),
            ("blit xrgb faded", PixelFormat::Xrgb8888, 1.0, 0.5),
            ("blit xrgb scaled", PixelFormat::Xrgb8888, 1.3, 1.0),
        ] {
            v.push((
                name,
                Box::new(move |c| {
                    let data = source(12, 9);
                    let img = Image {
                        data: &data,
                        width: 12,
                        height: 9,
                        stride: 48,
                        format,
                    };
                    c.blit(
                        &all,
                        &Rect::new(3.0, 2.0, 12.0 * scale, 9.0 * scale),
                        &img,
                        &img.bounds(),
                        opacity,
                    );
                }),
            ));
        }
        v.push((
            "xrgb scaled store",
            Box::new(move |c| {
                let data = source(12, 9);
                let img = Image {
                    data: &data,
                    width: 12,
                    height: 9,
                    stride: 48,
                    format: PixelFormat::Xrgb8888,
                };
                c.blit_xrgb_scaled(&all, &IRect::new(5, 5, 20, 13), &img, &img.bounds());
            }),
        ));
        v.push((
            "nv12",
            Box::new(move |c| {
                let y = vec![120u8; 16 * 8];
                let uv = vec![90u8; 16 * 4];
                let f = Nv12 {
                    y: &y,
                    y_stride: 16,
                    uv: &uv,
                    uv_stride: 16,
                    width: 16,
                    height: 8,
                };
                c.blit_nv12(
                    &all,
                    &IRect::new(1, 1, 24, 12),
                    &f,
                    &f.bounds(),
                    YuvEncoding::default(),
                );
            }),
        ));
        v
    }

    /// A 12x9-ish straight-alpha source with every kind of alpha and
    /// garbage-free colour.
    fn source(w: usize, h: usize) -> Vec<u8> {
        let mut d = vec![0u8; w * h * 4];
        for (i, p) in d.chunks_exact_mut(4).enumerate() {
            let a = match i % 5 {
                0 => 255,
                1 => 0,
                k => (k * 60) as u8,
            };
            p.copy_from_slice(&[(i * 31) as u8, (i * 17) as u8, (i * 7) as u8, a]);
        }
        d
    }

    fn surface_filled(px: [u8; 4]) -> Surface {
        let mut s = Surface::new(40, 24);
        for d in s.data.chunks_exact_mut(4) {
            d.copy_from_slice(&px);
        }
        s
    }

    #[test]
    fn every_op_over_an_opaque_canvas_leaves_alpha_255() {
        for (name, op) in ops() {
            let mut s = surface_filled([0x30, 0x60, 0x90, 255]);
            op(&mut s.canvas());
            for y in 0..iw(s.h) {
                for x in 0..iw(s.w) {
                    let o = y as usize * s.stride as usize + x as usize * 4;
                    assert_eq!(s.data[o + 3], 255, "{name} ({x},{y})");
                }
            }
        }
    }

    /// Over a hole the colour bytes are exactly what the op produces over
    /// opaque black (the colour formulas do not read destination alpha), and
    /// the alpha is the op's effective coverage: a valid premultiplied pixel
    /// (`c <= a`, within rounding).
    #[test]
    fn every_op_over_a_hole_is_premultiplied() {
        for (name, op) in ops() {
            let mut hole = surface_filled([0, 0, 0, 0]);
            let mut black = surface_filled([0, 0, 0, 255]);
            op(&mut hole.canvas());
            op(&mut black.canvas());
            for y in 0..iw(hole.h) {
                for x in 0..iw(hole.w) {
                    let o = y as usize * hole.stride as usize + x as usize * 4;
                    let (h, b) = (&hole.data[o..o + 4], &black.data[o..o + 4]);
                    assert_eq!(&h[..3], &b[..3], "{name} colour ({x},{y})");
                    let a = h[3];
                    assert!(
                        h[..3].iter().all(|&c| c <= a.saturating_add(1)),
                        "{name} ({x},{y}) not premultiplied: {h:?}"
                    );
                    assert_eq!(b[3], 255, "{name} ({x},{y})");
                }
            }
        }
    }

    #[test]
    fn translucent_fill_over_a_hole_matches_the_premultiplied_reference() {
        let mut s = surface_filled([0, 0, 0, 0]);
        let all = s.canvas().bounds();
        let c = Color::rgba(200, 100, 40, 128);
        s.canvas().fill_irect(&all, &all, c);
        let want = |v: u8| (f64::from(v) * 128.0 / 255.0).round() as i32;
        for p in s.data.chunks_exact(4).take(40) {
            assert_eq!(p[3], 128);
            for (got, v) in [(p[0], c.b), (p[1], c.g), (p[2], c.r)] {
                assert!((i32::from(got) - want(v)).abs() <= 1, "{p:?}");
            }
        }
        // A second 50 % layer: premultiplied source-over, float reference.
        s.canvas().fill_irect(&all, &all, c);
        let a2 = 128.0 + 128.0 * (1.0 - 128.0 / 255.0);
        let p = &s.data[0..4];
        assert!((f64::from(p[3]) - a2).abs() <= 1.0, "{p:?}");
        let c2 = f64::from(want(c.r)) * (2.0 - 128.0 / 255.0);
        assert!((f64::from(p[2]) - c2).abs() <= 1.5, "{p:?}");
    }

    #[test]
    fn clear_zeroes_only_inside_clip_and_rect() {
        let mut s = Surface::new(30, 20);
        s.canvas().fill_irect(
            &IRect::new(0, 0, 30, 20),
            &IRect::new(0, 0, 30, 20),
            Color::rgb(1, 2, 3),
        );
        let clip = IRect::new(4, 3, 12, 10);
        s.canvas().clear_irect(&clip, &IRect::new(-5, 6, 100, 100));
        let cleared = clip.intersect(&IRect::new(-5, 6, 100, 100));
        for y in 0..20 {
            for x in 0..30 {
                let o = y as usize * s.stride as usize + x as usize * 4;
                let p = &s.data[o..o + 4];
                if cleared.contains(x, y) {
                    assert_eq!(p, &[0, 0, 0, 0], "({x},{y})");
                } else {
                    assert_eq!(p, &[3, 2, 1, 255], "({x},{y})");
                }
            }
        }
        // Off-surface and empty rects are no-ops.
        let before = s.data.clone();
        s.canvas().clear_irect(&clip, &IRect::new(100, 100, 5, 5));
        s.canvas().clear_irect(&IRect::new(-10, -10, 5, 5), &clip);
        assert_eq!(s.data, before);
    }

    #[test]
    fn clear_never_writes_outside_clip_on_a_sentinel_canvas() {
        let mut s = Surface::new(33, 17);
        let clip = IRect::new(5, 2, 9, 7);
        s.canvas().clear_irect(&clip, &IRect::new(0, 0, 33, 17));
        s.assert_untouched_outside(&clip);
    }
}

// ---------------------------------------------------------------------------
// fill_opaque_overlaid (#3929): one store equals a store and a blend
// ---------------------------------------------------------------------------

#[test]
fn an_overlaid_opaque_fill_equals_the_fill_then_the_overlay() {
    use crate::Overlay;
    let (cw, ch) = (37, 23);
    let full = Rect::new(0.0, 0.0, 37.0, 23.0);
    let fills = [
        Fill::Solid(Color::rgb(0x12, 0x80, 0xfe)),
        Fill::Linear {
            start: Point::new(0.0, 0.0),
            end: Point::new(0.0, 23.0),
            c0: Color::rgb(10, 200, 30),
            c1: Color::rgb(250, 3, 99),
        },
        Fill::Linear {
            start: Point::new(3.0, 0.0),
            end: Point::new(30.0, 0.0),
            c0: Color::rgb(0, 0, 0),
            c1: Color::rgb(255, 255, 255),
        },
    ];
    let mut rng = Rng::new(0x3929);
    for fill in fills {
        for (color, opacity) in [
            (Color::rgba(0, 0, 0, 0xA0), 1.0),
            (Color::rgba(0, 0, 0, 0xA0), 0.5),
            (Color::rgba(0, 0, 0, 0xA0), 0.0),
            (Color::rgba(40, 90, 200, 255), 1.0),
            (Color::rgba(40, 90, 200, 17), 0.37),
        ] {
            for _ in 0..8 {
                let x = iw(rng.next_u32() % cw);
                let y = iw(rng.next_u32() % ch);
                let clip = IRect::new(x, y, iw(rng.next_u32() % cw), iw(rng.next_u32() % ch));
                let mut want = Surface::new(cw, ch);
                {
                    let mut c = want.canvas();
                    c.fill_rect(&clip, &full, &fill, 0.0, 1.0);
                    c.fill_rect(&clip, &full, &Fill::Solid(color), 0.0, opacity);
                }
                let mut got = Surface::new(cw, ch);
                assert!(got.canvas().fill_opaque_overlaid(
                    &clip,
                    &fill,
                    Overlay::new(color, opacity)
                ));
                assert_eq!(got.data, want.data, "{fill:?} {color:?} {opacity} {clip:?}");
            }
        }
    }
    let mut s = Surface::new(4, 4);
    let translucent = Fill::Solid(Color::rgba(1, 2, 3, 4));
    assert!(!s.canvas().fill_opaque_overlaid(
        &IRect::new(0, 0, 4, 4),
        &translucent,
        Overlay::new(Color::BLACK, 1.0)
    ));
    s.assert_untouched_outside(&IRect::EMPTY);
}

#[test]
fn overlay_over_matches_fill_irect_blend_for_every_alpha() {
    use crate::Overlay;
    for a in 0..=255u8 {
        let top = Color::rgba(0x33, 0x99, 0xEE, a);
        for base in [
            Color::rgb(0, 0, 0),
            Color::rgb(255, 255, 255),
            Color::rgb(7, 130, 251),
        ] {
            let mut s = Surface::new(3, 1);
            let r = IRect::new(0, 0, 3, 1);
            s.canvas().fill_irect(&r, &r, base);
            s.canvas().fill_irect(&r, &r, top);
            assert_eq!(s.bgr(1, 0), rgb(Overlay::new(top, 1.0).over(base)), "a {a}");
            assert_eq!(s.data[7], 255);
        }
    }
}
