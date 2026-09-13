//! Tree shape: create, destroy, reparent, ordering — and every way the
//! mutation API can be misused.

mod common;

use common::{CLIENT, OTHER, OUT, group, rect, scene, settle, window};
use nitro_core::{Color, IRect, Transform};
use nitro_core::{Point, Rect, Size};
use nitro_scene::{
    BufferDesc, ClientId, Error, Fill, ImageRef, Layer, MAX_DEPTH, NodeKind, OutputId, Scene,
};

#[test]
fn create_builds_a_tree_with_ordered_children() {
    let mut s = scene();
    let (win, root) = window(&mut s);

    let a = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    let b = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    // `before: None` appends, so children are back to front.
    assert_eq!(s.node(root).unwrap().children(), &[a, b]);

    // Insert before `b`: between the two.
    let c = s
        .create_node(CLIENT, NodeKind::Rect, root, Some(b))
        .unwrap();
    assert_eq!(s.node(root).unwrap().children(), &[a, c, b]);

    assert_eq!(s.node(a).unwrap().parent(), Some(root));
    assert_eq!(s.node(root).unwrap().parent(), None);
    assert_eq!(s.node(a).unwrap().window(), win);
    assert_eq!(s.node(a).unwrap().client(), CLIENT);
    assert_eq!(s.node_count(), 4);
    assert_eq!(s.window_count(), 1);
}

#[test]
fn destroy_is_recursive_and_detaches_from_the_parent() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let a = rect(&mut s, g, Rect::new(0.0, 0.0, 10.0, 10.0));
    let b = rect(&mut s, a, Rect::new(0.0, 0.0, 5.0, 5.0));
    let sibling = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    assert_eq!(s.node_count(), 5);

    s.destroy_node(CLIENT, g).unwrap();
    assert_eq!(s.node_count(), 2);
    assert_eq!(s.node(root).unwrap().children(), &[sibling]);
    // Every key in the destroyed subtree is now stale.
    for key in [g, a, b] {
        assert_eq!(s.node(key).unwrap_err(), Error::StaleKey);
    }
}

#[test]
fn destroying_a_window_takes_its_whole_tree_and_z_order_slot() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let child = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    settle(&mut s);

    s.destroy_window(CLIENT, win).unwrap();
    assert_eq!(s.node_count(), 0);
    assert_eq!(s.window_count(), 0);
    assert_eq!(s.node(child).unwrap_err(), Error::StaleKey);
    assert_eq!(s.window_info(win).unwrap_err(), Error::StaleKey);
    assert_eq!(s.windows(OUT).count(), 0);
}

#[test]
fn a_window_root_cannot_be_destroyed_or_reparented() {
    let mut s = scene();
    let (_, root_a) = window(&mut s);
    let (_, root_b) = window(&mut s);
    assert_eq!(s.destroy_node(CLIENT, root_a).unwrap_err(), Error::RootNode);
    assert_eq!(
        s.reparent(CLIENT, root_a, root_b, None).unwrap_err(),
        Error::RootNode
    );
}

#[test]
fn reparent_moves_the_subtree_and_keeps_order() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g1 = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let g2 = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let a = rect(&mut s, g1, Rect::new(0.0, 0.0, 10.0, 10.0));
    let deep = rect(&mut s, a, Rect::new(0.0, 0.0, 5.0, 5.0));
    let first = rect(&mut s, g2, Rect::new(0.0, 0.0, 10.0, 10.0));

    s.reparent(CLIENT, a, g2, Some(first)).unwrap();
    assert_eq!(s.node(g1).unwrap().children(), &[]);
    assert_eq!(s.node(g2).unwrap().children(), &[a, first]);
    assert_eq!(s.node(a).unwrap().parent(), Some(g2));
    // The descendant came along.
    assert_eq!(s.node(deep).unwrap().parent(), Some(a));
}

