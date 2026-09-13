//! The software cursor: a 24×24 ARGB arrow the server paints over the
//! composited scene, last, every frame that shows it.
//!
//! M1 has no hardware cursor plane. That is a deliberate simplification —
//! a KMS cursor plane is a second scanout surface with its own format,
//! size and position constraints per driver, and it buys latency we do not
//! yet measure. It also has a property that makes it actively unhelpful
//! during bring-up: the plane is composited by the display engine, not
//! into the framebuffer, so it does **not** appear in a screenshot taken
//! by dumping the back buffer (see `docs/testbox.md`). Every visual test
//! run over SSH would lose the pointer exactly when it matters — when
//! checking that the pointer is where the input stack thinks it is.
//!
//! So the cursor is just another paint: an image blitted after the scene,
//! inside the frame's damage clip, with the same source-over blend as any
//! other surface. The cost is one 24×24 blit per frame plus the damage of
//! the previous and current cursor rects; the benefit is that what a
//! screenshot shows is what the display shows, and that there is one
//! composition path rather than two.
//!
//! The bitmap is compiled in rather than loaded: a cursor theme is a file
//! format, a search path and a fallback policy, none of which belong in
//! the server's first milestone. [`MASK`] is ASCII art — `#` outline, `.`
//! interior, space transparent — converted once, in [`Cursor::new`], into
//! the straight-alpha `[B, G, R, A]` bytes that
//! [`nitro_raster::PixelFormat::Argb8888`] expects. Editing the arrow means
//! editing the art, which is the point.
//!
//! The arrow is the X11 `left_ptr` shape: tip at the top-left corner, a
//! vertical left edge, a diagonal right edge down to the notch, and a tail
//! running down and to the right. The hotspot is the tip, at image pixel
//! `(0, 0)`, so the covered rect is simply the 24×24 square whose top-left
//! corner is the pointer position — no per-frame hotspot arithmetic beyond
//! [`Cursor::rect`].

use nitro_core::{IRect, Rect};
use nitro_raster::{Canvas, Image, PixelFormat};

/// Width and height of the cursor image in pixels.
pub const CURSOR_SIZE: i32 = 24;

/// Where the arrow's tip sits inside the image (hotspot), in pixels.
pub const HOTSPOT: (i32, i32) = (0, 0);

/// Bytes per ARGB8888 pixel.
const BPP: usize = 4;

/// The arrow, one byte per pixel: `#` outline, `.` interior, space
/// transparent. Exactly [`CURSOR_SIZE`] rows of [`CURSOR_SIZE`] bytes;
/// [`Cursor::new`] asserts as much in debug builds.
const MASK: [&[u8; 24]; 24] = [
    b"#                       ",
    b"##                      ",
    b"#.#                     ",
    b"#..#                    ",
    b"#...#                   ",
    b"#....#                  ",
    b"#.....#                 ",
    b"#......#                ",
    b"#.......#               ",
    b"#........#              ",
    b"#.........#             ",
    b"#..........#            ",
    b"#.....######            ",
    b"#....##..#              ",
    b"#...#  #..#             ",
    b"#..#    #..#            ",
    b"#.#      #..#           ",
    b"##        #..#          ",
    b"#          #..#         ",
    b"            #..#        ",
    b"             #..#       ",
    b"              #..#      ",
    b"               ####     ",
    b"                        ",
];

/// The cursor's ARGB8888 pixels, built once.
///
/// One allocation, at startup: `CURSOR_SIZE * CURSOR_SIZE * 4` bytes. The
/// value is immutable afterwards, so painting never allocates and never
/// touches the mask again.
#[derive(Debug, Clone)]
pub struct Cursor {
    /// `[B, G, R, A]` per pixel, straight alpha, row-major, tightly packed.
    pixels: Vec<u8>,
}

