//! The VT grid: cells, the screen, the scrollback ring and damage.
//!
//! This module is the whole model of "what the terminal looks like". It
//! knows nothing about escape sequences — [`vt`](crate::vt) parses those
//! and calls the operations at the bottom of this file — and nothing
//! about drawing. That split is what makes the grid testable: every
//! behaviour a terminal is judged on (deferred wrap, scroll regions,
//! scrollback, wide characters) is exercised here with plain method
//! calls, and the parser's tests only have to check that a byte sequence
//! reaches the right method.
//!
//! Three decisions are worth knowing before reading further.
//!
//! **Wrap is deferred.** Writing into the last column does not move the
//! cursor; it sets a pending-wrap flag that the *next* printable
//! character acts on. A real VT does this, and it is not a detail: a
//! program that fills the last column of the last row must not scroll,
//! or every full-width line of output would cost a blank one.
//!
//! **Damage is per row, as one column span.** A viewer asks
//! [`Grid::row_damage`] for the columns of a row that changed since it
//! last caught up. One span per row rather than a bitmap is the right
//! trade for a terminal: writes are overwhelmingly runs of adjacent
//! cells, and a span costs two `usize` per row instead of a bit per cell.
//!
//! **The scrollback is a ring of finished lines, not a resizable
//! history.** Lines pushed off the top of the primary screen land in it;
//! the alternate screen has none, because full-screen programs would
//! otherwise fill it with frames of themselves.
//!
//! **Erasing carries the background.** This is BCE, back colour erase:
//! the cells an erase leaves behind are painted with the pen's
//! background, not with the default one. It covers the operations a
//! program drives — ED, EL, ECH, ICH, DCH and every scroll (LF, RI, SU,
//! SD, IL, DL) — because that is what `tmux` assumes when it draws its
//! status line as `SGR 48;5;n` followed by an EL and then the labels:
//! without BCE the bar would stop where the text stops. It deliberately
//! does *not* cover resize, RIS or entering the alternate screen, where
//! the pen in force is an accident rather than an instruction. Only the
//! background survives an erase; see [`Style::erase`].

use std::collections::VecDeque;

// ---------------------------------------------------------------------
// Cells
// ---------------------------------------------------------------------

/// A colour as the VT names it.
///
/// The grid deliberately does not resolve indices to pixels: a palette is
/// a *theme* decision, and a repaint after a theme change must not have
/// to re-run the program that painted the screen. `Default` is a third
/// state rather than "index 7", so a viewer can tell "this cell asked for
/// the foreground colour" from "this cell asked for white".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellColor {
    /// The terminal's default foreground or background.
    #[default]
    Default,
    /// One of the 256 palette entries.
    Indexed(u8),
    /// A direct 24-bit colour, as `SGR 38;2;r;g;b` sets.
    Rgb(u8, u8, u8),
}

/// Character attributes, a bitmask.
///
/// A byte of flags rather than four `bool`s, because [`Style`] is
/// compared for equality once per cell when a row is split into runs and
/// copied around on every scroll; keeping it small and `Copy` is what
/// makes those loops cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs(u8);

impl Attrs {
    /// No attributes — what `SGR 0` leaves behind.
    pub const NONE: Self = Self(0);
    /// `SGR 1`.
    pub const BOLD: Self = Self(1 << 0);
    /// `SGR 3`.
    pub const ITALIC: Self = Self(1 << 1);
    /// `SGR 4`.
    pub const UNDERLINE: Self = Self(1 << 2);
    /// `SGR 7`: foreground and background swap at paint time.
    pub const INVERSE: Self = Self(1 << 3);

    /// Whether every bit of `other` is set here.
    ///
    /// With `Self::NONE` as `other` this is vacuously true, which is the
    /// useful answer: "contains nothing" is always satisfied.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Set every bit of `other`.
    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clear every bit of `other`.
    pub const fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Whether bold is set.
    #[must_use]
    pub fn bold(self) -> bool {
        self.contains(Self::BOLD)
    }

    /// Whether italic is set.
    #[must_use]
    pub fn italic(self) -> bool {
        self.contains(Self::ITALIC)
    }

    /// Whether underline is set.
    #[must_use]
    pub fn underline(self) -> bool {
        self.contains(Self::UNDERLINE)
    }

    /// Whether inverse video is set.
    #[must_use]
    pub fn inverse(self) -> bool {
        self.contains(Self::INVERSE)
    }
}

/// Everything about a cell except which character is in it.
///
/// Split out from [`Cell`] because it is what a run of cells shares: the
/// viewer draws one styled run at a time, and comparing whole `Style`s is
/// how runs are found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground colour.
    pub fg: CellColor,
    /// Background colour.
    pub bg: CellColor,
    /// Bold, italic, underline, inverse.
    pub attrs: Attrs,
}

impl Style {
    /// The style an erase performed with this pen leaves behind (BCE).
    ///
    /// Only the *background* survives: a program that erases under a
    /// bold underlined pen wants the colour behind the cursor, not an
    /// underline stretching to the right margin — which is what every
    /// other terminal does and what `tmux`'s status line assumes.
    ///
    /// Inverse is resolved the way a viewer resolves it, so `SGR 7`
    /// followed by an EL erases with the *foreground*. When that
    /// foreground is the terminal's own, no [`CellColor`] can name it and
    /// the `INVERSE` bit is kept instead — the one attribute that has to
    /// survive, because it is the only way to say "the text colour".
    #[must_use]
    pub fn erase(self) -> Style {
        let bg = if self.attrs.inverse() {
            self.fg
        } else {
            self.bg
        };
        match bg {
            CellColor::Default if self.attrs.inverse() => Style {
                fg: CellColor::Default,
                bg: CellColor::Default,
                attrs: Attrs::INVERSE,
            },
            CellColor::Default => Style::default(),
            bg => Style {
                fg: CellColor::Default,
                bg,
                attrs: Attrs::NONE,
            },
        }
    }
}

/// Whether a cell is the first half of a double-width character, the
/// second (a placeholder that draws nothing), or neither.
///
/// The placeholder exists so that column arithmetic stays honest:
/// everything else in the grid — cursor motion, insert/delete, damage
/// spans — counts cells, and a wide character really does occupy two of
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wide {
    /// An ordinary single-width cell.
    #[default]
    No,
    /// Holds a double-width character; the cell to its right is a
    /// [`Wide::Tail`].
    Lead,
    /// The right half of the cell to its left. Draws nothing.
    Tail,
}

/// One cell of the grid.
///
/// Not `Eq`, only `PartialEq`, because `char` comparison is all we want
/// and adding `Eq` would invite using cells as map keys — which would
/// hide the fact that two cells can look identical and still differ in
/// how they were produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    /// The character in the cell. A [`Wide::Tail`] holds a space.
    pub ch: char,
    /// Colours and attributes.
    pub style: Style,
    /// Which half of a double-width character this is, if any.
    pub wide: Wide,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: Style::default(),
            wide: Wide::No,
        }
    }
}

impl Cell {
    /// An empty cell: a space with default colours.
    ///
    /// This is the *default*-styled blank, which is what a resize, a
    /// reset or a switch to the alternate screen leaves behind. A
    /// program's own erase uses [`Cell::erased`] instead, so that the
    /// pen's background survives it.
    #[must_use]
    pub fn blank() -> Self {
        Self::default()
    }

    /// An erased cell: a space carrying `pen`'s background (BCE).
    #[must_use]
    pub fn erased(pen: Style) -> Self {
        Self {
            ch: ' ',
            style: pen.erase(),
            wide: Wide::No,
        }
    }

    /// Whether the cell is indistinguishable from an erased one.
    ///
    /// A [`Wide::Tail`] is never blank even though it holds a space: it
    /// belongs to the character on its left, and dropping it would make
    /// the row's columns stop adding up.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.style == Style::default() && self.wide == Wide::No
    }
}

/// A maximal run of cells on one row sharing a style.
///
/// The unit a viewer draws. `cols` is the run's width in cells, which is
/// *not* `text.chars().count()` when a double-width character is in it —
/// hence both fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// First column of the run.
    pub col: usize,
    /// Width of the run in cells.
    pub cols: usize,
    /// The characters, tails omitted.
    pub text: String,
    /// The style every cell in the run shares.
    pub style: Style,
}

// ---------------------------------------------------------------------
// Character width
// ---------------------------------------------------------------------

/// Ranges of characters that take two cells, sorted and non-overlapping.
///
/// Hand-rolled rather than pulled from a width crate: this is a table of
/// East Asian Wide/Fullwidth blocks plus the emoji blocks, it changes
/// about once a Unicode release, and the alternative is a dependency
/// whose data we would have to trust anyway. Where a block is mostly but
/// not entirely wide (the emoji pictographs, say) the whole block is
/// taken as wide — being one column wrong for a rare dingbat is a much
/// smaller error than splitting a family emoji across a cell boundary.
const WIDE_RANGES: &[(u32, u32)] = &[
    (0x1100, 0x115F),     // Hangul Jamo, initial consonants
    (0x2E80, 0x303E),     // CJK radicals, Kangxi, CJK symbols and punctuation
    (0x3041, 0x33FF),     // Kana, Bopomofo, Hangul compat jamo, enclosed CJK
    (0x3400, 0x4DBF),     // CJK Unified Ideographs Extension A
    (0x4E00, 0x9FFF),     // CJK Unified Ideographs
    (0xA000, 0xA4CF),     // Yi syllables and radicals
    (0xA960, 0xA97F),     // Hangul Jamo Extended-A
    (0xAC00, 0xD7A3),     // Hangul syllables
    (0xF900, 0xFAFF),     // CJK compatibility ideographs
    (0xFE10, 0xFE19),     // Vertical forms
    (0xFE30, 0xFE6F),     // CJK compatibility forms, small form variants
    (0xFF00, 0xFF60),     // Fullwidth ASCII forms
    (0xFFE0, 0xFFE6),     // Fullwidth signs
    (0x1_6FE0, 0x1_6FFF), // Ideographic symbols
    (0x1_7000, 0x1_8AFF), // Tangut
    (0x1_B000, 0x1_B2FF), // Kana supplement and extended
    (0x1_F004, 0x1_F004), // Mahjong red dragon, the one wide tile
    (0x1_F0CF, 0x1_F0CF), // Playing card black joker
    (0x1_F18E, 0x1_F18E), // Negative squared AB
    (0x1_F191, 0x1_F19A), // Squared CL..VS
    (0x1_F200, 0x1_F2FF), // Enclosed ideographic supplement
    (0x1_F300, 0x1_F64F), // Misc symbols and pictographs, emoticons
    (0x1_F680, 0x1_F6FF), // Transport and map symbols
    (0x1_F900, 0x1_F9FF), // Supplemental symbols and pictographs
    (0x1_FA70, 0x1_FAFF), // Symbols and pictographs extended-A
    (0x2_0000, 0x3_FFFD), // CJK Unified Ideographs Extension B and later
];

