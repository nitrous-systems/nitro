//! The overview's window-grid layout (`src/overview.rs`), checked against
//! the invariants the algorithm promises and against the measured GNOME
//! arrangement in `docs/research/overview.md` §3.

// The exact comparisons here are against exact values — a floored
// coordinate against its own floor, the 0.95 cap the code applies with
// `min`, lerp endpoints — so equality is the assertion that means what it
// says; the inexact ones carry an explicit tolerance.
#![allow(clippy::float_cmp)]

use nitro_core::{Point, Rect, Size};
use nitro_scene::WindowKey;
use nitro_server::overview::{self, Slot, Thumb, WINDOW_PREVIEW_MAXIMUM_SCALE};

/// Slack for `f32` rounding when comparing rect edges.
const EPS: f32 = 1e-3;

const SCREEN: Rect = Rect {
    x: 0.0,
    y: 0.0,
    w: 1920.0,
    h: 1080.0,
};

fn key(i: usize) -> WindowKey {
    WindowKey::from_parts(u32::try_from(i).unwrap(), 1)
}

fn thumb(i: usize, w: f32, h: f32, cx: f32, cy: f32) -> Thumb {
    Thumb {
        window: key(i),
        size: Size::new(w, h),
        centre: Point::new(cx, cy),
    }
}

/// `n` windows with sizes and centres from a small deterministic
/// generator, so the counts tests see varied shapes.
fn varied(n: usize) -> Vec<Thumb> {
    let mut s: u32 = 0x9e37_79b9;
    let mut next = |m: u32| {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s % m
    };
    (0..n)
        .map(|i| {
            let w = 200 + next(1500);
            let h = 150 + next(850);
            thumb(i, w as f32, h as f32, next(1920) as f32, next(1080) as f32)
        })
        .collect()
}

/// The invariants every layout must satisfy.
fn check(thumbs: &[Thumb], area: Rect, slots: &[Slot]) {
    assert_eq!(slots.len(), thumbs.len(), "one slot per window");
    for t in thumbs {
        let n = slots.iter().filter(|s| s.window == t.window).count();
        assert_eq!(n, 1, "{:?} placed {n} times", t.window);
    }
    for s in slots {
        let t = thumbs.iter().find(|t| t.window == s.window).unwrap();
        let r = s.rect();
        // Containment.
        assert!(
            r.x >= area.x - EPS
                && r.y >= area.y - EPS
                && r.right() <= area.right() + EPS
                && r.bottom() <= area.bottom() + EPS,
            "{r:?} escapes {area:?}"
        );
        // Whole-pixel position.
        assert_eq!(s.pos.x, s.pos.x.floor());
        assert_eq!(s.pos.y, s.pos.y.floor());
        // The scale is what was applied, and never above the cap.
        assert!(s.scale > 0.0 && s.scale <= WINDOW_PREVIEW_MAXIMUM_SCALE);
        assert!((s.size.w - t.size.w * s.scale).abs() < 0.01);
        assert!((s.size.h - t.size.h * s.scale).abs() < 0.01);
        // Aspect ratio, to within a pixel of rounding once snapped.
        let want_h = s.size.w * t.size.h / t.size.w;
        assert!(
            (s.size.h.round() - want_h).abs() <= 1.0,
            "aspect of {:?}: {:?} vs {:?}",
            s.window,
            s.size,
            t.size
        );
    }
    // Non-overlap.
    for (i, a) in slots.iter().enumerate() {
        for b in &slots[i + 1..] {
            assert!(
                !a.rect().intersects(&b.rect()),
                "{:?} overlaps {:?}",
                a.rect(),
                b.rect()
            );
        }
    }
}

fn run(thumbs: &[Thumb], area: Rect) -> Vec<Slot> {
    let slots = overview::layout(thumbs, area, SCREEN.h);
    check(thumbs, area, &slots);
    slots
}

/// `docs/research/overview.md` §3: four windows on 1920×1080, two rows.
/// Window sizes are the measured thumbnails' aspect ratios at plausible
/// desktop sizes; centres put Firefox and Files on top, left and right.
fn gs320() -> Vec<Thumb> {
    vec![
        // Firefox, 771×374 thumbnail (2.06).
        thumb(0, 1542.0, 748.0, 700.0, 350.0),
        // Files, 390×272 (1.43).
        thumb(1, 780.0, 544.0, 1300.0, 400.0),
        // Software, 766×364 (2.10).
        thumb(2, 1532.0, 728.0, 700.0, 750.0),
        // Weather, 357×227 (1.57).
        thumb(3, 714.0, 454.0, 1300.0, 800.0),
    ]
}

fn slot_of(slots: &[Slot], i: usize) -> Slot {
    *slots.iter().find(|s| s.window == key(i)).unwrap()
}

