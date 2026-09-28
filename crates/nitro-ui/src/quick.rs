//! The **quick-settings** widgets: what a status menu is built from.
//!
//! GNOME's structure with a few macOS touches (`docs/ui.md`, "Quick
//! settings widgets"): round icon buttons for the top row, a rounded
//! *section card* with a small heading for a group such as Sound, a
//! uniform grid of *tiles* whose on-state lives in a round accent
//! **badge** rather than in the whole tile, and a *drill-down header*
//! with a back arrow for detail views (a device list, a power menu)
//! whose rows are *choice rows*. The chunky volume slider is
//! [`SliderBuilder::chunky`](crate::widgets::SliderBuilder::chunky).
//!
//! ```text
//! ╭──────────────────────────────────────╮
//! │ 87%                      (⚙) (🔒) (⏻) │  round_button
//! │ ╭──────────────────────────────────╮ │
//! │ │ Sound                            │ │  section_card
//! │ │ (🔊) ━━━━━━━━━━━●──────────  (🎧) │ │  chunky slider
//! │ ╰──────────────────────────────────╯ │
//! │ ╭────────────────╮ ╭───────────────╮ │
//! │ │ (◐) Dark Style │ │ (📶) Wi-Fi  › │ │  tile
//! │ │     On         │ │     Home      │ │
//! │ ╰────────────────╯ ╰───────────────╯ │
//! ╰──────────────────────────────────────╯
//! ```
//!
//! Every colour is a palette role, so the whole menu follows a
//! `theme.scheme` flip; every size is one of the constants below, so a
//! menu built from these is consistent without trying.

use nitro_core::{Color, Rect, Size};
use nitro_wire::msg::Fill;

use crate::ColorRole;
use crate::build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled, button, key};
use crate::layout::{Constraints, CrossAlign, Direction, Edges, Length};
use crate::split::Pressable;
use crate::ui::{Ui, WidgetMut};
use crate::widget::{Access, EventCx, MeasureCx, PaintCx, Role, Widget};
use crate::widgets::{FlexBuilder, PanelBuilder, column, label, panel, row};

// ---------------------------------------------------------------------
// The measurements, in one place
// ---------------------------------------------------------------------

/// Corner radius of the menu panel itself.
pub const QS_RADIUS: f32 = 24.0;
/// Padding inside the menu panel.
pub const QS_PAD: f32 = 16.0;
/// Gap between the menu's sections, and between tiles.
pub const QS_GAP: f32 = 10.0;
/// The menu's fixed width: drill-down views keep it, so switching view
/// reads as a height change and nothing jumps sideways.
pub const QS_WIDTH: f32 = 360.0;
/// A tile's height.
pub const TILE_H: f32 = 56.0;
/// A tile's corner radius.
pub const TILE_RADIUS: f32 = 14.0;
/// Diameter of a tile's round badge.
pub const BADGE: f32 = 32.0;
/// Diameter of a [`round_button`].
pub const ROUND_BTN: f32 = 36.0;
/// Height of a chunky slider — track and knob alike.
pub const CHUNKY_H: f32 = 24.0;
/// Corner radius of a [`section_card`].
pub const SECTION_RADIUS: f32 = 16.0;
/// Side of the symbolic icons on buttons, badges and rows.
pub const QS_ICON: f32 = 16.0;
/// A [`choice_row`]'s height.
pub const CHOICE_H: f32 = 36.0;

/// Inset of a tile's badge from its left edge.
const BADGE_INSET: f32 = 12.0;

type ToggleFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, bool)>;

// ---------------------------------------------------------------------
// RoundButton
// ---------------------------------------------------------------------

/// A circular icon button: the top row's settings / lock / power, the
/// Sound card's mute and output buttons, a drill-down's back arrow.
///
/// Its **label** is not drawn — the icon is the face — but it is what
/// `hey` and an accessibility client read, exactly as an icon-only
/// [`Button`](crate::widgets::Button) keeps its text.
pub struct RoundButton<S> {
    icon: String,
    label: String,
    accent: bool,
    enabled: bool,
    diameter: f32,
    press: Pressable<S>,
}

