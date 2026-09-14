//! The colour table: the sixteen ANSI colours, the 256-colour cube, and
//! the two colours a terminal calls "default".
//!
//! A [`Palette`] is a plain struct of colours rather than a theme trait,
//! for the same reason [`nitro_ui::Theme`] is: there is no styling
//! language here, and the day there needs to be one this is what it will
//! be built out of. It is separate from the toolkit's theme because the
//! two answer different questions — the theme says what a *button* looks
//! like, and nothing in it can say what `SGR 31` means.

use nitro_ui::Color;

/// A terminal's colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    /// The sixteen ANSI colours: 0–7 normal, 8–15 bright.
    pub ansi: [Color; 16],
    /// Default foreground, for `SGR 39` and for text that set none.
    pub foreground: Color,
    /// Default background. Painted by the window's backdrop rather than
    /// per cell, which is why a run with the default background costs no
    /// rect at all.
    pub background: Color,
    /// The cursor block.
    pub cursor: Color,
}

impl Palette {
    /// The colour of palette index `index`.
    ///
    /// Indices 0–15 are the table above; 16–231 are the 6×6×6 cube and
    /// 232–255 the greyscale ramp, both computed rather than stored,
    /// because they are defined by arithmetic — xterm's, which every
    /// program that emits `38;5;n` assumes.
    #[must_use]
    pub fn indexed(&self, index: u8) -> Color {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let n = index - 16;
                let (red, green, blue) = (n / 36, (n % 36) / 6, n % 6);
                Color::rgb(cube(red), cube(green), cube(blue))
            }
            232..=255 => {
                // The ramp is 24 steps from near-black to near-white,
                // deliberately excluding both ends: 0 and 255 are
                // already in the sixteen.
                let v = 8 + (index - 232) * 10;
                Color::rgb(v, v, v)
            }
        }
    }
}

/// One axis of the 6×6×6 colour cube. The first step is 0 and the rest
/// are 95 plus multiples of 40 — xterm's table, not a linear ramp, and
/// programs do depend on the exact values.
fn cube(n: u8) -> u8 {
    if n == 0 { 0 } else { 55 + n * 40 }
}

impl Default for Palette {
    /// A dark palette in the family every terminal ships: saturated but
    /// not fluorescent, with a light-on-dark default.
    ///
    /// Dark rather than following the toolkit's light theme, because a
    /// terminal's colours are not decoration — `ls --color` picks blue
    /// for a directory assuming a dark background, and on a light one it
    /// is unreadable. Choosing the background is therefore choosing
    /// whether the ANSI colours work at all.
    fn default() -> Self {
        Self {
            ansi: [
                Color::rgb(0x1c, 0x1c, 0x1c), // 0 black
                Color::rgb(0xcc, 0x33, 0x3c), // 1 red
                Color::rgb(0x5a, 0xb0, 0x38), // 2 green
                Color::rgb(0xc8, 0x9b, 0x27), // 3 yellow
                Color::rgb(0x3d, 0x82, 0xd6), // 4 blue
                Color::rgb(0xa6, 0x53, 0xc4), // 5 magenta
                Color::rgb(0x2f, 0xa8, 0xa8), // 6 cyan
                Color::rgb(0xc8, 0xc8, 0xc8), // 7 white
                Color::rgb(0x5c, 0x5c, 0x5c), // 8 bright black
                Color::rgb(0xf2, 0x5b, 0x63), // 9 bright red
                Color::rgb(0x84, 0xd6, 0x5c), // 10 bright green
                Color::rgb(0xf0, 0xc4, 0x4c), // 11 bright yellow
                Color::rgb(0x67, 0xa8, 0xf0), // 12 bright blue
                Color::rgb(0xc9, 0x82, 0xe8), // 13 bright magenta
                Color::rgb(0x55, 0xd0, 0xd0), // 14 bright cyan
                Color::rgb(0xf2, 0xf2, 0xf2), // 15 bright white
            ],
            foreground: Color::rgb(0xdc, 0xdc, 0xdc),
            background: Color::rgb(0x14, 0x14, 0x18),
            cursor: Color::rgb(0xdc, 0xdc, 0xdc),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sixteen_are_distinguishable_from_the_background() {
        let p = Palette::default();
        for (i, c) in p.ansi.iter().enumerate() {
            assert_ne!(*c, p.background, "ansi {i} is invisible");
        }
        assert_ne!(p.foreground, p.background);
    }

    #[test]
    fn the_cube_matches_xterms_arithmetic() {
        let p = Palette::default();
        // The corners, which is where an off-by-one shows.
        assert_eq!(p.indexed(16), Color::rgb(0, 0, 0));
        assert_eq!(p.indexed(231), Color::rgb(255, 255, 255));
        // 196 is the cube's pure red: (196-16) = 180 = 5*36 + 0 + 0.
        assert_eq!(p.indexed(196), Color::rgb(255, 0, 0));
        // The first step is 0, the second 95 — not 51, which a linear
        // ramp would give and which programs would render visibly wrong.
        assert_eq!(p.indexed(16 + 36), Color::rgb(95, 0, 0));
    }

    #[test]
    fn the_greyscale_ramp_excludes_both_ends() {
        let p = Palette::default();
        assert_eq!(p.indexed(232), Color::rgb(8, 8, 8));
        assert_eq!(p.indexed(255), Color::rgb(238, 238, 238));
    }

    #[test]
    fn the_low_sixteen_come_from_the_table() {
        let p = Palette::default();
        for i in 0..16u8 {
            assert_eq!(p.indexed(i), p.ansi[i as usize]);
        }
    }
}
