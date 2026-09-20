//! The **split view** blueprint: categories or places on the left,
//! content on the right.
//!
//! It is the layout every current desktop converges on — GNOME Settings
//! and Files, macOS System Settings and Finder — and this module ships
//! it as a set of builders so `nitro-settings`, `nitro-files` and the
//! apps after them share one anatomy rather than each drawing its own.
//! The research it is built from is in `docs/research/`; the anatomy and
//! the roles each part reads are in `docs/ui.md`, "Split view blueprint".
//!
//! ```text
//! ┌──────────────┬─────────────────────────────────────────────┐
//! │ Settings     │ Displays                          [header]  │
//! │              ├─────────────────────────────────────────────┤
//! │ ▣ Displays   │  Outputs                        [caption]   │
//! │   Keyboard   │ ┌─────────────────────────────────────────┐ │
//! │   Audio      │ │ Label                 control           │ │  card
//! │   Appearance │ │·········································│ │  row
//! │              │ │ Label                 control           │ │
//! │  [sidebar]   │ └─────────────────────────────────────────┘ │
//! │              │  A dim footnote under the card.  [footnote] │
//! └──────────────┴─────────────────────────────────────────────┘
//! ```
//!
//! Everything here is composed from the existing widgets plus five
//! small new ones — [`SidebarRow`], [`Card`], [`CardRow`], [`Pages`] and
//! [`Switch`] — and every colour is a palette role, so the whole view
//! follows a `theme.scheme` switch without being told.

use nitro_core::{Color, Rect, Size};
use nitro_wire::msg::Fill;

use crate::arena::Dirty;
use crate::build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled, button, key};
use crate::layout::{Constraints, CrossAlign, Direction, Edges, Length, ShrinkFloor};
use crate::theme::TextStyle;
use crate::ui::{Ui, WidgetMut};
use crate::widget::{Access, EventCx, LayoutCx, MeasureCx, PaintCx, Role, Widget};
use crate::widgets::{Flex, column, icon, label, panel, row, scroll, separator, spacer};
use crate::{ColorRole, WidgetId};

// ---------------------------------------------------------------------
// The measurements, in one place
// ---------------------------------------------------------------------

/// Default sidebar width. GNOME uses 200–230, macOS 180–215.
pub const SIDEBAR_WIDTH: f32 = 200.0;
/// The sidebar never shrinks below this.
pub const SIDEBAR_MIN_WIDTH: f32 = 160.0;
/// Height of a pane header (GNOME 47, macOS 52).
pub const HEADER_HEIGHT: f32 = 46.0;
/// Height of a sidebar row (GNOME 34, macOS 28).
pub const SIDEBAR_ROW_HEIGHT: f32 = 32.0;
/// Inset of the rows from the sidebar's edges.
pub const SIDEBAR_ROW_INSET: f32 = 6.0;
/// Corner radius of a sidebar row's selection pill.
pub const SIDEBAR_ROW_RADIUS: f32 = 8.0;
/// A sidebar row's icon side.
pub const SIDEBAR_ICON_PX: f32 = 16.0;
/// Gap between a sidebar row's icon and label.
pub const SIDEBAR_ICON_GAP: f32 = 10.0;
/// A sidebar row's label size.
pub const SIDEBAR_LABEL_PX: f32 = 14.0;
/// Horizontal padding inside a sidebar row.
pub const SIDEBAR_ROW_PAD_X: f32 = 12.0;
/// A card's corner radius (GNOME 12, macOS 8–10).
pub const CARD_RADIUS: f32 = 10.0;
/// A card row is never shorter than this.
pub const CARD_ROW_MIN_HEIGHT: f32 = 40.0;
/// Horizontal padding inside a card row; the row separators are inset
/// by the same amount on the left (the macOS convention).
pub const CARD_ROW_PAD_X: f32 = 12.0;
/// Vertical padding inside a card row.
pub const CARD_ROW_PAD_Y: f32 = 8.0;
/// A content column's side gutters.
pub const CONTENT_GUTTER: f32 = 20.0;
/// Gap between the items in a content column: a caption sits 6 above
/// its card and a footnote 6 below it; [`group_caption`] adds 18 above
/// itself so groups are 24 apart.
pub const CONTENT_GAP: f32 = 6.0;
/// The content column's default width clamp (GNOME's is 600).
pub const CONTENT_MAX_WIDTH: f32 = 600.0;
/// A switch's track, width then height.
pub const SWITCH_SIZE: (f32, f32) = (40.0, 22.0);
/// A group caption's size and weight.
pub const CAPTION_PX: f32 = 15.0;
/// A pane header title's size.
pub const HEADER_TITLE_PX: f32 = 15.0;
/// Size of the small text: subtitles, footnotes, section headers.
pub const SMALL_PX: f32 = 12.0;
/// Size of a card row's label.
pub const ROW_LABEL_PX: f32 = 14.0;

/// The names the split view's slots answer to, so a test or `hey` can
/// resolve them without holding ids.
pub mod names {
    /// The whole view.
    pub const SPLIT: &str = "split";
    /// The sidebar's rows column.
    pub const SIDEBAR: &str = "sidebar";
    /// The sidebar's header row.
    pub const SIDEBAR_HEADER: &str = "sidebar_header";
    /// The content pane.
    pub const CONTENT: &str = "content";
    /// The content header row.
    pub const CONTENT_HEADER: &str = "content_header";
    /// The content header's title label.
    pub const TITLE: &str = "title";
    /// The content body.
    pub const CONTENT_BODY: &str = "content_body";
    /// The content footer.
    pub const CONTENT_FOOTER: &str = "content_footer";
}

// ---------------------------------------------------------------------
// A pressable face: the state machine SidebarRow and CardRow share
// ---------------------------------------------------------------------

type ClickFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>)>;

/// The press/release/keyboard state of a clickable container, shared by
/// [`SidebarRow`] and a navigation [`CardRow`]. Deliberately the same
/// shape as [`Button`](crate::widgets::Button)'s so a scripted `click`
/// and a real release are indistinguishable to the app.
struct Pressable<S> {
    pressed: bool,
    on_click: Option<ClickFn<S>>,
}

