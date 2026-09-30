//! Menus: an icon button that opens a list of actions in a popup.
//!
//! [`menu_button`] is a round icon button (the face of a
//! [`RoundButton`](crate::RoundButton)) that owns a list of
//! [`MenuEntry`]s. Activating it opens a grabbing popup
//! ([`Ui::add_popup`]) under it holding one row per item; picking a row
//! closes the popup and hands the item's **id** to the app's
//! [`on_select`](MenuButtonBuilder::on_select):
//!
//! ```no_run
//! use nitro_ui::{MenuItem, Ui, menu_button};
//!
//! struct St { session: String }
//!
//! fn build(ui: &mut Ui<St>) -> nitro_ui::WidgetId {
//!     ui.build(
//!         menu_button("gear")
//!             .label("Session")
//!             .item(MenuItem::new("nitro", "Nitro").radio(true))
//!             .item(MenuItem::new("shell", "Text console").radio(false))
//!             .separator()
//!             .item(MenuItem::new("reboot", "Restart…").icon("bootstrap-reboot"))
//!             .item(MenuItem::new("off", "Power off").icon("power").disabled())
//!             .on_select(|s: &mut St, ui: &mut Ui<St>, id: &str| {
//!                 s.session = id.to_owned();
//!             }),
//!     )
//! }
//! ```
//!
//! # The contract
//!
//! - **Opening**: a left click, Enter, Space or Down (Down, Enter and
//!   Space highlight the checked item, else the first enabled one; Up
//!   opens with the last highlighted).
//! - **Navigating**: Up/Down move and wrap, skipping separators and
//!   disabled items; Home/End jump; Enter/Space activate. The keys work
//!   whichever window the server addresses them to: the parent (the
//!   focused button forwards them) or the popup (a keyboard grab, which
//!   the button takes for a `NO_FOCUS` shell parent such as a bar).
//!   The pointer highlights on hover and activates on click; disabled
//!   rows ignore both.
//! - **Closing**: activation closes the popup, then runs `on_select` —
//!   both in one deferred step, so they land in one commit. An outside
//!   press or Escape is the **server's** (a grabbing popup): it dismisses
//!   the popup, the event is consumed, and `PopupDone` runs the same
//!   close path. Every close returns the keyboard focus to the button
//!   (in a window where a click takes focus).
//! - **Placement**: [`PopupPlacement::below_left`] (or
//!   [`below`](PopupPlacement::below) with
//!   [`align_right`](MenuButtonBuilder::align_right)); its constraint
//!   adjustment slides the menu sideways and **flips it above** the
//!   button when there is no room below, so a button in a screen corner
//!   of a fullscreen surface (a greeter) still gets a visible menu.
//!
//! Each row is named by its item's id, so `hey` and tests reach it as
//! `window[N]/<id>`; ids must therefore be **unique** within a menu
//! (checked with a `debug_assert!`). A row's accessible value is its
//! checked state.
//!
//! The button's [`label`](MenuButtonBuilder::label) is its accessible
//! name. A hover tooltip showing it (a non-grabbing popup after a
//! delay) is future work: the toolkit has no tooltip facility yet.

use std::rc::Rc;

use nitro_core::{Color, Rect, Size};
use nitro_wire::msg::Fill;

use crate::ColorRole;
use crate::build::{Built, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled, button, key};
use crate::layout::Constraints;
use crate::popup::PopupPlacement;
use crate::quick::{QS_ICON, ROUND_BTN};
use crate::theme::TextStyle;
use crate::ui::{Ui, WidgetMut, WindowId};
use crate::widget::{Access, EventCx, LayoutCx, MeasureCx, PaintCx, Role, TextRun, Widget};
use crate::wire::TextMetrics;
use crate::WidgetId;

// ---------------------------------------------------------------------
// The measurements, in one place
// ---------------------------------------------------------------------

/// Height of one item row.
pub const MENU_ROW_H: f32 = 32.0;
/// Height of a separator (a hairline centred in it).
pub const MENU_SEP_H: f32 = 9.0;
/// Padding between the panel's edge and its rows.
pub const MENU_PAD: f32 = 6.0;
/// The narrowest a menu is.
pub const MENU_MIN_W: f32 = 160.0;
/// The widest a menu is; longer labels are clipped.
pub const MENU_MAX_W: f32 = 320.0;
/// The panel's corner radius.
pub const MENU_RADIUS: f32 = 12.0;
/// Horizontal padding inside a row.
const ROW_PAD: f32 = 10.0;
/// Gap between a row's columns.
const ROW_GAP: f32 = 8.0;
/// Label size.
const LABEL_PX: f32 = 14.0;
/// The radio mark's dot.
const RADIO_DOT: f32 = 8.0;

