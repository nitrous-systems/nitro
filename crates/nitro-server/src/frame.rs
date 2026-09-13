//! The frame path: scene damage in, painted and committed pixels out.
//!
//! # The age-2 rule
//!
//! The backend owns two buffers per output and alternates them strictly, so
//! the buffer handed out at frame `n` is the one that was on screen at
//! frame `n - 2`. Repainting only *this* frame's damage would therefore
//! leave the previous frame's changes stale in it. The region actually
//! painted is `damage(n) ∪ damage(n - 1)`, and that same region is what is
//! handed to `commit` as `FB_DAMAGE_CLIPS`: it is exactly the set of pixels
//! that differ between what this buffer holds and what must be on screen.
//! [`OutputState`] keeps the one-frame history that makes this work, and
//! [`OutputState::invalidate`] forces two full frames after a resume, a
//! modeset or a new output, when both buffers hold unknown pixels.
//!
//! # Painting one rect
//!
//! For each rect of the region: the server's background first (unless an
//! opaque client rect covers the whole thing — [`PaintItem::opaque_cover`]
//! is the scene's conservative promise about that), then the scene's paint
//! list clipped to the rect, then the software cursor last. The rasterizer
//! never writes outside the clip it was given, so one rect cannot smear
//! into another.
//!
//! # Deadlines
//!
//! A client that asked for a frame callback is told when to aim for:
//! [`frame_deadline`] extrapolates from the last vblank timestamp and the
//! output's refresh interval, minus a margin that is the client's share of
//! the frame. Missing the deadline costs a frame, so the margin is
//! deliberately generous.

use std::time::Duration;

use nitro_core::{Color, Damage, IRect, Rect};
use nitro_kms::{BufferMut, OutputId as KmsOutputId};
use nitro_raster::{Canvas, Image as RasterImage, PixelFormat};
use nitro_scene::{Fill as SceneFill, OutputId, PaintItem, PaintKind, Scene};
use nitro_wire::types::format;

use crate::cursor::Cursor;
use crate::render::paint_background;

/// How long before the next vblank a client should have committed, so the
/// server still has a whole rasterization pass left. Two milliseconds is
/// about a third of the paint budget measured on the test box.
pub const FRAME_MARGIN_NS: u64 = 2_000_000;

/// Per-output frame bookkeeping: damage, the one-frame history the age-2
/// rule needs, and the vblank clock the frame deadlines come from.
#[derive(Debug)]
pub struct OutputState {
    /// The KMS id, which is also what `Presented` reports to clients.
    pub kms_id: KmsOutputId,
    /// The scene's id for the same output.
    pub scene_id: OutputId,
    /// Output size in device pixels.
    pub width: u32,
    /// Output size in device pixels.
    pub height: u32,
    /// Damage accumulated since the last painted frame.
    pub damage: Damage,
    /// The region painted into the *other* buffer one frame ago.
    pub previous: Vec<IRect>,
    /// Frames that must still repaint everything (both buffers unknown).
    pub full_left: u8,
    /// Set when `commit` failed: the damage was kept and the next event
    /// must retry, or this output would stall until the next resume.
    pub retry: bool,
    /// Refresh interval in nanoseconds, from the mode.
    pub refresh_ns: u32,
    /// `CLOCK_MONOTONIC` timestamp of the last vblank, in nanoseconds.
    pub last_vblank_ns: u64,
    /// Vblank counter of the last flip.
    pub last_sequence: u64,
    /// Commit serials whose mutations are in the buffer now in flight,
    /// reported as `Presented` when it lands.
    pub in_flight: Vec<(u32, u32)>,
    /// Commit serials painted into the buffer being prepared.
    pub painting: Vec<(u32, u32)>,
    /// Newest input timestamp whose effect is in the buffer in flight.
    pub in_flight_input_ns: u64,
    /// Newest input timestamp consumed by the frame being painted.
    pub painting_input_ns: u64,
}

impl OutputState {
    /// A fresh output: everything unknown, so the first two frames repaint
    /// in full.
    #[must_use]
    pub fn new(
        kms_id: KmsOutputId,
        scene_id: OutputId,
        width: u32,
        height: u32,
        refresh_mhz: u32,
    ) -> Self {
        Self {
            kms_id,
            scene_id,
            width,
            height,
            damage: Damage::new(),
            previous: Vec::new(),
            full_left: 2,
            retry: false,
            refresh_ns: refresh_ns(refresh_mhz),
            last_vblank_ns: 0,
            last_sequence: 0,
            in_flight: Vec::new(),
            painting: Vec::new(),
            in_flight_input_ns: 0,
            painting_input_ns: 0,
        }
    }

