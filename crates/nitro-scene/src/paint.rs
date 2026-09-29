//! The paint list: a flat, ordered, pre-clipped description of one frame.
//!
//! The rasterizer consumes [`PaintItem`]s and never touches the scene, so the
//! two can run on different threads and the scene can be mutated again while a
//! frame is being drawn.

use nitro_core::{Color, IRect, Point, Rect, Region, Transform};

use crate::{
    BufferKey, Fill, Layer, NodeKey, OutputId, Scene, SurfaceColor, SurfaceData, WindowKey,
    node::{Border, IconRef, NodeData, TextAlign, TextRef},
    update::device_rect,
};

/// What to draw for one node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaintKind {
    /// A (rounded) rectangle of `size` local units at the item's origin.
    Rect {
        /// The rectangle's local size; the transform places it.
        size: (f32, f32),
        /// Interior fill.
        fill: Fill,
        /// Corner radius in local units.
        corner_radius: f32,
        /// Border drawn inside the rectangle.
        border: Option<Border>,
    },
    /// A buffer region stretched onto `size` local units.
    Image {
        /// The destination's local size.
        size: (f32, f32),
        /// The buffer to sample.
        buffer: BufferKey,
        /// Source region in buffer pixels.
        src: IRect,
        /// Whether the buffer's pixels are fully opaque, copied from its
        /// [`BufferDesc::is_opaque`](crate::BufferDesc::is_opaque) so that
        /// [`PaintItem::opaque_cover`] stays a pure function of the item.
        opaque: bool,
    },
    /// A shaped text run, drawn from the caller's text store.
    ///
    /// `origin` is the top-left corner of the *shaped block* in the node's
    /// local space — the alignment inside the node's bounds is already
    /// applied, so the painter only has to walk the run's lines and glyphs
    /// and offset them by this point. The colour is the node's; the item's
    /// `opacity` applies on top of it as for every other kind.
    ///
    /// **The painter must clip the run to the item's `bounds`.** This is the
    /// one kind whose content can exceed the rectangle it was given: `Rect`
    /// and `Image` are defined *by* their bounds, but a run's extent is
    /// whatever the shaper produced, and a long unwrapped label or a
    /// descender below a tight `bounds.h` overflows. Damage is computed from
    /// the bounds alone, so a pixel outside them is a pixel nothing will
    /// ever repaint: it would survive the next `set_text`, the node's
    /// destruction and the window's close, as a ghost.
    Text {
        /// Handle into the text store that owns the shaped run.
        key: u32,
        /// Top-left corner of the block in local units.
        origin: Point,
        /// Tint of every glyph.
        color: Color,
    },
    /// A symbolic icon, drawn from the caller's icon set.
    ///
    /// `origin` is the icon box's top-left corner in the node's local
    /// space (the box is centred in the node's bounds, so a 16 px icon in
    /// a 26 px row lands where a reader expects it). `size` is the box's
    /// side in local units — icons are square by contract.
    ///
    /// The **role**, not a colour: the painter resolves it against the
    /// palette it is holding at paint time, which is what makes a scheme
    /// switch recolour icons in the same frame as text. `role` equal to
    /// [`IconRef::AS_COLOURED`] means "do not tint".
    Icon {
        /// Handle into the painter's icon set.
        icon: u32,
        /// Top-left corner of the square icon box in local units.
        origin: Point,
        /// Box side in local units.
        size: f32,
        /// Palette role index to tint with.
        role: u8,
    },
    /// A hole: a Surface on an underlay hardware plane
    /// ([`SurfaceData::on_plane`](crate::SurfaceData::on_plane)).
    ///
    /// **The painter must set every pixel of the item's `bounds` to fully
    /// transparent (alpha 0), replacing — not blending with — whatever is
    /// below**, so the plane under the framebuffer shows through. Items
    /// after it in the list paint normally over the hole. `bounds` is the
    /// node's device rect rounded *outward* and already narrowed by the
    /// item's `clip`, so it is exactly the rect to clear; there is no
    /// anti-aliased edge.
    ///
    /// The item's `opacity` is ignored: a Surface is treated as opaque.
    /// A translucent Surface must therefore never be flagged on-plane —
    /// the server's job, since it is the only one who sets the flag.
    Hole {
        /// The surface's local size; the transform places it.
        size: (f32, f32),
    },
    /// A Surface composited on the CPU: a buffer region, with its colour
    /// metadata, stretched onto `size` local units.
    ///
    /// The painter places it on whole device pixels (the node's device
    /// rect rounded) and converts the buffer's format (which only it
    /// knows) to the framebuffer's. Opaque formats are *stored*, and the
    /// item's `opacity` is ignored for them in v1 (translucent video is
    /// later work, `docs/surfaces.md`); an alpha format blends.
    Surface {
        /// The destination's local size.
        size: (f32, f32),
        /// The buffer to sample.
        buffer: BufferKey,
        /// Source region in buffer pixels.
        src: IRect,
        /// Colour metadata.
        color: SurfaceColor,
        /// Whether the buffer's format was declared opaque.
        opaque: bool,
    },
}

