//! Outputs, windows and z-order.

use nitro_core::{IRect, Point, Size};

use crate::{NodeKey, key::define_key};

define_key!(
    /// A handle to a window in the scene's window arena.
    WindowKey
);

impl WindowKey {
    /// A key that never resolves; used as the window of a detached node.
    pub(crate) const NONE: Self = Self::from_parts(u32::MAX, 0);
}

/// An output's identity, assigned by the server (one per CRTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputId(pub u32);

/// A client's identity, assigned by the server (one per connection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(pub u32);

impl ClientId {
    /// The server itself: allowed to touch every node.
    pub const SERVER: Self = Self(0);

    /// Whether `self` may mutate something owned by `owner`.
    pub(crate) fn may_touch(self, owner: Self) -> bool {
        self == Self::SERVER || self == owner
    }
}

/// Stacking layers, back to front.
///
/// Windows are ordered by layer first, then by their position within the
/// layer, so a panel on [`Layer::Top`] is never covered by a normal window
/// however it is raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Layer {
    /// Wallpaper and other backdrops.
    Background,
    /// Ordinary application windows.
    Normal,
    /// Panels and docks.
    Top,
    /// Menus, tooltips, drag feedback.
    Overlay,
}

impl Layer {
    /// Every layer, back to front.
    pub const ALL: [Self; 4] = [Self::Background, Self::Normal, Self::Top, Self::Overlay];

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Background => 0,
            Self::Normal => 1,
            Self::Top => 2,
            Self::Overlay => 3,
        }
    }
}

/// A display: a device-pixel rectangle in the global compositor space plus a
/// scale factor applied to every window placed on it.
#[derive(Debug, Clone)]
pub(crate) struct Output {
    pub(crate) id: OutputId,
    pub(crate) rect: IRect,
    pub(crate) scale: f32,
    /// Z-order per layer, back to front.
    pub(crate) layers: [Vec<WindowKey>; 4],
}

impl Output {
    pub(crate) fn new(id: OutputId, rect: IRect, scale: f32) -> Self {
        Self {
            id,
            rect,
            scale,
            layers: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        }
    }

    pub(crate) fn remove(&mut self, win: WindowKey) {
        for layer in &mut self.layers {
            layer.retain(|w| *w != win);
        }
    }

    /// Every window on the output, back to front.
    pub(crate) fn z_order(&self) -> impl Iterator<Item = WindowKey> + '_ {
        self.layers.iter().flat_map(|l| l.iter().copied())
    }
}

/// A top-level window: a root node plus the metadata the shell needs.
#[derive(Debug, Clone)]
pub struct Window {
    pub(crate) root: NodeKey,
    pub(crate) client: ClientId,
    pub(crate) title: String,
    pub(crate) layer: Layer,
    /// Size the client asked for, in logical units.
    pub(crate) size: Size,
    /// Size the server last acknowledged, in logical units.
    pub(crate) configured: Size,
    pub(crate) output: Option<OutputId>,
    /// Top-left corner on the output, in logical units.
    pub(crate) position: Point,
}

impl Window {
    /// The window's root node (a `Group`).
    pub fn root(&self) -> NodeKey {
        self.root
    }

    /// The owning client.
    pub fn client(&self) -> ClientId {
        self.client
    }

    /// The window title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The stacking layer.
    pub fn layer(&self) -> Layer {
        self.layer
    }

    /// The requested size in logical units.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The output the window is placed on, if any. An unplaced window is not
    /// painted and cannot be hit.
    pub fn output(&self) -> Option<OutputId> {
        self.output
    }

    /// The window's top-left corner on its output, in logical units.
    pub fn position(&self) -> Point {
        self.position
    }
}

/// A window whose size changed during an [`update`](crate::Scene::update); the
/// server turns each of these into a `Configure` for the owning client.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Configure {
    /// The window that changed.
    pub window: WindowKey,
    /// Its new size in logical units.
    pub size: Size,
}
