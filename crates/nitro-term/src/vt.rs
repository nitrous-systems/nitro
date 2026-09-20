//! The VT parser: bytes from the pty in, [`Grid`] operations out.
//!
//! [`vte`] owns the state machine — the DEC ANSI table that says which
//! byte starts an escape sequence and which finishes it — and this module
//! owns every decision about what a finished sequence *means*. That
//! division is deliberate and is the whole reason the dependency is worth
//! it: the table is fiddly, well-tested elsewhere and never changes,
//! while the meanings are ours to get right and to test against the grid.
//!
//! The rule for anything not understood is to ignore it, silently and
//! without moving the cursor. A terminal that garbled its screen on an
//! unknown DCS would be worse than one that dropped it, and a terminal
//! that panicked on one would take the whole compositor session with it —
//! so [`Term::feed`] is written to be total over arbitrary bytes, and a
//! test feeds it noise to prove it.
//!
//! State that is not the grid's lives here: the window title from OSC,
//! and the two DECSET modes the input layer needs (application cursor
//! keys and bracketed paste). Replies the terminal owes the program —
//! cursor position reports, device attributes — are queued in a buffer
//! the caller drains and writes back to the pty, rather than written
//! here, because this module has no business owning a file descriptor.

use vte::{Params, Parser, Perform};

use crate::grid::{Attrs, CellColor, Grid, Style};

/// A terminal: the parser, the grid, and the bits of state the rest of
/// the app needs (title, application-cursor mode, bracketed paste).
pub struct Term {
    parser: Parser,
    inner: Inner,
}

/// Everything the parser mutates.
///
/// Split from [`Term`] so that [`Term::feed`] can hand `vte` a `&mut` to
/// the state while still holding `&mut` on the parser: the two are
/// disjoint fields, which is the cheapest way to satisfy the borrow
/// checker without an `Option` dance or interior mutability.
struct Inner {
    grid: Grid,
    /// Set by OSC 0/2, taken by the caller.
    title: Option<String>,
    /// Bytes owed to the program on the other end of the pty.
    replies: Vec<u8>,
    /// DECSET 1.
    app_cursor: bool,
    /// DECSET 2004.
    bracketed_paste: bool,
}

impl Term {
    /// A terminal showing a blank `cols` by `rows` grid with `scrollback`
    /// lines of history.
    #[must_use]
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        Self {
            parser: Parser::new(),
            inner: Inner {
                grid: Grid::new(cols, rows, scrollback),
                title: None,
                replies: Vec::new(),
                app_cursor: false,
                bracketed_paste: false,
            },
        }
    }

    /// The grid, for a viewer.
    #[must_use]
    pub fn grid(&self) -> &Grid {
        &self.inner.grid
    }

    /// The grid, for the parts of the app that drive it directly —
    /// clearing damage after a repaint, scrolling the view, selecting.
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.inner.grid
    }

    /// Feed bytes from the pty.
    ///
    /// Never panics, whatever the bytes are: a pty carries whatever the
    /// program writes, including binary files `cat`ted by accident.
    /// Partial UTF-8 and half-finished escape sequences are fine too —
    /// the parser is a state machine across calls, so a sequence split
    /// across two reads resumes rather than resets.
    pub fn feed(&mut self, bytes: &[u8]) {
        let Self { parser, inner } = self;
        parser.advance(inner, bytes);
    }

    /// Resize the screen, as a window resize or a `TIOCSWINSZ` does.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.inner.grid.resize(cols, rows);
    }

    /// The window title set by OSC 0/2 since the last call, if it
    /// changed.
    ///
    /// Taking rather than reading: the caller sets a window property with
    /// it, and doing that once per change is the point of the `Option`.
    pub fn take_title(&mut self) -> Option<String> {
        self.inner.title.take()
    }

    /// Bytes the terminal owes the pty (DSR/DA replies).
    ///
    /// Queued rather than written because this module holds no file
    /// descriptor; the caller drains this after each `feed` and writes it
    /// back, which also keeps the reply in the same event-loop turn as
    /// the request that produced it.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.inner.replies)
    }

    /// DECSET 1 — arrows send SS3 rather than CSI.
    #[must_use]
    pub fn application_cursor(&self) -> bool {
        self.inner.app_cursor
    }

    /// DECSET 2004 — paste is bracketed.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.inner.bracketed_paste
    }

    /// DECSET 1049 — the alternate screen is showing.
    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.inner.grid.alt_screen()
    }
}

/// The `n`th CSI parameter, or `default` when it is absent or zero.
///
/// Zero means "default" for nearly every CSI parameter — `CSI 0 A` moves
/// up one row, not none — so the two cases collapse here. The handful of
/// parameters where zero is a real value (SGR, the mode numbers, ED/EL)
/// read the params directly instead.
fn arg(params: &Params, n: usize, default: u16) -> u16 {
    match params.iter().nth(n).and_then(|p| p.first().copied()) {
        Some(0) | None => default,
        Some(v) => v,
    }
}

/// The `n`th CSI parameter with zero left alone, for ED/EL and modes.
fn arg_raw(params: &Params, n: usize) -> u16 {
    params
        .iter()
        .nth(n)
        .and_then(|p| p.first().copied())
        .unwrap_or(0)
}

/// A parameter as a count of cells, clamped into `usize`.
fn count(params: &Params, n: usize) -> usize {
    arg(params, n, 1) as usize
}

