//! Surface nodes with content: attach/detach, the buffer bookkeeping they
//! share with images, and the latch entry point's damage (#3897).

mod common;

use common::{CLIENT, OUT, damage, settle, window};
use nitro_core::{IRect, Rect};
use nitro_scene::{
    BufferDesc, BufferKey, ClientId, ColorMatrix, ColorRange, Error, ImageRef, NodeKey, NodeKind,
    PaintKind, Scene, SurfaceColor, SurfaceRef,
};

const NV12: u32 = u32::from_le_bytes(*b"NV12");

/// A 64x64 NV12 buffer: luma plane, then chroma at half height.
fn nv12_desc() -> BufferDesc {
    BufferDesc::new(64, 64, 64, NV12)
        .with_planes(0, Some((64 * 64, 64, 32)))
        .with_opaque(true)
}

fn nv12(s: &mut Scene) -> BufferKey {
    let d = nv12_desc();
    s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap()
}

fn color() -> SurfaceColor {
    SurfaceColor {
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
    }
}

fn full() -> IRect {
    IRect::new(0, 0, 64, 64)
}

/// A 64x64 surface node at (10, 10).
fn surface(s: &mut Scene) -> NodeKey {
    let (_, root) = window(s);
    let n = s
        .create_node(CLIENT, NodeKind::Surface, root, None)
        .unwrap();
    s.set_bounds(CLIENT, n, Rect::new(10.0, 10.0, 64.0, 64.0))
        .unwrap();
    n
}

fn released(s: &mut Scene) -> Vec<(ClientId, BufferKey)> {
    let mut out = Vec::new();
    s.take_released_buffers(&mut out);
    out
}

#[test]
fn byte_len_covers_both_planes() {
    let d = nv12_desc();
    // Chroma ends at 4096 + 64 * 31 + 1; the loose bound ignores the row
    // width, the server checks the exact one.
    assert_eq!(d.byte_len(), 64 * 64 + 64 * 31 + 1);
    // Too short a store is refused.
    let mut s = common::scene();
    assert_eq!(
        s.create_buffer(CLIENT, d, vec![0; 64 * 64]).unwrap_err(),
        Error::BadBuffer
    );
}

#[test]
fn attach_paints_and_detach_clears() {
    let mut s = common::scene();
    let n = surface(&mut s);
    // Empty surface: nothing painted.
    assert!(damage(&mut s).is_empty());
    let b = nv12(&mut s);
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(b, full(), color())))
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);

    let mut items = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 800, 600), &mut items);
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].kind,
        PaintKind::Surface {
            size: (64.0, 64.0),
            buffer: b,
            src: full(),
            color: color(),
            opaque: true,
        }
    );
    // Opaque, pixel-aligned and 1:1: it covers its rect.
    assert_eq!(items[0].opaque_cover(), Some(IRect::new(10, 10, 64, 64)));
    // Hit-testing sees it like an image.
    let hit = s.hit_test(OUT, nitro_core::Point::new(20.0, 20.0)).unwrap();
    assert_eq!(hit.node, n);

    s.set_surface(CLIENT, n, None).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);
    assert_eq!(released(&mut s), vec![(CLIENT, b)]);
}

#[test]
fn set_surface_on_an_image_is_wrong_kind_and_vice_versa() {
    let mut s = common::scene();
    let (_, root) = window(&mut s);
    let img = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    let b = nv12(&mut s);
    assert_eq!(
        s.set_surface(CLIENT, img, Some(SurfaceRef::new(b, full(), color())))
            .unwrap_err(),
        Error::WrongKind
    );
    let n = surface(&mut s);
    assert_eq!(
        s.set_image(CLIENT, n, Some(ImageRef::new(b, full())))
            .unwrap_err(),
        Error::WrongKind
    );
    // A src outside the buffer is refused.
    assert_eq!(
        s.set_surface(
            CLIENT,
            n,
            Some(SurfaceRef::new(b, IRect::new(0, 0, 65, 64), color()))
        )
        .unwrap_err(),
        Error::BadBuffer
    );
}

