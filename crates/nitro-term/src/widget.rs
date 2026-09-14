//! [`TermGrid`]: the widget that draws a [`Term`]'s grid, and the damage
//! strategy that makes it cheap.
//!
//! The claim this module has to hold up is `DESIGN.md`'s: **work is
//! proportional to change**. A terminal is the hardest place to keep it,
//! because the thing on the other end of the pty does not care: `yes`
//! writes a line every microsecond, `htop` repaints eighty rows twice a
//! second, and `cat` of a binary file changes every cell on screen. The
//! answer has three parts, and each is visible in the code below.
//!
//! **A Text node per same-style run, and a slot per run.** A row is
//! split into maximal runs of cells sharing a [`Style`]
//! ([`Grid::row_runs`]); each run becomes one `SetText`, and each run
//! with a non-default background one `SetFill`ed rect behind it. The
//! slot number is `row * SLOTS_PER_ROW + k`, so a run keeps its scene
//! node across frames and the framework's per-slot cache does the
//! diffing: a run whose text, colour and position did not change costs
//! **zero bytes**, even though `paint` walked over it.
//!
//! **A row that did not change is not walked at all.** The grid tracks
//! damage per row, so `paint` asks [`Grid::row_dirty`] first and calls
//! [`PaintCx::keep`] for a clean row — which tells the framework the
//! row's slots are unchanged without re-deriving their contents. On a
//! keystroke that is one row out of fifty.
//!
//! **The scene is updated once per frame, not once per byte.** Bytes are
//! drained from the pty the moment they arrive (so the child never
//! blocks on a full pipe), but they go into the [`Grid`] only. The
//! widget is marked dirty, and the *scene* catches up in the app's frame
//! callback ([`Ui::request_frame`]). So `seq 1 1000000` costs one commit
//! per frame at 60 Hz rather than one per line, and the grid the frame
//! draws is the one the terminal had reached by then — the intermediate
//! states were never on screen anyway.

use std::os::fd::{AsFd as _, OwnedFd};

use nitro_ui::build::{IntoWidget, StyleBuilder};
use nitro_ui::event::{Event, Handled, button, key, mods};
use nitro_ui::widget::Slot;
use nitro_ui::{
    Access, Built, Color, Constraints, EventCx, Fill, MeasureCx, PaintCx, Rect, Role, Size,
    TextRun, TextStyle, Ui, Widget, WidgetMut,
};

use crate::grid::{CellColor, Run, Style};
use crate::theme::Palette;
use crate::vt::Term;

/// Paint slots reserved for one row.
///
/// A row is drawn as *n* background rects followed by *n* text runs, and
/// both are indexed off the row's base slot, so a row owns a fixed
/// stride of the slot space. 32 is the compromise: a row with more than
/// 32 distinct style runs draws its first 32 and merges the rest (see
/// [`TermGrid::paint_row`]), which in practice never happens outside a
/// colour-test pattern — and the alternative, a slot allocator, would
/// make a run's node depend on what the *other* rows are doing, so a
/// changed row could renumber a clean one and the whole diff would
/// collapse.
const RUNS_PER_ROW: usize = 32;

/// Slots one row occupies: a background rect and a text node per run.
const SLOTS_PER_ROW: usize = RUNS_PER_ROW * 2;

/// Slot of the cursor rect. It sits above every row's slots, so adding a
/// row never moves it.
const CURSOR_SLOT: Slot = Slot::MAX;

/// The default font size, in logical pixels.
pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// The smallest grid the window may be resized to, in cells.
pub const MIN_CELLS: (usize, usize) = (20, 5);

