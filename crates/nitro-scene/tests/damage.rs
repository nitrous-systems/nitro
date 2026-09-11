//! Damage exactness: the rule is *old ∪ new*, narrowed by the clips in force,
//! and a clean scene costs nothing.

mod common;

use common::{
    CLIENT, OUT, OUT2, damage, damage_bounds, group, rect, rect_colored, scene, settle, update,
    window, window_at,
};
use nitro_core::{Color, Damage, IRect, Point, Rect, Size, Transform};
use nitro_scene::{Border, ClientId, DamageSink, Fill, Layer, NodeKind, Scene};

#[test]
fn a_new_node_damages_exactly_its_bounds() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    settle(&mut s);

    rect(&mut s, root, Rect::new(20.0, 30.0, 40.0, 50.0));
    assert_eq!(damage_bounds(&mut s), IRect::new(20, 30, 40, 50));
}

#[test]
fn moving_a_rect_damages_exactly_old_union_new() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    // Far enough apart that `Damage` keeps them as two rects.
    s.set_bounds(CLIENT, r, Rect::new(200.0, 200.0, 20.0, 20.0))
        .unwrap();
    let d = damage(&mut s);
    let mut expected = Damage::new();
    expected.add(IRect::new(10, 10, 20, 20));
    expected.add(IRect::new(200, 200, 20, 20));
    assert_eq!(d, expected);
    assert_eq!(d.bounds(), IRect::new(10, 10, 210, 210));
}

#[test]
fn a_small_move_merges_into_one_rect_covering_both() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    s.set_bounds(CLIENT, r, Rect::new(15.0, 10.0, 20.0, 20.0))
        .unwrap();
    let d = damage(&mut s);
    // Whatever the merge policy, the union must be covered exactly.
    assert_eq!(d.bounds(), IRect::new(10, 10, 25, 20));
    assert!(d.intersects(&IRect::new(10, 10, 1, 1)), "old edge damaged");
    assert!(d.intersects(&IRect::new(34, 29, 1, 1)), "new edge damaged");
}

#[test]
fn changing_the_fill_damages_exactly_the_bounds() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect_colored(
        &mut s,
        root,
        Rect::new(10.0, 10.0, 20.0, 20.0),
        Color::WHITE,
    );
    settle(&mut s);

    s.set_fill(CLIENT, r, Fill::Solid(Color::BLACK)).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);

    settle(&mut s);
    s.set_corner_radius(CLIENT, r, 4.0).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);

    settle(&mut s);
    s.set_border(CLIENT, r, Some(Border::new(2.0, Color::BLACK)))
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);
}

#[test]
fn hiding_and_showing_damage_exactly_the_bounds() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    s.set_visible(CLIENT, r, false).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);
    assert!(!s.node(r).unwrap().painted());
    assert!(s.node(r).unwrap().world_bounds().is_empty());

    s.set_visible(CLIENT, r, true).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);
    assert!(s.node(r).unwrap().painted());
}

#[test]
fn opacity_zero_is_as_good_as_hidden() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    s.set_opacity(CLIENT, r, 0.0).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);
    assert!(!s.node(r).unwrap().painted());

    // A partial opacity change still repaints the same pixels.
    s.set_opacity(CLIENT, r, 0.5).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 20, 20)]);
    assert!((s.node(r).unwrap().world_opacity() - 0.5).abs() < 1e-6);
}

