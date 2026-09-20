# GNOME 45–47 (libadwaita) — Settings & Files visual analysis

All measurements are at 1× scale, taken with ImageMagick pixel sampling on the
downloaded screenshots and cross-checked against the libadwaita stylesheet SCSS
(`_colors.scss`, `_sidebars.scss`, `_lists.scss`, `_header-bar.scss`,
`_buttons.scss`, `_labels.scss`, `_misc.scss`, `_preferences.scss`, `_switch.scss`,
`_palette.scss`, `_common.scss` — all saved next to this file).

## 1. Files downloaded

| File | Shows |
|---|---|
| `settings-appearance.png` (1047×738) | Settings, light. Sidebar + Appearance page; "Appearance" selected; style picker card + wallpaper card. |
| `settings-mouse.png` | Settings, light. Mouse & Touchpad page; content header holds a view-switcher (Mouse / Touchpad) instead of a title; toggle-group row, slider row, switch row, radio rows. |
| `settings-keyboard.png` | Settings, light. Keyboard page; group titles + descriptions, drag-handle row, "Add…" row, radio rows, navigation rows with chevron and dim value. |
| `settings-sound.png` | Settings, light. Sound page; combo-row with dropdown button, slider rows, level meter in group header, chevron row. |
| `nautilus-list.png` (1122×822) | Files, light (GNOME 48-era build, Adwaita Sans). Places sidebar, path bar, list view with Name/Size/Modified/star columns, hovered row. |
| `nautilus-grid.png` | Files, light. Same window, grid view (96 px folder icons). |
| `nautilus-search.png` | Files, light. Search mode: path bar becomes a focused search entry (blue focus ring), selected grid item, floating status pill bottom-right. |
| `rel47-nautilus-network-l-screenshot.webp` (1817×1379, ~2×) | Files 47 light, Network view (from release.gnome.org/47). Cantarell font. Shows mounts with eject buttons in sidebar, boxed rows with subtitles, bottom action bar. |
| `rel47-nautilus-network-d-screenshot.webp` | Same, **dark**. |
| `rel47-nautilus-{light,dark}-preview.png` | 60% downscales of the two above for quick viewing. |
| `wikimedia-files47-dark.png` (1920×1080) | Files 47.0 **dark**, full desktop, two windows (CentOS Stream 10). Real 1× pixels → used for dark colour sampling. |
| `wikimedia-files47-dark-preview.png` | 55% downscale of above. |
| `adw-navigation-split-view.png` / `-dark.png` | libadwaita doc: AdwNavigationSplitView — sidebar pane vs content pane colours, hairline separator. |
| `adw-sidebar.png` / `adw-sidebar-dark.png` (+ `-composited.png`) | libadwaita doc `.navigation-sidebar`: selected pill row, section header "Places" with + button, hairline separator. Dark PNG is white-on-transparent; composited version is on `#222226`. |
| `adw-boxed-lists.png` / `-dark.png` (+ `-composited.png`) | libadwaita doc `.boxed-list` card with 3 rows. |
| `adw-navigation-sidebar.png` | Minimal navigation-sidebar with selected row. |
| `adw-preferences-window.png`, `adw-preferences-page.png` | AdwPreferencesWindow/Page: group title + description + boxed list, clamp margins, view-switcher in header. |
| `adw-header-bar.png` | Plain AdwHeaderBar: 47 px, bold centred title, circular close button, 1 px bottom shade. |
| `adw-action-row.png`, `adw-switch-row.png`, `adw-combo-row.png`, `adw-expander-row.png` | Row anatomy: title/subtitle stack left, control right; expander nested rows on darker bg. |

Note on eras: the apps.gnome.org Settings shots and the Wikimedia dark shot use the
**libadwaita 1.5/1.6 neutral-grey palette** (`#fafafa / #ebebeb / #303030 / #1e1e1e`).
Current libadwaita `main` (1.7+, GNOME 48) shifted every neutral by a tiny blue tint
(`#fafafb / #ebebed / #2e2e32 / #1d1d20`). Both are listed in §7. The Nautilus
apps.gnome.org shots already use the new tint (`#ebebed` sidebar).

