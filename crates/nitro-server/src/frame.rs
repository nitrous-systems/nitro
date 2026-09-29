//! The frame path: scene damage in, painted and committed pixels out.
//!
//! # The shadow buffer
//!
//! Every output owns a heap-resident [`Shadow`]: a full-size, tightly
//! packed premultiplied `ARGB8888` copy of what that output must show
//! (alpha 255 everywhere except the holes punched for Surfaces on an
//! underlay plane, see [`fill_holes`]). The rasterizer
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
//! opaque client item covers the whole thing — [`PaintItem::opaque_cover`]
//! is the scene's conservative promise about that, and it covers an opaque
//! 1:1 image as well as a solid rect, which is what lets a maximised
//! window skip the desktop and everything under it), then the scene's
//! paint list clipped to the rect, then the software cursor last. A
//! [`PaintKind::Hole`] is an ordinary item: it clears its rect to alpha 0
//! (#3898), and every item above it — and the cursor — composites onto the
//! hole as premultiplied ARGB, which is what an underlay shows through. The
//! rasterizer never writes outside the clip it was given, so one rect
//! cannot smear into another.
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
//! # Scroll blits
//!
//! When a scene update was, apart from other damage, a pure whole-pixel
//! translation of one subtree ([`nitro_scene::Translation`]), the paint can
//! move pixels the shadow already holds instead of rasterizing them again —
//! `CopyArea`, inside the compositor. Only pixels provably the moved
//! subtree's own opaque, shift-exact content are moved, and never where
//! anything above the subtree, anything else that changed, or the cursor
//! is or was: [`blit_region`] states the rule `D = (R ∩ S ∩ C∩(C+d)) \ (A∪F)
//! \ ((A∪F)+d)`. Everything else of the rasterize region is painted as
//! usual. The hint rides *alongside* the damage and never replaces it, so
//! ignoring it (`NITRO_SCROLL_BLIT=0`) paints the same pixels.
//!
//! Two things it does **not** do. `damage_px` does not move: the copy out
//! of the shadow is still `damage(n) ∪ damage(n-1)`, pixels that really
//! differ in the age-2 back buffer. And that copy is not itself blitted —
//! the buffer handed out at frame `n` was on screen at `n-2`, which would
//! need a per-buffer translation accumulator. Only `paint_us` shrinks.
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

use nitro_core::{Color, Damage, IRect, Palette, Rect, Region};
use nitro_kms::{BufferMut, Image, OutputId as KmsOutputId};
use nitro_raster::{
    Canvas, Image as RasterImage, Nv12, Overlay, Packed422, Packed422Order, PixelFormat,
    YuvEncoding, YuvMatrix, YuvRange,
};
use nitro_scene::{
    ColorMatrix as SceneColorMatrix, ColorRange as SceneColorRange, Fill as SceneFill, OutputId,
    PaintItem, PaintKind, Scene, SurfaceColor,
};
use nitro_wire::types::format;

use crate::cursor::Cursor;
use crate::icons::IconEngine;
use crate::render::{paint_background, paint_background_overlaid};
use crate::text::TextEngine;

/// How long before the next vblank a client should have committed, so the
/// server still has a whole rasterization pass left. Two milliseconds is
/// about a third of the paint budget measured on the test box.
///
/// An **absolute** amount, deliberately: it is the server's own work — one
/// rasterization pass plus the copy out of the shadow — and that work does
/// not get faster because the panel got faster. See [`frame_margin_ns`]
/// for what happens when the period gets short enough that a fixed 2 ms is
/// most of it.
pub const FRAME_MARGIN_NS: u64 = 2_000_000;

/// The largest share of one refresh period the margin may take.
///
/// A quarter. At 60 Hz (16.67 ms) and at 120 Hz (8.33 ms) this is above
/// [`FRAME_MARGIN_NS`] and so changes nothing — the margin at both rates is
/// the measured 2 ms. It bites at 240 Hz and beyond (4.17 ms period), where
/// a fixed 2 ms would hand the client under half of each frame and the
/// deadline would start pushing commits onto the *following* vblank, which
/// is the exact latency the deferral machinery exists to avoid.
const FRAME_MARGIN_MAX_SHARE: u64 = 4;

/// Bytes per pixel of the only layout the frame path speaks (premultiplied
/// `ARGB8888`), scanned out by [`nitro_kms`] as `XRGB8888` or, while a hole
/// is on screen and the plane supports it, `ARGB8888` — the same bytes.
const BYTES_PER_PIXEL: u32 = 4;