#[test]
fn a_group_transform_damages_the_union_of_its_children_old_and_new() {
    let mut scn = scene();
    let (_, root) = window(&mut scn);
    let grp = group(&mut scn, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    let left = rect(&mut scn, grp, Rect::new(0.0, 0.0, 20.0, 20.0));
    let right = rect(&mut scn, grp, Rect::new(100.0, 0.0, 20.0, 20.0));
    settle(&mut scn);
    assert_eq!(
        scn.node(left).unwrap().world_bounds(),
        IRect::new(0, 0, 20, 20)
    );
    assert_eq!(
        scn.node(right).unwrap().world_bounds(),
        IRect::new(100, 0, 20, 20)
    );

    scn.set_transform(CLIENT, grp, Transform::translate(50.0, 0.0))
        .unwrap();
    let dmg = damage(&mut scn);
    // Old: [0,20) and [100,120). New: [50,70) and [150,170).
    assert_eq!(dmg.bounds(), IRect::from_edges(0, 0, 170, 20));
    for probe in [0, 19, 50, 69, 100, 119, 150, 169] {
        assert!(
            dmg.intersects(&IRect::new(probe, 0, 1, 1)),
            "x={probe} should be damaged"
        );
    }
    // The gap between the two children was never covered by either.
    assert!(!dmg.intersects(&IRect::new(80, 0, 1, 1)));
    assert_eq!(
        scn.node(left).unwrap().world_bounds(),
        IRect::new(50, 0, 20, 20)
    );
    assert_eq!(
        scn.node(right).unwrap().world_bounds(),
        IRect::new(150, 0, 20, 20)
    );
}

#[test]
fn a_clean_update_visits_nothing_and_damages_nothing() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    for i in 0..20 {
        rect(&mut s, root, Rect::new(i as f32 * 10.0, 0.0, 8.0, 8.0));
    }
    let (first, stats) = update(&mut s);
    assert!(!first.is_empty());
    assert!(stats.visited_nodes >= 21);

    let (second, stats) = update(&mut s);
    assert!(second.is_empty(), "idle frame must be free");
    assert_eq!(stats.visited_nodes, 0);
    assert_eq!(stats.damaged_nodes, 0);
    assert_eq!(stats.dirty_roots, 0);

    // And again, for good measure.
    let (third, stats) = update(&mut s);
    assert!(third.is_empty());
    assert_eq!(stats.visited_nodes, 0);
}

#[test]
fn setting_a_property_to_its_current_value_dirties_nothing() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    s.set_bounds(CLIENT, r, Rect::new(10.0, 10.0, 20.0, 20.0))
        .unwrap();
    s.set_fill(CLIENT, r, Fill::Solid(Color::WHITE)).unwrap();
    s.set_visible(CLIENT, r, true).unwrap();
    s.set_opacity(CLIENT, r, 1.0).unwrap();
    s.set_corner_radius(CLIENT, r, 0.0).unwrap();

    let (d, stats) = update(&mut s);
    assert!(d.is_empty());
    assert_eq!(stats.visited_nodes, 0);
}

#[test]
fn a_clip_group_bounds_the_damage_of_its_children() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(50.0, 50.0, 100.0, 100.0));
    s.set_clip(CLIENT, g, true).unwrap();
    // A child that sticks far out of the group in every direction.
    let r = rect(&mut s, g, Rect::new(-500.0, -500.0, 2000.0, 2000.0));
    settle(&mut s);

    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(50, 50, 100, 100),
        "the child's footprint is the clip"
    );

    s.set_fill(CLIENT, r, Fill::Solid(Color::BLACK)).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(50, 50, 100, 100)]);

    // Moving the child within the clip still cannot damage outside it.
    settle(&mut s);
    s.set_bounds(CLIENT, r, Rect::new(-400.0, -400.0, 2000.0, 2000.0))
        .unwrap();
    let d = damage(&mut s);
    assert!(IRect::new(50, 50, 100, 100).contains_rect(&d.bounds()) || d.is_empty());
}

#[test]
fn nested_clip_groups_intersect() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let outer = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    s.set_clip(CLIENT, outer, true).unwrap();
    // The inner group is offset, so the effective clip is the intersection.
    let inner = group(&mut s, outer, Rect::new(50.0, 50.0, 100.0, 100.0));
    s.set_clip(CLIENT, inner, true).unwrap();
    let r = rect(&mut s, inner, Rect::new(-200.0, -200.0, 1000.0, 1000.0));
    settle(&mut s);

    // outer = [0,100), inner = [50,150) => [50,100).
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(50, 50, 50, 50)
    );

    s.set_fill(CLIENT, r, Fill::Solid(Color::BLACK)).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(50, 50, 50, 50)]);
}

