//! [`List`] — a virtualised list of rows.
//!
//! Every other widget in this crate is the size of what it shows. A list
//! is the first one that is not: a directory of a hundred thousand files
//! is a hundred thousand rows, and a toolkit that turned each of them
//! into a widget would allocate a hundred thousand arena slots, measure
//! them all and hand the server a hundred thousand scene nodes to draw a
//! screenful of text. `docs/ui.md` said so under *Deviations* — "a list
//! of ten thousand rows costs ten thousand widgets; virtualisation is
//! M3 and is a widget, not a new mechanism" — and this is that widget.
//!
//! # What makes it cheap
//!
//! Three properties, each measured from the outside by counting
//! mutations in `crates/nitro-ui/tests/list.rs`, because a cost claim
//! nothing checks stops being true:
//!
//! * **The scene holds a screenful, whatever the model holds.** The
//!   widget materialises `visible + 2` rows into paint slots of its own
//!   — the [`PaintCx::keep`](crate::PaintCx::keep) mechanism
//!   `nitro-term` introduced for exactly this shape — so a 100 000-row
//!   model and a 100-row model create the *same* number of nodes.
//! * **A scroll that does not change the visible set is one
//!   `SetTransform`.** The rows hang under a clipping group of the
//!   widget's own and the group's transform is the scroll offset, so
//!   scrolling inside the two spare rows costs one mutation and no
//!   repaint at all. That is the same trick [`Scroll`] plays one level
//!   up, and it is why the spare rows exist.
//! * **A row that did not change sends nothing.** Slots are addressed
//!   `row_index % ring_len`, not `row_index - first_row`, so
//!   re-anchoring the window moves *only the rows that actually
//!   changed*: a full page down re-emits a page, a single row of
//!   re-anchoring re-emits a single row.
//!
//! # The rows are data, not widgets
//!
//! The model is a [`Row`] per line — an optional glyph, a primary text
//! and a right-aligned secondary text — and the widget owns it. There is
//! no per-row widget, no per-row callback and no per-row state: a row is
//! addressed by its index, and that index is what `on_activate` and
//! `on_select` are handed.
//!
//! An app with a large model of its own pays for the `Vec<Row>` it hands
//! over (three small allocations a row). That is the deliberate trade: a
//! borrowed model would have to be reachable from `paint`, which sees
//! `&mut Ui<S>` and not `&mut S` — the toolkit's central rule — so the
//! choice is between owning the rows and threading interior mutability
//! through the one place this crate has none. [`ListModel`] is the door
//! left open for an app that would rather generate a row than store it.
//!
//! # What selection costs, and why the text does not change colour
//!
//! Selection is drawn as the row's **background** and nothing else, so
//! moving it from one row to the next is exactly two `SetFill`s. Tinting
//! the text as well would have been prettier and would have cost a
//! `SetText` per run per row on every arrow key — a list whose selection
//! cannot be held down is not obviously better looking, and this is the
//! toolkit whose whole claim is that work is proportional to change.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nitro_core::{Color, Rect, Size};
use nitro_wire::msg::Fill;
use nitro_wire::types::Align;

use crate::build::{Built, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled, button, key, mods};
use crate::layout::{Constraints, ShrinkFloor};
use crate::theme::TextStyle;
use crate::ui::{Ui, WidgetMut};
use crate::widget::{Access, EventCx, LayoutCx, MeasureCx, PaintCx, Role, Slot, TextRun, Widget};

/// Paint slots a materialised row occupies: its background, its glyph,
/// its primary text and its secondary text.
const SLOTS_PER_ROW: Slot = 4;
/// Slot of the list's own background rect. It is what makes the widget
/// hit-testable when it is empty, for the reason `Scroll` paints one.
const BACKGROUND: Slot = 0;
/// Slot of the clipping group the rows hang under.
const VIEWPORT: Slot = 1;
/// First slot a row can use.
const ROW_BASE: Slot = 2;
/// Rows materialised beyond the ones that fit. Two is what makes a
/// one-row scroll free: the visible set is still inside the window, so
/// nothing is re-emitted.
const SPARE_ROWS: usize = 2;
/// How long a type-ahead prefix survives without another key.
const TYPE_AHEAD_GAP: Duration = Duration::from_millis(900);
/// Two clicks closer together than this on the same row are a
/// double-click, which activates it.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Width reserved for the glyph slot, as the spec's "16 px icon".
const ICON_W: f32 = 16.0;
/// Horizontal padding inside a row.
const ROW_PAD: f32 = 6.0;

