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
db.next_frame();
db.release_idle(); // when the loop is about to block: hands the bytes back
```

## Font discovery

`FontDb::scan()` walks, recursively and in one pass:

* the colon-separated directories in **`NITRO_FONT_DIRS`** when it is set, else
* `/usr/share/fonts`, `/usr/local/share/fonts` and `~/.local/share/fonts`.

`FontDb::scan_dirs(&dirs)` takes an explicit list and does **not** touch the
index cache; that is what the tests use, so a scan of a temp directory can
never write over the index the user's own session built.
`FontDb::scan_dirs_with_cache(&dirs, Some(path))` opts in.
Recursion stops at depth 8 so a symlink loop cannot hang the scan.

Files with a `.ttf`, `.otf`, `.ttc` or `.otc` extension (case-insensitive) are
read once at scan time and handed to swash's `FontDataRef`; every face in the
file is indexed separately, so a `.ttc` contributes several. A face's family
name comes from the `name` table — `StringId::TypographicFamily` when present,
else `StringId::Family` — its weight, width and slant from
`FontRef::attributes()` and its monospace flag from `post.isFixedPitch`. The
index key is the lowercased family name. **The bytes are dropped again the
moment the face is indexed**: what is retained is the family, the attributes
and the `(path, index-in-collection)` pair. See *Memory* below.

**There is no fontconfig**, no `~/.fonts.conf` parsing and no user font
aliasing. That is a deliberate dependency decision, not an oversight: the three
generic aliases below cover what a UI toolkit actually asks for, and fontconfig
is a C library with a configuration language attached. There *is* a cache file,
but it is ours and it holds only the index above — see *The index cache*.

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

Nearest `(slant, width, weight)`, scored as

```text
distance = 1_000_000 * (face.italic != want.italic)
         +     2_000 * |face.stretch - 100|
         +     1_000 * wrong_side
         + |face.weight - want.weight|