    /// The whole output as a rect.
    #[must_use]
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width.cast_signed(), self.height.cast_signed())
    }

    /// Both buffers hold unknown pixels (first frame, resume, modeset):
    /// repaint everything for the next two frames.
    pub fn invalidate(&mut self) {
        self.full_left = 2;
        self.previous.clear();
        self.damage.clear();
        self.damage.add(self.bounds());
    }

    /// Whether a frame would put anything new on screen.
    #[must_use]
    pub fn needs_paint(&self) -> bool {
        self.full_left > 0 || self.retry || !self.damage.is_empty()
    }

    /// The region to paint into the (age-2) back buffer: this frame's
    /// damage plus the region painted one frame ago.
    #[must_use]
    pub fn repaint_region(&self) -> Vec<IRect> {
        if self.full_left > 0 {
            return vec![self.bounds()];
        }
        let mut region = Damage::new();
        for r in self.damage.rects() {
            region.add(*r);
        }
        for r in &self.previous {
            region.add(*r);
        }
        region.take()
    }

    /// Note that a frame covering `region` was painted and committed.
    pub fn committed(&mut self, region: Vec<IRect>) {
        self.full_left = self.full_left.saturating_sub(1);
        self.previous = region;
        self.damage.clear();
        self.retry = false;
        self.in_flight = std::mem::take(&mut self.painting);
        self.in_flight_input_ns = self.painting_input_ns;
        self.painting_input_ns = 0;
    }

    /// Note that the commit failed: keep the damage (and whatever the
    /// region added to it) so the next event retries instead of stranding
    /// the output. The paint that already happened is not wasted — the back
    /// buffer holds it — but the buffers did not swap, so the *next* paint
    /// must cover the same region again.
    pub fn commit_failed(&mut self, region: &[IRect]) {
        for r in region {
            self.damage.add(*r);
        }
        self.retry = true;
    }

    /// When the next vblank is expected, in `CLOCK_MONOTONIC` nanoseconds.
    #[must_use]
    pub fn next_vblank_ns(&self, now_ns: u64) -> u64 {
        next_vblank(self.last_vblank_ns, self.refresh_ns, now_ns)
    }

    /// The deadline a client asking for a frame callback is given.
    #[must_use]
    pub fn frame_deadline_ns(&self, now_ns: u64) -> u64 {
        frame_deadline(self.last_vblank_ns, self.refresh_ns, now_ns)
    }
}

/// Nanoseconds per frame for a mode given in millihertz. A mode with no
/// refresh rate (or a nonsense one) is treated as 60 Hz rather than
/// producing a division by zero in the deadline maths.
#[must_use]
pub fn refresh_ns(refresh_mhz: u32) -> u32 {
    if refresh_mhz < 1_000 {
        return 16_666_667;
    }
    (1_000_000_000_000u64 / u64::from(refresh_mhz)) as u32
}

/// The first expected vblank strictly after `now_ns`, extrapolated from the
/// last one. Before any flip has been seen there is no phase to extrapolate
/// from, so the answer is simply one refresh from now.
#[must_use]
pub fn next_vblank(last_vblank_ns: u64, refresh_ns: u32, now_ns: u64) -> u64 {
    let period = u64::from(refresh_ns).max(1);
    if last_vblank_ns == 0 || last_vblank_ns > now_ns {
        return now_ns + period;
    }
    let elapsed = now_ns - last_vblank_ns;
    last_vblank_ns + (elapsed / period + 1) * period
}

/// The frame deadline for a client: the next expected vblank minus
/// [`FRAME_MARGIN_NS`], but never in the past — a client told to aim for a
/// moment that has already gone would only busy-loop. When the margin would
/// take the deadline behind `now`, aim for the vblank after it.
#[must_use]
pub fn frame_deadline(last_vblank_ns: u64, refresh_ns: u32, now_ns: u64) -> u64 {
    let period = u64::from(refresh_ns).max(1);
    let mut vblank = next_vblank(last_vblank_ns, refresh_ns, now_ns);
    while vblank.saturating_sub(FRAME_MARGIN_NS) <= now_ns {
        vblank += period;
    }
    vblank - FRAME_MARGIN_NS
}

/// Total area of a region, in pixels, for the `damage_px` statistic.
#[must_use]
pub fn region_area(region: &[IRect]) -> u64 {
    region.iter().map(|r| r.area().cast_unsigned()).sum()
}

/// Where the cursor is and whether to draw it.
#[derive(Debug, Clone, Copy)]
pub struct CursorState {
    /// Hotspot position in device pixels.
    pub x: i32,
    /// Hotspot position in device pixels.
    pub y: i32,
    /// Whether the cursor is drawn at all (no pointer device: no cursor).
    pub visible: bool,
}

