//! The board: one widget that paints sixty-four squares.
//!
//! `xilem-chess` builds its board as an 8×8 grid of buttons, and
//! `tiny-chess` paints it into an `egui` canvas. This is the second kind:
//! a widget with a slot per square, a slot per piece and a slot per move
//! hint, so the toolkit's per-slot diff does the work. A move changes
//! two pieces and four highlights, and that is what crosses the wire —
//! the other squares are re-emitted with the values they already had and
//! cost nothing.
//!
//! It knows nothing about chess. It is handed a [`View`] — the engine's
//! `[i8; 64]`, which square is selected, where that piece may go, the
//! last move — and reports clicks as a square index. The rules live in
//! [`crate::engine`], and the decisions in `lib.rs`.
//!
//! # Squares
//!
//! Indices are the engine's: `0` is **h1**, `7` is a1, `63` is a8 — the
//! files run *backwards* along a rank. [`square_name`] and
//! [`parse_square`] are the only two places that know it.

use nitro_ui::build::{Built, IntoWidget, StyleBuilder};
use nitro_ui::event::button;
use nitro_ui::layout::Constraints;
use nitro_ui::widget::{Access, EventCx, MeasureCx, PaintCx, Role, TextRun, Widget};
use nitro_ui::widgets::Flex;
use nitro_ui::{
    Align, Color, ColorRole, Event, Fill, Handled, Rect, Size, TextStyle, Ui, WidgetMut,
};

/// A square's side when nobody offers more, in logical pixels.
pub const SQUARE: f32 = 56.0;
/// The smallest square the board shrinks to before it overflows.
pub const MIN_SQUARE: f32 = 32.0;

/// A piece glyph as a fraction of its square.
const GLYPH: f32 = 0.78;
/// A move hint's dot as a fraction of its square.
const DOT: f32 = 0.3;
/// The width of the last-move ring, as a fraction of its square.
const RING: f32 = 0.06;

/// The slot bases: square `i` paints its background in `BG + i`, its
/// piece in `PIECE + i`, its hint in `HINT + i`.
const BG: u16 = 0;
const PIECE: u16 = 64;
const HINT: u16 = 128;

/// Everything the board shows, and nothing it does not.
///
/// `Copy` and `PartialEq`, so the app builds a fresh one after every
/// change and [`BoardMut::set_view`] drops it when it is the same — a
/// click on an empty square with nothing selected repaints nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// The engine's board: `+1..=+6` white pawn…king, negative black.
    pub squares: [i8; 64],
    /// The square whose piece is picked up.
    pub selected: Option<u8>,
    /// Where the picked-up piece may go, one bit per square.
    pub targets: u64,
    /// The last move's two squares.
    pub last: Option<(u8, u8)>,
    /// Black at the bottom.
    pub flipped: bool,
}

impl View {
    /// The board `squares`, with nothing highlighted.
    #[must_use]
    pub fn new(squares: [i8; 64]) -> Self {
        Self {
            squares,
            selected: None,
            targets: 0,
            last: None,
            flipped: false,
        }
    }

    /// The board as text, rank 8 first, the way `hey … get board value`
    /// prints it: `R N B Q K B N R` for white, lower case for black, `.`
    /// for an empty square.
    #[must_use]
    pub fn diagram(&self) -> String {
        let mut out = String::with_capacity(8 * 16);
        for rank in (0..8u8).rev() {
            for file in 0..8u8 {
                let f = self.squares[usize::from(index(file, rank))];
                if file > 0 {
                    out.push(' ');
                }
                out.push(letter(f));
            }
            if rank > 0 {
                out.push('\n');
            }
        }
        out
    }
}

/// The engine's index of `file` (`0` = a) and `rank` (`0` = 1).
#[must_use]
pub fn index(file: u8, rank: u8) -> u8 {
    rank * 8 + (7 - file)
}

