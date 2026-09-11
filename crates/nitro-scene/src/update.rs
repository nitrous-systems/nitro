//! The update pass: recompute world state for dirty subtrees and turn the
//! change into exact per-output damage.
//!
//! The rule is *old ∪ new*. For every node whose device-pixel footprint,
//! visibility, opacity, world transform or appearance changed, both the
//! rectangle it used to occupy and the one it occupies now are damaged: the
//! first so whatever was behind it gets repainted, the second so the node
//! itself appears. "Changed" is decided by comparing cached world state, not
//! the node's own dirty flags — a descendant dragged along by an ancestor
//! carries no flags of its own. Clean nodes are never visited, so an idle
//! scene costs nothing.

use nitro_core::{Damage, IRect, Rect, Transform};

use crate::{
    Configure, NodeKey, OutputId, Scene, UpdateStats,
    node::{DESCEND, Dirty},
};

/// Everything an [`update`](Scene::update) produced.
///
/// Damage is accumulated into the caller's per-output regions (so the server
/// can keep one `Damage` per output across frames); the configures and the
/// stats are fresh each call.
#[derive(Debug, Default)]
pub struct UpdateResult {
    /// Windows whose size changed; the server sends a `Configure` for each.
    pub configures: Vec<Configure>,
    /// Counters for the walk just performed.
    pub stats: UpdateStats,
}

/// Per-output damage sinks handed to [`Scene::update`].
///
/// The server owns one [`Damage`] per output and passes them in; the scene
/// adds to them and never clears them, so damage can accumulate across several
/// updates before a frame is drawn.
///
/// [`Scene::update`] clips every rect to the owning output's device rect on
/// the way in, so a region never contains pixels that output cannot draw. That
/// matters when something moves between outputs: the rectangle it vacated
/// belongs to the output it left, not to the one it arrived on.
pub struct DamageSink<'a> {
    outputs: &'a mut [(OutputId, &'a mut Damage)],
}

impl<'a> DamageSink<'a> {
    /// Wrap a slice of `(output, region)` pairs.
    ///
    /// Damage for an output not in the slice is dropped, which is what the
    /// server wants when it is only redrawing one screen.
    #[must_use]
    pub fn new(outputs: &'a mut [(OutputId, &'a mut Damage)]) -> Self {
        Self { outputs }
    }

    /// Add a device-pixel rect to one output's region, as given.
    ///
    /// [`Scene::update`] clips to the output before calling this; a caller
    /// adding damage by hand is trusted to pass a rect on that output.
    pub fn add(&mut self, id: OutputId, rect: IRect) {
        if rect.is_empty() {
            return;
        }
        for (out, damage) in &mut *self.outputs {
            if *out == id {
                damage.add(rect);
                return;
            }
        }
    }

    /// Add a rect after clipping it to `bounds` (the output's device rect).
    fn add_clipped(&mut self, id: OutputId, rect: IRect, bounds: IRect) {
        self.add(id, rect.intersect(&bounds));
    }
}

/// The output a walk is emitting damage for.
#[derive(Clone, Copy)]
struct Target {
    id: OutputId,
    /// The output's device rect; every emitted rect is clipped to it.
    rect: IRect,
}

/// State carried down the recursive walk.
#[derive(Clone, Copy)]
struct Inherited {
    transform: Transform,
    clip: IRect,
    opacity: f32,
    visible: bool,
}

impl Scene {
    /// Recompute every dirty subtree and add the resulting damage to `sink`.
    ///
    /// Damage is *old ∪ new*: the pixels a changed node used to cover plus the
    /// ones it covers now, both already narrowed by the clips in force and by
    /// the owning output's rect. A second call with nothing dirty adds nothing
    /// and visits nothing.
    pub fn update(&mut self, sink: &mut DamageSink<'_>) -> UpdateResult {
        self.stats = UpdateStats::default();

        // Damage banked by mutations whose cached bounds were about to become
        // unreachable (a destroyed node, an unplaced or restacked window).
        // Each entry names the output it belongs to, so clipping it to that
        // output drops anything that output could not draw anyway.
        let mut pending = std::mem::take(&mut self.pending);
        for (id, rect) in pending.drain(..) {
            let bounds = self
                .outputs
                .iter()
                .find(|o| o.id == id)
                .map_or(IRect::EMPTY, |o| o.rect);
            sink.add_clipped(id, rect, bounds);
            self.stats.damaged_nodes += 1;
        }
        if self.pending.is_empty() {
            self.pending = pending;
        }

        let mut roots = std::mem::take(&mut self.dirty_roots);
        for root in roots.drain(..) {
            let Some(node) = self.nodes.get_mut(root) else {
                continue;
            };
            node.queued = false;
            if node.dirty.is_clean() {
                continue;
            }
            self.stats.dirty_roots += 1;
            let window = self.node_ref(root).window;
            match self.root_placement(window) {
                Some((index, transform, clip)) => {
                    let target = Target {
                        id: self.outputs[index].id,
                        rect: self.outputs[index].rect,
                    };
                    let inherited = Inherited {
                        transform,
                        clip,
                        opacity: 1.0,
                        visible: true,
                    };
                    // `force: false` — the root's own dirty flags decide
                    // whether its subtree is revisited. Passing `true` here
                    // would cascade to every descendant and walk the whole
                    // tree on any change at all.
                    self.visit(root, &inherited, target, sink, false);
                }
                None => {
                    // Off-screen: keep the caches coherent but damage nothing.
                    self.clear_subtree(root);
                }
            }
        }
        if self.dirty_roots.is_empty() {
            self.dirty_roots = roots;
        }

        let mut configures = Vec::new();
        self.drain_configures(&mut configures);
        UpdateResult {
            configures,
            stats: self.stats,
        }
    }

