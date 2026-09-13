//! Window frames: the server-owned group wrapped around a client's own,
//! and the window state that goes with it.
//!
//! The *policy* — what a frame looks like, how thick it is, which
//! rectangle `Maximized` means — is the server's, in `nitro-server`'s `wm`
//! module. What is tested here is the structure: that framing a window
//! leaves the client's keys, bounds and coordinates alone, that the
//! geometry stays consistent through resizes, and that the state a window
//! carries does what it says.

// Every number here is an exact input propagated by exact arithmetic —
// whole-pixel insets and sizes — so equality is the assertion that means
// what it says; an epsilon would only hide a wrong formula.
#![allow(clippy::float_cmp)]

mod common;

use common::{CLIENT, OUT, rect, scene, settle, window_at};
use nitro_core::{Point, Rect, Size};
use nitro_scene::{Error, Insets, Layer, WindowFlags, WindowState};

const INSET: Insets = Insets {
    left: 1.0,
    top: 28.0,
    right: 1.0,
    bottom: 1.0,
};

#[test]
fn an_unframed_window_is_its_own_content() {
    let mut s = scene();
    let (win, root) = window_at(&mut s, Point::ZERO, Size::new(400.0, 300.0));
    let info = s.window_info(win).unwrap();
    assert_eq!(info.content(), root);
    assert!(!info.is_framed());
    assert_eq!(info.inset(), Insets::NONE);
    assert_eq!(info.frame_size(), info.size());
    assert_eq!(info.content_position(), info.position());
}

#[test]
fn framing_keeps_the_clients_node_and_grows_the_window_around_it() {
    let mut s = scene();
    let size = Size::new(400.0, 300.0);
    let (win, content) = window_at(&mut s, Point::new(50.0, 60.0), size);
    // A node the client created *before* the frame existed.
    let child = rect(&mut s, content, Rect::new(0.0, 0.0, 10.0, 10.0));

    let frame = s.frame_window(win, INSET).unwrap();
    let info = s.window_info(win).unwrap();

    // The client's key still names the client's group, which is the whole
    // point: an id it already holds must keep meaning what it meant.
    assert_eq!(info.content(), content);
    assert_ne!(frame, content);
    assert_eq!(info.root(), frame);
    assert!(info.is_framed());
    assert_eq!(s.node(child).unwrap().parent(), Some(content));

    // The content keeps its own size; the frame is that plus the insets.
    assert_eq!(info.size(), size);
    assert_eq!(
        info.frame_size(),
        Size::new(size.w + INSET.width(), size.h + INSET.height())
    );
    // `position` is the frame's; `content_position` is what a client is
    // told, and the difference is exactly the inset.
    assert_eq!(info.position(), Point::new(50.0, 60.0));
    assert_eq!(
        info.content_position(),
        Point::new(50.0 + INSET.left, 60.0 + INSET.top)
    );
    // The content sits inside the frame at the inset offset.
    let bounds = s.node(content).unwrap().bounds();
    assert_eq!((bounds.x, bounds.y), (INSET.left, INSET.top));
    // And the frame group is the server's, not the client's.
    assert_eq!(
        s.node(frame).unwrap().client(),
        nitro_scene::ClientId::SERVER
    );
    assert_eq!(s.node(content).unwrap().client(), CLIENT);
}

#[test]
fn a_framed_window_puts_the_content_on_top_of_the_decorations() {
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    let frame = s.frame_window(win, INSET).unwrap();
    // Decorations are created before the content, which puts them behind
    // it: a client can never paint over its own title bar, and the server
    // can never paint over the client's pixels.
    let bar = s
        .create_node(
            nitro_scene::ClientId::SERVER,
            nitro_scene::NodeKind::Rect,
            frame,
            Some(content),
        )
        .unwrap();
    assert_eq!(s.node(frame).unwrap().children(), &[bar, content]);
}

#[test]
fn framing_twice_is_refused() {
    let mut s = scene();
    let (win, _) = window_at(&mut s, Point::ZERO, Size::new(100.0, 100.0));
    s.frame_window(win, INSET).unwrap();
    assert_eq!(s.frame_window(win, INSET), Err(Error::BadParent));
}

#[test]
fn neither_the_root_nor_the_content_can_be_destroyed_on_its_own() {
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(100.0, 100.0));
    let frame = s.frame_window(win, INSET).unwrap();
    // The content now *has* a parent, so the old "no parent = a root"
    // test is no longer enough on its own.
    assert_eq!(s.destroy_node(CLIENT, content), Err(Error::RootNode));
    assert_eq!(
        s.destroy_node(nitro_scene::ClientId::SERVER, frame),
        Err(Error::RootNode)
    );
    // Closing the window takes both, and every decoration with them.
    let before = s.node_count();
    s.destroy_window(CLIENT, win).unwrap();
    assert!(s.node_count() < before);
    assert!(s.window_info(win).is_err());
}

