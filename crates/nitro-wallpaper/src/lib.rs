//! `nitro-wallpaper` — the desktop's backdrop: a `Background`-layer
//! surface anchored to every edge, painting a gradient, a colour or an
//! image.
//!
//! It is the smallest possible shell client, and that is the interesting
//! part. A wallpaper has no input, no timers and nothing to subscribe to:
//! it opens a window on the `Background` layer, anchors it to all four
//! edges, paints once, and then **never sends another byte** until an
//! output changes size. `a_settled_wallpaper_sends_nothing_at_all`
//! asserts exactly that, because a program that sits on screen for the
//! whole session and costs nothing to be there is a claim worth checking
//! rather than assuming.
//!
//! ```console
//! $ nitro-wallpaper                   # the themed gradient
//! $ nitro-wallpaper --color 202430    # one solid colour
//! $ nitro-wallpaper --image bg.ppm    # a picture (binary PPM only)
//! ```
//!
//! # One window, and what "per output" really costs
//!
//! The spec asks for one surface per output. That is **not implementable
//! on this protocol**, for exactly the reason `crates/nitro-bar` records
//! and `docs/shell.md` §Deferred states: `CreateWindow` carries no
//! output, every new window is placed on the primary one, and `SetAnchor`
//! anchors to whichever output its window is already on. N wallpaper
//! windows would all land on the primary output — N stacked backdrops on
//! one screen and none on the others, which is worse than one.
//!
//! So the wallpaper opens **one** window anchored to all edges, and the
//! honest description of what that gets you is: the primary output is
//! covered, and a second output shows the compositor's own background.
//! Closing the gap needs the `output` field on `SetAnchor` that
//! `docs/shell.md` already names as the fix; the README says so where a
//! reader of this crate will find it.
//!
//! # Hotplug is followed without watching anything
//!
//! The wallpaper does **not** subscribe to `Outputs`, and does not need
//! to. The server re-applies an anchor from `sync_outputs` on every mode
//! change, scale change and hotplug, and tells the client the only way a
//! client is ever told about its own geometry — a `Configure`. The
//! toolkit turns that into a relayout and one repaint.
//!
//! That is the whole hotplug story, and it is worth stating because the
//! obvious implementation (ask for `Outputs`, react to `OutputInfo`)
//! would be strictly worse: it subscribes to a stream of events to learn
//! something the `Configure` already said, and it creates a second
//! opinion about the window's size that has to agree with the server's.

pub mod ppm;

use nitro_ui::build::{Built, IntoWidget, StyleBuilder};
use nitro_ui::shell::Surface;
use nitro_ui::widgets::{column, image};
use nitro_ui::{
    App, Color, ColorRole, Constraints, Error, Fill, MeasureCx, PaintCx, Palette, Point, Role,
    Size, Ui, Widget, WidgetId,
};

/// The name the wallpaper registers under, and so the first argument to
/// `hey`.
pub const APP_NAME: &str = "nitro-wallpaper";

/// The `hey`-addressable name of the backdrop itself.
pub const BACKDROP: &str = "backdrop";

/// What the wallpaper paints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Paint {
    /// A vertical gradient between the desktop roles: the default.
    ///
    /// Carries no colours, because it does not own any: the two stops
    /// are [`ColorRole::DesktopTop`] and [`ColorRole::DesktopBottom`],
    /// read at paint time from whatever palette the server last pushed.
    /// That is what makes `theme.scheme = dark` change the wallpaper as
    /// well as everything on top of it — and it is why this variant is a
    /// unit variant where `Solid` is not: `--color` is the user
    /// overriding the desktop, and an override *is* a literal colour.
    Gradient,
    /// One colour everywhere, from `--color`.
    Solid(Color),
    /// An image, stretched to the output.
    Image(ppm::Pixels),
}

/// The themed gradient: the desktop roles, whatever they currently are.
///
/// A function rather than a constant so the call sites read the same as
/// they did when it *was* two colours, and so there is one name to grep
/// for "what does a bare `nitro-wallpaper` paint".
#[must_use]
pub fn default_gradient() -> Paint {
    Paint::Gradient
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// What to paint.
    pub paint: Paint,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            paint: default_gradient(),
        }
    }
}

