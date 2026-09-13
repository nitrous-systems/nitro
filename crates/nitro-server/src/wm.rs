//! Window management: the policy half of M3.
//!
//! The scene knows *where* windows are; this module decides where they
//! should be. It is deliberately free of I/O and of the event loop — every
//! function here is either a pure geometry decision or a mutation of the
//! [`WindowManager`]'s own bookkeeping, so the interesting rules (hit
//! regions, placement, MRU order, the state machine) are unit-testable
//! without a server.
//!
//! # The model
//!
//! * **Decorations are server-side and opt-out.** A decorated window's
//!   client group is wrapped in a *frame group* the server owns
//!   ([`nitro_scene::Scene::frame_window`]), carrying a title bar, a border
//!   and two buttons. A client that sets `UNDECORATED` gets no frame and
//!   still gets server move/resize through the `Super` modifier.
//! * **The server moves and resizes windows with zero client round-trips.**
//!   A move is one transform update per motion event and no protocol
//!   traffic at all; a resize is one `Configure` per motion, which the
//!   frame scheduler already throttles to one per frame.
//! * **Focus follows the raise**, the raise follows the click, and the MRU
//!   list is what `Alt+Tab` walks.
//!
//! What is *not* here: workspaces, cursor shapes and a persistent
//! multi-output layout. See `docs/wm.md`.

use nitro_core::{Color, Point, Rect, Size};
use nitro_scene::{
    ClientId, Insets, Layer, OutputId, Scene, WindowKey,
};

/// Title-bar height in logical pixels.
pub const TITLE_H: f32 = 28.0;
/// Border width in logical pixels, drawn on the left, right and bottom.
pub const BORDER: f32 = 1.0;
/// Corner radius of the frame's top corners, logical pixels.
pub const CORNER_RADIUS: f32 = 6.0;
/// How wide the resize grab band is, inside and outside the frame edge.
pub const RESIZE_BAND: f32 = 6.0;
/// Size of one title-bar button (a square), logical pixels.
pub const BUTTON: f32 = 14.0;
/// Gap between the buttons and from the right edge.
pub const BUTTON_GAP: f32 = 8.0;
/// Smallest content size a drag may resize a window to.
pub const MIN_CONTENT: Size = Size::new(64.0, 32.0);
/// Two clicks closer together than this on the title bar are a double
/// click.
pub const DOUBLE_CLICK_NS: u64 = 400_000_000;

/// The frame's insets for a decorated window.
#[must_use]
pub fn frame_insets() -> Insets {
    Insets::new(BORDER, TITLE_H, BORDER, BORDER)
}

/// Colours of the frame, focused and unfocused.
pub mod theme {
    use nitro_core::Color;

    /// Title bar of the focused window.
    pub const BAR_ACTIVE: Color = Color::rgb(0x2C, 0x3E, 0x55);
    /// Title bar of an unfocused window.
    pub const BAR_INACTIVE: Color = Color::rgb(0x23, 0x2A, 0x33);
    /// Border of the focused window.
    pub const BORDER_ACTIVE: Color = Color::rgb(0x5A, 0x8D, 0xC8);
    /// Border of an unfocused window.
    pub const BORDER_INACTIVE: Color = Color::rgb(0x3A, 0x42, 0x4C);
    /// Title text of the focused window.
    pub const TITLE_ACTIVE: Color = Color::rgb(0xF0, 0xF4, 0xF8);
    /// Title text of an unfocused window.
    pub const TITLE_INACTIVE: Color = Color::rgb(0x9A, 0xA4, 0xB0);
    /// The close button.
    pub const CLOSE: Color = Color::rgb(0xD9, 0x5B, 0x4E);
    /// The maximize button.
    pub const MAXIMIZE: Color = Color::rgb(0x62, 0xA8, 0x5C);
    /// Title font size in logical pixels.
    pub const TITLE_SIZE_PX: f32 = 13.0;
}

/// Which edge(s) a resize drag is pulling.
// Four independent facts about one drag; the alternative — an enum of the
// eight legal combinations — is a bigger table that says less.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Edges {
    /// Pulling the left edge.
    pub left: bool,
    /// Pulling the right edge.
    pub right: bool,
    /// Pulling the top edge.
    pub top: bool,
    /// Pulling the bottom edge.
    pub bottom: bool,
}

impl Edges {
    /// No edge.
    pub const NONE: Self = Self {
        left: false,
        right: false,
        top: false,
        bottom: false,
    };

