//! The widget set: [`Flex`], [`Panel`], [`Label`], [`Button`], [`Icon`] and
//! [`Spacer`].
//!
//! Each is a plain struct with a builder function (`column()`, `label()`,
//! `button()`) and setters reachable through
//! [`WidgetMut`](crate::WidgetMut). Reading one of these is the best
//! introduction to writing your own: `Label` shows a leaf that measures
//! its content, `Flex` a container that delegates to the solver, and
//! `Button` state, painting and event handling together.

use nitro_core::{Color, Point, Rect, Size};
use nitro_wire::msg::Fill;
use nitro_wire::types::{Align, BufferId};

use crate::arena::Dirty;
use crate::build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled, button, key};
use crate::layout::{Constraints, CrossAlign, Direction, MainAlign, ShrinkFloor};
use crate::theme::TextStyle;
use crate::ui::{Ui, WidgetMut};
use crate::widget::{
    Access, EventCx, IconTint, LayoutCx, MeasureCx, PaintCx, Role, TextRun, Widget,
};

// ---------------------------------------------------------------------
// Flex
// ---------------------------------------------------------------------

/// A row or column of children.
///
/// It draws nothing itself: everything about it — direction, gap,
/// padding, alignment — lives in its [`LayoutStyle`](crate::LayoutStyle),
/// which the framework's flex solver reads. That is deliberate; a
/// container that stored its own copy of the layout parameters would have
/// two sources of truth for every `set_gap`.
#[derive(Debug, Default)]
pub struct Flex;

impl<S: 'static> Widget<S> for Flex {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        measure_container(cx, constraints)
    }

    fn role(&self) -> Role {
        Role::Container
    }
}

/// Builder for a [`Flex`].
pub struct FlexBuilder<S> {
    built: Built<S>,
}

impl<S: 'static> StyleBuilder<S> for FlexBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for FlexBuilder<S> {}

impl<S: 'static> IntoWidget<S> for FlexBuilder<S> {
    fn into_widget(self) -> Built<S> {
        self.built
    }
}

/// A vertical stack.
#[must_use]
pub fn column<S: 'static>() -> FlexBuilder<S> {
    flex(Direction::Column)
}

/// A horizontal stack.
#[must_use]
pub fn row<S: 'static>() -> FlexBuilder<S> {
    flex(Direction::Row)
}

fn flex<S: 'static>(direction: Direction) -> FlexBuilder<S> {
    let mut built = Built::new(Flex);
    built.state_mut().style.direction = direction;
    FlexBuilder { built }
}

// ---------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------

/// A background rectangle with a corner radius and an optional border,
/// holding one child (or several — it lays them out like a [`Flex`]).
///
/// Each of the three is `None` by default, meaning "take the theme's",
/// so a panel follows a theme change without being told.
#[derive(Debug, Default)]
pub struct Panel {
    background: Option<Color>,
    radius: Option<f32>,
    border: Option<(f32, Color)>,
}

impl Panel {
    /// The background colour, or `None` for the theme's surface.
    #[must_use]
    pub fn background(&self) -> Option<Color> {
        self.background
    }
}

impl<S: 'static> Widget<S> for Panel {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        measure_container(cx, constraints)
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let fill = self.background.unwrap_or(theme.surface);
        let radius = self.radius.unwrap_or(theme.radius);
        let border = self.border.unwrap_or((theme.border_width, theme.border));
        let bounds = cx.bounds;
        cx.rect(0, bounds, Fill::Solid(fill), radius, border);
    }

    fn role(&self) -> Role {
        Role::Container
    }
}

/// Setters for a live [`Panel`].
impl<S: 'static> WidgetMut<'_, Panel, S> {
    /// Change the background colour.
    pub fn set_background(&mut self, color: Color) {
        self.background = Some(color);
        self.request_paint();
    }

    /// Change the corner radius.
    pub fn set_radius(&mut self, radius: f32) {
        self.radius = Some(radius);
        self.request_paint();
    }

    /// Change the border.
    pub fn set_border(&mut self, width: f32, color: Color) {
        self.border = Some((width, color));
        self.request_paint();
    }
}

/// Builder for a [`Panel`].
pub struct PanelBuilder<S> {
    built: Built<S>,
    panel: Panel,
}

impl<S: 'static> PanelBuilder<S> {
    /// Set the background colour (default: the theme's `surface`).
    #[must_use]
    pub fn background(mut self, color: Color) -> Self {
        self.panel.background = Some(color);
        self
    }

    /// Set the corner radius (default: the theme's).
    #[must_use]
    pub fn radius(mut self, radius: f32) -> Self {
        self.panel.radius = Some(radius);
        self
    }

    /// Set the border (default: the theme's).
    #[must_use]
    pub fn border(mut self, width: f32, color: Color) -> Self {
        self.panel.border = Some((width, color));
        self
    }
}

impl<S: 'static> StyleBuilder<S> for PanelBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for PanelBuilder<S> {}

impl<S: 'static> IntoWidget<S> for PanelBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(self.panel);
        self.built
    }
}

/// A panel with the theme's surface colour, radius and border.
#[must_use]
pub fn panel<S: 'static>() -> PanelBuilder<S> {
    let mut built: Built<S> = Built::new(Panel::default());
    built.state_mut().style.padding = crate::layout::Edges::all(8.0);
    PanelBuilder {
        built,
        panel: Panel::default(),
    }
}

// ---------------------------------------------------------------------
// Label
// ---------------------------------------------------------------------

/// A run of static text.
#[derive(Debug)]
pub struct Label {
    text: String,
    style: Option<TextStyle>,
    color: Option<Color>,
    /// A colour named by *role* rather than by value.
    ///
    /// Separate from `color` and checked first, because the two are
    /// different promises: a literal is "this exact colour whatever the
    /// desktop does", a role is "whatever the desktop calls this". A
    /// label built with `.color(ui.theme().text_disabled)` freezes the
    /// colour the palette had *at build time* and then never moves
    /// again — which is exactly the bug `.color_role(ColorRole::TextDim)`
    /// exists to stop, and why every app in this tree uses the latter.
    color_role: Option<nitro_core::Role>,
    align: Align,
    /// The measurement the last `measure` produced, so `paint` can place
    /// the baseline box without asking the server again.
    metrics: crate::wire::TextMetrics,
    /// The wrap width that measurement was taken at, which `paint` must
    /// reuse: the server learns a wrap width only from `SetText`, so
    /// measuring wrapped and painting unwrapped would reserve two lines
    /// and draw one overflowing line.
    wrap_width: f32,
    /// Whether a string too wide for the box is shortened with an
    /// ellipsis instead of overflowing it. See [`LabelBuilder::elide`].
    elide: bool,
    /// What an eliding label actually paints: the longest prefix of
    /// `text` that fits, plus `…`. Empty for a label that is not
    /// eliding, or whose text fits whole.
    ///
    /// Recomputed only when the width it was computed for changes, which
    /// is what keeps a repaint at a settled width free of `MeasureText`
    /// round trips — the work is proportional to the change, not to the
    /// number of paints.
    elided: Option<String>,
    /// The width `elided` was computed for. `f32::NAN` when nothing has
    /// been computed, so the first comparison always misses.
    elided_at: f32,
    /// How many elision searches this label has run.
    ///
    /// Observable ([`Label::elisions`]) because the memo it counts is not
    /// observable any other way: the toolkit caches measurements by
    /// `(text, style, max_width)`, so a search re-run at an unchanged
    /// width answers out of that cache and costs no wire traffic and no
    /// server-side layout pass. A `text_layouts` census would therefore
    /// pass whether or not the memo existed — which is exactly what it
    /// did when the memo was disabled to check. Counting the searches is
    /// the honest pin.
    elisions: u64,
}

/// What an elided label ends with.
const ELLIPSIS: &str = "\u{2026}";

/// How many characters of the original survive at an eliding label's
/// narrowest.
///
/// The floor an eliding label reports is `…` plus this many characters,
/// and the number is a judgement rather than a measurement: fewer than
/// three and the remnant names nothing (`H…` could be any connector),
/// many more and a crowded row is back to overflowing because the label
/// would not give the space up. Three is what distinguishes `HDM…` from
/// `VGA…`, which is the question a user asks of a truncated label.
const ELIDE_FLOOR_CHARS: usize = 3;

impl Label {
    /// The current text — the whole string, whatever is on screen.
    ///
    /// An elided label paints a prefix; this still answers the full text,
    /// because that is what the label *is* and what an accessibility
    /// client must read. [`Label::painted_text`] is the other question.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// What is actually on screen: the elided prefix when the label is
    /// eliding and does not fit, else the whole text.
    #[must_use]
    pub fn painted_text(&self) -> &str {
        self.elided.as_deref().unwrap_or(&self.text)
    }

    /// Whether this label shortens a string too wide for its box.
    #[must_use]
    pub fn is_eliding(&self) -> bool {
        self.elide
    }

    /// Whether the text on screen right now is a shortened one.
    #[must_use]
    pub fn is_elided(&self) -> bool {
        self.elided.is_some()
    }

    /// How many elision searches this label has run since it was built.
    ///
    /// The observable half of "work ∝ change": a label that re-elided on
    /// every paint would move this on every paint. See the field.
    #[must_use]
    pub fn elisions(&self) -> u64 {
        self.elisions
    }

    /// The explicit text style, if one was set.
    #[must_use]
    pub fn style(&self) -> Option<&TextStyle> {
        self.style.as_ref()
    }

    /// The explicit colour, if one was set.
    #[must_use]
    pub fn color(&self) -> Option<Color> {
        self.color
    }

    /// The colour role, if one was set.
    #[must_use]
    pub fn color_role(&self) -> Option<nitro_core::Role> {
        self.color_role
    }

    fn resolved_style(&self, theme: &crate::Theme) -> TextStyle {
        self.style
            .clone()
            .unwrap_or_else(|| TextStyle::from_theme(theme))
    }

    /// Forget any elision, so the next `measure` recomputes it.
    ///
    /// Called whenever the input to the search changed — the string, the
    /// style — rather than on every paint, which is the whole point:
    /// re-eliding at an unchanged width would put a `MeasureText` round
    /// trip (`log(len)` of them) into a repaint that has nothing new to
    /// say.
    fn invalidate_elision(&mut self) {
        self.elided = None;
        self.elided_at = f32::NAN;
    }

    /// Re-run the elision search if `width` is not what it was last run
    /// at.
    ///
    /// **`width` is the width the label is laid out at, not the width it
    /// was offered.** That distinction is the whole of #3725's review:
    /// the first version of this ran from `measure`, against
    /// `constraints.max.w` — which for a flex child is the *container's*
    /// available inner width, before `solve` takes the overflow back out
    /// of it. The prefix that fit the offer was then painted into the
    /// shrunk box and clipped mid-glyph by the scene, ellipsis and all,
    /// which is exactly the hard truncation the ellipsis exists to
    /// replace. Measured: a 122-px box painting a 205-px string.
    ///
    /// So the search runs from [`Widget::layout`], which is the first
    /// moment a leaf knows its own width, and `measure` does not search
    /// at all — an eliding label's *basis* is its whole text, which is
    /// what lets the solver decide how much to take away.
    ///
    /// The search is the server's own (`Text::elide` in `nitro-server`,
    /// which is what elides a title bar): a binary search over char
    /// boundaries for the longest prefix whose text plus `…` still fits,
    /// costing `log(len)` measurements rather than one per prefix. It
    /// runs in the client only because the client is where the layout
    /// happens; the measuring is the server's, over the wire and through
    /// the measurement cache, so a repeated search on a settled string
    /// costs no round trips either.
    ///
    /// Answers whether anything changed, so the caller can ask for a
    /// repaint only when there is something new to paint.
    fn reelide<S: 'static>(&mut self, ui: &mut Ui<S>, style: &TextStyle, width: f32) -> bool {
        if self.elided_at.to_bits() == width.to_bits() {
            return false;
        }
        self.elided_at = width;
        self.elisions += 1;
        let before = self.elided.clone();
        let whole = ui
            .measure_text(&self.text, style, 0.0)
            .unwrap_or_default()
            .width;
        self.elided = if whole <= width || self.text.is_empty() {
            None
        } else {
            Some(elide_to(ui, style, &self.text, width))
        };
        before != self.elided
    }

    /// The narrowest an eliding label is willing to be: `…` plus the
    /// first [`ELIDE_FLOOR_CHARS`] characters of its text.
    fn elide_floor<S: 'static>(&self, ui: &mut Ui<S>, style: &TextStyle) -> Size {
        let end = self
            .text
            .char_indices()
            .nth(ELIDE_FLOOR_CHARS)
            .map_or(self.text.len(), |(i, _)| i);
        let remnant = if end == self.text.len() {
            self.text.clone()
        } else {
            format!("{}{ELLIPSIS}", &self.text[..end])
        };
        ui.measure_text(&remnant, style, 0.0)
            .unwrap_or_default()
            .size()
    }
}

/// The longest prefix of `text` that, with `…` after it, fits in `width`.
///
/// A free function rather than a method because it is the toolkit's
/// answer to a question about a string, not about a label: a binary
/// search over char boundaries, so no candidate ever splits a code
/// point, and `log(len)` measurements rather than one per prefix.
/// `nitro-server`'s `Text::elide` is the same search over the server's
/// own font state, and the two agree on the result by construction —
/// they ask the same shaper the same questions.
fn elide_to<S: 'static>(ui: &mut Ui<S>, style: &TextStyle, text: &str, width: f32) -> String {
    if width <= 0.0 {
        return ELLIPSIS.to_owned();
    }
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    // `lo` always fits (the empty prefix does, or nothing can) and `hi`
    // never does.
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        let candidate = format!("{}{ELLIPSIS}", &text[..bounds[mid]]);
        let fits = ui
            .measure_text(&candidate, style, 0.0)
            .unwrap_or_default()
            .width
            <= width;
        if fits {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    // Not even one character plus an ellipsis fits. An ellipsis alone is
    // still more honest than half a glyph: it says "there is text here
    // and you cannot see it", which a clipped `H` does not.
    format!("{}{ELLIPSIS}", &text[..bounds[lo]])
}

