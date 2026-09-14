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

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::time::{Duration, Instant};

use nitro_core::{Point, Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::msg::ServerMsg;
use nitro_wire::types::{ButtonState, NodeId};

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
    /// Its children, in paint order.
    pub children: Vec<WidgetId>,
}

/// A handle to a descriptor hook registered with [`Ui::add_fd`].
///
/// It names our *duplicate* of the app's descriptor, which is the number
/// the app loop uses as its `epoll` token, so the two cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FdToken(RawFd);

impl FdToken {
    /// The raw descriptor, as the app loop's `epoll` token.
    #[must_use]
    pub fn raw(self) -> RawFd {
        self.0
    }

    /// Rebuild a token from an `epoll` token.
    #[must_use]
    pub fn from_raw(fd: RawFd) -> Self {
        Self(fd)
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
pub struct Ui<S> {
    arena: Arena<S>,
    root: Option<WidgetId>,
    theme: Theme,
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
    // Scratch pools. Layout recurses, so one vector is not enough; these
    // are stacks of reusable ones, which is what keeps a flush free of
    // per-widget allocation once the tree has settled.
    id_pool: Vec<Vec<WidgetId>>,
    item_pool: Vec<Vec<FlexItem>>,
    rect_pool: Vec<Vec<Rect>>,
    chain: Vec<(WidgetId, Point)>,
    fds: Vec<FdHook<S>>,
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
}

/// A shell-event handler; see [`Ui::on_shell`].
type ShellHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, &crate::shell::ShellEvent)>;

/// An app callback: it is handed the state and the whole tree, exactly
/// like a widget's own.
type Callback<S> = Box<dyn FnMut(&mut S, &mut Ui<S>)>;

/// A one-shot app callback, for timers.
type OnceCallback<S> = Box<dyn FnOnce(&mut S, &mut Ui<S>)>;

/// An app-level key handler; see [`Ui::on_key`].
type KeyHandler<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, &KeyEvent) -> Handled>;

struct FdHook<S> {
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
            wire: Wire::new(conn),
            window_open: false,
            window_size: Size::ZERO,
            window_position: Point::ZERO,
            scale: 1.0,
            backdrop: None,
            backdrop_wanted: true,
            backdrop_sent: None,
            control_path: None,
            focused: None,
            pending_focus: Vec::new(),
            hover_chain: Vec::new(),
            activations: Vec::new(),
            quit: false,
            id_pool: Vec::new(),
            item_pool: Vec::new(),
            rect_pool: Vec::new(),
            chain: Vec::new(),
            fds: Vec::new(),
            timers: Vec::new(),
            next_timer: 1,
            key_handlers: Vec::new(),
            surface: None,
            app_id: String::new(),
            shell_handlers: Vec::new(),
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
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        // The backdrop is not a widget, so no widget's repaint covers it.
        self.backdrop_sent = None;
        let ids: Vec<WidgetId> = self.all_ids();
        for id in ids {
            self.mark(id, Dirty::LAYOUT | Dirty::PAINT);
        }
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

    /// Forget the recorded mutations, leaving the tap on.
    pub fn clear_mutations(&mut self) {
        self.wire.clear_tap();
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
        self.mark(root, Dirty::LAYOUT | Dirty::PAINT | Dirty::TREE);
        Ok(())
    }

    /// Set the application id sent with the window: what a window list
    /// names this program by. [`App`](crate::App) sets it from the name
    /// the app was constructed with.
    pub fn set_app_id(&mut self, app_id: impl Into<String>) {
        self.app_id = app_id.into();
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
        self.pass_paint(root)?;
        self.pass_backdrop()?;
        self.wire.commit()
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
            items.push(FlexItem::new(cstyle, basis));
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
                self.quit = true;
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
        Ok(n)
    }

    /// Turn one `ServerMsg` into widget events.
    pub fn dispatch(&mut self, state: &mut S, msg: &ServerMsg) {
        match msg {
            ServerMsg::Configure(c) => {
                self.scale = c.scale;
                self.window_position = c.position;
                self.resize(c.size);
            }
            ServerMsg::Closed(_) => self.quit = true,
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
            _ => {}
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
    /// the app's own handlers. `Tab` is the framework's: it moves focus
    /// and is never offered to a widget.
    ///
    /// The order for a press is the whole of the key contract:
    ///
    /// 1. `KeyDown` from the focused widget (or the root, with nothing
    ///    focused) up through its ancestors;
    /// 2. if nobody took it and the key produced text, `Event::Text` the
    ///    same way — which is how a text field types a `q` that an app
    ///    also uses as a shortcut;
    /// 3. only then the handlers registered with [`Ui::on_key`] and
    ///    [`Ui::set_shortcut`], in registration order.
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
        if pressed && k.keycode == key::TAB {
            self.focus_next(state, ev.shift());
            return;
        }
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
            self.run_key_handlers(state, &ev);
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
    /// ```no_run
    /// # use nitro_ui::event::{Handled, KeyEvent, key};
    /// # use nitro_ui::Ui;
    /// # fn demo<S: 'static>(ui: &mut Ui<S>) {
    /// ui.on_key(|_s: &mut S, ui: &mut Ui<S>, k: &KeyEvent| {
    ///     if k.text == "q" || k.keycode == key::ESC {
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
        let token = FdToken(owned.as_raw_fd());
        self.fds.push(FdHook {
            fd: owned,
            callback: Some(Box::new(callback)),
        });
        Ok(token)
    }

    /// Drop a descriptor hook. Closing our duplicate also removes it
    /// from the app loop's `epoll` set.
    pub fn remove_fd(&mut self, token: FdToken) {
        self.fds.retain(|h| h.fd.as_raw_fd() != token.0);
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
    pub(crate) fn hook_fds(&self) -> Vec<(RawFd, BorrowedFd<'_>)> {
        self.fds
            .iter()
            .map(|h| (h.fd.as_raw_fd(), h.fd.as_fd()))
            .collect()
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
        for t in &mut self.timers {
            t.deadline -= by;
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
    }

    /// Run the callback registered for `token`. The app loop calls it
    /// when `epoll` says the descriptor is readable; an unknown token is
    /// ignored, because a hook may have been removed between the wakeup
    /// and the dispatch.
    pub fn run_fd(&mut self, state: &mut S, token: FdToken) {
        let fd = token.0;
        let Some(i) = self.fds.iter().position(|h| h.fd.as_raw_fd() == fd) else {
            return;
        };
        // Take the callback out for the same reason a widget leaves its
        // slot: it gets `&mut Ui` and must not alias itself.
        let Some(mut cb) = self.fds[i].callback.take() else {
            return;
        };
        cb(state, self);
        if let Some(h) = self.fds.iter_mut().find(|h| h.fd.as_raw_fd() == fd) {
            h.callback = Some(cb);
        }
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
    pub fn set_slot_transform(&mut self, slot: u8, transform: nitro_core::Transform) {
        let index = slot as usize;
        let Some(state) = self.ui.arena.slot_mut(self.id).map(|s| &mut s.state) else {
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
        let _ = self.ui.wire.set_transform(node, transform);
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