impl<S> std::fmt::Debug for RoundButton<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoundButton")
            .field("icon", &self.icon)
            .field("label", &self.label)
            .field("accent", &self.accent)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl<S> RoundButton<S> {
    /// The icon's name.
    #[must_use]
    pub fn icon(&self) -> &str {
        &self.icon
    }

    /// The accessible label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the face is accent-filled.
    #[must_use]
    pub fn is_accent(&self) -> bool {
        self.accent
    }

    /// Whether it reacts to input.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl<S: 'static> Widget<S> for RoundButton<S> {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        constraints.constrain(Size::new(self.diameter, self.diameter))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        let pressed = self.press.pressed;
        let (face, ink) = if !self.enabled {
            (ColorRole::ButtonDisabled, ColorRole::TextDim)
        } else if self.accent {
            let face = if pressed {
                ColorRole::AccentActive
            } else if hovered {
                ColorRole::AccentHover
            } else {
                ColorRole::Accent
            };
            (face, ColorRole::TextOnAccent)
        } else {
            let face = if pressed {
                ColorRole::ButtonActive
            } else if hovered {
                ColorRole::ButtonHover
            } else {
                ColorRole::Button
            };
            (face, ColorRole::ButtonText)
        };
        let border = if focused && self.enabled {
            (2.0, cx.color(ColorRole::Focus))
        } else {
            (0.0, Color::TRANSPARENT)
        };
        let b = cx.bounds;
        let d = self.diameter.min(b.w).min(b.h);
        let circle = Rect::new((b.w - d) / 2.0, (b.h - d) / 2.0, d, d);
        let face = cx.color(face);
        cx.rect(0, circle, Fill::Solid(face), d / 2.0, border);
        let icon = self.icon.clone();
        cx.icon(1, circle, &icon, QS_ICON, ink);
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        self.press.event(cx, ev).unwrap_or(Handled::No)
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.label.clone()),
            value: Some(self.label.clone()),
            actions: if self.enabled {
                vec!["click", "activate", "focus"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, _arg: Option<&str>) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        self.press.action(cx, action)
    }
}

/// Setters for a live [`RoundButton`]; each is a no-op when unchanged.
impl<S: 'static> WidgetMut<'_, RoundButton<S>, S> {
    /// Change the icon.
    pub fn set_icon(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.icon != name {
            self.icon = name;
            self.request_paint();
        }
    }

    /// Fill the face with the accent (an "on" state) or not.
    pub fn set_accent(&mut self, on: bool) {
        if self.accent != on {
            self.accent = on;
            self.request_paint();
        }
    }

    /// Enable or disable it.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            if !on {
                self.press.pressed = false;
            }
            self.set_focusable(on);
            self.request_paint();
        }
    }

    /// Change the accessible label.
    pub fn set_label(&mut self, text: impl Into<String>) {
        self.label = text.into();
    }
}

/// Builder for a [`RoundButton`].
pub struct RoundButtonBuilder<S> {
    built: Built<S>,
    button: RoundButton<S>,
}

impl<S: 'static> RoundButtonBuilder<S> {
    /// The accessible label (default: the icon's name).
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> Self {
        self.button.label = text.into();
        self
    }

    /// What a click does.
    #[must_use]
    pub fn on_click(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.button.press.on_click = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.button.enabled = false;
        self
    }

    /// Start with an accent face (or not).
    #[must_use]
    pub fn accent(mut self, on: bool) -> Self {
        self.button.accent = on;
        self
    }

    /// A diameter other than [`ROUND_BTN`].
    #[must_use]
    pub fn diameter(mut self, px: f32) -> Self {
        self.button.diameter = px;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for RoundButtonBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for RoundButtonBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.state_mut().focusable = self.button.enabled;
        self.built.replace_widget(self.button);
        self.built
    }
}

/// A round icon button showing `icon`.
#[must_use]
pub fn round_button<S: 'static>(icon: impl Into<String>) -> RoundButtonBuilder<S> {
    let icon = icon.into();
    RoundButtonBuilder {
        built: Built::new(crate::widgets::Flex),
        button: RoundButton {
            label: icon.clone(),
            icon,
            accent: false,
            enabled: true,
            diameter: ROUND_BTN,
            press: Pressable::new(),
        },
    }
}

// ---------------------------------------------------------------------
// Tile
// ---------------------------------------------------------------------

/// A quick-settings tile: a neutral raised surface with a **round badge**
/// on the left that fills with the accent when the tile is on, a bold
/// title and a dim subtitle beside it, and optionally a chevron that
/// opens a detail view.
///
/// The tile itself stays neutral when on — the macOS touch — so a grid
/// of six tiles with three on reads as three bright dots, not as a
/// checkerboard. A container: the title and subtitle are [`Label`]
/// (`crate::widgets::Label`) children named `title` and `subtitle`, and
/// the chevron (if any) is a [`RoundButton`] named `open`.
pub struct Tile<S> {
    title: String,
    icon: String,
    on: bool,
    enabled: bool,
    pressed: bool,
    on_toggle: Option<ToggleFn<S>>,
}