impl<S: 'static> Widget<S> for Label {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let style = self.resolved_style(cx.theme());
        // A finite width offered by the parent is a wrap width; an
        // unbounded one means "as wide as you like", which is 0 on the
        // wire.
        let max = if constraints.max.w.is_finite() {
            constraints.max.w
        } else {
            0.0
        };
        if self.elide {
            // An eliding label never wraps: it is one line that gets
            // shorter, and asking the server to wrap it as well would
            // reserve two lines for a string this widget has already
            // promised to fit on one.
            self.wrap_width = 0.0;
            // **The basis is the whole text, always — the search does not
            // run here.** What this method is offered is the container's
            // available width, and `solve` has not yet taken the
            // overflow back out of it, so a prefix chosen against that
            // number would be painted into a narrower box and clipped
            // mid-glyph (ellipsis included), which is the truncation the
            // ellipsis exists to replace. The eliding happens in
            // `layout`, at the width this label actually gets.
            //
            // Reporting the whole text as the basis is also what makes
            // the solver's arithmetic right: the deficit it divides is
            // the difference between what the row wants and what it has,
            // and a label that pre-shrank itself would understate it.
            self.metrics = cx.measure_text(&self.text, &style, 0.0).unwrap_or_default();
            // The floor is reported from here rather than left to a
            // `min_width`, because only this widget can compute it: it
            // depends on the server's fonts and on the string.
            let floor = self.elide_floor(cx.ui, &style);
            cx.report_floor(floor);
            return constraints.constrain(self.metrics.size());
        }
        self.wrap_width = max;
        self.metrics = cx.measure_text(&self.text, &style, max).unwrap_or_default();
        constraints.constrain(self.metrics.size())
    }

    /// Elide to the width this label was actually given.
    ///
    /// A leaf's `layout` is the **first moment it knows its own width**:
    /// `measure` is offered the container's available space, and the
    /// flex solver then takes the row's overflow back out of whatever
    /// can give it — an eliding label above all. Nothing re-measures a
    /// leaf afterwards (`Ui::layout_widget` calls `layout` and clears
    /// `LAYOUT` in the same pass), so this is also the *only* moment.
    ///
    /// It requests a **paint**, never a layout. The string it chooses is
    /// by construction no wider than the box it was chosen for, so it
    /// cannot change this label's size, and asking for a layout here
    /// would invite a measure/layout loop for no gain.
    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        if !self.elide {
            return;
        }
        let style = self.resolved_style(cx.theme());
        if self.reelide(cx.ui, &style, bounds.w) {
            cx.ui.mark(cx.id, Dirty::PAINT);
        }
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let color = match (self.color, self.color_role) {
            (Some(c), _) => c,
            (None, Some(role)) => cx.color(role),
            (None, None) => cx.theme().text,
        };
        let style = self.resolved_style(cx.theme());
        let bounds = cx.bounds;
        let run = TextRun::new(&style, color)
            .align(self.align)
            .wrap_at(self.wrap_width);
        let text = self.painted_text().to_owned();
        cx.text(0, bounds, &text, run);
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible(&self) -> Access {
        // The **whole** text, even when a prefix is what is on screen: an
        // accessibility client reads the label, and a screen reader that
        // announced "HDMI-A-1 1920×1080 @ 119…" would be reporting the
        // width of a box rather than the content of a label.
        Access {
            name: Some(self.text.clone()),
            value: Some(self.text.clone()),
            actions: Vec::new(),
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "set_text" | "set_value" => {
                arg.unwrap_or_default().clone_into(&mut self.text);
                self.invalidate_elision();
                cx.request_layout();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Label`].
impl<S: 'static> WidgetMut<'_, Label, S> {
    /// Replace the text. Marks layout *and* paint dirty, because a
    /// different string is almost always a different size.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.text == text {
            return;
        }
        self.text = text;
        self.invalidate_elision();
        self.request_layout();
    }

    /// Replace the text style.
    pub fn set_text_style(&mut self, style: TextStyle) {
        self.style = Some(style);
        self.invalidate_elision();
        self.request_layout();
    }

    /// Turn elision on or off; see [`LabelBuilder::elide`].
    pub fn set_elide(&mut self, on: bool) {
        if self.elide == on {
            return;
        }
        self.elide = on;
        self.invalidate_elision();
        self.request_layout();
    }

    /// Replace the colour. Paint only: a colour change cannot move
    /// anything.
    pub fn set_color(&mut self, color: Color) {
        self.color = Some(color);
        self.request_paint();
    }

    /// Take the colour from a palette role instead of a literal, so it
    /// follows the desktop's scheme.
    pub fn set_color_role(&mut self, role: nitro_core::Role) {
        self.color = None;
        self.color_role = Some(role);
        self.request_paint();
    }

    /// Replace the horizontal alignment inside the label's box.
    pub fn set_align(&mut self, align: Align) {
        self.align = align;
        self.request_paint();
    }
}

/// Builder for a [`Label`].
pub struct LabelBuilder<S> {
    built: Built<S>,
    label: Label,
}

impl<S: 'static> LabelBuilder<S> {
    /// Set the font size in logical pixels.
    #[must_use]
    pub fn size(mut self, px: f32) -> Self {
        let mut s = self.label.style.unwrap_or_default();
        s.size_px = px;
        self.label.style = Some(s);
        self
    }

    /// Set the font family: a name, or one of `sans`, `serif`, `mono`.
    #[must_use]
    pub fn family(mut self, family: impl Into<String>) -> Self {
        let mut s = self.label.style.unwrap_or_default();
        s.family = family.into();
        self.label.style = Some(s);
        self
    }

    /// Set the weight (400 regular, 700 bold).
    #[must_use]
    pub fn weight(mut self, weight: u16) -> Self {
        let mut s = self.label.style.unwrap_or_default();
        s.weight = weight;
        self.label.style = Some(s);
        self
    }

    /// Select an italic face.
    #[must_use]
    pub fn italic(mut self) -> Self {
        let mut s = self.label.style.unwrap_or_default();
        s.italic = true;
        self.label.style = Some(s);
        self
    }

    /// Set the colour to a literal (default: the theme's `text`).
    ///
    /// Prefer [`LabelBuilder::color_role`]: a literal does not follow
    /// the desktop's scheme, and `deploy/lint-colors.sh` will not let
    /// you write one down in the first place.
    #[must_use]
    pub fn color(mut self, color: Color) -> Self {
        self.label.color = Some(color);
        self
    }

    /// Take the colour from a palette role, so it follows the scheme.
    ///
    /// This is what `.color(ui.theme().text_disabled)` should have been:
    /// that call reads the palette **once**, at build time, and the
    /// resulting label keeps its light-scheme grey for ever. A role is
    /// resolved at paint time instead, so a `theme.scheme` switch moves
    /// it with everything else.
    #[must_use]
    pub fn color_role(mut self, role: nitro_core::Role) -> Self {
        self.label.color_role = Some(role);
        self
    }

    /// Set the alignment inside the label's own box.
    #[must_use]
    pub fn align(mut self, align: Align) -> Self {
        self.label.align = align;
        self
    }

    /// Shorten the text with `…` when the box is narrower than the
    /// string, instead of overflowing it.
    ///
    /// A label is the widget with no smaller honest version of itself,
    /// which is why the toolkit's default floor is its measured size
    /// (`docs/ui.md`) — a row of full-width labels overflows rather than
    /// squashing them. This is the opt-in for the label that *does* have
    /// one: it drops characters from the end and says so with an
    /// ellipsis, which is a smaller honest version.
    ///
    /// An eliding label takes part in shrink like a
    /// [`shrink_to_zero`](crate::build::StyleBuilder::shrink_to_zero)
    /// widget, but with a floor of its own: `…` plus its first three
    /// characters, reported from `measure` because only the label can
    /// compute it. So a crowded row narrows it instead of running past
    /// the window, and what survives is the front of the string — the
    /// part that names the thing.
    ///
    /// It also never wraps: an eliding label is one line by definition,
    /// and the two policies contradict each other (a wrapped label gets
    /// *taller* when it is given less width, which is the opposite of
    /// what a row needs). Use one or the other.
    ///
    /// The search runs from `layout`, at the width the label was
    /// actually given, and only when that width changes — so a settled
    /// label has searched exactly once and a repaint costs nothing. It
    /// deliberately does *not* run from `measure`: what `measure` is
    /// offered is the container's available width, before the solver
    /// takes the row's overflow back out of it, and a prefix chosen
    /// against that would be painted into a narrower box and clipped
    /// mid-glyph — ellipsis included, which is the truncation this
    /// exists to replace.
    #[must_use]
    pub fn elide(mut self, on: bool) -> Self {
        self.label.elide = on;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for LabelBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for LabelBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        // The label's text is its *accessible* name (`accessible()` says
        // so), but deliberately not its addressing name: a path built
        // out of a sentence would change every time the sentence did.
        // `.name("message")` is how a label gets a stable path.
        //
        // An eliding label opts out of the content floor here rather
        // than at every call site: it has said it is honestly smaller
        // than its text, and the floor it *does* have is the one it
        // reports from `measure`. A caller who wants a wider one still
        // writes `min_width`, which wins as it always did.
        if self.label.elide {
            self.built.state.style.shrink_floor = crate::layout::ShrinkFloor::Zero;
        }
        self.built.replace_widget(self.label);
        self.built
    }
}

/// A label showing `text`.
#[must_use]
pub fn label<S: 'static>(text: impl Into<String>) -> LabelBuilder<S> {
    let label = Label {
        text: text.into(),
        style: None,
        color: None,
        color_role: None,
        align: Align::Left,
        metrics: crate::wire::TextMetrics::default(),
        wrap_width: 0.0,
        elide: false,
        elided: None,
        elided_at: f32::NAN,
        elisions: 0,
    };
    LabelBuilder {
        built: Built::new(Flex),
        label,
    }
}

// ---------------------------------------------------------------------
// Button
// ---------------------------------------------------------------------

/// What a [`Button`] does when it is activated: it is handed the app's
/// state and the whole tree, and routes by widget id.
type ClickFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>)>;

/// A push button: label, hover/pressed/focused visuals, and a callback
/// that receives the app state and the whole tree.
///
/// It draws an icon in one of **two** modes, which are different things
/// and deliberately kept apart (see [`IconMode`]): *instead of* the
/// label, which is what a toolbar glyph button is, or *in front of* it,
/// which is what a launcher row and a window-list entry are.
pub struct Button<S> {
    text: String,
    /// An icon drawn instead of, or in front of, the label — by name.
    ///
    /// Naming an icon rather than putting a character in the label makes
    /// it the desktop's own artwork, rasterised at the output's scale:
    /// the `≡` this replaced came from whatever font on the box happened
    /// to have U+2630 and was the wrong weight beside everything else.
    /// See `docs/icons.md`. The text is still the accessible name, which
    /// is why both fields exist.
    icon: Option<String>,
    /// Whether [`Button::icon`] replaces the label or leads it.
    icon_mode: IconMode,
    /// How the icon is coloured, or `None` for "the colour the label has
    /// in this state".
    ///
    /// `None` is a distinct state from `Some(IconTint::Role(..))` rather
    /// than a spelling of the default role: it is what makes a *disabled*
    /// icon button's glyph go dim with its label without the caller
    /// restating the role for every state the button has. Naming a role
    /// pins it; [`IconTint::Coloured`] means the artwork's own colours.
    icon_tint: Option<IconTint>,
    /// A second icon name, tried once if the server does not have the
    /// first, with the tint to try it in; see
    /// [`ButtonBuilder::icon_fallback_tinted`].
    icon_fallback: Option<(String, IconTint)>,
    /// Whether that retry has been answered for. As [`Icon`]'s field of
    /// the same name: the once-ness is the `take`, this is the record.
    icon_fell_back: bool,
    /// An explicit icon side in logical pixels, or `None` for "as big as
    /// the label"; see [`ButtonBuilder::icon_size`].
    icon_size: Option<f32>,
    enabled: bool,
    pressed: bool,
    /// A pinned role for the label, or `None` for the one the button's
    /// current state implies; see [`ButtonBuilder::text_role`].
    text_role: Option<nitro_core::Role>,
    style: Option<TextStyle>,
    on_click: Option<ClickFn<S>>,
    /// The **middle**-button callback; see [`ButtonBuilder::on_alt_click`].
    on_alt_click: Option<ClickFn<S>>,
    metrics: crate::wire::TextMetrics,
}

/// Where a [`Button`]'s icon goes relative to its label.
///
/// Two modes rather than one mode with an empty label, because the two
/// have different *measurements* and the difference is visible: a glyph
/// button is a square plus padding and must not reserve room for a word
/// it does not draw, and a row button is `icon + gap + label` and must
/// not clip the word it does. Both are decided in
/// [`Button::shows_icon`], so `measure` and `paint` cannot disagree about
/// which of the three shapes a button is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconMode {
    /// The icon is drawn **instead of** the label, centred in the face.
    ///
    /// The text stays the accessible name — the glyph is for the eye, the
    /// word for everything else — and is what the button falls back to
    /// without [`caps::ICONS`](nitro_wire::types::caps::ICONS).
    Replace,
    /// The icon is drawn **in front of** the label, [`ICON_GAP`] apart.
    ///
    /// Without the capability the button is simply its label, at the
    /// label's own width: a row with a gap where an icon would be is a
    /// worse answer than a row without one.
    Leading,
}

impl<S> std::fmt::Debug for Button<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Button")
            .field("text", &self.text)
            .field("enabled", &self.enabled)
            .field("pressed", &self.pressed)
            .finish_non_exhaustive()
    }
}

impl<S> Button<S> {
    /// The label.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The icon drawn instead of, or in front of, the label — if any.
    #[must_use]
    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    /// Where that icon goes: [`IconMode::Replace`] or
    /// [`IconMode::Leading`].
    #[must_use]
    pub fn icon_mode(&self) -> IconMode {
        self.icon_mode
    }

    /// How the icon is coloured, or `None` when it takes whatever colour
    /// the label has in the button's current state.
    #[must_use]
    pub fn icon_tint(&self) -> Option<IconTint> {
        self.icon_tint
    }

    /// Whether the icon is an **application** icon, painted in its own
    /// colours rather than tinted from the palette.
    #[must_use]
    pub fn is_icon_coloured(&self) -> bool {
        self.icon_tint.is_some_and(IconTint::is_coloured)
    }

    /// The explicit icon side in logical pixels, or `None` when the icon
    /// is sized from the label.
    #[must_use]
    pub fn icon_size(&self) -> Option<f32> {
        self.icon_size
    }

    /// The icon name tried if the server does not have [`Button::icon`],
    /// if any. `None` once it has been taken.
    #[must_use]
    pub fn icon_fallback(&self) -> Option<&str> {
        self.icon_fallback.as_ref().map(|(n, _)| n.as_str())
    }

    /// The tint that fallback is asked for in, which need not be the
    /// icon's own: an application icon usually falls back to a symbolic
    /// one.
    #[must_use]
    pub fn icon_fallback_tint(&self) -> Option<IconTint> {
        self.icon_fallback.as_ref().map(|(_, t)| *t)
    }

    /// Whether that one retry has been taken.
    #[must_use]
    pub fn icon_fell_back(&self) -> bool {
        self.icon_fell_back
    }

    /// Swap in the fallback icon name and its tint, or answer `false` if
    /// there is nothing left to try. As [`Icon::take_fallback`].
    pub(crate) fn take_icon_fallback(&mut self) -> bool {
        if self.icon_fell_back {
            return false;
        }
        self.icon_fell_back = true;
        let Some((name, tint)) = self.icon_fallback.take() else {
            return false;
        };
        self.icon = Some(name);
        self.icon_tint = Some(tint);
        true
    }

    /// Whether the button reacts to input.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a pointer button is currently held on it.
    #[must_use]
    pub fn is_pressed(&self) -> bool {
        self.pressed
    }

    /// The role pinned for the label, or `None` when it follows the
    /// button's state; see [`ButtonBuilder::text_role`].
    #[must_use]
    pub fn text_role(&self) -> Option<nitro_core::Role> {
        self.text_role
    }

    fn resolved_style(&self, theme: &crate::Theme) -> TextStyle {
        self.style
            .clone()
            .unwrap_or_else(|| TextStyle::from_theme(theme))
    }
}

impl<S: 'static> Button<S> {
    /// Run the click callback. The callback is taken out for the call for
    /// the same reason a widget leaves its slot: it is handed the whole
    /// `Ui`, and that includes this button.
    fn activate(&mut self, cx: &mut EventCx<'_, S>) {
        if !self.enabled {
            return;
        }
        cx.report_activation();
        let Some(cb) = self.on_click.take() else {
            return;
        };
        cb(cx.state, cx.ui);
        self.on_click = Some(cb);
    }

    /// Run the middle-button callback, if there is one.
    ///
    /// Not reported as an activation: `report_activation` is what a
    /// watcher sees as "this button was pressed", and a middle click is a
    /// different act with a different meaning (in a task list, *close*
    /// rather than *focus*). Reporting both as the same event would tell
    /// a watcher a lie that is impossible to unpick.
    fn alt_activate(&mut self, cx: &mut EventCx<'_, S>) {
        if !self.enabled {
            return;
        }
        let Some(cb) = self.on_alt_click.take() else {
            return;
        };
        cb(cx.state, cx.ui);
        self.on_alt_click = Some(cb);
    }
}

impl<S: 'static> Button<S> {
    /// Whether this button draws its icon at all, and in which mode.
    ///
    /// Both halves of the question in one place, because **`measure` and
    /// `paint` must never disagree about it**: an icon box is a square, a
    /// label box is not and a leading-icon box is a third shape, so a
    /// button that measured one and painted another would reserve the
    /// wrong space. Without
    /// [`caps::ICONS`](nitro_wire::types::caps::ICONS) the answer is
    /// `None` and the button is its text.
    fn shows_icon(&self, has_icons: bool) -> Option<IconMode> {
        if has_icons && self.icon.is_some() {
            Some(self.icon_mode)
        } else {
            None
        }
    }

    /// The icon's square side, in logical pixels.
    ///
    /// The default ties the icon to the **label**: `max(font size, 16)`,
    /// because in [`IconMode::Replace`] the glyph stands in for the word
    /// and so should be the word's size. An explicit
    /// [`ButtonBuilder::icon_size`] overrides that in every mode — a
    /// caller who pinned one in `Replace` mode meant it — and is what a
    /// leading icon usually needs, since there the icon sits *beside* the
    /// word rather than standing in for it and the two sizes are
    /// unrelated: a launcher row's 16 px text wants a 24 px logo, and a
    /// bar's 13 px window list wants the 16 px its other icons use.
    ///
    /// A method rather than a free function now, because with the
    /// override it is a fact about this button and not only about the
    /// style and the grid.
    fn icon_side(&self, style: &TextStyle) -> f32 {
        self.icon_size
            .unwrap_or_else(|| style.size_px.max(crate::widgets::ICON_SIZE))
    }

    /// The tint the icon takes, given the role the label is taking in
    /// this state.
    ///
    /// The default (`None`) follows the label, which is the whole reason
    /// the field is an `Option`: a disabled icon button whose glyph kept
    /// `ButtonText` looks clickable, which is a lie about what it will
    /// do. An explicit tint — a pinned role, or [`IconTint::Coloured`] —
    /// does not follow it, because an application icon has no role to
    /// take and a caller who named one meant it.
    fn tint(&self, text_role: nitro_core::Role) -> IconTint {
        self.icon_tint.unwrap_or(IconTint::Role(text_role))
    }
}

impl<S: 'static> Widget<S> for Button<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let has_icons = cx.has_icons();
        let theme = cx.theme();
        let (px, py) = theme.button_padding;
        let style = self.resolved_style(theme);
        // An icon button measures to the icon's square box plus the same
        // padding a label gets, and costs **no round trip** — which is
        // the whole reason icons are square by contract.
        //
        // Only when the server actually has icons, though: without them
        // the button paints its label instead, and measuring a square
        // here would reserve a box the text does not fit in.
        if let Some(mode) = self.shows_icon(has_icons) {
            let side = self.icon_side(&style);
            match mode {
                IconMode::Replace => {
                    return constraints.constrain(Size::new(side + px * 2.0, side + py * 2.0));
                }
                IconMode::Leading => {
                    // The label is still measured — this mode draws both —
                    // and the box is icon + gap + label, so the word is
                    // never clipped by a square that was sized for the
                    // glyph alone. The height is the taller of the two,
                    // because an icon larger than the font's line box
                    // must not be cut off by it.
                    self.metrics = cx.measure_text(&self.text, &style, 0.0).unwrap_or_default();
                    let w = side + ICON_GAP + self.metrics.width + px * 2.0;
                    let h = side.max(self.metrics.height) + py * 2.0;
                    return constraints.constrain(Size::new(w, h));
                }
            }
        }
        self.metrics = cx.measure_text(&self.text, &style, 0.0).unwrap_or_default();
        constraints.constrain(Size::new(
            self.metrics.width + px * 2.0,
            self.metrics.height + py * 2.0,
        ))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let has_icons = cx.has_icons();
        let theme = cx.theme();
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        // The colour and the *role* that names it travel together: the
        // label takes the colour, the icon takes the role (the server
        // resolves it at paint time, which is what makes a scheme flip
        // free). Picking them in one place is what stops a disabled icon
        // button from looking enabled — which it did until this was a
        // pair rather than a hard-coded `ButtonText`.
        let (face, text_color, text_role) = if !self.enabled {
            (
                theme.button_disabled,
                theme.text_disabled,
                nitro_core::Role::TextDim,
            )
        } else if self.pressed {
            (
                theme.button_active,
                theme.button_text,
                nitro_core::Role::ButtonText,
            )
        } else if hovered {
            (
                theme.button_hover,
                theme.button_text,
                nitro_core::Role::ButtonText,
            )
        } else {
            (
                theme.button,
                theme.button_text,
                nitro_core::Role::ButtonText,
            )
        };
        let border = if focused {
            (theme.border_width.max(1.0) + 1.0, theme.focus)
        } else {
            (theme.border_width, theme.border)
        };
        let radius = theme.radius;
        // Copied out before the first `cx.rect`: `theme` borrows `cx`, and
        // every paint call below needs `cx` mutably.
        let (pad_x, _) = theme.button_padding;
        let style = self.resolved_style(theme);
        let bounds = cx.bounds;
        // A pinned role overrides whichever the state implied, and takes
        // the *colour* with it: the label and the icon must agree, or a
        // deliberately dimmed row would have a bright glyph beside a grey
        // word. Resolved from the palette rather than from the `Theme`'s
        // handful of named colours, because a pinned role may be any role
        // at all. Taken after `theme` is done with, since `cx.color`
        // borrows `cx` too.
        let (text_color, text_role) = match self.text_role {
            Some(role) => (cx.color(role), role),
            None => (text_color, text_role),
        };

        cx.rect(0, bounds, Fill::Solid(face), radius, border);
        // Without `caps::ICONS` this falls through to the label, and
        // that is the whole point of the guard. Emitting the icon anyway
        // would send `CreateNode { kind: Icon }`, which a server that
        // predates the icon set rejects as a decode error and closes the
        // connection on — so an icon button would *kill the app* against
        // an old server rather than costing a gap. Falling back to the
        // text is strictly better than a blank face, and the button
        // already keeps `text` as its accessible name for exactly this
        // reason: the glyph is for the eye, the word is for everything
        // else.
        let mode = self.shows_icon(has_icons);
        let mut text_box = Rect::new(0.0, 0.0, bounds.w, 0.0);
        let mut align = Align::Center;
        if let Some(mode) = mode
            && let Some(name) = self.icon.clone()
        {
            let side = self.icon_side(&style);
            let tint = self.tint(text_role);
            match mode {
                IconMode::Replace => {
                    // The icon is centred in the face by the scene, which
                    // centres an icon in its node's bounds; the node is
                    // the whole face, so the widget does no arithmetic at
                    // all.
                    cx.icon_tinted(1, bounds, &name, side, tint);
                    return;
                }
                IconMode::Leading => {
                    // Both are drawn, so the widget places them itself:
                    // the icon in a square box at the left padding,
                    // vertically centred in the face, and the label in
                    // what is left. Left-aligned rather than centred,
                    // because a row of these is a *list* — a launcher's
                    // entries, a window list — and centred labels beside
                    // left-aligned icons do not line up down the column.
                    let iy = ((bounds.h - side) / 2.0).max(0.0);
                    let icon_box = Rect::new(pad_x, iy, side, side);
                    cx.icon_tinted(2, icon_box, &name, side, tint);
                    let left = pad_x + side + ICON_GAP;
                    text_box = Rect::new(left, 0.0, (bounds.w - left - pad_x).max(0.0), 0.0);
                    align = Align::Left;
                }
            }
        }
        // The label is aligned horizontally by the text node itself, and
        // centred vertically by its box.
        let h = self.metrics.height.max(1.0);
        let y = ((bounds.h - h) / 2.0).max(0.0);
        text_box.y = y;
        text_box.h = h;
        // A button's label is measured unwrapped and the button is sized
        // around it, so it never wraps.
        let run = TextRun::new(&style, text_color).align(align);
        cx.text(1, text_box, &self.text.clone(), run);
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        match ev {
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                self.pressed = true;
                cx.request_focus();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerUp { pos, button } if *button == button::LEFT => {
                let was = self.pressed;
                self.pressed = false;
                cx.request_paint();
                if was && cx.contains(*pos) {
                    self.activate(cx);
                }
                Handled::Yes
            }
            // The middle button is claimed only when something is
            // listening for it. A button with no `on_alt_click` must let
            // it bubble: an ancestor (a scroller, a list) may want it,
            // and silently swallowing a button nobody handles is how a
            // widget breaks a gesture it has never heard of.
            Event::PointerDown { button, .. }
                if *button == button::MIDDLE && self.on_alt_click.is_some() =>
            {
                Handled::Yes
            }
            Event::PointerUp { pos, button }
                if *button == button::MIDDLE && self.on_alt_click.is_some() =>
            {
                if cx.contains(*pos) {
                    self.alt_activate(cx);
                }
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::SPACE || k.keycode == key::ENTER => {
                self.pressed = true;
                cx.request_paint();
                Handled::Yes
            }
            Event::KeyUp(k) if k.keycode == key::SPACE || k.keycode == key::ENTER => {
                let was = self.pressed;
                self.pressed = false;
                cx.request_paint();
                if was {
                    self.activate(cx);
                }
                Handled::Yes
            }
            // There is no pointer grab in M2, so a release that happens
            // after the pointer has wandered off is never routed here.
            // Dropping `pressed` on the way out is what keeps a button
            // from being left painted active for ever.
            Event::PointerLeave => {
                self.pressed = false;
                cx.request_paint();
                Handled::No
            }
            // Hover and focus change the face, so each needs a repaint —
            // but neither is *consumed*: an ancestor may want to react to
            // the same pointer crossing it.
            Event::PointerEnter { .. } | Event::FocusChanged { .. } => {
                cx.request_paint();
                Handled::No
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.text.clone()),
            // A button's label is its value as well as its name. Without
            // it `hey list` shows a row of buttons with nothing to tell
            // them apart but their paths, since the name column carries
            // the *addressing* name and most buttons have none.
            value: Some(self.text.clone()),
            actions: if self.enabled {
                if self.on_alt_click.is_some() {
                    vec!["click", "activate", "focus", "alt_click"]
                } else {
                    vec!["click", "activate", "focus"]
                }
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            // A scripted click runs `activate`, which is the same method
            // a real release over the button runs — the app's callback
            // cannot tell the two apart, which is the point.
            "click" | "activate" | "press" => {
                self.activate(cx);
                Handled::Yes
            }
            // Scriptable for the same reason `click` is: a task list's
            // close is a middle click, and an action a script cannot
            // reach is a feature `hey` cannot test.
            "alt_click" | "middle_click" => {
                self.alt_activate(cx);
                Handled::Yes
            }
            "set_text" | "set_value" | "set_label" => {
                arg.unwrap_or_default().clone_into(&mut self.text);
                cx.request_layout();
                Handled::Yes
            }
            "set_enabled" => {
                self.enabled = arg != Some("false");
                if !self.enabled {
                    self.pressed = false;
                }
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Button`].
impl<S: 'static> WidgetMut<'_, Button<S>, S> {
    /// Replace the label.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.text == text {
            return;
        }
        self.text = text;
        self.request_layout();
    }

    /// Enable or disable the button.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if !enabled {
            self.pressed = false;
        }
        self.request_paint();
    }

    /// Replace the click callback.
    pub fn set_on_click(&mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) {
        self.on_click = Some(Box::new(f));
    }

    /// Replace the middle-button callback.
    pub fn set_on_alt_click(&mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) {
        self.on_alt_click = Some(Box::new(f));
    }

    /// Show a **leading** symbolic icon, in front of the label, tinted
    /// with the label's own colour.
    ///
    /// Layout, not paint: the box is `icon + gap + label`, so a button
    /// that gained an icon is wider than it was.
    ///
    /// It does **not** switch a [`IconMode::Replace`] button into a
    /// leading one — the mode is set where the button is built, because
    /// changing it changes the button's whole shape and a caller that
    /// merely wanted a different glyph would silently get a different
    /// widget. Use [`WidgetMut::set_icon_mode`] to say so deliberately.
    pub fn set_icon(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.icon.as_deref() == Some(name.as_str()) {
            return;
        }
        self.icon = Some(name);
        self.icon_fell_back = false;
        self.request_layout();
    }

    /// Show a leading **application** icon: the name is resolved in the
    /// machine's icon theme and painted in its own colours.
    ///
    /// The shape a window list and a launcher row want, and the reason
    /// this is a setter as well as a builder method: both create the
    /// button before they know which application it is for.
    pub fn set_icon_coloured(&mut self, name: impl Into<String>) {
        self.icon_tint = Some(IconTint::Coloured);
        self.set_icon(name);
    }

    /// Drop the icon, leaving the label.
    pub fn clear_icon(&mut self) {
        if self.icon.is_none() {
            return;
        }
        self.icon = None;
        self.icon_fell_back = false;
        self.request_layout();
    }

    /// Put the icon in front of the label, or in place of it.
    pub fn set_icon_mode(&mut self, mode: IconMode) {
        if self.icon_mode == mode {
            return;
        }
        self.icon_mode = mode;
        self.request_layout();
    }

    /// Pin the icon's square side in logical pixels, or with `None` size
    /// it from the label again. Layout, because the box changes.
    pub fn set_icon_size(&mut self, px: Option<f32>) {
        if self.icon_size.map(f32::to_bits) == px.map(f32::to_bits) {
            return;
        }
        self.icon_size = px;
        self.request_layout();
    }

    /// Pin the label's palette role, or with `None` let it follow the
    /// button's state again. See [`ButtonBuilder::text_role`].
    pub fn set_text_role(&mut self, role: Option<nitro_core::Role>) {
        if self.text_role == role {
            return;
        }
        self.text_role = role;
        self.request_paint();
    }

    /// Pin the icon's tint, or with `None` let it follow the label's
    /// colour as the button's state changes.
    pub fn set_icon_tint(&mut self, tint: Option<IconTint>) {
        if self.icon_tint == tint {
            return;
        }
        self.icon_tint = tint;
        self.request_paint();
    }

    /// Set (or clear, with `None`) the icon name tried when the server
    /// does not have this one, keeping the icon's current tint, and arm
    /// the one retry again.
    pub fn set_icon_fallback(&mut self, name: Option<String>) {
        let tint = self
            .icon_tint
            .unwrap_or(IconTint::Role(nitro_core::Role::Text));
        self.set_icon_fallback_tinted(name.map(|n| (n, tint)));
    }

    /// As [`WidgetMut::set_icon_fallback`], with the fallback's own tint
    /// — the mixed case, which is the usual one: a coloured application
    /// icon falls back to a **symbolic** glyph, because the symbolic set
    /// is the one that is compiled in and so cannot be missing.
    pub fn set_icon_fallback_tinted(&mut self, fallback: Option<(String, IconTint)>) {
        self.icon_fallback = fallback;
        self.icon_fell_back = false;
    }
}

