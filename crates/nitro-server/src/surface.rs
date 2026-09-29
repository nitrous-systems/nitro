//! Surface frames (#3897): the vblank latch for `PresentSurface` and the
//! `SurfaceHint` tracker.
//!
//! The latch is deliberately outside the transaction machinery. A frame is
//! **queued** on its node when it arrives; at most one is queued per node
//! and a newer one supersedes it (the caller releases the superseded
//! buffer at once). At the node's output's next paint opportunity — no
//! flip pending, which in steady state is the wakeup right after a vblank —
//! the queued frame is **latched**: it becomes the node's current buffer
//! exactly as a committed `SetSurface` would, with the damage of every
//! frame queued since the last latch. The precise contract is in
//! `docs/wire.md` under `PresentSurface`.

use std::collections::HashMap;

use nitro_core::{Damage, IRect, Rect};
use nitro_scene::{BufferKey, ClientId, NodeKey, NodeKind, Scene, SurfaceColor, SurfaceRef};
use nitro_wire::types::NodeId;

/// Most frames queued per node (#3918). With acquire fences a node can
/// hold several: frames whose fences have not signalled wait behind the
/// shown one. On overflow the oldest is dropped at once, like a supersede.
pub const MAX_QUEUED: usize = 4;

/// A pending acquire fence, as [`crate::dmabuf::FenceSet`] names it.
pub type FenceKey = u64;

/// One queued frame.
#[derive(Debug, Clone)]
pub struct Queued {
    /// The presenting connection. Not necessarily the node's owner: an
    /// importer (#3904) presents into another client's node.
    pub token: u64,
    /// The presenting client, as the scene knows it.
    pub client: ClientId,
    /// The buffer to show.
    pub buffer: BufferKey,
    /// Answered with `Presented{serial}` once shown.
    pub serial: u32,
    /// Source rect in buffer pixels.
    pub src: IRect,
    /// Colour metadata.
    pub color: SurfaceColor,
    /// Union of this frame's damage and that of every frame it superseded.
    pub damage: Damage,
    /// Some frame folded into this one damaged everything (empty damage).
    pub whole: bool,
    /// The acquire fence still pending (#3918); `None` once the frame is
    /// ready. A frame is never latched while this is `Some`. The planes
    /// module (#3899) may instead latch early and hand the fence to the
    /// display as `IN_FENCE_FD` (`Backend::set_plane_fence`).
    pub fence: Option<FenceKey>,
}

impl Queued {
    fn absorb(&mut self, older: &Queued) {
        self.whole |= older.whole;
        self.damage.add_all(&older.damage);
    }
}

/// A frame latched into the scene: whom to answer and with what serial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Latched {
    /// The owning connection.
    pub token: u64,
    /// The owning client.
    pub client: ClientId,
    /// The frame's serial.
    pub serial: u32,
    /// The node it landed on.
    pub node: NodeKey,
    /// The buffer it showed.
    pub buffer: BufferKey,
    /// What the node showed before, if anything: the server ends its CPU
    /// read bracket on a dma-buf (#3918).
    pub previous: Option<BufferKey>,
}

/// What one [`Latch::latch_ready`] pass did.
#[derive(Debug, Default)]
pub struct LatchOutcome {
    /// Frames now current.
    pub latched: Vec<Latched>,
    /// Frames an older position lost to a newer ready one: the caller
    /// releases their buffers (unless shown) and drops their fences. No
    /// `Presented`.
    pub superseded: Vec<Queued>,
    /// Frames on a node or buffer that no longer exists: dropped silently
    /// (a destroy wins), fences to drop.
    pub dropped: Vec<Queued>,
}

/// The per-node frame queues, oldest first.
#[derive(Debug, Default)]
pub struct Latch {
    queued: HashMap<NodeKey, Vec<Queued>>,
}

impl Latch {
    /// Queue `frame` on `node`. A **ready** frame (no fence) supersedes
    /// every frame queued before it, ready or not: the latch would pick
    /// it over them anyway. An unready one waits behind them. Returns the
    /// frames that lost, for the caller to release (unless their buffer
    /// is still queued, [`Latch::holds`]) and to drop their fences.
    /// Their damage carries into the frame that replaced them.
    pub fn queue(&mut self, node: NodeKey, mut frame: Queued, rects: &[IRect]) -> Vec<Queued> {
        if rects.is_empty() {
            frame.whole = true;
        }
        for r in rects {
            frame.damage.add(*r);
        }
        let q = self.queued.entry(node).or_default();
        let mut lost = Vec::new();
        if frame.fence.is_none() {
            for old in q.drain(..) {
                frame.absorb(&old);
                lost.push(old);
            }
        }
        q.push(frame);
        while q.len() > MAX_QUEUED {
            let old = q.remove(0);
            q[0].absorb(&old);
            lost.push(old);
        }
        lost
    }