#[test]
fn a_swap_releases_the_old_buffer_and_destroy_releases_the_current() {
    let mut s = common::scene();
    let n = surface(&mut s);
    let a = nv12(&mut s);
    let b = nv12(&mut s);
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(a, full(), color())))
        .unwrap();
    settle(&mut s);
    assert!(released(&mut s).is_empty());
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(b, full(), color())))
        .unwrap();
    assert_eq!(released(&mut s), vec![(CLIENT, a)]);
    s.destroy_node(CLIENT, n).unwrap();
    assert_eq!(released(&mut s), vec![(CLIENT, b)]);
}

#[test]
fn destroying_the_buffer_empties_the_surface() {
    let mut s = common::scene();
    let n = surface(&mut s);
    let a = nv12(&mut s);
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(a, full(), color())))
        .unwrap();
    settle(&mut s);
    s.destroy_buffer(CLIENT, a).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);
    assert_eq!(s.node(n).unwrap().surface().unwrap().content, None);
    assert!(released(&mut s).is_empty());
}

#[test]
fn buffer_damage_reaches_a_surface_partially() {
    let mut s = common::scene();
    let n = surface(&mut s);
    let a = nv12(&mut s);
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(a, full(), color())))
        .unwrap();
    settle(&mut s);
    s.buffer_damaged(CLIENT, a, &[IRect::new(4, 4, 8, 8)])
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(14, 14, 8, 8)]);
}

#[test]
fn a_committed_swap_honours_buffer_damage() {
    let mut s = common::scene();
    let n = surface(&mut s);
    let a = nv12(&mut s);
    let b = nv12(&mut s);
    // Show both once so the swap rule applies.
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(b, full(), color())))
        .unwrap();
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(a, full(), color())))
        .unwrap();
    settle(&mut s);
    s.buffer_damaged(CLIENT, b, &[IRect::new(0, 0, 4, 4)])
        .unwrap();
    s.set_surface(CLIENT, n, Some(SurfaceRef::new(b, full(), color())))
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 4, 4)]);
}

#[test]
fn the_latch_takes_its_damage_from_the_frame() {
    let mut s = common::scene();
    let n = surface(&mut s);
    let a = nv12(&mut s);
    let b = nv12(&mut s);
    // First latch of a never-shown buffer: the whole node.
    s.set_surface_with_damage(
        CLIENT,
        n,
        SurfaceRef::new(a, full(), color()),
        &[IRect::new(0, 0, 1, 1)],
    )
    .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);
    // b never shown: whole node again.
    s.set_surface_with_damage(
        CLIENT,
        n,
        SurfaceRef::new(b, full(), color()),
        &[IRect::new(0, 0, 1, 1)],
    )
    .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);
    // Back to a (shown before): just the frame's damage.
    s.set_surface_with_damage(
        CLIENT,
        n,
        SurfaceRef::new(a, full(), color()),
        &[IRect::new(8, 8, 2, 2)],
    )
    .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(18, 18, 2, 2)]);
    // Re-presenting the current buffer with damage.
    s.set_surface_with_damage(
        CLIENT,
        n,
        SurfaceRef::new(a, full(), color()),
        &[IRect::new(1, 2, 3, 4)],
    )
    .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(11, 12, 3, 4)]);
    // Empty damage is the whole node.
    s.set_surface_with_damage(CLIENT, n, SurfaceRef::new(b, full(), color()), &[])
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);
    // A colour change repaints the whole node even under the swap rule.
    let other = SurfaceColor {
        matrix: ColorMatrix::Bt601,
        range: ColorRange::Full,
    };
    s.set_surface_with_damage(
        CLIENT,
        n,
        SurfaceRef::new(a, full(), other),
        &[IRect::new(0, 0, 1, 1)],
    )
    .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 64, 64)]);
}