/// Builder for a [`Button`].
pub struct ButtonBuilder<S> {
    built: Built<S>,
    button: Button<S>,
}

impl<S: 'static> ButtonBuilder<S> {
    /// What to do when the button is activated — clicked, or Space/Enter
    /// while it has focus.
    #[must_use]
    pub fn on_click(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.button.on_click = Some(Box::new(f));
        self
    }

    /// What to do when the button is **middle**-clicked.
    ///
    /// A second, distinct act on the same button — the shape a task list
    /// needs, where a click focuses a window and a middle click closes
    /// it. It is deliberately not reachable from the keyboard: there is
    /// no conventional key for "the other click", and inventing one
    /// (Shift-Enter?) would be a binding nobody would guess. Scripts
    /// reach it through the `alt_click` action instead.
    #[must_use]
    pub fn on_alt_click(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.button.on_alt_click = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.button.enabled = false;
        self
    }

    /// Draw a symbolic icon instead of the label.
    ///
    /// The `text` the button was built with stays its **accessible**
    /// name, so `hey app list` still says what the button does and a
    /// screen reader still has something to read. That is the whole
    /// bargain: the glyph is for the eye, the word is for everything
    /// else.
    #[must_use]
    pub fn icon(mut self, name: impl Into<String>) -> Self {
        self.button.icon = Some(name.into());
        self.button.icon_mode = IconMode::Replace;
        self
    }

    /// Draw a symbolic icon **in front of** the label, tinted with the
    /// label's own colour.
    ///
    /// The other icon mode: [`ButtonBuilder::icon`] replaces the word
    /// with a glyph, this puts one before it. A row in a list wants this
    /// — a launcher entry, a window-list button — because the word is the
    /// content and the icon is the hint, and neither is redundant.
    /// [`ICON_GAP`] separates them and the button measures to the sum, so
    /// nothing is clipped.
    #[must_use]
    pub fn icon_leading(mut self, name: impl Into<String>) -> Self {
        self.button.icon = Some(name.into());
        self.button.icon_mode = IconMode::Leading;
        self
    }

    /// Draw an **application** icon in front of the label, in its own
    /// colours.
    ///
    /// [`ButtonBuilder::icon_leading`] for a name from the machine's XDG
    /// icon theme rather than the desktop's symbolic set — a `.desktop`
    /// file's `Icon=`, a window's app id. The two namespaces do not fall
    /// back to each other, so pair it with
    /// [`ButtonBuilder::icon_fallback_tinted`]: a name that came from
    /// outside the program is a claim about the box, not a fact about it.
    #[must_use]
    pub fn icon_coloured(mut self, name: impl Into<String>) -> Self {
        self.button.icon_tint = Some(IconTint::Coloured);
        self.icon_leading(name)
    }

    /// Pin the icon's tint instead of letting it follow the label.
    ///
    /// By default a button's icon takes whatever role its label takes in
    /// the current state, which is what makes a disabled icon button look
    /// disabled. Pinning is for an icon whose meaning *is* its colour, and
    /// [`IconTint::Coloured`] is what [`ButtonBuilder::icon_coloured`]
    /// sets.
    #[must_use]
    pub fn icon_tint(mut self, tint: IconTint) -> Self {
        self.button.icon_tint = Some(tint);
        self
    }

    /// Pin the icon's square side in logical pixels, instead of sizing it
    /// from the label.
    ///
    /// Without this an icon button's icon is `max(font size, 16)`, which
    /// is right for [`ButtonBuilder::icon`] — the glyph stands in for the
    /// word, so it should be the word's size — and usually wrong for a
    /// **leading** icon, where the icon sits beside the word rather than
    /// replacing it and the two sizes have nothing to do with each other.
    /// A launcher row's 16 px text wants a 24 px application logo (a logo
    /// at 16 px is a smudge); a bar's 13 px window list wants 16 px,
    /// which is what the bar's other icons already use.
    ///
    /// It applies in **every** mode: a caller who pinned a size on a
    /// glyph button meant it, and honouring it only in one mode would be
    /// a setter whose effect depended on a call made somewhere else.
    ///
    /// 16, 24, 32 and 48 are the sizes the artwork is drawn for — the
    /// grid is 16 units, so an integer multiple puts every stroke on a
    /// whole pixel boundary. Anything else works and is properly
    /// anti-aliased; it is simply slightly softer. See `docs/icons.md`.
    ///
    /// Order-independent with respect to [`ButtonBuilder::icon`],
    /// [`icon_leading`](ButtonBuilder::icon_leading) and
    /// [`icon_coloured`](ButtonBuilder::icon_coloured): it sets a field
    /// of its own and never touches the name or the mode.
    #[must_use]
    pub fn icon_size(mut self, px: f32) -> Self {
        self.button.icon_size = Some(px);
        self
    }

    /// A second icon name, tried **once** if the server does not have the
    /// first, in the icon's own tint.
    ///
    /// The same contract as [`IconBuilder::fallback`], on the button's
    /// icon: a `BadIcon` naming this button's icon re-sends with this
    /// name through the normal paint path, and a `BadIcon` for the
    /// fallback itself is the end of it. The mode and size are kept.
    ///
    /// Keeping the tint is right for a theme icon falling back to a more
    /// generic theme icon. For the far more common
    /// application-icon-falls-back-to-symbolic-glyph case, use
    /// [`ButtonBuilder::icon_fallback_tinted`] — the reason is there.
    #[must_use]
    pub fn icon_fallback(mut self, name: impl Into<String>) -> Self {
        let tint = self
            .button
            .icon_tint
            .unwrap_or(IconTint::Role(nitro_core::Role::Text));
        self.button.icon_fallback = Some((name.into(), tint));
        self
    }

    /// A second icon name, tried **once**, in a tint of its own.
    ///
    /// **The mixed case, and it is the common one.** An application icon
    /// names a file in the machine's XDG icon theme, and the whole reason
    /// it needs a fallback is that the machine may not have that theme —
    /// or may have it and not this application. So the icon to reach for
    /// when the lookup fails has to come from the set that *cannot* be
    /// missing: the symbolic one compiled into the server. `window` is
    /// exactly that — it is in the symbolic set and in no icon theme —
    /// so an `icon_fallback("window")` that kept the coloured tint would
    /// earn a second `BadIcon` and leave a blank row for every
    /// application the theme does not have, which is precisely the case
    /// this feature exists to survive.
    ///
    /// ```no_run
    /// # use nitro_ui::{ColorRole, IconTint, widgets::button};
    /// # fn f<S: 'static>() {
    /// // A window-list button: the app's own icon if the box has it, the
    /// // desktop's `window` glyph if it does not.
    /// let b = button::<S>("Firefox")
    ///     .icon_coloured("firefox")
    ///     .icon_fallback_tinted("window", IconTint::Role(ColorRole::Text));
    /// # }
    /// ```
    #[must_use]
    pub fn icon_fallback_tinted(mut self, name: impl Into<String>, tint: IconTint) -> Self {
        self.button.icon_fallback = Some((name.into(), tint));
        self
    }

    /// Pin the label's palette role instead of letting it follow the
    /// button's state.
    ///
    /// By default a button's label takes `ButtonText` — or `TextDim` when
    /// it is disabled — and its icon follows, which is what makes a
    /// disabled icon button look disabled. Pinning is for a button whose
    /// label carries a *meaning* the state machine does not know about,
    /// and there is exactly one such case in this tree: the bar's window
    /// list dims the entry of a **minimized** window.
    ///
    /// It is deliberately not spelled `set_enabled(false)`, which is the
    /// obvious way to grey a button and would be wrong here: a disabled
    /// button ignores clicks, and clicking a minimized entry to bring the
    /// window back is precisely what has to keep working.
    ///
    /// The icon follows the pinned role like it follows any other, unless
    /// [`ButtonBuilder::icon_tint`] pins one of its own.
    #[must_use]
    pub fn text_role(mut self, role: nitro_core::Role) -> Self {
        self.button.text_role = Some(role);
        self
    }

    /// Set the label's font size.
    #[must_use]
    pub fn size(mut self, px: f32) -> Self {
        let mut s = self.button.style.unwrap_or_default();
        s.size_px = px;
        self.button.style = Some(s);
        self
    }
}