    /// Recompute one node, damage what changed, and descend as needed.
    ///
    /// `force` is set when an ancestor's transform, bounds, clip, opacity or
    /// visibility changed, so this node's world state must be recomputed even
    /// if it is itself clean. Returns the subtree's device-pixel extent.
    fn visit(
        &mut self,
        key: NodeKey,
        inherited: &Inherited,
        target: Target,
        sink: &mut DamageSink<'_>,
        force: bool,
    ) -> IRect {
        self.stats.visited_nodes += 1;
        let node = self.node_ref(key);
        let dirty = node.dirty;
        let self_changed = force || dirty.any(DESCEND.union(Dirty::PAINT));

        let old_bounds = node.world_bounds;
        let old_painted = node.painted;
        let old_transform = node.world_transform;
        let old_opacity = node.world_opacity;
        let old_visible = node.world_visible;

        // World transform: parent space, then this node's offset within it,
        // then (for groups) the node's own transform.
        let local = Transform::translate(node.bounds.x, node.bounds.y);
        let world_transform = inherited.transform.then(&local);
        let child_transform = world_transform.then(&node.transform);

        let visible = inherited.visible && node.visible;
        let opacity = inherited.opacity * node.opacity;
        let device_rect = device_rect(&world_transform, node.local_rect());
        let clip_rect = inherited.clip;
        let child_clip = if node.clip {
            clip_rect.intersect(&device_rect)
        } else {
            clip_rect
        };

        let paints = visible && opacity > 0.0 && node.has_content();
        let world_bounds = if paints {
            device_rect.intersect(&clip_rect)
        } else {
            IRect::EMPTY
        };

        let node = self.node_mut_ref(key);
        node.world_transform = world_transform;
        node.world_opacity = opacity;
        node.world_visible = visible;
        node.clip_rect = clip_rect;
        node.child_clip = child_clip;
        node.world_bounds = world_bounds;
        node.painted = paints;

        // A node's own pixels changed if it moved, appeared, vanished, or was
        // repainted in place. Damaging both the old and the new rectangle is
        // the whole contract: the first repaints whatever was behind it, the
        // second draws it where it is now. A node that merely *contains*
        // something that changed adds nothing of its own — its children
        // account for themselves, and the mutations that make a cached
        // rectangle unreachable (destroy, reparent, unplace, restack) bank it
        // before it is lost.
        //
        // The test is on the *cached world state*, not on this node's own
        // dirty flags: a descendant dragged along by an ancestor's opacity,
        // visibility or transform change carries no flags of its own, so
        // asking `dirty` would miss it. Comparing what was actually painted
        // last time against what will be painted now catches every case —
        // including the ones that leave the bounding box alone, such as
        // fading a group or rotating a square through 90°.
        if self_changed && (old_painted || paints) {
            let moved = old_bounds != world_bounds;
            let appeared = old_painted != paints;
            let recomposed = old_opacity.to_bits() != opacity.to_bits()
                || old_visible != visible
                || old_transform != world_transform;
            if moved || appeared || recomposed || dirty.any(Dirty::PAINT) {
                // `old_bounds` was clipped to whichever output the node was on
                // last time, which need not be this one; clip both to the
                // output actually being damaged.
                sink.add_clipped(target.id, old_bounds, target.rect);
                sink.add_clipped(target.id, world_bounds, target.rect);
                self.stats.damaged_nodes += 1;
            }
        }

        let descend_all = force || dirty.any(DESCEND);
        let child_inherited = Inherited {
            transform: child_transform,
            clip: child_clip,
            opacity,
            visible,
        };

        let mut subtree = world_bounds;
        let children = self.take_children(key);
        for child in &children {
            let child_dirty = self.node_ref(*child).dirty;
            if descend_all || !child_dirty.is_clean() {
                let extent = self.visit(*child, &child_inherited, target, sink, descend_all);
                subtree = subtree.union(&extent);
            } else {
                subtree = subtree.union(&self.node_ref(*child).subtree_bounds);
            }
        }
        self.put_children(key, children);

        let node = self.node_mut_ref(key);
        node.subtree_bounds = subtree;
        node.dirty = Dirty::NONE;

        subtree
    }

    /// Clear the dirty flags of an unplaced subtree and blank its caches, so
    /// nothing stale is left for a later placement to damage.
    fn clear_subtree(&mut self, root: NodeKey) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.push(root);
        while let Some(key) = scratch.pop() {
            let node = self.node_mut_ref(key);
            node.dirty = Dirty::NONE;
            node.world_bounds = IRect::EMPTY;
            node.subtree_bounds = IRect::EMPTY;
            node.painted = false;
            scratch.extend(node.children.iter().copied());
            self.stats.visited_nodes += 1;
        }
        scratch.clear();
        self.scratch = scratch;
    }

    /// Borrow a node's children out of the arena so the walk can recurse with
    /// `&mut self`, and hand the same vector straight back afterwards — so the
    /// allocation stays the node's own and steady state allocates nothing.
    ///
    /// Safe only because the update walk never changes the tree's shape: no
    /// child can appear while the list is out on loan.
    fn take_children(&mut self, key: NodeKey) -> Vec<NodeKey> {
        std::mem::take(&mut self.node_mut_ref(key).children)
    }

    fn put_children(&mut self, key: NodeKey, children: Vec<NodeKey>) {
        let slot = &mut self.node_mut_ref(key).children;
        debug_assert!(
            slot.is_empty(),
            "the update walk must not add children while the list is on loan"
        );
        *slot = children;
    }
}

/// Device-pixel bounding box of a local rect under a transform.
pub(crate) fn device_rect(transform: &Transform, local: Rect) -> IRect {
    if local.is_empty() {
        return IRect::EMPTY;
    }
    transform.apply_rect(&local).round_out()
}