    /// Whether any edge is being pulled.
    #[must_use]
    pub fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }

    /// The bottom-right corner, the default for a `Super`+right drag that
    /// starts in the middle of a window.
    #[must_use]
    pub fn corner(right: bool, bottom: bool) -> Self {
        Self {
            left: !right,
            right,
            top: !bottom,
            bottom,
        }
    }
}

/// What the pointer is over, in a window's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    /// The client's content: the event belongs to the client.
    Content,
    /// The title bar, away from the buttons: a move drag.
    TitleBar,
    /// The close button.
    Close,
    /// The maximize button.
    Maximize,
    /// A resize band, naming the edges it pulls.
    Resize(Edges),
}

/// Where a drag currently is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Drag {
    /// Moving a window: the pointer's offset from the frame's top-left
    /// corner, in logical units, held constant for the whole drag.
    Move {
        /// The window being moved.
        window: WindowKey,
        /// Pointer minus frame origin, logical units.
        grab: Point,
    },
    /// Resizing a window.
    Resize {
        /// The window being resized.
        window: WindowKey,
        /// Which edges move.
        edges: Edges,
        /// The frame rect when the drag started, logical units.
        start: Rect,
        /// The pointer position when the drag started, logical units on the
        /// window's output.
        origin: Point,
    },
    /// The press landed on a button and has not been released yet; the
    /// action fires on release, inside the same button.
    Button {
        /// The window whose button was pressed.
        window: WindowKey,
        /// Which button.
        region: Region,
    },
}

impl Drag {
    /// The window this drag belongs to.
    #[must_use]
    pub fn window(&self) -> WindowKey {
        match *self {
            Self::Move { window, .. } | Self::Resize { window, .. } | Self::Button { window, .. } => {
                window
            }
        }
    }
}

/// Hit-test a point against a decorated window's frame.
///
/// `frame` is the whole window in logical units (`inset` included) and
/// `point` is in the same space. `fixed` suppresses the resize bands and
/// the maximize button for a `FIXED_SIZE` window.
///
/// The resize bands **straddle the frame edge**: [`RESIZE_BAND`] logical
/// pixels outside it, and inward only as far as the frame's own border or
/// title bar. Reaching a full band inwards would be easier to grab and
/// quite wrong — with a 1-px border it would steal the outermost six
/// pixels of the client's content, so a button against the window edge
/// could not be clicked at all. Six pixels of slop outside the window is
/// what makes a 1-px border grabbable, and it costs the client nothing
/// because those pixels are not its.
#[must_use]
pub fn hit_frame(frame: Rect, inset: Insets, point: Point, fixed: bool) -> Option<Region> {
    let outer = Rect::new(
        frame.x - RESIZE_BAND,
        frame.y - RESIZE_BAND,
        frame.w + 2.0 * RESIZE_BAND,
        frame.h + 2.0 * RESIZE_BAND,
    );
    if !contains(outer, point) {
        return None;
    }
    if fixed {
        // Nothing to grab outside a window that cannot be resized.
        if !contains(frame, point) {
            return None;
        }
    } else {
        let edges = Edges {
            left: point.x <= frame.x + inset.left,
            right: point.x >= frame.x + frame.w - inset.right,
            top: point.y <= frame.y + inset.top.min(RESIZE_BAND),
            bottom: point.y >= frame.y + frame.h - inset.bottom,
        };
        if edges.any() {
            return Some(Region::Resize(edges));
        }
        if !contains(frame, point) {
            // Inside the outer band but past every edge test: the pointer
            // is in the slop *outside* the window, so it is grabbing
            // whichever edges it is beyond.
            let edges = Edges {
                left: point.x < frame.x,
                right: point.x >= frame.x + frame.w,
                top: point.y < frame.y,
                bottom: point.y >= frame.y + frame.h,
            };
            return edges.any().then_some(Region::Resize(edges));
        }
    }
    if point.y < frame.y + inset.top {
        // The title bar: buttons on the right, drag everywhere else.
        for (region, rect) in buttons(frame, fixed) {
            if contains(rect, point) {
                return Some(region);
            }
        }
        return Some(Region::TitleBar);
    }
    Some(Region::Content)
}