impl<S: 'static> StyleBuilder<S> for ButtonBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for ButtonBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let state = self.built.state_mut();
        state.focusable = true;
        // As `Label`: the button's text is its accessible name, and its
        // addressing name is whatever `.name()` said, so `window/ok`
        // keeps naming the same button when its label changes.
        self.built.replace_widget(self.button);
        self.built
    }
}

/// A button labelled `text`.
#[must_use]
pub fn button<S: 'static>(text: impl Into<String>) -> ButtonBuilder<S> {
    let button = Button {
        text: text.into(),
        icon: None,
        icon_mode: IconMode::Replace,
        icon_tint: None,
        icon_fallback: None,
        icon_fell_back: false,
        icon_size: None,
        enabled: true,
        pressed: false,
        text_role: None,
        style: None,
        on_click: None,
        on_alt_click: None,
        metrics: crate::wire::TextMetrics::default(),
    };
    ButtonBuilder {
        built: Built::new(Flex),
        button,
    }
}

// ---------------------------------------------------------------------
// Spacer
// ---------------------------------------------------------------------

/// Empty space. On its own it measures to nothing; with `.grow(1.0)` it
/// is how you push siblings apart.
#[derive(Debug, Default)]
pub struct Spacer;

impl<S: 'static> Widget<S> for Spacer {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        constraints.constrain(Size::ZERO)
    }

    fn role(&self) -> Role {
        Role::Spacer
    }
}

/// Builder for a [`Spacer`].
pub struct SpacerBuilder<S> {
    built: Built<S>,
}

impl<S: 'static> StyleBuilder<S> for SpacerBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for SpacerBuilder<S> {
    fn into_widget(self) -> Built<S> {
        self.built
    }
}

/// Flexible empty space, growing to fill what is left.
#[must_use]
pub fn spacer<S: 'static>() -> SpacerBuilder<S> {
    let mut built = Built::new(Spacer);
    built.state_mut().style.flex_grow = 1.0;
    SpacerBuilder { built }
}

// ---------------------------------------------------------------------
// shared
// ---------------------------------------------------------------------

/// The intrinsic size of a container: the flex solver's own arithmetic,
/// over the children's measurements.
fn measure_container<S: 'static>(cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
    let style = cx.ui.style(cx.id);
    let inner = constraints.loosen().deflate(style.padding);
    let children = cx.children();
    let mut items = Vec::with_capacity(children.len());
    for c in children {
        let cstyle = cx.ui.style(c);
        let avail = inner.deflate(cstyle.margin);
        let basis = cx.measure_child(c, avail);
        let floor = cx.ui.reported_floor(c);
        items.push(crate::layout::FlexItem::new(cstyle, basis).with_floor(floor));
    }
    let main = crate::layout::intrinsic_main(&style, &items);
    let cross = crate::layout::intrinsic_cross(&style, &items);
    let size = style.direction.size(main, cross);
    constraints.constrain(Size::new(
        size.w + style.padding.horizontal(),
        size.h + style.padding.vertical(),
    ))
}

/// Unused import guard: these are re-exported for builders.
const _: () = {
    let _ = (
        MainAlign::Start,
        CrossAlign::Start,
        Dirty::NONE,
        Point::ZERO,
    );
};

// ---------------------------------------------------------------------
// TextField
// ---------------------------------------------------------------------

/// What a [`TextField`] does when its contents change.
type ChangeFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, &str)>;

/// A single line of editable text: caret, selection, click-to-place and
/// horizontal scrolling.
///
/// Two things about it are worth stating, because they are the reasons
/// it looks the way it does.
///
/// **It does not re-measure on every keystroke.** `measure` sizes the
/// field from the theme's font and its own width style, not from its
/// contents, so typing never re-lays out the tree — it repaints one
/// widget. The one measurement it does need is *where the caret is*, and
/// [`Ui::cursor_positions`](crate::Ui::cursor_positions) answers that
/// from the same cache the text measurement uses.
///
/// **Overflow scrolls by composition, not by repaint.** The text node
/// hangs under a clipping group, and a caret past the right edge moves
/// that group's transform. Scrolling a long line is one `SetTransform`
/// and no `SetText`.
pub struct TextField<S> {
    text: String,
    placeholder: String,
    /// Caret position, as a byte offset into `text`.
    cursor: usize,
    /// The other end of the selection; equal to `cursor` when there is
    /// none.
    anchor: usize,
    enabled: bool,
    style: Option<TextStyle>,
    on_change: Option<ChangeFn<S>>,
    on_submit: Option<ChangeFn<S>>,
    metrics: crate::wire::TextMetrics,
    /// Cursor x positions for the current string, from the server.
    cursors: Vec<(u32, f32)>,
    /// How far the visible window has scrolled right, in pixels.
    scroll: f32,
    /// Inner width the last paint used, for the scroll arithmetic.
    view_width: f32,
}

impl<S> std::fmt::Debug for TextField<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextField")
            .field("text", &self.text)
            .field("cursor", &self.cursor)
            .field("anchor", &self.anchor)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// Paint slots of a [`TextField`]: background, clipping group, the text
/// inside it, the selection behind the text, and the caret.
mod field_slot {
    /// The field's background and border.
    pub(super) const BACKGROUND: crate::widget::Slot = 0;
    /// The clipping group holding everything that scrolls.
    pub(super) const VIEW: crate::widget::Slot = 1;
    /// Selection highlight, inside the view.
    pub(super) const SELECTION: crate::widget::Slot = 2;
    /// The text itself, inside the view.
    pub(super) const TEXT: crate::widget::Slot = 3;
    /// The caret, inside the view.
    pub(super) const CARET: crate::widget::Slot = 4;
}

impl<S> TextField<S> {
    /// The current contents.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret position, as a byte offset.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected range, as byte offsets `(start, end)`; empty when
    /// there is no selection.
    #[must_use]
    pub fn selection(&self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }

    /// The selected text.
    #[must_use]
    pub fn selected_text(&self) -> &str {
        let (a, b) = self.selection();
        &self.text[a..b]
    }