## 2. Layout blueprint (split view: sidebar left, content right)

```
┌──────────────────────────────┬────────────────────────────────────────────────┐
│ [🔍]     Settings      [☰]   │              Page title / view switcher    [×] │ ← 47 px header, one per pane
├──────────────────────────────┤                                                │
│  ▢ Network                   │                                                │
│  ▢ Bluetooth                 │      Group title                               │
│ ───────────────              │      ┌────────────────────────────────┐        │
│  ▢ Displays                  │      │ Row title            control   │        │
│  ▢ Sound                     │      │ ─────────────────────────────  │        │
│ ▐▓▓ Appearance ▓▓▌  (pill)   │      │ Row title            control > │        │
│                              │      └────────────────────────────────┘        │
│ sidebar_bg                   │ window_bg (Settings) / view_bg (Files)         │
└──────────────────────────────┴────────────────────────────────────────────────┘
                             ↑ 1 px inset border (sidebar_border_color)
```

### Window
- Corner radius **15 px** (`--window-radius: $button_radius + 6` = 9+6). Sampled: yes, all four corners rounded, no title bar above the header.
- No visible outer border in light; 1 px `rgba(255,255,255,0.07)` outline in dark. Drop shadow is compositor-provided (large, soft; in screenshots ≈ 0 12px 40px rgba(0,0,0,.25) + 0 0 0 1px rgba(0,0,0,.1)).
- Settings default window in shots: 924×616 content (1047×738 incl. shadow). Files: 1000×700.

### Sidebar pane
- **Width**: Settings **229 px** (x 62→290) + 1 px border; Files **198 px** (x 62→259) + 1 px border. Use 200 px (Files) / 230 px (Settings). libadwaita defaults for `AdwNavigationSplitView`: `sidebar-width-fraction 0.25`, `min-sidebar-width 180`, `max-sidebar-width 280`.
- **Background**: `sidebar_bg_color` — light `#ebebeb` (old) / `#ebebed` (new); dark `#303030` (old) / `#2e2e32` (new). Contrast vs content: light sidebar is ~6 % darker than content (`#fafafa`/`#ffffff`); in dark the sidebar is **lighter** than the content (`#303030` vs `#1e1e1e` view).
- **Separator to content**: 1 px inset hairline on the sidebar's right edge, `box-shadow: inset -1px 0 var(--sidebar-border-color)`; `sidebar_border_color` = `rgba(0,0,6,0.07)` light → renders `#dbdbdb`; dark `rgba(0,0,6,0.36)` → renders `#1f1f1f`–`#272727`. **No** gap, no shadow.
- **Sidebar header**: same bg as the sidebar (flat headerbar, no bottom shade line). Height **47 px** (`min-height: 47px`, `padding: 6px 7px 7px 7px`). Contents: left = 34×34 flat circular icon button (search, 16 px symbolic); centre = title (**bold**, 11 pt, e.g. "Settings" / "Files"); right = 34×34 flat icon button (hamburger `open-menu-symbolic`). Sampled title glyphs: x≈147–205, y≈70–84.
- **Search**: not a persistent field. Search button in the sidebar header toggles an `AdwSearchBar` that drops in below the header (Settings) or replaces the path bar (Files). When shown, the entry is a 34 px tall pill (radius 9), bg `#ebebeb`-ish on white with 2 px accent focus ring at 50 % (`#77a5d4` sampled at the edge in `nautilus-search.png`).
- **Rows** (`.navigation-sidebar > row`):
  - Files: **36 px tall**, pitch **38 px** (2 px `margin-bottom`), horizontal margin **6 px** each side (`$menu_margin`) → row spans x 67→254 in a 198 px sidebar. Padding `3px 14px`, icon–label gap **12 px** (`border-spacing: 12px`). Radius **9 px** (`$menu_radius`).
  - Settings: **43 px tall**, pitch **45 px**, same 6 px side margins (x 68→284 in 229 px sidebar). Settings uses its own row widget with extra vertical padding.
  - List padding top 6 px (`padding-top: $menu_margin`), so the first row starts ≈ 6 px below the header (Files: header ends y≈101, first row y 107).
  - **Icon**: 16 px symbolic, `-gtk-icon-style: symbolic`, coloured with fg (`rgba(0,0,6,0.8)` light / `#fff` dark). Icon x-start ≈ 20 px from sidebar edge (6 margin + 14 padding). Label baseline aligned, regular weight 11 pt.
  - **Selected**: filled rounded rect, radius 9 px, bg `$selected_color = color-mix(currentColor 10%, transparent)` → light **`#d8d8d8`** (on `#ebebeb`), dark **`#444444`** (on `#303030`), text stays fg (no white-on-blue!). Hover: 7 % (`#dfdfdf` / `#3f3f3f`), active 16 %, selected+hover 13 %.
  - No chevrons, no counts in these apps.
