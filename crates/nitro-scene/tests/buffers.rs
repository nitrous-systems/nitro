//! Buffers: ownership of the pixels, damage propagation to image nodes, and
//! what happens when a buffer goes away underneath them.

mod common;

use common::{CLIENT, OTHER, OUT, damage, group, rect, scene, settle, update, window};
use nitro_core::{IRect, Rect, Size, Transform};
use nitro_scene::{
    BufferDesc, BufferKey, ClientId, Error, ImageRef, Layer, NodeKey, NodeKind, PaintItem,
    PaintKind, Scene,
};

/// A 64x64 BGRX buffer, 4 bytes per pixel.
fn desc() -> BufferDesc {
    BufferDesc::new(64, 64, 64 * 4, 0x3458_5242)
}

#[test]
fn a_buffer_owns_a_copy_of_the_bytes() {
    let mut s = scene();
    let d = desc();
    let key = s
        .create_buffer(CLIENT, d, vec![0xAB; d.byte_len()])
        .unwrap();

    assert_eq!(s.buffer_count(), 1);
    assert_eq!(s.buffer(key).unwrap().desc(), d);
    assert_eq!(s.buffer(key).unwrap().client(), CLIENT);
    assert_eq!(s.buffer(key).unwrap().data().len(), d.byte_len());
    assert!(s.buffer(key).unwrap().data().iter().all(|b| *b == 0xAB));

    // Writing through `buffer_mut` is visible.
    s.buffer_mut(CLIENT, key).unwrap()[0] = 0x01;
    assert_eq!(s.buffer(key).unwrap().data()[0], 0x01);

    // Extra bytes beyond stride * h are accepted and kept.
    let padded = s
        .create_buffer(CLIENT, d, vec![0; d.byte_len() + 16])
        .unwrap();
    assert_eq!(s.buffer(padded).unwrap().data().len(), d.byte_len() + 16);
}

#[test]
fn an_image_node_paints_its_source_region() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(10.0, 10.0, 32.0, 32.0))
        .unwrap();

    // Without a buffer it paints nothing.
    assert!(damage(&mut s).is_empty());
    assert!(!s.node(image).unwrap().painted());

    let src = IRect::new(0, 0, 16, 16);
    s.set_image(CLIENT, image, Some(ImageRef::new(buffer, src)))
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(10, 10, 32, 32)]);

    let mut items = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 800, 600), &mut items);
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].kind,
        PaintKind::Image {
            size: (32.0, 32.0),
            buffer,
            src,
            opaque: false,
        }
    );
    // This buffer's format was not declared opaque, so the scene cannot see
    // through it and refuses to report a cover.
    assert_eq!(items[0].opaque_cover(), None);
}

#[test]
fn buffer_damage_reaches_only_the_images_that_sample_it() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let other = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();

    // Two images on the buffer, sampling disjoint halves.
    let left = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, left, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(
        CLIENT,
        left,
        Some(ImageRef::new(buffer, IRect::new(0, 0, 32, 64))),
    )
    .unwrap();

    let right = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, right, Rect::new(100.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(
        CLIENT,
        right,
        Some(ImageRef::new(buffer, IRect::new(32, 0, 32, 64))),
    )
    .unwrap();

    // An image on a different buffer entirely.
    let stranger = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, stranger, Rect::new(200.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, stranger, Some(ImageRef::new(other, d.full_rect())))
        .unwrap();
    settle(&mut s);

    // Damage in the left half touches only the left image.
    s.buffer_mut(CLIENT, buffer).unwrap()[0] = 1;
    s.buffer_damaged(CLIENT, buffer, &[IRect::new(0, 0, 8, 8)])
        .unwrap();
    let d1 = damage(&mut s);
    assert_eq!(d1.rects(), &[IRect::new(0, 0, 32, 32)]);

    // Damage spanning both halves touches both.
    s.buffer_damaged(CLIENT, buffer, &[IRect::new(0, 0, 64, 4)])
        .unwrap();
    let d2 = damage(&mut s);
    assert!(d2.intersects(&IRect::new(0, 0, 32, 32)));
    assert!(d2.intersects(&IRect::new(100, 0, 32, 32)));
    assert!(!d2.intersects(&IRect::new(200, 0, 32, 32)), "other buffer");

    // Damage that misses every source rect costs nothing.
    let (d3, stats) = update(&mut s);
    assert!(d3.is_empty());
    assert_eq!(stats.visited_nodes, 0);

    // The stranger answers to its own buffer, and only to that.
    s.buffer_damaged(CLIENT, other, &[IRect::new(0, 0, 1, 1)])
        .unwrap();
    let d4 = damage(&mut s);
    assert_eq!(d4.rects(), &[IRect::new(200, 0, 32, 32)]);
    assert_eq!(
        s.node(stranger).unwrap().world_bounds(),
        IRect::new(200, 0, 32, 32)
    );
}