    /// Whether a frame `token` presented with `buffer` is still queued
    /// anywhere: such a buffer is not released yet.
    #[must_use]
    pub fn holds(&self, token: u64, buffer: BufferKey) -> bool {
        self.queued
            .values()
            .flatten()
            .any(|f| f.token == token && f.buffer == buffer)
    }

    /// Mark the frame waiting on `fence` ready. Returns whether one was.
    pub fn fence_signalled(&mut self, fence: FenceKey) -> bool {
        for f in self.queued.values_mut().flatten() {
            if f.fence == Some(fence) {
                f.fence = None;
                return true;
            }
        }
        false
    }

    /// Drop `node`'s queued frames (a committed `SetSurface` won),
    /// returning them for the caller to release and un-fence.
    pub fn cancel(&mut self, node: NodeKey) -> Vec<Queued> {
        self.queued.remove(&node).unwrap_or_default()
    }

    /// Drop `node`'s frames that `token` presented (an import was revoked
    /// or dropped, #3904).
    pub fn cancel_from(&mut self, node: NodeKey, token: u64) -> Vec<Queued> {
        let Some(q) = self.queued.get_mut(&node) else {
            return Vec::new();
        };
        let (gone, keep): (Vec<_>, Vec<_>) = q.drain(..).partition(|f| f.token == token);
        if keep.is_empty() {
            self.queued.remove(&node);
        } else {
            *q = keep;
        }
        gone
    }

    /// Forget everything a disconnected client queued, returning it so the
    /// fences can be dropped.
    pub fn forget_client(&mut self, token: u64) -> Vec<Queued> {
        let mut out = Vec::new();
        self.queued.retain(|_, q| {
            let (gone, keep): (Vec<_>, Vec<_>) = q.drain(..).partition(|f| f.token == token);
            out.extend(gone);
            *q = keep;
            !q.is_empty()
        });
        out
    }

    /// Whether any frame is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    /// The node's newest queued frame, if any.
    #[must_use]
    pub fn get(&self, node: NodeKey) -> Option<&Queued> {
        self.queued.get(&node)?.last()
    }

    /// How many frames `node` has queued.
    #[must_use]
    pub fn depth(&self, node: NodeKey) -> usize {
        self.queued.get(&node).map_or(0, Vec::len)
    }

    /// For every node `ready` says may be latched now, latch its **newest
    /// ready** frame into `scene`. Older frames lose (superseded, their
    /// damage carried in); newer, still-fenced frames stay queued. Frames
    /// on a node or buffer that no longer exists are dropped silently (a
    /// destroy wins, `docs/wire.md`).
    ///
    /// # Panics
    /// Never: the picked index is in range by construction.
    pub fn latch_ready(
        &mut self,
        scene: &mut Scene,
        mut ready: impl FnMut(&Scene, NodeKey) -> bool,
    ) -> LatchOutcome {
        let mut out = LatchOutcome::default();
        let keys: Vec<NodeKey> = self.queued.keys().copied().collect();
        for node in keys {
            let Some(mut q) = self.queued.remove(&node) else {
                continue;
            };
            if scene.node(node).is_err() {
                out.dropped.extend(q);
                continue;
            }
            let (dead, live): (Vec<_>, Vec<_>) =
                q.drain(..).partition(|f| scene.buffer(f.buffer).is_err());
            out.dropped.extend(dead);
            q = live;
            let pick = q.iter().rposition(|f| f.fence.is_none());
            let Some(i) = pick.filter(|_| ready(scene, node)) else {
                if !q.is_empty() {
                    self.queued.insert(node, q);
                }
                continue;
            };
            let rest = q.split_off(i + 1);
            let mut f = q.pop().expect("index i exists");
            for old in &q {
                f.absorb(old);
            }
            let rects: Vec<IRect> = if f.whole {
                Vec::new()
            } else {
                f.damage.rects().to_vec()
            };
            let previous = scene
                .node(node)
                .ok()
                .and_then(nitro_scene::Node::surface)
                .and_then(|d| d.content)
                .map(|c| c.buffer);
            let surface = SurfaceRef::new(f.buffer, f.src, f.color);
            // The server acts: the presenter may be an importer that owns
            // the buffer but not the node (#3904). Both were checked at
            // receipt, and a stale key fails here anyway.
            if scene
                .set_surface_with_damage(ClientId::SERVER, node, surface, &rects)
                .is_ok()
            {
                out.latched.push(Latched {
                    token: f.token,
                    client: f.client,
                    serial: f.serial,
                    node,
                    buffer: f.buffer,
                    previous,
                });
            }
            out.superseded.extend(q);
            if !rest.is_empty() {
                self.queued.insert(node, rest);
            }
        }
        out
    }
}