/// Ranges of characters that take no cell at all: combining marks,
/// variation selectors and the zero-width format controls.
///
/// Not exhaustive — the full set of `Mn`/`Me` is thousands of ranges.
/// These are the blocks a terminal actually meets: Latin and Greek
/// combining accents, Hebrew and Arabic points, Indic and Thai marks, the
/// musical and variation selectors, and the zero-width spaces and joiners
/// that emoji sequences are built from.
const ZERO_RANGES: &[(u32, u32)] = &[
    (0x0300, 0x036F),     // Combining diacritical marks
    (0x0483, 0x0489),     // Cyrillic combining marks
    (0x0591, 0x05C7),     // Hebrew points and accents
    (0x0610, 0x061A),     // Arabic signs
    (0x064B, 0x065F),     // Arabic vowel marks
    (0x0670, 0x0670),     // Arabic superscript alef
    (0x06D6, 0x06ED),     // Quranic annotation marks
    (0x0711, 0x0711),     // Syriac superscript alaph
    (0x0730, 0x074A),     // Syriac points
    (0x07A6, 0x07B0),     // Thaana vowel signs
    (0x07EB, 0x07F3),     // NKo marks
    (0x0816, 0x082D),     // Samaritan marks
    (0x0859, 0x085B),     // Mandaic marks
    (0x08D3, 0x0902),     // Arabic extended marks, Devanagari anusvara
    (0x093C, 0x093C),     // Devanagari nukta
    (0x0941, 0x0948),     // Devanagari vowel signs
    (0x094D, 0x094D),     // Devanagari virama
    (0x0951, 0x0957),     // Devanagari stress and accent marks
    (0x0E31, 0x0E31),     // Thai mai han akat
    (0x0E34, 0x0E3A),     // Thai vowel signs above and below
    (0x0E47, 0x0E4E),     // Thai tone marks
    (0x0EB1, 0x0EB1),     // Lao vowel sign mai kan
    (0x0EB4, 0x0EBC),     // Lao vowel signs
    (0x0EC8, 0x0ECD),     // Lao tone marks
    (0x0F35, 0x0F39),     // Tibetan marks
    (0x1AB0, 0x1AFF),     // Combining diacriticals extended
    (0x1DC0, 0x1DFF),     // Combining diacriticals supplement
    (0x200B, 0x200F),     // Zero-width space, joiners, directional marks
    (0x2060, 0x2064),     // Word joiner and invisible operators
    (0x20D0, 0x20F0),     // Combining marks for symbols
    (0xFE00, 0xFE0F),     // Variation selectors
    (0xFE20, 0xFE2F),     // Combining half marks
    (0xFEFF, 0xFEFF),     // Zero-width no-break space (BOM)
    (0x1_D167, 0x1_D169), // Musical combining marks
    (0x1_D17B, 0x1_D182), // Musical combining marks
    (0xE_0100, 0xE_01EF), // Variation selectors supplement
];

