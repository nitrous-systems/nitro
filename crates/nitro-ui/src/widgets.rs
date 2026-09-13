//! The M2 widget set: [`Flex`], [`Panel`], [`Label`], [`Button`] and
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
use nitro_wire::types::Align;

use crate::arena::Dirty;
use crate::build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled, button, key};
use crate::layout::{Constraints, CrossAlign, Direction, MainAlign};
use crate::theme::TextStyle;
use crate::ui::{Ui, WidgetMut};
use crate::widget::{Access, EventCx, MeasureCx, PaintCx, Role, TextRun, Widget};

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
    align: Align,
    /// The measurement the last `measure` produced, so `paint` can place
    /// the baseline box without asking the server again.
    metrics: crate::wire::TextMetrics,
    /// The wrap width that measurement was taken at, which `paint` must
    /// reuse: the server learns a wrap width only from `SetText`, so
    /// measuring wrapped and painting unwrapped would reserve two lines
    /// and draw one overflowing line.
    wrap_width: f32,
}

impl Label {
    /// The current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
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

    fn resolved_style(&self, theme: &crate::Theme) -> TextStyle {
        self.style
            .clone()
            .unwrap_or_else(|| TextStyle::from_theme(theme))
    }
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
        self.wrap_width = max;
        self.metrics = cx.measure_text(&self.text, &style, max).unwrap_or_default();
        constraints.constrain(self.metrics.size())
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let color = self.color.unwrap_or(theme.text);
        let style = self.resolved_style(theme);
        let bounds = cx.bounds;
        let run = TextRun::new(&style, color)
            .align(self.align)
            .wrap_at(self.wrap_width);
        cx.text(0, bounds, &self.text.clone(), run);
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.text.clone()),
            value: Some(self.text.clone()),
            actions: Vec::new(),
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
        self.request_layout();
    }

    /// Replace the text style.
    pub fn set_text_style(&mut self, style: TextStyle) {
        self.style = Some(style);
        self.request_layout();
    }

    /// Replace the colour. Paint only: a colour change cannot move
    /// anything.
    pub fn set_color(&mut self, color: Color) {
        self.color = Some(color);
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

    /// Set the colour (default: the theme's `text`).
    #[must_use]
    pub fn color(mut self, color: Color) -> Self {
        self.label.color = Some(color);
        self
    }

    /// Set the alignment inside the label's own box.
    #[must_use]
    pub fn align(mut self, align: Align) -> Self {
        self.label.align = align;
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
        self.built.state_mut().name = Some(self.label.text.clone());
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
        align: Align::Left,
        metrics: crate::wire::TextMetrics::default(),
        wrap_width: 0.0,
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
pub struct Button<S> {
    text: String,
    enabled: bool,
    pressed: bool,
    style: Option<TextStyle>,
    on_click: Option<ClickFn<S>>,
    metrics: crate::wire::TextMetrics,
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
        let Some(cb) = self.on_click.take() else {
            return;
        };
        cb(cx.state, cx.ui);
        self.on_click = Some(cb);
    }
}

impl<S: 'static> Widget<S> for Button<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let theme = cx.theme();
        let (px, py) = theme.button_padding;
        let style = self.resolved_style(theme);
        self.metrics = cx.measure_text(&self.text, &style, 0.0).unwrap_or_default();
        constraints.constrain(Size::new(
            self.metrics.width + px * 2.0,
            self.metrics.height + py * 2.0,
        ))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        let (face, text_color) = if !self.enabled {
            (theme.button_disabled, theme.text_disabled)
        } else if self.pressed {
            (theme.button_active, theme.button_text)
        } else if hovered {
            (theme.button_hover, theme.button_text)
        } else {
            (theme.button, theme.button_text)
        };
        let border = if focused {
            (theme.border_width.max(1.0) + 1.0, theme.focus)
        } else {
            (theme.border_width, theme.border)
        };
        let radius = theme.radius;
        let style = self.resolved_style(theme);
        let bounds = cx.bounds;
        cx.rect(0, bounds, Fill::Solid(face), radius, border);
        // The label is centred by the text node's own alignment
        // horizontally, and by its box vertically.
        let h = self.metrics.height.max(1.0);
        let y = ((bounds.h - h) / 2.0).max(0.0);
        let text_box = Rect::new(0.0, y, bounds.w, h);
        // A button's label is measured unwrapped and the button is sized
        // around it, so it never wraps.
        let run = TextRun::new(&style, text_color).align(Align::Center);
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

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.text.clone()),
            value: None,
            actions: if self.enabled {
                vec!["activate"]
            } else {
                Vec::new()
            },
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

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.button.enabled = false;
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
        state.name = Some(self.button.text.clone());
        self.built.replace_widget(self.button);
        self.built
    }
}

/// A button labelled `text`.
#[must_use]
pub fn button<S: 'static>(text: impl Into<String>) -> ButtonBuilder<S> {
    let button = Button {
        text: text.into(),
        enabled: true,
        pressed: false,
        style: None,
        on_click: None,
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
        items.push(crate::layout::FlexItem::new(cstyle, basis));
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
