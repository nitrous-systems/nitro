//! The claim the whole crate exists to support: work is proportional to what
//! changed, not to the size of the tree.
//!
//! These tests build a large scene and assert on [`UpdateStats::visited_nodes`]
//! — if a future change makes the update pass walk the tree, they fail loudly
//! rather than merely getting slower.

mod common;

use common::{CLIENT, OUT, damage, group, rect, scene, settle, update, window, window_at};
use nitro_core::{Color, IRect, Point, Rect, Size, Transform};
use nitro_scene::{Fill, NodeKey, NodeKind, Scene};

/// Build `groups` groups of `per_group` rects each, laid out on a grid, and
/// return every leaf.
///
/// The grid is **50 columns**, so 1 000 groups of 8 px span 400 x 160 and fit
/// inside the 400 x 300 window `common::window` makes. That matters because a
/// window's content group clips: a grid 100 columns wide would span 800 px,
/// half of it outside the window, and the culling test below would count
/// 5 000 of its 10 000 leaves rather than measuring culling at all.
fn forest(s: &mut Scene, root: NodeKey, groups: usize, per_group: usize) -> Vec<NodeKey> {
    let mut leaves = Vec::with_capacity(groups * per_group);
    for g in 0..groups {
        let gx = (g % 50) as f32 * 8.0;
        let gy = (g / 50) as f32 * 8.0;
        let group = s.create_node(CLIENT, NodeKind::Group, root, None).unwrap();
        s.set_bounds(CLIENT, group, Rect::new(gx, gy, 8.0, 8.0))
            .unwrap();
        for i in 0..per_group {
            let x = (i % 4) as f32 * 2.0;
            let y = (i / 4) as f32 * 2.0;
            let leaf = s.create_node(CLIENT, NodeKind::Rect, group, None).unwrap();
            s.set_bounds(CLIENT, leaf, Rect::new(x, y, 2.0, 2.0))
                .unwrap();
            s.set_fill(CLIENT, leaf, Fill::Solid(Color::WHITE)).unwrap();
            leaves.push(leaf);
        }
    }
    leaves
}

/// 10 000 leaves in 1 000 groups: 11 001 nodes with the window root.
fn big_scene() -> (Scene, NodeKey, Vec<NodeKey>) {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let leaves = forest(&mut s, root, 1000, 10);
    settle(&mut s);
    (s, root, leaves)
}

#[test]
fn mutating_ten_of_ten_thousand_nodes_visits_only_their_paths() {
    let (mut s, _, leaves) = big_scene();
    assert_eq!(s.node_count(), 11_001);

    // Touch ten leaves scattered through the tree.
    let touched: Vec<NodeKey> = leaves.iter().step_by(997).copied().take(10).collect();
    assert_eq!(touched.len(), 10);
    for leaf in &touched {
        s.set_fill(CLIENT, *leaf, Fill::Solid(Color::BLACK))
            .unwrap();
    }

    let (dmg, stats) = update(&mut s);
    // Each touched leaf costs itself plus its group, plus the shared root:
    // 10 * 2 + 1 = 21. Anything near 11 000 means the walk lost its way.
    assert_eq!(stats.visited_nodes, 21);
    assert_eq!(stats.damaged_nodes, 10);
    assert_eq!(stats.dirty_roots, 1);
    assert!(!dmg.is_empty());

    // And the frame after is free again.
    let (dmg, stats) = update(&mut s);
    assert!(dmg.is_empty());
    assert_eq!(stats.visited_nodes, 0);
}

#[test]
fn one_mutation_costs_one_path() {
    let (mut s, _, leaves) = big_scene();
    s.set_visible(CLIENT, leaves[5000], false).unwrap();
    let (_, stats) = update(&mut s);
    // leaf + group + root.
    assert_eq!(stats.visited_nodes, 3);
    assert_eq!(stats.damaged_nodes, 1);
}

#[test]
fn a_group_transform_visits_only_that_group() {
    let (mut s, root, _) = big_scene();
    let group = s.node(root).unwrap().children()[500];

    s.set_transform(CLIENT, group, Transform::translate(1.0, 1.0))
        .unwrap();
    let (dmg, stats) = update(&mut s);
    // The group's own subtree must be revisited, plus the root: 1 + 10 + 1.
    assert_eq!(stats.visited_nodes, 12);
    // Its ten children each moved.
    assert_eq!(stats.damaged_nodes, 10);
    assert!(!dmg.is_empty());
}

