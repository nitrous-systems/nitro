//! The pure-translation hint (`UpdateResult::translations`) and the cover
//! query a consumer pairs it with (`Scene::translation_cover`).
//!
//! The hint is only ever an *addition* to the damage, so every test here
//! is about when it is emitted and what it says; that the damage itself is
//! unchanged is what the rest of the suite already pins.

mod common;

use common::{CLIENT, OUT, group, rect_colored, scene, settle, window, window_at};
use nitro_core::{Color, Damage, IRect, Point, Rect, Size, Transform};
use nitro_scene::{DamageSink, Fill, NodeKey, NodeKind, Scene, Translation};

fn update(s: &mut Scene) -> Vec<Translation> {
    let mut d = Damage::new();
    s.update(&mut DamageSink::new(&mut [(OUT, &mut d)]))
        .translations
}

/// The `nitro-bench scroll` shape: a clipper at (0, 0, 400, 300) holding a
/// content group of 40 rows 15 px tall with a 1 px gap. Returns the
/// clipper, the content group and the rows.
fn bench(s: &mut Scene) -> (NodeKey, NodeKey, Vec<NodeKey>) {
    let (_, root) = window(s);
    let clipper = group(s, root, Rect::new(0.0, 0.0, 400.0, 300.0));
    s.set_clip(CLIENT, clipper, true).unwrap();
    let content = group(s, clipper, Rect::new(0.0, 0.0, 400.0, 640.0));
    let rows = (0..40)
        .map(|i| {
            let c = Color::rgb((i * 5) as u8, 0x40, 0x80);
            rect_colored(s, content, Rect::new(0.0, i as f32 * 16.0, 400.0, 15.0), c)
        })
        .collect();
    settle(s);
    (clipper, content, rows)
}

fn scroll_bounds(s: &mut Scene, content: NodeKey, y: f32) {
    s.set_bounds(CLIENT, content, Rect::new(0.0, y, 400.0, 640.0))
        .unwrap();
}

#[test]
fn the_bench_shape_is_a_translation_of_the_content_group() {
    let mut s = scene();
    let (_, content, _) = bench(&mut s);
    scroll_bounds(&mut s, content, -16.0);
    let t = update(&mut s);
    assert_eq!(t.len(), 1, "{t:?}");
    assert_eq!(t[0].node, content);
    assert!(t[0].moves_node);
    assert_eq!(t[0].delta, (0, -16));
    assert_eq!(t[0].clip, IRect::new(0, 0, 400, 300));
    assert!(t[0].foreign.is_empty(), "{:?}", t[0].foreign);
    // And again, from the new position.
    scroll_bounds(&mut s, content, -23.0);
    let t = update(&mut s);
    assert_eq!(t[0].delta, (0, -7));
}

#[test]
fn a_transform_on_a_clipping_group_is_a_translation_of_its_children() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let viewport = group(&mut s, root, Rect::new(10.0, 20.0, 200.0, 100.0));
    s.set_clip(CLIENT, viewport, true).unwrap();
    for i in 0..10 {
        rect_colored(
            &mut s,
            viewport,
            Rect::new(0.0, i as f32 * 20.0, 200.0, 20.0),
            Color::WHITE,
        );
    }
    settle(&mut s);
    s.set_transform(CLIENT, viewport, Transform::translate(3.0, -20.0))
        .unwrap();
    let t = update(&mut s);
    assert_eq!(t.len(), 1, "{t:?}");
    assert_eq!(t[0].node, viewport);
    assert!(!t[0].moves_node, "the viewport itself stays put");
    assert_eq!(t[0].delta, (3, -20));
    assert_eq!(t[0].clip, IRect::new(10, 20, 200, 100));
}

