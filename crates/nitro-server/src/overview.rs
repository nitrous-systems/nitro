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

use nitro_core::{Color, Point, Rect, Size, Transform};
use nitro_scene::{
    ClientId, Error as SceneError, Fill, IconRef, Layer, NodeKey, NodeKind, OutputId,
    Scene, WindowFlags, WindowKey, WindowState,
};

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
/// (`ICON_OVERLAP = 0.7`), 19.2 px, rounded up with a little slack. The
/// badge is the icon alone since the thumbnail shows the window's own
/// (scaled) title bar; with the old caption pill underneath it was 64.
/// GNOME 3.20's measured row gap in `docs/research/overview.md` §3 is
/// 73 px because GNOME still hangs a caption there.
pub const ROW_SPACING: f32 = 24.0;

/// One window to lay out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thumb {
    /// Which window this slot is for; opaque to the algorithm.
    pub window: WindowKey,
    /// The window's *frame* size, i.e. what will be scaled: a thumbnail
    /// is the whole window, title bar and border included (`docs/wm.md`
    /// §Overview mode). For an undecorated (or fullscreen) window that is
    /// its content size.
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

// ------------------------------------------------------------ overview mode
//
// Everything above is the layout: pure arithmetic. Everything below turns
// a layout into scene state and back — the primitive task #3788 built —
// and is still free of server state: the caller (`Server::enter_overview`
// and friends in `lib.rs`) decides *which* windows and *when*, these
// helpers only know how to scale one window, build one badge and make the
// scrim. `docs/wm.md` §Overview mode has the design.

/// The scrim's colour: black at 5/8 alpha, over the whole output.
///
/// A constant rather than a palette role because it is a *dimming*, not
/// a colour — it has to read the same over a light desktop and a dark
/// one — which is also why GNOME's is not themed.
pub const SCRIM: Color = Color::rgba(0, 0, 0, 0xA0);

/// The app icon hanging off each thumbnail's bottom edge, logical pixels.
pub const OVERVIEW_ICON: f32 = 64.0;

/// How much of the icon overlaps the thumbnail: 70 %, so 30 % hangs below
/// it. GNOME's `ICON_OVERLAP`. [`ROW_SPACING`] is derived from it.
pub const ICON_OVERLAP: f32 = 0.7;

/// The overview's grid area: the output's work area minus the reserved
/// search band at its top ([`nitro_wire::types::overview::search_band`]),
/// where the launcher's search field goes. Reserved on every entry,
/// searching or not, so the thumbnails never move when typing starts.
#[must_use]
pub fn grid_area(work: Rect) -> Rect {
    #[allow(clippy::cast_precision_loss)] // a few dozen pixels
    let band = nitro_wire::types::overview::search_band() as f32;
    let band = band.min(work.h.max(0.0));
    Rect::new(work.x, work.y + band, work.w, work.h - band)
}

/// The size a thumbnail's title is shaped at so that, drawn under the
/// thumbnail's scale `k`, it rasterizes at a **whole** pixel size:
/// `round(px·k) / k` (at least `1 / k`).
///
/// Glyphs are cached per device size (`GlyphKey` quantizes to 1/64 px),
/// so a title drawn at `13·k` for each thumbnail's own `k` would
/// rasterize a fresh glyph set per thumbnail. Snapped, every thumbnail
/// shares one of a few whole sizes (4, 5, 6 … px), and re-entering the
/// overview finds them in the atlas. `docs/wm.md` §Overview mode has the
/// measurements.
#[must_use]
pub fn snapped_text_size(px: f32, k: f32) -> f32 {
    if !(k.is_finite() && k > 0.0) {
        return px;
    }
    (px * k).round().max(1.0) / k
}

/// What overview mode remembers about one thumbnail, so leaving can put
/// the window back exactly as it was.
#[derive(Debug, Clone)]
pub struct ThumbState {
    /// The window.
    pub window: WindowKey,
    /// Where its frame went, output-local logical pixels.
    pub slot: Slot,
    /// What a click selects: the slot, grown by the icon hanging below
    /// it. Output-local logical pixels.
    pub hit: Rect,
    /// The transform of the node we overwrote: the frame root's for a
    /// framed window (the server's own, so identity in practice), the
    /// client's own root's for an undecorated one.
    pub saved_transform: Transform,
    /// An undecorated window is **moved** to its slot as well as scaled
    /// (see [`apply_thumb`]); this is where it was.
    pub saved_position: Option<Point>,
    /// Whether we made a `Minimized` window's root visible, and so owe it
    /// a re-hide.
    pub unhid: bool,
    /// The group holding the icon, if one was built.
    pub badge: Option<NodeKey>,
}