- **Section separators**: 1 px `separator` inside the list, `margin: 6px` (so it spans x 67→285, i.e. the same width as rows); colour `$border_color = currentColor 15%` → light **`#cfcfcf`**, dark **`#464646`**. Vertical footprint: 1 px + 6 px margins (Settings: rows 173→231 = 45 + 13).
- **Section headers** (`adw-sidebar.png`): bold 11 pt label ("Places") with a flat `+` icon button trailing, `padding: 0 8px`, `min-height: 36px`, radius 9, margin `0 6px 2px`. Files 47 doesn't show them in the main sidebar; the libadwaita demo does.
- **Mounts** (`rel47-*`): row label truncates with ellipsis, trailing 16 px eject icon button (flat, circular) right-aligned inside the row.

### Content pane header
- Also **47 px**, flat, same bg as the pane behind it: Settings → `window_bg_color` (`#fafafa`), Files → `view_bg_color` (`#ffffff` / `#1e1e1e`). **No** bottom shade line in either app (Settings and Files use `.flat`/`AdwToolbarView` with `top-bar-style: flat`). In dark Files the header region is indistinguishable from the view except for the widgets.
- Settings: title centred, **bold 11 pt**, or an `AdwViewSwitcher` (pill toggle group, e.g. "Mouse | Touchpad": 120 px + 118 px wide, 34 px tall, selected pill bg `#e0e0e0`, radius 9). Right: circular close button (`window-controls`, 24 px circle, bg `currentColor 10%`, ✕ 16 px, `margin: 7px`).
- Files: `[<] [>]` flat icon buttons (34×34, arrows dim when disabled) → **path bar** → search toggle button → **split view-switcher button** (grid/list icon + `▾` dropdown, joined, 1 px separator) → close.
- Header content padding: 6/7 px; `border-spacing: 6px` between children.

### Content area (Settings / preferences pages)
- Page is an `AdwPreferencesPage`: `AdwClamp` **maximum-size 600 px**, tightening-threshold 400. Inside the clamp: `margin: 24px 12px`, groups separated by **24 px** (`border-spacing: 24px`). Measured card width in Settings **550 px** (x 364→913 in a 693 px pane) → Settings clamps at ~575 incl. 12 px side margins.
- First group title baseline ≈ 40 px below the header (header y 100 → title y 141: 24 px margin + text).
- **Group header**: title = `.heading` (bold 11 pt, `#2e2e2e`-ish fg); optional description below in dim fg (`--dim-opacity: 55%` → `#7f7f7f`), regular 11 pt; `margin-bottom: 6px` before the card; optional trailing suffix widget right-aligned to the card edge (e.g. `+ Add Picture…` flat button, level meter).
- **Boxed list card** (`list.boxed-list`, `%card`):
  - bg `card_bg_color` — light `#ffffff`, dark `rgba(255,255,255,0.08)` (→ `#333337` on `#222226`; older builds `#353535`).
  - radius **12 px** (`$card_radius`), applied to first/last row corners.
  - Shadow/outline: `0 0 0 1px rgba(0,0,6,.03), 0 1px 3px 1px rgba(0,0,6,.07), 0 2px 6px 2px rgba(0,0,6,.03)` — renders as a 1 px `#eaeaea` edge with a barely-there 2–3 px soft shadow below. **No** hard border. In dark the shadow is invisible; the card reads as a lighter surface.
  - Row separators: `border-bottom: 1px solid var(--card-shade-color)` → light `rgba(0,0,6,0.07)` on white = **`#ededed`**; dark `rgba(0,0,6,0.36)` ≈ `#212124` on the card. Full-width (edge to edge), none after the last row.
  - Row hover (activatable): `currentColor 3%` overlay (`#f7f7f7`); active 8 %.
