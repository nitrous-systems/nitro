# macOS System Settings & Finder — visual design analysis for nitro

Scope: modern macOS sidebar-left / content-right windows (System Settings and Finder).
Sources: Apple Support screenshots (real app captures), Apple HIG anatomy images
(light + dark), plus documented AppKit semantic colors. All pixel measurements were
taken with ImageMagick from the files below and converted to 1× points.

> Caveat on scale. Apple Support screenshots are @2× captures that have been
> downscaled (System Settings ≈ 0.8×, Finder ≈ 0.7× of native). Where a raw
> measurement is quoted I give both the measured value and the corrected /
> canonical 1× value. Colors are unaffected by scaling (except hairlines, which
> get antialiased — those are reported as "≈").
>
> Caveat on version. Apple has refreshed its support/HIG imagery for macOS 26
> ("Tahoe" – capsule toolbar buttons, larger radii). The *layout* (sidebar +
> grouped-form content, section headers, row metrics) is unchanged from
> Ventura → Sequoia (13–15); the Ventura–Sequoia values are what is specified
> below, with Tahoe deviations noted where they matter.

---

## 1. Downloaded files (all verified with `file` as PNG)

| File | What it shows |
|---|---|
| `system-settings-menubar-apple-support.png` | **Real System Settings window, light**, 1144×1112 @2×. Sidebar (search field, Apple-Account row, Wi-Fi/Bluetooth/…, selected "Menu Bar" row in accent blue), toolbar with back/forward + title, content = grouped form boxes with pop-ups, toggles, checkboxes, push buttons, section caption + footnote. Primary reference. |
| `finder-window-apple-support.png` | **Real Finder window, light**, 1144×726 @2×. Sidebar with Recents/Shared, *Favorites*, *Locations*, *Tags* sections, selected "Documents" row (gray, non-accent), unified toolbar: back/fwd, title "Documents — Local", view switcher (icon/list/column/gallery), group/share/tag/more, search. Icon-view content. Primary reference. |
| `hig-toolbars-mac-window-anatomy@2x.png` | HIG "Mac window anatomy" (Finder-style), **light**: sidebar #E2E2E2, sections Favorites/Locations, toolbar + content. Good for structural proportions (row pitch, header size, traffic-light geometry). |
| `hig-toolbars-mac-window-anatomy~dark@2x.png` | Same, **dark** variant: sidebar #252525, content black, toolbar buttons #1A1A1A, header text #7C7C79. Dark token source. |
| `hig-window-states@2x.png` | HIG window states: an **inactive Finder window** (gray traffic lights, dimmed text, no accent) behind an active Notes window and a Colors panel — shows inactive-state treatment and window layering/shadows. |
| `hig-toolbars-notes-app-expanded-icons@2x.png` | Notes toolbar with an open pull-down menu — menu styling (radius, highlight row in yellow accent, separators, 13 pt items). |
| `hig-sidebars-extend-content-beneath-sidebar-correct@2x.png` | HIG "content extends beneath the sidebar" — sidebar translucency/vibrancy demonstration (blurred, desaturated backdrop showing through the sidebar). |

Wikimedia Commons was rate-limited and contained no usable Ventura+/Sonoma
System Settings/Finder captures; the only download from there (a QuickLook
window) was discarded as off-topic.

---

## 2. Layout blueprint (both apps)

```
┌──────────────────────────────────────────────────────────────────────┐ radius 10–11 (Tahoe: ~16)
│ ● ● ●     [search]      │ ‹ ›  Title              [toolbar items]   🔍 │ ← toolbar 52 pt, spans full width
│  SIDEBAR (vibrant)      │  CONTENT (opaque)                          │
│  section header         │                                            │
│  ▣ Row                  │                                            │
│  ▣ Row  (selected)      │                                            │
│                         │                                            │
└─────────────────────────┴────────────────────────────────────────────┘
        ~215 pt (SS)          hairline divider (only visible in light)
        180–250 pt (Finder)
```