/// One drawing operation, already ordered, clipped and composed.
///
/// `transform` maps the item's local space (origin at its top-left corner) to
/// device pixels; `clip` is the device-pixel rectangle the rasterizer must not
/// draw outside; `opacity` already includes every ancestor's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaintItem {
    /// The node this came from, for debugging and hit-test cross-checks.
    pub node: NodeKey,
    /// The window the node belongs to.
    pub window: WindowKey,
    /// What to draw.
    pub kind: PaintKind,
    /// Local space to device pixels.
    pub transform: Transform,
    /// Device-pixel clip; never empty.
    pub clip: IRect,
    /// Accumulated opacity in `0.0..=1.0`.
    pub opacity: f32,
    /// Device-pixel bounding box of the item, already clipped.
    pub bounds: IRect,
}

impl PaintItem {
    /// The device-pixel rect this item is guaranteed to cover with fully
    /// opaque pixels — the occlusion hint a rasterizer uses to skip whatever
    /// is behind it.
    ///
    /// Deliberately conservative: it is only ever sound to *under*-report
    /// here, since over-reporting would let a caller skip content that is in
    /// fact visible through a partially covered pixel. Two kinds qualify:
    ///
    /// - a fully opaque, square-cornered, axis-aligned rect whose device
    ///   rect lands on exact pixel boundaries, filled with a solid colour or
    ///   a gradient between two opaque stops;
    /// - an image on a buffer whose format was declared opaque
    ///   ([`BufferDesc::is_opaque`](crate::BufferDesc::is_opaque)), drawn
    ///   axis-aligned, pixel-aligned and 1:1 with its source rect.
    ///
    /// - a [`Hole`](PaintKind::Hole) drawn axis-aligned: the painter clears
    ///   exactly `bounds` (outward-rounded, already clipped), *replacing*
    ///   what was below, so nothing below can show there — even through a
    ///   fractional edge, whose pixels are cleared whole rather than
    ///   blended. Opacity is ignored for it, as for its painting. A rotated
    ///   hole reports `None`: its `bounds` is only a bounding box, and a
    ///   painter is free to clear less.
    ///
    /// Anything else — rounded corners, a translucent fill or border,
    /// accumulated opacity below 1.0, rotation, a fractional edge, a scaled
    /// or alpha-carrying image, text, an icon — reports `None`.
    ///
    /// The 1:1 requirement on an image is what makes the answer checkable by
    /// eye: a scaled blit samples across its source edges, so its coverage of
    /// the destination's boundary pixels is a property of the sampler rather
    /// than of the rect, and the sound answer is to say nothing.
    #[must_use]
    pub fn opaque_cover(&self) -> Option<IRect> {
        if let PaintKind::Hole { .. } = self.kind {
            return self.transform.is_axis_aligned().then_some(self.bounds);
        }
        if self.opacity < 1.0 {
            return None;
        }
        match self.kind {
            PaintKind::Rect {
                size,
                fill,
                corner_radius,
                border,
            } => {
                // A gradient whose two stops are opaque is opaque at every
                // pixel: each channel, alpha included, is interpolated
                // between two 255s (#3929). The wallpaper is one.
                let fill_opaque = match fill {
                    Fill::Solid(c) => c.is_opaque(),
                    Fill::Linear { c0, c1, .. } => c0.is_opaque() && c1.is_opaque(),
                    Fill::None => false,
                };
                let border_opaque = border.is_none_or(|b| !b.is_visible() || b.color.is_opaque());
                if !(fill_opaque
                    && corner_radius <= 0.0
                    && border_opaque
                    && self.transform.is_axis_aligned())
                {
                    return None;
                }
                // `bounds` is rounded *outward*, so its edge pixels may only
                // be partially covered. Report a cover only when the exact
                // device rect is pixel-aligned, in which case the two agree.
                let exact = self
                    .transform
                    .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
                let aligned = exact.x.fract() == 0.0
                    && exact.y.fract() == 0.0
                    && exact.w.fract() == 0.0
                    && exact.h.fract() == 0.0;
                if aligned { Some(self.bounds) } else { None }
            }
            PaintKind::Image {
                size, src, opaque, ..
            }
            | PaintKind::Surface {
                size, src, opaque, ..
            } => {
                if !(opaque && self.transform.is_axis_aligned()) {
                    return None;
                }
                let local = Rect::new(0.0, 0.0, size.0, size.1);
                let exact = self.transform.apply_rect(&local);
                let aligned = exact.x.fract() == 0.0
                    && exact.y.fract() == 0.0
                    && exact.w.fract() == 0.0
                    && exact.h.fract() == 0.0;
                // Pixel-aligned, so the outward-rounded device rect *is* the
                // exact one and the 1:1 test can be made in integers. It is
                // deliberately stricter than the rasterizer's epsilon:
                // under-reporting is the sound direction, and exact equality
                // is the clause a reviewer can check by eye.
                let device = device_rect(&self.transform, local);
                let one_to_one = device.w == src.w && device.h == src.h && src.w > 0 && src.h > 0;
                if aligned && one_to_one {
                    Some(self.bounds)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

impl PaintItem {
    /// Whether drawing this item moved by a whole device pixel is
    /// *guaranteed* to yield the same pixels, moved — the property a
    /// scroll blit relies on for every item it copies.
    ///
    /// Deliberately narrow, like [`opaque_cover`](Self::opaque_cover):
    ///
    /// - a rect with a solid (or no) fill, square corners and an
    ///   axis-aligned device rect on exact pixel boundaries, whose border,
    ///   if visible, is a whole number of device pixels wide — every edge
    ///   the rasterizer computes coverage from is then an integer, and
    ///   stays one when shifted;
    /// - an image drawn 1:1, axis-aligned, onto exact pixel boundaries;
    /// - a [`Hole`](PaintKind::Hole): its pixels are a constant (transparent)
    ///   over an integer rect, so shifting it is exact.
    ///
    /// Everything else says `false`: a gradient's ramp and a resampled
    /// image are evaluated per pixel in floating point; text and icons are
    /// placed from `origin + transform.e`, and `f32` addition is not
    /// shift-invariant for a fractional glyph offset (the sum's ulp grows
    /// with its magnitude), so a glyph could land in a different subpixel
    /// bucket after the move.
    #[must_use]
    pub fn shift_exact(&self) -> bool {
        let aligned = |size: (f32, f32)| {
            let exact = self
                .transform
                .apply_rect(&Rect::new(0.0, 0.0, size.0, size.1));
            self.transform.is_axis_aligned()
                && [exact.x, exact.y, exact.w, exact.h]
                    .iter()
                    .all(|v| v.fract() == 0.0)
        };
        match self.kind {
            PaintKind::Rect {
                size,
                fill,
                corner_radius,
                border,
            } => {
                let scale = self.transform.a.abs().max(self.transform.b.abs());
                let border_ok =
                    border.is_none_or(|b| !b.is_visible() || (b.width * scale).fract() == 0.0);
                !matches!(fill, Fill::Linear { .. })
                    && corner_radius <= 0.0
                    && border_ok
                    && aligned(size)
            }
            PaintKind::Image { size, src, .. } | PaintKind::Surface { size, src, .. } => {
                let device = device_rect(&self.transform, Rect::new(0.0, 0.0, size.0, size.1));
                aligned(size) && device.w == src.w && device.h == src.h
            }
            PaintKind::Hole { .. } => true,
            PaintKind::Text { .. } | PaintKind::Icon { .. } => false,
        }
    }
}

/// Where a text node's shaped block starts inside bounds `width` units wide.
///
/// Vertical placement is deliberately *not* aligned: a text node's box is its
/// line box, and a client that wants the block centred vertically sets the
/// bounds it wants. Horizontal alignment is the one the toolkit actually needs
/// per label, and computing it here keeps the rasterizer free of any notion of
/// alignment at all.
fn text_origin(text: TextRef, width: f32) -> Point {
    let slack = width - text.size.w;
    let x = match text.align {
        TextAlign::Left => 0.0,
        TextAlign::Center => slack / 2.0,
        TextAlign::Right => slack,
    };
    Point::new(x, 0.0)
}

/// Where an icon's square box sits inside bounds `width` × `height`.
///
/// Centred on both axes, unlike a text block, and the asymmetry is not an
/// oversight: a text node's box *is* its line box, so its vertical
/// placement is the client's to decide, whereas an icon is a square glyph
/// a client puts in whatever row it has. Centring is what makes
/// `row(icon("gear"), label("Display"))` line up without the app doing
/// arithmetic. Rounded to whole local units so a 16 px icon in a 26 px
/// row does not land on a half-pixel and blur.
fn icon_origin(icon: IconRef, width: f32, height: f32) -> Point {
    let size = icon.size();
    Point::new(
        ((width - size) / 2.0).max(0.0).round(),
        ((height - size) / 2.0).max(0.0).round(),
    )
}

impl Scene {
    /// Append the items needed to redraw `clip` on `output`, in painter's
    /// order: layers back to front, windows back to front within a layer,
    /// children in order (later children on top).
    ///
    /// Invisible nodes, fully transparent nodes and subtrees whose cached
    /// extent misses `clip` are skipped without being walked. So are the
    /// windows the [`Admit`](crate::Admit) filter leaves out, and the
    /// [`Layer::Top`] windows of an output whose top layer is hidden
    /// ([`set_top_layer_hidden`](Scene::set_top_layer_hidden)), and
    /// offscreen windows ([`set_offscreen`](Scene::set_offscreen)). `out` is
    /// appended to, never cleared.
    ///
    /// Call after [`update`](Scene::update): the traversal reads the cached
    /// world state.
    pub fn paint_list(&self, output: OutputId, clip: &IRect, out: &mut Vec<PaintItem>) {
        if clip.is_empty() {
            return;
        }
        let Some(index) = self.output_index(output) else {
            return;
        };
        let top_hidden = self.output_at(index).top_hidden;
        let windows: Vec<WindowKey> = self.output_at(index).z_order().collect();
        for win in windows {
            let Some(window) = self.windows.get(win) else {
                continue;
            };
            if !self.admit.admits(window.client)
                || window.offscreen
                || (top_hidden && window.layer == Layer::Top)
            {
                continue;
            }
            let root = window.root;
            let node = self.node_ref(root);
            if !node.subtree_bounds.intersects(clip) {
                continue;
            }
            self.paint_node(root, clip, out);
        }
    }

    /// Append the items of one window's tree inside `clip` (global device
    /// pixels), in painter's order, with exactly
    /// [`paint_list`](Scene::paint_list)'s item semantics — but for that
    /// window alone, and whether or not it is
    /// [offscreen](Scene::set_offscreen), admitted or on a hidden layer.
    /// This is how an offscreen window is rendered somewhere else.
    ///
    /// A dead or unplaced window appends nothing. Call after
    /// [`update`](Scene::update).
    pub fn paint_window(&self, win: WindowKey, clip: &IRect, out: &mut Vec<PaintItem>) {
        if clip.is_empty() {
            return;
        }
        let Some(window) = self.windows.get(win) else {
            return;
        };
        if window.output.is_none() {
            return;
        }
        self.paint_node(window.root, clip, out);
    }

    /// What a consumer of a [`Translation`](crate::Translation) hint needs
    /// to know about the paint list inside `clip`: `(S, A)`, both exact
    /// and in global device pixels.
    ///
    /// * `S` — the union of [`PaintItem::opaque_cover`] over the
    ///   shift-exact ([`PaintItem::shift_exact`]) items `node`'s subtree
    ///   paints, within `clip` — `node`'s own item only if `moves_node`. Where a
    ///   subtree item is opaque, whatever is *below* the subtree cannot
    ///   show, so those pixels are a function of the subtree alone.
    /// * `A` — the union of the `bounds` of every item painted *after* the
    ///   subtree (later siblings, windows above, overlays), within `clip`.
    ///   Where one of those is, the subtree is not all there is.
    ///
    /// `paint_node` is a pre-order walk, so the subtree's items are one
    /// contiguous run of the list; anything before it is below.
    ///
    /// `None` when the node is dead, when the regions would exceed
    /// [`Region::MAX_SPANS`], or when the subtree paints nothing here.
    /// Call after [`update`](Scene::update).
    #[must_use]
    pub fn translation_cover(
        &self,
        output: OutputId,
        node: NodeKey,
        moves_node: bool,
        clip: &IRect,
    ) -> Option<(Region, Region)> {
        self.nodes.get(node)?;
        let mut items = Vec::new();
        self.paint_list(output, clip, &mut items);
        let inside = |item: &PaintItem| item.node == node || self.is_ancestor(node, item.node);
        let first = items.iter().position(inside)?;
        let len = items[first..].iter().take_while(|i| inside(i)).count();
        let (run, after) = items[first..].split_at(len);
        // Contiguity is a property of the walk; were it ever violated the
        // answer would be unsound, so check rather than assume.
        if after.iter().any(inside) {
            return None;
        }
        // With `moves_node == false` the node's own content stayed put
        // while its descendants moved, so it is no part of what moved.
        let covers: Vec<IRect> = run
            .iter()
            .filter(|i| moves_node || i.node != node)
            .filter_map(PaintItem::opaque_cover)
            .map(|r| r.intersect(clip))
            .collect();
        // Items whose pixels are computed per pixel in floating point from
        // device coordinates (a gradient's ramp, a resampled image) are
        // not guaranteed to come out bit-identical when shifted by a whole
        // pixel, so their footprint is never copied.
        let inexact: Vec<IRect> = run
            .iter()
            .filter(|i| !i.shift_exact())
            .map(|i| i.bounds.intersect(clip))
            .collect();
        let above: Vec<IRect> = after.iter().map(|i| i.bounds.intersect(clip)).collect();
        let s = Region::from_rects(&covers).subtract(&Region::from_rects(&inexact));
        let a = Region::from_rects(&above);
        (!s.overflowed() && !a.overflowed()).then_some((s, a))
    }

    fn paint_node(&self, key: NodeKey, clip: &IRect, out: &mut Vec<PaintItem>) {
        let node = self.node_ref(key);
        if !node.world_visible || node.world_opacity <= 0.0 {
            return;
        }
        if !node.subtree_bounds.intersects(clip) {
            return;
        }
        if node.painted {
            let bounds = node.world_bounds.intersect(clip);
            if !bounds.is_empty() {
                let size = (node.bounds.w, node.bounds.h);
                let kind = match node.data {
                    NodeData::Rect(r) => Some(PaintKind::Rect {
                        size,
                        fill: r.fill,
                        corner_radius: r.corner_radius,
                        border: r.border,
                    }),
                    NodeData::Image(Some(image)) => Some(PaintKind::Image {
                        size,
                        buffer: image.buffer,
                        src: image.src,
                        // A stale key cannot happen (the node would have been
                        // emptied), but if it did, `false` is the answer that
                        // costs nothing but an occlusion.
                        opaque: self
                            .buffers
                            .get(image.buffer)
                            .is_some_and(|b| b.desc.is_opaque()),
                    }),
                    NodeData::Text(Some(text)) => Some(PaintKind::Text {
                        key: text.key,
                        origin: text_origin(text, node.bounds.w),
                        color: text.color,
                    }),
                    NodeData::Icon(Some(icon)) => Some(PaintKind::Icon {
                        icon: icon.icon,
                        origin: icon_origin(icon, node.bounds.w, node.bounds.h),
                        size: icon.size(),
                        role: icon.role,
                    }),
                    NodeData::Surface(surface) if surface.on_plane => {
                        Some(PaintKind::Hole { size })
                    }
                    NodeData::Surface(SurfaceData {
                        content: Some(surface),
                        ..
                    }) => Some(PaintKind::Surface {
                        size,
                        buffer: surface.buffer,
                        src: surface.src,
                        color: surface.color,
                        opaque: self
                            .buffers
                            .get(surface.buffer)
                            .is_some_and(|b| b.desc.is_opaque()),
                    }),
                    _ => None,
                };
                if let Some(kind) = kind {
                    out.push(PaintItem {
                        node: key,
                        window: node.window,
                        kind,
                        transform: node.world_transform,
                        clip: node.clip_rect.intersect(clip),
                        opacity: node.world_opacity,
                        bounds,
                    });
                }
            }
        }
        for child in &node.children {
            self.paint_node(*child, clip, out);
        }
    }
}

/// Where a device-space point landed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// The window hit.
    pub window: WindowKey,
    /// The deepest node hit.
    pub node: NodeKey,
    /// The point in that node's local coordinates.
    pub local: Point,
}

impl Scene {
    /// Find the topmost node under a device-pixel point on `output`.
    ///
    /// Windows are tried front to back and, within a window, children front to
    /// back (later children are on top). Invisible and fully transparent
    /// subtrees are skipped, clip groups reject points outside their clip, and
    /// a node only counts as hit if it actually paints something. A group with
    /// no content never swallows a click. Windows that
    /// [`paint_list`](Scene::paint_list) leaves out (not admitted, offscreen,
    /// or on a hidden top layer) and hit-exempt windows are never hit.
    ///
    /// Call after [`update`](Scene::update).
    #[must_use]
    pub fn hit_test(&self, output: OutputId, point: Point) -> Option<Hit> {
        let index = self.output_index(output)?;
        let top_hidden = self.output_at(index).top_hidden;
        let ids: Vec<WindowKey> = self.output_at(index).z_order().collect();
        for win in ids.into_iter().rev() {
            // Defensive: the z-order should only name live windows. Skip a
            // stale entry rather than abandoning the whole hit test, matching
            // what `paint_list` does.
            let Some(window) = self.windows.get(win) else {
                continue;
            };
            if !self.admit.admits(window.client)
                || window.hit_exempt
                || window.offscreen
                || (top_hidden && window.layer == Layer::Top)
            {
                continue;
            }
            let root = window.root;
            if let Some(hit) = self.hit_node(root, point) {
                return Some(hit);
            }
        }
        None
    }

    fn hit_node(&self, key: NodeKey, point: Point) -> Option<Hit> {
        let node = self.node_ref(key);
        if !node.world_visible || node.world_opacity <= 0.0 {
            return None;
        }
        let px = point.x.floor() as i32;
        let py = point.y.floor() as i32;
        if !node.subtree_bounds.contains(px, py) {
            return None;
        }
        // Deepest first: later children are on top.
        for child in node.children.iter().rev() {
            if let Some(hit) = self.hit_node(*child, point) {
                return Some(hit);
            }
        }
        if node.painted && node.world_bounds.contains(px, py) {
            let inverse = node.world_transform.invert()?;
            let local = inverse.apply(point);
            let rect = node.local_rect();
            if rect.contains(local) {
                return Some(Hit {
                    window: node.window,
                    node: key,
                    local,
                });
            }
        }
        None
    }

    /// The device-pixel rectangle a node currently covers, recomputed from its
    /// cached world transform. Exposed for tests and tracing; the cached
    /// [`Node::world_bounds`](crate::Node::world_bounds) is the cheap answer.
    #[must_use]
    pub fn device_bounds(&self, key: NodeKey) -> IRect {
        match self.nodes.get(key) {
            Some(node) => device_rect(&node.world_transform, node.local_rect()),
            None => IRect::EMPTY,
        }
    }
}