/// One `SurfaceHint` target: who owns the node and what was last sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HintState {
    token: u64,
    id: NodeId,
    sent: Option<(u32, u32, u32)>,
}

/// A hint to send: `(token, node id, fourcc, width, height)`.
pub type Hint = (u64, NodeId, u32, u32, u32);

/// Tracks the Surface nodes of clients that listed `SURFACE` and what
/// `SurfaceHint` each was last sent. Keyed by node *and* connection: a
/// shared Surface (#3904) hints its owner and its importer, each under
/// its own id.
#[derive(Debug, Default)]
pub struct Hints {
    nodes: HashMap<(NodeKey, u64), HintState>,
}

impl Hints {
    /// Start tracking a Surface node for connection `token`, which names
    /// it `id`.
    pub fn track(&mut self, node: NodeKey, token: u64, id: NodeId) {
        self.nodes.entry((node, token)).or_insert(HintState {
            token,
            id,
            sent: None,
        });
    }

    /// Stop tracking `node` for `token` (an import went away).
    pub fn untrack(&mut self, node: NodeKey, token: u64) {
        self.nodes.remove(&(node, token));
    }

    /// Stop tracking a client's nodes.
    pub fn forget_client(&mut self, token: u64) {
        self.nodes.retain(|_, h| h.token != token);
    }

    /// Recompute every tracked node's preferred format and device size
    /// (`format(scene, node)` — the node's output's answer, #3899 — at
    /// the node's device-pixel size) and return the ones that changed.
    /// Call after a scene update. Dead nodes are dropped; an empty device
    /// rect sends nothing.
    pub fn changed(&mut self, scene: &Scene, format: impl Fn(&Scene, NodeKey) -> u32) -> Vec<Hint> {
        let mut out = Vec::new();
        self.nodes.retain(|(key, _), h| {
            let Ok(node) = scene.node(*key) else {
                return false;
            };
            if node.kind() != NodeKind::Surface {
                return false;
            }
            let device = device_rect(node);
            if device.is_empty() {
                return true;
            }
            let now = (
                format(scene, *key),
                device.w.cast_unsigned(),
                device.h.cast_unsigned(),
            );
            if h.sent != Some(now) {
                h.sent = Some(now);
                out.push((h.token, h.id, now.0, now.1, now.2));
            }
            true
        });
        out
    }
}

/// A Surface node's size in device pixels: its bounds through its world
/// transform, rounded out. What `SurfaceHint` reports.
fn device_rect(node: &nitro_scene::Node) -> IRect {
    let b = node.bounds();
    node.world_transform()
        .apply_rect(&Rect::new(0.0, 0.0, b.w, b.h))
        .round_out()
}

/// A Surface node's hinted size in device pixels, `None` while it has no
/// device rect (#3914: the `0 × 0` of `AllocSurfaceBuffers`).
#[must_use]
pub fn hinted_size(scene: &Scene, key: NodeKey) -> Option<(u32, u32)> {
    let d = device_rect(scene.node(key).ok()?);
    (!d.is_empty()).then(|| (d.w.cast_unsigned(), d.h.cast_unsigned()))
}

/// The format a server-allocated scanout buffer gets when the client
/// asks for "the server's choice": [`crate::planes::alloc_format`].
#[must_use]
pub fn default_scanout_format(planes: &[nitro_kms::PlaneInfo]) -> u32 {
    crate::planes::alloc_format(planes)
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)]
mod tests {
    use super::*;
    use nitro_core::{Point, Size};
    use nitro_scene::{BufferDesc, Layer};

