//! The paint list: a flat, ordered, pre-clipped description of one frame.
//!
//! The rasterizer consumes [`PaintItem`]s and never touches the scene, so the
//! two can run on different threads and the scene can be mutated again while a
//! frame is being drawn.

use nitro_core::{IRect, Point, Transform};

use crate::{
    BufferKey, Fill, NodeKey, OutputId, Scene, WindowKey,
    node::{Border, NodeData},
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
    /// Whether the item is an opaque solid rect covering `r` exactly — the
    /// occlusion hint a rasterizer uses to skip what is behind it.
    #[must_use]
    pub fn opaque_cover(&self) -> Option<IRect> {
        if self.opacity < 1.0 {
            return None;
        }
        match self.kind {
            PaintKind::Rect {
                fill: Fill::Solid(c),
                corner_radius,
                border,
                ..
            } => {
                let border_opaque = border.is_none_or(|b| !b.is_visible() || b.color.is_opaque());
                if c.is_opaque()
                    && corner_radius <= 0.0
                    && border_opaque
                    && self.transform.is_axis_aligned()
                {
                    Some(self.bounds)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
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
            let window = self.windows.get(win)?;
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
