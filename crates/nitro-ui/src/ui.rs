//! [`Ui`]: the widget tree, the five passes, event routing and the
//! mapping to scene nodes.
//!
//! # Take-out dispatch
//!
//! Calling a widget's method needs `&mut` to the widget *and* `&mut` to
//! the tree, which Rust will not give you at once. Rather than reach for
//! `RefCell`, the widget is **moved out of its arena slot** for the
//! duration of the call and put back afterwards. That is what makes the
//! callback signature `Fn(&mut S, &mut Ui<S>)` possible: inside a
//! button's `on_click` the whole tree is mutable, including the button's
//! siblings, its parent, and widgets created on the spot. The one thing
//! it cannot reach is *itself*, and asking for it is
//! [`Error::Busy`](crate::Error::Busy) — a value, not a panic.
//!
//! # Nodes
//!
//! Every widget owns one scene `Group`, positioned by its bounds, and —
//! once it has children — an inner content group holding their groups.
//! Its own painted nodes are created *before* the content group, so a
//! panel's background is under its children whatever order the passes ran
//! in. Moving a widget is therefore one `SetBounds` on its group and no
//! repaint of anything inside it.

use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, Instant};

use nitro_core::{Point, Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ButtonState, ErrorCode, NodeId};

use crate::arena::{Arena, Dirty, WidgetId};
use crate::build::IntoWidget;
use crate::error::Error;
use crate::event::{Event, Handled, KeyEvent, key, mods};
use crate::layout::{Constraints, FlexItem, LayoutStyle};
use crate::theme::{TextStyle, Theme};
use crate::widget::{AnyWidget, EventCx, LayoutCx, MeasureCx, PaintCx, Widget};
use crate::wire::{Mutation, TextMetrics, Wire};

/// The one window's root node id. Client-allocated, so it is simply the
/// first id; [`Wire`] hands out everything from 2 up.
pub(crate) const WINDOW: NodeId = NodeId(1);

/// How many times [`Ui::run_deferred`] drains a queue that keeps
/// refilling itself before giving up.
///
/// A deferred callback may legitimately defer again (a navigation that
/// triggers a re-listing), so one pass is not enough; a callback that
/// queues itself unconditionally is a bug, and an unbounded loop would
/// turn that bug into a hang with no output. Sixteen is far more than
/// any real chain and far less than forever.
pub const MAX_DEFER_ROUNDS: usize = 16;

/// One widget as the introspection tree sees it.
///
/// Produced by [`Ui::introspect`]. Everything an outside process needs
/// to name, read, locate and act on a widget, and nothing that depends
/// on the widget's concrete type — which is what lets one mechanism
/// serve accessibility, scripting and agents alike.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The widget's stable id.
    pub id: WidgetId,
    /// What it is.
    pub role: crate::widget::Role,
    /// Its name, value and actions.
    pub access: crate::widget::Access,
    /// Where it is, in window coordinates.
    pub bounds: Rect,
    /// Whether Tab can reach it.
    pub focusable: bool,
    /// Whether it currently has the keyboard focus.
    pub focused: bool,
    /// Whether it is on screen: its own flag and every ancestor's, see
    /// [`Ui::is_visible`]. A hidden widget keeps its bounds, so a hidden
    /// page is still listed and still resolves by name.
    pub visible: bool,
    /// Its children, in paint order.
    pub children: Vec<WidgetId>,
}

/// A handle to a descriptor hook registered with [`Ui::add_fd`].
///
/// A token is an **opaque id that is never reused**, not the descriptor
/// number, and that distinction cost a box run to find.
///
/// The obvious implementation is the raw fd of our duplicate: it is
/// already unique among live hooks and it is already what the app
/// loop's `epoll` set is keyed on, so the two cannot drift. What it is
/// not is unique over *time*. Closing a descriptor returns its number
/// to the kernel's free list, and the kernel hands out the lowest free
/// number — so a hook removed and another added in the same turn get
/// the **same number**, which is exactly what re-arming looks like:
/// `nitro-files` moves its inotify watch to the directory it just
/// navigated to by dropping one hook and adding the next.
///
/// The app loop keeps a list of what it has registered so it does not
/// `epoll_ctl` on every wakeup. Keyed on the number, that list said
/// "already registered" about a descriptor that had been closed (and so
/// silently removed from the set) and replaced. The result was a hook
/// that existed in the `Ui`, was never in the `epoll` set, and
/// therefore never fired: on the box, the file list refreshed itself in
/// the first directory and in no directory afterwards, with nothing
/// anywhere returning an error.
///
/// A monotonic `u64` cannot do that. `Ui::hook_fds` hands the loop the
/// id *and* the descriptor, so the set is still keyed on something the
/// two agree about, and a re-armed hook is a new id and a new
/// registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FdToken(u64);

impl FdToken {
    /// The raw id, as the app loop's `epoll` token.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Rebuild a token from an `epoll` token.
    #[must_use]
    pub fn from_raw(id: u64) -> Self {
        Self(id)
    }
}

/// A handle to a timer registered with [`Ui::set_timer`], for
/// [`Ui::cancel_timer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerId(u64);

/// The widget tree and everything the passes need.
///
/// One `Ui` owns one window. It is generic over the app's state type `S`,
/// which is what callbacks are handed alongside the tree itself.
#[allow(clippy::struct_excessive_bools)] // Independent facts about one window, not a state machine: `quit`, `window_open`, `backdrop_wanted`, `root_clipped` and `frame_requested` have no shared vocabulary to collapse into.
pub struct Ui<S> {
    arena: Arena<S>,
    root: Option<WidgetId>,
    theme: Theme,
    /// The colours the server pushed, which `theme` is a view on.
    ///
    /// Held whole rather than only as the `Theme` projection, because a
    /// custom widget — the terminal grid, the bar's clock — reads roles
    /// the built-in widgets have no field for.
    palette: nitro_core::Palette,
    wire: Wire,
    window_open: bool,
    window_size: Size,
    /// Where the server put the window on its output, from the last
    /// `Configure`. The introspection socket's `shot` needs it to crop
    /// an output screenshot down to this window.
    window_position: Point,
    scale: f32,
    /// The window's background: a `Rect` node under everything, filled
    /// with the theme's `background`. `None` for a transparent window
    /// ([`App::transparent`](crate::App::transparent)).
    ///
    /// The root widget paints nothing by default, so without this a
    /// dialog's dark text lands on whatever the desktop is showing.
    backdrop: Option<NodeId>,
    backdrop_wanted: bool,
    /// Whether the root's group has been told to clip to the window.
    ///
    /// One `SetClip`, remembered: the rectangle it clips to is the root
    /// widget's own bounds, which the layout pass keeps equal to the
    /// window on every `Configure`, so a resize needs no second message.
    /// See [`Ui::pass_clip`].
    root_clipped: bool,
    /// The colour and size the backdrop was last sent, so a resize or a
    /// theme change costs one mutation and an idle tree costs none.
    backdrop_sent: Option<(Size, nitro_core::Color)>,
    /// Override for the server control socket `shot` talks to.
    control_path: Option<std::path::PathBuf>,
    focused: Option<WidgetId>,
    /// Focus changes waiting to be reported, oldest first.
    ///
    /// [`Ui::focus`] can be called from inside a widget's own `event`,
    /// where that widget is out of its slot and the app state is already
    /// borrowed — so the notification is queued and delivered by
    /// [`Ui::deliver_focus_events`] once the batch is done.
    pending_focus: Vec<(WidgetId, bool)>,
    /// Callbacks queued with [`Ui::defer`], oldest first.
    ///
    /// The sibling of `pending_focus`, and it exists for the identical
    /// reason one step further out. Dispatch **takes a widget out of its
    /// arena slot** for the duration of its own callback, so inside that
    /// callback `ui.widget_mut(that_id)` is [`Error::Busy`] — which is
    /// exactly what a list's `on_activate` wants to do when activating a
    /// row means "show a different directory in this list". Queuing the
    /// work and running it once every widget is back in place is the
    /// same answer `focus` already takes for the same reason.
    pending_deferred: Vec<OnceCallback<S>>,
    hover_chain: Vec<WidgetId>,
    /// Widgets activated since the last drain, oldest first.
    ///
    /// A value change is visible by diffing the introspection tree, but
    /// an *activation* leaves no trace in it: a button that runs a
    /// callback looks exactly like a button that did not. Widgets whose
    /// whole purpose is to be activated therefore say so, and
    /// [`Ui::take_activations`] is where a watcher collects them.
    activations: Vec<WidgetId>,
    quit: bool,
    /// The last fatal [`ServerMsg::Error`] the server sent.
    ///
    /// `Server::disconnect` sends the error and *then* closes the socket,
    /// so the reason and the EOF arrive as two separate events; without
    /// this the app read the explanation, dropped it, and exited 0 on the
    /// EOF that followed.
    last_server_error: Option<nitro_wire::msg::Error>,
    // Scratch pools. Layout recurses, so one vector is not enough; these
    // are stacks of reusable ones, which is what keeps a flush free of
    // per-widget allocation once the tree has settled.
    id_pool: Vec<Vec<WidgetId>>,
    item_pool: Vec<Vec<FlexItem>>,
    rect_pool: Vec<Vec<Rect>>,
    chain: Vec<(WidgetId, Point)>,
    fds: Vec<FdHook<S>>,
    /// Next never-used descriptor-hook id. Monotonic, because a
    /// descriptor *number* is recycled the moment it is closed; see
    /// [`FdToken`].
    next_fd_token: u64,
    timers: Vec<Timer<S>>,
    next_timer: u64,
    /// App-level key handlers, in registration order; see [`Ui::on_key`].
    ///
    /// `Option` for the same reason a widget leaves its arena slot: a
    /// handler is handed `&mut Ui<S>`, so it must not be reachable
    /// through the tree it is holding.
    key_handlers: Vec<Option<KeyHandler<S>>>,
    /// What kind of window to open; `None` for an ordinary app.
    ///
    /// Held rather than applied immediately because the surface is
    /// described before the window exists: [`Ui::open_window`] turns it
    /// into the layer, the flags, the anchor and the zone of one commit.
    surface: Option<crate::shell::Surface>,
    /// The application id, sent with the window; see [`Ui::set_app_id`].
    app_id: String,
    /// Shell-event handlers, in registration order; see [`Ui::on_shell`].
    ///
    /// `Option` for the same reason a widget leaves its arena slot: a
    /// handler is handed `&mut Ui<S>`, so it must not be reachable
    /// through the tree it is holding.
    shell_handlers: Vec<Option<ShellHandler<S>>>,
    /// Frame-callback handlers, in registration order; see
    /// [`Ui::on_frame`].
    ///
    /// `Option` for the same reason a widget leaves its arena slot: a
    /// handler is handed `&mut Ui<S>`, so it must not be reachable
    /// through the tree it is holding.
    frame_handlers: Vec<Option<FrameHandler<S>>>,
    /// Window-resize handlers, in registration order; see
    /// [`Ui::on_resize`].
    ///
    /// `Option` for the same reason a widget leaves its arena slot: a
    /// handler is handed `&mut Ui<S>`, so it must not be reachable
    /// through the tree it is holding.
    resize_handlers: Vec<Option<ResizeHandler<S>>>,
    /// Palette-change handlers, in registration order; see
    /// [`Ui::on_theme`]. `Option` for the same reason the others are.
    theme_handlers: Vec<Option<ThemeHandler<S>>>,
    /// Whether a `RequestFrame` is outstanding, so asking twice in one
    /// turn does not put two requests on the wire.
    frame_requested: bool,
    /// The window title last sent, so an unchanged title costs nothing.
    /// A terminal re-sends one per OSC and a shell prompt that carries
    /// one sends the same string on every line.
    window_title: String,
    /// The size limits last sent; `None` until something set them.
    window_limits: Option<(Size, Size)>,
}

/// A shell-event handler; see [`Ui::on_shell`].
type ShellHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, &crate::shell::ShellEvent)>;

/// A frame-callback handler; see [`Ui::on_frame`].
type FrameHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, Frame)>;

/// A window-resize handler; see [`Ui::on_resize`].
type ResizeHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, Size)>;

/// A palette-change handler; see [`Ui::on_theme`].
type ThemeHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>)>;

/// What the server says when it answers a [`Ui::request_frame`].
///
/// The deadline is the target presentation time of the next flip, on
/// `CLOCK_MONOTONIC`, and `refresh_ns` is the output's refresh interval.
/// An app that produces output faster than the screen can show it uses
/// them to pace work it would otherwise repeat per input event —
/// rebuilding a list, recomputing a model — rather than as the gate on
/// painting at all; see [`Ui::request_frame`] for why that distinction
/// is load-bearing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// Target presentation time, `CLOCK_MONOTONIC` nanoseconds.
    pub deadline_ns: u64,
    /// The output's refresh interval in nanoseconds.
    pub refresh_ns: u32,
}

/// An app callback: it is handed the state and the whole tree, exactly
/// like a widget's own.
type Callback<S> = Box<dyn FnMut(&mut S, &mut Ui<S>)>;

/// A one-shot app callback, for timers.
type OnceCallback<S> = Box<dyn FnOnce(&mut S, &mut Ui<S>)>;

/// An app-level key handler; see [`Ui::on_key`].
type KeyHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, &KeyEvent) -> Handled>;

struct FdHook<S> {
    /// The hook's id: unique for the life of the process, so a
    /// descriptor number recycled by the kernel cannot make a new hook
    /// look like an old one. See [`FdToken`].
    id: u64,
    /// A `dup` of the descriptor the app handed us. Owning a duplicate
    /// is what lets the loop re-borrow it for `epoll` without `unsafe`
    /// and without the app promising anything about lifetimes.
    fd: std::os::fd::OwnedFd,
    callback: Option<Callback<S>>,
}

struct Timer<S> {
    id: u64,
    deadline: Instant,
    callback: Option<OnceCallback<S>>,
}