// ---------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------

/// What an item's mark column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    /// Nothing: an action.
    #[default]
    None,
    /// A check mark when checked: an independent toggle.
    Check,
    /// A dot when checked: one of several, "the current choice".
    Radio,
}

/// One actionable item of a menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    id: String,
    label: String,
    icon: Option<String>,
    mark: Mark,
    checked: bool,
    enabled: bool,
}

impl MenuItem {
    /// An enabled, unmarked item: `id` is what
    /// [`on_select`](MenuButtonBuilder::on_select) receives and the row's
    /// name; `label` is what it shows.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            mark: Mark::None,
            checked: false,
            enabled: true,
        }
    }

    /// A leading icon (a `nitro-icons` name).
    #[must_use]
    pub fn icon(mut self, name: impl Into<String>) -> Self {
        self.icon = Some(name.into());
        self
    }

    /// Make it a check item, checked or not.
    #[must_use]
    pub fn checked(mut self, on: bool) -> Self {
        self.mark = Mark::Check;
        self.checked = on;
        self
    }

    /// Make it a radio item, the current choice or not.
    #[must_use]
    pub fn radio(mut self, on: bool) -> Self {
        self.mark = Mark::Radio;
        self.checked = on;
        self
    }

    /// Greyed out and not activatable.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// The id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The icon's name, if any.
    #[must_use]
    pub fn icon_name(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    /// The mark kind.
    #[must_use]
    pub fn mark(&self) -> Mark {
        self.mark
    }

    /// Whether the mark is on.
    #[must_use]
    pub fn is_checked(&self) -> bool {
        self.checked
    }

    /// Whether it can be activated.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// One line of a menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuEntry {
    /// An item.
    Item(MenuItem),
    /// A dividing line.
    Separator,
}

impl From<MenuItem> for MenuEntry {
    fn from(item: MenuItem) -> Self {
        Self::Item(item)
    }
}

impl MenuEntry {
    fn item(&self) -> Option<&MenuItem> {
        match self {
            Self::Item(i) => Some(i),
            Self::Separator => None,
        }
    }

    /// Whether keyboard and pointer can land on it.
    fn selectable(&self) -> bool {
        self.item().is_some_and(|i| i.enabled)
    }
}

fn debug_check_unique(entries: &[MenuEntry]) {
    if cfg!(debug_assertions) {
        let mut seen = std::collections::HashSet::new();
        for i in entries.iter().filter_map(MenuEntry::item) {
            debug_assert!(
                seen.insert(i.id.as_str()),
                "menu item id {:?} is used twice: ids name the rows and must be unique",
                i.id
            );
        }
    }
}

// ---------------------------------------------------------------------
// Pure pieces: navigation and layout
// ---------------------------------------------------------------------

/// The keyboard state of an open menu: which entries can be landed on,
/// and which one is highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MenuNav {
    selectable: Vec<bool>,
    highlight: Option<usize>,
}

impl MenuNav {
    fn new(selectable: Vec<bool>) -> Self {
        Self {
            selectable,
            highlight: None,
        }
    }

    fn step(&self, from: Option<usize>, forward: bool) -> Option<usize> {
        let n = self.selectable.len();
        if n == 0 {
            return None;
        }
        // Before the first (forward) or after the last (backward) when
        // nothing is highlighted yet.
        let mut i = from.unwrap_or(if forward { n - 1 } else { 0 });
        for _ in 0..n {
            i = if forward { (i + 1) % n } else { (i + n - 1) % n };
            if self.selectable[i] {
                return Some(i);
            }
        }
        None
    }

    /// Move down, wrapping; the first item from nothing.
    fn next(&mut self) {
        self.highlight = self.step(self.highlight, true);
    }

    /// Move up, wrapping; the last item from nothing.
    fn prev(&mut self) {
        self.highlight = self.step(self.highlight, false);
    }

    fn first(&mut self) {
        self.highlight = self.step(None, true);
    }

    fn last(&mut self) {
        self.highlight = self.step(None, false);
    }

    /// Highlight `i`; refused (and nothing changes) for a separator or a
    /// disabled item.
    fn set(&mut self, i: usize) -> bool {
        if self.selectable.get(i).copied().unwrap_or(false) {
            self.highlight = Some(i);
            true
        } else {
            false
        }
    }

