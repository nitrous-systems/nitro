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
use nitro_ui::{Color, ColorRole, Palette, Size};
use nitro_wallpaper::{BACKDROP, Paint, Wallpaper, build_with, default_gradient, ppm};

/// The harness's output size.
const OUT: (f32, f32) = (320.0, 240.0);

/// A wallpaper painting `paint`, on the shell socket.
fn wallpaper(paint: &Paint) -> Harness<Wallpaper> {
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
    let mut h = wallpaper(&default_gradient());
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
    // The two stops the *palette* names, since the gradient no longer
    // owns any colours of its own.
    let palette = Palette::default();
    let (top, bottom) = (
        palette.get(ColorRole::DesktopTop),
        palette.get(ColorRole::DesktopBottom),
    );
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
    let mut h = wallpaper(&default_gradient());
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
    let mut h = wallpaper(&Paint::Solid(want));
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
    let mut h = wallpaper(&Paint::Image(px));
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
    let mut h = wallpaper(&default_gradient());
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
    let mut h = wallpaper(&Paint::Solid(Color::rgb(0x20, 0x24, 0x30)));
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
    let mut h = wallpaper(&default_gradient());
    h.settle();
    assert!(
        nitro_ui::introspect::resolve(h.ui(), &format!("window/{BACKDROP}")).is_some(),
        "no widget named {BACKDROP}"
    );
    h.quit();
}

#[test]
fn the_gradient_follows_the_scheme() {
    // The wallpaper is the largest surface on the desktop, so a scheme
    // switch that did not reach it would be the most visible failure
    // there is: dark windows on a light backdrop. The gradient carries
    // no colours of its own precisely so that this cannot happen — it
    // reads `DesktopTop`/`DesktopBottom` at paint time.
    let mut h = wallpaper(&default_gradient());
    h.settle();
    let want = |c: Color| u32::from(c.r) << 16 | u32::from(c.g) << 8 | u32::from(c.b);
    // Two rows near the ends, but not *at* them: the gradient is
    // interpolated, so the exact stop is only at y=0 and y=h-1 and a
    // one-pixel rounding difference there would make this flaky.
    let top_row = 0;
    let bottom_row = 239;

    let light = Palette::light();
    assert_eq!(
        output_pixel(&h, 160, top_row),
        want(light.get(ColorRole::DesktopTop)),
        "the light scheme's top stop"
    );
    assert_eq!(
        output_pixel(&h, 160, bottom_row),
        want(light.get(ColorRole::DesktopBottom)),
        "the light scheme's bottom stop"
    );

    h.ui().set_palette(Palette::dark());
    h.settle();

    let dark = Palette::dark();
    assert_eq!(
        output_pixel(&h, 160, top_row),
        want(dark.get(ColorRole::DesktopTop)),
        "the dark scheme's top stop"
    );
    assert_eq!(
        output_pixel(&h, 160, bottom_row),
        want(dark.get(ColorRole::DesktopBottom)),
        "the dark scheme's bottom stop"
    );
    // And it is still silent afterwards, which is the wallpaper's whole
    // reason for existing in the form it has.
    h.assert_idle(60);
    h.quit();
}

#[test]
fn an_explicit_colour_does_not_follow_the_scheme() {
    // `--color` is the user overriding the desktop. An override that the
    // desktop then overrode back would be no override at all.
    let chosen = Color::rgb(0x20, 0x24, 0x30);
    let mut h = wallpaper(&Paint::Solid(chosen));
    h.settle();
    let want = u32::from(chosen.r) << 16 | u32::from(chosen.g) << 8 | u32::from(chosen.b);
    assert_eq!(output_pixel(&h, 160, 120), want);

    h.ui().set_palette(Palette::dark());
    h.settle();
    assert_eq!(
        output_pixel(&h, 160, 120),
        want,
        "`--color` is the user's own choice and outranks the scheme"
    );
    h.quit();
}

// ---------------------------------------------------------------------
// One surface per output
// ---------------------------------------------------------------------

