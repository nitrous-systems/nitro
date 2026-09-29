//! `nitro-wallpaper` — the desktop's backdrop: a `Background`-layer
//! surface anchored to every edge, painting a gradient, a colour or an
//! image.
//!
//! It is the smallest possible shell client, and that is the interesting
//! part. A wallpaper has no input and no timers: it opens a window on the
//! `Background` layer, anchors it to all four edges, paints once, and then
//! **never sends another byte** until an output changes.
//! `a_settled_wallpaper_sends_nothing_at_all` asserts exactly that, because
//! a program that sits on screen for the whole session and costs nothing
//! to be there is a claim worth checking rather than assuming.
//!
//! ```console
//! $ nitro-wallpaper                   # the themed gradient
//! $ nitro-wallpaper --color 202430    # one solid colour
//! $ nitro-wallpaper --image bg.ppm    # a picture (binary PPM only)
//! ```
//!
//! # One surface per output
//!
//! One process, one shell connection, one [`Ui`] — and one backdrop
//! window per connected output, following `crates/nitro-bar`. The main
//! window covers whichever output the server placed it on; every other
//! output gets a window opened with [`Ui::add_surface_window`] and
//! `Surface::wallpaper().anchored(Anchor::fill().on(id))`, which is what
//! `SetAnchor { output }` (#3844) is for.
//!
//! The set follows hotplug by the bar's rule (`reconcile`): **the extra
//! windows' outputs are exactly the last `Outputs` snapshot minus the
//! main window's output**, re-run at every `OutputsEnd` and whenever the
//! server re-places the main window. An `OutputGone` closes that output's
//! window at once.
//!
//! # An image is decoded once and scaled per output
//!
//! The decoded `--image` stays in the state — one copy, see
//! [`Wallpaper`]. Each window gets its own copy scaled to its size in
//! device pixels ([`scale`]), which moves into its [`Image`] widget, goes
//! to the server in a memfd and is dropped client-side at the next paint.
//! Unplugging an output releases that window's buffer. A gradient or a
//! colour is filled by the server at any size and costs nothing per
//! output.
//!
//! # Size changes are still `Configure`-driven
//!
//! Subscribing to `Outputs` decides *which* windows exist, never how big
//! they are. The server re-applies each anchor on every mode change,
//! scale change and hotplug and tells the window with a `Configure`; the
//! toolkit relayouts, and an image window rescales from the source then —
//! from the size the server said, not from the snapshot, so there is one
//! opinion about a window's size.

pub mod ppm;

use nitro_ui::build::{Built, IntoWidget, StyleBuilder};
use nitro_ui::shell::{Anchor, ShellEvent, Surface};
use nitro_ui::widgets::{Image, column, image};
use nitro_ui::{
    App, Color, ColorRole, Constraints, Error, Fill, MeasureCx, PaintCx, Palette, Point, Role,
    Size, Ui, Widget, WidgetId, WindowId,
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
    /// [`Image`] widget is the thing that knows
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

/// The wallpaper's state: what it paints, and one entry per backdrop
/// window.
///
/// For an `--image` wallpaper it also holds the **decoded source**, once.
/// That is a change from the one-window wallpaper, whose state was `Copy`
/// on purpose — nothing read the pixels after the first paint, so keeping
/// them was 8 MB for nothing. With one surface per output something
/// *does* read them: an output plugged in an hour later needs its own
/// scaled copy, and the alternatives (re-reading the file, which can have
/// changed or gone, or up-scaling another output's copy) are worse. So
/// the source is kept, exactly one of it, and the per-output copies are
/// **not**: each is handed to its [`Image`] widget, which uploads it into
/// a memfd and drops it at the next paint. The resident cost per extra
/// output is the server's mapping of that memfd; `docs/budget.md` has
/// the numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct Wallpaper {
    kind: Kind,
    /// The decoded image, for an `--image` wallpaper; `None` otherwise.
    source: Option<ppm::Pixels>,
    /// The backdrop windows, the main one first. See [`reconcile`].
    screens: Vec<Screen>,
    /// The output ids of the last complete `Outputs` snapshot.
    outputs: Vec<u32>,
    /// The snapshot being received, between the `Output` events and the
    /// `OutputsEnd` that makes them `outputs`.
    pending_outputs: Vec<u32>,
    /// How many times an output's copy of the image was scaled. For the
    /// tests: once per output, and again only when its size changes.
    scales: u64,
}

