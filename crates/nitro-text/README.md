# `nitro-text`

Server-side text: font discovery, shaping and layout, measurement, and an A8
glyph atlas.

Clients send **strings and a style** over the wire; the server shapes them and
rasterizes the glyphs. That is what keeps the remote link thin — no pixels and
no font files cross it — and it is why this crate sits on the server side of
`nitro-wire`. One dependency, `swash`; no `nitro-core`, no `nitro-scene`, no
`nitro-wire`, no fontconfig, no logger, no `unsafe`.

```rust
use nitro_text::{Atlas, FontDb, GlyphKey, Layout, TextStyle};

let db = FontDb::scan();                 // once, at startup
let mut layout = Layout::new();          // one per server: owns the caches
let mut atlas = Atlas::new();

let style = TextStyle::default();        // sans, 14 px, weight 400, upright
let text = layout.shape(&db, "Hello, nitro", &style, Some(300.0), true);

for line in &text.lines {
    for glyph in &line.glyphs {
        let key = GlyphKey::new(glyph.font, glyph.id, text.size_px, glyph.x);
        if let Some(mask) = atlas.get(&db, key) {
            // blit mask.w × mask.h bytes from atlas.page(mask.page) at
            // (glyph.x.floor() + mask.left, line.baseline + glyph.y - mask.top)
        }
    }
}
atlas.next_frame();
```

## Font discovery

`FontDb::scan()` walks, recursively and in one pass:

* the colon-separated directories in **`NITRO_FONT_DIRS`** when it is set, else
* `/usr/share/fonts`, `/usr/local/share/fonts` and `~/.local/share/fonts`.

`FontDb::scan_dirs(&dirs)` takes an explicit list; that is what the tests use.
Recursion stops at depth 8 so a symlink loop cannot hang the scan.

Files with a `.ttf`, `.otf`, `.ttc` or `.otc` extension (case-insensitive) are
read whole and handed to swash's `FontDataRef`; every face in the file is
indexed separately, so a `.ttc` contributes several. A face's family name comes
from the `name` table — `StringId::TypographicFamily` when present, else
`StringId::Family` — and its weight and slant from `FontRef::attributes()`. The
index key is the lowercased family name.

**There is no fontconfig**, no cache file, no `~/.fonts.conf` parsing and no
user font aliasing. That is a deliberate dependency decision, not an oversight:
the three generic aliases below cover what a UI toolkit actually asks for, and
fontconfig is a C library with a configuration language attached.

**Nothing is logged.** A directory that cannot be read, a file that is not a
font and a face without a usable family name are all skipped silently, because
a broken font on the box must not stop the server booting. The server logs
`FontDb::len()` (faces indexed) and `FontDb::scan_time()` (wall time of the
scan) itself.

### Alias tables

`Family::parse` maps `"sans"`/`"sans-serif"` → `Sans`, `"mono"`/`"monospace"` →
`Mono`, `"serif"` → `Serif`, and anything else to `Named(String)` (matched
case-insensitively; an unknown name falls back to `Sans`, like a browser). Each
generic resolves against a preference list, **first match wins**:

| alias | preference list | then |
|---|---|---|
| `Sans` | Noto Sans, DejaVu Sans, Cantarell, Liberation Sans, Droid Sans | any family whose name contains neither `mono` nor `serif` |
| `Mono` | Noto Sans Mono, DejaVu Sans Mono, Liberation Mono, Droid Sans Mono | any family whose name contains `mono` |
| `Serif` | Noto Serif, DejaVu Serif, Liberation Serif, Droid Serif | any family whose name contains `serif` but not `sans` |

"Then" picks the alphabetically first matching family, so the choice is
deterministic across boxes with the same fonts. If even that finds nothing —
a db holding only, say, one monospace family when `Serif` was asked for — the
alphabetically first family in the db is used rather than returning `None`:
text in the wrong face beats no text at all. `select` returns `None` only when
the db is genuinely empty.

`fallbacks(&style)` returns `select` first, then one face from every *other*
family in the same alias list, alias order first and then the class-matching
families in name order. `Layout` walks this chain per run when the selected
face maps a character to glyph 0.

### Face selection within a family

Nearest `(weight, italic)`, scored as

```text
distance = 1_000_000 * (face.italic != want.italic)
         +     1_000 * wrong_side
         + |face.weight - want.weight|
```

where `wrong_side` is `face.weight < want.weight` when the request is ≥ 400 and
`face.weight > want.weight` when it is < 400. That is:

1. **Slant dominates.** An upright face is never chosen over an italic one when
   italic was asked for, whatever the weights.
2. **Then the CSS-ish nearest-weight rule.** For a request of 400 or more,
   heavier faces are preferred over lighter ones at equal distance; below 400,
   lighter ones are. The 1000 penalty is larger than any possible weight delta
   (≤ 900), so the preferred side always wins.
3. **Then the closest weight.**

Ties go to the face discovered first (files are scanned in sorted path order,
so this too is deterministic).