#[test]
fn gs320_rows_are_bottom_aligned() {
    let thumbs = gs320();
    let slots = run(&thumbs, SCREEN);
    let [ff, files, sw, weather] = [0, 1, 2, 3].map(|i| slot_of(&slots, i));

    // Two rows, by vertical position; left/right kept within each.
    assert!(ff.rect().bottom() <= sw.pos.y && files.rect().bottom() <= weather.pos.y);
    assert!(ff.rect().right() <= files.pos.x);
    assert!(sw.rect().right() <= weather.pos.x);

    // The shape §3 measured: the top row's two thumbnails differ a lot in
    // both height and width...
    assert!(ff.size.h - files.size.h > 80.0, "{ff:?} {files:?}");
    assert!(ff.size.w - files.size.w > 300.0, "{ff:?} {files:?}");
    // ...and share a bottom edge, to within the floor to whole pixels.
    for (a, b) in [(ff, files), (sw, weather)] {
        let d = (a.rect().bottom() - b.rect().bottom()).abs();
        assert!(d < 1.0, "bottoms differ by {d}: {a:?} {b:?}");
    }
    // The smaller windows got the bigger `window_scale` bump.
    assert!(files.scale > ff.scale && weather.scale > sw.scale);
}

#[test]
fn empty_is_empty() {
    assert!(overview::layout(&[], SCREEN, SCREEN.h).is_empty());
}

#[test]
fn degenerate_area_is_empty() {
    let t = [thumb(0, 800.0, 600.0, 400.0, 300.0)];
    assert!(overview::layout(&t, Rect::new(0.0, 0.0, 0.0, 500.0), 1080.0).is_empty());
}

#[test]
fn one_window_is_capped_and_centred() {
    let t = [thumb(0, 800.0, 600.0, 400.0, 300.0)];
    let area = Rect::new(100.0, 50.0, 8000.0, 6000.0);
    let slots = run(&t, area);
    let s = slots[0];
    assert_eq!(s.scale, WINDOW_PREVIEW_MAXIMUM_SCALE);
    assert_eq!(s.size, Size::new(760.0, 570.0));
    // Centred in the area, not filling it.
    let cx = s.pos.x + s.size.w / 2.0;
    let cy = s.pos.y + s.size.h / 2.0;
    assert!((cx - (area.x + area.w / 2.0)).abs() <= 1.0, "{s:?}");
    assert!((cy - (area.y + area.h / 2.0)).abs() <= 1.0, "{s:?}");
}

#[test]
fn one_window_on_the_screen() {
    // Smaller than the screen: `window_scale` >= 1 and the global scale is
    // already at the cap, so the thumbnail is exactly 0.95.
    let slots = run(&[thumb(0, 1280.0, 800.0, 960.0, 540.0)], SCREEN);
    assert_eq!(slots[0].scale, WINDOW_PREVIEW_MAXIMUM_SCALE);
    // Larger than the screen: shrunk to fit, and centred.
    let slots = run(&[thumb(0, 2560.0, 1600.0, 960.0, 540.0)], SCREEN);
    assert!(slots[0].scale < WINDOW_PREVIEW_MAXIMUM_SCALE);
    let s = slots[0].rect();
    assert!((s.x + s.w / 2.0 - 960.0).abs() <= 1.0);
    assert!((s.y + s.h / 2.0 - 540.0).abs() <= 1.0);
}

#[test]
fn two_windows() {
    let t = [
        thumb(0, 1280.0, 800.0, 1200.0, 500.0),
        thumb(1, 1280.0, 800.0, 400.0, 500.0),
    ];
    let slots = run(&t, SCREEN);
    // Side by side, left-to-right by centre.
    assert!(slot_of(&slots, 1).rect().right() <= slot_of(&slots, 0).pos.x);
}

#[test]
fn counts() {
    for n in [1, 2, 3, 5, 9, 16, 25, 40] {
        run(&varied(n), SCREEN);
    }
}

#[test]
fn identical_aspect_ratios() {
    for n in [4, 16, 25] {
        let t: Vec<Thumb> = (0..n)
            .map(|i| {
                thumb(
                    i,
                    1280.0,
                    800.0,
                    (i % 5) as f32 * 300.0,
                    (i / 5) as f32 * 200.0,
                )
            })
            .collect();
        let slots = run(&t, SCREEN);
        // Same window, same bump, same row fit: one scale for all when
        // every row is full.
        if n % 5 == 0 || n == 4 || n == 16 {
            let s0 = slots[0].scale;
            assert!(
                slots.iter().all(|s| (s.scale - s0).abs() < 1e-3),
                "{slots:?}"
            );
        }
    }
}

#[test]
fn wildly_mixed_aspect_ratios() {
    let t = [
        // A browser and a calculator.
        thumb(0, 1920.0, 1080.0, 960.0, 540.0),
        thumb(1, 300.0, 600.0, 1700.0, 400.0),
        thumb(2, 2400.0, 200.0, 800.0, 900.0),
        thumb(3, 100.0, 1000.0, 100.0, 300.0),
    ];
    let slots = run(&t, SCREEN);
    // The calculator is bumped relative to the browser.
    assert!(slot_of(&slots, 1).scale > slot_of(&slots, 0).scale);
}