- **Rows** (`AdwActionRow`):
  - `min-height: 50px` for the title box + `margin: 6px 0` on the title box → **single-line row ≈ 54–55 px** (measured pitch 55 px), **two-line (title+subtitle) ≈ 62–63 px** (Primary Button row 63 px).
  - Horizontal: `margin-left/right: 12px`, children `border-spacing: 6px`; prefix icon (16 px) then 12 px to the title. Title x = card-x + 15 (measured 364 → 379).
  - Title: regular 11 pt fg. Subtitle: `font-size: smaller` (≈ 82 % ≈ 9 pt / 12 px), dim (55 % opacity → `#7f7f7f`), 3 px below the title (`border-spacing: 3px`).
  - Trailing controls, right-aligned with 12 px inset: `GtkSwitch` (**48×26**: `border-radius 14px`, `padding 3px`, slider 20 px circle; off trough `currentColor 15%` = `#d6d6d6`, on trough `accent_bg_color #3584e4`, slider white with soft shadow), dim value label ("Right Alt") + **chevron** `go-next-symbolic` 16 px for navigation rows (chevron ≈ 16 px from the right edge), combo button (34 px tall, bg `#e6e6e6`, radius 9, icon + bold label + `▾`), toggle group (two joined 34 px buttons, selected bg `#c8c8c8`-ish), slider (`GtkScale`: 4 px trough `#d6d6d6`, fill accent, 20 px white knob with 1 px `rgba(0,0,6,.1)` outline), radio (`GtkCheckButton`: 20 px circle, checked = accent fill + white dot), `⋮` flat menu button, drag handle `list-drag-handle-symbolic` dim.
- **Expander rows**: header row like an action row with `▲/▼` chevron in accent colour when expanded; nested rows on `sidebar_shade`-like darker bg (`#f6f6f6` light) with the same 1 px separators.
- Scrollbar: overlay, 3 px thin `#8f8f8f` at 40 % until hovered, right edge of the pane, 6 px inset.

### Files-specific
- **Path bar** (`GtkEntry`-like box inside the header, `nautilus-list.png`): x 347→1049 (**703 px**, fills remaining header width), **34 px tall**, radius 9, bg `#ebebeb` light / `#343434` dark, no border. Contents: 16 px location icon (home symbolic) at 12 px inset, **bold** current-folder name ("Home") 8 px after the icon, breadcrumb parents shown as plain buttons when nested (`/ Documents / Work`), trailing `⋮` view/sort menu button (34 px) flush right inside the bar. In search mode the same box becomes a text entry with a 2 px accent ring, clear (⊗) and filter (⚙) icons on the right.
- **View switcher**: split button — main part shows the *other* view's icon (`view-grid-symbolic` when in list, `view-list-symbolic` when in grid), then a 1 px separator and a 16 px `pan-down-symbolic` dropdown part; both flat, joined; total ≈ 62×34.
- **List view** (`GtkColumnView` in Nautilus 45+):
  - Column header row: 28 px tall, labels in **caption-heading** (bold ≈ 12 px) dim `#8f8f8f`: `Name ˄` (sort indicator 10 px chevron), `Size`, `Modified`, star column unlabeled. Header text baseline at y≈116 (16 px below the header bar).
  - Rows **44 px tall**, pitch **52 px** (rows have 4 px vertical margin each → visible 8 px gaps between hover backgrounds), inset **24 px** from the pane edges (x 285→1036 in a 776 px pane), **radius 9 px** on hover/selection. Hover bg `#f7f7f7` (`view_hover_color` 4 %); selected bg `accent 25%` ≈ `#cddff8` when focused, grey `#e6e6e6` when unfocused.
  - Cell layout: 32 px file icon at 12 px inset (folder glyph is the Adwaita blue folder `#62a0ea`/`#3584e4` with white symbol), 12 px gap, name regular 11 pt; `Size` right-aligned dim ("0 items"); `Modified` left-aligned dim ("Yesterday 11:28 PM"); trailing `starred-symbolic` outline 16 px dim.