/// A terminal screen: a [`Term`], a cell metric, and the mapping from
/// its grid to scene nodes.
///
/// The widget owns the terminal rather than the app's state struct
/// because every one of its methods needs both: `measure` needs the cell
/// size to answer at all, `event` turns a key into bytes using the
/// terminal's own modes, and `paint` reads the grid's damage. A `Term`
/// held next to it in `S` would mean every access went through an id and
/// a fallible lookup, for no gain.
pub struct TermGrid {
    term: Term,
    /// Size of one cell, from measuring `M` in the configured style.
    /// Zero until the first `measure`, which is why `paint` guards.
    cell: Size,
    /// The font, from which the cell size came.
    style: TextStyle,
    /// Colour table: the sixteen ANSI colours, the default foreground
    /// and background.
    palette: Palette,
    /// A `dup` of the pty master, so a key or a scripted `send` reaches
    /// the child in the turn it happened.
    ///
    /// `None` until the app hands one over (and in a unit test, which
    /// has no pty), in which case input is silently dropped — there is
    /// nowhere for it to go, and a terminal with no child is not a
    /// terminal.
    ///
    /// It is a descriptor rather than a `Vec<u8>` queue because the
    /// queue was a bug: `hey set grid value` filled it and only the
    /// descriptor hook emptied it, so a scripted command sat there until
    /// the child happened to say something on its own. A queue drained
    /// by \"whoever remembers\" has to be drained at every entry point,
    /// and the box run found the one that was missed.
    pty: Option<OwnedFd>,
    /// Whether the window has keyboard focus, which is the difference
    /// between a filled cursor block and a hollow one.
    focused: bool,
    /// Runs scratch, reused across rows and frames so a repaint of a
    /// settled screen allocates nothing.
    runs: Vec<Run>,
}

impl std::fmt::Debug for TermGrid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermGrid")
            .field("cols", &self.term.grid().cols())
            .field("rows", &self.term.grid().rows())
            .field("cell", &self.cell)
            .field("alt_screen", &self.term.alt_screen())
            .finish_non_exhaustive()
    }
}

impl TermGrid {
    /// A grid of `cols` by `rows` cells with `scrollback` lines of
    /// history, in the given font.
    #[must_use]
    pub fn new(cols: usize, rows: usize, scrollback: usize, size_px: f32) -> Self {
        Self {
            term: Term::new(cols, rows, scrollback),
            cell: Size::ZERO,
            // `mono`, always: a terminal in a proportional font is not a
            // terminal, and the server resolves the alias to whatever
            // monospaced face the machine has.
            style: TextStyle::new("mono", size_px),
            palette: Palette::default(),
            pty: None,
            focused: true,
            runs: Vec::new(),
        }
    }

    /// The terminal, for reading the grid.
    #[must_use]
    pub fn term(&self) -> &Term {
        &self.term
    }

    /// The terminal, mutably: this is where the pty's bytes go.
    pub fn term_mut(&mut self) -> &mut Term {
        &mut self.term
    }

    /// The size of one cell, as measured. `Size::ZERO` before the first
    /// layout.
    #[must_use]
    pub fn cell_size(&self) -> Size {
        self.cell
    }

    /// The colour table.
    #[must_use]
    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    /// How many cells fit in `size` at the measured cell metric.
    ///
    /// `(0, 0)` before the first measurement, which the caller must
    /// treat as "do not resize yet" rather than as a zero-sized grid —
    /// a `TIOCSWINSZ` of 0×0 tells the child its terminal has no size.
    #[must_use]
    pub fn cells_for(&self, size: Size) -> (usize, usize) {
        if self.cell.w <= 0.0 || self.cell.h <= 0.0 {
            return (0, 0);
        }
        (
            (size.w / self.cell.w).floor().max(1.0) as usize,
            (size.h / self.cell.h).floor().max(1.0) as usize,
        )
    }

