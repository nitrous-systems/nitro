//! The frame path: scene damage in, painted and committed pixels out.
//!
//! # The shadow buffer
//!
//! Every output owns a heap-resident [`Shadow`]: a full-size, tightly
//! packed `XRGB8888` copy of what that output must show. The rasterizer
//! paints into *that*, and only the damaged rows are then streamed into
//! the scanout buffer with sequential, write-only [`copy_from_slice`] row
//! copies.
//!
//! [`copy_from_slice`]: slice::copy_from_slice
//!
//! The reason is the destination. A DRM dumb buffer is mapped
//! write-combined: writes are cheap and coalesced, but every *read* is
//! uncached, and source-over is read-modify-write — it reads every
//! destination pixel it blends. Measured on the test box (#539), the same
//! server painting the same scene with the same damage spent **5883 µs**
//! per frame into a dumb buffer and **758 µs** into heap memory: ~87 % of
//! paint was framebuffer traffic rather than raster arithmetic. Painting
//! into the heap and streaming the result back is the portable fix, and it
//! is what every CPU compositor ends up doing.
//!
//! `NITRO_SHADOW=0` turns the shadow off and paints straight into the
//! scanout buffer, so the two can be compared on hardware at any time.
//!
//! # The age-2 rule
//!
//! The backend owns two buffers per output and alternates them strictly, so
//! the buffer handed out at frame `n` is the one that was on screen at
//! frame `n - 2`. Writing only *this* frame's damage into it would
//! therefore leave the previous frame's changes stale in it. The region
//! brought up to date is `damage(n) ∪ damage(n - 1)`
//! ([`OutputState::repaint_region`]), and that same region is what is
//! handed to `commit` as `FB_DAMAGE_CLIPS`: it is exactly the set of pixels
//! that differ between what this buffer holds and what must be on screen.
//! [`OutputState`] keeps the one-frame history that makes this work, and
//! [`OutputState::invalidate`] forces two full frames after a resume, a
//! modeset or a new output, when both buffers hold unknown pixels.
//!
//! The shadow moves that union off the *paint*. The shadow is never stale
//! — every damage ever painted is still in it — so the rasterizer is given
//! `damage(n)` alone ([`OutputState::rasterize_region`]) and the age-2
//! union applies only to the copy out of it. A frame that changed nothing
//! but whose other buffer is two frames behind therefore rasterizes
//! *nothing* and copies the difference, which is the common case after any
//! one-off change. With `NITRO_SHADOW=0` the two regions are the same and
//! the rasterizer is given the union, exactly as before.
//!
//! "The whole buffer is stale" needs no separate flag: the paths that
//! cause it (resume, hotplug, mode set, a fresh output) all go through
//! [`OutputState::invalidate`], which makes the whole output this frame's
//! damage — so both the rasterize region and the copy region become the
//! whole output on their own.
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
//! deliberately generous. The same deadline bounds a *deferred* flip —
//! see [`crate::defer`].
//!
//! # Cursor damage is tracked apart from everything else
//!
//! Damage arrives from two places that mean very different things to the
//! scheduler: the scene (a client's pixels, a window moving, the desktop)
//! and the software cursor, which the server damages itself the instant
//! the pointer moves. [`OutputState::damage_content`] and
//! [`OutputState::damage_cursor`] add to the same region but keep that
//! distinction, because [`OutputState::cursor_only`] — "this frame would
//! put nothing on screen but a moved arrow" — is what decides whether the
//! flip can wait for the client that was just told about the input.

use std::time::Duration;

use nitro_core::{Color, Damage, IRect, Palette, Rect};
use nitro_kms::{BufferMut, Image, OutputId as KmsOutputId};
use nitro_raster::{Canvas, Image as RasterImage, PixelFormat};
use nitro_scene::{Fill as SceneFill, OutputId, PaintItem, PaintKind, Scene};
use nitro_wire::types::format;

use crate::cursor::Cursor;
use crate::render::paint_background;
use crate::text::TextEngine;

/// How long before the next vblank a client should have committed, so the
/// server still has a whole rasterization pass left. Two milliseconds is
/// about a third of the paint budget measured on the test box.
pub const FRAME_MARGIN_NS: u64 = 2_000_000;

/// Bytes per pixel of the only format the frame path speaks (`XRGB8888`),
/// the same one [`nitro_kms`] scans out.
const BYTES_PER_PIXEL: u32 = 4;

