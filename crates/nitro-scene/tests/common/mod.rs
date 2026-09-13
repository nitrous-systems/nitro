//! Shared fixture for the integration tests.
//!
//! Every test builds a scene through the same helpers so the coordinate
//! conventions (output origin, window position, scale) are stated in one
//! place and a failure points at behaviour, not at setup.

#![allow(dead_code)]

use nitro_core::{Color, Damage, IRect, Point, Rect, Size};
use nitro_scene::{
    ClientId, DamageSink, Fill, Layer, NodeKey, NodeKind, OutputId, Scene, TextAlign, TextRef,
    UpdateStats, WindowKey,
};

pub const OUT: OutputId = OutputId(0);
pub const OUT2: OutputId = OutputId(1);
pub const CLIENT: ClientId = ClientId(1);
pub const OTHER: ClientId = ClientId(2);

/// A scene with one 800x600 output at the origin, scale 1.
pub fn scene() -> Scene {
    let mut scene = Scene::new();
    scene.add_output(OUT, IRect::new(0, 0, 800, 600), 1.0);
    scene
}

/// A window placed at `pos` on `OUT`, returning it and its root node.
pub fn window_at(scene: &mut Scene, pos: Point, size: Size) -> (WindowKey, NodeKey) {
    let win = scene.create_window(CLIENT, "w", size, Layer::Normal);
    scene.place_window(win, Some(OUT), pos).unwrap();
    let root = scene.window_info(win).unwrap().root();
    (win, root)
}

/// A window at the origin, 400x300.
pub fn window(scene: &mut Scene) -> (WindowKey, NodeKey) {
    window_at(scene, Point::ZERO, Size::new(400.0, 300.0))
}

/// A solid white rect child of `parent` with the given local bounds.
pub fn rect(scene: &mut Scene, parent: NodeKey, bounds: Rect) -> NodeKey {
    rect_colored(scene, parent, bounds, Color::WHITE)
}

pub fn rect_colored(scene: &mut Scene, parent: NodeKey, bounds: Rect, color: Color) -> NodeKey {
    let key = scene
        .create_node(CLIENT, NodeKind::Rect, parent, None)
        .unwrap();
    scene.set_bounds(CLIENT, key, bounds).unwrap();
    scene.set_fill(CLIENT, key, Fill::Solid(color)).unwrap();
    key
}

pub fn group(scene: &mut Scene, parent: NodeKey, bounds: Rect) -> NodeKey {
    let key = scene
        .create_node(CLIENT, NodeKind::Group, parent, None)
        .unwrap();
    scene.set_bounds(CLIENT, key, bounds).unwrap();
    key
}

/// Run an update, returning the damage rects of `OUT` merged into a region.
pub fn damage(scene: &mut Scene) -> Damage {
    let mut d = Damage::new();
    scene.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    d
}

/// Run an update and return only the bounding box of `OUT`'s damage.
pub fn damage_bounds(scene: &mut Scene) -> IRect {
    damage(scene).bounds()
}

/// Run an update, discarding the damage: brings the scene to a clean state.
pub fn settle(scene: &mut Scene) {
    let _ = damage(scene);
}

/// Run an update and return its stats together with `OUT`'s damage.
pub fn update(scene: &mut Scene) -> (Damage, UpdateStats) {
    let mut d = Damage::new();
    let result = scene.update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    (d, result.stats)
}

/// A text node child of `parent`: `bounds` is the box, `block` the measured
/// size of the shaped run the (fake) store holds under `key`.
pub fn text(
    scene: &mut Scene,
    parent: NodeKey,
    bounds: Rect,
    key: u32,
    block: Size,
    align: TextAlign,
) -> NodeKey {
    let node = scene
        .create_node(CLIENT, NodeKind::Text, parent, None)
        .unwrap();
    scene.set_bounds(CLIENT, node, bounds).unwrap();
    scene
        .set_text(
            CLIENT,
            node,
            Some(TextRef {
                key,
                size: block,
                ascent: block.h * 0.8,
                color: Color::WHITE,
                align,
            }),
        )
        .unwrap();
    node
}