* **Toolbar/titlebar**: one *unified* toolbar spanning the whole window
  (NSWindow "full-size content view + unified toolbar"). Visually, however, the
  sidebar column is painted **behind** it: the sidebar's vibrant material
  extends up to the top edge, the traffic lights sit in the sidebar column, and
  the content pane's toolbar region is the content background color. There is
  **no horizontal line** under the toolbar in the sidebar column; in the content
  column a hairline appears **only once the content is scrolled** (measured
  #F5F5F5 line at toolbar bottom when unscrolled – essentially invisible).
* **Traffic lights**: three 12 pt circles, centers 20 pt apart, first circle's
  left edge 13 pt from the window's left edge (measured 12–13 pt / 20 pt pitch,
  scale-corrected), vertically centered in the 52 pt toolbar (center y ≈ 26).
  Colors: close **#FF5F57**, minimize **#FEBC2E**, zoom **#28C840** (HIG dark
  file, exact); the support screenshot has slightly desaturated
  #EC6765 / #F7CE46 / #65C466 (JPEG-ish rescale). Inactive window: all three
  become **#D9D9D9**-ish gray (rgba(0,0,0,0.15) on the sidebar).
* **Sidebar width**: System Settings **215 pt** fixed-ish (measured 169 pt at
  0.8 scale → 211). Finder default ≈ **180–200 pt**, user-resizable
  ~130–400 pt; measured 94 pt in the (narrow, scaled) support capture.
* **Sidebar background**: NSVisualEffectView `.sidebar` material (behind-window
  blending: blurred, desaturated desktop shows through). Opaque fallbacks that
  match the captures: light **#EDEDED** (support screenshots) – **#E2E2E2**
  (HIG, over a darker backdrop); dark **#252525** (HIG) — realistic dark range
  #232323–#2B2B2B. Rule: sidebar is *slightly darker* than content in light
  mode, *slightly lighter* than content in dark mode.
* **Sidebar/content divider**: 1 px hairline. Light: measured transition
  #EDEDED → #EAECEF/#ECECED → #FFFFFF, i.e. ≈ rgba(0,0,0,0.08) — barely there.
  Dark: none visible (#252525 → content directly). Use `separatorColor` at
  0.5 alpha, or omit in dark.
* **Sidebar rows** (both apps):
  * Row height **28 pt** (SS, "medium" sidebar icon size; measured 23 pt
    scaled → 28.75). Finder medium also 28 pt; Finder "small" = 24, "large" = 32.
    Support Finder capture has 38 px/2/0.7 ≈ 27 pt pitch.
  * Row pitch in System Settings sidebar = 31 pt (28 pt row + ~3 pt gap
    between adjacent selection rects; measured 25 pt scaled).
  * Selection rect inset **10 pt** from sidebar left edge and **10 pt** from
    the divider (measured 8.5 pt scaled each side); corner radius **5–6 pt**
    (measured: corner spans 4 px rows/cols at 2×·0.8 → 5 pt).
  * Icon: left edge at **10 pt** inside the selection rect (20 pt from sidebar
    edge). Finder: 16 pt SF Symbol, accent colored (#006BEF measured ≈
    systemBlue). System Settings: **20 pt rounded-square badge** (radius ~5 pt,
    white glyph on a per-pane color). Label starts **8–10 pt** after the icon.
  * Label: SF Pro Text **13 pt regular**, `labelColor`. Selected row (window
    active): label **white**, icon white-on-accent.
  * Selected-row fill: **accent color** when the *sidebar is the emphasized
    responder* (SS): measured **#2F6EED** in the capture (accent blue #007AFF
    composited on the vibrant sidebar; true AppKit value is
    `selectedContentBackgroundColor` = **#0063E1** light / **#0058D0** dark).
    Finder with focus in the content area shows the *unemphasized* selection:
    **#E2E2E2** measured ≈ `unemphasizedSelectedContentBackgroundColor`
    **#DCDCDC** light / **#464646** dark, label stays `labelColor`.
    Inactive window: always the gray unemphasized variant.
  * Hover: no hover highlight on macOS sidebar rows.
* **Section headers** (Finder: "Favorites", "iCloud", "Locations", "Tags";
  SS has none but uses 12 pt vertical gaps between groups): SF Pro **11 pt
  bold**, color `secondaryLabelColor` (measured **#616161–#666666** light on
  #EDEDED; **#7C7C79** dark on #252525), NOT uppercase (macOS 11+ uses
  Title Case; Big Sur dropped all-caps). Left-aligned with the *row label text*
  in HIG (x = 20 pt) — Apple's own Finder aligns it with the icon (measured
  x ≈ 20 pt from sidebar edge). Header row height 24 pt, top margin 12 pt
  before a new section, ~4 pt below header. Section header has a disclosure
  chevron on hover (Finder) that collapses the section.