impl<S: 'static> Pressable<S> {
    fn new() -> Self {
        Self {
            pressed: false,
            on_click: None,
        }
    }

    fn activate(&mut self, cx: &mut EventCx<'_, S>) {
        cx.report_activation();
        let Some(cb) = self.on_click.take() else {
            return;
        };
        cb(cx.state, cx.ui);
        self.on_click = Some(cb);
    }

    /// The common event handling. Answers `None` for an event the caller
    /// should look at itself.
    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Option<Handled> {
        match ev {
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                self.pressed = true;
                cx.request_focus();
                cx.request_paint();
                Some(Handled::Yes)
            }
            Event::PointerUp { pos, button } if *button == button::LEFT => {
                let was = self.pressed;
                self.pressed = false;
                cx.request_paint();
                if was && cx.contains(*pos) {
                    self.activate(cx);
                }
                Some(Handled::Yes)
            }
            Event::KeyDown(k) if k.keycode == key::SPACE || k.keycode == key::ENTER => {
                self.pressed = true;
                cx.request_paint();
                Some(Handled::Yes)
            }
            Event::KeyUp(k) if k.keycode == key::SPACE || k.keycode == key::ENTER => {
                let was = self.pressed;
                self.pressed = false;
                cx.request_paint();
                if was {
                    self.activate(cx);
                }
                Some(Handled::Yes)
            }
            Event::PointerLeave => {
                self.pressed = false;
                cx.request_paint();
                Some(Handled::No)
            }
            Event::PointerEnter { .. } | Event::FocusChanged { .. } => {
                cx.request_paint();
                Some(Handled::No)
            }
            _ => None,
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str) -> Handled {
        match action {
            "click" | "activate" | "press" => {
                self.activate(cx);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

// ---------------------------------------------------------------------
// SidebarRow
// ---------------------------------------------------------------------

/// One row of the sidebar: a 16 px symbolic icon, a label and an
/// optional badge, on a rounded pill that is transparent, hover-tinted
/// or selected.
///
/// A *container* rather than a single paint routine: the icon and the
/// label are its children, so the icon stays a named [`Icon`]
/// (`crate::widgets::Icon`) an app can address by name and tests can
/// find. The row paints only its face.
///
/// Selection is **neutral** ([`ColorRole::SidebarSelected`]) rather than
/// accent-coloured, as on both desktops: the accent is for the control
/// that wants attention, and the category you are already on is not it.
///
/// A row that selects *itself* from its own `on_click` must
/// [`Ui::defer`] the `set_selected`: the row is out of its slot while its
/// callback runs. The usual shape is one deferred `select(index)` that
/// updates every row.
pub struct SidebarRow<S> {
    label: String,
    selected: bool,
    press: Pressable<S>,
}

impl<S> std::fmt::Debug for SidebarRow<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidebarRow")
            .field("label", &self.label)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl<S> SidebarRow<S> {
    /// The row's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the row is the selected one.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// Whether the pointer is pressed on it.
    #[must_use]
    pub fn is_pressed(&self) -> bool {
        self.press.pressed
    }
}

/// The children a [`SidebarRow`] builds, by position.
mod sidebar_child {
    pub(super) const LABEL: usize = 1;
    pub(super) const BADGE: usize = 2;
}

impl<S: 'static> SidebarRow<S> {
    /// Move the focus to the previous or next sidebar row among this
    /// row's siblings, and activate it: Up/Down walk the categories.
    fn step(cx: &mut EventCx<'_, S>, backwards: bool) -> Handled {
        let Some(parent) = cx.ui.parent(cx.id) else {
            return Handled::No;
        };
        // This row is out of its slot while it handles the key, so it
        // cannot be downcast; it is a sidebar row by construction.
        let rows: Vec<WidgetId> = cx
            .ui
            .children(parent)
            .into_iter()
            .filter(|c| *c == cx.id || cx.ui.widget::<Self>(*c).is_ok())
            .collect();
        let Some(i) = rows.iter().position(|r| *r == cx.id) else {
            return Handled::No;
        };
        let next = if backwards {
            i.checked_sub(1)
        } else {
            (i + 1 < rows.len()).then_some(i + 1)
        };
        let Some(next) = next else {
            return Handled::Yes;
        };
        let target = rows[next];
        cx.ui.focus(target);
        // The sibling is in its slot (only *this* row is out), so its
        // own `click` runs the real callback with the real state.
        let _ = cx.ui.action(cx.state, target, "click", None);
        Handled::Yes
    }
}

impl<S: 'static> Widget<S> for SidebarRow<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        measure_container(cx, constraints)
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        // Transparent rather than omitted: the node exists either way,
        // so a hover is one `SetFill` and never a `CreateNode`.
        let face = if self.selected {
            cx.color(ColorRole::SidebarSelected)
        } else if hovered || self.press.pressed {
            cx.color(ColorRole::SidebarHover)
        } else {
            Color::TRANSPARENT
        };
        let border = if focused {
            (1.0, cx.color(ColorRole::Focus))
        } else {
            (0.0, Color::TRANSPARENT)
        };
        let bounds = cx.bounds;
        cx.rect(0, bounds, Fill::Solid(face), SIDEBAR_ROW_RADIUS, border);
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if let Some(h) = self.press.event(cx, ev) {
            return h;
        }
        match ev {
            Event::KeyDown(k) if k.keycode == key::UP => Self::step(cx, true),
            Event::KeyDown(k) if k.keycode == key::DOWN => Self::step(cx, false),
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.label.clone()),
            value: Some(self.label.clone()),
            actions: vec!["click", "activate", "focus", "set_selected"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "set_selected" => {
                let to = matches!(arg, Some("true" | "1" | "on" | "yes"));
                if self.selected != to {
                    self.selected = to;
                    cx.request_paint();
                }
                Handled::Yes
            }
            _ => self.press.action(cx, action),
        }
    }
}

/// Setters for a live [`SidebarRow`].
impl<S: 'static> WidgetMut<'_, SidebarRow<S>, S> {
    /// Select or deselect the row. Paint only; a no-op if unchanged.
    pub fn set_selected(&mut self, selected: bool) {
        if self.selected == selected {
            return;
        }
        self.selected = selected;
        self.request_paint();
    }

    /// Replace the label.
    pub fn set_label(&mut self, text: impl Into<String>) {
        let text = text.into();
        if self.label == text {
            return;
        }
        self.label.clone_from(&text);
        let id = self.id();
        let kids = self.ui().children(id);
        if let Some(l) = kids.get(sidebar_child::LABEL).copied()
            && let Ok(mut l) = self.ui().widget_mut::<crate::widgets::Label>(l)
        {
            l.set_text(text);
        }
    }