/// The title bar's buttons, right to left: close, then maximize.
///
/// A `FIXED_SIZE` window gets only the close button — offering to maximize
/// a window that cannot be resized would be a lie.
#[must_use]
pub fn buttons(frame: Rect, fixed: bool) -> Vec<(Region, Rect)> {
    let y = frame.y + (TITLE_H - BUTTON) / 2.0;
    let close_x = frame.x + frame.w - BUTTON_GAP - BUTTON;
    let mut out = vec![(Region::Close, Rect::new(close_x, y, BUTTON, BUTTON))];
    if !fixed {
        out.push((
            Region::Maximize,
            Rect::new(close_x - BUTTON_GAP - BUTTON, y, BUTTON, BUTTON),
        ));
    }
    out
}

/// Whether a logical rect contains a point (half-open on the far edges).
#[must_use]
pub fn contains(rect: Rect, p: Point) -> bool {
    p.x >= rect.x && p.y >= rect.y && p.x < rect.x + rect.w && p.y < rect.y + rect.h
}

/// The new frame rect for a resize drag that has moved to `point`.
///
/// `min`/`max` are content limits: the frame's insets are added back before
/// clamping, so a client's declared minimum is honoured exactly. An edge
/// that is not being pulled never moves, which is what makes a
/// left-edge drag grow the window leftwards rather than move it.
#[must_use]
pub fn resize_rect(
    start: Rect,
    origin: Point,
    point: Point,
    edges: Edges,
    inset: Insets,
    min: Size,
    max: Size,
) -> Rect {
    let dx = point.x - origin.x;
    let dy = point.y - origin.y;
    let min_w = min.w.max(MIN_CONTENT.w) + inset.width();
    let min_h = min.h.max(MIN_CONTENT.h) + inset.height();
    let max_w = if max.w > 0.0 {
        (max.w + inset.width()).max(min_w)
    } else {
        f32::INFINITY
    };
    let max_h = if max.h > 0.0 {
        (max.h + inset.height()).max(min_h)
    } else {
        f32::INFINITY
    };

    let mut rect = start;
    if edges.right {
        rect.w = (start.w + dx).clamp(min_w, max_w);
    } else if edges.left {
        let w = (start.w - dx).clamp(min_w, max_w);
        rect.x = start.x + start.w - w;
        rect.w = w;
    }
    if edges.bottom {
        rect.h = (start.h + dy).clamp(min_h, max_h);
    } else if edges.top {
        let h = (start.h - dy).clamp(min_h, max_h);
        rect.y = start.y + start.h - h;
        rect.h = h;
    }
    rect
}

/// The work area of an output, in logical units: everything a maximized or
/// newly placed window may use.
///
/// In M3-A that is the whole output; M3-B subtracts the shell's exclusive
/// zones, and this is the one function that will need to know about them.
#[must_use]
pub fn work_area(scene: &Scene, output: OutputId) -> Rect {
    scene.output_info(output).map_or(Rect::EMPTY, |(rect, scale)| {
        let s = if scale > 0.0 { scale } else { 1.0 };
        Rect::new(0.0, 0.0, rect.w as f32 / s, rect.h as f32 / s)
    })
}

/// Where a new window of `size` (frame included) goes inside `area`.
///
/// **Centred cascade**: the first window is centred, each subsequent one
/// steps down and right from there, and the walk starts over once it would
/// leave the area. The result is always clamped inside the work area, so a
/// window is never created with its title bar off screen — which is the one
/// placement bug a user cannot recover from without a keyboard shortcut.
#[must_use]
pub fn place(index: u32, size: Size, area: Rect) -> Point {
    /// One cascade step, logical pixels.
    const STEP: f32 = 28.0;
    /// Longest cascade before starting over, however much room there is.
    const MAX_STEPS: u32 = 8;

    let centre = Point::new(
        area.x + (area.w - size.w) / 2.0,
        area.y + (area.h - size.h) / 2.0,
    );
    // How many steps fit between the centre and the bottom-right corner:
    // walking further would only pile windows up against the edge, since
    // every one past that point clamps to the same place.
    let room = ((area.x + area.w - size.w - centre.x).min(area.y + area.h - size.h - centre.y)
        / STEP)
        .floor()
        .max(0.0) as u32;
    let wrap = room.min(MAX_STEPS) + 1;
    let steps = f32::from(u16::try_from(index % wrap).unwrap_or(0));
    clamp_into(
        Point::new(centre.x + steps * STEP, centre.y + steps * STEP),
        size,
        area,
    )
}

