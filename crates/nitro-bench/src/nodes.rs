//! The demo effects as **scene-graph mutations** rather than pixels.
//!
//! # Why every effect here has a twin in [`crate::pixels`]
//!
//! This is the argument the whole benchmark exists to settle. A bouncing
//! ball can be drawn two ways:
//!
//! * **as pixels** — recompute the sphere into a fullscreen buffer every
//!   frame and hand the server 8 MB. Cost is proportional to the
//!   *screen*.
//! * **as a node** — upload the sphere once as a sprite and then send
//!   nothing but a new rectangle. Cost is proportional to the *change*.
//!
//! The visual output is the same ball. `DESIGN.md`'s first goal says work
//! must be proportional to what changed; these scenarios are that goal
//! reduced to a pair of numbers that can be put next to each other. Any
//! difference between them is not a benchmark artefact — the two arms
//! drive the *same* simulation out of [`crate::effects`], so the ball is
//! in the same place at the same frame in both.
//!
//! The same pairing is done for the starfield (N stars as N tiny `Rect`
//! nodes vs. one buffer) because it is the case where the node arm might
//! plausibly *lose*: a thousand two-pixel rects is a thousand mutations
//! and a thousand damage rectangles, against one big memcpy. Reporting
//! only the case that flatters the design would be advocacy, not
//! measurement.

use nitro_core::{IRect, Rect};
use nitro_wire::msg::{ClientMsg, CreateBuffer, CreateNode, SetBounds, SetFill, SetImage};
use nitro_wire::types::{BufferId, NodeId, NodeKind, format};

use crate::effects::{Balls, Boing, Starfield};
use crate::harness::{Ctx, Error, FIRST_NODE, Scenario, WINDOW};
use crate::pixels::memfd;
use crate::x11perf::bench_color;

/// Buffer id of the Boing sprite.
pub const SPRITE_BUFFER: BufferId = BufferId(2);

/// The Boing ball as **one `Image` node that moves**.
///
/// The Amiga Boing Ball (Dale Luck and R.J. Mical, CES 1984) was itself a
/// demonstration of exactly this trick: the Amiga did not redraw the
/// sphere, it animated a palette and moved a bob. Doing it as a moved
/// sprite here is not a modernisation, it is the original technique, and
/// the pixel arm next to it is the brute-force version the Amiga could
/// not have afforded.
///
/// The sprite is uploaded **once**, at build time, and the per-frame cost
/// is a single `SetBounds` — 24 bytes on the wire against 8 294 400 for
/// the fullscreen arm, which is a factor the report prints rather than
/// rounds.
pub struct BoingNode {
    /// The shared simulation; the pixel arm uses the same one.
    boing: Boing,
    /// Sprite edge in pixels.
    edge: u32,
    /// Node id of the moving image.
    node: NodeId,
}

impl BoingNode {
    /// A Boing whose ball is a sprite of `edge` pixels, bouncing in a
    /// `width`×`height` box.
    #[must_use]
    pub fn new(width: u32, height: u32, edge: u32) -> Self {
        Self {
            boing: Boing::new(width, height),
            edge,
            node: NodeId(FIRST_NODE),
        }
    }
}

