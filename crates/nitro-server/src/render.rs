//! Placeholder scene: a vertical gradient, a 4-px frame and a vertical bar
//! that advances every flip. Enough to prove vblank pacing and damage on
//! real hardware; the raster crate replaces it in M1.
//!
//! The backend hands out two buffers per output in strict alternation, so
//! the buffer painted at frame `n` is the one painted at `n - 2`
//! ("age 2"). [`Scene`] remembers where the bar was on the last two
//! frames, restores the background there, paints the new bar, and reports
//! as damage the union of the bar on screen now (frame `n - 1`) and the
//! new one. After [`Scene::invalidate`] the next two frames repaint fully.

use nitro_kms::{BYTES_PER_PIXEL, BufferMut, Rect};

/// Width of the frame around the output, in pixels.
pub const FRAME: u32 = 4;
/// Width of the moving bar.
pub const BAR_WIDTH: u32 = 40;
/// Pixels the bar advances per flip.
pub const BAR_STEP: u32 = 8;
/// Frame colour (`0x00RRGGBB`).
pub const FRAME_COLOR: u32 = 0x00FF_FFFF;
/// Bar colour.
pub const BAR_COLOR: u32 = 0x00FF_7000;

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

/// What the background (everything but the bar) looks like at `(x, y)`.
#[must_use]
pub fn background_color(x: u32, y: u32, width: u32, height: u32) -> u32 {
    if x < FRAME || y < FRAME || x + FRAME >= width || y + FRAME >= height {
        FRAME_COLOR
    } else {
        gradient_color(y, height)
    }
}

/// Expected colour of `(x, y)` when the bar's left edge is at `bar_x`.
#[must_use]
pub fn expected_color(x: u32, y: u32, width: u32, height: u32, bar_x: u32) -> u32 {
    let in_bar = x >= bar_x && x < bar_x + BAR_WIDTH;
    let in_frame = x < FRAME || y < FRAME || x + FRAME >= width || y + FRAME >= height;
    if in_frame {
        FRAME_COLOR
    } else if in_bar {
        BAR_COLOR
    } else {
        gradient_color(y, height)
    }
}

/// Per-output animation state.
#[derive(Debug, Clone)]
pub struct Scene {
    width: u32,
    height: u32,
    bar_x: u32,
    /// Bar position painted one and two frames ago (`[n-1, n-2]`).
    history: [Option<u32>; 2],
    /// Frames left that must repaint everything.
    full_left: u8,
}

impl Scene {
    /// A scene for an output of the given size, bar at the left edge,
    /// both buffers unpainted.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            bar_x: 0,
            history: [None, None],
            full_left: 2,
        }
    }

    /// Current bar position.
    #[must_use]
    pub fn bar_x(&self) -> u32 {
        self.bar_x
    }

    /// Move the bar one step (wrapping to the left edge).
    pub fn advance(&mut self) {
        let next = self.bar_x + BAR_STEP;
        self.bar_x = if next + BAR_WIDTH > self.width {
            0
        } else {
            next
        };
    }

    /// Both buffers' contents are unknown (first frame, resume after a VT
    /// switch): repaint everything for the next two frames.
    pub fn invalidate(&mut self) {
        self.full_left = 2;
        self.history = [None, None];
    }

    /// True when a frame must be painted even if the bar is not moving.
    #[must_use]
    pub fn needs_full_repaint(&self) -> bool {
        self.full_left > 0
    }

    fn bar_rect(&self, x: u32) -> Rect {
        Rect::new(x.cast_signed(), 0, BAR_WIDTH, self.height)
    }

    /// Paint the current state into `buf` and append the damage (in output
    /// pixels, relative to what is on screen now) to `damage`.
    pub fn paint(&mut self, buf: &mut BufferMut<'_>, damage: &mut Vec<Rect>) {
        let full = Rect::new(0, 0, self.width, self.height);
        if self.full_left > 0 {
            self.full_left -= 1;
            paint_background(buf, full);
            damage.push(full);
        } else {
            // Restore the background where this buffer's old bar was
            // (two frames ago) and report the on-screen bar (one frame
            // ago) as changed.
            if let Some(old) = self.history[1] {
                paint_background(buf, self.bar_rect(old));
            }
            if let Some(on_screen) = self.history[0] {
                damage.push(self.bar_rect(on_screen));
            }
            damage.push(self.bar_rect(self.bar_x));
        }
        paint_bar(buf, self.bar_rect(self.bar_x));
        self.history = [Some(self.bar_x), self.history[0]];
    }
}