#[test]
fn repeated_marks_on_one_path_do_not_multiply_the_work() {
    let (mut s, _, leaves) = big_scene();
    let leaf = leaves[123];
    // Twenty mutations to the same node between two updates.
    for i in 0..20 {
        s.set_bounds(CLIENT, leaf, Rect::new(i as f32, 0.0, 2.0, 2.0))
            .unwrap();
    }
    let (_, stats) = update(&mut s);
    assert_eq!(stats.visited_nodes, 3, "the path is walked once");
    assert_eq!(stats.damaged_nodes, 1);
}

#[test]
fn touching_every_node_does_visit_every_node() {
    // The converse check: the cheapness above is real culling, not a bug that
    // drops work.
    let (mut s, _, leaves) = big_scene();
    for leaf in &leaves {
        s.set_fill(CLIENT, *leaf, Fill::Solid(Color::BLACK))
            .unwrap();
    }
    let (_, stats) = update(&mut s);
    assert_eq!(stats.visited_nodes, 11_001);
    assert_eq!(stats.damaged_nodes, 10_000);
}

#[test]
fn moving_a_window_revisits_its_tree_but_not_its_neighbours() {
    let mut s = scene();
    let (win_a, root_a) = window(&mut s);
    forest(&mut s, root_a, 100, 10);
    let (_, root_b) = window(&mut s);
    forest(&mut s, root_b, 100, 10);
    settle(&mut s);
    assert_eq!(s.node_count(), 2 * 1101);

    s.place_window(win_a, Some(OUT), Point::new(37.0, 11.0))
        .unwrap();
    let (_, stats) = update(&mut s);
    // Exactly the moved window's tree: 1 root + 100 groups + 1000 leaves.
    assert_eq!(stats.visited_nodes, 1101);
}

#[test]
fn an_idle_scene_of_ten_thousand_nodes_is_free_forever() {
    let (mut s, _, _) = big_scene();
    for _ in 0..10 {
        let (dmg, stats) = update(&mut s);
        assert!(dmg.is_empty());
        assert_eq!(stats.visited_nodes, 0);
        assert_eq!(stats.damaged_nodes, 0);
        assert_eq!(stats.dirty_roots, 0);
    }
}

#[test]
fn paint_list_culling_scales_with_the_region_not_the_tree() {
    let (s, _, _) = big_scene();
    let mut all = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 800, 600), &mut all);
    assert_eq!(all.len(), 10_000, "everything fits on the output");

    // A tiny region picks up only the handful of leaves that touch it.
    let mut few = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 8, 8), &mut few);
    assert!(
        (1..=16).contains(&few.len()),
        "expected a handful, got {}",
        few.len()
    );
    // Every item really does intersect the region.
    for item in &few {
        assert!(item.bounds.intersects(&IRect::new(0, 0, 8, 8)));
    }

    // A region off the side of everything is empty.
    let mut none = Vec::new();
    s.paint_list(OUT, &IRect::new(700, 500, 50, 50), &mut none);
    assert!(none.is_empty());
}

#[test]
fn hit_testing_a_large_tree_finds_the_right_leaf() {
    let (s, _, _) = big_scene();
    // Group 0 sits at (0,0); its leaf i=5 is at local (2, 2) within it.
    let hit = s.hit_test(OUT, Point::new(2.5, 2.5)).unwrap();
    let node = s.node(hit.node).unwrap();
    assert_eq!(node.kind(), NodeKind::Rect);
    assert_eq!(node.world_bounds(), IRect::new(2, 2, 2, 2));
    assert!((hit.local.x - 0.5).abs() < 1e-4);

    // Well past the last group.
    assert!(s.hit_test(OUT, Point::new(799.0, 599.0)).is_none());
}

#[test]
fn destroying_a_large_subtree_is_one_damage_rect_and_no_walk() {
    let (mut s, root, _) = big_scene();
    let group = s.node(root).unwrap().children()[0];
    let before = s.node_count();

    s.destroy_node(CLIENT, group).unwrap();
    assert_eq!(s.node_count(), before - 11);
    let (dmg, stats) = update(&mut s);
    // The banked rect, plus the root revisited to refresh its extent.
    assert!(!dmg.is_empty());
    assert!(stats.visited_nodes <= 1, "visited {}", stats.visited_nodes);
}

#[test]
fn a_deep_chain_only_walks_the_chain() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    // A 64-deep chain of groups with a leaf at the bottom...
    let mut parent = root;
    for _ in 0..64 {
        let g = s
            .create_node(CLIENT, NodeKind::Group, parent, None)
            .unwrap();
        s.set_bounds(CLIENT, g, Rect::new(1.0, 1.0, 500.0, 500.0))
            .unwrap();
        parent = g;
    }
    let leaf = rect(&mut s, parent, Rect::new(0.0, 0.0, 10.0, 10.0));
    // ...and a thousand unrelated nodes beside it.
    forest(&mut s, root, 100, 10);
    settle(&mut s);
    assert_eq!(
        s.node(leaf).unwrap().world_bounds(),
        IRect::new(64, 64, 10, 10)
    );

    s.set_fill(CLIENT, leaf, Fill::Solid(Color::BLACK)).unwrap();
    let (dmg, stats) = update(&mut s);
    // Root + 64 groups + the leaf.
    assert_eq!(stats.visited_nodes, 66);
    assert_eq!(dmg.rects(), &[IRect::new(64, 64, 10, 10)]);
}