    const C: ClientId = ClientId(1);

    fn world() -> (Scene, NodeKey, Vec<BufferKey>) {
        let mut s = Scene::new();
        s.add_output(nitro_scene::OutputId(0), IRect::new(0, 0, 200, 200), 1.0);
        let win = s.create_window(C, "w", Size::new(100.0, 100.0), Layer::Normal);
        s.place_window(win, Some(nitro_scene::OutputId(0)), Point::ZERO)
            .unwrap();
        let root = s.window_info(win).unwrap().root();
        let n = s.create_node(C, NodeKind::Surface, root, None).unwrap();
        s.set_bounds(C, n, Rect::new(0.0, 0.0, 16.0, 16.0)).unwrap();
        let d = BufferDesc::new(16, 16, 64, 0x3432_5258);
        let bufs = (0..3)
            .map(|_| s.create_buffer(C, d, vec![0; d.byte_len()]).unwrap())
            .collect();
        (s, n, bufs)
    }

    fn frame(buffer: BufferKey, serial: u32) -> Queued {
        Queued {
            token: 7,
            client: C,
            buffer,
            serial,
            src: IRect::new(0, 0, 16, 16),
            color: SurfaceColor::default(),
            damage: Damage::new(),
            whole: false,
            fence: None,
        }
    }

    fn fenced(buffer: BufferKey, serial: u32, fence: FenceKey) -> Queued {
        Queued {
            fence: Some(fence),
            ..frame(buffer, serial)
        }
    }

    fn shown(s: &Scene, n: NodeKey) -> BufferKey {
        s.node(n)
            .unwrap()
            .surface()
            .unwrap()
            .content
            .unwrap()
            .buffer
    }

    #[test]
    fn a_newer_frame_supersedes_and_unions_damage() {
        let (mut s, n, b) = world();
        let mut l = Latch::default();
        assert!(
            l.queue(n, frame(b[0], 1), &[IRect::new(0, 0, 2, 2)])
                .is_empty()
        );
        let lost = l.queue(n, frame(b[1], 2), &[IRect::new(4, 4, 2, 2)]);
        assert_eq!(lost.iter().map(|q| q.buffer).collect::<Vec<_>>(), [b[0]]);
        // Same buffer re-queued: the caller sees it is still held.
        let lost = l.queue(n, frame(b[1], 3), &[IRect::new(8, 8, 1, 1)]);
        assert_eq!(lost.len(), 1);
        assert!(l.holds(7, lost[0].buffer));
        let q = l.get(n).unwrap();
        assert_eq!(q.serial, 3);
        assert_eq!(q.damage.rects().len(), 3);
        assert!(!q.whole);
        // Held back while not ready.
        assert!(l.latch_ready(&mut s, |_, _| false).latched.is_empty());
        let latched = l.latch_ready(&mut s, |_, _| true).latched;
        assert_eq!(latched.len(), 1);
        assert_eq!(latched[0].serial, 3);
        assert!(l.is_empty());
        assert_eq!(
            s.node(n)
                .unwrap()
                .surface()
                .unwrap()
                .content
                .unwrap()
                .buffer,
            b[1]
        );
    }

    #[test]
    fn empty_damage_is_sticky_until_the_latch() {
        let (_, n, b) = world();
        let mut l = Latch::default();
        l.queue(n, frame(b[0], 1), &[]);
        l.queue(n, frame(b[1], 2), &[IRect::new(0, 0, 1, 1)]);
        assert!(l.get(n).unwrap().whole);
    }

    #[test]
    fn cancel_and_destroy_drop_the_frame() {
        let (mut s, n, b) = world();
        let mut l = Latch::default();
        l.queue(n, frame(b[2], 1), &[]);
        assert_eq!(l.cancel(n)[0].buffer, b[2]);
        l.queue(n, frame(b[2], 1), &[]);
        assert!(l.cancel_from(n, 8).is_empty(), "another presenter's frame");
        assert_eq!(l.cancel_from(n, 7)[0].buffer, b[2]);
        assert!(l.is_empty());
        l.queue(n, frame(b[2], 2), &[]);
        s.destroy_buffer(C, b[2]).unwrap();
        let o = l.latch_ready(&mut s, |_, _| true);
        assert!(o.latched.is_empty());
        assert_eq!(o.dropped.len(), 1);
        assert!(l.is_empty());
        l.queue(n, fenced(b[0], 3, 9), &[]);
        assert_eq!(l.forget_client(7)[0].fence, Some(9));
        assert!(l.is_empty());
    }