#[test]
fn turning_clipping_on_damages_what_is_no_longer_covered() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let r = rect(&mut s, g, Rect::new(0.0, 0.0, 200.0, 200.0));
    settle(&mut s);
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(0, 0, 200, 200)
    );

    s.set_clip(CLIENT, g, true).unwrap();
    let d = damage(&mut s);
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(0, 0, 100, 100)
    );
    // The band that used to be painted and now is not must be repainted.
    assert!(d.intersects(&IRect::new(150, 150, 10, 10)));
    assert_eq!(d.bounds(), IRect::new(0, 0, 200, 200));
}

#[test]
fn destroying_a_node_damages_what_it_covered() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    let a = rect(&mut s, g, Rect::new(10.0, 10.0, 20.0, 20.0));
    rect(&mut s, g, Rect::new(200.0, 200.0, 20.0, 20.0));
    settle(&mut s);

    s.destroy_node(CLIENT, a).unwrap();
    let d = damage(&mut s);
    assert!(d.intersects(&IRect::new(10, 10, 20, 20)));
    // The surviving sibling was not disturbed.
    assert!(!d.intersects(&IRect::new(205, 205, 5, 5)));
}

#[test]
fn destroying_a_group_damages_the_whole_subtree_extent() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    rect(&mut s, g, Rect::new(10.0, 10.0, 20.0, 20.0));
    rect(&mut s, g, Rect::new(100.0, 100.0, 20.0, 20.0));
    settle(&mut s);

    s.destroy_node(CLIENT, g).unwrap();
    let d = damage(&mut s);
    assert_eq!(d.bounds(), IRect::from_edges(10, 10, 120, 120));
}

#[test]
fn reparenting_damages_both_the_old_and_the_new_place() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let here = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let there = group(&mut s, root, Rect::new(300.0, 200.0, 100.0, 100.0));
    let r = rect(&mut s, here, Rect::new(0.0, 0.0, 20.0, 20.0));
    settle(&mut s);
    assert_eq!(s.node(r).unwrap().world_bounds(), IRect::new(0, 0, 20, 20));

    s.reparent(CLIENT, r, there, None).unwrap();
    let d = damage(&mut s);
    assert!(d.intersects(&IRect::new(0, 0, 20, 20)), "old place damaged");
    assert!(
        d.intersects(&IRect::new(300, 200, 20, 20)),
        "new place damaged"
    );
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(300, 200, 20, 20)
    );
}

#[test]
fn window_position_moves_everything_under_it() {
    let mut s = scene();
    let (win, root) = window_at(&mut s, Point::new(100.0, 100.0), Size::new(200.0, 200.0));
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(110, 110, 20, 20)
    );

    s.place_window(win, Some(OUT), Point::new(300.0, 100.0))
        .unwrap();
    let d = damage(&mut s);
    assert!(d.intersects(&IRect::new(110, 110, 20, 20)));
    assert!(d.intersects(&IRect::new(310, 110, 20, 20)));
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(310, 110, 20, 20)
    );
}

#[test]
fn output_scale_lands_in_the_window_root_transform() {
    let mut s = Scene::new();
    s.add_output(OUT, IRect::new(0, 0, 1600, 1200), 2.0);
    let win = s.create_window(CLIENT, "w", Size::new(200.0, 200.0), Layer::Normal);
    s.place_window(win, Some(OUT), Point::new(50.0, 25.0))
        .unwrap();
    let root = s.window_info(win).unwrap().root();
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    // (50 + 10) * 2 = 120, (25 + 10) * 2 = 70, 20 * 2 = 40.
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(120, 70, 40, 40)
    );
    // A local point maps through the same transform.
    let p = s
        .node(r)
        .unwrap()
        .world_transform()
        .apply(Point::new(5.0, 5.0));
    assert!((p.x - 130.0).abs() < 1e-4 && (p.y - 80.0).abs() < 1e-4);
}

#[test]
fn an_output_origin_offsets_every_window_on_it() {
    let mut s = Scene::new();
    s.add_output(OUT2, IRect::new(1920, 0, 800, 600), 1.0);
    let win = s.create_window(CLIENT, "w", Size::new(100.0, 100.0), Layer::Normal);
    s.place_window(win, Some(OUT2), Point::new(10.0, 10.0))
        .unwrap();
    let root = s.window_info(win).unwrap().root();
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0));

    let mut d = Damage::new();
    s.update(&mut DamageSink::new(&mut [(OUT2, &mut d)]));
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(1930, 10, 20, 20)
    );
    assert_eq!(d.bounds(), IRect::new(1930, 10, 20, 20));
}