#[test]
fn a_wide_tree_of_many_windows_stays_proportional() {
    let mut s = scene();
    let mut roots = Vec::new();
    for i in 0..50u16 {
        let win = s.create_window(
            CLIENT,
            "w",
            nitro_core::Size::new(16.0, 16.0),
            nitro_scene::Layer::Normal,
        );
        s.place_window(win, Some(OUT), Point::new(f32::from(i) * 16.0, 0.0))
            .unwrap();
        let root = s.window_info(win).unwrap().root();
        for j in 0..20u16 {
            let leaf = rect(
                &mut s,
                root,
                Rect::new(f32::from(j % 4) * 4.0, f32::from(j / 4) * 4.0, 4.0, 4.0),
            );
            if j == 0 {
                roots.push(leaf);
            }
        }
    }
    settle(&mut s);
    assert_eq!(s.node_count(), 50 * 21);

    // One leaf in one window.
    s.set_fill(CLIENT, roots[25], Fill::Solid(Color::BLACK))
        .unwrap();
    let (_, stats) = update(&mut s);
    assert_eq!(stats.visited_nodes, 2, "that window's root and the leaf");
    assert_eq!(stats.dirty_roots, 1);

    // One leaf in each of three windows: three independent roots.
    for r in [roots[0], roots[10], roots[49]] {
        s.set_fill(CLIENT, r, Fill::Solid(Color::BLACK)).unwrap();
    }
    let (_, stats) = update(&mut s);
    assert_eq!(stats.visited_nodes, 6);
    assert_eq!(stats.dirty_roots, 3);
}

#[test]
fn a_thousand_updates_do_not_leak_damage_or_work() {
    let (mut s, _, leaves) = big_scene();
    // Animate one node for a thousand frames; each frame must cost the same.
    // The offset starts at 1 so every frame is a real move (frame 0 must not
    // land on the node's existing bounds, or the no-op check makes it free
    // and the assertion would be measuring nothing).
    for frame in 0..1000 {
        s.set_bounds(
            CLIENT,
            leaves[0],
            Rect::new((frame % 100 + 1) as f32, 0.0, 2.0, 2.0),
        )
        .unwrap();
        let (dmg, stats) = update(&mut s);
        assert_eq!(stats.visited_nodes, 3, "frame {frame}");
        assert_eq!(stats.damaged_nodes, 1, "frame {frame}");
        assert!(!dmg.is_empty());
        assert!(dmg.rects().len() <= 2, "frame {frame}");
    }
    // The tree is exactly as big as it was.
    assert_eq!(s.node_count(), 11_001);
    assert!(damage(&mut s).is_empty());
}

/// The benchmark's `scroll` scene: a fixed clipper at the viewport with a
/// taller content column inside it, `rows` rows of 16 px.
fn scroll_scene(rows: usize) -> (Scene, NodeKey) {
    let mut s = scene();
    // 640x480 window at the origin, as `nitro-bench scroll` builds it.
    let (_, root) = window_at(&mut s, Point::ZERO, Size::new(640.0, 480.0));
    let clipper = group(&mut s, root, Rect::new(0.0, 0.0, 640.0, 480.0));
    s.set_clip(CLIENT, clipper, true).unwrap();
    let content = group(
        &mut s,
        clipper,
        Rect::new(0.0, 0.0, 640.0, rows as f32 * 16.0),
    );
    for i in 0..rows {
        rect(
            &mut s,
            content,
            Rect::new(0.0, i as f32 * 16.0, 640.0, 15.0),
        );
    }
    settle(&mut s);
    (s, content)
}

/// Scroll the content group up by `frame` rows.
fn scroll_to(s: &mut Scene, content: NodeKey, rows: usize, frame: usize) {
    let offset = -((frame % rows) as f32 * 16.0);
    s.set_bounds(
        CLIENT,
        content,
        Rect::new(0.0, offset, 640.0, rows as f32 * 16.0),
    )
    .unwrap();
}

