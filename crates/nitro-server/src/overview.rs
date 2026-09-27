//! Overview mode's window grid: where every thumbnail goes.
//!
//! A port of `gnome-shell`'s `UnalignedLayoutStrategy` (`js/ui/workspace.js`),
//! read off source and written up with every constant in
//! `docs/research/overview.md` §2. Windows keep their aspect ratios, are
//! packed into rows of unequal length, and one global scale is fitted to the
//! area. This module is policy in the same sense as [`crate::wm`]: pure
//! arithmetic over rectangles, no scene, no I/O, no server state — the caller
//! gathers the windows of one output, passes that output's work area, and
//! turns each [`Slot`] into a translate plus a `Transform::scale`.
//!
//! # Fidelity
//!
//! The port is deliberately verbatim, quirks included, because the reason to
//! copy the constants at all is that they encode years of tuning:
//!
//! * **The `full_height` read-ahead.** In `computeLayout`'s greedy row loop,
//!   `row.fullHeight = Math.max(row.fullHeight, height)` runs *before* the
//!   `_keepSameRow(...) || i === numRows - 1` test, so the window that fails
//!   the test and moves on to the next row has already raised the height of
//!   the row it left. That feeds `grid_height`, hence the layout scale,
//!   hence every slot. It is upstream behaviour, kept on purpose; see
//!   `build_candidate`.
//! * **`better`'s mixed cases** compare the two weighted deltas as signed
//!   numbers, exactly as `_isBetterScaleAndSpace` does.
//!
//! The one deviation: [`window_scale`] clamps its height ratio, because
//! nitro, unlike mutter, lets a window be taller than the monitor.
//!
//! The one thing nitro supplies that the source takes from the theme is the
//! spacing between cells, [`COLUMN_SPACING`] and [`ROW_SPACING`]; their
//! doc comments say where the numbers come from.

use nitro_core::{Point, Rect, Size};
use nitro_scene::WindowKey;

/// The largest scale a thumbnail is ever drawn at: a thumbnail is never
/// nearly full size. `WINDOW_PREVIEW_MAXIMUM_SCALE` in `workspace.js`.
pub const WINDOW_PREVIEW_MAXIMUM_SCALE: f32 = 0.95;
/// Weight of a thumbnail-scale gain in the row-count search.
/// `LAYOUT_SCALE_WEIGHT` in `workspace.js`.
pub const LAYOUT_SCALE_WEIGHT: f32 = 1.0;
/// Weight of an area-coverage gain in the row-count search: bigger
/// thumbnails beat a tidier fill ten to one. `LAYOUT_SPACE_WEIGHT`.
pub const LAYOUT_SPACE_WEIGHT: f32 = 0.1;
/// `window_scale`'s bump for a window of zero height; a full-monitor-height
/// window gets [`WINDOW_SCALE_FULL`]. `_computeWindowScale`'s `lerp(1.5, 1.0, …)`.
pub const WINDOW_SCALE_SMALL: f32 = 1.5;
/// `window_scale` for a window exactly as tall as the monitor.
pub const WINDOW_SCALE_FULL: f32 = 1.0;
/// Horizontal gap between two cells of a row, logical pixels.
///
/// GNOME takes this from the theme; the gap measured between neighbouring
/// thumbnails in `docs/research/overview.md` §3 is ~10 px. nitro uses a
/// little more, because its app icon (64 px, centred on a thumbnail's bottom
/// edge) is the widest thing that hangs off a cell.
pub const COLUMN_SPACING: f32 = 16.0;
/// Vertical gap between two rows, logical pixels.
///
/// It must hold what hangs *below* a thumbnail: 30 % of the 64 px icon
/// (`ICON_OVERLAP = 0.7`), `ICON_TITLE_SPACING = 6` and the 32 px caption
/// pill — 57.2 px, rounded up. GNOME 3.20's measured row gap in §3 is 73 px
/// for the same reason.
pub const ROW_SPACING: f32 = 64.0;

/// One window to lay out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thumb {
    /// Which window this slot is for; opaque to the algorithm.
    pub window: WindowKey,
    /// The window's *frame* size, i.e. what will be scaled.
    pub size: Size,
    /// The window's current centre, which decides row assignment (by `y`)
    /// and order within a row (by `x`).
    pub centre: Point,
}

