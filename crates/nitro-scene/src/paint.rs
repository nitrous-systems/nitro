//! The paint list: a flat, ordered, pre-clipped description of one frame.
//!
//! The rasterizer consumes [`PaintItem`]s and never touches the scene, so the
//! two can run on different threads and the scene can be mutated again while a
//! frame is being drawn.

use nitro_core::{Color, IRect, Point, Rect, Transform};

use crate::{
    BufferKey, Fill, NodeKey, OutputId, Scene, WindowKey,
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
    /// - a fully opaque, square-cornered, axis-aligned solid rect whose
    ///   device rect lands on exact pixel boundaries;
    /// - an image on a buffer whose format was declared opaque
    ///   ([`BufferDesc::is_opaque`](crate::BufferDesc::is_opaque)), drawn
    ///   axis-aligned, pixel-aligned and 1:1 with its source rect.
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
        if self.opacity < 1.0 {
            return None;
        }
        match self.kind {
            PaintKind::Rect {
                size,
                fill: Fill::Solid(c),
                corner_radius,
                border,
            } => {
                let border_opaque = border.is_none_or(|b| !b.is_visible() || b.color.is_opaque());
                if !(c.is_opaque()
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
    /// extent misses `clip` are skipped without being walked. `out` is
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
        let windows: Vec<WindowKey> = self.output_at(index).z_order().collect();
        for win in windows {
            let Some(window) = self.windows.get(win) else {
                continue;
            };
            let root = window.root;
            let node = self.node_ref(root);
            if !node.subtree_bounds.intersects(clip) {
                continue;
            }
            self.paint_node(root, clip, out);
        }
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
    /// no content never swallows a click.
    ///
    /// Call after [`update`](Scene::update).
    #[must_use]
    pub fn hit_test(&self, output: OutputId, point: Point) -> Option<Hit> {
        let index = self.output_index(output)?;
        let ids: Vec<WindowKey> = self.output_at(index).z_order().collect();
        for win in ids.into_iter().rev() {
            // Defensive: the z-order should only name live windows. Skip a
            // stale entry rather than abandoning the whole hit test, matching
            // what `paint_list` does.
            let Some(window) = self.windows.get(win) else {
                continue;
            };
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
