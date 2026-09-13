//! The [`Widget`] trait and the four pass contexts a widget is handed.
//!
//! A widget is a plain `struct` with data and a `Widget<S>` impl; `S` is
//! the app's own state type, which is what lets a callback be
//! `Fn(&mut S, &mut Ui<S>)` with no `Rc<RefCell>` anywhere. Every method
//! runs with the widget **out of its arena slot**, so `cx.ui` is a full
//! `&mut Ui<S>`: a widget can create, mutate and destroy other widgets
//! from inside its own `event`.

use std::any::Any;

use nitro_core::{Color, Point, Rect, Size};
use nitro_wire::msg::Fill;
use nitro_wire::types::{Align, BufferId, NodeId};

use crate::arena::{Dirty, WidgetId};
use crate::error::Error;
use crate::event::{Event, Handled};
use crate::layout::Constraints;
use crate::theme::{TextStyle, Theme};
use crate::ui::Ui;
use crate::wire::TextMetrics;

/// What a widget is, for introspection and accessibility.
///
/// It is the same vocabulary AT-SPI and the `hey`-style CLI will use, and
/// it is on the trait rather than in a registry so a widget cannot be
/// written without answering the question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Groups other widgets and draws little or nothing itself.
    Container,
    /// Static text.
    Label,
    /// Something you activate.
    Button,
    /// An editable line of text.
    TextField,
    /// Empty space.
    Spacer,
    /// Anything else.
    Other,
}

impl Role {
    /// The lowercase name used by the introspection protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Role::Container => "container",
            Role::Label => "label",
            Role::Button => "button",
            Role::TextField => "textfield",
            Role::Spacer => "spacer",
            Role::Other => "other",
        }
    }
}

/// What a widget exposes to the outside world.
///
/// M2 fills it in; the introspection socket that serves it is the next
/// task, and this is the shape it will read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Access {
    /// Human-readable name ("OK", "Username").
    pub name: Option<String>,
    /// Current value, as text.
    pub value: Option<String>,
    /// Actions that can be invoked, e.g. `["activate"]`.
    pub actions: Vec<&'static str>,
}

/// A retained widget.
///
/// The five passes are `measure`/`layout` (size and place), `paint` (emit
/// scene mutations), `event` (react to input) and, through `role` and
/// `accessible`, introspection. A widget implements only what it needs:
/// every method has a default that does the sensible thing for a leaf.
pub trait Widget<S: 'static>: 'static {
    /// Intrinsic size within `constraints`.
    ///
    /// A container measures its children (through
    /// [`MeasureCx::measure_child`]) and adds its own padding and gaps; a
    /// leaf answers from its own content. The framework caches the answer
    /// and only asks again when the widget is layout-dirty.
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let _ = cx;
        constraints.constrain(Size::ZERO)
    }

    /// Place children inside `bounds` (this widget's own box, in its
    /// parent's space).
    ///
    /// The default lays children out with the flex solver, which is what
    /// every container in this crate wants; a widget only overrides it to
    /// do something flexbox cannot express.
    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        cx.layout_children(bounds);
    }

    /// Emit the scene mutations for this widget's own nodes.
    ///
    /// Children paint themselves; this is only about what *this* widget
    /// draws, in its own coordinate space (`0, 0` is its top-left corner).
    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let _ = cx;
    }

    /// React to an event. Returning [`Handled::Yes`] stops it bubbling.
    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        let _ = (cx, ev);
        Handled::No
    }

    /// What this widget is.
    fn role(&self) -> Role {
        Role::Other
    }

    /// The accessibility record. The default derives it from the role.
    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: None,
            actions: if self.role() == Role::Button {
                vec!["activate"]
            } else {
                Vec::new()
            },
        }
    }
}

/// A [`Widget`] that can also be downcast back to its concrete type.
///
/// Blanket-implemented for every `Widget`; never implemented by hand. It
/// exists because `Box<dyn Widget<S>>` alone cannot answer "is this a
/// `Button`?", which is exactly what [`Ui::widget_mut`](crate::Ui) needs
/// in order to hand out a typed [`WidgetMut`](crate::WidgetMut).
pub trait AnyWidget<S: 'static>: Widget<S> {
    /// The widget as `&dyn Any`.
    fn as_any(&self) -> &dyn Any;
    /// The widget as `&mut dyn Any`.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<S: 'static, W: Widget<S>> AnyWidget<S> for W {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Context for [`Widget::measure`].
pub struct MeasureCx<'a, S> {
    /// The whole tree, minus the widget being measured.
    pub ui: &'a mut Ui<S>,
    /// The widget being measured.
    pub id: WidgetId,
}