/// **A characterisation test, not an aspiration.** It pins the cost that is
/// there today so that a change making it *worse* fails loudly.
///
/// Translating a group sets `Dirty::TRANSFORM`, so the walk descends into
/// every child to recompute world state — even the ~470 of every 500 rows
/// that are entirely outside the clip rectangle both before and after. The
/// walk is therefore proportional to **content** rows, not visible ones: at
/// 50 000 rows it is `visited_nodes` = 50 003 and ~2.1 ms of pure scene walk
/// per frame (measured; `docs/bench.md` §7.5).
///
/// **This is a known cliff and it is deliberately unfixed.** Culling the
/// descent was prototyped and measured during #3745 and not taken, for two
/// reasons worth recording so the next reader does not re-derive them:
///
/// 1. **No in-tree client pays it.** Every long list in this tree
///    virtualises: `nitro-term` materialises screen rows only (scrollback
///    lives in the model, never in the scene), `nitro_ui::List` materialises
///    `visible + 2` "whether the model holds a hundred rows or a hundred
///    thousand", and the one unvirtualised tall-child shape,
///    `nitro_ui::Scroll`, has a single in-tree user bounded at 20 results.
///    At the benchmark's own n=500 culling saved ~9 µs against 1 616 µs of
///    server CPU — well under 1 % of the frame.
/// 2. **Culling is not free and not local.** The sound version leaves a
///    skipped node's `world_transform` stale, which is safe for `paint_list`
///    and `hit_test` (both key off `subtree_bounds`, which stays `EMPTY`)
///    but *not* for `Scene::device_bounds`, which recomputes from that
///    cache and was measured returning a stale rectangle for a culled node.
///    A wider contract change than the win justifies while (1) holds.
///
/// If an unvirtualised tall column ever appears in a client, this test is
/// where the decision gets revisited — and `visited_nodes` is the right
/// probe for it, because unlike `damage_px_mean` it counts work done rather
/// than pixels claimed, so it cannot be satisfied by a corrupt screen
/// (`docs/bench.md` §11 D).
#[test]
fn a_tall_clipped_column_costs_a_walk_per_content_row() {
    for rows in [500usize, 2000] {
        let (mut s, content) = scroll_scene(rows);
        scroll_to(&mut s, content, rows, 1);
        let (_, stats) = update(&mut s);
        // Window root + clipper + content group + every row.
        assert_eq!(
            stats.visited_nodes,
            rows + 3,
            "the walk is proportional to content rows, not visible ones \
             ({rows} rows)"
        );
    }
}

/// The other half of the characterisation: the cost above is specific to
/// dragging a whole subtree along. Dirtying one row's fill still costs one
/// path however tall the column — so the cliff is about `descend_all`, not
/// about the tree being big.
#[test]
fn dirtying_one_row_of_a_tall_column_still_costs_one_path() {
    let rows = 2000;
    let (mut s, content) = scroll_scene(rows);
    let first = s.node(content).unwrap().children()[0];
    s.set_fill(CLIENT, first, Fill::Solid(Color::BLACK))
        .unwrap();
    let (dmg, stats) = update(&mut s);
    // Root + clipper + content + the one row.
    assert_eq!(stats.visited_nodes, 4);
    assert_eq!(stats.damaged_nodes, 1);
    assert!(!dmg.is_empty());
}

/// A scroll damages the viewport because a scroll *changes* the viewport —
/// the test that refutes the fix issue #570 recommended.
///
/// The proposal was to damage only the two thin bands at the leading and
/// trailing edges of a pure translation. That is true of a framebuffer
/// already blitted and false of a scene graph that has not been: shifting a
/// column of differently-coloured rows past a fixed viewport gives
/// essentially every pixel its neighbour's colour, so the damage really is
/// viewport-sized (450 of 480 rows genuinely change; the 30 that do not are
/// the 1-px inter-row gaps). Asserting it here means a future
/// "optimisation" that shrinks the damage to the exposed 640x16 = 10 240 px
/// band fails *in this crate*, instead of shipping and being discovered as
/// a frozen screen.
#[test]
fn scrolling_damages_the_viewport_not_the_exposed_band() {
    let rows = 500;
    let (mut s, content) = scroll_scene(rows);
    scroll_to(&mut s, content, rows, 1);
    let (dmg, stats) = update(&mut s);
    assert!(stats.damaged_nodes > 0);

    let area: i64 = dmg
        .rects()
        .iter()
        .map(|r| i64::from(r.w) * i64::from(r.h))
        .sum();
    // The viewport is 640x480 = 307 200 px, and the damage is essentially
    // all of it. The exposed band alone would be 10 240.
    assert!(
        area > 250_000,
        "a scroll must damage ~the viewport, got {area} px; \
         see docs/bench.md §7.5 and §11 D"
    );
    assert!(area <= 307_200, "and never more than the viewport: {area}");
}
