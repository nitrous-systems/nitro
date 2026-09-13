//! Shaping and layout: text plus a [`TextStyle`] in, positioned glyphs out.
//!
//! LTR only, one script for the whole string, greedy whitespace wrapping.
//! [`Layout`] owns swash's `ShapeContext` (its caches and scratch buffers), so
//! the server keeps one and reuses it.

use swash::shape::ShapeContext;
use swash::text::{Script, analyze};
use swash::{FontRef, Metrics as FontMetrics};

use crate::db::{FontDb, FontId, TextStyle};

/// Tab stop, in spaces. Tabs are expanded before shaping.
const TAB_SPACES: usize = 4;

/// One positioned glyph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    /// Face the glyph belongs to — the run's font, which may be a fallback.
    pub font: FontId,
    /// Glyph id within that face.
    pub id: u16,
    /// X of the glyph origin, relative to the start of its line.
    pub x: f32,
    /// Y of the glyph origin, relative to the line's baseline (down positive).
    pub y: f32,
    /// Advance of this glyph.
    pub advance: f32,
    /// Byte offset into the *original* text of the cluster this glyph came
    /// from. Several glyphs may share a cluster; a tab's four expanded spaces
    /// all report the offset of the tab.
    pub cluster: u32,
}

/// One laid-out line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// Glyphs, in visual (= logical, LTR) order.
    pub glyphs: Vec<Glyph>,
    /// Line width, excluding trailing whitespace.
    pub width: f32,
    /// Distance from the baseline up to the top of the line box.
    pub ascent: f32,
    /// Distance from the baseline down to the bottom of the line box.
    pub descent: f32,
    /// Y of this line's baseline relative to the text block's top, y down.
    pub baseline: f32,
}

/// A shaped paragraph: every line, with the block's overall box.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ShapedText {
    /// The lines, top to bottom.
    pub lines: Vec<Line>,
    /// Width of the widest line.
    pub width: f32,
    /// Sum of the line heights (ascent + descent + leading per line).
    pub height: f32,
    /// First line's ascent — what a caller aligns a single-line label by.
    pub ascent: f32,
    /// First line's descent.
    pub descent: f32,
    /// Em size the text was shaped at, in device pixels.
    pub size_px: f32,
}

/// Measurement result: the same layout, plus cursor positions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Metrics {
    /// Width of the widest line.
    pub width: f32,
    /// Total height.
    pub height: f32,
    /// First line's ascent.
    pub ascent: f32,
    /// First line's descent.
    pub descent: f32,
    /// Number of lines.
    pub line_count: u32,
    /// `(byte offset, x)` per cluster boundary, in increasing byte order; `x`
    /// is the pen position of that cluster *within its line*, so it resets to
    /// 0 at each line start and is non-decreasing within a line.
    ///
    /// A hard break's own offset gets a pair at the end of the line it
    /// terminates (so a caret can sit past the last character of a line); a
    /// soft wrap needs none, because the next line's first cluster already
    /// carries that offset. The last pair is the end-of-text offset. Offsets
    /// never repeat.
    pub cursor_x: Vec<(u32, f32)>,
}

/// The shaper. Holds swash's shaping caches; keep one per server.
pub struct Layout {
    shape_cx: ShapeContext,
}

impl Default for Layout {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layout").finish_non_exhaustive()
    }
}