* **Search field**: System Settings – top of sidebar, full width minus 8 pt
  each side, y ≈ 52 pt (directly under the traffic-light row), height
  **28 pt**, radius **6 pt**, fill light **#ECECEC–#F1F1F1** with 1 px border
  **#E0E0E0**; magnifying-glass icon 13 pt + placeholder "Search"
  `placeholderTextColor` (rgba(0,0,0,0.25)). Finder – rightmost toolbar item
  (collapsed to a 🔍 icon button until clicked, expands to ~200 pt).
* **Below the search field in SS**: the Apple Account row (32 pt avatar
  circle, 13 pt name + 11 pt "Apple Account" secondary), then a 12 pt gap,
  then connectivity group (Wi-Fi, Bluetooth, Network, VPN, Battery), 24 pt gap,
  then General … (the gaps are the only "section" cue).

---

## 3. System Settings content pane

* Toolbar: **back/forward** pair at left (Ventura–Sequoia: two borderless
  chevron buttons, each 28×24 pt, hover = rgba(0,0,0,0.06) rounded 6; Tahoe:
  a capsule with a divider, measured fill #FCFCFC + shadow). Title follows
  at x = toolbar-left + ~76 pt: SF Pro **15 pt bold** (`.title3`-ish), `labelColor`.
  Ventura–Sequoia title is 15 pt semibold. No other toolbar items.
* Content background: **#FFFFFF** light (`controlBackgroundColor`) — NOT the
  gray `windowBackgroundColor`; dark ≈ **#1E1E1E** (SS dark actually renders
  ~#292929 — between `windowBackgroundColor` #323232 and #1E1E1E; use #262626
  as the dark token).
* Scrolling column with **20 pt** left/right padding (measured 16.5 pt scaled
  → 20; group spans x = 20 … width−20), **top padding 10 pt** under the
  toolbar, bottom padding 20 pt. Content max width: none (groups stretch);
  SS window min width 715 pt.