/// One backdrop window: the main one, or another output's.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Screen {
    win: WindowId,
    /// The output it is on: from the main window's `Configure`, or the
    /// output an extra window was anchored to.
    output: Option<u32>,
    /// The widget painting it: a [`Backdrop`] or an [`Image`].
    inner: WidgetId,
    /// The device-pixel size its image was last scaled to.
    fitted: Option<(u32, u32)>,
}

/// What a wallpaper is showing, without the pixels. See [`Wallpaper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The themed gradient.
    Gradient,
    /// One solid colour.
    Solid(Color),
    /// An image, of this size in pixels.
    Image(u32, u32),
}

impl Kind {
    /// The kind of `paint`.
    #[must_use]
    pub fn of(paint: &Paint) -> Self {
        match paint {
            Paint::Gradient => Kind::Gradient,
            Paint::Solid(c) => Kind::Solid(*c),
            Paint::Image(px) => Kind::Image(px.width, px.height),
        }
    }
}

impl Wallpaper {
    /// A wallpaper showing `paint`. Clones an image's pixels; the binary
    /// uses [`Wallpaper::from_paint`], which moves them.
    #[must_use]
    pub fn new(paint: &Paint) -> Self {
        Self::from_paint(paint.clone())
    }

    /// A wallpaper showing `paint`, taking the decoded image — the one
    /// copy this process keeps.
    #[must_use]
    pub fn from_paint(paint: Paint) -> Self {
        let kind = Kind::of(&paint);
        let source = match paint {
            Paint::Image(px) => Some(px),
            Paint::Gradient | Paint::Solid(_) => None,
        };
        Self {
            kind,
            source,
            screens: Vec::new(),
            outputs: Vec::new(),
            pending_outputs: Vec::new(),
            scales: 0,
        }
    }

    /// What it is showing.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The pixel bytes the state keeps: the decoded source and nothing
    /// else, however many outputs there are. For the tests.
    #[must_use]
    pub fn resident_bytes(&self) -> usize {
        self.source.as_ref().map_or(0, |p| p.data.len())
    }

    /// How many backdrop windows are open, the main one included.
    #[must_use]
    pub fn surface_count(&self) -> usize {
        self.screens.len().max(1)
    }

    /// The output of every backdrop window, main first (`None` for one
    /// not yet placed).
    #[must_use]
    pub fn surface_outputs(&self) -> Vec<Option<u32>> {
        self.screens.iter().map(|s| s.output).collect()
    }

    /// The output the main window is on.
    #[must_use]
    pub fn main_output(&self) -> Option<u32> {
        self.screens.first().and_then(|s| s.output)
    }

    /// The output ids of the last `Outputs` snapshot.
    #[must_use]
    pub fn outputs(&self) -> &[u32] {
        &self.outputs
    }

    /// How many per-output copies of the image have been scaled.
    #[must_use]
    pub fn scales(&self) -> u64 {
        self.scales
    }

    /// The device-pixel size each window's image was scaled to, main
    /// first; `None` for a window not yet fitted or a non-image paint.
    #[must_use]
    pub fn fitted(&self) -> Vec<Option<(u32, u32)>> {
        self.screens.iter().map(|s| s.fitted).collect()
    }
}

impl Default for Wallpaper {
    fn default() -> Self {
        Self::new(&default_gradient())
    }
}