impl<S> std::fmt::Debug for Tile<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tile")
            .field("title", &self.title)
            .field("on", &self.on)
            .finish_non_exhaustive()
    }
}

impl<S> Tile<S> {
    /// Whether the tile is on.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on
    }

    /// The title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The badge's icon.
    #[must_use]
    pub fn icon(&self) -> &str {
        &self.icon
    }

    /// The badge's rectangle inside a tile of `size`.
    #[must_use]
    pub fn badge_rect(size: Size) -> Rect {
        Rect::new(BADGE_INSET, ((size.h - BADGE) / 2.0).max(0.0), BADGE, BADGE)
    }
}

impl<S: 'static> Tile<S> {
    fn toggle(&mut self, cx: &mut EventCx<'_, S>, to: bool) {
        if !self.enabled || self.on == to {
            return;
        }
        self.on = to;
        cx.report_activation();
        cx.request_paint();
        let Some(cb) = self.on_toggle.take() else {
            return;
        };
        cb(cx.state, cx.ui, to);
        self.on_toggle = Some(cb);
    }
}

impl<S: 'static> Widget<S> for Tile<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        crate::widgets::measure_container(cx, constraints)
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        let face = if hovered || self.pressed {
            ColorRole::ButtonHover
        } else {
            ColorRole::Surface
        };
        let border = if focused {
            (2.0, cx.color(ColorRole::Focus))
        } else {
            (1.0, cx.color(ColorRole::Hairline))
        };
        let b = cx.bounds;
        let face = cx.color(face);
        cx.rect(
            0,
            Rect::new(0.0, 0.0, b.w, b.h),
            Fill::Solid(face),
            TILE_RADIUS,
            border,
        );
        let (badge, ink) = if !self.enabled {
            (ColorRole::ButtonDisabled, ColorRole::TextDim)
        } else if self.on {
            (ColorRole::Accent, ColorRole::TextOnAccent)
        } else {
            (ColorRole::Button, ColorRole::Text)
        };
        let r = Tile::<S>::badge_rect(b.size());
        let badge = cx.color(badge);
        cx.rect(
            1,
            r,
            Fill::Solid(badge),
            BADGE / 2.0,
            (0.0, Color::TRANSPARENT),
        );
        let icon = self.icon.clone();
        cx.icon(2, r, &icon, QS_ICON, ink);
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
                    let to = !self.on;
                    self.toggle(cx, to);
                }
                Handled::Yes
            }
            Event::KeyDown(k) if k.keycode == key::SPACE || k.keycode == key::ENTER => {
                let to = !self.on;
                self.toggle(cx, to);
                Handled::Yes
            }
            Event::PointerLeave => {
                if !cx.is_captured() {
                    self.pressed = false;
                }
                cx.request_paint();
                Handled::No
            }
            Event::PointerEnter { .. } | Event::FocusChanged { .. } => {
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
            name: Some(self.title.clone()),
            value: Some(self.on.to_string()),
            actions: if self.enabled {
                vec!["toggle", "click", "set_value", "focus"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            "toggle" | "click" | "activate" => {
                let to = !self.on;
                self.toggle(cx, to);
                Handled::Yes
            }
            "set_value" | "set_checked" => {
                let to = matches!(arg, Some("true" | "1" | "on" | "yes"));
                self.toggle(cx, to);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`Tile`].
impl<S: 'static> WidgetMut<'_, Tile<S>, S> {
    /// Turn it on or off **without** running `on_toggle`. Paint only; a
    /// no-op when unchanged.
    pub fn set_on(&mut self, on: bool) {
        if self.on != on {
            self.on = on;
            self.request_paint();
        }
    }

    /// Change the badge's icon; a no-op when unchanged.
    pub fn set_icon(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.icon != name {
            self.icon = name;
            self.request_paint();
        }
    }

    /// Change the dim second line; a no-op when unchanged.
    pub fn set_subtitle(&mut self, text: impl Into<String>) {
        let id = self.id();
        let text = text.into();
        let ui = self.ui();
        let Some(col) = ui.children(id).first().copied() else {
            return;
        };
        if let Some(sub) = ui.children(col).get(1).copied()
            && let Ok(mut l) = ui.widget_mut::<crate::widgets::Label>(sub)
        {
            l.set_text(text);
        }
    }
}

/// Builder for a [`Tile`].
pub struct TileBuilder<S> {
    built: Built<S>,
    tile: Tile<S>,
    subtitle: String,
    chevron: Option<crate::split::ClickFn<S>>,
}

impl<S: 'static> TileBuilder<S> {
    /// Start on (or off).
    #[must_use]
    pub fn on(mut self, on: bool) -> Self {
        self.tile.on = on;
        self
    }

    /// The dim second line ("On", a network's name).
    #[must_use]
    pub fn subtitle(mut self, text: impl Into<String>) -> Self {
        self.subtitle = text.into();
        self
    }

    /// What a toggle does; handed the new state.
    #[must_use]
    pub fn on_toggle(mut self, f: impl Fn(&mut S, &mut Ui<S>, bool) + 'static) -> Self {
        self.tile.on_toggle = Some(Box::new(f));
        self
    }

    /// Add a split chevron segment on the right that opens a detail view
    /// instead of toggling.
    #[must_use]
    pub fn chevron(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.chevron = Some(Box::new(f));
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.tile.enabled = false;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for TileBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for TileBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let text = column()
            .gap(1.0)
            .grow(1.0)
            .shrink_to_zero()
            .child(
                label(self.tile.title.clone())
                    .name("title")
                    .size(14.0)
                    .weight(600)
                    .color_role(ColorRole::Text)
                    .elide(true),
            )
            .child(
                label(std::mem::take(&mut self.subtitle))
                    .name("subtitle")
                    .size(12.0)
                    .color_role(ColorRole::TextDim)
                    .elide(true),
            );
        self.built.push(text.into_widget());
        if let Some(f) = self.chevron.take() {
            let mut open = round_button("chevron-right")
                .name("open")
                .label("Open")
                .diameter(28.0);
            open.button.press.on_click = Some(f);
            self.built.push(open.into_widget());
        }
        self.built.state_mut().focusable = self.tile.enabled;
        self.built.replace_widget(self.tile);
        self.built
    }
}

/// A tile titled `title` with `icon` in its badge. Fills the width it is
/// given; put two in a [`row`] with `.grow(1.0)` each for the grid.
#[must_use]
pub fn tile<S: 'static>(title: impl Into<String>, icon: impl Into<String>) -> TileBuilder<S> {
    let mut built: Built<S> = Built::new(crate::widgets::Flex);
    {
        let st = built.state_mut();
        st.style.direction = Direction::Row;
        st.style.cross_align = CrossAlign::Center;
        st.style.gap = 8.0;
        st.style.height = Length::Px(TILE_H);
        st.style.padding = Edges {
            left: BADGE_INSET + BADGE + 10.0,
            right: 10.0,
            top: 0.0,
            bottom: 0.0,
        };
    }
    TileBuilder {
        built,
        tile: Tile {
            title: title.into(),
            icon: icon.into(),
            on: false,
            enabled: true,
            pressed: false,
            on_toggle: None,
        },
        subtitle: String::new(),
        chevron: None,
    }
}

// ---------------------------------------------------------------------
// Section card, drill-down header
// ---------------------------------------------------------------------

/// A rounded card with a small dim heading: the macOS Sound card. A
/// container — add the section's controls as children after the heading
/// (which is a label named `heading`).
#[must_use]
pub fn section_card<S: 'static>(heading: impl Into<String>) -> PanelBuilder<S> {
    panel()
        .background_role(ColorRole::Surface)
        .border_role(1.0, ColorRole::Hairline)
        .radius(SECTION_RADIUS)
        .padding(12.0)
        .gap(8.0)
        .width_percent(1.0)
        .cross_align(CrossAlign::Stretch)
        .child(
            label(heading)
                .name("heading")
                .size(11.0)
                .weight(600)
                .color_role(ColorRole::TextDim),
        )
}

