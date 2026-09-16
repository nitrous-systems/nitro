//! The x11perf ports: the operation micro-benchmarks, transposed to a
//! retained scene graph.
//!
//! # What x11perf was
//!
//! `x11perf` (Joel `McCormack` and Keith Packard, DEC/MIT, 1988) is the X11
//! server benchmark: it issues one graphics operation in a tight loop and
//! reports operations per second. `x11perf -rect100` draws 100×100
//! rectangles; `-ftext` draws text; `-putimage500` pushes 500×500 pixel
//! blocks; `-scroll500` copies a 500×500 area up by one line;
//! `-move` moves windows. Thirty-eight years later those names are still
//! how people talk about 2D performance, which is why they are the names
//! used here.
//!
//! # Why the port cannot be literal, and what replaces it
//!
//! x11perf's loop works because X11 ops are *immediate*: `XFillRectangle`
//! draws now, so drawing it a million times is a meaningful thing to time.
//! Nitro has no such op. A client mutates a retained tree and the server
//! paints once per vblank. Send a million `SetFill`s and the server
//! coalesces them into one frame — correctly, by design, and the
//! resulting "ops per second" would be a measurement of the socket.
//!
//! The honest transposition is **mutations per frame sustained at the
//! output's refresh rate**. Each scenario has a sweep parameter `N` and
//! the question is: how large can N get before the server misses a vblank
//! or `paint_us_max` crosses the frame period? That preserves what
//! x11perf was actually asking — *how much 2D work per unit time* — while
//! being a question this architecture can answer.
//!
//! # The ops x11perf has and nitro does not
//!
//! `-line`, `-circle`, `-ellipse`, `-poly`: **there is no line, arc or
//! polygon op on the nitro wire at all**. The primitive set is
//! `Rect`/`Text`/`Image`/`Icon` plus groups, and that is the whole of it.
//! Rather than fake a line with a thin rotated rect and report a number
//! for it, this crate does not implement those scenarios and
//! `docs/bench.md` says why. It is a finding about the protocol's scope,
//! not a gap in the benchmark: a chart widget or a drawing app would need
//! either a path primitive or a client-side buffer, and knowing which of
//! those is missing is worth more than a synthetic ops/s figure.
//!
//! `-move` ("flying windows": N decorated windows moved per frame) is the
//! second one, and it is missing for a sharper reason: **no client
//! message on this wire carries a window position.** `CreateWindow` has
//! `size`, `layer`, `flags` and `title` and no origin; `position` occurs
//! exactly once in the whole protocol, on the server's `Configure`.
//! Placement belongs to the window manager and a client is *told* where
//! it ended up, so a client-driven window move is not expressible at all
//! — not even privileged, since the `SHELL` block has focus, layer,
//! anchors and exclusive zones but no move either. `rects-move` is not a
//! substitute: it moves undecorated nodes inside one window and touches
//! no window-manager path. `docs/bench.md` §3 argues both gaps together.

use nitro_core::{Color, Rect};
use nitro_wire::msg::{ClientMsg, CreateNode, SetBounds, SetFill, SetText};
use nitro_wire::types::{Align, NodeId, NodeKind};

use crate::effects::BENCH_COLORS;
use crate::harness::{Ctx, Error, FIRST_NODE, Scenario, WINDOW};

/// The colours the rect scenarios cycle through, as wire `Color`s.
///
/// Derived from the effects module's BGRA table rather than written again,
/// so the palette a screenshot shows is the same one the pixel scenarios
/// use and there is exactly one place either can be changed.
///
/// The scenarios need a palette rather than a role for the reason the
/// lint's header allows a rasteriser one: these are not chrome. A rect
/// scenario cycles colours *so that consecutive frames differ*, because a
/// constant colour lets the server notice a node is unchanged and skip it,
/// and a benchmark that measured the skip would be measuring nothing. A
/// theme switch has no business changing a test pattern.
#[must_use]
pub fn bench_color(i: usize) -> Color {
    let bgra = BENCH_COLORS[i % BENCH_COLORS.len()];
    // lint-colors: allow — this unpacks somebody else's BGRA word into
    // its three channels. It chooses no colour: the value comes from
    // `BENCH_COLORS`, which is the one table, and a role cannot be
    // consulted for a benchmark pattern that must not follow the theme.
    Color::rgb(
        ((bgra >> 16) & 0xff) as u8,
        ((bgra >> 8) & 0xff) as u8,
        (bgra & 0xff) as u8,
    )
}

