//! The retained scene: arenas, the tree, and the mutation API.

use std::collections::HashMap;

use nitro_core::{Damage, IRect, Point, Rect, Size, Transform};

use crate::{
    Admit, Border, Buffer, BufferDesc, BufferKey, ClientId, Configure, Error, Fill, IconRef,
    ImageRef, Insets, Layer, Node, NodeKey, NodeKind, OutputId, PixelStore, SurfaceRef, TextRef,
    Window, WindowFlags, WindowKey, WindowState,
    key::Arena,
    node::{ALL_DIRTY, Dirty, NodeData},
    window::Output,
};

/// How deep the tree may get. Deeper trees are refused with
/// [`Error::TooDeep`]; the limit keeps the recursive walks inside a normal
/// stack and bounds the cost of a reparent.
pub const MAX_DEPTH: u32 = 128;

/// Counters from the last [`Scene::update`], for tests and tracing.
///
/// `visited_nodes` is the point of the whole design: it must stay
/// proportional to what changed, not to the size of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UpdateStats {
    /// Nodes whose world state was recomputed.
    pub visited_nodes: usize,
    /// Nodes that contributed damage.
    pub damaged_nodes: usize,
    /// Dirty subtree roots the walk entered (one per window with any dirt).
    pub dirty_roots: usize,
}

/// A retained tree of nodes grouped into windows, placed on outputs.
///
/// Pure data plus bookkeeping: no I/O, no rasterization. Mutations record
/// exactly what changed; [`update`](Scene::update) turns that into per-output
/// [`Damage`](nitro_core::Damage); [`paint_list`](Scene::paint_list) and
/// [`hit_test`](Scene::hit_test) walk only what a region touches.
#[derive(Debug)]
pub struct Scene {
    pub(crate) nodes: Arena<NodeKey, Node>,
    pub(crate) windows: Arena<WindowKey, Window>,
    pub(crate) buffers: Arena<BufferKey, Buffer>,
    pub(crate) outputs: Vec<Output>,
    /// Image nodes referencing each buffer, so `buffer_damaged` costs O(users)
    /// rather than a tree walk. Entries may name dead nodes; they are pruned
    /// when walked.
    buffer_users: HashMap<BufferKey, Vec<NodeKey>>,
    /// Buffers whose user list went empty since the last
    /// [`take_released_buffers`](Scene::take_released_buffers); filtered
    /// there, so re-attaches and destroys in the same batch are harmless.
    unreferenced: Vec<BufferKey>,
    /// Sub-rect damage waiting on image nodes flagged [`Dirty::PARTIAL`],
    /// in buffer pixels and already clipped to the node's `src`. Consumed
    /// (and mapped to device pixels) by the next `update`; an entry exists
    /// only while its node carries the flag.
    pub(crate) partial: HashMap<NodeKey, Damage>,
    /// Every `buffer_damaged` rect since the last `update`, per buffer, so a
    /// same-size buffer swap can pick up damage that arrived *before* the
    /// `SetImage` naming it in the same commit. A superset of what one
    /// commit sent is harmless: it only repaints a little more.
    pub(crate) recent: HashMap<BufferKey, Damage>,
    /// Surface nodes flagged on-plane, so [`has_holes`](Scene::has_holes)
    /// is O(1) when there are none. Kept exact: entries leave on
    /// [`set_surface_on_plane`](Scene::set_surface_on_plane)`(false)` and
    /// when the node is destroyed (`destroy_subtree`, the one path that
    /// removes nodes).
    on_plane: Vec<NodeKey>,

    /// Window roots carrying dirt, deduplicated by `Node::queued`.
    pub(crate) dirty_roots: Vec<NodeKey>,
    /// Damage from things that are no longer where their cache says (nodes
    /// destroyed, windows unplaced or restacked), flushed at the next
    /// `update`.
    pub(crate) pending: Vec<(OutputId, IRect)>,
    /// Whose windows are painted and hit-tested; see [`Admit`].
    pub(crate) admit: Admit,
    /// Scratch stack for subtree walks that cannot recurse.
    pub(crate) scratch: Vec<NodeKey>,
    /// Windows whose size changed since the last update.
    pub(crate) resized: Vec<WindowKey>,
    pub(crate) stats: UpdateStats,
    /// Translation detection, live only during `update`.
    pub(crate) tx: crate::update::TxState,
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

impl Scene {
    /// An empty scene with no outputs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Arena::new(),
            windows: Arena::new(),
            buffers: Arena::new(),
            outputs: Vec::new(),
            buffer_users: HashMap::new(),
            unreferenced: Vec::new(),
            partial: HashMap::new(),
            recent: HashMap::new(),
            on_plane: Vec::new(),