    /// Replace the badge text, or remove the badge with `None`.
    pub fn set_badge(&mut self, text: Option<String>) {
        let id = self.id();
        let kids = self.ui().children(id);
        let existing = kids.get(sidebar_child::BADGE).copied();
        match (existing, text) {
            (Some(b), Some(t)) => {
                if let Ok(mut l) = self.ui().widget_mut::<crate::widgets::Label>(b) {
                    l.set_text(t);
                }
            }
            (Some(b), None) => {
                let _ = self.ui().remove(b);
            }
            (None, Some(t)) => {
                let _ = self.ui().add_child(id, badge(t));
            }
            (None, None) => {}
        }
    }

    /// Replace the click callback.
    pub fn set_on_click(&mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) {
        self.press.on_click = Some(Box::new(f));
    }
}

fn badge<S: 'static>(text: String) -> crate::widgets::LabelBuilder<S> {
    label(text).size(SMALL_PX).color_role(ColorRole::TextDim)
}

/// Builder for a [`SidebarRow`].
pub struct SidebarRowBuilder<S> {
    built: Built<S>,
    row: SidebarRow<S>,
    icon: crate::widgets::IconBuilder<S>,
    badge: Option<String>,
}

impl<S: 'static> SidebarRowBuilder<S> {
    /// Start selected.
    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.row.selected = selected;
        self
    }

    /// What to do when the row is clicked (or activated by keyboard or
    /// script).
    #[must_use]
    pub fn on_click(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.row.press.on_click = Some(Box::new(f));
        self
    }

    /// A small dim count or note at the trailing end (a mailbox's
    /// unread count, a room's mentions).
    #[must_use]
    pub fn badge(mut self, text: impl Into<String>) -> Self {
        self.badge = Some(text.into());
        self
    }

    /// Give the icon child an addressing name of its own.
    #[must_use]
    pub fn icon_name(mut self, name: impl Into<String>) -> Self {
        self.icon = self.icon.name(name);
        self
    }
}

impl<S: 'static> StyleBuilder<S> for SidebarRowBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for SidebarRowBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let text = self.row.label.clone();
        self.built.push(self.icon.into_widget());
        self.built.push(
            label(text)
                .size(SIDEBAR_LABEL_PX)
                .color_role(ColorRole::Text)
                .elide(true)
                .grow(1.0)
                .into_widget(),
        );
        if let Some(b) = self.badge.take() {
            self.built.push(badge(b).into_widget());
        }
        self.built.replace_widget(self.row);
        self.built
    }
}

/// A sidebar row with `icon` (a symbolic icon name) and `label`.
#[must_use]
pub fn sidebar_row<S: 'static>(
    icon_name: impl Into<String>,
    text: impl Into<String>,
) -> SidebarRowBuilder<S> {
    let mut built: Built<S> = Built::new(Flex);
    {
        let st = built.state_mut();
        st.focusable = true;
        st.style.direction = Direction::Row;
        st.style.cross_align = CrossAlign::Center;
        st.style.gap = SIDEBAR_ICON_GAP;
        st.style.padding = Edges::symmetric(SIDEBAR_ROW_PAD_X, 0.0);
        st.style.height = Length::Px(SIDEBAR_ROW_HEIGHT);
        st.style.min_height = Some(SIDEBAR_ROW_HEIGHT);
        st.style.width = Length::Percent(1.0);
    }
    SidebarRowBuilder {
        built,
        row: SidebarRow {
            label: text.into(),
            selected: false,
            press: Pressable::new(),
        },
        icon: icon(icon_name)
            .size(SIDEBAR_ICON_PX)
            .color_role(ColorRole::Text),
        badge: None,
    }
}

/// A small bold dim heading above a run of sidebar rows ("Places").
#[must_use]
pub fn sidebar_section<S: 'static>(title: impl Into<String>) -> Built<S> {
    let mut b = row()
        .width_percent(1.0)
        .padding_xy(SIDEBAR_ROW_PAD_X, 0.0)
        .child(
            label(title)
                .size(SMALL_PX)
                .weight(700)
                .color_role(ColorRole::TextDim),
        )
        .into_widget();
    b.state_mut().style.margin = Edges {
        left: 0.0,
        top: 12.0,
        right: 0.0,
        bottom: 4.0,
    };
    b
}

/// A hairline between two runs of sidebar rows, with 6 px above and
/// below.
#[must_use]
pub fn sidebar_separator<S: 'static>() -> Built<S> {
    let mut b = separator()
        .color_role(ColorRole::Hairline)
        .width_percent(1.0)
        .into_widget();
    b.state_mut().style.margin = Edges::symmetric(SIDEBAR_ROW_PAD_X - SIDEBAR_ROW_INSET, 6.0);
    b
}

// ---------------------------------------------------------------------
// Card
// ---------------------------------------------------------------------

/// A "boxed list" / "form group": a surface with a hairline ring holding
/// [`CardRow`]s, with an inset hairline between each pair of rows and
/// none after the last.
///
/// The ring stands in for the shadow the two desktops draw: the scene
/// has no shadows (`Fill` is solid or a gradient), and a 1-px hairline is
/// what reads as a card without one.
#[derive(Debug, Default)]
pub struct Card {
    /// The y of each separator from the last layout, so a settled card
    /// repaints nothing and one whose rows moved repaints once.
    separators: Vec<f32>,
}

impl Card {
    /// The separators the card currently paints, as y offsets.
    #[must_use]
    pub fn separators(&self) -> &[f32] {
        &self.separators
    }
}

impl<S: 'static> Widget<S> for Card {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        measure_container(cx, constraints)
    }

    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        cx.layout_children(bounds);
        let ys: Vec<f32> = cx
            .children()
            .iter()
            .skip(1)
            .map(|c| cx.ui.bounds(*c).y)
            .collect();
        if ys != self.separators {
            self.separators = ys;
            let id = cx.id;
            cx.ui.mark(id, Dirty::PAINT);
        }
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let surface = cx.color(ColorRole::Surface);
        let hairline = cx.color(ColorRole::Hairline);
        let bounds = cx.bounds;
        cx.rect(
            0,
            bounds,
            Fill::Solid(surface),
            CARD_RADIUS,
            (1.0, hairline),
        );
        for (i, y) in self.separators.clone().into_iter().enumerate() {
            let slot = crate::widget::Slot::try_from(i + 1).unwrap_or(u16::MAX);
            cx.fill_rect(
                slot,
                Rect::new(CARD_ROW_PAD_X, y, (bounds.w - CARD_ROW_PAD_X).max(0.0), 1.0),
                hairline,
            );
        }
    }

    fn role(&self) -> Role {
        Role::Container
    }
}

/// Builder for a [`Card`].
pub struct CardBuilder<S> {
    built: Built<S>,
}