#[test]
fn window_larger_than_the_screen() {
    let t = [
        thumb(0, 6000.0, 4000.0, 960.0, 540.0),
        thumb(1, 800.0, 600.0, 400.0, 300.0),
    ];
    run(&t, SCREEN);
    run(&t[..1], SCREEN);
}

#[test]
fn work_area_offset() {
    // A bar's exclusive zone off the top: the layout lives in the area.
    let area = Rect::new(0.0, 32.0, 1920.0, 1048.0);
    run(&varied(7), area);
    run(&gs320(), area);
}

#[test]
fn growing_the_area_never_shrinks_a_slot() {
    for thumbs in [gs320(), varied(1), varied(3), varied(9), varied(16)] {
        let mut prev: Option<Vec<Slot>> = None;
        for step in 0..12 {
            let k = 1.0 + step as f32 * 0.25;
            let area = Rect::new(0.0, 0.0, 960.0 * k, 540.0 * k);
            let slots = run(&thumbs, area);
            if let Some(prev) = &prev {
                for s in &slots {
                    let p = prev.iter().find(|p| p.window == s.window).unwrap();
                    assert!(s.scale >= p.scale - 1e-6, "{p:?} -> {s:?} at {area:?}");
                }
            }
            prev = Some(slots);
        }
    }
}

#[test]
fn window_scale_lerp() {
    assert_eq!(overview::window_scale(0.0, 1000.0), 1.5);
    assert_eq!(overview::window_scale(500.0, 1000.0), 1.25);
    assert_eq!(overview::window_scale(1000.0, 1000.0), 1.0);
    // Clamped: taller than the monitor is treated as full height.
    assert_eq!(overview::window_scale(5000.0, 1000.0), 1.0);
}

/// GNOME's read-ahead quirk (`docs/research/overview.md` §2.2, the module
/// doc's "Fidelity"): in the greedy row loop the height bump runs *before*
/// the keep-same-row test, so the window that breaks out to the next row
/// has already raised the height of the row it left.
///
/// `monitor_h = 100` puts every window above the monitor height, so
/// `window_scale` is 1 for all of them and the arithmetic stays plain.
/// A and B (1000×200) sit on top, C (2000×800) below; Σ width = 4000.
///
/// * 1 row: width 4000, height 800. Scale = min((1920-32)/4000,
///   1080/800) = 0.472, space ≈ 0.350.
/// * 2 rows, ideal 2000: A, B fit (1000, then 2000 ≤ 2000). C would make
///   4000 (ratio 2 vs 1), so it breaks out — but has already bumped row 0's
///   `full_height` from 200 to 800. Rows: 800 + 800 = 1600 tall, 2000 wide,
///   2 columns. Scale = min((1920-16)/2000 = 0.952, (1080-64)/1600 =
///   0.635, 0.95) = 0.635, space = 1286·1080/2073600 ≈ 0.670: better on
///   both counts, taken.
/// * 3 rows, ideal 1333: A | B | C, heights 200 + 800 (C's read-ahead
///   again) + 800 = 1800, 1 column. Scale = min(1920/2000, 952/1800 ≈
///   0.529) — worse on both counts, so the search stops at 2 rows.
///
/// Without the quirk, row 0 would be 200 tall, the 2-row grid 1000, and
/// the scale the 0.95 cap (vertical 1016/1000, horizontal 0.952).
///
/// With it, at 0.635: rows are 508 + 64 + 508 = 1080, so row 0 starts at
/// y = 0 and row 1 at 572. Row 0 is 635 + 16 + 635 = 1286 wide, x =
/// (1920-1286)/2 = 317; A and B are 635×127, bottom-aligned in a 508 px
/// row, so y = 508 - 127 = 381 — the 381 px above them is the row height C
/// left behind. Row 1: C is 1270×508 at x = (1920-1270)/2 = 325.
#[test]
fn read_ahead_raises_the_row_left_behind() {
    let t = [
        thumb(0, 1000.0, 200.0, 400.0, 100.0),
        thumb(1, 1000.0, 200.0, 1400.0, 100.0),
        thumb(2, 2000.0, 800.0, 960.0, 700.0),
    ];
    let slots = overview::layout(&t, SCREEN, 100.0);
    check(&t, SCREEN, &slots);
    let [a, b, c] = [0, 1, 2].map(|i| slot_of(&slots, i));
    for s in [a, b, c] {
        assert!((s.scale - 0.635).abs() < 1e-5, "{s:?}");
    }
    assert_eq!(a.pos, Point::new(317.0, 381.0), "{a:?}");
    assert_eq!(b.pos, Point::new(968.0, 381.0), "{b:?}");
    assert_eq!(c.pos, Point::new(325.0, 572.0), "{c:?}");
    assert!((a.size.w - 635.0).abs() < EPS && (a.size.h - 127.0).abs() < EPS);
    assert!((c.size.w - 1270.0).abs() < EPS && (c.size.h - 508.0).abs() < EPS);
}