impl Scenario for BoingNode {
    fn name(&self) -> &'static str {
        "boing-node"
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        self.boing = Boing::new(ctx.size.w as u32, ctx.size.h as u32);
        ctx.size_px = self.edge;
        let sprite = self.boing.sprite(self.edge, 0);
        let fd = memfd(&sprite.data)?;
        let (x, y, r) = self.boing.position(0);
        Ok(vec![
            CreateNode {
                id: self.node,
                kind: NodeKind::Image,
                parent: WINDOW,
                before: NodeId::NONE,
            }
            .into(),
            CreateBuffer {
                id: SPRITE_BUFFER,
                width: self.edge,
                height: self.edge,
                stride: sprite.stride,
                // `AR24`, not `XR24`: the sprite is a circle in a square,
                // so the corners must be transparent or the ball drags a
                // black box around with it — and blending them is the
                // composite work this arm is meant to include.
                format: format::AR24,
                size: sprite.byte_len() as u32,
                fd,
            }
            .into(),
            SetImage {
                id: self.node,
                buffer: SPRITE_BUFFER,
                src: IRect::new(0, 0, self.edge.cast_signed(), self.edge.cast_signed()),
            }
            .into(),
            SetBounds {
                id: self.node,
                rect: Rect::new(x - r, y - r, r * 2.0, r * 2.0),
            }
            .into(),
        ])
    }

    fn frame(&mut self, _ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        let (x, y, r) = self.boing.position(frame + 1);
        Ok(vec![
            SetBounds {
                id: self.node,
                rect: Rect::new(x - r, y - r, r * 2.0, r * 2.0),
            }
            .into(),
        ])
    }
}

/// The starfield as **N small `Rect` nodes**, one per star.
///
/// The arm that might lose, and is included because of it. A thousand
/// stars is a thousand `SetBounds` per frame and a thousand damage
/// rectangles for the server to union; the buffer arm is one memcpy of
/// the whole screen. Where those two curves cross — as a function of N —
/// is the most useful single number this benchmark produces for somebody
/// deciding how to build a real widget.
pub struct StarNodes {
    /// The shared simulation.
    stars: Starfield,
    /// How many stars.
    count: usize,
}

impl StarNodes {
    /// `count` stars in a `width`×`height` box.
    #[must_use]
    pub fn new(count: usize, width: u32, height: u32) -> Self {
        Self {
            stars: Starfield::new(count, width, height),
            count,
        }
    }

    /// Node id of the `i`th star.
    #[must_use]
    pub fn node(i: usize) -> NodeId {
        NodeId(FIRST_NODE + i as u32)
    }
}

impl Scenario for StarNodes {
    fn name(&self) -> &'static str {
        "starfield-nodes"
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        self.stars = Starfield::new(self.count, ctx.size.w as u32, ctx.size.h as u32);
        let mut out = Vec::with_capacity(self.count * 3);
        let snapshot: Vec<_> = self
            .stars
            .stars(0)
            .iter()
            .map(|s| (s.x, s.y, s.size))
            .collect();
        for (i, (x, y, size)) in snapshot.into_iter().enumerate() {
            out.push(
                CreateNode {
                    id: Self::node(i),
                    kind: NodeKind::Rect,
                    parent: WINDOW,
                    before: NodeId::NONE,
                }
                .into(),
            );
            out.push(
                SetBounds {
                    id: Self::node(i),
                    rect: Rect::new(x, y, size, size),
                }
                .into(),
            );
            // White, and it stays white: a star's brightness *should*
            // follow its depth, but re-sending a fill every frame would
            // double this arm's mutation count and make the comparison
            // with the buffer arm a comparison of two different amounts of
            // work. The buffer arm shades its stars; the doc says so.
            out.push(
                SetFill {
                    id: Self::node(i),
                    fill: nitro_wire::msg::Fill::Solid(nitro_core::Color::WHITE),
                }
                .into(),
            );
        }
        Ok(out)
    }

    fn frame(&mut self, _ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        Ok(self
            .stars
            .stars(frame + 1)
            .iter()
            .enumerate()
            .map(|(i, s)| {
                SetBounds {
                    id: Self::node(i),
                    rect: Rect::new(s.x, s.y, s.size, s.size),
                }
                .into()
            })
            .collect())
    }
}

/// Bouncing balls as rounded `Rect` nodes.
///
/// x11perf never had this one; every 90s toolkit demo did, and it is the
/// shape a real UI animation actually takes — a handful of moving,
/// rounded, coloured boxes. A rounded rect with `corners = d/2` **is** a
/// circle, so the server's rounded-rect path is being asked to do
/// antialiased circle rasterization N times a frame, which is the most
/// expensive per-pixel work in the rect scenarios.
pub struct BallNodes {
    /// The shared simulation.
    balls: Balls,
    /// How many balls.
    count: usize,
}