/// One line of a [`List`].
///
/// Three fields rather than a formatted string, because the secondary
/// text is right-aligned in a column of its own: a size or a date lines
/// up down the list only if the widget knows which part of the row it
/// is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    /// A short glyph shown in a 16 px column: `"▸"`, `"/"`, whatever the
    /// app draws with the fonts the server happens to have. There is no
    /// icon theme and no image here on purpose — an icon loader is a
    /// feature, and this is the widget.
    pub icon: Option<String>,
    /// The row's main text, left-aligned and clipped to its column.
    pub text: String,
    /// Secondary text, right-aligned: a size, a date, a count.
    pub detail: String,
}

impl Row {
    /// A row with only a primary text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            icon: None,
            text: text.into(),
            detail: String::new(),
        }
    }

    /// The same row with a glyph in its icon column.
    #[must_use]
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }

    /// The same row with right-aligned secondary text.
    #[must_use]
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// Where a [`List`] gets its rows.
///
/// A `Vec<Row>` is one, and is what every app in this tree uses. The
/// trait exists for the app whose model is already a `Vec` of something
/// else and would rather format a row on demand than keep a second copy
/// of it: `row(i)` is called only for the rows that are materialised,
/// which is a screenful.
pub trait ListModel: 'static {
    /// How many rows there are.
    fn len(&self) -> usize;

    /// Whether there are no rows at all.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `index`, which is always `< len()`.
    fn row(&self, index: usize) -> Row;
}

impl ListModel for Vec<Row> {
    fn len(&self) -> usize {
        self.as_slice().len()
    }

    fn row(&self, index: usize) -> Row {
        self.as_slice()
            .get(index)
            .cloned()
            .unwrap_or_else(Row::default)
    }
}

/// What a materialised slot last drew, so the next paint can tell
/// "unchanged" from "a different row" without re-deriving the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowCache {
    /// Which row of the model this slot holds.
    row: usize,
    /// The model generation it was taken from; a model that was replaced
    /// invalidates every slot without the widget diffing any strings.
    generation: u64,
    /// Whether it was drawn as selected.
    selected: bool,
}

type IndexFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, usize)>;

/// A virtualised list of [`Row`]s.
///
/// See the module documentation for what it costs and why. The short
/// version: the scene holds `visible + 2` rows however long the model
/// is, scrolling inside those spare rows is one `SetTransform`, and
/// moving the selection is two `SetFill`s.
pub struct List<S> {
    /// The rows themselves.
    model: Box<dyn ListModel>,
    /// Bumped whenever the model is replaced, so every cached slot is
    /// stale at once and no string comparison is needed to notice.
    generation: u64,
    /// Height of one row, in logical pixels.
    row_h: f32,
    /// Line height of the row's text, from the last measurement.
    line_h: f32,
    /// Viewport height from the last layout.
    view_h: f32,
    /// Viewport width from the last layout; a change re-emits the rows,
    /// because their text boxes are cut from it.
    view_w: f32,
    /// How far down the content is scrolled, in logical pixels.
    offset: f32,
    /// First materialised row.
    anchor: usize,
    /// How many rows are materialised: `visible + SPARE_ROWS`.
    ring: usize,
    /// What each ring slot last drew.
    cache: Vec<Option<RowCache>>,
    /// The row the keyboard is on.
    cursor: usize,
    /// Every selected row. A set rather than a flag per row because the
    /// common case is one element and the model can be enormous.
    selected: BTreeSet<usize>,
    /// Where a Shift-extended selection started.
    extend_from: usize,
    /// Pixels per wheel notch.
    speed: f32,
    /// The type-ahead prefix and when it was last added to.
    prefix: String,
    /// When the prefix was last touched, so it expires without a timer:
    /// an idle list must schedule nothing at all.
    prefix_at: Option<Instant>,
    /// The last click, for double-click detection.
    last_click: Option<(usize, Instant)>,
    /// Set when something the rows are derived from changed (the size,
    /// the row height, the model), so the next paint re-emits them all
    /// rather than trusting the per-slot cache.
    dirty_rows: bool,
    /// The colours and style the rows currently on screen were painted
    /// with, so a paint can tell whether the *look* changed as well as
    /// the content.
    ///
    /// The per-slot cache keys on "is this the same row, selected the
    /// same way" — which is exactly right for a model change and
    /// exactly wrong for a theme change, because every one of those
    /// answers is still true when every colour has moved. Without this
    /// field a desktop-wide scheme switch left a settled list painting
    /// the old scheme's text for ever: `Ui::set_palette` marked the
    /// widget, the widget re-ran `paint`, and `paint` answered
    /// `cx.keep` for every row. Found on the box, with a file list in
    /// dark-scheme grey on a light window.
    painted_with: Option<RowPaint>,
    /// Invoked on Enter or a double-click.
    on_activate: Option<IndexFn<S>>,
    /// Invoked whenever the cursor lands on a different row.
    on_select: Option<IndexFn<S>>,
}