impl<S: 'static> StyleBuilder<S> for CardBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for CardBuilder<S> {}

impl<S: 'static> IntoWidget<S> for CardBuilder<S> {
    fn into_widget(self) -> Built<S> {
        self.built
    }
}

/// An empty card, full width, no padding: its children are the rows.
#[must_use]
pub fn card<S: 'static>() -> CardBuilder<S> {
    let mut built: Built<S> = Built::new(Card::default());
    {
        let st = built.state_mut();
        st.style.direction = Direction::Column;
        st.style.width = Length::Percent(1.0);
        st.style.cross_align = CrossAlign::Stretch;
    }
    CardBuilder { built }
}

// ---------------------------------------------------------------------
// CardRow
// ---------------------------------------------------------------------

/// One row of a [`Card`]: a label (and optional dim subtitle) on the
/// left, one trailing control on the right.
///
/// A plain row is a container and paints nothing. A row given
/// [`CardRowBuilder::on_click`] is a **navigation row**: it becomes a
/// button, gains a `chevron-right` and a hover face, and reports
/// `click` like any other button.
pub struct CardRow<S> {
    label: String,
    clickable: bool,
    press: Pressable<S>,
}

impl<S> std::fmt::Debug for CardRow<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardRow")
            .field("label", &self.label)
            .field("clickable", &self.clickable)
            .finish_non_exhaustive()
    }
}

impl<S> CardRow<S> {
    /// The row's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the row is a navigation row.
    #[must_use]
    pub fn is_clickable(&self) -> bool {
        self.clickable
    }
}

impl<S: 'static> Widget<S> for CardRow<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        measure_container(cx, constraints)
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        if !self.clickable {
            return;
        }
        // Hover only on a navigation row, and only faintly: nothing clips
        // to the card's radius, so a face on every row would poke out of
        // the corners. macOS draws none; GNOME's is 7 %.
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        let face = if hovered || self.press.pressed {
            cx.color(ColorRole::SidebarHover)
        } else {
            Color::TRANSPARENT
        };
        let border = if focused {
            (1.0, cx.color(ColorRole::Focus))
        } else {
            (0.0, Color::TRANSPARENT)
        };
        let bounds = cx.bounds;
        cx.rect(0, bounds, Fill::Solid(face), 0.0, border);
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if !self.clickable {
            return Handled::No;
        }
        self.press.event(cx, ev).unwrap_or(Handled::No)
    }

    fn role(&self) -> Role {
        if self.clickable {
            Role::Button
        } else {
            Role::Container
        }
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.label.clone()),
            value: Some(self.label.clone()),
            actions: if self.clickable {
                vec!["click", "activate", "focus"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, _arg: Option<&str>) -> Handled {
        if !self.clickable {
            return Handled::No;
        }
        self.press.action(cx, action)
    }
}

/// Builder for a [`CardRow`].
pub struct CardRowBuilder<S> {
    built: Built<S>,
    row: CardRow<S>,
    subtitle: Option<String>,
    trailing: Vec<Built<S>>,
}

impl<S: 'static> CardRowBuilder<S> {
    /// A dim second line under the label.
    #[must_use]
    pub fn subtitle(mut self, text: impl Into<String>) -> Self {
        self.subtitle = Some(text.into());
        self
    }

    /// The control at the trailing end: a switch, a field, a slider, a
    /// button, a dim value label — any widget.
    #[must_use]
    pub fn trailing(mut self, w: impl IntoWidget<S>) -> Self {
        self.trailing.push(w.into_widget());
        self
    }

    /// A dim value shown at the trailing end (`.trailing(label(..))` in
    /// the row's own style).
    #[must_use]
    pub fn value(self, text: impl Into<String>) -> Self {
        self.trailing(
            label(text)
                .size(ROW_LABEL_PX)
                .color_role(ColorRole::TextDim),
        )
    }

    /// Make this a navigation row: clickable, with a chevron.
    #[must_use]
    pub fn on_click(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.row.clickable = true;
        self.row.press.on_click = Some(Box::new(f));
        self.built.state_mut().focusable = true;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for CardRowBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for CardRowBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let mut text = column().gap(2.0).grow(1.0).shrink_to_zero().child(
            label(self.row.label.clone())
                .size(ROW_LABEL_PX)
                .color_role(ColorRole::Text)
                .elide(true),
        );
        if let Some(sub) = self.subtitle.take() {
            text = text.child(
                label(sub)
                    .size(SMALL_PX)
                    .color_role(ColorRole::TextDim)
                    .elide(true),
            );
        }
        self.built.push(text.into_widget());
        for t in self.trailing.drain(..) {
            self.built.push(t);
        }
        if self.row.clickable {
            self.built.push(
                icon("chevron-right")
                    .size(SIDEBAR_ICON_PX)
                    .color_role(ColorRole::TextDim)
                    .into_widget(),
            );
        }
        self.built.replace_widget(self.row);
        self.built
    }
}

/// A card row labelled `text`.
#[must_use]
pub fn card_row<S: 'static>(text: impl Into<String>) -> CardRowBuilder<S> {
    let mut built: Built<S> = Built::new(Flex);
    {
        let st = built.state_mut();
        st.style.direction = Direction::Row;
        st.style.cross_align = CrossAlign::Center;
        st.style.gap = CARD_ROW_PAD_X;
        st.style.padding = Edges::symmetric(CARD_ROW_PAD_X, CARD_ROW_PAD_Y);
        st.style.min_height = Some(CARD_ROW_MIN_HEIGHT);
        st.style.width = Length::Percent(1.0);
    }
    CardRowBuilder {
        built,
        row: CardRow {
            label: text.into(),
            clickable: false,
            press: Pressable::new(),
        },
        subtitle: None,
        trailing: Vec::new(),
    }
}

/// A bold caption above a card ("Outputs", "Colour scheme").
#[must_use]
pub fn group_caption<S: 'static>(text: impl Into<String>) -> crate::widgets::LabelBuilder<S> {
    let mut b = label(text)
        .size(CAPTION_PX)
        .weight(600)
        .color_role(ColorRole::Text)
        .width_percent(1.0);
    // 24 above (18 + the column's 6 gap), 6 below (the gap alone).
    b.built_mut().state_mut().style.margin = Edges {
        left: 0.0,
        top: 18.0,
        right: 0.0,
        bottom: 0.0,
    };
    b
}