/// The header of a drill-down view: a round back button (named `back`)
/// and a bold title (named `title`).
#[must_use]
pub fn drill_header<S: 'static>(
    title: impl Into<String>,
    on_back: impl Fn(&mut S, &mut Ui<S>) + 'static,
) -> FlexBuilder<S> {
    row()
        .gap(10.0)
        .width_percent(1.0)
        .cross_align(CrossAlign::Center)
        .child(
            round_button("arrow-left")
                .name("back")
                .label("Back")
                .diameter(32.0)
                .on_click(on_back),
        )
        .child(
            label(title)
                .name("title")
                .size(15.0)
                .weight(600)
                .color_role(ColorRole::Text)
                .elide(true)
                .grow(1.0)
                .shrink_to_zero(),
        )
}

// ---------------------------------------------------------------------
// ChoiceRow
// ---------------------------------------------------------------------

/// One row of a drill-down list: a check in the accent colour when it is
/// the current choice (blank space of the same width otherwise, so the
/// labels line up), an optional leading icon, and a label child named
/// `label`. Hover-tinted; a click runs `on_click`.
pub struct ChoiceRow<S> {
    label: String,
    selected: bool,
    icon: Option<String>,
    press: Pressable<S>,
}

impl<S> std::fmt::Debug for ChoiceRow<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChoiceRow")
            .field("label", &self.label)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl<S> ChoiceRow<S> {
    /// Whether it is the checked choice.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// The label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// Left padding of a choice row's check column.
const CHOICE_PAD: f32 = 10.0;

impl<S: 'static> Widget<S> for ChoiceRow<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        crate::widgets::measure_container(cx, constraints)
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        let face = if hovered || self.press.pressed {
            cx.color(ColorRole::ButtonHover)
        } else {
            Color::TRANSPARENT
        };
        let border = if focused {
            (2.0, cx.color(ColorRole::Focus))
        } else {
            (0.0, Color::TRANSPARENT)
        };
        let b = cx.bounds;
        cx.rect(
            0,
            Rect::new(0.0, 0.0, b.w, b.h),
            Fill::Solid(face),
            10.0,
            border,
        );
        let check = Rect::new(CHOICE_PAD, 0.0, QS_ICON, b.h);
        if self.selected {
            cx.icon(1, check, "check", QS_ICON, ColorRole::Accent);
        } else {
            cx.icon(1, check, "", QS_ICON, ColorRole::Accent);
        }
        if let Some(icon) = self.icon.clone() {
            let r = Rect::new(CHOICE_PAD + QS_ICON + 8.0, 0.0, QS_ICON, b.h);
            cx.icon(2, r, &icon, QS_ICON, ColorRole::Text);
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        self.press.event(cx, ev).unwrap_or(Handled::No)
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.label.clone()),
            value: Some(self.selected.to_string()),
            actions: vec!["click", "activate", "focus"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, _arg: Option<&str>) -> Handled {
        self.press.action(cx, action)
    }
}

