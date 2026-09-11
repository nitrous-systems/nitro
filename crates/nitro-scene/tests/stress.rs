//! The claim the whole crate exists to support: work is proportional to what
//! changed, not to the size of the tree.
//!
//! These tests build a large scene and assert on [`UpdateStats::visited_nodes`]
//! — if a future change makes the update pass walk the tree, they fail loudly
//! rather than merely getting slower.

mod common;

use common::{CLIENT, OUT, damage, rect, scene, settle, update, window};
use nitro_core::{Color, IRect, Point, Rect, Transform};
use nitro_scene::{Fill, NodeKey, NodeKind, Scene};

/// Build `groups` groups of `per_group` rects each, laid out on a grid, and
/// return every leaf.
fn forest(s: &mut Scene, root: NodeKey, groups: usize, per_group: usize) -> Vec<NodeKey> {
    let mut leaves = Vec::with_capacity(groups * per_group);
    for g in 0..groups {
        let gx = (g % 100) as f32 * 8.0;
        let gy = (g / 100) as f32 * 8.0;
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
