//! The wallpaper, driven through a real server on the shell socket.
//!
//! The tests here are about three claims, and each one is the kind that
//! stops being true the moment nothing checks it:
//!
//! * it covers the output — a `Background` surface anchored to all four
//!   edges, with real pixels where the desktop would otherwise be;
//! * it is **silent** after the first commit, which is the whole point of
//!   a program that sits on screen for the entire session;
//! * a mode change reaches it, without it subscribing to anything.

use nitro_core::Rect;
use nitro_ui::shell::Surface;
use nitro_ui::test::Harness;
use nitro_ui::{Color, Size};
use nitro_wallpaper::{BACKDROP, Paint, Wallpaper, build_with, default_gradient, ppm};

/// The harness's output size.
const OUT: (f32, f32) = (320.0, 240.0);

/// A wallpaper painting `paint`, on the shell socket.
fn wallpaper(paint: Paint) -> Harness<Wallpaper> {
    let for_build = paint.clone();
    Harness::shell(
        "nitro-wallpaper",
        Wallpaper::new(paint),
        Surface::wallpaper(),
        Some(Size::new(OUT.0, OUT.1)),
        move |ui| build_with(ui, &for_build),
    )
}

/// The pixel at `(x, y)` of the whole output, `0xRRGGBB`.
fn output_pixel(h: &Harness<Wallpaper>, x: u32, y: u32) -> u32 {
    h.output_shot().pixel(x, y) & 0x00ff_ffff
}

#[test]
fn the_wallpaper_covers_the_whole_output() {
    // The headline. A `Background` surface anchored to all four edges is
    // resized by the server to the output's own rectangle, so the
    // assertion is on the *output* screenshot rather than on the window
    // crop: what matters is that there is no desktop showing anywhere.
    let mut h = wallpaper(default_gradient());
    h.settle();
    let size = h.ui().window_size();
    assert!(
        (size.w - OUT.0).abs() < 1.0 && (size.h - OUT.1).abs() < 1.0,
        "the anchor spanned both axes: {size:?}"
    );
    assert_eq!(
        h.server().stat("exclusive_zones"),
        0,
        "a wallpaper reserves nothing: it *is* the desktop"
    );

    // Four corners and the middle, all painted by us rather than by the
    // compositor's own background — except the bottom-right, where the
    // harness parks the pointer and the software cursor is composited
    // over whatever is underneath.
    let Paint::Gradient(top, bottom) = default_gradient() else {
        panic!("the default is a gradient");
    };
    for (x, y) in [(0, 0), (319, 0), (0, 239), (160, 120), (60, 200)] {
        let px = output_pixel(&h, x, y);
        assert_ne!(px, 0x0000_0000, "nothing at ({x}, {y})");
        // Every pixel is somewhere between the two stops, on every
        // channel: a wallpaper that painted one corner wrong would
        // otherwise pass "it is not black".
        let (lo, hi) = (bottom.r.min(top.r), bottom.r.max(top.r));
        let r = ((px >> 16) & 0xff) as u8;
        assert!(
            r >= lo.saturating_sub(2) && r <= hi.saturating_add(2),
            "({x}, {y}) is outside the gradient: {px:#08x}"
        );
    }
    h.quit();
}

#[test]
fn the_gradient_really_is_a_gradient() {
    // A gradient that came out as a solid colour would still cover the
    // screen and pass every other test here.
    let mut h = wallpaper(default_gradient());
    h.settle();
    let top = output_pixel(&h, 160, 2);
    let bottom = output_pixel(&h, 160, 237);
    assert_ne!(top, bottom, "the top and the bottom differ");
    // And in the right direction: the default is lighter at the top.
    let lum = |p: u32| ((p >> 16) & 0xff) + ((p >> 8) & 0xff) + (p & 0xff);
    assert!(
        lum(top) > lum(bottom),
        "lighter at the top: {top:#08x} vs {bottom:#08x}"
    );
    h.quit();
}