    /// The keyboard-open highlight: the checked item if it can be
    /// landed on, else the first.
    fn initial(&mut self, checked: Option<usize>) {
        if !checked.is_some_and(|c| self.set(c)) {
            self.first();
        }
    }

    /// What Enter activates.
    fn activate(&self) -> Option<usize> {
        self.highlight.filter(|&i| self.selectable[i])
    }
}

/// Each entry's top (relative to the first row) and the rows' total
/// height, from which entries are separators.
fn row_offsets(separators: &[bool]) -> (Vec<f32>, f32) {
    let mut y = 0.0;
    let mut tops = Vec::with_capacity(separators.len());
    for &sep in separators {
        tops.push(y);
        y += if sep { MENU_SEP_H } else { MENU_ROW_H };
    }
    (tops, y)
}

// ---------------------------------------------------------------------
// MenuButton
// ---------------------------------------------------------------------

type SelectFn<S> = dyn Fn(&mut S, &mut Ui<S>, &str);

/// A round icon button that opens a menu; see the [module](self) docs.
pub struct MenuButton<S> {
    icon: String,
    label: String,
    entries: Vec<MenuEntry>,
    enabled: bool,
    diameter: f32,
    align_right: bool,
    pressed: bool,
    on_select: Option<Rc<SelectFn<S>>>,
    popup: Option<WindowId>,
    list: Option<WidgetId>,
}

impl<S> std::fmt::Debug for MenuButton<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MenuButton")
            .field("icon", &self.icon)
            .field("label", &self.label)
            .field("entries", &self.entries)
            .field("enabled", &self.enabled)
            .field("popup", &self.popup)
            .finish_non_exhaustive()
    }
}

/// How a menu was opened, which decides the first highlight.
#[derive(Clone, Copy)]
enum Opening {
    Pointer,
    First,
    Last,
}

impl<S> MenuButton<S> {
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

    /// The menu's entries.
    #[must_use]
    pub fn items(&self) -> &[MenuEntry] {
        &self.entries
    }

    /// Whether its menu is showing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.popup.is_some()
    }

    /// The open menu's window.
    #[must_use]
    pub fn popup(&self) -> Option<WindowId> {
        self.popup
    }

    /// Whether it reacts to input.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn checked_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.item().is_some_and(|i| i.checked && i.mark != Mark::None))
    }
}

impl<S: 'static> MenuButton<S> {
    /// Forget a popup the server or the app closed while this widget was
    /// out of its slot, so its close handler could not reach it.
    fn forget_stale(&mut self, ui: &Ui<S>) {
        if self.popup.is_some_and(|p| !ui.has_window(p)) {
            self.popup = None;
            self.list = None;
        }
    }

    fn open(&mut self, cx: &mut EventCx<'_, S>, how: Opening) {
        if self.popup.is_some() || !self.enabled {
            return;
        }
        let Some(parent) = cx.ui.window_of(cx.id) else {
            return;
        };
        let button = cx.id;
        let mut list = MenuList {
            select: self.on_select.clone(),
            ids: self
                .entries
                .iter()
                .map(|e| e.item().map(|i| i.id.clone()))
                .collect(),
            nav: MenuNav::new(self.entries.iter().map(MenuEntry::selectable).collect()),
        };
        match how {
            Opening::Pointer => {}
            Opening::First => list.nav.initial(self.checked_index()),
            Opening::Last => list.nav.last(),
        }
        let mut built = Built::new(list);
        built.state_mut().focusable = true;
        built.state_mut().name = Some("menu".to_owned());
        for (index, e) in self.entries.iter().enumerate() {
            built.push(match e {
                MenuEntry::Item(i) => {
                    let mut b = Built::new(MenuRow {
                        index,
                        item: i.clone(),
                        metrics: TextMetrics::default(),
                        held: false,
                    });
                    b.state_mut().name = Some(i.id.clone());
                    b
                }
                MenuEntry::Separator => Built::new(MenuSeparator),
            });
        }
        let root = cx.ui.build(built);
        let rect = cx.ui.window_bounds(button);
        let placement = if self.align_right {
            PopupPlacement::below(rect)
        } else {
            PopupPlacement::below_left(rect)
        };
        let win = match cx.ui.add_popup(parent, placement, None, root) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("nitro-ui: menu: {e}");
                let _ = cx.ui.remove(root);
                return;
            }
        };
        self.popup = Some(win);
        self.list = Some(root);
        let refocus = cx.ui.click_takes_focus();
        cx.ui.on_window_closed(win, move |_s: &mut S, ui: &mut Ui<S>| {
            if let Ok(mut b) = ui.widget_mut::<MenuButton<S>>(button)
                && b.popup == Some(win)
            {
                b.popup = None;
                b.list = None;
                b.request_paint();
            }
            if refocus {
                ui.focus(button);
            }
        });
        // A `NO_FOCUS` parent never gets keys; the popup reads them
        // through a grab instead (the server drops it with the popup).
        if cx.ui.is_shell() && !refocus {
            let _ = cx.ui.grab_keyboard_of(win, true);
        }
        cx.ui.focus(root);
        cx.request_paint();
    }
}

