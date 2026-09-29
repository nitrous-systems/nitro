//! Holes for Surfaces on an underlay plane (#3898): the shadow holds
//! premultiplied ARGB, an on-plane Surface clears its rect to alpha 0, and
//! everything above it composites onto the hole.
//!
//! Driven through `Scene` directly: the wire still rejects Surface nodes,
//! and plane assignment (#3899) is what will set the flag in the server.

#![allow(clippy::many_single_char_names)]

use nitro_core::{Color, Damage, IRect, Palette, Point, Rect, Size};
use nitro_kms::{Backend, FakeBackend, FakeOutputSpec, Image};
use nitro_scene::{
    ClientId, DamageSink, Fill, Layer, NodeKey, NodeKind, OutputId, PaintItem, Scene,
};
use nitro_server::cursor::{Cursor, Shape};
use nitro_server::frame::{
    self, CursorState, HOLE_PLACEHOLDER, OutputState, fill_holes, paint_region,
};
use nitro_server::icons::IconEngine;
use nitro_server::text::TextEngine;

const OUT: OutputId = OutputId(0);
const CLIENT: ClientId = ClientId(1);
const W: u32 = 120;
const H: u32 = 80;

struct World {
    scene: Scene,
    root: NodeKey,
}

impl World {
    fn new() -> Self {
        let mut scene = Scene::new();
        scene.add_output(OUT, IRect::new(0, 0, W.cast_signed(), H.cast_signed()), 1.0);
        let win = scene.create_window(CLIENT, "w", Size::new(100.0, 60.0), Layer::Normal);
        scene
            .place_window(win, Some(OUT), Point::new(10.0, 10.0))
            .unwrap();
        let root = scene.window_info(win).unwrap().root();
        Self { scene, root }
    }

    fn rect(&mut self, bounds: Rect, color: Color) -> NodeKey {
        let key = self
            .scene
            .create_node(CLIENT, NodeKind::Rect, self.root, None)
            .unwrap();
        self.scene.set_bounds(CLIENT, key, bounds).unwrap();
        self.scene
            .set_fill(CLIENT, key, Fill::Solid(color))
            .unwrap();
        key
    }

    fn surface(&mut self, bounds: Rect) -> NodeKey {
        let key = self
            .scene
            .create_node(CLIENT, NodeKind::Surface, self.root, None)
            .unwrap();
        self.scene.set_bounds(CLIENT, key, bounds).unwrap();
        self.scene.set_surface_on_plane(key, true).unwrap();
        key
    }

    fn update(&mut self) {
        let mut d = Damage::new();
        self.scene
            .update(&mut DamageSink::new(&mut [(OUT, &mut d)]));
    }

    /// Paint the whole output into a fresh buffer, cursor optional.
    fn paint(&mut self, cursor_at: Option<(i32, i32)>) -> Vec<u8> {
        self.update();
        let stride = W * 4;
        let mut data = vec![0u8; (stride * H) as usize];
        let mut canvas = nitro_raster::Canvas::new(&mut data, W, H, stride);
        let (x, y) = cursor_at.unwrap_or((0, 0));
        let state = CursorState {
            x,
            y,
            shape: Shape::Arrow,
            scale: 1,
            visible: cursor_at.is_some(),
        };
        let mut items: Vec<PaintItem> = Vec::new();
        paint_region(
            &mut canvas,
            &self.scene,
            &mut TextEngine::new(),
            &mut IconEngine::new(),
            OUT,
            &[IRect::new(0, 0, W.cast_signed(), H.cast_signed())],
            (&Cursor::new(), state),
            &mut items,
            &Palette::light(),
        );
        data
    }
}

fn px(data: &[u8], x: u32, y: u32) -> [u8; 4] {
    let o = ((y * W + x) * 4) as usize;
    data[o..o + 4].try_into().unwrap()
}

/// Device rect of a Surface at window-local (20, 15) 40x30: the window is
/// at (10, 10).
const HOLE: IRect = IRect::new(30, 25, 40, 30);

#[test]
fn a_hole_is_alpha_zero_and_everything_else_opaque() {
    let mut world = World::new();
    world.rect(Rect::new(0.0, 0.0, 100.0, 60.0), Color::rgb(200, 10, 10));
    world.surface(Rect::new(20.0, 15.0, 40.0, 30.0));
    let data = world.paint(None);
    for y in 0..H {
        for x in 0..W {
            let p = px(&data, x, y);
            if HOLE.contains(x.cast_signed(), y.cast_signed()) {
                assert_eq!(p, [0, 0, 0, 0], "({x},{y}) inside the hole");
            } else {
                assert_eq!(p[3], 255, "({x},{y}) outside the hole");
            }
        }
    }
    assert!(world.scene.has_holes(OUT));
}

