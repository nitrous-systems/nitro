//! Paint list order and culling, and hit testing.

mod common;

use common::{CLIENT, OUT, group, rect, rect_colored, scene, settle, text, window, window_at};
use nitro_core::{Color, IRect, Point, Rect, Size, Transform};
use nitro_scene::{
    Border, Fill, Layer, NodeKey, NodeKind, PaintItem, PaintKind, Scene, TextAlign, TextRef,
};

fn paint(scene: &Scene, clip: IRect) -> Vec<PaintItem> {
    let mut out = Vec::new();
    scene.paint_list(OUT, &clip, &mut out);
    out
}

fn painted_nodes(scene: &Scene, clip: IRect) -> Vec<NodeKey> {
    paint(scene, clip).into_iter().map(|i| i.node).collect()
}

const ALL: IRect = IRect::new(0, 0, 800, 600);

#[test]
fn paint_order_is_back_to_front() {
    let mut scn = scene();
    let (_, root) = window(&mut scn);
    let first = rect(&mut scn, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let grp = group(&mut scn, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let inner_back = rect(&mut scn, grp, Rect::new(0.0, 0.0, 50.0, 50.0));
    let inner_front = rect(&mut scn, grp, Rect::new(0.0, 0.0, 50.0, 50.0));
    let last = rect(&mut scn, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut scn);

    // Children in order, depth first; the group itself paints nothing.
    assert_eq!(
        painted_nodes(&scn, ALL),
        vec![first, inner_back, inner_front, last]
    );
}

#[test]
fn windows_paint_back_to_front_across_layers() {
    let mut s = scene();
    let (_normal, normal_root) = window(&mut s);
    let n = rect(&mut s, normal_root, Rect::new(0.0, 0.0, 100.0, 100.0));

    let panel = s.create_window(CLIENT, "p", Size::new(100.0, 100.0), Layer::Top);
    s.place_window(panel, Some(OUT), Point::ZERO).unwrap();
    let panel_root = s.window_info(panel).unwrap().root();
    let p = rect(&mut s, panel_root, Rect::new(0.0, 0.0, 100.0, 100.0));

    let back = s.create_window(CLIENT, "b", Size::new(100.0, 100.0), Layer::Background);
    s.place_window(back, Some(OUT), Point::ZERO).unwrap();
    let back_root = s.window_info(back).unwrap().root();
    let w = rect(&mut s, back_root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);

    assert_eq!(painted_nodes(&s, ALL), vec![w, n, p]);

    // Raising within the normal layer cannot put it above the panel.
    let (second, second_root) = window(&mut s);
    let n2 = rect(&mut s, second_root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);
    s.raise(second).unwrap();
    assert_eq!(painted_nodes(&s, ALL), vec![w, n, n2, p]);
}

#[test]
fn clip_culls_everything_that_does_not_intersect() {
    let mut s = scene();
    let (_, root) = window_at(&mut s, Point::ZERO, Size::new(800.0, 600.0));
    let near = rect(&mut s, root, Rect::new(0.0, 0.0, 50.0, 50.0));
    let far = rect(&mut s, root, Rect::new(400.0, 400.0, 50.0, 50.0));
    settle(&mut s);

    assert_eq!(painted_nodes(&s, ALL), vec![near, far]);
    assert_eq!(painted_nodes(&s, IRect::new(0, 0, 100, 100)), vec![near]);
    assert_eq!(painted_nodes(&s, IRect::new(390, 390, 100, 100)), vec![far]);
    assert!(painted_nodes(&s, IRect::new(200, 200, 50, 50)).is_empty());
    // An empty clip yields nothing.
    assert!(painted_nodes(&s, IRect::EMPTY).is_empty());
    // Touching only the exclusive edge is a miss.
    assert!(painted_nodes(&s, IRect::new(50, 0, 10, 10)).is_empty());
}

#[test]
fn a_culled_group_is_not_walked() {
    let mut s = scene();
    let (_, root) = window_at(&mut s, Point::ZERO, Size::new(800.0, 600.0));
    let g = group(&mut s, root, Rect::new(500.0, 500.0, 100.0, 100.0));
    for i in 0..10 {
        rect(&mut s, g, Rect::new(i as f32, 0.0, 10.0, 10.0));
    }
    let visible = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    settle(&mut s);

    // The whole group's subtree misses the clip.
    assert_eq!(painted_nodes(&s, IRect::new(0, 0, 100, 100)), vec![visible]);
    assert_eq!(painted_nodes(&s, ALL).len(), 11);
}

#[test]
fn invisible_and_transparent_subtrees_are_skipped() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let hidden = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    rect(&mut s, hidden, Rect::new(0.0, 0.0, 50.0, 50.0));
    s.set_visible(CLIENT, hidden, false).unwrap();

    let ghost = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    rect(&mut s, ghost, Rect::new(0.0, 0.0, 50.0, 50.0));
    s.set_opacity(CLIENT, ghost, 0.0).unwrap();

    let shown = rect(&mut s, root, Rect::new(0.0, 0.0, 50.0, 50.0));
    settle(&mut s);

    assert_eq!(painted_nodes(&s, ALL), vec![shown]);
}

#[test]
fn paint_items_carry_accumulated_opacity_and_clip() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(10.0, 10.0, 100.0, 100.0));
    s.set_opacity(CLIENT, g, 0.5).unwrap();
    s.set_clip(CLIENT, g, true).unwrap();
    let r = rect_colored(&mut s, g, Rect::new(0.0, 0.0, 500.0, 500.0), Color::BLACK);
    s.set_opacity(CLIENT, r, 0.5).unwrap();
    settle(&mut s);

    let items = paint(&s, ALL);
    assert_eq!(items.len(), 1);
    let item = items[0];
    assert_eq!(item.node, r);
    assert_eq!(item.window, win);
    assert!((item.opacity - 0.25).abs() < 1e-6, "0.5 * 0.5");
    // The group's clip bounds the item.
    assert_eq!(item.clip, IRect::new(10, 10, 100, 100));
    assert_eq!(item.bounds, IRect::new(10, 10, 100, 100));
    assert_eq!(
        item.kind,
        PaintKind::Rect {
            size: (500.0, 500.0),
            fill: Fill::Solid(Color::BLACK),
            corner_radius: 0.0,
            border: None,
        }
    );
    // The transform places the item's local origin at the group's origin.
    let origin = item.transform.apply(Point::ZERO);
    assert!((origin.x - 10.0).abs() < 1e-4 && (origin.y - 10.0).abs() < 1e-4);
}

