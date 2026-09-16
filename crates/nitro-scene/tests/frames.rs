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

// ------------------------------------------------- the window's own clip

#[test]
fn a_windows_content_clips_from_the_moment_it_exists() {
    // Not "after the server sets it up": the flag is on the node
    // `create_window` mints, so there is no window in any state, framed
    // or not, decorated or not, whose content does not clip.
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    assert!(s.node(content).unwrap().clip(), "undecorated");

    let frame = s.frame_window(win, INSET).unwrap();
    assert!(
        s.node(content).unwrap().clip(),
        "framing inserts a root above the content and leaves its flag alone"
    );
    assert!(
        !s.node(frame).unwrap().clip(),
        "the frame group does not clip: the title bar's rounded overhang \
         lives outside the content rect on purpose"
    );
}

#[test]
fn a_client_cannot_turn_its_windows_clip_off() {
    // The whole point of moving this into the compositor: a client that
    // does not want to be contained does not get to opt out. `SetClip`
    // is the only wire message that could ask, and it is refused.
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));

    assert_eq!(
        s.set_clip(CLIENT, content, false).unwrap_err(),
        Error::RootNode
    );
    assert!(s.node(content).unwrap().clip(), "and it really is still on");

    // Asking for the clip it already has is the no-op it always was, so
    // a toolkit that sets the flag on its own root — `nitro-ui` does —
    // is not broken by this.
    s.set_clip(CLIENT, content, true).unwrap();
    assert!(s.node(content).unwrap().clip());

    // Not even the server, and that is deliberate rather than an
    // oversight: the invariant is the compositor's promise to the user
    // about every window, so there is no privileged caller who may void
    // it. A `wm` that needed to would be a `wm` with a bug.
    assert_eq!(
        s.set_clip(nitro_scene::ClientId::SERVER, content, false)
            .unwrap_err(),
        Error::RootNode
    );

    // A framed window answers the same, through the same node.
    s.frame_window(win, INSET).unwrap();
    assert_eq!(
        s.set_clip(CLIENT, content, false).unwrap_err(),
        Error::RootNode
    );

    // And an ordinary group of the client's own is still the client's to
    // clip or not: the refusal is about the *window's* node, not a new
    // rule about clipping.
    let g = common::group(&mut s, content, Rect::new(0.0, 0.0, 50.0, 50.0));
    s.set_clip(CLIENT, g, true).unwrap();
    s.set_clip(CLIENT, g, false).unwrap();
    assert!(!s.node(g).unwrap().clip());
}

#[test]
fn a_node_outside_the_window_paints_nothing_and_cannot_be_hit() {
    // The three consequences that have to agree, in one test: what is
    // painted, what is hit, and what is damaged. They agree because all
    // three read the *same* `clip_rect` off the node — this pins that
    // they do, so a later change that gives the hit test a rectangle of
    // its own fails here rather than on a desktop.
    let mut s = scene();
    let (_, content) = window_at(&mut s, Point::new(10.0, 20.0), Size::new(200.0, 100.0));
    // `nitro-settings`' Displays row, with the arithmetic removed: a
    // child laid out past the window's right edge.
    let spill = rect(&mut s, content, Rect::new(180.0, 10.0, 200.0, 30.0));
    settle(&mut s);

    // Painted: only the 20 px that are inside the window.
    assert_eq!(
        s.node(spill).unwrap().world_bounds(),
        nitro_core::IRect::new(190, 30, 20, 30),
        "cut off at the window's edge, not painted past it"
    );

    // Hit: a point on the part inside reaches the client...
    let hit = s.hit_test(OUT, Point::new(195.0, 35.0)).unwrap();
    assert_eq!(hit.node, spill);
    // ...and a point on the part outside reaches nothing at all. This is
    // the click on the spilled checkbox that must not arrive: a pixel
    // the client did not get to paint is a pixel it does not own.
    assert!(
        s.hit_test(OUT, Point::new(260.0, 35.0)).is_none(),
        "a node outside the window is not hit-testable"
    );
}