impl<S: 'static> Ui<S> {
    /// A tree on an open connection, with no window and no root yet.
    #[must_use]
    pub fn new(conn: Connection, theme: Theme) -> Self {
        Self {
            arena: Arena::default(),
            root: None,
            theme,
            palette: nitro_core::Palette::default(),
            wire: Wire::new(conn),
            window_open: false,
            window_size: Size::ZERO,
            window_position: Point::ZERO,
            scale: 1.0,
            backdrop: None,
            backdrop_wanted: true,
            root_clipped: false,
            backdrop_sent: None,
            control_path: None,
            focused: None,
            pending_focus: Vec::new(),
            pending_deferred: Vec::new(),
            hover_chain: Vec::new(),
            activations: Vec::new(),
            quit: false,
            last_server_error: None,
            id_pool: Vec::new(),
            item_pool: Vec::new(),
            rect_pool: Vec::new(),
            chain: Vec::new(),
            fds: Vec::new(),
            next_fd_token: 1,
            timers: Vec::new(),
            next_timer: 1,
            key_handlers: Vec::new(),
            surface: None,
            app_id: String::new(),
            shell_handlers: Vec::new(),
            frame_handlers: Vec::new(),
            resize_handlers: Vec::new(),
            theme_handlers: Vec::new(),
            frame_requested: false,
            window_title: String::new(),
            window_limits: None,
        }
    }

    // -- tree construction --------------------------------------------

    /// Materialise a builder tree into the arena, returning the root of
    /// what was built.
    pub fn build(&mut self, b: impl IntoWidget<S>) -> WidgetId {
        let built = b.into_widget();
        self.insert_built(built, None)
    }

    fn insert_built(
        &mut self,
        built: crate::build::Built<S>,
        parent: Option<WidgetId>,
    ) -> WidgetId {
        let crate::build::Built {
            widget,
            mut state,
            children,
        } = built;
        state.parent = parent;
        let id = self.arena.insert_boxed(widget, state);
        for c in children {
            let cid = self.insert_built(c, Some(id));
            if let Some(slot) = self.arena.slot_mut(id) {
                slot.state.children.push(cid);
            }
        }
        id
    }

    /// Add `child` to `parent`'s children, at the end.
    ///
    /// # Errors
    /// [`Error::StaleWidget`] if either id is dead.
    pub fn add_child(
        &mut self,
        parent: WidgetId,
        child: impl IntoWidget<S>,
    ) -> Result<WidgetId, Error> {
        if !self.arena.is_live(parent) {
            return Err(Error::StaleWidget);
        }
        let built = child.into_widget();
        let id = self.insert_built(built, Some(parent));
        if let Some(slot) = self.arena.slot_mut(parent) {
            slot.state.children.push(id);
        }
        self.mark(parent, Dirty::TREE | Dirty::LAYOUT);
        // The new subtree's own slots are born dirty, but a flag with no
        // `SUB_` trail above it is invisible to the passes: on a settled
        // tree `pass_paint` stops at the clean root and the child is
        // never painted. Marking it lights the trail.
        self.mark(id, Dirty::TREE | Dirty::LAYOUT | Dirty::PAINT);
        Ok(id)
    }

    /// Move an already-built widget under `parent`, at the end.
    ///
    /// The idiom `let id = ui.build(..); ui.attach(parent, id)?` is how a
    /// builder closure gets an id it can capture *before* the parent that
    /// holds the widget exists.
    ///
    /// # Errors
    /// [`Error::StaleWidget`] if either id is dead.
    pub fn attach(&mut self, parent: WidgetId, child: WidgetId) -> Result<(), Error> {
        if !self.arena.is_live(parent) || !self.arena.is_live(child) {
            return Err(Error::StaleWidget);
        }
        if let Some(old) = self.parent(child)
            && let Some(slot) = self.arena.slot_mut(old)
        {
            slot.state.children.retain(|c| *c != child);
            self.mark(old, Dirty::TREE | Dirty::LAYOUT);
        }
        if let Some(slot) = self.arena.slot_mut(child) {
            slot.state.parent = Some(parent);
        }
        if let Some(slot) = self.arena.slot_mut(parent) {
            slot.state.children.push(child);
        }
        self.mark(parent, Dirty::TREE | Dirty::LAYOUT);
        self.mark(child, Dirty::TREE | Dirty::LAYOUT | Dirty::PAINT);
        Ok(())
    }

    /// Destroy a widget and its subtree, releasing its scene nodes.
    ///
    /// # Errors
    /// [`Error::StaleWidget`] if the id is dead, [`Error::Wire`] if the
    /// connection failed.
    pub fn remove(&mut self, id: WidgetId) -> Result<(), Error> {
        if !self.arena.is_live(id) {
            return Err(Error::StaleWidget);
        }
        let parent = self.arena.slot(id).and_then(|s| s.state.parent);
        if let Some(p) = parent
            && let Some(slot) = self.arena.slot_mut(p)
        {
            slot.state.children.retain(|c| *c != id);
            self.mark(p, Dirty::TREE | Dirty::LAYOUT);
        } else if self.root == Some(id) {
            self.root = None;
        }
        // One `DestroyNode` on the subtree's outermost group takes the
        // whole scene subtree with it. The ids underneath it are *not*
        // returned to the free list: the server frees them with the
        // subtree, and reusing one would need us to prove the commit
        // carrying the destroy has been applied. Ids are a monotonic
        // `u32` per client, so leaking them costs nothing a real app
        // would ever reach.
        let node = self.arena.slot(id).and_then(|s| s.state.node);
        let mut doomed = Vec::new();
        self.collect_subtree(id, &mut doomed);
        for d in &doomed {
            if self.arena.slot(*d).is_some_and(|s| s.state.focused) {
                self.focused = None;
            }
            self.arena.remove(*d);
        }
        self.hover_chain.retain(|h| !doomed.contains(h));
        self.pending_focus.retain(|(id, _)| !doomed.contains(id));
        if let Some(n) = node {
            self.wire.destroy_node(n)?;
        }
        Ok(())
    }

    fn collect_subtree(&self, id: WidgetId, out: &mut Vec<WidgetId>) {
        out.push(id);
        if let Some(slot) = self.arena.slot(id) {
            for c in slot.state.children.clone() {
                self.collect_subtree(c, out);
            }
        }
    }

    /// Make `id` the tree's root.
    ///
    /// # Errors
    /// [`Error::StaleWidget`] if the id is dead.
    pub fn set_root(&mut self, id: WidgetId) -> Result<(), Error> {
        if !self.arena.is_live(id) {
            return Err(Error::StaleWidget);
        }
        self.root = Some(id);
        // A new root is a new group, and the clip rides the group.
        self.root_clipped = false;
        self.mark(id, Dirty::LAYOUT | Dirty::PAINT | Dirty::TREE);
        Ok(())
    }

    /// The root widget, if one is set.
    #[must_use]
    pub fn root(&self) -> Option<WidgetId> {
        self.root
    }

    // -- accessors ----------------------------------------------------

    /// The theme every widget paints from.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Replace the theme and repaint everything.
    ///
    /// Apps do not normally call this: colours come from the server (see
    /// [`Ui::set_palette`]). It stays for tests and for the app that
    /// really does want its own metrics.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        // The backdrop is not a widget, so no widget's repaint covers it.
        self.backdrop_sent = None;
        let ids: Vec<WidgetId> = self.all_ids();
        for id in ids {
            self.mark(id, Dirty::LAYOUT | Dirty::PAINT);
        }
    }

    /// The desktop's colours, as the server last pushed them.
    #[must_use]
    pub fn palette(&self) -> &nitro_core::Palette {
        &self.palette
    }

    /// The colour of one [`Role`](nitro_core::Role).
    ///
    /// What a custom widget uses instead of writing a colour down: the
    /// terminal grid asks for `Role::Ansi1`, the bar for `Role::TextDim`.
    /// A role that does not exist yet is added to `nitro_core::palette`,
    /// not worked around — see `docs/theme.md`.
    #[must_use]
    pub fn color(&self, role: nitro_core::Role) -> nitro_core::Color {
        self.palette.get(role)
    }

    /// Adopt a palette the server pushed: re-derive the theme from it,
    /// keeping this app's own metrics, and mark everything for repaint.
    ///
    /// Returns whether the palette actually moved.
    ///
    /// The repaint is exactly one commit, because marking is not
    /// sending: every widget is flagged here and the next
    /// [`Ui::flush`] turns the whole lot into one transaction. An
    /// unchanged palette is dropped without marking anything, so a
    /// server that re-sends its palette costs a settled app nothing —
    /// and the `bool` is what lets [`Ui::dispatch`] hold the same
    /// promise for an app with an [`Ui::on_theme`] handler, whose hook
    /// may legitimately do expensive work (`nitro-term`'s damages its
    /// whole grid).
    pub fn set_palette(&mut self, palette: nitro_core::Palette) -> bool {
        if palette == self.palette {
            return false;
        }
        self.palette = palette;
        let theme = self.theme.with_palette(&self.palette);
        self.set_theme(theme);
        true
    }

    /// A widget's children, in paint order.
    #[must_use]
    pub fn children(&self, id: WidgetId) -> Vec<WidgetId> {
        self.arena
            .slot(id)
            .map(|s| s.state.children.clone())
            .unwrap_or_default()
    }

    /// A widget's parent.
    #[must_use]
    pub fn parent(&self, id: WidgetId) -> Option<WidgetId> {
        self.arena.slot(id).and_then(|s| s.state.parent)
    }

    /// A widget's bounds, in its parent's coordinate space.
    #[must_use]
    pub fn bounds(&self, id: WidgetId) -> Rect {
        self.arena.slot(id).map_or(Rect::EMPTY, |s| s.state.bounds)
    }

    /// A widget's bounds in window coordinates.
    ///
    /// Each ancestor contributes its origin **and its content
    /// transform**: a widget inside a scrolled [`Scroll`](crate::widgets::Scroll)
    /// has not moved in its parent's coordinate space — that is what
    /// makes scrolling one mutation — but it has moved on screen, and
    /// this is the rectangle an outside process points at.
    #[must_use]
    pub fn window_bounds(&self, id: WidgetId) -> Rect {
        let mut r = self.bounds(id);
        let mut cur = self.parent(id);
        while let Some(p) = cur {
            let b = self.bounds(p);
            let t = self.content_transform(p);
            r = r.translate(b.x + t.e, b.y + t.f);
            cur = self.parent(p);
        }
        r
    }

    /// The number of live widgets.
    #[must_use]
    pub fn widget_count(&self) -> usize {
        self.arena.len()
    }

    /// Whether the server can draw text at all (the `TEXT` capability).
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.wire.has_text()
    }

    /// Whether the server has the symbolic icon set (the `ICONS`
    /// capability).
    ///
    /// The same shape as [`Ui::has_text`]: without it an
    /// [`icon`](crate::widgets::icon) widget still *measures* its square
    /// box, so the layout is identical, and paints nothing. A gap, never
    /// a broken tree.
    #[must_use]
    pub fn has_icons(&self) -> bool {
        self.wire.has_icons()
    }

    /// Whether this app is talking to the server over a **remote** link
    /// (the `REMOTE` capability).
    ///
    /// What it costs is one thing: images. A buffer is passed as a file
    /// descriptor and a descriptor cannot cross TCP, so
    /// [`PaintCx::upload_image`](crate::widget::PaintCx::upload_image)
    /// returns `None` and an `Image` widget draws nothing. Everything
    /// else — rects, text, borders, layout, input — is unchanged, which
    /// is what makes an ordinary app remote-capable without knowing it.
    /// An app that *is* its pixels (the wallpaper) should check this and
    /// say so rather than come up blank; see `docs/remote.md`.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.wire.is_remote()
    }

    /// The window's current size in logical pixels.
    #[must_use]
    pub fn window_size(&self) -> Size {
        self.window_size
    }

    /// The output scale reported by the last `Configure`.
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Where the server placed this window on its output, in logical
    /// pixels, from the last `Configure`.
    ///
    /// The introspection socket's `shot` needs it: the server screenshots
    /// a whole output, and this is what crops it to one window.
    #[must_use]
    pub fn window_position(&self) -> Point {
        self.window_position
    }

    /// Where this window's introspection `shot` asks for pixels.
    ///
    /// Normally the server's control socket as the environment names it;
    /// the test harness overrides it, because its server is one of
    /// several in the same process tree and `$NITRO_CONTROL` is
    /// process-wide state a test must not fight over.
    #[must_use]
    pub fn control_path(&self) -> std::path::PathBuf {
        self.control_path
            .clone()
            .unwrap_or_else(crate::shot::control_path)
    }

    /// Point `shot` at a specific control socket.
    pub fn set_control_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.control_path = Some(path.into());
    }

    /// Whether the window paints the theme's background behind the tree.
    #[must_use]
    pub fn has_backdrop(&self) -> bool {
        self.backdrop_wanted
    }

    /// Paint (or stop painting) the theme's `background` colour behind
    /// the whole tree.
    ///
    /// On by default: the root widget paints nothing of its own, so a
    /// transparent window puts the theme's dark text straight onto the
    /// desktop, where it is barely readable. An app that wants the
    /// desktop to show through turns it off — see
    /// [`App::transparent`](crate::App::transparent).
    pub fn set_backdrop(&mut self, on: bool) {
        self.backdrop_wanted = on;
    }

    /// Whether [`Ui::quit`] has been called.
    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The last fatal error the server reported, if any.
    ///
    /// Latched by [`Ui::dispatch`] and taken by [`Ui::pump`], which turns
    /// it into the loop's return error so a client killed for a protocol
    /// violation says why instead of exiting 0.
    #[must_use]
    pub fn last_server_error(&self) -> Option<&nitro_wire::msg::Error> {
        self.last_server_error.as_ref()
    }

    /// Ask the app loop to stop after this event batch.
    pub fn quit(&mut self) {
        self.quit = true;
    }

    /// The connection's descriptor, for `epoll`.
    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.wire.conn().as_fd()
    }

    pub(crate) fn wire_mut(&mut self) -> &mut Wire {
        &mut self.wire
    }

    /// Commits sent since the tree was created.
    #[must_use]
    pub fn commit_count(&self) -> u32 {
        self.wire.commits
    }

    /// Record every mutation sent from now on (a test facility; see
    /// [`Ui::mutations`]).
    pub fn tap(&mut self, on: bool) {
        self.wire.set_tap(on);
    }

    /// Mutations recorded since the tap was turned on or last cleared.
    #[must_use]
    pub fn mutations(&self) -> &[Mutation] {
        self.wire.taped()
    }

    /// Every byte this tree has written to the server since [`Ui::tap`]
    /// turned the tap on: every message, measure requests included.
    ///
    /// What a test searches when the claim is "this string never left
    /// the process" (a secret [`TextField`](crate::widgets::TextField)).
    #[must_use]
    pub fn sent_bytes(&self) -> &[u8] {
        self.wire.sent_bytes()
    }

    /// Forget the recorded mutations, leaving the tap on.
    pub fn clear_mutations(&mut self) {
        self.wire.clear_tap();
    }

    /// Answer [`Ui::has_icons`] with `false` however the server answered
    /// (a test facility, like [`Ui::tap`]).
    ///
    /// It exists because the branch it reaches is the one a widget takes
    /// against a server **older than the icon set**, and such a server
    /// cannot be started from this tree. Masking the bit on a real
    /// connection runs the real code path with exactly one variable
    /// changed; a hand-built fake `Ui` would prove only that the fake
    /// works. What is under test is not cosmetic: a widget that emits an
    /// icon node anyway sends `CreateNode { kind: Icon }`, which an old
    /// server rejects as a decode error and **closes the connection** on.
    pub fn hide_icons(&mut self, on: bool) {
        self.wire.set_hide_icons(on);
    }

    /// Measure a string with the server's fonts.
    ///
    /// `max_width` of `0.0` means "no limit". The result is cached by
    /// `(text, style, max_width)`; see
    /// [`Wire::measure_text`](crate::wire) for why this is a synchronous
    /// round trip in M2.
    ///
    /// # Errors
    /// If the connection failed.
    pub fn measure_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: f32,
    ) -> Result<TextMetrics, Error> {
        self.wire.measure_text(text, style, max_width)
    }

    // -- typed access -------------------------------------------------

    /// Borrow a widget by type.
    ///
    /// # Errors
    /// [`Error::StaleWidget`] for a dead id, [`Error::Busy`] if the
    /// widget is currently running one of its own methods, and
    /// [`Error::WrongType`] if it is not a `W`.
    pub fn widget<W: Widget<S>>(&self, id: WidgetId) -> Result<&W, Error> {
        let slot = self.arena.slot(id).ok_or(Error::StaleWidget)?;
        let w = slot.widget.as_ref().ok_or(Error::Busy)?;
        w.as_any().downcast_ref::<W>().ok_or(Error::WrongType {
            expected: std::any::type_name::<W>(),
        })
    }

    /// Mutate a widget through a [`WidgetMut`], which is the only way its
    /// properties can change.
    ///
    /// # Errors
    /// As [`Ui::widget`].
    pub fn widget_mut<W: Widget<S>>(&mut self, id: WidgetId) -> Result<WidgetMut<'_, W, S>, Error> {
        {
            let slot = self.arena.slot(id).ok_or(Error::StaleWidget)?;
            let w = slot.widget.as_ref().ok_or(Error::Busy)?;
            if w.as_any().downcast_ref::<W>().is_none() {
                return Err(Error::WrongType {
                    expected: std::any::type_name::<W>(),
                });
            }
        }
        let widget = self.take(id)?;
        Ok(WidgetMut {
            ui: self,
            id,
            widget: Some(widget),
            _marker: std::marker::PhantomData,
        })
    }

    /// Where every cursor position inside `text` sits, in logical pixels
    /// from its left edge: `(byte offset, x)` in increasing order.
    ///
    /// A text field has to place a caret and it cannot compute this
    /// itself — the client has no fonts. It rides the same round trip
    /// and the same cache as [`Ui::measure_text`].
    ///
    /// # Errors
    /// If the connection failed.
    pub fn cursor_positions(
        &mut self,
        text: &str,
        style: &TextStyle,
    ) -> Result<Vec<(u32, f32)>, Error> {
        self.wire.cursor_positions(text, style)
    }

    /// Release a server-side buffer.
    ///
    /// Ids are monotonic and never recycled, so a released id cannot
    /// come back naming something else.
    pub fn release_buffer(&mut self, buffer: nitro_wire::types::BufferId) {
        let _ = self.wire.destroy_buffer(buffer);
    }

    /// Take the widgets activated since the last call.
    ///
    /// Recorded by [`EventCx::report_activation`](crate::EventCx::report_activation),
    /// which a widget calls when it does the thing it exists to do. The
    /// introspection socket's `watch` drains this to emit `click`
    /// events; an app with no watcher drains it too, so the list cannot
    /// grow without bound.
    pub fn take_activations(&mut self, out: &mut Vec<WidgetId>) {
        out.clear();
        out.append(&mut self.activations);
    }

    /// Record that `id` was activated. See [`Ui::take_activations`].
    pub(crate) fn report_activation(&mut self, id: WidgetId) {
        // Bounded: a burst of clicks between two drains is a handful of
        // ids, and a drain happens every loop turn.
        if self.activations.len() < 64 {
            self.activations.push(id);
        }
    }

    /// The widget's role, for introspection.
    ///
    /// # Errors
    /// As [`Ui::widget`], minus the type check.
    pub fn role(&self, id: WidgetId) -> Result<crate::widget::Role, Error> {
        let slot = self.arena.slot(id).ok_or(Error::StaleWidget)?;
        Ok(slot.widget.as_ref().ok_or(Error::Busy)?.role())
    }

    /// The widget's **addressing** name: the one `.name("ok")` set, and
    /// only that.
    ///
    /// Deliberately not the accessible name. A label's accessible name
    /// is its text — a sentence — and a path built out of it would move
    /// every time the text did. The two are separate fields so
    /// `window/message` stays `window/message`.
    #[must_use]
    pub fn address_name(&self, id: WidgetId) -> Option<String> {
        self.arena.slot(id).and_then(|s| s.state.name.clone())
    }

    /// Set the widget's addressing name after it was built — what
    /// `.name(..)` does on a builder. A builder that composes widgets an
    /// app built earlier (`split_view().content_id(..)`) uses it to give
    /// the slot its default name.
    pub fn set_address_name(&mut self, id: WidgetId, name: impl Into<String>) {
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.name = Some(name.into());
        }
    }

    /// The widget's accessible name: what it calls itself, falling back
    /// to its addressing name.
    #[must_use]
    pub fn name(&self, id: WidgetId) -> Option<String> {
        let slot = self.arena.slot(id)?;
        if let Some(n) = slot.widget.as_ref()?.accessible().name {
            return Some(n);
        }
        slot.state.name.clone()
    }

    /// Whether the widget reacts to input; see
    /// [`Widget::enabled`](crate::Widget::enabled).
    #[must_use]
    pub fn is_enabled(&self, id: WidgetId) -> bool {
        self.arena
            .slot(id)
            .and_then(|s| s.widget.as_ref())
            .is_some_and(|w| w.enabled())
    }

    /// Invoke a named action on a widget, as the introspection socket's
    /// `do` does.
    ///
    /// It goes through the same take-out dispatch a real event does, so
    /// the widget's callbacks run with the app's own `&mut S` and the
    /// whole tree, and the invalidation is identical.
    ///
    /// # Errors
    /// [`Error::StaleWidget`] for a dead id, [`Error::Busy`] if the
    /// widget is already out of its slot.
    pub fn action(
        &mut self,
        state: &mut S,
        id: WidgetId,
        action: &str,
        arg: Option<&str>,
    ) -> Result<Handled, Error> {
        // Framework-level actions first: they work on any widget, and a
        // widget cannot implement them (focus lives in the tree).
        match action {
            "focus" => {
                self.focus(id);
                self.deliver_focus_events(state);
                return Ok(Handled::Yes);
            }
            "set_name" => {
                if let Some(slot) = self.arena.slot_mut(id) {
                    slot.state.name = arg.map(str::to_owned);
                    return Ok(Handled::Yes);
                }
                return Err(Error::StaleWidget);
            }
            _ => {}
        }
        let bounds = self.arena.slot(id).ok_or(Error::StaleWidget)?.state.bounds;
        let mut widget = self.take(id)?;
        let handled = {
            let mut cx = EventCx {
                ui: self,
                state,
                id,
                bounds,
            };
            widget.action(&mut cx, action, arg)
        };
        self.untake(id, widget);
        self.deliver_focus_events(state);
        self.run_deferred(state);
        Ok(handled)
    }

    /// The widget's accessibility record.
    ///
    /// # Errors
    /// As [`Ui::role`].
    pub fn accessible(&self, id: WidgetId) -> Result<crate::widget::Access, Error> {
        let slot = self.arena.slot(id).ok_or(Error::StaleWidget)?;
        let mut a = slot.widget.as_ref().ok_or(Error::Busy)?.accessible();
        if a.name.is_none() {
            a.name.clone_from(&slot.state.name);
        }
        Ok(a)
    }

    /// One node of the introspection tree: what the next task's socket
    /// serves, and what an AT-SPI bridge reads later.
    ///
    /// The pass itself is a plain walk over the same arena every other
    /// pass uses — [`Ui::introspect`] — because the whole point of goal 5
    /// is that there is *one* tree, not a shadow one maintained
    /// alongside it.
    ///
    /// # Errors
    /// As [`Ui::role`].
    pub fn introspect_node(&self, id: WidgetId) -> Result<Node, Error> {
        let slot = self.arena.slot(id).ok_or(Error::StaleWidget)?;
        let widget = slot.widget.as_ref().ok_or(Error::Busy)?;
        let mut access = widget.accessible();
        if access.name.is_none() {
            access.name.clone_from(&slot.state.name);
        }
        Ok(Node {
            id,
            role: widget.role(),
            access,
            bounds: self.window_bounds(id),
            focusable: slot.state.focusable,
            focused: slot.state.focused,
            visible: self.is_visible(id),
            children: slot.state.children.clone(),
        })
    }

    /// Walk the whole tree in paint order, appending one [`Node`] per
    /// widget.
    ///
    /// This is the `introspect` pass, and it is what the introspection
    /// socket serves (`crate::introspect`, `docs/introspection.md`) and
    /// what an AT-SPI bridge will read. It is a plain walk over the same
    /// arena every other pass uses, because the point of goal 5 is that
    /// there is *one* tree, not a shadow one maintained alongside it. A
    /// widget that is out of its slot (one running a callback that asked
    /// to introspect) is skipped rather than failing the walk.
    pub fn introspect(&self, out: &mut Vec<Node>) {
        out.clear();
        if let Some(root) = self.root {
            self.introspect_into(root, out);
        }
    }

    fn introspect_into(&self, id: WidgetId, out: &mut Vec<Node>) {
        if let Ok(node) = self.introspect_node(id) {
            let children = node.children.clone();
            out.push(node);
            for c in children {
                self.introspect_into(c, out);
            }
        }
    }

    /// Show or hide a widget and everything under it, without touching
    /// its layout.
    ///
    /// This is the page-stack primitive: a widget that is not the
    /// current page keeps its bounds and its scene nodes, but its group
    /// is hidden on the server (one `SetVisible`, sent only when the
    /// value changes), the pointer does not enter it and Tab does not
    /// reach anything inside it. Nothing is re-laid-out or re-painted,
    /// which is what makes a page switch cost a handful of mutations
    /// rather than a rebuild of the page. Named widgets inside a hidden
    /// page still resolve, so `hey` can read and set them from any page.
    pub fn set_node_visible(&mut self, id: WidgetId, visible: bool) {
        let Some(slot) = self.arena.slot_mut(id) else {
            return;
        };
        slot.state.visible = visible;
        let node = slot.state.node;
        if let Some(n) = node
            && slot.state.sent_visible != Some(visible)
        {
            slot.state.sent_visible = Some(visible);
            let _ = self.wire.set_visible(n, visible);
        }
        // The tree pass (re)sends the flag for a group that does not
        // exist yet, so the order of `set_node_visible` and the first
        // flush does not matter.
        if node.is_none() {
            self.mark(id, Dirty::TREE);
        }
    }

    /// Take a widget out of its parent's layout, or put it back.
    ///
    /// The fold-away primitive, and the complement of
    /// [`Ui::set_node_visible`]: a hidden widget keeps its box, a
    /// collapsed one gives it back — its siblings share the space and
    /// no gap is left where it was (see [`LayoutStyle::collapsed`]).
    /// It is hidden as well, because a subtree with no box must not be
    /// hit-tested or reached by Tab either, and shown again on the way
    /// back.
    ///
    /// Collapsing costs the parent's re-layout and one `SetVisible`;
    /// the subtree's scene nodes are kept, so expanding it again
    /// repaints nothing that did not move.
    pub fn set_collapsed(&mut self, id: WidgetId, collapsed: bool) {
        let Some(slot) = self.arena.slot(id) else {
            return;
        };
        if slot.state.style.collapsed == collapsed {
            return;
        }
        let parent = slot.state.parent;
        let mut style = slot.state.style.clone();
        style.collapsed = collapsed;
        self.set_style(id, style);
        if let Some(p) = parent {
            self.mark(p, Dirty::LAYOUT);
        }
        self.set_node_visible(id, !collapsed);
    }

    /// Whether `id` is collapsed out of its parent's layout; see
    /// [`Ui::set_collapsed`].
    #[must_use]
    pub fn is_collapsed(&self, id: WidgetId) -> bool {
        self.arena.slot(id).is_some_and(|s| s.state.style.collapsed)
    }

    /// Whether `id` is shown — its own flag **and** every ancestor's,
    /// since a widget inside a hidden page is not on screen either. See
    /// [`Ui::set_node_visible`].
    #[must_use]
    pub fn is_visible(&self, id: WidgetId) -> bool {
        let mut cur = Some(id);
        while let Some(w) = cur {
            let Some(slot) = self.arena.slot(w) else {
                return false;
            };
            if !slot.state.visible {
                return false;
            }
            cur = slot.state.parent;
        }
        true
    }

    // -- dirty marking ------------------------------------------------

    /// Mark `flags` on `id` and light the matching `SUB_` trail up to the
    /// root, stopping as soon as an ancestor already has it.
    pub fn mark(&mut self, id: WidgetId, flags: Dirty) {
        let Some(slot) = self.arena.slot_mut(id) else {
            return;
        };
        slot.state.flags.insert(flags);
        if flags.has(Dirty::LAYOUT) {
            slot.state.measured = None;
        }
        let sub = flags.to_sub();
        let mut cur = slot.state.parent;
        while let Some(p) = cur {
            let Some(slot) = self.arena.slot_mut(p) else {
                break;
            };
            // `has_all`, not `has`: a mark of `LAYOUT | PAINT` needs
            // *both* `SUB_` bits on the ancestor chain, and stopping at
            // the first ancestor that already carried one of them left
            // the other unset for the whole chain above it. The paint
            // pass then skipped a subtree holding a `PAINT` widget, and
            // the symptom was a change that took effect in the tree and
            // never reached the screen — `a_combined_mark_lights_every_sub_flag`
            // in this file is the regression.
            if slot.state.flags.has_all(sub) {
                break;
            }
            slot.state.flags.insert(sub);
            if flags.has(Dirty::LAYOUT) {
                // An ancestor's size may depend on this one's, so its
                // cached measurement is stale too.
                slot.state.measured = None;
            }
            cur = slot.state.parent;
        }
    }

    /// Clip a widget's children to its own bounds.
    ///
    /// The clip is set on the widget's **content group**, the node its
    /// children's groups hang under, so it costs one `SetClip` and
    /// nothing repaints. A widget with no children yet remembers the
    /// request and applies it when the group appears.
    pub fn set_content_clip(&mut self, id: WidgetId, clip: bool) {
        let Some(slot) = self.arena.slot_mut(id) else {
            return;
        };
        if slot.state.content_clip == clip {
            return;
        }
        slot.state.content_clip = clip;
        if let Some(node) = slot.state.content {
            let _ = self.wire.set_clip(node, clip);
        }
        // A clipping group needs a rectangle to clip *to*. See
        // [`Ui::sync_content_bounds`].
        self.sync_content_bounds(id);
    }

    /// Give a clipping content group the widget's own rectangle.
    ///
    /// A content group is a bare `Group`: it paints nothing, so its
    /// bounds normally do not matter and nothing ever sent them. A group
    /// that **clips**, though, clips its children to its own device rect,
    /// and a group created at `Rect::EMPTY` clips every child away to
    /// nothing.
    ///
    /// That was the launcher's "the rows exist, have rects, are clickable
    /// and are not on screen" bug: the rows hung under a [`Scroll`]'s
    /// content group, the `Scroll` asked for the clip that makes a
    /// viewport a viewport, and the scene dutifully clipped the whole
    /// list to an empty rectangle. Only the size is sent — the origin
    /// stays at zero, because the group is already inside the widget's
    /// own group and shifting it would move the children twice.
    ///
    /// Sent only while the clip is on, so a widget that does not clip
    /// costs nothing, and cached, so a relayout that did not resize the
    /// widget costs nothing either.
    fn sync_content_bounds(&mut self, id: WidgetId) {
        let Some(slot) = self.arena.slot(id) else {
            return;
        };
        if !slot.state.content_clip {
            return;
        }
        let Some(node) = slot.state.content else {
            return;
        };
        let b = slot.state.bounds;
        let rect = Rect::new(0.0, 0.0, b.w, b.h);
        if slot.state.content_bounds == rect {
            return;
        }
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.content_bounds = rect;
        }
        let _ = self.wire.set_bounds(node, rect);
    }

    /// Translate (or otherwise transform) a widget's children without
    /// laying them out or painting them again.
    ///
    /// This is the mechanism behind [`Scroll`](crate::widgets::Scroll)
    /// and the reason scrolling is cheap: the children hang under one
    /// content group, and moving that group is exactly one
    /// `SetTransform` on the wire. Nothing inside is re-measured,
    /// re-placed or repainted.
    pub fn set_content_transform(&mut self, id: WidgetId, transform: nitro_core::Transform) {
        let Some(slot) = self.arena.slot_mut(id) else {
            return;
        };
        if slot.state.content_transform == transform {
            return;
        }
        slot.state.content_transform = transform;
        if let Some(node) = slot.state.content {
            let _ = self.wire.set_transform(node, transform);
        }
    }

    /// Move a **group** paint slot's children by `transform`, without a
    /// repaint — the same operation as
    /// [`WidgetMut::set_slot_transform`], reachable from a plain
    /// `&mut Ui`.
    ///
    /// Both doors exist because a widget needs this from two places. A
    /// setter has a `WidgetMut`; a widget's own `event` and `action` do
    /// not — they are handed `cx.ui`, a full `&mut Ui<S>`, precisely
    /// *because* the widget is out of its slot and cannot be taken
    /// again. `List` scrolls from both, and a version that only had the
    /// `WidgetMut` form would have had to ask for itself and get
    /// [`Error::Busy`].
    ///
    /// Nothing happens if the slot does not exist yet or is not a
    /// group; the next paint creates it.
    pub fn set_slot_transform(
        &mut self,
        id: WidgetId,
        slot: crate::widget::Slot,
        transform: nitro_core::Transform,
    ) {
        let index = slot as usize;
        let Some(state) = self.arena.slot_mut(id).map(|s| &mut s.state) else {
            return;
        };
        let Some(paint) = state.slots.get_mut(index) else {
            return;
        };
        if paint.node.is_none() || paint.transform == transform {
            return;
        }
        paint.transform = transform;
        let node = paint.node;
        let _ = self.wire.set_transform(node, transform);
    }

    /// The transform currently applied to a widget's children.
    #[must_use]
    pub fn content_transform(&self, id: WidgetId) -> nitro_core::Transform {
        self.arena
            .slot(id)
            .map_or(nitro_core::Transform::IDENTITY, |s| {
                s.state.content_transform
            })
    }

    /// A widget's style, as the layout pass reads it.
    #[must_use]
    pub fn style(&self, id: WidgetId) -> LayoutStyle {
        self.arena
            .slot(id)
            .map(|s| s.state.style.clone())
            .unwrap_or_default()
    }

    /// Replace a widget's style and mark it for layout.
    pub fn set_style(&mut self, id: WidgetId, style: LayoutStyle) {
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.style = style;
        }
        self.mark(id, Dirty::LAYOUT | Dirty::PAINT);
    }

    // -- take-out dispatch --------------------------------------------

    fn take(&mut self, id: WidgetId) -> Result<Box<dyn AnyWidget<S>>, Error> {
        let slot = self.arena.slot_mut(id).ok_or(Error::StaleWidget)?;
        slot.widget.take().ok_or(Error::Busy)
    }

    /// Put a widget back. A widget destroyed while it was out simply has
    /// nowhere to go back to, and is dropped here.
    fn untake(&mut self, id: WidgetId, widget: Box<dyn AnyWidget<S>>) {
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.widget = Some(widget);
        }
    }

    // -- the passes ---------------------------------------------------

    /// Open the one window, sized to `size` or to the root's measured
    /// size.
    ///
    /// # Errors
    /// [`Error::NoRoot`] without a root, or a wire error.
    pub fn open_window(&mut self, title: &str, size: Option<Size>) -> Result<(), Error> {
        let root = self.root.ok_or(Error::NoRoot)?;
        let size = if let Some(s) = size {
            s
        } else {
            let m = self.measure(root, Constraints::unbounded());
            Size::new(m.w.max(1.0).ceil(), m.h.max(1.0).ceil())
        };
        self.window_size = size;
        let surface = self.surface;
        let (layer, flags) = surface.map_or((nitro_wire::types::Layer::Normal, 0), |s| {
            (s.layer, s.flags)
        });
        self.wire.create_window(WINDOW, title, size, layer, flags)?;
        // The app id, in the same commit: it is what a window list names
        // the program by, and a window that existed for one frame without
        // one would appear in a bar as an anonymous row.
        if !self.app_id.is_empty() {
            let app_id = std::mem::take(&mut self.app_id);
            self.wire.set_app_id(WINDOW, &app_id)?;
            self.app_id = app_id;
        }
        // In the *same* transaction, which is the whole reason the server
        // buffers these two to the sender's commit: an anchor answered on
        // receipt would name a window `Commit` has not created yet, and a
        // bar that anchored a frame later would paint once at the
        // placeholder size above and then jump. See `docs/shell.md`.
        if let Some(s) = surface {
            if let Some(a) = s.anchor {
                self.wire.set_anchor(WINDOW, a.edges, a.margin)?;
            }
            if let Some((edge, px)) = s.zone {
                self.wire.set_exclusive_zone(WINDOW, edge, px)?;
            }
        }
        self.window_open = true;
        // Limits set before the window existed ride its first commit, for
        // the reason the anchor above does: a window that appeared
        // without them could be resized below its minimum in the frame
        // between.
        if let Some((min, max)) = self.window_limits {
            self.wire.set_window_limits(WINDOW, min, max)?;
        }
        // The window was created with `title`, so record it rather than
        // re-sending it: that is what makes the first `set_window_title`
        // with the same string free.
        title.clone_into(&mut self.window_title);
        self.mark(root, Dirty::LAYOUT | Dirty::PAINT | Dirty::TREE);
        Ok(())
    }

    /// Set the application id sent with the window: what a window list
    /// names this program by. [`App`](crate::App) sets it from the name
    /// the app was constructed with.
    pub fn set_app_id(&mut self, app_id: impl Into<String>) {
        self.app_id = app_id.into();
    }

    /// Change the window's title — what a bar's window list and the
    /// server's decoration show.
    ///
    /// Queued as a mutation, so it rides the next commit like everything
    /// else, and **an unchanged title sends nothing**. That last part is
    /// not an optimisation for its own sake: a shell that prints its
    /// working directory in an OSC sequence sets the same title on every
    /// prompt, and a terminal that forwarded each one would put a
    /// `SetWindowTitle` and a commit on the wire for every command the
    /// user runs — and make the server relist its windows each time.
    ///
    /// # Errors
    /// A wire failure, which is fatal.
    pub fn set_window_title(&mut self, title: impl Into<String>) -> Result<(), Error> {
        let title = title.into();
        if title == self.window_title {
            return Ok(());
        }
        self.window_title = title;
        if !self.window_open {
            // The title the window is *created* with is `open_window`'s
            // argument; setting one before there is a window would name
            // a node the server has not seen.
            return Ok(());
        }
        self.wire.set_window_title(WINDOW, &self.window_title)
    }

    /// The title last set with [`Ui::set_window_title`].
    #[must_use]
    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    /// Tell the server the smallest and largest content size this window
    /// can usefully be resized to. A zero component means "no limit".
    ///
    /// A terminal is the motivating case: a grid below about 20×5 cells
    /// is not a terminal any more, and the server is the only thing that
    /// can refuse the drag — a client that merely clamped its own layout
    /// would draw a letterbox inside a window the user is still
    /// shrinking. Repeating the same limits sends nothing.
    ///
    /// # Errors
    /// A wire failure, which is fatal.
    pub fn set_window_limits(&mut self, min: Size, max: Size) -> Result<(), Error> {
        if self.window_limits == Some((min, max)) {
            return Ok(());
        }
        self.window_limits = Some((min, max));
        if !self.window_open {
            return Ok(());
        }
        self.wire.set_window_limits(WINDOW, min, max)
    }

    /// The size the tree wants with nothing constraining it: what a
    /// window opened without an explicit size is given.
    ///
    /// With [`Ui::request_window_size`], this is how an app fits its
    /// window to its content after the content changed shape — a section
    /// collapsed or expanded — rather than leaving a gap or clipping.
    /// Collapsed subtrees count for nothing, as they do in layout.
    #[must_use]
    pub fn natural_size(&mut self) -> Size {
        let Some(root) = self.root else {
            return Size::ZERO;
        };
        let m = self.measure(root, Constraints::unbounded());
        Size::new(m.w.max(1.0).ceil(), m.h.max(1.0).ceil())
    }

    /// Ask the server to resize the window's content area to `size`.
    ///
    /// Sent as a `SetBounds` on the window's own root, which the server
    /// treats as a resize request and answers with a `Configure`
    /// (`docs/wire.md`); the tree re-lays out for the new size at once
    /// rather than a round trip later. The size is clamped to the limits
    /// set with [`Ui::set_window_limits`] first, because the server
    /// applies those to the user's drags, not to the client's own
    /// requests, and an app should not be able to talk its window past
    /// a floor it declared. Before the window is open it does nothing:
    /// [`Ui::open_window`] sizes a new window itself. An unchanged size
    /// sends nothing.
    ///
    /// # Errors
    /// A wire failure, which is fatal.
    pub fn request_window_size(&mut self, size: Size) -> Result<(), Error> {
        let size = match self.window_limits {
            Some((min, max)) => {
                let axis = |v: f32, lo: f32, hi: f32| {
                    let v = if lo > 0.0 { v.max(lo) } else { v };
                    if hi > 0.0 { v.min(hi.max(lo)) } else { v }
                };
                Size::new(axis(size.w, min.w, max.w), axis(size.h, min.h, max.h))
            }
            None => size,
        };
        let size = Size::new(size.w.max(1.0).ceil(), size.h.max(1.0).ceil());
        if !self.window_open || self.window_size == size {
            return Ok(());
        }
        self.resize(size);
        self.wire
            .set_bounds(WINDOW, Rect::new(0.0, 0.0, size.w, size.h))
    }

    /// Ask the server for a frame callback: it answers with one
    /// [`Frame`] carrying the deadline for the next flip.
    ///
    /// One request, one answer, no free-running loop — this is the
    /// toolkit's half of `RequestFrame` (`docs/wire.md`), and it is what
    /// an app uses when its *input* is faster than the screen: absorb
    /// everything into its own model as it arrives, and do the expensive
    /// tree work once per callback instead of once per event.
    ///
    /// **Do not make the callback the only thing that can paint.** One
    /// request is outstanding at a time and the server answers after a
    /// flip, so an app that only marks widgets dirty inside the handler
    /// has made painting depend on an answer that a server coalescing
    /// flips under load does not send. `nitro-term` tried it and froze
    /// its screen: four frames in twelve seconds of steady output, with
    /// consecutive framebuffer readbacks byte-identical while its model
    /// advanced normally. Bound the *input* instead (it reads at most
    /// 256 KiB of pty per turn) and let the server's flip coalescing
    /// supply "at most one change per refresh", which is its job. The
    /// worked reasoning is in `docs/ui.md`.
    ///
    /// Asking twice before the answer arrives sends one request: the
    /// second is folded into the first, because two callbacks per frame
    /// is precisely the free-running loop this exists to avoid.
    ///
    /// # Errors
    /// A wire failure, which is fatal.
    pub fn request_frame(&mut self) -> Result<(), Error> {
        if self.frame_requested || !self.window_open {
            return Ok(());
        }
        self.frame_requested = true;
        self.wire
            .send_now(&nitro_wire::msg::ClientMsg::RequestFrame(
                nitro_wire::msg::RequestFrame { window: WINDOW },
            ))
    }

    /// Whether a frame callback is outstanding.
    #[must_use]
    pub fn frame_pending(&self) -> bool {
        self.frame_requested
    }

    /// Register a handler for frame callbacks; see [`Ui::request_frame`].
    ///
    /// A handler is handed `&mut S` and `&mut Ui<S>`, exactly like a
    /// button's `on_click`, so it edits the tree rather than only
    /// setting a flag — a terminal's handler is where the grid's damage
    /// becomes `SetText`s. It is a list rather than a widget hung off the
    /// root for the same reason [`Ui::on_shell`] is: a frame deadline is
    /// news about the *output*, with no position to hit-test and no
    /// focus to follow. Every handler sees every frame; there is nothing
    /// to consume.
    pub fn on_frame(&mut self, handler: impl FnMut(&mut S, &mut Ui<S>, Frame) + 'static) {
        self.frame_handlers.push(Some(Box::new(handler)));
    }

    /// How many frame handlers are registered.
    #[must_use]
    pub fn frame_handler_count(&self) -> usize {
        self.frame_handlers.len()
    }

    /// Offer a frame callback to the handlers, oldest first.
    ///
    /// Public because an app driving `Ui` by hand — and the test harness
    /// — dispatches messages itself; [`Ui::dispatch`] calls it for a real
    /// `Frame`.
    pub fn dispatch_frame(&mut self, state: &mut S, frame: Frame) {
        self.frame_requested = false;
        for i in 0..self.frame_handlers.len() {
            // Out of the list for the call, for the same reason a widget
            // leaves its slot: the handler is handed the `Ui` the list
            // lives in.
            let Some(mut h) = self.frame_handlers.get_mut(i).and_then(Option::take) else {
                continue;
            };
            h(state, self, frame);
            if let Some(slot) = self.frame_handlers.get_mut(i) {
                *slot = Some(h);
            }
        }
    }

    /// Register a handler for window resizes.
    ///
    /// Called after a `Configure` has been applied — the window size and
    /// scale are already the new ones and the tree is marked for layout
    /// — and handed `&mut S` and `&mut Ui<S>` like every other callback,
    /// so it can do the work a *resize* implies rather than the work a
    /// re-layout implies.
    ///
    /// Those are different things, which is why this hook exists at all.
    /// Laying the tree out again is the framework's job and needs no
    /// help. But an app whose content is measured in its own units — a
    /// terminal's cells, a canvas's tiles — has to *recompute how much
    /// content fits*, and may have to tell something outside the process
    /// about it: `nitro-term` turns the new pixel size into a column and
    /// row count, reflows its grid and sends `TIOCSWINSZ` so the child
    /// gets `SIGWINCH`. None of that can be expressed as a `measure`,
    /// because `measure` answers "how big would you like to be" and this
    /// is "you are this big now, deal with it".
    ///
    /// A list rather than a widget hook, for the same reason
    /// [`Ui::on_frame`] and [`Ui::on_shell`] are: a new window size is
    /// news about the *window*, with no position to hit-test and no
    /// focus to follow. Every handler sees every resize; there is
    /// nothing to consume. A `Configure` that does not change the size
    /// fires nothing.
    pub fn on_resize(&mut self, handler: impl FnMut(&mut S, &mut Ui<S>, Size) + 'static) {
        self.resize_handlers.push(Some(Box::new(handler)));
    }

    /// How many resize handlers are registered.
    #[must_use]
    pub fn resize_handler_count(&self) -> usize {
        self.resize_handlers.len()
    }

    /// Offer a new window size to the resize handlers, oldest first.
    ///
    /// Public for the same reason [`Ui::dispatch_frame`] is: an app
    /// driving `Ui` by hand, and the test harness, dispatch messages
    /// themselves. [`Ui::dispatch`] calls it for a real `Configure`.
    pub fn dispatch_resize(&mut self, state: &mut S, size: Size) {
        for i in 0..self.resize_handlers.len() {
            // Out of the list for the call, for the same reason a widget
            // leaves its slot: the handler is handed the `Ui` the list
            // lives in.
            let Some(mut h) = self.resize_handlers.get_mut(i).and_then(Option::take) else {
                continue;
            };
            h(state, self, size);
            if let Some(slot) = self.resize_handlers.get_mut(i) {
                *slot = Some(h);
            }
        }
    }

    /// Register a handler called after the server pushed a new palette.
    ///
    /// The tree has already been re-themed and marked for repaint by the
    /// time a handler runs, so a handler is only for what the framework
    /// *cannot* know: a widget holding derived pixels — `nitro-term`'s
    /// grid caches per-cell colours — has to rebuild them, and an app
    /// that painted into an image buffer has to repaint it.
    ///
    /// A list rather than a widget hook, for the same reason
    /// [`Ui::on_resize`] is: a palette is news about the *desktop*, with
    /// no position to hit-test and no focus to follow.
    ///
    /// It fires only when the palette **actually changed** — a server
    /// re-sending the palette a client already has runs nothing — so a
    /// handler may do real work without costing a settled app a repaint
    /// for a message that said nothing.
    pub fn on_theme(&mut self, handler: impl FnMut(&mut S, &mut Ui<S>) + 'static) {
        self.theme_handlers.push(Some(Box::new(handler)));
    }

    /// How many theme handlers are registered.
    #[must_use]
    pub fn theme_handler_count(&self) -> usize {
        self.theme_handlers.len()
    }

    /// Tell the theme handlers the palette moved, oldest first.
    ///
    /// Public for the same reason [`Ui::dispatch_resize`] is: a test and
    /// an app driving `Ui` by hand dispatch messages themselves.
    pub fn dispatch_theme(&mut self, state: &mut S) {
        for i in 0..self.theme_handlers.len() {
            let Some(mut h) = self.theme_handlers.get_mut(i).and_then(Option::take) else {
                continue;
            };
            h(state, self);
            if let Some(slot) = self.theme_handlers.get_mut(i) {
                *slot = Some(h);
            }
        }
    }

    /// Open this window as a shell surface: a bar, dock, launcher or
    /// wallpaper rather than an ordinary application window.
    ///
    /// Must be called **before** [`Ui::open_window`] — the surface is
    /// what that call creates, and a window cannot change layer without
    /// the desktop seeing it on the wrong one first.
    /// [`App::shell`](crate::App::shell) is the form an app uses.
    pub fn set_surface(&mut self, surface: crate::shell::Surface) {
        self.surface = Some(surface);
    }

    /// What kind of surface this window is, if it is one.
    #[must_use]
    pub fn surface(&self) -> Option<crate::shell::Surface> {
        self.surface
    }

    /// Whether a widget that takes focus when it is clicked should be
    /// allowed to.
    ///
    /// False in a `NO_FOCUS` window — a bar, a dock, a wallpaper, a
    /// launcher overlay. Such a window never receives a key, so toolkit
    /// focus inside it buys nothing and costs something visible: the
    /// clicked button keeps a focus ring afterwards, and `hey … list`
    /// reports it `focused`, which is a lie about a surface the server
    /// will not focus. The click still *acts* — this suppresses the
    /// focus, not the activation.
    ///
    /// It gates [`EventCx::request_focus`](crate::EventCx::request_focus)
    /// only, which is how a widget asks for focus from inside its own
    /// event handling. [`Ui::focus`] is unchanged and deliberate: the
    /// launcher is `NO_FOCUS` and still focuses its query field, because
    /// it reads the keyboard through a grab rather than through focus.
    #[must_use]
    pub fn click_takes_focus(&self) -> bool {
        self.surface
            .is_none_or(|s| s.flags & nitro_wire::types::window_flags::NO_FOCUS == 0)
    }

    /// Whether this connection may send the shell ops, i.e. whether it
    /// arrived on `shell.sock`.
    ///
    /// Worth checking before binding a hotkey: a shell op on an
    /// unprivileged connection is a **fatal** protocol error, so the
    /// honest failure is to notice here rather than to be disconnected.
    #[must_use]
    pub fn is_shell(&self) -> bool {
        self.wire.conn().has_caps(nitro_wire::types::caps::SHELL)
    }

    /// Show or hide the whole window, without destroying anything.
    ///
    /// This is what a launcher uses to come and go: the tree is built
    /// once and hidden, so showing it again is **one** mutation rather
    /// than a rebuild — and everything under it (its measurements, its
    /// scene nodes, its text) survives the round trip. Hiding also
    /// releases what the server hangs on a window *showing*: an exclusive
    /// zone and a keyboard grab (`docs/shell.md`).
    ///
    /// A no-op change sends nothing, so a hide of an already-hidden
    /// window costs no commit.
    ///
    /// # Errors
    /// A wire failure, which is fatal.
    pub fn set_window_visible(&mut self, visible: bool) -> Result<(), Error> {
        self.wire.set_visible(WINDOW, visible)
    }

    /// Whether the window is currently shown; `true` until something
    /// hides it.
    #[must_use]
    pub fn window_visible(&self) -> bool {
        self.wire.window_visible()
    }

    /// Take or release the keyboard grab on this window.
    ///
    /// A grab replaces **focus** as the destination of key events, which
    /// is how a `NO_FOCUS` overlay reads the keyboard without making the
    /// window behind it look inactive. It does **not** outrank the shell's
    /// own [`bind_key`](Ui::bind_key) bindings: a bound chord arrives as a
    /// `HotKey` and is not also delivered here, which is what lets a
    /// launcher opened by a Super tap be closed by a second one.
    ///
    /// Queued as a mutation rather than sent at once, so it rides the
    /// same commit as an un-hide: the server drops a grab on a window
    /// that is not showing, so taking one in an earlier transaction than
    /// the `SetVisible` that shows the window would be dropped again
    /// immediately.
    ///
    /// # Errors
    /// A wire failure. On an unprivileged connection the server closes
    /// the connection instead — check [`Ui::is_shell`].
    pub fn grab_keyboard(&mut self, on: bool) -> Result<(), Error> {
        self.wire.grab_keyboard(WINDOW, on)
    }

    /// Resize the window's content area; the next flush re-lays out.
    pub fn resize(&mut self, size: Size) {
        if self.window_size == size {
            return;
        }
        self.window_size = size;
        if let Some(root) = self.root {
            self.mark(root, Dirty::LAYOUT | Dirty::PAINT);
        }
    }

    /// Run the passes and commit, if anything is dirty.
    ///
    /// Returns whether a commit was sent. **Nothing dirty means no
    /// commit**, which is what makes an idle app cost zero bytes.
    ///
    /// # Errors
    /// Any wire failure; they are all fatal.
    pub fn flush(&mut self) -> Result<bool, Error> {
        let Some(root) = self.root else {
            return Ok(false);
        };
        if !self.window_open {
            return Ok(false);
        }
        self.pass_tree(root)?;
        self.pass_layout(root)?;
        self.pass_clip(root)?;
        self.pass_paint(root)?;
        self.pass_backdrop()?;
        self.wire.commit()
    }

    /// CLIP: the root's group clips everything under it to the window.
    ///
    /// **A window's content is clipped to the window.** A layout that
    /// overflows is a bug either way, but there are two ways for it to
    /// fail and only one of them is survivable: cut off at the window's
    /// edge, or painted onto the desktop beside it. `nitro-settings`
    /// shipped the second one — a display row 700 px wide in a 560 px
    /// window put its slider, its checkbox and both position fields
    /// outside the frame, over whatever was behind.
    ///
    /// The clip is a single `SetClip` on the root widget's own group,
    /// which the layout pass already sizes to the window on every
    /// `Configure` — so "and on every size change" costs nothing extra
    /// and cannot go stale: the rectangle the scene clips to *is* the
    /// rectangle the root was laid out in. Sent once, then remembered.
    ///
    /// That equality holds because [`Ui::pass_layout`] lays the root out
    /// at `Rect(0, 0, window_size)` **regardless of the root's own
    /// width/height style** — not because a root style could not ask for
    /// something else. A root with `width(300.0)` is measured at 300 and
    /// still *placed* at the window's full size, so its group is the
    /// window's rectangle either way. If that ever changes, this clip
    /// has to become a rectangle of its own rather than a flag.
    ///
    /// The server **now** clips a window's content too, regardless of
    /// what its client asks for (#3726) — but **not this node**. The
    /// compositor clips the window's *content group*, which is the
    /// `WINDOW` id a `CreateWindow` binds; the root widget's group is a
    /// child of it, created by [`Ui::pass_tree`] from `alloc_node`. So
    /// this `SetClip` is a real mutation on a group whose `clip` starts
    /// `false`, not a no-op, and the two clips are **redundant rather
    /// than identical**: they narrow to the same rectangle, because the
    /// layout pass places the root at the window's full size (above), but
    /// they are two nodes one level apart.
    ///
    /// Kept rather than deleted, and not only for older servers: a
    /// toolkit that relies on the compositor to contain it is a toolkit
    /// whose bugs are invisible until they are somebody else's. The clip
    /// here is what makes an overflowing layout show up as cut-off in
    /// `nitro-ui`'s own tests, against its own harness, instead of only
    /// on a desktop.
    fn pass_clip(&mut self, root: WidgetId) -> Result<(), Error> {
        if self.root_clipped {
            return Ok(());
        }
        let Some(node) = self.arena.slot(root).and_then(|s| s.state.node) else {
            return Ok(());
        };
        self.wire.set_clip(node, true)?;
        self.root_clipped = true;
        Ok(())
    }

    /// The window background: one `Rect` node, created before anything
    /// else under the window root so it is behind the whole tree, and
    /// re-sent only when the size or the theme colour changed.
    fn pass_backdrop(&mut self) -> Result<(), Error> {
        if !self.backdrop_wanted {
            if let Some(node) = self.backdrop.take() {
                self.backdrop_sent = None;
                self.wire.destroy_node(node)?;
            }
            return Ok(());
        }
        let color = self.theme.background;
        let size = self.window_size;
        if self.backdrop_sent == Some((size, color)) {
            return Ok(());
        }
        let rect = Rect::new(0.0, 0.0, size.w, size.h);
        let node = if let Some(n) = self.backdrop {
            n
        } else {
            {
                let n = self.wire.alloc_node();
                // `before` is the root widget's group, which the TREE
                // pass created first: the backdrop goes in front of it in
                // sibling order, which is *behind* it on screen.
                let before = self
                    .root
                    .and_then(|r| self.arena.slot(r))
                    .and_then(|s| s.state.node)
                    .unwrap_or(NodeId::NONE);
                self.wire.create_rect(n, WINDOW, before)?;
                self.backdrop = Some(n);
                n
            }
        };
        self.wire.set_backdrop(node, rect, color)?;
        self.backdrop_sent = Some((size, color));
        Ok(())
    }

    /// TREE: create, reparent and destroy the scene groups of
    /// structurally dirty subtrees.
    fn pass_tree(&mut self, id: WidgetId) -> Result<(), Error> {
        let Some(slot) = self.arena.slot(id) else {
            return Ok(());
        };
        let flags = slot.state.flags;
        if !flags.has(Dirty::TREE | Dirty::SUB_TREE) {
            return Ok(());
        }
        // The root's own group hangs off the window.
        if slot.state.node.is_none() {
            let parent_node = match slot.state.parent {
                Some(p) => self
                    .arena
                    .slot(p)
                    .and_then(|s| s.state.content)
                    .unwrap_or(WINDOW),
                None => WINDOW,
            };
            let node = self.wire.alloc_node();
            self.wire.create_group(node, parent_node, NodeId::NONE)?;
            if let Some(slot) = self.arena.slot_mut(id) {
                slot.state.node = Some(node);
            }
        }
        self.sync_visible(id)?;
        if flags.has(Dirty::TREE) {
            self.sync_children(id)?;
        }
        let children = self.borrow_children(id);
        for c in &children {
            self.pass_tree(*c)?;
        }
        self.return_children(children);
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.flags.remove(Dirty::TREE | Dirty::SUB_TREE);
        }
        Ok(())
    }

    /// Send a hidden widget's flag once its group exists. A group is
    /// created visible, so only `false` — or a value that differs from
    /// the last one sent — costs a message.
    fn sync_visible(&mut self, id: WidgetId) -> Result<(), Error> {
        let Some(slot) = self.arena.slot_mut(id) else {
            return Ok(());
        };
        let Some(node) = slot.state.node else {
            return Ok(());
        };
        let want = slot.state.visible;
        let sent = slot.state.sent_visible.unwrap_or(true);
        if sent == want {
            return Ok(());
        }
        slot.state.sent_visible = Some(want);
        self.wire.set_visible(node, want)
    }

    /// Bring the scene's child order in line with the widget's.
    fn sync_children(&mut self, id: WidgetId) -> Result<(), Error> {
        let children = self.borrow_children(id);
        if children.is_empty() {
            self.return_children(children);
            return Ok(());
        }
        // The content group holds children only, and is created after the
        // widget's own paint nodes so those stay underneath.
        let mut clipped = false;
        let content = if let Some(c) = self.arena.slot(id).and_then(|s| s.state.content) {
            c
        } else {
            let parent = self.arena.slot(id).and_then(|s| s.state.node);
            let Some(parent) = parent else {
                self.return_children(children);
                return Ok(());
            };
            let node = self.wire.alloc_node();
            self.wire.create_group(node, parent, NodeId::NONE)?;
            if let Some(slot) = self.arena.slot_mut(id) {
                slot.state.content = Some(node);
            }
            // A clip or transform asked for before the group existed is
            // applied now, so the order of `set_content_clip` and the
            // first child does not matter.
            let (clip, transform) = self
                .arena
                .slot(id)
                .map_or((false, nitro_core::Transform::IDENTITY), |s| {
                    (s.state.content_clip, s.state.content_transform)
                });
            if clip {
                self.wire.set_clip(node, true)?;
                clipped = true;
            }
            if transform != nitro_core::Transform::IDENTITY {
                self.wire.set_transform(node, transform)?;
            }
            node
        };
        // A group that clips needs a rectangle to clip *to*, and it was
        // just created empty. See [`Ui::sync_content_bounds`].
        if clipped {
            self.sync_content_bounds(id);
        }
        // Walk backwards so `before` always names a sibling already in
        // place: creating or moving a node in front of the one that
        // follows it is enough to reproduce the whole order.
        let mut before = NodeId::NONE;
        for c in children.iter().rev() {
            let existing = self.arena.slot(*c).and_then(|s| s.state.node);
            let node = if let Some(n) = existing {
                n
            } else {
                let n = self.wire.alloc_node();
                self.wire.create_group(n, content, before)?;
                if let Some(slot) = self.arena.slot_mut(*c) {
                    slot.state.node = Some(n);
                }
                n
            };
            if existing.is_some() {
                let attached = self.arena.slot(*c).map(|s| s.state.attached);
                if attached != Some(Some((content, before))) {
                    self.wire.reparent(node, content, before)?;
                }
            }
            if let Some(slot) = self.arena.slot_mut(*c) {
                slot.state.attached = Some((content, before));
            }
            before = node;
        }
        self.return_children(children);
        Ok(())
    }

    /// LAYOUT: measure and place the dirty subtrees.
    fn pass_layout(&mut self, root: WidgetId) -> Result<(), Error> {
        let Some(slot) = self.arena.slot(root) else {
            return Ok(());
        };
        if !slot.state.flags.has(Dirty::LAYOUT | Dirty::SUB_LAYOUT) {
            return Ok(());
        }
        let rect = Rect::new(0.0, 0.0, self.window_size.w, self.window_size.h);
        self.measure(root, Constraints::tight(self.window_size));
        self.layout_widget(root, rect)?;
        Ok(())
    }

    /// Place one widget and, if it needs it, lay its children out.
    fn layout_widget(&mut self, id: WidgetId, rect: Rect) -> Result<(), Error> {
        let Some(slot) = self.arena.slot_mut(id) else {
            return Ok(());
        };
        let old = slot.state.bounds;
        slot.state.bounds = rect;
        let moved = old.origin() != rect.origin();
        let resized = old.size() != rect.size();
        if resized {
            slot.state.flags.insert(Dirty::LAYOUT | Dirty::PAINT);
        }
        let node = slot.state.node;
        let flags = slot.state.flags;
        // One mutation moves a widget and everything under it; its
        // content does not repaint.
        if (moved || resized)
            && let Some(n) = node
        {
            self.wire.set_bounds(n, rect)?;
        }
        // A clipping content group is sized with the widget, or a
        // resized viewport would go on clipping to its old rectangle.
        if resized {
            self.sync_content_bounds(id);
        }
        if !flags.has(Dirty::LAYOUT | Dirty::SUB_LAYOUT) {
            return Ok(());
        }
        let Ok(mut widget) = self.take(id) else {
            return Ok(());
        };
        {
            let mut cx = LayoutCx { ui: self, id };
            widget.layout(&mut cx, rect);
        }
        self.untake(id, widget);
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.flags.remove(Dirty::LAYOUT | Dirty::SUB_LAYOUT);
        }
        Ok(())
    }

    /// Measure a widget, honouring its explicit width/height and its
    /// min/max clamps, and memoizing the answer.
    pub(crate) fn measure(&mut self, id: WidgetId, constraints: Constraints) -> Size {
        let Some(slot) = self.arena.slot(id) else {
            return Size::ZERO;
        };
        let style = slot.state.style.clone();
        let c = apply_style(&style, constraints);
        if let Some((cached_c, size)) = slot.state.measured
            && cached_c == c
            && !slot.state.flags.has(Dirty::LAYOUT)
        {
            return size;
        }
        let Ok(mut widget) = self.take(id) else {
            return Size::ZERO;
        };
        // A widget re-reports its floor from `measure`, so the stale one
        // is cleared first: a label that stopped eliding must not keep
        // the floor it had while it did.
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.floor = None;
        }
        let size = {
            let mut cx = MeasureCx { ui: self, id };
            widget.measure(&mut cx, c)
        };
        self.untake(id, widget);
        let size = c.constrain(style.clamp(size));
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.measured = Some((c, size));
        }
        size
    }

    /// Record a floor a widget reported from its own `measure`; see
    /// [`MeasureCx::report_floor`](crate::widget::MeasureCx::report_floor).
    pub(crate) fn set_reported_floor(&mut self, id: WidgetId, floor: Option<Size>) {
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.floor = floor;
        }
    }

    /// The floor a widget last reported, for the flex solver.
    pub(crate) fn reported_floor(&self, id: WidgetId) -> Option<Size> {
        self.arena.slot(id).and_then(|s| s.state.floor)
    }

    /// The default container layout: flex-solve the children of `id`
    /// inside `bounds` and recurse.
    pub(crate) fn layout_flex_children(&mut self, id: WidgetId, bounds: Rect) {
        let children = self.borrow_children(id);
        if children.is_empty() {
            self.return_children(children);
            return;
        }
        let style = self.style(id);
        let inner = Size::new(
            (bounds.w - style.padding.horizontal()).max(0.0),
            (bounds.h - style.padding.vertical()).max(0.0),
        );
        let mut items = self.item_pool.pop().unwrap_or_default();
        items.clear();
        for c in &children {
            let cstyle = self.style(*c);
            let avail = Size::new(
                (inner.w - cstyle.margin.horizontal()).max(0.0),
                (inner.h - cstyle.margin.vertical()).max(0.0),
            );
            let basis = self.measure(*c, Constraints::loose(avail));
            items.push(FlexItem::new(cstyle, basis).with_floor(self.reported_floor(*c)));
        }
        let mut rects = self.rect_pool.pop().unwrap_or_default();
        crate::layout::solve(&style, inner, &items, &mut rects);
        for (c, r) in children.iter().zip(&rects) {
            let _ = self.layout_widget(*c, *r);
        }
        items.clear();
        rects.clear();
        self.item_pool.push(items);
        self.rect_pool.push(rects);
        self.return_children(children);
    }

    /// Place one child explicitly.
    pub(crate) fn place(&mut self, child: WidgetId, bounds: Rect) {
        let _ = self.layout_widget(child, bounds);
    }

    /// PAINT: every widget with [`Dirty::PAINT`] emits its own nodes.
    fn pass_paint(&mut self, id: WidgetId) -> Result<(), Error> {
        let Some(slot) = self.arena.slot(id) else {
            return Ok(());
        };
        let flags = slot.state.flags;
        if !flags.has(Dirty::PAINT | Dirty::SUB_PAINT) {
            return Ok(());
        }
        if flags.has(Dirty::PAINT) {
            self.paint_one(id)?;
        }
        let children = self.borrow_children(id);
        let mut result = Ok(());
        for c in &children {
            result = self.pass_paint(*c);
            if result.is_err() {
                break;
            }
        }
        self.return_children(children);
        result?;
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.flags.remove(Dirty::PAINT | Dirty::SUB_PAINT);
        }
        Ok(())
    }

    fn paint_one(&mut self, id: WidgetId) -> Result<(), Error> {
        let Some(slot) = self.arena.slot_mut(id) else {
            return Ok(());
        };
        let Some(group) = slot.state.node else {
            return Ok(());
        };
        let before = slot.state.content.unwrap_or(NodeId::NONE);
        let bounds = slot.state.bounds;
        let mut slots = std::mem::take(&mut slot.state.slots);
        Wire::begin_paint(&mut slots);
        let Ok(mut widget) = self.take(id) else {
            if let Some(slot) = self.arena.slot_mut(id) {
                slot.state.slots = slots;
            }
            return Ok(());
        };
        let mut cx = PaintCx {
            ui: self,
            id,
            bounds: Rect::new(0.0, 0.0, bounds.w, bounds.h),
            group,
            before,
            slots,
            error: None,
        };
        widget.paint(&mut cx);
        let PaintCx {
            mut slots, error, ..
        } = cx;
        self.untake(id, widget);
        let cleanup = self.wire.end_paint(&mut slots);
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.slots = slots;
        }
        match error {
            Some(e) => Err(e),
            None => cleanup,
        }
    }

    // -- events -------------------------------------------------------

    /// Drain and dispatch everything the server has sent.
    ///
    /// # Errors
    /// A wire failure, which is fatal.
    pub fn pump(&mut self, state: &mut S) -> Result<usize, Error> {
        let mut batch = std::mem::take(&mut self.wire.stray);
        match self.wire.conn_mut().poll(&mut batch) {
            Ok(_) => {}
            Err(nitro_wire::Error::Closed) => {
                // The server may have explained itself in an earlier batch
                // and then closed. Report that reason rather than exiting
                // 0 — or with a bare `connection closed` — on the EOF.
                self.quit = true;
                if let Some(e) = self.last_server_error.take() {
                    return Err(nitro_wire::Error::Rejected {
                        code: e.code,
                        msg: e.msg,
                    }
                    .into());
                }
            }
            Err(e) => return Err(e.into()),
        }
        let n = batch.len();
        for msg in batch.drain(..) {
            self.dispatch(state, &msg);
        }
        // Append rather than assign: a handler may have called
        // `measure_text`, and the round trip parks whatever else arrived
        // while it waited in `stray`. Overwriting it here would drop the
        // very keystroke the queue exists to keep.
        self.wire.stray.append(&mut batch);
        self.deliver_focus_events(state);
        self.run_deferred(state);
        // A latched error is fatal by construction (the non-fatal codes
        // never latch), and the server closes the socket right behind it.
        // Report it now rather than waiting for the EOF: a `flush` later
        // in the same iteration would otherwise fail with a bare `EPIPE`
        // and bury the reason the server gave.
        if let Some(e) = self.last_server_error.take() {
            self.quit = true;
            return Err(nitro_wire::Error::Rejected {
                code: e.code,
                msg: e.msg,
            }
            .into());
        }
        Ok(n)
    }

    /// Turn one `ServerMsg` into widget events.
    pub fn dispatch(&mut self, state: &mut S, msg: &ServerMsg) {
        match msg {
            ServerMsg::Configure(c) => {
                self.scale = c.scale;
                self.window_position = c.position;
                let changed = self.window_size != c.size;
                self.resize(c.size);
                // After the resize, so a handler sees the new size in
                // `window_size` and can mark the tree itself; and only
                // when the size actually moved, because the server also
                // sends a `Configure` for a move or a scale change and
                // an app should not reflow its content for those.
                if changed {
                    self.dispatch_resize(state, c.size);
                }
            }
            // Only our own window: the toolkit binds exactly one
            // (`WINDOW`, see `open_window`), and a `Closed` naming any
            // other id is not ours to act on. Quitting on it turned every
            // stray into the quietest exit in the tree — exit 0, no
            // output.
            ServerMsg::Closed(c) if c.window == WINDOW => self.quit = true,
            ServerMsg::Closed(c) => {
                eprintln!(
                    "nitro-ui: ignoring Closed for window {} (ours is {})",
                    c.window.raw(),
                    WINDOW.raw()
                );
            }
            // The desktop's colours changed (or arrived for the first
            // time, right behind the `Welcome`). Not routed to a widget:
            // every widget is affected, so this marks the whole tree and
            // the next flush pays for it once.
            ServerMsg::Theme(t) => {
                // Only when it moved: a handler may do real work (the
                // terminal's damages its whole grid), so a server that
                // re-sent an unchanged palette would otherwise cost a
                // settled app a full repaint. `set_palette` answers
                // whether anything changed.
                if self.set_palette(t.palette()) {
                    self.dispatch_theme(state);
                }
            }
            ServerMsg::PointerEnter(e) => self.pointer_move(state, e.pos),
            ServerMsg::PointerMotion(e) => self.pointer_move(state, e.pos),
            ServerMsg::PointerLeave(_) => self.pointer_leave(state),
            ServerMsg::PointerButton(b) => {
                self.pointer_button(state, b.button, b.state == ButtonState::Pressed);
            }
            ServerMsg::PointerAxis(a) => {
                let ev = Event::Scroll { dx: a.dx, dy: a.dy };
                let target = self.hover_chain.last().copied();
                if let Some(t) = target {
                    self.bubble(state, t, &ev);
                }
            }
            ServerMsg::Key(k) => self.key(state, k),
            ServerMsg::Frame(f) => self.dispatch_frame(
                state,
                Frame {
                    deadline_ns: f.deadline_ns,
                    refresh_ns: f.refresh_ns,
                },
            ),
            ServerMsg::Focus(f) if !f.focused => self.blur(state),
            // The shell socket's news. Not input, so not routed to a
            // widget: offered to the handlers `on_shell` registered.
            ServerMsg::WindowInfo(i) => {
                self.dispatch_shell(state, &crate::shell::ShellEvent::Window(i.clone()));
            }
            ServerMsg::WindowGone(g) => {
                self.dispatch_shell(state, &crate::shell::ShellEvent::WindowGone(g.window));
            }
            ServerMsg::WindowListEnd(_) => {
                self.dispatch_shell(state, &crate::shell::ShellEvent::WindowListEnd);
            }
            ServerMsg::OutputInfo(o) => {
                self.dispatch_shell(state, &crate::shell::ShellEvent::Output(o.clone()));
            }
            ServerMsg::OutputGone(o) => {
                self.dispatch_shell(state, &crate::shell::ShellEvent::OutputGone(o.id));
            }
            ServerMsg::OutputsEnd(_) => {
                self.dispatch_shell(state, &crate::shell::ShellEvent::OutputsEnd);
            }
            ServerMsg::HotKey(h) => {
                self.dispatch_shell(
                    state,
                    &crate::shell::ShellEvent::HotKey {
                        id: h.id,
                        pressed: h.pressed,
                    },
                );
            }
            // An unknown icon name. One of the protocol's two **non-fatal**
            // errors: the node is cleared, the rest of the transaction
            // applied, the connection kept — so this is news, not a
            // failure, and it is the cue for `.fallback(…)`.
            ServerMsg::Error(e) if e.code == ErrorCode::BadIcon => {
                self.bad_icon(e);
            }
            ServerMsg::Error(e) => self.server_error(e),
            _ => {}
        }
    }

    /// Report a `ServerMsg::Error` the toolkit cannot act on, and latch
    /// it so [`Ui::pump`] can turn it into the loop's return error.
    ///
    /// Every code but `BadIcon` (handled by the arm above) and
    /// `BadBuffer` on a remote link is fatal by the protocol's
    /// definition — the server closes the socket right behind it, see
    /// `docs/wire.md` and `Server::disconnect`. The remote `BadBuffer`
    /// carve-out is `docs/remote.md` §3: the server sends it and *keeps*
    /// the client, so latching it would make an unrelated later EOF
    /// report the wrong reason.
    fn server_error(&mut self, e: &nitro_wire::msg::Error) {
        eprintln!("nitro-ui: server error {:?}: {}", e.code, e.msg);
        if e.code == ErrorCode::BadBuffer && self.is_remote() {
            return;
        }
        self.last_server_error = Some(e.clone());
    }

    /// Route an `Error { BadIcon }` to the widget (or widgets) that asked
    /// for the name the server refused, and let each take its fallback.
    ///
    /// **Why it is keyed on the name.** `Error` carries `serial`, `code`
    /// and a human-readable `msg` and *no node id* — see
    /// `nitro_wire::msg::Error` and the server's `report_bad_icons`,
    /// which formats `no icon named "foo"` and sends it after the batch.
    /// So there is genuinely nothing in the message to key a widget off
    /// directly, and the two honest options are both here:
    ///
    /// 1. Parse the quoted name out of `msg` and match the widgets whose
    ///    icon slot currently holds exactly that name. `msg` is
    ///    documented as "for logs and never parsed", which this is in
    ///    tension with — so what is parsed is the **quoted name**, never
    ///    the prose around it: the sentence may be reworded freely and
    ///    this still works, and a message with no quoted name falls to
    ///    (2) rather than misfiring.
    /// 2. Failing that, offer the fallback to every widget whose icon has
    ///    not been answered for yet. That is a superset of the right
    ///    answer and it is bounded by the same exactly-once latch, so the
    ///    worst case is a widget taking its fallback one error early.
    ///
    /// Both are keyed on the tree's own record of what it sent (the paint
    /// slots hold the last `SetIcon` per slot), not on a guess about
    /// ordering: a serial covers a whole transaction and several icons
    /// can be refused in one.
    ///
    /// The fix for the tension is a node id on `Error`, which is a wire
    /// change and therefore a task of its own — noted rather than
    /// smuggled in here.
    fn bad_icon(&mut self, e: &nitro_wire::msg::Error) {
        let refused = quoted(&e.msg);
        // Collected first, then mutated: `take_fallback` needs the widget
        // out of its slot, and the walk needs the arena.
        let hit: Vec<WidgetId> = (0..self.arena.slots.len())
            .filter_map(|i| {
                let slot = self.arena.slots.get(i)?;
                if !slot.alive {
                    return None;
                }
                let id = WidgetId {
                    index: i as u32,
                    generation: slot.generation,
                };
                // What this widget last put on the wire. A widget with no
                // icon slot never matches, whatever the message said.
                let sent = slot
                    .state
                    .slots
                    .iter()
                    .filter_map(crate::wire::PaintSlot::icon_name)
                    .any(|n| refused.is_none_or(|r| r == n));
                sent.then_some(id)
            })
            .collect();
        for id in hit {
            let took = match self.arena.slot_mut(id).and_then(|s| s.widget.as_mut()) {
                Some(w) => {
                    let any = w.as_any_mut();
                    if let Some(icon) = any.downcast_mut::<crate::widgets::Icon>() {
                        icon.take_fallback()
                    } else if let Some(b) = any.downcast_mut::<crate::widgets::Button<S>>() {
                        b.take_icon_fallback()
                    } else {
                        false
                    }
                }
                None => false,
            };
            if took {
                // Through the **normal paint path**, deliberately: an
                // out-of-band `SetIcon` would leave the slot's cached
                // name saying the old one, so the next repaint would diff
                // against a lie and re-send the name that just failed.
                // Marking the widget is how every other state change in
                // this toolkit reaches the wire, and it makes the retry
                // idempotent — several `BadIcon`s before the next flush
                // cost one `SetIcon`, because the latch has already
                // fired.
                self.mark(id, Dirty::PAINT);
            }
        }
    }

    /// Route a pointer position: hover bookkeeping, then a move event
    /// offered deepest-first.
    pub fn pointer_move(&mut self, state: &mut S, pos: Point) {
        let mut chain = std::mem::take(&mut self.chain);
        self.hit_chain(pos, &mut chain);
        let new: Vec<WidgetId> = chain.iter().map(|(id, _)| *id).collect();
        let old = std::mem::take(&mut self.hover_chain);
        for id in old.iter().rev() {
            if !new.contains(id) {
                self.set_hovered(*id, false);
                self.bubble_one(state, *id, &Event::PointerLeave);
            }
        }
        for (id, local) in &chain {
            if !old.contains(id) {
                self.set_hovered(*id, true);
                self.bubble_one(state, *id, &Event::PointerEnter { pos: *local });
            }
        }
        self.hover_chain = new;
        for (id, local) in chain.iter().rev() {
            if self
                .bubble_one(state, *id, &Event::PointerMove { pos: *local })
                .is_handled()
            {
                break;
            }
        }
        // The chain is **kept**, not returned to the scratch pool: it is
        // where a later `PointerDown` learns the position inside each
        // widget (`local_pos`), and the button that arrives after the
        // move is a separate message. Clearing it here is why an early
        // version reported every press at the widget's top-left corner,
        // which no widget noticed until one cared *where* it was
        // clicked. `hit_chain` clears it on the next move.
        self.chain = chain;
    }

    /// The pointer left the window.
    pub fn pointer_leave(&mut self, state: &mut S) {
        self.chain.clear();
        let old = std::mem::take(&mut self.hover_chain);
        for id in old.iter().rev() {
            self.set_hovered(*id, false);
            self.bubble_one(state, *id, &Event::PointerLeave);
        }
    }

    /// Route a button press or release to the hovered chain.
    pub fn pointer_button(&mut self, state: &mut S, button: u32, pressed: bool) {
        let chain: Vec<WidgetId> = self.hover_chain.clone();
        for id in chain.iter().rev() {
            let local = self.local_pos(*id);
            let ev = if pressed {
                Event::PointerDown { pos: local, button }
            } else {
                Event::PointerUp { pos: local, button }
            };
            if self.bubble_one(state, *id, &ev).is_handled() {
                break;
            }
        }
    }

    /// Route a key to the focused widget, bubbling to the root, then to
    /// the app's own handlers. A pressed, unhandled `Tab` (including
    /// Shift-Tab) finally falls back to framework focus traversal; a widget
    /// or app handler can take it to override that behavior.
    ///
    /// The order for a press is the whole of the key contract:
    ///
    /// 1. `KeyDown` from the focused widget (or the root, with nothing
    ///    focused) up through its ancestors;
    /// 2. if nobody took it and the key produced text, `Event::Text` the
    ///    same way — which is how a text field types a `q` that an app
    ///    also uses as a shortcut;
    /// 3. only then the handlers registered with [`Ui::on_key`] and
    ///    [`Ui::set_shortcut`], in registration order;
    /// 4. for a pressed, still-unhandled Tab or Shift-Tab, focus traversal.
    ///
    /// A widget therefore always wins over an app shortcut, and an app
    /// shortcut always gets the keys no widget wanted.
    pub fn key(&mut self, state: &mut S, k: &nitro_wire::msg::Key) {
        let pressed = k.state == ButtonState::Pressed;
        let ev = KeyEvent {
            keycode: k.keycode,
            keysym: k.keysym,
            mods: k.mods,
            text: k.utf8.clone(),
        };
        let target = self.focused.or(self.root);
        let mut handled = Handled::No;
        if let Some(target) = target {
            handled = if pressed {
                self.bubble(state, target, &Event::KeyDown(ev.clone()))
            } else {
                self.bubble(state, target, &Event::KeyUp(ev.clone()))
            };
            if pressed && !handled.is_handled() && !ev.text.is_empty() {
                handled = self.bubble(
                    state,
                    target,
                    &Event::Text {
                        text: ev.text.clone(),
                    },
                );
            }
        }
        // Releases are not offered: a shortcut that fired on the press
        // and again on the release would run twice, and the handler
        // signature has no way to tell the two apart.
        if pressed && !handled.is_handled() {
            handled = self.run_key_handlers(state, &ev);
        }
        if pressed && k.keycode == key::TAB && !handled.is_handled() {
            self.focus_next(state, ev.shift());
        }
    }

    /// Register an app-level key handler.
    ///
    /// It is offered every press **no widget took** — after the focused
    /// chain has declined both the `KeyDown` and the `Event::Text` it
    /// produced (see [`Ui::key`]) — and handlers run in registration
    /// order until one answers [`Handled::Yes`].
    ///
    /// This is what a global shortcut is: keys bubble *upward* from the
    /// focused widget, so no widget in the tree can see a key the focused
    /// subtree never passed on, and an app that needs one is not asking
    /// about a widget at all.
    ///
    /// A shortcut that quits is spelled with a modifier: a bare letter
    /// closing an app is one stray keystroke away from losing the user's
    /// work, and keys do land in the wrong window.
    ///
    /// ```no_run
    /// # use nitro_ui::event::{Handled, KeyEvent, key, mods};
    /// # use nitro_ui::Ui;
    /// # fn demo<S: 'static>(ui: &mut Ui<S>) {
    /// ui.on_key(|_s: &mut S, ui: &mut Ui<S>, k: &KeyEvent| {
    ///     if k.keycode == key::Q && k.mods & mods::MASK == mods::CTRL {
    ///         ui.quit();
    ///         return Handled::Yes;
    ///     }
    ///     Handled::No
    /// });
    /// # }
    /// ```
    pub fn on_key(
        &mut self,
        handler: impl FnMut(&mut S, &mut Ui<S>, &KeyEvent) -> Handled + 'static,
    ) {
        self.key_handlers.push(Some(Box::new(handler)));
    }

    /// Register an app-level shortcut: one modifier combination, one
    /// keycode, one callback.
    ///
    /// Sugar over [`Ui::on_key`] with the same ordering. `modifiers` is
    /// an exact match over [`mods::MASK`](crate::event::mods::MASK), so
    /// `mods::NONE` means *no* modifier and `Ctrl-Q` does not fire a
    /// plain `Q` shortcut; the bits outside the mask (`Lock`, `NumLock`,
    /// the layout's group) are ignored, so Caps Lock does not disable an
    /// app's shortcuts.
    ///
    /// ```no_run
    /// # use nitro_ui::event::{key, mods};
    /// # use nitro_ui::Ui;
    /// # fn demo<S: 'static>(ui: &mut Ui<S>) {
    /// ui.set_shortcut(mods::NONE, key::ESC, |_s: &mut S, ui: &mut Ui<S>| ui.quit());
    /// ui.set_shortcut(mods::CTRL, key::Q, |_s: &mut S, ui: &mut Ui<S>| ui.quit());
    /// # }
    /// ```
    pub fn set_shortcut(
        &mut self,
        modifiers: u32,
        keycode: u32,
        mut handler: impl FnMut(&mut S, &mut Ui<S>) + 'static,
    ) {
        let wanted = modifiers & mods::MASK;
        self.on_key(move |s, ui, k| {
            if k.keycode == keycode && k.mods & mods::MASK == wanted {
                handler(s, ui);
                Handled::Yes
            } else {
                Handled::No
            }
        });
    }

    /// How many app-level key handlers are registered.
    #[must_use]
    pub fn key_handler_count(&self) -> usize {
        self.key_handlers.len()
    }

    // -- the shell socket ---------------------------------------------

    /// Register a handler for [`ShellEvent`](crate::shell::ShellEvent)s:
    /// window-list changes, output hotplug and hotkeys.
    ///
    /// A handler is handed `&mut S` and `&mut Ui<S>`, exactly like a
    /// button's `on_click`, so it edits the tree rather than only setting
    /// a flag — which is what lets a bar's window list be ordinary tree
    /// code.
    ///
    /// This is a list rather than a widget hung off the root, for the
    /// same reason [`Ui::on_key`] is: these events have no widget to be
    /// routed to. A `WindowInfo` is news about *somebody else's* window,
    /// and there is no position to hit-test and no focus to follow.
    pub fn on_shell(
        &mut self,
        handler: impl FnMut(&mut S, &mut Ui<S>, &crate::shell::ShellEvent) + 'static,
    ) {
        self.shell_handlers.push(Some(Box::new(handler)));
    }

    /// How many shell handlers are registered.
    #[must_use]
    pub fn shell_handler_count(&self) -> usize {
        self.shell_handlers.len()
    }

    /// Offer a shell event to every handler, in registration order.
    ///
    /// Every handler sees every event — unlike a key, which stops at the
    /// first taker. There is nothing to consume: two handlers interested
    /// in the window list are both right, and a "handled" answer would
    /// only let the first one silently starve the second.
    pub fn dispatch_shell(&mut self, state: &mut S, ev: &crate::shell::ShellEvent) {
        let mut i = 0;
        // By index rather than over an iterator: a handler holds `&mut
        // Ui<S>` and may register another one.
        while i < self.shell_handlers.len() {
            let Some(mut h) = self.shell_handlers[i].take() else {
                i += 1;
                continue;
            };
            h(state, self, ev);
            if i < self.shell_handlers.len() {
                self.shell_handlers[i] = Some(h);
            }
            i += 1;
        }
    }

    /// Ask for the window list and subscribe to its changes.
    ///
    /// Answered with one
    /// [`ShellEvent::Window`](crate::shell::ShellEvent::Window) per
    /// window then a `WindowListEnd`; afterwards changes arrive unasked,
    /// so a bar never polls.
    ///
    /// # Errors
    /// A wire failure. Sending this on an unprivileged connection is a
    /// fatal protocol error at the server — check [`Ui::is_shell`].
    pub fn window_list(&mut self) -> Result<(), Error> {
        self.wire.send_now(&nitro_wire::msg::ClientMsg::WindowList(
            nitro_wire::msg::WindowList,
        ))
    }

    /// Ask for the output list and subscribe to hotplug.
    ///
    /// # Errors
    /// As [`Ui::window_list`].
    pub fn outputs(&mut self) -> Result<(), Error> {
        self.wire.send_now(&nitro_wire::msg::ClientMsg::Outputs(
            nitro_wire::msg::Outputs,
        ))
    }

    /// Give keyboard focus to another client's window.
    ///
    /// Silently refused by the server when it cannot be honoured — a
    /// `NO_FOCUS` or minimized window, a stale ref — on exactly the terms
    /// a click on it would be. There is no per-request error in this
    /// protocol, and a bar's window list must not be able to wedge the
    /// keyboard by naming the wrong row.
    ///
    /// # Errors
    /// As [`Ui::window_list`].
    pub fn focus_window(&mut self, window: nitro_wire::types::WindowRef) -> Result<(), Error> {
        self.wire.send_now(&nitro_wire::msg::ClientMsg::FocusWindow(
            nitro_wire::msg::FocusWindow { window },
        ))
    }

    /// Ask another client's window to close.
    ///
    /// A *request*: the owning client is told and decides, so unsaved
    /// work survives a misclick in a task list.
    ///
    /// # Errors
    /// As [`Ui::window_list`].
    pub fn close_window(&mut self, window: nitro_wire::types::WindowRef) -> Result<(), Error> {
        self.wire.send_now(&nitro_wire::msg::ClientMsg::CloseWindow(
            nitro_wire::msg::CloseWindow { window },
        ))
    }

    /// Put another client's window into a state.
    ///
    /// # Errors
    /// As [`Ui::window_list`].
    pub fn set_window_state_for(
        &mut self,
        window: nitro_wire::types::WindowRef,
        state: crate::shell::WindowState,
    ) -> Result<(), Error> {
        self.wire
            .send_now(&nitro_wire::msg::ClientMsg::SetWindowStateFor(
                nitro_wire::msg::SetWindowStateFor { window, state },
            ))
    }

    /// Bind a server-global hotkey.
    ///
    /// `mods` is a [`nitro_wire::types::mod_mask`] bitmask, **not** the
    /// xkb mask a [`KeyEvent`] carries: xkb's serialized mask depends on
    /// the compiled keymap, so it cannot be compared against a constant.
    /// `keysym: 0` asks for the bare-modifier **tap**, which fires once,
    /// on the release.
    ///
    /// While bound, the chord is not delivered to the focused client at
    /// all — a global hotkey the focused application could also see would
    /// be a keylogger and an ambiguity at once.
    ///
    /// # Errors
    /// As [`Ui::window_list`].
    pub fn bind_key(&mut self, id: u32, mods: u32, keysym: u32) -> Result<(), Error> {
        self.wire.send_now(&nitro_wire::msg::ClientMsg::BindKey(
            nitro_wire::msg::BindKey { id, mods, keysym },
        ))
    }

    /// Release a hotkey binding. Unbinding an id that is not bound is a
    /// deliberate no-op: a shell shutting down should not have to
    /// remember what it managed to bind.
    ///
    /// # Errors
    /// As [`Ui::window_list`].
    pub fn unbind_key(&mut self, id: u32) -> Result<(), Error> {
        self.wire.send_now(&nitro_wire::msg::ClientMsg::UnbindKey(
            nitro_wire::msg::UnbindKey { id },
        ))
    }

    /// Offer a key to the app-level handlers, in registration order.
    fn run_key_handlers(&mut self, state: &mut S, ev: &KeyEvent) -> Handled {
        let mut i = 0;
        // By index rather than over an iterator: a handler holds `&mut
        // Ui<S>` and may register another one, and handlers are never
        // removed, so an index stays valid across the call.
        while i < self.key_handlers.len() {
            // Out of its slot for the duration, for the reason a widget
            // is: it cannot be reachable through the tree it is handed.
            let Some(mut h) = self.key_handlers[i].take() else {
                i += 1;
                continue;
            };
            let handled = h(state, self, ev);
            self.key_handlers[i] = Some(h);
            if handled.is_handled() {
                return Handled::Yes;
            }
            i += 1;
        }
        Handled::No
    }

    /// Offer an event to `id` and then to each ancestor until one takes
    /// it.
    fn bubble(&mut self, state: &mut S, id: WidgetId, ev: &Event) -> Handled {
        let mut cur = Some(id);
        while let Some(w) = cur {
            if self.bubble_one(state, w, ev).is_handled() {
                return Handled::Yes;
            }
            cur = self.parent(w);
        }
        Handled::No
    }

    fn bubble_one(&mut self, state: &mut S, id: WidgetId, ev: &Event) -> Handled {
        let Some(slot) = self.arena.slot(id) else {
            return Handled::No;
        };
        let bounds = slot.state.bounds;
        let Ok(mut widget) = self.take(id) else {
            return Handled::No;
        };
        let handled = {
            let mut cx = EventCx {
                ui: self,
                state,
                id,
                bounds,
            };
            widget.event(&mut cx, ev)
        };
        self.untake(id, widget);
        handled
    }

    fn set_hovered(&mut self, id: WidgetId, hovered: bool) {
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.hovered = hovered;
        }
    }

    /// Where the pointer sits inside `id`, from the last hover walk.
    fn local_pos(&self, id: WidgetId) -> Point {
        self.chain
            .iter()
            .find(|(w, _)| *w == id)
            .map_or(Point::ZERO, |(_, p)| *p)
    }

    /// The chain of widgets under `pos`, outermost first, each with the
    /// position in its own coordinate space.
    ///
    /// A widget's **content transform** is applied on the way down: a
    /// scrolled viewport leaves its children's bounds alone and moves
    /// the group they hang under, so the point has to travel the same
    /// way the pixels did or a scrolled row is clickable where it used
    /// to be.
    fn hit_chain(&self, pos: Point, out: &mut Vec<(WidgetId, Point)>) {
        out.clear();
        let Some(root) = self.root else { return };
        let mut id = root;
        let mut p = pos;
        loop {
            let Some(slot) = self.arena.slot(id) else {
                return;
            };
            let b = slot.state.bounds;
            let local = Point::new(p.x - b.x, p.y - b.y);
            if local.x < 0.0 || local.y < 0.0 || local.x >= b.w || local.y >= b.h {
                return;
            }
            out.push((id, local));
            // Children are positioned inside the content group, so the
            // point enters their space with the group's transform undone.
            // M2 only ever produces a translation.
            let t = slot.state.content_transform;
            let inner = Point::new(local.x - t.e, local.y - t.f);
            // Later children are on top, so the deepest hit is found by
            // walking backwards.
            let mut next = None;
            for c in slot.state.children.iter().rev() {
                if let Some(cs) = self.arena.slot(*c)
                    && cs.state.visible
                    && cs.state.bounds.contains(inner)
                {
                    next = Some(*c);
                    break;
                }
            }
            match next {
                Some(c) => {
                    id = c;
                    p = inner;
                }
                None => return,
            }
        }
    }

    // -- focus --------------------------------------------------------

    /// Whether `id` has the keyboard focus.
    #[must_use]
    pub fn is_focused(&self, id: WidgetId) -> bool {
        self.focused == Some(id)
    }

    /// Whether the pointer is inside `id`.
    #[must_use]
    pub fn is_hovered(&self, id: WidgetId) -> bool {
        self.arena.slot(id).is_some_and(|s| s.state.hovered)
    }

    /// The focused widget, if any.
    #[must_use]
    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    /// Move the focus to `id`.
    ///
    /// Callable without an `&mut S`, which is what
    /// [`EventCx::request_focus`](crate::EventCx::request_focus) needs:
    /// the state is already borrowed there, and the widget asking is out
    /// of its slot. The `FocusChanged` events are therefore **queued**
    /// and delivered by [`Ui::deliver_focus_events`], which `pump` calls
    /// once the dispatch that caused them has finished and every widget
    /// is back in its slot.
    pub fn focus(&mut self, id: WidgetId) {
        if self.focused == Some(id) {
            return;
        }
        if let Some(old) = self.focused
            && let Some(slot) = self.arena.slot_mut(old)
        {
            slot.state.focused = false;
            self.mark(old, Dirty::PAINT);
            self.pending_focus.push((old, false));
        }
        self.focused = Some(id);
        if let Some(slot) = self.arena.slot_mut(id) {
            slot.state.focused = true;
        }
        self.mark(id, Dirty::PAINT);
        self.pending_focus.push((id, true));
    }

    /// Run `callback` once the current dispatch is over and every widget
    /// is back in its arena slot.
    ///
    /// **This is how a widget's callback changes that same widget.**
    /// Take-out dispatch moves a widget out of its slot for the duration
    /// of its own `event`, `action` or app callback, which is what makes
    /// `Fn(&mut S, &mut Ui<S>)` possible at all — and it means the one
    /// widget the callback cannot reach is itself: `widget_mut` answers
    /// [`Error::Busy`], a value rather than a panic or a second mutable
    /// borrow. For most widgets that is the end of it, because the
    /// callback changes something *else*.
    ///
    /// A list is the case where it is not. "Activate this row" means
    /// "show a different directory **in this list**", and a file manager
    /// that wrote the new rows from inside `on_activate` wrote them into
    /// an `Err` it never looked at: the path bar updated, the rows did
    /// not, and nothing anywhere returned an error anybody read
    /// (`crates/nitro-files/tests/files.rs` is where that was caught).
    ///
    /// So: queue it. The callback is handed `&mut S` and `&mut Ui<S>`
    /// like every other, and runs from [`Ui::run_deferred`] with the
    /// tree whole. This is the same mechanism [`Ui::focus`] has always
    /// used for the same reason — a focus notification cannot be
    /// delivered to a widget that is out of its slot either — rather
    /// than a second one invented alongside it.
    ///
    /// Deferring from *inside* a deferred callback is allowed and drains
    /// in the same pass, bounded by [`Ui::MAX_DEFER_ROUNDS`] so a
    /// callback that re-queues itself for ever is cut off rather than
    /// hanging the loop.
    pub fn defer(&mut self, callback: impl FnOnce(&mut S, &mut Ui<S>) + 'static) {
        self.pending_deferred.push(Box::new(callback));
    }

    /// Run everything queued with [`Ui::defer`].
    ///
    /// Called wherever [`Ui::deliver_focus_events`] is — after an event
    /// batch, after an introspection action, and by the app loop — so
    /// app code never has to know the queue exists.
    pub fn run_deferred(&mut self, state: &mut S) {
        for _ in 0..MAX_DEFER_ROUNDS {
            if self.pending_deferred.is_empty() {
                return;
            }
            for cb in std::mem::take(&mut self.pending_deferred) {
                cb(state, self);
            }
        }
        // Still refilling after sixteen rounds: something re-queues
        // itself unconditionally. Drop what is left rather than spin,
        // and say so, because silence here is a UI that stops updating
        // for no visible reason.
        if !self.pending_deferred.is_empty() {
            self.pending_deferred.clear();
            eprintln!(
                "nitro-ui: deferred callbacks still queuing after {MAX_DEFER_ROUNDS} rounds; dropped the rest"
            );
        }
    }

    /// How many callbacks are waiting, for tests.
    #[must_use]
    pub fn deferred_count(&self) -> usize {
        self.pending_deferred.len()
    }

    /// Deliver the [`Event::FocusChanged`] events queued by [`Ui::focus`].
    ///
    /// Separate from `focus` because focus is most often taken from
    /// inside a widget's own `event`, where that widget is out of its
    /// slot and could not receive the notification. `pump` drains the
    /// queue after each batch; an app driving `Ui` by hand should too.
    pub fn deliver_focus_events(&mut self, state: &mut S) {
        while !self.pending_focus.is_empty() {
            let queued = std::mem::take(&mut self.pending_focus);
            for (id, focused) in queued {
                // Skip a notification the tree has already overtaken:
                // focus may have moved again before we got here.
                if self.is_focused(id) != focused {
                    continue;
                }
                self.bubble_one(state, id, &Event::FocusChanged { focused });
            }
        }
    }

    /// Drop the focus without an `&mut S`, for a setter that finds the
    /// focused widget has just been hidden (`Pages::show`). The
    /// `FocusChanged` is queued like [`Ui::focus`]'s and delivered by
    /// [`Ui::deliver_focus_events`].
    pub fn unfocus(&mut self) {
        if let Some(old) = self.focused.take() {
            if let Some(slot) = self.arena.slot_mut(old) {
                slot.state.focused = false;
            }
            self.mark(old, Dirty::PAINT);
            self.pending_focus.push((old, false));
        }
    }

    /// Drop the focus entirely.
    pub fn blur(&mut self, state: &mut S) {
        if let Some(old) = self.focused.take() {
            if let Some(slot) = self.arena.slot_mut(old) {
                slot.state.focused = false;
            }
            self.mark(old, Dirty::PAINT);
            self.bubble_one(state, old, &Event::FocusChanged { focused: false });
        }
    }

    /// Move the focus to the next (or previous) focusable widget, in tree
    /// order. Wraps around.
    pub fn focus_next(&mut self, state: &mut S, backwards: bool) {
        let order = self.focus_order();
        if order.is_empty() {
            return;
        }
        let cur = self
            .focused
            .and_then(|f| order.iter().position(|id| *id == f));
        let next = match (cur, backwards) {
            (Some(i), false) => (i + 1) % order.len(),
            (Some(i), true) => (i + order.len() - 1) % order.len(),
            (None, false) => 0,
            (None, true) => order.len() - 1,
        };
        let target = order[next];
        if self.focused == Some(target) {
            return;
        }
        if let Some(old) = self.focused.take() {
            if let Some(slot) = self.arena.slot_mut(old) {
                slot.state.focused = false;
            }
            self.mark(old, Dirty::PAINT);
            self.bubble_one(state, old, &Event::FocusChanged { focused: false });
        }
        self.focused = Some(target);
        self.pending_focus.retain(|(id, _)| *id != target);
        if let Some(slot) = self.arena.slot_mut(target) {
            slot.state.focused = true;
        }
        self.mark(target, Dirty::PAINT);
        self.bubble_one(state, target, &Event::FocusChanged { focused: true });
    }

    /// Every focusable widget, in pre-order: the Tab order.
    #[must_use]
    pub fn focus_order(&self) -> Vec<WidgetId> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.collect_focusable(root, &mut out);
        }
        out
    }

    fn collect_focusable(&self, id: WidgetId, out: &mut Vec<WidgetId>) {
        let Some(slot) = self.arena.slot(id) else {
            return;
        };
        // A hidden subtree is not on screen, so nothing in it can take
        // the focus: Tab would otherwise land on a field nobody can see.
        if !slot.state.visible {
            return;
        }
        if slot.state.focusable {
            out.push(id);
        }
        for c in &slot.state.children {
            self.collect_focusable(*c, out);
        }
    }

    fn all_ids(&self) -> Vec<WidgetId> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.collect_subtree(root, &mut out);
        }
        out
    }

    // -- app-owned fds and timers -------------------------------------

    /// Call `callback` whenever `fd` is readable.
    ///
    /// The descriptor is `dup`ped, so the app may close its own copy;
    /// the returned [`FdToken`] is what names the hook afterwards, for
    /// [`Ui::run_fd`] and [`Ui::remove_fd`].
    ///
    /// # Errors
    /// If the descriptor cannot be duplicated.
    pub fn add_fd(
        &mut self,
        fd: BorrowedFd<'_>,
        callback: impl FnMut(&mut S, &mut Ui<S>) + 'static,
    ) -> Result<FdToken, Error> {
        let owned = rustix::io::dup(fd)?;
        let id = self.next_fd_token;
        self.next_fd_token += 1;
        let token = FdToken(id);
        self.fds.push(FdHook {
            id,
            fd: owned,
            callback: Some(Box::new(callback)),
        });
        Ok(token)
    }

    /// Drop a descriptor hook. Closing our duplicate also removes it
    /// from the app loop's `epoll` set.
    pub fn remove_fd(&mut self, token: FdToken) {
        self.fds.retain(|h| h.id != token.0);
    }

    /// Call `callback` once, `ms` milliseconds from now.
    pub fn set_timer(
        &mut self,
        ms: u64,
        callback: impl FnOnce(&mut S, &mut Ui<S>) + 'static,
    ) -> TimerId {
        let id = self.next_timer;
        self.next_timer += 1;
        self.timers.push(Timer {
            id,
            deadline: Instant::now() + Duration::from_millis(ms),
            callback: Some(Box::new(callback)),
        });
        TimerId(id)
    }

    /// Cancel a timer that has not fired.
    pub fn cancel_timer(&mut self, timer: &TimerId) {
        self.timers.retain(|t| t.id != timer.0);
    }

    /// The descriptors registered with [`Ui::add_fd`], with a borrow of
    /// each so the loop can hand them to `epoll`.
    pub(crate) fn hook_fds(&self) -> Vec<(u64, BorrowedFd<'_>)> {
        self.fds.iter().map(|h| (h.id, h.fd.as_fd())).collect()
    }

    /// Bring every pending timer `by` closer to firing, as though that
    /// much time had passed.
    ///
    /// For tests that supply their own clock. A timer's deadline is an
    /// `Instant` from the monotonic clock, which no amount of faking a
    /// *wall* clock moves — so a test of the bar's minute-aligned tick
    /// would otherwise have to wait a real minute to see it. Shifting the
    /// deadlines is the honest form of that fast-forward: it preserves
    /// the relative order of the timers and fires exactly the ones that
    /// the elapsed time would have.
    pub fn advance_timers(&mut self, by: Duration) {
        // Saturating, because `Instant` arithmetic panics on underflow and
        // this is a public test-support API: fast-forwarding an hour on a
        // process that has been up for a minute means "fire everything",
        // not "abort". A saturated deadline is `now`, which is due on the
        // next `run_timers` — exactly what the elapsed time would have
        // done, and the relative order of the timers that did *not*
        // saturate is untouched.
        let floor = Instant::now();
        for t in &mut self.timers {
            t.deadline = t.deadline.checked_sub(by).unwrap_or(floor);
        }
    }

    /// Milliseconds until the next timer, or `None` when there is none.
    #[must_use]
    pub fn next_timeout(&self) -> Option<u64> {
        self.timers
            .iter()
            .map(|t| t.deadline)
            .min()
            .map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u64)
    }

    /// Run every timer whose deadline has passed. The app loop calls it
    /// after each wakeup.
    pub fn run_timers(&mut self, state: &mut S) {
        let now = Instant::now();
        let due: Vec<u64> = self
            .timers
            .iter()
            .filter(|t| t.deadline <= now)
            .map(|t| t.id)
            .collect();
        for id in due {
            let Some(i) = self.timers.iter().position(|t| t.id == id) else {
                continue;
            };
            let mut t = self.timers.remove(i);
            if let Some(cb) = t.callback.take() {
                cb(state, self);
            }
        }
        // A timer's callback is app code with the same rights as a
        // button's, so it may defer too.
        self.run_deferred(state);
    }

    /// Run the callback registered for `token`. The app loop calls it
    /// when `epoll` says the descriptor is readable; an unknown token is
    /// ignored, because a hook may have been removed between the wakeup
    /// and the dispatch.
    pub fn run_fd(&mut self, state: &mut S, token: FdToken) {
        let id = token.0;
        let Some(i) = self.fds.iter().position(|h| h.id == id) else {
            return;
        };
        // Take the callback out for the same reason a widget leaves its
        // slot: it gets `&mut Ui` and must not alias itself.
        let Some(mut cb) = self.fds[i].callback.take() else {
            return;
        };
        cb(state, self);
        if let Some(h) = self.fds.iter_mut().find(|h| h.id == id) {
            h.callback = Some(cb);
        }
        // Same as a timer: a descriptor hook is app code, and
        // `nitro-files`'s scan hook writes rows into the very list whose
        // callback started the scan.
        self.run_deferred(state);
    }

    // -- scratch ------------------------------------------------------

    fn borrow_children(&mut self, id: WidgetId) -> Vec<WidgetId> {
        let mut v = self.id_pool.pop().unwrap_or_default();
        v.clear();
        if let Some(slot) = self.arena.slot(id) {
            v.extend_from_slice(&slot.state.children);
        }
        v
    }

    fn return_children(&mut self, mut v: Vec<WidgetId>) {
        v.clear();
        self.id_pool.push(v);
    }
}