impl BallNodes {
    /// `count` balls bouncing in a `width`×`height` box.
    #[must_use]
    pub fn new(count: usize, width: u32, height: u32) -> Self {
        Self {
            balls: Balls::new(count, width, height),
            count,
        }
    }

    /// Node id of the `i`th ball.
    #[must_use]
    pub fn node(i: usize) -> NodeId {
        NodeId(FIRST_NODE + i as u32)
    }
}

impl Scenario for BallNodes {
    fn name(&self) -> &'static str {
        "balls-nodes"
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        self.balls = Balls::new(self.count, ctx.size.w as u32, ctx.size.h as u32);
        let snapshot: Vec<_> = self
            .balls
            .balls(0)
            .iter()
            .map(|b| (b.x, b.y, b.d, b.color))
            .collect();
        let mut out = Vec::with_capacity(self.count * 4);
        for (i, (x, y, d, color)) in snapshot.into_iter().enumerate() {
            out.push(
                CreateNode {
                    id: Self::node(i),
                    kind: NodeKind::Rect,
                    parent: WINDOW,
                    before: NodeId::NONE,
                }
                .into(),
            );
            out.push(
                SetBounds {
                    id: Self::node(i),
                    rect: Rect::new(x, y, d, d),
                }
                .into(),
            );
            out.push(
                SetFill {
                    id: Self::node(i),
                    fill: nitro_wire::msg::Fill::Solid(bench_color(color)),
                }
                .into(),
            );
            out.push(
                nitro_wire::msg::SetCorners {
                    id: Self::node(i),
                    radius: d / 2.0,
                }
                .into(),
            );
        }
        Ok(out)
    }

    fn frame(&mut self, _ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        Ok(self
            .balls
            .balls(frame + 1)
            .iter()
            .enumerate()
            .map(|(i, b)| {
                SetBounds {
                    id: Self::node(i),
                    rect: Rect::new(b.x, b.y, b.d, b.d),
                }
                .into()
            })
            .collect())
    }
}

/// `x11perf -scroll500`: a tall content node moved inside a clipping
/// group, one line per frame.
///
/// X11's `-scroll` is a `CopyArea` of the window onto itself — the
/// classic terminal scroll, where the server blits the existing pixels up
/// and repaints only the newly exposed line. Nitro has no `CopyArea`, and
/// asking for one would miss the point: a **retained** scroll is a
/// transform on a group, and the server decides for itself whether that
/// can be a copy or has to be a repaint.
///
/// So this scenario is the honest port: a clipping group holding a column
/// of rects taller than the window, whose offset moves one line per
/// frame. `damage_px_mean` then answers the question the x11perf number
/// was a proxy for — is a scroll the size of the viewport, or the size of
/// the exposed line? The two differ by the viewport's height in rows, and
/// which one this server does is a fact about it, not about the benchmark.
pub struct Scroll {
    /// Rows of content; the column is this many rects tall.
    pub rows: usize,
    /// Height of one row in logical pixels.
    pub row_h: f32,
    /// The clipping group.
    group: NodeId,
}

impl Scroll {
    /// A scroller with `rows` rows of `row_h` pixels.
    #[must_use]
    pub fn new(rows: usize, row_h: f32) -> Self {
        Self {
            rows,
            row_h,
            group: NodeId(FIRST_NODE),
        }
    }

    /// Node id of the `i`th row.
    #[must_use]
    pub fn row(i: usize) -> NodeId {
        NodeId(FIRST_NODE + 1 + i as u32)
    }
}