#[test]
fn moving_a_node_out_of_the_window_damages_nothing_outside_it() {
    // The damage half of the same rule, and the one a compositor gets
    // wrong quietly: a repaint of pixels the window does not own is not
    // visible as a wrong colour, only as a window somewhere else
    // flickering — or as a damage figure nobody can account for.
    let mut s = scene();
    let (_, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    let r = rect(&mut s, content, Rect::new(10.0, 10.0, 20.0, 20.0));
    settle(&mut s);

    // Straight out of the window, 300 px to the right.
    s.set_bounds(CLIENT, r, Rect::new(310.0, 10.0, 20.0, 20.0))
        .unwrap();
    let d = common::damage(&mut s);
    // Old ∪ new, and the new half is empty: the node vacated its old
    // rectangle and arrived nowhere paintable.
    assert_eq!(
        d.rects(),
        &[nitro_core::IRect::new(10, 10, 20, 20)],
        "the pixels it left, and nothing outside the window"
    );
    assert!(s.node(r).unwrap().world_bounds().is_empty());
    // `painted` is deliberately *not* the answer here: it is the node's
    // own content-ness (visible, opaque, has something to draw), and the
    // clip shows up in `world_bounds`. Both are read on the paint path —
    // `paint_list` and `hit_node` require `painted` **and** a
    // `world_bounds` hit — so a fully clipped node still reaches neither
    // the rasterizer nor a click.
    assert!(s.node(r).unwrap().painted());
}

#[test]
fn the_clip_follows_every_resize_because_it_is_the_content_bounds() {
    // The failure mode a clip rectangle stored beside the window would
    // have: correct on the day it was written, then one path forgets to
    // update it and the window clips to a size it no longer has. Nothing
    // here re-sets a clip, because the clip *is* the content bounds.
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    let frame = s.frame_window(win, INSET).unwrap();
    // A child that sticks out in every direction, so the clip is the
    // only thing deciding its footprint.
    let child = rect(&mut s, content, Rect::new(-500.0, -500.0, 2000.0, 2000.0));
    settle(&mut s);

    let content_rect = |s: &nitro_scene::Scene| s.node(child).unwrap().world_bounds();

    // Framed and Normal: the content rect, inside the insets.
    assert_eq!(
        content_rect(&s),
        nitro_core::IRect::new(1, 28, 200, 100),
        "clipped to the content rect, not the frame"
    );

    // A server-driven resize (maximize, an edge drag).
    s.set_window_size(nitro_scene::ClientId::SERVER, win, Size::new(400.0, 250.0))
        .unwrap();
    settle(&mut s);
    assert_eq!(content_rect(&s), nitro_core::IRect::new(1, 28, 400, 250));

    // Fullscreen: the insets go to zero and the content takes the whole
    // frame, so an undecorated window clips to its full bounds.
    s.set_window_inset(win, Insets::NONE).unwrap();
    settle(&mut s);
    assert_eq!(
        content_rect(&s),
        nitro_core::IRect::new(0, 0, 400, 250),
        "UNDECORATED/fullscreen: the clip is the whole window"
    );
    // And back, which is what leaving fullscreen does.
    s.set_window_inset(win, INSET).unwrap();
    settle(&mut s);
    assert_eq!(content_rect(&s), nitro_core::IRect::new(1, 28, 400, 250));

    // A *client's* own resize of its top-level group, which the scene
    // treats as a resize request and re-origins inside the frame.
    s.set_bounds(CLIENT, content, Rect::new(0.0, 0.0, 120.0, 60.0))
        .unwrap();
    settle(&mut s);
    assert_eq!(content_rect(&s), nitro_core::IRect::new(1, 28, 120, 60));

    // The frame group grew and shrank with it all along.
    assert_eq!(
        s.node(frame).unwrap().bounds().size(),
        Size::new(120.0 + INSET.width(), 60.0 + INSET.height())
    );
}

#[test]
fn a_client_cannot_paint_over_its_own_title_bar() {
    // The other direction, and the one a client reaches by accident: a
    // negative offset. The frame's decorations are the content group's
    // *siblings*, so without a clip a node at y = -28 would paint over
    // the title bar the server drew — a window with no close button.
    let mut s = scene();
    let (win, content) = window_at(&mut s, Point::ZERO, Size::new(200.0, 100.0));
    s.frame_window(win, INSET).unwrap();
    let over = rect(
        &mut s,
        content,
        Rect::new(0.0, -INSET.top, 200.0, INSET.top),
    );
    settle(&mut s);

    assert!(
        s.node(over).unwrap().world_bounds().is_empty(),
        "the title bar is not the client's to paint on"
    );
    // The title bar's own pixels are reached by nothing of the client's.
    assert!(s.hit_test(OUT, Point::new(100.0, 14.0)).is_none());
}