#[test]
fn resizing_a_framed_window_moves_the_frame_and_the_content_together() {
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    let frame = s.frame_window(win, INSET).unwrap();

    let new = Size::new(320.0, 240.0);
    s.set_window_size(CLIENT, win, new).unwrap();
    let info = s.window_info(win).unwrap();
    assert_eq!(info.size(), new);
    assert_eq!(
        info.frame_size(),
        Size::new(new.w + INSET.width(), new.h + INSET.height())
    );
    assert_eq!(s.node(content).unwrap().bounds().size(), new);
    assert_eq!(
        s.node(frame).unwrap().bounds().size(),
        Size::new(new.w + INSET.width(), new.h + INSET.height())
    );
}

#[test]
fn a_clients_own_set_bounds_resizes_it_without_sliding_it_out_of_the_frame() {
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    s.frame_window(win, INSET).unwrap();

    // Every toolkit sends its window bounds as `(0, 0, w, h)`. Taken
    // literally that would slide the content out from under the title
    // bar; the origin inside the frame belongs to the frame.
    s.set_bounds(CLIENT, content, Rect::new(0.0, 0.0, 300.0, 200.0))
        .unwrap();
    let bounds = s.node(content).unwrap().bounds();
    assert_eq!((bounds.x, bounds.y), (INSET.left, INSET.top));
    assert_eq!(s.window_info(win).unwrap().size(), Size::new(300.0, 200.0));
    assert_eq!(
        s.window_info(win).unwrap().frame_size(),
        Size::new(300.0 + INSET.width(), 200.0 + INSET.height())
    );
}

#[test]
fn changing_the_inset_moves_the_content_and_resizes_the_frame() {
    let mut s = scene();
    let size = Size::new(200.0, 100.0);
    let (win, content) = window_at(&mut s, Point::ZERO, size);
    let frame = s.frame_window(win, INSET).unwrap();

    // Fullscreen hides the decorations by zeroing the insets rather than
    // destroying the frame group, which would restructure the tree under
    // a live client.
    s.set_window_inset(win, Insets::NONE).unwrap();
    assert_eq!(s.node(content).unwrap().bounds().x, 0.0);
    assert_eq!(s.node(frame).unwrap().bounds().size(), size);
    assert_eq!(s.window_info(win).unwrap().frame_size(), size);

    s.set_window_inset(win, INSET).unwrap();
    assert_eq!(s.node(content).unwrap().bounds().y, INSET.top);
}

#[test]
fn the_frame_is_what_is_painted_and_hit() {
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::new(10.0, 20.0), Size::new(200.0, 100.0));
    let frame = s.frame_window(win, INSET).unwrap();
    // A decoration behind the client's own rect.
    let bar = s
        .create_node(
            nitro_scene::ClientId::SERVER,
            nitro_scene::NodeKind::Rect,
            frame,
            Some(content),
        )
        .unwrap();
    s.set_bounds(
        nitro_scene::ClientId::SERVER,
        bar,
        Rect::new(0.0, 0.0, 200.0 + INSET.width(), INSET.top),
    )
    .unwrap();
    s.set_fill(
        nitro_scene::ClientId::SERVER,
        bar,
        nitro_scene::Fill::Solid(nitro_core::Color::WHITE),
    )
    .unwrap();
    let client_rect = rect(&mut s, content, Rect::new(0.0, 0.0, 200.0, 100.0));
    settle(&mut s);

    // A point in the title bar hits the server's node; one in the content
    // hits the client's.
    let hit = s.hit_test(OUT, Point::new(50.0, 25.0)).unwrap();
    assert_eq!(hit.node, bar);
    assert_eq!(hit.window, win);
    let hit = s.hit_test(OUT, Point::new(50.0, 60.0)).unwrap();
    assert_eq!(hit.node, client_rect);
}

