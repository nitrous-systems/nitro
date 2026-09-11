//! Nodes: kinds, properties, cached world state and dirty flags.

use nitro_core::{Color, IRect, Point, Rect, Transform};

use crate::{BufferKey, ClientId, WindowKey, key::define_key};

define_key!(
    /// A handle to a node in the scene's arena.
    NodeKey
);

/// What a node is.
///
/// `Text` and `Surface` are reserved: the scene stores the kind and the common
/// properties, produces no paint item and no damage for them, and grows a
/// payload when the rasterizer (text runs) and the Wayland adapter (external
/// buffers) need one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A container: transform, clip and opacity for its children.
    Group,
    /// A (rounded) rectangle with a fill and an optional border.
    Rect,
    /// A rectangle textured from a region of a buffer.
    Image,
    /// Reserved: a shaped text run.
    Text,
    /// Reserved: an externally-provided surface (dma-buf).
    Surface,
}

/// How a [`NodeKind::Rect`] is filled.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Fill {
    /// Nothing is painted inside the rect (a border may still be).
    #[default]
    None,
    /// A single colour.
    Solid(Color),
    /// A linear gradient from `start` to `end`, in the node's local
    /// coordinates (origin at the node's top-left corner).
    Linear {
        /// Where `c0` sits.
        start: Point,
        /// Where `c1` sits.
        end: Point,
        /// Colour at `start`.
        c0: Color,
        /// Colour at `end`.
        c1: Color,
    },
}

impl Fill {
    /// Whether this fill can put any pixel on screen.
    pub fn is_visible(self) -> bool {
        match self {
            Self::None => false,
            Self::Solid(c) => !c.is_transparent(),
            Self::Linear { c0, c1, .. } => !c0.is_transparent() || !c1.is_transparent(),
        }
    }
}

/// A border drawn inside the node's bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Border {
    /// Width in local units; `<= 0` means no border.
    pub width: f32,
    /// Border colour.
    pub color: Color,
}

impl Border {
    /// Construct a border.
    pub const fn new(width: f32, color: Color) -> Self {
        Self { width, color }
    }

    /// Whether this border can put any pixel on screen.
    pub fn is_visible(self) -> bool {
        self.width > 0.0 && !self.color.is_transparent()
    }
}

/// The region of a buffer an [`NodeKind::Image`] node samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageRef {
    /// The buffer sampled.
    pub buffer: BufferKey,
    /// Source rectangle in buffer pixels, stretched onto the node's bounds.
    pub src: IRect,
}

impl ImageRef {
    /// Construct an image reference.
    pub const fn new(buffer: BufferKey, src: IRect) -> Self {
        Self { buffer, src }
    }
}

/// Paint properties of a [`NodeKind::Rect`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RectData {
    /// Interior fill.
    pub fill: Fill,
    /// Corner radius in local units; the rasterizer clamps it to half the
    /// smaller side.
    pub corner_radius: f32,
    /// Optional border drawn inside the bounds.
    pub border: Option<Border>,
}

/// Kind-specific payload.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) enum NodeData {
    #[default]
    Group,
    Rect(RectData),
    Image(Option<ImageRef>),
    Text,
    Surface,
}

impl NodeData {
    pub(crate) fn kind(self) -> NodeKind {
        match self {
            Self::Group => NodeKind::Group,
            Self::Rect(_) => NodeKind::Rect,
            Self::Image(_) => NodeKind::Image,
            Self::Text => NodeKind::Text,
            Self::Surface => NodeKind::Surface,
        }
    }

    pub(crate) fn for_kind(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Group => Self::Group,
            NodeKind::Rect => Self::Rect(RectData::default()),
            NodeKind::Image => Self::Image(None),
            NodeKind::Text => Self::Text,
            NodeKind::Surface => Self::Surface,
        }
    }
}

/// What changed about a node since the last [`Scene::update`](crate::Scene::update).
///
/// The flags drive the update walk: an untouched node is never visited, which
/// is what makes work proportional to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty(u8);

impl Dirty {
    /// Nothing changed.
    pub const NONE: Self = Self(0);
    /// The node's group transform changed; descendants must be recomputed.
    pub const TRANSFORM: Self = Self(1);
    /// The node's local bounds changed; its geometry and every descendant's
    /// world transform must be recomputed.
    pub const BOUNDS: Self = Self(2);
    /// The node's own appearance changed (fill, radius, border, image).
    pub const PAINT: Self = Self(4);
    /// Something descendants inherit changed (opacity, visibility, clip).
    pub const INHERIT: Self = Self(8);
    /// The child list changed, so the subtree's extent must be recomputed.
    pub const STRUCTURE: Self = Self(16);
    /// Some descendant carries a flag; this node is only a waypoint on the
    /// path to it. Set by the scene, never by a mutation directly.
    pub const SUBTREE: Self = Self(32);