    /// Whether the field accepts input.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The placeholder shown while the field is empty.
    #[must_use]
    pub fn placeholder(&self) -> &str {
        &self.placeholder
    }

    /// How far the view has scrolled right, in logical pixels.
    #[must_use]
    pub fn scroll_offset(&self) -> f32 {
        self.scroll
    }

    fn resolved_style(&self, theme: &crate::Theme) -> TextStyle {
        self.style
            .clone()
            .unwrap_or_else(|| TextStyle::from_theme(theme))
    }

    /// The x of byte offset `at`, from the cursor table.
    fn x_of(&self, at: usize) -> f32 {
        let at = at as u32;
        // The table is in increasing offset order; the last entry at or
        // before `at` is the answer. A string the server has not
        // measured yet has an empty table and everything sits at 0,
        // which is correct for an empty field and corrected by the next
        // measurement otherwise.
        let mut x = 0.0;
        for (offset, cx) in &self.cursors {
            if *offset <= at {
                x = *cx;
            } else {
                break;
            }
        }
        x
    }

    /// The byte offset nearest to `x`, for click-to-place.
    fn offset_at(&self, x: f32) -> usize {
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for (offset, cx) in &self.cursors {
            let d = (cx - x).abs();
            if d < best_d {
                best_d = d;
                best = *offset as usize;
            }
        }
        best.min(self.text.len())
    }

    /// The next character boundary after `at`, or `at` at the end.
    fn next_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    /// The previous character boundary before `at`, or 0 at the start.
    fn prev_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .chars()
            .next_back()
            .map_or(0, |c| at - c.len_utf8())
    }

    /// Replace the selection (or nothing) with `s` and put the caret
    /// after it. Returns whether the text changed.
    fn replace_selection(&mut self, s: &str) -> bool {
        let (a, b) = self.selection();
        if a == b && s.is_empty() {
            return false;
        }
        self.text.replace_range(a..b, s);
        self.cursor = a + s.len();
        self.anchor = self.cursor;
        true
    }

    /// Keep the caret inside the visible window by scrolling the view.
    fn scroll_to_cursor(&mut self) {
        let caret = self.x_of(self.cursor);
        let width = self.view_width;
        if width <= 0.0 {
            return;
        }
        if caret - self.scroll > width {
            self.scroll = caret - width;
        }
        if caret < self.scroll {
            self.scroll = caret;
        }
        // Never scroll past the text: a field that is shorter than its
        // box shows its left edge, whatever the caret did before.
        let max = (self.metrics.width - width).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }
}

impl<S: 'static> TextField<S> {
    /// Refresh the cursor table for the current string.
    fn remeasure(&mut self, ui: &mut Ui<S>) {
        let style = self.resolved_style(ui.theme());
        self.metrics = ui.measure_text(&self.text, &style, 0.0).unwrap_or_default();
        self.cursors = ui.cursor_positions(&self.text, &style).unwrap_or_default();
    }

    /// Run `on_change`, taken out for the call as a button's `on_click`
    /// is: it is handed the whole tree, including this field.
    fn fire(&mut self, cx: &mut EventCx<'_, S>, submit: bool) {
        let slot = if submit {
            &mut self.on_submit
        } else {
            &mut self.on_change
        };
        let Some(cb) = slot.take() else { return };
        cb(cx.state, cx.ui, &self.text.clone());
        if submit {
            self.on_submit = Some(cb);
        } else {
            self.on_change = Some(cb);
        }
    }

    /// Everything an edit has to do afterwards: re-measure, keep the
    /// caret visible, repaint and tell the app.
    fn after_edit(&mut self, cx: &mut EventCx<'_, S>) {
        self.remeasure(cx.ui);
        self.scroll_to_cursor();
        cx.request_paint();
        self.fire(cx, false);
    }

    /// Handle one key. Returns whether it was consumed.
    fn key(&mut self, cx: &mut EventCx<'_, S>, k: &crate::event::KeyEvent) -> bool {
        let shift = k.shift();
        let (sel_a, sel_b) = self.selection();
        match k.keycode {
            key::LEFT => {
                let to = if sel_a != sel_b && !shift {
                    sel_a
                } else {
                    self.prev_boundary(self.cursor)
                };
                self.cursor = to;
                if !shift {
                    self.anchor = to;
                }
                self.scroll_to_cursor();
                cx.request_paint();
                true
            }
            key::RIGHT => {
                let to = if sel_a != sel_b && !shift {
                    sel_b
                } else {
                    self.next_boundary(self.cursor)
                };
                self.cursor = to;
                if !shift {
                    self.anchor = to;
                }
                self.scroll_to_cursor();
                cx.request_paint();
                true
            }
            key::HOME => {
                self.cursor = 0;
                if !shift {
                    self.anchor = 0;
                }
                self.scroll_to_cursor();
                cx.request_paint();
                true
            }
            key::END => {
                self.cursor = self.text.len();
                if !shift {
                    self.anchor = self.cursor;
                }
                self.scroll_to_cursor();
                cx.request_paint();
                true
            }
            key::A if k.ctrl() => {
                self.anchor = 0;
                self.cursor = self.text.len();
                cx.request_paint();
                true
            }
            key::BACKSPACE => {
                if sel_a != sel_b {
                    self.replace_selection("");
                } else if self.cursor > 0 {
                    let from = self.prev_boundary(self.cursor);
                    self.text.replace_range(from..self.cursor, "");
                    self.cursor = from;
                    self.anchor = from;
                } else {
                    return true;
                }
                self.after_edit(cx);
                true
            }
            key::DELETE => {
                if sel_a != sel_b {
                    self.replace_selection("");
                } else if self.cursor < self.text.len() {
                    let to = self.next_boundary(self.cursor);
                    self.text.replace_range(self.cursor..to, "");
                } else {
                    return true;
                }
                self.after_edit(cx);
                true
            }
            key::ENTER => {
                self.fire(cx, true);
                true
            }
            _ => false,
        }
    }
}

impl<S: 'static> Widget<S> for TextField<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let theme = cx.theme();
        let (px, py) = theme.button_padding;
        let style = self.resolved_style(theme);
        // Sized from the *font*, not the contents: a field whose height
        // depended on its text would re-lay out the whole tree on every
        // keystroke, which is exactly what a retained toolkit is for
        // avoiding. Its width comes from its style, or a sensible
        // default of twenty characters.
        let line = cx
            .measure_text("Xg", &style, 0.0)
            .unwrap_or_default()
            .height
            .max(style.size_px);
        self.metrics = cx.measure_text(&self.text, &style, 0.0).unwrap_or_default();
        self.cursors = cx.cursor_positions(&self.text, &style).unwrap_or_default();
        let default_width = style.size_px * 0.55 * 20.0 + px * 2.0;
        constraints.constrain(Size::new(default_width, line + py * 2.0))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let (px, py) = theme.button_padding;
        let focused = cx.ui.is_focused(cx.id);
        let style = self.resolved_style(theme);
        let (field, text_color, place_color, caret_color, sel_color) = (
            if self.enabled {
                theme.field
            } else {
                theme.button_disabled
            },
            if self.enabled {
                theme.text
            } else {
                theme.text_disabled
            },
            theme.placeholder,
            theme.caret,
            theme.selection,
        );
        let border = if focused {
            (theme.border_width.max(1.0) + 1.0, theme.focus)
        } else {
            (theme.border_width, theme.border)
        };
        let radius = theme.radius;
        let bounds = cx.bounds;
        cx.rect(
            field_slot::BACKGROUND,
            bounds,
            Fill::Solid(field),
            radius,
            border,
        );

        // Everything that scrolls lives under one clipping group, so a
        // long line is cut at the field's edge and scrolling it costs
        // one `SetTransform`.
        let line_h = self.metrics.height.max(style.size_px);
        let inner = Rect::new(
            px,
            py,
            (bounds.w - px * 2.0).max(0.0),
            (bounds.h - py * 2.0).max(0.0),
        );
        self.view_width = inner.w;
        let view = cx.group(
            field_slot::VIEW,
            inner,
            true,
            nitro_core::Transform::translate(-self.scroll, 0.0),
        );
        if view.is_none() {
            return;
        }
        let top = ((inner.h - line_h) / 2.0).max(0.0);

        // A slot the paint does not emit has its node destroyed, which
        // is how the selection and the caret come and go without a
        // special case for "used to be there".
        let (sel_from, sel_to) = self.selection();
        if sel_from != sel_to && focused {
            let (x0, x1) = (self.x_of(sel_from), self.x_of(sel_to));
            cx.rect_in(
                view,
                field_slot::SELECTION,
                Rect::new(x0, top, (x1 - x0).max(1.0), line_h),
                Fill::Solid(sel_color),
                0.0,
                (0.0, Color::TRANSPARENT),
            );
        }

        let (shown, color) = if self.text.is_empty() {
            (self.placeholder.clone(), place_color)
        } else {
            (self.text.clone(), text_color)
        };
        cx.text_in(
            view,
            field_slot::TEXT,
            Rect::new(0.0, top, self.metrics.width.max(inner.w), line_h),
            &shown,
            TextRun::new(&style, color),
        );

        if focused && self.enabled && sel_from == sel_to {
            let caret_x = self.x_of(self.cursor);
            cx.rect_in(
                view,
                field_slot::CARET,
                Rect::new(caret_x, top, 1.0, line_h),
                Fill::Solid(caret_color),
                0.0,
                (0.0, Color::TRANSPARENT),
            );
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        match ev {
            Event::PointerDown { pos, button } if *button == button::LEFT => {
                cx.request_focus();
                let (px, _) = cx.theme().button_padding;
                let at = self.offset_at(pos.x - px + self.scroll);
                self.cursor = at;
                self.anchor = at;
                cx.request_paint();
                Handled::Yes
            }
            Event::KeyDown(k) => Handled::from(self.key(cx, k)),
            Event::Text { text } => {
                // A control chord produces no text, so anything that
                // arrives here is a character the field should insert.
                if text.chars().any(char::is_control) {
                    return Handled::No;
                }
                self.replace_selection(text);
                self.after_edit(cx);
                Handled::Yes
            }
            Event::FocusChanged { .. } => {
                cx.request_paint();
                Handled::No
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::TextField
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: Some(self.text.clone()),
            actions: if self.enabled {
                vec!["set_value", "focus", "submit", "clear"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "set_value" | "set_text" => {
                arg.unwrap_or_default().clone_into(&mut self.text);
                self.cursor = self.text.len();
                self.anchor = self.cursor;
                self.scroll = 0.0;
                self.after_edit(cx);
                Handled::Yes
            }
            "clear" => {
                self.text.clear();
                self.cursor = 0;
                self.anchor = 0;
                self.scroll = 0.0;
                self.after_edit(cx);
                Handled::Yes
            }
            "submit" => {
                self.fire(cx, true);
                Handled::Yes
            }
            "set_placeholder" => {
                arg.unwrap_or_default().clone_into(&mut self.placeholder);
                cx.request_paint();
                Handled::Yes
            }
            "set_enabled" => {
                self.enabled = arg != Some("false");
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`TextField`].
impl<S: 'static> WidgetMut<'_, TextField<S>, S> {
    /// Replace the contents, putting the caret at the end.
    ///
    /// Paint-only: the field's size comes from its font and its style,
    /// not from its text, so typing never re-lays out the tree.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.text == text {
            return;
        }
        self.text = text;
        self.cursor = self.text.len();
        self.anchor = self.cursor;
        self.scroll = 0.0;
        let id = self.id();
        let (t, style) = (self.text.clone(), {
            let theme = self.ui().theme().clone();
            self.resolved_style(&theme)
        });
        let metrics = self.ui().measure_text(&t, &style, 0.0).unwrap_or_default();
        let cursors = self.ui().cursor_positions(&t, &style).unwrap_or_default();
        self.metrics = metrics;
        self.cursors = cursors;
        self.ui().mark(id, Dirty::PAINT);
    }

    /// Replace the placeholder.
    pub fn set_placeholder(&mut self, text: impl Into<String>) {
        self.placeholder = text.into();
        self.request_paint();
    }

    /// Enable or disable the field.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.request_paint();
    }

    /// Move the caret to a byte offset, clamped to a character boundary.
    pub fn set_cursor(&mut self, at: usize) {
        let at = at.min(self.text.len());
        let at = (0..=at)
            .rev()
            .find(|i| self.text.is_char_boundary(*i))
            .unwrap_or(0);
        self.cursor = at;
        self.anchor = at;
        self.scroll_to_cursor();
        self.request_paint();
    }

    /// Select everything.
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
        self.request_paint();
    }

    /// Replace the change callback.
    pub fn set_on_change(&mut self, f: impl Fn(&mut S, &mut Ui<S>, &str) + 'static) {
        self.on_change = Some(Box::new(f));
    }
}

/// Builder for a [`TextField`].
pub struct TextFieldBuilder<S> {
    built: Built<S>,
    field: TextField<S>,
}

impl<S: 'static> TextFieldBuilder<S> {
    /// Text shown while the field is empty.
    #[must_use]
    pub fn placeholder(mut self, text: impl Into<String>) -> Self {
        self.field.placeholder = text.into();
        self
    }

    /// What to do whenever the contents change.
    #[must_use]
    pub fn on_change(mut self, f: impl Fn(&mut S, &mut Ui<S>, &str) + 'static) -> Self {
        self.field.on_change = Some(Box::new(f));
        self
    }

    /// What to do when Enter is pressed.
    #[must_use]
    pub fn on_submit(mut self, f: impl Fn(&mut S, &mut Ui<S>, &str) + 'static) -> Self {
        self.field.on_submit = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.field.enabled = false;
        self
    }

    /// Set the font size.
    #[must_use]
    pub fn size(mut self, px: f32) -> Self {
        let mut s = self.field.style.unwrap_or_default();
        s.size_px = px;
        self.field.style = Some(s);
        self
    }
}

impl<S: 'static> StyleBuilder<S> for TextFieldBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for TextFieldBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.state_mut().focusable = true;
        self.built.replace_widget(self.field);
        self.built
    }
}

/// An editable line of text, initially `text`.
///
/// Takes the [`ShrinkFloor::Zero`] floor: a field is a viewport over its
/// own string — it scrolls horizontally to keep the caret visible — so
/// a narrower field shows less of the text rather than smaller text.
/// Its measured width is a default of twenty characters, for when
/// nobody said otherwise, and is not a claim on the space. Use
/// `min_width` for the narrowest field still worth typing into.
#[must_use]
pub fn text_field<S: 'static>(text: impl Into<String>) -> TextFieldBuilder<S> {
    let text = text.into();
    let cursor = text.len();
    let field = TextField {
        text,
        placeholder: String::new(),
        cursor,
        anchor: cursor,
        enabled: true,
        style: None,
        on_change: None,
        on_submit: None,
        metrics: crate::wire::TextMetrics::default(),
        cursors: Vec::new(),
        scroll: 0.0,
        view_width: 0.0,
    };
    let mut built = Built::new(Flex);
    built.state_mut().style.shrink_floor = ShrinkFloor::Zero;
    TextFieldBuilder { built, field }
}