/// A dim, wrapped note under a card.
#[must_use]
pub fn footnote<S: 'static>(text: impl Into<String>) -> crate::widgets::LabelBuilder<S> {
    let mut b = label(text)
        .size(SMALL_PX)
        .color_role(ColorRole::TextDim)
        .width_percent(1.0);
    b.built_mut().state_mut().style.margin = Edges {
        left: CARD_ROW_PAD_X,
        top: 0.0,
        right: CARD_ROW_PAD_X,
        bottom: 0.0,
    };
    b
}

// ---------------------------------------------------------------------
// content_column
// ---------------------------------------------------------------------

/// Builder for the scrolling content body: a [`Scroll`]
/// (`crate::widgets::Scroll`) holding a centred column with 20 px gutters
/// and 24 px between groups, clamped to [`CONTENT_MAX_WIDTH`] unless
/// [`ContentColumnBuilder::unclamped`].
pub struct ContentColumnBuilder<S> {
    children: Vec<Built<S>>,
    name: Option<String>,
    max_width: Option<f32>,
}

impl<S: 'static> ContentColumnBuilder<S> {
    /// Append a group: a caption, a card, a footnote.
    #[must_use]
    pub fn child(mut self, c: impl IntoWidget<S>) -> Self {
        self.children.push(c.into_widget());
        self
    }

    /// Append several.
    #[must_use]
    pub fn children<I>(mut self, cs: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoWidget<S>,
    {
        for c in cs {
            self.children.push(c.into_widget());
        }
        self
    }

    /// The addressing name of the groups column (a `container`, so
    /// `hey` addresses a page's controls as `<name>/<control>`).
    #[must_use]
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = Some(n.into());
        self
    }

    /// Clamp the column to `px` (default [`CONTENT_MAX_WIDTH`]).
    #[must_use]
    pub fn max_width(mut self, px: f32) -> Self {
        self.max_width = Some(px);
        self
    }

    /// No clamp: the column is as wide as the pane.
    #[must_use]
    pub fn unclamped(mut self) -> Self {
        self.max_width = None;
        self
    }
}

/// The ids of a built [`ContentColumnBuilder`], for an app that attaches
/// groups it built earlier (because their callbacks capture ids).
#[derive(Debug, Clone, Copy)]
pub struct ContentColumn {
    /// The scrolling page; what goes into a [`pages`] stack or a
    /// [`SplitViewBuilder::content`].
    pub page: WidgetId,
    /// The column the groups go in: attach captions, cards and
    /// footnotes here.
    pub column: WidgetId,
}

impl<S: 'static> ContentColumnBuilder<S> {
    /// Materialise the column and hand back its ids.
    ///
    /// # Panics
    /// Never in practice: the attach names an id built a line earlier.
    pub fn build(self, ui: &mut Ui<S>) -> ContentColumn {
        let page = ui.build(self);
        let wrapper = ui.children(page)[0];
        let column = ui.children(wrapper)[0];
        ContentColumn { page, column }
    }
}

impl<S: 'static> IntoWidget<S> for ContentColumnBuilder<S> {
    fn into_widget(self) -> Built<S> {
        let mut inner = column()
            .width_percent(1.0)
            .gap(CONTENT_GAP)
            .cross_align(CrossAlign::Stretch);
        if let Some(n) = self.name {
            inner = inner.name(n);
        }
        inner.built_mut().state_mut().style.padding = Edges {
            left: CONTENT_GUTTER,
            top: CONTENT_GAP,
            right: CONTENT_GUTTER,
            bottom: 2.0 * CONTENT_GAP,
        };
        if let Some(m) = self.max_width {
            inner = inner.max_width(m);
        }
        let mut inner = inner.into_widget();
        for c in self.children {
            inner.push(c);
        }
        let wrapper = column()
            .width_percent(1.0)
            .cross_align(CrossAlign::Center)
            .child(inner);
        scroll()
            .grow(1.0)
            .width_percent(1.0)
            .child(wrapper)
            .into_widget()
    }
}

/// The scrolling body of a content pane.
#[must_use]
pub fn content_column<S: 'static>() -> ContentColumnBuilder<S> {
    ContentColumnBuilder {
        children: Vec::new(),
        name: None,
        max_width: Some(CONTENT_MAX_WIDTH),
    }
}

// ---------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------

/// A stack of pages of which one is shown.
///
/// Every child is laid out at the full bounds and **kept** laid out, so
/// a switch is not a relayout: the pages that are not current are hidden
/// with [`Ui::set_node_visible`], which costs one `SetVisible` each way
/// and nothing else. Hidden pages stay in the tree, keep their names and
/// answer `hey get`, but the pointer does not enter them and Tab does not
/// reach into them. The widget measures to the largest page so the
/// window holds every one of them.
#[derive(Debug, Default)]
pub struct Pages {
    current: usize,
}

impl Pages {
    /// The index of the page on show.
    #[must_use]
    pub fn current(&self) -> usize {
        self.current
    }
}

