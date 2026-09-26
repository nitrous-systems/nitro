//! Popup geometry: where a menu or tooltip lands, and what to do when it
//! does not fit.
//!
//! Pure arithmetic on one output's logical space — no `Scene`, no
//! `Server` — for the reason [`crate::wm`] is: the interesting rules (the
//! nine anchors, the nine gravities, and the flip → slide → resize order
//! Wayland specifies) are then unit-testable without a compositor, and the
//! server half is left with plumbing.
//!
//! The model is `ui::OwnedWindowAnchor`, which is 1:1 with
//! `xdg_positioner`: an anchor rectangle in the parent's space, which
//! point of it the popup hangs off, which way it grows from that point,
//! and what the server may do when the result leaves the work area. See
//! `docs/wm.md` § Popups.

use nitro_core::{Point, Rect, Size};
use nitro_scene::WindowKey;
use nitro_wire::types::{PopupAnchor, PopupGravity, constraint_adjust};

/// How deep a popup chain may get.
///
/// Wayland has no cap; nitro refuses unbounded server-side recursion on a
/// client's say-so. Sixteen is well past any real submenu tree (Chromium's
/// deepest stock menu is three) and small enough that the chain walks stay
/// trivially bounded.
pub const MAX_POPUP_DEPTH: usize = 16;

/// What the server remembers about a live popup.
///
/// The positioner parameters are kept, not just the answer they produced:
/// `RepositionPopup` replaces them, and a parent that moves re-derives the
/// placement from them rather than dismissing the chain.
#[derive(Debug, Clone, Copy)]
pub struct PopupInfo {
    /// The window this popup hangs off: a toplevel, or another popup.
    pub parent: WindowKey,
    /// Anchor rectangle in the parent's **content** space, logical pixels.
    pub anchor_rect: Rect,
    /// Which point of `anchor_rect` the popup hangs off.
    pub anchor: PopupAnchor,
    /// Which way it grows from that point.
    pub gravity: PopupGravity,
    /// `constraint_adjust` bitmask: what the server may do to make it fit.
    pub constraint: u32,
    /// Whether it took the pointer grab (`popup_flags::GRAB`).
    pub grab: bool,
    /// Whether it has already been dismissed. A dismissed popup is
    /// unplaced and stays that way: a later `RepositionPopup` on it is
    /// ignored rather than resurrecting it.
    pub dismissed: bool,
}

/// The point of `rect` an anchor names.
#[must_use]
pub fn anchor_point(rect: Rect, anchor: PopupAnchor) -> Point {
    let x = match anchor {
        PopupAnchor::Left | PopupAnchor::TopLeft | PopupAnchor::BottomLeft => rect.x,
        PopupAnchor::Right | PopupAnchor::TopRight | PopupAnchor::BottomRight => rect.x + rect.w,
        _ => rect.x + rect.w / 2.0,
    };
    let y = match anchor {
        PopupAnchor::Top | PopupAnchor::TopLeft | PopupAnchor::TopRight => rect.y,
        PopupAnchor::Bottom | PopupAnchor::BottomLeft | PopupAnchor::BottomRight => rect.y + rect.h,
        _ => rect.y + rect.h / 2.0,
    };
    Point::new(x, y)
}

/// Where a popup of `size` lands, before any constraint adjustment.
///
/// The anchor picks a point of the rectangle; the gravity says which way
/// the popup grows from it. `Left` gravity puts the popup's *right* edge
/// on the point, `Right` its left edge, and the centre-ish gravities
/// (`None`, `Top`, `Bottom` on x) centre it — exactly `xdg_positioner`.
#[must_use]
pub fn place(anchor_rect: Rect, anchor: PopupAnchor, gravity: PopupGravity, size: Size) -> Rect {
    let p = anchor_point(anchor_rect, anchor);
    let x = match gravity {
        PopupGravity::Left | PopupGravity::TopLeft | PopupGravity::BottomLeft => p.x - size.w,
        PopupGravity::Right | PopupGravity::TopRight | PopupGravity::BottomRight => p.x,
        _ => p.x - size.w / 2.0,
    };
    let y = match gravity {
        PopupGravity::Top | PopupGravity::TopLeft | PopupGravity::TopRight => p.y - size.h,
        PopupGravity::Bottom | PopupGravity::BottomLeft | PopupGravity::BottomRight => p.y,
        _ => p.y - size.h / 2.0,
    };
    Rect::new(x, y, size.w, size.h)
}