// ---------------------------------------------------------------------
// Checkbox
// ---------------------------------------------------------------------

/// What a [`Checkbox`] does when it is toggled.
type ToggleFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, bool)>;

/// A box with a label that is either checked or not.
pub struct Checkbox<S> {
    label: String,
    checked: bool,
    enabled: bool,
    style: Option<TextStyle>,
    on_toggle: Option<ToggleFn<S>>,
    metrics: crate::wire::TextMetrics,
}

impl<S> std::fmt::Debug for Checkbox<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkbox")
            .field("label", &self.label)
            .field("checked", &self.checked)
            .finish_non_exhaustive()
    }
}

impl<S> Checkbox<S> {
    /// Whether the box is checked.
    #[must_use]
    pub fn is_checked(&self) -> bool {
        self.checked
    }

    /// The label beside the box.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether it reacts to input.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn resolved_style(&self, theme: &crate::Theme) -> TextStyle {
        self.style
            .clone()
            .unwrap_or_else(|| TextStyle::from_theme(theme))
    }
}

impl<S: 'static> Checkbox<S> {
    /// Flip the box and tell the app, as a click or `toggle` does.
    fn toggle(&mut self, cx: &mut EventCx<'_, S>, to: bool) {
        if !self.enabled || self.checked == to {
            // A no-op toggle still repaints nothing and calls nothing:
            // `set_value true` on a checked box is not a change.
            return;
        }
        self.checked = to;
        cx.report_activation();
        cx.request_paint();
        let Some(cb) = self.on_toggle.take() else {
            return;
        };
        cb(cx.state, cx.ui, self.checked);
        self.on_toggle = Some(cb);
    }
}

impl<S: 'static> Widget<S> for Checkbox<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let theme = cx.theme();
        let (box_size, gap) = (theme.checkbox_size, theme.gap);
        let style = self.resolved_style(theme);
        self.metrics = cx
            .measure_text(&self.label, &style, 0.0)
            .unwrap_or_default();
        let w = if self.label.is_empty() {
            box_size
        } else {
            box_size + gap + self.metrics.width
        };
        constraints.constrain(Size::new(w, box_size.max(self.metrics.height)))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let (box_size, gap) = (theme.checkbox_size, theme.gap);
        let focused = cx.ui.is_focused(cx.id);
        let hovered = cx.ui.is_hovered(cx.id);
        let theme = cx.theme();
        let style = self.resolved_style(theme);
        let face = if !self.enabled {
            theme.button_disabled
        } else if self.checked {
            theme.accent
        } else if hovered {
            theme.button_hover
        } else {
            theme.field
        };
        let border = if focused {
            (theme.border_width.max(1.0) + 1.0, theme.focus)
        } else {
            (theme.border_width, theme.border)
        };
        let text_color = if self.enabled {
            theme.text
        } else {
            theme.text_disabled
        };
        let tick = theme.button_text;
        let radius = (theme.radius * 0.5).min(box_size / 4.0);
        let bounds = cx.bounds;
        let y = ((bounds.h - box_size) / 2.0).max(0.0);
        cx.rect(
            0,
            Rect::new(0.0, y, box_size, box_size),
            Fill::Solid(face),
            radius,
            border,
        );
        // The tick is a smaller inset rect rather than a glyph: the
        // server draws rects and text, and a checkmark string would need
        // a font that has one.
        if self.checked {
            let inset = box_size * 0.28;
            cx.fill_rect(
                1,
                Rect::new(
                    inset,
                    y + inset,
                    box_size - inset * 2.0,
                    box_size - inset * 2.0,
                ),
                tick,
            );
        }
        if !self.label.is_empty() {
            let h = self.metrics.height.max(1.0);
            let ly = ((bounds.h - h) / 2.0).max(0.0);
            cx.text(
                2,
                Rect::new(box_size + gap, ly, (bounds.w - box_size - gap).max(0.0), h),
                &self.label.clone(),
                TextRun::new(&style, text_color),
            );
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        match ev {
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                cx.request_focus();
                Handled::Yes
            }
            Event::PointerUp { pos, button } if *button == button::LEFT => {
                if cx.contains(*pos) {
                    let to = !self.checked;
                    self.toggle(cx, to);
                }
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::SPACE => {
                let to = !self.checked;
                self.toggle(cx, to);
                Handled::Yes
            }
            Event::PointerEnter { .. } | Event::PointerLeave | Event::FocusChanged { .. } => {
                cx.request_paint();
                Handled::No
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Checkbox
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn accessible(&self) -> Access {
        Access {
            name: if self.label.is_empty() {
                None
            } else {
                Some(self.label.clone())
            },
            value: Some(self.checked.to_string()),
            actions: if self.enabled {
                vec!["toggle", "set_value", "focus"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "toggle" | "click" | "activate" => {
                let to = !self.checked;
                self.toggle(cx, to);
                Handled::Yes
            }
            "set_value" | "set_checked" => {
                let to = matches!(arg, Some("true" | "1" | "on" | "yes"));
                self.toggle(cx, to);
                Handled::Yes
            }
            "set_label" => {
                arg.unwrap_or_default().clone_into(&mut self.label);
                cx.request_layout();
                Handled::Yes
            }
            "set_enabled" => {
                self.enabled = arg != Some("false");
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Checkbox`].
impl<S: 'static> WidgetMut<'_, Checkbox<S>, S> {
    /// Check or uncheck the box. Does **not** run `on_toggle`: a setter
    /// is the app changing its own mind, not the user doing something.
    pub fn set_checked(&mut self, checked: bool) {
        if self.checked == checked {
            return;
        }
        self.checked = checked;
        self.request_paint();
    }

    /// Replace the label.
    pub fn set_label(&mut self, label: impl Into<String>) {
        let label = label.into();
        if self.label == label {
            return;
        }
        self.label = label;
        self.request_layout();
    }

    /// Enable or disable the box.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.request_paint();
    }
}

/// Builder for a [`Checkbox`].
pub struct CheckboxBuilder<S> {
    built: Built<S>,
    checkbox: Checkbox<S>,
}

impl<S: 'static> CheckboxBuilder<S> {
    /// Start checked.
    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.checkbox.checked = checked;
        self
    }

    /// What to do when it is toggled.
    #[must_use]
    pub fn on_toggle(mut self, f: impl Fn(&mut S, &mut Ui<S>, bool) + 'static) -> Self {
        self.checkbox.on_toggle = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.checkbox.enabled = false;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for CheckboxBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for CheckboxBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let state = self.built.state_mut();
        state.focusable = true;
        self.built.replace_widget(self.checkbox);
        self.built
    }
}

/// A checkbox labelled `label`.
#[must_use]
pub fn checkbox<S: 'static>(label: impl Into<String>) -> CheckboxBuilder<S> {
    let checkbox = Checkbox {
        label: label.into(),
        checked: false,
        enabled: true,
        style: None,
        on_toggle: None,
        metrics: crate::wire::TextMetrics::default(),
    };
    CheckboxBuilder {
        built: Built::new(Flex),
        checkbox,
    }
}

// ---------------------------------------------------------------------
// Slider
// ---------------------------------------------------------------------

/// What a [`Slider`] does when its value changes.
type ValueFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, f32)>;

/// A horizontal track with a knob: a number picked from a range.
pub struct Slider<S> {
    value: f32,
    min: f32,
    max: f32,
    step: f32,
    enabled: bool,
    dragging: bool,
    on_change: Option<ValueFn<S>>,
}

impl<S> std::fmt::Debug for Slider<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slider")
            .field("value", &self.value)
            .field("range", &(self.min, self.max))
            .finish_non_exhaustive()
    }
}

impl<S> Slider<S> {
    /// The current value.
    #[must_use]
    pub fn value(&self) -> f32 {
        self.value
    }

    /// The range, `(min, max)`.
    #[must_use]
    pub fn range(&self) -> (f32, f32) {
        (self.min, self.max)
    }

    /// Whether it reacts to input.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Where the value sits in its range, as `0.0..=1.0`.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.max <= self.min {
            return 0.0;
        }
        ((self.value - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
    }

    /// Clamp and snap `v` to the range and the step.
    fn quantize(&self, v: f32) -> f32 {
        let v = v.clamp(self.min, self.max);
        if self.step <= 0.0 {
            return v;
        }
        let steps = ((v - self.min) / self.step).round();
        (self.min + steps * self.step).clamp(self.min, self.max)
    }
}

impl<S: 'static> Slider<S> {
    /// Set the value and tell the app, if it really changed.
    fn set(&mut self, cx: &mut EventCx<'_, S>, v: f32) {
        let v = self.quantize(v);
        if v.to_bits() == self.value.to_bits() {
            return;
        }
        self.value = v;
        cx.request_paint();
        let Some(cb) = self.on_change.take() else {
            return;
        };
        cb(cx.state, cx.ui, v);
        self.on_change = Some(cb);
    }

    /// The value at pointer x `x` inside a track of width `w`.
    fn value_at(&self, x: f32, w: f32, knob: f32) -> f32 {
        let usable = (w - knob).max(1.0);
        let t = ((x - knob / 2.0) / usable).clamp(0.0, 1.0);
        self.min + t * (self.max - self.min)
    }
}

impl<S: 'static> Widget<S> for Slider<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let theme = cx.theme();
        // Wide enough to be draggable, tall enough for the knob. The
        // width is a default: a slider is normally given one, or grows.
        constraints.constrain(Size::new(theme.slider_knob * 8.0, theme.slider_knob))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let (track_h, knob) = (theme.slider_track, theme.slider_knob);
        let focused = cx.ui.is_focused(cx.id);
        let theme = cx.theme();
        let (filled, rest, knob_face) = if self.enabled {
            (theme.accent, theme.track, theme.button)
        } else {
            (theme.button_disabled, theme.track, theme.button_disabled)
        };
        let border = if focused {
            (theme.border_width.max(1.0) + 1.0, theme.focus)
        } else {
            (theme.border_width, theme.border)
        };
        let bounds = cx.bounds;
        let ty = ((bounds.h - track_h) / 2.0).max(0.0);
        let usable = (bounds.w - knob).max(0.0);
        let x = knob / 2.0 + usable * self.fraction();
        cx.rect(
            0,
            Rect::new(0.0, ty, bounds.w, track_h),
            Fill::Solid(rest),
            track_h / 2.0,
            (0.0, Color::TRANSPARENT),
        );
        cx.rect(
            1,
            Rect::new(0.0, ty, x, track_h),
            Fill::Solid(filled),
            track_h / 2.0,
            (0.0, Color::TRANSPARENT),
        );
        let ky = ((bounds.h - knob) / 2.0).max(0.0);
        cx.rect(
            2,
            Rect::new(x - knob / 2.0, ky, knob, knob),
            Fill::Solid(knob_face),
            knob / 2.0,
            border,
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        let knob = cx.theme().slider_knob;
        let w = cx.bounds.w;
        match ev {
            Event::PointerDown { pos, button } if *button == button::LEFT => {
                cx.request_focus();
                self.dragging = true;
                let v = self.value_at(pos.x, w, knob);
                self.set(cx, v);
                Handled::Yes
            }
            Event::PointerMove { pos } if self.dragging => {
                let v = self.value_at(pos.x, w, knob);
                self.set(cx, v);
                Handled::Yes
            }
            Event::PointerUp { button, .. } if *button == button::LEFT => {
                self.dragging = false;
                Handled::Yes
            }
            // No pointer grab in M2, so a drag that wanders out of the
            // widget ends there rather than being routed back.
            Event::PointerLeave => {
                self.dragging = false;
                Handled::No
            }
            Event::KeyDown(k) if k.keycode == key::LEFT || k.keycode == key::DOWN => {
                let step = if self.step > 0.0 {
                    self.step
                } else {
                    (self.max - self.min) / 20.0
                };
                let v = self.value - step;
                self.set(cx, v);
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::RIGHT || k.keycode == key::UP => {
                let step = if self.step > 0.0 {
                    self.step
                } else {
                    (self.max - self.min) / 20.0
                };
                let v = self.value + step;
                self.set(cx, v);
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::HOME => {
                let v = self.min;
                self.set(cx, v);
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::END => {
                let v = self.max;
                self.set(cx, v);
                Handled::Yes
            }
            Event::FocusChanged { .. } => {
                cx.request_paint();
                Handled::No
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Slider
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: Some(format_number(self.value)),
            actions: if self.enabled {
                vec!["set_value", "focus"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "set_value" => {
                let Some(v) = arg.and_then(|a| a.trim().parse::<f32>().ok()) else {
                    return Handled::No;
                };
                self.set(cx, v);
                Handled::Yes
            }
            "set_enabled" => {
                self.enabled = arg != Some("false");
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Slider`].
impl<S: 'static> WidgetMut<'_, Slider<S>, S> {
    /// Set the value, clamped to the range and snapped to the step.
    /// Does not run `on_change`.
    pub fn set_value(&mut self, value: f32) {
        let v = self.quantize(value);
        if v.to_bits() == self.value.to_bits() {
            return;
        }
        self.value = v;
        self.request_paint();
    }

    /// Replace the range, re-clamping the value.
    pub fn set_range(&mut self, min: f32, max: f32) {
        self.min = min;
        self.max = max.max(min);
        self.value = self.value.clamp(self.min, self.max);
        self.request_paint();
    }

    /// Enable or disable the slider.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.request_paint();
    }
}

/// Builder for a [`Slider`].
pub struct SliderBuilder<S> {
    built: Built<S>,
    slider: Slider<S>,
}

impl<S: 'static> SliderBuilder<S> {
    /// Set the range (default `0.0..=1.0`).
    #[must_use]
    pub fn range(mut self, min: f32, max: f32) -> Self {
        self.slider.min = min;
        self.slider.max = max.max(min);
        self.slider.value = self.slider.value.clamp(min, self.slider.max);
        self
    }

    /// Snap the value to multiples of `step` (0 = continuous).
    #[must_use]
    pub fn step(mut self, step: f32) -> Self {
        self.slider.step = step;
        self
    }

    /// What to do when the value changes.
    #[must_use]
    pub fn on_change(mut self, f: impl Fn(&mut S, &mut Ui<S>, f32) + 'static) -> Self {
        self.slider.on_change = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.slider.enabled = false;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for SliderBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for SliderBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.state_mut().focusable = true;
        self.built.replace_widget(self.slider);
        self.built
    }
}

/// A horizontal slider at `value`, over `0.0..=1.0` unless `.range()`
/// says otherwise.
///
/// Takes the [`ShrinkFloor::Zero`] floor: a slider has no content, and
/// the size it measures to is a *default* for when nobody gave it one,
/// not a statement about what it needs. A narrower track is still a
/// track — the same drag, the same value — so a slider is the natural
/// place for a crowded row's overflow to go. Give it a `min_width` to
/// say how narrow is still draggable.
#[must_use]
pub fn slider<S: 'static>(value: f32) -> SliderBuilder<S> {
    let slider = Slider {
        value,
        min: 0.0,
        max: 1.0,
        step: 0.0,
        enabled: true,
        dragging: false,
        on_change: None,
    };
    let mut built = Built::new(Flex);
    built.state_mut().style.shrink_floor = ShrinkFloor::Zero;
    SliderBuilder { built, slider }
}

/// A number as the introspection protocol prints it: no trailing `.0`
/// on a whole number, no exponent, and stable enough to compare.
#[must_use]
fn format_number(v: f32) -> String {
    if v.fract() == 0.0 && v.abs() < 1e9 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

// ---------------------------------------------------------------------
// Scroll
// ---------------------------------------------------------------------

/// A vertical viewport over one taller child.
///
/// **Scrolling is composition, not layout.** The child's group hangs
/// under a clipping group, and scrolling moves that group's transform:
/// exactly one `SetTransform` on the wire, no relayout and no repaint of
/// anything inside. The harness test
/// `scrolling_is_one_set_transform_and_nothing_else` asserts precisely
/// that from the outside, because a claim about cost that nothing checks
/// stops being true.
#[derive(Debug, Default)]
pub struct Scroll {
    offset: f32,
    /// The child's measured height, so `max_offset` is known without
    /// asking the tree.
    content_height: f32,
    /// The viewport height from the last layout.
    view_height: f32,
    /// Pixels per wheel notch.
    speed: f32,
}

impl Scroll {
    /// How far down the content is scrolled, in logical pixels.
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// The largest offset that still shows content.
    #[must_use]
    pub fn max_offset(&self) -> f32 {
        (self.content_height - self.view_height).max(0.0)
    }

    /// The viewport's height from the last layout.
    #[must_use]
    pub fn view_height(&self) -> f32 {
        self.view_height
    }

    /// The content's measured height.
    #[must_use]
    pub fn content_height(&self) -> f32 {
        self.content_height
    }
}

impl Scroll {
    /// Clamp `to` into the scrollable range.
    fn clamp(&self, to: f32) -> f32 {
        to.clamp(0.0, self.max_offset())
    }
}

impl<S: 'static> Widget<S> for Scroll {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let children = cx.children();
        // The child is measured with an unbounded height: that is what
        // lets it be taller than the viewport, which is the entire point.
        let inner = Constraints {
            min: Size::ZERO,
            max: Size::new(constraints.max.w, f32::INFINITY),
        };
        let mut h = 0.0f32;
        let mut w = 0.0f32;
        for c in children {
            let s = cx.measure_child(c, inner);
            h = h.max(s.h);
            w = w.max(s.w);
        }
        self.content_height = h;
        // The viewport takes the height it is offered, not the content's.
        let want = if constraints.max.h.is_finite() {
            constraints.max.h
        } else {
            h
        };
        constraints.constrain(Size::new(w, want))
    }

    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        self.view_height = bounds.h;
        let children = cx.children();
        for c in children {
            // The child is placed at its full height inside the
            // viewport's own space; the clip cuts it and the content
            // group's transform moves it.
            cx.place_child(
                c,
                Rect::new(0.0, 0.0, bounds.w, self.content_height.max(bounds.h)),
            );
        }
        // A shrunken content (or viewport) can leave the offset past the
        // end; clamp here rather than showing blank.
        let max = self.max_offset();
        if self.offset > max {
            self.offset = max;
            let id = cx.id;
            cx.ui
                .set_content_transform(id, nitro_core::Transform::translate(0.0, -max));
        }
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        // The viewport is the widget's own **content group** — the node
        // the children's groups already hang under — so there is no
        // extra node and no second coordinate space. Clipping it is one
        // `SetClip`, and scrolling it is one `SetTransform`.
        let id = cx.id;
        let offset = self.offset;
        cx.ui.set_content_clip(id, true);
        cx.ui
            .set_content_transform(id, nitro_core::Transform::translate(0.0, -offset));
        // A widget that paints nothing is not hit-tested by the server,
        // so a scroll area with a transparent background would never see
        // a wheel event. One transparent rect is what makes it hittable.
        let bounds = cx.bounds;
        cx.fill_rect(0, bounds, Color::TRANSPARENT);
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        let to = match ev {
            Event::Scroll { dy, .. } => self.offset - dy * self.speed,
            Event::KeyDown(k) => match k.keycode {
                key::DOWN => self.offset + self.speed,
                key::UP => self.offset - self.speed,
                key::PAGE_DOWN => self.offset + self.view_height,
                key::PAGE_UP => self.offset - self.view_height,
                key::HOME => 0.0,
                key::END => self.max_offset(),
                _ => return Handled::No,
            },
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                cx.request_focus();
                return Handled::No;
            }
            _ => return Handled::No,
        };
        let to = self.clamp(to);
        if to.to_bits() == self.offset.to_bits() {
            // Already at the end: still consumed, so the wheel does not
            // bubble out to an enclosing scroller mid-gesture.
            return Handled::Yes;
        }
        self.offset = to;
        let id = cx.id;
        cx.ui
            .set_content_transform(id, nitro_core::Transform::translate(0.0, -to));
        Handled::Yes
    }

    fn role(&self) -> Role {
        Role::Scroll
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: Some(format_number(self.offset)),
            actions: vec!["scroll_to", "scroll_by", "focus"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        let to = match action {
            "scroll_to" | "set_value" | "set_offset" => {
                let Some(v) = arg.and_then(|a| a.trim().parse::<f32>().ok()) else {
                    return Handled::No;
                };
                v
            }
            "scroll_by" => {
                let Some(v) = arg.and_then(|a| a.trim().parse::<f32>().ok()) else {
                    return Handled::No;
                };
                self.offset + v
            }
            _ => return Handled::No,
        };
        let to = self.clamp(to);
        if to.to_bits() != self.offset.to_bits() {
            self.offset = to;
            let id = cx.id;
            cx.ui
                .set_content_transform(id, nitro_core::Transform::translate(0.0, -to));
        }
        Handled::Yes
    }
}

/// Setters for a live [`Scroll`].
impl<S: 'static> WidgetMut<'_, Scroll, S> {
    /// Scroll to `offset`, clamped. **One `SetTransform` and nothing
    /// else**: no relayout, no repaint of the content.
    pub fn scroll_to(&mut self, offset: f32) {
        let to = offset.clamp(0.0, self.max_offset());
        if to.to_bits() == self.offset.to_bits() {
            return;
        }
        self.offset = to;
        let id = self.id();
        self.ui()
            .set_content_transform(id, nitro_core::Transform::translate(0.0, -to));
    }

    /// Scroll by `delta` pixels.
    pub fn scroll_by(&mut self, delta: f32) {
        let to = self.offset + delta;
        self.scroll_to(to);
    }

    /// Set how far one wheel notch scrolls.
    pub fn set_speed(&mut self, px: f32) {
        self.speed = px;
    }
}

/// Builder for a [`Scroll`].
pub struct ScrollBuilder<S> {
    built: Built<S>,
    scroll: Scroll,
}

impl<S: 'static> ScrollBuilder<S> {
    /// How far one wheel notch scrolls (default 40 px).
    #[must_use]
    pub fn speed(mut self, px: f32) -> Self {
        self.scroll.speed = px;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for ScrollBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for ScrollBuilder<S> {}

impl<S: 'static> IntoWidget<S> for ScrollBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.state_mut().focusable = true;
        self.built.replace_widget(self.scroll);
        self.built
    }
}

/// A vertical scrolling viewport.
///
/// Opts out of the default content shrink floor
/// ([`ShrinkFloor::Zero`]) for the reason [`list`](crate::list) does: a
/// viewport given less room shows less of its child, which is an honest
/// smaller version of itself. Its `measure` already takes the height it
/// is offered rather than its content's, so the floor is what keeps the
/// two answers consistent when the offer arrives as an overflow.
#[must_use]
pub fn scroll<S: 'static>() -> ScrollBuilder<S> {
    let mut built = Built::new(Scroll::default());
    built.state_mut().style.shrink_floor = ShrinkFloor::Zero;
    ScrollBuilder {
        built,
        scroll: Scroll {
            offset: 0.0,
            content_height: 0.0,
            view_height: 0.0,
            speed: 40.0,
        },
    }
}