            dirty_roots: Vec::new(),
            pending: Vec::new(),
            admit: Admit::All,
            scratch: Vec::new(),
            resized: Vec::new(),
            stats: UpdateStats {
                visited_nodes: 0,
                damaged_nodes: 0,
                dirty_roots: 0,
            },
            tx: crate::update::TxState::default(),
        }
    }

    // ---------------------------------------------------------------- access

    /// Look up a node.
    ///
    /// # Errors
    /// [`Error::StaleKey`] if the key does not name a live node.
    pub fn node(&self, key: NodeKey) -> Result<&Node, Error> {
        self.nodes.get(key).ok_or(Error::StaleKey)
    }

    /// Look up a window.
    ///
    /// # Errors
    /// [`Error::StaleKey`] if the key does not name a live window.
    pub fn window_info(&self, key: WindowKey) -> Result<&Window, Error> {
        self.windows.get(key).ok_or(Error::StaleKey)
    }

    /// Look up a buffer, including its pixels.
    ///
    /// # Errors
    /// [`Error::StaleKey`] if the key does not name a live buffer.
    pub fn buffer(&self, key: BufferKey) -> Result<&Buffer, Error> {
        self.buffers.get(key).ok_or(Error::StaleKey)
    }

    /// Number of live nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of live windows.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// Number of live buffers.
    #[must_use]
    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Counters from the last [`update`](Scene::update).
    #[must_use]
    pub fn stats(&self) -> UpdateStats {
        self.stats
    }

    pub(crate) fn node_ref(&self, key: NodeKey) -> &Node {
        self.nodes
            .get(key)
            .expect("scene invariant: tree links name live nodes")
    }

    pub(crate) fn node_mut_ref(&mut self, key: NodeKey) -> &mut Node {
        self.nodes
            .get_mut(key)
            .expect("scene invariant: tree links name live nodes")
    }

    pub(crate) fn window_ref(&self, key: WindowKey) -> &Window {
        self.windows
            .get(key)
            .expect("scene invariant: nodes name live windows")
    }

    // ----------------------------------------------------------------- admit

    /// Paint and hit-test only the windows `admit` admits.
    ///
    /// A change damages every output whole: which windows show changes
    /// everywhere at once, and the next [`update`](Scene::update) reports
    /// it like any other damage. Setting the value already in force does
    /// nothing, so it costs a frame only when it means one.
    pub fn set_admit(&mut self, admit: Admit) {
        if self.admit == admit {
            return;
        }
        self.admit = admit;
        for o in &self.outputs {
            self.pending.push((o.id, o.rect));
        }
    }

    /// Make a window painted but never hit (or undo it): the pointer
    /// passes through it to whatever is below. A drag icon sits under the
    /// pointer for the whole drag, and must not hide the drop target.
    /// Changes no pixels, so it damages nothing.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_hit_exempt(&mut self, win: WindowKey, exempt: bool) -> Result<(), Error> {
        self.windows.get_mut(win).ok_or(Error::StaleKey)?.hit_exempt = exempt;
        Ok(())
    }

    /// Take a window **offscreen** (or bring it back): it stays placed,
    /// laid out and updated exactly as before, but
    /// [`paint_list`](Scene::paint_list) and [`hit_test`](Scene::hit_test)
    /// skip it, and every rect of damage an [`update`](Scene::update)
    /// finds inside it is reported per window in
    /// [`UpdateResult::offscreen`](crate::UpdateResult::offscreen) instead
    /// of on its output. [`paint_window`](Scene::paint_window) still lists
    /// its items.
    ///
    /// What it is for: the server renders an offscreen window into a
    /// buffer of its own (the overview's thumbnail atlas) and shows that,
    /// so the window's own changes must reach the renderer as "this
    /// window changed here" rather than as output pixels.
    ///
    /// A change banks damage for the window's cached extent on its output
    /// (it appears or disappears there). Setting the value already in
    /// force does nothing.
    ///
    /// Damage *banked* for an offscreen window by a mutation that makes a
    /// cached rect unreachable (a restack, `place_window`, a destroyed
    /// node) still goes to the output. That over-repaints a little and is
    /// never wrong.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_offscreen(&mut self, win: WindowKey, offscreen: bool) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if window.offscreen == offscreen {
            return Ok(());
        }
        window.offscreen = offscreen;
        let (root, output) = (window.root, window.output);
        if let Some(id) = output {
            let bounds = self.node_ref(root).subtree_bounds;
            if !bounds.is_empty() {
                self.pending.push((id, bounds));
            }
        }
        Ok(())
    }

    /// Whose windows are painted and hit-tested.
    #[must_use]
    pub fn admit(&self) -> Admit {
        self.admit
    }

    /// Whether `win` is admitted: painted and hit-tested. `false` for a
    /// window that does not exist.
    #[must_use]
    pub fn admits_window(&self, win: WindowKey) -> bool {
        self.windows
            .get(win)
            .is_some_and(|w| self.admit.admits(w.client))
    }

    /// Leave [`Layer::Top`] on `output` out of painting and hit-testing
    /// (or put it back). A fullscreen window must cover the panels on its
    /// output without losing its layer, so the windows stay where they are
    /// and only the two walks skip them; [`Layer::Overlay`] is unaffected.
    ///
    /// A change damages the output whole. Setting the value already in
    /// force, or naming an unknown output, does nothing.
    pub fn set_top_layer_hidden(&mut self, output: OutputId, hidden: bool) {
        let Some(o) = self.outputs.iter_mut().find(|o| o.id == output) else {
            return;
        };
        if o.top_hidden == hidden {
            return;
        }
        o.top_hidden = hidden;
        self.pending.push((o.id, o.rect));
    }

    /// Whether [`Layer::Top`] on `output` is hidden. `false` for an unknown
    /// output.
    #[must_use]
    pub fn top_layer_hidden(&self, output: OutputId) -> bool {
        self.outputs.iter().any(|o| o.id == output && o.top_hidden)
    }

    /// Whether `win` sits on a hidden top layer: on [`Layer::Top`] of an
    /// output whose top layer is hidden. Such a window is neither painted
    /// nor hit. `false` for a window that does not exist or is unplaced.
    #[must_use]
    pub fn is_layer_hidden(&self, win: WindowKey) -> bool {
        self.windows.get(win).is_some_and(|w| {
            w.layer == Layer::Top && w.output.is_some_and(|o| self.top_layer_hidden(o))
        })
    }

    // --------------------------------------------------------------- outputs

    /// Add (or replace) an output.
    ///
    /// `rect` is the output's area in the global device-pixel space and
    /// `scale` the factor from logical units to device pixels for every window
    /// placed on it.
    pub fn add_output(&mut self, id: OutputId, rect: IRect, scale: f32) {
        if let Some(existing) = self.outputs.iter_mut().find(|o| o.id == id) {
            let moved = existing.rect != rect || existing.scale.to_bits() != scale.to_bits();
            let old_rect = existing.rect;
            existing.rect = rect;
            existing.scale = scale;
            if moved {
                self.pending.push((id, old_rect));
                self.pending.push((id, rect));
                let roots: Vec<NodeKey> = self
                    .windows
                    .keys()
                    .filter(|w| self.window_ref(*w).output == Some(id))
                    .map(|w| self.window_ref(w).root)
                    .collect();
                for root in roots {
                    self.mark(root, Dirty::TRANSFORM);
                }
            }
            return;
        }
        self.outputs.push(Output::new(id, rect, scale));
    }

    /// Remove an output. Windows placed on it become unplaced.
    ///
    /// No damage is recorded: the output is gone, so nobody will draw it
    /// again.
    pub fn remove_output(&mut self, id: OutputId) {
        let Some(index) = self.outputs.iter().position(|o| o.id == id) else {
            return;
        };
        self.outputs.remove(index);
        // Damage banked for the departed output would never be drawn.
        self.pending.retain(|(out, _)| *out != id);
        let orphans: Vec<WindowKey> = self
            .windows
            .keys()
            .filter(|w| self.window_ref(*w).output == Some(id))
            .collect();
        for win in orphans {
            let root = self.window_ref(win).root;
            self.window_mut_unchecked(win).output = None;
            self.mark(root, Dirty::TRANSFORM);
        }
    }

    /// Every output, in the order they were added.
    pub fn outputs(&self) -> impl Iterator<Item = (OutputId, IRect, f32)> + '_ {
        self.outputs.iter().map(|o| (o.id, o.rect, o.scale))
    }

    /// An output's device-pixel rectangle and scale.
    #[must_use]
    pub fn output_info(&self, id: OutputId) -> Option<(IRect, f32)> {
        self.outputs
            .iter()
            .find(|o| o.id == id)
            .map(|o| (o.rect, o.scale))
    }

    pub(crate) fn output_index(&self, id: OutputId) -> Option<usize> {
        self.outputs.iter().position(|o| o.id == id)
    }

    pub(crate) fn output_at(&self, index: usize) -> &Output {
        &self.outputs[index]
    }

    /// Where a window's root node sits in device pixels: the output's origin,
    /// plus the window's logical position, scaled by the output's scale, and
    /// rounded to whole device pixels (#3940). At a fractional scale this keeps a
    /// window whose buffer is its device size on the pixel grid, so it is drawn
    /// 1:1 (opaque copy, XR24 fast path) instead of resampled.
    ///
    /// Returns the output's index, that transform, and the output's rect (the
    /// root clip).
    pub(crate) fn root_placement(&self, win: WindowKey) -> Option<(usize, Transform, IRect)> {
        let window = self.windows.get(win)?;
        let index = self.output_index(window.output?)?;
        let output = &self.outputs[index];
        let s = output.scale;
        let t = Transform::translate(
            (output.rect.x as f32 + window.position.x * s).round(),
            (output.rect.y as f32 + window.position.y * s).round(),
        )
        .then(&Transform::scale(s, s));
        Some((index, t, output.rect))
    }

    // --------------------------------------------------------------- windows

    /// Create a top-level window and its root [`Group`](NodeKind::Group) node.
    ///
    /// The window starts unplaced and undecorated: nothing is painted and
    /// nothing can be hit until [`place_window`](Scene::place_window) puts it
    /// on an output, and its content group *is* its root until
    /// [`frame_window`](Scene::frame_window) wraps one around it.
    ///
    /// Its content group **clips**: see
    /// [`create_window_with`](Scene::create_window_with).
    pub fn create_window(
        &mut self,
        client: ClientId,
        title: impl Into<String>,
        size: Size,
        layer: Layer,
    ) -> WindowKey {
        self.create_window_with(client, title, size, layer, WindowFlags::default())
    }

    /// [`create_window`](Scene::create_window) with explicit flags.
    ///
    /// # A window's content is always clipped to the window
    ///
    /// The content group is created with [`clip`](Node::clip) set, and
    /// [`set_clip`](Scene::set_clip) refuses to clear it. So a client's
    /// nodes are bounded by its window's content rectangle whatever the
    /// client sends: a node laid out past the right edge is cut off at
    /// the edge, not painted on the desktop beside the frame, and one
    /// given a negative `y` does not paint over its own title bar.
    /// Paint, hit testing and damage all read the same clip, so a pixel
    /// that is not drawn cannot be clicked and cannot be damaged either.
    ///
    /// **The rectangle it clips to is the node's own bounds, and that is
    /// the whole of why it cannot go stale.** A clip rectangle stored
    /// beside the window would have to be rewritten on every resize,
    /// maximize, fullscreen and inset change, and any path that forgot
    /// would clip to yesterday's window — invisibly, because the common
    /// case is a client that does not overflow. The content group's
    /// bounds are *already* the content rectangle: the scene keeps them
    /// so in [`set_window_size`](Scene::set_window_size),
    /// [`set_window_inset`](Scene::set_window_inset) and in
    /// [`set_bounds`](Scene::set_bounds) when a client resizes its own
    /// top-level group. Clipping to them is therefore correct in every
    /// one of those cases by construction, with nothing to keep in step.
    pub fn create_window_with(
        &mut self,
        client: ClientId,
        title: impl Into<String>,
        size: Size,
        layer: Layer,
        flags: WindowFlags,
    ) -> WindowKey {
        let root = self
            .nodes
            .insert(Node::new(NodeKind::Group, client, WindowKey::NONE, 0));
        let win = self.windows.insert(Window {
            root,
            content: root,
            client,
            title: title.into(),
            app_id: String::new(),
            layer,
            size,
            configured: Size::ZERO,
            output: None,
            position: Point::ZERO,
            inset: Insets::NONE,
            state: WindowState::Normal,
            flags,
            min: Size::ZERO,
            max: Size::ZERO,
            restore: None,
            parent: None,
            popup: false,
            hit_exempt: false,
            offscreen: false,
        });
        self.note_resized(win);
        let node = self.node_mut_ref(root);
        node.window = win;
        node.bounds = Rect::new(0.0, 0.0, size.w, size.h);
        // The window's own clip, and the one thing here a client cannot
        // undo. This node is the content group — `frame_window` inserts a
        // new root *above* it and leaves it alone — so the flag set here
        // is the one in force for a framed window too.
        node.clip = true;
        self.mark(root, ALL_DIRTY);
        win
    }

    /// Create a **popup**: a window anchored to another window.
    ///
    /// A popup is an ordinary [`Window`] carrying a
    /// [`parent`](Window::parent) link, not a node kind of its own. That
    /// buys three things for free:
    ///
    /// * it **escapes the parent's clip**, because
    ///   [`paint_list`](Scene::paint_list) and
    ///   [`hit_test`](Scene::hit_test) walk per-window roots and one
    ///   window's clip never applies to another's tree. Nothing about
    ///   [`set_clip`](Scene::set_clip)'s refusal to unclip a content group
    ///   has to change, and nothing should;
    /// * it is positioned in output space in its own right, so the server
    ///   can constrain it against the work area;
    /// * it hit-tests as its own surface and gets its own `Configure`.
    ///
    /// It inherits the parent's [`Layer`] — a menu over a `Top` panel is
    /// itself `Top`, so it is above the panel and below `Overlay`, with no
    /// new layer and no special case — and is created **undecorated, fixed
    /// size and unfocusable**: a menu must never grow a title bar and must
    /// never take the caret away from the window it belongs to.
    ///
    /// It starts unplaced, like every other window;
    /// [`place_window`](Scene::place_window) puts it directly above its
    /// parent's block.
    ///
    /// # Errors
    /// [`Error::StaleKey`] if `parent` does not name a live window.
    pub fn create_popup(
        &mut self,
        client: ClientId,
        parent: WindowKey,
        size: Size,
    ) -> Result<WindowKey, Error> {
        let layer = self.windows.get(parent).ok_or(Error::StaleKey)?.layer;
        let win = self.create_window_with(
            client,
            "",
            size,
            layer,
            WindowFlags {
                decorated: false,
                fixed_size: true,
                focusable: false,
            },
        );
        let window = self.window_mut_unchecked(win);
        window.parent = Some(parent);
        window.popup = true;
        Ok(win)
    }

    /// The windows that name `win` as their parent, in stacking order.
    ///
    /// Direct children only; the server walks the chain itself. Scanned out
    /// of the layer stacks rather than read from a child list on
    /// [`Window`], so there is exactly one copy of the parent edge.
    pub fn popup_children(&self, win: WindowKey) -> Vec<WindowKey> {
        let Some(window) = self.windows.get(win) else {
            return Vec::new();
        };
        let Some(index) = window.output.and_then(|id| self.output_index(id)) else {
            return Vec::new();
        };
        self.outputs[index].layers[window.layer.index()]
            .iter()
            .copied()
            .filter(|w| {
                self.windows
                    .get(*w)
                    .is_some_and(|c| c.parent == Some(win) && *w != win)
            })
            .collect()
    }

    /// A window plus every popup descending from it, in the stack's own
    /// order: the contiguous run [`place_window`](Scene::place_window) and
    /// [`restack`](Scene::restack) move as one.
    ///
    /// Computed in one place so the invariant "a popup sits immediately
    /// above its parent's block" has one implementation to be right.
    fn block(&self, stack: &[WindowKey], win: WindowKey) -> Vec<WindowKey> {
        stack
            .iter()
            .copied()
            .filter(|w| {
                let mut cur = *w;
                // Bounded by the stack length: a parent chain inside one
                // layer cannot be longer than the layer itself.
                for _ in 0..=stack.len() {
                    if cur == win {
                        return true;
                    }
                    match self.windows.get(cur).and_then(|i| i.parent) {
                        Some(p) => cur = p,
                        None => return false,
                    }
                }
                false
            })
            .collect()
    }

    /// The root of a popup chain: the first ancestor that is not itself a
    /// popup, or `win` when it is a toplevel.
    pub fn chain_root(&self, win: WindowKey) -> WindowKey {
        let mut cur = win;
        for _ in 0..self.windows.len() {
            match self.windows.get(cur).and_then(|i| i.parent) {
                Some(p) if self.windows.contains(p) => cur = p,
                _ => break,
            }
        }
        cur
    }

    /// Wrap a window's content in a **frame group** owned by the server.
    ///
    /// A new root [`Group`](NodeKind::Group) owned by
    /// [`ClientId::SERVER`] is inserted above the client's group, the client's
    /// group becomes its *last* child — so decorations created before it are
    /// painted behind, and ones created after are painted on top — and the
    /// content is offset by `inset`. Every rectangle the client sees is
    /// unchanged: its own group keeps its bounds, and the frame grows around
    /// it.
    ///
    /// Only the server may call this in practice; a client cannot name
    /// another client's window at all.
    ///
    /// # Errors
    /// [`Error::StaleKey`] for a dead window; [`Error::BadParent`] if the
    /// window is already framed.
    pub fn frame_window(&mut self, win: WindowKey, inset: Insets) -> Result<NodeKey, Error> {
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        if window.content != window.root {
            return Err(Error::BadParent);
        }
        let content = window.content;
        let size = window.size;
        // The old root keeps its own bounds (the content's size) and simply
        // gains a parent; the frame takes over as the window's root.
        let frame = self
            .nodes
            .insert(Node::new(NodeKind::Group, ClientId::SERVER, win, 0));
        {
            let node = self.node_mut_ref(frame);
            node.children.push(content);
            node.bounds = Rect::new(0.0, 0.0, size.w + inset.width(), size.h + inset.height());
        }
        {
            let node = self.node_mut_ref(content);
            node.parent = Some(frame);
            node.bounds.x = inset.left;
            node.bounds.y = inset.top;
        }
        // The whole subtree moved one level down.
        self.rewrite_subtree(content, 1, win);
        let window = self.window_mut_unchecked(win);
        window.root = frame;
        window.inset = inset;
        // The old root was a dirty root in its own right; the frame is the
        // one now, and `mark` walks up to it.
        self.dirty_roots.retain(|r| *r != content);
        self.node_mut_ref(content).queued = false;
        self.mark(frame, ALL_DIRTY);
        self.mark(content, ALL_DIRTY);
        Ok(frame)
    }

    /// Set a framed window's inset, moving the content and resizing the
    /// frame to match. A no-op on an unframed window.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_window_inset(&mut self, win: WindowKey, inset: Insets) -> Result<(), Error> {
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        if window.content == window.root || window.inset == inset {
            return Ok(());
        }
        let (content, root, size) = (window.content, window.root, window.size);
        self.window_mut_unchecked(win).inset = inset;
        let node = self.node_mut_ref(content);
        node.bounds.x = inset.left;
        node.bounds.y = inset.top;
        self.mark(content, Dirty::BOUNDS);
        let node = self.node_mut_ref(root);
        node.bounds.w = size.w + inset.width();
        node.bounds.h = size.h + inset.height();
        self.mark(root, Dirty::BOUNDS);
        Ok(())
    }

    /// Set a window's application id (`"org.nitro.calc"`), for the shell's
    /// window list.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_app_id(
        &mut self,
        client: ClientId,
        win: WindowKey,
        app_id: impl Into<String>,
    ) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if !client.may_touch(window.client) {
            return Err(Error::NotOwner);
        }
        window.app_id = app_id.into();
        Ok(())
    }

    /// Set the content size limits the server will respect when resizing.
    /// A zero component means "no limit"; a `max` below `min` is clamped up
    /// rather than refused.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_window_limits(
        &mut self,
        client: ClientId,
        win: WindowKey,
        min: Size,
        max: Size,
    ) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if !client.may_touch(window.client) {
            return Err(Error::NotOwner);
        }
        let sane = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
        let min = Size::new(sane(min.w), sane(min.h));
        let max = Size::new(sane(max.w), sane(max.h));
        window.min = min;
        window.max = Size::new(
            if max.w > 0.0 { max.w.max(min.w) } else { 0.0 },
            if max.h > 0.0 { max.h.max(min.h) } else { 0.0 },
        );
        Ok(())
    }

    /// Clamp a content size to a window's limits.
    #[must_use]
    pub fn clamp_to_limits(&self, win: WindowKey, size: Size) -> Size {
        let Some(w) = self.windows.get(win) else {
            return size;
        };
        let axis = |v: f32, min: f32, max: f32| {
            let v = if min > 0.0 { v.max(min) } else { v };
            if max > 0.0 { v.min(max) } else { v }
        };
        Size::new(
            axis(size.w, w.min.w, w.max.w),
            axis(size.h, w.min.h, w.max.h),
        )
    }

    /// Set a window's state, hiding it when it becomes
    /// [`Minimized`](WindowState::Minimized) and showing it again otherwise.
    ///
    /// Geometry is not touched: a maximized window is *placed* and *resized*
    /// by the caller, which is the only thing that knows the work area.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_window_state(&mut self, win: WindowKey, state: WindowState) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if window.state == state {
            return Ok(());
        }
        window.state = state;
        let root = window.root;
        let visible = state != WindowState::Minimized;
        let node = self.node_mut_ref(root);
        if node.visible != visible {
            node.visible = visible;
            self.mark(root, Dirty::INHERIT);
        }
        Ok(())
    }

    /// Remember (or forget, with `None`) the frame position and content size
    /// to return to when a window leaves `Maximized`/`Fullscreen`.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_window_restore(
        &mut self,
        win: WindowKey,
        restore: Option<(Point, Size)>,
    ) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        window.restore = restore;
        Ok(())
    }

    /// Destroy a window and its whole node tree. Buffers survive.
    ///
    /// # Errors
    /// [`Error::StaleKey`] for a dead key, [`Error::NotOwner`] if `client`
    /// does not own the window.
    pub fn destroy_window(&mut self, client: ClientId, win: WindowKey) -> Result<(), Error> {
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        if !client.may_touch(window.client) {
            return Err(Error::NotOwner);
        }
        let root = window.root;
        if let Some(id) = window.output {
            let bounds = self.node_ref(root).subtree_bounds;
            self.pending.push((id, bounds));
        }
        for output in &mut self.outputs {
            output.remove(win);
        }
        self.destroy_subtree(root);
        self.windows.remove(win);
        // Defensive: the server dismisses a window's popups before it
        // destroys it, so this should find nothing — but a dangling
        // `WindowKey` in a parent link is a key that could later be
        // followed onto a recycled slot, and there is no cheap way to spot
        // that after the fact.
        for key in self.windows.keys().collect::<Vec<_>>() {
            if let Some(w) = self.windows.get_mut(key)
                && w.parent == Some(win)
            {
                w.parent = None;
            }
        }
        Ok(())
    }

    /// Set a window's title.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_window_title(
        &mut self,
        client: ClientId,
        win: WindowKey,
        title: impl Into<String>,
    ) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if !client.may_touch(window.client) {
            return Err(Error::NotOwner);
        }
        window.title = title.into();
        Ok(())
    }

    /// Resize a window: its content group's bounds and its requested size.
    /// A framed window's root grows by the frame's insets.
    ///
    /// The next [`update`](Scene::update) reports a [`Configure`] for it.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_window_size(
        &mut self,
        client: ClientId,
        win: WindowKey,
        size: Size,
    ) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if !client.may_touch(window.client) {
            return Err(Error::NotOwner);
        }
        window.size = size;
        let (root, content, inset) = (window.root, window.content, window.inset);
        self.note_resized(win);
        let node = self.node_mut_ref(content);
        if node.bounds.size() != size {
            node.bounds.w = size.w;
            node.bounds.h = size.h;
            self.mark(content, Dirty::BOUNDS);
        }
        if root != content {
            let frame = Size::new(size.w + inset.width(), size.h + inset.height());
            let node = self.node_mut_ref(root);
            if node.bounds.size() != frame {
                node.bounds.w = frame.w;
                node.bounds.h = frame.h;
                self.mark(root, Dirty::BOUNDS);
            }
        }
        Ok(())
    }

    /// Place a window on an output at a logical position, or take it off
    /// screen with `output = None`.
    ///
    /// Placing a window puts it at the front of its layer if it was not
    /// already in the output's z-order — unless it is a **popup**, which
    /// goes directly above its parent's block instead, so a menu is above
    /// the window it belongs to and below whatever was above that.
    ///
    /// `output = None` is also how a popup is *unmapped*: an unplaced
    /// window is in no z-order, so it is neither painted nor hit, and
    /// unlike a `visible` flag the client cannot undo it.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::UnknownOutput`].
    pub fn place_window(
        &mut self,
        win: WindowKey,
        output: Option<OutputId>,
        position: Point,
    ) -> Result<(), Error> {
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        let old_output = window.output;
        let root = window.root;
        let parent = window.parent;
        if let Some(id) = output
            && self.output_index(id).is_none()
        {
            return Err(Error::UnknownOutput);
        }
        // Whatever it covered now has to be repainted without it.
        if let Some(id) = old_output {
            let bounds = self.node_ref(root).subtree_bounds;
            self.pending.push((id, bounds));
        }
        if old_output != output {
            for out in &mut self.outputs {
                out.remove(win);
            }
        }
        let layer = window.layer;
        let window = self.window_mut_unchecked(win);
        window.output = output;
        window.position = position;
        if let Some(id) = output
            && let Some(index) = self.output_index(id)
            && !self.outputs[index].layers[layer.index()].contains(&win)
        {
            {
                // A popup goes immediately above its parent's block, not at
                // the front of the layer: the front is where an unrelated
                // toplevel belongs, and a menu that jumped there would float
                // over windows it has nothing to do with.
                let at = parent.and_then(|p| {
                    let stack = &self.outputs[index].layers[layer.index()];
                    let block = self.block(stack, p);
                    let last = block.last()?;
                    stack.iter().position(|w| w == last).map(|i| i + 1)
                });
                let stack = &mut self.outputs[index].layers[layer.index()];
                match at {
                    Some(i) => stack.insert(i, win),
                    None => stack.push(win),
                }
            }
        }
        self.mark(root, Dirty::TRANSFORM);
        Ok(())
    }

    /// Move a window to a different stacking layer, keeping it frontmost
    /// within the new layer.
    ///
    /// Its popups go with it, keeping their order: a chain that stayed
    /// behind in the old layer would be a menu floating over a window it
    /// does not belong to, and the "immediately above its parent's block"
    /// invariant would be broken with nothing to restore it.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_layer(&mut self, win: WindowKey, layer: Layer) -> Result<(), Error> {
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        if window.layer == layer {
            return Ok(());
        }
        let output = window.output;
        let root = window.root;
        let old = window.layer.index();
        let block = match output.and_then(|id| self.output_index(id)) {
            Some(index) => self.block(&self.outputs[index].layers[old], win),
            None => vec![win],
        };
        for w in &block {
            if let Some(info) = self.windows.get_mut(*w) {
                info.layer = layer;
            }
        }
        if let Some(index) = output.and_then(|id| self.output_index(id)) {
            let id = self.outputs[index].id;
            let out = &mut self.outputs[index];
            for w in &block {
                out.remove(*w);
            }
            out.layers[layer.index()].extend_from_slice(&block);
            let bounds = self.node_ref(root).subtree_bounds;
            self.pending.push((id, bounds));
        }
        Ok(())
    }

    /// Raise a window to the front of its layer.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn raise(&mut self, win: WindowKey) -> Result<(), Error> {
        self.restack(win, true)
    }

    /// Lower a window to the back of its layer.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn lower(&mut self, win: WindowKey) -> Result<(), Error> {
        self.restack(win, false)
    }

    /// Move a window within its layer, carrying its popups with it.
    ///
    /// A window and its popup descendants are one contiguous **block**: a
    /// raise moves the whole run and preserves the order inside it, and a
    /// raise naming a popup is redirected to the chain's root. Without the
    /// redirect a plain left-click inside a menu (which reaches
    /// `raise_and_focus` like any other click) would pull the popup to the
    /// front of the layer and leave its parent behind — silently, because
    /// it still looks right until some unrelated window is raised.
    fn restack(&mut self, win: WindowKey, front: bool) -> Result<(), Error> {
        let win = self.chain_root(win);
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        let layer = window.layer.index();
        let root = window.root;
        let Some(index) = window.output.and_then(|id| self.output_index(id)) else {
            return Ok(());
        };
        let id = self.outputs[index].id;
        let block = self.block(&self.outputs[index].layers[layer], win);
        let stack = &mut self.outputs[index].layers[layer];
        if block.is_empty() {
            return Ok(());
        }
        let at_end = stack.len() >= block.len() && stack[stack.len() - block.len()..] == block[..];
        let at_start = stack.len() >= block.len() && stack[..block.len()] == block[..];
        if (front && at_end) || (!front && at_start) {
            return Ok(());
        }
        stack.retain(|w| !block.contains(w));
        if front {
            stack.extend_from_slice(&block);
        } else {
            stack.splice(0..0, block.iter().copied());
        }
        // Stacking does not move pixels, but it changes who is on top of them.
        let bounds = self.node_ref(root).subtree_bounds;
        self.pending.push((id, bounds));
        Ok(())
    }

    /// The windows on an output, back to front across all layers.
    pub fn windows(&self, output: OutputId) -> impl Iterator<Item = WindowKey> + '_ {
        self.output_index(output)
            .into_iter()
            .flat_map(move |i| self.outputs[i].z_order())
    }

    /// The windows on an output, front to back.
    pub fn windows_front_to_back(&self, output: OutputId) -> impl Iterator<Item = WindowKey> + '_ {
        self.output_index(output).into_iter().flat_map(move |i| {
            self.outputs[i]
                .layers
                .iter()
                .rev()
                .flat_map(|l| l.iter().rev().copied())
        })
    }

    fn window_mut_unchecked(&mut self, win: WindowKey) -> &mut Window {
        self.windows
            .get_mut(win)
            .expect("scene invariant: window key checked by the caller")
    }

    // ----------------------------------------------------------------- nodes

    /// Create a node under `parent`, inserted before the sibling `before` (or
    /// at the front, on top, when `before` is `None`).
    ///
    /// # Errors
    /// [`Error::StaleKey`] for a dead parent, [`Error::NotOwner`] if `client`
    /// does not own the parent, [`Error::BadSibling`] if `before` is not a
    /// child of `parent`, [`Error::TooDeep`] past [`MAX_DEPTH`].
    pub fn create_node(
        &mut self,
        client: ClientId,
        kind: NodeKind,
        parent: NodeKey,
        before: Option<NodeKey>,
    ) -> Result<NodeKey, Error> {
        let p = self.nodes.get(parent).ok_or(Error::StaleKey)?;
        if !client.may_touch(p.client) {
            return Err(Error::NotOwner);
        }
        let depth = p.depth + 1;
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        let position = child_position(p, before)?;
        let window = p.window;
        let owner = p.client;
        let key = self.nodes.insert(Node::new(kind, owner, window, depth));
        let p = self.node_mut_ref(parent);
        p.children.insert(position, key);
        self.node_mut_ref(key).parent = Some(parent);
        self.mark(key, ALL_DIRTY);
        self.mark(parent, Dirty::STRUCTURE);
        Ok(key)
    }

    /// Destroy a node and its descendants. Buffers they referenced survive.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::RootNode`] for a
    /// window's root (use [`destroy_window`](Scene::destroy_window)).
    pub fn destroy_node(&mut self, client: ClientId, key: NodeKey) -> Result<(), Error> {
        let node = self.nodes.get(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(node.client) {
            return Err(Error::NotOwner);
        }
        // A window's root and its content group both belong to the window,
        // not to whoever holds a key to them: destroying either is closing
        // the window, and `destroy_window` is the call that says so. A framed
        // window's content *has* a parent, so the parent check alone is no
        // longer enough.
        if self
            .windows
            .get(node.window)
            .is_some_and(|w| w.root == key || w.content == key)
        {
            return Err(Error::RootNode);
        }
        let Some(parent) = node.parent else {
            return Err(Error::RootNode);
        };
        self.damage_now(key);
        let p = self.node_mut_ref(parent);
        p.children.retain(|c| *c != key);
        // The parent's cached subtree bounds now cover a hole.
        self.mark(parent, Dirty::STRUCTURE);
        self.destroy_subtree(key);
        Ok(())
    }

    /// Move a node under a new parent.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::RootNode`],
    /// [`Error::BadParent`] when the new parent is the node itself or one of
    /// its descendants, [`Error::BadSibling`], [`Error::TooDeep`].
    pub fn reparent(
        &mut self,
        client: ClientId,
        key: NodeKey,
        parent: NodeKey,
        before: Option<NodeKey>,
    ) -> Result<(), Error> {
        let node = self.nodes.get(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(node.client) {
            return Err(Error::NotOwner);
        }
        if self
            .windows
            .get(node.window)
            .is_some_and(|w| w.root == key || w.content == key)
        {
            return Err(Error::RootNode);
        }
        let Some(old_parent) = node.parent else {
            return Err(Error::RootNode);
        };
        let new = self.nodes.get(parent).ok_or(Error::StaleKey)?;
        if !client.may_touch(new.client) {
            return Err(Error::NotOwner);
        }
        if key == parent || self.is_ancestor(key, parent) {
            return Err(Error::BadParent);
        }
        // Validated before anything is mutated, so a bad sibling aborts
        // the batch without having moved the node; the *position* it
        // yields is recomputed below, after the removal.
        child_position(new, before)?;
        // "Put me before myself" is where I already am. Answered here, as
        // a no-op, because the position below is measured after the node
        // has left the list — at which point it names a sibling that is
        // no longer there, and the request would fail as `BadSibling`
        // instead of doing the nothing it asks for.
        if before == Some(key) {
            return Ok(());
        }
        let new_depth = new.depth + 1;
        let window = new.window;
        if new_depth + self.subtree_height(key) > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        // Repaint what it used to cover, wherever that was.
        self.damage_now(key);
        self.node_mut_ref(old_parent).children.retain(|c| *c != key);
        self.mark(old_parent, Dirty::STRUCTURE);
        // Recomputed *after* the removal, not before, because a reparent
        // within the same parent is a **reorder**: the node has just left
        // the child list the position was measured against, so an index
        // taken earlier is off by one — and for "move to the end" it is
        // one past the end, which is a panic rather than a wrong answer.
        // A client can ask for exactly that (a window list dropping a
        // button re-orders its siblings), so this was reachable from the
        // wire: `a_reorder_within_one_parent_is_not_off_by_one` pins it.
        let p = self.node_mut_ref(parent);
        let position = child_position(p, before)?;
        p.children.insert(position, key);
        self.node_mut_ref(key).parent = Some(parent);
        self.rewrite_subtree(key, new_depth, window);
        self.mark(key, ALL_DIRTY);
        self.mark(parent, Dirty::STRUCTURE);
        Ok(())
    }

    /// Set a node's local bounds, in its parent's coordinate space.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_bounds(
        &mut self,
        client: ClientId,
        key: NodeKey,
        bounds: Rect,
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        if node.bounds == bounds {
            return Ok(());
        }
        node.bounds = bounds;
        let win = node.window;
        self.mark(key, Dirty::BOUNDS);
        // A client resizing its own top-level group is a resize request:
        // the *content* group is the one that means it, which is the root
        // itself for an undecorated window and the frame's child otherwise.
        if self.windows.get(win).is_some_and(|w| w.content == key) {
            let (inset, root) = {
                let w = self.window_ref(win);
                (w.inset, w.root)
            };
            let window = self.window_mut_unchecked(win);
            window.size = bounds.size();
            self.note_resized(win);
            if root != key {
                // The content's *origin* inside the frame belongs to the
                // frame, not to the client: a client that sends its own
                // window bounds as `(0, 0, w, h)` — which every toolkit
                // does — must not slide its content out from under the
                // title bar.
                let node = self.node_mut_ref(key);
                node.bounds.x = inset.left;
                node.bounds.y = inset.top;
                let frame = Rect::new(
                    0.0,
                    0.0,
                    bounds.w + inset.width(),
                    bounds.h + inset.height(),
                );
                let node = self.node_mut_ref(root);
                if node.bounds != frame {
                    node.bounds = frame;
                    self.mark(root, Dirty::BOUNDS);
                }
            }
        }
        Ok(())
    }

    /// Set a group's transform, applied to its children about the group's own
    /// origin.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`] on
    /// anything but a group.
    pub fn set_transform(
        &mut self,
        client: ClientId,
        key: NodeKey,
        transform: Transform,
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        if node.data.kind() != NodeKind::Group {
            return Err(Error::WrongKind);
        }
        if node.transform == transform {
            return Ok(());
        }
        node.transform = transform;
        self.mark(key, Dirty::TRANSFORM);
        Ok(())
    }

    /// Set a node's opacity; it multiplies with every ancestor's. Clamped to
    /// `0.0..=1.0`.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_opacity(
        &mut self,
        client: ClientId,
        key: NodeKey,
        opacity: f32,
    ) -> Result<(), Error> {
        let opacity = opacity.clamp(0.0, 1.0);
        let node = self.check_mut(client, key)?;
        if node.opacity.to_bits() == opacity.to_bits() {
            return Ok(());
        }
        node.opacity = opacity;
        self.mark(key, Dirty::INHERIT);
        Ok(())
    }

    /// Set whether a group clips its descendants to its bounds.
    ///
    /// **A window's content group always clips** and this call cannot
    /// clear it: `clip: false` on it is [`Error::RootNode`], the same
    /// answer [`destroy_node`](Scene::destroy_node) and
    /// [`reparent`](Scene::reparent) give for the other two things a
    /// client may not do to a node the window owns. `clip: true` on it
    /// stays the no-op the equality check below always made it, so a
    /// client that re-asserts the flag on its own window node is not
    /// disconnected for a redundant request. Clearing it is refused
    /// rather than silently ignored because a client asking for it has a
    /// layout that will now be cut off, and a compositor that answers
    /// "done" to a request it did not honour teaches the client the
    /// wrong thing.
    ///
    /// A toolkit's own clip is a *different* node and is unaffected
    /// either way: `nitro-ui` clips its root widget's group, which hangs
    /// under the window's content group rather than being it.
    ///
    /// See [`create_window_with`](Scene::create_window_with) for why the
    /// window clips at all, and why its own bounds are the rectangle.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`],
    /// [`Error::RootNode`] when clearing a window content group's clip.
    pub fn set_clip(&mut self, client: ClientId, key: NodeKey, clip: bool) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        if node.data.kind() != NodeKind::Group {
            return Err(Error::WrongKind);
        }
        let window = node.window;
        if node.clip == clip {
            return Ok(());
        }
        // Only ever reached with `clip: false`, since the content group is
        // created clipping: the equality above answers `true` first.
        if self.windows.get(window).is_some_and(|w| w.content == key) {
            return Err(Error::RootNode);
        }
        let node = self.node_mut_ref(key);
        node.clip = clip;
        self.mark(key, Dirty::INHERIT);
        Ok(())
    }

    /// Show or hide a node and its subtree.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn set_visible(
        &mut self,
        client: ClientId,
        key: NodeKey,
        visible: bool,
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        if node.visible == visible {
            return Ok(());
        }
        node.visible = visible;
        self.mark(key, Dirty::INHERIT);
        Ok(())
    }

    /// Set a rect node's fill.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`].
    pub fn set_fill(&mut self, client: ClientId, key: NodeKey, fill: Fill) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        let NodeData::Rect(data) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if data.fill == fill {
            return Ok(());
        }
        data.fill = fill;
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Set a rect node's corner radius, in local units.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`].
    pub fn set_corner_radius(
        &mut self,
        client: ClientId,
        key: NodeKey,
        radius: f32,
    ) -> Result<(), Error> {
        let radius = radius.max(0.0);
        let node = self.check_mut(client, key)?;
        let NodeData::Rect(data) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if data.corner_radius.to_bits() == radius.to_bits() {
            return Ok(());
        }
        data.corner_radius = radius;
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Set (or clear, with `None`) a rect node's border.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`].
    pub fn set_border(
        &mut self,
        client: ClientId,
        key: NodeKey,
        border: Option<Border>,
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        let NodeData::Rect(data) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if data.border == border {
            return Ok(());
        }
        data.border = border;
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Flag a Surface node as scanned out on an underlay hardware plane (or
    /// not). An on-plane Surface paints as a
    /// [`PaintKind::Hole`](crate::PaintKind::Hole); one that is not paints
    /// nothing. Toggling damages the node's bounds; setting the current
    /// value is a no-op.
    ///
    /// Server-side only: there is no client ownership check and no wire
    /// message. The Surface is treated as opaque, so the server must not
    /// flag a translucent one.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::WrongKind`] on anything but a Surface.
    pub fn set_surface_on_plane(&mut self, key: NodeKey, on: bool) -> Result<(), Error> {
        let node = self.nodes.get_mut(key).ok_or(Error::StaleKey)?;
        let NodeData::Surface(data) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if data.on_plane == on {
            return Ok(());
        }
        data.on_plane = on;
        if on {
            self.on_plane.push(key);
        } else {
            self.on_plane.retain(|n| *n != key);
        }
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Whether `key` is a Surface flagged on a plane
    /// ([`Scene::set_surface_on_plane`]). Its content changes damage
    /// nothing: it paints as a hole.
    #[must_use]
    pub fn surface_on_plane(&self, key: NodeKey) -> bool {
        !self.on_plane.is_empty()
            && self
                .nodes
                .get(key)
                .is_some_and(|n| matches!(n.data, NodeData::Surface(s) if s.on_plane))
    }

    /// Whether `output`'s paint list contains any
    /// [`PaintKind::Hole`](crate::PaintKind::Hole): an on-plane Surface that
    /// painted on `output` at the last [`update`](Scene::update), in a
    /// window [`paint_list`](Scene::paint_list) does not leave out.
    ///
    /// O(1) `false` when no Surface is on a plane; otherwise O(on-plane
    /// Surfaces). Reads the cached world state, so call after `update`.
    #[must_use]
    pub fn has_holes(&self, output: OutputId) -> bool {
        if self.on_plane.is_empty() {
            return false;
        }
        let Some(index) = self.output_index(output) else {
            return false;
        };
        let out = self.output_at(index);
        self.on_plane.iter().any(|key| {
            let Some(node) = self.nodes.get(*key) else {
                return false;
            };
            let Some(window) = self.windows.get(node.window) else {
                return false;
            };
            node.painted
                && node.world_visible
                && node.world_opacity > 0.0
                && node.last_output == Some(output)
                && window.output == Some(output)
                && self.admit.admits(window.client)
                && !window.offscreen
                && !(out.top_hidden && window.layer == Layer::Top)
                && node.world_bounds.intersects(&out.rect)
        })
    }

    /// Point a text node at a shaped run, or clear it with `None`.
    ///
    /// The scene does not shape anything: `text.key` is an opaque handle into
    /// the caller's store and `text.size` the extent that store measured. Both
    /// the handle and the measured size are part of the node's appearance, so
    /// a re-shape that changes either is a repaint — and, because the measured
    /// size decides where an aligned block lands inside the bounds, changing
    /// it moves pixels even when the bounds did not change.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`] on
    /// anything but a text node.
    pub fn set_text(
        &mut self,
        client: ClientId,
        key: NodeKey,
        text: Option<TextRef>,
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        let NodeData::Text(slot) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if *slot == text {
            return Ok(());
        }
        *slot = text;
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Point an icon node at an icon, or clear it with `None`.
    ///
    /// The scene does not rasterise anything: `icon.icon` is an opaque handle
    /// into the painter's icon set, exactly as `TextRef::key` is one into its
    /// text store. The *role* is stored rather than a colour, which is the
    /// whole point — the painter resolves it against the current palette on
    /// every frame, so a scheme switch recolours every icon on screen without
    /// a single client message or a single scene mutation.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`] on
    /// anything but an icon node.
    pub fn set_icon(
        &mut self,
        client: ClientId,
        key: NodeKey,
        icon: Option<IconRef>,
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        let NodeData::Icon(slot) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if *slot == icon {
            return Ok(());
        }
        *slot = icon;
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Point an image node at a region of a buffer, or clear it with `None`.
    ///
    /// # Errors
    /// [`Error::StaleKey`] for a dead node or buffer, [`Error::NotOwner`],
    /// [`Error::WrongKind`], [`Error::BadBuffer`] if `src` is empty or leaves
    /// the buffer.
    pub fn set_image(
        &mut self,
        client: ClientId,
        key: NodeKey,
        image: Option<ImageRef>,
    ) -> Result<(), Error> {
        if let Some(image) = image {
            let buffer = self.buffers.get(image.buffer).ok_or(Error::StaleKey)?;
            if !client.may_touch(buffer.client) {
                return Err(Error::NotOwner);
            }
            if image.src.is_empty() || !buffer.desc.full_rect().contains_rect(&image.src) {
                return Err(Error::BadBuffer);
            }
        }
        let node = self.check_mut(client, key)?;
        let NodeData::Image(slot) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        if *slot == image {
            return Ok(());
        }
        let old = slot.take();
        *slot = image;
        self.attach(
            key,
            old.map(|i| (i.buffer, i.src)),
            image.map(|i| (i.buffer, i.src)),
            None,
        );
        Ok(())
    }

    /// Point a surface node at a region of a buffer with its colour
    /// metadata, or clear it with `None`. The buffer bookkeeping and the
    /// swap/damage contract are exactly [`Scene::set_image`]'s; a colour
    /// change repaints the whole node.
    ///
    /// # Errors
    /// As [`Scene::set_image`], with [`Error::WrongKind`] on anything but a
    /// Surface.
    pub fn set_surface(
        &mut self,
        client: ClientId,
        key: NodeKey,
        surface: Option<SurfaceRef>,
    ) -> Result<(), Error> {
        self.set_surface_inner(client, key, surface, None)
    }

    /// The vblank-latch entry point: attach `surface` as
    /// [`Scene::set_surface`] does, but take the damage from `rects`
    /// (buffer pixels; empty means the whole `src`) rather than from the
    /// buffer's recent `buffer_damaged` calls.
    ///
    /// Under the swap rule — same shape and format, same `src` and colour,
    /// the new buffer shown before, or the *same* buffer re-presented —
    /// only `rects` repaint; otherwise the whole node.
    ///
    /// # Errors
    /// As [`Scene::set_surface`].
    pub fn set_surface_with_damage(
        &mut self,
        client: ClientId,
        key: NodeKey,
        surface: SurfaceRef,
        rects: &[IRect],
    ) -> Result<(), Error> {
        self.set_surface_inner(client, key, Some(surface), Some(rects))
    }

    fn set_surface_inner(
        &mut self,
        client: ClientId,
        key: NodeKey,
        surface: Option<SurfaceRef>,
        rects: Option<&[IRect]>,
    ) -> Result<(), Error> {
        if let Some(surface) = surface {
            let buffer = self.buffers.get(surface.buffer).ok_or(Error::StaleKey)?;
            if !client.may_touch(buffer.client) {
                return Err(Error::NotOwner);
            }
            if surface.src.is_empty() || !buffer.desc.full_rect().contains_rect(&surface.src) {
                return Err(Error::BadBuffer);
            }
        }
        let node = self.check_mut(client, key)?;
        let NodeData::Surface(data) = &mut node.data else {
            return Err(Error::WrongKind);
        };
        let old = data.content;
        if old == surface {
            // Re-presenting the current buffer: the client rewrote it in
            // place (it may, once released... or it tears; its problem).
            if let (Some(s), Some(rects)) = (surface, rects)
                && !self.surface_on_plane(key)
            {
                if rects.is_empty() {
                    self.mark(key, Dirty::PAINT);
                } else {
                    self.mark_partial(key, s.src, rects);
                }
            }
            return Ok(());
        }
        data.content = surface;
        let color_changed = matches!((old, surface), (Some(a), Some(b)) if a.color != b.color);
        self.attach(
            key,
            old.map(|s| (s.buffer, s.src)),
            surface.map(|s| (s.buffer, s.src)),
            rects,
        );
        if color_changed && !self.surface_on_plane(key) {
            self.mark(key, Dirty::PAINT);
        }
        Ok(())
    }

    /// The buffer bookkeeping shared by [`Scene::set_image`] and
    /// [`Scene::set_surface`]: move the node from `old`'s users to `new`'s,
    /// queue `old` for release if nothing else uses it, and mark the
    /// node — partially under the swap rule, wholly otherwise. `damage`
    /// overrides the buffer's recent damage (the latch path); an empty
    /// override means the whole node.
    fn attach(
        &mut self,
        key: NodeKey,
        old: Option<(BufferKey, IRect)>,
        new: Option<(BufferKey, IRect)>,
        damage: Option<&[IRect]>,
    ) {
        let swap = self.same_size_swap(old, new);
        // A Surface on a hardware plane paints as a hole whatever it shows
        // (#3899): a new buffer there changes the plane, not a pixel of the
        // output buffer, so it damages nothing.
        let on_plane = self.surface_on_plane(key);
        if let Some((old, _)) = old
            && let Some(users) = self.buffer_users.get_mut(&old)
        {
            users.retain(|n| *n != key);
            if users.is_empty() {
                self.unreferenced.push(old);
            }
        }
        if let Some((buffer, _)) = new {
            self.buffer_users.entry(buffer).or_default().push(key);
            if let Some(buffer) = self.buffers.get_mut(buffer) {
                buffer.shown = true;
            }
        }
        if on_plane {
            // Nothing to repaint.
        } else if swap && let Some((buffer, src)) = new {
            // The client promises the new buffer matches the old one outside
            // the rects it damages in this commit (`docs/wire.md`), so only
            // those need repainting — whether they arrived before this call
            // (`recent`) or arrive after it (`buffer_damaged` finds the node
            // on its new buffer).
            match damage {
                Some([]) => self.mark(key, Dirty::PAINT),
                Some(rects) => self.mark_partial(key, src, rects),
                None => {
                    if let Some(recent) = self.recent.get(&buffer) {
                        let rects: Vec<IRect> = recent.rects().to_vec();
                        self.mark_partial(key, src, &rects);
                    }
                }
            }
        } else {
            self.mark(key, Dirty::PAINT);
        }
    }

    /// Declare which pixels of an image or surface node's buffer are fully
    /// opaque, in buffer pixels; replaces any earlier region, and an empty
    /// slice clears it. Persists across buffer swaps (`set_image`,
    /// `set_surface`, a latched `PresentSurface`). A painter may
    /// then copy those pixels instead of blending them (#3877).
    ///
    /// The node is repainted: output only changes if the client lied about
    /// its alpha, but that is exactly the case the next frame must show.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::WrongKind`] for a
    /// node that is neither an `Image` nor a `Surface`.
    pub fn set_opaque_region(
        &mut self,
        client: ClientId,
        key: NodeKey,
        rects: &[IRect],
    ) -> Result<(), Error> {
        let node = self.check_mut(client, key)?;
        // Image, and Surface since #3919: Chromium's out-of-process GPU
        // presents its AR24 window into an exported Surface, and the frame
        // must keep #3877's opaque copy there.
        if !matches!(node.data, NodeData::Image(_) | NodeData::Surface(_)) {
            return Err(Error::WrongKind);
        }
        let rects: Vec<IRect> = rects.iter().copied().filter(|r| !r.is_empty()).collect();
        if node.opaque == rects {
            return Ok(());
        }
        node.opaque = rects;
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    /// Whether replacing `old` by `new` is a swap between two buffers of the
    /// same shape, sampled at the same `src`, the new one shown before — the
    /// case where the node's pixels change only where the client says so.
    ///
    /// Anything else (a size, format or `src` change, `None` ↔ `Some`, a
    /// buffer never shown yet) has no previous frame to be relative to and
    /// repaints the whole node.
    fn same_size_swap(
        &self,
        old: Option<(BufferKey, IRect)>,
        new: Option<(BufferKey, IRect)>,
    ) -> bool {
        let (Some((old_buf, old_src)), Some((new_buf, new_src))) = (old, new) else {
            return false;
        };
        if old_buf == new_buf || old_src != new_src {
            return false;
        }
        let (Some(a), Some(b)) = (self.buffers.get(old_buf), self.buffers.get(new_buf)) else {
            return false;
        };
        let (da, db) = (a.desc, b.desc);
        b.shown
            && da.w == db.w
            && da.h == db.h
            && da.format == db.format
            && da.is_opaque() == db.is_opaque()
    }

    // --------------------------------------------------------------- buffers

    /// Take ownership of a client's pixels — a [`PixelStore`], which is a
    /// mapping of the client's memfd in production and a `Vec<u8>` in tests.
    ///
    /// # Errors
    /// [`Error::BadBuffer`] if the description is degenerate or `data` is
    /// shorter than `stride * h`.
    pub fn create_buffer(
        &mut self,
        client: ClientId,
        desc: BufferDesc,
        data: impl PixelStore + 'static,
    ) -> Result<BufferKey, Error> {
        desc.validate(data.cpu_readable().then(|| data.bytes().len()))?;
        Ok(self.buffers.insert(Buffer {
            desc,
            client,
            data: Box::new(data),
            shown: false,
        }))
    }

    /// Mutable access to a buffer's pixels.
    ///
    /// Writing here changes nothing on screen until
    /// [`buffer_damaged`](Scene::buffer_damaged) says which pixels moved.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::ReadOnly`] if the
    /// buffer's store is a read-only mapping.
    pub fn buffer_mut(&mut self, client: ClientId, key: BufferKey) -> Result<&mut [u8], Error> {
        let buffer = self.buffers.get_mut(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(buffer.client) {
            return Err(Error::NotOwner);
        }
        buffer.data.bytes_mut().ok_or(Error::ReadOnly)
    }

    /// Write a buffer's pixels while still reading the rest of the scene:
    /// `f` gets the scene and the buffer's bytes at once.
    ///
    /// The buffer's store is taken out for the length of the call (a
    /// swap with an empty placeholder, which allocates nothing), so inside
    /// `f` that one buffer reads as **empty** — an image node sampling it
    /// paints nothing — and every other buffer is as usual. This is how
    /// the server renders scene content *into* a server-owned buffer (the
    /// overview's thumbnail atlas) that the scene also shows elsewhere.
    ///
    /// Like [`buffer_mut`](Scene::buffer_mut), it changes nothing on
    /// screen until [`buffer_damaged`](Scene::buffer_damaged).
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`], [`Error::ReadOnly`].
    pub fn with_buffer_detached<R>(
        &mut self,
        client: ClientId,
        key: BufferKey,
        f: impl FnOnce(&Self, &mut [u8]) -> R,
    ) -> Result<R, Error> {
        let buffer = self.buffers.get_mut(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(buffer.client) {
            return Err(Error::NotOwner);
        }
        let mut store = std::mem::replace(&mut buffer.data, Box::new(Detached));
        let result = match store.bytes_mut() {
            Some(bytes) => Ok(f(self, bytes)),
            None => Err(Error::ReadOnly),
        };
        // `f` only had `&Self`, so the buffer is still there.
        if let Some(buffer) = self.buffers.get_mut(key) {
            buffer.data = store;
        }
        result
    }

    /// Declare which parts of a buffer changed, in buffer pixels.
    ///
    /// Every image node sampling an overlapping source rect gets the
    /// overlap as sub-rect damage: the next [`update`](Scene::update) maps it
    /// into device pixels and repaints only that, falling back to the whole
    /// node only where the mapping is rotated, sheared or flipped. The
    /// rest of the tree is untouched.
    ///
    /// The rects are also remembered until the next `update`, so a
    /// [`set_image`](Scene::set_image) swapping to this buffer afterwards
    /// in the same commit repaints just them.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn buffer_damaged(
        &mut self,
        client: ClientId,
        key: BufferKey,
        rects: &[IRect],
    ) -> Result<(), Error> {
        let buffer = self.buffers.get(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(buffer.client) {
            return Err(Error::NotOwner);
        }
        let recent = self.recent.entry(key).or_default();
        for r in rects {
            recent.add(*r);
        }
        let Some(users) = self.buffer_users.get(&key) else {
            return Ok(());
        };
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.extend(users.iter().copied());
        for node_key in scratch.drain(..) {
            let Some(node) = self.nodes.get(node_key) else {
                continue;
            };
            let Some((buffer, src)) = node.data.buffer_ref() else {
                continue;
            };
            if buffer != key || self.surface_on_plane(node_key) {
                continue;
            }
            self.mark_partial(node_key, src, rects);
        }
        self.scratch = scratch;
        self.prune_buffer_users(key);
        Ok(())
    }

    /// Destroy a buffer. Image nodes referencing it become empty and dirty.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn destroy_buffer(&mut self, client: ClientId, key: BufferKey) -> Result<(), Error> {
        let buffer = self.buffers.get(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(buffer.client) {
            return Err(Error::NotOwner);
        }
        if let Some(users) = self.buffer_users.remove(&key) {
            for node_key in users {
                let Some(node) = self.nodes.get_mut(node_key) else {
                    continue;
                };
                match &mut node.data {
                    NodeData::Image(slot) if slot.is_some_and(|i| i.buffer == key) => {
                        *slot = None;
                    }
                    NodeData::Surface(data) if data.content.is_some_and(|s| s.buffer == key) => {
                        data.content = None;
                    }
                    _ => continue,
                }
                self.mark(node_key, Dirty::PAINT);
            }
        }
        self.buffers.remove(key);
        self.recent.remove(&key);
        Ok(())
    }

    /// Drain the buffers no image node references any more, with their
    /// owning clients, into `out` (which is not cleared).
    ///
    /// Painting always reads the *current* scene, so a live buffer that no
    /// `Image` node references will never be read again, on any output —
    /// its owner may rewrite it. Each buffer is reported once per transition
    /// to unreferenced; a buffer re-attached since, destroyed since, or never
    /// attached is not reported.
    pub fn take_released_buffers(&mut self, out: &mut Vec<(ClientId, BufferKey)>) {
        let mut pending = std::mem::take(&mut self.unreferenced);
        pending.sort_unstable();
        pending.dedup();
        for key in pending.drain(..) {
            let Some(buffer) = self.buffers.get(key) else {
                continue;
            };
            let nodes = &self.nodes;
            let used = self.buffer_users.get(&key).is_some_and(|users| {
                users.iter().any(|n| {
                    nodes
                        .get(*n)
                        .is_some_and(|node| node.data.buffer() == Some(key))
                })
            });
            if !used {
                out.push((buffer.client, key));
            }
        }
        self.unreferenced = pending;
    }

    /// Whether any live Image or Surface node samples `key` right now.
    #[must_use]
    pub fn buffer_in_use(&self, key: BufferKey) -> bool {
        self.buffer_users.get(&key).is_some_and(|users| {
            users.iter().any(|n| {
                self.nodes
                    .get(*n)
                    .is_some_and(|node| node.data.buffer() == Some(key))
            })
        })
    }

    fn prune_buffer_users(&mut self, key: BufferKey) {
        let live = &self.nodes;
        if let Some(users) = self.buffer_users.get_mut(&key) {
            let before = users.len();
            users.retain(|n| {
                live.get(*n)
                    .is_some_and(|node| node.data.buffer() == Some(key))
            });
            if before > 0 && users.is_empty() {
                self.unreferenced.push(key);
            }
        }
    }

    // ------------------------------------------------------------ dirtiness

    /// Mark a node dirty and light the `SUBTREE` trail up to its window root,
    /// so `update` can find it without walking the tree.
    pub(crate) fn mark(&mut self, key: NodeKey, flags: Dirty) {
        let Some(node) = self.nodes.get_mut(key) else {
            return;
        };
        node.dirty.insert(flags);
        let mut cur = key;
        loop {
            let Some(parent) = self.node_ref(cur).parent else {
                let root = self.node_mut_ref(cur);
                if !root.queued {
                    root.queued = true;
                    self.dirty_roots.push(cur);
                }
                return;
            };
            let p = self.node_mut_ref(parent);
            if p.dirty.any(Dirty::SUBTREE) {
                // The trail is already lit all the way to the root.
                return;
            }
            p.dirty.insert(Dirty::SUBTREE);
            cur = parent;
        }
    }

    /// Record `rects` (buffer pixels) clipped to `src` as sub-rect damage on
    /// an image node. A node already due a whole repaint needs nothing more;
    /// rects missing `src` entirely do not dirty the node at all.
    pub(crate) fn mark_partial(&mut self, key: NodeKey, src: IRect, rects: &[IRect]) {
        let Some(node) = self.nodes.get(key) else {
            return;
        };
        if node.dirty.any(Dirty::PAINT) {
            return;
        }
        let mut any = false;
        for r in rects {
            let r = r.intersect(&src);
            if !r.is_empty() {
                self.partial.entry(key).or_default().add(r);
                any = true;
            }
        }
        if any {
            self.mark(key, Dirty::PARTIAL);
        }
    }

    /// Record the pixels a subtree currently occupies as damaged, for
    /// mutations that make the cached bounds unreachable (destroy, reparent).
    fn damage_now(&mut self, key: NodeKey) {
        let node = self.node_ref(key);
        let bounds = node.subtree_bounds;
        if bounds.is_empty() {
            return;
        }
        if let Some(window) = self.windows.get(node.window)
            && let Some(id) = window.output
        {
            self.pending.push((id, bounds));
        }
    }

    /// Windows whose size changed since the last update; acknowledges them.
    pub(crate) fn drain_configures(&mut self, out: &mut Vec<Configure>) {
        let mut resized = std::mem::take(&mut self.resized);
        for key in resized.drain(..) {
            let Some(window) = self.windows.get_mut(key) else {
                continue;
            };
            if window.size == window.configured {
                continue;
            }
            window.configured = window.size;
            let size = window.size;
            out.push(Configure { window: key, size });
        }
        if self.resized.is_empty() {
            self.resized = resized;
        }
    }

    fn note_resized(&mut self, win: WindowKey) {
        if !self.resized.contains(&win) {
            self.resized.push(win);
        }
    }

    // ------------------------------------------------------------- internals

    fn check_mut(&mut self, client: ClientId, key: NodeKey) -> Result<&mut Node, Error> {
        let node = self.nodes.get_mut(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(node.client) {
            return Err(Error::NotOwner);
        }
        Ok(node)
    }

    pub(crate) fn is_ancestor(&self, ancestor: NodeKey, mut node: NodeKey) -> bool {
        while let Some(parent) = self.node_ref(node).parent {
            if parent == ancestor {
                return true;
            }
            node = parent;
        }
        false
    }

    fn subtree_height(&mut self, key: NodeKey) -> u32 {
        let base = self.node_ref(key).depth;
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.push(key);
        let mut max = base;
        while let Some(k) = scratch.pop() {
            let node = self.node_ref(k);
            max = max.max(node.depth);
            scratch.extend(node.children.iter().copied());
        }
        scratch.clear();
        self.scratch = scratch;
        max - base
    }

    /// Re-stamp depth and window ownership after a move.
    fn rewrite_subtree(&mut self, key: NodeKey, depth: u32, window: WindowKey) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.push(key);
        let base = self.node_ref(key).depth;
        while let Some(k) = scratch.pop() {
            let node = self.node_mut_ref(k);
            node.depth = node.depth - base + depth;
            node.window = window;
            scratch.extend(node.children.iter().copied());
        }
        scratch.clear();
        self.scratch = scratch;
    }

    /// Remove a node and everything under it from the arena.
    fn destroy_subtree(&mut self, key: NodeKey) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.push(key);
        while let Some(k) = scratch.pop() {
            let Some(node) = self.nodes.get(k) else {
                continue;
            };
            scratch.extend(node.children.iter().copied());
            if let Some(buffer) = node.data.buffer()
                && let Some(users) = self.buffer_users.get_mut(&buffer)
            {
                users.retain(|n| *n != k);
                if users.is_empty() {
                    self.unreferenced.push(buffer);
                }
            }
            if let NodeData::Surface(surface) = node.data
                && surface.on_plane
            {
                self.on_plane.retain(|n| *n != k);
            }
            self.partial.remove(&k);
            self.nodes.remove(k);
        }
        scratch.clear();
        self.scratch = scratch;
        self.dirty_roots.retain(|r| self.nodes.contains(*r));
    }
}

/// Where a new child goes: in front of `before`, or at the front of the list.
///
/// Children are stored back to front, so "on top" is the end of the vector and
/// `before: None` means topmost.
fn child_position(parent: &Node, before: Option<NodeKey>) -> Result<usize, Error> {
    match before {
        None => Ok(parent.children.len()),
        Some(sibling) => parent
            .children
            .iter()
            .position(|c| *c == sibling)
            .ok_or(Error::BadSibling),
    }
}

/// The placeholder a buffer's store is swapped for by
/// [`Scene::with_buffer_detached`]: zero-sized, so boxing it allocates
/// nothing, and empty.
#[derive(Debug)]
struct Detached;

impl crate::PixelStore for Detached {
    fn bytes(&self) -> &[u8] {
        &[]
    }

    fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        None
    }
}