/// The anchor mirrored across the anchor rectangle's vertical centre.
#[must_use]
pub fn flip_anchor_x(a: PopupAnchor) -> PopupAnchor {
    match a {
        PopupAnchor::Left => PopupAnchor::Right,
        PopupAnchor::Right => PopupAnchor::Left,
        PopupAnchor::TopLeft => PopupAnchor::TopRight,
        PopupAnchor::TopRight => PopupAnchor::TopLeft,
        PopupAnchor::BottomLeft => PopupAnchor::BottomRight,
        PopupAnchor::BottomRight => PopupAnchor::BottomLeft,
        other => other,
    }
}

/// The anchor mirrored across the anchor rectangle's horizontal centre.
#[must_use]
pub fn flip_anchor_y(a: PopupAnchor) -> PopupAnchor {
    match a {
        PopupAnchor::Top => PopupAnchor::Bottom,
        PopupAnchor::Bottom => PopupAnchor::Top,
        PopupAnchor::TopLeft => PopupAnchor::BottomLeft,
        PopupAnchor::BottomLeft => PopupAnchor::TopLeft,
        PopupAnchor::TopRight => PopupAnchor::BottomRight,
        PopupAnchor::BottomRight => PopupAnchor::TopRight,
        other => other,
    }
}

/// The gravity mirrored on x.
#[must_use]
pub fn flip_gravity_x(g: PopupGravity) -> PopupGravity {
    match g {
        PopupGravity::Left => PopupGravity::Right,
        PopupGravity::Right => PopupGravity::Left,
        PopupGravity::TopLeft => PopupGravity::TopRight,
        PopupGravity::TopRight => PopupGravity::TopLeft,
        PopupGravity::BottomLeft => PopupGravity::BottomRight,
        PopupGravity::BottomRight => PopupGravity::BottomLeft,
        other => other,
    }
}

/// The gravity mirrored on y.
#[must_use]
pub fn flip_gravity_y(g: PopupGravity) -> PopupGravity {
    match g {
        PopupGravity::Top => PopupGravity::Bottom,
        PopupGravity::Bottom => PopupGravity::Top,
        PopupGravity::TopLeft => PopupGravity::BottomLeft,
        PopupGravity::BottomLeft => PopupGravity::TopLeft,
        PopupGravity::TopRight => PopupGravity::BottomRight,
        PopupGravity::BottomRight => PopupGravity::TopRight,
        other => other,
    }
}

/// Whether a rectangle overflows an area on x / on y.
fn overflows_x(r: Rect, area: Rect) -> bool {
    r.x < area.x || r.x + r.w > area.x + area.w
}

fn overflows_y(r: Rect, area: Rect) -> bool {
    r.y < area.y || r.y + r.h > area.y + area.h
}

/// Apply the constraint adjustments `constraint` allows, in the order
/// `xdg_positioner` specifies: **flip, then slide, then resize**, each
/// axis independently.
///
/// * *flip* inverts the anchor **and** the gravity on that axis and
///   re-places — and keeps the result **only if it fits**. Reverting an
///   unhelpful flip is the documented behaviour and the part most
///   implementations get wrong: a menu near the bottom of a tall screen
///   would otherwise be flipped up into an even worse position.
/// * *slide* translates the rectangle back inside the area, clamped so
///   the near edge never crosses the area's origin — a popup wider than
///   the work area ends flush against it rather than centred outside.
/// * *resize* shrinks it to the intersection on that axis.
///
/// A bit that is not set means "let it overflow": a client that asked for
/// no adjustment gets exactly what its positioner says.
///
/// The final position is rounded to whole logical pixels, for
/// [`crate::wm::clamp_into`]'s reason: a menu on a half pixel is a blurred
/// menu, and every rectangle the window manager produces is whole.
#[must_use]
pub fn constrain(
    anchor_rect: Rect,
    anchor: PopupAnchor,
    gravity: PopupGravity,
    constraint: u32,
    size: Size,
    area: Rect,
) -> Rect {
    let mut rect = place(anchor_rect, anchor, gravity, size);
    // Flip: x and y independently, each kept only if it actually helps.
    if constraint & constraint_adjust::FLIP_X != 0 && overflows_x(rect, area) {
        let flipped = place(
            anchor_rect,
            flip_anchor_x(anchor),
            flip_gravity_x(gravity),
            size,
        );
        if !overflows_x(flipped, area) {
            rect.x = flipped.x;
        }
    }
    if constraint & constraint_adjust::FLIP_Y != 0 && overflows_y(rect, area) {
        let flipped = place(
            anchor_rect,
            flip_anchor_y(anchor),
            flip_gravity_y(gravity),
            size,
        );
        if !overflows_y(flipped, area) {
            rect.y = flipped.y;
        }
    }
    // Slide.
    if constraint & constraint_adjust::SLIDE_X != 0 && overflows_x(rect, area) {
        if rect.x + rect.w > area.x + area.w {
            rect.x = area.x + area.w - rect.w;
        }
        rect.x = rect.x.max(area.x);
    }
    if constraint & constraint_adjust::SLIDE_Y != 0 && overflows_y(rect, area) {
        if rect.y + rect.h > area.y + area.h {
            rect.y = area.y + area.h - rect.h;
        }
        rect.y = rect.y.max(area.y);
    }
    // Resize: the intersection on that axis, never below zero.
    if constraint & constraint_adjust::RESIZE_X != 0 && overflows_x(rect, area) {
        let left = rect.x.max(area.x);
        let right = (rect.x + rect.w).min(area.x + area.w);
        rect.x = left;
        rect.w = (right - left).max(0.0);
    }
    if constraint & constraint_adjust::RESIZE_Y != 0 && overflows_y(rect, area) {
        let top = rect.y.max(area.y);
        let bottom = (rect.y + rect.h).min(area.y + area.h);
        rect.y = top;
        rect.h = (bottom - top).max(0.0);
    }
    Rect::new(rect.x.round(), rect.y.round(), rect.w, rect.h)
}