impl<S: 'static> Widget<S> for MenuButton<S> {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        constraints.constrain(Size::new(self.diameter, self.diameter))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let hovered = cx.ui.is_hovered(cx.id);
        let focused = cx.ui.is_focused(cx.id);
        let (face, ink) = if !self.enabled {
            (ColorRole::ButtonDisabled, ColorRole::TextDim)
        } else if self.pressed || self.popup.is_some() {
            (ColorRole::ButtonActive, ColorRole::ButtonText)
        } else if hovered {
            (ColorRole::ButtonHover, ColorRole::ButtonText)
        } else {
            (ColorRole::Button, ColorRole::ButtonText)
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
        self.forget_stale(cx.ui);
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
                    self.open(cx, Opening::Pointer);
                }
                Handled::Yes
            }
            Event::PointerEnter { .. } | Event::PointerLeave | Event::FocusChanged { .. } => {
                cx.request_paint();
                Handled::No
            }
            Event::KeyDown(k) => {
                if let Some(list) = self.list {
                    // Keys addressed to the parent window: drive the
                    // open menu from here.
                    return drive(cx.ui, list, k.keycode);
                }
                match k.keycode {
                    key::ENTER | key::SPACE | key::DOWN => self.open(cx, Opening::First),
                    key::UP => self.open(cx, Opening::Last),
                    _ => return Handled::No,
                }
                Handled::Yes
            }
            // The release of the key that opened (or activated) it.
            Event::KeyUp(k) if matches!(k.keycode, key::ENTER | key::SPACE) => Handled::Yes,
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
            name: Some(self.label.clone()),
            value: Some(if self.popup.is_some() { "open" } else { "closed" }.to_owned()),
            actions: if self.enabled {
                vec!["click", "activate", "focus", "open"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, _arg: Option<&str>) -> Handled {
        if !self.enabled {
            return Handled::No;
        }
        self.forget_stale(cx.ui);
        match action {
            "click" | "activate" | "press" | "open" => {
                self.open(cx, Opening::Pointer);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`MenuButton`].
impl<S: 'static> WidgetMut<'_, MenuButton<S>, S> {
    /// Replace the entries. An open menu keeps showing the old ones until
    /// it is next opened.
    pub fn set_items(&mut self, entries: impl IntoIterator<Item = MenuEntry>) {
        self.entries = entries.into_iter().collect();
        debug_check_unique(&self.entries);
    }

    /// Check or uncheck the item `id`. Checking a [`Mark::Radio`] item
    /// unchecks the other radio items. An open menu updates at once.
    pub fn set_checked(&mut self, id: &str, on: bool) {
        let radio = self
            .entries
            .iter()
            .filter_map(MenuEntry::item)
            .any(|i| i.id == id && i.mark == Mark::Radio);
        for e in &mut self.entries {
            if let MenuEntry::Item(i) = e {
                if i.id == id {
                    i.checked = on;
                } else if radio && on && i.mark == Mark::Radio {
                    i.checked = false;
                }
            }
        }
        let Some(list) = self.list else {
            return;
        };
        let marks: Vec<Option<bool>> = self
            .entries
            .iter()
            .map(|e| e.item().map(|i| i.checked))
            .collect();
        let ui = self.ui();
        for (row, mark) in ui.children(list).into_iter().zip(marks) {
            if let (Some(on), Ok(mut r)) = (mark, ui.widget_mut::<MenuRow>(row))
                && r.item.checked != on
            {
                r.item.checked = on;
                r.request_paint();
            }
        }
    }

    /// Enable or disable the button. Disabling it does not close an open
    /// menu; [`close`](Self::close) does.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled != on {
            self.enabled = on;
            if !on {
                self.pressed = false;
            }
            self.set_focusable(on);
            self.request_paint();
        }
    }

    /// Change the icon.
    pub fn set_icon(&mut self, name: impl Into<String>) {
        let name = name.into();
        if self.icon != name {
            self.icon = name;
            self.request_paint();
        }
    }

    /// Close the menu, if it is open, at the end of this turn.
    pub fn close(&mut self) {
        if let Some(p) = self.popup {
            self.ui().defer(move |s: &mut S, ui: &mut Ui<S>| {
                let _ = ui.remove_window(s, p);
            });
        }
    }
}

/// Builder for a [`MenuButton`].
pub struct MenuButtonBuilder<S> {
    built: Built<S>,
    button: MenuButton<S>,
}

impl<S: 'static> MenuButtonBuilder<S> {
    /// The accessible label (default: the icon's name).
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> Self {
        self.button.label = text.into();
        self
    }

    /// Append an item.
    #[must_use]
    pub fn item(mut self, item: MenuItem) -> Self {
        self.button.entries.push(MenuEntry::Item(item));
        self
    }

    /// Append a separator.
    #[must_use]
    pub fn separator(mut self) -> Self {
        self.button.entries.push(MenuEntry::Separator);
        self
    }

    /// Append several entries.
    #[must_use]
    pub fn items(mut self, entries: impl IntoIterator<Item = MenuEntry>) -> Self {
        self.button.entries.extend(entries);
        self
    }

    /// What activating an item does; receives the item's id. The menu is
    /// already closed when it runs.
    #[must_use]
    pub fn on_select(mut self, f: impl Fn(&mut S, &mut Ui<S>, &str) + 'static) -> Self {
        self.button.on_select = Some(Rc::new(f));
        self
    }

    /// Align the menu's right edge with the button's (default: left
    /// edges aligned).
    #[must_use]
    pub fn align_right(mut self, on: bool) -> Self {
        self.button.align_right = on;
        self
    }

    /// Start disabled.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.button.enabled = false;
        self
    }

    /// A diameter other than [`ROUND_BTN`].
    #[must_use]
    pub fn diameter(mut self, px: f32) -> Self {
        self.button.diameter = px;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for MenuButtonBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for MenuButtonBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        debug_check_unique(&self.button.entries);
        self.built.state_mut().focusable = self.button.enabled;
        self.built.replace_widget(self.button);
        self.built
    }
}