/// Scale `src` to `width × height`, bilinearly, sampling pixel centres.
///
/// Bilinear rather than nearest because a photograph is the usual
/// wallpaper and nearest turns its edges into stairs; not a box filter
/// because the common case is an image within 2× of the screen, where
/// bilinear is indistinguishable and a tenth of the code. An image the
/// output's size is returned as-is.
#[must_use]
pub fn scale(src: &ppm::Pixels, width: u32, height: u32) -> ppm::Pixels {
    if (src.width, src.height) == (width, height) || src.data.is_empty() {
        return src.clone();
    }
    let (sw, sh) = (src.width as usize, src.height as usize);
    // Source column/row pairs and the weight of the second, per output
    // column/row: computed once, used for every row.
    let taps = |out: u32, len: usize| -> Vec<(usize, usize, f32)> {
        let step = len as f32 / out as f32;
        (0..out)
            .map(|i| {
                let f = ((i as f32 + 0.5) * step - 0.5).clamp(0.0, (len - 1) as f32);
                let a = f.floor() as usize;
                (a, (a + 1).min(len - 1), f - a as f32)
            })
            .collect()
    };
    let xs = taps(width, sw);
    let ys = taps(height, sh);
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for &(y0, y1, fy) in &ys {
        let (r0, r1) = (&src.data[y0 * sw * 4..], &src.data[y1 * sw * 4..]);
        for &(x0, x1, fx) in &xs {
            for c in 0..4 {
                let p = |row: &[u8], x: usize| f32::from(row[x * 4 + c]);
                let top = p(r0, x0) + (p(r0, x1) - p(r0, x0)) * fx;
                let bottom = p(r1, x0) + (p(r1, x1) - p(r1, x0)) * fx;
                data.push((top + (bottom - top) * fy).round() as u8);
            }
        }
    }
    ppm::Pixels {
        width,
        height,
        data,
    }
}