/// `e2` for the engine's `11`.
#[must_use]
pub fn square_name(sq: u8) -> String {
    let file = char::from(b'a' + 7 - sq % 8);
    let rank = char::from(b'1' + sq / 8);
    format!("{file}{rank}")
}

/// The engine's index of a square named like `e2`; `None` for anything
/// else.
#[must_use]
pub fn parse_square(name: &str) -> Option<u8> {
    match name.trim().as_bytes() {
        &[f @ b'a'..=b'h', r @ b'1'..=b'8'] => Some(index(f - b'a', r - b'1')),
        _ => None,
    }
}

/// Whether square `sq` is a light one. a1 is dark: its engine index is
/// 7, and counting the file from the right flips the parity.
fn is_light(sq: u8) -> bool {
    (sq / 8 + sq % 8).is_multiple_of(2)
}

/// The FEN-style letter of a piece, `.` for none.
fn letter(f: i8) -> char {
    let c = match f.unsigned_abs() {
        1 => 'p',
        2 => 'n',
        3 => 'b',
        4 => 'r',
        5 => 'q',
        6 => 'k',
        _ => return '.',
    };
    if f > 0 { c.to_ascii_uppercase() } else { c }
}

/// The Unicode glyph of a piece: outlined for white, solid for black.
///
/// The convention of the chess symbols block itself, and what
/// `xilem-chess` draws by default. Both are painted in the text colour,
/// so the pieces follow the desktop's scheme rather than a colour of
/// their own; the shape says whose they are.
fn glyph(f: i8) -> Option<&'static str> {
    const WHITE: [&str; 6] = ["♙", "♘", "♗", "♖", "♕", "♔"];
    const BLACK: [&str; 6] = ["♟", "♞", "♝", "♜", "♛", "♚"];
    let i = usize::from(f.unsigned_abs()).checked_sub(1)?;
    if f > 0 { WHITE.get(i) } else { BLACK.get(i) }.copied()
}

/// A callback for a clicked square.
type SquareFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>, u8)>;

/// The board widget. Build it with [`board`].
pub struct Board<S> {
    /// What is on it.
    view: View,
    /// Called with the engine index of a clicked square.
    on_square: Option<SquareFn<S>>,
    /// The glyph's line height at the size last measured, for centring.
    glyph_h: f32,
    /// The size `glyph_h` was measured at.
    glyph_px: f32,
}

impl<S: 'static> Board<S> {
    /// The side of one square and the board's top-left corner, for a
    /// widget of `size`: the largest whole board that fits, centred.
    fn geometry(size: Size) -> (f32, f32, f32) {
        let cell = (size.w.min(size.h) / 8.0).floor().max(1.0);
        let left = ((size.w - cell * 8.0) / 2.0).floor();
        let top = ((size.h - cell * 8.0) / 2.0).floor();
        (cell, left, top)
    }

    /// Where square `sq` is drawn, for squares `cell` wide at
    /// `(x0, y0)`.
    fn square_rect(&self, sq: u8, cell: f32, left: f32, top: f32) -> Rect {
        // Engine index 0 is h1: the file counts from the right.
        let (mut col, mut row) = (7 - sq % 8, 7 - sq / 8);
        if self.view.flipped {
            (col, row) = (7 - col, 7 - row);
        }
        Rect::new(
            left + f32::from(col) * cell,
            top + f32::from(row) * cell,
            cell,
            cell,
        )
    }

    /// The square under `(x, y)`, if any.
    fn square_at(&self, size: Size, x: f32, y: f32) -> Option<u8> {
        let (cell, left, top) = Self::geometry(size);
        let (u, v) = ((x - left) / cell, (y - top) / cell);
        if !(0.0..8.0).contains(&u) || !(0.0..8.0).contains(&v) {
            return None;
        }
        let (col, row) = (u as u8, v as u8);
        let (file, rank) = if self.view.flipped {
            (7 - col, row)
        } else {
            (col, 7 - row)
        };
        Some(index(file, rank))
    }

    /// Run the click callback, taken out for the call like a button's.
    fn fire(&mut self, cx: &mut EventCx<'_, S>, sq: u8) {
        let Some(cb) = self.on_square.take() else {
            return;
        };
        cb(cx.state, cx.ui, sq);
        self.on_square = Some(cb);
    }
}