There is **no synthetic bold or oblique**: if a family has no italic face, the
upright one is used as-is. Synthesis is a rendering decision that belongs with
the scaler and is not in M2.

### Memory

`FontDb` keeps **the whole byte content of every font file it indexed** for its
lifetime, because both the shaper and the scaler take a `&[u8]` on every call
and re-reading the file per frame is not an option. The cost is the size of the
font files on disk, not the number of faces — a `.ttc` is stored once.

For scale: DejaVu Sans/Serif/Mono in their four styles is ~6 MB; a full Noto
install (which includes CJK) can reach 100 MB+, at which point a real
compositor would want to mmap instead. Point `NITRO_FONT_DIRS` at the handful
of directories the UI actually needs and the number stays in single-digit MB;
the dev box scan is **8 faces, ~3.5 MB, 1.6–4.8 ms**.

## Layout

`Layout::shape(db, text, style, max_width, wrap)` produces a `ShapedText`: a
`Vec<Line>`, each holding positioned `Glyph`s, plus the block's width, height,
ascent and descent.

* **Coordinate space.** Device pixels, y down. A glyph's `x`/`y` are relative
  to its line's start and baseline; a line's `baseline` is relative to the
  block's top and is cumulative. A glyph's pen position in the block is
  therefore `(glyph.x, line.baseline + glyph.y)`.
* **Line box.** `ascent`/`descent` come from the font's `Metrics` scaled to
  `size_px`; a line's height is `ascent + descent + leading`, and
  `ShapedText::height` is the sum over lines. `ShapedText::ascent` is the
  *first* line's ascent — what a caller vertically centres a single-line label
  by. A line that used a fallback face gets the more generous of the two
  ascents and descents, so the box fits both.
* **Line breaks.** `\n` and `\r\n` start a new line, always. Tabs expand to
  four spaces before shaping.
* **Wrapping.** With `wrap == true` and `max_width == Some(w)`, greedy breaking
  at Unicode whitespace: no line is wider than `w`. Trailing whitespace is
  excluded from a line's `width`, so the space a line broke at never pushes it
  over. **Exception:** a single word wider than `w` has no break opportunity
  and is cut at a cluster boundary instead — such a line can exceed `w`. With
  `wrap == false`, `max_width` is ignored.
* **Clusters.** Every glyph's `cluster` is a byte offset **into the original
  `text`**, not into the tab-expanded string: the four spaces of a tab all
  report the tab's own offset, and the first glyph of the second line of
  `"a\nb"` reports 2. Several glyphs can share a cluster (marks, ligatures).
* **Script.** One script for the whole string, taken from the first strong
  character via `swash::text::analyze`, falling back to Latin.

`Layout::measure` runs the same layout and adds `cursor_x`: one `(byte offset,
x)` pair per cluster boundary in increasing byte order, `x` being the pen
position of that cluster **within its line**, plus a final pair for the
end-of-text offset. A hard break's own offset (the `\n`) also gets a pair, at
the end of the line it terminates, so a caret can sit past the last character
of a line; a *soft* (wrap) break needs none, because the next line's first
cluster already carries that offset. Offsets never repeat — a tab's four
expanded spaces contribute one pair, at the tab's offset. The table is
non-decreasing in `x` within a line and resets to 0 at each line start, so a
caller maps a click to a byte offset by picking the line first and then binary
searching `x`.

`Layout` owns swash's `ShapeContext` — its font caches and scratch buffers — so
keep one per server and reuse it; a fresh one per call throws the caches away.
Neither `shape` nor `measure` allocates a page or touches the atlas.

## Atlas

A8 coverage masks, one per `GlyphKey`.

* **Key quantization.** `GlyphKey::new(font, glyph, size_px, x)` stores
  `size_q = round(size_px * 64)` (1/64 px) and `subpx`, the pen's fractional x
  rounded to **quarter pixels** (0, 0.25, 0.5, 0.75). Text sliding horizontally
  therefore reuses four masks per glyph instead of one per position. Blit at
  `x.floor()`; the phase is already baked into the mask.
* **Rendering.** swash `Render` over `&[Source::Bitmap(BestFit),
  Source::Outline]`, `Format::Alpha`, offset by the key's subpixel x. Hinting
  is **on at or below 24 px** (`ScalerBuilder::hint(true)`) where stem snapping
  earns its keep, and off above it where it only distorts.
* **Placement.** `MaskInfo::left`/`top` are swash's `Placement`: the offset
  from the glyph origin (pen position on the baseline) to the mask's top-left
  corner, with `top` measured *upward*. In a y-down space the mask's top edge
  is at `baseline - top`.
* **Packing.** Shelf packing into 1024×1024 pages, 1 px of padding to the right
  and below each entry so a bilinear or off-by-one read cannot pick up the
  neighbour. An entry taller than the current shelf starts a new one; when a
  page cannot fit an entry, a new page is opened. A 14 px Latin face fits its
  whole repertoire × 4 phases in **one page** (measured: 3000 distinct keys →
  1 page).