#[test]
fn moving_a_whole_window_is_a_translation_of_its_root() {
    let mut s = scene();
    let (win, root) = window_at(&mut s, Point::new(50.0, 50.0), Size::new(100.0, 100.0));
    rect_colored(
        &mut s,
        root,
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Color::WHITE,
    );
    settle(&mut s);
    s.place_window(win, Some(OUT), Point::new(60.0, 40.0))
        .unwrap();
    let t = update(&mut s);
    assert_eq!(t.len(), 1, "{t:?}");
    assert_eq!(t[0].node, root);
    assert_eq!(t[0].delta, (10, -10));
    // The vacated rect is banked by `place_window`, so it is foreign.
    assert!(t[0].foreign.intersects(&IRect::new(50, 50, 1, 1)));
}

/// Every precondition, one mutation each: none of these may yield a hint.
#[test]
fn anything_but_a_pure_integral_move_is_declined() {
    type Case = (&'static str, fn(&mut Scene, NodeKey, NodeKey, &[NodeKey]));
    let cases: [Case; 12] = [
        ("fractional delta", |s, _, c, _| scroll_bounds(s, c, -15.5)),
        ("scale", |s, cl, _, _| {
            s.set_transform(CLIENT, cl, Transform::scale(2.0, 2.0))
                .unwrap();
        }),
        ("size", |s, _, c, _| {
            s.set_bounds(CLIENT, c, Rect::new(0.0, -16.0, 400.0, 600.0))
                .unwrap();
        }),
        ("fill inside", |s, _, c, r| {
            scroll_bounds(s, c, -16.0);
            s.set_fill(CLIENT, r[3], Fill::Solid(Color::BLACK)).unwrap();
        }),
        ("child created", |s, _, c, _| {
            scroll_bounds(s, c, -16.0);
            rect_colored(s, c, Rect::new(0.0, 0.0, 5.0, 5.0), Color::WHITE);
        }),
        ("child destroyed", |s, _, c, r| {
            scroll_bounds(s, c, -16.0);
            s.destroy_node(CLIENT, r[2]).unwrap();
        }),
        ("opacity", |s, _, c, _| {
            scroll_bounds(s, c, -16.0);
            s.set_opacity(CLIENT, c, 0.5).unwrap();
        }),
        ("visibility inside", |s, _, c, r| {
            scroll_bounds(s, c, -16.0);
            s.set_visible(CLIENT, r[1], false).unwrap();
        }),
        ("clip toggled", |s, _, c, _| {
            scroll_bounds(s, c, -16.0);
            s.set_clip(CLIENT, c, true).unwrap();
        }),
        ("clipper moved too", |s, cl, c, _| {
            scroll_bounds(s, c, -16.0);
            s.set_bounds(CLIENT, cl, Rect::new(0.0, 4.0, 400.0, 300.0))
                .unwrap();
        }),
        ("clip slides against content", |s, _, c, _| {
            // A clipping node whose bounds move one way and transform the
            // other: its children move, but not with its clip.
            s.set_clip(CLIENT, c, true).unwrap();
            settle(s);
            s.set_bounds(CLIENT, c, Rect::new(0.0, -16.0, 400.0, 640.0))
                .unwrap();
            s.set_transform(CLIENT, c, Transform::translate(0.0, 8.0))
                .unwrap();
        }),
        ("two moved subtrees", |s, cl, c, _| {
            scroll_bounds(s, c, -16.0);
            let other = group(s, cl, Rect::new(0.0, 0.0, 10.0, 10.0));
            rect_colored(s, other, Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE);
            settle(s);
            scroll_bounds(s, c, -32.0);
            s.set_bounds(CLIENT, other, Rect::new(0.0, 5.0, 10.0, 10.0))
                .unwrap();
        }),
    ];
    for (name, mutate) in cases {
        let mut s = scene();
        let (clipper, content, rows) = bench(&mut s);
        mutate(&mut s, clipper, content, &rows);
        let t = update(&mut s);
        assert!(t.is_empty(), "{name}: {t:?}");
    }
}

#[test]
fn a_fractional_origin_is_declined_even_for_a_whole_pixel_delta() {
    let mut s = scene();
    let (_, content, _) = bench(&mut s);
    scroll_bounds(&mut s, content, -0.5);
    settle(&mut s);
    scroll_bounds(&mut s, content, -16.5);
    assert!(update(&mut s).is_empty());
}

#[test]
fn an_image_getting_new_pixels_inside_the_subtree_is_declined() {
    use nitro_scene::{BufferDesc, ImageRef};
    let mut s = scene();
    let (_, content, _) = bench(&mut s);
    let buf = s
        .create_buffer(
            CLIENT,
            BufferDesc::new(8, 8, 32, 0x3432_5258).with_opaque(true),
            vec![0u8; 256],
        )
        .unwrap();
    let img = s
        .create_node(CLIENT, NodeKind::Image, content, None)
        .unwrap();
    s.set_bounds(CLIENT, img, Rect::new(0.0, 0.0, 8.0, 8.0))
        .unwrap();
    s.set_image(
        CLIENT,
        img,
        Some(ImageRef::new(buf, IRect::new(0, 0, 8, 8))),
    )
    .unwrap();
    settle(&mut s);
    scroll_bounds(&mut s, content, -16.0);
    s.buffer_damaged(CLIENT, buf, &[IRect::new(0, 0, 2, 2)])
        .unwrap();
    assert!(update(&mut s).is_empty());
}

#[test]
fn another_windows_change_in_the_same_update_is_foreign() {
    let mut s = scene();
    let (_, content, _) = bench(&mut s);
    let (_, other) = window_at(&mut s, Point::new(500.0, 400.0), Size::new(50.0, 50.0));
    let r = rect_colored(&mut s, other, Rect::new(0.0, 0.0, 50.0, 50.0), Color::WHITE);
    settle(&mut s);
    scroll_bounds(&mut s, content, -16.0);
    s.set_fill(CLIENT, r, Fill::Solid(Color::BLACK)).unwrap();
    let t = update(&mut s);
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].foreign.bounds(), IRect::new(500, 400, 50, 50));
}