/// Fold a widget's explicit `width`/`height` into the constraints its
/// parent offered.
fn apply_style(style: &LayoutStyle, c: Constraints) -> Constraints {
    let mut out = c;
    if let Some(w) = style.width.resolve(c.max.w) {
        out.min.w = w;
        out.max.w = w;
    }
    if let Some(h) = style.height.resolve(c.max.h) {
        out.min.h = h;
        out.max.h = h;
    }
    out.min = style.clamp(out.min);
    out.max = Size::new(
        clamp_max(out.max.w, style.min_width, style.max_width),
        clamp_max(out.max.h, style.min_height, style.max_height),
    );
    out
}

fn clamp_max(v: f32, min: Option<f32>, max: Option<f32>) -> f32 {
    let mut v = v;
    if let Some(m) = max {
        v = v.min(m);
    }
    if let Some(m) = min {
        v = v.max(m);
    }
    v
}

/// The text between the first pair of `"` in `s`, if there is one.
///
/// The one thing [`Ui::bad_icon`] reads out of an error's prose, and it
/// is deliberately the *quoted* part rather than a pattern over the
/// sentence: the server writes `no icon named "foo"` with `{name:?}`, and
/// a future rewording — a translation, an added hint — leaves the quoting
/// alone. A message with no quotes answers `None`, which is the cue to
/// fall back to the broader match rather than to match nothing.
///
/// It does not un-escape: `{:?}` escapes a `"` inside a name as `\"`, and
/// a name containing a quote would therefore be truncated here. That is
/// the right failure — it ends in the broad path, which is bounded — and
/// unescaping would be a second, subtler parser of a field documented as
/// never parsed.
fn quoted(s: &str) -> Option<&str> {
    let rest = s.split_once('"')?.1;
    rest.split_once('"').map(|(name, _)| name)
}