impl Scenario for Scroll {
    fn name(&self) -> &'static str {
        "scroll"
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        let mut out = vec![
            CreateNode {
                id: self.group,
                kind: NodeKind::Group,
                parent: WINDOW,
                before: NodeId::NONE,
            }
            .into(),
            SetBounds {
                id: self.group,
                rect: Rect::new(0.0, 0.0, ctx.size.w, ctx.size.h),
            }
            .into(),
            nitro_wire::msg::SetClip {
                id: self.group,
                clip: true,
            }
            .into(),
        ];
        for i in 0..self.rows {
            out.push(
                CreateNode {
                    id: Self::row(i),
                    kind: NodeKind::Rect,
                    parent: self.group,
                    before: NodeId::NONE,
                }
                .into(),
            );
            out.push(
                SetBounds {
                    id: Self::row(i),
                    rect: Rect::new(0.0, i as f32 * self.row_h, ctx.size.w, self.row_h - 1.0),
                }
                .into(),
            );
            out.push(
                SetFill {
                    id: Self::row(i),
                    fill: nitro_wire::msg::Fill::Solid(bench_color(i)),
                }
                .into(),
            );
        }
        Ok(out)
    }

    fn frame(&mut self, ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        // The group's bounds are its origin; moving them scrolls the whole
        // column in one mutation, which is the retained scroll. One row
        // per frame, wrapping at the content's height, so the scenario
        // runs for ever without drifting into an empty region.
        let content = self.rows as f32 * self.row_h;
        let offset = -((frame as f32 * self.row_h) % content.max(1.0));
        Ok(vec![
            SetBounds {
                id: self.group,
                rect: Rect::new(0.0, offset, ctx.size.w, ctx.size.h + content),
            }
            .into(),
        ])
    }
}

/// `x11perf -create / -map`: windows created and destroyed every frame.
///
/// The one x11perf scenario whose *units* survive the transposition
/// unchanged, because window creation is not a per-frame paint: the
/// question really is "how many per second". A retained-scene client
/// creates a subtree and destroys it, which is the toolkit operation
/// behind opening a menu, a tooltip or a dialog — the interaction where a
/// user notices latency most and where a benchmark almost never looks.
///
/// The invariant this scenario is really testing is in the server's
/// `nodes` counter: after the run it must come back to what it was before.
/// A leak of one node per menu is invisible for an hour and fatal for a
/// session, and it is exactly what the `stats_before`/`stats_after` pair
/// in every record makes checkable.
pub struct CreateDestroy {
    /// How many nodes each cycle creates.
    pub count: usize,
    /// The subtree root of the current generation.
    generation: u32,
}

impl CreateDestroy {
    /// A scenario that churns `count` nodes per frame.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self {
            count,
            generation: 0,
        }
    }

    /// The root id of generation `g`.
    ///
    /// Generations alternate between two id ranges rather than counting
    /// upward for ever: a client id space is 32 bits, a `Commit` is
    /// atomic, and reusing an id in the *same* transaction that destroys
    /// it is the one ordering a client may not rely on. Two ranges make
    /// the reuse unambiguous and the arithmetic checkable.
    #[must_use]
    pub fn root(g: u32) -> NodeId {
        NodeId(FIRST_NODE + (g % 2) * 4096)
    }

    /// The `i`th child of generation `g`.
    #[must_use]
    pub fn child(g: u32, i: usize) -> NodeId {
        NodeId(Self::root(g).raw() + 1 + i as u32)
    }
}