/// A parameter as a signed number of steps, for the relative moves.
///
/// `isize` by conversion rather than a cast, so that no arithmetic on a
/// parameter a program chose can wrap; the grid clamps to the screen
/// anyway, so the magnitude beyond a screen's worth is immaterial.
fn steps(params: &Params, n: usize) -> isize {
    isize::try_from(arg(params, n, 1)).unwrap_or(isize::MAX)
}

/// A 1-based VT coordinate as a 0-based grid index.
fn index(params: &Params, n: usize) -> usize {
    (arg(params, n, 1) as usize).saturating_sub(1)
}

/// Narrow a colour component; a program that sends 300 gets white.
fn component(v: u16) -> u8 {
    v.min(255) as u8
}

impl Inner {
    /// Queue a reply to the program.
    fn reply(&mut self, bytes: &[u8]) {
        self.replies.extend_from_slice(bytes);
    }

    /// SGR: fold the parameters into the pen.
    ///
    /// The two extended-colour forms have to be handled together.
    /// `38;5;n` spreads the colour over three *parameters*, while
    /// `38:5:n` arrives as one parameter with sub-parameters — the same
    /// colour, two encodings, both in the wild (the colon form is what
    /// modern libvte and `tmux` emit). `vte` hands each parameter over as
    /// a `&[u16]`, so the sub-parameter form is the slice being longer
    /// than one, and the semicolon form is reading ahead in the iterator.
    fn sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.grid.set_pen(Style::default());
            return;
        }
        let mut style = self.grid.pen();
        let mut iter = params.iter();
        while let Some(param) = iter.next() {
            let Some(&code) = param.first() else { continue };
            match code {
                0 => style = Style::default(),
                1 => style.attrs.insert(Attrs::BOLD),
                3 => style.attrs.insert(Attrs::ITALIC),
                4 => style.attrs.insert(Attrs::UNDERLINE),
                7 => style.attrs.insert(Attrs::INVERSE),
                // 21 is "doubly underlined" in ECMA-48 and "bold off" in
                // much older practice; treating it as bold off is what
                // the programs that still send it mean.
                21 | 22 => style.attrs.remove(Attrs::BOLD),
                23 => style.attrs.remove(Attrs::ITALIC),
                24 => style.attrs.remove(Attrs::UNDERLINE),
                27 => style.attrs.remove(Attrs::INVERSE),
                30..=37 => style.fg = CellColor::Indexed((code - 30) as u8),
                38 => {
                    if let Some(c) = extended_color(param, &mut iter) {
                        style.fg = c;
                    }
                }
                39 => style.fg = CellColor::Default,
                40..=47 => style.bg = CellColor::Indexed((code - 40) as u8),
                48 => {
                    if let Some(c) = extended_color(param, &mut iter) {
                        style.bg = c;
                    }
                }
                49 => style.bg = CellColor::Default,
                90..=97 => style.fg = CellColor::Indexed((code - 90 + 8) as u8),
                100..=107 => style.bg = CellColor::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
        }
        self.grid.set_pen(style);
    }

    /// DECSET/DECRST: the private modes worth honouring.
    ///
    /// Everything else is ignored on purpose. Mouse reporting, origin
    /// mode and the rest are either not implemented or not wanted, and a
    /// terminal that acknowledged a mode it does not implement would have
    /// programs drawing for a terminal that does not exist.
    fn set_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.app_cursor = on,
            25 => self.grid.set_cursor_visible(on),
            // 47 and 1047 switch screens without touching the cursor;
            // 1048 saves the cursor without switching. 1049 is both, and
            // is what every program written in the last twenty years
            // sends. The grid's alt-screen switch implements 1049, so the
            // older three are approximated by it — the difference only
            // shows for a program that mixes them, which none do.
            47 | 1047 | 1049 => self.grid.set_alt_screen(on),
            1048 => {
                if on {
                    self.grid.save_cursor();
                } else {
                    self.grid.restore_cursor();
                }
            }
            2004 => self.bracketed_paste = on,
            _ => {}
        }
    }
}

/// The colour named by an SGR `38`/`48` parameter.
///
/// `param` is the `38` (or `48`) parameter itself, which in the colon
/// form already carries the whole colour as sub-parameters; `iter` is the
/// parameter stream, which in the semicolon form carries the rest.
/// Returns `None` for a form we do not recognise, leaving the pen alone —
/// the alternative, guessing, paints the screen a colour nobody asked
/// for.
fn extended_color(param: &[u16], iter: &mut vte::ParamsIter<'_>) -> Option<CellColor> {
    if param.len() > 1 {
        // Colon form: `38:5:n` or `38:2:<colorspace>:r:g:b`, where the
        // colorspace id is usually empty and `vte` reports it as 0. Both
        // the five- and six-element shapes are seen in the wild.
        return match param[1] {
            5 => param.get(2).map(|&n| CellColor::Indexed(component(n))),
            2 => match param.len() {
                5 => Some(CellColor::Rgb(
                    component(param[2]),
                    component(param[3]),
                    component(param[4]),
                )),
                n if n >= 6 => Some(CellColor::Rgb(
                    component(param[3]),
                    component(param[4]),
                    component(param[5]),
                )),
                _ => None,
            },
            _ => None,
        };
    }
    // Semicolon form: the kind and the components are separate params.
    let next = |iter: &mut vte::ParamsIter<'_>| iter.next().and_then(|p| p.first().copied());
    match next(iter)? {
        5 => Some(CellColor::Indexed(component(next(iter)?))),
        2 => {
            let r = component(next(iter)?);
            let g = component(next(iter)?);
            let b = component(next(iter)?);
            Some(CellColor::Rgb(r, g, b))
        }
        _ => None,
    }
}