/// A round button showing `icon` that opens a menu; add entries with
/// [`MenuButtonBuilder::item`] and react with
/// [`MenuButtonBuilder::on_select`].
#[must_use]
pub fn menu_button<S: 'static>(icon: impl Into<String>) -> MenuButtonBuilder<S> {
    let icon = icon.into();
    MenuButtonBuilder {
        built: Built::new(crate::widgets::Flex),
        button: MenuButton {
            label: icon.clone(),
            icon,
            entries: Vec::new(),
            enabled: true,
            diameter: ROUND_BTN,
            align_right: false,
            pressed: false,
            on_select: None,
            popup: None,
            list: None,
        },
    }
}

// ---------------------------------------------------------------------
// The popup's content
// ---------------------------------------------------------------------

/// What a key did to an open menu.
#[derive(Clone, Copy)]
enum Outcome {
    Ignored,
    Moved,
    Activate(usize),
    Close,
}

/// The popup's root: the panel face, the highlight, and the rows laid
/// out top to bottom. Its children are one per entry, in order.
struct MenuList<S> {
    select: Option<Rc<SelectFn<S>>>,
    ids: Vec<Option<String>>,
    nav: MenuNav,
}

impl<S: 'static> MenuList<S> {
    fn key(&mut self, keycode: u32) -> Outcome {
        let before = self.nav.highlight;
        match keycode {
            key::DOWN => self.nav.next(),
            key::UP => self.nav.prev(),
            key::HOME => self.nav.first(),
            key::END => self.nav.last(),
            key::ENTER | key::SPACE => {
                return self.nav.activate().map_or(Outcome::Ignored, Outcome::Activate);
            }
            key::ESC => return Outcome::Close,
            _ => return Outcome::Ignored,
        }
        if self.nav.highlight == before {
            Outcome::Ignored
        } else {
            Outcome::Moved
        }
    }

    /// What activating entry `i` needs, if it can be activated.
    fn activation(&self, i: usize) -> Option<Activation<S>> {
        if !self.nav.selectable.get(i).copied().unwrap_or(false) {
            return None;
        }
        let id = self.ids.get(i)?.clone()?;
        Some(Activation {
            id,
            select: self.select.clone(),
        })
    }

    fn separators(&self) -> Vec<bool> {
        self.ids.iter().map(Option::is_none).collect()
    }
}

struct Activation<S> {
    id: String,
    select: Option<Rc<SelectFn<S>>>,
}