#[test]
fn damage_goes_only_to_the_output_the_window_is_on() {
    let mut s = Scene::new();
    s.add_output(OUT, IRect::new(0, 0, 800, 600), 1.0);
    s.add_output(OUT2, IRect::new(800, 0, 800, 600), 1.0);
    let win = s.create_window(CLIENT, "w", Size::new(100.0, 100.0), Layer::Normal);
    s.place_window(win, Some(OUT2), Point::new(0.0, 0.0))
        .unwrap();
    let root = s.window_info(win).unwrap().root();
    rect(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0));

    let mut d0 = Damage::new();
    let mut d1 = Damage::new();
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d0), (OUT2, &mut d1)]));
    assert!(d0.is_empty(), "the other output is untouched");
    assert_eq!(d1.bounds(), IRect::new(800, 0, 20, 20));
}

#[test]
fn an_unplaced_window_produces_no_damage() {
    let mut s = scene();
    let win = s.create_window(CLIENT, "w", Size::new(100.0, 100.0), Layer::Normal);
    let root = s.window_info(win).unwrap().root();
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0));
    assert!(damage(&mut s).is_empty());
    assert!(s.node(r).unwrap().world_bounds().is_empty());

    // Placing it makes it appear.
    s.place_window(win, Some(OUT), Point::new(5.0, 5.0))
        .unwrap();
    assert_eq!(damage_bounds(&mut s), IRect::new(5, 5, 20, 20));

    // Unplacing it damages what it covered, once.
    s.place_window(win, None, Point::ZERO).unwrap();
    assert_eq!(damage_bounds(&mut s), IRect::new(5, 5, 20, 20));
    assert!(damage(&mut s).is_empty());
}

#[test]
fn an_invisible_subtree_is_not_damaged_by_changes_inside_it() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let r = rect(&mut s, g, Rect::new(0.0, 0.0, 20.0, 20.0));
    settle(&mut s);

    s.set_visible(CLIENT, g, false).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 20, 20)]);

    // Changing the hidden child paints nothing.
    s.set_fill(CLIENT, r, Fill::Solid(Color::BLACK)).unwrap();
    s.set_bounds(CLIENT, r, Rect::new(50.0, 50.0, 10.0, 10.0))
        .unwrap();
    assert!(damage(&mut s).is_empty());

    // Showing the group again paints it in its new state.
    s.set_visible(CLIENT, g, true).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(50, 50, 10, 10)]);
}

#[test]
fn an_empty_or_invisible_fill_paints_nothing() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    // A rect with no fill and no border.
    let plain = s.create_node(CLIENT, NodeKind::Rect, root, None).unwrap();
    s.set_bounds(CLIENT, plain, Rect::new(0.0, 0.0, 20.0, 20.0))
        .unwrap();
    // A fully transparent fill.
    let ghost = rect_colored(
        &mut s,
        root,
        Rect::new(50.0, 0.0, 20.0, 20.0),
        Color::TRANSPARENT,
    );
    // A zero-area rect.
    let flat = rect(&mut s, root, Rect::new(100.0, 0.0, 0.0, 20.0));
    // A group, which never paints on its own.
    let g = group(&mut s, root, Rect::new(150.0, 0.0, 20.0, 20.0));

    assert!(damage(&mut s).is_empty());
    for key in [plain, ghost, flat, g] {
        assert!(!s.node(key).unwrap().painted());
    }

    // A border alone is enough to paint.
    s.set_border(CLIENT, plain, Some(Border::new(1.0, Color::WHITE)))
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 20, 20)]);
}

#[test]
fn a_gradient_fill_paints_when_either_stop_is_visible() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = s.create_node(CLIENT, NodeKind::Rect, root, None).unwrap();
    s.set_bounds(CLIENT, r, Rect::new(0.0, 0.0, 20.0, 20.0))
        .unwrap();
    s.set_fill(
        CLIENT,
        r,
        Fill::Linear {
            start: Point::ZERO,
            end: Point::new(20.0, 0.0),
            c0: Color::TRANSPARENT,
            c1: Color::WHITE,
        },
    )
    .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 20, 20)]);
    assert!(s.node(r).unwrap().painted());
}