impl<S: 'static> Widget<S> for Board<S> {
    fn measure(&mut self, _cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        constraints.constrain(Size::new(SQUARE * 8.0, SQUARE * 8.0))
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let (cell, left, top) = Self::geometry(cx.size());
        let light = cx.color(ColorRole::Surface);
        let dark = cx.color(ColorRole::Track);
        let selected = cx.color(ColorRole::Selection);
        let accent = cx.color(ColorRole::Accent);
        let ink = cx.color(ColorRole::Text);

        let mut style = TextStyle::from_theme(cx.theme());
        style.size_px = (cell * GLYPH).floor();
        if (style.size_px - self.glyph_px).abs() > f32::EPSILON {
            // One measurement per board size, cached by the toolkit; the
            // piece is centred on the line box it is drawn in.
            self.glyph_h = cx
                .ui
                .measure_text("♚", &style, 0.0)
                .map_or(style.size_px, |m| m.height);
            self.glyph_px = style.size_px;
        }
        let glyph_h = self.glyph_h;

        for sq in 0..64u8 {
            let slot = u16::from(sq);
            let r = self.square_rect(sq, cell, left, top);
            let face = if self.view.selected == Some(sq) {
                selected
            } else if is_light(sq) {
                light
            } else {
                dark
            };
            let last = matches!(self.view.last, Some((a, b)) if a == sq || b == sq);
            let ring = if last {
                ((cell * RING).ceil(), accent)
            } else {
                (0.0, Color::TRANSPARENT)
            };
            cx.rect(BG + slot, r, Fill::Solid(face), 0.0, ring);

            if let Some(g) = glyph(self.view.squares[usize::from(sq)]) {
                let text_box =
                    Rect::new(r.x, r.y + ((cell - glyph_h) / 2.0).max(0.0), cell, glyph_h);
                let run = TextRun::new(&style, ink).align(Align::Center);
                cx.text(PIECE + slot, text_box, g, run);
            }

            if self.view.targets & (1 << sq) != 0 {
                let d = (cell * DOT).round();
                let dot = Rect::new(r.x + (cell - d) / 2.0, r.y + (cell - d) / 2.0, d, d);
                cx.rect(
                    HINT + slot,
                    dot,
                    Fill::Solid(accent),
                    d / 2.0,
                    (0.0, Color::TRANSPARENT),
                );
            }
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        match ev {
            Event::PointerDown {
                pos,
                button: button::LEFT,
            } => {
                if let Some(sq) = self.square_at(cx.bounds.size(), pos.x, pos.y) {
                    self.fire(cx, sq);
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Other
    }

    fn accessible(&self) -> Access {
        Access {
            name: Some("board".to_owned()),
            value: Some(self.view.diagram()),
            actions: vec!["click", "move"],
        }
    }

    /// `click e2` clicks a square by name; `move e2e4` is the two clicks
    /// of a move. Both run the callback a pointer click runs, so a script
    /// is held to exactly the rules a user is.
    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, arg: Option<&str>) -> Handled {
        let arg = arg.unwrap_or_default().trim();
        match action {
            "click" => {
                let Some(sq) = parse_square(arg) else {
                    return Handled::No;
                };
                self.fire(cx, sq);
                Handled::Yes
            }
            "move" => {
                let (Some(a), Some(b)) = (
                    arg.get(..2).and_then(parse_square),
                    arg.get(2..).and_then(parse_square),
                ) else {
                    return Handled::No;
                };
                self.fire(cx, a);
                self.fire(cx, b);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// The board's setter, as a trait because an inherent `impl` on
/// `WidgetMut` has to live in `nitro-ui` (`docs/ui.md`, "Writing a
/// widget").
pub trait BoardMut<S> {
    /// Show `view`; a view equal to the current one costs nothing.
    fn set_view(&mut self, view: View);
    /// Replace the click callback. For an app whose callback needs the
    /// board's own id, which does not exist until the board is built.
    fn set_on_square(&mut self, f: impl Fn(&mut S, &mut Ui<S>, u8) + 'static);
}

impl<S: 'static> BoardMut<S> for WidgetMut<'_, Board<S>, S> {
    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.request_paint();
    }

    fn set_on_square(&mut self, f: impl Fn(&mut S, &mut Ui<S>, u8) + 'static) {
        self.on_square = Some(Box::new(f));
    }
}

impl<S: 'static> Board<S> {
    /// What the board is showing.
    #[must_use]
    pub fn view(&self) -> &View {
        &self.view
    }
}

/// Builder for a [`Board`].
pub struct BoardBuilder<S> {
    built: Built<S>,
    board: Board<S>,
}

impl<S: 'static> BoardBuilder<S> {
    /// What to do when a square is clicked; the argument is the engine's
    /// index of the square.
    #[must_use]
    pub fn on_square(mut self, f: impl Fn(&mut S, &mut Ui<S>, u8) + 'static) -> Self {
        self.board.on_square = Some(Box::new(f));
        self
    }
}

impl<S: 'static> StyleBuilder<S> for BoardBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> IntoWidget<S> for BoardBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(self.board);
        self.built
    }
}

/// A board showing `view`.
#[must_use]
pub fn board<S: 'static>(view: View) -> BoardBuilder<S> {
    BoardBuilder {
        built: Built::new(Flex),
        board: Board {
            view,
            on_square: None,
            glyph_h: 0.0,
            glyph_px: 0.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_names_round_trip_and_h1_is_zero() {
        assert_eq!(square_name(0), "h1");
        assert_eq!(square_name(7), "a1");
        assert_eq!(square_name(63), "a8");
        assert_eq!(parse_square("e2"), Some(11));
        for sq in 0..64 {
            assert_eq!(parse_square(&square_name(sq)), Some(sq));
        }
        assert_eq!(parse_square("i1"), None);
        assert_eq!(parse_square("e9"), None);
        assert_eq!(parse_square("e"), None);
    }

    #[test]
    fn a1_is_dark_and_h1_light() {
        let light = |name| is_light(parse_square(name).unwrap());
        assert!(!light("a1") && light("h1") && light("a8") && !light("h8"));
        assert!(!light("e5") && light("d5"));
    }

    #[test]
    fn the_diagram_has_white_at_the_bottom() {
        let mut squares = [0i8; 64];
        squares[usize::from(parse_square("e1").unwrap())] = 6;
        squares[usize::from(parse_square("d8").unwrap())] = -5;
        let d = View::new(squares).diagram();
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines[0], ". . . q . . . .");
        assert_eq!(lines[7], ". . . . K . . .");
    }

    #[test]
    fn a_click_maps_back_to_its_square_either_way_up() {
        let size = Size::new(400.0, 400.0);
        let mut b = Board::<()> {
            view: View::new([0; 64]),
            on_square: None,
            glyph_h: 0.0,
            glyph_px: 0.0,
        };
        // 50 px squares: a1 is bottom left, h8 top right.
        assert_eq!(b.square_at(size, 10.0, 390.0), parse_square("a1"));
        assert_eq!(b.square_at(size, 390.0, 10.0), parse_square("h8"));
        b.view.flipped = true;
        assert_eq!(b.square_at(size, 10.0, 390.0), parse_square("h8"));
        assert_eq!(b.square_at(size, 390.0, 10.0), parse_square("a1"));
        for sq in 0..64 {
            let r = b.square_rect(sq, 50.0, 0.0, 0.0);
            assert_eq!(b.square_at(size, r.x + 25.0, r.y + 25.0), Some(sq));
        }
    }
}
