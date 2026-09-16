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
//!   ([`nitro_scene::Scene::frame_window`]), carrying a title bar, a border,
//!   the application's icon and three buttons. A client that sets
//!   `UNDECORATED` gets no frame and
//!   still gets server move/resize through the `Super` modifier.
//! * **The server moves and resizes windows with zero client round-trips.**
//!   A drag is one scene mutation per motion event plus one
//!   *one-way* `Configure` (a move changes `Configure.position`, which is
//!   what a client crops a screenshot with); the client is never asked
//!   anything and nothing waits for it. The frame scheduler throttles
//!   those `Configure`s to one per frame.
//! * **Focus follows the raise**, the raise follows the click, and the MRU
//!   list is what `Alt+Tab` walks.
//!
//! What is *not* here: workspaces, cursor shapes and a persistent
//! multi-output layout. See `docs/wm.md`.

use nitro_core::{Color, Palette, Point, Rect, Role, Size};
use nitro_scene::{ClientId, Insets, Layer, OutputId, Scene, WindowKey};

/// Title-bar height in logical pixels.
pub const TITLE_H: f32 = 28.0;
/// Border width in logical pixels, drawn on the left, right and bottom.
pub const BORDER: f32 = 1.0;
/// Corner radius of the frame's top corners, logical pixels.
pub const CORNER_RADIUS: f32 = 6.0;
/// How wide the resize grab band is, inside and outside the frame edge.
pub const RESIZE_BAND: f32 = 6.0;
/// Size of one title-bar button (a square), logical pixels.
///
/// It is both the **hit** region and the hover disc the pointer lights
/// up: letting the two differ — a slightly larger disc looks better —
/// would put a visible affordance edge where a press does nothing, which
/// is the mistake [`Role::ResizeHint`] already exists to have fixed once.
pub const BUTTON: f32 = 14.0;
/// Gap between the buttons and from the right edge.
pub const BUTTON_GAP: f32 = 8.0;
/// The application icon in the title bar, logical pixels.
///
/// 16 rather than [`BUTTON`]'s 14: this is the one icon in the frame that
/// may be somebody else's PNG, and 16 is the size every icon theme ships
/// (`docs/icons.md`). A 14 would resample every one of them.
pub const APP_ICON: f32 = 16.0;
/// Gap between the application icon and the title text.
pub const APP_ICON_GAP: f32 = 6.0;
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

/// Non-colour constants of the frame's look.
///
/// The colours used to live here as `const`s and now come from the
/// server's [`Palette`] — `TitleBarActive`, `WindowBorderInactive` and
/// the rest — so that the user's `theme.scheme` reaches the decorations
/// like it reaches everything else. What is left is the one number that
/// is a *metric*, not a colour, and so is not a palette role.
pub mod theme {
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
    /// The minimize button.
    Minimize,
    /// A resize band, naming the edges it pulls.
    Resize(Edges),
}