#[test]
fn resizing_a_window_reports_a_configure() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    // Creating the window is itself a configure.
    let mut d = Damage::new();
    let result = s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    assert_eq!(result.configures.len(), 1);
    assert_eq!(result.configures[0].window, win);
    assert_eq!(result.configures[0].size, Size::new(400.0, 300.0));

    // No change, no configure.
    let result = s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    assert!(result.configures.is_empty());

    s.set_window_size(CLIENT, win, Size::new(500.0, 400.0))
        .unwrap();
    let result = s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    assert_eq!(result.configures.len(), 1);
    assert_eq!(result.configures[0].size, Size::new(500.0, 400.0));
    assert_eq!(
        s.node(root).unwrap().bounds().size(),
        Size::new(500.0, 400.0)
    );

    // Setting the root's bounds directly is the same thing.
    s.set_bounds(CLIENT, root, Rect::new(0.0, 0.0, 600.0, 500.0))
        .unwrap();
    let result = s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    assert_eq!(result.configures.len(), 1);
    assert_eq!(result.configures[0].size, Size::new(600.0, 500.0));
    assert_eq!(s.window_info(win).unwrap().size(), Size::new(600.0, 500.0));
}

#[test]
fn restacking_damages_the_window_area() {
    let mut s = scene();
    let (back, back_root) = window_at(&mut s, Point::ZERO, Size::new(100.0, 100.0));
    rect(&mut s, back_root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let (front, front_root) = window_at(&mut s, Point::new(50.0, 50.0), Size::new(100.0, 100.0));
    rect(&mut s, front_root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);

    s.raise(back).unwrap();
    assert_eq!(damage_bounds(&mut s), IRect::new(0, 0, 100, 100));
    assert_eq!(
        s.windows(OUT).collect::<Vec<_>>(),
        vec![front, back],
        "raised to the front"
    );

    s.lower(back).unwrap();
    assert_eq!(damage_bounds(&mut s), IRect::new(0, 0, 100, 100));
    assert_eq!(s.windows(OUT).collect::<Vec<_>>(), vec![back, front]);

    // Raising the already-front window changes nothing.
    s.raise(front).unwrap();
    assert!(damage(&mut s).is_empty());
}

#[test]
fn moving_an_output_damages_it_and_its_windows() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    rect(&mut s, root, Rect::new(0.0, 0.0, 50.0, 50.0));
    settle(&mut s);

    s.add_output(OUT, IRect::new(0, 0, 1024, 768), 1.0);
    let d = damage(&mut s);
    assert_eq!(d.bounds(), IRect::new(0, 0, 1024, 768));
    assert_eq!(s.output_info(OUT), Some((IRect::new(0, 0, 1024, 768), 1.0)));
}

#[test]
fn removing_an_output_unplaces_its_windows() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    rect(&mut s, root, Rect::new(0.0, 0.0, 50.0, 50.0));
    settle(&mut s);

    s.remove_output(OUT);
    assert_eq!(s.window_info(win).unwrap().output(), None);
    assert_eq!(s.output_info(OUT), None);
    // And the scene is still usable: nothing panics, nothing is damaged.
    let (d, _) = update(&mut s);
    assert!(d.is_empty());
    assert!(s.node(root).unwrap().subtree_bounds().is_empty());
}

#[test]
fn the_server_may_move_another_clients_window() {
    let mut s = scene();
    let win = s.create_window(
        ClientId(7),
        "theirs",
        Size::new(100.0, 100.0),
        Layer::Normal,
    );
    let root = s.window_info(win).unwrap().root();
    let r = s
        .create_node(ClientId(7), NodeKind::Rect, root, None)
        .unwrap();
    s.set_bounds(ClientId(7), r, Rect::new(0.0, 0.0, 20.0, 20.0))
        .unwrap();
    s.set_fill(ClientId(7), r, Fill::Solid(Color::WHITE))
        .unwrap();
    // Placement is a server decision and takes no client id.
    s.place_window(win, Some(OUT), Point::new(40.0, 40.0))
        .unwrap();
    assert_eq!(damage_bounds(&mut s), IRect::new(40, 40, 20, 20));
}