/// Paint `region` of one output into `buf`.
///
/// Returns the microseconds spent, which is what the `paint_us` statistic
/// records: it covers the rasterization only, not the commit.
pub fn paint_region(
    buf: &mut BufferMut<'_>,
    scene: &Scene,
    output: OutputId,
    region: &[IRect],
    cursor: (&Cursor, CursorState),
    items: &mut Vec<PaintItem>,
) -> u64 {
    let start = std::time::Instant::now();
    let (width, height, stride) = (buf.width, buf.height, buf.stride);
    let mut canvas = Canvas::new(buf.data, width, height, stride);
    let (cursor_image, cursor_state) = cursor;
    for clip in region {
        let clip = clip.intersect(&canvas.bounds());
        if clip.is_empty() {
            continue;
        }
        items.clear();
        scene.paint_list(output, &clip, items);
        // Everything below the last item that opaquely covers the whole
        // clip is invisible — including the background. This is the scene's
        // occlusion promise, and it is deliberately conservative there, so
        // trusting it here cannot produce a wrong pixel.
        let first = items
            .iter()
            .rposition(|item| {
                item.opaque_cover()
                    .is_some_and(|cover| cover.contains_rect(&clip))
            })
            .unwrap_or(0);
        if first == 0 && !covers_all(items.first(), &clip) {
            paint_background(&mut canvas, &clip, width, height);
        }
        for item in &items[first..] {
            paint_item(&mut canvas, &clip, item, scene);
        }
        if cursor_state.visible {
            cursor_image.paint(&mut canvas, &clip, cursor_state.x, cursor_state.y);
        }
    }
    items.clear();
    duration_us(start.elapsed())
}

/// Whether the first item alone already hides the background.
fn covers_all(first: Option<&PaintItem>, clip: &IRect) -> bool {
    first.is_some_and(|item| {
        item.opaque_cover()
            .is_some_and(|cover| cover.contains_rect(clip))
    })
}

/// Microseconds of a duration, saturating (a paint that took longer than
/// 584 000 years is not a case worth a `u128`).
fn duration_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Draw one paint item, already clipped by the caller to a damage rect.
fn paint_item(canvas: &mut Canvas<'_>, clip: &IRect, item: &PaintItem, scene: &Scene) {
    let clip = clip.intersect(&item.clip);
    if clip.is_empty() {
        return;
    }
    match item.kind {
        PaintKind::Rect {
            size,
            fill,
            corner_radius,
            border,
        } => {
            let local = Rect::new(0.0, 0.0, size.0, size.1);
            let device = item.transform.apply_rect(&local);
            // The world transform is axis-aligned in M1 (the rasterizer
            // says so), so one scale factor describes it; lengths that are
            // not coordinates — the radius, the border width — need it.
            let scale = item.transform.a.abs().max(item.transform.b.abs());
            let radius = corner_radius * scale;
            if let Some(fill) = raster_fill(fill, item) {
                canvas.fill_rect(&clip, &device, &fill, radius, item.opacity);
            }
            if let Some(border) = border.filter(|b| b.is_visible()) {
                canvas.stroke_rect_inside(
                    &clip,
                    &device,
                    border.width * scale,
                    border.color,
                    radius,
                    item.opacity,
                );
            }
        }
        PaintKind::Image { size, buffer, src } => {
            let Ok(buffer) = scene.buffer(buffer) else {
                return;
            };
            let Some(pixel_format) = pixel_format(buffer.desc().format) else {
                return;
            };
            let image = RasterImage {
                data: buffer.data(),
                width: buffer.desc().w,
                height: buffer.desc().h,
                stride: buffer.desc().stride,
                format: pixel_format,
            };
            let device = item
                .transform
                .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
            canvas.blit(&clip, &device, &image, &src, item.opacity);
        }
    }
}

/// Translate a scene fill into a rasterizer fill, moving the gradient's
/// endpoints from the node's local space into device pixels. `Fill::None`
/// paints nothing, which is `None` here.
fn raster_fill(fill: SceneFill, item: &PaintItem) -> Option<nitro_raster::Fill> {
    match fill {
        SceneFill::None => None,
        SceneFill::Solid(c) => (!c.is_transparent()).then_some(nitro_raster::Fill::Solid(c)),
        SceneFill::Linear { start, end, c0, c1 } => Some(nitro_raster::Fill::Linear {
            start: item.transform.apply(start),
            end: item.transform.apply(end),
            c0,
            c1,
        }),
    }
}