    #[test]
    fn a_ready_frame_latches_while_a_newer_one_waits_for_its_fence() {
        let (mut s, n, b) = world();
        let mut l = Latch::default();
        l.queue(n, frame(b[0], 1), &[]);
        assert!(l.queue(n, fenced(b[1], 2, 5), &[]).is_empty());
        let o = l.latch_ready(&mut s, |_, _| true);
        assert_eq!(o.latched[0].serial, 1);
        assert!(o.superseded.is_empty());
        assert_eq!(shown(&s, n), b[0]);
        assert_eq!(l.depth(n), 1, "B waits");
        // Not ready yet: nothing more latches.
        assert!(l.latch_ready(&mut s, |_, _| true).latched.is_empty());
        assert!(l.fence_signalled(5));
        assert!(!l.fence_signalled(5));
        let o = l.latch_ready(&mut s, |_, _| true);
        assert_eq!(o.latched[0].serial, 2);
        assert_eq!(shown(&s, n), b[1]);
        assert!(l.is_empty());
    }

    #[test]
    fn a_newer_ready_frame_overtakes_an_unready_one() {
        let (mut s, n, b) = world();
        let mut l = Latch::default();
        l.queue(n, fenced(b[0], 1, 5), &[IRect::new(0, 0, 1, 1)]);
        // A ready frame supersedes everything queued before it.
        let lost = l.queue(n, frame(b[1], 2), &[IRect::new(2, 2, 1, 1)]);
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0].serial, lost[0].fence), (1, Some(5)));
        assert_eq!(l.get(n).unwrap().damage.rects().len(), 2);
        let o = l.latch_ready(&mut s, |_, _| true);
        assert_eq!(o.latched[0].serial, 2);
        assert!(!l.fence_signalled(5), "the dropped frame's fence is gone");
    }

    #[test]
    fn the_newest_ready_frame_wins_and_older_ones_are_superseded() {
        let (mut s, n, b) = world();
        let mut l = Latch::default();
        l.queue(n, fenced(b[0], 1, 5), &[]);
        l.queue(n, fenced(b[1], 2, 6), &[]);
        l.queue(n, fenced(b[2], 3, 7), &[]);
        // Signal out of order: the second becomes ready first.
        l.fence_signalled(6);
        l.fence_signalled(5);
        let o = l.latch_ready(&mut s, |_, _| true);
        assert_eq!(o.latched[0].serial, 2);
        assert_eq!(
            o.superseded.iter().map(|q| q.serial).collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(l.depth(n), 1);
        assert_eq!(l.get(n).unwrap().fence, Some(7));
    }

    #[test]
    fn overflow_drops_the_oldest() {
        let (_, n, b) = world();
        let mut l = Latch::default();
        for i in 0..MAX_QUEUED {
            assert!(
                l.queue(n, fenced(b[i % 3], i as u32, i as u64), &[])
                    .is_empty()
            );
        }
        let lost = l.queue(n, fenced(b[0], 99, 99), &[IRect::new(0, 0, 1, 1)]);
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].serial, 0);
        assert_eq!(l.depth(n), MAX_QUEUED);
    }

    #[test]
    fn hints_fire_on_first_size_and_on_change() {
        let (mut s, n, _) = world();
        let mut h = Hints::default();
        h.track(n, 7, NodeId(5));
        let mut d = Damage::new();
        s.update(&mut nitro_scene::DamageSink::new(&mut [(
            nitro_scene::OutputId(0),
            &mut d,
        )]));
        assert_eq!(h.changed(&s, |_, _| 1), vec![(7, NodeId(5), 1, 16, 16)]);
        assert!(h.changed(&s, |_, _| 1).is_empty());
        s.set_bounds(C, n, Rect::new(0.0, 0.0, 32.0, 8.0)).unwrap();
        s.update(&mut nitro_scene::DamageSink::new(&mut [(
            nitro_scene::OutputId(0),
            &mut d,
        )]));
        assert_eq!(h.changed(&s, |_, _| 1), vec![(7, NodeId(5), 1, 32, 8)]);
        s.destroy_node(C, n).unwrap();
        assert!(h.changed(&s, |_, _| 1).is_empty());
    }
}
