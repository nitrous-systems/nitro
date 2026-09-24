//! `Admit`: which clients' windows are painted and hit-tested. The scene's
//! half of a session lock.

mod common;

use common::{CLIENT, OTHER, OUT, damage_bounds, rect, scene, settle, window};
use nitro_core::{IRect, Point, Rect, Size};
use nitro_scene::{Admit, Layer, NodeKey, Scene, WindowKey};

const ALL: IRect = IRect::new(0, 0, 800, 600);

fn painted(scene: &Scene) -> Vec<NodeKey> {
    let mut out = Vec::new();
    scene.paint_list(OUT, &ALL, &mut out);
    out.into_iter().map(|i| i.node).collect()
}

/// An ordinary window of `CLIENT` and an overlay of `OTHER` on top of it,
/// both covering the point (50, 50). Returns the two windows and a
/// painted rect in each.
fn two_clients(s: &mut Scene) -> (WindowKey, NodeKey, WindowKey, NodeKey) {
    let (app, app_root) = window(s);
    let app_rect = rect(s, app_root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let lock = s.create_window(OTHER, "lock", Size::new(100.0, 100.0), Layer::Overlay);
    s.place_window(lock, Some(OUT), Point::ZERO).unwrap();
    let lock_root = s.window_info(lock).unwrap().root();
    let lock_rect = s
        .create_node(OTHER, nitro_scene::NodeKind::Rect, lock_root, None)
        .unwrap();
    s.set_bounds(OTHER, lock_rect, Rect::new(0.0, 0.0, 50.0, 50.0))
        .unwrap();
    s.set_fill(
        OTHER,
        lock_rect,
        nitro_scene::Fill::Solid(nitro_core::Color::WHITE),
    )
    .unwrap();
    settle(s);
    (app, app_rect, lock, lock_rect)
}

#[test]
fn all_is_the_default_and_paints_everyone() {
    let mut s = scene();
    let (_, app_rect, _, lock_rect) = two_clients(&mut s);
    assert_eq!(s.admit(), Admit::All);
    assert_eq!(painted(&s), vec![app_rect, lock_rect]);
}

#[test]
fn only_one_client_paints_and_hits_only_that_client() {
    let mut s = scene();
    let (app, app_rect, lock, lock_rect) = two_clients(&mut s);
    s.set_admit(Admit::Only(OTHER));
    settle(&mut s);
    assert_eq!(painted(&s), vec![lock_rect]);
    assert!(s.admits_window(lock));
    assert!(!s.admits_window(app));

    // The lock rect covers (0,0)-(50,50); outside it, where only the app
    // window is, nothing is hit rather than the app.
    assert_eq!(
        s.hit_test(OUT, Point::new(10.0, 10.0)).map(|h| h.node),
        Some(lock_rect)
    );
    assert!(s.hit_test(OUT, Point::new(80.0, 80.0)).is_none());

    s.set_admit(Admit::Only(CLIENT));
    settle(&mut s);
    assert_eq!(painted(&s), vec![app_rect]);
}

#[test]
fn nobody_paints_and_hits_nothing() {
    let mut s = scene();
    let _ = two_clients(&mut s);
    s.set_admit(Admit::Nobody);
    settle(&mut s);
    assert!(painted(&s).is_empty());
    assert!(s.hit_test(OUT, Point::new(10.0, 10.0)).is_none());
}

#[test]
fn a_change_damages_every_output_whole_and_a_repeat_nothing() {
    let mut s = scene();
    let _ = two_clients(&mut s);
    s.set_admit(Admit::Nobody);
    assert_eq!(damage_bounds(&mut s), ALL);
    s.set_admit(Admit::Nobody);
    assert!(damage_bounds(&mut s).is_empty(), "the same value is free");
    s.set_admit(Admit::All);
    assert_eq!(damage_bounds(&mut s), ALL);
}

#[test]
fn a_window_created_while_filtered_follows_the_filter() {
    let mut s = scene();
    s.set_admit(Admit::Only(OTHER));
    settle(&mut s);
    let (app, root) = window(&mut s);
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    settle(&mut s);
    assert!(!s.admits_window(app));
    assert!(!painted(&s).contains(&r));
    // Its own state is untouched: admitting everyone shows it as it is.
    s.set_admit(Admit::All);
    settle(&mut s);
    assert!(painted(&s).contains(&r));
}