/// The positioner a client that gave **no** anchor gets.
///
/// Chromium reaches this path whenever a `PlatformWindow` is created
/// without a `ui::OwnedWindowAnchor`, and it has its own defaults for it
/// (`ui/ozone/platform/wayland/host/xdg_popup.cc:246-251`): the anchor
/// rectangle collapsed to 1×1, anchor `TopLeft`, gravity `BottomRight`,
/// constraint `FlipY`. Those are reproduced here exactly rather than
/// invented, because a client hitting the path has no say in what it gets
/// and the two ends must agree.
///
/// nitro's `CreatePopup` carries no `bounds` field to collapse, so the
/// **trigger** is an empty `anchor_rect` — zero or negative on either
/// axis, which is what a backend with no anchor to send produces — and the
/// 1×1 rectangle is taken at that rectangle's own origin, which is where
/// the client wanted the popup (Chromium's `params.bounds.origin()`). The
/// other three fields are overridden whatever the client put in them: on
/// this path they are not a request, they are unset. See `docs/wire.md`.
///
/// Returns `None` when the anchor rectangle is real, i.e. the client did
/// give a positioner and it is to be honoured as sent.
#[must_use]
pub fn fallback(anchor_rect: Rect) -> Option<(Rect, PopupAnchor, PopupGravity, u32)> {
    if anchor_rect.w > 0.0 && anchor_rect.h > 0.0 {
        return None;
    }
    Some((
        Rect::new(anchor_rect.x, anchor_rect.y, 1.0, 1.0),
        PopupAnchor::TopLeft,
        PopupGravity::BottomRight,
        constraint_adjust::FLIP_Y,
    ))
}

#[cfg(test)]
mod tests {
    // Every number here is exact arithmetic on whole pixels and halves of
    // even numbers, so equality is the assertion that means what it says.
    #![allow(clippy::float_cmp)]

    use super::*;