impl<S: 'static> MeasureCx<'_, S> {
    /// This widget's children, in order.
    #[must_use]
    pub fn children(&self) -> Vec<WidgetId> {
        self.ui.children(self.id)
    }

    /// Measure a child, honouring its own `width`/`height` style.
    pub fn measure_child(&mut self, child: WidgetId, constraints: Constraints) -> Size {
        self.ui.measure(child, constraints)
    }

    /// The theme.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        self.ui.theme()
    }

    /// Measure a string, synchronously; see
    /// [`Ui::measure_text`](crate::Ui::measure_text).
    ///
    /// # Errors
    /// If the connection failed.
    pub fn measure_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: f32,
    ) -> Result<TextMetrics, Error> {
        self.ui.measure_text(text, style, max_width)
    }
}

/// Context for [`Widget::layout`].
pub struct LayoutCx<'a, S> {
    /// The whole tree, minus the widget being laid out.
    pub ui: &'a mut Ui<S>,
    /// The widget being laid out.
    pub id: WidgetId,
}

impl<S: 'static> LayoutCx<'_, S> {
    /// This widget's children, in order.
    #[must_use]
    pub fn children(&self) -> Vec<WidgetId> {
        self.ui.children(self.id)
    }

    /// Run the flex solver over this widget's children and place them.
    pub fn layout_children(&mut self, bounds: Rect) {
        self.ui.layout_flex_children(self.id, bounds);
    }

    /// Place one child explicitly, in this widget's coordinate space.
    pub fn place_child(&mut self, child: WidgetId, bounds: Rect) {
        self.ui.place(child, bounds);
    }

    /// The theme.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        self.ui.theme()
    }
}

/// How a run of text is drawn: style, colour, alignment and wrap width.
///
/// `max_width` is the one field that is not merely cosmetic. It **must**
/// be the width the run was measured at (see
/// [`MeasureCx::measure_text`]), because `SetText` is the only place the
/// server learns a wrap width: measuring wrapped and painting unwrapped
/// reserves two lines of height and draws one overflowing line, and
/// measure and paint disagreeing is the one thing a retained tree cannot
/// tolerate. `0.0` means no wrapping.
#[derive(Debug, Clone, Copy)]
pub struct TextRun<'a> {
    /// Family, size, weight and slant.
    pub style: &'a TextStyle,
    /// Text colour.
    pub color: Color,
    /// Horizontal alignment inside the node's box.
    pub align: Align,
    /// Wrap width in logical pixels; `0.0` = no limit.
    pub max_width: f32,
}

impl<'a> TextRun<'a> {
    /// An unwrapped, left-aligned run in `style`.
    #[must_use]
    pub fn new(style: &'a TextStyle, color: Color) -> Self {
        Self {
            style,
            color,
            align: Align::Left,
            max_width: 0.0,
        }
    }

    /// Set the alignment.
    #[must_use]
    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    /// Set the wrap width; it must match the measurement.
    #[must_use]
    pub fn wrap_at(mut self, max_width: f32) -> Self {
        self.max_width = max_width;
        self
    }
}

/// Context for [`Widget::paint`]: the mapping from a widget to its scene
/// nodes.
///
/// A widget paints into numbered **slots**. Slot 0 is its background,
/// slot 1 its label, and so on — whatever the widget decides, as long as
/// it is stable between paints, because the slot number is what the
/// framework diffs against. The first paint creates the node; later ones
/// send only the properties that changed, so a repaint that produces the
/// same values costs nothing on the wire.
pub struct PaintCx<'a, S> {
    /// The whole tree, minus the widget being painted.
    pub ui: &'a mut Ui<S>,
    /// The widget being painted.
    pub id: WidgetId,
    /// This widget's box, in its own space: `(0, 0, w, h)`.
    pub bounds: Rect,
    pub(crate) group: NodeId,
    /// The content group, if any: the widget's own nodes go in front of
    /// it so children paint on top.
    pub(crate) before: NodeId,
    pub(crate) slots: Vec<crate::wire::PaintSlot>,
    pub(crate) error: Option<Error>,
}

impl<S: 'static> PaintCx<'_, S> {
    /// This widget's size.
    #[must_use]
    pub fn size(&self) -> Size {
        self.bounds.size()
    }