/// A screenshot of the output named `name`: `(width, height, pixels)`,
/// each pixel `0xRRGGBB`. The harness's own `output_shot` reads only the
/// first output.
fn shot_of(h: &Harness<Wallpaper>, name: &str) -> (u32, u32, Vec<u32>) {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    let stream =
        std::os::unix::net::UnixStream::connect(h.server().control_path()).expect("control");
    let mut conn = BufReader::new(stream);
    conn.get_mut()
        .write_all(format!("shot {name}\n").as_bytes())
        .unwrap();
    let mut header = String::new();
    conn.read_line(&mut header).unwrap();
    let fields: Vec<u32> = header
        .trim_end()
        .strip_prefix("ok ")
        .unwrap_or_else(|| panic!("shot {name}: {header}"))
        .split(' ')
        .map(|v| v.parse().unwrap())
        .collect();
    let (width, height, stride) = (fields[0], fields[1], fields[2]);
    let mut data = vec![0u8; (stride * height) as usize];
    conn.read_exact(&mut data).unwrap();
    let mut px = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let o = (y * stride + x * 4) as usize;
            px.push(u32::from_le_bytes([data[o], data[o + 1], data[o + 2], 0]));
        }
    }
    (width, height, px)
}

/// The output names and sizes, from the control socket's `outputs`.
fn output_names(h: &Harness<Wallpaper>) -> Vec<(String, u32, u32)> {
    h.server()
        .request("outputs\n")
        .iter()
        .filter_map(|l| {
            let mut w = l.split(' ');
            let name = w.next()?.to_owned();
            let mode = w.next()?.split('@').next()?;
            let (x, y) = mode.split_once('x')?;
            Some((name, x.parse().ok()?, y.parse().ok()?))
        })
        .collect()
}