/// Close the popup `list` is the root of, then report the item: one
/// deferred step, so both land in one commit.
fn fire<S: 'static>(ui: &mut Ui<S>, list: WidgetId, act: Option<Activation<S>>) {
    ui.defer(move |s: &mut S, ui: &mut Ui<S>| {
        if let Some(win) = ui.window_of(list) {
            let _ = ui.remove_window(s, win);
        }
        if let Some(Activation {
            id,
            select: Some(f),
        }) = act
        {
            f(s, ui, &id);
        }
    });
}

/// Apply a key to the menu rooted at `list`, from outside it.
fn drive<S: 'static>(ui: &mut Ui<S>, list: WidgetId, keycode: u32) -> Handled {
    let (outcome, act) = match ui.widget_mut::<MenuList<S>>(list) {
        Ok(mut l) => {
            let o = l.key(keycode);
            let act = match o {
                Outcome::Activate(i) => l.activation(i),
                Outcome::Moved => {
                    l.request_paint();
                    None
                }
                _ => None,
            };
            (o, act)
        }
        Err(_) => return Handled::No,
    };
    finish(ui, list, outcome, act, keycode)
}

fn finish<S: 'static>(
    ui: &mut Ui<S>,
    list: WidgetId,
    outcome: Outcome,
    act: Option<Activation<S>>,
    keycode: u32,
) -> Handled {
    match outcome {
        Outcome::Activate(_) => fire(ui, list, act),
        Outcome::Close => fire(ui, list, None),
        Outcome::Moved => {}
        // Navigation keys are the menu's even when they change nothing
        // (Down on a one-item menu), so they never reach a shortcut.
        Outcome::Ignored => {
            if !matches!(
                keycode,
                key::UP | key::DOWN | key::HOME | key::END | key::ENTER | key::SPACE
            ) {
                return Handled::No;
            }
        }
    }
    Handled::Yes
}

/// Move the highlight of the menu rooted at `list` to row `index`.
fn hover<S: 'static>(ui: &mut Ui<S>, list: WidgetId, index: usize) {
    if let Ok(mut l) = ui.widget_mut::<MenuList<S>>(list)
        && l.nav.highlight != Some(index)
        && l.nav.set(index)
    {
        l.request_paint();
    }
}

/// Activate row `index` of the menu rooted at `list`.
fn activate_row<S: 'static>(ui: &mut Ui<S>, list: WidgetId, index: usize) -> bool {
    let act = match ui.widget_mut::<MenuList<S>>(list) {
        Ok(l) => l.activation(index),
        Err(_) => None,
    };
    if act.is_none() {
        return false;
    }
    fire(ui, list, act);
    true
}

impl<S: 'static> Widget<S> for MenuList<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let mut w: f32 = 0.0;
        for c in cx.children() {
            w = w.max(cx.measure_child(c, Constraints::unbounded()).w);
        }
        let (_, h) = row_offsets(&self.separators());
        let w = (w + 2.0 * MENU_PAD).clamp(MENU_MIN_W, MENU_MAX_W);
        constraints.constrain(Size::new(w, h + 2.0 * MENU_PAD))
    }

    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        let seps = self.separators();
        let (tops, _) = row_offsets(&seps);
        let w = (bounds.w - 2.0 * MENU_PAD).max(0.0);
        for (i, c) in cx.children().into_iter().enumerate() {
            let (top, sep) = (tops.get(i).copied().unwrap_or(0.0), seps.get(i) == Some(&true));
            let h = if sep { MENU_SEP_H } else { MENU_ROW_H };
            cx.place_child(c, Rect::new(MENU_PAD, MENU_PAD + top, w, h));
        }
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let b = cx.bounds;
        let (surface, border) = (cx.color(ColorRole::Surface), cx.color(ColorRole::Border));
        cx.rect(
            0,
            Rect::new(0.0, 0.0, b.w, b.h),
            Fill::Solid(surface),
            MENU_RADIUS,
            (1.0, border),
        );
        let (tops, _) = row_offsets(&self.separators());
        let (rect, face) = match self.nav.highlight.and_then(|i| tops.get(i)) {
            Some(&top) => (
                Rect::new(MENU_PAD, MENU_PAD + top, b.w - 2.0 * MENU_PAD, MENU_ROW_H),
                cx.color(ColorRole::ButtonHover),
            ),
            None => (Rect::new(0.0, 0.0, 0.0, 0.0), Color::TRANSPARENT),
        };
        cx.rect(1, rect, Fill::Solid(face), 8.0, (0.0, Color::TRANSPARENT));
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        // Keys addressed to the popup itself: a keyboard grab.
        let Event::KeyDown(k) = ev else {
            return Handled::No;
        };
        let outcome = self.key(k.keycode);
        let act = match outcome {
            Outcome::Activate(i) => self.activation(i),
            Outcome::Moved => {
                cx.request_paint();
                None
            }
            _ => None,
        };
        finish(cx.ui, cx.id, outcome, act, k.keycode)
    }

    fn role(&self) -> Role {
        Role::List
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: self
                .nav
                .highlight
                .and_then(|i| self.ids.get(i).cloned().flatten()),
            actions: vec!["focus"],
        }
    }
}