* **Empty masks.** `get` returns `None` for a glyph with no pixels — a space, a
  control character. The negative result **is** cached (and counts in
  `glyph_count`), so a paragraph full of spaces does not re-scale one glyph per
  space per frame; `renders()` therefore equals the number of distinct keys
  ever asked for. The caller still advances the pen by the glyph's `advance`;
  only the blit is skipped.
* **Memory.** `PAGE * PAGE = 1 MiB` per page, allocated lazily: an atlas that
  has rendered nothing holds zero pages.

### Eviction policy — stated honestly

**The atlas never evicts.** It records an LRU frame stamp per entry, bumped by
`Atlas::next_frame()` and refreshed on every hit, and `get` updates it — but
nothing reads it yet. On a page that cannot fit, a new page is opened.

This is the "eviction only when a page cannot fit" policy taken to its
simplest working point, and it is honest about the trade: a server that shapes
text at hundreds of distinct sizes will grow pages without bound, 1 MiB at a
time. The bound in practice is small — a UI uses a handful of sizes, and one
page covers a Latin face at one size with all four phases — but it *is*
unbounded in theory. The stamp is there so the eviction pass, when it is
written, is a filter over `entries` and a repack, with no API change: `get`,
`MaskInfo` and the page layout stay as they are. Until a profile of a real
session says pages are growing, adding the pass would be untested code on a
hot path.

### Colour glyphs (emoji) — M3

**Not supported, deliberately.** A colour glyph renders through the alpha
outline path: swash's `Source::ColorOutline` and `Source::ColorBitmap` produce
`Content::Color`, four bytes per pixel, which cannot go into an A8 page — so
the atlas asks for outlines and alpha bitmaps only, and rejects anything that
comes back non-`Mask`. A colour emoji font therefore yields either a
monochrome outline (when the face has one) or `None` (when it is bitmap-only,
as Noto Color Emoji is).

A separate `ColorAtlas` holding ARGB pages is the fix, and it is not cheap in
the way the spec hoped: it needs a second page format, a second key space, a
second blit path in `nitro-raster`, and a scene-level decision about whether a
text run can mix mask and colour glyphs. That is an M3 task, and pretending
otherwise here would have meant shipping a half-wired colour path.

## Limitations (M2)

- **LTR only, no bidi.** The whole string is shaped left-to-right. Arabic and
  Hebrew will shape (swash handles the joining) but will be laid out in the
  wrong visual order. `analyze()` can report `needs_bidi_resolution`; acting on
  it needs a bidi algorithm, which is a later decision.
- **One script per string.** Detected from the first strong character. A string
  mixing Latin and Devanagari is shaped entirely with the first one's rules.
- **No rich text.** One `TextStyle` per call. A run of mixed styles is several
  calls, positioned by the caller.
- **Fallback is per run, not per cluster.** A contiguous stretch of characters
  is shaped with the first face in the chain that maps it; shaping state is not
  carried across a fallback boundary, so kerning between the last glyph of one
  run and the first of the next is lost. Whitespace and control characters
  never force a run break. This is enough for "the UI font lacks one symbol",
  which is the M2 case; a proper per-cluster fallback with `CharCluster::map`
  status is the follow-up.
- **No synthetic bold or oblique**, no variable-font axes, no OpenType feature
  settings, no letter/word spacing, no justification, no ellipsizing.
- **No vertical text**, no per-line alignment (the caller offsets by
  `line.width`), no tab stops beyond the flat four-space expansion.
- **The atlas never evicts** (above), and **colour glyphs are M3** (above).

## Tests

`cargo test -p nitro-text` — unit tests per module plus `tests/text.rs` against
the box's real fonts. Every test that needs glyphs calls a `db()` helper first;
when `FontDb::scan()` comes back empty it prints why and returns, so a CI image
without fonts stays green instead of failing on something it cannot have. The
empty-db path is covered explicitly by scanning an empty temp directory:
`select` returns `None`, and `shape`/`measure` return an empty result without
panicking.

`cargo test -p nitro-text --test text -- --nocapture timings` prints the scan
time, the per-call shape time for a 200-character paragraph, and the atlas's
first-pass cost.

## Measurements (dev box, AMD EPYC 7B13, release)

| | |
|---|---|
| `FontDb::scan()` | **1.6–4.8 ms** (varies with the page cache), 8 faces, ~3.5 MB (DejaVu Sans/Serif/Mono in `/usr/share/fonts`) |
| `Layout::shape`, 200 chars @ 14 px, wrap 400 px | **37–45 µs** per call, warm caches (~690 µs debug) |
| `Atlas`, first (cold) pass over that paragraph | 2.8 ms, 90 rasterizations, 1 page |
| 3000 distinct atlas keys | 1 page, 3000 cached, 3000 renders |

The scan is dominated by reading the font files off disk, so it varies with
the page cache; it is a once-per-boot cost the server logs.
The shape number is the one that matters for the frame budget: a typical UI
frame re-shapes nothing (the `TextStore` holds it) and at worst re-shapes one
label, which is a few µs.