impl Cursor {
    /// Build the arrow bitmap.
    #[must_use]
    pub fn new() -> Self {
        let side = CURSOR_SIZE as usize;
        debug_assert_eq!(MASK.len(), side, "mask must have CURSOR_SIZE rows");
        let mut pixels = vec![0u8; side * side * BPP];
        for (y, row) in MASK.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                // Outline is opaque black, interior opaque white, and
                // anything else fully transparent. Straight alpha: the
                // colour of a transparent pixel is irrelevant, but zero
                // keeps the buffer boring to look at in a hex dump.
                let argb = match cell {
                    b'#' => [0x00, 0x00, 0x00, 0xFF],
                    b'.' => [0xFF, 0xFF, 0xFF, 0xFF],
                    _ => [0x00, 0x00, 0x00, 0x00],
                };
                let off = (y * side + x) * BPP;
                pixels[off..off + BPP].copy_from_slice(&argb);
            }
        }
        Self { pixels }
    }

    /// The device-pixel rectangle the cursor covers with its hotspot at
    /// `(x, y)` device pixels.
    ///
    /// The frame loop damages this rect at the old and the new pointer
    /// position, which is why it is a free function of the position rather
    /// than a method: the old rect outlives no `Cursor` borrow.
    #[must_use]
    pub fn rect(x: i32, y: i32) -> IRect {
        IRect::new(x - HOTSPOT.0, y - HOTSPOT.1, CURSOR_SIZE, CURSOR_SIZE)
    }

    /// Paint the cursor with its hotspot at `(x, y)`, clipped to `clip`.
    ///
    /// The destination is the 24×24 rect from [`Cursor::rect`] at integer
    /// coordinates, so the blit is 1:1 and integer-aligned and
    /// [`Canvas::blit`] takes its nearest-neighbour path: no filtering, no
    /// resampling blur, one source pixel per destination pixel.
    pub fn paint(&self, canvas: &mut Canvas<'_>, clip: &IRect, x: i32, y: i32) {
        let r = Self::rect(x, y);
        let dst = Rect::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32);
        let side = CURSOR_SIZE as u32;
        let image = Image {
            data: &self.pixels,
            width: side,
            height: side,
            stride: side * BPP as u32,
            format: PixelFormat::Argb8888,
        };
        let src = IRect::new(0, 0, CURSOR_SIZE, CURSOR_SIZE);
        canvas.blit(clip, &dst, &image, &src, 1.0);
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{CURSOR_SIZE, Cursor, HOTSPOT};
    use nitro_core::IRect;
    use nitro_raster::Canvas;

    /// Colour every pixel of a canvas buffer with, so that "did anything
    /// outside the clip change?" is a byte comparison.
    const SENTINEL: [u8; 4] = [0x11, 0x22, 0x33, 0xFF];

    fn pixel(c: &Cursor, x: usize, y: usize) -> [u8; 4] {
        let off = (y * CURSOR_SIZE as usize + x) * 4;
        c.pixels[off..off + 4].try_into().expect("4 bytes")
    }

    #[test]
    fn image_has_the_documented_size() {
        let c = Cursor::new();
        assert_eq!(c.pixels.len(), (CURSOR_SIZE * CURSOR_SIZE * 4) as usize);
    }

    #[test]
    fn tip_pixel_is_opaque() {
        let c = Cursor::new();
        let (hx, hy) = HOTSPOT;
        assert_eq!(pixel(&c, hx as usize, hy as usize)[3], 0xFF);
        // The tip is outline, so black.
        assert_eq!(pixel(&c, 0, 0), [0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn outside_the_arrow_is_transparent() {
        let c = Cursor::new();
        // Top-right corner: as far from the arrow as the image gets.
        assert_eq!(pixel(&c, 23, 0)[3], 0x00);
        // Bottom-left corner, below where the left edge ends.
        assert_eq!(pixel(&c, 0, 23)[3], 0x00);
    }

    #[test]
    fn interior_is_white() {
        let c = Cursor::new();
        // Row 6 is `#.....#`: column 3 is interior.
        assert_eq!(pixel(&c, 3, 6), [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn rect_is_the_image_square_at_the_hotspot() {
        let r = Cursor::rect(100, 50);
        assert_eq!(r, IRect::new(100 - HOTSPOT.0, 50 - HOTSPOT.1, 24, 24));
        assert_eq!(r.right(), 124 - HOTSPOT.0);
        assert_eq!(r.bottom(), 74 - HOTSPOT.1);
        // Negative positions stay well-formed: the blit clips.
        assert_eq!(Cursor::rect(-5, -7), IRect::new(-5, -7, 24, 24));
    }

    #[test]
    fn paint_writes_inside_the_clip_and_nowhere_else() {
        let (w, h) = (64usize, 64usize);
        let mut buf: Vec<u8> = SENTINEL.iter().copied().cycle().take(w * h * 4).collect();
        let before = buf.clone();
        let clip = IRect::new(10, 10, 12, 12);
        {
            let mut canvas = Canvas::new(&mut buf, w as u32, h as u32, (w * 4) as u32);
            Cursor::new().paint(&mut canvas, &clip, 8, 8);
        }

        let mut changed_inside = 0u32;
        for y in 0..h {
            for x in 0..w {
                let off = (y * w + x) * 4;
                let now = &buf[off..off + 4];
                let was = &before[off..off + 4];
                let inside = clip.contains(i32::try_from(x).unwrap(), i32::try_from(y).unwrap());
                if inside {
                    changed_inside += u32::from(now != was);
                } else {
                    assert_eq!(now, was, "wrote outside the clip at ({x}, {y})");
                }
            }
        }
        assert!(changed_inside > 0, "painted nothing inside the clip");
    }

    #[test]
    fn paint_far_from_the_clip_is_a_no_op() {
        let (w, h) = (64usize, 64usize);
        let mut buf: Vec<u8> = SENTINEL.iter().copied().cycle().take(w * h * 4).collect();
        let before = buf.clone();
        let clip = IRect::new(0, 0, 8, 8);
        {
            let mut canvas = Canvas::new(&mut buf, w as u32, h as u32, (w * 4) as u32);
            Cursor::new().paint(&mut canvas, &clip, 40, 40);
        }
        assert_eq!(buf, before);
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Cursor::default().pixels, Cursor::new().pixels);
    }
}