/// The `--help` text, and what an unknown argument prints.
pub const USAGE: &str = "\
usage: nitro-wallpaper [--color RRGGBB] [--image FILE.ppm]

  --color RRGGBB   paint one solid colour
  --image FILE     paint a binary PPM (P6). There is no PNG or JPEG
                   decoder in this tree on purpose; convert with e.g.
                   `magick in.png out.ppm`.

With no arguments it paints a dark themed gradient.";

/// Parse the command line.
///
/// `read` is the file reader, injected so the parser can be tested
/// without a filesystem and so a missing file is this function's error
/// rather than a panic somewhere else.
///
/// # Errors
/// A message suitable for printing: an unknown flag, a colour that is not
/// six hex digits, a file that cannot be read, or an image that does not
/// decode. Every one of them **refuses to start** rather than falling
/// back to the gradient — a wallpaper that quietly ignored `--image`
/// would look exactly like one whose file was wrong, and the user would
/// go looking in the wrong place.
pub fn parse_args<F>(args: &[String], read: F) -> Result<Options, String>
where
    F: Fn(&str) -> Result<Vec<u8>, String>,
{
    let mut out = Options::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--color" | "--colour" => {
                let v = args.get(i + 1).ok_or("--color needs RRGGBB")?;
                out.paint = Paint::Solid(parse_color(v)?);
                i += 2;
            }
            "--image" => {
                let path = args.get(i + 1).ok_or("--image needs a file")?;
                let bytes = read(path)?;
                let px = ppm::parse_ppm(&bytes).map_err(|e| format!("{path}: {e}"))?;
                out.paint = Paint::Image(px);
                i += 2;
            }
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    Ok(out)
}

/// Parse `RRGGBB`, with or without a leading `#`.
///
/// Six digits, not eight: a wallpaper is opaque by definition, and a
/// translucent one would show the compositor's own background through
/// it. The digits themselves are handed to `nitro_core`'s parser rather
/// than decoded again here — there is one colour syntax on this desktop,
/// and `server.conf` and `--color` have to agree about it.
///
/// # Errors
/// A message naming what was wrong.
pub fn parse_color(s: &str) -> Result<Color, String> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return Err(format!("`{s}` is not RRGGBB (six hex digits)"));
    }
    nitro_ui::palette::parse_color(hex)
        .ok_or_else(|| format!("`{s}` is not RRGGBB (six hex digits)"))
}

/// The widget that *is* the wallpaper: one rectangle, filled.
///
/// A custom widget rather than a `Panel`, because a `Panel`'s background
/// is one solid colour and a gradient is the default. It is also the
/// smallest possible example of writing one: a `measure` that takes
/// whatever it is offered, and a `paint` that emits a single node — and
/// of reading colours from *roles* rather than owning them, which is why
/// the gradient case stores nothing at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backdrop {
    /// The one solid colour, when there is one. `None` is the themed
    /// gradient, whose stops come from the palette at paint time.
    solid: Option<Color>,
}

impl Backdrop {
    /// A backdrop for `paint`.
    ///
    /// [`Paint::Image`] is **not** handled here: the pixels have to reach
    /// the server in a memfd, and the toolkit's
    /// [`Image`](nitro_ui::widgets::Image) widget is the thing that knows
    /// how. [`build_with`] picks between the two.
    #[must_use]
    pub fn new(paint: &Paint) -> Self {
        match paint {
            Paint::Gradient => Self { solid: None },
            Paint::Solid(c) => Self { solid: Some(*c) },
            // An image is a different widget; black is what this would
            // paint if one were somehow asked for here, and black is the
            // right "something went wrong" for a backdrop.
            Paint::Image(_) => Self {
                solid: Some(Color::BLACK),
            },
        }
    }

    /// The fill it would paint at `height` under `palette`. Public for
    /// the tests, which assert on the fill rather than only on pixels — a
    /// gradient that came out as a solid colour still covers the screen.
    #[must_use]
    pub fn fill_at(&self, height: f32, palette: &Palette) -> Fill {
        if let Some(c) = self.solid {
            return Fill::Solid(c);
        }
        Fill::Linear {
            // In the **node's own** space, from its top edge to its
            // bottom: the server recomputes it when the node is resized,
            // so a mode change costs the repaint the `Configure` already
            // caused and nothing more.
            start: Point::new(0.0, 0.0),
            end: Point::new(0.0, height),
            c0: palette.get(ColorRole::DesktopTop),
            c1: palette.get(ColorRole::DesktopBottom),
        }
    }
}