impl Scenario for CreateDestroy {
    fn name(&self) -> &'static str {
        "create"
    }

    fn build(&mut self, _ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        Ok(Vec::new())
    }

    fn frame(&mut self, ctx: &mut Ctx, _frame: u64) -> Result<Vec<ClientMsg>, Error> {
        let old = self.generation;
        let new = old + 1;
        self.generation = new;
        let mut out = Vec::with_capacity(self.count * 3 + 2);
        if old > 0 {
            out.push(
                nitro_wire::msg::DestroyNode {
                    id: Self::root(old),
                }
                .into(),
            );
        }
        out.push(
            CreateNode {
                id: Self::root(new),
                kind: NodeKind::Group,
                parent: WINDOW,
                before: NodeId::NONE,
            }
            .into(),
        );
        out.push(
            SetBounds {
                id: Self::root(new),
                rect: Rect::new(0.0, 0.0, ctx.size.w, ctx.size.h),
            }
            .into(),
        );
        let edge = 24.0;
        for (i, r) in crate::x11perf::grid(self.count, edge, ctx.size.w, ctx.size.h)
            .into_iter()
            .enumerate()
        {
            out.push(
                CreateNode {
                    id: Self::child(new, i),
                    kind: NodeKind::Rect,
                    parent: Self::root(new),
                    before: NodeId::NONE,
                }
                .into(),
            );
            out.push(
                SetBounds {
                    id: Self::child(new, i),
                    rect: r,
                }
                .into(),
            );
            out.push(
                SetFill {
                    id: Self::child(new, i),
                    fill: nitro_wire::msg::Fill::Solid(bench_color(i)),
                }
                .into(),
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nitro_core::Size;

    fn ctx() -> Ctx {
        Ctx {
            size: Size::new(640.0, 480.0),
            scale: 1.0,
            refresh_ns: 16_666_667,
            n: 0,
            size_px: 0,
        }
    }

    /// The claim the whole retained argument rests on: a moving sprite
    /// costs exactly one mutation per frame, whatever the ball is doing.
    #[test]
    fn a_boing_node_frame_is_one_set_bounds() {
        let mut s = BoingNode::new(640, 480, 64);
        s.build(&mut ctx()).unwrap();
        for f in 0..8 {
            let msgs = s.frame(&mut ctx(), f).unwrap();
            assert_eq!(msgs.len(), 1, "frame {f}");
            assert!(matches!(msgs[0], ClientMsg::SetBounds(_)));
        }
    }

    /// The sprite is uploaded once. A `CreateBuffer` in a per-frame batch
    /// would mean the arm is secretly a pixel-push scenario.
    #[test]
    fn the_boing_sprite_is_uploaded_once_and_never_again() {
        let mut s = BoingNode::new(640, 480, 64);
        let build = s.build(&mut ctx()).unwrap();
        assert_eq!(
            build
                .iter()
                .filter(|m| matches!(m, ClientMsg::CreateBuffer(_)))
                .count(),
            1
        );
        for f in 0..8 {
            assert!(
                s.frame(&mut ctx(), f)
                    .unwrap()
                    .iter()
                    .all(|m| !matches!(m, ClientMsg::CreateBuffer(_) | ClientMsg::BufferDamage(_))),
                "frame {f} re-uploaded the sprite"
            );
        }
    }

    /// The sprite's transparency is what stops the ball dragging a black
    /// square; `XR24` would silently lose it.
    #[test]
    fn the_boing_sprite_buffer_has_alpha() {
        let mut s = BoingNode::new(640, 480, 32);
        let build = s.build(&mut ctx()).unwrap();
        let ClientMsg::CreateBuffer(b) = build
            .iter()
            .find(|m| matches!(m, ClientMsg::CreateBuffer(_)))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(b.format, format::AR24);
    }

    #[test]
    fn a_star_frame_is_one_bounds_per_star_and_no_fills() {
        let mut s = StarNodes::new(50, 640, 480);
        s.build(&mut ctx()).unwrap();
        let msgs = s.frame(&mut ctx(), 1).unwrap();
        assert_eq!(msgs.len(), 50);
        assert!(msgs.iter().all(|m| matches!(m, ClientMsg::SetBounds(_))));
    }

    /// The circle is a rounded rect whose radius is half its diameter; a
    /// radius that drifted from that would quietly turn the scenario into
    /// a squares benchmark.
    #[test]
    fn a_ball_is_a_rounded_rect_of_radius_half_its_diameter() {
        let mut s = BallNodes::new(4, 640, 480);
        let build = s.build(&mut ctx()).unwrap();
        let mut d = None;
        for m in &build {
            match m {
                ClientMsg::SetBounds(b) if b.id == BallNodes::node(0) => d = Some(b.rect.w),
                ClientMsg::SetCorners(c) if c.id == BallNodes::node(0) => {
                    let half = d.expect("bounds before corners") / 2.0;
                    assert!((c.radius - half).abs() < 1e-4, "{} != {half}", c.radius);
                }
                _ => {}
            }
        }
        assert!(d.is_some(), "no bounds for ball 0");
    }

    /// A retained scroll is one mutation, whatever the content's height —
    /// which is the whole difference from `CopyArea`.
    #[test]
    fn a_scroll_frame_moves_the_group_and_nothing_else() {
        let mut s = Scroll::new(200, 16.0);
        s.build(&mut ctx()).unwrap();
        for f in 0..8 {
            let msgs = s.frame(&mut ctx(), f).unwrap();
            assert_eq!(msgs.len(), 1, "frame {f}");
        }
    }

    /// The offset wraps rather than running away: at frame 10 000 the
    /// content is still over the viewport.
    #[test]
    fn a_scroll_wraps_instead_of_drifting_into_nothing() {
        let mut s = Scroll::new(100, 16.0);
        s.build(&mut ctx()).unwrap();
        let content = 100.0 * 16.0;
        for f in [0u64, 99, 100, 10_000] {
            let ClientMsg::SetBounds(b) = s.frame(&mut ctx(), f).unwrap().remove(0) else {
                panic!()
            };
            assert!(
                b.rect.y <= 0.0 && b.rect.y > -content,
                "frame {f}: {:?}",
                b.rect
            );
        }
    }

    /// Every generation destroys the one before it, so the server's
    /// `nodes` count is bounded — the leak this scenario exists to catch.
    #[test]
    fn every_create_generation_destroys_its_predecessor() {
        let mut s = CreateDestroy::new(10);
        let first = s.frame(&mut ctx(), 0).unwrap();
        assert!(
            !first.iter().any(|m| matches!(m, ClientMsg::DestroyNode(_))),
            "nothing exists to destroy on the first frame"
        );
        for f in 1..6 {
            let msgs = s.frame(&mut ctx(), f).unwrap();
            assert_eq!(
                msgs.iter()
                    .filter(|m| matches!(m, ClientMsg::DestroyNode(_)))
                    .count(),
                1,
                "frame {f}"
            );
            // Exactly one root plus `count` children are created.
            assert_eq!(
                msgs.iter()
                    .filter(|m| matches!(m, ClientMsg::CreateNode(_)))
                    .count(),
                11,
                "frame {f}"
            );
        }
    }

    /// Two id ranges, alternating: a generation may never reuse the ids
    /// the transaction is destroying in the same breath.
    #[test]
    fn consecutive_generations_use_disjoint_id_ranges() {
        for g in 1..6u32 {
            assert_ne!(CreateDestroy::root(g), CreateDestroy::root(g + 1));
            assert_eq!(CreateDestroy::root(g), CreateDestroy::root(g + 2));
            assert_ne!(CreateDestroy::child(g, 9), CreateDestroy::child(g + 1, 9));
        }
    }

    #[test]
    fn the_scenario_names_are_the_ones_the_report_groups_on() {
        assert_eq!(BoingNode::new(1, 1, 8).name(), "boing-node");
        assert_eq!(StarNodes::new(1, 640, 480).name(), "starfield-nodes");
        assert_eq!(BallNodes::new(1, 640, 480).name(), "balls-nodes");
        assert_eq!(Scroll::new(1, 1.0).name(), "scroll");
        assert_eq!(CreateDestroy::new(1).name(), "create");
    }
}