    /// Whether any of `other`'s flags are set.
    pub const fn any(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether no flag at all is set.
    pub const fn is_clean(self) -> bool {
        self.0 == 0
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// Flags whose effect reaches every descendant.
pub(crate) const DESCEND: Dirty = Dirty::TRANSFORM.union(Dirty::BOUNDS).union(Dirty::INHERIT);
/// Everything a brand-new node needs: nothing about it is accounted for yet.
pub(crate) const ALL_DIRTY: Dirty = DESCEND.union(Dirty::PAINT).union(Dirty::STRUCTURE);

/// A node in the retained tree.
///
/// The local properties are what clients set; the `world_*` values are caches
/// recomputed by [`Scene::update`](crate::Scene::update) and are only
/// meaningful once it has run.
// The bools are independent facts about one node (set by the client, derived
// by the update pass, owned by the bookkeeping); folding them into a state
// machine would only obscure that.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub struct Node {
    pub(crate) data: NodeData,
    pub(crate) client: ClientId,
    pub(crate) window: WindowKey,
    pub(crate) parent: Option<NodeKey>,
    pub(crate) children: Vec<NodeKey>,
    pub(crate) depth: u32,

    // Local properties, in the parent's coordinate space.
    pub(crate) bounds: Rect,
    pub(crate) transform: Transform,
    pub(crate) opacity: f32,
    pub(crate) clip: bool,
    pub(crate) visible: bool,

    // Caches; valid after `update`.
    pub(crate) world_transform: Transform,
    pub(crate) world_bounds: IRect,
    pub(crate) subtree_bounds: IRect,
    /// The clip this node's own content is subject to (its parent's
    /// `child_clip`).
    pub(crate) clip_rect: IRect,
    /// Clip handed to this node's children: `clip_rect`, narrowed by the
    /// node's own device rect when `clip` is set.
    pub(crate) child_clip: IRect,
    pub(crate) world_opacity: f32,
    pub(crate) world_visible: bool,
    pub(crate) painted: bool,

    pub(crate) dirty: Dirty,
    /// Whether the node is already in the scene's dirty list.
    pub(crate) queued: bool,
}

impl Node {
    pub(crate) fn new(kind: NodeKind, client: ClientId, window: WindowKey, depth: u32) -> Self {
        Self {
            data: NodeData::for_kind(kind),
            client,
            window,
            parent: None,
            children: Vec::new(),
            depth,
            bounds: Rect::EMPTY,
            transform: Transform::IDENTITY,
            opacity: 1.0,
            clip: false,
            visible: true,
            world_transform: Transform::IDENTITY,
            world_bounds: IRect::EMPTY,
            subtree_bounds: IRect::EMPTY,
            clip_rect: IRect::EMPTY,
            child_clip: IRect::EMPTY,
            world_opacity: 1.0,
            world_visible: true,
            painted: false,
            dirty: ALL_DIRTY,
            queued: false,
        }
    }

    /// The node's kind.
    pub fn kind(&self) -> NodeKind {
        self.data.kind()
    }

    /// The client that owns the node.
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// The window this node belongs to.
    pub fn window(&self) -> WindowKey {
        self.window
    }

    /// The parent, or `None` for a window's root node.
    pub fn parent(&self) -> Option<NodeKey> {
        self.parent
    }

    /// Children, back to front.
    pub fn children(&self) -> &[NodeKey] {
        &self.children
    }

    /// Local bounds, in the parent's coordinate space.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The group transform applied to children (identity for other kinds).
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// Opacity in `0.0..=1.0`, multiplied down the tree.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Whether this group clips its children to its bounds.
    pub fn clip(&self) -> bool {
        self.clip
    }

    /// Whether the node and its subtree are drawn at all.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Rect fill; [`Fill::None`] for other kinds.
    pub fn fill(&self) -> Fill {
        match self.data {
            NodeData::Rect(r) => r.fill,
            _ => Fill::None,
        }
    }

    /// Rect corner radius; `0.0` for other kinds.
    pub fn corner_radius(&self) -> f32 {
        match self.data {
            NodeData::Rect(r) => r.corner_radius,
            _ => 0.0,
        }
    }

    /// Rect border, if any.
    pub fn border(&self) -> Option<Border> {
        match self.data {
            NodeData::Rect(r) => r.border,
            _ => None,
        }
    }

    /// Image reference; `None` unless this is an `Image` node with a buffer.
    pub fn image(&self) -> Option<ImageRef> {
        match self.data {
            NodeData::Image(i) => i,
            _ => None,
        }
    }

    /// Transform from the node's local space (origin at its top-left corner)
    /// to device pixels. Cached; valid after `update`.
    pub fn world_transform(&self) -> Transform {
        self.world_transform
    }

    /// Device-pixel bounding box of this node's own content, already narrowed
    /// by the inherited clip; empty when the node paints nothing. Cached;
    /// valid after `update`.
    pub fn world_bounds(&self) -> IRect {
        self.world_bounds
    }

    /// Device-pixel bounding box of this node and its descendants. Cached;
    /// valid after `update`.
    pub fn subtree_bounds(&self) -> IRect {
        self.subtree_bounds
    }

    /// Accumulated opacity (this node's, times every ancestor's). Cached.
    pub fn world_opacity(&self) -> f32 {
        self.world_opacity
    }

    /// Whether the node is visible with every ancestor taken into account.
    /// Cached.
    pub fn world_visible(&self) -> bool {
        self.world_visible
    }

    /// The device-pixel clip rectangle this node's content is subject to.
    /// Cached.
    pub fn clip_rect(&self) -> IRect {
        self.clip_rect
    }

    /// Whether the node put pixels on screen at the last `update`.
    pub fn painted(&self) -> bool {
        self.painted
    }

    /// Pending change flags.
    pub fn dirty(&self) -> Dirty {
        self.dirty
    }

    /// The node's content rectangle in its own local space: its size at the
    /// origin.
    pub(crate) fn local_rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.bounds.w, self.bounds.h)
    }

    /// Whether the node itself can put pixels on screen, ignoring inherited
    /// visibility and opacity.
    pub(crate) fn has_content(&self) -> bool {
        if self.bounds.is_empty() {
            return false;
        }
        match self.data {
            NodeData::Rect(r) => r.fill.is_visible() || r.border.is_some_and(Border::is_visible),
            NodeData::Image(i) => i.is_some(),
            // Reserved kinds store nothing, so they paint nothing.
            NodeData::Group | NodeData::Text | NodeData::Surface => false,
        }
    }
}