impl<S> std::fmt::Debug for List<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("List")
            .field("rows", &self.model.len())
            .field("cursor", &self.cursor)
            .field("offset", &self.offset)
            .field("materialised", &self.ring)
            .finish_non_exhaustive()
    }
}

impl<S: 'static> Default for List<S> {
    fn default() -> Self {
        Self {
            model: Box::new(Vec::<Row>::new()),
            generation: 0,
            row_h: 0.0,
            line_h: 0.0,
            view_h: 0.0,
            view_w: 0.0,
            offset: 0.0,
            anchor: 0,
            ring: 0,
            cache: Vec::new(),
            cursor: 0,
            selected: BTreeSet::new(),
            extend_from: 0,
            speed: 3.0,
            prefix: String::new(),
            prefix_at: None,
            last_click: None,
            dirty_rows: true,
            painted_with: None,
            on_activate: None,
            on_select: None,
        }
    }
}

// -- reading a list ---------------------------------------------------

impl<S: 'static> List<S> {
    /// How many rows the model holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.model.len()
    }

    /// Whether the model is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
    }

    /// Row `index` of the model, or `None` past the end.
    #[must_use]
    pub fn row(&self, index: usize) -> Option<Row> {
        (index < self.model.len()).then(|| self.model.row(index))
    }

    /// The row the keyboard is on.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Every selected row, ascending.
    #[must_use]
    pub fn selection(&self) -> Vec<usize> {
        self.selected.iter().copied().collect()
    }

    /// Height of one row, in logical pixels.
    #[must_use]
    pub fn row_height(&self) -> f32 {
        self.row_h
    }

    /// How far down the content is scrolled.
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// The largest offset that still shows content.
    #[must_use]
    pub fn max_offset(&self) -> f32 {
        (self.content_height() - self.view_h).max(0.0)
    }

    /// Height of the whole model, as if every row were drawn.
    #[must_use]
    pub fn content_height(&self) -> f32 {
        self.model.len() as f32 * self.row_h
    }

    /// How many rows are materialised into scene nodes.
    ///
    /// **This is the number the virtualisation claim is about**, and it
    /// is public so a test can assert it from the outside: it is
    /// `visible + 2` whether the model holds a hundred rows or a
    /// hundred thousand.
    #[must_use]
    pub fn materialised(&self) -> usize {
        self.ring.min(self.model.len())
    }

    /// The rows currently inside the viewport, as `(index, row)`.
    ///
    /// What `hey <app> get <list> text` reports, and the honest answer
    /// to "what does this widget read": the model is not on screen, the
    /// window into it is.
    #[must_use]
    pub fn visible_rows(&self) -> Vec<(usize, Row)> {
        let first = self.first_visible();
        let last = (first + self.rows_that_fit()).min(self.model.len());
        (first..last).map(|i| (i, self.model.row(i))).collect()
    }

    /// The first row the viewport shows, whole or in part.
    #[must_use]
    pub fn first_visible(&self) -> usize {
        if self.row_h <= 0.0 {
            return 0;
        }
        ((self.offset / self.row_h).floor().max(0.0) as usize).min(self.model.len())
    }

    /// How many rows the viewport can show at once, rounded up: a half
    /// row at the bottom is still a row somebody can see.
    #[must_use]
    pub fn rows_that_fit(&self) -> usize {
        if self.row_h <= 0.0 {
            return 0;
        }
        (self.view_h / self.row_h).ceil().max(1.0) as usize
    }
}

// -- the private half -------------------------------------------------