#[test]
fn minimizing_hides_the_whole_window_and_restoring_brings_it_back() {
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::new(10.0, 20.0), Size::new(200.0, 100.0));
    s.frame_window(win, INSET).unwrap();
    rect(&mut s, content, Rect::new(0.0, 0.0, 200.0, 100.0));
    settle(&mut s);

    assert_eq!(s.window_info(win).unwrap().state(), WindowState::Normal);
    s.set_window_state(win, WindowState::Minimized).unwrap();
    // Hiding damages exactly what the window covered, and nothing else.
    let d = common::damage(&mut s);
    assert!(!d.is_empty());
    assert!(s.hit_test(OUT, Point::new(50.0, 60.0)).is_none());

    s.set_window_state(win, WindowState::Normal).unwrap();
    settle(&mut s);
    assert!(s.hit_test(OUT, Point::new(50.0, 60.0)).is_some());
    // Un-minimizing does not move anything: it lands where it was.
    assert_eq!(
        s.window_info(win).unwrap().position(),
        Point::new(10.0, 20.0)
    );
}

#[test]
fn the_state_is_stored_but_not_interpreted() {
    let mut s = scene();
    let (win, _) = window_at(&mut s, Point::new(10.0, 20.0), Size::new(200.0, 100.0));
    // Which rectangle `Maximized` means depends on the work area, and the
    // work area is the server's business; the scene only remembers.
    let before = s.window_info(win).unwrap().position();
    s.set_window_state(win, WindowState::Maximized).unwrap();
    assert_eq!(s.window_info(win).unwrap().state(), WindowState::Maximized);
    assert_eq!(s.window_info(win).unwrap().position(), before);
}

#[test]
fn the_restore_rectangle_is_remembered_verbatim() {
    let mut s = scene();
    let (win, _) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    assert_eq!(s.window_info(win).unwrap().restore(), None);
    let want = (Point::new(7.0, 9.0), Size::new(11.0, 13.0));
    s.set_window_restore(win, Some(want)).unwrap();
    assert_eq!(s.window_info(win).unwrap().restore(), Some(want));
    s.set_window_restore(win, None).unwrap();
    assert_eq!(s.window_info(win).unwrap().restore(), None);
}

#[test]
fn limits_clamp_a_size_on_both_ends_and_zero_means_no_limit() {
    let mut s = scene();
    let (win, _) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    // The default is "resize me freely".
    assert_eq!(
        s.clamp_to_limits(win, Size::new(1.0, 9999.0)),
        Size::new(1.0, 9999.0)
    );

    s.set_window_limits(CLIENT, win, Size::new(100.0, 50.0), Size::new(400.0, 300.0))
        .unwrap();
    assert_eq!(
        s.clamp_to_limits(win, Size::new(10.0, 10.0)),
        Size::new(100.0, 50.0)
    );
    assert_eq!(
        s.clamp_to_limits(win, Size::new(9999.0, 9999.0)),
        Size::new(400.0, 300.0)
    );
    assert_eq!(
        s.clamp_to_limits(win, Size::new(200.0, 100.0)),
        Size::new(200.0, 100.0)
    );

    // A zero component is "no limit" on that axis, not "clamp to zero".
    s.set_window_limits(CLIENT, win, Size::new(0.0, 50.0), Size::ZERO)
        .unwrap();
    assert_eq!(
        s.clamp_to_limits(win, Size::new(1.0, 1.0)),
        Size::new(1.0, 50.0)
    );

    // A `max` below `min` is clamped up rather than refused: limits are a
    // hint the window manager applies, not a request that can fail.
    s.set_window_limits(CLIENT, win, Size::new(200.0, 200.0), Size::new(10.0, 10.0))
        .unwrap();
    assert_eq!(
        s.clamp_to_limits(win, Size::new(500.0, 500.0)),
        Size::new(200.0, 200.0)
    );

    // Nonsense is dropped rather than poisoning every later clamp.
    s.set_window_limits(CLIENT, win, Size::new(f32::NAN, -5.0), Size::ZERO)
        .unwrap();
    assert_eq!(
        s.clamp_to_limits(win, Size::new(3.0, 4.0)),
        Size::new(3.0, 4.0)
    );
}

#[test]
fn flags_and_the_app_id_survive_the_round_trip() {
    let mut s = scene();
    let flags = WindowFlags {
        decorated: false,
        fixed_size: true,
        focusable: false,
    };
    let win = s.create_window_with(CLIENT, "t", Size::new(10.0, 10.0), Layer::Normal, flags);
    assert_eq!(s.window_info(win).unwrap().flags(), flags);
    // The default is the opt-out model: decorated, resizable, focusable.
    let other = s.create_window(CLIENT, "t", Size::new(10.0, 10.0), Layer::Normal);
    assert_eq!(
        s.window_info(other).unwrap().flags(),
        WindowFlags::default()
    );

    assert_eq!(s.window_info(win).unwrap().app_id(), "");
    s.set_app_id(CLIENT, win, "org.nitro.calc").unwrap();
    assert_eq!(s.window_info(win).unwrap().app_id(), "org.nitro.calc");
}