impl Region {
    /// Whether this region is one of the title bar's buttons.
    ///
    /// Every button behaves the same in three places — a press begins a
    /// [`Drag::Button`], a hover lights a disc, a release fires inside
    /// itself — so the set is named once rather than spelled out as a
    /// three-arm `matches!` in each.
    #[must_use]
    pub fn is_button(self) -> bool {
        matches!(self, Self::Close | Self::Maximize | Self::Minimize)
    }
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
            Self::Move { window, .. }
            | Self::Resize { window, .. }
            | Self::Button { window, .. } => window,
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

/// The title bar's buttons, right to left: close, maximize, minimize.
///
/// A `FIXED_SIZE` window keeps close and minimize and loses **maximize**
/// — offering to maximize a window that cannot be resized would be a lie,
/// while putting one away is something any window can do. It is also what
/// makes the missing button *mean* something: a frame with two buttons is
/// a frame that will not resize, visible before you try it.
///
/// Right to left rather than left to right because close is the button
/// whose position a user knows without looking, so it is the one that has
/// to stay pinned to the corner when the others come and go.
#[must_use]
pub fn buttons(frame: Rect, fixed: bool) -> Vec<(Region, Rect)> {
    let y = frame.y + (TITLE_H - BUTTON) / 2.0;
    let mut x = frame.x + frame.w - BUTTON_GAP - BUTTON;
    let mut out = vec![(Region::Close, Rect::new(x, y, BUTTON, BUTTON))];
    if !fixed {
        x -= BUTTON_GAP + BUTTON;
        out.push((Region::Maximize, Rect::new(x, y, BUTTON, BUTTON)));
    }
    x -= BUTTON_GAP + BUTTON;
    out.push((Region::Minimize, Rect::new(x, y, BUTTON, BUTTON)));
    out
}

/// The left edge of the leftmost title-bar button: everything the frame
/// draws to the left of it — the application icon and the title — has to
/// fit inside that.
///
/// Derived from [`buttons`] rather than computed from the constants a
/// second time, which is the whole reason it exists: the layout and the
/// hit test have to agree about where the buttons start, and two copies
/// of that arithmetic are two chances to disagree.
#[must_use]
pub fn buttons_start(frame: Rect, fixed: bool) -> f32 {
    buttons(frame, fixed)
        .last()
        .map_or(frame.x + frame.w, |(_, r)| r.x)
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
    scene
        .output_info(output)
        .map_or(Rect::EMPTY, |(rect, scale)| {
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

    /// Move a window to the *back* of the MRU order without removing it.
    ///
    /// What minimizing does. A minimized window is still in the list — that
    /// is what lets `Alt+Tab` bring it back — but it is emphatically not
    /// the most recently used one any more: the user just put it away.
    /// Leaving it at the front is what makes the first `Alt+Tab` after a
    /// minimize land on the window that already has focus and appear to do
    /// nothing at all.
    pub fn demote(&mut self, window: WindowKey) {
        if self.mru.contains(&window) {
            self.mru.retain(|w| *w != window);
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

/// The symbolic icon names the frame draws with.
///
/// Named here rather than inlined because they are the frame's whole
/// visual vocabulary, and because each is a choice:
///
/// * `x` for close and `dash` for minimize — the two shapes every desktop
///   uses, and the two that survive being ten logical pixels wide.
/// * `square` for maximize rather than `arrows-angle-expand`. The arrows
///   read better in a vector viewer and turn to mush at this size: four
///   diagonal heads and their tails inside a ten-unit box, against one
///   outlined rectangle whose four strokes land on whole pixels. The
///   square is also what the button *does* — fill the screen — rather
///   than a metaphor for it.
/// * `window` is the application-icon fallback, matching the rule
///   `nitro-bar` already uses for its window list (`docs/shell.md`).
pub mod icon_names {
    /// The close button.
    pub const CLOSE: &str = "x";
    /// The maximize button.
    pub const MAXIMIZE: &str = "square";
    /// The minimize button.
    pub const MINIMIZE: &str = "dash";
    /// What the title bar shows when the window's `app_id` resolves to
    /// no icon at all.
    pub const FALLBACK_APP: &str = "window";
}

/// The glyph inside a title-bar button, logical pixels.
///
/// Smaller than [`BUTTON`] so the hover disc reads as a disc *behind* the
/// glyph rather than as a box around it.
pub const BUTTON_ICON: f32 = 10.0;

/// One title-bar button's two nodes.
///
/// A rect and an icon, and they cannot be one node: an icon node has no
/// fill and a rect node has no artwork. The rect is transparent until the
/// pointer is on the button, which is what makes a resting title bar
/// three glyphs on a flat strip rather than three coloured lozenges.
#[derive(Debug, Clone, Copy)]
pub struct FrameButton {
    /// The hover disc, transparent while nothing is pointing at it.
    pub background: nitro_scene::NodeKey,
    /// The symbolic glyph, tinted from the palette.
    pub icon: nitro_scene::NodeKey,
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
    /// The application's icon, left of the title.
    pub app_icon: nitro_scene::NodeKey,
    /// The title text node.
    pub title: nitro_scene::NodeKey,
    /// The close button.
    pub close: FrameButton,
    /// The maximize button, absent on a `FIXED_SIZE` window.
    pub maximize: Option<FrameButton>,
    /// The minimize button.
    pub minimize: FrameButton,
}

impl FrameNodes {
    /// The button a [`Region`] names, if it names one this frame has.
    #[must_use]
    pub fn button(&self, region: Region) -> Option<FrameButton> {
        match region {
            Region::Close => Some(self.close),
            Region::Maximize => self.maximize,
            Region::Minimize => Some(self.minimize),
            _ => None,
        }
    }

    /// Every node in the frame, for the paths that treat them alike:
    /// hiding the decorations for fullscreen, and counting them.
    #[must_use]
    pub fn all(&self) -> Vec<nitro_scene::NodeKey> {
        let mut out = vec![
            self.background,
            self.bar,
            self.app_icon,
            self.title,
            self.close.background,
            self.close.icon,
            self.minimize.background,
            self.minimize.icon,
        ];
        if let Some(m) = self.maximize {
            out.push(m.background);
            out.push(m.icon);
        }
        out
    }
}

/// Build the decoration nodes of a freshly framed window.
///
/// Every node is owned by [`ClientId::SERVER`] and lives *under the frame
/// group, before the client's content*, so the client always paints on top
/// of its own frame's background and can never paint over the title bar.
///
/// # The node count, and why each one is there
///
/// **Eleven** for a resizable window, **nine** for a `FIXED_SIZE` one
/// — `docs/budget.md` multiplies this by the 240 bytes a `Node` costs, so
/// it is pinned by a test:
///
/// | node | why it cannot be shared |
/// |---|---|
/// | frame group | the window's root; the insets hang off it |
/// | background | the border and the body, one rounded rect |
/// | title bar | a different colour from the body |
/// | app icon | artwork, which no rect can hold |
/// | title | a shaped text run |
/// | 3 × (disc + glyph) | a rect has no artwork and an icon has no fill |
///
/// The buttons are the expensive half, and the alternative was
/// considered: **one** disc, moved to whichever button is hovered, since
/// only one can be — nine nodes and seven. It was not taken because the
/// disc's bounds would then change on every hover, damaging its old
/// rectangle *and* its new one, where three fixed discs each damage only
/// themselves; a hover would cost twice the pixels it does now, on the
/// motion path.
///
/// # Errors
/// Anything the scene refuses; in practice only a dead window key.
pub fn build_frame(
    scene: &mut Scene,
    win: WindowKey,
    fixed: bool,
    palette: &Palette,
) -> Result<FrameNodes, nitro_scene::Error> {
    let root = scene.window_info(win)?.root();
    let content = scene.window_info(win)?.content();
    let s = ClientId::SERVER;
    let rect = |scene: &mut Scene| -> Result<nitro_scene::NodeKey, nitro_scene::Error> {
        scene.create_node(s, nitro_scene::NodeKind::Rect, root, Some(content))
    };
    let icon_node = |scene: &mut Scene| -> Result<nitro_scene::NodeKey, nitro_scene::Error> {
        scene.create_node(s, nitro_scene::NodeKind::Icon, root, Some(content))
    };
    let background = rect(scene)?;
    let bar = rect(scene)?;
    let app_icon = icon_node(scene)?;
    let title = scene.create_node(s, nitro_scene::NodeKind::Text, root, Some(content))?;
    // Created in the order they are drawn: the disc first, the glyph on
    // top of it. Siblings paint in creation order, so this is the whole
    // of what puts the glyph over its own background.
    let button = |scene: &mut Scene, name: &str| -> Result<FrameButton, nitro_scene::Error> {
        let background = rect(scene)?;
        let glyph = icon_node(scene)?;
        scene.set_corner_radius(s, background, BUTTON / 2.0)?;
        if let Some(index) = nitro_icons::index_of(name) {
            // The role is a stored *index*, resolved per frame against
            // whatever palette the server holds — which is why a scheme
            // flip recolours the frame's glyphs with no re-raster,
            // exactly as it does a client's. `style_frame` overwrites it
            // with the focus-dependent one immediately; this is the
            // value a frame would keep if it were never styled.
            scene.set_icon(
                s,
                glyph,
                Some(nitro_scene::IconRef::new(
                    index,
                    BUTTON_ICON,
                    role_byte(Role::TitleTextActive),
                )),
            )?;
        }
        Ok(FrameButton {
            background,
            icon: glyph,
        })
    };
    let close = button(scene, icon_names::CLOSE)?;
    let maximize = if fixed {
        None
    } else {
        Some(button(scene, icon_names::MAXIMIZE)?)
    };
    let minimize = button(scene, icon_names::MINIMIZE)?;

    scene.set_corner_radius(s, background, CORNER_RADIUS)?;
    scene.set_corner_radius(s, bar, CORNER_RADIUS)?;
    let nodes = FrameNodes {
        root,
        background,
        bar,
        app_icon,
        title,
        close,
        maximize,
        minimize,
    };
    layout_frame(scene, win, &nodes)?;
    style_frame(scene, &nodes, false, false, None, palette)?;
    Ok(nodes)
}

/// A palette role as the byte a [`nitro_scene::IconRef`] stores.
///
/// Exact by construction: `Role` is `repr(u8)`, so its index cannot
/// exceed 255.
#[must_use]
pub fn role_byte(role: Role) -> u8 {
    role.index() as u8
}

/// Point a frame's application-icon node at an icon, or clear it.
///
/// The frame is the server's **own** tree, so there is no `SetIcon` on
/// the wire and no client to answer: this is the internal twin of the
/// path a client's `SetIcon` takes, and it is why the server can do the
/// fallback synchronously. A client that names an icon the server lacks
/// is told `BadIcon` and sends its own fallback a message later; the
/// server *is* the resolver, so it simply resolves the fallback in the
/// same call and the node is never briefly blank.
///
/// `icon` is the `(handle, role)` pair the engine already resolved:
/// `AS_COLOURED` for a theme PNG, a palette role for a symbolic shape
/// (see [`crate::icons::AppIcon`]).
///
/// # Errors
/// Anything the scene refuses.
pub fn set_app_icon(
    scene: &mut Scene,
    nodes: &FrameNodes,
    icon: Option<(u32, u8)>,
) -> Result<(), nitro_scene::Error> {
    let reference = icon.map(|(handle, role)| nitro_scene::IconRef::new(handle, APP_ICON, role));
    scene.set_icon(ClientId::SERVER, nodes.app_icon, reference)
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
    // Where the leftmost button starts is what everything to its left has
    // to fit inside. Taken from `buttons` rather than recomputed, so the
    // layout and the hit test cannot disagree about it.
    let frame = Rect::new(0.0, 0.0, size.w, size.h);
    let all = buttons(frame, fixed);
    let start = all.last().map_or(size.w, |(_, r)| r.x);

    // **The icon goes away before it is drawn through.** A window can be
    // dragged down to a 64-pixel content box (`MIN_CONTENT`), which is a
    // 66-pixel frame — narrower than three buttons and their gaps. The
    // icon's box would then start at 8 and the leftmost button at 0, so
    // the artwork and the glyph would be composited on top of each other.
    //
    // Clipping is the honest failure and overlap is not: an overlapped
    // icon is two shapes nobody can read, where a dropped one is a title
    // bar that is visibly too small for it. (#3709 and #3710 reached the
    // same rule for the toolkit's flex solver from the other end: a row
    // drawn through a sentence is worse than a row that is not drawn.)
    // An empty rect paints nothing — `Node::has_content` is false for one
    // — without touching `visible`, which fullscreen owns.
    let icon_fits = BUTTON_GAP + APP_ICON + APP_ICON_GAP <= start;
    let icon_box = if icon_fits {
        Rect::new(BUTTON_GAP, (TITLE_H - APP_ICON) / 2.0, APP_ICON, APP_ICON)
    } else {
        Rect::new(BUTTON_GAP, (TITLE_H - APP_ICON) / 2.0, 0.0, 0.0)
    };
    scene.set_bounds(s, nodes.app_icon, icon_box)?;
    // The title starts after the application icon — or at the left border
    // when there was no room for one — and stops one gap before the
    // buttons, so a long title is elided rather than running under
    // either. The icon took `APP_ICON + APP_ICON_GAP` off its left end,
    // which is what makes a narrow window elide sooner than it used to
    // rather than draw its title through the artwork.
    let title_x = if icon_fits {
        BUTTON_GAP + APP_ICON + APP_ICON_GAP
    } else {
        BUTTON_GAP
    };
    let title_w = (start - BUTTON_GAP - title_x).max(0.0);
    scene.set_bounds(
        s,
        nodes.title,
        Rect::new(
            title_x,
            (TITLE_H - TITLE_SIZE_LINE) / 2.0,
            title_w,
            TITLE_SIZE_LINE,
        ),
    )?;
    for (region, rect) in all {
        let Some(button) = nodes.button(region) else {
            continue;
        };
        scene.set_bounds(s, button.background, rect)?;
        // The glyph's node is the *whole* button, not the glyph's own
        // box: the scene centres an icon in its bounds, so this is what
        // puts a 10 px shape in the middle of a 14 px disc without any
        // arithmetic that could disagree with the disc's.
        scene.set_bounds(s, button.icon, rect)?;
    }
    Ok(())
}

/// Line height reserved for the title text: enough for the ascender and
/// descender of the 13 px face without measuring it.
pub const TITLE_SIZE_LINE: f32 = 18.0;

/// Restyle a frame for its focus state and the current palette.
///
/// Every colour a decoration has is set here — including the buttons'
/// glyph tints and hover discs, which do not depend on focus but do
/// depend on the palette — so that a `theme.scheme` change is one call
/// per frame and needs no second path for "the colours moved but the
/// focus did not".
///
/// `hint` lights the border up in [`Role::ResizeHint`]: the pointer is in
/// this window's resize band, and until cursor shapes land (M4) the
/// border changing colour is the only thing that says so. See
/// [`border_color`].
///
/// `hover` is the button the pointer is on, whose disc is painted; every
/// other disc is transparent. It is the same motion path `hint` rides,
/// and for the same reason: a frame button with no hover state is a
/// symbol that gives no sign it can be clicked.
///
/// # Why the icon nodes are rewritten and the title is not
///
/// Retinting an icon is `set_icon` with a different role **byte**, which
/// is a `PAINT` mark and nothing else — no raster, because the cache
/// holds coverage (`docs/icons.md`), and no allocation. Retinting the
/// *title* means re-eliding and re-shaping, which is why `retitle` is a
/// separate call the hover path does not make. That asymmetry is the
/// whole reason a hover can be `style_only`.
///
/// # Errors
/// Anything the scene refuses.
pub fn style_frame(
    scene: &mut Scene,
    nodes: &FrameNodes,
    focused: bool,
    hint: bool,
    hover: Option<Region>,
    palette: &Palette,
) -> Result<(), nitro_scene::Error> {
    let s = ClientId::SERVER;
    let bar = if focused {
        palette.get(Role::TitleBarActive)
    } else {
        palette.get(Role::TitleBarInactive)
    };
    let border = border_color(focused, hint, palette);
    scene.set_fill(s, nodes.background, nitro_scene::Fill::Solid(bar))?;
    scene.set_border(
        s,
        nodes.background,
        Some(nitro_scene::Border::new(BORDER, border)),
    )?;
    scene.set_fill(s, nodes.bar, nitro_scene::Fill::Solid(bar))?;
    let text = title_role(focused);
    for region in [Region::Close, Region::Maximize, Region::Minimize] {
        let Some(button) = nodes.button(region) else {
            continue;
        };
        let hovered = hover == Some(region);
        let disc = if hovered {
            nitro_scene::Fill::Solid(palette.get(button_hover_role(region)))
        } else {
            // Transparent, not the bar's colour: a disc painted in the
            // bar's own colour is still a node with a fill, so it still
            // rasterises a rounded rect every frame and still has to be
            // repainted when the bar's colour moves. `Fill::None` paints
            // nothing at all, which is what a resting button costs here.
            nitro_scene::Fill::None
        };
        scene.set_fill(s, button.background, disc)?;
        // The glyph follows the title's colour, so an unfocused window's
        // buttons recede exactly as its title does — one statement about
        // focus rather than two that could disagree. On hover it takes
        // the colour that reads on the disc, whatever the focus: the
        // pointer is on it, and a button that dims while being pointed
        // at reads as disabled.
        let tint = if hovered {
            button_hover_text_role(region)
        } else {
            text
        };
        retint(scene, button.icon, tint)?;
    }
    Ok(())
}

/// Re-point an icon node at the same icon and size in a different palette
/// role.
///
/// The handle and the size are read back from the node rather than kept
/// in a shadow copy beside it: two records of one icon can disagree, and
/// this way there is only one. What changes is the **role index**, never
/// a colour — the painter resolves it per frame, which is what makes a
/// scheme flip free (`docs/icons.md`).
fn retint(
    scene: &mut Scene,
    key: nitro_scene::NodeKey,
    role: Role,
) -> Result<(), nitro_scene::Error> {
    let Some(existing) = scene.node(key).ok().and_then(nitro_scene::Node::icon) else {
        // No glyph: the icon set lacks the name, which `build_frame`
        // already tolerated. Nothing to tint.
        return Ok(());
    };
    scene.set_icon(
        ClientId::SERVER,
        key,
        Some(nitro_scene::IconRef::new(
            existing.icon,
            existing.size(),
            role_byte(role),
        )),
    )
}

/// The disc colour under a hovered title-bar button.
///
/// Close is [`Role::TitleClose`] and the others are
/// [`Role::TitleButtonHover`], which is the one place the frame still
/// spends the red: a close button that looks like its neighbours until
/// the moment you point at it, and then unmistakably does not. Painting
/// it red at rest is what the frame did before #3715, and it made the
/// most destructive control on the window the most eye-catching thing on
/// it.
#[must_use]
pub fn button_hover_role(region: Region) -> Role {
    match region {
        Region::Close => Role::TitleClose,
        _ => Role::TitleButtonHover,
    }
}

/// The glyph colour on a hovered title-bar button.
///
/// On close that is [`Role::TextOnAccent`]: the disc underneath is a
/// saturated red in both schemes, and the title text colour — near-black
/// on light, near-white on dark — would read against it by luck rather
/// than by design. `text_on_accent` is the role that exists for "ink on a
/// saturated field", and it is the one that flips with the scheme.
#[must_use]
pub fn button_hover_text_role(region: Region) -> Role {
    match region {
        Region::Close => Role::TextOnAccent,
        _ => Role::TitleTextActive,
    }
}

/// The colour of a frame's border: its focus colour, or
/// [`Role::ResizeHint`] while the pointer is in its resize band.
///
/// # Why the band is *shown* rather than made bigger
///
/// The band already straddles the edge — [`RESIZE_BAND`] outwards and
/// inwards as far as the frame's own border ([`hit_frame`]) — so pressing
/// *on* the visible border has always worked. What did not work was
/// knowing that, and #3713 is a user reporting exactly that: "resizing
/// does not work (by grabbing a border)" on a border one pixel wide, with
/// no cursor shape to say otherwise.
///
/// Widening the band inwards would not have fixed it either — it steals
/// the client's outermost pixels and makes a button flush against the
/// window edge unclickable, which is the mistake
/// `the_resize_band_never_steals_the_clients_content` exists to keep out.
/// So the fix is to make the band *visible*: the frame's own border, which
/// the user is already aiming at, changes colour the moment the pointer is
/// somewhere a press would resize. It costs no node and no pixel of
/// anyone's content, and it goes away on its own when cursor shapes
/// arrive.
#[must_use]
pub fn border_color(focused: bool, hint: bool, palette: &Palette) -> Color {
    if hint {
        return palette.get(Role::ResizeHint);
    }
    if focused {
        palette.get(Role::WindowBorderActive)
    } else {
        palette.get(Role::WindowBorderInactive)
    }
}

/// The title colour for a focus state.
#[must_use]
pub fn title_color(focused: bool, palette: &Palette) -> Color {
    palette.get(title_role(focused))
}

/// The palette role a frame's title text and button glyphs take.
///
/// A role rather than a colour, because an icon node stores an **index**
/// and resolves it per frame: that is what makes a `theme.scheme` flip
/// recolour the frame's glyphs with no re-raster and no second path.
/// [`title_color`] is the same answer for the text node, which stores a
/// resolved colour because a shaped run does.
#[must_use]
pub fn title_role(focused: bool) -> Role {
    if focused {
        Role::TitleTextActive
    } else {
        Role::TitleTextInactive
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
        // Close and minimize stay: putting a window away is something any
        // window can do, and only *maximize* would be a lie on one that
        // cannot be resized. The gap it leaves is the affordance — a
        // two-button frame is a frame that will not resize, visible
        // before you try it.
        let regions: Vec<Region> = buttons(f, true).into_iter().map(|(r, _)| r).collect();
        assert_eq!(regions, vec![Region::Close, Region::Minimize]);
        // And the two are where the three would have put them, minus the
        // middle one: close is still pinned to the corner.
        assert_eq!(buttons(f, true)[0].1, buttons(f, false)[0].1);
        assert_eq!(buttons(f, true)[1].1, buttons(f, false)[1].1);
    }

    #[test]
    fn the_buttons_are_hit_before_the_bar() {
        let f = frame();
        let i = frame_insets();
        let all = buttons(f, false);
        assert_eq!(
            all.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            vec![Region::Close, Region::Maximize, Region::Minimize],
            "right to left: close is the one whose position users know"
        );
        for (region, rect) in &all {
            let centre = Point::new(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
            assert_eq!(hit_frame(f, i, centre, false), Some(*region));
            assert!(region.is_button(), "{region:?}");
        }
        // They do not overlap, and they are all inside the title bar.
        for pair in all.windows(2) {
            let (right, left) = (pair[0].1, pair[1].1);
            assert!(left.x + left.w <= right.x, "{left:?} {right:?}");
        }
        for (_, rect) in &all {
            assert!(rect.y >= f.y && rect.y + rect.h <= f.y + TITLE_H);
            assert!(rect.x >= f.x && rect.x + rect.w <= f.x + f.w);
        }
        // A hit on the bar away from them is still a move drag.
        assert_eq!(
            hit_frame(f, i, Point::new(f.x + 60.0, f.y + 14.0), false),
            Some(Region::TitleBar)
        );
        assert!(!Region::TitleBar.is_button());
        assert!(!Region::Content.is_button());
    }

    #[test]
    fn the_title_gets_what_the_icon_and_the_buttons_leave() {
        // The arithmetic `layout_frame` does, checked here because it is
        // the one number a frame's look depends on that no pixel test
        // would localise: a title too wide runs under the buttons, a
        // title too narrow elides a name that would have fitted.
        let width = 300.0;
        let title_x = BUTTON_GAP + APP_ICON + APP_ICON_GAP;
        for fixed in [false, true] {
            let frame = Rect::new(0.0, 0.0, width, 200.0);
            let start = buttons_start(frame, fixed);
            let title_w = start - BUTTON_GAP - title_x;
            assert!(
                title_x + title_w <= start,
                "fixed={fixed}: the title ends at {} and the first button starts at {start}",
                title_x + title_w,
            );
            // And it stops exactly one gap short, rather than wasting
            // room a long title could have used.
            assert_eq!(title_x + title_w + BUTTON_GAP, start);
            // `buttons_start` is the leftmost button's own x, not a
            // second opinion about it.
            assert_eq!(
                start,
                buttons(frame, fixed).last().expect("buttons").1.x,
                "fixed={fixed}"
            );
        }
        // The icon sits between the left edge and the title, and the two
        // do not overlap.
        assert!(BUTTON_GAP + APP_ICON <= title_x);
    }

    #[test]
    fn a_frame_too_narrow_for_the_icon_drops_it_rather_than_overlapping() {
        // A window can be dragged to a 64 × 32 content box, so the frame
        // can be 66 logical pixels wide — narrower than three buttons and
        // their gaps. The icon's box starts at 8 and the leftmost button
        // at 0, so a layout that placed the icon unconditionally would
        // composite the artwork and a glyph on top of each other.
        //
        // Overlap is the worse failure and this tree has said so before:
        // #3709 traded a clipping bug for an overlap bug and the review
        // called it a regression, because clipping ends a thing early
        // where overlap writes one thing through another. The rule here
        // is the same, in the one direction a *frame* can be squeezed.
        let narrow = MIN_CONTENT.w + 2.0 * BORDER;
        for fixed in [false, true] {
            let frame = Rect::new(0.0, 0.0, narrow, 200.0);
            let start = buttons_start(frame, fixed);
            assert!(
                BUTTON_GAP + APP_ICON + APP_ICON_GAP > start,
                "fixed={fixed}: this frame is not actually too narrow, so \
                 the test is measuring nothing"
            );
            // Every button is still inside the frame it belongs to — the
            // buttons are what a narrow frame keeps.
            for (region, rect) in buttons(frame, fixed) {
                assert!(
                    rect.x >= 0.0 && rect.x + rect.w <= narrow,
                    "{region:?} left the frame at width {narrow}"
                );
            }
        }
        // And a frame with room keeps its icon, so the rule is a
        // threshold rather than "never draw one".
        let roomy = Rect::new(0.0, 0.0, 300.0, 200.0);
        assert!(BUTTON_GAP + APP_ICON + APP_ICON_GAP <= buttons_start(roomy, false));
    }

    #[test]
    fn a_hovered_button_takes_a_role_and_close_is_the_only_red_one() {
        // The red moved from "what a close button looks like" to "what it
        // looks like when a click would close the window", which is the
        // whole visual argument of #3715 — so it has to still be there on
        // hover and nowhere else.
        let p = Palette::light();
        assert_eq!(button_hover_role(Region::Close), Role::TitleClose);
        assert_eq!(button_hover_role(Region::Maximize), Role::TitleButtonHover);
        assert_eq!(button_hover_role(Region::Minimize), Role::TitleButtonHover);
        // A hover disc nobody can see is no affordance: the shared role
        // has to differ from both title bars, which is why it is its own
        // role rather than `button_hover` (two units apart from
        // `title_bar_active` in the light scheme).
        for scheme in [Palette::light(), Palette::dark()] {
            let disc = scheme.get(Role::TitleButtonHover);
            assert_ne!(disc, scheme.get(Role::TitleBarActive));
            assert_ne!(disc, scheme.get(Role::TitleBarInactive));
        }
        // And the glyph on the red disc is the role that exists for ink
        // on a saturated field, not whichever title colour happens to
        // read against it.
        assert_eq!(button_hover_text_role(Region::Close), Role::TextOnAccent);
        assert_ne!(
            p.get(button_hover_text_role(Region::Close)),
            p.get(Role::TitleClose)
        );
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
    fn minimizing_demotes_a_window_without_losing_it() {
        let mut wm = WindowManager::new();
        wm.add(key(1));
        wm.add(key(2));
        wm.touch(key(2));
        assert_eq!(wm.mru(), [key(2), key(1)]);
        // Put the front window away: it stays reachable, at the back.
        wm.demote(key(2));
        assert_eq!(wm.mru(), [key(1), key(2)]);
        // One Alt+Tab from the window that inherited the focus reaches it
        // again, which is the behaviour a user expects and the whole
        // reason `demote` exists rather than leaving it at the front.
        let all = wm.mru().to_vec();
        assert_eq!(wm.cycle_next(&all, true), Some(key(2)));
        // Demoting something that is not in the list is a no-op.
        wm.demote(key(9));
        assert_eq!(wm.mru(), [key(1), key(2)]);
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
    fn the_border_shows_the_resize_hint_over_its_focus_colour() {
        // The hint is a *hover* state, so it has to win over both focus
        // colours: a user hovering an unfocused window's edge is told the
        // same thing as one hovering the focused window's.
        let p = Palette::light();
        assert_eq!(
            border_color(true, false, &p),
            p.get(Role::WindowBorderActive)
        );
        assert_eq!(
            border_color(false, false, &p),
            p.get(Role::WindowBorderInactive)
        );
        for focused in [true, false] {
            assert_eq!(border_color(focused, true, &p), p.get(Role::ResizeHint));
        }
        // And it is a colour of its own, not one of the two it replaces —
        // a hint indistinguishable from the resting border is no hint.
        assert_ne!(p.get(Role::ResizeHint), p.get(Role::WindowBorderActive));
        assert_ne!(p.get(Role::ResizeHint), p.get(Role::WindowBorderInactive));
    }

    #[test]
    fn the_band_a_hint_advertises_is_the_band_that_grabs() {
        // The hint lights the *border*, so pressing on the border must
        // resize: an affordance drawn somewhere a press does nothing is
        // worse than none, and #3713 is a user finding exactly that.
        let f = frame();
        let i = frame_insets();
        let on_border = Point::new(f.x + i.left / 2.0, f.y + f.h / 2.0);
        assert!(
            matches!(hit_frame(f, i, on_border, false), Some(Region::Resize(_))),
            "the lit border is inside the band"
        );
        // And a fixed-size window, which never lights up, has no band
        // there either — the two rules agree.
        assert_eq!(hit_frame(f, i, on_border, true), Some(Region::Content));
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