/// One output's overview: the scrim plus every thumbnail's restore state.
#[derive(Debug, Clone)]
pub struct Overview {
    /// The output it is on. Only one output is in overview at a time.
    pub output: OutputId,
    /// The server-owned window that dims the desktop; see [`create_scrim`].
    pub scrim: WindowKey,
    /// One per thumbnail, in slot order.
    pub thumbs: Vec<ThumbState>,
    /// When the badges started fading in (`CLOCK_MONOTONIC` ns): `Some`
    /// while they are fading, `None` once settled. See [`badge_opacity`].
    pub fade_start_ns: Option<u64>,
    /// Whether search results have replaced the grid (the shell sent
    /// [`OverviewRequest::Search`](nitro_wire::types::OverviewRequest::Search)):
    /// every thumbnail and badge is hidden and [`Overview::slot_at`]
    /// selects nothing. See [`set_grid_visible`].
    pub grid_hidden: bool,
}

impl Overview {
    /// The window whose thumbnail (or badge) is under an output-local
    /// point, if any.
    ///
    /// Selection is by **geometry**, not by which node the scene hit: that
    /// makes it indifferent to badges, the scrim, and whether a window is
    /// framed.
    ///
    /// While the grid is hidden behind search results this is `None`
    /// everywhere — GNOME's `_workspacesDisplay.reactive = false`. Hiding
    /// the thumbnails' nodes is not enough on its own, and neither is the
    /// overview swallowing `Normal`-layer pointer events (#3788): that
    /// only stops the *clients* hearing them, while selection here is by
    /// slot geometry and would still pick an invisible thumbnail. So a
    /// click on the hidden grid leaves without selecting, like a click on
    /// the bare scrim.
    #[must_use]
    pub fn slot_at(&self, point: Point) -> Option<WindowKey> {
        if self.grid_hidden {
            return None;
        }
        self.thumbs
            .iter()
            .find(|t| t.slot.rect().contains(point))
            .or_else(|| self.thumbs.iter().find(|t| t.hit.contains(point)))
            .map(|t| t.window)
    }

    /// Whether `win` has a thumbnail here.
    #[must_use]
    pub fn contains(&self, win: WindowKey) -> bool {
        self.thumbs.iter().any(|t| t.window == win)
    }
}

/// A window as the layout sees it: its **frame** rectangle (the content
/// rectangle for an undecorated or fullscreen window, whose insets are
/// zero), in its output's logical space.
#[must_use]
pub fn thumb_of(win: WindowKey, info: &nitro_scene::Window) -> Thumb {
    let r = info.frame_rect();
    Thumb {
        window: win,
        size: Size::new(r.w, r.h),
        centre: Point::new(r.x + r.w / 2.0, r.y + r.h / 2.0),
    }
}

/// The frame-root transform that puts a framed window's frame exactly on
/// `slot`: `translate(slot.pos - pos) ∘ scale(k)`.
///
/// `pos` is the frame's top-left, output-local; after the transform it
/// lands on `slot.pos` — whole pixels, because slots are floored — and
/// the content at `slot.pos + k*inset`.
#[must_use]
pub fn thumb_transform(pos: Point, slot: &Slot) -> Transform {
    let k = slot.scale;
    Transform::translate(slot.pos.x - pos.x, slot.pos.y - pos.y).then(&Transform::scale(k, k))
}

/// Scale one window onto its slot. Returns the transform overwritten and,
/// for an undecorated window, the position it was moved from.
///
/// **Framed:** only the frame root's transform changes — the server's own
/// node, so the client's tree is untouched and cannot notice.
///
/// **Undecorated:** the root *is* the client's content group, and that
/// group clips to its own bounds under its *pre-transform* world
/// transform. A translate on it would carry the content outside that
/// clip and nothing would be drawn, so the window is instead moved to the
/// slot (a server-side `place_window`, which sends no `Configure`) and
/// only scaled about its origin. The clip then spans the unscaled size at
/// the slot, which contains the scaled content.
///
/// # Errors
/// Anything the scene refuses; in practice a dead key.
pub fn apply_thumb(
    scene: &mut Scene,
    win: WindowKey,
    slot: &Slot,
) -> Result<(Transform, Option<Point>), SceneError> {
    let info = scene.window_info(win)?;
    let (root, framed, pos, output) = (
        info.root(),
        info.is_framed(),
        info.position(),
        info.output(),
    );
    let saved = scene.node(root)?.transform();
    if framed {
        scene.set_transform(ClientId::SERVER, root, thumb_transform(pos, slot))?;
        Ok((saved, None))
    } else {
        scene.place_window(win, output, slot.pos)?;
        let k = slot.scale;
        scene.set_transform(ClientId::SERVER, root, Transform::scale(k, k))?;
        Ok((saved, Some(pos)))
    }
}