#[test]
fn layers_stack_regardless_of_raise() {
    let mut s = scene();
    let normal = s.create_window(CLIENT, "n", Size::new(100.0, 100.0), Layer::Normal);
    let panel = s.create_window(CLIENT, "p", Size::new(100.0, 100.0), Layer::Top);
    let wallpaper = s.create_window(CLIENT, "b", Size::new(100.0, 100.0), Layer::Background);
    for w in [normal, panel, wallpaper] {
        s.place_window(w, Some(OUT), Point::ZERO).unwrap();
    }
    assert_eq!(
        s.windows(OUT).collect::<Vec<_>>(),
        vec![wallpaper, normal, panel]
    );

    // Raising within a layer cannot jump a layer.
    s.raise(normal).unwrap();
    assert_eq!(
        s.windows(OUT).collect::<Vec<_>>(),
        vec![wallpaper, normal, panel]
    );

    // Changing layer does.
    s.set_layer(normal, Layer::Overlay).unwrap();
    assert_eq!(
        s.windows(OUT).collect::<Vec<_>>(),
        vec![wallpaper, panel, normal]
    );
    assert_eq!(s.window_info(normal).unwrap().layer(), Layer::Overlay);
    assert_eq!(
        s.windows_front_to_back(OUT).collect::<Vec<_>>(),
        vec![normal, panel, wallpaper]
    );
}

#[test]
fn damage_accumulates_across_updates_until_the_caller_clears_it() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let a = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    settle(&mut s);

    let mut d = Damage::new();
    s.set_fill(CLIENT, a, Fill::Solid(Color::BLACK)).unwrap();
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    let b = rect(&mut s, root, Rect::new(500.0, 500.0, 10.0, 10.0));
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));

    assert!(d.intersects(&IRect::new(0, 0, 10, 10)));
    assert!(d.intersects(&IRect::new(500, 500, 10, 10)));
    assert_eq!(
        s.node(b).unwrap().world_bounds(),
        IRect::new(500, 500, 10, 10)
    );
}

#[test]
fn damage_for_an_output_missing_from_the_sink_is_dropped() {
    let mut s = Scene::new();
    s.add_output(OUT, IRect::new(0, 0, 800, 600), 1.0);
    s.add_output(OUT2, IRect::new(800, 0, 800, 600), 1.0);
    let win = s.create_window(CLIENT, "w", Size::new(100.0, 100.0), Layer::Normal);
    s.place_window(win, Some(OUT2), Point::ZERO).unwrap();
    let root = s.window_info(win).unwrap().root();
    rect(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0));

    // Only OUT is in the sink; the OUT2 damage is simply not recorded, and the
    // scene still ends up clean.
    let mut d = Damage::new();
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    assert!(d.is_empty());
    let (d2, stats) = update(&mut s);
    assert!(d2.is_empty());
    assert_eq!(stats.visited_nodes, 0);
}

#[test]
fn a_rotated_group_damages_the_bounding_box() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(100.0, 100.0, 100.0, 100.0));
    let r = rect(&mut s, g, Rect::new(0.0, 0.0, 40.0, 20.0));
    settle(&mut s);

    // A 90-degree rotation about the group's origin.
    let quarter_turn = Transform {
        a: 0.0,
        b: 1.0,
        c: -1.0,
        d: 0.0,
        e: 0.0,
        f: 0.0,
    };
    s.set_transform(CLIENT, g, quarter_turn).unwrap();
    settle(&mut s);
    // The 40x20 rect becomes 20x40, hanging up-left of the group origin.
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(80, 100, 20, 40)
    );
    assert!(!s.node(r).unwrap().world_transform().is_axis_aligned());
}