/// Whether `c` falls in one of the sorted, non-overlapping `ranges`.
fn in_ranges(ranges: &[(u32, u32)], c: u32) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// How many cells `ch` occupies: 0, 1 or 2.
///
/// Control characters are zero, and so are combining marks — see
/// [`Grid::put_char`] for what the grid does with a zero-width character
/// (it drops it, and says why).
#[must_use]
pub fn char_width(ch: char) -> usize {
    let c = ch as u32;
    if c < 0x20 || (0x7F..0xA0).contains(&c) {
        return 0;
    }
    if c < 0x300 {
        return 1; // the overwhelmingly common case, before any table lookup
    }
    if in_ranges(ZERO_RANGES, c) {
        0
    } else if in_ranges(WIDE_RANGES, c) {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------
// The grid
// ---------------------------------------------------------------------

/// The cursor and the state that must be saved and restored with it.
///
/// `wrap_pending` travels with the cursor because DECSC/DECRC are
/// specified to save it: a program that saves the cursor in the last
/// column and restores it later must find the same deferred wrap waiting.
#[derive(Debug, Clone, Copy, Default)]
struct Cursor {
    row: usize,
    col: usize,
    pen: Style,
    wrap_pending: bool,
}

/// A blank row of `cols` cells.
fn blank_line(cols: usize) -> Vec<Cell> {
    vec![Cell::blank(); cols]
}

/// The grid.
///
/// Holds one screen of cells, a scrollback ring, the cursor, and the
/// damage spans. When the alternate screen is active the primary screen
/// is parked in `saved_lines` untouched, which is what makes leaving the
/// alt screen exact rather than a repaint request.
pub struct Grid {
    cols: usize,
    rows: usize,
    /// The active screen, `rows` rows of `cols` cells.
    lines: Vec<Vec<Cell>>,
    /// The primary screen while the alternate one is active.
    saved_lines: Option<Vec<Vec<Cell>>>,
    /// Finished lines pushed off the top of the primary screen.
    /// Finished lines, oldest first, **trimmed of trailing default
    /// blanks**.
    ///
    /// A scrollback line is immutable history: nothing will ever write
    /// to column 60 of a line that ended at column 12, so the 68 blank
    /// cells after it are 1 088 bytes recording that nothing is there.
    /// Storing them cost 12.8 MB of the 29.4 MB RSS the first box run
    /// measured with a full 10 000-line buffer, which is why this is a
    /// `Vec` of *varying* length while `lines` (the live screen, which
    /// is written to everywhere) is not.
    ///
    /// [`Grid::display_row`] still hands out a full-width row — see
    /// [`Grid::pad`] — so nothing outside this module sees the
    /// difference.
    history: VecDeque<Vec<Cell>>,
    /// A full row of blanks, to pad a trimmed history line back to width
    /// without allocating on every read. Resized with the grid.
    pad_row: Vec<Cell>,
    /// Ring capacity; zero means no scrollback at all.
    history_max: usize,
    /// How many lines back the view is scrolled, 0 = live.
    view_offset: usize,
    cursor: Cursor,
    /// DECSC's slot.
    saved_cursor: Cursor,
    /// Where `1049` parks the cursor while the alt screen is up.
    alt_saved_cursor: Cursor,
    alt: bool,
    cursor_visible: bool,
    /// DECSTBM, 0-based and inclusive.
    scroll_top: usize,
    scroll_bot: usize,
    /// Per row, the changed columns as `(start, end_exclusive)`.
    damage: Vec<Option<(usize, usize)>>,
}

impl Grid {
    /// A blank grid of `cols` by `rows` with room for `scrollback` lines
    /// of history.
    ///
    /// Both dimensions are forced to at least one: a zero-column grid has
    /// no representable cursor position, and every operation below would
    /// need a special case for it.
    #[must_use]
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            lines: vec![blank_line(cols); rows],
            saved_lines: None,
            history: VecDeque::new(),
            pad_row: vec![Cell::blank(); cols],
            history_max: scrollback,
            view_offset: 0,
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            alt_saved_cursor: Cursor::default(),
            alt: false,
            cursor_visible: true,
            scroll_top: 0,
            scroll_bot: rows - 1,
            damage: vec![None; rows],
        }
    }

    /// Width of the screen in cells.
    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Height of the screen in cells.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Resize the screen.
    ///
    /// Rows are added or removed at the bottom, except that shrinking
    /// pushes lines off the *top* — into the scrollback, as if they had
    /// scrolled — when that is what it takes to keep the cursor on
    /// screen; otherwise a shell prompt at the bottom would be thrown
    /// away by a window drag.
    ///
    /// Scrollback is **not** rewrapped: lines already in history keep the
    /// widths they were written at. Rewrapping needs per-line "this was a
    /// continuation" bookkeeping that nothing else in the grid would use,
    /// and the payoff is confined to re-reading old output after a
    /// resize. This is a documented limitation, not an oversight.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        for line in &mut self.lines {
            line.resize(cols, Cell::blank());
            sanitize_line(line);
        }
        if let Some(saved) = &mut self.saved_lines {
            for line in saved.iter_mut() {
                line.resize(cols, Cell::blank());
                sanitize_line(line);
            }
        }
        self.cols = cols;
        // The pad row follows the width. History lines are deliberately
        // *not* resized: they are trimmed already, and a narrowing that
        // rewrote ten thousand of them would be the reflow this design
        // explicitly does not do (`docs/term.md`). A history line longer
        // than the new width is clipped where it is read.
        self.pad_row = vec![Cell::blank(); cols];
        self.fit_rows(rows);
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        self.view_offset = self.view_offset.min(self.history.len());
        self.damage = vec![None; rows];
        self.damage_all();
    }

    /// Add or drop rows, keeping the cursor on screen.
    fn fit_rows(&mut self, rows: usize) {
        if let Some(saved) = &mut self.saved_lines {
            saved.resize(rows, blank_line(self.cols));
        }
        if rows > self.rows {
            self.lines.resize(rows, blank_line(self.cols));
            return;
        }
        let drop = self.rows - rows;
        let from_top = drop.min(self.cursor.row.saturating_sub(rows - 1));
        for _ in 0..from_top {
            let line = self.lines.remove(0);
            self.push_history(&line);
            // `line` is dropped here, full width: that is what the blank
            // row appended below reuses.
        }
        self.cursor.row -= from_top;
        self.lines.truncate(rows);
    }

    // --- what a viewer reads -------------------------------------

    /// Row `i` of what is on screen *now*, honouring the scrollback
    /// offset, **padded to the full width**.
    ///
    /// A scrollback line is stored trimmed of its trailing blanks (see
    /// the `history` field), so this returns one of two slices: the live
    /// row itself, or a trimmed history row — and for the short case the
    /// short case is padded with blanks on the way out. Callers that only
    /// look at the significant cells (`row_runs`, `row_text`, which
    /// trim anyway) can use [`Grid::display_row_raw`] and skip the
    /// question entirely.
    ///
    /// # Panics
    ///
    /// If `i` is not a valid row index.
    #[must_use]
    pub fn display_row(&self, i: usize) -> std::borrow::Cow<'_, [Cell]> {
        let row = self.display_row_raw(i);
        if row.len() >= self.cols {
            return std::borrow::Cow::Borrowed(&row[..self.cols]);
        }
        let mut out = Vec::with_capacity(self.cols);
        out.extend_from_slice(row);
        out.extend_from_slice(&self.pad_row[row.len()..self.cols]);
        std::borrow::Cow::Owned(out)
    }

    /// Row `i` as it is stored: the live row at full width, or a
    /// scrollback row trimmed of its trailing default blanks.
    ///
    /// This is the allocation-free form, and the one the widget's paint
    /// path uses — every caller inside this module trims the row before
    /// looking at it anyway, so the padding [`Grid::display_row`] adds
    /// would be built only to be ignored.
    ///
    /// # Panics
    ///
    /// If `i` is not a valid row index.
    #[must_use]
    pub fn display_row_raw(&self, i: usize) -> &[Cell] {
        assert!(i < self.rows, "row {i} out of range");
        // The view is a window over `history ++ lines`, anchored
        // `view_offset` rows above the bottom of `lines`.
        let hist = self.history.len();
        let idx = hist - self.view_offset.min(hist) + i;
        if idx < hist {
            &self.history[idx]
        } else {
            &self.lines[idx - hist]
        }
    }

    /// Row `i` split into maximal same-style runs, appended to `out`.
    ///
    /// A [`Wide::Tail`] contributes no character but does extend the
    /// run's `cols`, so a run's `col + cols` is always where the next run
    /// starts. Trailing default-styled blanks produce no run at all,
    /// which is what makes an idle screen cost a viewer nothing.
    ///
    /// # Panics
    ///
    /// If `i` is not a valid row index.
    pub fn row_runs(&self, i: usize, out: &mut Vec<Run>) {
        let cells = self.display_row_raw(i);
        let end = trimmed_len(cells);
        let mut col = 0;
        while col < end {
            let style = cells[col].style;
            let start = col;
            let mut text = String::new();
            while col < end && cells[col].style == style {
                if cells[col].wide != Wide::Tail {
                    text.push(cells[col].ch);
                }
                col += 1;
            }
            out.push(Run {
                col: start,
                cols: col - start,
                text,
                style,
            });
        }
    }

    /// Row `i` as text, trailing blanks trimmed.
    ///
    /// # Panics
    ///
    /// If `i` is not a valid row index.
    #[must_use]
    pub fn row_text(&self, i: usize) -> String {
        let cells = self.display_row_raw(i);
        let end = trimmed_len(cells);
        cells[..end]
            .iter()
            .filter(|c| c.wide != Wide::Tail)
            .map(|c| c.ch)
            .collect()
    }

    /// The whole visible screen as text, rows joined with `\n`.
    ///
    /// Mostly a test and debugging affordance: it is how a test says
    /// "this is what the screen should read".
    #[must_use]
    pub fn text(&self) -> String {
        (0..self.rows)
            .map(|i| self.row_text(i))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Cursor position as `(row, col)` in *display* space, or `None` when
    /// it is scrolled out of view.
    ///
    /// `None` rather than a clamped position: a viewer that drew the
    /// cursor at the edge of a scrolled-back screen would be lying about
    /// where typing goes.
    #[must_use]
    pub fn cursor(&self) -> Option<(usize, usize)> {
        let row = self.cursor.row + self.view_offset;
        (row < self.rows).then_some((row, self.cursor.col))
    }

    /// The cursor in screen space, ignoring the scrollback offset.
    ///
    /// What a DSR (cursor position report) must answer with: the program
    /// on the other end of the pty asks where *it* is writing, which has
    /// nothing to do with where the user has scrolled the view.
    #[must_use]
    pub fn cursor_screen(&self) -> (usize, usize) {
        (self.cursor.row, self.cursor.col)
    }

    /// Whether the cursor should be drawn (DECTCEM).
    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// Whether the alternate screen is showing.
    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.alt
    }

    /// How many lines of scrollback exist.
    ///
    /// Zero on the alternate screen: the primary's history is still
    /// there, parked, but it is not what the view is showing and
    /// scrolling into it would splice two unrelated screens together.
    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        if self.alt { 0 } else { self.history.len() }
    }

    /// How many lines back the view is scrolled; 0 is live.
    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.view_offset
    }

    // --- damage ---------------------------------------------------

    /// Whether row `i` changed since the last [`Grid::clear_damage`].
    #[must_use]
    pub fn row_dirty(&self, i: usize) -> bool {
        self.row_damage(i).is_some()
    }

    /// The columns of row `i` that changed, as an inclusive-exclusive
    /// range, or `None` when nothing did.
    ///
    /// One span per row, so a write at column 0 and another at column 79
    /// report `(0, 80)` even though the middle is untouched. Repainting a
    /// few extra cells is cheaper than tracking them precisely.
    #[must_use]
    pub fn row_damage(&self, i: usize) -> Option<(usize, usize)> {
        self.damage.get(i).copied().flatten()
    }

    /// Whether anything at all changed.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.damage.iter().any(Option::is_some)
    }

    /// Forget the damage; the viewer has caught up.
    pub fn clear_damage(&mut self) {
        for d in &mut self.damage {
            *d = None;
        }
    }

    /// Mark every row damaged (a resize, a scroll, a fresh viewer).
    pub fn damage_all(&mut self) {
        let span = Some((0, self.cols));
        for d in &mut self.damage {
            *d = span;
        }
    }

    /// Widen row `row`'s damage span to cover `from..to`.
    ///
    /// Takes a *screen* row and maps it into display space, because that
    /// is the space a viewer indexes. While the view is scrolled back a
    /// write can land off-screen entirely, and then there is nothing to
    /// report.
    fn damage_span(&mut self, row: usize, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let display = row + self.view_offset;
        if display >= self.rows {
            return;
        }
        let slot = &mut self.damage[display];
        *slot = Some(match *slot {
            Some((a, b)) => (a.min(from), b.max(to)),
            None => (from, to),
        });
    }

    /// Damage caused by moving lines around within `top..=bot`.
    ///
    /// While the view is scrolled back the *whole* window's content
    /// shifts under the viewer — the window is anchored to the bottom of
    /// the grid, not to the history — so the honest answer is "everything
    /// changed". It is rare enough not to be worth being clever about.
    fn damage_scroll(&mut self, top: usize, bot: usize) {
        if self.view_offset > 0 {
            self.damage_all();
            return;
        }
        for row in top..=bot.min(self.rows - 1) {
            self.damage_span(row, 0, self.cols);
        }
    }

    // --- scrollback ----------------------------------------------

    /// Scroll the view back by `lines`, clamped to the scrollback.
    ///
    /// A no-op on the alternate screen, damage included: full-screen
    /// programs redraw themselves and there is no history to reach.
    pub fn scroll_up(&mut self, lines: usize) {
        if self.alt {
            return;
        }
        let want = (self.view_offset + lines).min(self.history.len());
        if want != self.view_offset {
            self.view_offset = want;
            self.damage_all();
        }
    }

    /// Scroll the view forward by `lines`, clamped at the live screen.
    pub fn scroll_down(&mut self, lines: usize) {
        if self.alt {
            return;
        }
        let want = self.view_offset.saturating_sub(lines);
        if want != self.view_offset {
            self.view_offset = want;
            self.damage_all();
        }
    }

    /// Jump the view back to the live screen.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_down(self.view_offset);
    }

    // --- the operations the parser drives -------------------------

    /// The style new characters are written with.
    #[must_use]
    pub fn pen(&self) -> Style {
        self.cursor.pen
    }

    /// Set the style new characters are written with (SGR).
    pub fn set_pen(&mut self, style: Style) {
        self.cursor.pen = style;
    }

    /// Write one character at the cursor and advance.
    ///
    /// Zero-width characters — combining marks, variation selectors, the
    /// zero-width joiner — are **dropped**. A [`Cell`] holds a single
    /// `char`, so there is nowhere to put a mark that belongs to the
    /// previous cell short of giving every cell a growable string, which
    /// would cost an allocation per cell for a case a monospaced grid
    /// renders poorly anyway. Dropping them is wrong but stable: no mark
    /// ever consumes a cell, so nothing downstream is knocked out of
    /// alignment.
    pub fn put_char(&mut self, ch: char) {
        let mut width = char_width(ch);
        if width == 0 {
            return;
        }
        if width == 2 && self.cols < 2 {
            width = 1; // a one-column grid cannot hold a wide character
        }
        if self.cursor.wrap_pending {
            self.wrap();
        }
        if width == 2 && self.cursor.col + 2 > self.cols {
            // A wide character never straddles the right edge; it moves
            // to the next row whole, leaving the last cell blank.
            self.wrap();
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.clear_wide_partner(row, col);
        if width == 2 {
            self.clear_wide_partner(row, col + 1);
        }
        let style = self.cursor.pen;
        self.lines[row][col] = Cell {
            ch,
            style,
            wide: if width == 2 { Wide::Lead } else { Wide::No },
        };
        if width == 2 {
            self.lines[row][col + 1] = Cell {
                ch: ' ',
                style,
                wide: Wide::Tail,
            };
        }
        self.damage_span(row, col, col + width);
        if col + width >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.wrap_pending = true;
        } else {
            self.cursor.col += width;
        }
    }

    /// Take the deferred wrap: to column 0 of the next line, scrolling if
    /// that means leaving the scroll region.
    fn wrap(&mut self) {
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
        self.index();
    }

    /// Overwriting half of a double-width pair blanks the other half,
    /// which would otherwise be left as a lead with no tail (or a tail
    /// with no lead) and draw as a stray glyph.
    fn clear_wide_partner(&mut self, row: usize, col: usize) {
        let cell = self.lines[row][col];
        match cell.wide {
            Wide::Lead if col + 1 < self.cols => {
                self.lines[row][col + 1] = Cell::blank();
                self.damage_span(row, col + 1, col + 2);
            }
            Wide::Tail if col > 0 => {
                self.lines[row][col - 1] = Cell::blank();
                self.damage_span(row, col - 1, col);
            }
            _ => {}
        }
    }

    /// CR: to column 0 of the same row.
    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    /// LF: down one row, scrolling the region when already at its bottom.
    pub fn line_feed(&mut self) {
        self.cursor.wrap_pending = false;
        self.index();
    }

    /// The "move down, scroll at the bottom" primitive behind LF, IND and
    /// the deferred wrap.
    fn index(&mut self) {
        if self.cursor.row == self.scroll_bot {
            self.scroll_region_up(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    /// RI: up one row, scrolling the region down at its top.
    pub fn reverse_line_feed(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_region_down(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    /// BS: left one column, stopping at column 0.
    ///
    /// Does not wrap back to the previous row. Shells assume that: a
    /// backspace at column 0 is how they rub out a character they never
    /// wrote.
    pub fn backspace(&mut self) {
        if self.cursor.wrap_pending {
            self.cursor.wrap_pending = false;
        } else if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    /// HT: to the next tab stop, every eight columns.
    ///
    /// Fixed stops, not a settable set: HTS/TBC are vanishingly rare in
    /// programs a terminal has to please today, and every one of them
    /// assumes the default eight.
    pub fn tab(&mut self) {
        self.cursor.wrap_pending = false;
        let next = (self.cursor.col / 8 + 1) * 8;
        self.cursor.col = next.min(self.cols - 1);
    }

    /// Back-tab: to the previous tab stop.
    pub fn back_tab(&mut self) {
        self.cursor.wrap_pending = false;
        self.cursor.col = self.cursor.col.saturating_sub(1) / 8 * 8;
    }

    /// Absolute cursor move, clamped to the screen.
    pub fn move_to(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    /// Relative cursor move, clamped to the screen.
    ///
    /// Clamped, not wrapped and not scrolling: CUU at the top row is
    /// specified to do nothing, and a program that wanted a scroll would
    /// have asked for one.
    pub fn move_by(&mut self, drow: isize, dcol: isize) {
        let row = offset(self.cursor.row, drow, self.rows - 1);
        let col = offset(self.cursor.col, dcol, self.cols - 1);
        self.move_to(row, col);
    }

    /// Absolute column move (CHA), clamped.
    pub fn move_to_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    /// Absolute row move (VPA), clamped.
    pub fn move_to_row(&mut self, row: usize) {
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.wrap_pending = false;
    }

    /// ED: 0 = to end of screen, 1 = to start, 2 = all, 3 = all plus the
    /// scrollback.
    ///
    /// The cursor does not move — xterm's behaviour, and what `clear`
    /// relies on when it follows the ED with an explicit CUP.
    pub fn erase_in_display(&mut self, mode: u16) {
        self.cursor.wrap_pending = false;
        let (row, col) = (self.cursor.row, self.cursor.col);
        match mode {
            0 => {
                self.erase_cells(row, col, self.cols);
                for r in row + 1..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
            }
            1 => {
                for r in 0..row {
                    self.erase_cells(r, 0, self.cols);
                }
                self.erase_cells(row, 0, col + 1);
            }
            2 | 3 => {
                for r in 0..self.rows {
                    self.erase_cells(r, 0, self.cols);
                }
                if mode == 3 {
                    self.history.clear();
                    self.view_offset = 0;
                    self.damage_all();
                }
            }
            _ => {}
        }
    }

    /// EL: 0 = to end of line, 1 = to start, 2 = the whole line.
    pub fn erase_in_line(&mut self, mode: u16) {
        self.cursor.wrap_pending = false;
        let (row, col) = (self.cursor.row, self.cursor.col);
        match mode {
            0 => self.erase_cells(row, col, self.cols),
            1 => self.erase_cells(row, 0, col + 1),
            2 => self.erase_cells(row, 0, self.cols),
            _ => {}
        }
    }

    /// Blank `row`'s cells in `from..to`, repairing any wide pair the
    /// range cuts in half.
    fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        let to = to.min(self.cols);
        if from >= to {
            return;
        }
        self.clear_wide_partner(row, from);
        self.clear_wide_partner(row, to - 1);
        let fill = Cell::erased(self.cursor.pen);
        for cell in &mut self.lines[row][from..to] {
            *cell = fill;
        }
        self.damage_span(row, from, to);
    }

    /// ICH: open `n` blank cells at the cursor, pushing the rest of the
    /// row right and off the end.
    pub fn insert_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let n = n.min(self.cols - col);
        if n == 0 {
            return;
        }
        let fill = Cell::erased(self.cursor.pen);
        let line = &mut self.lines[row];
        line[col..].rotate_right(n);
        for cell in &mut line[col..col + n] {
            *cell = fill;
        }
        sanitize_line(line);
        self.damage_span(row, col, self.cols);
    }

    /// DCH: remove `n` cells at the cursor, pulling the rest of the row
    /// left and blanking the end.
    pub fn delete_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let n = n.min(self.cols - col);
        if n == 0 {
            return;
        }
        let fill = Cell::erased(self.cursor.pen);
        let cols = self.cols;
        let line = &mut self.lines[row];
        line[col..].rotate_left(n);
        for cell in &mut line[cols - n..] {
            *cell = fill;
        }
        sanitize_line(line);
        self.damage_span(row, col, self.cols);
    }

    /// ECH: blank `n` cells at the cursor without moving anything.
    pub fn erase_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.erase_cells(row, col, col + n);
    }

    /// IL: open `n` blank lines at the cursor row, pushing the rest of
    /// the scroll region down.
    ///
    /// Ignored when the cursor is outside the scroll region, and the
    /// cursor goes to column 0 — both as DEC specifies.
    pub fn insert_lines(&mut self, n: usize) {
        if !self.in_region(self.cursor.row) {
            return;
        }
        let (top, bot) = (self.cursor.row, self.scroll_bot);
        self.shift_down(top, bot, n);
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    /// DL: remove `n` lines at the cursor row, pulling the rest of the
    /// scroll region up.
    pub fn delete_lines(&mut self, n: usize) {
        if !self.in_region(self.cursor.row) {
            return;
        }
        let (top, bot) = (self.cursor.row, self.scroll_bot);
        self.shift_up(top, bot, n, false);
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    /// SU: scroll the region up `n` lines, cursor unmoved.
    pub fn scroll_region_up(&mut self, n: usize) {
        let (top, bot) = (self.scroll_top, self.scroll_bot);
        self.shift_up(top, bot, n, true);
    }

    /// SD: scroll the region down `n` lines, cursor unmoved.
    pub fn scroll_region_down(&mut self, n: usize) {
        let (top, bot) = (self.scroll_top, self.scroll_bot);
        self.shift_down(top, bot, n);
    }

    /// Whether `row` is inside the scroll region.
    fn in_region(&self, row: usize) -> bool {
        row >= self.scroll_top && row <= self.scroll_bot
    }

    /// Move `top..=bot` up by `n`, blanking the bottom.
    ///
    /// `to_history` is the difference between a scroll and a deletion:
    /// only a scroll of the *whole* primary screen puts the line it loses
    /// into the scrollback. A scroll region is a program painting a pane,
    /// and DL is a program editing a line — neither is history.
    fn shift_up(&mut self, top: usize, bot: usize, n: usize, to_history: bool) {
        if top > bot || bot >= self.rows || n == 0 {
            return;
        }
        let n = n.min(bot - top + 1);
        let keep = to_history && !self.alt && top == 0 && bot == self.rows - 1;
        let fill = Cell::erased(self.cursor.pen);
        for _ in 0..n {
            let mut line = self.lines.remove(top);
            if !keep {
                line.fill(fill);
                self.lines.insert(bot, line);
                continue;
            }
            self.push_history(&line);
            // The row goes back to the bottom blanked rather than being
            // freed and re-allocated: it is already the right width, and
            // recycling it is what keeps a scrolling terminal from
            // churning one full-width allocation per line.
            line.fill(fill);
            self.lines.insert(bot, line);
        }
        self.damage_scroll(top, bot);
    }

    /// Move `top..=bot` down by `n`, blanking the top. Nothing is ever
    /// pushed to history this way: those lines are still on screen.
    fn shift_down(&mut self, top: usize, bot: usize, n: usize) {
        if top > bot || bot >= self.rows || n == 0 {
            return;
        }
        let n = n.min(bot - top + 1);
        let fill = Cell::erased(self.cursor.pen);
        for _ in 0..n {
            let mut line = self.lines.remove(bot);
            line.fill(fill);
            self.lines.insert(top, line);
        }
        self.damage_scroll(top, bot);
    }

    /// Push one finished line into the scrollback ring.
    ///
    /// The view offset is deliberately *not* adjusted: it counts lines
    /// back from the live screen, so output arriving while the user is
    /// scrolled back moves the content under them rather than pinning it.
    /// That matches what the scroll keys mean ("show me `n` lines back")
    /// and keeps the invariant that offset 0 is always the live screen.
    fn push_history(&mut self, line: &[Cell]) {
        if self.history_max == 0 {
            return;
        }
        if self.history.len() == self.history_max {
            self.history.pop_front();
        }
        // Trim on the way in: see the field's docs.
        //
        // The copy is deliberate, and the first version of this got it
        // wrong in a way only the box could show. Truncating the row in
        // place and calling `shrink_to_fit` leaves the allocator holding
        // a full-width hole that the *next* blank row, being one cell
        // longer than the trimmed one, cannot reuse — so RSS still grew
        // by a full row per line (1.44 kB at 90 columns) even though
        // every stored row was two cells long. Allocating a right-sized
        // copy and letting the full-width original go back to the free
        // list reuses it for the blank row that replaces it, which is
        // the whole point.
        let keep = trimmed_len(line);
        self.history.push_back(line[..keep].to_vec());
        self.view_offset = self.view_offset.min(self.history.len());
    }

    /// DECSTBM. `None` resets to the whole screen. Rows are 0-based and
    /// inclusive; an invalid region is ignored, as xterm does.
    ///
    /// Setting a region homes the cursor, which is what a program that
    /// sets one expects: the sequence is almost always followed by
    /// painting from the top.
    pub fn set_scroll_region(&mut self, region: Option<(usize, usize)>) {
        match region {
            Some((top, bot)) if top < bot && bot < self.rows => {
                self.scroll_top = top;
                self.scroll_bot = bot;
            }
            Some(_) => return,
            None => {
                self.scroll_top = 0;
                self.scroll_bot = self.rows - 1;
            }
        }
        self.move_to(0, 0);
    }

    /// The scroll region as 0-based inclusive rows.
    #[must_use]
    pub fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bot)
    }

    /// DECTCEM: whether the cursor is drawn.
    pub fn set_cursor_visible(&mut self, on: bool) {
        self.cursor_visible = on;
    }

    /// DECSC: save cursor position, pen and pending wrap.
    pub fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
    }

    /// DECRC: restore what [`Grid::save_cursor`] saved, clamped in case
    /// the screen shrank in between.
    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
    }

    /// Enter or leave the alternate screen (DECSET/DECRST 1049).
    ///
    /// The alt screen has no scrollback and is discarded on leave;
    /// entering saves the cursor and leaving restores it, as 1049
    /// specifies. The primary screen is kept whole rather than replayed,
    /// so leaving `vim` restores the shell output exactly, and the view
    /// jumps back to the live screen because the alt screen is by
    /// definition the bottom of the world.
    pub fn set_alt_screen(&mut self, on: bool) {
        if on == self.alt {
            return;
        }
        if on {
            self.alt_saved_cursor = self.cursor;
            self.saved_lines = Some(std::mem::replace(
                &mut self.lines,
                vec![blank_line(self.cols); self.rows],
            ));
            self.alt = true;
            self.view_offset = 0;
        } else {
            if let Some(saved) = self.saved_lines.take() {
                self.lines = saved;
            }
            self.alt = false;
            self.cursor = self.alt_saved_cursor;
            self.cursor.row = self.cursor.row.min(self.rows - 1);
            self.cursor.col = self.cursor.col.min(self.cols - 1);
        }
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.damage_all();
    }

    /// RIS: back to the state of a freshly opened terminal, scrollback
    /// included.
    pub fn reset(&mut self) {
        self.alt = false;
        self.saved_lines = None;
        self.lines = vec![blank_line(self.cols); self.rows];
        self.history.clear();
        self.view_offset = 0;
        self.cursor = Cursor::default();
        self.saved_cursor = Cursor::default();
        self.alt_saved_cursor = Cursor::default();
        self.cursor_visible = true;
        self.scroll_top = 0;
        self.scroll_bot = self.rows - 1;
        self.damage_all();
    }
}

