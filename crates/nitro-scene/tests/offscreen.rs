//! Offscreen windows (`Scene::set_offscreen`): kept up to date, never
//! painted to or hit on their output, their damage reported per window.

mod common;

use common::{OUT, damage, rect, scene, settle, window, window_at};
use nitro_core::{Damage, IRect, Point, Rect, Size, Transform};
use nitro_scene::{ClientId, DamageSink, NodeKey, Scene, UpdateResult};

const ALL: IRect = IRect::new(0, 0, 800, 600);

fn painted(scene: &Scene) -> Vec<NodeKey> {
    let mut out = Vec::new();
    scene.paint_list(OUT, &ALL, &mut out);
    out.into_iter().map(|i| i.node).collect()
}

fn full(scene: &mut Scene) -> (Damage, UpdateResult) {
    let mut d = Damage::new();
    let r = scene.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    (d, r)
}

#[test]
fn an_offscreen_window_paints_nothing_and_is_not_hit() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);
    assert_eq!(painted(&s), vec![r]);
    assert!(s.hit_test(OUT, Point::new(10.0, 10.0)).is_some());

    s.set_offscreen(win, true).unwrap();
    assert!(s.window_info(win).unwrap().is_offscreen());
    settle(&mut s);
    assert!(painted(&s).is_empty());
    assert!(s.hit_test(OUT, Point::new(10.0, 10.0)).is_none());
}

#[test]
fn a_change_inside_it_is_offscreen_damage_only() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    s.set_offscreen(win, true).unwrap();
    settle(&mut s);

    s.set_bounds(common::CLIENT, r, Rect::new(10.0, 10.0, 50.0, 50.0))
        .unwrap();
    let (d, result) = full(&mut s);
    assert!(d.is_empty(), "the output is not damaged: {:?}", d.rects());
    assert!(!result.offscreen.is_empty());
    assert!(result.offscreen.iter().all(|(w, _)| *w == win));
    let bounds = result
        .offscreen
        .iter()
        .fold(IRect::EMPTY, |a, (_, r)| a.union(r));
    assert_eq!(bounds, IRect::new(0, 0, 100, 100), "old ∪ new");
    let (d, result) = full(&mut s);
    assert!(d.is_empty() && result.offscreen.is_empty(), "then quiet");
}

#[test]
fn toggling_damages_the_output_once_then_goes_quiet() {
    let mut s = scene();
    let (win, root) = window_at(&mut s, Point::new(20.0, 30.0), Size::new(100.0, 80.0));
    rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 80.0));
    settle(&mut s);

    s.set_offscreen(win, true).unwrap();
    assert_eq!(damage(&mut s).bounds(), IRect::new(20, 30, 100, 80));
    assert!(damage(&mut s).is_empty());
    // The same value again is free.
    s.set_offscreen(win, true).unwrap();
    assert!(damage(&mut s).is_empty());

    s.set_offscreen(win, false).unwrap();
    assert_eq!(damage(&mut s).bounds(), IRect::new(20, 30, 100, 80));
    assert!(damage(&mut s).is_empty());
}

#[test]
fn paint_window_lists_its_items_in_order() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let a = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let b = rect(&mut s, root, Rect::new(50.0, 50.0, 100.0, 100.0));
    // Another window: not in the list.
    let (_, other) = window(&mut s);
    rect(&mut s, other, Rect::new(0.0, 0.0, 10.0, 10.0));
    s.set_offscreen(win, true).unwrap();
    settle(&mut s);
    let mut out = Vec::new();
    s.paint_window(win, &ALL, &mut out);
    assert_eq!(out.iter().map(|i| i.node).collect::<Vec<_>>(), vec![a, b]);
    // Clipped like paint_list.
    out.clear();
    s.paint_window(win, &IRect::new(0, 0, 40, 40), &mut out);
    assert_eq!(out.iter().map(|i| i.node).collect::<Vec<_>>(), vec![a]);
    assert_eq!(out[0].bounds, IRect::new(0, 0, 40, 40));
}

#[test]
fn no_translation_hint_comes_from_an_offscreen_window() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    s.set_offscreen(win, true).unwrap();
    settle(&mut s);
    s.set_transform(ClientId::SERVER, root, Transform::translate(0.0, 10.0))
        .unwrap();
    let (d, result) = full(&mut s);
    assert!(result.translations.is_empty());
    assert!(d.is_empty());
    assert!(!result.offscreen.is_empty());
}