- **Grid view**: icon size **96 px** (medium zoom) inside a 154 × ~140 px cell (measured column pitch 154 px, row pitch 138 px); label centred below, 6 px gap, 11 pt fg, ellipsised to 2 lines. Selected/hovered item: rounded rect (radius 9) `#e0e0e0`-ish (unfocused) covering icon+label with 12 px padding (`nautilus-search.png` x 297→511 ≈ 214 px wide? — that item shows a 170×170 thumbnail card, so the selection rect is ~214×198). Grid starts 24 px below the header and 24 px from the left pane edge.
- **Status/toast pill** bottom-right (`nautilus-search.png`): floating rounded pill, radius 9, bg `#e6e6e6`, 30 px tall, 12 px padding, dim-fg text; 12 px from the pane edges.
- **Bottom action bar** (Network view, `rel47-*`): 47 px, same bg as view, contains a 34 px entry ("Server address") + "Connect" pill button (bg `#e6e6e6`, radius 9, disabled text dim) + circular info button.
- **Boxed rows with subtitles in Files** (Network): 40 px circular icon well (bg `sidebar_bg` `#ebebeb`/`#343434`) holding a 16 px symbolic icon; title 11 pt; subtitle 9 pt dim monospace-ish URL; trailing eject button. Rows are `boxed-list-separate`-like (individual cards with 12 px gap) with hover `#e6e6e6`.

## 3. Typography

- **Family**: GNOME 45–47 default is **Cantarell 11 pt** (visible in `rel47-*` and the Settings shots: round single-storey `g`, wide `a`). GNOME 48 switched to **Adwaita Sans 11 pt** (a metric-tuned Inter fork; visible in `nautilus-*.png`). Monospace: Source Code Pro 10 pt (45–47) → Adwaita Mono (48). For nitro: **Inter/Adwaita Sans, 11 pt = 14.67 px @96 dpi**; treat 15 px as the body size at 1×.
- Scale (from `_labels.scss`, relative to 11 pt / 14.67 px):
  - `.title-1`: 800 / 181 % → **26.5 px**
  - `.title-2`: 800 / 136 % → **20 px**
  - `.title-3`: 700 / 136 % → 20 px
  - `.title-4`: 700 / 118 % → **17.3 px**
  - `.heading` (group titles, header-bar title, row titles in `.property` rows): **700 / 100 %** → 14.67 px bold
  - `.body`: 400, line-height 140 %
  - `.caption-heading`: 700 / 82 % → **12 px** (list column headers)
  - `.caption` / `.subtitle` / `font-size: smaller`: 400 / 82 % → **12 px**, line-height 140 %
- Header-bar title: bold 11 pt, `padding 0 12px`, centred. Subtitle (rare): smaller, dim.
- Dim text: `--dim-opacity: 55%` of fg (light `rgba(0,0,6,.8)` × .55 ≈ `#7f7f7f`; dark white × .55 ≈ `#8c8c8c`-on-`#1e1e1e`).
- Disabled: `--disabled-opacity: 50%`.

## 4. Exact colour tokens

### Neutral surfaces (libadwaita `_colors.scss`)