#[test]
fn deeply_nested_transforms_compose() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let mut parent = root;
    // Ten nested groups, each offset by (1, 2) in bounds and scaling by 1.0.
    for _ in 0..10 {
        parent = group(&mut s, parent, Rect::new(1.0, 2.0, 1000.0, 1000.0));
    }
    let r = rect(&mut s, parent, Rect::new(0.0, 0.0, 5.0, 5.0));
    settle(&mut s);
    assert_eq!(s.node(r).unwrap().world_bounds(), IRect::new(10, 20, 5, 5));

    // Scaling the outermost group scales everything under it.
    let outer = s.node(root).unwrap().children()[0];
    s.set_transform(CLIENT, outer, Transform::scale(2.0, 2.0))
        .unwrap();
    settle(&mut s);
    // The first group's own offset (1,2) is outside its transform; the nine
    // below it are doubled: (1 + 2*9, 2 + 2*18) = (19, 38), size 10x10.
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(19, 38, 10, 10)
    );
}

// --------------------------------------------------------------------------
// Inherited recomposition: a node dragged along by an ancestor carries no
// dirty flags of its own, so the damage test must compare cached world state.
// These are regression tests for a bug where fading a group repainted
// nothing.
// --------------------------------------------------------------------------

#[test]
fn a_group_opacity_change_damages_its_children() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let r = rect(&mut s, g, Rect::new(0.0, 0.0, 20.0, 20.0));
    settle(&mut s);

    // A partial fade moves nothing and changes no property of the child.
    s.set_opacity(CLIENT, g, 0.5).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 20, 20)]);
    assert!((s.node(r).unwrap().world_opacity() - 0.5).abs() < 1e-6);

    // Every further step of the fade damages it again.
    for (step, expected) in [(0.75_f32, 0.75_f32), (0.25, 0.25), (1.0, 1.0)] {
        s.set_opacity(CLIENT, g, step).unwrap();
        assert_eq!(
            damage(&mut s).rects(),
            &[IRect::new(0, 0, 20, 20)],
            "opacity {step}"
        );
        assert!((s.node(r).unwrap().world_opacity() - expected).abs() < 1e-6);
    }
}

#[test]
fn nested_group_opacities_multiply_and_damage_the_leaf() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let outer = group(&mut s, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let inner = group(&mut s, outer, Rect::new(0.0, 0.0, 100.0, 100.0));
    let r = rect(&mut s, inner, Rect::new(0.0, 0.0, 20.0, 20.0));
    s.set_opacity(CLIENT, r, 0.5).unwrap();
    settle(&mut s);
    assert!((s.node(r).unwrap().world_opacity() - 0.5).abs() < 1e-6);

    // Fading the outermost group reaches two levels down.
    s.set_opacity(CLIENT, outer, 0.5).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 20, 20)]);
    assert!((s.node(r).unwrap().world_opacity() - 0.25).abs() < 1e-6);

    // And so does the middle one: 0.5 * 0.5 * 0.5.
    s.set_opacity(CLIENT, inner, 0.5).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 20, 20)]);
    assert!((s.node(r).unwrap().world_opacity() - 0.125).abs() < 1e-6);
    assert!(s.node(r).unwrap().painted());
}

#[test]
fn a_group_opacity_change_damages_every_child_in_its_subtree() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    rect(&mut s, g, Rect::new(0.0, 0.0, 20.0, 20.0));
    rect(&mut s, g, Rect::new(200.0, 200.0, 20.0, 20.0));
    // A sibling outside the group must not be disturbed.
    rect(&mut s, root, Rect::new(350.0, 0.0, 20.0, 20.0));
    settle(&mut s);

    s.set_opacity(CLIENT, g, 0.5).unwrap();
    let d = damage(&mut s);
    assert!(d.intersects(&IRect::new(0, 0, 20, 20)));
    assert!(d.intersects(&IRect::new(200, 200, 20, 20)));
    assert!(
        !d.intersects(&IRect::new(350, 0, 20, 20)),
        "sibling untouched"
    );
}