/// A heap-resident copy of one output's pixels: what the rasterizer paints
/// into when the shadow is enabled.
///
/// Same format as the scanout buffer (`XRGB8888`) and, once the first frame
/// has seen the real back buffer, the same stride — which lets a full-frame
/// copy be one `memcpy` instead of a row loop. Roughly 8 MB at 1080p, per
/// output; see `docs/budget.md` for why that is worth paying.
///
/// The shadow is never stale: every rect ever painted is still in it. That
/// is the property the whole design rests on — it is why the rasterizer can
/// be given this frame's damage alone while the age-2 union applies only to
/// the copy out of it.
#[derive(Debug)]
pub struct Shadow {
    width: u32,
    height: u32,
    stride: u32,
    data: Vec<u8>,
    complete: bool,
}

impl Shadow {
    /// A black shadow for a `width × height` output, tightly packed.
    ///
    /// The stride is provisional: [`Shadow::ensure`] adopts the scanout
    /// buffer's the first time a frame sees it, which on the fake backend
    /// (padded to 64 bytes, deliberately, so stride bugs surface) is not
    /// the tight one. The shadow starts *incomplete* either way — it holds
    /// no pixels until something has been painted into it.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width * BYTES_PER_PIXEL;
        Self {
            width,
            height,
            stride,
            data: vec![0; (stride as usize) * (height as usize)],
            complete: false,
        }
    }

    /// Resize (and clear) if the output's geometry or the scanout stride
    /// changed. Returns whether the contents were dropped, which the caller
    /// must answer with a full repaint.
    pub fn ensure(&mut self, width: u32, height: u32, stride: u32) -> bool {
        if self.width == width && self.height == height && self.stride == stride {
            return false;
        }
        // Built here rather than via `Shadow::new` + a fix-up, so the one
        // allocation made is the one kept: `new` assumes a tight stride and
        // the scanout buffer's is often padded.
        *self = Self {
            width,
            height,
            stride,
            data: vec![0; (stride as usize) * (height as usize)],
            complete: false,
        };
        true
    }

    /// Whether every pixel of the shadow has been painted at least once.
    ///
    /// False from allocation until a frame rasterizes the whole output into
    /// it, which is the very next frame in every path that allocates one
    /// (a new output and a mode change both [`OutputState::invalidate`]).
    /// Until then the shadow is black and must not be read — that is what
    /// keeps a screenshot taken this early honest.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Note what was just rasterized into the shadow: a region covering the
    /// whole output makes it complete.
    pub fn note_painted(&mut self, region: &[IRect]) {
        if self.complete {
            return;
        }
        let all = IRect::new(0, 0, self.width.cast_signed(), self.height.cast_signed());
        self.complete = region.iter().any(|r| r.contains_rect(&all));
    }

    /// Resident bytes, for the `shadow_bytes` statistic.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.data.len() as u64
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// A canvas over the whole shadow, for the rasterizer.
    pub fn canvas(&mut self) -> Canvas<'_> {
        Canvas::new(&mut self.data, self.width, self.height, self.stride)
    }

    /// Stream `region` into the scanout buffer.
    ///
    /// Write-only and sequential, one row per `copy_from_slice`: no byte of
    /// `dst` is ever read, which is the entire point — the destination is
    /// write-combined memory where a read costs an uncached fetch. When the
    /// region covers whole rows and the strides agree the rows are one
    /// contiguous block and go out as a single copy.
    ///
    /// Rects are clipped to both buffers, so a stale region after a mode
    /// change cannot write out of bounds.
    pub fn stream_to(&self, dst: &mut BufferMut<'_>, region: &[IRect]) {
        let width = self.width.min(dst.width);
        let height = self.height.min(dst.height);
        let bounds = IRect::new(0, 0, width.cast_signed(), height.cast_signed());
        let (src_stride, dst_stride) = (self.stride as usize, dst.stride as usize);
        for rect in region {
            let rect = rect.intersect(&bounds);
            if rect.is_empty() {
                continue;
            }
            let left = rect.x.cast_unsigned();
            let top = rect.y.cast_unsigned();
            let cols = rect.w.cast_unsigned();
            let rows = rect.h.cast_unsigned();
            let row_bytes = (cols * BYTES_PER_PIXEL) as usize;
            let x_off = (left * BYTES_PER_PIXEL) as usize;
            if left == 0 && cols == width && src_stride == dst_stride {
                let start = (top as usize) * src_stride;
                let len = (rows as usize) * src_stride;
                dst.data[start..start + len].copy_from_slice(&self.data[start..start + len]);
                continue;
            }
            for row in top..top + rows {
                let from = (row as usize) * src_stride + x_off;
                let to = (row as usize) * dst_stride + x_off;
                dst.data[to..to + row_bytes].copy_from_slice(&self.data[from..from + row_bytes]);
            }
        }
    }

    /// A tightly packed copy of the shadow, in the shape
    /// [`nitro_kms::Backend::read_front`] returns — so a screenshot taken
    /// from here is indistinguishable from one taken off the front buffer.
    #[must_use]
    pub fn image(&self) -> Image {
        let row = (self.width * BYTES_PER_PIXEL) as usize;
        let mut data = Vec::with_capacity(row * self.height as usize);
        for y in 0..self.height as usize {
            let start = y * self.stride as usize;
            data.extend_from_slice(&self.data[start..start + row]);
        }
        Image {
            width: self.width,
            height: self.height,
            stride: self.width * BYTES_PER_PIXEL,
            data,
        }
    }
}

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
    /// Damage accumulated since the last painted frame. Private, because
    /// every addition has to say whether it is content or the cursor:
    /// [`OutputState::damage_content`] and [`OutputState::damage_cursor`].
    damage: Damage,
    /// Whether any of that damage is something other than the cursor.
    content_damage: bool,
    /// The damage the previous frame consumed — the *damage*, not the
    /// region painted. Those differ: the region painted is already this
    /// frame's damage unioned with the last one's, and feeding that back
    /// would fold every frame's region into the next and never converge.
    pub previous: Vec<IRect>,
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
    /// The heap buffer this output is painted into, when the shadow is
    /// enabled. `None` under `NITRO_SHADOW=0`, where paint goes straight
    /// into the scanout buffer.
    ///
    /// Allocated when the output is added, resized on a mode change and
    /// dropped with the output — the whole lifecycle is this field's.
    pub shadow: Option<Shadow>,
}