/// A heap-resident copy of one output's pixels: what the rasterizer paints
/// into when the shadow is enabled.
///
/// Same layout as the scanout buffer (premultiplied `ARGB8888`, alpha 255
/// outside holes) and, once the first frame
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

    /// Copy `shadow[p] ← shadow[p − (dx, dy)]` for every pixel `p` of
    /// `dst`, as if from a snapshot taken before the first write.
    ///
    /// Snapshot semantics without a snapshot: rows are visited so that a
    /// row is always read before anything overwrites it (bottom to top
    /// when the content moves down, top to bottom when it moves up), and
    /// on a purely horizontal move the spans of one row are visited so
    /// that each is read before its neighbour's write lands on it. Each
    /// span is one `copy_within` — a `memmove`, so a span overlapping its
    /// own source is fine.
    ///
    /// Pixels outside `dst` are not touched. A destination or source
    /// outside the shadow is clipped away (and is a caller bug).
    pub fn translate_region(&mut self, dst: &Region, dx: i32, dy: i32) {
        let bounds = IRect::new(0, 0, self.width.cast_signed(), self.height.cast_signed());
        let valid = bounds.intersect(&bounds.translate(dx, dy));
        let mut rows: Vec<(i32, i32, i32)> = Vec::new();
        for r in dst.rects() {
            let r = r.intersect(&valid);
            debug_assert!(!dst.overflowed());
            for y in r.y..r.bottom() {
                rows.push((y, r.x, r.right()));
            }
        }
        // Readers before writers: see the doc comment.
        rows.sort_unstable_by(|a, b| {
            let by_y = if dy > 0 { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) };
            let by_x = if dx > 0 { b.1.cmp(&a.1) } else { a.1.cmp(&b.1) };
            by_y.then(by_x)
        });
        let stride = self.stride as usize;
        let bpp = BYTES_PER_PIXEL as usize;
        for (y, x0, x1) in rows {
            if x1 <= x0 {
                continue;
            }
            let to = y.cast_unsigned() as usize * stride + x0.cast_unsigned() as usize * bpp;
            let from = (y - dy).cast_unsigned() as usize * stride
                + (x0 - dx).cast_unsigned() as usize * bpp;
            let len = (x1 - x0).cast_unsigned() as usize * bpp;
            self.data.copy_within(from..from + len, to);
        }
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
#[allow(clippy::struct_excessive_bools)] // Independent per-output flags, not a state machine.
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
    /// A scroll-blit hint waiting for the next paint, in output-local
    /// pixels; see [`PendingScroll`].
    scroll: Option<PendingScroll>,
    /// The heap buffer this output is painted into, when the shadow is
    /// enabled. `None` under `NITRO_SHADOW=0`, where paint goes straight
    /// into the scanout buffer.
    ///
    /// Allocated when the output is added, resized on a mode change and
    /// dropped with the output — the whole lifecycle is this field's.
    pub shadow: Option<Shadow>,
    /// Whether this output's scanout is currently switched to `ARGB8888`
    /// ([`nitro_kms::Backend::set_scanout_alpha`]): true only while a hole
    /// is on it and the plane can blend alpha (#3898). The bytes are the
    /// same either way, so flipping it never needs a repaint.
    pub alpha: bool,
    /// Holes were on this output while its plane cannot scan out alpha,
    /// and that was logged once.
    pub alpha_warned: std::cell::Cell<bool>,
    /// The overview's thumbnail atlas for this output (#3902): allocated
    /// by the server when the output appears (the scene owns the buffer,
    /// so it is not built here), re-allocated on a size change, freed with
    /// the output. `None` when `overview.animate` is off (the default) or when
    /// the allocation failed, and overview then snaps.
    pub atlas: Option<crate::overview::Atlas>,
    /// Which Surfaces go on hardware planes, with its cache and
    /// hysteresis (#3899).
    pub planner: crate::planes::Planner,
    /// The plane layout staged on the backend, and which Surfaces it
    /// places. Default: composite, nothing staged.
    pub decision: crate::planes::Decision,
    /// The staged layout changed (new buffers on the same planes, or a
    /// hysteresis wait wants another decision) and must be committed
    /// even with nothing to paint: a plane-only flip.
    pub planes_dirty: bool,
    /// The `SurfaceHint` format for Surfaces on this output
    /// ([`crate::planes::hint_format`]), read from the planes when the
    /// output appears or changes, not per settle.
    pub hint_format: u32,
    /// The output's planes, re-read when the output appears or changes.
    /// Empty on a backend without planes, which keeps the planes module
    /// off entirely.
    pub plane_info: Vec<nitro_kms::PlaneInfo>,
    /// A commit went in: the backend lit it. The first commit of an
    /// output modesets every other lit one, which drops their layouts.
    pub lit: bool,
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
            scroll: None,
            shadow: shadow.then(|| Shadow::new(width, height)),
            alpha: false,
            alpha_warned: std::cell::Cell::new(false),
            atlas: None,
            planner: crate::planes::Planner::default(),
            decision: crate::planes::Decision::default(),
            planes_dirty: false,
            hint_format: nitro_wire::types::format::NV12,
            plane_info: Vec::new(),
            lit: false,
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
        self.scroll = None;
    }

    /// Add damage from the scene: a client's pixels, a window that moved,
    /// the desktop under one that closed. A frame carrying any of this is
    /// never deferred.
    pub fn damage_content(&mut self, rect: IRect) {
        self.damage.add(rect);
        self.content_damage = true;
        if let Some(scroll) = self.scroll.as_mut() {
            scroll.foreign.add(rect);
        }
    }

    /// Add the damage of a scene update that carried a translation hint
    /// for this output, and remember the hint for the next paint.
    ///
    /// `rects` is the update's whole damage for this output, exactly as
    /// [`OutputState::damage_content`] would have been given it — the hint
    /// never *replaces* damage. What the shadow holds is the last
    /// **painted** state, not the last updated one, so damage already
    /// waiting here becomes foreign: those pixels are stale in the shadow
    /// and must not be copied from. A second hint before a paint blocks
    /// the fast path for that paint — the shadow is two moves behind.
    pub fn damage_scroll(&mut self, rects: &[IRect], hint: PendingScroll) {
        let mut hint = hint;
        if self.scroll.is_some() {
            if let Some(scroll) = self.scroll.as_mut() {
                scroll.blocked = true;
            }
        } else {
            hint.foreign.add_all(&self.damage);
            self.scroll = Some(hint);
        }
        for r in rects {
            self.damage.add(*r);
            self.content_damage = true;
        }
        if let Some(scroll) = self.scroll.as_mut()
            && scroll.blocked
        {
            for r in rects {
                scroll.foreign.add(*r);
            }
        }
    }

    /// Take the pending scroll hint, if any, for the paint about to run.
    /// Whatever happens to that paint, the hint is spent: after it the
    /// shadow holds the new state (moved, or repainted).
    pub fn take_scroll(&mut self) -> Option<PendingScroll> {
        self.scroll.take()
    }

    /// Add damage the server made for its own software cursor.
    ///
    /// Kept apart from [`OutputState::damage_content`] only so that
    /// [`OutputState::cursor_only`] can tell them apart; the region
    /// painted is the union either way.
    pub fn damage_cursor(&mut self, rect: IRect) {
        self.damage.add(rect);
        if let Some(scroll) = self.scroll.as_mut() {
            scroll.foreign.add(rect);
        }
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
        !self.content_damage && !self.retry && !self.planes_dirty
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
        self.needs_raster() || self.planes_dirty
    }

    /// Whether the output buffer has anything to catch up on — as
    /// [`OutputState::needs_paint`], without a plane-only change.
    #[must_use]
    pub fn needs_raster(&self) -> bool {
        self.retry || !self.damage.is_empty() || !self.previous.is_empty()
    }

    /// Drop every pending repaint: the output buffer is not on screen
    /// (direct scanout, #3899), and leaving that mode invalidates.
    pub fn discard_damage(&mut self) {
        self.previous.clear();
        self.damage.clear();
        self.content_damage = false;
        self.retry = false;
        self.scroll = None;
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
        self.planes_dirty = false;
        self.lit = true;
        self.previous = self.damage.take();
        self.content_damage = false;
        self.scroll = None;
        self.damage.clear();
        self.retry = false;
        self.in_flight = std::mem::take(&mut self.painting);
        self.in_flight_input_ns = self.painting_input_ns;
        self.painting_input_ns = 0;
    }

    /// Note that a plane-only commit (`Backend::commit_planes`) went in:
    /// the serials latched for it ride that flip. No buffer swapped, so
    /// the damage history stays exactly as it was.
    pub fn planes_committed(&mut self) {
        self.planes_dirty = false;
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
        // The shadow already holds the moved pixels; a retry must copy
        // them out again, never move them a second time.
        self.scroll = None;
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

/// A scroll-blit hint as an output keeps it between the scene update that
/// produced it and the paint that may use it: the scene's
/// [`Translation`](nitro_scene::Translation), shifted to output-local
/// pixels, plus everything else damaged on the output in the meantime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingScroll {
    /// The node whose subtree moved.
    pub node: nitro_scene::NodeKey,
    /// Whether `node`'s own content moved too (see
    /// [`Translation::moves_node`](nitro_scene::Translation::moves_node)).
    pub moves_node: bool,
    /// Device-pixel delta, new minus old.
    pub delta: (i32, i32),
    /// The fixed clip the moved content is confined to, output-local.
    pub clip: IRect,
    /// Every rect that changed for a reason other than the move, output-
    /// local: the update's own foreign damage, damage that was already
    /// waiting, and anything added until the paint (cursor included).
    pub foreign: Damage,
    /// A second hint arrived before a paint: do not use this one.
    pub blocked: bool,
}

