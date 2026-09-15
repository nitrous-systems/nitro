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
    /// A two-state box.
    Checkbox,
    /// A scrollable viewport.
    Scroll,
    /// A list of rows, of which only the visible ones exist as nodes.
    ///
    /// Its value is the **visible** rows as text, which is what makes
    /// `hey nitro-files get list text` a screenful rather than a
    /// hundred thousand lines: the widget genuinely does not draw the
    /// rest, and reporting them would be a claim about the screen that
    /// is not true. AT-SPI has had the role since the beginning, for the
    /// same reason it has a terminal: a list is addressed by row.
    List,
    /// A value picked from a range.
    Slider,
    /// A terminal emulator's screen.
    ///
    /// Its value is the text on the screen, which is what makes
    /// `hey nitro-term get grid text` a screen dump. The vocabulary is
    /// AT-SPI's, which has had a terminal role since the beginning for
    /// the same reason: a screen reader has to know that this widget's
    /// text is a *screen* — rewritten in place, addressed by row and
    /// column — rather than a document that grows.
    Terminal,
    /// A dividing line.
    Separator,
    /// A picture.
    Image,
    /// A symbolic icon, named rather than drawn.
    ///
    /// Its own role rather than `Image`, because the two are different
    /// things to a script and to a screen reader: an image's value is
    /// "how many pixels", an icon's is *the name of the icon*, which is
    /// the only meaningful thing about it. `hey app get path name` on an
    /// icon answers `gear`.
    Icon,
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
            Role::Checkbox => "checkbox",
            Role::Scroll => "scroll",
            Role::List => "list",
            Role::Slider => "slider",
            Role::Terminal => "terminal",
            Role::Separator => "separator",
            Role::Image => "image",
            Role::Icon => "icon",
            Role::Spacer => "spacer",
            Role::Other => "other",
        }
    }

    /// The role named by `name`, for a protocol path segment.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Role::Container,
            Role::Label,
            Role::Button,
            Role::TextField,
            Role::Checkbox,
            Role::Scroll,
            Role::List,
            Role::Slider,
            Role::Terminal,
            Role::Separator,
            Role::Image,
            Role::Icon,
            Role::Spacer,
            Role::Other,
        ]
        .into_iter()
        .find(|r| r.name() == name)
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

    /// Whether the widget reacts to input at all.
    ///
    /// Part of the introspection surface rather than a convention,
    /// because "is this button greyed out?" is a question an
    /// accessibility client and a test both have to be able to ask of a
    /// widget whose concrete type they do not know.
    fn enabled(&self) -> bool {
        true
    }

    /// Invoke a named action — `click`, `toggle`, `set_value`, … — and
    /// answer [`Handled::Yes`] if this widget knows it.
    ///
    /// This is the introspection socket's `do`, and it is on the trait
    /// next to `event` on purpose: it runs through the same take-out
    /// dispatch, so `cx.state` and `cx.ui` are the real ones and a
    /// scripted `click` fires exactly the callback a real click fires,
    /// with exactly the same invalidation. A widget that answers
    /// [`Handled::No`] reports `err unknown action`; it never panics.
    ///
    /// The names a widget answers to should be the ones it lists in
    /// [`Widget::accessible`]'s `actions`.
    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        let _ = (cx, action, arg);
        Handled::No
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

    /// Whether the server has the symbolic icon set (the `ICONS`
    /// capability).
    ///
    /// Needed at *measure* time, not only at paint time, by any widget
    /// that falls back to something else without it: a
    /// [`Button`](crate::widgets::Button) with an icon draws its label
    /// instead, and a label is not the same size as a 16-px square. A
    /// widget whose measurement disagreed with its paint about which of
    /// the two it was showing would reserve the wrong box.
    #[must_use]
    pub fn has_icons(&self) -> bool {
        self.ui.has_icons()
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

    /// Where every cursor position inside `text` sits, as the server
    /// shaped it. See [`Ui::cursor_positions`](crate::Ui::cursor_positions).
    ///
    /// # Errors
    /// If the connection failed.
    pub fn cursor_positions(
        &mut self,
        text: &str,
        style: &TextStyle,
    ) -> Result<Vec<(u32, f32)>, Error> {
        self.ui.cursor_positions(text, style)
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

/// A paint-slot number.
///
/// A widget's painted nodes are numbered, and the number is what the
/// framework diffs a repaint against, so it has to be stable between
/// paints — slot 0 is *this* widget's background every time.
///
/// It is a `u16` rather than a `u8` because a slot number is not always
/// a name for one of a widget's parts. A widget whose slots *are* its
/// content needs as many as the content has pieces: `nitro-term` gives
/// every same-style run on every row its own slot, which is what lets an
/// unchanged row send nothing, and a wide terminal holds well over 256
/// of them. Nothing else in the tree needs more than five, so the cost
/// of the wider index is two bytes in a struct that already holds a
/// rectangle and a colour.
pub type Slot = u16;

/// How the server is to colour an icon: from the palette, or from the
/// artwork itself.
///
/// The two arms are **two different icon sets**, not two renderings of
/// one. A palette role names the symbolic set compiled into the server
/// (`gear`, `list`, `cpu`), whose artwork is a coverage mask that is
/// tinted at paint time — which is what makes a `theme.scheme` flip free.
/// [`IconTint::Coloured`] names an **application** icon in the machine's
/// XDG icon theme (`firefox`, `org.gnome.Calculator`), which is a picture
/// and is painted in its own colours. Nothing falls back between them:
/// the server answers `BadIcon` for a symbolic name asked for as
/// coloured, and for a theme-only name asked for with a role. See
/// `docs/icons.md`.
///
/// There is deliberately no `Color` arm, for the reason there is no
/// `.color(Color)` on an [`IconBuilder`](crate::widgets::IconBuilder): a
/// colour written down in a widget is a colour the desktop's scheme
/// switch cannot reach, and `deploy/lint-colors.sh` fails the build for
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconTint {
    /// Tint the symbolic set's coverage mask with this palette role.
    Role(nitro_core::Role),
    /// Paint an application icon's own colours, untinted.
    Coloured,
}

impl IconTint {
    /// The `SetIcon.role` byte this tint travels as.
    ///
    /// A `u8` rather than an enum on the wire because the palette has
    /// fewer than 255 roles and the protocol reserved `0xff` for exactly
    /// this before there was anything to put in it — so the full-colour
    /// mode arrived with no wire change at all
    /// (`nitro_wire::msg::SetIcon::AS_COLOURED`).
    #[must_use]
    pub fn role_byte(self) -> u8 {
        match self {
            IconTint::Role(r) => r.index() as u8,
            IconTint::Coloured => nitro_wire::msg::SetIcon::AS_COLOURED,
        }
    }

    /// Whether this is [`IconTint::Coloured`].
    #[must_use]
    pub fn is_coloured(self) -> bool {
        self == IconTint::Coloured
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
///
/// The slot number is a [`Slot`] (a `u16`), not a `u8`. Every built-in
/// widget uses fewer than five, but a widget whose slots are its
/// *content* rather than its parts needs many more: `nitro-term`'s grid
/// gives every style run on every row a slot of its own, which is what
/// lets a row that did not change send nothing at all, and a 200-column
/// terminal can hold more than 256 of them on a single row.
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

    /// The desktop's colours.
    #[must_use]
    pub fn palette(&self) -> &nitro_core::Palette {
        self.ui.palette()
    }

    /// The colour of one [`ColorRole`](nitro_core::Role).
    ///
    /// What a custom widget paints with instead of a literal: the
    /// wallpaper asks for `DesktopTop`, the terminal grid for `Ansi1`.
    /// A colour with no role yet gets one added to `nitro_core::palette`
    /// rather than written down here — see `docs/theme.md`.
    #[must_use]
    pub fn color(&self, role: nitro_core::Role) -> nitro_core::Color {
        self.ui.color(role)
    }

    /// Whether the server can draw text at all.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.ui.has_text()
    }

    /// Whether the server has the symbolic icon set (the `ICONS`
    /// capability).
    #[must_use]
    pub fn has_icons(&self) -> bool {
        self.ui.has_icons()
    }

    /// Keep slot `slot` exactly as it was last painted.
    ///
    /// A slot the paint does not emit has its node destroyed — which is
    /// right for a widget whose slots are its *parts* (a button that
    /// stopped drawing a focus ring wants the ring gone) and wrong for a
    /// widget whose slots are its *content*. `nitro-term` gives every
    /// style run on every row a slot; on a frame where two rows changed,
    /// re-emitting the other fifty rows would cost a string comparison
    /// per run to send nothing, and *not* emitting them would delete the
    /// screen. This is the third answer: say the slot is unchanged, and
    /// the framework neither diffs it nor destroys it.
    ///
    /// Returns whether the slot exists; a slot never painted answers
    /// `false` and nothing happens.
    pub fn keep(&mut self, slot: Slot) -> bool {
        crate::wire::keep_slot(&mut self.slots, slot as usize)
    }

    /// Draw a (rounded, optionally bordered) rectangle in `slot`.
    pub fn rect(&mut self, slot: Slot, rect: Rect, fill: Fill, radius: f32, border: (f32, Color)) {
        let at = self.slot_at(slot);
        let r = self
            .ui
            .wire_mut()
            .paint_rect(&mut self.slots, at, rect, fill, radius, border);
        self.note(r);
    }

    /// Draw a solid rectangle: the common case of [`PaintCx::rect`].
    pub fn fill_rect(&mut self, slot: Slot, rect: Rect, color: Color) {
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
    pub fn text(&mut self, slot: Slot, rect: Rect, text: &str, run: TextRun<'_>) {
        let at = self.slot_at(slot);
        let r = self
            .ui
            .wire_mut()
            .paint_text(&mut self.slots, at, rect, text, run);
        self.note(r);
    }

    /// Draw a symbolic icon in `slot`, by **name**.
    ///
    /// `rect` is the box the icon is centred in; `size` is the icon's own
    /// square side in logical pixels and `role` the palette role the
    /// server tints it with. No pixels cross the wire, which is what
    /// makes it work identically on a remote link and recolour itself
    /// when the scheme flips — see `docs/icons.md`.
    ///
    /// For a full-colour **application** icon use
    /// [`PaintCx::icon_tinted`] with [`IconTint::Coloured`]; the two are
    /// different namespaces on the server and nothing falls back between
    /// them.
    pub fn icon(&mut self, slot: Slot, rect: Rect, name: &str, size: f32, role: nitro_core::Role) {
        self.icon_tinted(slot, rect, name, size, IconTint::Role(role));
    }

    /// Draw an icon in `slot` with an explicit [`IconTint`]: a palette
    /// role, or the icon's own colours.
    ///
    /// The additive twin of [`PaintCx::icon`] rather than a change to its
    /// signature, because `icon(.., role)` is the overwhelmingly common
    /// call and every existing caller — inside this crate and in
    /// `nitro-bar`, `nitro-launcher`, `nitro-settings` — means exactly
    /// that. A widget that wants the other mode says so.
    pub fn icon_tinted(&mut self, slot: Slot, rect: Rect, name: &str, size: f32, tint: IconTint) {
        let at = self.slot_at(slot);
        let r =
            self.ui
                .wire_mut()
                .paint_icon(&mut self.slots, at, rect, name, size, tint.role_byte());
        self.note(r);
    }

    /// Draw a region of a client buffer in `slot`.
    pub fn image(&mut self, slot: Slot, rect: Rect, buffer: BufferId, src: nitro_core::IRect) {
        let at = self.slot_at(slot);
        let r = self
            .ui
            .wire_mut()
            .paint_image(&mut self.slots, at, rect, buffer, src);
        self.note(r);
    }

    /// Draw a **group** in `slot`: a node of this widget's own that can
    /// clip and translate what is painted inside it, and whose node id
    /// is returned so later slots can be parented to it.
    ///
    /// This is how a scrolling or clipping widget is built: put the
    /// content in a clipping group and scroll it by moving the group's
    /// transform, and the content underneath never repaints. A group
    /// slot's transform can be changed on its own with
    /// [`WidgetMut::set_slot_transform`](crate::WidgetMut::set_slot_transform),
    /// which is one `SetTransform` and nothing else.
    ///
    /// Returns [`NodeId::NONE`] if the slot could not be created; the
    /// error is reported by the pass.
    pub fn group(
        &mut self,
        slot: Slot,
        rect: Rect,
        clip: bool,
        transform: nitro_core::Transform,
    ) -> NodeId {
        let at = self.slot_at(slot);
        match self
            .ui
            .wire_mut()
            .paint_group(&mut self.slots, at, rect, clip, transform)
        {
            Ok(n) => n,
            Err(e) => {
                self.note(Err(e));
                NodeId::NONE
            }
        }
    }

    /// Draw a rect in `slot`, parented to a group slot's node rather
    /// than to the widget's own group.
    pub fn rect_in(
        &mut self,
        parent: NodeId,
        slot: Slot,
        rect: Rect,
        fill: Fill,
        radius: f32,
        border: (f32, Color),
    ) {
        let at = crate::wire::SlotAt {
            parent,
            before: NodeId::NONE,
            index: slot as usize,
        };
        let r = self
            .ui
            .wire_mut()
            .paint_rect(&mut self.slots, at, rect, fill, radius, border);
        self.note(r);
    }

    /// Draw a symbolic icon in `slot`, parented to a group slot's node
    /// rather than to the widget's own group.
    ///
    /// The `_in` twin of [`PaintCx::icon_tinted`], for the same reason
    /// [`PaintCx::text_in`] exists: a virtualised widget puts its content
    /// inside a clipping group of its own so that scrolling is one
    /// `SetTransform`, and the content's slots have to be parented there.
    pub fn icon_in(
        &mut self,
        parent: NodeId,
        slot: Slot,
        rect: Rect,
        name: &str,
        size: f32,
        tint: IconTint,
    ) {
        let at = crate::wire::SlotAt {
            parent,
            before: NodeId::NONE,
            index: slot as usize,
        };
        let r =
            self.ui
                .wire_mut()
                .paint_icon(&mut self.slots, at, rect, name, size, tint.role_byte());
        self.note(r);
    }

    /// Draw text in `slot`, parented to a group slot's node.
    pub fn text_in(
        &mut self,
        parent: NodeId,
        slot: Slot,
        rect: Rect,
        text: &str,
        run: TextRun<'_>,
    ) {
        let at = crate::wire::SlotAt {
            parent,
            before: NodeId::NONE,
            index: slot as usize,
        };
        let r = self
            .ui
            .wire_mut()
            .paint_text(&mut self.slots, at, rect, text, run);
        self.note(r);
    }

    /// Register `pixels` with the server as a buffer and return its id.
    ///
    /// The pixels go into a memfd and the descriptor is passed over the
    /// wire, so the server maps them rather than copying: an image costs
    /// one page-table entry per side, not two copies of the picture.
    /// `None` if the memfd or the send failed.
    ///
    /// **A remote link is `None`, not an error.** A buffer is a file
    /// descriptor and a descriptor cannot cross TCP
    /// ([`caps::REMOTE`](nitro_wire::types::caps::REMOTE)), which is a
    /// permanent property of the connection rather than something that
    /// went wrong — so it is reported the way "there is no buffer" is
    /// already reported, and the widget draws nothing.
    ///
    /// Routing it through [`PaintCx::note`] like any other error would
    /// fail the paint pass, and a failed paint pass ends
    /// [`Ui::flush`](crate::Ui::flush), which ends the app's event loop:
    /// a remote app with one `Image` in its tree would **exit on its
    /// first paint** instead of drawing the rest of the tree. Everything
    /// else — a failed memfd, a broken socket — is a real failure and
    /// still goes through `note`.
    pub fn upload_image(
        &mut self,
        width: u32,
        height: u32,
        alpha: bool,
        pixels: &[u8],
    ) -> Option<BufferId> {
        match self
            .ui
            .wire_mut()
            .create_buffer(width, height, alpha, pixels)
        {
            Ok(id) => Some(id),
            Err(Error::Wire(nitro_wire::Error::RemoteNoFds)) => None,
            Err(e) => {
                self.note(Err(e));
                None
            }
        }
    }

    /// Release a server-side buffer this widget allocated.
    ///
    /// The server frees its mapping at the next commit. Client-side ids
    /// are monotonic and never recycled, so there is no window in which
    /// a released id could name something else.
    pub fn release_image(&mut self, buffer: BufferId) {
        let _ = self.ui.wire_mut().destroy_buffer(buffer);
    }

    /// Where slot `slot`'s node goes: under this widget's group, in
    /// front of its content group so children paint on top.
    fn slot_at(&self, slot: Slot) -> crate::wire::SlotAt {
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
    ///
    /// **Ignored in a `NO_FOCUS` window** — a bar, a dock, a launcher
    /// overlay, a wallpaper. That window never receives a key, so focus
    /// inside it buys nothing and costs a focus ring on whatever was last
    /// clicked, plus a `focused` flag in `hey … list` that is a lie about
    /// a surface the server will not focus. A click still activates the
    /// widget; only the focus move is dropped. See
    /// [`Ui::click_takes_focus`](crate::Ui::click_takes_focus), and use
    /// [`Ui::focus`](crate::Ui::focus) directly for the deliberate case
    /// (the launcher's query field, which reads the keyboard through a
    /// grab).
    pub fn request_focus(&mut self) {
        if !self.ui.click_takes_focus() {
            return;
        }
        self.ui.focus(self.id);
    }

    /// Whether this widget has the keyboard focus.
    #[must_use]
    pub fn has_focus(&self) -> bool {
        self.ui.is_focused(self.id)
    }

    /// Announce that this widget did the thing it exists to do — a
    /// button was clicked, a menu item chosen.
    ///
    /// A value change is visible to a watcher by diffing the
    /// introspection tree; an activation is not, because a button that
    /// ran its callback looks exactly like one that did not. This is how
    /// it becomes a `click` event on the introspection socket, and it
    /// costs nothing when nothing is watching.
    pub fn report_activation(&mut self) {
        self.ui.report_activation(self.id);
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
        assert_eq!(Role::from_name("slider"), Some(Role::Slider));
        assert_eq!(Role::from_name("nope"), None);
        for r in [
            Role::Container,
            Role::Label,
            Role::Button,
            Role::TextField,
            Role::Checkbox,
            Role::Scroll,
            Role::Slider,
            Role::Separator,
            Role::Image,
            Role::Icon,
            Role::Spacer,
            Role::Other,
        ] {
            assert_eq!(Role::from_name(r.name()), Some(r), "{r:?}");
        }
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