#[test]
fn buffer_damage_outside_every_source_rect_is_free() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(
        CLIENT,
        image,
        Some(ImageRef::new(buffer, IRect::new(0, 0, 16, 16))),
    )
    .unwrap();
    settle(&mut s);

    // The bottom-right corner is not sampled by anyone.
    s.buffer_damaged(CLIENT, buffer, &[IRect::new(32, 32, 16, 16)])
        .unwrap();
    let (dmg, stats) = update(&mut s);
    assert!(dmg.is_empty());
    assert_eq!(stats.visited_nodes, 0);

    // An empty damage list likewise.
    s.buffer_damaged(CLIENT, buffer, &[]).unwrap();
    assert!(damage(&mut s).is_empty());
}

#[test]
fn destroying_a_buffer_clears_the_images_using_it() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let a = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, a, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, a, Some(ImageRef::new(buffer, d.full_rect())))
        .unwrap();
    let b = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, b, Rect::new(100.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, b, Some(ImageRef::new(buffer, d.full_rect())))
        .unwrap();
    settle(&mut s);
    assert!(s.node(a).unwrap().painted());

    s.destroy_buffer(CLIENT, buffer).unwrap();
    // Both images went empty, and both places need repainting.
    assert_eq!(s.node(a).unwrap().image(), None);
    assert_eq!(s.node(b).unwrap().image(), None);
    let dmg = damage(&mut s);
    assert!(dmg.intersects(&IRect::new(0, 0, 32, 32)));
    assert!(dmg.intersects(&IRect::new(100, 0, 32, 32)));
    assert!(!s.node(a).unwrap().painted());

    // The nodes survive; they are just empty.
    assert_eq!(s.node(a).unwrap().kind(), NodeKind::Image);
    assert_eq!(s.buffer_count(), 0);
    // And they can be pointed at a new buffer.
    let fresh = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    s.set_image(CLIENT, a, Some(ImageRef::new(fresh, d.full_rect())))
        .unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 32, 32)]);
}

#[test]
fn destroying_image_nodes_does_not_disturb_their_buffer() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let image = s.create_node(CLIENT, NodeKind::Image, g, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, image, Some(ImageRef::new(buffer, d.full_rect())))
        .unwrap();
    settle(&mut s);

    // Destroying the whole subtree leaves the buffer alone...
    s.destroy_node(CLIENT, g).unwrap();
    assert_eq!(s.buffer_count(), 1);
    assert_eq!(s.buffer(buffer).unwrap().data().len(), d.byte_len());

    // ...and damaging it afterwards does not resurrect or panic on the node.
    s.buffer_damaged(CLIENT, buffer, &[d.full_rect()]).unwrap();
    let _ = damage(&mut s);
    assert_eq!(s.buffer_count(), 1);
}

#[test]
fn repointing_an_image_detaches_it_from_the_old_buffer() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let first = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let second = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, image, Some(ImageRef::new(first, d.full_rect())))
        .unwrap();
    settle(&mut s);

    s.set_image(CLIENT, image, Some(ImageRef::new(second, d.full_rect())))
        .unwrap();
    settle(&mut s);

    // The old buffer no longer reaches it.
    s.buffer_damaged(CLIENT, first, &[d.full_rect()]).unwrap();
    let (dmg, stats) = update(&mut s);
    assert!(dmg.is_empty());
    assert_eq!(stats.visited_nodes, 0);

    // The new one does.
    s.buffer_damaged(CLIENT, second, &[d.full_rect()]).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 32, 32)]);

    // Destroying the old buffer does not clear the image.
    s.destroy_buffer(CLIENT, first).unwrap();
    assert_eq!(s.node(image).unwrap().image().unwrap().buffer, second);

    // Clearing the image by hand damages its bounds.
    settle(&mut s);
    s.set_image(CLIENT, image, None).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 32, 32)]);
    assert_eq!(s.node(image).unwrap().image(), None);
}

