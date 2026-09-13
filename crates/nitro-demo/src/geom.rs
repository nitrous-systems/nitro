//! Follower geometry: where a pointer move puts the follower and its
//! trail, and the outline rects that make the damage visible.
//!
//! # What the follower is for
//!
//! A pointer move must produce *client* pixels, not just the server's
//! cursor, or the latency being measured is only the cursor's. The
//! follower is a 24×24 rect the client moves to the pointer, and the trail
//! is a 2-px frame left at the previous position. Together they are two
//! damage rects per motion — the old follower's and the new one's — which
//! is exactly the shape a widget under the cursor produces, and small
//! enough that the frame cost stays in the noise.
//!
//! # The outlines
//!
//! With `--damage`, the client draws the rects it believes it damaged as
//! four hairline edges each, so a screenshot shows what the server had to
//! repaint. They are the client's *own* claim, drawn from the same numbers
//! it sent — if they and the server's repaint disagree, the screenshot
//! shows it as a smear outside an outline, which is precisely the bug
//! class worth catching.
//!
//! Everything here is pure geometry over [`Rect`], so it is unit-testable
//! without a socket, a server or a screen.

use nitro_core::{Point, Rect};

/// Edge length of the follower square, in logical pixels.
pub const FOLLOWER_SIZE: f32 = 24.0;

/// Thickness of the trail's frame and of a damage outline.
pub const TRAIL_WIDTH: f32 = 2.0;

/// The follower's rect for a pointer at `pos`, centred on it and kept
/// inside a window of `size`.
///
/// Clamped rather than allowed to hang over the edge: a node partly
/// outside its window is clipped by the server, and a damage rect that
/// extends past the window would make the outline test ambiguous about
/// whose job the clipping was.
#[must_use]
pub fn follower_rect(pos: Point, size: (f32, f32)) -> Rect {
    let half = FOLLOWER_SIZE / 2.0;
    let x = (pos.x - half).clamp(0.0, (size.0 - FOLLOWER_SIZE).max(0.0));
    let y = (pos.y - half).clamp(0.0, (size.1 - FOLLOWER_SIZE).max(0.0));
    Rect::new(x, y, FOLLOWER_SIZE, FOLLOWER_SIZE)
}

/// The four edge rects that outline `r` with a `width`-thick hairline,
/// drawn *inside* the rect.
///
/// Four rects rather than one bordered rect because the outline must be
/// drawable by a client that owns exactly four extra nodes: the server's
/// `SetBorder` would work too, but then the demo would be testing the
/// server's border path instead of showing the damage.
///
/// A rect too small to hold two edges collapses to the rect itself, which
/// is the honest answer — there is no inside left.
#[must_use]
pub fn outline(r: Rect, width: f32) -> [Rect; 4] {
    let w = width.max(0.0);
    if r.w <= 2.0 * w || r.h <= 2.0 * w {
        return [r, Rect::EMPTY, Rect::EMPTY, Rect::EMPTY];
    }
    [
        // Top and bottom span the full width; the sides fill the gap
        // between them, so the four rects tile the frame without
        // overlapping — an overlap would double-blend a translucent
        // outline and show up as brighter corners.
        Rect::new(r.x, r.y, r.w, w),
        Rect::new(r.x, r.bottom() - w, r.w, w),
        Rect::new(r.x, r.y + w, w, r.h - 2.0 * w),
        Rect::new(r.right() - w, r.y + w, w, r.h - 2.0 * w),
    ]
}

/// The region a follower move damages: the rect it left and the one it
/// arrived at, both in window-local coordinates.
///
/// Returns one rect when they overlap enough that their union is no
/// bigger than the two apart — the server would merge them anyway (the
/// damage accumulator coalesces), and one rect is one paint-list walk
/// instead of two.
#[must_use]
pub fn moved_damage(from: Rect, to: Rect) -> Vec<Rect> {
    if from.is_empty() {
        return vec![to];
    }
    if from == to {
        return vec![to];
    }
    let union = from.union(&to);
    let separate = from.w * from.h + to.w * to.h;
    if union.w * union.h <= separate {
        vec![union]
    } else {
        vec![from, to]
    }
}