```

where `stretch` is swash's raw width value (100 = normal, one unit = half a
percent of the normal aspect ratio) and `wrong_side` is
`face.weight < want.weight` when the request is ≥ 400 and
`face.weight > want.weight` when it is < 400. That is:

1. **Slant dominates.** An upright face is never chosen over an italic one when
   italic was asked for, whatever the weights.
2. **Then width.** Nothing on the wire asks for a condensed face, so the face
   closest to normal width wins; the 2000 penalty is larger than any possible
   weight distance (≤ 1900), so a normal-width face always beats a condensed
   one. Without this a family shipping "Condensed" under the same name —
   several Noto families do — could win on weight alone and narrow the UI.
3. **Then the CSS-ish nearest-weight rule.** For a request of 400 or more,
   heavier faces are preferred over lighter ones at equal distance; below 400,
   lighter ones are. The 1000 penalty is larger than any possible weight delta
   (≤ 900), so the preferred side always wins.
4. **Then the closest weight.**

Ties go to the face discovered first (files are scanned in sorted path order,
so this too is deterministic).

A family counts as monospace when its *name* says so **or** any of its faces
sets `post.isFixedPitch` — that is what lets `Family::Mono` find a face called
"Terminus" or "Fixed", which the name test alone never would. Serif is still a
name test (`serif` and not `sans`), because no font table records it.

There is **no synthetic bold or oblique**: if a family has no italic face, the
upright one is used as-is. Synthesis is a rendering decision that belongs with
the scaler and is not in M2.

### Memory

`FontDb` holds **no font bytes at all** until a face is used, and hands them
back when it stops being used. This is the fix for issue #528, where the scan's
old "read everything and keep it" behaviour put the server 2.5× over its RSS
budget on the test box (47 faces, 12.6 MB of font files, of which a desktop
draws with two or three).

Three mechanisms, in the order they matter:

1. **The index holds no bytes.** `scan` reads each file, takes the family name,
   the attributes and the monospace flag, and drops the bytes. What is retained
   per face is a `(file index, face index)` pair and two short strings.
2. **`FontDb::face(id)` loads on first use**, caching the file's bytes in an
   LRU capped by **`NITRO_FONT_CACHE_MB`** (default 8). Cloning the returned
   `FaceData` is an `Arc` bump, so several faces of one `.ttc` share the bytes
   and an eviction can never pull the rug out from under a shaper mid-call.
3. **`FontDb::release_idle()` returns the bytes when nothing needs them.** The
   server calls it each time its event loop is about to block. This, not the
   cap, is what keeps the steady state small: on a box whose fonts fit inside
   8 MB the cap never fires at all, and a desktop that has drawn its labels
   would otherwise hold those files for the session.

Eviction is safe because **the atlas keeps the rendered masks**. A face is
needed to *shape* a run and to *rasterize a glyph the atlas has not seen*;
neither happens again once a label is on screen, so a released face costs one
`read(2)` the next time a genuinely new glyph appears — measured at 20 µs to
shape a warm line against 53 µs when the face has to be read back first, 0.2 %
of a 16 ms frame. No glyph is ever re-rendered and nothing on screen changes.

What the server reports: `fonts` (faces indexed), `fonts_loaded` (font files
resident *now*) and `font_bytes` (their total size). On the test box with text
on screen those are `47 / 0 / 0` in the settled state and `47 / 3 / 1.9 MB`
mid-paint. `FontDb::loads()`, `FontDb::releases()` and `FontDb::evictions()`
are the counters behind them: loads is the miss counter, releases counts
files handed back by the idle sweep — rising steadily is the normal rhythm of
a desktop that paints now and then — and evictions counts files the *cap*
dropped mid-frame. A non-zero `evictions` means the working set of a single
frame genuinely did not fit in `NITRO_FONT_CACHE_MB`, a distinct and more
alarming fact than an ordinary idle release.

A shape only loads the faces it needs: the fallback chain is a list of *ids*,
and the faces after the primary are read only when the primary cannot map a
character, stopping as soon as the remainder is covered. Latin text in the UI
font therefore touches exactly one file however long the chain is.

### The index cache

The scan still has to *walk* the directories and, on a cold cache, read every
font file to build the index. That second part is written to
**`$XDG_CACHE_HOME/nitro/fonts.idx`** (else `~/.cache/nitro/fonts.idx`;
`NITRO_FONT_INDEX_CACHE` overrides the path, or disables it with `0`/`off`).
The format is hand-written — a magic, a version, length-prefixed strings; no
serde, no new dependency.

It is validated against the walk that just happened: the same directories with
the same mtimes and file counts, and the same font files with the same paths,
**sizes and mtimes**. Any mismatch, any parse failure, any truncation → the
cache is discarded and the scan runs for real, then rewrites it. A font
installed, removed or replaced in place between boots is therefore picked up;
a stale index can never make the server shape with a face that is not there.
The write is write-then-rename, so a crash mid-write leaves the old file.

On the test box: **5.2 ms cold, 0.4 ms warm**, 47 faces, a 5 520-byte cache
file. On the dev machine: 3.5 ms cold, 36 µs warm for 8 faces. The scan was
already fast enough to be optional by the spec's own rule; it is in because it
is 20 lines of format code and takes the one remaining per-boot read of 12 MB
off the critical path.

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
- **Fonts are not hot-reloaded.** The index is built once; a font installed
  while the server runs is picked up at the next restart. The *file* is re-read
  lazily, so a font edited in place under a running server can be picked up
  mid-session — an accident of the design, not a feature: the index still
  describes the old file, and a face count or family name that changed is
  simply not seen until a restart.
- **The face cache is not shared between processes.** Two servers on one box
  each read their own copy; they do share the index cache file, which is what
  it is for. `mmap` would fix both and is the obvious next step if a box ever
  wants a 100 MB CJK font resident.

## Tests

`cargo test -p nitro-text` — unit tests per module plus `tests/text.rs` against
the box's real fonts. Every test that needs glyphs calls a `db()` helper first;
when `FontDb::scan()` comes back empty it prints why and returns, so a CI image
without fonts stays green instead of failing on something it cannot have. The
empty-db path is covered explicitly by scanning an empty temp directory:
`select` returns `None`, and `shape`/`measure` return an empty result without
panicking.

The lazy-loading behaviour has four tests of its own, each copying real font
files into a temp directory so it owns what it scans: a scan holds zero bytes
until the first shape and then exactly one file; a 1-byte cap evicts while the
atlas keeps every mask and re-asking costs neither a render nor a read; the
index cache round-trips and is discarded when a font is added or the file is
clobbered; and an idle release hands the bytes back with the masks intact.

`cargo test -p nitro-text --test text -- --nocapture timings` prints the scan
time and whether it hit the index cache, the per-call shape time for a
200-character paragraph, and the atlas's first-pass cost.

## Measurements (dev box, AMD EPYC 7B13, release; box = Pentium G3240)

| | |
|---|---|
| `FontDb::scan()`, cold index cache | **3.5 ms** dev (8 faces), **5.2 ms** box (47 faces) |
| `FontDb::scan()`, warm index cache | **36 µs** dev, **0.4 ms** box — no font file is read |
| Font bytes resident after a scan | **0**, whatever is installed |
| Font bytes resident, box, text on screen | 1.9 MB mid-paint, **0** once the loop goes idle |
| `Layout::shape`, 200 chars @ 14 px, wrap 400 px | **37–45 µs** per call, warm caches (~690 µs debug) |
| `Layout::shape`, one line, face already resident | 20 µs |
| `Layout::shape`, one line, face re-read first | 53 µs — the cost of an eviction |
| `Atlas`, first (cold) pass over that paragraph | 2.8 ms, 90 rasterizations, 1 page |
| 3000 distinct atlas keys | 1 page, 3000 cached, 3000 renders |

The cold scan is dominated by reading every font file to build the index, so it
varies with the page cache; the warm one only walks the directories and `stat`s
what it finds. Both are once-per-boot costs the server logs, with which path it
took.

The shape number is the one that matters for the frame budget: a typical UI
frame re-shapes nothing (the `TextStore` holds it) and at worst re-shapes one
label, which is a few µs — or a few tens, if the face has to come back off
disk first.