// ---------------------------------------------------------------------
// Separator
// ---------------------------------------------------------------------

/// A dividing line, horizontal or vertical.
#[derive(Debug)]
pub struct Separator {
    vertical: bool,
    color: Option<Color>,
}

impl Separator {
    /// Whether the line runs top to bottom.
    #[must_use]
    pub fn is_vertical(&self) -> bool {
        self.vertical
    }
}

impl<S: 'static> Widget<S> for Separator {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let w = cx.theme().separator_width;
        // Thin on its own axis, and as long as it is allowed on the
        // other: a separator is a line, and a line in a column should
        // span the column.
        let size = if self.vertical {
            Size::new(w, constraints.max.h.min(f32::MAX))
        } else {
            Size::new(constraints.max.w.min(f32::MAX), w)
        };
        let size = Size::new(
            if size.w.is_finite() { size.w } else { w },
            if size.h.is_finite() { size.h } else { w },
        );
        constraints.constrain(size)
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let color = self.color.unwrap_or(cx.theme().track);
        let bounds = cx.bounds;
        cx.fill_rect(0, bounds, color);
    }

    fn role(&self) -> Role {
        Role::Separator
    }
}

/// Setters for a live [`Separator`].
impl<S: 'static> WidgetMut<'_, Separator, S> {
    /// Replace the line's colour.
    pub fn set_color(&mut self, color: Color) {
        self.color = Some(color);
        self.request_paint();
    }
}

/// Builder for a [`Separator`].
pub struct SeparatorBuilder<S> {
    built: Built<S>,
    separator: Separator,
}

impl<S: 'static> SeparatorBuilder<S> {
    /// Run the line top to bottom instead of left to right.
    #[must_use]
    pub fn vertical(mut self) -> Self {
        self.separator.vertical = true;
        self
    }

    /// Set the colour (default: the theme's `track`).
    #[must_use]
    pub fn color(mut self, color: Color) -> Self {
        self.separator.color = Some(color);
        self
    }
}

impl<S: 'static> StyleBuilder<S> for SeparatorBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for SeparatorBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(self.separator);
        self.built
    }
}

/// A horizontal dividing line.
#[must_use]
pub fn separator<S: 'static>() -> SeparatorBuilder<S> {
    SeparatorBuilder {
        built: Built::new(Flex),
        separator: Separator {
            vertical: false,
            color: None,
        },
    }
}

// ---------------------------------------------------------------------
// Image
// ---------------------------------------------------------------------

/// A picture, from a buffer of `ARGB` pixels the app owns.
///
/// The pixels go to the server once, in a memfd, and the widget draws a
/// region of that buffer — so a repaint of an image is one `SetBounds`
/// and no pixels at all. Replacing the pixels is a new buffer: the
/// server maps what it is given and never copies on the client's behalf.
#[derive(Debug)]
pub struct Image {
    width: u32,
    height: u32,
    /// The buffer, once it has been registered with the server.
    buffer: Option<BufferId>,
    /// Pixels waiting to be registered, `ARGB` as `[b, g, r, a]` rows.
    pending: Option<Vec<u8>>,
    /// A buffer replaced by `set_pixels` and not yet released.
    ///
    /// The release is deferred to the next paint because that is where
    /// the widget has a connection to send on; a setter has only itself
    /// and the tree. Holding exactly one keeps a widget that replaces
    /// its pixels every frame at two live buffers rather than N.
    stale: Option<BufferId>,
    /// Whether the pixels have an alpha channel worth blending.
    alpha: bool,
}

impl Image {
    /// The image's pixel size.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The server-side buffer, once it exists.
    #[must_use]
    pub fn buffer(&self) -> Option<BufferId> {
        self.buffer
    }
}

impl<S: 'static> Widget<S> for Image {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        constraints.constrain(Size::new(self.width as f32, self.height as f32))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        if let Some(px) = self.pending.take() {
            // Registered at first paint rather than at build time: a
            // widget is built before it has a connection to send on, and
            // the `Ui` is only reachable from a pass context.
            self.buffer = cx.upload_image(self.width, self.height, self.alpha, &px);
        }
        // The buffer the new pixels replaced is released here, after the
        // node has stopped pointing at it, for the same reason: a setter
        // has no connection. Without this an app that replaces its image
        // leaks one server-side buffer per replacement.
        if let Some(old) = self.stale.take() {
            cx.release_image(old);
        }
        let Some(buffer) = self.buffer else {
            return;
        };
        let bounds = cx.bounds;
        cx.image(
            0,
            bounds,
            buffer,
            nitro_core::IRect::new(0, 0, self.width.cast_signed(), self.height.cast_signed()),
        );
    }

    fn role(&self) -> Role {
        Role::Image
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: Some(format!("{}x{}", self.width, self.height)),
            actions: Vec::new(),
        }
    }
}

/// Setters for a live [`Image`].
impl<S: 'static> WidgetMut<'_, Image, S> {
    /// Replace the pixels. `pixels` is `width * height * 4` bytes,
    /// `[b, g, r, a]` per pixel, rows tightly packed.
    ///
    /// The buffer being replaced is released at the next paint, so an
    /// app that updates its image repeatedly holds two server-side
    /// buffers, not one per update.
    ///
    /// # Panics
    /// Never; a wrongly sized buffer is ignored and the old image stays.
    pub fn set_pixels(&mut self, width: u32, height: u32, pixels: Vec<u8>) {
        if pixels.len() != (width as usize) * (height as usize) * 4 {
            return;
        }
        self.width = width;
        self.height = height;
        self.pending = Some(pixels);
        // A replacement that arrives before the previous one was ever
        // painted has nothing new to retire, so the older `stale` (if
        // any) is kept rather than overwritten and leaked.
        if let Some(old) = self.buffer.take()
            && let Some(older) = self.stale.replace(old)
        {
            self.ui().release_buffer(older);
        }
        self.request_layout();
    }
}

/// Builder for an [`Image`].
pub struct ImageBuilder<S> {
    built: Built<S>,
    image: Image,
}

impl<S: 'static> ImageBuilder<S> {
    /// Treat the buffer's alpha channel as opaque, which lets the
    /// server skip blending.
    #[must_use]
    pub fn opaque(mut self) -> Self {
        self.image.alpha = false;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for ImageBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for ImageBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(self.image);
        self.built
    }
}

/// An image `width × height` pixels, from `pixels`: `[b, g, r, a]` per
/// pixel, rows tightly packed.
///
/// A buffer of the wrong length produces an image that draws nothing
/// rather than an error, because there is nowhere to report one from a
/// builder.
#[must_use]
pub fn image<S: 'static>(width: u32, height: u32, pixels: Vec<u8>) -> ImageBuilder<S> {
    let ok = pixels.len() == (width as usize) * (height as usize) * 4;
    ImageBuilder {
        built: Built::new(Flex),
        image: Image {
            width: if ok { width } else { 0 },
            height: if ok { height } else { 0 },
            buffer: None,
            pending: ok.then_some(pixels),
            stale: None,
            alpha: true,
        },
    }
}

// ---------------------------------------------------------------------
// Icon
// ---------------------------------------------------------------------