impl<S: 'static> Widget<S> for Backdrop {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        // Whatever it is given: a wallpaper's size is its output's, and
        // the anchor is what decides that.
        constraints.max
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let bounds = cx.bounds;
        let fill = self.fill_at(bounds.h, cx.palette());
        cx.rect(0, bounds, fill, 0.0, (0.0, Color::TRANSPARENT));
    }

    fn role(&self) -> Role {
        Role::Image
    }
}

/// The builder for [`Backdrop`], so it gets the toolkit's style setters
/// (`.name()`, `.width_percent()`) like every other widget.
pub struct BackdropBuilder<S> {
    built: Built<S>,
}

impl<S: 'static> StyleBuilder<S> for BackdropBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for BackdropBuilder<S> {
    fn into_widget(self) -> Built<S> {
        self.built
    }
}

/// A [`Backdrop`] painting `paint`, ready for `ui.build`.
#[must_use]
pub fn backdrop<S: 'static>(paint: &Paint) -> BackdropBuilder<S> {
    BackdropBuilder {
        built: Built::new(Backdrop::new(paint)),
    }
}

/// The wallpaper's state, which is **deliberately almost empty**.
///
/// It records what kind of thing is on screen, and specifically *not*
/// the pixels of it. That is not tidiness: an `--image` wallpaper's
/// pixels are `w * h * 4` bytes — 8 MB at 1920x1080 — and they are handed
/// to the toolkit's [`Image`](nitro_ui::widgets::Image) widget, which
/// uploads them into a memfd and drops its own copy. A state that also
/// held a `Paint::Image` would keep a second copy resident **for the
/// whole session**, for nothing: no callback reads it, because a
/// wallpaper has no callbacks.
///
/// So [`Kind`] is what survives the build, and it is three words wide.
/// There is no other state either — no input, no timers, no
/// subscriptions, nothing that changes — which is what makes "zero
/// traffic after the first commit" achievable rather than aspirational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wallpaper {
    kind: Kind,
}

/// What a wallpaper is showing, without the pixels. See [`Wallpaper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A gradient between two colours.
    Gradient,
    /// One solid colour.
    Solid,
    /// An image, of this size in pixels.
    Image(u32, u32),
}

impl Wallpaper {
    /// A wallpaper showing `paint`.
    ///
    /// Takes the paint by reference and keeps only its [`Kind`]: the
    /// caller still owns the pixels and hands them to the tree, which is
    /// the only place they belong.
    #[must_use]
    pub fn new(paint: &Paint) -> Self {
        let kind = match paint {
            Paint::Gradient => Kind::Gradient,
            Paint::Solid(_) => Kind::Solid,
            Paint::Image(px) => Kind::Image(px.width, px.height),
        };
        Self { kind }
    }

    /// What it is showing.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }
}

impl Default for Wallpaper {
    fn default() -> Self {
        Self::new(&default_gradient())
    }
}

/// Build the tree: one widget, filling the window.
///
/// Public because the tests build the tree the binary builds. `build`
/// takes the paint explicitly rather than reading it out of the state,
/// because a tree builder is handed only the `Ui` — and the alternative
/// (a `Paint` smuggled through a global) would be worse than a parameter.
///
/// # Panics
/// Never in practice — the `attach` names two ids this function has just
/// created, and a fresh id cannot be stale.
pub fn build_with<S: 'static>(ui: &mut Ui<S>, paint: &Paint) -> WidgetId {
    let inner = match paint {
        Paint::Image(px) => ui.build(
            // One clone, and it is unavoidable: the widget takes the
            // pixels by value because it owns them until the upload. It
            // drops them at the first paint, which is why the resident
            // cost of an image wallpaper is the server's mapping and not
            // a copy in this process. See [`Wallpaper`].
            image(px.width, px.height, px.data.clone())
                // No alpha: a wallpaper is opaque by definition, and
                // saying so lets the server skip blending every pixel of
                // the largest node on the screen.
                .opaque()
                .name(BACKDROP)
                .width_percent(1.0)
                .height_percent(1.0),
        ),
        other => ui.build(
            backdrop(other)
                .name(BACKDROP)
                .width_percent(1.0)
                .height_percent(1.0),
        ),
    };
    // The backdrop hangs under a container rather than *being* the root,
    // and the reason is addressing: `introspect::path_of` names the root
    // `window` whatever else it is called, so a named root is a name
    // nothing can resolve — `hey nitro-wallpaper get backdrop value`
    // would find nothing at all. One `Flex` that paints nothing costs a
    // scene group and a `SetBounds` per resize, and buys the same
    // `window/<name>` addressing every other nitro app has.
    let root = ui.build(column().width_percent(1.0).height_percent(1.0));
    ui.attach(root, inner).unwrap();
    root
}