#[test]
fn a_paint_items_clip_is_narrowed_by_the_requested_region() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    rect(&mut s, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    settle(&mut s);

    let items = paint(&s, IRect::new(50, 50, 40, 40));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].clip, IRect::new(50, 50, 40, 40));
    assert_eq!(items[0].bounds, IRect::new(50, 50, 40, 40));
}

#[test]
fn paint_list_appends_and_reports_opaque_covers() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let solid = rect_colored(
        &mut s,
        root,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Color::BLACK,
    );
    let translucent = rect_colored(
        &mut s,
        root,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Color::BLACK.with_alpha(128),
    );
    let rounded = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    s.set_corner_radius(CLIENT, rounded, 8.0).unwrap();
    settle(&mut s);

    let mut out = vec![];
    s.paint_list(OUT, &ALL, &mut out);
    let before = out.len();
    s.paint_list(OUT, &ALL, &mut out);
    assert_eq!(out.len(), before * 2, "paint_list appends");

    let by_node = |key| out.iter().find(|i| i.node == key).copied().unwrap();
    assert_eq!(
        by_node(solid).opaque_cover(),
        Some(IRect::new(0, 0, 100, 100))
    );
    assert_eq!(by_node(translucent).opaque_cover(), None);
    assert_eq!(
        by_node(rounded).opaque_cover(),
        None,
        "rounded corners leak"
    );

    // A partially transparent node cannot be an occluder either.
    s.set_opacity(CLIENT, solid, 0.5).unwrap();
    settle(&mut s);
    let items = paint(&s, ALL);
    let item = items.iter().find(|i| i.node == solid).unwrap();
    assert_eq!(item.opaque_cover(), None);
}