/// Where one window's thumbnail goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    /// The window, as passed in.
    pub window: WindowKey,
    /// Top-left of the scaled thumbnail, floored to whole pixels.
    pub pos: Point,
    /// The scaled size: `thumb.size * scale`.
    pub size: Size,
    /// The scale that was applied; the caller turns this into a
    /// `Transform::scale`. Never above [`WINDOW_PREVIEW_MAXIMUM_SCALE`].
    pub scale: f32,
}

impl Slot {
    /// The slot as a rect.
    #[must_use]
    pub fn rect(&self) -> Rect {
        Rect::new(self.pos.x, self.pos.y, self.size.w, self.size.h)
    }
}

/// `_computeWindowScale`: a per-window bump applied before layout so a
/// small window (a calculator) is not lost next to a large one.
///
/// The height ratio is clamped to `0..=1`, which upstream does not do:
/// mutter constrains windows to the monitor, nitro does not, and an
/// unclamped lerp turns negative for a window over 3× the monitor height.
#[must_use]
pub fn window_scale(height: f32, monitor_h: f32) -> f32 {
    let ratio = (f64::from(height) / f64::from(monitor_h)).clamp(0.0, 1.0);
    lerp(
        f64::from(WINDOW_SCALE_SMALL),
        f64::from(WINDOW_SCALE_FULL),
        ratio,
    ) as f32
}

/// `Util.lerp(start, end, progress)`.
fn lerp(start: f64, end: f64, progress: f64) -> f64 {
    start + progress * (end - start)
}

/// Lay `thumbs` out in `area`, one [`Slot`] per thumb, in row order (top row
/// first, left to right within a row).
///
/// `monitor_h` is the height of the output itself, not of the work area:
/// [`window_scale`]'s lerp is relative to the monitor, as in GNOME.
///
/// An empty input, or an `area` with no extent, yields no slots.
#[must_use]
pub fn layout(thumbs: &[Thumb], area: Rect, monitor_h: f32) -> Vec<Slot> {
    if thumbs.is_empty() || area.w <= 0.0 || area.h <= 0.0 {
        return Vec::new();
    }
    let area = Area {
        x: f64::from(area.x),
        y: f64::from(area.y),
        w: f64::from(area.w),
        h: f64::from(area.h),
    };
    let monitor_h = if monitor_h > 0.0 {
        monitor_h
    } else {
        area.h as f32
    };
    let windows: Vec<Win> = thumbs
        .iter()
        .map(|t| Win {
            thumb: *t,
            w: f64::from(t.size.w.max(1.0)),
            h: f64::from(t.size.h.max(1.0)),
            ws: f64::from(window_scale(t.size.h.max(1.0), monitor_h)),
        })
        .collect();
    let best = best_layout(&windows, &area);
    window_slots(&windows, &best, &area)
}

/// The layout area, widened to `f64` so the arithmetic matches JS numbers.
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// A thumb with its sizes widened and its `window_scale` precomputed.
struct Win {
    thumb: Thumb,
    w: f64,
    h: f64,
    ws: f64,
}

/// One row of a candidate layout.
#[derive(Default)]
struct Row {
    /// Indices into the window list, left to right once sorted.
    windows: Vec<usize>,
    /// Σ width × `window_scale` of the row's windows.
    full_width: f64,
    /// Max height × `window_scale` — including the read-ahead bump.
    full_height: f64,
}

/// One candidate: `computeLayout`'s return value plus the fitted scale.
struct Candidate {
    rows: Vec<Row>,
    max_columns: usize,
    grid_width: f64,
    grid_height: f64,
    scale: f64,
}

/// The outer search over row counts (`WorkspaceLayout._computeLayout`).
fn best_layout(windows: &[Win], area: &Area) -> Candidate {
    let mut last: Option<(Candidate, f64)> = None;
    let mut last_cols = usize::MAX;
    for num_rows in 1.. {
        let num_cols = windows.len().div_ceil(num_rows);
        // Another row bought no column: 9 windows in 3 rows is 3 columns,
        // and so is 4 rows.
        if num_cols == last_cols {
            break;
        }
        let mut cand = build_candidate(windows, num_rows);
        let (scale, space) = scale_and_space(&cand, area);
        cand.scale = scale;
        if let Some((prev, prev_space)) = &last
            && !better(prev.scale, *prev_space, scale, space)
        {
            break;
        }
        last = Some((cand, space));
        last_cols = num_cols;
    }
    last.expect("one row is always a candidate").0
}