/// One item of an open menu: the mark column, the icon and the label.
/// The highlight behind it is the [`MenuList`]'s.
struct MenuRow {
    index: usize,
    item: MenuItem,
    metrics: TextMetrics,
    held: bool,
}

impl MenuRow {
    fn text_style(cx_theme: &crate::Theme) -> TextStyle {
        let mut s = TextStyle::from_theme(cx_theme);
        s.size_px = LABEL_PX;
        s
    }

    fn label_x(&self) -> f32 {
        let mut x = ROW_PAD + QS_ICON + ROW_GAP;
        if self.item.icon.is_some() {
            x += QS_ICON + ROW_GAP;
        }
        x
    }

    fn list<S: 'static>(cx: &EventCx<'_, S>) -> Option<WidgetId> {
        cx.ui.parent(cx.id)
    }
}

impl<S: 'static> Widget<S> for MenuRow {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let style = Self::text_style(cx.theme());
        self.metrics = cx
            .measure_text(&self.item.label, &style, 0.0)
            .unwrap_or_default();
        constraints.constrain(Size::new(
            self.label_x() + self.metrics.width + ROW_PAD,
            MENU_ROW_H,
        ))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let b = cx.bounds;
        let ink = if self.item.enabled {
            ColorRole::Text
        } else {
            ColorRole::TextDim
        };
        let mark_ink = if self.item.enabled {
            ColorRole::Accent
        } else {
            ColorRole::TextDim
        };
        let mark = Rect::new(ROW_PAD, 0.0, QS_ICON, b.h);
        let (glyph, size) = match (self.item.mark, self.item.checked) {
            (Mark::Check, true) => ("check", QS_ICON),
            (Mark::Radio, true) => ("circle-fill", RADIO_DOT),
            _ => ("", QS_ICON),
        };
        cx.icon(0, mark, glyph, size, mark_ink);
        let icon = self.item.icon.clone().unwrap_or_default();
        let r = Rect::new(ROW_PAD + QS_ICON + ROW_GAP, 0.0, QS_ICON, b.h);
        cx.icon(1, r, &icon, QS_ICON, ink);
        let style = Self::text_style(cx.theme());
        let color = cx.color(ink);
        let h = self.metrics.height.max(1.0);
        let x = self.label_x();
        let label = self.item.label.clone();
        cx.text(
            2,
            Rect::new(x, ((b.h - h) / 2.0).max(0.0), (b.w - x - ROW_PAD).max(0.0), h),
            &label,
            TextRun::new(&style, color),
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        let Some(list) = Self::list(cx) else {
            return Handled::No;
        };
        match ev {
            Event::PointerEnter { .. } | Event::PointerMove { .. } => {
                if self.item.enabled {
                    hover(cx.ui, list, self.index);
                }
                Handled::No
            }
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                self.held = self.item.enabled;
                Handled::Yes
            }
            Event::PointerUp { pos, button } if *button == button::LEFT => {
                let was = std::mem::take(&mut self.held);
                if was && cx.contains(*pos) {
                    activate_row(cx.ui, list, self.index);
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn enabled(&self) -> bool {
        self.item.enabled
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some(self.item.label.clone()),
            value: Some(self.item.checked.to_string()),
            actions: if self.item.enabled {
                vec!["click", "activate"]
            } else {
                Vec::new()
            },
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, _arg: Option<&str>) -> Handled {
        match action {
            "click" | "activate" | "press" if self.item.enabled => {
                let Some(list) = Self::list(cx) else {
                    return Handled::No;
                };
                if activate_row(cx.ui, list, self.index) {
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            _ => Handled::No,
        }
    }
}

/// A hairline between groups of items.
struct MenuSeparator;

impl<S: 'static> Widget<S> for MenuSeparator {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        constraints.constrain(Size::new(0.0, MENU_SEP_H))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let b = cx.bounds;
        let line = cx.color(ColorRole::Track);
        cx.fill_rect(
            0,
            Rect::new(ROW_PAD, (b.h / 2.0).floor(), (b.w - 2.0 * ROW_PAD).max(0.0), 1.0),
            line,
        );
    }

    fn role(&self) -> Role {
        Role::Separator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(sel: &[bool]) -> MenuNav {
        MenuNav::new(sel.to_vec())
    }

    #[test]
    fn next_and_prev_skip_what_cannot_be_landed_on_and_wrap() {
        // item, separator, disabled, item, item
        let mut n = nav(&[true, false, false, true, true]);
        n.next();
        assert_eq!(n.highlight, Some(0));
        n.next();
        assert_eq!(n.highlight, Some(3));
        n.next();
        assert_eq!(n.highlight, Some(4));
        n.next();
        assert_eq!(n.highlight, Some(0), "wraps");
        n.prev();
        assert_eq!(n.highlight, Some(4), "wraps back");
        n.prev();
        assert_eq!(n.highlight, Some(3));
        n.prev();
        assert_eq!(n.highlight, Some(0), "skips the disabled item and the separator");
    }

    #[test]
    fn prev_from_nothing_is_the_last() {
        let mut n = nav(&[true, true, false]);
        n.prev();
        assert_eq!(n.highlight, Some(1));
    }

    #[test]
    fn nothing_enabled_highlights_nothing() {
        let mut n = nav(&[false, false]);
        n.next();
        assert_eq!(n.highlight, None);
        n.last();
        assert_eq!(n.highlight, None);
        n.initial(Some(0));
        assert_eq!(n.highlight, None);
        assert_eq!(n.activate(), None);
        let mut empty = nav(&[]);
        empty.next();
        assert_eq!(empty.highlight, None);
    }

    #[test]
    fn home_and_end_jump() {
        let mut n = nav(&[false, true, true, true, false]);
        n.last();
        assert_eq!(n.highlight, Some(3));
        n.first();
        assert_eq!(n.highlight, Some(1));
    }

    #[test]
    fn the_initial_highlight_is_the_checked_item() {
        let mut n = nav(&[true, true, true]);
        n.initial(Some(2));
        assert_eq!(n.highlight, Some(2));
        let mut n = nav(&[true, false, true]);
        n.initial(Some(1));
        assert_eq!(n.highlight, Some(0), "a disabled checked item falls back");
        let mut n = nav(&[false, true]);
        n.initial(None);
        assert_eq!(n.highlight, Some(1));
    }

    #[test]
    fn set_rejects_disabled_items_and_separators() {
        let mut n = nav(&[true, false]);
        assert!(n.set(0));
        assert!(!n.set(1));
        assert!(!n.set(7));
        assert_eq!(n.highlight, Some(0));
        assert_eq!(n.activate(), Some(0));
    }

    #[test]
    #[allow(clippy::float_cmp)] // exact sums of small constants
    fn offsets_account_for_separators() {
        let (tops, h) = row_offsets(&[false, true, false, false]);
        assert_eq!(tops, vec![0.0, MENU_ROW_H, MENU_ROW_H + MENU_SEP_H, 2.0 * MENU_ROW_H + MENU_SEP_H]);
        assert_eq!(h, 3.0 * MENU_ROW_H + MENU_SEP_H);
        assert_eq!(row_offsets(&[]), (Vec::new(), 0.0));
    }

    #[test]
    fn list_keys_move_activate_and_close() {
        let mut l: MenuList<()> = MenuList {
            select: None,
            ids: vec![Some("a".into()), None, Some("b".into())],
            nav: nav(&[true, false, true]),
        };
        assert!(matches!(l.key(key::ENTER), Outcome::Ignored), "nothing highlighted");
        assert!(matches!(l.key(key::DOWN), Outcome::Moved));
        assert!(matches!(l.key(key::DOWN), Outcome::Moved));
        assert!(matches!(l.key(key::ENTER), Outcome::Activate(2)));
        assert_eq!(l.activation(2).map(|a| a.id), Some("b".to_owned()));
        assert!(l.activation(1).is_none());
        assert!(matches!(l.key(key::ESC), Outcome::Close));
        assert!(matches!(l.key(key::A), Outcome::Ignored));
    }

    #[test]
    fn items_build_up() {
        let i = MenuItem::new("x", "X").icon("gear").radio(true).disabled();
        assert_eq!(i.id(), "x");
        assert_eq!(i.icon_name(), Some("gear"));
        assert_eq!(i.mark(), Mark::Radio);
        assert!(i.is_checked());
        assert!(!i.is_enabled());
        assert!(!MenuEntry::from(i).selectable());
        assert!(!MenuEntry::Separator.selectable());
    }
}