/// Clamp a frame of `size` at `pos` so it lies inside `area` when it fits,
/// and so its top-left corner is at least reachable when it does not.
///
/// The result is rounded to whole logical pixels: a window at a half pixel
/// puts every edge and every glyph in it between device pixels, which is
/// both blurrier and more expensive to rasterize than it is worth.
#[must_use]
pub fn clamp_into(pos: Point, size: Size, area: Rect) -> Point {
    let max_x = (area.x + area.w - size.w).max(area.x);
    let max_y = (area.y + area.h - size.h).max(area.y);
    Point::new(
        pos.x.clamp(area.x, max_x).round(),
        pos.y.clamp(area.y, max_y).round(),
    )
}

/// Halves of a work area, for `Super+Left` / `Super+Right`.
#[must_use]
pub fn tile_rect(area: Rect, left: bool) -> Rect {
    let w = area.w / 2.0;
    Rect::new(if left { area.x } else { area.x + w }, area.y, w, area.h)
}

/// The whole window-management state the server carries: the MRU order,
/// the focus, the drag in flight and the `Alt+Tab` cycle.
#[derive(Debug, Default)]
pub struct WindowManager {
    /// Most-recently-used order, front = most recent. Minimized windows
    /// stay in it, which is what lets `Alt+Tab` un-minimize one.
    mru: Vec<WindowKey>,
    /// The window with keyboard focus.
    focus: Option<WindowKey>,
    /// The drag in flight, if any.
    drag: Option<Drag>,
    /// While `Alt` is held for a cycle: where in the MRU list we are. The
    /// list itself is not reordered until the modifier is released, so
    /// `Alt+Tab+Tab` reaches the third window rather than bouncing between
    /// two.
    cycle: Option<usize>,
    /// Windows created so far, for the cascade.
    placed: u32,
    /// The last title-bar press, for double-click detection.
    last_title_click: Option<(WindowKey, u64)>,
}

impl WindowManager {
    /// An empty manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The MRU order, most recent first.
    #[must_use]
    pub fn mru(&self) -> &[WindowKey] {
        &self.mru
    }

    /// The focused window.
    #[must_use]
    pub fn focus(&self) -> Option<WindowKey> {
        self.focus
    }

    /// Set the focused window without touching the MRU order. The caller
    /// sends the `Focus` events.
    pub fn set_focus(&mut self, window: Option<WindowKey>) {
        self.focus = window;
    }

    /// The drag in flight.
    #[must_use]
    pub fn drag(&self) -> Option<Drag> {
        self.drag
    }

    /// Start a drag.
    pub fn begin_drag(&mut self, drag: Drag) {
        self.drag = Some(drag);
    }

    /// End whatever drag was in flight, returning it.
    pub fn end_drag(&mut self) -> Option<Drag> {
        self.drag.take()
    }

    /// Whether an `Alt+Tab` cycle is in progress.
    #[must_use]
    pub fn cycling(&self) -> bool {
        self.cycle.is_some()
    }

    /// Take the next cascade index for a new window.
    pub fn next_placement(&mut self) -> u32 {
        let n = self.placed;
        self.placed = self.placed.wrapping_add(1);
        n
    }

    /// Record a window as the most recently used, moving it to the front.
    pub fn touch(&mut self, window: WindowKey) {
        self.mru.retain(|w| *w != window);
        self.mru.insert(0, window);
    }

    /// Add a window at the *back* of the MRU order: it exists but has never
    /// been used, so `Alt+Tab` reaches it last.
    pub fn add(&mut self, window: WindowKey) {
        if !self.mru.contains(&window) {
            self.mru.push(window);
        }
    }

    /// Forget a window entirely (it closed).
    pub fn remove(&mut self, window: WindowKey) {
        self.mru.retain(|w| *w != window);
        if self.focus == Some(window) {
            self.focus = None;
        }
        if self.drag.is_some_and(|d| d.window() == window) {
            self.drag = None;
        }
        if self.last_title_click.is_some_and(|(w, _)| w == window) {
            self.last_title_click = None;
        }
        self.cycle = None;
    }