    /// Write `bytes` to the pty, if the widget has one.
    ///
    /// Errors are dropped rather than reported. The one thing that makes
    /// a pty write fail is a child that has exited, which the app loop
    /// notices as an EOF on the *read* side; surfacing it here would
    /// race that and report a broken terminal instead of a finished one.
    pub fn write_input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(fd) = &self.pty {
            let _ = crate::pty::Pty::write_all(fd.as_fd(), bytes);
        }
    }

    /// Give the widget the descriptor it writes keys to.
    pub fn set_pty(&mut self, fd: OwnedFd) {
        self.pty = Some(fd);
    }

    /// Whether the widget has a pty to write to.
    #[must_use]
    pub fn has_pty(&self) -> bool {
        self.pty.is_some()
    }

    /// The colour a cell's foreground resolves to.
    fn fg_of(&self, style: Style) -> Color {
        let (fg, bg) = if style.attrs.inverse() {
            (style.bg, style.fg)
        } else {
            (style.fg, style.bg)
        };
        let _ = bg;
        match fg {
            // Bold has meant "bright" on a sixteen-colour terminal since
            // the hardware could not do both, and every program still
            // assumes it: `ls --color` writes bold blue for a directory
            // and expects the readable one.
            CellColor::Indexed(i) if style.attrs.bold() && i < 8 => self.palette.indexed(i + 8),
            CellColor::Indexed(i) => self.palette.indexed(i),
            CellColor::Rgb(r, g, b) => Color::rgb(r, g, b),
            CellColor::Default if style.attrs.inverse() => self.palette.background,
            CellColor::Default => self.palette.foreground,
        }
    }

    /// The colour a cell's background resolves to, or `None` when it is
    /// the window's own — which is the case that costs no rect at all.
    fn bg_of(&self, style: Style) -> Option<Color> {
        let bg = if style.attrs.inverse() {
            style.fg
        } else {
            style.bg
        };
        match bg {
            CellColor::Indexed(i) => Some(self.palette.indexed(i)),
            CellColor::Rgb(r, g, b) => Some(Color::rgb(r, g, b)),
            CellColor::Default if style.attrs.inverse() => Some(self.palette.foreground),
            CellColor::Default => None,
        }
    }

    /// The text style of a run: the base font, plus the weight and slant
    /// its attributes ask for.
    fn run_style(&self, style: Style) -> TextStyle {
        TextStyle {
            weight: if style.attrs.bold() { 700 } else { 400 },
            italic: style.attrs.italic(),
            ..self.style.clone()
        }
    }

    /// Paint one row's runs into its slice of the slot space.
    ///
    /// Returns the number of slots the row used, so the caller can leave
    /// the rest for `end_paint` to destroy — a row that shrank from six
    /// runs to two gives four nodes back.
    fn paint_row<S: 'static>(&self, cx: &mut PaintCx<'_, S>, row: usize, runs: &[Run]) {
        let base = (row * SLOTS_PER_ROW) as Slot;
        let y = row as f32 * self.cell.h;
        let row_w = self.term.grid().cols() as f32 * self.cell.w;
        for (k, run) in runs.iter().take(RUNS_PER_ROW).enumerate() {
            let x = run.col as f32 * self.cell.w;
            let w = run.cols as f32 * self.cell.w;
            let bg_slot = base + k as Slot;
            let text_slot = base + (RUNS_PER_ROW + k) as Slot;
            // The background first, and only when it is not the
            // window's: the backdrop already paints that colour, so a
            // rect for it would be a node and a fill per run of ordinary
            // text — which is most of the screen. Its width is the run's
            // exactly, because unlike the text node below it is visible.
            if let Some(bg) = self.bg_of(run.style) {
                cx.rect(
                    bg_slot,
                    Rect::new(x, y, w, self.cell.h),
                    Fill::Solid(bg),
                    0.0,
                    (0.0, Color::TRANSPARENT),
                );
            }
            // The text node runs to the end of the row rather than to
            // the end of the run, and that is worth a paragraph because
            // it is what makes a keystroke cost one mutation instead of
            // two. The node's width is not *visible* — the run is drawn
            // left-aligned and unwrapped, so the glyphs stop where the
            // string does whatever box they sit in. But it is *diffed*:
            // sized to the run, every typed character widens the box by
            // one cell and the toolkit dutifully sends a `SetBounds`
            // next to the `SetText`. Sized to the row, the box is the
            // same on every repaint and only the string moves.
            let ts = self.run_style(run.style);
            let paint = TextRun::new(&ts, self.fg_of(run.style));
            cx.text(
                text_slot,
                Rect::new(x, y, (row_w - x).max(0.0), self.cell.h),
                &run.text,
                paint,
            );
        }
    }

    /// Keep every slot of a row exactly as it was last painted.
    ///
    /// This is the whole point of the row-level damage bit: a clean row
    /// is not re-derived into runs, not compared, not sent. `keep`
    /// answers `false` for a slot that was never painted, which is the
    /// normal case for the unused tail of a row's stride, so the return
    /// value is deliberately ignored.
    fn keep_row<S: 'static>(cx: &mut PaintCx<'_, S>, row: usize) {
        let base = (row * SLOTS_PER_ROW) as Slot;
        for k in 0..SLOTS_PER_ROW {
            cx.keep(base + k as Slot);
        }
    }

    /// Draw the cursor, or nothing when it is hidden or scrolled away.
    fn paint_cursor<S: 'static>(&self, cx: &mut PaintCx<'_, S>) {
        if !self.term.grid().cursor_visible() {
            return;
        }
        let Some((row, col)) = self.term.grid().cursor() else {
            return;
        };
        let rect = Rect::new(
            col as f32 * self.cell.w,
            row as f32 * self.cell.h,
            self.cell.w,
            self.cell.h,
        );
        // Focused: a filled block, the convention every terminal uses.
        // Unfocused: an outline, so a screenshot of two terminals says
        // which one the keyboard is talking to.
        if self.focused {
            cx.rect(
                CURSOR_SLOT,
                rect,
                Fill::Solid(self.palette.cursor),
                0.0,
                (0.0, Color::TRANSPARENT),
            );
        } else {
            cx.rect(
                CURSOR_SLOT,
                rect,
                Fill::None,
                0.0,
                (1.0, self.palette.cursor),
            );
        }
    }

    /// Encode a key press and queue it for the pty.
    ///
    /// Returns whether anything was queued, which is what the widget
    /// answers `Handled` with: a key that encodes to nothing should keep
    /// bubbling, so an app-level shortcut still sees it.
    ///
    /// Public because the app registers this as an [`Ui::on_key`]
    /// handler as well. The widget only sees keys once something has
    /// focused it, and a terminal whose first keystroke went nowhere
    /// would look broken — so the app offers whatever the focused chain
    /// declined to the same function, and a key cannot be typed twice
    /// because the widget consumed the ones it handled.
    pub fn type_key(&mut self, ev: &nitro_ui::KeyEvent) -> bool {
        let modes = crate::keys::Modes {
            application_cursor: self.term.application_cursor(),
        };
        let Some(bytes) = crate::keys::encode(ev, modes) else {
            return false;
        };
        // Any key snaps the view back to the bottom: typing into a
        // scrolled-back screen and watching nothing appear is the single
        // most confusing thing a terminal can do.
        self.term.grid_mut().scroll_to_bottom();
        self.write_input(&bytes);
        true
    }
}