impl<S: 'static> List<S> {
    /// Clamp an offset into the scrollable range.
    fn clamp_offset(&self, to: f32) -> f32 {
        to.clamp(0.0, self.max_offset())
    }

    /// Whether the materialised window still covers everything the
    /// viewport shows. When it does, a scroll is one `SetTransform` and
    /// the rows are not touched at all — which is the whole reason the
    /// two spare rows exist.
    fn window_covers_view(&self) -> bool {
        if self.ring == 0 {
            return false;
        }
        let first = self.first_visible();
        let last = (first + self.rows_that_fit()).min(self.model.len());
        first >= self.anchor && last <= self.anchor + self.ring
    }

    /// Where the materialised window should start for the current
    /// offset: at the first visible row, pulled back so the window never
    /// runs off the end of a model shorter than it.
    fn want_anchor(&self) -> usize {
        let last_start = self.model.len().saturating_sub(self.ring);
        self.first_visible().min(last_start)
    }

    /// The transform that puts the content at the current offset.
    fn content_transform(&self) -> nitro_core::Transform {
        nitro_core::Transform::translate(0.0, -self.offset)
    }

    /// Scroll to `to`, and say whether the rows have to be re-emitted.
    ///
    /// Returns `false` when nothing moved at all, so a wheel at the end
    /// of the list sends nothing.
    fn scroll_to_offset(&mut self, ui: &mut Ui<S>, id: crate::WidgetId, to: f32) -> bool {
        let to = self.clamp_offset(to);
        if to.to_bits() == self.offset.to_bits() {
            return false;
        }
        self.offset = to;
        ui.set_slot_transform(id, VIEWPORT, self.content_transform());
        true
    }

    /// Bring row `index` inside the viewport, scrolling the least that
    /// does it. Returns whether the offset changed.
    fn reveal(&mut self, ui: &mut Ui<S>, id: crate::WidgetId, index: usize) -> bool {
        if self.row_h <= 0.0 {
            return false;
        }
        let top = index as f32 * self.row_h;
        let bottom = top + self.row_h;
        let to = if top < self.offset {
            top
        } else if bottom > self.offset + self.view_h {
            bottom - self.view_h
        } else {
            return false;
        };
        self.scroll_to_offset(ui, id, to)
    }

    /// Run `on_select` with the cursor's index, taking the callback out
    /// for the call the way a button's `on_click` is taken out: it is
    /// handed the whole tree, and that includes this widget.
    fn fire_select(&mut self, cx: &mut EventCx<'_, S>) {
        let Some(cb) = self.on_select.take() else {
            return;
        };
        cb(cx.state, cx.ui, self.cursor);
        self.on_select = Some(cb);
    }

    /// Run `on_activate` for `index`.
    fn fire_activate(&mut self, cx: &mut EventCx<'_, S>, index: usize) {
        cx.report_activation();
        let Some(cb) = self.on_activate.take() else {
            return;
        };
        cb(cx.state, cx.ui, index);
        self.on_activate = Some(cb);
    }

    /// Move the cursor to `index` and replace the selection with it (or
    /// extend the selection to it, for Shift).
    ///
    /// The repaint it asks for is a *row* repaint, not a re-emission:
    /// only the two rows whose selected-ness changed are re-drawn, and
    /// each of them costs one `SetFill`.
    fn move_cursor(&mut self, cx: &mut EventCx<'_, S>, index: usize, extend: bool) {
        if self.model.is_empty() {
            return;
        }
        let index = index.min(self.model.len() - 1);
        let moved = index != self.cursor;
        self.cursor = index;
        if extend {
            let (lo, hi) = if self.extend_from <= index {
                (self.extend_from, index)
            } else {
                (index, self.extend_from)
            };
            self.selected = (lo..=hi).collect();
        } else {
            self.extend_from = index;
            self.selected.clear();
            self.selected.insert(index);
        }
        let id = cx.id;
        self.reveal(cx.ui, id, index);
        cx.request_paint();
        if moved {
            self.fire_select(cx);
        }
    }

    /// Add `text` to the type-ahead prefix and jump to the first row
    /// that starts with it.
    ///
    /// The prefix expires by **elapsed time checked on the next key**
    /// rather than by a timer, because a list that armed a timer on
    /// every keystroke would wake the loop half a second after the user
    /// stopped typing, to do nothing. Idle is free, and it stays free.
    fn type_ahead(&mut self, cx: &mut EventCx<'_, S>, text: &str) -> bool {
        if text.is_empty() || text.chars().any(char::is_control) {
            return false;
        }
        let now = Instant::now();
        let stale = self
            .prefix_at
            .is_none_or(|at| now.duration_since(at) > TYPE_AHEAD_GAP);
        if stale {
            self.prefix.clear();
        }
        self.prefix.push_str(&text.to_lowercase());
        self.prefix_at = Some(now);
        // Search from the row after the cursor and wrap, so typing the
        // same letter twice walks the matches rather than sticking on
        // the first one.
        let len = self.model.len();
        let from = if stale { self.cursor + 1 } else { self.cursor };
        let hit = (0..len).map(|k| (from + k) % len.max(1)).find(|i| {
            self.model
                .row(*i)
                .text
                .to_lowercase()
                .starts_with(&self.prefix)
        });
        match hit {
            Some(i) => {
                self.move_cursor(cx, i, false);
                true
            }
            None => false,
        }
    }

    /// Handle one key press. Split out of `event` because a widget's
    /// event method that handles keys, wheels and clicks in one function
    /// is a function nobody reads to the end.
    fn key(&mut self, cx: &mut EventCx<'_, S>, k: &crate::KeyEvent) -> Handled {
        let extend = k.mods & mods::MASK == mods::SHIFT;
        let page = self.rows_that_fit().max(1);
        let last = self.model.len().saturating_sub(1);
        let to = match k.keycode {
            key::DOWN => self.cursor.saturating_add(1).min(last),
            key::UP => self.cursor.saturating_sub(1),
            key::PAGE_DOWN => self.cursor.saturating_add(page).min(last),
            key::PAGE_UP => self.cursor.saturating_sub(page),
            key::HOME => 0,
            key::END => last,
            key::ENTER => {
                if self.model.is_empty() {
                    return Handled::No;
                }
                let index = self.cursor;
                self.fire_activate(cx, index);
                return Handled::Yes;
            }
            key::SPACE if k.mods & mods::MASK == mods::CTRL => {
                // Ctrl-Space toggles the cursor's row without moving it,
                // which is the keyboard half of a Ctrl-click.
                if !self.selected.remove(&self.cursor) {
                    self.selected.insert(self.cursor);
                }
                cx.request_paint();
                return Handled::Yes;
            }
            _ => return Handled::No,
        };
        if self.model.is_empty() {
            return Handled::No;
        }
        self.move_cursor(cx, to, extend);
        Handled::Yes
    }

    /// Handle a press inside the list: select the row under the
    /// pointer, and activate it on the second click.
    ///
    /// A click carries no modifier mask — [`Event::PointerDown`] has a
    /// position and a button and nothing else — so Ctrl-click and
    /// Shift-click are *not* the pointer half of the multi-selection
    /// the keyboard has. Putting the mask on every pointer event to
    /// give one widget two more gestures is a protocol change, and it
    /// is not this widget's to make; the limitation is recorded in
    /// `docs/ui.md`.
    fn press(&mut self, cx: &mut EventCx<'_, S>, pos: nitro_core::Point) -> Handled {
        cx.request_focus();
        if self.row_h <= 0.0 || self.model.is_empty() {
            return Handled::Yes;
        }
        let row = ((pos.y + self.offset) / self.row_h).floor().max(0.0) as usize;
        if row >= self.model.len() {
            return Handled::Yes;
        }
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(r, at)| r == row && now.duration_since(at) < DOUBLE_CLICK);
        self.last_click = Some((row, now));
        self.move_cursor(cx, row, false);
        if double {
            self.fire_activate(cx, row);
        }
        Handled::Yes
    }
}

