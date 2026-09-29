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
    Configure, NodeKey, OutputId, Scene, UpdateStats, WindowKey,
    node::{DESCEND, Dirty},
};

/// A **hint** that one output's change in an [`update`](Scene::update)
/// was, apart from `foreign`, a pure integral translation of one subtree.
///
/// Everything is in global device pixels. The normal damage is reported
/// exactly as it would be without the hint, so a consumer that ignores it
/// is still correct; one that honours it may copy pixels it already has
/// instead of repainting them. What it may copy is narrower than "the
/// subtree": see [`Scene::translation_cover`] and the server's frame
/// module for the rule.
///
/// Emitted only when every one of these held for the node, `node`:
///
/// * its only dirt was a transform and/or bounds change — no paint,
///   opacity, visibility, clip or structure change on it, and **nothing
///   dirty anywhere below it**;
/// * the linear part of the transform its children are placed with is
///   bitwise unchanged, and the translation part moved by a whole number of
///   device pixels from a whole-pixel origin to a whole-pixel origin (so
///   the rasterizer's output is the same pixels, shifted);
/// * either the node itself stayed put (`moves_node == false`, the
///   children scroll inside it and `clip` is its own clip for them), or it
///   moved by the same delta as its children without changing size
///   (`moves_node == true`, and `clip` is the clip it is itself inside);
/// * it was on this output at the previous update, and no other node on
///   the same output qualified in the same update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation {
    /// The output the subtree is on.
    pub output: OutputId,
    /// The node whose subtree moved.
    pub node: NodeKey,
    /// Whether `node`'s own content moved too, or only its descendants.
    pub moves_node: bool,
    /// Device-pixel delta, new minus old.
    pub delta: (i32, i32),
    /// The fixed clip the moved content is confined to, before and after.
    pub clip: IRect,
    /// Every other rect this update damaged on `output`: nodes outside the
    /// subtree, other windows, damage banked by earlier mutations. An
    /// over-approximation (merged), which is the safe direction.
    pub foreign: Damage,
}

/// Transient state of the translation detection during one update.
#[derive(Debug, Default)]
pub(crate) struct TxState {
    /// How many candidate subtrees the walk is currently inside (0 or 1).
    depth: u32,
    /// Damage emitted outside any candidate, per output.
    foreign: Vec<(OutputId, Damage)>,
    /// Candidates found, `foreign` not filled in yet.
    found: Vec<Translation>,
    /// Damage inside offscreen windows, per window.
    offscreen: Vec<(WindowKey, IRect)>,
}

impl TxState {
    fn foreign(&mut self, id: OutputId) -> &mut Damage {
        if let Some(i) = self.foreign.iter().position(|(o, _)| *o == id) {
            return &mut self.foreign[i].1;
        }
        self.foreign.push((id, Damage::new()));
        &mut self.foreign.last_mut().expect("just pushed").1
    }
}

/// Dirt that rules a node out as a translation candidate.
const NOT_TRANSLATION: Dirty = Dirty::PAINT
    .union(Dirty::INHERIT)
    .union(Dirty::STRUCTURE)
    .union(Dirty::SUBTREE)
    .union(Dirty::PARTIAL);

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
    /// Pure-translation hints, at most one per output. See [`Translation`].
    pub translations: Vec<Translation>,
    /// Damage inside [offscreen](Scene::set_offscreen) windows, in global
    /// device pixels clipped to the window's output, one entry per rect
    /// (unmerged, possibly overlapping). None of it is in the sink: these
    /// pixels are not on any output, so nothing here is in a
    /// [`Translation`]'s `foreign` either, and no hint comes from inside
    /// an offscreen window.
    pub offscreen: Vec<(WindowKey, IRect)>,
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
}

