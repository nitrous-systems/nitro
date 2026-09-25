//! Text the player shows: the clock and the scrolling title.

/// `mm:ss` for a position in seconds, and `-mm:ss` when `remaining`.
///
/// Minutes are not wrapped into hours: Winamp shows a 75-minute mix as
/// `75:00`, and a two-field clock that changes shape at the hour would
/// make the display jump sideways mid-track.
#[must_use]
pub fn clock(secs: f64, remaining: bool) -> String {
    let t = secs.max(0.0).floor() as u64;
    let sign = if remaining { "-" } else { "" };
    format!("{sign}{:02}:{:02}", t / 60, t % 60)
}

/// A track's length as the playlist prints it: `m:ss`, or empty when
/// not yet known.
#[must_use]
pub fn length(secs: Option<f64>) -> String {
    match secs {
        Some(s) if s.is_finite() && s >= 0.0 => {
            let t = s.round() as u64;
            format!("{}:{:02}", t / 60, t % 60)
        }
        _ => String::new(),
    }
}

/// The title line: `N. Title (m:ss)`, as the main window scrolls it.
#[must_use]
pub fn title_line(number: usize, title: &str, secs: Option<f64>) -> String {
    let len = length(secs);
    if len.is_empty() {
        format!("{number}. {title}")
    } else {
        format!("{number}. {title} ({len})")
    }
}

/// What joins the end of a scrolling title to its start.
const SEPARATOR: &str = "  ***  ";

/// A fixed-width window onto a string that scrolls when the string does
/// not fit — the main window's title display.
///
/// Counted in `char`s, and meant for a monospace label: the window is a
/// number of cells, which is what makes a one-character step move the
/// text by exactly one cell and never reflow the line.
#[derive(Debug, Clone, Default)]
pub struct Marquee {
    text: Vec<char>,
    offset: usize,
}

impl Marquee {
    /// Replace the text and scroll back to its start. The same text
    /// again is not a change, so a status poll that re-offers the title
    /// does not reset the scroll.
    pub fn set(&mut self, text: &str) {
        let chars: Vec<char> = text.chars().collect();
        if chars != self.text {
            self.text = chars;
            self.offset = 0;
        }
    }

    /// Whether `width` cells are too few for the text, so it scrolls.
    #[must_use]
    pub fn scrolls(&self, width: usize) -> bool {
        self.text.len() > width
    }

    /// Advance by one cell (only if it scrolls at `width`).
    pub fn step(&mut self, width: usize) {
        if self.scrolls(width) {
            let cycle = self.text.len() + SEPARATOR.chars().count();
            self.offset = (self.offset + 1) % cycle;
        }
    }

    /// What `width` cells show now.
    #[must_use]
    pub fn window(&self, width: usize) -> String {
        if !self.scrolls(width) {
            return self.text.iter().collect();
        }
        self.text
            .iter()
            .copied()
            .chain(SEPARATOR.chars())
            .cycle()
            .skip(self.offset)
            .take(width)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_counts_minutes_past_the_hour() {
        assert_eq!(clock(0.0, false), "00:00");
        assert_eq!(clock(61.9, false), "01:01");
        assert_eq!(clock(4_500.0, false), "75:00");
        assert_eq!(clock(5.0, true), "-00:05");
        assert_eq!(clock(-3.0, false), "00:00");
    }

    #[test]
    fn lengths_and_title_lines() {
        assert_eq!(length(Some(185.4)), "3:05");
        assert_eq!(length(None), "");
        assert_eq!(length(Some(f64::NAN)), "");
        assert_eq!(title_line(3, "Song", Some(60.0)), "3. Song (1:00)");
        assert_eq!(title_line(1, "Song", None), "1. Song");
    }

    #[test]
    fn a_short_title_sits_still_and_a_long_one_scrolls_round() {
        let mut m = Marquee::default();
        m.set("abc");
        m.step(5);
        assert_eq!(m.window(5), "abc");
        m.set("abcdef");
        assert_eq!(m.window(4), "abcd");
        m.step(4);
        assert_eq!(m.window(4), "bcde");
        for _ in 0..5 {
            m.step(4);
        }
        assert_eq!(m.window(4), "  **");
        // All the way round is back to the start.
        for _ in 0..7 {
            m.step(4);
        }
        assert_eq!(m.window(4), "abcd");
        // Re-offering the same text keeps the scroll.
        m.step(4);
        m.set("abcdef");
        assert_eq!(m.window(4), "bcde");
    }
}