#[test]
fn a_solid_colour_is_painted_exactly() {
    // Exactly, not approximately: this goes through the fill path with
    // no blending and no gradient interpolation, so any difference at all
    // would be a colour-space bug rather than rounding.
    let want = Color::rgb(0x20, 0x24, 0x30);
    let mut h = wallpaper(Paint::Solid(want));
    h.settle();
    // Not the bottom-right corner: the harness parks the pointer there
    // and the software cursor is composited over it.
    for (x, y) in [(0, 0), (319, 0), (0, 239), (160, 120)] {
        assert_eq!(
            output_pixel(&h, x, y),
            0x0020_2430,
            "({x}, {y}) is not the requested colour"
        );
    }
    h.quit();
}

#[test]
fn an_image_is_uploaded_once_and_stretched_to_the_output() {
    // A 2×2 image blown up to 320×240: what this pins down is that the
    // pixels really crossed the memfd and landed in the right *order* —
    // a decoder that got `[b, g, r, a]` backwards would put the red
    // quadrant where the blue one belongs and every other test would pass.
    let mut file = b"P6\n2 2\n255\n".to_vec();
    file.extend_from_slice(&[
        0xff, 0x00, 0x00, // top-left  red
        0x00, 0xff, 0x00, // top-right green
        0x00, 0x00, 0xff, // bottom-left  blue
        0xff, 0xff, 0x00, // bottom-right yellow
    ]);
    let px = ppm::parse_ppm(&file).expect("a P6 file");
    let mut h = wallpaper(Paint::Image(px));
    h.settle();

    // Sampled well inside each quadrant, so the bilinear filter at the
    // seams does not decide the test.
    for (x, y, want, corner) in [
        (20u32, 20u32, 0x00ff_0000u32, "top-left red"),
        (300, 20, 0x0000_ff00, "top-right green"),
        (20, 220, 0x0000_00ff, "bottom-left blue"),
        // Well clear of the parked pointer in the very corner.
        (280, 200, 0x00ff_ff00, "bottom-right yellow"),
    ] {
        assert_eq!(
            output_pixel(&h, x, y),
            want,
            "{corner} is wrong at ({x}, {y})"
        );
    }
    h.quit();
}

#[test]
fn a_settled_wallpaper_sends_nothing_at_all() {
    // The claim the whole crate rests on: a program that is on screen for
    // the entire session and costs nothing to be there. There is nothing
    // to subscribe to and nothing to poll, so "idle" here is stronger
    // than the bar's — not even a timer is armed.
    let mut h = wallpaper(default_gradient());
    h.settle();
    assert_eq!(h.next_timeout(), None, "nothing is armed at all");
    let commits = h.commits();
    h.assert_idle(300);
    assert_eq!(h.commits(), commits, "and no commit was sent");
    h.quit();
}

#[test]
fn a_mode_change_resizes_it_without_it_subscribing_to_anything() {
    // Hotplug, without an `Outputs` subscription: the server re-applies
    // the anchor from `sync_outputs` and tells the client the only way it
    // ever tells a client about its own geometry — a `Configure`. So the
    // wallpaper follows a mode change by doing nothing special at all,
    // which is the point.
    let mut h = wallpaper(Paint::Solid(Color::rgb(0x20, 0x24, 0x30)));
    h.settle();
    h.tap();
    h.clear_tap();

    h.configure(Size::new(200.0, 150.0));
    h.settle();
    assert_eq!(h.ui().window_size(), Size::new(200.0, 150.0));
    // The backdrop was re-laid-out to the new size, which is one
    // `SetBounds` on its node — the fill did not change, so no `SetFill`.
    let ops: Vec<&str> = h.mutations().iter().map(|m| m.op).collect();
    assert!(ops.contains(&"SetBounds"), "it was resized: {ops:?}");
    let id =
        nitro_ui::introspect::resolve(h.ui(), &format!("window/{BACKDROP}")).expect("the backdrop");
    assert_eq!(
        h.ui().window_bounds(id),
        Rect::new(0.0, 0.0, 200.0, 150.0),
        "and it still fills the window"
    );

    // And then it goes quiet again.
    h.assert_idle(150);
    h.quit();
}

#[test]
fn the_backdrop_is_addressable_for_hey() {
    // `hey nitro-wallpaper list` is how you find out whether the
    // wallpaper is running at all, which on a box with a black screen is
    // exactly the question.
    let mut h = wallpaper(default_gradient());
    h.settle();
    assert!(
        nitro_ui::introspect::resolve(h.ui(), &format!("window/{BACKDROP}")).is_some(),
        "no widget named {BACKDROP}"
    );
    h.quit();
}