/// Lay `count` rects out in a grid that fills `size`, each `edge` on a
/// side, and hand back their rectangles.
///
/// A grid rather than random placement because damage is the subject: a
/// grid of N rects covers a predictable area, so `damage_px_mean` in the
/// server's stats is a number the reader can check against N × edge²
/// rather than a number they have to trust.
#[must_use]
pub fn grid(count: usize, edge: f32, w: f32, h: f32) -> Vec<Rect> {
    let cols = ((w / edge).floor() as usize).max(1);
    (0..count)
        .map(|i| {
            let col = i % cols;
            let row = i / cols;
            let x = col as f32 * edge;
            let y = (row as f32 * edge) % h.max(edge);
            Rect::new(x, y, edge - 1.0, edge - 1.0)
        })
        .collect()
}

/// `x11perf -rect10 / -rect100 / -rect500`: N rectangles, recoloured or
/// moved every frame.
///
/// The x11perf original fills the same rectangle over and over. The
/// retained equivalent is N nodes that *change* every frame, because a
/// node that does not change costs the server nothing — which is the
/// property under test and the reason the sweep is interesting at all.
///
/// Two modes, and the difference between them is a finding rather than a
/// detail: recolouring dirties each node's own bounds, moving dirties the
/// union of the old and the new. A benchmark that only did one of them
/// would miss the more expensive half.
pub struct Rects {
    /// How many nodes; the sweep point.
    pub count: usize,
    /// Edge length of each rect in logical pixels — x11perf's `10`, `100`
    /// and `500`.
    pub edge: f32,
    /// Whether the mutation is a move (`true`) or a recolour (`false`).
    pub moving: bool,
    /// The rects as laid out at build time, kept so a move can be
    /// expressed as an offset from a known origin rather than accumulated
    /// (which would drift the scene off-screen over a long run and
    /// silently turn a damage benchmark into a clipping benchmark).
    origin: Vec<Rect>,
}

impl Rects {
    /// A recolouring sweep of `count` rects of `edge` pixels.
    #[must_use]
    pub fn recolour(count: usize, edge: f32) -> Self {
        Self {
            count,
            edge,
            moving: false,
            origin: Vec::new(),
        }
    }

    /// A moving sweep; see the type docs for why both exist.
    #[must_use]
    pub fn moving(count: usize, edge: f32) -> Self {
        Self {
            count,
            edge,
            moving: true,
            origin: Vec::new(),
        }
    }

    /// Node id of the `i`th rect.
    #[must_use]
    pub fn node(i: usize) -> NodeId {
        NodeId(FIRST_NODE + i as u32)
    }
}

impl Scenario for Rects {
    fn name(&self) -> &'static str {
        if self.moving { "rects-move" } else { "rects" }
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        self.origin = grid(self.count, self.edge, ctx.size.w, ctx.size.h);
        let mut out = Vec::with_capacity(self.count * 3);
        for (i, r) in self.origin.iter().enumerate() {
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
                    rect: *r,
                }
                .into(),
            );
            out.push(
                SetFill {
                    id: Self::node(i),
                    fill: nitro_wire::msg::Fill::Solid(bench_color(i)),
                }
                .into(),
            );
        }
        Ok(out)
    }

    fn frame(&mut self, _ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        let mut out = Vec::with_capacity(self.count);
        for (i, r) in self.origin.iter().enumerate() {
            if self.moving {
                // A four-phase orbit round a 2×2 square:
                // (2,0) → (2,2) → (0,2) → (0,0) → …
                //
                // **Consecutive phases always differ on at least one
                // axis**, and that is the whole property. The first
                // version used an eight-phase cycle whose position
                // repeated on consecutive frames, so half of the `SetBounds`
                // it sent were `node.bounds == bounds` and the server's
                // `set_bounds` early-returned: the arm mutated on
                // alternate frames while the recolour arm mutated on every
                // one, and the two were not comparable. The doc then drew
                // a finding from the gap. Caught in review, with a
                // regression test below.
                //
                // Two pixels of travel, which is enough that the old and
                // new bounds only partly overlap — the case that makes a
                // damage union bigger than either rect, which is what
                // separates this arm from the recolouring one.
                let phase = (frame + i as u64) % 4;
                let dx = f32::from(u8::from(phase < 2)) * 2.0;
                let dy = f32::from(u8::from(!phase.is_multiple_of(3))) * 2.0;
                out.push(
                    SetBounds {
                        id: Self::node(i),
                        rect: Rect::new(r.x + dx, r.y + dy, r.w, r.h),
                    }
                    .into(),
                );
            } else {
                out.push(
                    SetFill {
                        id: Self::node(i),
                        fill: nitro_wire::msg::Fill::Solid(bench_color(
                            i + usize::try_from(frame).unwrap_or(0),
                        )),
                    }
                    .into(),
                );
            }
        }
        Ok(out)
    }
}