/// The rasterizer's layout for a client's fourcc, or `None` for a format
/// the server does not accept (it rejected it at `CreateBuffer` time, so
/// this is belt and braces).
fn pixel_format(fourcc: u32) -> Option<PixelFormat> {
    match fourcc {
        format::XR24 => Some(PixelFormat::Xrgb8888),
        format::AR24 => Some(PixelFormat::Argb8888),
        _ => None,
    }
}

/// Paint a solid rectangle of `color` — used by tests and by the "no
/// scene at all" path, where the whole output is the background.
pub fn fill(canvas: &mut Canvas<'_>, clip: &IRect, rect: &IRect, color: Color) {
    canvas.fill_irect(clip, rect, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> OutputState {
        OutputState::new(KmsOutputId(1), OutputId(1), 100, 50, 60_000)
    }

    #[test]
    fn refresh_interval_from_millihertz() {
        assert_eq!(refresh_ns(60_000), 16_666_666);
        assert_eq!(refresh_ns(59_940), 16_683_350);
        // Nonsense modes fall back to 60 Hz rather than dividing by zero.
        assert_eq!(refresh_ns(0), 16_666_667);
    }

    #[test]
    fn the_next_vblank_is_extrapolated_from_the_last() {
        let period = 16_666_666;
        // Just after a vblank: the next one is a full period away.
        assert_eq!(next_vblank(1_000, period, 1_100), 1_000 + u64::from(period));
        // Three periods late: skip the ones already missed.
        let now = 1_000 + 3 * u64::from(period) + 5;
        assert_eq!(
            next_vblank(1_000, period, now),
            1_000 + 4 * u64::from(period)
        );
        // No flip seen yet: one refresh from now.
        assert_eq!(next_vblank(0, period, 500), 500 + u64::from(period));
    }

    #[test]
    fn the_deadline_is_a_margin_before_the_vblank_and_never_in_the_past() {
        let period = 16_666_666;
        let now = 1_000;
        let d = frame_deadline(now, period, now);
        assert_eq!(d, now + u64::from(period) - FRAME_MARGIN_NS);
        assert!(d > now);
        // Within the margin of the next vblank: aim for the one after.
        let late = now + u64::from(period) - FRAME_MARGIN_NS / 2;
        let d = frame_deadline(now, period, late);
        assert_eq!(d, now + 2 * u64::from(period) - FRAME_MARGIN_NS);
        assert!(d > late);
    }

    #[test]
    fn a_fresh_output_repaints_fully_twice() {
        let mut s = state();
        assert!(s.needs_paint());
        assert_eq!(s.repaint_region(), [s.bounds()]);
        s.committed(vec![s.bounds()]);
        assert_eq!(s.full_left, 1);
        assert_eq!(s.repaint_region(), [s.bounds()]);
        s.committed(vec![s.bounds()]);
        assert_eq!(s.full_left, 0);
        assert!(!s.needs_paint());
    }

    #[test]
    fn the_repaint_region_is_this_frame_plus_the_last() {
        let mut s = state();
        s.full_left = 0;
        s.previous = vec![IRect::new(0, 0, 10, 10)];
        s.damage.add(IRect::new(60, 30, 10, 10));
        let region = s.repaint_region();
        assert!(region.contains(&IRect::new(0, 0, 10, 10)), "{region:?}");
        assert!(region.contains(&IRect::new(60, 30, 10, 10)), "{region:?}");
        s.committed(region);
        // The new frame's history is what it just painted, and the damage
        // is spent.
        assert_eq!(s.previous.len(), 2);
        assert!(s.damage.is_empty());
        assert!(!s.needs_paint());
    }

    #[test]
    fn a_failed_commit_keeps_the_region_and_asks_for_a_retry() {
        let mut s = state();
        s.full_left = 0;
        let region = vec![IRect::new(4, 4, 8, 8)];
        s.commit_failed(&region);
        assert!(s.retry);
        assert!(s.needs_paint());
        assert_eq!(s.damage.rects(), region);
    }

    #[test]
    fn invalidate_forces_a_full_repaint() {
        let mut s = state();
        s.full_left = 0;
        s.previous = vec![IRect::new(0, 0, 4, 4)];
        s.invalidate();
        assert_eq!(s.full_left, 2);
        assert!(s.previous.is_empty());
        assert_eq!(s.repaint_region(), [s.bounds()]);
    }

    #[test]
    fn area_sums_the_region() {
        assert_eq!(
            region_area(&[IRect::new(0, 0, 10, 10), IRect::new(50, 0, 2, 3)]),
            106
        );
        assert_eq!(region_area(&[]), 0);
    }
}