// -- painting ---------------------------------------------------------

/// The colours one paint of a list draws with, resolved from the theme
/// once rather than per row.
///
/// Compared between paints as well as used by them: a row whose *style*
/// changed is not an unchanged row, however unchanged its text is. See
/// [`List::paint`].
#[derive(Clone, PartialEq)]
struct RowPaint {
    style: TextStyle,
    text: Color,
    detail: Color,
    selection: Color,
}

impl<S: 'static> List<S> {
    /// Emit (or keep) one materialised row.
    ///
    /// Three answers, and the middle one is the point: everything, the
    /// background only (the selection moved), or nothing at all.
    fn paint_row(
        &self,
        cx: &mut PaintCx<'_, S>,
        group: nitro_wire::types::NodeId,
        index: usize,
        paint: &RowPaint,
    ) {
        let slot = ROW_BASE + (index % self.ring) as Slot * SLOTS_PER_ROW;
        let selected = self.selected.contains(&index);
        let cached = self.cache.get(index % self.ring).copied().flatten();
        let same_row = cached.is_some_and(|c| c.row == index && c.generation == self.generation);
        let unchanged =
            same_row && cached.is_some_and(|c| c.selected == selected) && !self.dirty_rows;
        if unchanged {
            for k in 0..SLOTS_PER_ROW {
                cx.keep(slot + k);
            }
            return;
        }
        let y = index as f32 * self.row_h;
        let fill = if selected {
            Fill::Solid(paint.selection)
        } else {
            // Transparent rather than omitted: the rect is also what
            // makes the row hit-testable, and a slot that comes and goes
            // would cost a CreateNode every time the selection moved.
            Fill::Solid(Color::TRANSPARENT)
        };
        cx.rect_in(
            group,
            slot,
            Rect::new(0.0, y, self.view_w, self.row_h),
            fill,
            0.0,
            (0.0, Color::TRANSPARENT),
        );
        if same_row && !self.dirty_rows {
            // Only the selection changed: the row's strings and their
            // boxes are identical, so keeping them is one `SetFill` for
            // the whole row.
            for k in 1..SLOTS_PER_ROW {
                cx.keep(slot + k);
            }
            return;
        }
        self.paint_row_text(cx, group, slot, index, y, paint);
    }

    /// The three text nodes of a row: glyph, primary, secondary.
    fn paint_row_text(
        &self,
        cx: &mut PaintCx<'_, S>,
        group: nitro_wire::types::NodeId,
        slot: Slot,
        index: usize,
        top_y: f32,
        paint: &RowPaint,
    ) {
        let row = self.model.row(index);
        let top = top_y + ((self.row_h - paint.style.size_px) / 2.0).max(0.0);
        let line = self.line_h.max(paint.style.size_px);
        let has_icon = row.icon.is_some();
        if let Some(icon) = &row.icon {
            cx.text_in(
                group,
                slot + 1,
                Rect::new(ROW_PAD, top, ICON_W, line),
                icon,
                TextRun::new(&paint.style, paint.detail),
            );
        }
        let left = ROW_PAD + if has_icon { ICON_W + 4.0 } else { 0.0 };
        // The secondary column is a third of the row, capped: a date is
        // a fixed width and a name is not, so the name gets the slack.
        let detail_w = if row.detail.is_empty() {
            0.0
        } else {
            (self.view_w * 0.34).min(190.0)
        };
        let text_w = (self.view_w - ROW_PAD - detail_w - left).max(0.0);
        cx.text_in(
            group,
            slot + 2,
            Rect::new(left, top, text_w, line),
            &row.text,
            TextRun::new(&paint.style, paint.text),
        );
        if detail_w > 0.0 {
            cx.text_in(
                group,
                slot + 3,
                Rect::new(self.view_w - ROW_PAD - detail_w, top, detail_w, line),
                &row.detail,
                TextRun::new(&paint.style, paint.detail).align(Align::Right),
            );
        }
    }
}

