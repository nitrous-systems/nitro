//! The desktop background: a vertical gradient inside a thin frame.
//!
//! M0 painted a gradient, a frame and a moving bar straight into the KMS
//! buffer; M1 keeps only the first two. The background is what the server
//! itself owns — everything else on screen is a client's scene nodes drawn
//! on top of it by [`crate::frame`] — and it earns its place twice over: a
//! screenshot of an empty desktop is still recognisably *this* server, and
//! the exact per-pixel colours give the integration tests something to
//! assert against outside every window.
//!
//! The functions are pure so tests can predict any pixel without running a
//! frame: [`background_color`] answers "what should `(x, y)` be with
//! nothing on top?".

use nitro_core::{Color, IRect};
use nitro_raster::Canvas;

/// Width of the frame around the output, in pixels.
pub const FRAME: u32 = 4;
/// Frame colour (`0x00RRGGBB`).
pub const FRAME_COLOR: u32 = 0x00FF_FFFF;

/// Gradient colour of row `y` in an output `height` rows tall: dark blue
/// at the top, light cyan at the bottom.
#[must_use]
pub fn gradient_color(y: u32, height: u32) -> u32 {
    let t = (u64::from(y) * 255 / u64::from(height.max(2) - 1)) as u32;
    let red = 16 + t * 48 / 255;
    let green = 32 + t * 160 / 255;
    let blue = 96 + t * 159 / 255;
    (red << 16) | (green << 8) | blue
}

/// What the background looks like at `(x, y)` on a `width × height` output.
#[must_use]
pub fn background_color(x: u32, y: u32, width: u32, height: u32) -> u32 {
    if x < FRAME || y < FRAME || x + FRAME >= width || y + FRAME >= height {
        FRAME_COLOR
    } else {
        gradient_color(y, height)
    }
}

/// The `0x00RRGGBB` constants above as a [`Color`].
///
/// `Color::from_u32` reads `0xRRGGBBAA`, and the desktop's colours predate
/// it: they are the `XRGB8888` words the M0 renderer wrote straight into a
/// dumb buffer, so they carry no alpha byte at all.
#[must_use]
pub fn color(xrgb: u32) -> Color {
    Color::rgb((xrgb >> 16) as u8, (xrgb >> 8) as u8, xrgb as u8)
}

/// Paint the background inside `clip`.
///
/// One `fill_irect` per row: the gradient is constant along a row, so the
/// rasterizer's opaque store path writes each row in one pass and the whole
/// background costs one pass over the damaged pixels. The frame is painted
/// by the same loop (it is part of [`background_color`]) rather than as
/// four extra rects, because a damage rect is usually far from any edge and
/// the row-wise test is a single comparison.
pub fn paint_background(canvas: &mut Canvas<'_>, clip: &IRect, width: u32, height: u32) {
    let area = clip.intersect(&canvas.bounds());
    if area.is_empty() {
        return;
    }
    let frame = FRAME.cast_signed();
    let (w, h) = (width.cast_signed(), height.cast_signed());
    for y in area.y..area.bottom() {
        let row = IRect::new(area.x, y, area.w, 1);
        if y < frame || y + frame >= h {
            canvas.fill_irect(&area, &row, color(FRAME_COLOR));
            continue;
        }
        // Interior row: left frame, gradient, right frame. Each piece is
        // clipped by `fill_irect`, so an empty one costs nothing.
        let gradient = color(gradient_color(y.cast_unsigned(), height));
        canvas.fill_irect(&area, &IRect::new(0, y, frame, 1), color(FRAME_COLOR));
        canvas.fill_irect(&area, &IRect::new(frame, y, w - 2 * frame, 1), gradient);
        canvas.fill_irect(
            &area,
            &IRect::new(w - frame, y, frame, 1),
            color(FRAME_COLOR),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_pixel(data: &[u8], stride: u32, x: u32, y: u32) -> u32 {
        let o = (y * stride + x * 4) as usize;
        u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) & 0x00FF_FFFF
    }

    #[test]
    fn frame_surrounds_the_gradient() {
        let (w, h) = (40, 20);
        assert_eq!(background_color(0, 0, w, h), FRAME_COLOR);
        assert_eq!(background_color(w - 1, h - 1, w, h), FRAME_COLOR);
        assert_eq!(background_color(FRAME - 1, 10, w, h), FRAME_COLOR);
        assert_eq!(background_color(FRAME, 10, w, h), gradient_color(10, h));
        assert_ne!(gradient_color(0, h), gradient_color(h - 1, h));
    }

    #[test]
    fn painting_matches_background_color_and_respects_the_clip() {
        let (w, h) = (32, 16);
        let stride = w * 4 + 16;
        let mut data = vec![0xEEu8; (stride * h) as usize];
        let mut canvas = Canvas::new(&mut data, w, h, stride);
        let clip = IRect::new(8, 4, 10, 6);
        paint_background(&mut canvas, &clip, w, h);
        for y in 0..h {
            for x in 0..w {
                let got = canvas_pixel(&data, stride, x, y);
                let inside = clip.contains(x.cast_signed(), y.cast_signed());
                if inside {
                    assert_eq!(got, background_color(x, y, w, h), "({x},{y})");
                } else {
                    assert_eq!(got, 0x00EE_EEEE, "wrote outside the clip at ({x},{y})");
                }
            }
        }
    }
}