/// Build the default tree: the themed gradient. What a test that does not
/// care which paint it gets uses.
///
/// # Panics
/// Never in practice.
pub fn build(ui: &mut Ui<Wallpaper>) -> WidgetId {
    build_with(ui, &default_gradient())
}

/// Connect, open the backdrop and run the loop.
///
/// # Errors
/// Any connection, wire or `epoll` failure. They are all fatal — and a
/// failure to reach the **shell** socket is the loudest: a wallpaper on
/// the ordinary socket would open an ordinary window in the middle of the
/// desktop, on top of everything, which is the exact opposite of what it
/// is for.
pub fn run(args: &[String]) -> Result<(), Error> {
    let options = match parse_args(args, read_file) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("nitro-wallpaper: {e}");
            std::process::exit(2);
        }
    };
    let paint = options.paint;
    let state = Wallpaper::new(&paint);
    App::shell(APP_NAME)?
        .title("nitro-wallpaper")
        .surface(Surface::wallpaper())
        // A size the anchor immediately overrides; the server decides the
        // real one, which is the point of anchoring to all four edges.
        .size(Size::new(640.0, 480.0))
        // The backdrop paints every pixel of the window itself, so the
        // toolkit's own window background underneath it would be a second
        // full-screen rect, re-sent on every resize and never seen.
        .transparent()
        // `paint` is **moved** into the builder and dropped with it, so
        // an image's pixels exist in this process exactly twice — once in
        // the decoded `Paint` and once in the widget's pending buffer —
        // and neither outlives the first paint. See [`Wallpaper`].
        .run(state, move |ui| build_with(ui, &paint))
}