| Token | Light (main/1.7+) | Light (1.5–1.6, GNOME 45–47 shots) | Dark (main/1.7+) | Dark (1.5–1.6, GNOME 45–47 shots) |
|---|---|---|---|---|
| `window_bg_color` | `#fafafb` | `#fafafa` | `#222226` | `#242424` |
| `window_fg_color` | `rgba(0,0,6,.8)` | `rgba(0,0,0,.8)` | `#ffffff` | `#ffffff` |
| `view_bg_color` | `#ffffff` | `#ffffff` | `#1d1d20` | `#1e1e1e` |
| `view_fg_color` | `rgba(0,0,6,.8)` | `rgba(0,0,0,.8)` | `#ffffff` | `#ffffff` |
| `headerbar_bg_color` | `#ffffff` | `#ffffff` | `#2e2e32` | `#303030` |
| `headerbar_fg_color` | `rgba(0,0,6,.8)` | — | `#ffffff` | `#ffffff` |
| `headerbar_border_color` | `rgba(0,0,6,.8)` | — | `#ffffff` | — |
| `headerbar_backdrop_color` | = window_bg | — | = window_bg | — |
| `headerbar_shade_color` | `rgba(0,0,6,.12)` | `rgba(0,0,0,.12)` | `rgba(0,0,6,.36)` | `rgba(0,0,0,.36)` |
| `headerbar_darker_shade_color` | `rgba(0,0,6,.12)` | — | `rgba(0,0,12,.9)` | — |
| `sidebar_bg_color` | `#ebebed` | `#ebebeb` | `#2e2e32` | `#303030` |
| `sidebar_fg_color` | `rgba(0,0,6,.8)` | — | `#ffffff` | `#ffffff` |
| `sidebar_backdrop_color` | `#f2f2f4` | `#f2f2f2` | `#28282c` | `#2a2a2a` |
| `sidebar_shade_color` | `rgba(0,0,6,.07)` | — | `rgba(0,0,6,.25)` | — |
| `sidebar_border_color` | `rgba(0,0,6,.07)` | — | `rgba(0,0,6,.36)` | — |
| `secondary_sidebar_bg_color` | `#f3f3f5` | `#f3f3f3` | `#28282c` | `#2a2a2a` |
| `secondary_sidebar_backdrop_color` | `#f6f6fa` | — | `#252529` | — |
| `card_bg_color` | `#ffffff` | `#ffffff` | `rgba(255,255,255,.08)` | `rgba(255,255,255,.08)` |
| `card_shade_color` | `rgba(0,0,6,.07)` | — | `rgba(0,0,6,.36)` | — |
| `dialog_bg_color` / `popover_bg_color` | `#fafafb` / `#ffffff` | `#fafafa` / `#ffffff` | `#36363a` / `#36363a` | `#383838` / `#383838` |
| `popover_shade_color` | `rgba(0,0,6,.07)` | — | `rgba(0,0,6,.25)` | — |
| `thumbnail_bg_color` | `#ffffff` | — | `#39393d` | — |
| `shade_color` | `rgba(0,0,6,.07)` | — | `rgba(0,0,6,.25)` | — |
| `scrollbar_outline_color` | `#ffffff` | — | `rgba(0,0,12,.95)` | — |
| `--active-toggle-bg-color` | `#ffffff` | — | `rgba(255,255,255,.2)` | — |
| `--overview-bg-color` | `#f3f3f5` | — | `#28282c` | — |
| `$toast_bg_color` | `#505053` | — | `#505053` | — |
| `$osd_bg_color` / `$osd_fg_color` | `rgba(0,0,0,.7)` / `rgba(255,255,255,.9)` | | same | |
| `$window_outline_color` (dark only) | — | — | `rgba(255,255,255,.07)` | — |

**Sampled from screenshots (rendered values, use these as flat fallbacks):**

| Element | Light | Dark |
|---|---|---|
| Sidebar bg | `#ebebeb` (Settings) / `#ebebed` (Files 48) | `#303030` |
| Content bg | `#fafafa` (Settings, window_bg) / `#ffffff` (Files, view_bg) | `#1e1e1e` (Files view) |
| Sidebar↔content hairline | `#dbdbdb` | `#1f1f1f`–`#272727` |
| Sidebar selected row | `#d8d8d8` | `#444444` |
| Sidebar hover row | `#dfdfdf` | `#3f3f3f` |
| Sidebar section separator | `#cfcfcf` | `#464646` |
| Card bg | `#ffffff` | `#333337` (new) / `#353535` (old) |
| Card edge ring | `#eaeaea` (1 px) | n/a |
| Card row separator | `#ededed` | `#212124` |
| Path bar / entry bg | `#ebebeb` (on white) | `#343434` |
| Flat button hover | `#e6e6e6`–`#e0e0e0` | `#3a3a3a` |
| List row hover | `#f7f7f7` | `#262626` |
| Toggle-group selected | `#c8c8c8` | `#4a4a4a` |
| Body text | `#2e2e2e`–`#333333` (0,0,6 @ 80 %) | `#ffffff` |
| Dim text | `#7f7f7f` | `#8c8c8c` |
| Close-button circle | `#e0e0e0` bg, `#2e2e2e` glyph | `#3d3d3d` bg, white glyph |