/// The only handle through which a widget's properties change.
///
/// Taking one moves the widget out of its arena slot, so the `&mut Ui` it
/// also carries cannot reach the same widget again; dropping it puts the
/// widget back. Every setter on it marks the widget layout- or
/// paint-dirty, which is the point: invalidation is not something an app
/// author can forget, because there is no other way in.
pub struct WidgetMut<'a, W, S: 'static> {
    ui: &'a mut Ui<S>,
    id: WidgetId,
    widget: Option<Box<dyn AnyWidget<S>>>,
    _marker: std::marker::PhantomData<fn() -> W>,
}

impl<W: Widget<S>, S: 'static> WidgetMut<'_, W, S> {
    /// The widget's id.
    #[must_use]
    pub fn id(&self) -> WidgetId {
        self.id
    }

    /// The rest of the tree.
    pub fn ui(&mut self) -> &mut Ui<S> {
        self.ui
    }

    /// Mark the widget for a repaint.
    pub fn request_paint(&mut self) {
        self.ui.mark(self.id, Dirty::PAINT);
    }

    /// Mark the widget for measurement and layout (and so for a repaint).
    pub fn request_layout(&mut self) {
        self.ui.mark(self.id, Dirty::LAYOUT | Dirty::PAINT);
    }

    /// Replace the flex style.
    pub fn set_style(&mut self, style: LayoutStyle) {
        if let Some(slot) = self.ui.arena.slot_mut(self.id) {
            slot.state.style = style;
        }
        self.request_layout();
    }