/// A symbolic icon, named rather than drawn.
///
/// The widget sends a **name** and a palette role; the server owns the
/// artwork, rasterises it at the output's device scale and tints it from
/// the current scheme. Nothing about the icon crosses the wire as pixels,
/// which is what makes it work over a remote link, follow a
/// `theme.scheme` flip with no repaint from the app, and be crisp on a 2×
/// output. `docs/icons.md` makes the argument in full.
///
/// **Icons are square by contract**, and that is what makes them free:
/// the widget measures to `size × size` with no round trip at all, unlike
/// a [`Label`], which cannot know its width until the server has shaped
/// its string. Every icon in the set is authored on a 16-unit grid, so
/// `size` is both the width and the height.
///
/// Without [`caps::ICONS`](nitro_wire::types::caps::ICONS) the widget
/// measures exactly the same box and paints nothing: an old or icon-less
/// server costs a gap in a row, never a broken layout.
#[derive(Debug)]
pub struct Icon {
    name: String,
    size: f32,
    tint: IconTint,
    /// A second name to try when the server says the first one is
    /// unknown, with the tint to try it in; see
    /// [`IconBuilder::fallback`].
    ///
    /// The tint is part of the fallback rather than inherited, because
    /// the mixed case is the *common* one: an application icon falls back
    /// to a **symbolic** one, and those are two different sets with two
    /// different `role` bytes. See [`IconBuilder::fallback_tinted`].
    fallback: Option<(String, IconTint)>,
    /// Whether the fallback question has been answered for this name.
    ///
    /// The once-ness itself comes from `Option::take` below — a consumed
    /// fallback cannot be consumed twice — so this is not what *stops*
    /// the loop. What it adds is the record: [`Icon::fell_back`] can
    /// report that the retry happened, and an icon with **no** fallback
    /// is marked answered too, so it is not re-examined for every later
    /// `BadIcon` some other widget earned. [`WidgetMut::set_icon`] and
    /// [`WidgetMut::set_fallback`] clear it, because a new name deserves
    /// the same one try the first one had.
    fell_back: bool,
}

impl Icon {
    /// The icon's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The icon's square side in logical pixels.
    #[must_use]
    pub fn size(&self) -> f32 {
        self.size
    }

    /// The palette role the server tints it with.
    ///
    /// A **coloured** icon has no role — it is painted in the artwork's
    /// own colours — and this answers the role it would take if it were
    /// switched back to a tinted one: the last
    /// [`IconBuilder::color_role`], or the default
    /// [`ColorRole::Text`](nitro_core::Role::Text). Ask
    /// [`Icon::is_coloured`] first when the difference matters; the
    /// honest single answer is [`Icon::tint`].
    ///
    /// It keeps returning a `Role` rather than becoming an
    /// `Option<Role>` because every caller in this tree —
    /// `crates/nitro-ui/tests/icons.rs`, `nitro-settings`' tests — asks
    /// *which* role, and an `Option` would make all of them unwrap a
    /// case they never construct.
    #[must_use]
    pub fn color_role(&self) -> nitro_core::Role {
        match self.tint {
            IconTint::Role(r) => r,
            IconTint::Coloured => nitro_core::Role::Text,
        }
    }

    /// How the icon is coloured: a palette role, or its own colours.
    #[must_use]
    pub fn tint(&self) -> IconTint {
        self.tint
    }

    /// Whether this is an **application** icon, painted in its own
    /// colours rather than tinted from the palette.
    #[must_use]
    pub fn is_coloured(&self) -> bool {
        self.tint.is_coloured()
    }

    /// The name tried when the server does not have [`Icon::name`], if
    /// any. `None` once it has been taken.
    #[must_use]
    pub fn fallback(&self) -> Option<&str> {
        self.fallback.as_ref().map(|(n, _)| n.as_str())
    }

    /// The tint that fallback is asked for in, which need not be this
    /// icon's: an application icon usually falls back to a symbolic one.
    #[must_use]
    pub fn fallback_tint(&self) -> Option<IconTint> {
        self.fallback.as_ref().map(|(_, t)| *t)
    }

    /// Whether the fallback has already been taken — so [`Icon::name`] is
    /// the fallback's now, and no further one is coming.
    #[must_use]
    pub fn fell_back(&self) -> bool {
        self.fell_back
    }

    /// Swap in the fallback name **and its tint**, or answer `false` if
    /// there is nothing left to try.
    ///
    /// The whole of the fallback policy, in one place with exactly one
    /// caller ([`Ui::dispatch`](crate::Ui::dispatch)'s `BadIcon` arm), so
    /// "exactly once" is a property of this function rather than of the
    /// routing above it — which matters because one transaction can earn
    /// several `BadIcon`s and the routing offers the fallback to a widget
    /// once per error.
    ///
    /// The `take` is what makes it once: the fallback is *moved* out, so
    /// a second call finds `None` however it got here. The retry is
    /// itself a name the server may not have, and a widget that re-armed
    /// on its own failure would trade a missing icon for a
    /// `SetIcon`-per-frame loop against a server answering perfectly
    /// honestly — `a_bad_fallback_does_not_loop` is the assertion.
    pub(crate) fn take_fallback(&mut self) -> bool {
        if self.fell_back {
            return false;
        }
        // Marked answered even when there is no fallback, so a widget
        // whose only name is unknown is not re-examined for every later
        // `BadIcon`.
        self.fell_back = true;
        let Some((name, tint)) = self.fallback.take() else {
            return false;
        };
        self.name = name;
        self.tint = tint;
        true
    }
}

impl<S: 'static> Widget<S> for Icon {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        // No round trip, and no dependence on the `ICONS` capability
        // either: the measurement is the contract (square, `size` a
        // side), not a question about what the server happens to have.
        // A fontless-server-equivalent icon set therefore lays out
        // identically and simply draws nothing.
        constraints.constrain(Size::new(self.size, self.size))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        if !cx.has_icons() || self.name.is_empty() {
            return;
        }
        let bounds = cx.bounds;
        let (name, size, tint) = (self.name.clone(), self.size, self.tint);
        cx.icon_tinted(0, bounds, &name, size, tint);
    }

    fn role(&self) -> Role {
        Role::Icon
    }

    fn accessible(&self) -> Access {
        // The *name* is both the accessible name and the value: it is
        // the only meaningful thing about an icon, and it is what makes
        // `hey app get path name` answer `gear` rather than `16x16`.
        Access {
            name: Some(self.name.clone()),
            value: Some(self.name.clone()),
            actions: Vec::new(),
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "set_icon" | "set_value" => {
                arg.unwrap_or_default().clone_into(&mut self.name);
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Icon`].
impl<S: 'static> WidgetMut<'_, Icon, S> {
    /// Show a different icon. Paint only: every icon is the same square.
    ///
    /// The tint is untouched — a coloured icon stays coloured — and the
    /// fallback latch is **reset**, because this is a new name and it
    /// deserves the same one try at its fallback the first one had.
    pub fn set_icon(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.name == name {
            return;
        }
        self.name = name;
        self.fell_back = false;
        self.request_paint();
    }

    /// Resize the icon. Layout, because the box changes.
    pub fn set_size(&mut self, px: f32) {
        if self.size.to_bits() == px.to_bits() {
            return;
        }
        self.size = px;
        self.request_layout();
    }

    /// Take the tint from another palette role.
    ///
    /// Turns a coloured icon back into a tinted one, for the same reason
    /// [`IconBuilder::color_role`] does after
    /// [`IconBuilder::coloured`]: the two are mutually exclusive and the
    /// last word wins.
    pub fn set_color_role(&mut self, role: nitro_core::Role) {
        self.tint = IconTint::Role(role);
        self.request_paint();
    }

    /// Paint this icon's own colours: it names an **application** icon in
    /// the machine's icon theme rather than one of the desktop's symbolic
    /// glyphs.
    pub fn set_coloured(&mut self, coloured: bool) {
        let tint = if coloured {
            IconTint::Coloured
        } else {
            IconTint::Role(self.color_role())
        };
        if self.tint == tint {
            return;
        }
        self.tint = tint;
        self.request_paint();
    }

    /// Set (or clear, with `None`) the name tried when the server does
    /// not have this one, keeping this icon's own tint, and arm the one
    /// retry again.
    pub fn set_fallback(&mut self, name: Option<String>) {
        let tint = self.tint;
        self.set_fallback_tinted(name.map(|n| (n, tint)));
    }

    /// As [`WidgetMut::set_fallback`], with the fallback's own tint — the
    /// mixed case, which is the usual one: a coloured application icon
    /// falls back to a **symbolic** glyph.
    pub fn set_fallback_tinted(&mut self, fallback: Option<(String, IconTint)>) {
        self.fallback = fallback;
        self.fell_back = false;
    }
}

/// Builder for an [`Icon`].
pub struct IconBuilder<S> {
    built: Built<S>,
    icon: Icon,
}

impl<S: 'static> IconBuilder<S> {
    /// Set the icon's square side in logical pixels (default 16).
    ///
    /// 16, 24, 32 and 48 are the recommended sizes: the artwork is drawn
    /// on a 16-unit grid, so an integer multiple keeps its strokes on
    /// pixel boundaries. Anything else still works and is still
    /// anti-aliased; it is simply slightly softer.
    #[must_use]
    pub fn size(mut self, px: f32) -> Self {
        self.icon.size = px;
        self
    }

    /// Take the tint from a palette role (default
    /// [`ColorRole::Text`](nitro_core::Role::Text)).
    ///
    /// There is deliberately **no** `.color(Color)`: an icon takes a role
    /// and nothing else, so a scheme flip can never leave one behind.
    /// `deploy/lint-colors.sh` enforces the same rule from the outside.
    ///
    /// Mutually exclusive with [`IconBuilder::coloured`], and the **last
    /// call wins**: `icon("x").coloured().color_role(Text)` is a tinted
    /// symbolic `x`. One field holds one answer, so there is no order in
    /// which the two can both be true and no state in which the widget
    /// has to guess which the caller meant.
    #[must_use]
    pub fn color_role(mut self, role: nitro_core::Role) -> Self {
        self.icon.tint = IconTint::Role(role);
        self
    }

    /// This name is an **application** icon: paint the artwork's own
    /// colours instead of tinting it.
    ///
    /// The name is then resolved in the machine's XDG icon theme
    /// (`firefox`, `org.gnome.Calculator`) rather than in the symbolic
    /// set compiled into the server, and the two do not fall back to each
    /// other in either direction — `icon("gear").coloured()` is a
    /// `BadIcon` on a box whose theme has no `gear`, exactly as
    /// `icon("firefox")` without this is. That is deliberate: one
    /// namespace searched "ours first" would make the same call mean the
    /// desktop's glyph on one box and somebody's application icon on
    /// another, invisibly. `docs/icons.md` has the argument.
    ///
    /// Pair it with [`IconBuilder::fallback_tinted`] when the name comes
    /// from outside the program — a `.desktop` file's `Icon=`, a
    /// window's app id — since such a name is a *claim* about the box's
    /// icon theme rather than a fact.
    #[must_use]
    pub fn coloured(mut self) -> Self {
        self.icon.tint = IconTint::Coloured;
        self
    }

    /// A second name to show if the server does not have the first, in
    /// **this icon's own tint**.
    ///
    /// Tried **exactly once**, and only for a name the server actually
    /// complained about: the widget re-sends with this name the next time
    /// it paints, and a `BadIcon` for the fallback itself is the end of
    /// it. The size is kept.
    ///
    /// This is the shorthand for a fallback within the *same* set — one
    /// theme icon falling back to a more generic theme icon,
    /// `icon("firefox").coloured().fallback("application-x-executable")`.
    /// When the fallback comes from the other set, which is the usual
    /// case, use [`IconBuilder::fallback_tinted`].
    ///
    /// It is a client-side retry rather than a server-side search list
    /// because the fallback is a *policy* — a launcher wants a generic
    /// application icon, a bar's window list wants `window` — and a
    /// policy that lived in the server would be one every app had to
    /// share. It costs one extra `SetIcon` in the case where the first
    /// name was wrong, and nothing at all otherwise.
    #[must_use]
    pub fn fallback(mut self, name: impl Into<String>) -> Self {
        let tint = self.icon.tint;
        self.icon.fallback = Some((name.into(), tint));
        self
    }

    /// A second name to show if the server does not have the first, in a
    /// tint of its own.
    ///
    /// **The mixed case, and it is the common one.** An application icon
    /// is a name from the machine's XDG icon theme, and the whole reason
    /// it needs a fallback is that the machine may not have that theme —
    /// or may have it and not this application. So the icon to reach for
    /// when the lookup fails has to come from the set that *cannot* fail:
    /// the symbolic one compiled into the server. `window` is exactly
    /// that — it is in the symbolic set and in no icon theme — so
    /// `.fallback("window")` on a coloured icon would keep the coloured
    /// tint, earn a second `BadIcon` and leave a blank box on every
    /// application the theme does not have, which is precisely the case
    /// this feature exists to survive.
    ///
    /// ```no_run
    /// # use nitro_ui::{ColorRole, IconTint, widgets::icon};
    /// # fn f<S: 'static>() {
    /// let app = icon::<S>("firefox")
    ///     .coloured()
    ///     .fallback_tinted("window", IconTint::Role(ColorRole::Text));
    /// # }
    /// ```
    #[must_use]
    pub fn fallback_tinted(mut self, name: impl Into<String>, tint: IconTint) -> Self {
        self.icon.fallback = Some((name.into(), tint));
        self
    }
}

impl<S: 'static> StyleBuilder<S> for IconBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for IconBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(self.icon);
        self.built
    }
}

/// The default side of an [`Icon`], in logical pixels.
pub const ICON_SIZE: f32 = 16.0;

/// The gap between a [`Button`]'s leading icon and its label, in logical
/// pixels.
///
/// Narrower than the theme's `gap` (8 px), which separates *widgets*: an
/// icon and the word it belongs to are one thing, and spacing them like
/// two siblings makes a row read as two columns. 6 px is the same figure
/// [`List`](crate::List) puts between its glyph column and its text, so
/// a launcher row and a file row line up.
pub const ICON_GAP: f32 = 6.0;

/// A symbolic icon called `name`, 16 px square, tinted
/// [`ColorRole::Text`](nitro_core::Role::Text).
///
/// ```no_run
/// # use nitro_ui::{ColorRole, widgets::icon};
/// # fn f<S: 'static>() {
/// let gear = icon::<S>("gear").size(24.0).color_role(ColorRole::TextDim);
/// // An application icon from the machine's icon theme, with a symbolic
/// // one to fall back on if the box does not have it.
/// let app = icon::<S>("firefox").coloured().fallback("window");
/// # }
/// ```
///
/// An unknown name draws nothing and leaves the connection alone — the
/// server answers `Error { BadIcon }` and keeps going, because a missing
/// icon must never be able to close an application. With
/// [`IconBuilder::fallback`] that error is also the cue to try the other
/// name, once.
#[must_use]
pub fn icon<S: 'static>(name: impl Into<String>) -> IconBuilder<S> {
    IconBuilder {
        built: Built::new(Flex),
        icon: Icon {
            name: name.into(),
            size: ICON_SIZE,
            tint: IconTint::Role(nitro_core::Role::Text),
            fallback: None,
            fell_back: false,
        },
    }
}