### Accent & semantic
- `accent_bg_color` (default "blue"): **`#3584e4`** (`$blue_3`); `accent_fg_color`: `#ffffff`.
- `accent_color` (standalone text/icon on neutral bg): `oklab(from accent_bg min(l,.5) a b)` → light **`#1c71d8`**-ish (matches `$blue_4`), dark `max(l,.85)` → **`#78aeed`**-ish (≈ `$blue_2` lightened).
- GNOME 47 system accent palette (`--accent-*`): blue `#3584e4`, teal `#2190a4`, green `#3a944a`, yellow `#c88800`, orange `#ed5b00`, red `#e62d42`, pink `#d56199`, purple `#9141ac`, slate `#6f8396`.
- destructive `#e01b24` light / `#c01c28` dark (fg white); success `#2ec27e` / `#26a269`; warning `#e5a50a` (fg `rgba(0,0,0,.8)`) / `#cd9309`; error = destructive.
- Palette (`_palette.scss`): blue `#99c1f1 #62a0ea #3584e4 #1c71d8 #1a5fb4`; green `#8ff0a4 #57e389 #33d17a #2ec27e #26a269`; yellow `#f9f06b #f8e45c #f6d32d #f5c211 #e5a50a`; orange `#ffbe6f #ffa348 #ff7800 #e66100 #c64600`; red `#f66151 #ed333b #e01b24 #c01c28 #a51d2d`; purple `#dc8add #c061cb #9141ac #813d9c #613583`; light `#ffffff #f6f5f4 #deddda #c0bfbc #9a9996`; dark `#77767b #5e5c64 #3d3846 #241f31 #000000`.

### State overlays (all `color-mix(currentColor N%, transparent)` — i.e. fg-tinted, so they auto-adapt to dark)
- `$border_color`: **15 %** (hc 50 %) → separators, button outlines
- `$hover_color` 7 %, `$active_color` 16 %
- `$selected_color` 10 %, `$selected_hover_color` 13 %, `$selected_active_color` 19 %  (sidebar rows)
- `$view_hover_color` 4 %, `$view_active_color` 8 %  (list/grid views)
- `$view_selected_color` = **accent 25 %**, hover 32 %, active 39 %  (list/grid selection)
- `$trough_color` 15 % / hover 20 % / active 25 %  (switch off, scale trough, progress)
- `$focus_border_color` = accent 50 %, 2 px ring, offset −1..−2 px
- Flat button hover = 7 %, active 16 %; regular button bg = 10 % (`#e6e6e6` on `#fafafa`), hover 15 %, active 30 %.

## 5. Corner radii, borders, shadows

| Thing | Radius |
|---|---|
| Window (`--window-radius`) | **15 px** (top and bottom; GTK4 rounds all four) |
| Dialogs (`$dialog_radius`) | 15 px |
| Popovers (`$popover_radius`) | 15 px (menu radius 9 + 6) |
| Cards / boxed lists (`$card_radius`) | **12 px** |
| Buttons, entries, path bar, menu items, sidebar rows, list-view rows, grid selection (`$button_radius`/`$menu_radius`) | **9 px** |
| Toggle-group segments | 9 px outer, 0 inner |
| Switch trough | 14 px (fully round, 26 px tall) |
| Circular buttons (close, eject, info) | 50 % |
| Toasts / status pill | 9 px (toast uses 150 px pill in newer builds) |