/// Paint the gradient plus frame within `rect` (clipped to the buffer).
fn paint_background(buf: &mut BufferMut<'_>, rect: Rect) {
    let Some(r) = rect.clipped_to(buf.width, buf.height) else {
        return;
    };
    let (w, h) = (buf.width, buf.height);
    let x0 = r.x.cast_unsigned();
    for y in r.y.cast_unsigned()..r.y.cast_unsigned() + r.h {
        let start = (y * buf.stride + x0 * BYTES_PER_PIXEL) as usize;
        let row = &mut buf.data[start..start + (r.w * BYTES_PER_PIXEL) as usize];
        for (i, px) in row.chunks_exact_mut(BYTES_PER_PIXEL as usize).enumerate() {
            let x = x0 + i as u32;
            px.copy_from_slice(&background_color(x, y, w, h).to_le_bytes());
        }
    }
}

/// Paint the bar, leaving the frame on top.
fn paint_bar(buf: &mut BufferMut<'_>, bar: Rect) {
    let inner = Rect::new(
        bar.x.max(FRAME.cast_signed()),
        FRAME.cast_signed(),
        bar.w,
        buf.height.saturating_sub(2 * FRAME),
    );
    let Some(mut r) = inner.clipped_to(buf.width.saturating_sub(FRAME), buf.height) else {
        return;
    };
    // `clipped_to` clipped the right edge to `width - FRAME` already.
    r.w = r.w.min(bar.w - (r.x - bar.x).cast_unsigned());
    buf.fill_rect(r, BAR_COLOR);
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_kms::Image;

    fn buffer(w: u32, h: u32) -> (Vec<u8>, u32) {
        let stride = w * BYTES_PER_PIXEL + 16;
        (vec![0u8; (stride * h) as usize], stride)
    }

    fn check_all(data: &[u8], w: u32, h: u32, stride: u32, bar_x: u32) {
        let img = Image {
            width: w,
            height: h,
            stride,
            data: data.to_vec(),
        };
        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    img.pixel(x, y),
                    expected_color(x, y, w, h, bar_x),
                    "pixel ({x},{y}) with bar at {bar_x}"
                );
            }
        }
    }

    #[test]
    fn full_paint_matches_expected_colors() {
        let (w, h) = (100, 30);
        let (mut data, stride) = buffer(w, h);
        let mut scene = Scene::new(w, h);
        let mut damage = Vec::new();
        scene.paint(
            &mut BufferMut {
                width: w,
                height: h,
                stride,
                data: &mut data,
            },
            &mut damage,
        );
        assert_eq!(damage, [Rect::new(0, 0, w, h)]);
        check_all(&data, w, h, stride, 0);
    }

    #[test]
    fn incremental_paint_restores_old_bar_on_age_2_buffer() {
        let (w, h) = (120, 20);
        let (mut a, stride) = buffer(w, h);
        let (mut b, _) = buffer(w, h);
        let mut scene = Scene::new(w, h);
        let mut damage = Vec::new();
        let bufs = [&mut a, &mut b];
        for frame in 0..6u32 {
            let buf = &mut *bufs[(frame % 2) as usize];
            damage.clear();
            scene.paint(
                &mut BufferMut {
                    width: w,
                    height: h,
                    stride,
                    data: buf,
                },
                &mut damage,
            );
            let bar_x = scene.bar_x();
            check_all(buf, w, h, stride, bar_x);
            if frame >= 2 {
                let prev = bar_x - BAR_STEP;
                assert_eq!(
                    damage,
                    [
                        Rect::new(prev.cast_signed(), 0, BAR_WIDTH, h),
                        Rect::new(bar_x.cast_signed(), 0, BAR_WIDTH, h)
                    ]
                );
            } else {
                assert_eq!(damage, [Rect::new(0, 0, w, h)]);
            }
            scene.advance();
        }
    }

    #[test]
    fn bar_wraps() {
        let mut scene = Scene::new(64, 8);
        let mut seen = vec![scene.bar_x()];
        for _ in 0..5 {
            scene.advance();
            seen.push(scene.bar_x());
        }
        assert_eq!(seen, [0, 8, 16, 24, 0, 8]);
    }

    #[test]
    fn invalidate_forces_two_full_frames() {
        let mut scene = Scene::new(64, 8);
        let (mut data, stride) = buffer(64, 8);
        let mut damage = Vec::new();
        let mut paint = |scene: &mut Scene, damage: &mut Vec<Rect>| {
            damage.clear();
            scene.paint(
                &mut BufferMut {
                    width: 64,
                    height: 8,
                    stride,
                    data: &mut data,
                },
                damage,
            );
        };
        paint(&mut scene, &mut damage);
        paint(&mut scene, &mut damage);
        assert!(!scene.needs_full_repaint());
        paint(&mut scene, &mut damage);
        assert_ne!(damage, [Rect::new(0, 0, 64, 8)]);
        scene.invalidate();
        assert!(scene.needs_full_repaint());
        paint(&mut scene, &mut damage);
        assert_eq!(damage, [Rect::new(0, 0, 64, 8)]);
        assert!(scene.needs_full_repaint());
        paint(&mut scene, &mut damage);
        assert!(!scene.needs_full_repaint());
    }
}
