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

/// Which clients' windows the scene paints and hit-tests.
///
/// The one lever a session lock needs from the scene: while it is locked,
/// only the lock screen's client exists as far as pixels and the pointer
/// are concerned, and before a lock screen has connected, nobody does.
/// Filtering here, in the two walks that read the z-order, rather than
/// toggling each window's root visibility, leaves every window's own
/// state (minimized, hidden by its client) untouched, so unlocking is
/// the same one call back to [`Admit::All`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Admit {
    /// Every window: the ordinary state.
    #[default]
    All,
    /// Only the windows `client` owns.
    Only(ClientId),
    /// No window at all.
    Nobody,
}

impl Admit {
    /// Whether a window owned by `owner` is painted and hit-tested.
    #[must_use]
    pub fn admits(self, owner: ClientId) -> bool {
        match self {
            Self::All => true,
            Self::Only(c) => c == owner,
            Self::Nobody => false,
        }
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

/// What a top-level window is doing: its geometry mode.
///
/// The scene only stores it and hides a [`Minimized`](WindowState::Minimized)
/// window; deciding *which* rectangle a state implies is policy and lives in
/// the server's window manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WindowState {
    /// Floating at its own size and position.
    #[default]
    Normal,
    /// Filling its output's work area.
    Maximized,
    /// Covering its whole output, decorations hidden.
    Fullscreen,
    /// Hidden, but still a window: it keeps its geometry, its place in the
    /// z-order and its place in the focus-cycling order.
    Minimized,
}

/// Space a window's frame takes on each side of its content, in logical
/// units. `NONE` for an undecorated window.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    /// Left edge.
    pub left: f32,
    /// Top edge (the title bar).
    pub top: f32,
    /// Right edge.
    pub right: f32,
    /// Bottom edge.
    pub bottom: f32,
}

impl Insets {
    /// No frame at all.
    pub const NONE: Self = Self {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };

    /// Construct from the four edges.
    #[must_use]
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Total horizontal space, `left + right`.
    #[must_use]
    pub fn width(self) -> f32 {
        self.left + self.right
    }

    /// Total vertical space, `top + bottom`.
    #[must_use]
    pub fn height(self) -> f32 {
        self.top + self.bottom
    }

    /// Whether there is no frame.
    #[must_use]
    pub fn is_none(self) -> bool {
        self == Self::NONE
    }
}

/// What a window asked for at creation, as policy rather than protocol: the
/// scene stores the bits, the server acts on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFlags {
    /// The server draws a frame around it.
    pub decorated: bool,
    /// The user may not resize it.
    pub fixed_size: bool,
    /// It may take keyboard focus.
    pub focusable: bool,
}

impl Default for WindowFlags {
    fn default() -> Self {
        Self {
            decorated: true,
            fixed_size: false,
            focusable: true,
        }
    }
}

/// A top-level window: a root node plus the metadata the shell needs.
///
/// The **root** is the whole window including whatever frame the server drew
/// around it; the **content** is the group the owning client attached its
/// nodes to. They are the same node for an undecorated window, and the
/// content is a child of the root — offset by [`Window::inset`] — for a
/// decorated one. Every size and position a client is told is the content's.
#[derive(Debug, Clone)]
pub struct Window {
    pub(crate) root: NodeKey,
    /// The client's group: `root` itself when undecorated.
    pub(crate) content: NodeKey,
    pub(crate) client: ClientId,
    pub(crate) title: String,
    pub(crate) app_id: String,
    pub(crate) layer: Layer,
    /// Size the client asked for, in logical units. The *content* size.
    pub(crate) size: Size,
    /// Size the server last acknowledged, in logical units.
    pub(crate) configured: Size,
    pub(crate) output: Option<OutputId>,
    /// Top-left corner of the **frame** on the output, in logical units.
    pub(crate) position: Point,
    /// Frame thickness on each side; `NONE` when undecorated.
    pub(crate) inset: Insets,
    pub(crate) state: WindowState,
    pub(crate) flags: WindowFlags,
    /// Smallest content size the server will resize to; a zero component
    /// means "no limit".
    pub(crate) min: Size,
    /// Largest content size; a zero component means "no limit".
    pub(crate) max: Size,
    /// Frame position and content size to go back to when leaving
    /// `Maximized`/`Fullscreen`.
    pub(crate) restore: Option<(Point, Size)>,
}

impl Window {
    /// The window's root node (a `Group`): the frame, decorations included.
    pub fn root(&self) -> NodeKey {
        self.root
    }

    /// The group the client owns. Equal to [`root`](Window::root) unless the
    /// server framed the window.
    pub fn content(&self) -> NodeKey {
        self.content
    }

    /// Whether the server drew a frame around this window.
    pub fn is_framed(&self) -> bool {
        self.content != self.root
    }

    /// The frame's thickness on each side.
    pub fn inset(&self) -> Insets {
        self.inset
    }

    /// The window's state.
    pub fn state(&self) -> WindowState {
        self.state
    }

    /// The creation flags.
    pub fn flags(&self) -> WindowFlags {
        self.flags
    }

    /// The application id, or an empty string if the client never set one.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Minimum content size; a zero component means "no limit".
    pub fn min_size(&self) -> Size {
        self.min
    }

    /// Maximum content size; a zero component means "no limit".
    pub fn max_size(&self) -> Size {
        self.max
    }

    /// Frame position and content size remembered for the way back out of
    /// `Maximized`/`Fullscreen`.
    pub fn restore(&self) -> Option<(Point, Size)> {
        self.restore
    }

    /// The whole window's size, frame included, in logical units.
    pub fn frame_size(&self) -> Size {
        Size::new(
            self.size.w + self.inset.width(),
            self.size.h + self.inset.height(),
        )
    }

    /// The whole window's rectangle on its output, frame included, in
    /// logical units.
    pub fn frame_rect(&self) -> nitro_core::Rect {
        let size = self.frame_size();
        nitro_core::Rect::new(self.position.x, self.position.y, size.w, size.h)
    }

    /// The content's top-left corner on the output, in logical units. This
    /// is what a `Configure` reports.
    pub fn content_position(&self) -> Point {
        Point::new(
            self.position.x + self.inset.left,
            self.position.y + self.inset.top,
        )
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

    /// The **frame's** top-left corner on its output, in logical units. For
    /// the corner a client is told about, see
    /// [`content_position`](Window::content_position).
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