* **Group ("form box")**:
  * Fill: measured **#F7F7F7** on white = black @ ~3.5 %. Dark: ~#333333 on
    #262626 ≈ white @ 5–6 %.
  * Border: at this scale none is resolvable (edge goes #F7F7F7→#FFFFFF
    directly). Native rendering has a 1 px inset stroke of ≈ rgba(0,0,0,0.05)
    light / rgba(255,255,255,0.08) dark. Optional; if omitted the fill alone
    matches the capture.
  * Corner radius **8–10 pt** (measured 13 px at 2×·0.8 = 8 pt; SwiftUI grouped
    Form on macOS uses 8, Tahoe ~10–12). **Use 8 pt.**
  * Vertical gap between consecutive groups **10 pt** (measured 8 scaled).
  * Rows: **36 pt** tall for plain label+control rows (measured 29 scaled → 36);
    **40 pt** when the row carries a 22 pt app-icon badge (measured 33 → 40).
    Multi-line description rows are taller (text wraps, 11 pt secondary line
    under the 13 pt label).
  * Row separator: 1 px, measured **#EBEBEB** on #F7F7F7 (≈ rgba(0,0,0,0.05));
    dark ≈ rgba(255,255,255,0.08). **Inset 10 pt from the left** of the group
    (starts at the label's x) and runs to the right edge of the group
    (measured: begins x = 390 vs group x = 374 at 2×·0.8 ≈ 10 pt).
    No separator after the last row.
  * Label: **13 pt regular** `labelColor` (#262626 measured), left at 10 pt
    inset; vertically centered. Sub-label: 11 pt `secondaryLabelColor`.
  * Control: right-aligned, **10 pt** from group right edge (measured 8.5).
    Row content x-range therefore = [10, width−10].
  * Controls seen: pop-up button (value text 13 pt + 16×16 up/down chevron
    "stepper" capsule with fill **#E4E4E4**, radius 5), **toggle** (Ventura+
    switch, 26×15 pt, on-fill **#3374EE** ≈ accent, knob white with 1 px
    shadow, off-track white with 1 px border **#DDDDDD**), **checkbox**
    (14×14, radius 3, checked fill **#3375F0**, white ✓), **push button**
    (secondary: height 22 pt, radius 6, fill **#E4E4E4** light / #5A5A5A dark,
    13 pt label; primary: accent fill, white text; measured "Clock Options…"
    height ≈ 17 scaled → 22 pt).
  * Section caption above a group ("Menu Bar Controls"): **13 pt bold**
    `labelColor`, left-aligned with the *group edge* (x = 20 pt), 24 pt above
    the group top (measured 14 → ~18 scaled), 6 pt gap to the group.
  * Footnote / explanatory text *inside* a group header row: **11 pt
    regular** `secondaryLabelColor` (#6C6C6C measured), wrapped, with an
    optional decorative image floated right; footnotes *below* a group are
    the same 11 pt secondary, x = 20 + 10 = 30 pt inset, 6 pt below.

---

## 4. Finder specifics

* **Sidebar sections** (order): *(no header)* Recents, Shared (Ventura+
  "Shared" with people icon); **Favorites** (AirDrop, Recents, Applications,
  Desktop, Documents, Downloads, user folders); **iCloud** (iCloud Drive,
  Shared); **Locations** (computer, disks, network, Trash in Tahoe); **Tags**
  (colored 10 pt dots: red **#EE2C21**, orange **#EE8600**, yellow
  **#EFBC00**, green **#19BC32**, blue **#006BEF**, purple, gray). Sidebar
  icons are SF Symbols 16 pt, tinted **accent** (#006BEF ≈ systemBlue).
* **Toolbar** (52 pt, left→right): back/forward (each 24 pt wide, 16 pt
  chevron, `labelColor` @ 0.75; disabled at 0.3), title = folder name
  **15 pt bold** (`#4D4D4D`-looking because of AA; it is labelColor), then
  centered-ish group: **view switcher** – a 4-segment segmented control
  (icon/list/column/gallery), each segment 32×24 pt, 15 pt SF Symbols, selected
  segment fill **#E5E5E5** light / rgba(255,255,255,0.15) dark, radius 6;
  group-by pull-down (⊞ ▾); then a right cluster: share ↑, tag 🏷, more ⋯,
  and finally the **search** button. Ventura–Sequoia toolbar items are
  *borderless* (icon only, 17 pt symbols, hover rgba(0,0,0,0.05) radius 6);
  Tahoe puts them on capsule glass backgrounds (#FCFCFC + shadow, measured).
  Icon color `labelColor` @ ~0.85 (#333333 measured).
* **Content**: icon view – 64 pt icons on a 100–120 pt grid, labels 12 pt
  centered, 2 lines max with middle-truncation ("Furniture Collecti…posal.pdf"),
  selection = 5 pt-radius rounded rect behind icon (rgba(0,0,0,0.1)) + label
  pill in accent. Folder icon blue **#5EB1F5**-ish, green shared folder.
* **List view**: header row 24 pt, header labels 11 pt regular secondary,
  column dividers hairline `separatorColor`, sortable column shows ▲/▼;
  rows 20 pt (small icon 16 pt), alternating row backgrounds (white /
  **#F5F5F5** light, #1E1E1E / #262626 dark); selected row accent (or
  #DCDCDC unemphasized); disclosure triangles 11 pt; default columns Name,
  Date Modified, Size, Kind.
* **Column view**: columns 220 pt default (min ~100), resizable; hairline
  dividers; preview column on the far right.
* **Path bar** (optional, bottom): 22 pt tall, breadcrumb items = 16 pt icon
  + 11 pt label, "›" separators in `tertiaryLabelColor`, hairline top border.
* **Status bar** (optional, below path bar): 22 pt, centered 11 pt
  `secondaryLabelColor` "N items, X GB available", hairline top border; when
  both bars show, path bar sits above status bar.
* Sidebar divider drags to resize; double-click resets. Sidebar collapses via
  ⌥⌘S or dragging to < ~130 pt.

---

## 5. Typography (SF Pro; fall back to Inter/Roboto with same metrics)

| Role | Size / weight | Color |
|---|---|---|
| Body, sidebar rows, form labels, buttons, pop-up values | **13 pt regular** | labelColor |
| Emphasized body / group captions | 13 pt **bold** (semibold in Ventura) | labelColor |
| Toolbar / page title | **15 pt bold** (Ventura–Sequoia 15 pt semibold; large-title pages 17–26 pt) | labelColor |
| Sidebar section headers | **11 pt bold** | secondaryLabelColor |
| Secondary line under a label, footnotes, status/path bar, list headers | **11 pt regular** | secondaryLabelColor |
| Icon-view file labels | 12 pt regular | labelColor |
| Menu items | 13 pt regular | labelColor |

Line height ≈ 1.23× (13 pt → 16 pt). Letter-spacing 0. Measured cap-height
ratio header/body = 7/13 px ≈ 0.54 → confirms 11 pt vs 13 pt.

---

## 6. Color tokens

### 6a. Documented AppKit semantic colors (default blue accent)

| Token | Light | Dark |
|---|---|---|
| accent / systemBlue | **#007AFF** | **#0A84FF** |
| selectedContentBackground (emphasized selection) | **#0063E1** | **#0058D0** |
| unemphasizedSelectedContentBackground | **#DCDCDC** | **#464646** |
| selectedTextBackground | #B3D7FF | #3F638B |
| windowBackground | **#ECECEC** | **#323232** |
| controlBackground / textBackground (content, lists) | **#FFFFFF** | **#1E1E1E** |
| underPageBackground | #969696 @ 90 % → ≈ #E6E6E6 effective | #282828 |
| control (button face) | #FFFFFF | rgba(255,255,255,0.25) |
| labelColor (primary) | rgba(0,0,0,0.85) → **#262626** on white | rgba(255,255,255,0.85) → **#D9D9D9** |
| secondaryLabel | rgba(0,0,0,0.50) → **#808080** | rgba(255,255,255,0.55) → #8C8C8C on #1E1E1E |
| tertiaryLabel | rgba(0,0,0,0.25) | rgba(255,255,255,0.25) |
| quaternaryLabel / placeholderText | rgba(0,0,0,0.10) / 0.25 | rgba(255,255,255,0.10) / 0.25 |
| separator | rgba(0,0,0,0.10) → **#E6E6E6** on white | rgba(255,255,255,0.10) → #333333 on #1E1E1E |
| grid | rgba(0,0,0,0.10) | rgba(255,255,255,0.10) |
| link | #0068DA | #419CFF |
| systemGray | #8E8E93 | #98989D |
| systemRed / Orange / Yellow / Green | #FF3B30 / #FF9500 / #FFCC00 / #28CD41 | #FF453A / #FF9F0A / #FFD60A / #32D74B |
| systemTeal / Indigo / Purple / Pink | #59ADC4 / #5856D6 / #AF52DE / #FF2D55 | #6AC4DC / #5E5CE6 / #BF5AF2 / #FF375F |
| traffic lights close / min / zoom | #FF5F57 / #FEBC2E / #28C840 | same |
| traffic lights inactive | rgba(0,0,0,0.15) ≈ #D9D9D9 | rgba(255,255,255,0.15) ≈ #4A4A4A |

### 6b. Measured / composed tokens for the two apps (opaque fallbacks)

| Surface | Light (measured) | Dark (measured HIG / recommended) |
|---|---|---|
| Sidebar material fallback | **#EDEDED** (over light desktop) … #E2E2E2 (darker desktop) | **#252525** … #2B2B2B |
| Sidebar selected row (emphasized) | **#2F6EED** (accent composited) → use #0063E1 or accent | #0058D0 |
| Sidebar selected row (unemphasized/inactive) | **#E2E2E2** → token #DCDCDC | #464646 |
| Sidebar row text | #000000 @ 0.85 | #FFFFFF @ 0.85 |
| Sidebar section header text | **#616161** | **#7C7C79** |
| Sidebar↔content divider | ≈ #E0E0E0 (rgba(0,0,0,0.08)) | none / rgba(255,255,255,0.06) |
| Content / toolbar background | **#FFFFFF** | **#1E1E1E** (Finder) · #262626 (SS) |
| Toolbar bottom line (only when scrolled) | #F5F5F5 → rgba(0,0,0,0.08) | rgba(255,255,255,0.08) |
| Form group fill | **#F7F7F7** (black @ 3.5 %) | #333333 (white @ 6 % on #262626) |
| Form group border (optional) | rgba(0,0,0,0.05) | rgba(255,255,255,0.08) |
| Form row separator | **#EBEBEB** (black @ 5 % on group) | rgba(255,255,255,0.08) |
| Secondary button fill | **#E4E4E4** | #5A5A5A |
| Pop-up stepper capsule | **#E4E4E4** | #5A5A5A |
| Toggle on / checkbox on | **#3374EE** / **#3375F0** (≈ accent) | #0A84FF |
| Toggle off track border | **#DDDDDD** | #5A5A5A |
| Search field fill / border | **#ECECEC** / **#E0E0E0** | #2F2F2F / #3E3E3E |
| Segmented control selected segment | **#E5E5E5** | rgba(255,255,255,0.15) |
| Toolbar icon | **#333333** (label @ 0.8) | #B8B8B8 |
| Toolbar capsule button (Tahoe only) | #FCFCFC + shadow | #1A1A1A |
| Finder tag dots | #EE2C21 #EE8600 #EFBC00 #19BC32 #006BEF | same |
| Finder sidebar symbol tint | **#006BEF** (accent) | #0A84FF |

---

## 7. Corner radii, borders, shadows

| Element | Radius |
|---|---|
| Window | **10 pt** (macOS 11–14), 11 pt (Sequoia); measured ≈ 11–13. Tahoe ≈ 16–26. |
| Form group | **8 pt** (accept 8–10) |
| Sidebar selection row | **5 pt** (accept 5–6) |
| Push button / segmented control / search field / pop-up | **6 pt** (Ventura–Sequoia); 5 pt on 22 pt-tall buttons |
| Toggle switch | fully rounded (h/2 = 7.5) |
| Checkbox | 3 pt |
| Sidebar icon badge (SS) | 5 pt on a 20 pt tile |
| Menu / popover | 6 pt (Ventura–Sequoia), 10+ in Tahoe; 1 px border rgba(0,0,0,0.1), item highlight radius 4 |
| Icon-view selection | 5 pt |

Borders: macOS uses almost no opaque strokes — hairlines are 1 px alpha
blacks/whites (`separatorColor` rgba(0,0,0,0.1)) and control borders are
rgba(0,0,0,0.05–0.12). Never use a gray > #E0E0E0 as a stroke in light mode.

Shadows:
* Active window: `0 12pt 40pt rgba(0,0,0,0.45)` + `0 0 1px rgba(0,0,0,0.5)`
  (1 px dark rim = window border). Inactive: `0 6pt 20pt rgba(0,0,0,0.25)`.
* Push button / capsule toolbar button: `0 0.5pt 1pt rgba(0,0,0,0.12)` + 1 px
  border rgba(0,0,0,0.06). Toggle knob: `0 1pt 2pt rgba(0,0,0,0.25)`.
* Menus/popovers: `0 6pt 20pt rgba(0,0,0,0.25)`.
* Nothing else in the window carries a shadow (groups, sidebar, toolbar flat).

---

## 8. Spacing rhythm (1× pt)

Base unit 4; recurring values 8 / 10 / 12 / 20.

| Metric | Value |
|---|---|
| Toolbar height | 52 (titlebar-only windows 28) |
| Traffic lights | 12 ⌀, 20 pitch, x = 13 from window edge, centered in toolbar |
| Sidebar width | SS 215 · Finder 180–200 (min ~130) |
| Sidebar outer padding | 10 left/right, 8 top below search |
| Sidebar row | 28 tall, 31 pitch (SS) / 28 pitch (Finder, no gap) |
| Sidebar icon | 16 symbol (Finder) / 20 badge (SS); x = 20; label gap 8–10 |
| Sidebar section header | 11 pt bold, 24 tall, 12 above, 4 below |
| Sidebar search field | 28 tall, inset 8, radius 6 |
| Content padding | 20 horizontal, 10 top, 20 bottom |
| Group gap | 10 vertical; 24 above a captioned group, 6 caption→group |
| Group row | 36 tall (40 with app-icon badge); label/control inset 10 |
| Row separator | 1 px, inset 10 from left, 0 from right |
| Controls | button 22 tall, toggle 26×15, checkbox 14, pop-up 22 tall |
| Finder view switcher | 4 × 32×24 segments; toolbar item spacing 8 |
| Finder path/status bar | 22 tall each |
| List view | header 24, rows 20 (small) – 22 |

---

## 9. State summary

| State | Sidebar selection | Text | Traffic lights | Toolbar icons |
|---|---|---|---|---|
| Active, sidebar focused | accent (#0063E1 / #0058D0), white label | full | colored | label @ 0.8 |
| Active, content focused | #DCDCDC / #464646, labelColor | full | colored | label @ 0.8 |
| Inactive window | #DCDCDC / #464646 | title & sidebar labels drop to secondaryLabel (see `hig-window-states@2x.png`: everything reads ~#8A8A8A) | gray #D9D9D9 | label @ 0.35 |
| Disabled control | — | tertiaryLabel | — | — |

---

## 10. Implementation checklist for nitro

1. Window: 10 pt radius, 1 px rim rgba(0,0,0,0.5)@light-desktop, big soft shadow.
2. Two columns; **no** toolbar strip drawn across the sidebar column. Draw the
   sidebar first (full height), then the content column with its own 52 pt
   toolbar region using the content background.
3. Sidebar: material/blur if available, else #EDEDED / #252525. Rows 28 pt,
   10 pt inset selection rect, radius 5, accent when focused else #DCDCDC.
   Section headers 11 pt bold secondary.
4. System Settings content: white/#262626, 20 pt gutters, groups #F7F7F7/#333333
   radius 8, rows 36 pt, 13 pt labels, controls right at 10 pt, 1 px
   rgba(0,0,0,0.05) separators inset 10 pt.
5. Finder content: white/#1E1E1E; toolbar with back/fwd, 15 pt bold title,
   segmented view switcher, borderless 17 pt symbol buttons; optional 22 pt
   path + status bars with hairline tops.
6. All strokes are alpha hairlines; all text is alpha-black/white so it works
   on any of the surfaces above.
