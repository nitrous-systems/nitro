# Design research: what GNOME and macOS share, and what nitro does about it

Two measured analyses of the layout every current desktop converges on —
**categories / places on the left, content on the right** — and the
column that matters: which of the traits they share nitro implements in
the split-view blueprint (`docs/ui.md`, "Split view blueprint",
`crates/nitro-ui/src/split.rs`), which it approximates, and which it
cannot do and why.

* [`gnome-settings-files.md`](gnome-settings-files.md) — GNOME 45–47
  (libadwaita) Settings and Files: split view, sidebar rows, boxed-list
  cards, exact hex tokens light/dark, radii, spacing.
* [`macos-settings-finder.md`](macos-settings-finder.md) — macOS System
  Settings and Finder: sidebar, form groups, AppKit semantic colours,
  radii, spacing.

The screenshots and SCSS the analyses cite live in the gitignored
`tmp/research/`; the numbers were read off them and are reproduced in
the documents, so the PNGs are not needed in-tree.

## The shared traits, and nitro's answer

| # | trait | GNOME | macOS | in nitro |
|---|---|---|---|---|
| 1 | Two panes, sidebar + content, separated by a 1 px alpha hairline; sidebar a shade *darker* than content in light, *lighter* in dark | 200–230 px | 180–215 pt | **yes** — `split_view()`, 200 px (min 160), `Hairline` separator; `sidebar_background` is pinned darker/lighter by a palette test. The hairline is an opaque role rather than alpha-tinted, which is simpler and testable and reads the same |
| 2 | Sidebar rows 28–36 px, inset 6–10 px, rounded selection rect, 16 px symbolic icon, 8–12 px gap, 13–15 px label; faint hover | 34 px, r9, fg‑10 % neutral, hover 7 % | 28 pt, r5–6, accent when focused / grey otherwise | **yes** — `sidebar_row`: 32 px, inset 6, r8, 16 px icon, 10 px gap, 14 px label, `SidebarHover`/`SidebarSelected` (neutral, GNOME's choice) |
| 3 | Section headers: small bold dim text, or a hairline with 6 px margins; search at the top | hairline; header button | 11 pt bold secondary, Title Case; search field | **yes** — `sidebar_section` (12 px bold `TextDim`), `sidebar_separator` (6 px margins). Search is `sidebar_header_widget(text_field(..))`; no app uses it yet |
| 4 | Per-pane flat header, same bg as the pane, bold ~15 px title, 34 px flat icon buttons, no line under it | 47 px | 52 pt | **yes** — 46 px, bold 15 px title, leading/trailing slots; flat, no line |
| 5 | Content = scrolling column of grouped cards: r8–12, `surface` on window bg, rows 36–54 px, label left / control right / dim subtitle, inset hairlines between rows, bold caption above, dim footnote below; content clamped (GNOME 600 px) or 20 px gutters (macOS) | 600 clamp, r12, barely-there ring | 20 pt gutters, r8–10, shadow | **yes** — `card` (r10, 1 px `Hairline` ring), `card_row` (≥ 40 px, subtitle), `group_caption`, `footnote`, `content_column` (20 px gutters *and* 600 clamp, `.unclamped()` to opt out). The ring stands in for macOS's shadow — see 8 |
| 6 | Typography: body 13–15, captions 11–12 dim (~55 % fg), headings 15 @ 600–700; dim text and hairlines are alpha-tinted fg | | | **yes** for the ladder (per-label `size`/`weight`); **partial** for the alpha: `text_dim` and `hairline` are opaque roles chosen per scheme, which is why they are palette entries rather than a formula |
| 7 | Toggle switch for booleans, accent blue, chevron `›` on navigation rows, values dim on the right | `#3584e4` | `#007aff` | **yes** — `switch` (40×22 pill, `Accent` when on, `checkbox` role), `card_row(..).on_click` adds `chevron-right`, `.value(..)` is a dim trailing label. The accent stays nitro's own `#0f5fbe`/`#6ca8f0`: nudging it would move every screenshot |
| 8 | Radii ladder: buttons/fields 6–9, cards 8–12, sidebar rows 5–9, windows 10–15; shadows nearly absent inside windows | | | **partial** — buttons/fields are the theme's 6, cards 10, sidebar rows 8. **Window radius is the server's 6 px** (`docs/wm.md`), not 10–15: the frame is server-drawn and this is not the place to change it. **No shadows**: `Fill` is `None | Solid | Linear`, the scene has no blur or drop shadow, so cards get a hairline ring |
| 9 | Spacing rhythm 4/6/8/12/24 | 3/6/12/24 | 4-base | **yes** — 6/12/20/24 throughout `split.rs` |
| — | Translucency / vibrancy (macOS sidebar blurs the desktop behind it) | — | yes | **no** — no blur in the scene; the sidebar is opaque `sidebar_background`. Alpha *fills* are possible (`ModalBackground` proves it) but a blur is a compositor feature nitro does not have |

The three "no"s — shadows, vibrancy, window corner radius above 6 — are
all the same fact: they are things the *compositor* draws, and nitro's
scene draws solid and gradient rects, text and icons. Everything a
toolkit can do with those, the blueprint does.