impl Layout {
    /// A fresh shaper with empty caches.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shape_cx: ShapeContext::new(),
        }
    }

    /// Shape and lay out `text`.
    ///
    /// `max_width` bounds the lines when `wrap` is true; with `wrap` false it
    /// is ignored (lines break only at `\n`). Returns an empty [`ShapedText`]
    /// when the db has no usable face.
    pub fn shape(
        &mut self,
        db: &FontDb,
        text: &str,
        style: &TextStyle,
        max_width: Option<f32>,
        wrap: bool,
    ) -> ShapedText {
        self.run(db, text, style, max_width, wrap, false).0
    }

    /// Lay out `text` and report its box plus cursor positions.
    pub fn measure(
        &mut self,
        db: &FontDb,
        text: &str,
        style: &TextStyle,
        max_width: Option<f32>,
        wrap: bool,
    ) -> Metrics {
        let (shaped, cursor_x) = self.run(db, text, style, max_width, wrap, true);
        Metrics {
            width: shaped.width,
            height: shaped.height,
            ascent: shaped.ascent,
            descent: shaped.descent,
            line_count: shaped.lines.len() as u32,
            cursor_x,
        }
    }

    /// The shared layout pass. `want_cursor` adds the cursor table.
    fn run(
        &mut self,
        db: &FontDb,
        text: &str,
        style: &TextStyle,
        max_width: Option<f32>,
        wrap: bool,
        want_cursor: bool,
    ) -> (ShapedText, Vec<(u32, f32)>) {
        let chain = db.fallbacks(style);
        let Some(primary) = chain.first().copied() else {
            return (ShapedText::default(), Vec::new());
        };
        let fonts: Vec<(FontId, FontRef<'_>)> = chain
            .iter()
            .filter_map(|id| db.font_ref(*id).map(|f| (*id, f)))
            .collect();
        if fonts.is_empty() {
            return (ShapedText::default(), Vec::new());
        }
        let size = style.size_px.max(1.0);
        let script = detect_script(text);
        let default_metrics = fonts[0].1.metrics(&[]).scale(size);

        let mut out = ShapedText {
            size_px: size,
            ..ShapedText::default()
        };
        let mut cursor: Vec<(u32, f32)> = Vec::new();
        let mut baseline = 0.0f32;
        for logical in split_logical_lines(text) {
            let (disp, map) = expand_tabs(&text[logical.clone()], logical.start as u32);
            let (clusters, metrics) = self.shape_line(&disp, &map, &fonts, primary, size, script);
            let metrics = metrics.unwrap_or(default_metrics);
            let (ascent, descent, leading) = (
                metrics.ascent.max(0.0),
                metrics.descent.max(0.0),
                metrics.leading.max(0.0),
            );
            for range in wrap_line(&clusters, max_width, wrap) {
                let (glyphs, width, cursors) = emit_line(&clusters[range], want_cursor);
                baseline += ascent;
                if want_cursor {
                    push_cursors(&mut cursor, &cursors);
                }
                out.width = out.width.max(width);
                out.height += ascent + descent + leading;
                out.lines.push(Line {
                    glyphs,
                    width,
                    ascent,
                    descent,
                    baseline,
                });
                baseline += descent + leading;
            }
            if want_cursor {
                // A hard break's own offset gets a pair at the end of the last
                // visual line it produced, so a caret can sit past the last
                // character of a line. A *soft* break needs none: the next
                // line's first cluster already carries that offset.
                let end_x = out.lines.last().map_or(0.0, line_end_x);
                push_cursors(&mut cursor, &[(logical.end as u32, end_x)]);
            }
        }
        if let Some(first) = out.lines.first() {
            out.ascent = first.ascent;
            out.descent = first.descent;
        }
        if want_cursor {
            let end_x = out.lines.last().map_or(0.0, line_end_x);
            push_cursors(&mut cursor, &[(text.len() as u32, end_x)]);
        }
        (out, cursor)
    }

    /// Shape one logical line into clusters, splitting it into runs by font.
    ///
    /// Returns the clusters and the widest font metrics seen on the line
    /// (`None` when the line produced no run at all, i.e. it is empty).
    fn shape_line(
        &mut self,
        disp: &str,
        map: &[u32],
        fonts: &[(FontId, FontRef<'_>)],
        primary: FontId,
        size: f32,
        script: Script,
    ) -> (Vec<ClusterItem>, Option<FontMetrics>) {
        let mut clusters = Vec::new();
        let mut metrics: Option<FontMetrics> = None;
        for run in split_runs(disp, fonts, primary) {
            let Some((_, font_ref)) = fonts.iter().find(|(id, _)| *id == run.font) else {
                continue;
            };
            let mut shaper = self
                .shape_cx
                .builder(*font_ref)
                .script(script)
                .direction(swash::shape::Direction::LeftToRight)
                .size(size)
                .build();
            let run_metrics = shaper.metrics();
            metrics = Some(match metrics {
                Some(m) => wider(m, run_metrics),
                None => run_metrics,
            });
            shaper.add_str(&disp[run.start..run.end]);
            let base = run.start;
            shaper.shape_with(|cluster| {
                let disp_start = base + cluster.source.start as usize;
                let item = ClusterItem {
                    font: run.font,
                    source: map.get(disp_start).copied().unwrap_or(0),
                    advance: cluster.advance(),
                    whitespace: disp[disp_start..]
                        .chars()
                        .next()
                        .is_some_and(char::is_whitespace),
                    glyphs: cluster
                        .glyphs
                        .iter()
                        .map(|g| (g.id, g.x, g.y, g.advance))
                        .collect(),
                };
                clusters.push(item);
            });
        }
        (clusters, metrics)
    }
}

/// One shaped cluster, before line breaking.
#[derive(Debug, Clone)]
struct ClusterItem {
    font: FontId,
    /// Byte offset in the original text.
    source: u32,
    advance: f32,
    whitespace: bool,
    /// `(glyph id, x offset, y offset, advance)`.
    glyphs: Vec<(u16, f32, f32, f32)>,
}

/// A contiguous range of the display string shaped with one font.
#[derive(Debug, Clone, Copy)]
struct Run {
    font: FontId,
    start: usize,
    end: usize,
}

/// Keep the more generous of two metric sets, so a line with a fallback run
/// gets a line box that fits both fonts.
fn wider(a: FontMetrics, b: FontMetrics) -> FontMetrics {
    let mut out = a;
    out.ascent = a.ascent.max(b.ascent);
    out.descent = a.descent.max(b.descent);
    out.leading = a.leading.max(b.leading);
    out
}

/// Script of the first strong character, Latin when there is none.
fn detect_script(text: &str) -> Script {
    for (props, _) in analyze(text.chars()) {
        let script = props.script();
        if !matches!(script, Script::Common | Script::Inherited | Script::Unknown) {
            return script;
        }
    }
    Script::Latin
}

/// Byte ranges of the logical lines, splitting on `\n` and `\r\n`.
fn split_logical_lines(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            let end = if i > 0 && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            out.push(start..end);
            start = i + 1;
        }
    }
    out.push(start..text.len());
    out
}