/// The pixels a scroll blit may copy rather than rasterize, or `None`
/// when it may copy nothing. All regions output-local.
///
/// With `d` the delta, `C` the fixed clip, `R` this frame's rasterize
/// region, `S` the moved subtree's own opaque, shift-exact cover, `A`
/// whatever the scene paints above the subtree and `F` everything else
/// that changed (plus the cursor):
///
/// ```text
/// D = (R ∩ S ∩ (C + d) ∩ out ∩ (out + d)) \ (A ∪ F) \ ((A ∪ F) + d)
/// ```
///
/// For `p ∈ D` the previous frame showed, at `p − d`, the subtree's own
/// opaque pixel with nothing on top of it and nothing else changed there
/// since, and the new frame shows that same pixel at `p`. So
/// `shadow[p] ← shadow[p − d]` is exact. Over-approximating `A` or `F`
/// only shrinks `D`; `S`, `R` and the result are exact [`Region`]s.
#[must_use]
pub fn blit_region(
    delta: (i32, i32),
    clip: IRect,
    output: IRect,
    rasterize: &[IRect],
    cover: &Region,
    above: &Region,
    foreign: &Region,
) -> Option<Region> {
    let (dx, dy) = delta;
    let fixed = Region::rect(clip.intersect(&output)).intersect(&Region::rect(
        clip.translate(dx, dy).intersect(&output.translate(dx, dy)),
    ));
    let busy = above.union(foreign);
    let d = Region::from_rects(rasterize)
        .intersect(cover)
        .intersect(&fixed)
        .subtract(&busy)
        .subtract(&busy.translate(dx, dy));
    (!d.overflowed() && !d.is_empty()).then_some(d)
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

/// The margin to use at a given refresh period.
///
/// [`FRAME_MARGIN_NS`], or a quarter of the period when that is smaller.
/// Unchanged at 60 and 120 Hz; see [`FRAME_MARGIN_MAX_SHARE`].
#[must_use]
pub fn frame_margin_ns(refresh_ns: u32) -> u64 {
    let period = u64::from(refresh_ns).max(1);
    FRAME_MARGIN_NS.min(period / FRAME_MARGIN_MAX_SHARE)
}

/// The frame deadline for a client: the next expected vblank minus
/// [`frame_margin_ns`], but never in the past — a client told to aim for a
/// moment that has already gone would only busy-loop. When the margin would
/// take the deadline behind `now`, aim for the vblank after it.
#[must_use]
pub fn frame_deadline(last_vblank_ns: u64, refresh_ns: u32, now_ns: u64) -> u64 {
    let period = u64::from(refresh_ns).max(1);
    let margin = frame_margin_ns(refresh_ns);
    let mut vblank = next_vblank(last_vblank_ns, refresh_ns, now_ns);
    while vblank.saturating_sub(margin) <= now_ns {
        vblank += period;
    }
    vblank - margin
}

/// Total area of a region, in pixels, for the `damage_px` statistic.
#[must_use]
pub fn region_area(region: &[IRect]) -> u64 {
    region.iter().map(|r| r.area().cast_unsigned()).sum()
}

/// Where the cursor is, which shape it is showing, and whether to draw it.
#[derive(Debug, Clone, Copy)]
pub struct CursorState {
    /// Hotspot position in device pixels.
    pub x: i32,
    /// Hotspot position in device pixels.
    pub y: i32,
    /// Which shape the pointer is showing.
    ///
    /// Until M5-E (#3771) this said "chosen by the server alone — there is
    /// no client request for one yet". There is now: a client with pointer
    /// focus may choose over its own content with `SetCursor`, and the
    /// server's chrome still wins (`Server::cursor_choice`). When a client
    /// has hidden the cursor this is a placeholder and `visible` is false.
    pub shape: crate::cursor::Shape,
    /// The whole factor the cursor is magnified by on this output, so a
    /// 2× output gets a 48-device-pixel cursor rather than a physically
    /// half-size one. See [`Cursor::paint_scale`].
    pub scale: i32,
    /// Whether the cursor is drawn at all: false with no pointer device,
    /// and false when a client with pointer focus hid it
    /// (`CursorShape::None`).
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
///
/// `fast_scaled` is [`paint_items`]' flag: scaled opaque XR24 images go
/// through [`Canvas::blit_xrgb_scaled`]. The server sets it only on the
/// output of a **snap** overview, where every scaled item is a thumbnail.
#[allow(clippy::too_many_arguments)] // One paint call's inputs, not a structure: bundling them would be a struct built per frame to satisfy a lint.
pub fn paint_region(
    canvas: &mut Canvas<'_>,
    scene: &Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    output: OutputId,
    region: &[IRect],
    cursor: (&Cursor, CursorState),
    items: &mut Vec<PaintItem>,
    palette: &Palette,
    fast_scaled: bool,
) -> u64 {
    if fast_scaled {
        // A snap overview's thumbnails are exact covers: split the damage
        // along them so nothing under them is painted (#3929).
        return paint_region_shared(
            canvas,
            scene,
            text,
            icons,
            output,
            region,
            cursor,
            items,
            palette,
            fast_scaled,
        );
    }
    let start = std::time::Instant::now();
    for clip in region {
        let clip = clip.intersect(&canvas.bounds());
        if clip.is_empty() {
            continue;
        }
        items.clear();
        scene.paint_list(output, &clip, items);
        paint_clip(
            canvas,
            scene,
            text,
            icons,
            &clip,
            items,
            cursor,
            palette,
            fast_scaled,
        );
    }
    items.clear();
    duration_us(start.elapsed())
}

/// [`paint_region`] for many small rects close together — the thin
/// leftovers of a scroll blit — with **one** paint-list walk over their
/// bounding box instead of one per rect.
///
/// Pixel-identical to [`paint_region`]: each rect is drawn from the items
/// of the shared list whose bounds reach it, in the same order, and every
/// item is re-clipped to the rect, so an item's clip and bounds taken
/// against the larger box are narrowed to exactly what the per-rect list
/// would have held. Occlusion (`opaque_cover` containing the rect) asks
/// the same question of the same world bounds.
#[allow(clippy::too_many_arguments)] // As `paint_region`.
pub fn paint_region_shared(
    canvas: &mut Canvas<'_>,
    scene: &Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    output: OutputId,
    region: &[IRect],
    cursor: (&Cursor, CursorState),
    items: &mut Vec<PaintItem>,
    palette: &Palette,
    fast_scaled: bool,
) -> u64 {
    let start = std::time::Instant::now();
    let area = region
        .iter()
        .fold(IRect::EMPTY, |acc, r| acc.union(r))
        .intersect(&canvas.bounds());
    items.clear();
    scene.paint_list(output, &area, items);
    let mut local: Vec<PaintItem> = Vec::with_capacity(items.len());
    // Declared opaque regions (#3877), in device px: splitting every damage
    // rect along them lets `paint_clip`'s occlusion test skip whatever lies
    // under a translucent-format window's opaque interior (the wallpaper,
    // the background) exactly as it does for an XR24 one.
    // In `fast_scaled` mode the thumbnails' stored rects are covers too.
    let covers: Vec<IRect> = items
        .iter()
        .flat_map(|item| {
            let mut c = opaque_region_device(scene, item);
            if fast_scaled {
                c.extend(fast_scaled_covers(scene, item));
            }
            c
        })
        .collect();
    let covers = Region::from_rects(&covers);
    let mut clips: Vec<IRect> = Vec::with_capacity(region.len());
    for clip in region {
        let clip = clip.intersect(&canvas.bounds());
        if clip.is_empty() {
            continue;
        }
        let whole = Region::rect(clip);
        let inside = whole.intersect(&covers);
        let outside = whole.subtract(&covers);
        if covers.is_empty() || inside.is_empty() || inside.overflowed() || outside.overflowed() {
            clips.push(clip);
        } else {
            clips.extend(inside.rects());
            clips.extend(outside.rects());
        }
    }
    for clip in clips {
        local.clear();
        local.extend(items.iter().filter_map(|item| {
            let bounds = item.bounds.intersect(&clip);
            (!bounds.is_empty()).then_some(PaintItem {
                bounds,
                clip: item.clip.intersect(&clip),
                ..*item
            })
        }));
        paint_clip(
            canvas,
            scene,
            text,
            icons,
            &clip,
            &local,
            cursor,
            palette,
            fast_scaled,
        );
    }
    items.clear();
    duration_us(start.elapsed())
}

/// Paint one clip rect from its paint list: background unless occluded,
/// the items, then the cursor.
#[allow(clippy::too_many_arguments)] // One rect's inputs.
fn paint_clip(
    canvas: &mut Canvas<'_>,
    scene: &Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    clip: &IRect,
    items: &[PaintItem],
    cursor: (&Cursor, CursorState),
    palette: &Palette,
    fast_scaled: bool,
) {
    let (width, height) = (canvas.width(), canvas.height());
    let (cursor_image, cursor_state) = cursor;
    // Everything below the last item that opaquely covers the whole
    // clip is invisible — including the background. This is the scene's
    // occlusion promise, and it is deliberately conservative there, so
    // trusting it here cannot produce a wrong pixel.
    let covered = items
        .iter()
        .rposition(|item| covers_clip(scene, item, clip, fast_scaled));
    // A full-clip translucent solid rect right above the base (the
    // overview's scrim) is folded into the base's store: one pass instead
    // of a store and a blend, byte for byte the same pixels (#3929).
    let above = covered.map_or(0, |i| i + 1);
    let overlay = items.get(above).and_then(|item| clip_overlay(item, clip));
    let fused = overlay.is_some_and(|overlay| match covered {
        None => {
            paint_background_overlaid(canvas, clip, width, height, palette, Some(overlay));
            true
        }
        Some(i) => fill_overlaid(canvas, clip, &items[i], overlay),
    });
    let first = if fused {
        above + 1
    } else {
        if covered.is_none() {
            paint_background(canvas, clip, width, height, palette);
        }
        covered.unwrap_or(0)
    };
    for item in items.get(first..).unwrap_or(&[]) {
        if fast_scaled && paint_xrgb_scaled(canvas, clip, item, scene) {
            continue;
        }
        paint_item(canvas, clip, item, scene, text, icons, palette);
    }
    if cursor_state.visible {
        cursor_image.paint(
            canvas,
            clip,
            cursor_state.x,
            cursor_state.y,
            cursor_state.shape,
            cursor_state.scale,
        );
    }
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

/// Whether `item` alone hides everything under `clip`: the scene's
/// [`PaintItem::opaque_cover`], or one rect of the image's declared opaque
/// region (`SetOpaqueRegion`, #3877) containing the whole clip. With
/// `fast_scaled`, also a rect [`paint_xrgb_scaled`] stores in full
/// ([`fast_scaled_covers`]).
fn covers_clip(scene: &Scene, item: &PaintItem, clip: &IRect, fast_scaled: bool) -> bool {
    item.opaque_cover()
        .is_some_and(|cover| cover.contains_rect(clip))
        || opaque_region_device(scene, item)
            .iter()
            .any(|r| r.contains_rect(clip))
        || (fast_scaled
            && fast_scaled_covers(scene, item)
                .iter()
                .any(|r| r.contains_rect(clip)))
}

/// The overlay `item` lays over all of `clip`, if it is one: a solid rect
/// with square corners and no visible border, axis-aligned, whose exact
/// device rect and clip contain the whole of `clip`, so that
/// [`Canvas::fill_rect`] would give every pixel of `clip` the same blend.
/// The overview's scrim is the case this exists for.
fn clip_overlay(item: &PaintItem, clip: &IRect) -> Option<Overlay> {
    let PaintKind::Rect {
        size,
        fill: SceneFill::Solid(c),
        corner_radius,
        border,
    } = item.kind
    else {
        return None;
    };
    if corner_radius > 0.0
        || border.is_some_and(nitro_scene::Border::is_visible)
        || !item.transform.is_axis_aligned()
        || !item.clip.contains_rect(clip)
    {
        return None;
    }
    let exact = item
        .transform
        .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
    #[allow(clippy::cast_precision_loss)] // device coordinates, far below 2^24
    let inside = exact.x <= clip.x as f32
        && exact.y <= clip.y as f32
        && exact.right() >= clip.right() as f32
        && exact.bottom() >= clip.bottom() as f32;
    inside.then(|| Overlay::new(c, item.opacity))
}

/// Paint the covering `item` over all of `clip` with `overlay` folded in,
/// exactly as `paint_item` followed by the overlay's own fill would.
/// Only a square-cornered, borderless rect with an opaque fill at opacity
/// 1 qualifies; `false`, having painted nothing, otherwise (an image
/// wallpaper, say: that is painted and then blended as before).
fn fill_overlaid(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    item: &PaintItem,
    overlay: Overlay,
) -> bool {
    let PaintKind::Rect {
        fill,
        corner_radius,
        border,
        ..
    } = item.kind
    else {
        return false;
    };
    if corner_radius > 0.0
        || border.is_some_and(nitro_scene::Border::is_visible)
        || item.opacity < 1.0
        || !item.clip.contains_rect(clip)
    {
        return false;
    }
    raster_fill(fill, item).is_some_and(|f| canvas.fill_opaque_overlaid(clip, &f, overlay))
}

/// An image item's declared opaque region mapped to device px and clipped
/// to where the item paints — empty unless the item is an AR24 image drawn
/// 1:1, pixel-aligned, at opacity 1 (the only mapping under which the
/// region's pixels land on whole device pixels unblended).
fn opaque_region_device(scene: &Scene, item: &PaintItem) -> Vec<IRect> {
    let (PaintKind::Image {
        size, buffer, src, ..
    }
    | PaintKind::Surface {
        size, buffer, src, ..
    }) = item.kind
    else {
        return Vec::new();
    };
    let Ok(node) = scene.node(item.node) else {
        return Vec::new();
    };
    let opaque = node.opaque_region();
    if opaque.is_empty()
        || item.opacity < 1.0
        || !item.shift_exact()
        || scene
            .buffer(buffer)
            .map_or(true, |b| b.desc().format != format::AR24)
    {
        return Vec::new();
    }
    let device = item
        .transform
        .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
    // `shift_exact` guarantees integer device coordinates and a 1:1 size.
    #[allow(clippy::cast_possible_truncation)]
    let (dx, dy) = (device.x as i32 - src.x, device.y as i32 - src.y);
    let bound = item.clip.intersect(&item.bounds);
    opaque
        .iter()
        .map(|r| r.intersect(&src).translate(dx, dy).intersect(&bound))
        .filter(|r| !r.is_empty())
        .collect()
}

/// Microseconds of a duration, saturating (a paint that took longer than
/// 584 000 years is not a case worth a `u128`).
fn duration_us(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

/// Draw `items` (a [`Scene::paint_list`] or [`Scene::paint_window`]
/// list, global device pixels) into `canvas`, clipped to `clip`
/// (canvas pixels), each item moved by `offset` first — the canvas's
/// origin in global device pixels, `(0, 0)` for an output at the origin.
/// No background and no cursor: this is the item loop alone, for a
/// canvas that is not an output (the overview's thumbnail atlas).
///
/// `fast_scaled` sends an opaque (XR24) image drawn scaled, at opacity 1
/// and axis-aligned, through [`Canvas::blit_xrgb_scaled`] (~4 ns/px)
/// onto its device rect rounded to whole pixels, rather than the general
/// resampling blend (~12 ns/px) onto the exact one. They agree to ±1
/// inside the rect; the rect's fractional edge pixels are the
/// difference. The live output path sets it only on the output of a
/// snap overview (whose scaled items are all thumbnails), so no live
/// pixel outside one changes.
#[allow(clippy::too_many_arguments)] // One paint call's inputs, as `paint_region`.
pub fn paint_items(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    offset: (i32, i32),
    items: &[PaintItem],
    scene: &Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    palette: &Palette,
    fast_scaled: bool,
) {
    let clip = clip.intersect(&canvas.bounds());
    if clip.is_empty() {
        return;
    }
    #[allow(clippy::cast_precision_loss)] // device coordinates, far below 2^24
    let shift = nitro_core::Transform::translate(-offset.0 as f32, -offset.1 as f32);
    for item in items {
        let item = if offset == (0, 0) {
            *item
        } else {
            PaintItem {
                transform: shift.then(&item.transform),
                clip: item.clip.translate(-offset.0, -offset.1),
                bounds: item.bounds.translate(-offset.0, -offset.1),
                ..*item
            }
        };
        if fast_scaled && paint_xrgb_scaled(canvas, &clip, &item, scene) {
            continue;
        }
        paint_item(canvas, &clip, &item, scene, text, icons, palette);
    }
}

/// What [`paint_xrgb_scaled`] needs of an item it applies to.
struct ScaledTarget<'a> {
    /// The device rect, rounded to whole pixels.
    dst: IRect,
    /// The source crop.
    src: IRect,
    /// The pixels, read as `Xrgb8888` (alpha ignored).
    image: RasterImage<'a>,
    /// The straight-alpha format, for an AR24 image: its declared opaque
    /// region is what may be stored, mapped into `dst` (device px,
    /// already inset and rounded inward). `None` for XR24: all of it.
    opaque: Option<Vec<IRect>>,
    /// The exact (fractional) device rect, for the general blit.
    exact: Rect,
}

/// Whether [`paint_xrgb_scaled`] applies to `item`, and with what: an
/// image at opacity 1, drawn axis-aligned and not 1:1, that is XR24 or an
/// AR24 whose client declared an opaque region (`SetOpaqueRegion`) that
/// maps onto at least one whole device pixel.
fn scaled_target<'a>(scene: &'a Scene, item: &PaintItem) -> Option<ScaledTarget<'a>> {
    let PaintKind::Image {
        size, buffer, src, ..
    } = item.kind
    else {
        return None;
    };
    if item.opacity < 1.0 || !item.transform.is_axis_aligned() || item.shift_exact() {
        return None;
    }
    let buffer = scene.buffer(buffer).ok()?;
    let desc = buffer.desc();
    let exact = item
        .transform
        .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
    #[allow(clippy::cast_possible_truncation)] // device pixels
    let dst = IRect::from_edges(
        exact.x.round() as i32,
        exact.y.round() as i32,
        exact.right().round() as i32,
        exact.bottom().round() as i32,
    );
    let image = RasterImage {
        data: buffer.data(),
        width: desc.w,
        height: desc.h,
        stride: desc.stride,
        format: PixelFormat::Xrgb8888,
    };
    let opaque = match desc.format {
        format::XR24 => None,
        format::AR24 => {
            let node = scene.node(item.node).ok()?;
            let mapped: Vec<IRect> = node
                .opaque_region()
                .iter()
                .filter_map(|r| opaque_texels_to_device(r, &src, &dst))
                .collect();
            if mapped.is_empty() {
                return None;
            }
            Some(mapped)
        }
        _ => return None,
    };
    Some(ScaledTarget {
        dst,
        src,
        image,
        opaque,
        exact,
    })
}

/// The device pixels of `dst` whose bilinear taps (from `src` stretched
/// onto `dst`) all land inside the opaque source rect `r`.
///
/// The rect is inset by one texel on every side that is not the crop's
/// edge — a tap reaches one texel past the sample point, and at the crop's
/// edge the sampler clamps instead — then mapped and rounded **inward**. A
/// pixel `d` inside then samples at `s ≥ x0 − ½` and `s < x1 − ½` (in the
/// inset rect `[x0, x1)`), so both its taps `⌊s⌋` and `⌊s⌋ + 1` are
/// texels of `r`.
fn opaque_texels_to_device(r: &IRect, src: &IRect, dst: &IRect) -> Option<IRect> {
    let r = r.intersect(src);
    if r.is_empty() || src.is_empty() || dst.is_empty() {
        return None;
    }
    let inset = |lo: i32, hi: i32, first: i32, end: i32| {
        (
            if lo > first { lo + 1 } else { lo },
            if hi < end { hi - 1 } else { hi },
        )
    };
    let (x0, x1) = inset(r.x, r.right(), src.x, src.right());
    let (y0, y1) = inset(r.y, r.bottom(), src.y, src.bottom());
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    // Device coordinate of source edge `v`: `d0 + (v − s0) · dlen / slen`,
    // rounded up for a leading edge and down for a trailing one, in exact
    // integer arithmetic.
    let map = |v: i32, s0: i32, slen: i32, d0: i32, dlen: i32, up: bool| -> i32 {
        let num = i64::from(v - s0) * i64::from(dlen);
        let den = i64::from(slen);
        let q = if up {
            num.div_euclid(den) + i64::from(num.rem_euclid(den) != 0)
        } else {
            num.div_euclid(den)
        };
        d0 + i32::try_from(q).unwrap_or(i32::MAX - d0)
    };
    let out = IRect::from_edges(
        map(x0, src.x, src.w, dst.x, dst.w, true),
        map(y0, src.y, src.h, dst.y, dst.h, true),
        map(x1, src.x, src.w, dst.x, dst.w, false),
        map(y1, src.y, src.h, dst.y, dst.h, false),
    )
    .intersect(dst);
    (!out.is_empty()).then_some(out)
}

/// The device rects [`paint_xrgb_scaled`] **stores** for `item`, clipped
/// to the item's clip: exact covers, so in `fast_scaled` mode nothing
/// under them needs painting. Empty when the fast path does not apply.
fn fast_scaled_covers(scene: &Scene, item: &PaintItem) -> Vec<IRect> {
    let Some(t) = scaled_target(scene, item) else {
        return Vec::new();
    };
    let rects = t.opaque.unwrap_or_else(|| vec![t.dst]);
    rects
        .iter()
        .map(|r| r.intersect(&item.clip))
        .filter(|r| !r.is_empty())
        .collect()
}

/// [`paint_items`]' fast path: an opaque image at opacity 1, drawn
/// axis-aligned and not 1:1, stored with [`Canvas::blit_xrgb_scaled`]
/// onto its device rect rounded to whole pixels. An XR24 image goes that
/// way whole; an AR24 one only inside its declared opaque region
/// ([`scaled_target`]), and the rest of it through the general blend onto
/// the exact rect, as before. A client that lies about its region gets
/// opaque pixels there, as with the 1:1 `blit_with_opaque_region`.
/// Returns `false`, having painted nothing, when it does not apply.
fn paint_xrgb_scaled(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    item: &PaintItem,
    scene: &Scene,
) -> bool {
    let Some(t) = scaled_target(scene, item) else {
        return false;
    };
    let clip = clip.intersect(&item.clip);
    if t.dst.is_empty() || clip.is_empty() {
        return true;
    }
    let Some(opaque) = t.opaque else {
        canvas.blit_xrgb_scaled(&clip, &t.dst, &t.image, &t.src);
        return true;
    };
    let stored = Region::from_rects(&opaque).intersect(&Region::rect(clip));
    let rest = Region::rect(clip).subtract(&stored);
    let straight = RasterImage {
        format: PixelFormat::Argb8888,
        ..t.image
    };
    if stored.overflowed() || rest.overflowed() {
        canvas.blit(&clip, &t.exact, &straight, &t.src, 1.0);
        return true;
    }
    for r in stored.rects() {
        canvas.blit_xrgb_scaled(&r, &t.dst, &t.image, &t.src);
    }
    for r in rest.rects() {
        canvas.blit(&r, &t.exact, &straight, &t.src, 1.0);
    }
    true
}

/// Draw one paint item, already clipped by the caller to a damage rect.
#[allow(clippy::too_many_lines)] // One arm per kind.
fn paint_item(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    item: &PaintItem,
    scene: &Scene,
    text: &mut TextEngine,
    icons: &mut IconEngine,
    palette: &Palette,
) {
    let clip = clip.intersect(&item.clip);
    if clip.is_empty() {
        return;
    }
    match item.kind {
        PaintKind::Hole { .. } => {
            // A store, not a blend: the scene's bounds are already the
            // device rect it promised as `opaque_cover`, and opacity is
            // ignored (Surfaces on a plane are opaque, #3898).
            canvas.clear_irect(&clip, &item.bounds);
        }
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
        PaintKind::Image {
            size, buffer, src, ..
        } => {
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
            let opaque = scene.node(item.node).map_or(&[][..], |n| n.opaque_region());
            if !blit_with_opaque_region(canvas, &clip, item, &device, &image, &src, opaque) {
                canvas.blit(&clip, &device, &image, &src, item.opacity);
            }
        }
        PaintKind::Surface {
            size,
            buffer,
            src,
            color,
            ..
        } => paint_surface(canvas, &clip, item, scene, size, buffer, src, color),
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
        PaintKind::Icon {
            icon,
            origin,
            size,
            role,
        } => {
            // Narrowed to the node's box for the same reason text is: an
            // icon's mask is a square rasterised from a 16-unit grid, and
            // a rounding that put its last row one pixel past the bounds
            // would put a pixel outside what the scene damaged for it —
            // which nothing would ever repaint.
            let clip = clip.intersect(&item.bounds);
            if clip.is_empty() {
                return;
            }
            // The palette is resolved *here*, per frame, which is why a
            // `theme.scheme` flip recolours icons with no client message
            // and no re-raster: the cache holds coverage, not pixels.
            icons.paint(
                canvas,
                &clip,
                &item.transform,
                icon,
                origin,
                size,
                role,
                palette,
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

/// Paint an AR24 image whose client declared an opaque region
/// (`SetOpaqueRegion`, #3877): the region through the opaque copy (the same
/// pixels read as `Xrgb8888`, so the alpha byte is ignored), the rest through
/// the straight-alpha blend. Returns `false`, having painted nothing, when
/// the fast path does not apply — no region, an opaque format already,
/// opacity below 1, or a mapping that is not 1:1 and pixel-aligned (a
/// scaled window, an overview thumbnail) — so the caller blends it all.
///
/// Exactly equal to the plain blend whenever the client told the truth,
/// because a straight-alpha blend at `a == 255` *is* a copy.
fn blit_with_opaque_region(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    item: &PaintItem,
    device: &Rect,
    image: &RasterImage<'_>,
    src: &IRect,
    opaque: &[IRect],
) -> bool {
    if opaque.is_empty()
        || image.format != PixelFormat::Argb8888
        || item.opacity < 1.0
        || !item.shift_exact()
    {
        return false;
    }
    // `shift_exact` guarantees integer device coordinates and a 1:1 size.
    #[allow(clippy::cast_possible_truncation)]
    let (dx, dy) = (device.x as i32 - src.x, device.y as i32 - src.y);
    let mapped: Vec<IRect> = opaque
        .iter()
        .map(|r| r.intersect(src).translate(dx, dy).intersect(clip))
        .filter(|r| !r.is_empty())
        .collect();
    if mapped.is_empty() {
        return false;
    }
    let region = Region::from_rects(&mapped);
    let rest = Region::rect(*clip).subtract(&region);
    if region.overflowed() || rest.overflowed() {
        return false;
    }
    let as_opaque = RasterImage {
        format: PixelFormat::Xrgb8888,
        ..*image
    };
    for r in region.rects() {
        canvas.blit(&r, device, &as_opaque, src, 1.0);
    }
    for r in rest.rects() {
        canvas.blit(&r, device, image, src, 1.0);
    }
    true
}

/// Draw a CPU-composited Surface (#3897): the node's device rect rounded
/// to whole pixels, the buffer converted by its fourcc. YUV and XR24 are
/// stores and ignore `item.opacity` in v1 (translucent video is later work,
/// `docs/surfaces.md`); AR24 blends like an image.
#[allow(clippy::too_many_arguments)]
fn paint_surface(
    canvas: &mut Canvas<'_>,
    clip: &IRect,
    item: &PaintItem,
    scene: &Scene,
    size: (f32, f32),
    buffer: nitro_scene::BufferKey,
    src: IRect,
    color: SurfaceColor,
) {
    let Ok(buffer) = scene.buffer(buffer) else {
        return;
    };
    let desc = buffer.desc();
    let data = buffer.data();
    let local = Rect::new(0.0, 0.0, size.0, size.1);
    let exact = item.transform.apply_rect(&local);
    // Outward, like the scene's `bounds`: a video surface lands on whole
    // pixels, and the damage it was given is exactly that rect.
    let dst = exact.round_out();
    if !buffer.cpu_readable() {
        // A client dma-buf the CPU cannot read (tiled, compressed; #3918):
        // the documented placeholder until the planes module scans it
        // out. Never a read of pages that are not there.
        let [b, g, r] = HOLE_PLACEHOLDER;
        canvas.fill_irect(clip, &dst, Color::rgb(r, g, b));
        PLACEHOLDER_PAINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    let enc = yuv_encoding(color);
    let from = |off: u32| data.get(off as usize..).unwrap_or(&[]);
    match desc.format {
        format::NV12 => {
            let Some((off1, stride1, _)) = desc.plane1 else {
                return;
            };
            let frame = Nv12 {
                y: from(desc.offset0),
                y_stride: desc.stride,
                uv: from(off1),
                uv_stride: stride1,
                width: desc.w,
                height: desc.h,
            };
            canvas.blit_nv12(clip, &dst, &frame, &src, enc);
        }
        format::YUYV | format::UYVY => {
            let frame = Packed422 {
                data: from(desc.offset0),
                stride: desc.stride,
                width: desc.w,
                height: desc.h,
                order: if desc.format == format::YUYV {
                    Packed422Order::Yuyv
                } else {
                    Packed422Order::Uyvy
                },
            };
            canvas.blit_yuyv(clip, &dst, &frame, &src, enc);
        }
        format::XR24 | format::AR24 => {
            let image = RasterImage {
                data: from(desc.offset0),
                width: desc.w,
                height: desc.h,
                stride: desc.stride,
                format: if desc.format == format::XR24 {
                    PixelFormat::Xrgb8888
                } else {
                    PixelFormat::Argb8888
                },
            };
            let one_to_one = dst.w == src.w && dst.h == src.h;
            if desc.format == format::AR24 {
                // A declared opaque region (#3919: Chromium's GPU process
                // presents its CSD window here) takes the copy path.
                let opaque = scene.node(item.node).map_or(&[][..], |n| n.opaque_region());
                if !blit_with_opaque_region(canvas, clip, item, &exact, &image, &src, opaque) {
                    canvas.blit(clip, &exact, &image, &src, item.opacity);
                }
            } else if one_to_one {
                canvas.blit(clip, &dst.to_rect(), &image, &src, 1.0);
            } else {
                canvas.blit_xrgb_scaled(clip, &dst, &image, &src);
            }
        }
        _ => {}
    }
}

/// The rasterizer's YUV encoding for a surface's colour metadata.
fn yuv_encoding(color: SurfaceColor) -> YuvEncoding {
    YuvEncoding::new(
        match color.matrix {
            SceneColorMatrix::Bt601 => YuvMatrix::Bt601,
            SceneColorMatrix::Bt709 => YuvMatrix::Bt709,
            SceneColorMatrix::Bt2020 => YuvMatrix::Bt2020,
        },
        match color.range {
            SceneColorRange::Limited => YuvRange::Limited,
            SceneColorRange::Full => YuvRange::Full,
        },
    )
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

/// Switch `output`'s scanout to `ARGB8888` exactly while `holes` is true
/// and the plane can blend alpha, back to `XRGB8888` otherwise (#3898).
/// Calls the backend only when the answer changes; logs once when holes are
/// on an output that cannot show them.
pub fn select_scanout_alpha(
    backend: &mut dyn nitro_kms::Backend,
    output: &mut OutputState,
    holes: bool,
) {
    let id = output.kms_id;
    let want = holes && backend.scanout_alpha(id);
    if holes && !want && !output.alpha_warned.get() {
        crate::warn!("{id}: holes on screen but the primary plane cannot scan out ARGB8888");
        output.alpha_warned.set(true);
    }
    if want == output.alpha {
        return;
    }
    match backend.set_scanout_alpha(id, want) {
        Ok(()) => output.alpha = want,
        Err(e) => crate::warn!("{id}: scanout alpha {want}: {e}"),
    }
}

/// Surface paints that showed [`HOLE_PLACEHOLDER`] because the buffer was
/// a client dma-buf the CPU cannot read (#3918). Process-wide because
/// painting may run on a worker thread; `stats` reads it.
static PLACEHOLDER_PAINTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The `dmabuf_placeholder_paints` stat.
#[must_use]
pub fn placeholder_paints() -> u64 {
    PLACEHOLDER_PAINTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// What `shot` shows where the screen has a hole and the Surface behind it
/// is not CPU-readable: opaque 50 % grey (`0x808080`). A screenshot always
/// comes out opaque; a hole never reads as black-with-alpha-0.
pub const HOLE_PLACEHOLDER: [u8; 3] = [0x80, 0x80, 0x80];

/// Composite an image of the (premultiplied ARGB) screen over what is
/// behind its holes, so a screenshot is honest and opaque: for every pixel
/// with `a < 255`, `c += round(u * (255 - a) / 255)` and `a = 255`, where
/// `u` is `underlay(x, y)` as `[b, g, r]`.
///
/// A screen with no holes is a pure scan that changes nothing. Returns
/// whether any pixel was translucent.
pub fn fill_holes(image: &mut Image, underlay: impl Fn(u32, u32) -> [u8; 3]) -> bool {
    let mut any = false;
    let (width, stride) = (image.width as usize, image.stride as usize);
    for (y, row) in image.data.chunks_mut(stride).enumerate() {
        for (x, p) in row[..width * 4].chunks_exact_mut(4).enumerate() {
            let a = p[3];
            if a == 255 {
                continue;
            }
            any = true;
            #[allow(clippy::cast_possible_truncation)] // image dimensions fit u32
            let u = underlay(x as u32, y as u32);
            let inv = 255 - u32::from(a);
            for (c, u) in p[..3].iter_mut().zip(u) {
                let t = u32::from(u) * inv + 128;
                let v = u32::from(*c) + ((t + (t >> 8)) >> 8);
                *c = v.min(255) as u8;
            }
            p[3] = 255;
        }
    }
    any
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
        // The rates `output.<c>.mode` makes reachable on the test box.
        assert_eq!(refresh_ns(120_000), 8_333_333);
        assert_eq!(refresh_ns(240_000), 4_166_666);
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
    fn the_margin_is_unchanged_at_60_and_120_and_shrinks_only_past_that() {
        // The audit's question, answered as a test rather than a comment:
        // the 2 ms margin is a *measured* rasterization pass, so it must
        // not scale with the panel — and it does not, at either rate the
        // box can actually run.
        assert_eq!(frame_margin_ns(refresh_ns(60_000)), FRAME_MARGIN_NS);
        assert_eq!(frame_margin_ns(refresh_ns(120_000)), FRAME_MARGIN_NS);
        // 8.33 ms / 4 = 2.08 ms, just above the constant — which is how
        // close 120 Hz is to the cap, and why the cap is here at all.
        assert!(u64::from(refresh_ns(120_000)) / 4 > FRAME_MARGIN_NS);
        // At 240 Hz a fixed 2 ms would be most of a 4.17 ms frame, so the
        // quarter-period cap takes over and the client keeps three
        // quarters of every frame.
        let fast = refresh_ns(240_000);
        assert_eq!(frame_margin_ns(fast), u64::from(fast) / 4);
        assert!(frame_margin_ns(fast) < FRAME_MARGIN_NS);
        // The deadline follows, and is still strictly in the future.
        let now = 1_000;
        let d = frame_deadline(now, fast, now);
        assert_eq!(d, now + u64::from(fast) - frame_margin_ns(fast));
        assert!(d > now);
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
        paint_item(
            &mut canvas,
            &surface,
            &item,
            &Scene::new(),
            &mut engine,
            &mut IconEngine::new(),
            &Palette::light(),
        );

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

    /// `translate_region` against a naive copy out of a snapshot, in every
    /// direction, with overlapping source and destination and ragged
    /// regions: every pixel in `dst` is its source's old value, every
    /// pixel outside is byte-identical.
    #[test]
    fn translate_region_matches_a_snapshot_copy() {
        let (w, h, stride) = (40u32, 30u32, 192u32);
        let dst = Region::from_rects(&[
            IRect::new(3, 4, 20, 10),
            IRect::new(10, 12, 25, 9),
            IRect::new(0, 25, 7, 3),
        ]);
        for (dx, dy) in [(0, 5), (0, -5), (4, 0), (-4, 0), (3, -2), (-7, 6), (1, 1)] {
            let mut shadow = Shadow::new(w, h);
            shadow.ensure(w, h, stride);
            paint_pattern(&mut shadow);
            let before = shadow.image();
            let valid = shadow_bounds(&shadow).intersect(&shadow_bounds(&shadow).translate(dx, dy));
            let dst = dst.intersect(&Region::rect(valid));
            shadow.translate_region(&dst, dx, dy);
            let after = shadow.image();
            for y in 0..h.cast_signed() {
                for x in 0..w.cast_signed() {
                    let (ux, uy) = (x.cast_unsigned(), y.cast_unsigned());
                    let want = if dst.contains(x, y) {
                        before.pixel((x - dx).cast_unsigned(), (y - dy).cast_unsigned())
                    } else {
                        before.pixel(ux, uy)
                    };
                    assert_eq!(after.pixel(ux, uy), want, "d=({dx},{dy}) at ({x},{y})");
                }
            }
        }
    }

    fn shadow_bounds(s: &Shadow) -> IRect {
        IRect::new(0, 0, s.width().cast_signed(), s.height().cast_signed())
    }

    fn hint() -> PendingScroll {
        PendingScroll {
            node: nitro_scene::NodeKey::from_parts(0, 0),
            moves_node: true,
            delta: (0, -16),
            clip: IRect::new(0, 0, 100, 50),
            foreign: Damage::new(),
            blocked: false,
        }
    }

    #[test]
    fn a_scroll_hint_lives_until_the_next_paint_and_collects_foreign_damage() {
        let mut s = state();
        frame(&mut s);
        frame(&mut s);
        // Damage waiting before the hint: the shadow does not have it yet.
        let early = IRect::new(0, 0, 5, 5);
        s.damage_content(early);
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        // And anything after it, content or cursor.
        let later = IRect::new(60, 10, 4, 4);
        s.damage_content(later);
        let cursor = IRect::new(80, 30, 8, 8);
        s.damage_cursor(cursor);
        assert!(!s.cursor_only());
        let h = s.take_scroll().expect("pending");
        assert!(!h.blocked);
        for r in [early, later, cursor] {
            assert!(h.foreign.intersects(&r), "{r:?} in {:?}", h.foreign);
        }
        // Taken once.
        assert!(s.take_scroll().is_none());
    }

    /// An invalidated output (plug, resume, mode set) has the whole output
    /// waiting when the hint arrives, so everything is foreign and the
    /// blit has nothing it may copy.
    #[test]
    fn a_hint_after_an_invalidate_leaves_nothing_to_copy() {
        let mut s = state();
        frame(&mut s);
        frame(&mut s);
        s.invalidate();
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        let h = s.take_scroll().expect("pending");
        let foreign = Region::from_rects(h.foreign.rects());
        let out = s.bounds();
        let d = blit_region(
            h.delta,
            h.clip,
            out,
            &s.rasterize_region(),
            &Region::rect(out),
            &Region::new(),
            &foreign,
        );
        assert!(d.is_none(), "{d:?}");
    }

    #[test]
    fn a_second_hint_before_a_paint_blocks_the_first() {
        let mut s = state();
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        assert!(s.take_scroll().expect("pending").blocked);
    }

    #[test]
    fn commit_invalidate_and_a_failed_commit_each_drop_the_hint() {
        let mut s = state();
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        s.committed();
        assert!(s.take_scroll().is_none(), "committed");
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        s.invalidate();
        assert!(s.take_scroll().is_none(), "invalidate");
        s.damage_scroll(&[IRect::new(0, 0, 100, 50)], hint());
        s.commit_failed(&[IRect::new(0, 0, 100, 50)]);
        assert!(s.take_scroll().is_none(), "a retry must never move twice");
    }

    /// The hint never replaces damage: the regions an output paints and
    /// copies are the same with it as without it.
    #[test]
    fn a_hint_leaves_the_damage_exactly_as_it_was() {
        let rects = [IRect::new(0, 0, 100, 20), IRect::new(0, 30, 100, 20)];
        let mut a = state();
        let mut b = state();
        frame(&mut a);
        frame(&mut b);
        for r in rects {
            a.damage_content(r);
        }
        b.damage_scroll(&rects, hint());
        assert_eq!(a.rasterize_region(), b.rasterize_region());
        assert_eq!(a.repaint_region(), b.repaint_region());
    }

    #[test]
    fn the_blit_region_excludes_the_busy_pixels_and_their_sources() {
        let out = IRect::new(0, 0, 100, 100);
        let clip = IRect::new(0, 0, 100, 60);
        let cover = Region::rect(IRect::new(0, 0, 100, 60));
        let above = Region::rect(IRect::new(10, 10, 10, 10));
        let d = blit_region((0, -5), clip, out, &[out], &cover, &above, &Region::new()).unwrap();
        // The band exposed at the bottom of the clip has no source.
        assert!(!d.contains(50, 57));
        assert!(d.contains(50, 54));
        // Neither the window above nor what would be copied out of it.
        assert!(!d.contains(15, 15));
        assert!(!d.contains(15, 7));
        assert!(!d.contains(15, 60), "outside the clip");
        // A delta that jumps past the whole clip leaves nothing to copy.
        assert!(
            blit_region(
                (0, -70),
                clip,
                out,
                &[out],
                &cover,
                &Region::new(),
                &Region::new()
            )
            .is_none()
        );
    }

    // ------------------------------------------------ opaque region (#3877)

    /// A 40×30 AR24 image: opaque everywhere except a transparent 4 px
    /// border ring and a half-alpha stripe on row 10. `lie` sets alpha 0
    /// at (20, 15), inside where the tests declare the region.
    fn window_pixels(lie: bool) -> Vec<u8> {
        let (w, h) = (40u32, 30u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                let edge = x.min(y).min(w - 1 - x).min(h - 1 - y);
                let a = if edge < 4 {
                    (edge * 60) as u8
                } else if y == 10 {
                    128
                } else {
                    255
                };
                px[o] = (x * 5) as u8;
                px[o + 1] = (y * 7) as u8;
                px[o + 2] = (x * y) as u8;
                px[o + 3] = if lie && x == 20 && y == 15 { 0 } else { a };
            }
        }
        px
    }

    fn image_item(transform: nitro_core::Transform, opacity: f32) -> PaintItem {
        PaintItem {
            node: nitro_scene::NodeKey::from_parts(0, 0),
            window: nitro_scene::WindowKey::from_parts(0, 0),
            kind: PaintKind::Image {
                size: (40.0, 30.0),
                buffer: nitro_scene::BufferKey::from_parts(0, 0),
                src: IRect::new(0, 0, 40, 30),
                opaque: false,
            },
            transform,
            clip: IRect::new(0, 0, 64, 48),
            opacity,
            bounds: IRect::new(0, 0, 64, 48),
        }
    }

    /// Paint with the opaque-region path (falling back as `paint_item`
    /// does) and with the plain blend; return both canvases.
    fn paint_both(
        px: &[u8],
        item: &PaintItem,
        clip: IRect,
        opaque: &[IRect],
    ) -> (Vec<u8>, Vec<u8>, bool) {
        let (w, h) = (64u32, 48u32);
        let image = RasterImage {
            data: px,
            width: 40,
            height: 30,
            stride: 160,
            format: PixelFormat::Argb8888,
        };
        let src = IRect::new(0, 0, 40, 30);
        let PaintKind::Image { size, .. } = item.kind else {
            unreachable!()
        };
        let device = item
            .transform
            .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
        let bg: Vec<u8> = (0..w * h * 4)
            .map(|i| if i % 4 == 3 { 0 } else { (i * 11) as u8 })
            .collect();
        let mut fast = bg.clone();
        let mut canvas = Canvas::new(&mut fast, w, h, w * 4);
        let took = blit_with_opaque_region(&mut canvas, &clip, item, &device, &image, &src, opaque);
        if !took {
            canvas.blit(&clip, &device, &image, &src, item.opacity);
        }
        let mut plain = bg;
        let mut canvas = Canvas::new(&mut plain, w, h, w * 4);
        canvas.blit(&clip, &device, &image, &src, item.opacity);
        (fast, plain, took)
    }

    const INNER: IRect = IRect::new(4, 4, 32, 22);

    #[test]
    fn an_honest_opaque_region_paints_exactly_the_blend() {
        let px = window_pixels(false);
        let item = image_item(nitro_core::Transform::translate(7.0, 5.0), 1.0);
        // Rows 4..10 and 11..26 inside the ring are opaque; row 10 is not.
        let region = [IRect::new(4, 4, 32, 6), IRect::new(4, 11, 32, 15)];
        for clip in [
            IRect::new(0, 0, 64, 48),
            // Partly overlapping the region and the ring.
            IRect::new(9, 3, 20, 13),
            IRect::new(30, 20, 30, 20),
        ] {
            let (fast, plain, took) = paint_both(&px, &item, clip, &region);
            assert!(took, "the fast path must be taken for {clip:?}");
            assert!(fast == plain, "clip {clip:?}");
        }
    }

    #[test]
    fn a_lying_region_paints_opaque_inside_it() {
        let px = window_pixels(true);
        let item = image_item(nitro_core::Transform::translate(7.0, 5.0), 1.0);
        let region = [IRect::new(4, 4, 32, 6), IRect::new(4, 11, 32, 15)];
        let (fast, plain, took) = paint_both(&px, &item, IRect::new(0, 0, 64, 48), &region);
        assert!(took);
        // The lie is at image (20, 15) = device (27, 20).
        let o = (20 * 64 + 27) * 4;
        assert_eq!(
            &fast[o..o + 4],
            &[100, 105, 44, 255],
            "copied, alpha forced opaque"
        );
        assert_ne!(&plain[o..o + 3], &fast[o..o + 3], "the blend skipped it");
        // Everywhere else (the honest pixels) they agree.
        let mut f = fast.clone();
        f[o..o + 4].copy_from_slice(&plain[o..o + 4]);
        assert!(f == plain, "only the lying pixel differs");
    }

    #[test]
    fn scaled_offset_or_translucent_items_fall_back_to_the_blend() {
        let px = window_pixels(false);
        for (what, item) in [
            (
                "scaled (overview thumbnail)",
                image_item(
                    nitro_core::Transform::translate(2.0, 2.0)
                        .then(&nitro_core::Transform::scale(0.5, 0.5)),
                    1.0,
                ),
            ),
            (
                "sub-pixel offset",
                image_item(nitro_core::Transform::translate(7.5, 5.0), 1.0),
            ),
            (
                "opacity < 1",
                image_item(nitro_core::Transform::translate(7.0, 5.0), 0.5),
            ),
        ] {
            let (fast, plain, took) = paint_both(&px, &item, IRect::new(0, 0, 64, 48), &[INNER]);
            assert!(!took, "{what}: must fall back");
            assert!(fast == plain, "{what}");
        }
        // No region, or one entirely outside the clip: no fast path.
        let item = image_item(nitro_core::Transform::translate(7.0, 5.0), 1.0);
        assert!(!paint_both(&px, &item, IRect::new(0, 0, 64, 48), &[]).2);
        assert!(!paint_both(&px, &item, IRect::new(0, 40, 64, 8), &[INNER]).2);
    }
}

/// #3929: the fused scrim and the snap overview's thumbnail covers change
/// no pixel.
#[cfg(test)]
#[allow(clippy::cast_possible_wrap)] // small test dimensions
mod overlay_tests {
    use super::*;
    use nitro_core::{Point, Size, Transform};
    use nitro_scene::{BufferDesc, ClientId, DamageSink, ImageRef, Layer, NodeKind};

    const OUT: OutputId = OutputId(0);
    const C: ClientId = ClientId(1);
    const W: u32 = 160;
    const H: u32 = 100;

    #[derive(Clone, Copy)]
    enum Base {
        None,
        Solid,
        Gradient,
        Image,
    }

    fn pixels(w: u32, h: u32, alpha: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                px[o..o + 4].copy_from_slice(&[
                    (x * 13) as u8,
                    (y * 7 + x) as u8,
                    (x * y) as u8,
                    alpha(x, y),
                ]);
            }
        }
        px
    }

    /// A desktop: `base` as the wallpaper, a scrim at `scrim` opacity, and
    /// two scaled thumbnails — XR24, and AR24 with an opaque region (an
    /// alpha ring around an opaque interior).
    fn world(base: Base, scrim: f32) -> Scene {
        let mut s = Scene::new();
        s.add_output(OUT, IRect::new(0, 0, W as i32, H as i32), 1.0);
        let size = Size::new(W as f32, H as f32);
        let wall = s.create_window(C, "wall", size, Layer::Background);
        s.place_window(wall, Some(OUT), Point::ZERO).unwrap();
        let root = s.window_info(wall).unwrap().root();
        match base {
            Base::None => {}
            Base::Solid | Base::Gradient => {
                let r = s.create_node(C, NodeKind::Rect, root, None).unwrap();
                s.set_bounds(C, r, Rect::new(0.0, 0.0, size.w, size.h))
                    .unwrap();
                let fill = if let Base::Solid = base {
                    SceneFill::Solid(Color::rgb(30, 60, 90))
                } else {
                    SceneFill::Linear {
                        start: Point::new(0.0, 0.0),
                        end: Point::new(0.0, size.h),
                        c0: Color::rgb(10, 200, 30),
                        c1: Color::rgb(250, 3, 99),
                    }
                };
                s.set_fill(C, r, fill).unwrap();
            }
            Base::Image => {
                let desc = BufferDesc::new(W, H, W * 4, format::XR24).with_opaque(true);
                let b = s.create_buffer(C, desc, pixels(W, H, |_, _| 0)).unwrap();
                let i = s.create_node(C, NodeKind::Image, root, None).unwrap();
                s.set_bounds(C, i, Rect::new(0.0, 0.0, size.w, size.h))
                    .unwrap();
                s.set_image(
                    C,
                    i,
                    Some(ImageRef::new(b, IRect::new(0, 0, W as i32, H as i32))),
                )
                .unwrap();
            }
        }
        let ov = s.create_window(C, "scrim", size, Layer::Normal);
        s.place_window(ov, Some(OUT), Point::ZERO).unwrap();
        let root = s.window_info(ov).unwrap().root();
        let r = s.create_node(C, NodeKind::Rect, root, None).unwrap();
        s.set_bounds(C, r, Rect::new(0.0, 0.0, size.w, size.h))
            .unwrap();
        s.set_fill(C, r, SceneFill::Solid(Color::rgba(0, 0, 0, 0xA0)))
            .unwrap();
        s.set_opacity(C, r, scrim).unwrap();
        for (i, (fmt, at, k)) in [
            (format::XR24, (7.3, 9.0), 0.37),
            (format::AR24, (70.0, 20.0), 0.6),
        ]
        .into_iter()
        .enumerate()
        {
            let (bw, bh) = (90u32, 70u32);
            let px = pixels(bw, bh, |x, y| {
                if x.min(y).min(bw - 1 - x).min(bh - 1 - y) < 5 {
                    (x * 40) as u8
                } else {
                    255
                }
            });
            let desc = BufferDesc::new(bw, bh, bw * 4, fmt).with_opaque(fmt == format::XR24);
            let b = s.create_buffer(C, desc, px).unwrap();
            let win = s.create_window(C, format!("t{i}"), size, Layer::Normal);
            s.place_window(win, Some(OUT), Point::ZERO).unwrap();
            let root = s.window_info(win).unwrap().root();
            let g = s.create_node(C, NodeKind::Group, root, None).unwrap();
            s.set_transform(
                C,
                g,
                Transform::translate(at.0, at.1).then(&Transform::scale(k, k)),
            )
            .unwrap();
            let img = s.create_node(C, NodeKind::Image, g, None).unwrap();
            s.set_bounds(C, img, Rect::new(0.0, 0.0, bw as f32, bh as f32))
                .unwrap();
            s.set_image(
                C,
                img,
                Some(ImageRef::new(b, IRect::new(0, 0, bw as i32, bh as i32))),
            )
            .unwrap();
            if fmt == format::AR24 {
                s.set_opaque_region(C, img, &[IRect::new(5, 5, 80, 60)])
                    .unwrap();
            }
        }
        let mut d = Damage::new();
        s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
        s
    }

    fn cursor() -> (Cursor, CursorState) {
        let state = CursorState {
            x: 0,
            y: 0,
            shape: crate::cursor::Shape::Arrow,
            scale: 1,
            visible: false,
        };
        (Cursor::new(), state)
    }

    fn painted(s: &Scene, region: &[IRect], fast: bool) -> Vec<u8> {
        let mut data = vec![0x5Au8; (W * H * 4) as usize];
        let mut canvas = Canvas::new(&mut data, W, H, W * 4);
        let (cur, state) = cursor();
        paint_region(
            &mut canvas,
            s,
            &mut TextEngine::new(),
            &mut IconEngine::new(),
            OUT,
            region,
            (&cur, state),
            &mut Vec::new(),
            &Palette::default(),
            fast,
        );
        data
    }

    /// Every layer painted in full, one after the other: no occlusion, no
    /// fusion, no split.
    fn reference(s: &Scene, region: &[IRect], fast: bool) -> Vec<u8> {
        let mut data = vec![0x5Au8; (W * H * 4) as usize];
        let mut canvas = Canvas::new(&mut data, W, H, W * 4);
        let (mut text, mut icons) = (TextEngine::new(), IconEngine::new());
        for clip in region {
            let mut items = Vec::new();
            s.paint_list(OUT, clip, &mut items);
            paint_background(&mut canvas, clip, W, H, &Palette::default());
            for item in &items {
                if fast && paint_xrgb_scaled(&mut canvas, clip, item, s) {
                    continue;
                }
                paint_item(
                    &mut canvas,
                    clip,
                    item,
                    s,
                    &mut text,
                    &mut icons,
                    &Palette::default(),
                );
            }
        }
        data
    }

    #[test]
    fn fusing_the_scrim_and_skipping_covered_pixels_changes_nothing() {
        let regions: [&[IRect]; 3] = [
            &[IRect::new(0, 0, 160, 100)],
            &[IRect::new(3, 2, 50, 41), IRect::new(60, 30, 99, 70)],
            &[IRect::new(20, 20, 1, 1), IRect::new(0, 99, 160, 1)],
        ];
        for base in [Base::None, Base::Solid, Base::Gradient, Base::Image] {
            for scrim in [1.0, 0.5, 0.0] {
                let s = world(base, scrim);
                for region in regions {
                    for fast in [false, true] {
                        assert!(
                            painted(&s, region, fast) == reference(&s, region, fast),
                            "base {} scrim {scrim} fast {fast} {region:?}",
                            base as u8
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_fast_path_stores_an_opaque_region_and_blends_the_rest() {
        let s = world(Base::Gradient, 1.0);
        let full = [IRect::new(0, 0, 160, 100)];
        let (fast, slow) = (painted(&s, &full, true), painted(&s, &full, false));
        // The AR24 thumbnail: 90x70 at 0.6 from (70, 20): 54x42, whole px.
        let mut items = Vec::new();
        s.paint_list(OUT, &full[0], &mut items);
        let ar24 = items
            .iter()
            .rfind(|i| matches!(i.kind, PaintKind::Image { .. }))
            .unwrap();
        let covers = fast_scaled_covers(&s, ar24);
        assert_eq!(covers.len(), 1, "the region maps to one device rect");
        let inner = covers[0];
        // Well inside the region, and the ring outside it, strictly.
        assert!(inner.x > 70 && inner.y > 21 && inner.right() < 125 && inner.bottom() < 63);
        let mut max = 0;
        for y in 0..H as i32 {
            for x in 0..W as i32 {
                let o = ((y * W as i32 + x) * 4) as usize;
                for c in 0..4 {
                    let d = fast[o + c].abs_diff(slow[o + c]);
                    if inner.contains(x, y) {
                        max = max.max(d);
                    } else if !(ar24.bounds.contains(x, y) || x < 45 && y < 40) {
                        assert_eq!(d, 0, "({x},{y}) outside both thumbnails");
                    }
                }
            }
        }
        assert!(max <= 2, "inside the opaque region: ±{max}");
    }

    #[test]
    fn the_opaque_texels_map_inward() {
        let src = IRect::new(0, 0, 100, 100);
        let dst = IRect::new(10, 10, 50, 50);
        // Whole crop: the edges clamp, no inset.
        assert_eq!(opaque_texels_to_device(&src, &src, &dst), Some(dst));
        // Inset one texel inside, then rounded inward at scale 1/2.
        assert_eq!(
            opaque_texels_to_device(&IRect::new(10, 10, 80, 80), &src, &dst),
            Some(IRect::from_edges(16, 16, 54, 54))
        );
        assert_eq!(
            opaque_texels_to_device(&IRect::new(40, 40, 2, 2), &src, &dst),
            None
        );
    }
}
