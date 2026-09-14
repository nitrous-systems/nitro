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
                            for b in &s.data {
                                hash ^= u64::from(*b);
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