/// `_isBetterScaleAndSpace`, verbatim.
fn better(old_scale: f64, old_space: f64, scale: f64, space: f64) -> bool {
    let space_power = (space - old_space) * f64::from(LAYOUT_SPACE_WEIGHT);
    let scale_power = (scale - old_scale) * f64::from(LAYOUT_SCALE_WEIGHT);
    if scale > old_scale && space > old_space {
        true
    } else if scale > old_scale {
        scale_power > space_power
    } else if space > old_space {
        space_power > scale_power
    } else {
        false
    }
}

/// `_keepSameRow`: the window fits under the ideal width, or overshooting
/// with it lands closer to ideal than stopping short does.
fn keep_same_row(row: &Row, width: f64, ideal: f64) -> bool {
    if row.full_width + width <= ideal {
        return true;
    }
    let old_ratio = row.full_width / ideal;
    let new_ratio = (row.full_width + width) / ideal;
    (1.0 - new_ratio).abs() < (1.0 - old_ratio).abs()
}

/// `computeLayout`: greedy row filling over the vertically sorted windows.
fn build_candidate(windows: &[Win], num_rows: usize) -> Candidate {
    let total_width: f64 = windows.iter().map(|w| w.w * w.ws).sum();
    let ideal = total_width / num_rows as f64;

    // Vertical sort decides which row a window lands in ("minimize travel
    // distance"). Stable, like JS's `Array.prototype.sort`.
    let mut sorted: Vec<usize> = (0..windows.len()).collect();
    sorted.sort_by(|&a, &b| {
        windows[a]
            .thumb
            .centre
            .y
            .total_cmp(&windows[b].thumb.centre.y)
    });

    let mut rows: Vec<Row> = Vec::with_capacity(num_rows);
    let mut idx = 0;
    for i in 0..num_rows {
        let mut row = Row::default();
        while idx < sorted.len() {
            let w = &windows[sorted[idx]];
            let width = w.w * w.ws;
            let height = w.h * w.ws;
            // Upstream quirk, kept verbatim: the height bump happens before
            // the keep-same-row test, so a window that breaks out to the
            // next row has still raised this row's height. See the module doc.
            row.full_height = row.full_height.max(height);
            if keep_same_row(&row, width, ideal) || i == num_rows - 1 {
                row.windows.push(sorted[idx]);
                row.full_width += width;
                idx += 1;
            } else {
                break;
            }
        }
        rows.push(row);
    }

    let mut grid_height = 0.0;
    let mut max_row = 0;
    for row in &mut rows {
        // Horizontal sort decides the order within a row.
        row.windows.sort_by(|&a, &b| {
            windows[a]
                .thumb
                .centre
                .x
                .total_cmp(&windows[b].thumb.centre.x)
        });
        grid_height += row.full_height;
    }
    for (i, row) in rows.iter().enumerate() {
        // `!maxRow || row.fullWidth > maxRow.fullWidth`: the first widest wins.
        if row.full_width > rows[max_row].full_width {
            max_row = i;
        }
    }
    Candidate {
        max_columns: rows[max_row].windows.len(),
        grid_width: rows[max_row].full_width,
        grid_height,
        rows,
        scale: 0.0,
    }
}

/// `_computeScaleAndSpace`: the largest global scale at which the grid
/// fits `area` (capped at [`WINDOW_PREVIEW_MAXIMUM_SCALE`]), and the
/// fraction of `area` the scaled grid then covers.
fn scale_and_space(cand: &Candidate, area: &Area) -> (f64, f64) {
    let hspacing = cand.max_columns.saturating_sub(1) as f64 * f64::from(COLUMN_SPACING);
    let vspacing = cand.rows.len().saturating_sub(1) as f64 * f64::from(ROW_SPACING);
    let horizontal = (area.w - hspacing) / cand.grid_width;
    let vertical = (area.h - vspacing) / cand.grid_height;
    let scale = horizontal
        .min(vertical)
        .min(f64::from(WINDOW_PREVIEW_MAXIMUM_SCALE));
    let space = (cand.grid_width * scale + hspacing) * (cand.grid_height * scale + vspacing)
        / (area.w * area.h);
    (scale, space)
}