/// Total area of a list of rects, ignoring overlap. Used by the tests and
/// by the damage report.
#[must_use]
pub fn area(rects: &[Rect]) -> f32 {
    rects.iter().map(|r| r.w.max(0.0) * r.h.max(0.0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn the_follower_is_centred_on_the_pointer() {
        let r = follower_rect(Point::new(100.0, 80.0), (800.0, 500.0));
        assert!(approx(r.x, 100.0 - FOLLOWER_SIZE / 2.0));
        assert!(approx(r.y, 80.0 - FOLLOWER_SIZE / 2.0));
        assert!(approx(r.w, FOLLOWER_SIZE) && approx(r.h, FOLLOWER_SIZE));
    }

    #[test]
    fn the_follower_stays_inside_the_window() {
        let size = (800.0, 500.0);
        let tl = follower_rect(Point::new(-50.0, -50.0), size);
        assert!(approx(tl.x, 0.0) && approx(tl.y, 0.0));
        let br = follower_rect(Point::new(10_000.0, 10_000.0), size);
        assert!(approx(br.right(), size.0) && approx(br.bottom(), size.1));
    }

    /// A window smaller than the follower has nowhere to clamp to; the
    /// rect must still be at the origin rather than negative.
    #[test]
    fn a_tiny_window_pins_the_follower_at_the_origin() {
        let r = follower_rect(Point::new(5.0, 5.0), (10.0, 10.0));
        assert!(approx(r.x, 0.0) && approx(r.y, 0.0));
    }

    #[test]
    fn an_outline_tiles_the_frame_without_overlapping() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        let edges = outline(r, TRAIL_WIDTH);
        // Every edge is inside the rect.
        for e in &edges {
            assert!(
                approx(e.intersect(&r).w * e.intersect(&r).h, e.w * e.h),
                "{e:?}"
            );
        }
        // And they do not overlap each other.
        for i in 0..edges.len() {
            for j in (i + 1)..edges.len() {
                assert!(
                    !edges[i].intersects(&edges[j]),
                    "{:?} overlaps {:?}",
                    edges[i],
                    edges[j]
                );
            }
        }
        // Their area is the frame's: outer minus inner.
        let inner = (r.w - 2.0 * TRAIL_WIDTH) * (r.h - 2.0 * TRAIL_WIDTH);
        assert!(approx(area(&edges), r.w * r.h - inner));
    }

    #[test]
    fn an_outline_of_a_rect_too_thin_to_have_an_inside_is_the_rect() {
        let r = Rect::new(0.0, 0.0, 3.0, 3.0);
        let edges = outline(r, TRAIL_WIDTH);
        assert_eq!(edges[0], r);
        assert!(edges[1..].iter().all(Rect::is_empty));
    }

    #[test]
    fn a_move_far_enough_damages_two_rects() {
        let from = Rect::new(0.0, 0.0, 24.0, 24.0);
        let to = Rect::new(400.0, 300.0, 24.0, 24.0);
        let d = moved_damage(from, to);
        assert_eq!(d, vec![from, to]);
        assert!(approx(area(&d), 2.0 * 24.0 * 24.0));
    }

    /// A one-pixel nudge — what a 100 Hz mouse actually produces — must
    /// collapse to a single rect barely bigger than the follower.
    #[test]
    fn a_small_move_collapses_to_one_rect() {
        let from = Rect::new(100.0, 100.0, 24.0, 24.0);
        let to = from.translate(1.0, 1.0);
        let d = moved_damage(from, to);
        assert_eq!(d.len(), 1);
        assert!(approx(d[0].w, 25.0) && approx(d[0].h, 25.0));
    }

    #[test]
    fn the_first_move_has_nothing_to_leave_behind() {
        let to = Rect::new(5.0, 5.0, 24.0, 24.0);
        assert_eq!(moved_damage(Rect::EMPTY, to), vec![to]);
    }

    #[test]
    fn a_move_to_the_same_place_damages_it_once() {
        let r = Rect::new(5.0, 5.0, 24.0, 24.0);
        assert_eq!(moved_damage(r, r), vec![r]);
    }
}