    /// The theme.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        self.ui.theme()
    }

    /// Whether the server can draw text at all.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.ui.has_text()
    }

    /// Draw a (rounded, optionally bordered) rectangle in `slot`.
    pub fn rect(&mut self, slot: u8, rect: Rect, fill: Fill, radius: f32, border: (f32, Color)) {
        let at = self.slot_at(slot);
        let r = self
            .ui
            .wire_mut()
            .paint_rect(&mut self.slots, at, rect, fill, radius, border);
        self.note(r);
    }

    /// Draw a solid rectangle: the common case of [`PaintCx::rect`].
    pub fn fill_rect(&mut self, slot: u8, rect: Rect, color: Color) {
        self.rect(
            slot,
            rect,
            Fill::Solid(color),
            0.0,
            (0.0, Color::TRANSPARENT),
        );
    }

    /// Draw a run of text in `slot`, aligned and wrapped as `run` says.
    ///
    /// See [`TextRun`] for why the wrap width has to be the one the run
    /// was measured at.
    pub fn text(&mut self, slot: u8, rect: Rect, text: &str, run: TextRun<'_>) {
        let at = self.slot_at(slot);
        let r = self
            .ui
            .wire_mut()
            .paint_text(&mut self.slots, at, rect, text, run);
        self.note(r);
    }

    /// Draw a region of a client buffer in `slot`.
    pub fn image(&mut self, slot: u8, rect: Rect, buffer: BufferId, src: nitro_core::IRect) {
        let at = self.slot_at(slot);
        let r = self
            .ui
            .wire_mut()
            .paint_image(&mut self.slots, at, rect, buffer, src);
        self.note(r);
    }

    /// Where slot `slot`'s node goes: under this widget's group, in
    /// front of its content group so children paint on top.
    fn slot_at(&self, slot: u8) -> crate::wire::SlotAt {
        crate::wire::SlotAt {
            parent: self.group,
            before: self.before,
            index: slot as usize,
        }
    }

    fn note(&mut self, r: Result<(), Error>) {
        if let Err(e) = r
            && self.error.is_none()
        {
            self.error = Some(e);
        }
    }
}

/// Context for [`Widget::event`].
pub struct EventCx<'a, S> {
    /// The whole tree, minus the widget handling the event.
    pub ui: &'a mut Ui<S>,
    /// The app's state, for callbacks.
    pub state: &'a mut S,
    /// The widget handling the event.
    pub id: WidgetId,
    /// This widget's box, in its parent's space.
    pub bounds: Rect,
}

impl<S: 'static> EventCx<'_, S> {
    /// Ask for a repaint of this widget.
    pub fn request_paint(&mut self) {
        self.ui.mark(self.id, Dirty::PAINT);
    }

    /// Ask for a re-layout of this widget (and so of its ancestors, if its
    /// size changes).
    pub fn request_layout(&mut self) {
        self.ui.mark(self.id, Dirty::LAYOUT | Dirty::PAINT);
    }

    /// Give this widget the keyboard focus.
    pub fn request_focus(&mut self) {
        self.ui.focus(self.id);
    }

    /// Whether this widget has the keyboard focus.
    #[must_use]
    pub fn has_focus(&self) -> bool {
        self.ui.is_focused(self.id)
    }

    /// Whether the pointer is over this widget.
    #[must_use]
    pub fn is_hovered(&self) -> bool {
        self.ui.is_hovered(self.id)
    }

    /// Whether `pos` (in this widget's own space) is inside it.
    #[must_use]
    pub fn contains(&self, pos: Point) -> bool {
        pos.x >= 0.0 && pos.y >= 0.0 && pos.x < self.bounds.w && pos.y < self.bounds.h
    }

    /// The theme.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        self.ui.theme()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Plain;
    impl Widget<()> for Plain {}

    struct Btn;
    impl Widget<()> for Btn {
        fn role(&self) -> Role {
            Role::Button
        }
    }

    #[test]
    fn the_default_access_comes_from_the_role() {
        assert!(Widget::<()>::accessible(&Plain).actions.is_empty());
        assert_eq!(Widget::<()>::accessible(&Btn).actions, ["activate"]);
        assert_eq!(Widget::<()>::role(&Plain), Role::Other);
        assert_eq!(Role::Button.name(), "button");
        assert_eq!(Role::Container.name(), "container");
        assert_eq!(Role::Label.name(), "label");
        assert_eq!(Role::TextField.name(), "textfield");
        assert_eq!(Role::Spacer.name(), "spacer");
        assert_eq!(Role::Other.name(), "other");
    }

    #[test]
    fn a_default_widget_measures_to_its_minimum() {
        let c = Constraints {
            min: Size::new(4.0, 4.0),
            max: Size::new(10.0, 10.0),
        };
        // No `Ui` is needed to check the default's arithmetic.
        assert_eq!(c.constrain(Size::ZERO), Size::new(4.0, 4.0));
    }

    #[test]
    fn any_widget_downcasts_back() {
        let w: Box<dyn AnyWidget<()>> = Box::new(Btn);
        assert!(w.as_any().downcast_ref::<Btn>().is_some());
        assert!(w.as_any().downcast_ref::<Plain>().is_none());
    }
}