/// Read a file, as a `String` error rather than an `io::Error`.
fn read_file(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that refuses everything, for the argument tests that do
    /// not involve a file.
    fn no_files(path: &str) -> Result<Vec<u8>, String> {
        Err(format!("{path}: no such file (test)"))
    }

    #[test]
    fn no_arguments_is_the_themed_gradient() {
        let o = parse_args(&[], no_files).expect("the default");
        assert_eq!(o.paint, default_gradient());
        assert!(matches!(o.paint, Paint::Gradient));
    }

    #[test]
    fn a_colour_is_six_hex_digits_with_or_without_a_hash() {
        assert_eq!(parse_color("202430"), Ok(Color::rgb(0x20, 0x24, 0x30)));
        assert_eq!(parse_color("#202430"), Ok(Color::rgb(0x20, 0x24, 0x30)));
        assert_eq!(parse_color(" ff0000 "), Ok(Color::rgb(0xff, 0, 0)));
        for bad in ["", "20243", "2024301", "gggggg", "#12345", "0x202430"] {
            assert!(parse_color(bad).is_err(), "{bad:?} should not parse");
        }
        let o =
            parse_args(&["--color".to_owned(), "202430".to_owned()], no_files).expect("a colour");
        assert_eq!(o.paint, Paint::Solid(Color::rgb(0x20, 0x24, 0x30)));
        // `--colour` too, because half the world spells it that way and
        // the cost of accepting both is one match arm.
        let o =
            parse_args(&["--colour".to_owned(), "202430".to_owned()], no_files).expect("a colour");
        assert_eq!(o.paint, Paint::Solid(Color::rgb(0x20, 0x24, 0x30)));
    }

    #[test]
    fn a_bad_argument_refuses_to_start_rather_than_falling_back() {
        // The whole rule: a wallpaper that quietly ignored a flag would
        // look exactly like one whose file was wrong.
        for args in [
            vec!["--image".to_owned()],
            vec!["--color".to_owned()],
            vec!["--color".to_owned(), "nope".to_owned()],
            vec!["--nonsense".to_owned()],
            vec!["bg.ppm".to_owned()],
            vec!["--image".to_owned(), "missing.ppm".to_owned()],
        ] {
            assert!(parse_args(&args, no_files).is_err(), "{args:?}");
        }
        // And `--help` is an "error" that prints the usage, which is what
        // makes `nitro-wallpaper --help` exit non-zero with the text
        // rather than starting a wallpaper.
        let e = parse_args(&["--help".to_owned()], no_files).expect_err("help");
        assert!(e.contains("usage:"), "{e}");
    }

    #[test]
    fn an_image_argument_decodes_the_file() {
        let mut ppm = b"P6\n1 1\n255\n".to_vec();
        ppm.extend_from_slice(&[0x11, 0x22, 0x33]);
        let o = parse_args(&["--image".to_owned(), "bg.ppm".to_owned()], |_| {
            Ok(ppm.clone())
        })
        .expect("an image");
        let Paint::Image(px) = o.paint else {
            panic!("expected an image");
        };
        assert_eq!((px.width, px.height), (1, 1));
        assert_eq!(px.data, vec![0x33, 0x22, 0x11, 0xff]);
    }

    #[test]
    fn an_undecodable_image_names_the_file_in_the_error() {
        let e = parse_args(&["--image".to_owned(), "bg.ppm".to_owned()], |_| {
            Ok(b"not a ppm at all".to_vec())
        })
        .expect_err("garbage");
        assert!(e.starts_with("bg.ppm:"), "{e}");
    }

    /// Compiles only for a `Copy` type; see the test below.
    fn assert_copy<T: Copy>(_: &T) {}

    #[test]
    fn the_state_does_not_keep_a_copy_of_the_pixels() {
        // The bug this prevents is invisible on a gradient and 8 MB on a
        // 1920x1080 image: the state is handed to `App::run` and lives
        // for the whole session, so a `Paint::Image` in it would be a
        // second resident copy of the picture that nothing ever reads —
        // a wallpaper has no callbacks to read it *with*.
        let mut ppm = b"P6\n2 2\n255\n".to_vec();
        ppm.extend_from_slice(&[0; 12]);
        let px = ppm::parse_ppm(&ppm).expect("a P6 file");
        let w = Wallpaper::new(&Paint::Image(px));
        assert_eq!(w.kind(), Kind::Image(2, 2));
        // `Copy` is the structural proof: a type holding a `Vec` cannot
        // be one, so this stops compiling the moment somebody puts the
        // pixels back into the state.
        assert_copy(&w);
        assert_eq!(
            std::mem::size_of::<Wallpaper>(),
            std::mem::size_of::<Kind>(),
            "the state is its kind and nothing else"
        );
        assert_eq!(Wallpaper::new(&default_gradient()).kind(), Kind::Gradient);
        assert_eq!(
            Wallpaper::new(&Paint::Solid(Color::BLACK)).kind(),
            Kind::Solid
        );
    }

    #[test]
    fn a_gradient_is_in_the_nodes_own_space() {
        // Why it matters: a gradient expressed in the *output's* space
        // would have to be re-sent by the client on every mode change,
        // and the wallpaper's whole claim is that it sends nothing.
        let b = Backdrop::new(&default_gradient());
        let Fill::Linear { start, end, c0, c1 } = b.fill_at(1080.0, &Palette::default()) else {
            panic!("the default is a gradient");
        };
        assert_eq!(start, Point::new(0.0, 0.0));
        assert_eq!(end, Point::new(0.0, 1080.0));
        assert_ne!(c0, c1, "two distinguishable stops");
        // And it follows the node: a taller window gets a taller
        // gradient, not a stretched copy of a short one.
        let Fill::Linear { end, .. } = b.fill_at(240.0, &Palette::default()) else {
            panic!("still a gradient");
        };
        assert_eq!(end, Point::new(0.0, 240.0));
    }

    #[test]
    fn a_solid_colour_is_a_solid_fill_at_any_size() {
        let b = Backdrop::new(&Paint::Solid(Color::rgb(1, 2, 3)));
        for h in [1.0, 240.0, 4096.0] {
            assert_eq!(
                b.fill_at(h, &Palette::default()),
                Fill::Solid(Color::rgb(1, 2, 3))
            );
        }
    }
}