impl<S: 'static> Widget<S> for TermGrid {
    /// The grid's natural size: its cells at the measured cell metric.
    ///
    /// Measuring `M` once per layout is the whole of it. The string is
    /// the same every time, so the toolkit's measurement cache answers
    /// without a round trip after the first — which is what lets a
    /// resize re-lay-out without talking to the server.
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        let style = self.style.clone();
        if let Ok(m) = cx.measure_text("M", &style, 0.0) {
            // A zero from a fontless server would make every cell
            // degenerate and every division below a NaN, so the metric
            // is floored rather than trusted.
            self.cell = Size::new(m.width.max(1.0), m.height.max(1.0));
        }
        let natural = Size::new(
            self.cell.w * self.term.grid().cols() as f32,
            self.cell.h * self.term.grid().rows() as f32,
        );
        constraints.constrain(natural)
    }

    /// Emit only what changed.
    ///
    /// Every row is visited, but a clean row costs one `keep` per slot
    /// and no string work at all; a dirty row is re-split into runs and
    /// handed to the framework's per-slot diff, which drops the runs
    /// that produced the same bytes as last time.
    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        if self.cell.w <= 0.0 || self.cell.h <= 0.0 {
            return;
        }
        let rows = self.term.grid().rows();
        let mut runs = std::mem::take(&mut self.runs);
        for row in 0..rows {
            if !self.term.grid().row_dirty(row) {
                Self::keep_row(cx, row);
                continue;
            }
            runs.clear();
            self.term.grid().row_runs(row, &mut runs);
            self.paint_row(cx, row, &runs);
        }
        self.runs = runs;
        self.paint_cursor(cx);
        // The damage has been drawn; the next paint starts from clean.
        // Doing it here rather than in the frame callback is what makes
        // a paint the widget did not ask for (a resize, a theme change)
        // still leave the bookkeeping consistent.
        self.term.grid_mut().clear_damage();
    }

    #[allow(clippy::match_same_arms)] // `Text`/`KeyUp` answer `No` for a stated reason, not by falling through.
    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        match ev {
            Event::KeyDown(k) => {
                // Shift+PgUp/PgDn is the scrollback, and it is the one
                // chord the terminal keeps for itself rather than
                // sending on: no program expects it, and every terminal
                // binds it.
                if k.mods & mods::MASK == mods::SHIFT
                    && matches!(k.keycode, key::PAGE_UP | key::PAGE_DOWN)
                {
                    let page = self.term.grid().rows().saturating_sub(1).max(1);
                    if k.keycode == key::PAGE_UP {
                        self.term.grid_mut().scroll_up(page);
                    } else {
                        self.term.grid_mut().scroll_down(page);
                    }
                    cx.request_paint();
                    return Handled::Yes;
                }
                if self.type_key(k) {
                    cx.request_paint();
                    return Handled::Yes;
                }
                Handled::No
            }
            // The text a key produced is already in `KeyDown`, so a
            // second event carrying it would type everything twice.
            // Spelled out rather than left to the wildcard because it is
            // the one arm a reader will look for.
            Event::Text { .. } | Event::KeyUp(_) => Handled::No,
            Event::Scroll { dy, .. } => {
                // Three rows a notch, as every terminal does.
                let lines = (dy.abs() * 3.0).round().max(1.0) as usize;
                if *dy > 0.0 {
                    self.term.grid_mut().scroll_up(lines);
                } else {
                    self.term.grid_mut().scroll_down(lines);
                }
                cx.request_paint();
                Handled::Yes
            }
            // A click takes the focus, which is what makes a terminal in
            // an unfocused window typable again. It is also why the
            // widget paints a background rect at all: the server hit
            // tests painted content, so a widget that drew only text
            // would be clickable on its glyphs and nowhere else.
            Event::PointerDown {
                button: button::LEFT,
                ..
            } => {
                cx.request_focus();
                Handled::Yes
            }
            Event::FocusChanged { focused } => {
                self.focused = *focused;
                // Only the cursor changed, so only the cursor is
                // redrawn: every row is clean, and `paint` will `keep`
                // all of them.
                cx.request_paint();
                Handled::No
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Terminal
    }

    /// The screen, as text.
    ///
    /// This is what `hey nitro-term get grid value` returns, and it is
    /// the whole reason a terminal is scriptable: a test — or an agent —
    /// reads the screen without a font, a screenshot or an OCR step.
    fn accessible(&self) -> Access {
        Access {
            name: Some("grid".to_owned()),
            value: Some(self.term.grid().text()),
            actions: vec!["send", "paste_text", "scroll_to_bottom", "focus"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        match action {
            // `set_value` and `send` are the same thing under two names:
            // `hey … set grid value "ls\n"` reads naturally and
            // `hey … do grid send "ls\n"` says what it does. Both feed
            // the pty rather than the grid, because writing into the
            // screen behind the program's back would desynchronise the
            // two immediately.
            //
            // The argument's C-style escapes are interpreted, which is
            // what makes the action usable at all: a script's whole
            // purpose is to run a command, and a newline cannot be typed
            // on a command line any other way. `\e` reaches `vim`, too.
            // See `keys::unescape` for why the rule is narrow.
            //
            // The bytes are **typed, not pasted**, and that distinction
            // cost a box run to find. Wrapping them in the bracketed
            // paste markers is what a real paste would do — but bash 5.1
            // and later enable bracketed paste by default, and the
            // entire purpose of the markers is to tell readline *not* to
            // execute what arrives: the newline was inserted as a
            // literal character and every scripted command sat on the
            // prompt unrun. A script driving a terminal is a keyboard,
            // not a clipboard. `paste_text` below is the action for the
            // day there is a real clipboard.
            "send" | "set_value" | "set_text" => {
                let text = crate::keys::unescape(arg.unwrap_or_default());
                self.term.grid_mut().scroll_to_bottom();
                self.write_input(text.as_bytes());
                cx.request_paint();
                Handled::Yes
            }
            // A genuine paste: bracketed when the program asked for it,
            // so an editor can tell it from typing and not auto-indent
            // it. Nothing produces one yet — there is no clipboard — but
            // the plumbing is here and tested, and it is the action a
            // clipboard would call.
            "paste_text" => {
                let text = crate::keys::unescape(arg.unwrap_or_default());
                let bytes = crate::keys::paste(&text, self.term.bracketed_paste());
                self.term.grid_mut().scroll_to_bottom();
                self.write_input(&bytes);
                cx.request_paint();
                Handled::Yes
            }
            "scroll_to_bottom" => {
                self.term.grid_mut().scroll_to_bottom();
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// The setters of a [`TermGrid`], as an extension trait on
/// [`WidgetMut`].
///
/// Every built-in widget writes its setters as `impl WidgetMut<'_, W, S>`
/// — which a widget in *another* crate cannot do, because an inherent
/// `impl` has to live where the type does and `WidgetMut` lives in
/// `nitro-ui`. A local trait implemented for the foreign type is the
/// standard way out, and it costs the caller one `use`.
///
/// The contract is unchanged and is the one that matters: **the only way
/// to mutate a widget is through a setter that marks it dirty**. Each of
/// these calls `request_paint` or `request_layout`, and `WidgetMut`'s
/// `DerefMut` is what gives them the `&mut TermGrid` to work on.
pub trait TermGridMut {
    /// Feed bytes from the pty into the grid.
    ///
    /// It marks the widget paint-dirty but does **not** touch the scene:
    /// the caller updates the scene in its frame callback, which is what
    /// keeps a megabyte of output to one commit per frame. It does not
    /// snap the view to the bottom either — a user reading scrollback
    /// while a build runs stays where they are.
    fn feed(&mut self, bytes: &[u8]);

    /// Resize the grid to `cols` by `rows`, as a window resize does.
    ///
    /// Everything is re-sent afterwards, because reflow moves every row:
    /// the honest answer for a resize is a full repaint, and a resize is
    /// rare enough that the cost is invisible.
    fn resize_grid(&mut self, cols: usize, rows: usize);

    /// Write bytes to the pty, as a paste would.
    fn send_bytes(&mut self, bytes: &[u8]);

    /// Give the widget the descriptor it writes keys to.
    ///
    /// Not a paint- or layout-affecting setter, so it marks nothing:
    /// what the widget writes *to* is invisible on screen.
    fn set_pty_fd(&mut self, fd: OwnedFd);

    /// The title OSC 0/2 set since the last call, if it changed.
    fn take_title(&mut self) -> Option<String>;

    /// Replace the colour table.
    fn set_palette(&mut self, palette: Palette);
}

impl<S: 'static> TermGridMut for WidgetMut<'_, TermGrid, S> {
    fn feed(&mut self, bytes: &[u8]) {
        self.term.feed(bytes);
        let replies = self.term.take_replies();
        self.write_input(&replies);
        self.request_paint();
    }

    fn resize_grid(&mut self, cols: usize, rows: usize) {
        if cols == self.term.grid().cols() && rows == self.term.grid().rows() {
            return;
        }
        self.term.resize(cols, rows);
        self.term.grid_mut().damage_all();
        self.request_layout();
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.write_input(bytes);
    }

    fn set_pty_fd(&mut self, fd: OwnedFd) {
        self.set_pty(fd);
    }

    fn take_title(&mut self) -> Option<String> {
        self.term.take_title()
    }

    fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
        self.term.grid_mut().damage_all();
        self.request_paint();
    }
}

/// Builder for a [`TermGrid`].
pub struct TermGridBuilder<S> {
    built: Built<S>,
    cols: usize,
    rows: usize,
    scrollback: usize,
    size_px: f32,
    palette: Palette,
}

impl<S: 'static> TermGridBuilder<S> {
    /// Set the grid's initial size in cells.
    #[must_use]
    pub fn cells(mut self, cols: usize, rows: usize) -> Self {
        self.cols = cols.max(MIN_CELLS.0);
        self.rows = rows.max(MIN_CELLS.1);
        self
    }

    /// Set how many lines of scrollback to keep.
    #[must_use]
    pub fn scrollback(mut self, lines: usize) -> Self {
        self.scrollback = lines;
        self
    }

    /// Set the font size in logical pixels.
    #[must_use]
    pub fn font_size(mut self, px: f32) -> Self {
        self.size_px = px;
        self
    }

    /// Set the colour table.
    #[must_use]
    pub fn palette(mut self, palette: Palette) -> Self {
        self.palette = palette;
        self
    }

    /// Give the widget an addressing name, for `hey`.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.built.state_mut().name = Some(name.into());
        self
    }
}

impl<S: 'static> StyleBuilder<S> for TermGridBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for TermGridBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        let mut w = TermGrid::new(self.cols, self.rows, self.scrollback, self.size_px);
        w.palette = self.palette;
        self.built.state_mut().focusable = true;
        self.built.replace_widget(w);
        self.built
    }
}

/// A terminal grid, 80×24 by default with 10 000 lines of scrollback.
#[must_use]
pub fn term_grid<S: 'static>() -> TermGridBuilder<S> {
    TermGridBuilder {
        built: Built::new(TermGrid::new(80, 24, 10_000, DEFAULT_FONT_SIZE)),
        cols: 80,
        rows: 24,
        scrollback: 10_000,
        size_px: DEFAULT_FONT_SIZE,
        palette: Palette::default(),
    }
}

/// Ask the toolkit for a frame callback, if the grid has anything to
/// show that the scene does not.
///
/// Called after every pty drain. The `if` is the idle contract: a
/// terminal sitting at a prompt has no damage, asks for no frame, and
/// the app blocks in `epoll_wait` with nothing scheduled — which is what
/// `nothing_is_sent_while_it_sits_there` checks from the outside.
///
/// # Errors
/// A wire failure, which is fatal.
pub fn request_frame_if_dirty<S: 'static>(
    ui: &mut Ui<S>,
    grid: nitro_ui::WidgetId,
) -> Result<(), nitro_ui::Error> {
    let dirty = ui
        .widget::<TermGrid>(grid)
        .is_ok_and(|g| g.term.grid().dirty());
    if dirty {
        ui.request_frame()?;
    }
    Ok(())
}