    /// Advance an `Alt+Tab` cycle and return the window to highlight.
    ///
    /// `candidates` is the focusable set in MRU order; `forward` walks
    /// towards the less recently used. The first press of a cycle lands on
    /// the *second* entry, which is what makes `Alt+Tab` a toggle between
    /// the last two windows.
    pub fn cycle_next(&mut self, candidates: &[WindowKey], forward: bool) -> Option<WindowKey> {
        if candidates.is_empty() {
            return None;
        }
        let len = candidates.len();
        let index = match self.cycle {
            None => {
                if forward {
                    1 % len
                } else {
                    len - 1
                }
            }
            Some(i) => {
                if forward {
                    (i + 1) % len
                } else {
                    (i + len - 1) % len
                }
            }
        };
        self.cycle = Some(index);
        candidates.get(index).copied()
    }

    /// End an `Alt+Tab` cycle (the modifier came up).
    pub fn end_cycle(&mut self) -> Option<usize> {
        self.cycle.take()
    }

    /// Record a title-bar press and report whether it completes a double
    /// click on the same window.
    pub fn title_click(&mut self, window: WindowKey, time_ns: u64) -> bool {
        let double = self
            .last_title_click
            .is_some_and(|(w, t)| w == window && time_ns.saturating_sub(t) <= DOUBLE_CLICK_NS);
        // A double click consumes its history, so three clicks are one
        // double click and one single, not two doubles.
        self.last_title_click = if double {
            None
        } else {
            Some((window, time_ns))
        };
        double
    }
}

/// The windows `Alt+Tab` may reach, in MRU order: everything focusable,
/// minimized ones included.
#[must_use]
pub fn cycle_candidates(scene: &Scene, mru: &[WindowKey]) -> Vec<WindowKey> {
    mru.iter()
        .copied()
        .filter(|w| {
            scene
                .window_info(*w)
                .is_ok_and(|info| info.flags().focusable && info.layer() == Layer::Normal)
        })
        .collect()
}

/// The node ids of one window's frame decorations, so the server can
/// restyle them on a focus change and retitle them on a `SetWindowTitle`.
#[derive(Debug, Clone, Copy)]
pub struct FrameNodes {
    /// The frame group itself (the window's root).
    pub root: nitro_scene::NodeKey,
    /// The background rect covering the whole frame: border plus body.
    pub background: nitro_scene::NodeKey,
    /// The title bar rect.
    pub bar: nitro_scene::NodeKey,
    /// The title text node.
    pub title: nitro_scene::NodeKey,
    /// The close button.
    pub close: nitro_scene::NodeKey,
    /// The maximize button, absent on a `FIXED_SIZE` window.
    pub maximize: Option<nitro_scene::NodeKey>,
}

/// Build the decoration nodes of a freshly framed window.
///
/// Every node is owned by [`ClientId::SERVER`] and lives *under the frame
/// group, before the client's content*, so the client always paints on top
/// of its own frame's background and can never paint over the title bar.
///
/// # Errors
/// Anything the scene refuses; in practice only a dead window key.
pub fn build_frame(
    scene: &mut Scene,
    win: WindowKey,
    fixed: bool,
) -> Result<FrameNodes, nitro_scene::Error> {
    let root = scene.window_info(win)?.root();
    let content = scene.window_info(win)?.content();
    let s = ClientId::SERVER;
    let rect = |scene: &mut Scene| -> Result<nitro_scene::NodeKey, nitro_scene::Error> {
        scene.create_node(s, nitro_scene::NodeKind::Rect, root, Some(content))
    };
    let background = rect(scene)?;
    let bar = rect(scene)?;
    let title = scene.create_node(s, nitro_scene::NodeKind::Text, root, Some(content))?;
    let close = rect(scene)?;
    let maximize = if fixed { None } else { Some(rect(scene)?) };

    scene.set_corner_radius(s, background, CORNER_RADIUS)?;
    scene.set_corner_radius(s, bar, CORNER_RADIUS)?;
    scene.set_corner_radius(s, close, BUTTON / 2.0)?;
    if let Some(m) = maximize {
        scene.set_corner_radius(s, m, BUTTON / 2.0)?;
    }
    scene.set_fill(s, close, nitro_scene::Fill::Solid(theme::CLOSE))?;
    if let Some(m) = maximize {
        scene.set_fill(s, m, nitro_scene::Fill::Solid(theme::MAXIMIZE))?;
    }
    let nodes = FrameNodes {
        root,
        background,
        bar,
        title,
        close,
        maximize,
    };
    layout_frame(scene, win, &nodes)?;
    Ok(nodes)
}

