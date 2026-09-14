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
//! Since M4-F the gradient's two stops are [`Role::DesktopTop`] and
//! [`Role::DesktopBottom`] — the **same roles `nitro-wallpaper` paints**,
//! which is the point: on a desktop with no wallpaper running, or in the
//! moment before it has painted, what shows through is the same colour it
//! is about to be covered with rather than a second opinion about what a
//! backdrop looks like.
//!
//! The functions are pure so tests can predict any pixel without running a
//! frame: [`background_color`] answers "what should `(x, y)` be with
//! nothing on top?".

use nitro_core::{Color, IRect, Palette, Role};
use nitro_raster::Canvas;

/// Width of the frame around the output, in pixels.
pub const FRAME: u32 = 4;

/// The frame around the output.
///
/// White, and deliberately *not* a palette role. It is not decoration: it
/// is an M0 debugging aid that makes a screenshot of an empty desktop
/// recognisably this server's, and it has to stay legible against both
/// schemes' gradients rather than follow either. A role for it would be a
/// role nobody would ever set.
pub const FRAME_COLOR: Color = Color::WHITE;

/// The background's gradient colour at row `y` of an output `height` rows
/// tall: [`Role::DesktopTop`] at the top, [`Role::DesktopBottom`] at the
/// bottom.
///
/// Interpolated in sRGB space, like every other gradient in this tree
/// (`nitro-raster`'s `Fill::Linear` does the same): a linear-light ramp
/// would be more correct and would not match what a client's own
/// gradient does, and the two meeting at a window edge is the thing that
/// would be visible.
#[must_use]
pub fn gradient_color(y: u32, height: u32, palette: &Palette) -> Color {
    let (top, bottom) = (
        palette.get(Role::DesktopTop),
        palette.get(Role::DesktopBottom),
    );
    let t = (u64::from(y) * 255 / u64::from(height.max(2) - 1)) as u32;
    Color::rgb(
        lerp(top.r, bottom.r, t),
        lerp(top.g, bottom.g, t),
        lerp(top.b, bottom.b, t),
    )
}

/// One channel, `a` at `t = 0` and `b` at `t = 255`.
fn lerp(a: u8, b: u8, t: u32) -> u8 {
    let (a, b) = (i32::from(a), i32::from(b));
    (a + (b - a) * t.cast_signed() / 255) as u8
}

/// What the background looks like at `(x, y)` on a `width × height`
/// output under `palette`.
#[must_use]
pub fn background_color(x: u32, y: u32, width: u32, height: u32, palette: &Palette) -> Color {
    if x < FRAME || y < FRAME || x + FRAME >= width || y + FRAME >= height {
        FRAME_COLOR
    } else {
        gradient_color(y, height, palette)
    }
}

/// Paint the background inside `clip`.
///
/// One `fill_irect` per row: the gradient is constant along a row, so the
/// rasterizer's opaque store path writes each row in one pass and the whole
/// background costs one pass over the damaged pixels. The frame is painted
/// by the same loop (it is part of [`background_color`]) rather than as
/// four extra rects, because a damage rect is usually far from any edge and
/// the row-wise test is a single comparison.
pub fn paint_background(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    width: u32,
    height: u32,
    palette: &Palette,
) {
    let area = clip.intersect(&canvas.bounds());
    if area.is_empty() {
        return;
    }
    let frame = FRAME.cast_signed();
    let (w, h) = (width.cast_signed(), height.cast_signed());
    for y in area.y..area.bottom() {
        let row = IRect::new(area.x, y, area.w, 1);
        if y < frame || y + frame >= h {
            canvas.fill_irect(&area, &row, FRAME_COLOR);
            continue;
        }
        // Interior row: left frame, gradient, right frame. Each piece is
        // clipped by `fill_irect`, so an empty one costs nothing.
        let gradient = gradient_color(y.cast_unsigned(), height, palette);
        canvas.fill_irect(&area, &IRect::new(0, y, frame, 1), FRAME_COLOR);
        canvas.fill_irect(&area, &IRect::new(frame, y, w - 2 * frame, 1), gradient);
        canvas.fill_irect(&area, &IRect::new(w - frame, y, frame, 1), FRAME_COLOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_pixel(data: &[u8], stride: u32, x: u32, y: u32) -> u32 {
        let o = (y * stride + x * 4) as usize;
        u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) & 0x00FF_FFFF
    }

    /// A `Color` as the `0x00RRGGBB` word a canvas readback gives.
    fn word(c: Color) -> u32 {
        (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)
    }

    #[test]
    fn frame_surrounds_the_gradient() {
        let (w, h) = (40, 20);
        let p = Palette::default();
        assert_eq!(background_color(0, 0, w, h, &p), FRAME_COLOR);
        assert_eq!(background_color(w - 1, h - 1, w, h, &p), FRAME_COLOR);
        assert_eq!(background_color(FRAME - 1, 10, w, h, &p), FRAME_COLOR);
        assert_eq!(
            background_color(FRAME, 10, w, h, &p),
            gradient_color(10, h, &p)
        );
        assert_ne!(gradient_color(0, h, &p), gradient_color(h - 1, h, &p));
    }

    #[test]
    fn the_gradient_is_the_desktop_roles_and_follows_the_scheme() {
        // The server's own backdrop and `nitro-wallpaper`'s gradient read
        // the *same two roles*, which is what stops the uncovered desktop
        // being a second opinion about what a backdrop looks like.
        let h = 100;
        for p in [Palette::light(), Palette::dark()] {
            assert_eq!(gradient_color(0, h, &p), p.get(Role::DesktopTop));
            assert_eq!(gradient_color(h - 1, h, &p), p.get(Role::DesktopBottom));
        }
        // And the two schemes really produce different pixels, or every
        // assertion above would hold for a gradient that ignored the
        // palette entirely.
        assert_ne!(
            gradient_color(50, h, &Palette::light()),
            gradient_color(50, h, &Palette::dark())
        );
    }

    #[test]
    fn a_one_row_output_does_not_divide_by_zero() {
        // `height.max(2) - 1` is why; a fake backend can be asked for
        // anything, and a panic in the paint path takes the desktop down.
        let p = Palette::default();
        assert_eq!(gradient_color(0, 1, &p), p.get(Role::DesktopTop));
        assert_eq!(gradient_color(0, 0, &p), p.get(Role::DesktopTop));
    }

    #[test]
    fn painting_matches_background_color_and_respects_the_clip() {
        let (w, h) = (32, 16);
        let stride = w * 4 + 16;
        let mut data = vec![0xEEu8; (stride * h) as usize];
        let mut canvas = Canvas::new(&mut data, w, h, stride);
        let clip = IRect::new(8, 4, 10, 6);
        let p = Palette::default();
        paint_background(&mut canvas, &clip, w, h, &p);
        for y in 0..h {
            for x in 0..w {
                let got = canvas_pixel(&data, stride, x, y);
                let inside = clip.contains(x.cast_signed(), y.cast_signed());
                if inside {
                    assert_eq!(got, word(background_color(x, y, w, h, &p)), "({x},{y})");
                } else {
                    assert_eq!(got, 0x00EE_EEEE, "wrote outside the clip at ({x},{y})");
                }
            }
        }
    }
}