#[test]
fn an_opaque_border_does_not_spoil_the_cover() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect_colored(
        &mut s,
        root,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Color::BLACK,
    );
    s.set_border(CLIENT, r, Some(Border::new(2.0, Color::WHITE)))
        .unwrap();
    settle(&mut s);
    assert!(paint(&s, ALL)[0].opaque_cover().is_some());

    // A translucent border can show what is behind it.
    s.set_border(
        CLIENT,
        r,
        Some(Border::new(2.0, Color::WHITE.with_alpha(100))),
    )
    .unwrap();
    settle(&mut s);
    assert_eq!(paint(&s, ALL)[0].opaque_cover(), None);
}

#[test]
fn paint_list_for_an_unknown_output_is_empty() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);

    let mut out = Vec::new();
    s.paint_list(nitro_scene::OutputId(42), &ALL, &mut out);
    assert!(out.is_empty());
}

// ---------------------------------------------------------------- hit tests

#[test]
fn hit_test_finds_the_deepest_node() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(10.0, 10.0, 200.0, 200.0));
    let back = rect(&mut s, g, Rect::new(0.0, 0.0, 100.0, 100.0));
    let front = rect(&mut s, g, Rect::new(50.0, 50.0, 100.0, 100.0));
    settle(&mut s);

    // Where only `back` is.
    let hit = s.hit_test(OUT, Point::new(20.0, 20.0)).unwrap();
    assert_eq!(hit.node, back);
    assert_eq!(hit.window, win);
    assert!((hit.local.x - 10.0).abs() < 1e-4 && (hit.local.y - 10.0).abs() < 1e-4);

    // Where both are: the later child wins.
    let hit = s.hit_test(OUT, Point::new(70.0, 70.0)).unwrap();
    assert_eq!(hit.node, front);
    assert!((hit.local.x - 10.0).abs() < 1e-4);

    // Outside everything.
    assert!(s.hit_test(OUT, Point::new(500.0, 500.0)).is_none());
    assert!(s.hit_test(OUT, Point::new(-1.0, -1.0)).is_none());
}

#[test]
fn hit_test_picks_the_topmost_window() {
    let mut s = scene();
    let (back, back_root) = window_at(&mut s, Point::ZERO, Size::new(200.0, 200.0));
    let b = rect(&mut s, back_root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let (front, front_root) = window_at(&mut s, Point::new(100.0, 100.0), Size::new(200.0, 200.0));
    let f = rect(&mut s, front_root, Rect::new(0.0, 0.0, 200.0, 200.0));
    settle(&mut s);

    // The overlap belongs to the front window.
    let hit = s.hit_test(OUT, Point::new(150.0, 150.0)).unwrap();
    assert_eq!(hit.window, front);
    assert_eq!(hit.node, f);
    assert!((hit.local.x - 50.0).abs() < 1e-4);

    // Outside the overlap, the back window still answers.
    let hit = s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap();
    assert_eq!(hit.window, back);
    assert_eq!(hit.node, b);

    // Raising the back window flips the overlap.
    s.raise(back).unwrap();
    settle(&mut s);
    let hit = s.hit_test(OUT, Point::new(150.0, 150.0)).unwrap();
    assert_eq!(hit.window, back);
}

#[test]
fn a_higher_layer_wins_the_hit_regardless_of_stacking() {
    let mut s = scene();
    let (normal, normal_root) = window_at(&mut s, Point::ZERO, Size::new(200.0, 200.0));
    let n = rect(&mut s, normal_root, Rect::new(0.0, 0.0, 200.0, 200.0));

    let panel = s.create_window(CLIENT, "p", Size::new(200.0, 50.0), Layer::Top);
    s.place_window(panel, Some(OUT), Point::ZERO).unwrap();
    let panel_root = s.window_info(panel).unwrap().root();
    let p = rect(&mut s, panel_root, Rect::new(0.0, 0.0, 200.0, 50.0));
    settle(&mut s);

    // The panel is on a higher layer, so it takes the overlap...
    let hit = s.hit_test(OUT, Point::new(10.0, 10.0)).unwrap();
    assert_eq!(hit.window, panel);
    assert_eq!(hit.node, p);

    // ...and below it, the normal window answers.
    let hit = s.hit_test(OUT, Point::new(10.0, 100.0)).unwrap();
    assert_eq!(hit.window, normal);
    assert_eq!(hit.node, n);

    // Raising the normal window does not let it jump the layer.
    s.raise(normal).unwrap();
    settle(&mut s);
    assert_eq!(
        s.hit_test(OUT, Point::new(10.0, 10.0)).unwrap().window,
        panel
    );
}

#[test]
fn hit_test_respects_clip() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    s.set_clip(CLIENT, g, true).unwrap();
    let r = rect(&mut s, g, Rect::new(0.0, 0.0, 300.0, 300.0));
    settle(&mut s);

    // Inside the clip: a hit.
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, r);
    // Inside the rect but outside the clip: no hit.
    assert!(s.hit_test(OUT, Point::new(150.0, 150.0)).is_none());
}