/// Lay the decoration nodes out for the window's current size.
///
/// # Errors
/// Anything the scene refuses.
pub fn layout_frame(
    scene: &mut Scene,
    win: WindowKey,
    nodes: &FrameNodes,
) -> Result<(), nitro_scene::Error> {
    let info = scene.window_info(win)?;
    let size = info.frame_size();
    let fixed = info.flags().fixed_size;
    let s = ClientId::SERVER;
    scene.set_bounds(s, nodes.background, Rect::new(0.0, 0.0, size.w, size.h))?;
    scene.set_bounds(s, nodes.bar, Rect::new(0.0, 0.0, size.w, TITLE_H))?;
    // The title starts after the left border and stops before the buttons,
    // so a long title is elided rather than running under them.
    let button_room = if fixed {
        BUTTON + 2.0 * BUTTON_GAP
    } else {
        2.0 * BUTTON + 3.0 * BUTTON_GAP
    };
    let title_w = (size.w - BUTTON_GAP - button_room).max(0.0);
    scene.set_bounds(
        s,
        nodes.title,
        Rect::new(BUTTON_GAP, (TITLE_H - TITLE_SIZE_LINE) / 2.0, title_w, TITLE_SIZE_LINE),
    )?;
    let frame = Rect::new(0.0, 0.0, size.w, size.h);
    for (region, rect) in buttons(frame, fixed) {
        let key = match region {
            Region::Close => Some(nodes.close),
            Region::Maximize => nodes.maximize,
            _ => None,
        };
        if let Some(key) = key {
            scene.set_bounds(s, key, rect)?;
        }
    }
    Ok(())
}

/// Line height reserved for the title text: enough for the ascender and
/// descender of the 13 px face without measuring it.
pub const TITLE_SIZE_LINE: f32 = 18.0;

/// Restyle a frame for its focus state.
///
/// # Errors
/// Anything the scene refuses.
pub fn style_frame(
    scene: &mut Scene,
    nodes: &FrameNodes,
    focused: bool,
) -> Result<(), nitro_scene::Error> {
    let s = ClientId::SERVER;
    let (bar, border) = if focused {
        (theme::BAR_ACTIVE, theme::BORDER_ACTIVE)
    } else {
        (theme::BAR_INACTIVE, theme::BORDER_INACTIVE)
    };
    scene.set_fill(s, nodes.background, nitro_scene::Fill::Solid(bar))?;
    scene.set_border(
        s,
        nodes.background,
        Some(nitro_scene::Border::new(BORDER, border)),
    )?;
    scene.set_fill(s, nodes.bar, nitro_scene::Fill::Solid(bar))?;
    Ok(())
}

/// The title colour for a focus state.
#[must_use]
pub fn title_color(focused: bool) -> Color {
    if focused {
        theme::TITLE_ACTIVE
    } else {
        theme::TITLE_INACTIVE
    }
}