/// The output a walk is emitting damage for.
#[derive(Clone, Copy)]
struct Target {
    id: OutputId,
    /// The output's device rect; every emitted rect is clipped to it.
    rect: IRect,
    /// The window being walked, when it is offscreen: its damage goes to
    /// [`UpdateResult::offscreen`] instead of the sink.
    offscreen: Option<WindowKey>,
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
        // Damage remembered for a later same-size swap only lives until the
        // commit(s) it arrived with are drawn.
        self.recent.clear();
        self.tx = TxState::default();

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
            let rect = rect.intersect(&bounds);
            sink.add(id, rect);
            if !rect.is_empty() {
                self.tx.foreign(id).add(rect);
            }
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
                        offscreen: self
                            .windows
                            .get(window)
                            .is_some_and(|w| w.offscreen)
                            .then_some(window),
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
        let tx = std::mem::take(&mut self.tx);
        let mut translations = tx.found;
        // Two moved subtrees on one output cannot be served by one copy.
        let crowded: Vec<OutputId> = translations
            .iter()
            .filter(|t| translations.iter().filter(|u| u.output == t.output).count() > 1)
            .map(|t| t.output)
            .collect();
        translations.retain(|t| !crowded.contains(&t.output));
        for t in &mut translations {
            if let Some((_, d)) = tx.foreign.iter().find(|(o, _)| *o == t.output) {
                t.foreign = d.clone();
            }
        }
        UpdateResult {
            configures,
            stats: self.stats,
            translations,
            offscreen: tx.offscreen,
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
        let dirty = self.node_ref(key).dirty;
        let self_changed = force || dirty.any(DESCEND.union(Dirty::PAINT).union(Dirty::PARTIAL));
        // An entry exists exactly while the flag is set; taking it here (and
        // in `clear_subtree`) means it never outlives the flag, whichever
        // path below ends up damaging the node.
        let partial = if dirty.any(Dirty::PARTIAL) {
            self.partial.remove(&key)
        } else {
            None
        };
        let node = self.node_ref(key);

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

        let size = (node.bounds.w, node.bounds.h);
        let candidate = self.candidate(
            key,
            dirty,
            force,
            target,
            [world_transform, child_transform],
            [clip_rect, child_clip],
        );

        let node = self.node_mut_ref(key);
        node.world_child_transform = child_transform;
        node.last_size = size;
        node.last_output = Some(target.id);
        node.world_transform = world_transform;
        node.world_opacity = opacity;
        node.world_visible = visible;
        node.clip_rect = clip_rect;
        node.child_clip = child_clip;
        node.world_bounds = world_bounds;
        node.painted = paints;
        if candidate.is_some() {
            self.tx.depth += 1;
        }

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
                self.emit(sink, target, old_bounds);
                self.emit(sink, target, world_bounds);
                self.stats.damaged_nodes += 1;
            } else if paints && let Some(partial) = partial {
                self.emit_partial(sink, target, key, &world_transform, world_bounds, &partial);
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
        if let Some(t) = candidate {
            self.tx.depth -= 1;
            self.tx.found.push(t);
        }

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
            self.partial.remove(&key);
            let node = self.node_mut_ref(key);
            node.world_bounds = IRect::EMPTY;
            node.subtree_bounds = IRect::EMPTY;
            node.painted = false;
            node.last_output = None;
            scratch.extend(node.children.iter().copied());
            self.stats.visited_nodes += 1;
        }
        scratch.clear();
        self.scratch = scratch;
    }

    /// Whether `key`, not dragged along by an ancestor and not inside
    /// another candidate, moved its subtree rigidly this update. Reads the
    /// *old* cached state off the node, so call before overwriting it.
    ///
    /// `transforms` is the new `[world, child]` transform pair and `clips`
    /// the new `[clip_rect, child_clip]`, as `visit` computed them.
    fn candidate(
        &self,
        key: NodeKey,
        dirty: Dirty,
        force: bool,
        target: Target,
        transforms: [Transform; 2],
        clips: [IRect; 2],
    ) -> Option<Translation> {
        if force || self.tx.depth > 0 || target.offscreen.is_some() {
            return None;
        }
        let [world_transform, child_transform] = transforms;
        let [clip_rect, child_clip] = clips;
        let node = self.node_ref(key);
        let size = (node.bounds.w, node.bounds.h);
        if dirty.is_clean()
            || dirty.any(NOT_TRANSLATION)
            || node.last_output != Some(target.id)
            || node.last_size.0.to_bits() != size.0.to_bits()
            || node.last_size.1.to_bits() != size.1.to_bits()
        {
            return None;
        }
        let (moves_node, delta, clip) = translation(
            [node.world_transform, world_transform],
            [node.world_child_transform, child_transform],
            [node.clip_rect, clip_rect],
            [node.child_clip, child_clip],
        )?;
        Some(Translation {
            output: target.id,
            node: key,
            moves_node,
            delta,
            clip,
            foreign: Damage::new(),
        })
    }

    /// Damage the device rects an image node's partially-updated buffer
    /// texels map to.
    fn emit_partial(
        &mut self,
        sink: &mut DamageSink<'_>,
        target: Target,
        key: NodeKey,
        world_transform: &Transform,
        world_bounds: IRect,
        partial: &Damage,
    ) {
        // Only part of the image's buffer changed. Nothing moved, so
        // the old and new footprints agree and the damaged texels
        // map to one device rect each — or, where the mapping is not
        // a plain translate or integer scale, the whole node.
        let node = self.node_ref(key);
        let src = node.data.buffer_ref().map(|(_, src)| src);
        let size = (node.bounds.w, node.bounds.h);
        // All or nothing: one rect needing the fallback makes the
        // whole node the damage, which covers the rest anyway.
        let mapped: Option<Vec<IRect>> = src.and_then(|src| {
            partial
                .rects()
                .iter()
                .map(|r| partial_device_rect(world_transform, size, src, *r))
                .collect()
        });
        match mapped {
            Some(rects) => {
                for d in rects {
                    self.emit(sink, target, d.intersect(&world_bounds));
                }
            }
            None => self.emit(sink, target, world_bounds),
        }
    }

    /// Add one rect of damage for `target`, clipped to it, and note it as
    /// foreign unless the walk is inside a translation candidate — or, for
    /// an offscreen window, record it per window and nothing else.
    fn emit(&mut self, sink: &mut DamageSink<'_>, target: Target, rect: IRect) {
        let rect = rect.intersect(&target.rect);
        if rect.is_empty() {
            return;
        }
        if let Some(win) = target.offscreen {
            self.tx.offscreen.push((win, rect));
            return;
        }
        sink.add(target.id, rect);
        if self.tx.depth == 0 {
            self.tx.foreign(target.id).add(rect);
        }
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

/// Given `[old, new]` pairs of a candidate's cached world state, whether
/// its subtree moved rigidly: `(moves_node, delta, fixed clip)`.
fn translation(
    transform: [Transform; 2],
    child_transform: [Transform; 2],
    clip_rect: [IRect; 2],
    child_clip: [IRect; 2],
) -> Option<(bool, (i32, i32), IRect)> {
    let delta = integral_delta(&child_transform[0], &child_transform[1])?;
    if delta == (0, 0) {
        return None;
    }
    let (moves_node, clip) = if same_transform(&transform[0], &transform[1]) {
        // The node stayed; its children scroll inside its own clip.
        (child_clip[0] == child_clip[1]).then_some((false, child_clip[1]))?
    } else {
        // The node moved with its children, inside its parent's clip.
        let own = integral_delta(&transform[0], &transform[1])?;
        (own == delta && clip_rect[0] == clip_rect[1]).then_some((true, clip_rect[1]))?
    };
    (!clip.is_empty()).then_some((moves_node, delta, clip))
}

/// Bitwise equality: "did this change", not "are these close".
fn same_transform(a: &Transform, b: &Transform) -> bool {
    [a.a, a.b, a.c, a.d, a.e, a.f]
        .iter()
        .zip([b.a, b.b, b.c, b.d, b.e, b.f])
        .all(|(x, y)| x.to_bits() == y.to_bits())
}

/// The whole-pixel translation taking `old` to `new`, if that is all that
/// changed: the linear parts bitwise equal and both origins on whole
/// device pixels (so the shift is exact in `f32` and the rasterizer draws
/// the same pixels, moved).
fn integral_delta(old: &Transform, new: &Transform) -> Option<(i32, i32)> {
    let linear = |t: &Transform| [t.a, t.b, t.c, t.d].map(f32::to_bits);
    if linear(old) != linear(new) {
        return None;
    }
    // Well inside `f32`'s exact-integer range, so `v + d` is exact too.
    let whole = |v: f32| {
        (v.is_finite() && v.round().to_bits() == v.to_bits() && v.abs() < 8_388_608.0)
            .then_some(v as i32)
    };
    Some((whole(new.e)? - whole(old.e)?, whole(new.f)? - whole(old.f)?))
}

/// Device-pixel bounding box of a local rect under a transform.
pub(crate) fn device_rect(transform: &Transform, local: Rect) -> IRect {
    if local.is_empty() {
        return IRect::EMPTY;
    }
    transform.apply_rect(&local).round_out()
}

/// How far a scale may be from an integer and a device origin from a whole
/// pixel and still be treated as exact — the rasterizer's own tolerance for
/// its 1:1 blit (`nitro-raster`'s `blit_impl`).
const EXACT: f32 = 1e-4;

/// Device-pixel rect covering the output of damaged buffer texels `rect` of
/// an image node sampling `src` into a `size` box under `world`, or `None`
/// when the mapping is too general to bound tightly (rotation, shear, a
/// fractional or non-integer scale, a sub-pixel origin), in which case the
/// caller damages the whole node.
///
/// At scale 1 on a whole-pixel origin the rasterizer copies texels 1:1, so
/// the rect maps exactly. At an integer scale above 1 it samples bilinearly,
/// so a destination pixel next to a damaged texel blends it in too: the
/// source rect is widened by one texel on each side before mapping.
pub(crate) fn partial_device_rect(
    world: &Transform,
    size: (f32, f32),
    src: IRect,
    rect: IRect,
) -> Option<IRect> {
    let rect = rect.intersect(&src);
    if rect.is_empty() {
        return Some(IRect::EMPTY);
    }
    if !world.is_axis_aligned() || src.w <= 0 || src.h <= 0 {
        return None;
    }
    let sx = world.a * size.0 / src.w as f32;
    let sy = world.d * size.1 / src.h as f32;
    let integral = |v: f32| v.is_finite() && v >= 1.0 - EXACT && (v - v.round()).abs() < EXACT;
    if !integral(sx) || !integral(sy) {
        return None;
    }
    // The device origin of the node's box: whole pixels only.
    let (ox, oy) = (world.e, world.f);
    if !ox.is_finite() || !oy.is_finite() {
        return None;
    }
    if (ox - ox.round()).abs() >= EXACT || (oy - oy.round()).abs() >= EXACT {
        return None;
    }
    let (sx, sy) = (sx.round() as i32, sy.round() as i32);
    let rect = if sx == 1 && sy == 1 {
        rect
    } else {
        IRect::from_edges(rect.x - 1, rect.y - 1, rect.right() + 1, rect.bottom() + 1)
            .intersect(&src)
    };
    let (ox, oy) = (ox.round() as i32, oy.round() as i32);
    Some(IRect::from_edges(
        ox + (rect.x - src.x) * sx,
        oy + (rect.y - src.y) * sy,
        ox + (rect.right() - src.x) * sx,
        oy + (rect.bottom() - src.y) * sy,
    ))
}