/// Undo [`apply_thumb`].
///
/// # Errors
/// Anything the scene refuses; in practice a dead key.
pub fn restore_thumb(
    scene: &mut Scene,
    win: WindowKey,
    saved_transform: Transform,
    saved_position: Option<Point>,
) -> Result<(), SceneError> {
    let info = scene.window_info(win)?;
    let (root, output) = (info.root(), info.output());
    scene.set_transform(ClientId::SERVER, root, saved_transform)?;
    if let Some(pos) = saved_position {
        scene.place_window(win, output, pos)?;
    }
    Ok(())
}

/// Create the scrim: a server-owned, undecorated, unfocusable
/// `Layer::Normal` window covering the output, **lowered to the bottom of
/// its layer** — above every `Background` window (the wallpaper), below
/// every real window.
///
/// Why a window: `create_node` needs a parent and every node hangs off
/// some window's tree, and no existing window spans the output at the
/// right depth. Why it stays at the bottom: nothing in the server calls
/// `lower`, and `raise` and `place_window` only ever push to the *front*
/// of a layer, so no later raise can put a thumbnail under it. A unit test
/// pins that.
///
/// It is never added to the window manager's MRU list and belongs to no
/// wire client, so no shell ever lists it.
///
/// # Errors
/// Anything the scene refuses; in practice an unknown output.
pub fn create_scrim(
    scene: &mut Scene,
    output: OutputId,
    size: Size,
) -> Result<WindowKey, SceneError> {
    let s = ClientId::SERVER;
    let win = scene.create_window_with(
        s,
        "overview",
        size,
        Layer::Normal,
        WindowFlags {
            decorated: false,
            fixed_size: true,
            focusable: false,
        },
    );
    let build = |scene: &mut Scene| -> Result<(), SceneError> {
        scene.place_window(win, Some(output), Point::ZERO)?;
        scene.lower(win)?;
        let root = scene.window_info(win)?.root();
        let rect = scene.create_node(s, NodeKind::Rect, root, None)?;
        scene.set_bounds(s, rect, Rect::new(0.0, 0.0, size.w, size.h))?;
        scene.set_fill(s, rect, Fill::Solid(SCRIM))
    };
    if let Err(e) = build(scene) {
        let _ = scene.destroy_window(s, win);
        return Err(e);
    }
    Ok(win)
}

/// What goes in one thumbnail's badge: the app icon, `(handle, role)` as
/// `IconEngine` resolved it. The title is not repeated under the
/// thumbnail — the scaled title bar shows it.
#[derive(Debug, Clone, Copy)]
pub struct Badge {
    /// The app icon.
    pub icon: Option<(u32, u8)>,
}

/// Build one thumbnail's icon group under `parent`, on top of its
/// siblings.
///
/// `origin` is the thumbnail's bottom-centre in `parent`'s coordinates
/// and `inv_scale` undoes whatever scale `parent` is under: for a framed
/// window the group hangs off the *scaled* frame root and is given
/// `scale(1/k)`, so its world scale is exactly the output's and the
/// `IconEngine` rasterizes the icon at its ordinary 64 px rather than a
/// fresh size per thumbnail.
///
/// # Errors
/// Anything the scene refuses.
pub fn build_badge(
    scene: &mut Scene,
    parent: NodeKey,
    origin: Point,
    inv_scale: f32,
    badge: &Badge,
) -> Result<NodeKey, SceneError> {
    let s = ClientId::SERVER;
    let group = scene.create_node(s, NodeKind::Group, parent, None)?;
    scene.set_bounds(s, group, Rect::new(origin.x, origin.y, 0.0, 0.0))?;
    scene.set_transform(s, group, Transform::scale(inv_scale, inv_scale))?;
    if let Some((handle, role)) = badge.icon {
        let icon = scene.create_node(s, NodeKind::Icon, group, None)?;
        scene.set_bounds(s, icon, badge_rect())?;
        scene.set_icon(s, icon, Some(IconRef::new(handle, OVERVIEW_ICON, role)))?;
    }
    Ok(group)
}