#[test]
fn hit_test_skips_invisible_and_transparent_nodes() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let under = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let over = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, over);

    // A hidden node does not catch the point; the one below does.
    s.set_visible(CLIENT, over, false).unwrap();
    settle(&mut s);
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, under);

    // Nor does a fully transparent one.
    s.set_visible(CLIENT, over, true).unwrap();
    s.set_opacity(CLIENT, over, 0.0).unwrap();
    settle(&mut s);
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, under);

    // A partly transparent one still catches it.
    s.set_opacity(CLIENT, over, 0.5).unwrap();
    settle(&mut s);
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, over);
}

#[test]
fn an_empty_group_never_swallows_a_hit() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let under = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    // A group covering everything, with no content of its own.
    group(&mut s, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    settle(&mut s);
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, under);

    // Nor does a rect with nothing to draw.
    let blank = s
        .create_node(CLIENT, nitro_scene::NodeKind::Rect, root, None)
        .unwrap();
    s.set_bounds(CLIENT, blank, Rect::new(0.0, 0.0, 400.0, 300.0))
        .unwrap();
    settle(&mut s);
    assert_eq!(s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().node, under);
}

#[test]
fn hit_test_inverts_the_world_transform() {
    let mut s = Scene::new();
    s.add_output(OUT, IRect::new(0, 0, 1600, 1200), 2.0);
    let win = s.create_window(CLIENT, "w", Size::new(400.0, 300.0), Layer::Normal);
    s.place_window(win, Some(OUT), Point::new(10.0, 10.0))
        .unwrap();
    let root = s.window_info(win).unwrap().root();
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    s.set_transform(CLIENT, g, Transform::scale(2.0, 2.0))
        .unwrap();
    let r = rect(&mut s, g, Rect::new(10.0, 10.0, 50.0, 50.0));
    settle(&mut s);

    // Device x = (10 + 2*(10 + local)) * 2. local 5 -> (10 + 30) * 2 = 80.
    assert_eq!(
        s.node(r).unwrap().world_bounds(),
        IRect::new(60, 60, 200, 200)
    );
    let hit = s.hit_test(OUT, Point::new(80.0, 80.0)).unwrap();
    assert_eq!(hit.node, r);
    assert!(
        (hit.local.x - 5.0).abs() < 1e-4 && (hit.local.y - 5.0).abs() < 1e-4,
        "local was {:?}",
        hit.local
    );
}

#[test]
fn a_rotated_node_is_hit_only_inside_its_actual_shape() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(100.0, 100.0, 200.0, 200.0));
    // 45-degree rotation: the bounding box has corners the shape does not.
    let k = std::f32::consts::FRAC_1_SQRT_2;
    s.set_transform(
        CLIENT,
        g,
        Transform {
            a: k,
            b: k,
            c: -k,
            d: k,
            e: 0.0,
            f: 0.0,
        },
    )
    .unwrap();
    let r = rect(&mut s, g, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);

    // The rect's local centre maps to (100, 100) + rot(50, 50) = (100, 170.7).
    let hit = s.hit_test(OUT, Point::new(100.0, 170.0));
    assert_eq!(hit.map(|h| h.node), Some(r));
    // A corner of the bounding box that the rotated square misses.
    let bounds = s.node(r).unwrap().world_bounds();
    let corner = Point::new(bounds.x as f32 + 1.0, bounds.y as f32 + 1.0);
    assert!(
        s.hit_test(OUT, corner).is_none(),
        "the bounding-box corner is outside the rotated square"
    );
}