    /// Set the accessible name.
    pub fn set_name(&mut self, name: impl Into<String>) {
        if let Some(slot) = self.ui.arena.slot_mut(self.id) {
            slot.state.name = Some(name.into());
        }
    }

    /// Make the widget reachable (or not) by Tab.
    pub fn set_focusable(&mut self, focusable: bool) {
        if let Some(slot) = self.ui.arena.slot_mut(self.id) {
            slot.state.focusable = focusable;
        }
    }

    /// Move a **group** paint slot's children by `transform`, without a
    /// repaint.
    ///
    /// This is what makes scrolling cheap: the widget's content hangs
    /// under a group node, and shifting that group is exactly one
    /// `SetTransform` on the wire — no relayout, no repaint of anything
    /// inside it. Nothing happens if the slot does not exist yet or is
    /// not a group; the next paint creates it.
    pub fn set_slot_transform(
        &mut self,
        slot: crate::widget::Slot,
        transform: nitro_core::Transform,
    ) {
        let id = self.id;
        self.ui.set_slot_transform(id, slot, transform);
    }
}

impl<W: Widget<S>, S: 'static> std::ops::Deref for WidgetMut<'_, W, S> {
    type Target = W;
    fn deref(&self) -> &W {
        self.widget
            .as_ref()
            .and_then(|w| w.as_any().downcast_ref::<W>())
            .expect("type checked when the WidgetMut was made")
    }
}

impl<W: Widget<S>, S: 'static> std::ops::DerefMut for WidgetMut<'_, W, S> {
    fn deref_mut(&mut self) -> &mut W {
        self.widget
            .as_mut()
            .and_then(|w| w.as_any_mut().downcast_mut::<W>())
            .expect("type checked when the WidgetMut was made")
    }
}

impl<W, S: 'static> Drop for WidgetMut<'_, W, S> {
    fn drop(&mut self) {
        if let Some(w) = self.widget.take() {
            self.ui.untake(self.id, w);
        }
    }
}