#[test]
fn translucent_content_and_the_cursor_over_a_hole_are_premultiplied() {
    let mut world = World::new();
    world.surface(Rect::new(20.0, 15.0, 40.0, 30.0));
    // 50 % white over the hole's left half.
    world.rect(
        Rect::new(20.0, 15.0, 20.0, 30.0),
        Color::rgba(255, 255, 255, 128),
    );
    let data = world.paint(Some((55, 30)));
    let p = px(&data, 32, 40);
    assert_eq!(
        p,
        [128, 128, 128, 128],
        "50 % white on a hole, premultiplied"
    );
    // The arrow's tip is opaque black: an opaque pixel inside the hole.
    assert_eq!(px(&data, 55, 30), [0, 0, 0, 255]);
    // Every pixel is a valid premultiplied value.
    for c in data.chunks_exact(4) {
        assert!(c[..3].iter().all(|&v| v <= c[3]), "{c:?}");
    }
}

#[test]
fn an_opaque_node_above_the_hole_covers_it() {
    let mut world = World::new();
    world.surface(Rect::new(20.0, 15.0, 40.0, 30.0));
    world.rect(Rect::new(25.0, 20.0, 10.0, 10.0), Color::rgb(1, 2, 3));
    let data = world.paint(None);
    assert_eq!(px(&data, 36, 31), [3, 2, 1, 255]);
    assert_eq!(px(&data, 50, 40), [0, 0, 0, 0]);
}

#[test]
fn fill_holes_makes_a_shot_opaque_with_the_placeholder() {
    let mut world = World::new();
    world.rect(Rect::new(0.0, 0.0, 100.0, 60.0), Color::rgb(200, 10, 10));
    world.surface(Rect::new(20.0, 15.0, 40.0, 30.0));
    world.rect(
        Rect::new(20.0, 15.0, 20.0, 30.0),
        Color::rgba(255, 255, 255, 128),
    );
    let data = world.paint(None);
    let mut img = Image {
        width: W,
        height: H,
        stride: W * 4,
        data: data.clone(),
    };
    assert!(fill_holes(&mut img, |_, _| HOLE_PLACEHOLDER));
    assert!(img.data.chunks_exact(4).all(|p| p[3] == 255));
    let [b, g, r] = HOLE_PLACEHOLDER;
    assert_eq!(img.pixel(50, 40), u32::from_le_bytes([b, g, r, 0]));
    // 128 + 0x80 * 127 / 255 = 192.
    assert_eq!(img.pixel(32, 40), 0x00c0_c0c0);
    // Opaque pixels are untouched.
    assert_eq!(
        &img.data[..],
        &{
            let mut d = data;
            for (o, n) in d.chunks_exact_mut(4).zip(img.data.chunks_exact(4)) {
                if o[3] == 255 {
                    assert_eq!(o, n);
                }
                o.copy_from_slice(n);
            }
            d
        }[..]
    );
}

#[test]
fn without_holes_every_pixel_is_opaque_and_fill_holes_is_a_no_op() {
    let mut world = World::new();
    world.rect(Rect::new(0.0, 0.0, 50.0, 30.0), Color::rgba(0, 200, 0, 90));
    let data = world.paint(Some((40, 40)));
    assert!(!world.scene.has_holes(OUT));
    assert!(data.chunks_exact(4).all(|p| p[3] == 255));
    let mut img = Image {
        width: W,
        height: H,
        stride: W * 4,
        data: data.clone(),
    };
    assert!(!fill_holes(&mut img, |_, _| HOLE_PLACEHOLDER));
    assert_eq!(img.data, data);
}

#[test]
fn scanout_alpha_follows_holes_and_capability() {
    for capable in [true, false] {
        let mut backend = FakeBackend::new(&[FakeOutputSpec::new(W, H).alpha(capable)]).unwrap();
        let id = backend.outputs()[0].id;
        let mut out = OutputState::new(id, OUT, W, H, 60_000, true);
        for holes in [false, false, true, true, false, false, true] {
            frame::select_scanout_alpha(&mut backend, &mut out, holes);
            assert_eq!(out.alpha, holes && capable);
            assert_eq!(backend.scanout_alpha_on(id), holes && capable);
        }
        // Only the transitions reached the backend, and never on a plane
        // that cannot blend.
        let want = if capable { 3 } else { 0 };
        assert_eq!(backend.scanout_alpha_sets(id), want, "capable {capable}");
        assert_eq!(out.alpha_warned.get(), !capable);
    }
}