#[test]
fn hit_test_on_an_unknown_or_empty_output_is_none() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);

    assert!(
        s.hit_test(nitro_scene::OutputId(42), Point::new(1.0, 1.0))
            .is_none()
    );
    // An unplaced window cannot be hit.
    let stray = s.create_window(CLIENT, "s", Size::new(100.0, 100.0), Layer::Overlay);
    let stray_root = s.window_info(stray).unwrap().root();
    rect(&mut s, stray_root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);
    assert_ne!(
        s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().window,
        stray
    );
}

#[test]
fn hit_test_edges_are_half_open() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    assert_eq!(
        s.hit_test(OUT, Point::new(10.0, 10.0)).map(|h| h.node),
        Some(r)
    );
    assert_eq!(
        s.hit_test(OUT, Point::new(29.9, 29.9)).map(|h| h.node),
        Some(r)
    );
    assert!(s.hit_test(OUT, Point::new(30.0, 20.0)).is_none());
    assert!(s.hit_test(OUT, Point::new(9.9, 20.0)).is_none());
}

#[test]
fn opaque_cover_is_refused_on_a_fractional_edge() {
    // `bounds` is rounded outward, so a rect on a half-pixel boundary covers
    // its edge pixels only partially. Reporting it as an opaque cover would
    // let a rasterizer skip content genuinely visible through those pixels.
    let mut s = scene();
    let (_, root) = window(&mut s);
    let aligned = rect_colored(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0), Color::BLACK);
    settle(&mut s);
    let items = paint(&s, ALL);
    let item = items.iter().find(|i| i.node == aligned).unwrap();
    assert_eq!(item.opaque_cover(), Some(IRect::new(0, 0, 20, 20)));

    // Shift it by half a pixel: same outward-rounded bounds, no longer a cover.
    s.set_bounds(CLIENT, aligned, Rect::new(0.5, 0.0, 20.0, 20.0))
        .unwrap();
    settle(&mut s);
    let items = paint(&s, ALL);
    let item = items.iter().find(|i| i.node == aligned).unwrap();
    assert_eq!(item.bounds, IRect::new(0, 0, 21, 20), "rounded outward");
    assert_eq!(
        item.opaque_cover(),
        None,
        "a partially covered edge pixel is not an opaque cover"
    );

    // A fractional size is refused for the same reason.
    s.set_bounds(CLIENT, aligned, Rect::new(0.0, 0.0, 20.5, 20.0))
        .unwrap();
    settle(&mut s);
    let items = paint(&s, ALL);
    assert_eq!(items[0].opaque_cover(), None);
}

#[test]
fn opaque_cover_survives_an_integer_scale() {
    // A 2x output keeps everything pixel-aligned, so the cover still holds.
    let mut s = Scene::new();
    s.add_output(OUT, IRect::new(0, 0, 1600, 1200), 2.0);
    let win = s.create_window(CLIENT, "w", Size::new(200.0, 200.0), Layer::Normal);
    s.place_window(win, Some(OUT), Point::new(10.0, 10.0))
        .unwrap();
    let root = s.window_info(win).unwrap().root();
    rect_colored(&mut s, root, Rect::new(0.0, 0.0, 20.0, 20.0), Color::BLACK);
    settle(&mut s);

    let mut items = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 1600, 1200), &mut items);
    assert_eq!(items[0].opaque_cover(), Some(IRect::new(20, 20, 40, 40)));
}

#[test]
fn opaque_cover_is_refused_under_rotation() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(100.0, 100.0, 100.0, 100.0));
    rect_colored(&mut s, g, Rect::new(0.0, 0.0, 40.0, 40.0), Color::BLACK);
    settle(&mut s);
    assert!(paint(&s, ALL)[0].opaque_cover().is_some());

    let k = std::f32::consts::FRAC_1_SQRT_2;
    s.set_transform(
        CLIENT,
        g,
        Transform {
            a: k,
            b: k,
            c: -k,
            d: k,
            e: 0.0,
            f: 0.0,
        },
    )
    .unwrap();
    settle(&mut s);
    assert_eq!(paint(&s, ALL)[0].opaque_cover(), None);
}