#[test]
fn reparent_across_windows_rewrites_ownership() {
    let mut s = scene();
    let (w1, r1) = window(&mut s);
    let (w2, r2) = window(&mut s);
    let a = rect(&mut s, r1, Rect::new(0.0, 0.0, 10.0, 10.0));
    let child = rect(&mut s, a, Rect::new(0.0, 0.0, 5.0, 5.0));
    assert_eq!(s.node(child).unwrap().window(), w1);

    s.reparent(CLIENT, a, r2, None).unwrap();
    assert_eq!(s.node(a).unwrap().window(), w2);
    assert_eq!(s.node(child).unwrap().window(), w2);
}

#[test]
fn reparent_rejects_cycles() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let a = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let b = group(&mut s, a, Rect::new(0.0, 0.0, 50.0, 50.0));
    let c = group(&mut s, b, Rect::new(0.0, 0.0, 25.0, 25.0));

    // Into itself.
    assert_eq!(
        s.reparent(CLIENT, a, a, None).unwrap_err(),
        Error::BadParent
    );
    // Into a direct child.
    assert_eq!(
        s.reparent(CLIENT, a, b, None).unwrap_err(),
        Error::BadParent
    );
    // Into a distant descendant.
    assert_eq!(
        s.reparent(CLIENT, a, c, None).unwrap_err(),
        Error::BadParent
    );
    // The tree is untouched by the refusals.
    assert_eq!(s.node(a).unwrap().children(), &[b]);
    assert_eq!(s.node(b).unwrap().children(), &[c]);
}

#[test]
fn stale_keys_are_rejected_everywhere() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let node = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    let buffer = s
        .create_buffer(CLIENT, BufferDesc::new(4, 4, 16, 0), vec![0; 64])
        .unwrap();

    s.destroy_node(CLIENT, node).unwrap();
    s.destroy_buffer(CLIENT, buffer).unwrap();

    assert_eq!(s.node(node).unwrap_err(), Error::StaleKey);
    assert_eq!(s.buffer(buffer).unwrap_err(), Error::StaleKey);
    assert_eq!(
        s.set_bounds(CLIENT, node, Rect::EMPTY).unwrap_err(),
        Error::StaleKey
    );
    assert_eq!(
        s.set_visible(CLIENT, node, false).unwrap_err(),
        Error::StaleKey
    );
    assert_eq!(
        s.set_opacity(CLIENT, node, 0.5).unwrap_err(),
        Error::StaleKey
    );
    assert_eq!(
        s.set_fill(CLIENT, node, Fill::None).unwrap_err(),
        Error::StaleKey
    );
    assert_eq!(s.destroy_node(CLIENT, node).unwrap_err(), Error::StaleKey);
    assert_eq!(
        s.create_node(CLIENT, NodeKind::Rect, node, None)
            .unwrap_err(),
        Error::StaleKey
    );
    assert_eq!(s.buffer_mut(CLIENT, buffer).unwrap_err(), Error::StaleKey);
    assert_eq!(
        s.buffer_damaged(CLIENT, buffer, &[IRect::new(0, 0, 1, 1)])
            .unwrap_err(),
        Error::StaleKey
    );

    // Recycled slots do not resurrect the old key.
    let fresh = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    assert_ne!(fresh, node);
    assert_eq!(s.node(node).unwrap_err(), Error::StaleKey);

    s.destroy_window(CLIENT, win).unwrap();
    assert_eq!(s.destroy_window(CLIENT, win).unwrap_err(), Error::StaleKey);
    assert_eq!(
        s.place_window(win, Some(OUT), Point::ZERO).unwrap_err(),
        Error::StaleKey
    );
}