#[test]
fn buffers_are_owned_by_their_client() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let mine = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();

    // Another client cannot read, write, damage or destroy it...
    assert_eq!(s.buffer_mut(OTHER, mine).unwrap_err(), Error::NotOwner);
    assert_eq!(
        s.buffer_damaged(OTHER, mine, &[d.full_rect()]).unwrap_err(),
        Error::NotOwner
    );
    assert_eq!(s.destroy_buffer(OTHER, mine).unwrap_err(), Error::NotOwner);
    // ...nor point one of its own nodes at it.
    let theirs = s.create_window(OTHER, "t", Size::new(50.0, 50.0), Layer::Normal);
    let their_root = s.window_info(theirs).unwrap().root();
    let their_image = s
        .create_node(OTHER, NodeKind::Image, their_root, None)
        .unwrap();
    assert_eq!(
        s.set_image(OTHER, their_image, Some(ImageRef::new(mine, d.full_rect())))
            .unwrap_err(),
        Error::NotOwner
    );

    // The server may do all of it.
    s.buffer_mut(ClientId::SERVER, mine).unwrap();
    s.buffer_damaged(ClientId::SERVER, mine, &[d.full_rect()])
        .unwrap();
    s.set_image(
        ClientId::SERVER,
        image,
        Some(ImageRef::new(mine, d.full_rect())),
    )
    .unwrap();
    s.destroy_buffer(ClientId::SERVER, mine).unwrap();
    assert_eq!(s.buffer_count(), 0);
}

#[test]
fn an_image_outside_its_clip_is_still_marked_by_buffer_damage() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 50.0, 50.0));
    s.set_clip(CLIENT, g, true).unwrap();
    let image = s.create_node(CLIENT, NodeKind::Image, g, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 200.0, 200.0))
        .unwrap();
    s.set_image(CLIENT, image, Some(ImageRef::new(buffer, d.full_rect())))
        .unwrap();
    settle(&mut s);

    // The damage is bounded by the clip, not by the node's own bounds.
    s.buffer_damaged(CLIENT, buffer, &[d.full_rect()]).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 50, 50)]);
}

#[test]
fn images_track_their_window_across_a_reparent() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let here = group(&mut s, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    let there = group(&mut s, root, Rect::new(300.0, 0.0, 200.0, 200.0));
    let image = s.create_node(CLIENT, NodeKind::Image, here, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, image, Some(ImageRef::new(buffer, d.full_rect())))
        .unwrap();
    settle(&mut s);

    s.reparent(CLIENT, image, there, None).unwrap();
    settle(&mut s);
    assert_eq!(
        s.node(image).unwrap().world_bounds(),
        IRect::new(300, 0, 32, 32)
    );

    // Buffer damage now lands in the new place.
    s.buffer_damaged(CLIENT, buffer, &[d.full_rect()]).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(300, 0, 32, 32)]);
}

#[test]
fn a_hidden_image_costs_nothing_when_its_buffer_changes() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, image, Rect::new(0.0, 0.0, 32.0, 32.0))
        .unwrap();
    s.set_image(CLIENT, image, Some(ImageRef::new(buffer, d.full_rect())))
        .unwrap();
    s.set_visible(CLIENT, image, false).unwrap();
    settle(&mut s);

    s.buffer_damaged(CLIENT, buffer, &[d.full_rect()]).unwrap();
    assert!(damage(&mut s).is_empty());

    // Revealing it paints the fresh contents.
    s.set_visible(CLIENT, image, true).unwrap();
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 0, 32, 32)]);
}

#[test]
fn the_scene_survives_a_buffer_used_by_many_images() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let mut images = Vec::new();
    for i in 0..50u16 {
        let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
        s.set_bounds(CLIENT, image, Rect::new(f32::from(i) * 2.0, 0.0, 2.0, 2.0))
            .unwrap();
        s.set_image(CLIENT, image, Some(ImageRef::new(buffer, d.full_rect())))
            .unwrap();
        images.push(image);
    }
    settle(&mut s);

    // Destroy half of them, then damage the buffer: no panic, no stale hits.
    for image in images.iter().take(25) {
        s.destroy_node(CLIENT, *image).unwrap();
    }
    settle(&mut s);
    s.buffer_damaged(CLIENT, buffer, &[d.full_rect()]).unwrap();
    let (dmg, stats) = update(&mut s);
    assert_eq!(dmg.bounds(), IRect::from_edges(50, 0, 100, 2));
    assert_eq!(stats.damaged_nodes, 25);

    // And destroying the buffer clears exactly the survivors.
    s.destroy_buffer(CLIENT, buffer).unwrap();
    for image in images.iter().skip(25) {
        assert_eq!(s.node(*image).unwrap().image(), None);
    }
    settle(&mut s);
    // A window-level sanity check: nothing else moved.
    let stray = rect(&mut s, root, Rect::new(0.0, 100.0, 5.0, 5.0));
    assert_eq!(damage(&mut s).rects(), &[IRect::new(0, 100, 5, 5)]);
    assert_eq!(
        s.node(stray).unwrap().world_bounds(),
        IRect::new(0, 100, 5, 5)
    );
}

/// The same 64x64 geometry, but declared opaque — an `XR24`-style buffer.
fn opaque_desc() -> BufferDesc {
    desc().with_opaque(true)
}