/// Settle until `f` holds.
fn until(h: &mut Harness<Wallpaper>, what: &str, f: impl Fn(&Harness<Wallpaper>) -> bool) {
    for _ in 0..400 {
        if f(h) {
            return;
        }
        h.settle();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

/// The 2×2 red/green/blue/yellow test image.
fn quadrants() -> Paint {
    let mut file = b"P6\n2 2\n255\n".to_vec();
    file.extend_from_slice(&[
        0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0x00,
    ]);
    Paint::Image(ppm::parse_ppm(&file).expect("a P6 file"))
}

#[test]
fn a_second_output_gets_its_own_wallpaper_and_loses_it_on_unplug() {
    // The headline of #657: on two outputs both show wallpaper, on one
    // connection, and an unplug closes the surface that output had.
    let want = Color::rgb(0x20, 0x24, 0x30);
    let mut h = wallpaper(&Paint::Solid(want));
    h.settle();
    until(&mut h, "the snapshot", |h| h.state().outputs().len() == 1);
    assert_eq!(h.state().surface_count(), 1, "one output, one surface");

    assert_eq!(h.server().request_line("plug 400x300\n"), "ok");
    until(&mut h, "the second surface", |h| {
        h.state().surface_count() == 2
    });
    h.settle();
    assert_eq!(h.ui().windows().len(), 2, "two windows in one Ui");
    assert_eq!(h.server().stat("shell_clients"), 1, "on one connection");
    let outs = h.state().surface_outputs();
    assert_ne!(outs[0], outs[1], "one surface on each output");
    let second = h.ui().windows()[1];
    assert_eq!(h.ui().window_size_of(second), Size::new(400.0, 300.0));

    // The first output is still covered. The second's *pixels* are
    // asserted in `a_second_output_is_painted`.
    let (sw, sh, px) = shot_of(&h, &output_names(&h)[0].0);
    for (x, y) in [(0, 0), (sw - 1, 0), (0, sh - 1), (sw / 2, sh / 2)] {
        assert_eq!(px[(y * sw + x) as usize], 0x0020_2430, "({x}, {y})");
    }

    // Unplug: the surface on that output goes, the main one stays.
    assert_eq!(h.server().request_line("unplug\n"), "ok");
    until(&mut h, "the surface to close", |h| {
        h.state().surface_count() == 1
    });
    h.settle();
    assert_eq!(h.ui().windows().len(), 1);
    assert_eq!(h.server().stat("windows"), 1, "and the server agrees");

    // Plug again: back to two. And then quiet.
    assert_eq!(h.server().request_line("plug 640x480\n"), "ok");
    until(&mut h, "a surface again", |h| {
        h.state().surface_count() == 2
    });
    h.settle();
    h.assert_idle(100);
    h.quit();
}

#[test]
fn an_image_is_decoded_once_and_scaled_per_output() {
    // Different sizes on the two outputs: each gets a copy scaled to its
    // own size, from the one decoded source the state holds.
    let paint = quadrants();
    let mut h = wallpaper(&paint);
    h.settle();
    until(&mut h, "the main window fitted", |h| {
        h.state().fitted().first().copied().flatten().is_some()
    });
    let resident = h.state().resident_bytes();
    assert_eq!(resident, 16, "the 2×2 source, once");
    let buffers_one = h.server().stat("buffers");

    assert_eq!(h.server().request_line("plug 400x200\n"), "ok");
    until(&mut h, "the second surface fitted", |h| {
        h.state().fitted().len() == 2 && h.state().fitted()[1].is_some()
    });
    h.settle();
    assert_eq!(
        h.state().fitted(),
        vec![Some((320, 240)), Some((400, 200))],
        "each output's copy is its own size"
    );
    assert_eq!(h.state().scales(), 2, "one scale per output");
    assert_eq!(h.state().resident_bytes(), resident, "still one source");
    assert_eq!(
        h.server().stat("buffers"),
        buffers_one + 1,
        "one per output"
    );

    // The quadrants land in the right place on the first output; the
    // second's are asserted in `a_second_output_is_painted`.
    let (name, _, _) = output_names(&h)[0].clone();
    assert_quadrants(&h, &name);

    // Unplugging drops that output's copy: its buffer is released.
    assert_eq!(h.server().request_line("unplug\n"), "ok");
    until(&mut h, "the surface to close", |h| {
        h.state().surface_count() == 1
    });
    h.settle();
    assert_eq!(h.server().stat("buffers"), buffers_one, "its copy is gone");
    h.assert_idle(100);
    h.quit();
}

/// The four quadrants of [`quadrants`] on the output named `name`,
/// sampled well inside each.
fn assert_quadrants(h: &Harness<Wallpaper>, name: &str) {
    let (sw, sh, px) = shot_of(h, name);
    let at = |x: u32, y: u32| px[(y * sw + x) as usize];
    assert_eq!(at(sw / 8, sh / 8), 0x00ff_0000, "{name} top-left red");
    assert_eq!(
        at(sw * 7 / 8, sh / 8),
        0x0000_ff00,
        "{name} top-right green"
    );
    assert_eq!(
        at(sw / 8, sh * 7 / 8),
        0x0000_00ff,
        "{name} bottom-left blue"
    );
    assert_eq!(
        at(sw * 6 / 8, sh * 6 / 8),
        0x00ff_ff00,
        "{name} bottom-right yellow"
    );
}

#[test]
fn a_second_output_is_painted() {
    // Pixels on the second output, each output's copy scaled to its own
    // size — including the columns left of the first output's width,
    // which the server once painted shifted by the output's origin
    // (#3936).
    let mut h = wallpaper(&quadrants());
    h.settle();
    assert_eq!(h.server().request_line("plug 400x200\n"), "ok");
    until(&mut h, "the second surface fitted", |h| {
        h.state().fitted().len() == 2 && h.state().fitted()[1].is_some()
    });
    h.settle();
    for (name, w, hh) in output_names(&h) {
        let (sw, sh, _) = shot_of(&h, &name);
        assert_eq!((sw, sh), (w, hh));
        assert_quadrants(&h, &name);
    }
    h.quit();
}