    /// An 800x600 work area at the origin.
    const AREA: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);
    /// A 40x20 anchor rectangle in the middle of it.
    const ANCHOR: Rect = Rect::new(100.0, 100.0, 40.0, 20.0);
    const SIZE: Size = Size::new(60.0, 30.0);

    #[test]
    fn every_anchor_names_its_own_point() {
        let table = [
            (PopupAnchor::None, 120.0, 110.0),
            (PopupAnchor::Top, 120.0, 100.0),
            (PopupAnchor::Bottom, 120.0, 120.0),
            (PopupAnchor::Left, 100.0, 110.0),
            (PopupAnchor::Right, 140.0, 110.0),
            (PopupAnchor::TopLeft, 100.0, 100.0),
            (PopupAnchor::TopRight, 140.0, 100.0),
            (PopupAnchor::BottomLeft, 100.0, 120.0),
            (PopupAnchor::BottomRight, 140.0, 120.0),
        ];
        for (anchor, x, y) in table {
            let p = anchor_point(ANCHOR, anchor);
            assert_eq!((p.x, p.y), (x, y), "{anchor:?}");
        }
    }

    #[test]
    fn gravity_decides_which_way_the_popup_grows() {
        // Anchored on the rectangle's bottom-left corner (100, 120).
        let a = PopupAnchor::BottomLeft;
        let table = [
            // Centred on the point.
            (PopupGravity::None, 70.0, 105.0),
            (PopupGravity::Top, 70.0, 90.0),
            (PopupGravity::Bottom, 70.0, 120.0),
            (PopupGravity::Left, 40.0, 105.0),
            (PopupGravity::Right, 100.0, 105.0),
            (PopupGravity::TopLeft, 40.0, 90.0),
            (PopupGravity::TopRight, 100.0, 90.0),
            (PopupGravity::BottomLeft, 40.0, 120.0),
            (PopupGravity::BottomRight, 100.0, 120.0),
        ];
        for (gravity, x, y) in table {
            let r = place(ANCHOR, a, gravity, SIZE);
            assert_eq!((r.x, r.y), (x, y), "{gravity:?}");
            assert_eq!((r.w, r.h), (SIZE.w, SIZE.h));
        }
    }

    #[test]
    fn a_popup_that_fits_is_not_adjusted_at_all() {
        let r = constrain(
            ANCHOR,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::ALL,
            SIZE,
            AREA,
        );
        assert_eq!((r.x, r.y, r.w, r.h), (100.0, 120.0, 60.0, 30.0));
    }

    #[test]
    fn without_a_bit_the_popup_is_allowed_to_overflow() {
        // Hard against the right edge, growing right: 100 px outside.
        let anchor = Rect::new(760.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomRight,
            PopupGravity::BottomRight,
            0,
            SIZE,
            AREA,
        );
        assert_eq!((r.x, r.y), (800.0, 120.0));
    }

    #[test]
    fn flip_x_swaps_the_side_when_that_helps() {
        let anchor = Rect::new(760.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomRight,
            PopupGravity::BottomRight,
            constraint_adjust::FLIP_X,
            SIZE,
            AREA,
        );
        // Flipped to hang off the rectangle's left edge, growing left.
        assert_eq!((r.x, r.y), (700.0, 120.0));
    }

    #[test]
    fn flip_y_swaps_the_side_when_that_helps() {
        // A menu at the bottom of the screen, wanting to drop down.
        let anchor = Rect::new(100.0, 580.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::FLIP_Y,
            SIZE,
            AREA,
        );
        // Flipped to hang off the top edge, growing up: 580 - 30.
        assert_eq!((r.x, r.y), (100.0, 550.0));
    }

    #[test]
    fn a_flip_that_does_not_help_keeps_the_original_side() {
        // The popup is taller than the whole area, so neither side fits.
        let tall = Size::new(60.0, 700.0);
        let anchor = Rect::new(100.0, 580.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::FLIP_Y,
            tall,
            AREA,
        );
        // Unflipped: down from the anchor's bottom edge.
        assert_eq!(r.y, 600.0);
    }

    #[test]
    fn slide_brings_the_popup_back_inside() {
        let anchor = Rect::new(760.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomRight,
            PopupGravity::BottomRight,
            constraint_adjust::SLIDE_X,
            SIZE,
            AREA,
        );
        assert_eq!(r.x, 740.0);
        assert_eq!(r.w, 60.0);

        // ...and on y.
        let anchor = Rect::new(100.0, 580.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::SLIDE_Y,
            SIZE,
            AREA,
        );
        assert_eq!(r.y, 570.0);
    }

    #[test]
    fn a_slide_never_pushes_the_near_edge_out() {
        // Wider than the whole work area: flush left, not centred outside.
        let wide = Size::new(900.0, 30.0);
        let anchor = Rect::new(400.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::SLIDE_X,
            wide,
            AREA,
        );
        assert_eq!(r.x, 0.0);
        assert_eq!(r.w, 900.0);
    }

    #[test]
    fn resize_shrinks_to_the_intersection() {
        let anchor = Rect::new(760.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomRight,
            PopupGravity::BottomRight,
            constraint_adjust::RESIZE_X,
            SIZE,
            AREA,
        );
        assert_eq!((r.x, r.w), (800.0, 0.0));

        let anchor = Rect::new(700.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::RESIZE_X,
            Size::new(200.0, 30.0),
            AREA,
        );
        assert_eq!((r.x, r.w), (700.0, 100.0));

        let anchor = Rect::new(100.0, 580.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomLeft,
            PopupGravity::BottomRight,
            constraint_adjust::RESIZE_Y,
            SIZE,
            AREA,
        );
        assert_eq!((r.y, r.h), (600.0, 0.0));
    }

    #[test]
    fn flip_is_tried_before_slide() {
        // Both bits set, and the flip fits: the flip wins and no sliding
        // happens, so the popup keeps touching its anchor.
        let anchor = Rect::new(760.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomRight,
            PopupGravity::BottomRight,
            constraint_adjust::FLIP_X | constraint_adjust::SLIDE_X,
            SIZE,
            AREA,
        );
        assert_eq!(r.x, 700.0);
    }

    #[test]
    fn slide_picks_up_after_a_flip_that_did_not_help() {
        // 900 wide: neither side fits, so the flip reverts and the slide
        // clamps it flush to the near edge.
        let wide = Size::new(900.0, 30.0);
        let anchor = Rect::new(760.0, 100.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::BottomRight,
            PopupGravity::BottomRight,
            constraint_adjust::FLIP_X | constraint_adjust::SLIDE_X,
            wide,
            AREA,
        );
        assert_eq!(r.x, 0.0);
    }

    #[test]
    fn the_work_area_does_not_have_to_start_at_the_origin() {
        // A bar at the top reserves 30px: the area starts at y = 30, and
        // a flip must respect that edge, not the output's.
        let area = Rect::new(0.0, 30.0, 800.0, 570.0);
        let anchor = Rect::new(100.0, 40.0, 40.0, 20.0);
        let r = constrain(
            anchor,
            PopupAnchor::TopLeft,
            PopupGravity::TopRight,
            constraint_adjust::FLIP_Y,
            SIZE,
            area,
        );
        // Up would put it at 10, above the work area: flipped down.
        assert_eq!(r.y, 60.0);
    }

    #[test]
    fn the_no_anchor_fallback_matches_chromiums_defaults() {
        // An empty anchor rect is the trigger; the origin is kept.
        let (rect, anchor, gravity, constraint) =
            fallback(Rect::new(50.0, 60.0, 0.0, 0.0)).expect("empty is the fallback trigger");
        assert_eq!((rect.x, rect.y, rect.w, rect.h), (50.0, 60.0, 1.0, 1.0));
        assert_eq!(anchor, PopupAnchor::TopLeft);
        assert_eq!(gravity, PopupGravity::BottomRight);
        assert_eq!(constraint, constraint_adjust::FLIP_Y);

        // One zero axis is enough.
        assert!(fallback(Rect::new(0.0, 0.0, 10.0, 0.0)).is_some());
        assert!(fallback(Rect::new(0.0, 0.0, -1.0, 5.0)).is_some());
        // A real rectangle is honoured as sent.
        assert!(fallback(ANCHOR).is_none());
    }

    #[test]
    fn the_fallback_places_the_popup_at_the_origin_it_named() {
        let (rect, anchor, gravity, constraint) =
            fallback(Rect::new(50.0, 60.0, 0.0, 0.0)).unwrap();
        let r = constrain(rect, anchor, gravity, constraint, SIZE, AREA);
        assert_eq!((r.x, r.y), (50.0, 60.0));
    }

    #[test]
    fn flips_are_involutions() {
        for a in [
            PopupAnchor::None,
            PopupAnchor::Top,
            PopupAnchor::Bottom,
            PopupAnchor::Left,
            PopupAnchor::Right,
            PopupAnchor::TopLeft,
            PopupAnchor::TopRight,
            PopupAnchor::BottomLeft,
            PopupAnchor::BottomRight,
        ] {
            assert_eq!(flip_anchor_x(flip_anchor_x(a)), a);
            assert_eq!(flip_anchor_y(flip_anchor_y(a)), a);
        }
        for g in [
            PopupGravity::None,
            PopupGravity::Top,
            PopupGravity::Bottom,
            PopupGravity::Left,
            PopupGravity::Right,
            PopupGravity::TopLeft,
            PopupGravity::TopRight,
            PopupGravity::BottomLeft,
            PopupGravity::BottomRight,
        ] {
            assert_eq!(flip_gravity_x(flip_gravity_x(g)), g);
            assert_eq!(flip_gravity_y(flip_gravity_y(g)), g);
        }
    }
}
