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
    /// Union of the damage of every frame queued since the last latch.
    pub damage: Damage,
    /// Some frame since the last latch damaged everything (empty damage).
    pub whole: bool,
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
}

/// The per-node frame queue.
#[derive(Debug, Default)]
pub struct Latch {
    queued: HashMap<NodeKey, Queued>,
}

impl Latch {
    /// Queue `frame` on `node`, superseding any frame already queued.
    /// Returns the superseded frame, for the caller to release its buffer
    /// to its presenter if nothing shows it — or `None` when there was none
    /// or it is the new frame's buffer too. The damage accumulates across
    /// supersedes.
    pub fn queue(&mut self, node: NodeKey, mut frame: Queued, rects: &[IRect]) -> Option<Queued> {
        if rects.is_empty() {
            frame.whole = true;
        }
        for r in rects {
            frame.damage.add(*r);
        }
        let old = self.queued.remove(&node);
        if let Some(old) = &old {
            frame.whole |= old.whole;
            frame.damage.add_all(&old.damage);
        }
        self.queued.insert(node, frame);
        let new = self.queued.get(&node).map(|q| (q.token, q.buffer));
        old.filter(|o| Some((o.token, o.buffer)) != new)
    }

    /// Drop `node`'s queued frame (a committed `SetSurface` won), returning
    /// it for the caller to release its buffer to its presenter.
    pub fn cancel(&mut self, node: NodeKey) -> Option<Queued> {
        self.queued.remove(&node)
    }

    /// Drop `node`'s queued frame only if `token` presented it (an import
    /// was revoked or dropped, #3904).
    pub fn cancel_from(&mut self, node: NodeKey, token: u64) -> Option<Queued> {
        if self.queued.get(&node)?.token != token {
            return None;
        }
        self.queued.remove(&node)
    }

    /// Forget everything a disconnected client queued, silently.
    pub fn forget_client(&mut self, token: u64) {
        self.queued.retain(|_, q| q.token != token);
    }

    /// Whether any frame is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    /// The node's queued frame, if any.
    #[must_use]
    pub fn get(&self, node: NodeKey) -> Option<&Queued> {
        self.queued.get(&node)
    }

    /// Latch every queued frame whose node `ready` says may be latched
    /// now, into `scene`. Frames on a node or buffer that no longer exists
    /// are dropped silently (a destroy wins, `docs/wire.md`).
    pub fn latch_ready(
        &mut self,
        scene: &mut Scene,
        mut ready: impl FnMut(&Scene, NodeKey) -> bool,
    ) -> Vec<Latched> {
        let mut out = Vec::new();
        let keys: Vec<NodeKey> = self.queued.keys().copied().collect();
        for node in keys {
            let alive = scene.node(node).is_ok() && {
                let q = &self.queued[&node];
                scene.buffer(q.buffer).is_ok()
            };
            if !alive {
                self.queued.remove(&node);
                continue;
            }
            if !ready(scene, node) {
                continue;
            }
            let Some(q) = self.queued.remove(&node) else {
                continue;
            };
            let rects: Vec<IRect> = if q.whole {
                Vec::new()
            } else {
                q.damage.rects().to_vec()
            };
            let surface = SurfaceRef::new(q.buffer, q.src, q.color);
            // The server acts: the presenter may be an importer that owns
            // the buffer but not the node (#3904). Both were checked at
            // receipt, and a stale key fails here anyway.
            if scene
                .set_surface_with_damage(ClientId::SERVER, node, surface, &rects)
                .is_ok()
            {
                out.push(Latched {
                    token: q.token,
                    client: q.client,
                    serial: q.serial,
                    node,
                });
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
    /// (`format` at the node's device-pixel size, v1's CPU-path answer)
    /// and return the ones that changed. Call after a scene update. Dead
    /// nodes are dropped; an empty device rect sends nothing.
    pub fn changed(&mut self, scene: &Scene, format: u32) -> Vec<Hint> {
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
            let now = (format, device.w.cast_unsigned(), device.h.cast_unsigned());
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
/// asks for "the server's choice" (#3914, v1): `NV12` if some plane of the
/// node's output lists linear NV12, else `YUYV` if one lists it, else
/// `XR24`. The planes module (#3899) replaces this with its own answer.
#[must_use]
pub fn default_scanout_format(planes: &[nitro_kms::PlaneInfo]) -> u32 {
    use nitro_kms::{Fourcc, MOD_LINEAR};
    use nitro_wire::types::format;
    let listed = |f: Fourcc| planes.iter().any(|p| p.supports(f, MOD_LINEAR));
    if listed(Fourcc::NV12) {
        format::NV12
    } else if listed(Fourcc::YUYV) {
        format::YUYV
    } else {
        format::XR24
    }
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
        }
    }

    #[test]
    fn a_newer_frame_supersedes_and_unions_damage() {
        let (mut s, n, b) = world();
        let mut l = Latch::default();
        assert!(
            l.queue(n, frame(b[0], 1), &[IRect::new(0, 0, 2, 2)])
                .is_none()
        );
        assert_eq!(
            l.queue(n, frame(b[1], 2), &[IRect::new(4, 4, 2, 2)])
                .map(|q| q.buffer),
            Some(b[0])
        );
        // Same buffer re-queued: nothing to release.
        assert!(
            l.queue(n, frame(b[1], 3), &[IRect::new(8, 8, 1, 1)])
                .is_none()
        );
        let q = l.get(n).unwrap();
        assert_eq!(q.serial, 3);
        assert_eq!(q.damage.rects().len(), 3);
        assert!(!q.whole);
        // Held back while not ready.
        assert!(l.latch_ready(&mut s, |_, _| false).is_empty());
        let latched = l.latch_ready(&mut s, |_, _| true);
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
        assert_eq!(l.cancel(n).map(|q| q.buffer), Some(b[2]));
        l.queue(n, frame(b[2], 1), &[]);
        assert!(l.cancel_from(n, 8).is_none(), "another presenter's frame");
        assert_eq!(l.cancel_from(n, 7).map(|q| q.buffer), Some(b[2]));
        assert!(l.is_empty());
        l.queue(n, frame(b[2], 2), &[]);
        s.destroy_buffer(C, b[2]).unwrap();
        assert!(l.latch_ready(&mut s, |_, _| true).is_empty());
        assert!(l.is_empty());
        l.queue(n, frame(b[0], 3), &[]);
        l.forget_client(7);
        assert!(l.is_empty());
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
        assert_eq!(h.changed(&s, 1), vec![(7, NodeId(5), 1, 16, 16)]);
        assert!(h.changed(&s, 1).is_empty());
        s.set_bounds(C, n, Rect::new(0.0, 0.0, 32.0, 8.0)).unwrap();
        s.update(&mut nitro_scene::DamageSink::new(&mut [(
            nitro_scene::OutputId(0),
            &mut d,
        )]));
        assert_eq!(h.changed(&s, 1), vec![(7, NodeId(5), 1, 32, 8)]);
        s.destroy_node(C, n).unwrap();
        assert!(h.changed(&s, 1).is_empty());
    }
}