#[test]
fn a_destroyed_window_is_skipped_by_both_traversals() {
    // Both traversals must survive a z-order entry that no longer resolves in
    // the same (defensive) way: skip it, do not abandon the walk.
    let mut s = scene();
    let (back, back_root) = window_at(&mut s, Point::ZERO, Size::new(200.0, 200.0));
    let b = rect(&mut s, back_root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let (front, front_root) = window_at(&mut s, Point::ZERO, Size::new(200.0, 200.0));
    rect(&mut s, front_root, Rect::new(0.0, 0.0, 200.0, 200.0));
    settle(&mut s);
    assert_eq!(
        s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap().window,
        front
    );

    // Destroying the front window drops it from the z-order entirely...
    s.destroy_window(CLIENT, front).unwrap();
    settle(&mut s);
    // ...and the window behind it answers, rather than the hit test failing.
    let hit = s.hit_test(OUT, Point::new(50.0, 50.0)).unwrap();
    assert_eq!(hit.window, back);
    assert_eq!(hit.node, b);
    assert_eq!(painted_nodes(&s, ALL), vec![b]);
}

#[test]
fn a_text_node_paints_its_run_with_the_alignment_applied() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    // A 200-wide box holding a 60-wide block, once per alignment.
    let block = Size::new(60.0, 16.0);
    let left = text(
        &mut s,
        root,
        Rect::new(10.0, 10.0, 200.0, 16.0),
        7,
        block,
        TextAlign::Left,
    );
    let centre = text(
        &mut s,
        root,
        Rect::new(10.0, 40.0, 200.0, 16.0),
        8,
        block,
        TextAlign::Center,
    );
    let right = text(
        &mut s,
        root,
        Rect::new(10.0, 70.0, 200.0, 16.0),
        9,
        block,
        TextAlign::Right,
    );
    settle(&mut s);

    let items = paint(&s, ALL);
    assert_eq!(
        items.iter().map(|i| i.node).collect::<Vec<_>>(),
        vec![left, centre, right]
    );
    let origins: Vec<(u32, f32)> = items
        .iter()
        .map(|i| match i.kind {
            PaintKind::Text { key, origin, color } => {
                assert_eq!(color, Color::WHITE);
                (key, origin.x)
            }
            other => panic!("expected a text item, got {other:?}"),
        })
        .collect();
    // The slack is 200 - 60 = 140.
    assert_eq!(origins, vec![(7, 0.0), (8, 70.0), (9, 140.0)]);
    // The item's transform places the node; the origin is local to it.
    let t = items[0].transform;
    assert_eq!(
        (t.e.to_bits(), t.f.to_bits()),
        (10.0f32.to_bits(), 10.0f32.to_bits())
    );
}

#[test]
fn a_text_node_with_no_run_or_no_colour_paints_nothing() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let bare = s.create_node(CLIENT, NodeKind::Text, root, None).unwrap();
    s.set_bounds(CLIENT, bare, Rect::new(0.0, 0.0, 100.0, 20.0))
        .unwrap();
    let clear = text(
        &mut s,
        root,
        Rect::new(0.0, 30.0, 100.0, 20.0),
        1,
        Size::new(50.0, 16.0),
        TextAlign::Left,
    );
    s.set_text(
        CLIENT,
        clear,
        Some(TextRef {
            key: 1,
            size: Size::new(50.0, 16.0),
            ascent: 12.0,
            color: Color::TRANSPARENT,
            align: TextAlign::Left,
        }),
    )
    .unwrap();
    // An empty block is nothing to draw either.
    let empty = text(
        &mut s,
        root,
        Rect::new(0.0, 60.0, 100.0, 20.0),
        2,
        Size::new(0.0, 0.0),
        TextAlign::Left,
    );
    settle(&mut s);

    assert!(painted_nodes(&s, ALL).is_empty());
    for key in [bare, clear, empty] {
        assert!(!s.node(key).unwrap().painted());
    }
}