/// One backdrop window's tree: a container, and the widget that paints.
/// Returns `(root, inner)`.
///
/// An image starts as a **placeholder** that paints nothing (an `image`
/// whose buffer is the wrong length, which the builder documents as
/// drawing nothing): the window's real size is not known until its first
/// `Configure`, and uploading the source only to replace it with the
/// scaled copy one round trip later would be a full-screen memfd for
/// nothing. [`fit`] supplies the pixels.
fn tree<S: 'static>(ui: &mut Ui<S>, kind: Kind) -> (WidgetId, WidgetId) {
    let inner = match kind {
        Kind::Image(..) => ui.build(
            image(1, 1, Vec::new())
                // No alpha: a wallpaper is opaque by definition, and
                // saying so lets the server skip blending every pixel of
                // the largest node on the screen.
                .opaque()
                .name(BACKDROP)
                .width_percent(1.0)
                .height_percent(1.0),
        ),
        Kind::Gradient | Kind::Solid(_) => ui.build(
            BackdropBuilder {
                built: Built::new(Backdrop {
                    solid: match kind {
                        Kind::Solid(c) => Some(c),
                        _ => None,
                    },
                }),
            }
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
    (root, inner)
}

/// Build the tree and wire up the per-output surfaces.
///
/// Public because the tests build the tree the binary builds. Only the
/// *kind* of `paint` is read here — an image's pixels come from the
/// state, where [`Wallpaper::new`] put them.
///
/// # Panics
/// Never in practice — the `attach` names two ids this function has just
/// created, and a fresh id cannot be stale.
pub fn build_with(ui: &mut Ui<Wallpaper>, paint: &Paint) -> WidgetId {
    build_kind(ui, Kind::of(paint))
}

/// [`build_with`], from a [`Kind`]: what the binary uses, since by then
/// the pixels have been moved into the state.
///
/// # Panics
/// Never in practice.
pub fn build_kind(ui: &mut Ui<Wallpaper>, kind: Kind) -> WidgetId {
    let (root, inner) = tree(ui, kind);
    install(ui, inner);
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

/// Subscribe to the outputs and follow them: see [`reconcile`].
fn install(ui: &mut Ui<Wallpaper>, main_inner: WidgetId) {
    ui.on_shell(
        move |s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, ev: &ShellEvent| {
            ensure_main(s, main_inner);
            match ev {
                // Reconciled against the complete snapshot, not output by
                // output: a hotplug re-sends the whole list.
                ShellEvent::Output(info) => s.pending_outputs.push(info.id),
                ShellEvent::OutputsEnd => {
                    s.outputs = std::mem::take(&mut s.pending_outputs);
                    reconcile(s, ui);
                }
                // The snapshot that follows would close it too; closing it
                // here saves the server's migration a pointless rescale.
                ShellEvent::OutputGone(id) => {
                    let gone: Vec<WindowId> = s
                        .screens
                        .iter()
                        .skip(1)
                        .filter(|x| x.output == Some(*id))
                        .map(|x| x.win)
                        .collect();
                    close_screens(s, ui, &gone);
                }
                _ => {}
            }
        },
    );
    // Where the main window landed: the output it must *not* open a second
    // surface on. `Outputs` is answered before the first `Configure`.
    ui.on_window_placed(
        WindowId::MAIN,
        move |s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, out| {
            ensure_main(s, main_inner);
            s.screens[0].output = Some(out);
            reconcile(s, ui);
            fit(s, ui, WindowId::MAIN);
        },
    );
    ui.on_window_resize(
        WindowId::MAIN,
        move |s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, _| {
            ensure_main(s, main_inner);
            fit(s, ui, WindowId::MAIN);
        },
    );
    // Asking subscribes: every hotplug after this arrives unasked. An
    // unprivileged connection would be disconnected for asking, so the
    // capability is checked; and a failure is not fatal — the main
    // window still covers its output.
    if ui.is_shell()
        && let Err(e) = ui.outputs()
    {
        eprintln!("nitro-wallpaper: outputs: {e}");
    }
}

/// Make the main window the first [`Screen`], if it is not yet. Called
/// at the top of every callback, because the build has no state to write
/// into.
fn ensure_main(s: &mut Wallpaper, inner: WidgetId) {
    if s.screens.is_empty() {
        s.screens.push(Screen {
            win: WindowId::MAIN,
            output: None,
            inner,
            fitted: None,
        });
    }
}

/// Make the backdrop windows match the outputs: one on every output in
/// the last snapshot, the main window's excepted — `nitro-bar`'s rule.
///
/// Idempotent, and safe in either order of `OutputsEnd` and the main
/// window's placement: before the main window is placed nothing is done.
/// Also closes a window whose output the main window was migrated
/// *onto*, since the server re-homes an orphaned main window to the
/// primary.
fn reconcile(s: &mut Wallpaper, ui: &mut Ui<Wallpaper>) {
    let Some(main) = s.main_output() else {
        return;
    };
    let stale: Vec<WindowId> = s
        .screens
        .iter()
        .skip(1)
        .filter(|x| {
            x.output
                .is_none_or(|o| o == main || !s.outputs.contains(&o))
        })
        .map(|x| x.win)
        .collect();
    close_screens(s, ui, &stale);
    for out in s.outputs.clone() {
        if out != main && !s.screens.iter().any(|x| x.output == Some(out)) {
            open_screen(s, ui, out);
        }
    }
}

/// Open a backdrop on `output`: the same tree, anchored to that output.
fn open_screen(s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, output: u32) {
    let (root, inner) = tree(ui, s.kind);
    let surface = Surface::wallpaper().anchored(Anchor::fill().on(output));
    // A size the anchor immediately overrides, as in `run`.
    let win = match ui.add_surface_window(
        "nitro-wallpaper",
        Some(Size::new(640.0, 480.0)),
        root,
        surface,
    ) {
        Ok(win) => win,
        Err(e) => {
            // Not fatal: the other outputs are still covered.
            eprintln!("nitro-wallpaper: surface on output {output}: {e}");
            let _ = ui.remove(root);
            return;
        }
    };
    ui.on_window_closed(win, move |s: &mut Wallpaper, _ui: &mut Ui<Wallpaper>| {
        s.screens.retain(|x| x.win != win);
    });
    // Its first `Configure` places it; a mode change resizes it. Either
    // way the image is fitted to what the server said, never to what the
    // `Outputs` snapshot said — one opinion about the window's size.
    ui.on_window_placed(win, move |s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, _| {
        fit(s, ui, win);
    });
    ui.on_window_resize(win, move |s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, _| {
        fit(s, ui, win);
    });
    s.screens.push(Screen {
        win,
        output: Some(output),
        inner,
        fitted: None,
    });
}

/// Close the given extra windows, releasing each one's image buffer —
/// the scaled copy that output had — and forget them.
fn close_screens(s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, wins: &[WindowId]) {
    for win in wins {
        if *win == WindowId::MAIN {
            continue;
        }
        // The buffer belongs to the connection, not the node: destroying
        // the window would leave it mapped in the server until exit.
        if let Some(x) = s.screens.iter().find(|x| x.win == *win)
            && let Ok(img) = ui.widget::<Image>(x.inner)
            && let Some(b) = img.buffer()
        {
            ui.release_buffer(b);
        }
        let _ = ui.remove_window(s, *win);
        s.screens.retain(|x| x.win != *win);
    }
}

/// Give `win`'s image a copy scaled to the window's size in device
/// pixels, unless it already has one that size. Nothing for a gradient or
/// a colour, which the server fills at any size.
fn fit(s: &mut Wallpaper, ui: &mut Ui<Wallpaper>, win: WindowId) {
    let Some(src) = &s.source else {
        return;
    };
    let Some(i) = s.screens.iter().position(|x| x.win == win) else {
        return;
    };
    // An extra window is first placed on the primary at its requested
    // size, and moved to its own output by the anchor one `Configure`
    // later: scaling for the first would be a full-screen copy for
    // nothing.
    if i > 0 && ui.window_output(win) != s.screens[i].output {
        return;
    }
    let size = ui.window_size_of(win);
    let factor = ui.scale_of(win);
    let (w, h) = (
        ((size.w * factor).round() as u32).max(1),
        ((size.h * factor).round() as u32).max(1),
    );
    if s.screens[i].fitted == Some((w, h)) {
        return;
    }
    let px = scale(src, w, h);
    if let Ok(mut img) = ui.widget_mut::<Image>(s.screens[i].inner) {
        // Moved into the widget, which drops it once it is in a memfd.
        img.set_pixels(w, h, px.data);
        s.screens[i].fitted = Some((w, h));
        s.scales += 1;
    }
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
    // The decoded image moves into the state: the one copy kept.
    let state = Wallpaper::from_paint(options.paint);
    let kind = state.kind();
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
        .run(state, move |ui| build_kind(ui, kind))
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

    #[test]
    fn the_state_keeps_exactly_one_copy_of_the_pixels() {
        // One surface per output means a later output needs its own
        // scaled copy, so the decoded source stays — once. The scaled
        // copies go to the `Image` widgets and are not the state's.
        let mut ppm = b"P6\n2 2\n255\n".to_vec();
        ppm.extend_from_slice(&[0; 12]);
        let px = ppm::parse_ppm(&ppm).expect("a P6 file");
        let w = Wallpaper::from_paint(Paint::Image(px));
        assert_eq!(w.kind(), Kind::Image(2, 2));
        assert_eq!(w.resident_bytes(), 16, "the source, and only it");
        // A gradient or a colour keeps no pixels at all.
        assert_eq!(Wallpaper::new(&default_gradient()).kind(), Kind::Gradient);
        assert_eq!(Wallpaper::new(&default_gradient()).resident_bytes(), 0);
        let c = Color::BLACK;
        assert_eq!(Wallpaper::new(&Paint::Solid(c)).kind(), Kind::Solid(c));
        assert_eq!(Wallpaper::new(&Paint::Solid(c)).resident_bytes(), 0);
    }

    #[test]
    fn scaling_keeps_each_quadrant_and_the_exact_size() {
        // Red, green / blue, yellow, as [b, g, r, a].
        let src = ppm::Pixels {
            width: 2,
            height: 2,
            data: vec![
                0, 0, 255, 255, 0, 255, 0, 255, //
                255, 0, 0, 255, 0, 255, 255, 255,
            ],
        };
        for (w, h) in [(320, 240), (200, 150), (7, 3), (1, 1)] {
            let out = scale(&src, w, h);
            assert_eq!((out.width, out.height), (w, h));
            assert_eq!(out.data.len(), (w * h * 4) as usize);
        }
        let big = scale(&src, 100, 100);
        let at = |x: usize, y: usize| &big.data[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4];
        assert_eq!(at(5, 5), &[0, 0, 255, 255], "top-left red");
        assert_eq!(at(94, 5), &[0, 255, 0, 255], "top-right green");
        assert_eq!(at(5, 94), &[255, 0, 0, 255], "bottom-left blue");
        assert_eq!(at(94, 94), &[0, 255, 255, 255], "bottom-right yellow");
        // The same size is the same pixels.
        assert_eq!(scale(&src, 2, 2), src);
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