/// The badge's icon box relative to the thumbnail's bottom-centre, in
/// unscaled logical pixels: 70 % over the thumbnail, 30 % below it.
#[must_use]
pub fn badge_rect() -> Rect {
    Rect::new(
        -OVERVIEW_ICON / 2.0,
        -(OVERVIEW_ICON * ICON_OVERLAP).round(),
        OVERVIEW_ICON,
        OVERVIEW_ICON,
    )
}

/// How far the badge hangs below a thumbnail's bottom edge: what a click
/// there still selects.
#[must_use]
pub fn badge_below() -> f32 {
    let r = badge_rect();
    r.y + r.h
}

/// How long the badges take to fade in on entry:
/// `WINDOW_OVERLAY_FADE_TIME` in GNOME's `windowPreview.js`.
///
/// Only the badges animate. The thumbnails are the live buffers under
/// `scale(k)`, so any per-frame damage over them (a scrim fade, slot
/// motion) goes through the scaled blit at ~17 ms a frame; that waits
/// on a downscale cache. See `docs/wm.md` §Overview mode.
pub const BADGE_FADE_NS: u64 = 200_000_000;

/// The badges' opacity `elapsed_ns` into the fade: ease-out-quad from
/// exactly `0.0` to exactly `1.0` at [`BADGE_FADE_NS`] and beyond.
#[must_use]
pub fn badge_opacity(elapsed_ns: u64) -> f32 {
    if elapsed_ns >= BADGE_FADE_NS {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss)] // both are below 2^28
    let t = (elapsed_ns as f64 / BADGE_FADE_NS as f64).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation)] // in 0..=1
    {
        (t * (2.0 - t)) as f32
    }
}

/// Set every thumbnail's badge group to `opacity`. A framed window's
/// badge hangs off its frame root, an undecorated one's lives in the
/// scrim; both are server nodes. A badge the scene refuses (its window
/// went) is skipped.
pub fn set_badge_opacity(scene: &mut Scene, thumbs: &[ThumbState], opacity: f32) {
    for badge in thumbs.iter().filter_map(|t| t.badge) {
        let _ = scene.set_opacity(ClientId::SERVER, badge, opacity);
    }
}

/// Show or hide the whole window grid: each thumbnail's window root and
/// each badge, one `SetVisible` apiece. Nothing is rebuilt, moved or
/// faded — see `docs/wm.md` §Overview mode for why the search/grid
/// cross-fade is deferred (an opacity ramp over the scaled grid is the
/// measured ~17 ms/frame case until a downscale cache exists).
///
/// A framed window's badge hangs off its root and goes with it; an
/// undecorated one's lives in the scrim and is toggled separately.
/// Showing skips a window that is minimized and whose root the overview
/// did not un-hide (it was minimized after entry), so it does not
/// reappear. Nodes the scene refuses (the window went) are skipped.
pub fn set_grid_visible(scene: &mut Scene, ov: &Overview, visible: bool) {
    let s = ClientId::SERVER;
    for t in &ov.thumbs {
        let Ok(info) = scene.window_info(t.window) else {
            continue;
        };
        let (root, framed, minimized) = (
            info.root(),
            info.is_framed(),
            info.state() == WindowState::Minimized,
        );
        if !visible || !minimized || t.unhid {
            let _ = scene.set_visible(s, root, visible);
        }
        if !framed && let Some(badge) = t.badge {
            let _ = scene.set_visible(s, badge, visible);
        }
    }
}

/// Whether a window takes part in overview mode: a `Normal`-layer
/// toplevel, minimized or on screen.
#[must_use]
pub fn wants_thumb(scene: &Scene, win: WindowKey) -> bool {
    let Ok(info) = scene.window_info(win) else {
        return false;
    };
    if info.layer() != Layer::Normal || info.is_popup() {
        return false;
    }
    info.state() == WindowState::Minimized
        || scene
            .node(info.content())
            .is_ok_and(nitro_scene::Node::visible)
}