impl OutputState {
    /// A fresh output: everything unknown, so the first two frames repaint
    /// in full.
    ///
    /// `shadow` says whether this output paints into a heap buffer
    /// (`NITRO_SHADOW`); the buffer is allocated here, so an output never
    /// exists without the memory its frames need.
    #[must_use]
    pub fn new(
        kms_id: KmsOutputId,
        scene_id: OutputId,
        width: u32,
        height: u32,
        refresh_mhz: u32,
        shadow: bool,
    ) -> Self {
        let mut damage = Damage::new();
        damage.add(IRect::new(0, 0, width.cast_signed(), height.cast_signed()));
        Self {
            kms_id,
            scene_id,
            width,
            height,
            damage,
            content_damage: true,
            previous: Vec::new(),
            retry: false,
            refresh_ns: refresh_ns(refresh_mhz),
            last_vblank_ns: 0,
            last_sequence: 0,
            in_flight: Vec::new(),
            painting: Vec::new(),
            in_flight_input_ns: 0,
            painting_input_ns: 0,
            shadow: shadow.then(|| Shadow::new(width, height)),
        }
    }

    /// The whole output as a rect.
    #[must_use]
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width.cast_signed(), self.height.cast_signed())
    }

    /// Both buffers hold unknown pixels (first frame, resume, modeset):
    /// repaint everything.
    ///
    /// One rect of damage is all this takes, and it is worth seeing why:
    /// the whole output becomes this frame's damage, so this frame paints
    /// everything *and* hands "everything" to the next frame as its
    /// history — which is exactly the second full repaint the other buffer
    /// needs. A separate "repaint fully for N frames" counter would say
    /// the same thing twice, and get it subtly wrong the moment a client
    /// commits in between the two.
    pub fn invalidate(&mut self) {
        self.previous.clear();
        self.damage.clear();
        self.damage.add(self.bounds());
        self.content_damage = true;
    }

    /// Add damage from the scene: a client's pixels, a window that moved,
    /// the desktop under one that closed. A frame carrying any of this is
    /// never deferred.
    pub fn damage_content(&mut self, rect: IRect) {
        self.damage.add(rect);
        self.content_damage = true;
    }

    /// Add damage the server made for its own software cursor.
    ///
    /// Kept apart from [`OutputState::damage_content`] only so that
    /// [`OutputState::cursor_only`] can tell them apart; the region
    /// painted is the union either way.
    pub fn damage_cursor(&mut self, rect: IRect) {
        self.damage.add(rect);
    }

    /// Whether a frame painted now would put nothing new on screen but a
    /// moved cursor.
    ///
    /// Two things disqualify it, and each is a frame somebody is already
    /// waiting for: content damage of its own, and a failed commit that
    /// has to be retried.
    ///
    /// The age-2 carry in [`OutputState::previous`] deliberately does
    /// *not*. Those pixels are already on screen — they are in the front
    /// buffer, and the repaint only brings the *other* buffer up to date —
    /// so nobody is waiting for them and holding the frame back costs
    /// nothing. Counting them would be worse than pointless: a pointer
    /// moving over a client that answers every motion produces content
    /// damage on alternate frames, so every second frame would refuse to
    /// wait and put the client's next answer a flip behind again, which is
    /// exactly the bug this is here to fix.
    #[must_use]
    pub fn cursor_only(&self) -> bool {
        !self.content_damage && !self.retry
    }

    /// Whether there is damage waiting for a frame at all (ignoring the
    /// age-2 history and the retry flag). Tests and assertions only.
    #[must_use]
    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }

    /// Whether a frame would put anything new on screen.
    #[must_use]
    pub fn needs_paint(&self) -> bool {
        self.retry || !self.damage.is_empty() || !self.previous.is_empty()
    }

    /// The damage this frame alone carries: what the *rasterizer* has to
    /// draw when there is a shadow buffer to draw into.
    ///
    /// The shadow already holds every rect ever painted, so nothing older
    /// than this frame needs re-drawing — the age-2 union belongs to the
    /// copy out of the shadow, not to the paint. Without a shadow the
    /// caller uses [`OutputState::repaint_region`] for both.
    #[must_use]
    pub fn rasterize_region(&self) -> Vec<IRect> {
        self.damage.rects().to_vec()
    }

    /// The region to bring the (age-2) back buffer up to date over.
    ///
    /// The buffer about to be written was last on screen two frames ago,
    /// so what it must be brought up to date with is everything that
    /// changed since: `damage(n) ∪ damage(n-1)`. Not the *region painted*
    /// at `n-1` — that was itself a union of two frames' damage, and
    /// feeding it back would make every frame at least as large as the one
    /// before it and never shrink again.
    #[must_use]
    pub fn repaint_region(&self) -> Vec<IRect> {
        let mut region = Damage::new();
        for r in self.damage.rects() {
            region.add(*r);
        }
        for r in &self.previous {
            region.add(*r);
        }
        region.take()
    }

    /// Note that the frame just painted was committed.
    ///
    /// The history kept is the damage this frame consumed, unconditionally
    /// — including on a full repaint, where it is the whole output. It has
    /// to be: a client that commits between the two invalidation frames
    /// changes the image between them, and that change belongs to the
    /// second buffer's next repaint. Dropping it leaves one of the two
    /// buffers permanently missing that client's pixels, which shows up as
    /// a window that flickers away every other frame.
    pub fn committed(&mut self) {
        self.previous = self.damage.take();
        self.content_damage = false;
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

/// Paint `region` of one output into `canvas`.
///
/// The canvas is the shadow buffer when there is one and the scanout
/// buffer's mapping when there is not; nothing below here can tell the
/// difference, which is why `NITRO_SHADOW=0` is a fair A/B and not a
/// second code path.
///
/// Returns the microseconds spent, which is what the `paint_us` statistic
/// records: it covers the rasterization only, not the copy or the commit.
#[allow(clippy::too_many_arguments)] // One paint call's inputs, not a structure: bundling them would be a struct built per frame to satisfy a lint.
pub fn paint_region(
    canvas: &mut Canvas<'_>,
    scene: &Scene,
    text: &mut TextEngine,
    output: OutputId,
    region: &[IRect],
    cursor: (&Cursor, CursorState),
    items: &mut Vec<PaintItem>,
    palette: &Palette,
) -> u64 {
    let start = std::time::Instant::now();
    let (width, height) = (canvas.width(), canvas.height());
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
            paint_background(canvas, &clip, width, height, palette);
        }
        for item in &items[first..] {
            paint_item(canvas, &clip, item, scene, text);
        }
        if cursor_state.visible {
            cursor_image.paint(canvas, &clip, cursor_state.x, cursor_state.y);
        }
    }
    items.clear();
    duration_us(start.elapsed())
}