#[test]
fn the_cover_leaves_out_the_gaps_and_stays_in_the_clip() {
    let mut s = scene();
    let (_, content, _) = bench(&mut s);
    scroll_bounds(&mut s, content, -16.0);
    let t = update(&mut s);
    let (cover, above) = s
        .translation_cover(OUT, content, t[0].moves_node, &t[0].clip)
        .unwrap();
    assert!(above.is_empty());
    let clip = t[0].clip;
    assert!(cover.rects().iter().all(|r| clip.contains_rect(r)));
    // Rows now start at y = -16 + 16i, so the gap of row i is at 16i - 2.
    for y in 0..300 {
        let gap = y % 16 == 15;
        assert_eq!(cover.contains(10, y), !gap, "y = {y}");
    }
}

#[test]
fn a_window_over_the_viewport_is_above() {
    let mut s = scene();
    let (_, content, _) = bench(&mut s);
    let (_, over) = window_at(&mut s, Point::new(100.0, 100.0), Size::new(50.0, 50.0));
    rect_colored(&mut s, over, Rect::new(0.0, 0.0, 50.0, 50.0), Color::BLACK);
    settle(&mut s);
    scroll_bounds(&mut s, content, -16.0);
    let t = update(&mut s);
    let (_, above) = s
        .translation_cover(OUT, content, t[0].moves_node, &t[0].clip)
        .unwrap();
    assert_eq!(above.rects(), [IRect::new(100, 100, 50, 50)]);
}

#[test]
fn a_node_that_stayed_put_is_not_in_its_own_cover() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    // An opaque viewport *rect* can't have children in this scene API, so
    // the node that stays is a group; its own item is absent either way.
    let viewport = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    s.set_clip(CLIENT, viewport, true).unwrap();
    rect_colored(
        &mut s,
        viewport,
        Rect::new(0.0, 0.0, 100.0, 50.0),
        Color::WHITE,
    );
    settle(&mut s);
    s.set_transform(CLIENT, viewport, Transform::translate(0.0, 10.0))
        .unwrap();
    let t = update(&mut s);
    let (cover, _) = s
        .translation_cover(OUT, viewport, t[0].moves_node, &t[0].clip)
        .unwrap();
    assert_eq!(cover.rects(), [IRect::new(0, 10, 100, 50)]);
}
