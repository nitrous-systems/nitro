# GNOME's Activities overview — window grid, search, and what nitro can do about it

The issue (#616) asks for "a view with all windows arranged so they're all
visible and with their icons … a search bar, typing engages search". That is
GNOME's Activities overview, so this document is a study of it: the layout
algorithm read from `gnome-shell` source, the visual arrangement measured off
published screenshots, and — the column that matters — which parts nitro can
build, which it must rebuild differently, and which assume a GPU compositor it
does not have.

It also carries the **architecture decision** for nitro's own overview, because
that decision rests on measurements taken here. The short form is in
`docs/wm.md` §Overview mode; the reasoning and the numbers are §7 below.

## 1. Sources

Unlike the two sibling analyses in this directory, which are screenshot studies
of a *visual* design, half of this document is about an *algorithm*. For that,
source is strictly better than a screenshot, so the layout section is read off
`gnome-shell` `main`:

| File | What was taken from it |
|---|---|
| `js/ui/workspace.js` | `UnalignedLayoutStrategy`, the `numRows` search, `_isBetterScaleAndSpace`, `_keepSameRow`, `_computeWindowScale`, `computeWindowSlots`, and the constants `WINDOW_PREVIEW_MAXIMUM_SCALE = 0.95`, `LAYOUT_SCALE_WEIGHT = 1`, `LAYOUT_SPACE_WEIGHT = 0.1` |
| `js/ui/windowPreview.js` | `ICON_SIZE = 64`, `ICON_OVERLAP = 0.7`, `ICON_TITLE_SPACING = 6`, `WINDOW_OVERLAY_FADE_TIME = 200`, the icon/title/close-button constraints |
| `js/ui/overviewControls.js` | `ControlsState { HIDDEN: 0, WINDOW_PICKER: 1, APP_GRID: 2 }`, `SIDE_CONTROLS_ANIMATION_TIME = 250`, `_onSearchChanged` |
| `js/ui/overview.js` | `ANIMATION_TIME = 250` |
| `js/ui/searchController.js` | the `Escape` ladder |
| `js/ui/search.js` | `MAX_LIST_SEARCH_RESULTS_ROWS = 5` |
| `data/theme/gnome-shell-sass/widgets/_window-picker.scss` | `.window-picker`, `.window-icon`, `.window-caption`, `.window-close`, `.workspace-background` |

Screenshots, for the visual questions source cannot answer. Following this
directory's precedent (`README.md`), the PNGs live in the gitignored
`tmp/research/` and the numbers read off them are reproduced below:

| File | Shows |
|---|---|
| `dash-after.webp` → `.png` (1024×768, release.gnome.org/47) | GNOME 47 overview, dark. One window, search field, dash, caption pill, app icon straddling the thumbnail's bottom edge. The **icon-placement** reference. |
| `gs320.png` (1920×1080, Wikimedia, GNOME 3.20) | **Four** windows of wildly different aspect ratios in **two rows**. The layout reference — the only published shot found with enough windows to measure row assignment and alignment against. |
| `gs40-overview.png` (1920×1080, Wikimedia, GNOME Shell 40) | One window plus the workspace strip; confirms the single-row centring case. |
| `rocky-overview.png` (3840×2160, Wikimedia, Rocky Linux 8.4) | The **zero-window** empty state. |
| `search45.png`, `super-s.webp` | release.gnome.org/45; neither turned out to show overview search — see §6's honesty note. |

**Not visually verified**, and said so rather than silently omitted:

* **A dense grid.** No published overview screenshot with 9+ windows was found.
  The many-window behaviour in §5 is derived from the algorithm, not observed.
* **Search replacing the grid in a modern shell.** The only search-active
  overview shot found is GNOME Shell 3.0, whose overview is a different design
  (a `Windows`/`Applications` tab pair). The modern behaviour in §4 is read off
  `_onSearchChanged`, not off an image.
* **The animation.** A still cannot show one. §4's timings are source constants.

## 2. The window-grid layout algorithm

GNOME's is `UnalignedLayoutStrategy`: windows keep their aspect ratios, are
packed into rows of unequal length, and one global scale is fitted to the area.
It is *not* a uniform grid — that is the whole point, and it is why a terminal
and a browser both stay readable.

### 2.1 The outer search

```
best_layout(windows, area):
    last = none
    for num_rows in 1, 2, 3, …:
        num_cols = ceil(len(windows) / num_rows)
        if num_cols == last_cols:
            break                  # another row bought no column: stop
        layout       = compute_layout(windows, num_rows)
        scale, space = compute_scale_and_space(layout, area)
        if last and not better(last_scale, last_space, scale, space):
            break
        last = layout; last_cols = num_cols
        last_scale = scale; last_space = space
    return last
```

Two stopping rules, and both matter. The column check is the cheap one — with
9 windows, 3 rows gives 3 columns and so does 4, so 4 rows is pointless. The
objective is the interesting one:

```
better(old_scale, old_space, scale, space):
    d_space = (space - old_space) * LAYOUT_SPACE_WEIGHT   # 0.1
    d_scale = (scale - old_scale) * LAYOUT_SCALE_WEIGHT   # 1
    scale ↑ and space ↑  ->  true            # win-win
    scale ↑, space ↓     ->  d_scale > d_space
    scale ↓, space ↑     ->  d_space > d_scale
    scale ↓ and space ↓  ->  false
```

`scale` is how large the thumbnails are; `space` is the fraction of the area the
grid covers. The 1 : 0.1 weighting says **bigger thumbnails beat a tidier fill,
ten to one** — which is the right prior for a window picker, where the job is
recognising a window, not admiring the packing.

### 2.2 Building one candidate

```
window_scale(w) = lerp(1.5, 1.0, w.height / monitor.height)
```

A per-window nudge applied *before* the layout: a window a tenth of the screen
tall is bumped by ~1.45×, a full-height one by 1.0. The source's comment says
it plainly — "for something like a calculator window, we need to bump up the
size just a bit". Height, not width, because windows sit side by side in a row
and equal-height neighbours line up.

```
compute_layout(windows, num_rows):
    total_width    = Σ  w.width * window_scale(w)
    ideal_row_width = total_width / num_rows
    sorted = windows sorted by centre.y          # vertical sort -> which row
    for i in 0 .. num_rows-1:
        greedily append windows while keep_same_row(...) or i is the last row
        row.full_height = max over its windows of height * window_scale
    for each row: sort its windows by centre.x   # horizontal sort -> order in row
    grid_width  = max row.full_width
    grid_height = Σ row.full_height

keep_same_row(row, w, width, ideal):
    row.full_width + width <= ideal                       -> true
    |1 - (row.full_width+width)/ideal| < |1 - row.full_width/ideal|  -> true
    otherwise                                              -> false
```

The two sorts are the part that makes the overview feel like *your desktop
rearranged* rather than a fresh list: a window near the top of the screen lands
in a top row, and a window on the left stays on the left. The source calls this
"minimize travel distance", and it is cheap to copy exactly.

`keep_same_row`'s second clause is a nicety worth keeping: a row already at 90 %
of ideal will still take a window that overshoots to 105 %, because 105 is
closer to ideal than 90 is.

### 2.3 Fitting to the area

```
compute_scale_and_space(layout, area):
    hspacing = (max_columns - 1) * column_spacing
    vspacing = (num_rows    - 1) * row_spacing
    scale = min((area.w - hspacing) / grid_width,
                (area.h - vspacing) / grid_height,
                WINDOW_PREVIEW_MAXIMUM_SCALE)          # 0.95
    space = (grid_width*scale + hspacing) * (grid_height*scale + vspacing)
            / (area.w * area.h)
```

`WINDOW_PREVIEW_MAXIMUM_SCALE = 0.95` is the "a thumbnail is never nearly
full size" cap. It is applied **per window at the end**, not by shrinking the
global scale, because shrinking the global scale to satisfy the one biggest
window would needlessly shrink every window already under the cap. The source's
trick, in its own words: "we simply cheat: we keep each window's *cell* area the
same, but we shrink the thumbnail and centre it horizontally, and align it to
the bottom vertically."

### 2.4 Placing the windows

```
compute_window_slots(layout, area):
    row.width  = row.full_width * scale + (n-1)*column_spacing
    row.height = row.full_height * scale
    height_no_spacing = Σ row.height
    extra_v = min(1, (area.h - vspacing) / height_no_spacing)

    for each row:
        width_no_spacing = row.width - hspacing_in_row
        extra_h = min(1, (area.w - hspacing_in_row) / width_no_spacing)
        row.extra = min(extra_h, extra_v)
        if extra_h < extra_v: compensation += (extra_v - extra_h) * row.height
        row.x = area.x + max(area.w - (width_no_spacing*row.extra
                                       + hspacing_in_row), 0) / 2
        row.y = area.y + max(area.h - (height_no_spacing + vspacing), 0) / 2 + y
        y    += row.height * row.extra + row_spacing
    compensation /= 2

    for each row, each window left to right:
        s     = scale * window_scale(w) * row.extra
        cell  = (w.width * s, w.height * s)
        s     = min(s, 0.95)
        clone = (w.width * s, w.height * s)
        x = cursor + (cell.w - clone.w) / 2
        y = (one row) ? row_y + (row_height - clone.h) / 2      # centre
                      : row_y + row_height - cell.h             # bottom-align
        slot = (floor(x), floor(y), clone.w, clone.h)
        cursor += cell.w + column_spacing
```

Three details worth copying verbatim:

* **Rows are centred horizontally, and the grid is centred vertically.** Ragged
  row lengths are what makes an unaligned layout read as deliberate rather than
  broken.
* **Multi-row bottom-aligns; a single row centres.** Measured and confirmed
  (§3).
* **`floor` to the pixel grid.** The source's comment is "align with the pixel
  grid to prevent blurry windows at scale = 1". nitro has exactly the same
  concern, stated in `docs/wm.md` §Work area and placement — "a window at a half
  pixel puts every edge and glyph in it between device pixels".

## 3. What the arrangement actually looks like — measured

From `gs320.png`, 1920×1080, four windows, two rows:

| window | slot (x, y, w, h) | bottom edge |
|---|---|---|
| Firefox | 279, 150, 771 × 374 | **523** |
| Files | 1060, 252, 390 × 272 | **523** |
| Software | 304, 596, 766 × 364 | 991 |
| Weather | 1069, 733, 357 × 227 | ~980 |

**The bottom-alignment rule is confirmed exactly**: the top row's two windows
differ by 102 px in height and by 381 px in width, and their bottom edges are
the same pixel. The aspect ratios are preserved (Firefox 2.06, Files 1.43,
Weather 1.57) — this is not a uniform grid with letterboxing.

From `dash-after.png`, 1024×768, GNOME 47, one window:

| thing | measured |
|---|---|
| scrim / overview background | `#282828`, filling the whole screen including behind the top bar |
| search field | x 328–695 (**368 wide**), y 44–80 (**37 tall**), fill `#505050`, pill-rounded, centred |
| workspace background card | x 149–1023, y 108–628, rounded (`border-radius: 30px` in SCSS), with a drop shadow |
| app icon | ~64 px square, horizontally centred on the thumbnail, straddling its **bottom edge** |
| caption pill | x 363–658 (296 wide), y 597–628 (**32 tall**), near-black `#03060e` translucent, rounded |

The icon placement is the one thing the issue calls out ("with their icons") and
it is worth being precise: the icon is **not** in a corner and **not** beside
the title. It is centred on the thumbnail's bottom edge with `ICON_OVERLAP =
0.7` — 70 % of the icon inside the thumbnail, 30 % hanging below it — and the
caption sits `ICON_TITLE_SPACING = 6` px below the icon's overhang. The icon
straddling the boundary is what makes it read as *belonging to* that thumbnail
without eating its content.

## 4. Search, triggers and animation

**Search replaces the grid; it does not sit beside it.** `_onSearchChanged`
cross-fades three things over `SIDE_CONTROLS_ANIMATION_TIME = 250 ms`,
`EASE_OUT_QUAD`:

| actor | opacity |
|---|---|
| `_workspacesDisplay` (the window grid) | → 0 when search is active |
| `_appDisplay` (the app grid) | → 0 when search is active |
| `_searchController` (the results) | → 255 when search is active |

and — the detail that matters for nitro — on completion it sets
`_workspacesDisplay.reactive = false`. The faded-out grid is made
**non-interactive**, not merely invisible. A half-faded thumbnail must not be
clickable.

**`Escape` is a ladder, not a toggle** (`searchController.js`): if search is
active, clear the search; else if the app grid is showing, go back to the window
picker; else hide the overview. Three presses to get from "typing in search" to
"desktop", and each one undoes exactly one thing.

**Entering and leaving** is `Overview.ANIMATION_TIME = 250 ms`, and the overlays
(icon, caption, close button) fade separately over
`WINDOW_OVERLAY_FADE_TIME = 200 ms` so the chrome arrives slightly after the
thumbnails have settled.

**Trigger.** The `Activities` corner/button and the `toggle-overview` keybinding
(Super) are the same path. GNOME has no separate "launcher" — the overview *is*
the launcher, which is the model the issue's "for now only searches
applications" implies.

## 5. The edge cases the issue asks about

**Zero windows** (`rocky-overview.png`, confirmed visually): the grid area is
simply **empty** — wallpaper, and nothing else. There is no "No Windows"
placeholder, no empty-state illustration, no text. The search field, dash and
workspace strip are all still there, so the view is not empty, only the grid is.
This is the right answer and it is free: an overview with nothing in it is still
a search box, which is what the user probably wanted anyway.

**One window**: single-row centring (`rows.length === 1` → vertically centre),
capped at 0.95 scale. `gs40-overview.png` shows exactly this — one large
thumbnail, centred, noticeably smaller than the screen.

**Many windows**: the algorithm degrades by adding rows until another row stops
buying a column, then stops on the scale/space objective. Because the objective
weights scale 10 : 1 over coverage, it prefers fewer, larger rows to a tidy
dense fill. **Not visually verified** — no published dense screenshot was found.
What the algorithm guarantees is that every window keeps its aspect ratio and
every window is inside the area; what it does not guarantee is legibility at 25
windows, where a 1280×800 window is ~298×186.

## 6. What nitro copies, and what it cannot

| # | GNOME | in nitro |
|---|---|---|
| 1 | Unaligned row packing, aspect ratios preserved, one global scale fitted to the area | **yes, ported exactly.** It is pure arithmetic over rectangles — no toolkit, no GPU. The constants (0.95, 1 : 0.1, the 1.5→1.0 lerp) are carried over because they encode taste that has been tuned against real desktops for fifteen years |
| 2 | Vertical sort for row assignment, horizontal sort within a row | **yes.** Free, and it is what makes the overview read as a rearrangement of your desktop |
| 3 | `floor` to the pixel grid | **yes**, and nitro needs it more than GNOME does: `docs/wm.md` already rounds window placement to whole logical pixels for the same reason, and nitro's rasterizer has a hard 1:1 fast path (§7) that a half-pixel offset misses |
| 4 | Multi-row bottom-align, single-row centre | **yes** |
| 5 | Thumbnails are **live** windows, not captures | **yes** — and for the same structural reason: mutter and the shell are one process, and so are nitro's compositor and WM. See §7 |
| 6 | 64 px app icon straddling the thumbnail's bottom edge, 70 % in | **yes.** `nitro-icons` exists and the WM frame already draws a per-window app icon (`FrameNodes::app_icon`) |
| 7 | Caption pill below the icon, ellipsized | **yes**, but drawn **unscaled** — see §7's text hazard |
| 8 | Search replaces the grid, cross-faded, grid made non-reactive | **yes** for the replace and the non-reactive part; the cross-fade is an opacity animation, which the scene supports |
| 9 | `Escape` ladder | **yes**, shortened: nitro has no app grid, so it is two rungs (clear search → leave overview) |
| 10 | Entering/leaving animation | **partial.** Position interpolation and a scrim fade, yes. A **scale** animation is refused on measured grounds (§7) — GNOME's costs nothing because Clutter scales a texture on the GPU; nitro's would cost 17 ms a frame |
| 11 | Blurred background behind the search field, shadowed thumbnail chrome, `0 4px 16px 4px` on the workspace card, `0 2px 4px` on the close button | **no.** `Fill` is `None | Solid | Linear`; the scene has no blur and no drop shadow. The same three "no"s the sibling analyses hit, for the same reason, recorded in `README.md` |
| 12 | Rounded 30 px workspace-background card | **no**, and not wanted: it is a *workspace* affordance, and workspaces are deferred (`docs/wm.md` §What is deferred) |
| 13 | Workspace thumbnail strip, dash, app grid | **no.** Out of scope for #616, and the first two presuppose workspaces |
| 14 | Spring/elastic easing on the window actors | **no.** Clutter easing modes are a toolkit feature; nitro would need an animation driver, and `EASE_OUT_QUAD` on a position is the affordable subset |

Items 11–14 are all one fact restated: GNOME's overview is a **GPU compositor's**
overview. Every effect nitro drops is one that costs a shader or a texture
transform GNOME gets for free and nitro would pay for in CPU pixels.

### An honesty note on the screenshots

The issue asked for screenshots, and they were pulled and measured — §3's
numbers are all read off real images. But two of the five questions this
document set out to answer visually (a dense grid, and modern search-active)
had no published image to answer them, and are marked as source-derived in §1
rather than dressed up. An unmarked gap would be worse than an admitted one.

## 7. The decision: server-side overview

The spec that opened this work assumed the blocking problem was a *missing
primitive* — "there is no way to display a window's contents at reduced size" —
and framed the choice as server-side compositing versus a new client-facing
screencopy protocol. **The primitive is not missing.** That was tested, twice,
and the measurements reverse the prior.

### 7.1 The primitive already exists, and is already correctly privileged

`Scene::set_transform` takes an arbitrary affine, and `frame_window` makes a
framed window's root a `ClientId::SERVER`-owned group. So the server can already
scale a live window in place. Measured on this tree (throwaway integration test,
400×300 window at (100,100), 1 / 28 / 1 / 1 insets, `scale(0.25)`):

| assertion | result |
|---|---|
| `set_transform(SERVER, frame, scale(0.25))` scales the **client's own content node** | ✅ device rect `101,128 400×300` → `100,107 101×75` |
| a client attempting the same on its frame root | ✅ `Err(NotOwner)` — the privilege boundary is already right |
| damage of the scale change | ✅ one rect, `100,107 401×321` = old ∪ new |
| **a settled overview produces zero damage** | ✅ `damage.is_empty()` |
| `hit_test` through the scaled transform | ✅ resolves to the **live client**, `local = (39, 12)` |

No new wire message, no new capability, no new privilege. The paint, damage and
hit-test paths already do the right thing — including the last row, which is a
*hazard* rather than a feature (§7.4).

### 7.2 The cost, which is the real constraint

`nitro-raster`'s `blit` has a 1:1 fast path, gated on `one_to_one` in
`blit_impl`: source and destination the same size, destination integer-aligned.
Miss it and every pixel takes a bilinear fetch of two texel pairs plus a blend.
Measured, release build, 1920×1080 XRGB destination, 1280×800 source:

| path | ns/px |
|---|---|
| 1:1 blit, `Xrgb8888` (today's ordinary window path) | **0.21** |
| scaled blit, `Xrgb8888` | **12.0** |
| scaled blit, `Argb8888` | **15.8** |

**A ~57× per-pixel cliff.** These are order-of-magnitude figures measured on one
box: the 1:1 number in particular is memory-bandwidth-bound and moves by ~2×
with machine state, so it is the **ratio** that is load-bearing, not the third
significant figure.

The cost is destination-area dominated, and so almost independent of *how many*
windows are in the grid:

| grid, 67 % screen coverage | per frame |
|---|---|
| 4 thumbs (745×466 each) | 17.0 ms |
| 9 thumbs (497×311) | 17.1 ms |
| 16 thumbs (373×233) | 17.3 ms |
| 25 thumbs (298×186) | 17.5 ms |

Against `docs/latency.md`'s measured **0.19 ms mean / 0.32 ms max** server paint
per frame, a naively-redrawn overview is **50–90× the entire frame budget** and
misses a 16.7 ms vsync outright.

The levers, also measured:

| | cost |
|---|---|
| downscale 1280×800 → 372×232 once into a cache | 1.04 ms, paid **on change**, not per frame |
| 16 cached thumbnails, 1:1 blit per frame | **0.37 ms/frame** |
| one live thumbnail rescaled (damage-driven steady state) | 1.05 ms |

**And that is the conclusion that decides the architecture.** The desktop is
damage-driven: nothing repaints until something changes. A *settled* overview
costs **zero** — measured, §7.1's fourth row. A settled 16-window overview with
one animating client costs ~1 ms. What is unaffordable is animating the
**scale**, which rescales every thumbnail every frame.

### 7.3 Why not a client-side overview

The rejected option was a `Screencopy`/`Thumbnail` wire message plus a
capability bit, letting `nitro-launcher` receive window contents and draw the
grid itself in the toolkit. Its cost, stated:

1. **It pays the identical downscale.** ~12 ns/px is `blit`'s, not the server's;
   a client doing it in `nitro-ui` pays exactly the same — **plus** a buffer copy
   across a socket, **plus** per-frame buffer churn for N thumbnails.
2. **It is the most security-sensitive thing this protocol could grow**: one
   client reading every other client's pixels. And it cannot be scoped today —
   `docs/shell.md` §Deferred records that per-app allow lists are blocked on peer
   identity, with no candidate that works, so the grant would have to be "any
   shell client may read every client's pixels".
3. **Liveness becomes a loop.** Server-side, a thumbnail of a playing video plays
   because it *is* the window. Client-side, liveness is a capture-and-transmit
   loop per thumbnail per frame.
4. **It would have to land inside the M5 wire reshape** (#3767 onward) rather
   than after it, coupling a large new surface to an in-flight chain.

In exchange it buys one thing: the grid being drawable by the toolkit. That is
not worth items 1–4 — and it is not even needed, because under the chosen design
the server draws only scaled windows it already owns plus a scrim and
per-thumbnail icon and label nodes, all of which are `Rect`/`Icon`/`Text` nodes
`wm::build_frame` already constructs today.

### 7.4 Two hazards the scaled transform creates

1. **A scaled window is still live and still interactive.** §7.1's last row is
   the proof: `input::hit` → `window_local` inverts the world transform, so a
   click on a thumbnail is delivered to the client as an ordinary click at
   scaled-down local coordinates. The overview must **explicitly** swallow
   input. This is a correctness requirement, not polish.

   The rule must be **scoped by layer**, not global: pointer events over
   `Layer::Normal` windows are swallowed by the WM and reinterpreted as
   thumbnail selection, while `Overlay` and `Top` windows route normally — which
   is what keeps the search field and its result rows clickable, since the
   search UI is itself a client. `hit_test` resolves topmost-first, so `Overlay`
   naturally wins over the thumbnails beneath it; the rule is a filter on the
   hit's *layer*, not a mute.

2. **Scaling re-rasterizes text.** `TextEngine::paint` computes
   `device_size = run.size_px * scale` and `GlyphKey` quantizes size to 1/64 px,
   so a scaled title bar rasterizes a **fresh glyph set** and pollutes the atlas
   — and an animated scale would do so every frame. Frame decorations are
   therefore **hidden** in overview (`FrameNodes::all()` already exists for
   exactly this, used by fullscreen), and the icon and caption are drawn
   unscaled per thumbnail. Which is also what GNOME does, for its own reasons.

### 7.5 The shape that follows

**Server-side overview as a WM mode, with the search UI left in
`nitro-launcher` as an `Overlay` client.** A hybrid — and the seam the original
spec worried about turns out to be the cheap part, because it is *layers*, which
already work: the thumbnails are `Normal`-layer windows scaled in place, and
`nitro-launcher` is already on `Overlay`, i.e. already above them. The search
field needs no new mechanism; it is the launcher window it is today.

**The plain launcher is absorbed, not kept.** Two overlapping Super-triggered
overlays is the confusing outcome.

| trigger | today | after |
|---|---|---|
| bare-Super tap | opens the launcher | opens the **overview** (grid + empty search field) |
| bar hamburger | counts a press (a stub) | the same path as the tap |
| `Super+Space` | opens the launcher | **survives**, opens the overview with the search field focused |
| typing | searches apps | results **replace** the grid |
| `Escape` | closes the launcher | clears the search, or leaves the overview if it is already clear |

`nitro-launcher` keeps its crate, its `APP_NAME`, its `hey` addressability and
its never-rebuilt-only-hidden invariant (stated at the top of its `lib.rs`); it
grows overview state rather than being replaced. Search scope is an enum from
day one — `Apps` now, with `Windows` and `Files` named as future variants — so
the issue's "for now only searches applications" is a starting point rather than
a thing to refactor away from later.

**Animation** is a scrim fade plus a **position** interpolation over the slots,
budgeted at the measured 0.37 ms/frame for 16 cached thumbnails. A **scale**
animation is out: it would need a downscale cache in `nitro-raster`, which is
recorded as a lever with its number (1.04 ms per thumbnail on change) rather
than built speculatively.

**Multi-output**: the overview is per-output, like the MRU list and the z-order
(`docs/wm.md` §Multi-output), and the layout's `area` is the **work area** — the
output rect with the bar's exclusive zone subtracted, via
`Server::local_work_area`. GNOME keeps its top bar visible over the overview and
nitro should keep its bar visible for the same reason: the bar is how you leave.

The window set is `Layer::Normal` only — the bar, the wallpaper and the launcher
are not thumbnails — and **includes** `WindowState::Minimized`, which GNOME also
shows. A minimized window keeps its geometry and its place in the MRU order
(`docs/wm.md` §States), so including it costs nothing and an overview that
cannot reach a minimized window would be a worse `Alt+Tab`.