/// An image node on `buffer` covering `bounds`, sampling `src`.
fn image_node(s: &mut Scene, root: NodeKey, b: BufferKey, bounds: Rect, src: IRect) -> NodeKey {
    let node = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    s.set_bounds(CLIENT, node, bounds).unwrap();
    s.set_image(CLIENT, node, Some(ImageRef::new(b, src)))
        .unwrap();
    node
}

#[test]
fn an_opaque_pixel_aligned_one_to_one_image_is_an_occluder() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = opaque_desc();
    let buffer = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    let node = image_node(
        &mut s,
        root,
        buffer,
        Rect::new(10.0, 20.0, 64.0, 64.0),
        d.full_rect(),
    );
    settle(&mut s);

    let mut items = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 800, 600), &mut items);
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].kind,
        PaintKind::Image {
            size: (64.0, 64.0),
            buffer,
            src: d.full_rect(),
            opaque: true,
        }
    );
    assert_eq!(items[0].opaque_cover(), Some(IRect::new(10, 20, 64, 64)));

    // Below full opacity it stops qualifying, like every other kind.
    s.set_opacity(CLIENT, node, 0.5).unwrap();
    settle(&mut s);
    assert_eq!(paint_one(&s).opaque_cover(), None);
}

fn paint_one(s: &Scene) -> PaintItem {
    let mut items = Vec::new();
    s.paint_list(OUT, &IRect::new(0, 0, 800, 600), &mut items);
    assert_eq!(items.len(), 1);
    items[0]
}

#[test]
fn an_image_that_is_not_provably_opaque_reports_no_cover() {
    // Every geometric negative in one table: an alpha-carrying format, a
    // scaled image, a source rect smaller than its destination, a
    // fractionally placed one and a fractionally sized one. Rotation needs a
    // transform rather than bounds, so it gets its own test below.
    let alpha = desc(); // not declared opaque
    let opaque = opaque_desc();
    let full = opaque.full_rect();
    let cases: [(&str, BufferDesc, Rect, IRect); 5] = [
        ("alpha format", alpha, Rect::new(0.0, 0.0, 64.0, 64.0), full),
        ("scaled 2x", opaque, Rect::new(0.0, 0.0, 128.0, 128.0), full),
        (
            "src smaller than dst",
            opaque,
            Rect::new(0.0, 0.0, 64.0, 64.0),
            IRect::new(0, 0, 32, 32),
        ),
        (
            "fractional x",
            opaque,
            Rect::new(0.5, 0.0, 64.0, 64.0),
            full,
        ),
        (
            "fractional width",
            opaque,
            Rect::new(0.0, 0.0, 64.5, 64.0),
            full,
        ),
    ];

    for (what, d, bounds, src) in cases {
        let mut s = scene();
        let (_, root) = window(&mut s);
        let b = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
        image_node(&mut s, root, b, bounds, src);
        settle(&mut s);
        assert_eq!(paint_one(&s).opaque_cover(), None, "{what}");
    }
}

#[test]
fn a_rotated_opaque_image_reports_no_cover() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let desc = opaque_desc();
    let buffer = s
        .create_buffer(CLIENT, desc, vec![0; desc.byte_len()])
        .unwrap();
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 200.0, 200.0));
    image_node(
        &mut s,
        g,
        buffer,
        Rect::new(0.0, 0.0, 64.0, 64.0),
        desc.full_rect(),
    );
    settle(&mut s);
    assert!(paint_one(&s).opaque_cover().is_some(), "upright");

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
    assert_eq!(paint_one(&s).opaque_cover(), None, "rotated");
}

#[test]
fn a_format_the_server_would_reject_is_never_an_occluder() {
    // The hazard: two places decide "this image paints opaque pixels" — the
    // scene's flag and the server's `pixel_format` lookup. If the flag says
    // yes for a fourcc the painter cannot map, `paint_region` skips the
    // background *and* every item beneath, and the painter then draws
    // nothing: a hole. `BufferDesc::new` defaults the flag to false, so a
    // desc built without `with_opaque` — which is every desc the server does
    // not vouch for — cannot trigger it.
    let mut s = scene();
    let (_, root) = window(&mut s);
    let d = BufferDesc::new(64, 64, 64 * 4, 0);
    assert!(!d.is_opaque(), "the conservative default");
    let b = s.create_buffer(CLIENT, d, vec![0; d.byte_len()]).unwrap();
    image_node(
        &mut s,
        root,
        b,
        Rect::new(0.0, 0.0, 64.0, 64.0),
        d.full_rect(),
    );
    settle(&mut s);
    assert_eq!(paint_one(&s).opaque_cover(), None);
}