#[cfg(test)]
mod scene_tests {
    //! Overview mode on a bare [`Scene`]: the transforms, the scrim's
    //! z-position and the settled-damage guard: the scene state a layout
    //! turns into, rather than the layout arithmetic itself.

    // Every number here is whole-pixel arithmetic on whole-pixel inputs.
    #![allow(clippy::float_cmp)]

    use super::*;
    use nitro_core::{Damage, IRect};
    use nitro_scene::DamageSink;

    const OUT: OutputId = OutputId(1);
    const CLIENT: ClientId = ClientId(7);

    fn update(scene: &mut Scene) -> Damage {
        let mut damage = Damage::new();
        scene.update(&mut DamageSink::new(&mut [(OUT, &mut damage)]));
        damage
    }

    /// A client window with one solid rect, framed when `framed`.
    fn window(scene: &mut Scene, pos: Point, size: Size, framed: bool) -> WindowKey {
        let win = scene.create_window(CLIENT, "w", size, Layer::Normal);
        let content = scene.window_info(win).unwrap().content();
        let rect = scene
            .create_node(CLIENT, NodeKind::Rect, content, None)
            .unwrap();
        scene
            .set_bounds(CLIENT, rect, Rect::new(0.0, 0.0, size.w, size.h))
            .unwrap();
        scene
            .set_fill(CLIENT, rect, Fill::Solid(Color::rgb(0xff, 0, 0)))
            .unwrap();
        if framed {
            scene.frame_window(win, crate::wm::frame_insets()).unwrap();
        }
        scene.place_window(win, Some(OUT), pos).unwrap();
        win
    }

    /// Two framed windows, one undecorated one and a minimized framed one
    /// over a wallpaper, on a 1000x800 output.
    fn desktop() -> (Scene, Vec<WindowKey>, WindowKey) {
        let mut scene = Scene::new();
        scene.add_output(OUT, IRect::new(0, 0, 1000, 800), 1.0);
        let wallpaper =
            scene.create_window(CLIENT, "bg", Size::new(1000.0, 800.0), Layer::Background);
        scene
            .place_window(wallpaper, Some(OUT), Point::ZERO)
            .unwrap();
        let a = window(
            &mut scene,
            Point::new(100.0, 100.0),
            Size::new(400.0, 300.0),
            true,
        );
        let b = window(
            &mut scene,
            Point::new(500.0, 300.0),
            Size::new(300.0, 200.0),
            true,
        );
        let c = window(
            &mut scene,
            Point::new(50.0, 500.0),
            Size::new(200.0, 150.0),
            false,
        );
        let d = window(
            &mut scene,
            Point::new(600.0, 50.0),
            Size::new(200.0, 100.0),
            true,
        );
        scene.set_window_state(d, WindowState::Minimized).unwrap();
        update(&mut scene);
        (scene, vec![a, b, c, d], wallpaper)
    }

    fn slots_for(scene: &Scene, wins: &[WindowKey]) -> Vec<Slot> {
        let thumbs: Vec<Thumb> = wins
            .iter()
            .map(|w| thumb_of(*w, scene.window_info(*w).unwrap()))
            .collect();
        layout(&thumbs, Rect::new(0.0, 0.0, 1000.0, 800.0), 800.0)
    }

    /// Everything `Server::enter_overview` does to the scene, minus the
    /// server: scale, hide the decorations, badge, scrim.
    fn enter(scene: &mut Scene, wins: &[WindowKey]) -> (WindowKey, Vec<Slot>) {
        let (scrim, slots, _) = enter_badged(scene, wins);
        (scrim, slots)
    }

    /// [`enter`], also returning each thumbnail's badge group.
    fn enter_badged(scene: &mut Scene, wins: &[WindowKey]) -> (WindowKey, Vec<Slot>, Vec<NodeKey>) {
        let mut badges = Vec::new();
        let slots = slots_for(scene, wins);
        let scrim = create_scrim(scene, OUT, Size::new(1000.0, 800.0)).unwrap();
        let scrim_root = scene.window_info(scrim).unwrap().root();
        for slot in &slots {
            apply_thumb(scene, slot.window, slot).unwrap();
            let info = scene.window_info(slot.window).unwrap();
            let (root, framed, size) = (info.root(), info.is_framed(), info.frame_size());
            // The decorations stay: a thumbnail is the whole frame.
            scene.set_visible(ClientId::SERVER, root, true).unwrap();
            let badge = Badge { icon: Some((0, 0)) };
            if framed {
                let origin = Point::new(size.w / 2.0, size.h);
                badges.push(build_badge(scene, root, origin, 1.0 / slot.scale, &badge).unwrap());
            } else {
                let origin = Point::new(slot.pos.x + slot.size.w / 2.0, slot.pos.y + slot.size.h);
                badges.push(build_badge(scene, scrim_root, origin, 1.0, &badge).unwrap());
            }
        }
        (scrim, slots, badges)
    }