#[test]
fn kind_mismatches_are_rejected() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let r = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();
    let text = s.create_node(CLIENT, NodeKind::Text, root, None).unwrap();

    // Rect-only properties.
    for key in [g, image, text] {
        assert_eq!(
            s.set_fill(CLIENT, key, Fill::Solid(Color::WHITE))
                .unwrap_err(),
            Error::WrongKind
        );
        assert_eq!(
            s.set_corner_radius(CLIENT, key, 4.0).unwrap_err(),
            Error::WrongKind
        );
        assert_eq!(
            s.set_border(CLIENT, key, None).unwrap_err(),
            Error::WrongKind
        );
    }
    // Group-only properties.
    for key in [r, image, text] {
        assert_eq!(
            s.set_transform(CLIENT, key, Transform::IDENTITY)
                .unwrap_err(),
            Error::WrongKind
        );
        assert_eq!(s.set_clip(CLIENT, key, true).unwrap_err(), Error::WrongKind);
    }
    // Image-only properties.
    for key in [g, r, text] {
        assert_eq!(
            s.set_image(CLIENT, key, None).unwrap_err(),
            Error::WrongKind
        );
    }
    // Text-only properties.
    for key in [g, r, image] {
        assert_eq!(s.set_text(CLIENT, key, None).unwrap_err(), Error::WrongKind);
    }
    // The common properties work on every kind.
    for key in [g, r, image, text] {
        s.set_bounds(CLIENT, key, Rect::new(1.0, 1.0, 2.0, 2.0))
            .unwrap();
        s.set_opacity(CLIENT, key, 0.5).unwrap();
        s.set_visible(CLIENT, key, false).unwrap();
    }
}

#[test]
fn a_client_may_only_touch_its_own_nodes() {
    let mut s = scene();
    let (win, root) = window(&mut s);
    let node = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));
    let buffer = s
        .create_buffer(CLIENT, BufferDesc::new(4, 4, 16, 0), vec![0; 64])
        .unwrap();

    assert_eq!(
        s.set_bounds(OTHER, node, Rect::EMPTY).unwrap_err(),
        Error::NotOwner
    );
    assert_eq!(
        s.set_visible(OTHER, node, false).unwrap_err(),
        Error::NotOwner
    );
    assert_eq!(
        s.set_fill(OTHER, node, Fill::None).unwrap_err(),
        Error::NotOwner
    );
    assert_eq!(s.destroy_node(OTHER, node).unwrap_err(), Error::NotOwner);
    assert_eq!(
        s.create_node(OTHER, NodeKind::Rect, root, None)
            .unwrap_err(),
        Error::NotOwner
    );
    assert_eq!(s.destroy_window(OTHER, win).unwrap_err(), Error::NotOwner);
    assert_eq!(
        s.set_window_title(OTHER, win, "hijack").unwrap_err(),
        Error::NotOwner
    );
    assert_eq!(s.buffer_mut(OTHER, buffer).unwrap_err(), Error::NotOwner);
    assert_eq!(
        s.destroy_buffer(OTHER, buffer).unwrap_err(),
        Error::NotOwner
    );

    // The server may touch anything.
    s.set_bounds(ClientId::SERVER, node, Rect::new(0.0, 0.0, 1.0, 1.0))
        .unwrap();
    s.buffer_mut(ClientId::SERVER, buffer).unwrap();
    s.set_window_title(ClientId::SERVER, win, "server").unwrap();
    assert_eq!(s.window_info(win).unwrap().title(), "server");
}

#[test]
fn reparenting_into_another_clients_tree_is_refused() {
    let mut s = scene();
    let mine = s.create_window(CLIENT, "mine", Size::new(100.0, 100.0), Layer::Normal);
    let theirs = s.create_window(OTHER, "theirs", Size::new(100.0, 100.0), Layer::Normal);
    let my_root = s.window_info(mine).unwrap().root();
    let their_root = s.window_info(theirs).unwrap().root();
    let node = rect(&mut s, my_root, Rect::new(0.0, 0.0, 10.0, 10.0));

    assert_eq!(
        s.reparent(CLIENT, node, their_root, None).unwrap_err(),
        Error::NotOwner
    );
}

#[test]
fn before_must_be_a_child_of_the_parent() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let g = group(&mut s, root, Rect::new(0.0, 0.0, 100.0, 100.0));
    let stranger = rect(&mut s, root, Rect::new(0.0, 0.0, 10.0, 10.0));

    assert_eq!(
        s.create_node(CLIENT, NodeKind::Rect, g, Some(stranger))
            .unwrap_err(),
        Error::BadSibling
    );
    let node = rect(&mut s, g, Rect::new(0.0, 0.0, 10.0, 10.0));
    assert_eq!(
        s.reparent(CLIENT, node, root, Some(node)).unwrap_err(),
        Error::BadSibling
    );
}