/// `x11perf -ftext / -f24text`: N text nodes **relabelled** every frame.
///
/// This is the shaping benchmark. Every `SetText` with a changed string
/// makes the server shape a run: measure it, select faces, map codepoints
/// to glyphs, rasterize whatever is not in the atlas. `text_layouts` in
/// the server's stats counts exactly that, so the report can divide it by
/// frames and check the client got the cost it asked for.
///
/// The strings are a rolling counter rather than random text on purpose:
/// a benchmark whose glyph set changes run to run cannot be compared with
/// itself, and digits are the case a clock, a CPU meter and every
/// progress readout in a real desktop actually hit.
pub struct Text {
    /// How many text nodes.
    pub count: usize,
    /// Font size in logical pixels — 12 for `-ftext`, 24 for `-f24text`.
    pub size_px: f32,
    /// Whether the node moves instead of changing its string; see
    /// [`Text::static_text`].
    pub static_text: bool,
}

impl Text {
    /// Node id of the `i`th label.
    #[must_use]
    pub fn node(i: usize) -> NodeId {
        NodeId(FIRST_NODE + i as u32)
    }

    /// Relabelled every frame: the shaping cost.
    #[must_use]
    pub fn relabelled(count: usize, size_px: f32) -> Self {
        Self {
            count,
            size_px,
            static_text: false,
        }
    }

    /// **Moved** every frame, never relabelled — the retained win, and the
    /// scenario x11perf could not have had.
    ///
    /// X11 has no retained text: a moved string is a redraw, so it costs
    /// the same as a new one. Here the string is unchanged, so the server
    /// re-uses its layout and its glyph tiles and only composites. The
    /// number that proves it is `glyph_renders`, which must not move at
    /// all across the run — and `text_layouts`, which must not either.
    /// Those two pinned counters are the whole point of this scenario;
    /// the microseconds are corroboration.
    #[must_use]
    pub fn moved(count: usize, size_px: f32) -> Self {
        Self {
            count,
            size_px,
            static_text: true,
        }
    }

    /// The string the `i`th label carries at `frame`.
    ///
    /// Padded to a fixed width so the shaped run's *length* is constant
    /// and the sweep varies only the content: a counter that grows from
    /// `9` to `10` would otherwise make later frames shape one more glyph
    /// than earlier ones, and a benchmark that gets slower as it runs for
    /// reasons of its own is worse than no benchmark.
    #[must_use]
    pub fn label(i: usize, frame: u64) -> String {
        format!("{:04}", (frame * 7 + i as u64) % 10_000)
    }
}

impl Scenario for Text {
    fn name(&self) -> &'static str {
        if self.static_text {
            "text-static"
        } else {
            "text"
        }
    }

    fn build(&mut self, ctx: &mut Ctx) -> Result<Vec<ClientMsg>, Error> {
        let line_h = self.size_px * 1.4;
        let col_w = self.size_px * 4.0;
        let cols = ((ctx.size.w / col_w).floor() as usize).max(1);
        let rows = ((ctx.size.h / line_h).floor() as usize).max(1);
        let mut out = Vec::with_capacity(self.count * 3);
        for i in 0..self.count {
            let x = (i % cols) as f32 * col_w;
            let y = ((i / cols) % rows) as f32 * line_h;
            out.push(
                CreateNode {
                    id: Self::node(i),
                    kind: NodeKind::Text,
                    parent: WINDOW,
                    before: NodeId::NONE,
                }
                .into(),
            );
            out.push(
                SetBounds {
                    id: Self::node(i),
                    rect: Rect::new(x, y, col_w, line_h),
                }
                .into(),
            );
            out.push(self.set_text(i, 0));
        }
        Ok(out)
    }

    fn frame(&mut self, ctx: &mut Ctx, frame: u64) -> Result<Vec<ClientMsg>, Error> {
        let line_h = self.size_px * 1.4;
        let col_w = self.size_px * 4.0;
        let cols = ((ctx.size.w / col_w).floor() as usize).max(1);
        let rows = ((ctx.size.h / line_h).floor() as usize).max(1);
        Ok((0..self.count)
            .map(|i| {
                if self.static_text {
                    // Four pixels of horizontal travel, wrapping: the
                    // glyphs are identical, only the node's bounds move.
                    let x = (i % cols) as f32 * col_w
                        + ((frame * 4) % u64::from(col_w.max(1.0) as u32)) as f32;
                    let y = ((i / cols) % rows) as f32 * line_h;
                    SetBounds {
                        id: Self::node(i),
                        rect: Rect::new(x, y, col_w, line_h),
                    }
                    .into()
                } else {
                    self.set_text(i, frame)
                }
            })
            .collect())
    }
}