/// `base + delta`, saturating at 0 and `max`.
///
/// Written with `unsigned_abs` rather than a cast to `isize` so that no
/// arithmetic can wrap: the deltas come from escape sequences, and a
/// program is free to ask for `CUB 65535`.
fn offset(base: usize, delta: isize, max: usize) -> usize {
    let moved = if delta >= 0 {
        base.saturating_add(delta.unsigned_abs())
    } else {
        base.saturating_sub(delta.unsigned_abs())
    };
    moved.min(max)
}

/// How much of a row is worth looking at: everything up to the last cell
/// that is not a default-styled blank.
fn trimmed_len(cells: &[Cell]) -> usize {
    cells
        .iter()
        .rposition(|c| !c.is_blank())
        .map_or(0, |i| i + 1)
}

/// Blank any half of a wide pair whose partner is gone.
///
/// Shifting cells around (ICH, DCH, a narrowing resize) can cut a pair in
/// two. Repairing the row afterwards is simpler — and cheaper to be sure
/// of — than teaching every shift about pairs, and it keeps the invariant
/// the rest of the module relies on: a `Lead` is always followed by a
/// `Tail`, and a `Tail` always follows a `Lead`.
fn sanitize_line(line: &mut [Cell]) {
    let n = line.len();
    for i in 0..n {
        match line[i].wide {
            Wide::Lead if i + 1 >= n || line[i + 1].wide != Wide::Tail => {
                line[i] = Cell::blank();
            }
            Wide::Tail if i == 0 || line[i - 1].wide != Wide::Lead => {
                line[i] = Cell::blank();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a string through `put_char`, as the parser would.
    fn put(grid: &mut Grid, s: &str) {
        for ch in s.chars() {
            grid.put_char(ch);
        }
    }

    fn runs(grid: &Grid, row: usize) -> Vec<Run> {
        let mut out = Vec::new();
        grid.row_runs(row, &mut out);
        out
    }

    /// Every wide/tail pairing holds and no row has the wrong length.
    fn assert_consistent(grid: &Grid) {
        for r in 0..grid.rows() {
            let row = grid.display_row(r);
            assert_eq!(row.len(), grid.cols(), "row {r} has the wrong width");
            for (i, cell) in row.iter().enumerate() {
                match cell.wide {
                    Wide::Lead => assert_eq!(row[i + 1].wide, Wide::Tail, "lead without tail"),
                    Wide::Tail => {
                        assert!(i > 0, "tail at column 0");
                        assert_eq!(row[i - 1].wide, Wide::Lead, "tail without lead");
                    }
                    Wide::No => {}
                }
            }
        }
        let (row, col) = grid.cursor_screen();
        assert!(row < grid.rows() && col < grid.cols(), "cursor off screen");
    }

    // --- attributes and styles ---------------------------------------

    #[test]
    fn attrs_are_a_set_of_flags() {
        let mut a = Attrs::NONE;
        assert!(!a.bold());
        a.insert(Attrs::BOLD);
        a.insert(Attrs::UNDERLINE);
        assert!(a.bold() && a.underline());
        assert!(!a.italic() && !a.inverse());
        a.remove(Attrs::BOLD);
        assert!(!a.bold() && a.underline());
        assert!(a.contains(Attrs::NONE));
    }

    #[test]
    fn a_default_cell_is_blank() {
        assert!(Cell::blank().is_blank());
        let mut c = Cell::blank();
        c.style.fg = CellColor::Indexed(1);
        assert!(!c.is_blank(), "a styled space is not blank");
        let tail = Cell {
            ch: ' ',
            style: Style::default(),
            wide: Wide::Tail,
        };
        assert!(!tail.is_blank(), "a tail belongs to its lead");
    }

    // --- writing ------------------------------------------------------

    #[test]
    fn plain_characters_land_in_consecutive_cells() {
        let mut g = Grid::new(10, 3, 0);
        put(&mut g, "hi");
        assert_eq!(g.row_text(0), "hi");
        assert_eq!(g.cursor(), Some((0, 2)));
        assert_consistent(&g);
    }

    #[test]
    fn the_whole_screen_reads_as_text() {
        let mut g = Grid::new(4, 3, 0);
        put(&mut g, "ab");
        g.line_feed();
        g.carriage_return();
        put(&mut g, "cd");
        assert_eq!(g.text(), "ab\ncd\n");
    }

    #[test]
    fn a_carriage_return_goes_to_column_zero() {
        let mut g = Grid::new(10, 3, 0);
        put(&mut g, "abc");
        g.carriage_return();
        put(&mut g, "X");
        assert_eq!(g.row_text(0), "Xbc");
    }

    #[test]
    fn backspace_stops_at_column_zero() {
        let mut g = Grid::new(4, 1, 0);
        g.backspace();
        assert_eq!(g.cursor(), Some((0, 0)));
        put(&mut g, "ab");
        g.backspace();
        assert_eq!(g.cursor(), Some((0, 1)));
    }

    #[test]
    fn tab_moves_to_every_eighth_column() {
        let mut g = Grid::new(20, 1, 0);
        g.tab();
        assert_eq!(g.cursor(), Some((0, 8)));
        g.tab();
        assert_eq!(g.cursor(), Some((0, 16)));
        g.tab();
        assert_eq!(g.cursor(), Some((0, 19)), "clamped to the last column");
        g.back_tab();
        assert_eq!(g.cursor(), Some((0, 16)));
    }

    #[test]
    fn writing_the_last_column_defers_the_wrap() {
        let mut g = Grid::new(4, 2, 0);
        put(&mut g, "abcd");
        assert_eq!(g.cursor(), Some((0, 3)), "still on the last column");
        assert_eq!(g.row_text(1), "");
        put(&mut g, "e");
        assert_eq!(g.cursor(), Some((1, 1)));
        assert_eq!(g.row_text(1), "e");
    }

    #[test]
    fn a_deferred_wrap_is_cancelled_by_a_cursor_move() {
        let mut g = Grid::new(4, 2, 0);
        put(&mut g, "abcd");
        g.move_to_col(3);
        put(&mut g, "X");
        assert_eq!(g.row_text(0), "abcX", "no wrap happened");
        assert_eq!(g.row_text(1), "");
    }

    #[test]
    fn filling_the_last_row_does_not_scroll_until_the_next_character() {
        let mut g = Grid::new(3, 2, 10);
        g.move_to(1, 0);
        put(&mut g, "xyz");
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.row_text(1), "xyz");
        put(&mut g, "!");
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.row_text(1), "!");
    }

    // --- wide characters ---------------------------------------------

    #[test]
    fn a_wide_char_occupies_two_cells() {
        let mut g = Grid::new(10, 1, 0);
        put(&mut g, "你好");
        assert_eq!(g.cursor(), Some((0, 4)));
        let row = g.display_row(0);
        assert_eq!(row[0].wide, Wide::Lead);
        assert_eq!(row[1].wide, Wide::Tail);
        assert_eq!(row[2].ch, '好');
        assert_eq!(g.row_text(0), "你好");
        assert_consistent(&g);
    }

    #[test]
    fn overwriting_a_lead_clears_its_tail() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "你好");
        g.move_to_col(0);
        put(&mut g, "a");
        assert_eq!(g.row_text(0), "a 好");
        assert_consistent(&g);
    }

    #[test]
    fn overwriting_a_tail_clears_its_lead() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "你好");
        g.move_to_col(1);
        put(&mut g, "a");
        assert_eq!(g.row_text(0), " a好");
        assert_consistent(&g);
    }

    #[test]
    fn a_wide_char_that_does_not_fit_wraps_whole() {
        let mut g = Grid::new(3, 2, 0);
        put(&mut g, "ab你");
        assert_eq!(g.row_text(0), "ab");
        assert_eq!(g.row_text(1), "你");
        assert_consistent(&g);
    }

    #[test]
    fn combining_marks_never_consume_a_cell() {
        let mut g = Grid::new(4, 1, 0);
        put(&mut g, "e\u{301}x");
        assert_eq!(g.row_text(0), "ex", "the mark is dropped, not spaced");
        assert_eq!(g.cursor(), Some((0, 2)));
    }

    #[test]
    fn the_width_table_knows_the_common_blocks() {
        for ch in ['a', 'é', '→', '\u{a0}'] {
            assert_eq!(char_width(ch), 1, "{ch:?}");
        }
        for ch in ['漢', 'あ', 'カ', '한', 'Ａ', '😀'] {
            assert_eq!(char_width(ch), 2, "{ch:?}");
        }
        for ch in ['\u{301}', '\u{200d}', '\u{fe0f}', '\u{0}'] {
            assert_eq!(char_width(ch), 0, "{ch:?}");
        }
    }

    // --- runs ---------------------------------------------------------

    #[test]
    fn an_empty_row_produces_no_runs() {
        let g = Grid::new(8, 2, 0);
        assert!(runs(&g, 0).is_empty());
    }

    #[test]
    fn adjacent_cells_of_one_style_are_one_run() {
        let mut g = Grid::new(10, 1, 0);
        put(&mut g, "ab");
        let red = Style {
            fg: CellColor::Indexed(1),
            ..Style::default()
        };
        g.set_pen(red);
        put(&mut g, "cd");
        let r = runs(&g, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].text, "ab");
        assert_eq!(r[1].text, "cd");
        assert_eq!(r[1].col, 2);
        assert_eq!(r[1].style, red);
    }

    #[test]
    fn a_run_counts_a_wide_tail_in_its_columns() {
        let mut g = Grid::new(10, 1, 0);
        put(&mut g, "你a");
        let r = runs(&g, 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "你a");
        assert_eq!(r[0].cols, 3);
    }

    #[test]
    fn trailing_blanks_are_dropped_but_styled_ones_are_not() {
        let mut g = Grid::new(8, 1, 0);
        put(&mut g, "ab");
        let r = runs(&g, 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].cols, 2);

        let bg = Style {
            bg: CellColor::Rgb(1, 2, 3),
            ..Style::default()
        };
        g.set_pen(bg);
        put(&mut g, "  ");
        let r = runs(&g, 0);
        assert_eq!(r.len(), 2, "coloured spaces are content");
        assert_eq!(r[1].cols, 2);
    }

    #[test]
    fn runs_are_appended_to_the_caller_s_vec() {
        let mut g = Grid::new(4, 2, 0);
        put(&mut g, "ab");
        let mut out = Vec::new();
        g.row_runs(0, &mut out);
        g.row_runs(0, &mut out);
        assert_eq!(out.len(), 2);
    }

    // --- erasing and editing ------------------------------------------

    #[test]
    fn erase_in_line_honours_its_three_modes() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "abcdef");
        g.move_to_col(3);
        g.erase_in_line(0);
        assert_eq!(g.row_text(0), "abc");

        put(&mut g, "XYZ");
        g.move_to_col(3);
        g.erase_in_line(1);
        assert_eq!(g.row_text(0), "    YZ");

        g.erase_in_line(2);
        assert_eq!(g.row_text(0), "");
    }

    #[test]
    fn erase_in_display_honours_its_four_modes() {
        let mut g = Grid::new(4, 3, 10);
        for row in 0..3 {
            g.move_to(row, 0);
            put(&mut g, "abcd");
        }
        g.move_to(1, 2);
        g.erase_in_display(0);
        assert_eq!(g.text(), "abcd\nab\n");

        g.move_to(1, 1);
        g.erase_in_display(1);
        assert_eq!(g.text(), "\n\n");

        g.move_to(0, 0);
        put(&mut g, "zz");
        g.erase_in_display(2);
        assert_eq!(g.text(), "\n\n");
    }

    #[test]
    fn erase_in_display_three_also_drops_the_scrollback() {
        let mut g = Grid::new(4, 2, 10);
        for _ in 0..5 {
            put(&mut g, "ab");
            g.line_feed();
            g.carriage_return();
        }
        assert!(g.scrollback_len() > 0);
        g.erase_in_display(3);
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn insert_and_delete_chars_shift_the_row() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "abcdef");
        g.move_to_col(1);
        g.insert_chars(2);
        assert_eq!(g.row_text(0), "a  bcd");
        g.delete_chars(2);
        assert_eq!(g.row_text(0), "abcd");
    }

    #[test]
    fn erase_chars_blanks_in_place() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "abcdef");
        g.move_to_col(2);
        g.erase_chars(2);
        assert_eq!(g.row_text(0), "ab  ef");
        g.move_to_col(2);
        g.erase_chars(99);
        assert_eq!(g.row_text(0), "ab", "a huge count is clamped");
    }

    #[test]
    fn shifting_a_row_never_leaves_half_a_wide_char() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "你好");
        g.move_to_col(1);
        g.delete_chars(1);
        assert_consistent(&g);
        assert_eq!(g.row_text(0), " 好");
    }

    // --- back colour erase (BCE) ---------------------------------------

    /// The cells of a display row, as a `Vec` so a test can index freely.
    fn cells(grid: &Grid, row: usize) -> Vec<Cell> {
        grid.display_row(row).to_vec()
    }

    #[test]
    fn an_erase_carries_the_pens_background() {
        let mut g = Grid::new(10, 2, 0);
        put(&mut g, "abc");
        g.set_pen(Style {
            bg: CellColor::Indexed(4),
            ..Style::default()
        });
        g.move_to_col(3);
        g.erase_in_line(0);
        for (col, cell) in cells(&g, 0).into_iter().enumerate().skip(3) {
            assert_eq!(cell.ch, ' ', "col {col}");
            assert_eq!(cell.style.bg, CellColor::Indexed(4), "col {col}");
            assert_eq!(cell.style.fg, CellColor::Default, "col {col}");
            assert_eq!(cell.style.attrs, Attrs::NONE, "col {col}");
        }
        // And the row now reaches the right margin: that is the bar.
        let r = runs(&g, 0);
        assert_eq!(r.last().unwrap().col + r.last().unwrap().cols, 10);
    }

    #[test]
    fn an_erase_under_a_default_pen_is_still_a_plain_blank() {
        let mut g = Grid::new(10, 2, 0);
        put(&mut g, "abc");
        g.move_to_col(0);
        g.erase_in_line(2);
        assert!(cells(&g, 0).iter().all(Cell::is_blank));
        assert_eq!(trimmed_len(g.display_row_raw(0)), 0);
        assert!(runs(&g, 0).is_empty(), "an idle row still costs nothing");
    }

    #[test]
    fn an_erase_under_an_inverse_pen_erases_with_the_foreground() {
        let mut g = Grid::new(6, 1, 0);
        g.set_pen(Style {
            fg: CellColor::Indexed(3),
            attrs: Attrs::INVERSE,
            ..Style::default()
        });
        g.erase_in_line(2);
        for cell in cells(&g, 0) {
            assert_eq!(cell.style.bg, CellColor::Indexed(3));
            assert_eq!(cell.style.attrs, Attrs::NONE);
        }

        // With default colours there is no `CellColor` for "the terminal's
        // foreground", so the inverse bit survives and the viewer
        // resolves it.
        let mut g = Grid::new(6, 1, 0);
        g.set_pen(Style {
            attrs: Attrs::INVERSE,
            ..Style::default()
        });
        g.erase_in_line(2);
        for cell in cells(&g, 0) {
            assert_eq!(cell.style.bg, CellColor::Default);
            assert_eq!(cell.style.fg, CellColor::Default);
            assert_eq!(cell.style.attrs, Attrs::INVERSE);
            assert!(!cell.is_blank(), "an inverse erase is visible");
        }
    }

    #[test]
    fn an_erase_drops_the_pens_other_attributes() {
        let mut g = Grid::new(6, 1, 0);
        let mut attrs = Attrs::BOLD;
        attrs.insert(Attrs::UNDERLINE);
        g.set_pen(Style {
            fg: CellColor::Indexed(1),
            bg: CellColor::Rgb(1, 2, 3),
            attrs,
        });
        g.erase_in_line(2);
        for cell in cells(&g, 0) {
            assert_eq!(cell.style.bg, CellColor::Rgb(1, 2, 3));
            assert_eq!(cell.style.fg, CellColor::Default);
            assert_eq!(
                cell.style.attrs,
                Attrs::NONE,
                "an underline must not stretch to the margin"
            );
        }
    }

    #[test]
    fn a_scroll_blanks_the_new_row_with_the_pen() {
        let pen = Style {
            bg: CellColor::Indexed(2),
            ..Style::default()
        };

        // LF at the bottom of the screen: the recycled row is painted.
        let mut g = Grid::new(4, 3, 10);
        put(&mut g, "top");
        g.set_pen(pen);
        g.move_to(2, 0);
        g.line_feed();
        for cell in cells(&g, 2) {
            assert_eq!(cell.style.bg, CellColor::Indexed(2));
        }

        // RI at the top: same, at the other end.
        let mut g = Grid::new(4, 3, 0);
        g.set_pen(pen);
        g.move_to(0, 0);
        g.reverse_line_feed();
        for cell in cells(&g, 0) {
            assert_eq!(cell.style.bg, CellColor::Indexed(2));
        }

        // And inside a scroll region, via DL and IL.
        let mut g = Grid::new(4, 4, 0);
        g.set_scroll_region(Some((1, 2)));
        g.set_pen(pen);
        g.move_to(1, 0);
        g.delete_lines(1);
        for cell in cells(&g, 2) {
            assert_eq!(cell.style.bg, CellColor::Indexed(2));
        }
        g.move_to(1, 0);
        g.insert_lines(1);
        for cell in cells(&g, 1) {
            assert_eq!(cell.style.bg, CellColor::Indexed(2));
        }
    }

    #[test]
    fn ich_and_dch_open_and_close_with_the_pens_background() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "abcdef");
        g.set_pen(Style {
            bg: CellColor::Indexed(5),
            ..Style::default()
        });
        g.move_to_col(1);
        g.insert_chars(2);
        assert_eq!(g.row_text(0), "a  bcd");
        for col in 1..3 {
            assert_eq!(cells(&g, 0)[col].style.bg, CellColor::Indexed(5));
        }
        g.delete_chars(2);
        // The cells pulled in at the right margin are erased too.
        for col in 4..6 {
            assert_eq!(cells(&g, 0)[col].style.bg, CellColor::Indexed(5));
        }
    }

    #[test]
    fn a_resize_adds_plain_blank_rows() {
        // A window drag is not a program's erase: whatever pen happens to
        // be in force is an accident, so the new rows stay plain.
        let mut g = Grid::new(6, 2, 0);
        g.set_pen(Style {
            bg: CellColor::Indexed(4),
            ..Style::default()
        });
        g.resize(8, 4);
        for row in 0..4 {
            assert!(
                cells(&g, row).iter().all(Cell::is_blank),
                "row {row} should be plain"
            );
        }

        // Nor is RIS, nor entering the alternate screen.
        let mut g = Grid::new(6, 2, 0);
        g.set_pen(Style {
            bg: CellColor::Indexed(4),
            ..Style::default()
        });
        g.set_alt_screen(true);
        assert!(cells(&g, 0).iter().all(Cell::is_blank));
        g.set_alt_screen(false);
        g.reset();
        assert!(cells(&g, 0).iter().all(Cell::is_blank));
    }

    #[test]
    fn insert_and_delete_lines_move_rows_within_the_region() {
        let mut g = Grid::new(4, 4, 0);
        for row in 0..4 {
            g.move_to(row, 0);
            put(&mut g, &format!("r{row}"));
        }
        g.move_to(1, 0);
        g.insert_lines(1);
        assert_eq!(g.text(), "r0\n\nr1\nr2");
        g.move_to(1, 0);
        g.delete_lines(1);
        assert_eq!(g.text(), "r0\nr1\nr2\n");
    }

    // --- scroll region -------------------------------------------------

    #[test]
    fn a_line_feed_at_the_region_bottom_scrolls_only_the_region() {
        let mut g = Grid::new(4, 4, 10);
        for row in 0..4 {
            g.move_to(row, 0);
            put(&mut g, &format!("r{row}"));
        }
        g.set_scroll_region(Some((1, 2)));
        g.move_to(2, 0);
        g.line_feed();
        put(&mut g, "new");
        assert_eq!(g.text(), "r0\nr2\nnew\nr3");
        assert_eq!(g.scrollback_len(), 0, "a region scroll is not history");
    }

    #[test]
    fn an_invalid_scroll_region_is_ignored() {
        let mut g = Grid::new(4, 4, 0);
        g.set_scroll_region(Some((2, 1)));
        assert_eq!(g.scroll_region(), (0, 3));
        g.set_scroll_region(Some((0, 99)));
        assert_eq!(g.scroll_region(), (0, 3));
        g.set_scroll_region(Some((1, 2)));
        assert_eq!(g.scroll_region(), (1, 2));
        g.set_scroll_region(None);
        assert_eq!(g.scroll_region(), (0, 3));
    }

    #[test]
    fn reverse_line_feed_scrolls_the_region_down_at_its_top() {
        let mut g = Grid::new(4, 3, 0);
        for row in 0..3 {
            g.move_to(row, 0);
            put(&mut g, &format!("r{row}"));
        }
        g.move_to(0, 0);
        g.reverse_line_feed();
        assert_eq!(g.text(), "\nr0\nr1");
        assert_eq!(g.cursor(), Some((0, 0)));
    }

    #[test]
    fn su_and_sd_move_the_region_without_the_cursor() {
        let mut g = Grid::new(4, 3, 10);
        for row in 0..3 {
            g.move_to(row, 0);
            put(&mut g, &format!("r{row}"));
        }
        g.move_to(1, 1);
        g.scroll_region_up(1);
        assert_eq!(g.text(), "r1\nr2\n");
        assert_eq!(g.cursor(), Some((1, 1)));
        g.scroll_region_down(1);
        assert_eq!(g.text(), "\nr1\nr2");
    }

    // --- scrollback -----------------------------------------------------

    #[test]
    fn the_scrollback_ring_keeps_the_newest_lines() {
        let mut g = Grid::new(8, 24, 10);
        for i in 0..100 {
            put(&mut g, &format!("line {i}"));
            g.line_feed();
            g.carriage_return();
        }
        assert_eq!(g.scrollback_len(), 10);
        g.scroll_up(10);
        // 100 line feeds, 23 of which only moved the cursor down: lines 0
        // through 76 were pushed, and the ring kept the last ten.
        assert_eq!(g.row_text(0), "line 67", "oldest kept line");
        assert_eq!(g.row_text(9), "line 76", "newest line in history");
    }

    #[test]
    fn a_zero_capacity_scrollback_keeps_nothing() {
        let mut g = Grid::new(8, 2, 0);
        for _ in 0..5 {
            g.line_feed();
        }
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn scrolling_the_view_clamps_at_both_ends() {
        let mut g = Grid::new(8, 2, 4);
        for i in 0..6 {
            put(&mut g, &format!("{i}"));
            g.line_feed();
            g.carriage_return();
        }
        assert_eq!(g.scrollback_len(), 4);
        g.scroll_up(99);
        assert_eq!(g.scroll_offset(), 4);
        // "0" fell off the end of the four-line ring.
        assert_eq!(g.row_text(0), "1");
        g.scroll_down(1);
        assert_eq!(g.scroll_offset(), 3);
        assert_eq!(g.row_text(0), "2");
        g.scroll_down(99);
        assert_eq!(g.scroll_offset(), 0);
        g.scroll_up(2);
        g.scroll_to_bottom();
        assert_eq!(g.scroll_offset(), 0);
    }

    #[test]
    fn the_cursor_disappears_when_it_is_scrolled_out_of_view() {
        let mut g = Grid::new(8, 2, 8);
        for _ in 0..4 {
            g.line_feed();
        }
        assert!(g.cursor().is_some());
        g.scroll_up(4);
        assert_eq!(g.cursor(), None);
        assert_eq!(g.cursor_screen(), (1, 0), "the program is still there");
        g.scroll_to_bottom();
        assert!(g.cursor().is_some());
    }

    #[test]
    fn output_while_scrolled_back_does_not_snap_the_view() {
        let mut g = Grid::new(8, 2, 8);
        for i in 0..4 {
            put(&mut g, &format!("{i}"));
            g.line_feed();
            g.carriage_return();
        }
        g.scroll_up(2);
        let before = g.scroll_offset();
        put(&mut g, "new");
        assert_eq!(g.scroll_offset(), before);
    }

    // --- damage ----------------------------------------------------------

    #[test]
    fn one_character_damages_one_row_and_one_column() {
        let mut g = Grid::new(10, 3, 0);
        g.clear_damage();
        assert!(!g.dirty());
        g.move_to(1, 4);
        put(&mut g, "x");
        assert_eq!(g.row_damage(1), Some((4, 5)));
        assert!(!g.row_dirty(0) && !g.row_dirty(2));
        assert!(g.dirty());
        g.clear_damage();
        assert_eq!(g.row_damage(1), None);
    }

    #[test]
    fn damage_spans_widen_to_cover_every_write() {
        let mut g = Grid::new(10, 1, 0);
        g.clear_damage();
        g.move_to_col(1);
        put(&mut g, "a");
        g.move_to_col(7);
        put(&mut g, "b");
        assert_eq!(g.row_damage(0), Some((1, 8)));
    }

    #[test]
    fn a_full_screen_redraw_damages_every_row() {
        let mut g = Grid::new(4, 3, 0);
        g.clear_damage();
        for row in 0..3 {
            g.move_to(row, 0);
            put(&mut g, "abcd");
        }
        for row in 0..3 {
            assert_eq!(g.row_damage(row), Some((0, 4)));
        }
        g.clear_damage();
        g.damage_all();
        assert!((0..3).all(|r| g.row_dirty(r)));
    }

    #[test]
    fn a_scroll_damages_the_whole_region() {
        let mut g = Grid::new(4, 4, 4);
        g.set_scroll_region(Some((1, 2)));
        g.clear_damage();
        g.move_to(2, 0);
        g.line_feed();
        assert!(!g.row_dirty(0) && !g.row_dirty(3));
        assert!(g.row_dirty(1) && g.row_dirty(2));
    }

    #[test]
    fn damage_while_scrolled_back_stays_in_display_space() {
        let mut g = Grid::new(4, 2, 8);
        for _ in 0..4 {
            g.line_feed();
        }
        g.scroll_up(1);
        g.clear_damage();
        g.move_to(0, 1);
        put(&mut g, "x");
        assert_eq!(g.row_damage(1), Some((1, 2)), "screen row 0 shows as row 1");
        assert!(!g.row_dirty(0));

        g.clear_damage();
        g.move_to(1, 0);
        g.line_feed();
        assert!(
            g.row_dirty(0) && g.row_dirty(1),
            "a scroll moves everything"
        );
    }

    #[test]
    fn scrolling_the_view_damages_everything_but_a_clamped_scroll_does_not() {
        let mut g = Grid::new(4, 2, 8);
        for _ in 0..4 {
            g.line_feed();
        }
        g.clear_damage();
        g.scroll_up(1);
        assert!(g.dirty());
        g.clear_damage();
        g.scroll_down(99);
        assert!(g.dirty());
        g.clear_damage();
        g.scroll_down(1);
        assert!(!g.dirty(), "already at the bottom");
    }

    // --- cursor, alt screen, reset ---------------------------------------

    #[test]
    fn save_and_restore_carry_the_pen_along() {
        let mut g = Grid::new(8, 4, 0);
        g.move_to(2, 3);
        let mut pen = Style::default();
        pen.attrs.insert(Attrs::BOLD);
        g.set_pen(pen);
        g.save_cursor();
        g.move_to(0, 0);
        g.set_pen(Style::default());
        g.restore_cursor();
        assert_eq!(g.cursor(), Some((2, 3)));
        assert_eq!(g.pen(), pen);
    }

    #[test]
    fn the_alt_screen_leaves_the_primary_exactly_as_it_was() {
        let mut g = Grid::new(8, 2, 10);
        put(&mut g, "shell");
        g.move_to(1, 2);
        let before = g.text();
        g.set_alt_screen(true);
        assert_eq!(g.text(), "\n");
        put(&mut g, "editor");
        g.set_alt_screen(false);
        assert_eq!(g.text(), before);
        assert_eq!(g.cursor(), Some((1, 2)));
        assert!(!g.alt_screen());
    }

    #[test]
    fn the_alt_screen_has_no_scrollback() {
        let mut g = Grid::new(4, 2, 10);
        for _ in 0..5 {
            g.line_feed();
        }
        let history = g.scrollback_len();
        assert!(history > 0);
        g.set_alt_screen(true);
        assert_eq!(g.scrollback_len(), 0);
        for _ in 0..5 {
            g.line_feed();
        }
        assert_eq!(g.scrollback_len(), 0);
        g.scroll_up(3);
        assert_eq!(g.scroll_offset(), 0, "scrolling the alt screen is a no-op");
        g.set_alt_screen(false);
        assert_eq!(g.scrollback_len(), history, "the primary history survived");
    }

    #[test]
    fn reset_puts_everything_back() {
        let mut g = Grid::new(4, 2, 10);
        put(&mut g, "abc");
        g.set_alt_screen(true);
        g.set_cursor_visible(false);
        g.set_scroll_region(Some((0, 1)));
        g.set_pen(Style {
            fg: CellColor::Indexed(9),
            ..Style::default()
        });
        g.reset();
        assert_eq!(g.text(), "\n");
        assert!(!g.alt_screen() && g.cursor_visible());
        assert_eq!(g.scrollback_len(), 0);
        assert_eq!(g.cursor(), Some((0, 0)));
        assert_eq!(g.scroll_region(), (0, 1), "reset keeps the screen size");
    }

    #[test]
    fn cursor_visibility_is_reported() {
        let mut g = Grid::new(4, 2, 0);
        assert!(g.cursor_visible());
        g.set_cursor_visible(false);
        assert!(!g.cursor_visible());
    }

    // --- resize -----------------------------------------------------------

    #[test]
    fn growing_the_screen_adds_blank_rows_at_the_bottom() {
        let mut g = Grid::new(4, 2, 10);
        put(&mut g, "ab");
        g.resize(6, 4);
        assert_eq!(g.cols(), 6);
        assert_eq!(g.rows(), 4);
        assert_eq!(g.row_text(0), "ab");
        assert_eq!(g.scroll_region(), (0, 3));
        assert_consistent(&g);
    }

    #[test]
    fn shrinking_pushes_lines_off_the_top_to_keep_the_cursor() {
        let mut g = Grid::new(4, 4, 10);
        for row in 0..4 {
            g.move_to(row, 0);
            put(&mut g, &format!("r{row}"));
        }
        g.move_to(3, 0);
        g.resize(4, 2);
        assert_eq!(g.text(), "r2\nr3");
        assert_eq!(g.cursor(), Some((1, 0)));
        assert_eq!(g.scrollback_len(), 2);
    }

    #[test]
    fn shrinking_with_the_cursor_high_drops_rows_from_the_bottom() {
        let mut g = Grid::new(4, 4, 10);
        for row in 0..4 {
            g.move_to(row, 0);
            put(&mut g, &format!("r{row}"));
        }
        g.move_to(0, 0);
        g.resize(4, 2);
        assert_eq!(g.text(), "r0\nr1");
        assert_eq!(g.scrollback_len(), 0);
    }

    #[test]
    fn narrowing_truncates_rows_and_repairs_wide_pairs() {
        let mut g = Grid::new(6, 1, 0);
        put(&mut g, "ab你好");
        g.resize(3, 1);
        assert_eq!(g.row_text(0), "ab");
        assert_consistent(&g);
    }

    #[test]
    fn a_degenerate_size_is_clamped_to_one_cell() {
        let mut g = Grid::new(0, 0, 0);
        assert_eq!((g.cols(), g.rows()), (1, 1));
        put(&mut g, "你");
        assert_eq!(g.row_text(0), "你", "a wide char in one column is narrowed");
        g.resize(0, 0);
        assert_eq!((g.cols(), g.rows()), (1, 1));
    }

    #[test]
    fn scrollback_lines_are_stored_trimmed_but_read_back_full_width() {
        // The box run measured 29.4 MB of RSS with a full 10 000-line
        // buffer against a 6 MB target, and the arithmetic said why:
        // 10 000 rows x 80 cells x 16 bytes is 12.8 MB, nearly all of it
        // recording that nothing is there. A scrollback line is
        // immutable, so its trailing blanks carry no information.
        //
        // Both halves matter: the storage is short, and every reader
        // still sees a full-width row.
        let mut g = Grid::new(80, 2, 10);
        for _ in 0..5 {
            put(&mut g, "hi");
            g.carriage_return();
            g.line_feed();
        }
        assert!(g.scrollback_len() >= 3);
        // A live row is full width: it is written to everywhere, so
        // there is nothing safe to trim. Check that before scrolling,
        // because at offset 0 the display *is* the live screen.
        assert_eq!(g.display_row_raw(0).len(), g.cols());

        // Now bring the history into view. Row 0 is then a stored
        // scrollback line, and it is stored short...
        g.scroll_up(2);
        assert_eq!(
            g.display_row_raw(0).len(),
            2,
            "a scrollback row keeps only its significant cells"
        );
        // ...and read back full width, with the same text.
        assert_eq!(g.display_row(0).len(), g.cols());
        assert_eq!(g.row_text(0), "hi");
        assert!(g.display_row(0)[40].is_blank());
        assert_consistent(&g);
    }

    #[test]
    fn a_full_scrollback_costs_what_its_text_costs() {
        // The number the box run put a price on. 10 000 lines of a few
        // characters each must cost about what those characters cost —
        // not 10 000 full-width rows.
        //
        // This is a *resident memory* assertion, which is unusual in a
        // unit test and is here because the bug it pins was invisible to
        // every other kind. The rows really were trimmed, and RSS still
        // grew by a full row per line: `shrink_to_fit` left the
        // allocator holding a full-width hole that the next blank row,
        // one cell longer than the trimmed one, could not reuse. Only a
        // measurement of the process could tell the two apart.
        fn rss_kb() -> usize {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS"))?
                        .split_whitespace()
                        .nth(1)?
                        .parse()
                        .ok()
                })
                .unwrap_or(0)
        }
        if rss_kb() == 0 {
            return; // not Linux, or no procfs: nothing to assert on.
        }
        let before = rss_kb();
        let mut g = Grid::new(90, 24, 10_000);
        for i in 1..=10_000u32 {
            for c in i.to_string().chars() {
                g.put_char(c);
            }
            g.carriage_return();
            g.line_feed();
        }
        let grown = rss_kb().saturating_sub(before);
        assert!(g.scrollback_len() > 9_000, "the buffer really filled");
        // Full-width storage is 90 x 16 x 10 000 = 14 MB. The text is
        // well under 1 MB. The threshold sits between the two, far
        // enough from both that allocator noise cannot reach it.
        assert!(
            grown < 4_000,
            "10 000 short lines grew RSS by {grown} kB; trimmed storage \
             should cost about a tenth of that, and full-width rows \
             would cost 14 MB"
        );
    }

    #[test]
    fn a_narrowed_grid_clips_its_history_rather_than_reflowing_it() {
        // No rewrap of scrollback is a documented limitation; what must
        // not happen is a row that reads back wider than the grid.
        let mut g = Grid::new(20, 2, 10);
        put(&mut g, "0123456789abcdef");
        g.carriage_return();
        g.line_feed();
        g.resize(8, 2);
        g.scroll_up(1);
        assert_eq!(g.display_row(0).len(), 8);
        assert_eq!(g.row_text(0), "01234567");
        assert_consistent(&g);
    }

    #[test]
    fn resize_to_the_same_size_is_a_no_op() {
        let mut g = Grid::new(4, 2, 0);
        put(&mut g, "ab");
        g.clear_damage();
        g.resize(4, 2);
        assert!(!g.dirty());
    }
}