impl Perform for Inner {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.grid.backspace(),
            0x09 => self.grid.tab(),
            // LF, VT and FF all index: a vertical tab in a terminal has
            // meant "line feed" since hardware terminals stopped having
            // vertical tab stops.
            0x0A..=0x0C => self.grid.line_feed(),
            0x0D => self.grid.carriage_return(),
            // BEL is dropped rather than forwarded: an audible bell needs
            // a sound server and a visual one needs the compositor, and
            // neither belongs to the VT model.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        // A private marker (`?`, and the rarer `>` and `<`) arrives as an
        // intermediate. Only `?` means anything here, and a sequence
        // carrying any other intermediate is for a device we are not.
        let private = intermediates.first().copied();
        if private == Some(b'?') {
            match action {
                'h' | 'l' => {
                    let on = action == 'h';
                    for p in params {
                        if let Some(&mode) = p.first() {
                            self.set_mode(mode, on);
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if private.is_some() {
            return;
        }
        match action {
            'A' => self.grid.move_by(-steps(params, 0), 0),
            'B' | 'e' => self.grid.move_by(steps(params, 0), 0),
            'C' | 'a' => self.grid.move_by(0, steps(params, 0)),
            'D' => self.grid.move_by(0, -steps(params, 0)),
            'E' => {
                self.grid.move_by(steps(params, 0), 0);
                self.grid.carriage_return();
            }
            'F' => {
                self.grid.move_by(-steps(params, 0), 0);
                self.grid.carriage_return();
            }
            'G' | '`' => self.grid.move_to_col(index(params, 0)),
            'H' | 'f' => {
                let (row, col) = (index(params, 0), index(params, 1));
                self.grid.move_to(row, col);
            }
            'I' => {
                for _ in 0..count(params, 0) {
                    self.grid.tab();
                }
            }
            'J' => self.grid.erase_in_display(arg_raw(params, 0)),
            'K' => self.grid.erase_in_line(arg_raw(params, 0)),
            'L' => self.grid.insert_lines(count(params, 0)),
            'M' => self.grid.delete_lines(count(params, 0)),
            'P' => self.grid.delete_chars(count(params, 0)),
            'S' => self.grid.scroll_region_up(count(params, 0)),
            'T' => self.grid.scroll_region_down(count(params, 0)),
            'X' => self.grid.erase_chars(count(params, 0)),
            'Z' => {
                for _ in 0..count(params, 0) {
                    self.grid.back_tab();
                }
            }
            '@' => self.grid.insert_chars(count(params, 0)),
            'd' => self.grid.move_to_row(index(params, 0)),
            'r' => {
                // `CSI r` with no parameters resets the region; anything
                // else names it, 1-based and inclusive.
                if params.is_empty() {
                    self.grid.set_scroll_region(None);
                } else {
                    let top = index(params, 0);
                    let bot = arg(params, 1, self.grid.rows() as u16) as usize;
                    self.grid
                        .set_scroll_region(Some((top, bot.saturating_sub(1))));
                }
            }
            's' => self.grid.save_cursor(),
            'u' => self.grid.restore_cursor(),
            'm' => self.sgr(params),
            'n' => {
                if arg_raw(params, 0) == 6 {
                    // DSR: where the *program* is writing, which is the
                    // screen cursor — not where the user has scrolled to.
                    let (row, col) = self.grid.cursor_screen();
                    let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                    self.reply(reply.as_bytes());
                }
            }
            'c' => {
                // Primary DA. We answer as a VT102: enough for programs
                // to enable colour and cursor addressing, not enough for
                // them to expect sixel or ReGIS.
                self.reply(b"\x1b[?6c");
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        // Charset designations (`ESC ( B` and friends) carry an
        // intermediate; we are UTF-8 only, so they are dropped rather
        // than mistaken for one of the sequences below.
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.grid.save_cursor(),
            b'8' => self.grid.restore_cursor(),
            b'D' => self.grid.line_feed(),
            b'E' => {
                self.grid.line_feed();
                self.grid.carriage_return();
            }
            b'M' => self.grid.reverse_line_feed(),
            b'c' => {
                self.grid.reset();
                self.app_cursor = false;
                self.bracketed_paste = false;
                self.title = None;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let [kind, rest @ ..] = params else { return };
        // 0 sets both the window and the icon title, 2 only the window
        // title, and 1 only the icon title — which a Wayland window does
        // not have, so it is dropped.
        if *kind != b"0" && *kind != b"2" {
            return;
        }
        let Some(text) = rest.first() else { return };
        // Lossy, not a rejection: a title is decoration, and a program
        // that sent a stray byte in one should still get the rest of it.
        self.title = Some(String::from_utf8_lossy(text).into_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{Run, Wide};

    /// A terminal with the given size, fed `bytes`.
    fn term(cols: usize, rows: usize, bytes: &[u8]) -> Term {
        let mut t = Term::new(cols, rows, 0);
        t.feed(bytes);
        t
    }

    fn style_at(t: &Term, row: usize, col: usize) -> Style {
        t.grid().display_row(row)[col].style
    }

    fn runs(t: &Term, row: usize) -> Vec<Run> {
        let mut out = Vec::new();
        t.grid().row_runs(row, &mut out);
        out
    }

    // --- the basics ---------------------------------------------------

    #[test]
    fn plain_text_lands_in_the_grid() {
        let t = term(10, 2, b"hello");
        assert_eq!(t.grid().row_text(0), "hello");
        assert_eq!(t.grid().cursor(), Some((0, 5)));
    }

    #[test]
    fn a_carriage_return_and_line_feed_move_the_cursor() {
        let t = term(10, 3, b"one\r\ntwo");
        assert_eq!(t.grid().row_text(0), "one");
        assert_eq!(t.grid().row_text(1), "two");
        assert_eq!(t.grid().cursor(), Some((1, 3)));
    }

    #[test]
    fn backspace_tab_and_bell_do_what_they_should() {
        let t = term(20, 1, b"ab\x08X\tY\x07Z");
        assert_eq!(t.grid().row_text(0), "aX      YZ");
    }

    #[test]
    fn utf8_split_across_two_feeds_is_reassembled() {
        let mut t = Term::new(10, 1, 0);
        let bytes = "é".as_bytes();
        t.feed(&bytes[..1]);
        t.feed(&bytes[1..]);
        assert_eq!(t.grid().row_text(0), "é");
    }

    // --- cursor motion -------------------------------------------------

    #[test]
    fn the_cursor_motion_sequences_move_where_they_say() {
        let mut t = Term::new(20, 10, 0);
        t.feed(b"\x1b[5;7H");
        assert_eq!(t.grid().cursor(), Some((4, 6)));
        t.feed(b"\x1b[2A\x1b[3B\x1b[4C\x1b[2D");
        assert_eq!(t.grid().cursor(), Some((5, 8)));
        t.feed(b"\x1b[3d");
        assert_eq!(t.grid().cursor(), Some((2, 8)));
        t.feed(b"\x1b[1G");
        assert_eq!(t.grid().cursor(), Some((2, 0)));
        t.feed(b"\x1b[H");
        assert_eq!(t.grid().cursor(), Some((0, 0)), "no params means home");
        t.feed(b"\x1b[4;4f\x1b[2E");
        assert_eq!(t.grid().cursor(), Some((5, 0)));
        t.feed(b"\x1b[1F");
        assert_eq!(t.grid().cursor(), Some((4, 0)));
    }

    #[test]
    fn motion_is_clamped_to_the_screen() {
        let t = term(10, 4, b"\x1b[99;99H");
        assert_eq!(t.grid().cursor(), Some((3, 9)));
        let t = term(10, 4, b"\x1b[9;9H\x1b[65535A\x1b[65535D");
        assert_eq!(t.grid().cursor(), Some((0, 0)));
    }

    #[test]
    fn the_tab_sequences_step_between_stops() {
        let t = term(40, 1, b"\x1b[3I");
        assert_eq!(t.grid().cursor(), Some((0, 24)));
        let t = term(40, 1, b"\x1b[3I\x1b[2Z");
        assert_eq!(t.grid().cursor(), Some((0, 8)));
    }

    // --- SGR --------------------------------------------------------------

    #[test]
    fn sgr_sets_the_basic_colours_and_attributes() {
        let t = term(20, 1, b"\x1b[1;4;31;42mx");
        let s = style_at(&t, 0, 0);
        assert!(s.attrs.bold() && s.attrs.underline());
        assert_eq!(s.fg, CellColor::Indexed(1));
        assert_eq!(s.bg, CellColor::Indexed(2));
    }

    #[test]
    fn sgr_turns_attributes_off_one_at_a_time() {
        let t = term(20, 1, b"\x1b[1;3;4;7m\x1b[22ma\x1b[23mb\x1b[24mc\x1b[27md");
        assert!(!style_at(&t, 0, 0).attrs.bold());
        assert!(style_at(&t, 0, 0).attrs.italic());
        assert!(!style_at(&t, 0, 1).attrs.italic());
        assert!(!style_at(&t, 0, 2).attrs.underline());
        assert!(!style_at(&t, 0, 3).attrs.inverse());
    }

    #[test]
    fn sgr_zero_and_an_empty_sgr_both_reset() {
        let t = term(20, 1, b"\x1b[1;31ma\x1b[0mb\x1b[1;31mc\x1b[md");
        assert_ne!(style_at(&t, 0, 0), Style::default());
        assert_eq!(style_at(&t, 0, 1), Style::default());
        assert_eq!(style_at(&t, 0, 3), Style::default());
    }

    #[test]
    fn the_bright_colour_codes_are_the_upper_palette() {
        let t = term(20, 1, b"\x1b[93;104mx");
        let s = style_at(&t, 0, 0);
        assert_eq!(s.fg, CellColor::Indexed(11));
        assert_eq!(s.bg, CellColor::Indexed(12));
    }

    #[test]
    fn default_colour_codes_go_back_to_the_theme() {
        let t = term(20, 1, b"\x1b[31;41m\x1b[39;49mx");
        assert_eq!(style_at(&t, 0, 0), Style::default());
    }

    #[test]
    fn the_256_colour_form_works_with_semicolons_and_colons() {
        let t = term(20, 1, b"\x1b[38;5;208m\x1b[48;5;17ma\x1b[38:5:99mb");
        assert_eq!(style_at(&t, 0, 0).fg, CellColor::Indexed(208));
        assert_eq!(style_at(&t, 0, 0).bg, CellColor::Indexed(17));
        assert_eq!(style_at(&t, 0, 1).fg, CellColor::Indexed(99));
    }

    #[test]
    fn truecolor_works_with_semicolons_and_both_colon_forms() {
        let t = term(20, 1, b"\x1b[38;2;10;20;30ma");
        assert_eq!(style_at(&t, 0, 0).fg, CellColor::Rgb(10, 20, 30));
        // `38:2::r:g:b` — the colorspace sub-parameter is empty, which is
        // what libvte and tmux emit.
        let t = term(20, 1, b"\x1b[38:2::1:2:3m\x1b[48:2:0:4:5:6ma");
        assert_eq!(style_at(&t, 0, 0).fg, CellColor::Rgb(1, 2, 3));
        assert_eq!(style_at(&t, 0, 0).bg, CellColor::Rgb(4, 5, 6));
        // The five-element colon form without a colorspace slot.
        let t = term(20, 1, b"\x1b[38:2:7:8:9ma");
        assert_eq!(style_at(&t, 0, 0).fg, CellColor::Rgb(7, 8, 9));
    }

    #[test]
    fn a_truncated_extended_colour_leaves_the_pen_alone() {
        let t = term(20, 1, b"\x1b[31m\x1b[38;2;10ma");
        assert_eq!(style_at(&t, 0, 0).fg, CellColor::Indexed(1));
    }

    // --- erasing and editing ------------------------------------------------

    #[test]
    fn the_ed_modes_erase_the_right_part_of_the_screen() {
        let stream = b"aaa\r\nbbb\r\nccc\x1b[2;2H";
        let mut t = term(3, 3, stream);
        t.feed(b"\x1b[0J");
        assert_eq!(t.grid().text(), "aaa\nb\n");

        let mut t = term(3, 3, stream);
        t.feed(b"\x1b[1J");
        assert_eq!(t.grid().text(), "\n  b\nccc");

        let mut t = term(3, 3, stream);
        t.feed(b"\x1b[2J");
        assert_eq!(t.grid().text(), "\n\n");
    }

    #[test]
    fn the_el_modes_erase_the_right_part_of_the_line() {
        let mut t = term(6, 1, b"abcdef\x1b[3G");
        t.feed(b"\x1b[K");
        assert_eq!(t.grid().row_text(0), "ab");

        let mut t = term(6, 1, b"abcdef\x1b[3G\x1b[1K");
        assert_eq!(t.grid().row_text(0), "   def");
        t.feed(b"\x1b[2K");
        assert_eq!(t.grid().row_text(0), "");
    }

    #[test]
    fn ich_and_dch_shift_the_line() {
        let t = term(8, 1, b"abcdef\x1b[2G\x1b[2@");
        assert_eq!(t.grid().row_text(0), "a  bcdef");
        let t = term(8, 1, b"abcdef\x1b[2G\x1b[3P");
        assert_eq!(t.grid().row_text(0), "aef");
    }

    #[test]
    fn ech_blanks_without_moving() {
        let t = term(8, 1, b"abcdef\x1b[3G\x1b[2X");
        assert_eq!(t.grid().row_text(0), "ab  ef");
        assert_eq!(t.grid().cursor(), Some((0, 2)));
    }

    /// The regression pin for the tmux status bar: `SGR 48;5;n` then an
    /// `EL` then the labels. Without back colour erase the bar stops
    /// where the text stops, which is exactly what the bug looked like.
    #[test]
    fn the_status_bar_sequence_tmux_emits_fills_the_row() {
        let t = term(20, 2, b"\x1b[2;1H\x1b[48;5;4m\x1b[38;5;0m\x1b[K[0] bash");
        let r = runs(&t, 1);
        // The erased tail drops the pen's *foreground* (BCE keeps only
        // the background), so the labels and the bar are two runs; what
        // matters, and what was broken, is that the background is
        // continuous all the way to the right margin.
        assert_eq!(r.first().unwrap().col, 0);
        let end = r.last().unwrap();
        assert_eq!(end.col + end.cols, 20, "the bar reaches the margin: {r:?}");
        assert!(
            r.iter().all(|run| run.style.bg == CellColor::Indexed(4)),
            "every cell of the bar is painted: {r:?}"
        );
        assert_eq!(style_at(&t, 1, 19).bg, CellColor::Indexed(4));

        // With no foreground of its own the whole row really is one run.
        let t = term(20, 1, b"\x1b[48;5;4m\x1b[K[0] bash");
        let r = runs(&t, 0);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].cols, 20);

        // The active-window segment is inverse rather than coloured:
        // `SGR 7` + EL must erase with the *foreground*, so the viewer
        // still resolves a visible background.
        let t = term(10, 1, b"\x1b[7m\x1b[Kab");
        let r = runs(&t, 0);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].cols, 10);
        assert!(r[0].style.attrs.inverse());
    }

    #[test]
    fn sgr_49_puts_the_erase_background_back() {
        let t = term(10, 2, b"\x1b[48;5;4m\x1b[K\x1b[2;1H\x1b[49m\x1b[K");
        assert_eq!(runs(&t, 0).len(), 1, "row 0 is still painted");
        assert!(
            runs(&t, 1).is_empty(),
            "a default-background erase still costs nothing"
        );
        assert_eq!(style_at(&t, 1, 0), Style::default());
    }

    #[test]
    fn il_and_dl_move_whole_lines() {
        let t = term(4, 4, b"r0\r\nr1\r\nr2\r\nr3\x1b[2;1H\x1b[1L");
        assert_eq!(t.grid().text(), "r0\n\nr1\nr2");
        let t = term(4, 4, b"r0\r\nr1\r\nr2\r\nr3\x1b[2;1H\x1b[2M");
        assert_eq!(t.grid().text(), "r0\nr3\n\n");
    }

    #[test]
    fn su_and_sd_scroll_the_screen() {
        let t = term(4, 3, b"r0\r\nr1\r\nr2\x1b[1S");
        assert_eq!(t.grid().text(), "r1\nr2\n");
        let t = term(4, 3, b"r0\r\nr1\r\nr2\x1b[1T");
        assert_eq!(t.grid().text(), "\nr0\nr1");
    }

    // --- scroll region --------------------------------------------------------

    #[test]
    fn output_inside_a_scroll_region_scrolls_and_outside_does_not() {
        // Rows 2..=3 of a four-row screen are the region; the header on
        // row 1 and the footer on row 4 must survive the scrolling.
        let mut t = Term::new(8, 4, 0);
        t.feed(b"head\r\n\x1b[4;1Hfoot");
        t.feed(b"\x1b[2;3r");
        t.feed(b"\x1b[2;1Ha\r\nb\r\nc\r\nd");
        assert_eq!(t.grid().text(), "head\nc\nd\nfoot");
        assert_eq!(t.grid().scroll_region(), (1, 2));
    }

    #[test]
    fn setting_a_scroll_region_homes_the_cursor_and_resetting_restores_it() {
        let mut t = Term::new(8, 4, 0);
        t.feed(b"\x1b[3;9H\x1b[2;3r");
        assert_eq!(t.grid().cursor(), Some((0, 0)));
        t.feed(b"\x1b[r");
        assert_eq!(t.grid().scroll_region(), (0, 3));
    }

    #[test]
    fn reverse_index_at_the_top_of_the_region_scrolls_it_down() {
        let mut t = Term::new(4, 3, 0);
        t.feed(b"r0\r\nr1\r\nr2\x1b[1;1H\x1bM");
        assert_eq!(t.grid().text(), "\nr0\nr1");
    }

    // --- escapes ---------------------------------------------------------------

    #[test]
    fn decsc_and_decrc_save_and_restore_the_cursor_and_pen() {
        let mut t = Term::new(10, 4, 0);
        t.feed(b"\x1b[3;5H\x1b[1;31m\x1b7");
        t.feed(b"\x1b[1;1H\x1b[0m\x1b8x");
        assert_eq!(t.grid().cursor(), Some((2, 5)));
        let s = style_at(&t, 2, 4);
        assert!(s.attrs.bold());
        assert_eq!(s.fg, CellColor::Indexed(1));
    }

    #[test]
    fn esc_e_is_a_newline_and_esc_d_an_index() {
        let t = term(6, 3, b"ab\x1bEcd");
        assert_eq!(t.grid().text(), "ab\ncd\n");
        let t = term(6, 3, b"ab\x1bDcd");
        assert_eq!(t.grid().text(), "ab\n  cd\n");
    }

    #[test]
    fn ris_resets_everything() {
        let mut t = Term::new(8, 3, 4);
        t.feed(b"\x1b[?1h\x1b[?2004h\x1b]0;t\x07hello\r\n");
        t.feed(b"\x1bc");
        assert_eq!(t.grid().text(), "\n\n");
        assert!(!t.application_cursor() && !t.bracketed_paste());
        assert_eq!(t.take_title(), None, "a reset drops the pending title");
    }

    #[test]
    fn a_charset_designation_is_ignored_rather_than_misread() {
        let t = term(8, 2, b"\x1b(Bok");
        assert_eq!(t.grid().row_text(0), "ok");
    }

    // --- modes ------------------------------------------------------------------

    #[test]
    fn the_decset_modes_we_care_about_are_tracked() {
        let mut t = Term::new(8, 2, 0);
        assert!(!t.application_cursor() && !t.bracketed_paste());
        t.feed(b"\x1b[?1h\x1b[?2004h\x1b[?25l");
        assert!(t.application_cursor() && t.bracketed_paste());
        assert!(!t.grid().cursor_visible());
        t.feed(b"\x1b[?1l\x1b[?2004l\x1b[?25h");
        assert!(!t.application_cursor() && !t.bracketed_paste());
        assert!(t.grid().cursor_visible());
    }

    #[test]
    fn several_modes_in_one_sequence_are_all_applied() {
        let mut t = Term::new(8, 2, 0);
        t.feed(b"\x1b[?1;2004h");
        assert!(t.application_cursor() && t.bracketed_paste());
    }

    #[test]
    fn the_alt_screen_leaves_the_primary_grid_untouched() {
        let mut t = Term::new(12, 3, 8);
        t.feed(b"$ vim file\r\n");
        let before = t.grid().text();
        t.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1Hediting\x1b[3;1H-- INSERT --");
        assert!(t.alt_screen());
        assert_eq!(t.grid().row_text(0), "editing");
        t.feed(b"\x1b[?1049l");
        assert!(!t.alt_screen());
        assert_eq!(t.grid().text(), before);
    }

    #[test]
    fn the_older_alt_screen_modes_switch_too() {
        let mut t = Term::new(8, 2, 0);
        t.feed(b"\x1b[?47h");
        assert!(t.alt_screen());
        t.feed(b"\x1b[?47l");
        assert!(!t.alt_screen());
        t.feed(b"\x1b[?1047h");
        assert!(t.alt_screen());
        t.feed(b"\x1b[?1047l");
        assert!(!t.alt_screen());
    }

    #[test]
    fn mode_1048_only_saves_the_cursor() {
        let mut t = Term::new(8, 3, 0);
        t.feed(b"\x1b[2;3H\x1b[?1048h\x1b[1;1H\x1b[?1048l");
        assert!(!t.alt_screen());
        assert_eq!(t.grid().cursor(), Some((1, 2)));
    }

    // --- replies -------------------------------------------------------------

    #[test]
    fn dsr_reports_the_cursor_in_one_based_coordinates() {
        let mut t = Term::new(20, 10, 0);
        t.feed(b"\x1b[4;9H\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[4;9R");
        assert!(t.take_replies().is_empty(), "replies are drained");
    }

    #[test]
    fn dsr_answers_from_the_screen_not_the_scrolled_view() {
        let mut t = Term::new(8, 2, 8);
        t.feed(b"a\r\nb\r\nc\r\nd");
        t.grid_mut().scroll_up(2);
        t.feed(b"\x1b[6n");
        assert_eq!(t.take_replies(), b"\x1b[2;2R");
    }

    #[test]
    fn da_identifies_us_as_a_vt102() {
        let mut t = Term::new(8, 2, 0);
        t.feed(b"\x1b[c");
        assert_eq!(t.take_replies(), b"\x1b[?6c");
    }

    // --- OSC ---------------------------------------------------------------------

    #[test]
    fn osc_zero_and_two_set_the_title_and_one_does_not() {
        let mut t = Term::new(8, 2, 0);
        t.feed(b"\x1b]0;hello\x07");
        assert_eq!(t.take_title().as_deref(), Some("hello"));
        assert_eq!(t.take_title(), None, "taken once");
        t.feed(b"\x1b]2;second\x1b\\");
        assert_eq!(t.take_title().as_deref(), Some("second"));
        t.feed(b"\x1b]1;icon\x07");
        assert_eq!(t.take_title(), None, "icon names are dropped");
    }

    #[test]
    fn a_title_with_utf8_survives() {
        let mut t = Term::new(8, 2, 0);
        t.feed("\x1b]0;~/プロジェクト — ✨\x07".as_bytes());
        assert_eq!(t.take_title().as_deref(), Some("~/プロジェクト — ✨"));
    }

    // --- wide characters and wrapping -----------------------------------------------

    #[test]
    fn a_wide_char_is_two_columns_and_overwriting_it_clears_the_tail() {
        let mut t = Term::new(10, 1, 0);
        t.feed("你好".as_bytes());
        assert_eq!(t.grid().cursor(), Some((0, 4)));
        assert_eq!(t.grid().row_text(0), "你好");
        t.feed(b"\x1b[1Ga");
        assert_eq!(t.grid().row_text(0), "a 好");
        assert_eq!(t.grid().display_row(0)[1].wide, Wide::No);
    }

    #[test]
    fn eighty_characters_on_an_eighty_column_grid_stay_on_row_zero() {
        let t = term(80, 24, &[b'x'; 80]);
        assert_eq!(t.grid().cursor(), Some((0, 79)));
        assert_eq!(t.grid().row_text(1), "");
    }

    #[test]
    fn the_eighty_first_character_wraps() {
        let mut line = vec![b'x'; 80];
        line.push(b'y');
        let t = term(80, 24, &line);
        assert_eq!(t.grid().cursor(), Some((1, 1)));
        assert_eq!(t.grid().row_text(1), "y");
    }

    // --- damage ---------------------------------------------------------------------

    #[test]
    fn writing_one_character_damages_one_row_and_one_column() {
        let mut t = Term::new(20, 5, 0);
        t.grid_mut().clear_damage();
        t.feed(b"\x1b[3;5Hx");
        assert_eq!(t.grid().row_damage(2), Some((4, 5)));
        for row in [0, 1, 3, 4] {
            assert!(!t.grid().row_dirty(row));
        }
    }

    #[test]
    fn a_full_screen_redraw_damages_every_row() {
        let mut t = Term::new(8, 4, 0);
        t.grid_mut().clear_damage();
        t.feed(b"\x1b[2J\x1b[1;1H");
        for row in 0..4 {
            assert!(t.grid().row_dirty(row), "row {row}");
        }
    }

    // --- scrollback --------------------------------------------------------------------

    #[test]
    fn a_hundred_lines_into_a_short_grid_fill_the_scrollback_ring() {
        let mut t = Term::new(20, 24, 10);
        for i in 0..100 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(t.grid().scrollback_len(), 10);
        t.grid_mut().scroll_up(4);
        assert_eq!(t.grid().scroll_offset(), 4);
        t.grid_mut().scroll_up(100);
        assert_eq!(t.grid().scroll_offset(), 10, "clamped to the ring");
        t.grid_mut().scroll_to_bottom();
        assert_eq!(t.grid().scroll_offset(), 0);
    }

    // --- capture-stream tests ------------------------------------------------------------
    //
    // Not literal captures, but byte-for-byte the kinds of sequence the
    // named programs emit; they are here so that a change to the parser
    // has to keep a real screen looking right, not just a unit test.

    /// What `ls --color=auto` writes for a directory listing: `SGR 1;34`
    /// for a directory, `SGR 32` for an executable, `SGR 0` between
    /// entries, and plain text for a regular file.
    const LS_COLOR: &[u8] =
        b"\x1b[0m\x1b[01;34msrc\x1b[0m  \x1b[01;32mbuild.sh\x1b[0m  Cargo.toml\r\n";

    #[test]
    fn a_coloured_ls_listing_keeps_its_runs() {
        let t = term(40, 2, LS_COLOR);
        assert_eq!(t.grid().row_text(0), "src  build.sh  Cargo.toml");
        let r = runs(&t, 0);
        // Four runs, not five: the gap after the executable and the
        // plain file share the default style, and runs are maximal.
        assert_eq!(r.len(), 4, "dir, gap, exe, gap+plain");
        assert_eq!(r[0].text, "src");
        assert!(r[0].style.attrs.bold());
        assert_eq!(r[0].style.fg, CellColor::Indexed(4));
        assert_eq!(r[1].text, "  ");
        assert_eq!(r[1].style, Style::default());
        assert_eq!(r[2].text, "build.sh");
        assert_eq!(r[2].style.fg, CellColor::Indexed(2));
        assert_eq!(r[3].text, "  Cargo.toml");
        assert_eq!(r[3].style, Style::default());
        assert_eq!(t.grid().cursor(), Some((1, 0)));
    }

    /// What `vim` writes on entry and exit: alt screen on, clear, paint
    /// the buffer, the tilde column, an inverse status line, park the
    /// cursor — then on `:q`, clear, alt screen off, reset the pen.
    const VIM_SESSION: &[u8] = b"\x1b[?1049h\x1b[?1h\x1b[2J\x1b[1;1Hfn main() {}\r\n\
\x1b[2;1H~\x1b[3;1H~\x1b[4;1H\x1b[7m\"main.rs\" 1L, 13B\x1b[0m\x1b[1;13H";

    /// The other half of the session: `:q` and the tear-down vim emits.
    const VIM_LEAVE: &[u8] = b"\x1b[4;1H\x1b[K\x1b[?1049l\x1b[?1l\x1b[0m";

    #[test]
    fn a_vim_session_leaves_the_shell_screen_exactly_as_it_was() {
        let mut t = Term::new(24, 4, 20);
        t.feed(b"$ vim main.rs\r\n");
        let before = t.grid().text();
        let cursor_before = t.grid().cursor();

        t.feed(VIM_SESSION);
        assert!(t.alt_screen() && t.application_cursor());
        assert_eq!(t.grid().row_text(0), "fn main() {}");
        assert_eq!(t.grid().row_text(1), "~");
        assert_eq!(t.grid().row_text(3), "\"main.rs\" 1L, 13B");
        assert!(
            style_at(&t, 3, 0).attrs.inverse(),
            "the status line is inverse video"
        );
        assert_eq!(t.grid().cursor(), Some((0, 12)));
        assert_eq!(t.grid().scrollback_len(), 0, "no history on the alt screen");

        t.feed(VIM_LEAVE);
        assert!(!t.alt_screen() && !t.application_cursor());
        assert_eq!(t.grid().text(), before);
        assert_eq!(t.grid().cursor(), cursor_before);
        assert_eq!(t.grid().pen(), Style::default());
    }

    /// What `htop` writes for a frame: alt screen, cursor off, a scroll
    /// region around the process list, an inverse header row painted with
    /// coloured meters, two process rows, and a repaint of the list by
    /// scrolling the region rather than redrawing it.
    const HTOP_FRAME: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[1;1H\
\x1b[7m  PID USER      CPU%\x1b[0m\
\x1b[2;1H\x1b[32m  1 root       0.0\x1b[0m\
\x1b[3;1H\x1b[33m  42 kaspar     3.7\x1b[0m\
\x1b[2;3r\x1b[3;1H\n\x1b[36m 100 kaspar     1.2\x1b[0m";

    #[test]
    fn an_htop_frame_paints_a_header_and_scrolls_only_its_region() {
        let mut t = Term::new(24, 3, 20);
        t.feed(HTOP_FRAME);
        assert!(t.alt_screen());
        assert!(!t.grid().cursor_visible());
        assert_eq!(t.grid().scroll_region(), (1, 2));
        // The header is outside the region, so the scroll left it alone;
        // the first process row moved up and the new one came in below.
        assert_eq!(t.grid().row_text(0), "  PID USER      CPU%");
        assert!(style_at(&t, 0, 0).attrs.inverse());
        assert_eq!(t.grid().row_text(1), "  42 kaspar     3.7");
        assert_eq!(t.grid().row_text(2), " 100 kaspar     1.2");
        assert_eq!(style_at(&t, 2, 1).fg, CellColor::Indexed(6));
    }

    // --- robustness ------------------------------------------------------------------------

    #[test]
    fn ten_kilobytes_of_noise_never_panic() {
        // A deterministic LCG (the one from Numerical Recipes) so a
        // failure is reproducible; a random crate would buy nothing but a
        // dependency and a flaky test.
        let mut state: u32 = 0x1234_5678;
        let mut bytes = Vec::with_capacity(10 * 1024);
        for _ in 0..10 * 1024 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            bytes.push((state >> 16) as u8);
        }
        let mut t = Term::new(80, 24, 100);
        // In chunks, so sequences are split across calls the way pty
        // reads split them.
        for chunk in bytes.chunks(37) {
            t.feed(chunk);
        }
        // Whatever it did, the grid must still be coherent.
        let g = t.grid();
        for row in 0..g.rows() {
            assert_eq!(g.display_row(row).len(), g.cols());
            let mut out = Vec::new();
            g.row_runs(row, &mut out);
            let total: usize = out.iter().map(|r| r.cols).sum();
            assert!(total <= g.cols());
        }
        assert!(g.scrollback_len() <= 100);
        let _ = t.take_replies();
        let _ = t.take_title();
    }

    #[test]
    fn unknown_and_malformed_sequences_are_ignored() {
        let mut t = Term::new(10, 2, 0);
        t.feed(b"\x1b[>4;1m\x1b[?2026h\x1bP1$r0m\x1b\\\x1b[99999;99999;99999Zok");
        assert_eq!(t.grid().row_text(0), "ok");
        t.feed(b"\x1b[");
        t.feed(b"38;5;");
        t.feed(b"9m!");
        assert_eq!(style_at(&t, 0, 2).fg, CellColor::Indexed(9));
    }

    #[test]
    fn a_resize_keeps_the_terminal_usable() {
        let mut t = Term::new(20, 4, 10);
        t.feed(b"hello\r\nworld");
        t.resize(10, 2);
        assert_eq!(t.grid().cols(), 10);
        t.feed(b"\r\nmore");
        assert_eq!(t.grid().row_text(1), "more");
    }
}