#[test]
fn a_transform_that_preserves_the_bounding_box_still_damages() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(100.0, 100.0, 100.0, 100.0));
    // A square, so a quarter turn about its centre leaves the bbox identical.
    let r = rect(&mut s, g, Rect::new(-20.0, -20.0, 40.0, 40.0));
    settle(&mut s);
    let before = s.node(r).unwrap().world_bounds();
    assert_eq!(before, IRect::new(80, 80, 40, 40));

    // 90 degrees about the group origin, which is the square's centre.
    let quarter_turn = Transform {
        a: 0.0,
        b: 1.0,
        c: -1.0,
        d: 0.0,
        e: 0.0,
        f: 0.0,
    };
    s.set_transform(CLIENT, g, quarter_turn).unwrap();
    let d = damage(&mut s);
    // The bounding box is unchanged...
    assert_eq!(s.node(r).unwrap().world_bounds(), before);
    // ...but the pixels are not, so it must be repainted.
    assert_eq!(d.rects(), &[before], "a rotation in place must damage");
}

#[test]
fn a_mirror_transform_damages_even_though_the_box_is_identical() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(100.0, 100.0, 100.0, 100.0));
    let r = rect(&mut s, g, Rect::new(-30.0, -10.0, 60.0, 20.0));
    // A gradient makes the mirror visible; a solid fill would not care.
    s.set_fill(
        CLIENT,
        r,
        Fill::Linear {
            start: Point::ZERO,
            end: Point::new(60.0, 0.0),
            c0: Color::BLACK,
            c1: Color::WHITE,
        },
    )
    .unwrap();
    settle(&mut s);
    let before = s.node(r).unwrap().world_bounds();

    s.set_transform(CLIENT, g, Transform::scale(-1.0, 1.0))
        .unwrap();
    let d = damage(&mut s);
    assert_eq!(s.node(r).unwrap().world_bounds(), before, "same box");
    assert_eq!(d.rects(), &[before], "mirroring must still damage");
}

#[test]
fn an_identity_transform_reset_damages_nothing_extra() {
    // The flip side: recomposition that genuinely changes nothing is free.
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    rect(&mut s, g, Rect::new(0.0, 0.0, 20.0, 20.0));
    settle(&mut s);

    // Setting the transform it already has is a no-op at the mutation level.
    s.set_transform(CLIENT, g, Transform::IDENTITY).unwrap();
    let (d, stats) = update(&mut s);
    assert!(d.is_empty());
    assert_eq!(stats.visited_nodes, 0);

    // A round trip away and back does damage (twice), but ends up clean.
    s.set_transform(CLIENT, g, Transform::translate(5.0, 0.0))
        .unwrap();
    assert!(!damage(&mut s).is_empty());
    s.set_transform(CLIENT, g, Transform::IDENTITY).unwrap();
    assert!(!damage(&mut s).is_empty());
    let (d, stats) = update(&mut s);
    assert!(d.is_empty());
    assert_eq!(stats.visited_nodes, 0);
}

#[test]
fn moving_a_window_between_outputs_damages_each_within_its_own_rect() {
    let mut s = Scene::new();
    s.add_output(OUT, IRect::new(0, 0, 800, 600), 1.0);
    s.add_output(OUT2, IRect::new(800, 0, 800, 600), 1.0);
    let win = s.create_window(CLIENT, "w", Size::new(100.0, 100.0), Layer::Normal);
    s.place_window(win, Some(OUT), Point::new(10.0, 10.0))
        .unwrap();
    let root = s.window_info(win).unwrap().root();
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0));

    let mut d0 = Damage::new();
    let mut d1 = Damage::new();
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d0), (OUT2, &mut d1)]));
    d0.clear();
    d1.clear();

    // Move it to the second output.
    s.place_window(win, Some(OUT2), Point::new(5.0, 5.0))
        .unwrap();
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d0), (OUT2, &mut d1)]));

    // The vacated rect belongs to the output it left...
    assert_eq!(d0.bounds(), IRect::new(10, 10, 20, 20));
    // ...and the new rect to the one it arrived on.
    assert_eq!(d1.bounds(), IRect::new(805, 5, 20, 20));
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(805, 5, 20, 20)
    );

    // Neither region contains a pixel its own output cannot draw.
    for rect in d0.rects() {
        assert!(IRect::new(0, 0, 800, 600).contains_rect(rect));
    }
    for rect in d1.rects() {
        assert!(IRect::new(800, 0, 800, 600).contains_rect(rect));
    }
}