Borders: libadwaita almost never draws solid borders. Separation is by **surface colour change** plus **hairlines**: 1 px `inset` box-shadows at `sidebar_border_color` (7 % / 36 %) between panes, `headerbar_shade_color` under non-flat header bars (Settings & Files use flat → none), `card_shade_color` between card rows, `$border_color` (15 %) for `GtkSeparator` and outlined widgets (high-contrast mode raises these to 50 %).

Shadows: only cards (`0 0 0 1px rgba(0,0,6,.03), 0 1px 3px 1px rgba(0,0,6,.07), 0 2px 6px 2px rgba(0,0,6,.03)`), popovers/dialogs (`0 1px 5px 1px rgba(0,0,0,.09), 0 2px 14px 5px rgba(0,0,0,.05)` + 1 px 12 %/outline), and the compositor window shadow. Flat header bars have none.

## 6. Spacing rhythm

libadwaita is on a **3 px grid with 6 px as the working unit**: 3 / 6 / 9 / 12 / 18 / 24 (/ 36 / 48).

- 3 px: title↔subtitle gap, header-bar child spacing in compact mode, toolbar `border-spacing`
- 6 px: `$menu_margin` — sidebar row side margins, sidebar list top padding, separator margins, header-bar child spacing, row child spacing, group title → card, switch/combos inside rows, header padding (6/7)
- 9 px: button/menu radius, header widget margin (`margin: 9px` for the title widget area)
- 12 px: row horizontal margin, title padding in header bar, `+`/prefix icon → label gap (`border-spacing: 12px`), clamp side margin, gap between `boxed-list-separate` cards, list-view row inset-to-cell
- 14 px: sidebar row horizontal padding (`3px 14px`)
- 18 px: `list.content` top padding
- 24 px: preferences-page top/bottom margin, gap between preference groups, Files content inset from the pane edges (list rows and grid start 24 px in), first group title below header
- Heights: 34 px controls (buttons, entries, path bar, combos, toggle groups) ; 36 px sidebar rows ; 44 px list rows ; 47 px header bars ; 50–55 px single-line boxed rows ; 62–63 px two-line rows.
- Icon sizes: 16 px symbolic everywhere in chrome; 32 px file icons in list view; 96 px (or 64/128/256 via zoom) in grid view; 20 px switch slider / radio; 24 px close-button circle; 40 px icon wells (Network rows).

## 7. Implementation cheat-sheet (light / dark, GNOME-47-era values)

```
window.bg          #fafafa / #242424    radius 15
sidebar.bg         #ebebeb / #303030    width 200 (Files) | 230 (Settings), 1px right hairline #dbdbdb / #1f1f1f
sidebar.row        h36 (Files) | h43 (Settings), mx6, px14, gap12, r9, icon16
sidebar.row.sel    #d8d8d8 / #444444    (fg 10%)  ; hover fg 7%
sidebar.separator  1px #cfcfcf / #464646, mx6, my6
header             h47 flat, same bg as pane; title bold 15px centred; 34px flat icon buttons, 6px gap, 7px edge pad
content.bg         Settings #fafafa / #242424 ; Files #ffffff / #1e1e1e
clamp              600 max, margin 24 12, group gap 24
group.title        bold 15px fg ; description 15px dim(55%) ; 6px above card
card               #ffffff / rgba(255,255,255,.08)  r12  ring 1px rgba(0,0,6,.03) + soft shadow
card.row           h54 (1-line) | h62 (2-line), mx12, sep 1px rgba(0,0,6,.07)/(.36) ; title 15px, subtitle 12px dim
switch             48×26 r14, off fg15%, on #3584e4, knob 20 white
pathbar            h34 r9 #ebebeb / #343434, icon16 + bold label, ⋮ trailing
list.row (Files)   h44 pitch52 r9 inset24, icon32 gap12, hover fg4%, sel accent25%
grid.cell (Files)  icon96, cell ~154×138, label 15px centred, sel r9
accent             #3584e4 ; standalone #1c71d8 / #78aeed
fg                 rgba(0,0,6,.8) / #ffffff ; dim 55% ; disabled 50%
font               Cantarell (45–47) → Adwaita Sans/Inter (48+), 11pt = 14.67px; caption 82% = 12px; heading 700
```