impl<S: 'static> Widget<S> for Pages {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let mut size = Size::ZERO;
        for c in cx.children() {
            let s = cx.measure_child(c, constraints);
            size = Size::new(size.w.max(s.w), size.h.max(s.h));
        }
        constraints.constrain(size)
    }

    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        let inner = Rect::new(0.0, 0.0, bounds.w, bounds.h);
        for (i, c) in cx.children().into_iter().enumerate() {
            cx.place_child(c, inner);
            cx.ui.set_node_visible(c, i == self.current);
        }
    }

    fn role(&self) -> Role {
        Role::Container
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: Some(self.current.to_string()),
            actions: vec!["set_value"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "set_value" | "show" => {
                let Some(i) = arg.and_then(|a| a.parse::<usize>().ok()) else {
                    return Handled::No;
                };
                self.current = i;
                for (k, c) in cx.ui.children(cx.id).into_iter().enumerate() {
                    cx.ui.set_node_visible(c, k == i);
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Pages`].
impl<S: 'static> WidgetMut<'_, Pages, S> {
    /// Show page `index` and hide the rest. A no-op if it is already
    /// current.
    pub fn show(&mut self, index: usize) {
        if self.current == index {
            return;
        }
        self.current = index;
        let id = self.id();
        for (k, c) in self.ui().children(id).into_iter().enumerate() {
            self.ui().set_node_visible(c, k == index);
        }
        // A field on the page just hidden must not keep the keyboard: a
        // scripted switch moves no pointer, so nothing else would take
        // the focus off it and keystrokes would land in a widget nobody
        // can see. Dropped to nowhere rather than moved to a guess.
        if let Some(f) = self.ui().focused()
            && !self.ui().is_visible(f)
        {
            self.ui().unfocus();
        }
    }
}

/// Builder for a [`Pages`].
pub struct PagesBuilder<S> {
    built: Built<S>,
    pages: Pages,
}

impl<S: 'static> PagesBuilder<S> {
    /// Start on page `index` (default 0).
    #[must_use]
    pub fn current(mut self, index: usize) -> Self {
        self.pages.current = index;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for PagesBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for PagesBuilder<S> {}

impl<S: 'static> IntoWidget<S> for PagesBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(self.pages);
        self.built
    }
}

/// A page stack; its children are the pages.
#[must_use]
pub fn pages<S: 'static>() -> PagesBuilder<S> {
    let mut built: Built<S> = Built::new(Pages::default());
    {
        let st = built.state_mut();
        st.style.width = Length::Percent(1.0);
        st.style.flex_grow = 1.0;
        st.style.shrink_floor = ShrinkFloor::Zero;
    }
    PagesBuilder {
        built,
        pages: Pages::default(),
    }
}

// ---------------------------------------------------------------------
// Switch
// ---------------------------------------------------------------------

type ToggleFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, bool)>;

/// A toggle pill: accent when on, with a knob that slides.
///
/// The same *role* as a [`Checkbox`](crate::widgets::Checkbox) — it
/// answers `toggle`, `set_value`, reports `true`/`false` and toggles on
/// Space — so `hey set … value true` and every test written against a
/// checkbox keep working when a row grows up into a switch. The two
/// desktops both use a switch, not a square box, for a boolean row.
pub struct Switch<S> {
    label: String,
    checked: bool,
    enabled: bool,
    style: Option<TextStyle>,
    on_toggle: Option<ToggleFn<S>>,
    metrics: crate::wire::TextMetrics,
}

impl<S> std::fmt::Debug for Switch<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Switch")
            .field("label", &self.label)
            .field("checked", &self.checked)
            .finish_non_exhaustive()
    }
}

impl<S> Switch<S> {
    /// Whether the switch is on.
    #[must_use]
    pub fn is_checked(&self) -> bool {
        self.checked
    }

    /// The label beside the switch (may be empty).
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

impl<S: 'static> Switch<S> {
    fn toggle(&mut self, cx: &mut EventCx<'_, S>, to: bool) {
        if !self.enabled || self.checked == to {
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

impl<S: 'static> Widget<S> for Switch<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let (tw, th) = SWITCH_SIZE;
        let gap = cx.theme().gap;
        let style = self.resolved_style(cx.theme());
        self.metrics = cx
            .measure_text(&self.label, &style, 0.0)
            .unwrap_or_default();
        let w = if self.label.is_empty() {
            tw
        } else {
            tw + gap + self.metrics.width
        };
        constraints.constrain(Size::new(w, th.max(self.metrics.height)))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let (tw, th) = SWITCH_SIZE;
        let focused = cx.ui.is_focused(cx.id);
        let hovered = cx.ui.is_hovered(cx.id);
        let gap = cx.theme().gap;
        let style = self.resolved_style(cx.theme());
        let track = if !self.enabled {
            cx.color(ColorRole::ButtonDisabled)
        } else if self.checked && hovered {
            cx.color(ColorRole::AccentHover)
        } else if self.checked {
            cx.color(ColorRole::Accent)
        } else if hovered {
            cx.color(ColorRole::ButtonHover)
        } else {
            cx.color(ColorRole::Track)
        };
        let knob = if self.checked {
            cx.color(ColorRole::TextOnAccent)
        } else {
            cx.color(ColorRole::Surface)
        };
        let ring = cx.color(ColorRole::Hairline);
        let border = if focused {
            (2.0, cx.color(ColorRole::Focus))
        } else {
            (0.0, Color::TRANSPARENT)
        };
        let text_color = if self.enabled {
            cx.color(ColorRole::Text)
        } else {
            cx.color(ColorRole::TextDim)
        };
        let bounds = cx.bounds;
        let y = ((bounds.h - th) / 2.0).max(0.0);
        cx.rect(
            0,
            Rect::new(0.0, y, tw, th),
            Fill::Solid(track),
            th / 2.0,
            border,
        );
        let side = th - 4.0;
        let kx = if self.checked { tw - side - 2.0 } else { 2.0 };
        cx.rect(
            1,
            Rect::new(kx, y + 2.0, side, side),
            Fill::Solid(knob),
            side / 2.0,
            (1.0, ring),
        );
        if !self.label.is_empty() {
            let h = self.metrics.height.max(1.0);
            let ly = ((bounds.h - h) / 2.0).max(0.0);
            cx.text(
                2,
                Rect::new(tw + gap, ly, (bounds.w - tw - gap).max(0.0), h),
                &self.label.clone(),
                crate::widget::TextRun::new(&style, text_color),
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

/// Setters for a live [`Switch`].
impl<S: 'static> WidgetMut<'_, Switch<S>, S> {
    /// Turn the switch on or off. Does **not** run `on_toggle`: a setter
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

    /// Enable or disable the switch.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.request_paint();
    }
}

/// Builder for a [`Switch`].
pub struct SwitchBuilder<S> {
    built: Built<S>,
    switch: Switch<S>,
}

impl<S: 'static> SwitchBuilder<S> {
    /// Start on.
    #[must_use]
    pub fn checked(mut self, checked: bool) -> Self {
        self.switch.checked = checked;
        self
    }

    /// What to do when it is toggled.
    #[must_use]
    pub fn on_toggle(mut self, f: impl Fn(&mut S, &mut Ui<S>, bool) + 'static) -> Self {
        self.switch.on_toggle = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.switch.enabled = false;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for SwitchBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for SwitchBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.state_mut().focusable = true;
        self.built.replace_widget(self.switch);
        self.built
    }
}

/// A switch, with an optional label to its right (`""` for none).
#[must_use]
pub fn switch<S: 'static>(text: impl Into<String>) -> SwitchBuilder<S> {
    SwitchBuilder {
        built: Built::new(Flex),
        switch: Switch {
            label: text.into(),
            checked: false,
            enabled: true,
            style: None,
            on_toggle: None,
            metrics: crate::wire::TextMetrics::default(),
        },
    }
}

// ---------------------------------------------------------------------
// split_view
// ---------------------------------------------------------------------

/// The ids of a built [`SplitViewBuilder`]'s slots, for an app that
/// attaches rows later or changes the header title.
#[derive(Debug, Clone, Copy)]
pub struct SplitParts {
    /// The root row; what the app returns from its build closure.
    pub root: WidgetId,
    /// The sidebar's rows column: attach [`sidebar_row`]s here.
    pub sidebar: WidgetId,
    /// The content header's title label, if a header was asked for.
    pub title: Option<WidgetId>,
    /// The content body.
    pub content_body: WidgetId,
    /// The content footer, if one was given.
    pub content_footer: Option<WidgetId>,
}

/// Builder for a split view. Unlike the other builders it can also be
/// [`SplitViewBuilder::build`]-ed for a [`SplitParts`].
pub struct SplitViewBuilder<S> {
    sidebar_width: f32,
    sidebar_name: String,
    sidebar_header: Option<Part<S>>,
    sidebar_children: Vec<Part<S>>,
    header_title: Option<String>,
    header_leading: Vec<Part<S>>,
    header_trailing: Vec<Part<S>>,
    content: Option<Part<S>>,
    footer: Option<Part<S>>,
    name: String,
}

/// A slot's content: a subtree still to be built, or a widget the app
/// built earlier because a callback needed its id.
enum Part<S> {
    Built(Box<Built<S>>),
    Id(WidgetId),
}

impl<S: 'static> Part<S> {
    fn build(self, ui: &mut Ui<S>) -> WidgetId {
        match self {
            Self::Built(b) => ui.build(*b),
            Self::Id(id) => id,
        }
    }

    fn state_mut<'a>(&'a mut self, ui: &'a mut Ui<S>) -> StatePatch<'a, S> {
        match self {
            Self::Built(b) => StatePatch::Built(b.state_mut()),
            Self::Id(id) => StatePatch::Live(ui, *id),
        }
    }
}

/// A way to touch the framework state of either kind of [`Part`].
enum StatePatch<'a, S> {
    Built(&'a mut crate::WidgetState),
    Live(&'a mut Ui<S>, WidgetId),
}

impl<S: 'static> StatePatch<'_, S> {
    fn set(self, f: impl FnOnce(&mut Option<String>, &mut crate::LayoutStyle)) {
        match self {
            Self::Built(st) => f(&mut st.name, &mut st.style),
            Self::Live(ui, id) => {
                let mut name = ui.address_name(id);
                let mut style = ui.style(id);
                f(&mut name, &mut style);
                ui.set_style(id, style);
                if let Some(n) = name
                    && ui.address_name(id).is_none()
                {
                    ui.set_address_name(id, n);
                }
            }
        }
    }
}

impl<S: 'static> SplitViewBuilder<S> {
    /// The sidebar's width (default [`SIDEBAR_WIDTH`]).
    #[must_use]
    pub fn sidebar_width(mut self, px: f32) -> Self {
        self.sidebar_width = px.max(SIDEBAR_MIN_WIDTH);
        self
    }

    /// The addressing name of the sidebar's rows column (default
    /// `sidebar`).
    #[must_use]
    pub fn sidebar_name(mut self, n: impl Into<String>) -> Self {
        self.sidebar_name = n.into();
        self
    }

    /// The addressing name of the whole view (default `split`).
    #[must_use]
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = n.into();
        self
    }

    /// A bold title at the top of the sidebar.
    #[must_use]
    pub fn sidebar_header(mut self, title: impl Into<String>) -> Self {
        self.sidebar_header = Some(Part::Built(Box::new(
            row()
                .name(names::SIDEBAR_HEADER)
                .height(HEADER_HEIGHT)
                .width_percent(1.0)
                .padding_xy(SIDEBAR_ROW_PAD_X, 0.0)
                .cross_align(CrossAlign::Center)
                .child(
                    label(title)
                        .size(HEADER_TITLE_PX)
                        .weight(600)
                        .color_role(ColorRole::Text)
                        .elide(true),
                )
                .into_widget(),
        )));
        self
    }

    /// Any widget as the sidebar header (a search field, say).
    #[must_use]
    pub fn sidebar_header_widget(mut self, w: impl IntoWidget<S>) -> Self {
        self.sidebar_header = Some(Part::Built(Box::new(w.into_widget())));
        self
    }

    /// A row, section or separator in the sidebar.
    #[must_use]
    pub fn sidebar_child(mut self, w: impl IntoWidget<S>) -> Self {
        self.sidebar_children
            .push(Part::Built(Box::new(w.into_widget())));
        self
    }

    /// A sidebar child the app built earlier (its `on_click` captured
    /// an id, say).
    #[must_use]
    pub fn sidebar_child_id(mut self, id: WidgetId) -> Self {
        self.sidebar_children.push(Part::Id(id));
        self
    }

    /// Several sidebar children.
    #[must_use]
    pub fn sidebar_children<I>(mut self, ws: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoWidget<S>,
    {
        for w in ws {
            self.sidebar_children
                .push(Part::Built(Box::new(w.into_widget())));
        }
        self
    }

    /// A content header with this title (label named `title`).
    #[must_use]
    pub fn content_header(mut self, title: impl Into<String>) -> Self {
        self.header_title = Some(title.into());
        self
    }

    /// A widget before the title in the content header (a back button).
    #[must_use]
    pub fn content_header_leading(mut self, w: impl IntoWidget<S>) -> Self {
        self.header_leading
            .push(Part::Built(Box::new(w.into_widget())));
        self
    }

    /// As [`SplitViewBuilder::content_header_leading`], for a widget the
    /// app built earlier.
    #[must_use]
    pub fn content_header_leading_id(mut self, id: WidgetId) -> Self {
        self.header_leading.push(Part::Id(id));
        self
    }

    /// A widget after the title, at the trailing end (an action button).
    #[must_use]
    pub fn content_header_trailing(mut self, w: impl IntoWidget<S>) -> Self {
        self.header_trailing
            .push(Part::Built(Box::new(w.into_widget())));
        self
    }

    /// As [`SplitViewBuilder::content_header_trailing`], for a widget
    /// the app built earlier.
    #[must_use]
    pub fn content_header_trailing_id(mut self, id: WidgetId) -> Self {
        self.header_trailing.push(Part::Id(id));
        self
    }

    /// The content body — usually a [`content_column`], a [`pages`] of
    /// them, or a list. It grows to fill the pane; it is not wrapped in
    /// a scroll, since a content column scrolls itself and a list does
    /// too.
    #[must_use]
    pub fn content(mut self, w: impl IntoWidget<S>) -> Self {
        self.content = Some(Part::Built(Box::new(w.into_widget())));
        self
    }

    /// As [`SplitViewBuilder::content`], for a widget the app built
    /// earlier (a list whose callbacks captured ids, a [`pages`] stack
    /// the app attached pages to).
    #[must_use]
    pub fn content_id(mut self, id: WidgetId) -> Self {
        self.content = Some(Part::Id(id));
        self
    }

    /// A non-scrolling row under the body (Apply/Revert, a status line).
    #[must_use]
    pub fn content_footer(mut self, w: impl IntoWidget<S>) -> Self {
        self.footer = Some(Part::Built(Box::new(w.into_widget())));
        self
    }

    /// As [`SplitViewBuilder::content_footer`], for a widget the app
    /// built earlier.
    #[must_use]
    pub fn content_footer_id(mut self, id: WidgetId) -> Self {
        self.footer = Some(Part::Id(id));
        self
    }

    /// Materialise the view and hand back the ids of its slots.
    ///
    /// # Panics
    /// Never in practice: every attach is of an id built a line earlier.
    pub fn build(mut self, ui: &mut Ui<S>) -> SplitParts {
        let (side, rows) = self.build_sidebar(ui);
        let (content, title, body, footer) = self.build_content(ui);
        let root = ui.build(
            row()
                .name(self.name.clone())
                .width_percent(1.0)
                .height_percent(1.0)
                .cross_align(CrossAlign::Stretch),
        );
        ui.attach(root, side).expect("fresh ids");
        let line = ui.build(
            separator()
                .vertical()
                .color_role(ColorRole::Hairline)
                .height_percent(1.0),
        );
        ui.attach(root, line).expect("fresh ids");
        ui.attach(root, content).expect("fresh ids");
        SplitParts {
            root,
            sidebar: rows,
            title,
            content_body: body,
            content_footer: footer,
        }
    }

    /// The sidebar pane: `(pane, rows column)`.
    fn build_sidebar(&mut self, ui: &mut Ui<S>) -> (WidgetId, WidgetId) {
        let rows = ui.build(
            column()
                .name(self.sidebar_name.clone())
                .width_percent(1.0)
                .padding(SIDEBAR_ROW_INSET)
                .gap(2.0)
                .cross_align(CrossAlign::Stretch),
        );
        for c in self.sidebar_children.drain(..) {
            let c = c.build(ui);
            ui.attach(rows, c).expect("fresh ids");
        }
        let body = ui.build(scroll().grow(1.0).width_percent(1.0));
        ui.attach(body, rows).expect("fresh ids");
        let side = ui.build(
            panel()
                .background_role(ColorRole::SidebarBackground)
                .radius(0.0)
                .border(0.0, Color::TRANSPARENT)
                .padding(0.0)
                .width(self.sidebar_width)
                .min_width(SIDEBAR_MIN_WIDTH)
                .height_percent(1.0)
                .cross_align(CrossAlign::Stretch),
        );
        if let Some(h) = self.sidebar_header.take() {
            let h = h.build(ui);
            ui.attach(side, h).expect("fresh ids");
        }
        ui.attach(side, body).expect("fresh ids");
        (side, rows)
    }

    /// The content pane: `(pane, title, body, footer)`.
    fn build_content(
        &mut self,
        ui: &mut Ui<S>,
    ) -> (WidgetId, Option<WidgetId>, WidgetId, Option<WidgetId>) {
        let content = ui.build(
            column()
                .name(names::CONTENT)
                .grow(1.0)
                .shrink_to_zero()
                .height_percent(1.0)
                .cross_align(CrossAlign::Stretch),
        );
        let mut title = None;
        if self.header_title.is_some() || !self.header_leading.is_empty() {
            let header = self.build_header(ui, &mut title);
            ui.attach(content, header).expect("fresh ids");
        }
        let body = match self.content.take() {
            Some(mut b) => {
                b.state_mut(ui).set(|name, style| {
                    if name.is_none() {
                        *name = Some(names::CONTENT_BODY.to_owned());
                    }
                    style.flex_grow = 1.0;
                    style.shrink_floor = ShrinkFloor::Zero;
                    if style.width == Length::Auto {
                        style.width = Length::Percent(1.0);
                    }
                });
                b.build(ui)
            }
            None => ui.build(
                column()
                    .name(names::CONTENT_BODY)
                    .grow(1.0)
                    .width_percent(1.0),
            ),
        };
        ui.attach(content, body).expect("fresh ids");
        let mut footer = None;
        if let Some(mut f) = self.footer.take() {
            f.state_mut(ui).set(|name, _| {
                if name.is_none() {
                    *name = Some(names::CONTENT_FOOTER.to_owned());
                }
            });
            let f = f.build(ui);
            ui.attach(content, f).expect("fresh ids");
            footer = Some(f);
        }
        (content, title, body, footer)
    }

    /// The content header row; fills `title` with the label's id.
    fn build_header(&mut self, ui: &mut Ui<S>, title: &mut Option<WidgetId>) -> WidgetId {
        let header = ui.build(
            row()
                .name(names::CONTENT_HEADER)
                .height(HEADER_HEIGHT)
                .width_percent(1.0)
                .padding_xy(CONTENT_GUTTER, 0.0)
                .gap(8.0)
                .cross_align(CrossAlign::Center),
        );
        for w in self.header_leading.drain(..) {
            let w = w.build(ui);
            ui.attach(header, w).expect("fresh ids");
        }
        if let Some(t) = self.header_title.take() {
            let t = ui.build(
                label(t)
                    .name(names::TITLE)
                    .size(HEADER_TITLE_PX)
                    .weight(600)
                    .color_role(ColorRole::Text)
                    .elide(true)
                    .grow(1.0),
            );
            ui.attach(header, t).expect("fresh ids");
            *title = Some(t);
        } else {
            let s = ui.build(spacer().grow(1.0));
            ui.attach(header, s).expect("fresh ids");
        }
        for w in self.header_trailing.drain(..) {
            let w = w.build(ui);
            ui.attach(header, w).expect("fresh ids");
        }
        header
    }
}

/// An empty split view.
#[must_use]
pub fn split_view<S: 'static>() -> SplitViewBuilder<S> {
    SplitViewBuilder {
        sidebar_width: SIDEBAR_WIDTH,
        sidebar_name: names::SIDEBAR.to_owned(),
        sidebar_header: None,
        sidebar_children: Vec::new(),
        header_title: None,
        header_leading: Vec::new(),
        header_trailing: Vec::new(),
        content: None,
        footer: None,
        name: names::SPLIT.to_owned(),
    }
}

/// The intrinsic size of a container, over its children — the same
/// arithmetic `Flex` uses.
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