impl<S: 'static> Widget<S> for List<S> {
    /// A list takes the space it is offered.
    ///
    /// It is the one widget in this crate whose intrinsic size is not a
    /// function of its content, and deliberately so: a hundred thousand
    /// rows' worth of natural height is not an answer any parent can
    /// use, and asking for it is what the virtualisation exists to
    /// avoid. The one measurement it does need is a line of text, for
    /// the row height, and the toolkit's cache answers it without a
    /// round trip after the first.
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let theme = cx.theme();
        let style = TextStyle::from_theme(theme);
        let line = cx
            .measure_text("Xg", &style, 0.0)
            .unwrap_or_default()
            .height
            .max(style.size_px);
        self.line_h = line;
        let row_h = (line + 8.0).ceil();
        if row_h.to_bits() != self.row_h.to_bits() {
            self.row_h = row_h;
            self.dirty_rows = true;
        }
        let w = if constraints.max.w.is_finite() {
            constraints.max.w
        } else {
            240.0
        };
        let h = if constraints.max.h.is_finite() {
            constraints.max.h
        } else {
            self.content_height()
        };
        constraints.constrain(Size::new(w, h))
    }

    /// Size the ring to the viewport. A resize is the one thing that
    /// changes how many rows are materialised, and it re-emits them all
    /// because their text boxes are cut from the width.
    fn layout(&mut self, _cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        if bounds.w.to_bits() != self.view_w.to_bits() {
            self.view_w = bounds.w;
            self.dirty_rows = true;
        }
        self.view_h = bounds.h;
        let want = self.rows_that_fit() + SPARE_ROWS;
        if want != self.ring {
            self.ring = want;
            self.cache.clear();
            self.cache.resize(want, None);
            self.dirty_rows = true;
        }
        self.offset = self.clamp_offset(self.offset);
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let theme = cx.theme();
        let paint = RowPaint {
            style: TextStyle::from_theme(theme),
            text: theme.text,
            detail: theme.text_disabled,
            selection: theme.selection,
        };
        // A colour or font change invalidates every materialised row,
        // and nothing else here would notice: the per-slot cache keys on
        // the row index, the generation and the selection, all three of
        // which are unchanged when the desktop switches scheme. See
        // `painted_with`.
        if self.painted_with.as_ref() != Some(&paint) {
            self.dirty_rows = true;
            self.painted_with = Some(paint.clone());
        }
        // A widget that paints nothing is not hit-tested by the server,
        // so an empty list would never see a click. One rect fixes that
        // and costs one node.
        let bounds = cx.bounds;
        cx.fill_rect(BACKGROUND, bounds, Color::TRANSPARENT);
        if self.ring == 0 || self.row_h <= 0.0 {
            return;
        }
        let group = cx.group(VIEWPORT, bounds, true, self.content_transform());
        if group.is_none() {
            return;
        }
        self.anchor = self.want_anchor();
        let end = (self.anchor + self.ring).min(self.model.len());
        for i in self.anchor..end {
            self.paint_row(cx, group, i, &paint);
        }
        // Record what each slot now holds, so the next paint can answer
        // "unchanged" without deriving a single row.
        for slot in &mut self.cache {
            *slot = None;
        }
        for i in self.anchor..end {
            self.cache[i % self.ring] = Some(RowCache {
                row: i,
                generation: self.generation,
                selected: self.selected.contains(&i),
            });
        }
        self.dirty_rows = false;
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        match ev {
            Event::Scroll { dy, .. } => {
                let to = self.offset - dy * self.speed * self.row_h;
                let id = cx.id;
                if !self.scroll_to_offset(cx.ui, id, to) {
                    // Consumed even at the end, so a gesture does not
                    // suddenly start scrolling an enclosing viewport.
                    return Handled::Yes;
                }
                if !self.window_covers_view() {
                    cx.request_paint();
                }
                Handled::Yes
            }
            Event::KeyDown(k) => self.key(cx, k),
            Event::Text { text } => Handled::from(self.type_ahead(cx, text)),
            Event::PointerDown { pos, button } if *button == button::LEFT => self.press(cx, *pos),
            Event::FocusChanged { .. } => {
                // The selection is drawn the same focused or not — a
                // list that lost its highlight when the window lost
                // focus would be a repaint of every selected row for no
                // information. Nothing to do.
                Handled::No
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::List
    }

    /// The **visible** rows, one per line, tab-separated within a row.
    ///
    /// `hey <app> get <list> text` is how a script reads a list, and the
    /// honest answer is the window into the model rather than the model:
    /// a hundred thousand rows down a socket is not a value anybody
    /// wanted, and the widget genuinely does not draw them.
    fn accessible(&self) -> Access {
        let rows: Vec<String> = self
            .visible_rows()
            .into_iter()
            .map(|(_, r)| {
                if r.detail.is_empty() {
                    r.text
                } else {
                    format!("{}\t{}", r.text, r.detail)
                }
            })
            .collect();
        Access {
            name: None,
            value: Some(rows.join("\n")),
            actions: vec!["activate", "select", "scroll_to", "scroll_by", "focus"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        let index = arg.and_then(|a| a.trim().parse::<usize>().ok());
        match action {
            "activate" | "click" => {
                let i = index.unwrap_or(self.cursor);
                if i >= self.model.len() {
                    return Handled::No;
                }
                if index.is_some() {
                    self.move_cursor(cx, i, false);
                }
                self.fire_activate(cx, i);
                Handled::Yes
            }
            "select" | "set_value" | "set_cursor" => {
                let Some(i) = index else {
                    return Handled::No;
                };
                if i >= self.model.len() {
                    return Handled::No;
                }
                self.move_cursor(cx, i, false);
                Handled::Yes
            }
            "scroll_to" | "scroll_by" => {
                let Some(v) = arg.and_then(|a| a.trim().parse::<f32>().ok()) else {
                    return Handled::No;
                };
                let to = if action == "scroll_by" {
                    self.offset + v
                } else {
                    v
                };
                let id = cx.id;
                if self.scroll_to_offset(cx.ui, id, to) && !self.window_covers_view() {
                    cx.request_paint();
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// Setters for a live [`List`].
impl<S: 'static> WidgetMut<'_, List<S>, S> {
    /// Replace the rows.
    ///
    /// One `u64` bump invalidates every materialised slot, so the next
    /// paint re-emits the screenful it holds and nothing else — a
    /// directory refresh costs a screenful, not a model.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.set_model(Box::new(rows));
    }

    /// Replace the model with any [`ListModel`].
    pub fn set_model(&mut self, model: Box<dyn ListModel>) {
        self.model = model;
        self.generation = self.generation.wrapping_add(1);
        self.dirty_rows = true;
        let len = self.model.len();
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.selected.retain(|i| *i < len);
        self.extend_from = self.cursor;
        let max = self.max_offset();
        if self.offset > max {
            self.offset = max;
            let id = self.id();
            let t = nitro_core::Transform::translate(0.0, -max);
            self.ui().set_slot_transform(id, VIEWPORT, t);
        }
        self.request_paint();
    }

    /// Move the cursor and make it the whole selection, scrolling it
    /// into view. Does not fire `on_select`: an app that set the
    /// selection itself already knows.
    pub fn select(&mut self, index: usize) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let index = index.min(len - 1);
        self.cursor = index;
        self.extend_from = index;
        self.selected.clear();
        self.selected.insert(index);
        self.scroll_to_row(index);
        self.request_paint();
    }

    /// Scroll until row `index` is inside the viewport.
    pub fn scroll_to_row(&mut self, index: usize) {
        let row_h = self.row_h;
        if row_h <= 0.0 {
            return;
        }
        let top = index as f32 * row_h;
        let bottom = top + row_h;
        let (offset, view) = (self.offset, self.view_h);
        let to = if top < offset {
            top
        } else if bottom > offset + view {
            bottom - view
        } else {
            return;
        };
        self.scroll_to(to);
    }

    /// Scroll to an absolute offset, clamped. **One `SetTransform` and
    /// nothing else** when the visible set does not change.
    pub fn scroll_to(&mut self, offset: f32) {
        let to = self.clamp_offset(offset);
        if to.to_bits() == self.offset.to_bits() {
            return;
        }
        self.offset = to;
        let id = self.id();
        let t = nitro_core::Transform::translate(0.0, -to);
        self.ui().set_slot_transform(id, VIEWPORT, t);
        if !self.window_covers_view() {
            self.request_paint();
        }
    }

    /// Replace the activation callback (Enter, double-click, `do …
    /// activate`).
    pub fn set_on_activate(&mut self, f: impl Fn(&mut S, &mut Ui<S>, usize) + 'static) {
        self.on_activate = Some(Box::new(f));
    }

    /// Replace the selection callback.
    pub fn set_on_select(&mut self, f: impl Fn(&mut S, &mut Ui<S>, usize) + 'static) {
        self.on_select = Some(Box::new(f));
    }

    /// How many rows one wheel notch scrolls.
    pub fn set_speed(&mut self, rows: f32) {
        self.speed = rows;
    }
}

/// Builder for a [`List`].
pub struct ListBuilder<S> {
    built: Built<S>,
    list: List<S>,
}

impl<S: 'static> ListBuilder<S> {
    /// The rows to start with.
    #[must_use]
    pub fn rows(mut self, rows: Vec<Row>) -> Self {
        self.list.model = Box::new(rows);
        self
    }

    /// Start from any [`ListModel`].
    #[must_use]
    pub fn model(mut self, model: Box<dyn ListModel>) -> Self {
        self.list.model = model;
        self
    }

    /// Called with the row index on Enter or a double-click.
    #[must_use]
    pub fn on_activate(mut self, f: impl Fn(&mut S, &mut Ui<S>, usize) + 'static) -> Self {
        self.list.on_activate = Some(Box::new(f));
        self
    }

    /// Called with the row index whenever the cursor moves.
    #[must_use]
    pub fn on_select(mut self, f: impl Fn(&mut S, &mut Ui<S>, usize) + 'static) -> Self {
        self.list.on_select = Some(Box::new(f));
        self
    }

    /// How many rows one wheel notch scrolls (default 3).
    #[must_use]
    pub fn speed(mut self, rows: f32) -> Self {
        self.list.speed = rows;
        self
    }
}

impl<S: 'static> StyleBuilder<S> for ListBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for ListBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.state_mut().focusable = true;
        self.built.replace_widget(self.list);
        self.built
    }
}

/// A virtualised list of rows.
///
/// The one widget in this crate that is **not** the size of its content,
/// so it is also the one that opts out of the default content shrink
/// floor ([`ShrinkFloor::Zero`]): showing fewer rows is exactly what a
/// list does when it is given less room, which is what virtualisation
/// is for. Every other widget would rather overflow than be squashed.
#[must_use]
pub fn list<S: 'static>() -> ListBuilder<S> {
    let mut built = Built::new(List::<S>::default());
    built.state_mut().style.shrink_floor = ShrinkFloor::Zero;
    ListBuilder {
        built,
        list: List::default(),
    }
}