/// Expand tabs to [`TAB_SPACES`] spaces.
///
/// Returns the display string and a table mapping every byte index of it (plus
/// the end) to a byte offset in the original text; `base` is the offset of the
/// line in that text.
fn expand_tabs(line: &str, base: u32) -> (String, Vec<u32>) {
    let mut disp = String::with_capacity(line.len());
    let mut map = Vec::with_capacity(line.len() + 1);
    for (i, ch) in line.char_indices() {
        let src = base + i as u32;
        if ch == '\t' {
            for _ in 0..TAB_SPACES {
                disp.push(' ');
                map.push(src);
            }
        } else {
            disp.push(ch);
            for _ in 0..ch.len_utf8() {
                map.push(src);
            }
        }
    }
    map.push(base + line.len() as u32);
    (disp, map)
}

/// Split a display string into runs by font, following the fallback chain.
///
/// A character is shaped with the run's current font when that font has a
/// glyph for it, else with the first font in the chain that does; whitespace
/// and control characters never break a run. This is *per run*, not per
/// script: a string that mixes two writing systems gets one font per
/// contiguous stretch, which is all M2 needs.
fn split_runs(disp: &str, fonts: &[(FontId, FontRef<'_>)], primary: FontId) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    let charmaps: Vec<(FontId, swash::Charmap<'_>)> =
        fonts.iter().map(|(id, f)| (*id, f.charmap())).collect();
    let mut current = primary;
    for (i, ch) in disp.char_indices() {
        if !ch.is_whitespace() && !ch.is_control() {
            let keeps = charmaps
                .iter()
                .find(|(id, _)| *id == current)
                .is_some_and(|(_, cm)| cm.map(ch) != 0);
            if !keeps {
                current = charmaps
                    .iter()
                    .find(|(_, cm)| cm.map(ch) != 0)
                    .map_or(primary, |(id, _)| *id);
            }
        }
        match runs.last_mut() {
            Some(run) if run.font == current => run.end = i + ch.len_utf8(),
            _ => runs.push(Run {
                font: current,
                start: i,
                end: i + ch.len_utf8(),
            }),
        }
    }
    runs
}

/// Greedy line breaking: the cluster ranges each visual line covers.
fn wrap_line(
    clusters: &[ClusterItem],
    max_width: Option<f32>,
    wrap: bool,
) -> Vec<std::ops::Range<usize>> {
    let limit = match (wrap, max_width) {
        (true, Some(w)) if w > 0.0 => w,
        _ => return std::iter::once(0..clusters.len()).collect(),
    };
    let mut lines = Vec::new();
    let mut start = 0;
    let mut x = 0.0f32;
    let mut last_break: Option<usize> = None;
    for (i, cluster) in clusters.iter().enumerate() {
        if !cluster.whitespace && i > start && x + cluster.advance > limit {
            // Break after the last whitespace on this line; a word longer than
            // the limit has no break opportunity and is cut here instead.
            let brk = match last_break {
                Some(b) if b > start => b,
                _ => i,
            };
            lines.push(start..brk);
            start = brk;
            x = clusters[brk..i].iter().map(|c| c.advance).sum();
            last_break = None;
        }
        x += cluster.advance;
        if cluster.whitespace {
            last_break = Some(i + 1);
        }
    }
    lines.push(start..clusters.len());
    lines
}

/// Position one visual line's clusters, returning glyphs, width and cursors.
///
/// The width excludes trailing whitespace, so a wrapped line never exceeds the
/// wrap width because of the space it broke at.
fn emit_line(clusters: &[ClusterItem], want_cursor: bool) -> (Vec<Glyph>, f32, Vec<(u32, f32)>) {
    let mut glyphs = Vec::new();
    let mut cursors = Vec::new();
    let mut x = 0.0f32;
    let mut width = 0.0f32;
    for cluster in clusters {
        if want_cursor {
            cursors.push((cluster.source, x));
        }
        let mut pen = x;
        for (id, gx, gy, advance) in &cluster.glyphs {
            glyphs.push(Glyph {
                font: cluster.font,
                id: *id,
                x: pen + gx,
                y: *gy,
                advance: *advance,
                cluster: cluster.source,
            });
            pen += advance;
        }
        x += cluster.advance;
        if !cluster.whitespace {
            width = x;
        }
    }
    (glyphs, width, cursors)
}

/// Pen x just past a line's last cluster (including trailing whitespace).
fn line_end_x(line: &Line) -> f32 {
    line.glyphs
        .last()
        .map_or(line.width, |g| (g.x + g.advance).max(line.width))
}

/// Append cursor pairs, dropping repeats of the previous byte offset.
///
/// A tab expands to four clusters that all carry the tab's offset; only the
/// first of them is a real cursor position.
fn push_cursors(out: &mut Vec<(u32, f32)>, add: &[(u32, f32)]) {
    for (offset, x) in add {
        if out.last().is_some_and(|(prev, _)| prev == offset) {
            continue;
        }
        out.push((*offset, *x));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_lines_split_on_lf_and_crlf() {
        let text = "a\nb\r\nc";
        let lines: Vec<&str> = split_logical_lines(text)
            .into_iter()
            .map(|r| &text[r])
            .collect();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn trailing_newline_makes_an_empty_last_line() {
        let text = "a\n";
        assert_eq!(split_logical_lines(text).len(), 2);
    }

    #[test]
    fn tabs_expand_and_map_back_to_the_tab() {
        let (disp, map) = expand_tabs("a\tb", 10);
        assert_eq!(disp, "a    b");
        assert_eq!(map[0], 10);
        // All four spaces report the tab's offset.
        assert_eq!(&map[1..5], &[11, 11, 11, 11]);
        assert_eq!(map[5], 12);
        assert_eq!(map[6], 13);
    }

    #[test]
    fn cursor_pairs_dedupe_repeated_offsets() {
        let mut out = Vec::new();
        push_cursors(&mut out, &[(0, 0.0), (1, 4.0), (1, 8.0), (2, 12.0)]);
        assert_eq!(out, vec![(0, 0.0), (1, 4.0), (2, 12.0)]);
    }
}