    #[test]
    fn a_thumbnail_s_frame_lands_exactly_on_its_slot() {
        let (mut scene, wins, _) = desktop();
        let (_, slots) = enter(&mut scene, &wins);
        update(&mut scene);
        assert_eq!(slots.len(), 4, "the minimized window is a thumbnail too");
        for slot in &slots {
            let info = scene.window_info(slot.window).unwrap();
            let (inset, fsize) = (info.inset(), info.frame_size());
            // The slot is the whole frame, at the frame's aspect.
            assert!((slot.size.w - fsize.w * slot.scale).abs() < 1e-3, "{slot:?}");
            assert!((slot.size.h - fsize.h * slot.scale).abs() < 1e-3, "{slot:?}");
            // The client's own rect at (0, 0) in its content lands at the
            // slot's top-left plus the scaled inset, at the slot's scale
            // (the output's scale is 1). The rect rather than the content
            // group, because a group's own transform applies to its
            // children only and an undecorated window's content group *is*
            // its root.
            let rect = scene.node(info.content()).unwrap().children()[0];
            let t = scene.node(rect).unwrap().world_transform();
            let k = slot.scale;
            assert!((t.e - (slot.pos.x + k * inset.left)).abs() < 1e-3, "{slot:?}");
            assert!((t.f - (slot.pos.y + k * inset.top)).abs() < 1e-3, "{slot:?}");
            assert!((t.a - k).abs() < 1e-6, "{} vs {}", t.a, k);
        }
    }

    #[test]
    fn a_snapped_title_lands_on_a_whole_device_size() {
        for k in [0.1, 0.23, 0.37, 0.5, 0.618, 0.95] {
            let px = snapped_text_size(13.0, k);
            let device = px * k;
            assert!((device - device.round()).abs() < 1e-4, "{k}: {device}");
            assert!((device - 13.0 * k).abs() <= 0.5 + 1e-4, "{k}: {device}");
        }
        assert_eq!(snapped_text_size(13.0, 0.01) * 0.01, 1.0, "never below 1 px");
        assert_eq!(snapped_text_size(13.0, 1.0), 13.0);
    }

    #[test]
    fn the_grid_area_leaves_the_search_band_free() {
        let band = nitro_wire::types::overview::search_band() as f32;
        let work = Rect::new(0.0, 32.0, 1000.0, 768.0);
        let area = grid_area(work);
        assert_eq!(area, Rect::new(0.0, 32.0 + band, 1000.0, 768.0 - band));
        // A work area shorter than the band collapses, never inverts.
        assert_eq!(grid_area(Rect::new(0.0, 0.0, 10.0, 20.0)).h, 0.0);
    }

    #[test]
    fn a_settled_overview_produces_no_damage() {
        let (mut scene, wins, _) = desktop();
        enter(&mut scene, &wins);
        let first = update(&mut scene);
        assert!(!first.is_empty(), "entering repaints the output");
        // The guard `docs/wm.md` §Overview mode is about: a scaled blit is
        // ~57x the 1:1 one, so anything that re-dirtied the grid every
        // update would cost ~17 ms a frame.
        let settled = update(&mut scene);
        assert!(
            settled.is_empty(),
            "settled overview damaged {:?}",
            settled.rects()
        );
    }