/// Setters for a live [`ChoiceRow`].
impl<S: 'static> WidgetMut<'_, ChoiceRow<S>, S> {
    /// Check or uncheck it; a no-op when unchanged.
    pub fn set_selected(&mut self, on: bool) {
        if self.selected != on {
            self.selected = on;
            self.request_paint();
        }
    }
}

/// Builder for a [`ChoiceRow`].
pub struct ChoiceRowBuilder<S> {
    built: Built<S>,
    row: ChoiceRow<S>,
}

impl<S: 'static> ChoiceRowBuilder<S> {
    /// What a click does.
    #[must_use]
    pub fn on_click(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.row.press.on_click = Some(Box::new(f));
        self
    }

    /// A leading icon after the check column.
    #[must_use]
    pub fn icon(mut self, name: impl Into<String>) -> Self {
        self.row.icon = Some(name.into());
        self
    }
}

impl<S: 'static> StyleBuilder<S> for ChoiceRowBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for ChoiceRowBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let mut left = CHOICE_PAD + QS_ICON + 10.0;
        if self.row.icon.is_some() {
            left += QS_ICON + 8.0;
        }
        self.built.state_mut().style.padding = Edges {
            left,
            right: 10.0,
            top: 0.0,
            bottom: 0.0,
        };
        self.built.push(
            label(self.row.label.clone())
                .name("label")
                .size(14.0)
                .color_role(ColorRole::Text)
                .elide(true)
                .grow(1.0)
                .shrink_to_zero()
                .into_widget(),
        );
        self.built.state_mut().focusable = true;
        self.built.replace_widget(self.row);
        self.built
    }
}

/// A list row labelled `label`, checked when `selected`.
#[must_use]
pub fn choice_row<S: 'static>(label: impl Into<String>, selected: bool) -> ChoiceRowBuilder<S> {
    let mut built: Built<S> = Built::new(crate::widgets::Flex);
    {
        let st = built.state_mut();
        st.style.direction = Direction::Row;
        st.style.cross_align = CrossAlign::Center;
        st.style.height = Length::Px(CHOICE_H);
        st.style.width = Length::Percent(1.0);
    }
    ChoiceRowBuilder {
        built,
        row: ChoiceRow {
            label: label.into(),
            selected,
            icon: None,
            press: Pressable::new(),
        },
    }
}
