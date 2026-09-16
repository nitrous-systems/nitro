//! The retained scene: arenas, the tree, and the mutation API.

use std::collections::HashMap;

use nitro_core::{IRect, Point, Rect, Size, Transform};

use crate::{
    Border, Buffer, BufferDesc, BufferKey, ClientId, Configure, Error, Fill, IconRef, ImageRef,
    Insets, Layer, Node, NodeKey, NodeKind, OutputId, TextRef, Window, WindowFlags, WindowKey,
    WindowState,
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
    /// Window roots carrying dirt, deduplicated by `Node::queued`.
    pub(crate) dirty_roots: Vec<NodeKey>,
    /// Damage from things that are no longer where their cache says (nodes
    /// destroyed, windows unplaced or restacked), flushed at the next
    /// `update`.
    pub(crate) pending: Vec<(OutputId, IRect)>,
    /// Scratch stack for subtree walks that cannot recurse.
    pub(crate) scratch: Vec<NodeKey>,
    /// Windows whose size changed since the last update.
    pub(crate) resized: Vec<WindowKey>,
    pub(crate) stats: UpdateStats,
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
            dirty_roots: Vec::new(),
            pending: Vec::new(),
            scratch: Vec::new(),
            resized: Vec::new(),
            stats: UpdateStats {
                visited_nodes: 0,
                damaged_nodes: 0,
                dirty_roots: 0,
            },
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
    /// plus the window's logical position, scaled by the output's scale.
    ///
    /// Returns the output's index, that transform, and the output's rect (the
    /// root clip).
    pub(crate) fn root_placement(&self, win: WindowKey) -> Option<(usize, Transform, IRect)> {
        let window = self.windows.get(win)?;
        let index = self.output_index(window.output?)?;
        let output = &self.outputs[index];
        let s = output.scale;
        let t = Transform::translate(
            output.rect.x as f32 + window.position.x * s,
            output.rect.y as f32 + window.position.y * s,
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
    /// already in the output's z-order.
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
        {
            let stack = &mut self.outputs[index].layers[layer.index()];
            if !stack.contains(&win) {
                stack.push(win);
            }
        }
        self.mark(root, Dirty::TRANSFORM);
        Ok(())
    }

    /// Move a window to a different stacking layer, keeping it frontmost
    /// within the new layer.
    ///
    /// # Errors
    /// [`Error::StaleKey`].
    pub fn set_layer(&mut self, win: WindowKey, layer: Layer) -> Result<(), Error> {
        let window = self.windows.get_mut(win).ok_or(Error::StaleKey)?;
        if window.layer == layer {
            return Ok(());
        }
        window.layer = layer;
        let output = window.output;
        let root = window.root;
        if let Some(index) = output.and_then(|id| self.output_index(id)) {
            let id = self.outputs[index].id;
            let out = &mut self.outputs[index];
            out.remove(win);
            out.layers[layer.index()].push(win);
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

    fn restack(&mut self, win: WindowKey, front: bool) -> Result<(), Error> {
        let window = self.windows.get(win).ok_or(Error::StaleKey)?;
        let layer = window.layer.index();
        let root = window.root;
        let Some(index) = window.output.and_then(|id| self.output_index(id)) else {
            return Ok(());
        };
        let id = self.outputs[index].id;
        let stack = &mut self.outputs[index].layers[layer];
        let Some(pos) = stack.iter().position(|w| *w == win) else {
            return Ok(());
        };
        let at_end = pos + 1 == stack.len();
        if (front && at_end) || (!front && pos == 0) {
            return Ok(());
        }
        stack.remove(pos);
        if front {
            stack.push(win);
        } else {
            stack.insert(0, win);
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
    /// client may not do to a node the window owns. `clip: true` on it is
    /// the no-op it already was, so a toolkit that sets the flag itself
    /// (`nitro-ui` does, on its root widget's group) keeps working
    /// unchanged. Refused rather than silently ignored because a client
    /// asking for it has a layout that will now be cut off, and a
    /// compositor that answers "done" to a request it did not honour
    /// teaches the client the wrong thing.
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
        if let Some(old) = old
            && let Some(users) = self.buffer_users.get_mut(&old.buffer)
        {
            users.retain(|n| *n != key);
        }
        if let Some(image) = image {
            self.buffer_users.entry(image.buffer).or_default().push(key);
        }
        self.mark(key, Dirty::PAINT);
        Ok(())
    }

    // --------------------------------------------------------------- buffers

    /// Take ownership of a copy of a client's pixels.
    ///
    /// # Errors
    /// [`Error::BadBuffer`] if the description is degenerate or `data` is
    /// shorter than `stride * h`.
    pub fn create_buffer(
        &mut self,
        client: ClientId,
        desc: BufferDesc,
        data: Vec<u8>,
    ) -> Result<BufferKey, Error> {
        desc.validate(data.len())?;
        Ok(self.buffers.insert(Buffer { desc, client, data }))
    }

    /// Mutable access to a buffer's pixels.
    ///
    /// Writing here changes nothing on screen until
    /// [`buffer_damaged`](Scene::buffer_damaged) says which pixels moved.
    ///
    /// # Errors
    /// [`Error::StaleKey`], [`Error::NotOwner`].
    pub fn buffer_mut(&mut self, client: ClientId, key: BufferKey) -> Result<&mut [u8], Error> {
        let buffer = self.buffers.get_mut(key).ok_or(Error::StaleKey)?;
        if !client.may_touch(buffer.client) {
            return Err(Error::NotOwner);
        }
        Ok(&mut buffer.data)
    }

    /// Declare which parts of a buffer changed, in buffer pixels.
    ///
    /// Every image node sampling an overlapping source rect is marked for
    /// repaint; the rest of the tree is untouched.
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
            let NodeData::Image(Some(image)) = node.data else {
                continue;
            };
            if image.buffer != key {
                continue;
            }
            if rects.iter().any(|r| r.intersects(&image.src)) {
                self.mark(node_key, Dirty::PAINT);
            }
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
                let NodeData::Image(slot) = &mut node.data else {
                    continue;
                };
                if slot.is_some_and(|i| i.buffer == key) {
                    *slot = None;
                    self.mark(node_key, Dirty::PAINT);
                }
            }
        }
        self.buffers.remove(key);
        Ok(())
    }

    fn prune_buffer_users(&mut self, key: BufferKey) {
        let live = &self.nodes;
        if let Some(users) = self.buffer_users.get_mut(&key) {
            users.retain(|n| {
                live.get(*n).is_some_and(
                    |node| matches!(node.data, NodeData::Image(Some(i)) if i.buffer == key),
                )
            });
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

    fn is_ancestor(&self, ancestor: NodeKey, mut node: NodeKey) -> bool {
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
            if let NodeData::Image(Some(image)) = node.data
                && let Some(users) = self.buffer_users.get_mut(&image.buffer)
            {
                users.retain(|n| *n != k);
            }
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