    #[test]
    fn hiding_the_grid_hides_every_thumbnail_and_badge_then_goes_quiet() {
        let (mut scene, wins, _) = desktop();
        let (scrim, slots, badges) = enter_badged(&mut scene, &wins);
        let minimized = wins[3];
        let ov = Overview {
            output: OUT,
            scrim,
            thumbs: slots
                .iter()
                .zip(&badges)
                .map(|(s, b)| ThumbState {
                    window: s.window,
                    slot: *s,
                    hit: s.rect(),
                    saved_transform: Transform::IDENTITY,
                    saved_position: None,
                    unhid: s.window == minimized,
                    badge: Some(*b),
                })
                .collect(),
            fade_start_ns: None,
            grid_hidden: true,
        };
        update(&mut scene);
        assert!(update(&mut scene).is_empty());

        set_grid_visible(&mut scene, &ov, false);
        assert!(!update(&mut scene).is_empty(), "hiding repaints");
        for (s, b) in slots.iter().zip(&badges) {
            let root = scene.window_info(s.window).unwrap().root();
            assert!(!scene.node(root).unwrap().visible(), "{s:?}");
            // A framed badge hangs off the (hidden) root; an undecorated
            // one is in the scrim and must be hidden itself.
            let framed = scene.window_info(s.window).unwrap().is_framed();
            assert!(framed || !scene.node(*b).unwrap().visible());
        }
        let settled = update(&mut scene);
        assert!(
            settled.is_empty(),
            "hidden grid damaged {:?}",
            settled.rects()
        );

        set_grid_visible(&mut scene, &ov, true);
        assert!(!update(&mut scene).is_empty(), "showing repaints");
        for (s, b) in slots.iter().zip(&badges) {
            let root = scene.window_info(s.window).unwrap().root();
            assert!(scene.node(root).unwrap().visible(), "{s:?}");
            assert!(scene.node(*b).unwrap().visible());
        }
        assert!(update(&mut scene).is_empty());
    }

    #[test]
    fn a_badge_fade_damages_only_the_badges_and_then_goes_quiet() {
        let (mut scene, wins, _) = desktop();
        let (_, slots, badges) = enter_badged(&mut scene, &wins);
        assert!(!badges.is_empty());
        let thumbs: Vec<ThumbState> = slots
            .iter()
            .zip(&badges)
            .map(|(s, b)| ThumbState {
                window: s.window,
                slot: *s,
                hit: s.rect(),
                saved_transform: Transform::IDENTITY,
                saved_position: None,
                unhid: false,
                badge: Some(*b),
            })
            .collect();
        set_badge_opacity(&mut scene, &thumbs, 0.0);
        assert!(!update(&mut scene).is_empty(), "entering repaints");
        assert!(update(&mut scene).is_empty());

        // The icon's box in output coordinates: what one badge may damage,
        // plus a pixel of anti-aliasing slack.
        let icon = badge_rect();
        let boxes: Vec<Rect> = slots
            .iter()
            .map(|s| {
                Rect::new(
                    s.pos.x + s.size.w / 2.0 + icon.x - 1.0,
                    s.pos.y + s.size.h + icon.y - 1.0,
                    icon.w + 2.0,
                    icon.h + 2.0,
                )
            })
            .collect();
        let output_area = 1000 * 800;
        for step in 1..=4u64 {
            let o = badge_opacity(step * BADGE_FADE_NS / 5);
            set_badge_opacity(&mut scene, &thumbs, o);
            let damage = update(&mut scene);
            assert!(!damage.is_empty(), "step {step} repaints the badges");
            let mut area = 0;
            for r in damage.rects() {
                area += r.w * r.h;
                let inside = boxes.iter().any(|b| {
                    r.x as f32 >= b.x.floor()
                        && r.y as f32 >= b.y.floor()
                        && (r.x + r.w) as f32 <= b.right().ceil()
                        && (r.y + r.h) as f32 <= b.bottom().ceil()
                });
                assert!(inside, "step {step}: {r:?} is outside every badge");
            }
            assert!(
                area * 20 < output_area,
                "step {step}: {area} px damaged — the grid, not the badges"
            );
        }
        set_badge_opacity(&mut scene, &thumbs, badge_opacity(BADGE_FADE_NS));
        assert!(!update(&mut scene).is_empty(), "the last step lands");
        let settled = update(&mut scene);
        assert!(
            settled.is_empty(),
            "faded in, damaged {:?}",
            settled.rects()
        );
        // Re-asserting the settled opacity is free too.
        set_badge_opacity(&mut scene, &thumbs, 1.0);
        assert!(update(&mut scene).is_empty());
    }