#[test]
fn depth_is_bounded() {
    let mut s = scene();
    let (_, root) = window(&mut s);
    let mut parent = root;
    // The root is depth 0, so MAX_DEPTH more groups fit.
    for _ in 0..MAX_DEPTH {
        parent = s
            .create_node(CLIENT, NodeKind::Group, parent, None)
            .unwrap();
    }
    assert_eq!(
        s.create_node(CLIENT, NodeKind::Group, parent, None)
            .unwrap_err(),
        Error::TooDeep
    );
    // Reparenting a tall subtree under a deep node is refused too.
    let shallow = s.create_node(CLIENT, NodeKind::Group, root, None).unwrap();
    let deeper = s
        .create_node(CLIENT, NodeKind::Group, shallow, None)
        .unwrap();
    assert_eq!(
        s.reparent(CLIENT, shallow, parent, None).unwrap_err(),
        Error::TooDeep
    );
    // ...and the refusal left the tree alone.
    assert_eq!(s.node(deeper).unwrap().parent(), Some(shallow));
    assert_eq!(s.node(shallow).unwrap().parent(), Some(root));
}

#[test]
fn unknown_outputs_are_rejected() {
    let mut s = scene();
    let win = s.create_window(CLIENT, "w", Size::new(10.0, 10.0), Layer::Normal);
    assert_eq!(
        s.place_window(win, Some(OutputId(99)), Point::ZERO)
            .unwrap_err(),
        Error::UnknownOutput
    );
    // ...and the window stayed unplaced.
    assert_eq!(s.window_info(win).unwrap().output(), None);
}

#[test]
fn bad_buffers_are_rejected() {
    let mut s = Scene::new();
    // Zero extent.
    assert_eq!(
        s.create_buffer(CLIENT, BufferDesc::new(0, 4, 4, 0), vec![0; 16])
            .unwrap_err(),
        Error::BadBuffer
    );
    // Stride narrower than the width.
    assert_eq!(
        s.create_buffer(CLIENT, BufferDesc::new(8, 4, 4, 0), vec![0; 16])
            .unwrap_err(),
        Error::BadBuffer
    );
    // Not enough bytes.
    assert_eq!(
        s.create_buffer(CLIENT, BufferDesc::new(4, 4, 16, 0), vec![0; 32])
            .unwrap_err(),
        Error::BadBuffer
    );

    let desc = BufferDesc::new(4, 4, 16, 0x3432_5258);
    let buffer = s.create_buffer(CLIENT, desc, vec![7; 64]).unwrap();
    assert_eq!(s.buffer(buffer).unwrap().desc(), desc);
    assert_eq!(s.buffer(buffer).unwrap().data().len(), 64);

    let win = s.create_window(CLIENT, "w", Size::new(10.0, 10.0), Layer::Normal);
    let root = s.window_info(win).unwrap().root();
    let image = s.create_node(CLIENT, NodeKind::Image, root, None).unwrap();

    // A source rect outside the buffer.
    assert_eq!(
        s.set_image(
            CLIENT,
            image,
            Some(ImageRef::new(buffer, IRect::new(0, 0, 8, 8)))
        )
        .unwrap_err(),
        Error::BadBuffer
    );
    // An empty source rect.
    assert_eq!(
        s.set_image(CLIENT, image, Some(ImageRef::new(buffer, IRect::EMPTY)))
            .unwrap_err(),
        Error::BadBuffer
    );
    // The whole buffer is fine.
    s.set_image(CLIENT, image, Some(ImageRef::new(buffer, desc.full_rect())))
        .unwrap();
    assert_eq!(s.node(image).unwrap().image().unwrap().buffer, buffer);
}

#[test]
fn errors_display_and_are_std_errors() {
    let e: Box<dyn std::error::Error> = Box::new(Error::StaleKey);
    assert_eq!(e.to_string(), "stale key");
    assert_eq!(Error::NotOwner.to_string(), "not the owner");
}