/// `Math.floor`, forgiving float noise: a coordinate a hair under a whole
/// pixel (`-1e-13` for a row at the area's top edge) is that pixel, not the
/// one before it, which would put the slot outside the area.
fn snap(v: f64) -> f32 {
    (v + 1e-6).floor() as f32
}

/// A row's placement, from `_computeRowSizes`' pass in `computeWindowSlots`.
struct Placed {
    x: f64,
    y: f64,
    height: f64,
    extra: f64,
}

/// `computeWindowSlots` (with `_computeRowSizes` folded in).
fn window_slots(windows: &[Win], cand: &Candidate, area: &Area) -> Vec<Slot> {
    let scale = cand.scale;
    if scale <= 0.0 {
        // The spacing alone does not fit the area: there is no layout.
        return Vec::new();
    }
    let column_spacing = f64::from(COLUMN_SPACING);
    let row_spacing = f64::from(ROW_SPACING);
    let rows = &cand.rows;
    let n_rows = rows.len();

    // Row sizes at the global scale.
    let row_h: Vec<f64> = rows.iter().map(|r| r.full_height * scale).collect();
    let height_no_spacing: f64 = row_h.iter().sum();
    let vspacing = n_rows.saturating_sub(1) as f64 * row_spacing;
    let extra_v = ((area.h - vspacing) / height_no_spacing).min(1.0);

    // Per-row fit-up: a row with more windows than the widest one has more
    // spacing, and may not fit at the global scale; it gets an additional
    // scale of its own. Shrinking a row horizontally shortens the grid,
    // which `compensation` re-centres.
    let mut compensation = 0.0;
    let mut y = 0.0;
    let mut placed = Vec::with_capacity(n_rows);
    for (row, &height) in rows.iter().zip(&row_h) {
        let hspacing = row.windows.len().saturating_sub(1) as f64 * column_spacing;
        let width_no_spacing = row.full_width * scale;
        let extra_h = ((area.w - hspacing) / width_no_spacing).min(1.0);
        let extra = if extra_h < extra_v {
            compensation += (extra_v - extra_h) * height;
            extra_h
        } else {
            // No compensation when scaling vertically: centring on a too
            // large height would undo what the vertical scale achieves.
            extra_v
        };
        let x = area.x + (area.w - (width_no_spacing * extra + hspacing)).max(0.0) / 2.0;
        let row_y = area.y + (area.h - (height_no_spacing + vspacing)).max(0.0) / 2.0 + y;
        y += height * extra + row_spacing;
        placed.push(Placed {
            x,
            y: row_y,
            height,
            extra,
        });
    }
    compensation /= 2.0;

    let cap = f64::from(WINDOW_PREVIEW_MAXIMUM_SCALE);
    let mut slots = Vec::with_capacity(windows.len());
    for (row, p) in rows.iter().zip(&placed) {
        let row_y = p.y + compensation;
        let row_height = p.height * p.extra;
        let mut x = p.x;
        for &i in &row.windows {
            let w = &windows[i];
            let s = scale * w.ws * p.extra;
            let cell_w = w.w * s;
            let cell_h = w.h * s;
            // "We simply cheat": the cell keeps the uncapped size and the
            // capped thumbnail is centred in it, so capping the one big
            // window does not shrink every other window.
            let s = s.min(cap);
            let clone_w = w.w * s;
            let clone_h = w.h * s;
            let clone_x = x + (cell_w - clone_w) / 2.0;
            let clone_y = if n_rows == 1 {
                // One row: centre vertically in the row.
                row_y + (row_height - clone_h) / 2.0
            } else {
                // Several rows: align cells to the row's bottom edge.
                row_y + row_height - cell_h
            };
            slots.push(Slot {
                window: w.thumb.window,
                // Align with the pixel grid: `blit`'s 1:1 fast path wants
                // an integer destination.
                pos: Point::new(snap(clone_x), snap(clone_y)),
                size: Size::new(
                    (f64::from(w.thumb.size.w) * s) as f32,
                    (f64::from(w.thumb.size.h) * s) as f32,
                ),
                scale: s as f32,
            });
            x += cell_w + column_spacing;
        }
    }
    slots
}