    #[test]
    fn the_scrim_stays_under_every_thumbnail_whatever_is_raised() {
        let (mut scene, wins, wallpaper) = desktop();
        let (scrim, _) = enter(&mut scene, &wins);
        let order = |scene: &Scene| scene.windows(OUT).collect::<Vec<_>>();
        assert_eq!(&order(&scene)[..2], &[wallpaper, scrim]);
        for w in &wins {
            scene.raise(*w).unwrap();
            assert_eq!(
                &order(&scene)[..2],
                &[wallpaper, scrim],
                "after raising {w:?}"
            );
        }
        // A window mapped during overview goes to the front, not under it.
        let late = window(
            &mut scene,
            Point::new(10.0, 10.0),
            Size::new(50.0, 50.0),
            true,
        );
        let now = order(&scene);
        assert_eq!(&now[..2], &[wallpaper, scrim]);
        assert_eq!(now.last(), Some(&late));
    }

    #[test]
    fn a_badge_is_drawn_at_the_output_s_own_scale() {
        let (mut scene, wins, _) = desktop();
        enter(&mut scene, &wins);
        update(&mut scene);
        let mut checked = 0;
        for w in &wins[..2] {
            let root = scene.window_info(*w).unwrap().root();
            let badge = *scene.node(root).unwrap().children().last().unwrap();
            for child in scene.node(badge).unwrap().children() {
                let t = scene.node(*child).unwrap().world_transform();
                assert!((t.a - 1.0).abs() < 1e-5, "icon/caption scale {}", t.a);
                assert!((t.d - 1.0).abs() < 1e-5);
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn only_the_server_may_scale_a_frame() {
        let (mut scene, wins, _) = desktop();
        let root = scene.window_info(wins[0]).unwrap().root();
        assert_eq!(
            scene.set_transform(CLIENT, root, Transform::scale(0.25, 0.25)),
            Err(SceneError::NotOwner)
        );
        assert!(
            scene
                .set_transform(ClientId::SERVER, root, Transform::scale(0.25, 0.25))
                .is_ok()
        );
    }

    #[test]
    fn restoring_puts_every_window_back() {
        let (mut scene, wins, _) = desktop();
        let before: Vec<(Point, Transform)> = wins
            .iter()
            .map(|w| {
                let i = scene.window_info(*w).unwrap();
                (i.position(), scene.node(i.root()).unwrap().transform())
            })
            .collect();
        let slots = slots_for(&scene, &wins);
        let saved: Vec<_> = slots
            .iter()
            .map(|s| apply_thumb(&mut scene, s.window, s).unwrap())
            .collect();
        for (slot, (t, p)) in slots.iter().zip(saved) {
            restore_thumb(&mut scene, slot.window, t, p).unwrap();
        }
        for (w, want) in wins.iter().zip(before) {
            let i = scene.window_info(*w).unwrap();
            assert_eq!(
                (i.position(), scene.node(i.root()).unwrap().transform()),
                want
            );
        }
    }

    #[test]
    fn a_click_selects_by_slot_geometry() {
        let (scene, wins, _) = desktop();
        let slots = slots_for(&scene, &wins);
        let ov = Overview {
            output: OUT,
            scrim: wins[0],
            thumbs: slots
                .iter()
                .map(|s| ThumbState {
                    window: s.window,
                    slot: *s,
                    hit: s.rect(),
                    saved_transform: Transform::IDENTITY,
                    saved_position: None,
                    unhid: false,
                    badge: None,
                })
                .collect(),
            fade_start_ns: None,
            grid_hidden: false,
        };
        for s in &slots {
            let centre = Point::new(s.pos.x + s.size.w / 2.0, s.pos.y + s.size.h / 2.0);
            assert_eq!(ov.slot_at(centre), Some(s.window));
        }
        assert_eq!(ov.slot_at(Point::new(-5.0, -5.0)), None);
        // Search results replaced the grid: nothing is selectable.
        let hidden = Overview {
            grid_hidden: true,
            ..ov
        };
        for s in &slots {
            let centre = Point::new(s.pos.x + s.size.w / 2.0, s.pos.y + s.size.h / 2.0);
            assert_eq!(hidden.slot_at(centre), None);
        }
    }

    #[test]
    fn the_badge_hangs_thirty_percent_below_the_thumbnail() {
        let icon = badge_rect();
        assert_eq!(icon.w, OVERVIEW_ICON);
        assert_eq!(
            icon.y + icon.h,
            (OVERVIEW_ICON * (1.0 - ICON_OVERLAP)).round()
        );
        assert_eq!(badge_below(), icon.y + icon.h);
        assert!(badge_below() <= ROW_SPACING, "fits in the row gap");
    }
}