/// Stream `region` from `shadow` into `buf`, reporting the microseconds it
/// took — the `copy_us` statistic.
///
/// Separate from [`paint_region`] because they measure different hardware:
/// paint is CPU and cached memory, the copy is the write-combined mapping.
/// Keeping them apart is what lets `paint_us` mean "rasterization" again
/// (`docs/latency.md`).
pub fn copy_region(shadow: &Shadow, buf: &mut BufferMut<'_>, region: &[IRect]) -> u64 {
    let start = std::time::Instant::now();
    shadow.stream_to(buf, region);
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
fn paint_item(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    item: &PaintItem,
    scene: &Scene,
    text: &mut TextEngine,
) {
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
        PaintKind::Text { key, origin, color } => {
            // The glyphs themselves live in the text engine's atlas; the
            // scene knows only the handle, the already-aligned origin and
            // the colour. Everything about fonts stops here.
            //
            // Narrowed to `item.bounds` — the node's box — and that is a
            // correctness requirement, not tidiness. Every other kind's
            // geometry *is* its bounds, so it cannot paint outside them;
            // a text run's is not. A run wider or taller than the box it
            // was given (a long unwrapped label, a descender below a tight
            // `bounds.h`) would otherwise put pixels outside the rectangle
            // the scene damaged for it — and since the *next* mutation
            // damages only that same rectangle, the spill would never be
            // repainted and would sit on screen as a ghost. Clipping to
            // the box is what keeps the damage contract true, and it is
            // what the node's bounds are for.
            let clip = clip.intersect(&item.bounds);
            if clip.is_empty() {
                return;
            }
            text.paint(
                canvas,
                &clip,
                &item.transform,
                key,
                origin,
                color,
                item.opacity,
            );
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
        OutputState::new(KmsOutputId(1), OutputId(1), 100, 50, 60_000, false)
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

    /// Paint one frame and report the region it covered.
    fn frame(s: &mut OutputState) -> Vec<IRect> {
        let region = s.repaint_region();
        s.committed();
        region
    }

    #[test]
    fn a_fresh_output_repaints_fully_twice_then_stops() {
        let mut s = state();
        assert!(s.needs_paint());
        assert_eq!(frame(&mut s), [s.bounds()]);
        assert_eq!(frame(&mut s), [s.bounds()]);
        // Both buffers now hold the same correct image, so a third frame
        // has nothing to do at all. Carrying the *painted region* forward
        // instead of the damage is what used to repaint the whole screen
        // for ever after.
        assert!(!s.needs_paint());
        assert!(s.repaint_region().is_empty());
    }

    #[test]
    fn damage_arriving_between_the_two_full_frames_still_reaches_both_buffers() {
        // The regression the fake-backend integration test caught: a client
        // that commits after the first full frame but before the second.
        // Its pixels are in the second buffer because that frame was full;
        // they reach the first only if the damage is carried forward.
        let mut s = state();
        assert_eq!(frame(&mut s), [s.bounds()]);
        let window = IRect::new(0, 0, 40, 30);
        s.damage_content(window);
        assert_eq!(
            frame(&mut s),
            [s.bounds()],
            "the second frame is still full"
        );
        assert!(s.needs_paint(), "the other buffer still lacks the window");
        // And it costs exactly the window, not another full screen: the
        // first buffer was already repainted in full, so the window is the
        // only thing that changed since.
        assert_eq!(frame(&mut s), [window]);
        assert!(!s.needs_paint());
    }

    #[test]
    fn a_small_change_repaints_a_small_region_for_two_frames_then_nothing() {
        let mut s = state();
        frame(&mut s);
        frame(&mut s);
        let moved = IRect::new(10, 10, 4, 4);
        s.damage_content(moved);
        // The frame that draws it, and the one after (whose buffer is two
        // frames stale), both repaint exactly that rect — and no more.
        assert_eq!(frame(&mut s), [moved]);
        assert_eq!(frame(&mut s), [moved]);
        // Both buffers are now current again.
        assert!(!s.needs_paint());
        assert!(s.repaint_region().is_empty());
    }

    #[test]
    fn the_repaint_region_is_this_frame_plus_the_last() {
        let mut s = state();
        s.damage.clear();
        s.content_damage = false;
        s.previous = vec![IRect::new(0, 0, 10, 10)];
        let fresh = IRect::new(60, 30, 10, 10);
        s.damage_content(fresh);
        let region = s.repaint_region();
        assert!(region.contains(&IRect::new(0, 0, 10, 10)), "{region:?}");
        assert!(region.contains(&fresh), "{region:?}");
        s.committed();
        // The history kept is this frame's damage alone, so the next frame
        // repaints `fresh` and not the rect inherited from the one before.
        assert_eq!(s.previous, [fresh]);
        assert!(!s.has_damage());
        assert_eq!(s.repaint_region(), [fresh]);
        s.committed();
        assert!(!s.needs_paint(), "and then it is done");
    }

    /// The split the shadow buys: the rasterizer is given this frame's
    /// damage, the copy the age-2 union.
    #[test]
    fn the_rasterize_region_is_this_frame_alone() {
        let mut s = state();
        s.damage.clear();
        s.content_damage = false;
        let old = IRect::new(0, 0, 10, 10);
        s.previous = vec![old];
        let fresh = IRect::new(60, 30, 10, 10);
        s.damage_content(fresh);
        // The shadow already holds `old`, so nothing has to redraw it.
        assert_eq!(s.rasterize_region(), [fresh]);
        let copy = s.repaint_region();
        assert!(copy.contains(&old), "{copy:?}");
        assert!(copy.contains(&fresh), "{copy:?}");
        // And the frame after: nothing new to draw at all, but the other
        // buffer is still behind by `fresh`, which the copy covers. That
        // is the frame the shadow makes free.
        s.committed();
        assert!(s.rasterize_region().is_empty());
        assert_eq!(s.repaint_region(), [fresh]);
    }

    /// `invalidate` needs no separate "the whole buffer is stale" flag:
    /// making the output this frame's damage covers both regions.
    #[test]
    fn invalidate_makes_both_regions_the_whole_output() {
        let mut s = state();
        frame(&mut s);
        frame(&mut s);
        s.invalidate();
        assert_eq!(s.rasterize_region(), [s.bounds()]);
        assert_eq!(s.repaint_region(), [s.bounds()]);
    }

    #[test]
    fn a_failed_commit_keeps_the_region_and_asks_for_a_retry() {
        let mut s = state();
        s.damage.clear();
        s.previous.clear();
        let region = vec![IRect::new(4, 4, 8, 8)];
        s.commit_failed(&region);
        assert!(s.retry);
        assert!(s.needs_paint());
        assert_eq!(s.damage.rects(), region);
    }

    #[test]
    fn invalidate_forces_two_full_repaints() {
        let mut s = state();
        frame(&mut s);
        frame(&mut s);
        assert!(!s.needs_paint());
        s.invalidate();
        assert!(s.previous.is_empty());
        assert_eq!(frame(&mut s), [s.bounds()]);
        assert_eq!(frame(&mut s), [s.bounds()], "both buffers were unknown");
        assert!(!s.needs_paint());
    }

    /// A settled output where only the cursor moved is the one case a
    /// flip may be held back for a client's answer.
    #[test]
    fn cursor_only_is_true_only_when_nothing_else_is_pending() {
        let mut s = state();
        // A fresh output is a full repaint, which is content.
        assert!(!s.cursor_only());
        frame(&mut s);
        frame(&mut s);
        assert!(!s.needs_paint());
        // Settled and quiet: vacuously cursor-only, but `needs_paint` is
        // false so nothing is deferred either.
        assert!(s.cursor_only());

        s.damage_cursor(IRect::new(4, 4, 24, 24));
        assert!(s.needs_paint());
        assert!(s.cursor_only(), "a moved arrow and nothing else");

        // A client's pixels in the same frame: not deferrable.
        s.damage_content(IRect::new(40, 10, 10, 10));
        assert!(!s.cursor_only());

        // The frame after that one still repaints the content under the
        // age-2 rule, but those pixels are already on screen — only the
        // other buffer lacks them — so nobody is waiting and the frame is
        // still deferrable.
        frame(&mut s);
        s.damage_cursor(IRect::new(8, 4, 24, 24));
        assert!(
            s.cursor_only(),
            "an age-2 carry is already on screen; nobody waits for it"
        );
    }

    /// A commit that failed has to be retried now, not at some deadline.
    #[test]
    fn a_retry_is_never_cursor_only() {
        let mut s = state();
        frame(&mut s);
        frame(&mut s);
        s.commit_failed(&[IRect::new(0, 0, 4, 4)]);
        s.damage_cursor(IRect::new(4, 4, 24, 24));
        assert!(!s.cursor_only());
    }

    #[test]
    fn area_sums_the_region() {
        assert_eq!(
            region_area(&[IRect::new(0, 0, 10, 10), IRect::new(50, 0, 2, 3)]),
            106
        );
        assert_eq!(region_area(&[]), 0);
    }

    /// A scanout buffer with a padded stride, which is what the fake
    /// backend hands out (deliberately: stride bugs must surface).
    fn scanout(height: u32, stride: u32) -> Vec<u8> {
        vec![0u8; (stride * height) as usize]
    }

    /// Fill the shadow with a recognisable per-pixel pattern: the pixel at
    /// `(x, y)` is `0x00_10_xx_yy`, so a byte that came from the wrong row
    /// or column is visible in the value rather than merely unequal.
    fn paint_pattern(shadow: &mut Shadow) {
        let (w, h) = (shadow.width(), shadow.height());
        let mut canvas = shadow.canvas();
        for y in 0..h.cast_signed() {
            for x in 0..w.cast_signed() {
                let px = IRect::new(x, y, 1, 1);
                canvas.fill_irect(&px, &px, Color::rgb(0x10, x as u8, y as u8));
            }
        }
    }

    #[test]
    fn a_fresh_shadow_is_incomplete_and_sized_for_the_output() {
        let s = Shadow::new(8, 4);
        assert_eq!((s.width(), s.height()), (8, 4));
        assert_eq!(s.bytes(), 8 * 4 * 4);
        assert!(!s.is_complete(), "nothing has been painted into it yet");
    }

    #[test]
    fn a_shadow_becomes_complete_only_when_the_whole_output_is_painted() {
        let mut s = Shadow::new(8, 4);
        s.note_painted(&[IRect::new(0, 0, 8, 3)]);
        assert!(!s.is_complete(), "one row short");
        s.note_painted(&[IRect::new(0, 0, 8, 4)]);
        assert!(s.is_complete());
        // And it stays complete: later frames paint less, not less of it.
        s.note_painted(&[IRect::new(1, 1, 2, 2)]);
        assert!(s.is_complete());
    }

    #[test]
    fn ensure_reallocates_only_when_the_geometry_or_stride_moved() {
        let mut s = Shadow::new(8, 4);
        assert!(s.ensure(8, 4, 64), "the padded stride is a new buffer");
        assert_eq!(s.bytes(), 64 * 4);
        assert!(!s.ensure(8, 4, 64), "same again: no reallocation");
        s.note_painted(&[IRect::new(0, 0, 8, 4)]);
        assert!(s.is_complete());
        assert!(s.ensure(16, 4, 64));
        assert!(!s.is_complete(), "a resized shadow holds nothing");
    }

    /// The copy is the whole point, so it gets an exactness test: the
    /// streamed rects match the shadow byte for byte and *nothing else in
    /// the destination is touched* — including the stride padding, which a
    /// row loop that used `width * 4` as the stride would smear over.
    #[test]
    fn streaming_writes_exactly_the_region_and_nothing_else() {
        let (w, h, stride) = (8u32, 4u32, 64u32);
        let mut shadow = Shadow::new(w, h);
        shadow.ensure(w, h, stride);
        paint_pattern(&mut shadow);
        let full = shadow.image();

        let mut data = scanout(h, stride);
        let mut buf = BufferMut {
            width: w,
            height: h,
            stride,
            data: &mut data,
        };
        let rect = IRect::new(2, 1, 3, 2);
        shadow.stream_to(&mut buf, &[rect]);
        let img = Image {
            width: w,
            height: h,
            stride,
            data,
        };
        for y in 0..h {
            for x in 0..w {
                let want = if rect.contains(x.cast_signed(), y.cast_signed()) {
                    full.pixel(x, y)
                } else {
                    0
                };
                assert_eq!(img.pixel(x, y), want, "at ({x},{y})");
            }
        }
        // The padding beyond `width * 4` on the first streamed row.
        assert_eq!(
            &img.data[stride as usize + 32..2 * stride as usize],
            &[0; 32]
        );
    }

    /// A sequence of partial streams adds up to the full image: this is
    /// the property the whole design rests on, since the back buffer is
    /// only ever brought up to date one damage region at a time.
    #[test]
    fn partial_streams_accumulate_to_a_full_copy() {
        let (w, h, stride) = (8u32, 4u32, 64u32);
        let mut shadow = Shadow::new(w, h);
        shadow.ensure(w, h, stride);
        paint_pattern(&mut shadow);

        let mut piecemeal = scanout(h, stride);
        let mut buf = BufferMut {
            width: w,
            height: h,
            stride,
            data: &mut piecemeal,
        };
        for rect in [
            IRect::new(0, 0, 8, 1),
            IRect::new(0, 1, 4, 3),
            IRect::new(4, 1, 4, 3),
        ] {
            shadow.stream_to(&mut buf, &[rect]);
        }

        let mut whole = scanout(h, stride);
        let mut buf = BufferMut {
            width: w,
            height: h,
            stride,
            data: &mut whole,
        };
        shadow.stream_to(&mut buf, &[IRect::new(0, 0, 8, 4)]);
        assert_eq!(piecemeal, whole);
    }

    /// A region left over from a larger mode must not write out of bounds,
    /// and must not wrap onto the next row either.
    #[test]
    fn streaming_clips_a_stale_region_to_both_buffers() {
        let (w, h, stride) = (8u32, 4u32, 32u32);
        let mut shadow = Shadow::new(w, h);
        paint_pattern(&mut shadow);
        let mut data = scanout(h, stride);
        let mut buf = BufferMut {
            width: w,
            height: h,
            stride,
            data: &mut data,
        };
        shadow.stream_to(&mut buf, &[IRect::new(-4, -4, 400, 400)]);
        let img = Image {
            width: w,
            height: h,
            stride,
            data,
        };
        assert_eq!(img, shadow.image(), "clipped to the full output");
    }

    /// `image()` must produce exactly what `read_front` would: tightly
    /// packed, stride `width * 4`, so a screenshot off the shadow and one
    /// off the framebuffer are the same bytes.
    #[test]
    fn the_shadow_image_is_tightly_packed() {
        let mut shadow = Shadow::new(8, 4);
        shadow.ensure(8, 4, 64);
        paint_pattern(&mut shadow);
        let img = shadow.image();
        assert_eq!(img.stride, 8 * 4);
        assert_eq!(img.data.len(), 8 * 4 * 4);
        assert_eq!(img.pixel(7, 3), 0x0010_0703);
    }

    /// A shaped run that overflows its node's bounds must not paint outside
    /// them.
    ///
    /// Text is the first node kind that *can* break the damage contract:
    /// every other kind's geometry is its bounds, so it cannot paint outside
    /// them, but a run's extent is whatever the shaper produced. The scene
    /// damages `world_bounds` — computed from `node.bounds` alone — so a
    /// pixel drawn outside the box is a pixel nothing will ever repaint: it
    /// survives the next `SetText`, the node's destruction and the window's
    /// close, as a ghost.
    ///
    /// Tested here, on `paint_item` directly, rather than through the fake
    /// backend, because a screenshot cannot see it. The backend double
    /// buffers: the frame that draws the overflowing run and the frame that
    /// replaces it land in *different* buffers, and a shot returns whichever
    /// is on the front — so the spill is real, is in a buffer, and is
    /// invisible to `shot` until the buffers happen to rotate. One canvas and
    /// one call have no such ambiguity.
    #[test]
    fn a_text_run_is_clipped_to_its_nodes_bounds() {
        let db = nitro_text::FontDb::scan();
        if db.is_empty() {
            eprintln!("skipping: no fonts on this box");
            return;
        }
        let mut engine = crate::text::TextEngine::new();
        if !engine.has_fonts() {
            eprintln!("skipping: no fonts on this box");
            return;
        }

        // A long unwrapped run in a deliberately small box.
        let request = crate::text::StyleRequest::new("sans", 20.0, 400, false, 0.0, false);
        let (key, shaped) = engine.shape(1, &request, "wwwwwwwwwwwwwwwwwwwwwwwwwwwwww");
        let (block_w, block_h) = (shaped.width, shaped.height);
        let bounds = IRect::new(20, 20, 40, 18);
        assert!(
            block_w > bounds.w as f32,
            "the test needs an overflowing run: {block_w} vs {}",
            bounds.w
        );

        // A canvas far larger than the box, so a spill has somewhere to land.
        let (w, h) = (320u32, 64u32);
        let stride = w * 4;
        let mut data = vec![0u8; (stride * h) as usize];
        let mut canvas = Canvas::new(&mut data, w, h, stride);
        let surface = canvas.bounds();

        let item = PaintItem {
            node: nitro_scene::NodeKey::from_parts(0, 0),
            window: nitro_scene::WindowKey::from_parts(0, 0),
            kind: PaintKind::Text {
                key: key.0,
                origin: nitro_core::Point::new(0.0, 0.0),
                color: Color::WHITE,
            },
            transform: nitro_core::Transform::translate(bounds.x as f32, bounds.y as f32),
            // The clip the frame path would hand it: the whole damage rect.
            clip: surface,
            opacity: 1.0,
            // What the scene damaged, and therefore the only pixels that may
            // be touched.
            bounds,
        };
        paint_item(&mut canvas, &surface, &item, &Scene::new(), &mut engine);

        let mut inside = 0u32;
        for y in 0..h {
            for x in 0..w {
                let o = (y * stride + x * 4) as usize;
                let lit = data[o] != 0 || data[o + 1] != 0 || data[o + 2] != 0;
                let in_bounds = bounds.contains(x.cast_signed(), y.cast_signed());
                assert!(
                    !lit || in_bounds,
                    "glyph pixel at ({x},{y}), outside the node's bounds {bounds:?}"
                );
                if lit {
                    inside += 1;
                }
            }
        }
        assert!(
            inside > 10,
            "expected glyphs inside the box, found {inside} (block {block_w}x{block_h})"
        );
    }
}