#[cfg(test)]
// Every number here is produced by exact arithmetic on exact inputs (sums
// and halves of small integers), so equality is the assertion that means
// what it says; an epsilon would only hide a wrong formula.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn frame() -> Rect {
        Rect::new(100.0, 50.0, 300.0, 200.0)
    }

    #[test]
    fn the_title_bar_moves_and_the_content_does_not() {
        let f = frame();
        let i = frame_insets();
        assert_eq!(
            hit_frame(f, i, Point::new(200.0, 60.0), false),
            Some(Region::TitleBar)
        );
        assert_eq!(
            hit_frame(f, i, Point::new(200.0, 150.0), false),
            Some(Region::Content)
        );
        // Well outside: nothing at all.
        assert_eq!(hit_frame(f, i, Point::new(500.0, 500.0), false), None);
    }

    #[test]
    fn edges_and_corners_resize() {
        let f = frame();
        let i = frame_insets();
        let left = hit_frame(f, i, Point::new(100.5, 150.0), false);
        assert_eq!(
            left,
            Some(Region::Resize(Edges {
                left: true,
                ..Edges::NONE
            }))
        );
        let corner = hit_frame(f, i, Point::new(399.5, 249.5), false);
        assert_eq!(
            corner,
            Some(Region::Resize(Edges {
                right: true,
                bottom: true,
                ..Edges::NONE
            }))
        );
        // The band straddles the edge, so just outside still grabs — which
        // is the only thing that makes a 1-px border usable.
        assert!(matches!(
            hit_frame(f, i, Point::new(402.0, 150.0), false),
            Some(Region::Resize(_))
        ));
        assert_eq!(
            hit_frame(f, i, Point::new(402.0, 252.0), false),
            Some(Region::Resize(Edges {
                right: true,
                bottom: true,
                ..Edges::NONE
            })),
            "the outside corner grabs both edges"
        );
        // The top band is inside the title bar, not inside the content.
        assert_eq!(
            hit_frame(f, i, Point::new(200.0, 51.0), false),
            Some(Region::Resize(Edges {
                top: true,
                ..Edges::NONE
            }))
        );
    }

    #[test]
    fn the_resize_band_never_steals_the_clients_content() {
        // Issue found by the toolkit tests: a band reaching six pixels
        // *inwards* over a 1-px border makes a button flush against the
        // window edge unclickable.
        let f = frame();
        let i = frame_insets();
        for d in 1..=6 {
            let x = f.x + i.left + d as f32;
            assert_eq!(
                hit_frame(f, i, Point::new(x, 150.0), false),
                Some(Region::Content),
                "{d} px inside the left border is the client's"
            );
            let y = f.y + f.h - i.bottom - d as f32;
            assert_eq!(
                hit_frame(f, i, Point::new(200.0, y), false),
                Some(Region::Content),
                "{d} px above the bottom border is the client's"
            );
        }
    }

    #[test]
    fn a_fixed_size_window_has_no_resize_bands_and_no_maximize() {
        let f = frame();
        let i = frame_insets();
        assert_eq!(
            hit_frame(f, i, Point::new(101.0, 150.0), true),
            Some(Region::Content)
        );
        assert_eq!(hit_frame(f, i, Point::new(402.0, 150.0), true), None);
        let regions: Vec<Region> = buttons(f, true).into_iter().map(|(r, _)| r).collect();
        assert_eq!(regions, vec![Region::Close]);
    }

    #[test]
    fn the_buttons_are_hit_before_the_bar() {
        let f = frame();
        let i = frame_insets();
        let close = buttons(f, false)[0].1;
        let centre = Point::new(close.x + close.w / 2.0, close.y + close.h / 2.0);
        assert_eq!(hit_frame(f, i, centre, false), Some(Region::Close));
        let max = buttons(f, false)[1].1;
        let centre = Point::new(max.x + max.w / 2.0, max.y + max.h / 2.0);
        assert_eq!(hit_frame(f, i, centre, false), Some(Region::Maximize));
        // The buttons do not overlap.
        assert!(max.x + max.w <= close.x);
    }

    #[test]
    fn a_left_edge_drag_grows_leftwards_and_respects_the_minimum() {
        let start = Rect::new(100.0, 50.0, 300.0, 200.0);
        let edges = Edges {
            left: true,
            ..Edges::NONE
        };
        let i = frame_insets();
        let r = resize_rect(
            start,
            Point::new(100.0, 150.0),
            Point::new(60.0, 150.0),
            edges,
            i,
            Size::ZERO,
            Size::ZERO,
        );
        assert_eq!(r.x, 60.0);
        assert_eq!(r.w, 340.0);
        assert_eq!((r.y, r.h), (50.0, 200.0), "the other axis never moves");

        // Dragging the left edge past the right one stops at the minimum
        // and keeps the right edge where it was.
        let r = resize_rect(
            start,
            Point::new(100.0, 150.0),
            Point::new(9999.0, 150.0),
            edges,
            i,
            Size::ZERO,
            Size::ZERO,
        );
        assert_eq!(r.w, MIN_CONTENT.w + i.width());
        assert_eq!(r.x + r.w, start.x + start.w);
    }

    #[test]
    fn limits_bound_a_resize_on_both_ends() {
        let start = Rect::new(0.0, 0.0, 300.0, 200.0);
        let edges = Edges {
            right: true,
            bottom: true,
            ..Edges::NONE
        };
        let i = Insets::NONE;
        let r = resize_rect(
            start,
            Point::ZERO,
            Point::new(1000.0, 1000.0),
            edges,
            i,
            Size::new(100.0, 100.0),
            Size::new(500.0, 400.0),
        );
        assert_eq!((r.w, r.h), (500.0, 400.0));
        let r = resize_rect(
            start,
            Point::ZERO,
            Point::new(-1000.0, -1000.0),
            edges,
            i,
            Size::new(150.0, 120.0),
            Size::new(500.0, 400.0),
        );
        assert_eq!((r.w, r.h), (150.0, 120.0));
    }

    #[test]
    fn placement_is_a_centred_cascade_clamped_to_the_work_area() {
        let area = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let size = Size::new(400.0, 300.0);
        let first = place(0, size, area);
        assert_eq!(first, Point::new(300.0, 250.0), "the first is centred");
        let second = place(1, size, area);
        assert!(
            second.x > first.x && second.y > first.y,
            "{first:?} {second:?}"
        );
        for i in 0..32 {
            let p = place(i, size, area);
            assert!(p.x >= 0.0 && p.y >= 0.0, "{i}: {p:?}");
            assert!(p.x + size.w <= area.w + 0.01, "{i}: {p:?}");
            assert!(p.y + size.h <= area.h + 0.01, "{i}: {p:?}");
        }
        // A window bigger than the work area is pinned to its origin
        // rather than pushed off the top-left.
        let huge = place(3, Size::new(2000.0, 2000.0), area);
        assert_eq!(huge, Point::new(0.0, 0.0));
        // A window that only just fits gets no cascade at all, rather
        // than a walk that clamps every step onto the same pixel.
        let tight = Size::new(990.0, 790.0);
        assert_eq!(place(0, tight, area), place(5, tight, area));
    }

    #[test]
    fn tiling_splits_the_work_area_exactly() {
        let area = Rect::new(10.0, 20.0, 800.0, 600.0);
        let l = tile_rect(area, true);
        let r = tile_rect(area, false);
        assert_eq!((l.x, l.w), (10.0, 400.0));
        assert_eq!((r.x, r.w), (410.0, 400.0));
        assert_eq!(l.x + l.w, r.x);
        assert_eq!((l.y, l.h), (20.0, 600.0));
    }

    fn key(n: u32) -> WindowKey {
        WindowKey::from_parts(n, 1)
    }

    #[test]
    fn the_mru_moves_a_touched_window_to_the_front() {
        let mut wm = WindowManager::new();
        wm.add(key(1));
        wm.add(key(2));
        wm.add(key(3));
        assert_eq!(wm.mru(), [key(1), key(2), key(3)]);
        wm.touch(key(3));
        assert_eq!(wm.mru(), [key(3), key(1), key(2)]);
        wm.touch(key(3));
        assert_eq!(wm.mru(), [key(3), key(1), key(2)], "idempotent");
        wm.remove(key(1));
        assert_eq!(wm.mru(), [key(3), key(2)]);
    }

    #[test]
    fn alt_tab_walks_the_mru_and_holds_its_place() {
        let mut wm = WindowManager::new();
        let all = [key(1), key(2), key(3)];
        // First press: the previously used window.
        assert_eq!(wm.cycle_next(&all, true), Some(key(2)));
        assert!(wm.cycling());
        // Second press: the one before that, not back to the first.
        assert_eq!(wm.cycle_next(&all, true), Some(key(3)));
        assert_eq!(wm.cycle_next(&all, true), Some(key(1)));
        // Shift walks the other way.
        assert_eq!(wm.cycle_next(&all, false), Some(key(3)));
        wm.end_cycle();
        assert!(!wm.cycling());
        // A fresh cycle starts from the top again.
        assert_eq!(wm.cycle_next(&all, true), Some(key(2)));
    }

    #[test]
    fn alt_tab_on_one_window_stays_on_it() {
        let mut wm = WindowManager::new();
        assert_eq!(wm.cycle_next(&[key(7)], true), Some(key(7)));
        assert_eq!(wm.cycle_next(&[], true), None);
    }

    #[test]
    fn a_double_click_needs_the_same_window_and_the_interval() {
        let mut wm = WindowManager::new();
        assert!(!wm.title_click(key(1), 1_000));
        assert!(wm.title_click(key(1), 1_000 + DOUBLE_CLICK_NS));
        // Consumed: the next click starts over.
        assert!(!wm.title_click(key(1), 1_000 + DOUBLE_CLICK_NS + 1));
        // Too slow.
        assert!(!wm.title_click(key(1), 10_000_000_000));
        // Different window.
        assert!(!wm.title_click(key(2), 10_000_000_001));
    }

    #[test]
    fn removing_a_window_cancels_its_drag_and_its_focus() {
        let mut wm = WindowManager::new();
        wm.add(key(1));
        wm.set_focus(Some(key(1)));
        wm.begin_drag(Drag::Move {
            window: key(1),
            grab: Point::ZERO,
        });
        wm.remove(key(1));
        assert!(wm.focus().is_none());
        assert!(wm.drag().is_none());
    }
}