impl Text {
    /// The `SetText` for label `i` at `frame`.
    fn set_text(&self, i: usize, frame: u64) -> ClientMsg {
        SetText {
            node: Self::node(i),
            size_px: self.size_px,
            weight: 400,
            italic: false,
            max_width: 0.0,
            wrap: false,
            align: Align::Left,
            color: bench_color(i),
            family: "sans".to_owned(),
            text: Self::label(i, frame),
        }
        .into()
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

    #[test]
    fn a_grid_stays_inside_its_box() {
        let rs = grid(200, 32.0, 640.0, 480.0);
        assert_eq!(rs.len(), 200);
        for r in &rs {
            assert!(r.x >= 0.0 && r.x < 640.0, "{r:?}");
            assert!(r.y >= 0.0 && r.y < 480.0, "{r:?}");
        }
    }

    /// One rect wider than the window still lands at the origin rather
    /// than dividing by a zero column count.
    #[test]
    fn a_rect_bigger_than_the_window_does_not_divide_by_zero() {
        let rs = grid(3, 2000.0, 640.0, 480.0);
        assert_eq!(rs.len(), 3);
        assert!(rs[0].x.abs() < f32::EPSILON, "{}", rs[0].x);
    }

    #[test]
    fn a_rect_build_creates_bounds_and_a_fill_for_every_node() {
        let mut s = Rects::recolour(5, 32.0);
        let msgs = s.build(&mut ctx()).unwrap();
        assert_eq!(msgs.len(), 15);
        let creates = msgs
            .iter()
            .filter(|m| matches!(m, ClientMsg::CreateNode(_)))
            .count();
        assert_eq!(creates, 5);
    }

    /// The whole point of the recolour arm: one mutation per node per
    /// frame, and it is a fill, not a move.
    #[test]
    fn a_recolour_frame_is_one_fill_per_node() {
        let mut s = Rects::recolour(5, 32.0);
        s.build(&mut ctx()).unwrap();
        let msgs = s.frame(&mut ctx(), 3).unwrap();
        assert_eq!(msgs.len(), 5);
        assert!(msgs.iter().all(|m| matches!(m, ClientMsg::SetFill(_))));
    }

    #[test]
    fn a_moving_frame_is_one_bounds_per_node() {
        let mut s = Rects::moving(5, 32.0);
        s.build(&mut ctx()).unwrap();
        let msgs = s.frame(&mut ctx(), 3).unwrap();
        assert_eq!(msgs.len(), 5);
        assert!(msgs.iter().all(|m| matches!(m, ClientMsg::SetBounds(_))));
    }

    /// The defect review caught: a "moving" arm whose position repeats on
    /// consecutive frames is not moving on those frames.
    ///
    /// `Scene::set_bounds` early-returns when the bounds are unchanged, so
    /// a repeated position makes the `SetBounds` a server-side no-op. The
    /// first version cycled through eight phases that collapsed to four
    /// positions *in pairs* — (2,2),(2,2),(2,0),(2,0),(0,2),(0,2),(0,0),(0,0)
    /// — so the move arm did half the work of the recolour arm it was
    /// being compared against, and `docs/bench.md` drew a finding from the
    /// difference. The comparison is only like-for-like if **every frame
    /// actually moves every node.**
    #[test]
    fn every_moving_frame_actually_moves_every_rect() {
        let mut s = Rects::moving(6, 32.0);
        s.build(&mut ctx()).unwrap();
        let at = |s: &mut Rects, f: u64| -> Vec<(f32, f32)> {
            s.frame(&mut ctx(), f)
                .unwrap()
                .into_iter()
                .map(|m| {
                    let ClientMsg::SetBounds(b) = m else {
                        panic!("the moving arm must send bounds")
                    };
                    (b.rect.x, b.rect.y)
                })
                .collect()
        };
        let mut previous = at(&mut s, 0);
        for f in 1..24u64 {
            let current = at(&mut s, f);
            for (i, (was, now)) in previous.iter().zip(current.iter()).enumerate() {
                assert_ne!(
                    was, now,
                    "frame {f}: rect {i} did not move, so its SetBounds is a no-op \
                     and this arm is not comparable with the recolour arm"
                );
            }
            previous = current;
        }
    }

    /// And the recolour arm has to satisfy the same property, or the
    /// comparison tilts the other way: a repeated colour is an equally
    /// silent no-op.
    #[test]
    fn every_recolour_frame_actually_recolours_every_rect() {
        let mut s = Rects::recolour(6, 32.0);
        s.build(&mut ctx()).unwrap();
        let at = |s: &mut Rects, f: u64| -> Vec<nitro_wire::msg::Fill> {
            s.frame(&mut ctx(), f)
                .unwrap()
                .into_iter()
                .map(|m| {
                    let ClientMsg::SetFill(f) = m else {
                        panic!("the recolour arm must send fills")
                    };
                    f.fill
                })
                .collect()
        };
        let mut previous = at(&mut s, 0);
        for f in 1..24u64 {
            let current = at(&mut s, f);
            for (i, (was, now)) in previous.iter().zip(current.iter()).enumerate() {
                assert_ne!(was, now, "frame {f}: rect {i} kept its colour");
            }
            previous = current;
        }
    }

    /// A move expressed as an offset from a stored origin cannot drift:
    /// after a thousand frames the rects are still on screen, so the
    /// scenario is still measuring damage and not clipping.
    #[test]
    fn moving_rects_do_not_drift_off_the_window() {
        let mut s = Rects::moving(20, 32.0);
        s.build(&mut ctx()).unwrap();
        for f in [0u64, 1, 7, 999, 100_000] {
            for m in s.frame(&mut ctx(), f).unwrap() {
                let ClientMsg::SetBounds(b) = m else {
                    panic!("expected bounds")
                };
                assert!(
                    b.rect.x >= 0.0 && b.rect.x < 660.0,
                    "frame {f}: {:?}",
                    b.rect
                );
                assert!(
                    b.rect.y >= 0.0 && b.rect.y < 500.0,
                    "frame {f}: {:?}",
                    b.rect
                );
            }
        }
    }

    /// A label that grew a digit would shape one more glyph on later
    /// frames, so the run would get slower for a reason of its own.
    #[test]
    fn every_label_is_the_same_width() {
        for f in [0u64, 1, 9, 1234, 99_999] {
            for i in [0usize, 1, 500] {
                assert_eq!(Text::label(i, f).len(), 4, "frame {f} label {i}");
            }
        }
    }

    #[test]
    fn a_relabelled_frame_is_one_set_text_per_node() {
        let mut s = Text::relabelled(4, 12.0);
        s.build(&mut ctx()).unwrap();
        let msgs = s.frame(&mut ctx(), 2).unwrap();
        assert_eq!(msgs.len(), 4);
        assert!(msgs.iter().all(|m| matches!(m, ClientMsg::SetText(_))));
    }

    /// The retained arm sends no `SetText` at all. If it ever did, the
    /// scenario would be measuring shaping again and its `glyph_renders`
    /// claim would be false.
    #[test]
    fn the_static_text_arm_never_reshapes() {
        let mut s = Text::moved(4, 12.0);
        s.build(&mut ctx()).unwrap();
        for f in 0..8 {
            let msgs = s.frame(&mut ctx(), f).unwrap();
            assert!(
                msgs.iter().all(|m| matches!(m, ClientMsg::SetBounds(_))),
                "frame {f} sent a SetText"
            );
        }
    }

    #[test]
    fn the_two_text_arms_have_different_names() {
        assert_eq!(Text::relabelled(1, 12.0).name(), "text");
        assert_eq!(Text::moved(1, 12.0).name(), "text-static");
        assert_eq!(Rects::recolour(1, 10.0).name(), "rects");
        assert_eq!(Rects::moving(1, 10.0).name(), "rects-move");
    }

    #[test]
    fn node_ids_start_above_the_harnesss_own() {
        assert!(Rects::node(0).raw() >= FIRST_NODE);
        assert_ne!(Rects::node(0), WINDOW);
    }
}
