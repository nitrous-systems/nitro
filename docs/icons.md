# Icons: named, not drawn

A nitro client never sends an icon. It sends the **name** of one —
`"gear"`, `"list"`, `"cpu"` — plus a palette role and a size, and the
server rasterises the artwork it owns, at that output's device scale, in
the colour that role currently names.

```rust,ignore
icon("gear").size(16.0).color_role(ColorRole::TextDim)
```

That is one `SetIcon` on the wire: nine bytes and a four-character
string. It is the same bargain `SetText` makes for strings, and it is
made for the same reason — the server is the process that owns fonts,
palettes and output scales, so it is the process that should own
artwork too.

This document is the argument, the set and its licence, how to add an
icon, the cache and its key, the sizes contract, and what is deliberately
deferred.

## Why by name — the three reasons

Each of these is a property that a client-side bitmap cannot have, and
each was measured rather than assumed.

**1. Remote.** An `Image` node is a region of a client buffer, and a
buffer is a file descriptor. A descriptor cannot cross TCP
([`caps::REMOTE`](wire.md#handshake), `docs/remote.md`), so the one thing
a remote app definitively cannot do is put pixels on the screen. A
`SetIcon` carries no descriptor at all, which is why
`a_set_icon_crosses_tcp_unchanged` in `crates/nitro-wire/tests/tcp.rs` is
a test and not a special case: the remote path and the local path are the
same path.

**2. Theme.** The node stores a **role index**, not a colour. The colour
is resolved in `IconEngine::paint`, per frame, against whatever palette
the server is holding — so `theme.scheme = dark` in `server.conf`
recolours every icon on the desktop in the same frame as the text, with
no client message and, because the cache holds coverage rather than
tinted pixels, **no re-rasterisation at all**. That is asserted rather
than claimed:
`flipping_the_scheme_recolours_an_icon_with_no_client_message_and_no_re_raster`
in `crates/nitro-server/tests/icons.rs` checks the pixels moved *and*
that `icon_renders` did not budge.

The corollary is the rule `deploy/lint-colors.sh` enforces: an icon takes
a **role**, never a colour. There is no `.color(Color)` on an
`IconBuilder` to write one down with.

**3. Scale.** The mask is rasterised at `round(size_logical × scale)`
device pixels. The same 16 px icon on a 1× and a 2× output is rasterised
**twice**, exactly as a glyph is, and the two masks are different images
— not one scaled. A client-side bitmap could only ever be a doubled 16 px
tile, which is the blur every toolkit that ships PNGs has.

"It is crisp" is a claim about pixels, so it is settled on pixels: a
nearest-neighbour 2× of a 16 px mask has exactly **four times** its
anti-aliased edge pixels, while a real 32 px raster has fewer — the
artwork's edges are curves whose length grows by 2, not by 4. Both
`a_two_times_output_really_rasterises_at_two_times` (the engine) and
`a_two_times_output_rasterises_a_real_two_times_icon` (through the whole
server, off a real readback) assert that inequality.

## The set

`crates/nitro-icons` holds a curated subset of **Bootstrap Icons**
(MIT, <https://github.com/twbs/icons>), pinned at commit
`6945b7006285d444cc17ff2e22c7691719229526`. The upstream licence text
ships beside the crate as `LICENSE.bootstrap-icons` and is recorded in
`DEPENDENCIES.md`'s vendored-assets section.

47 icons, in five groups:

| group | names |
|---|---|
| shell | `list` `window` `house` `search` `terminal` `calculator` |
| status | `battery` `battery-half` `battery-full` `battery-charging` `volume-up` `volume-down` `volume-mute` `wifi` `wifi-off` `cpu` `memory` `hdd` |
| settings | `display` `keyboard` `speaker` `palette` `sliders` `gear` |
| window controls | `x` `dash` `square` `arrows-angle-expand` |
| files | `folder-fill` `folder2-open` `file-earmark` `file-earmark-text` `file-earmark-image` `file-earmark-zip` `file-earmark-code` `file-earmark-font` `file-earmark-play` `file-earmark-music` |
| general | `sun` `moon` `arrow-left` `arrow-up` `recycle` `check` `circle-fill` `exclamation-triangle` `info-circle` |

**One substitution from the list the task named**, and it is worth
recording because it is a property of the upstream set rather than a
preference. `arrow-clockwise` mixes fill rules between its two paths —
the ring is `evenodd`, the arrowhead is `nonzero` — and the importer
refuses to flatten those into one path, because concatenating them would
silently draw the wrong shape. `arrow-counterclockwise` and
`arrow-repeat` are identically affected (95 of the 2 078 upstream icons
are). `recycle` carries the same "refresh, go round again" meaning in a
single nonzero path and is used instead.

### Adding one

```console
$ bash deploy/icons-import.sh volume-off shield-lock
```

The script appends the names to `crates/nitro-icons/icons.txt`, fetches
every icon in that file from the pinned commit, and regenerates
`crates/nitro-icons/src/set.rs`. With no arguments it simply regenerates
from the list, and it is idempotent: running it twice produces a
byte-identical file. Four things are **hard errors** rather than
warnings, each named with the icon that caused it:

* a name that does not exist upstream;
* a `viewBox` that is not `0 0 16 16`;
* an element the importer does not understand (`<rect>` and `<circle>`
  are converted to equivalent path data; anything else stops the import);
* two paths in one icon that disagree on `fill-rule`.

Bump the pinned sha in the script when upstream is worth following;
`set.rs` records it in its header so a generated file can always be
traced to the commit it came from.

### zeno and the arcs

The rasteriser is [`zeno`](https://docs.rs/zeno), already in the tree via
swash, so the icon set costs **no new external crate**.

Bootstrap's paths use SVG arcs heavily (~22 000 `A`/`a` commands across
the full upstream set), and zeno 0.3.3 *implements* arcs but its parser
mishandles **implicit repeated arc argument sets** — `a rx ry φ f1 f2 x y
rx ry φ f1 f2 x y` with the command letter written once. 13 of our 47
icons are written that way.

The dangerous part is the failure mode: `render_into` reports **no
error**. It silently renders the prefix it managed to parse, so a
mishandled icon is a stray fragment with nothing anywhere saying so.
Waving it through would have shipped broken artwork that no test looking
at return values could see.

So `deploy/icons-import.sh` performs the standard arc → cubic conversion
(SVG F.6.5/F.6.6) at import time and writes **arc-free** path data with
an explicit command letter per segment, which makes the construct
unwritable rather than merely unused. The conversion was checked against
the 33 icons zeno does parse natively: worst per-pixel alpha difference
38/255 at an anti-aliased edge, and **zero** solid-pixel flips at 16, 32,
64 and 128 px.

`every_icon_rasterises_to_something` in `crates/nitro-icons` is the
tripwire that keeps this honest — every entry, at 16 and 32 px, must
produce a non-empty mask.

## The wire

`SetIcon` (0x0208), behind `caps::ICONS` (bit 7), on an `Icon` node
(`NodeKind::Icon`, 6). `docs/wire.md` has the byte layout and the
versioning argument; the two decisions worth repeating here:

**`Icon` is its own node kind, not an `Image`.** An `Image` node *is* a
region of a client buffer — it names a `BufferId`, the server maps a
descriptor for it, and a remote client can never have registered one.
Overloading it would have meant a node whose meaning depended on which of
two setters was last called, and a `SetImage`-then-`SetIcon` sequence
with no defined answer. A separate kind costs one enum variant and makes
the invalid states unrepresentable.

**An unknown name is not fatal.** Every other error in this protocol
closes the connection; `Error { BadIcon }` does not. The node is cleared,
the rest of the commit applies, the client is told, and it keeps running.
A desktop must not lose an application because one of its widgets named
an icon that a newer icon set has — and a client cannot check in advance,
since the set is the server's.

`name = ""` clears the node, and clearing is never an error: it is how a
widget that stopped showing an icon says so without destroying and
re-creating a node.

## The cache

Keyed by `(icon index, device px)` → an **A8 coverage mask**.

* **The index, not the name.** `nitro_icons::index_of` resolves the name
  once, at commit time; the scene stores a `u32`. No string ever crosses
  into `nitro-scene`, and a repaint compares two integers.
* **Coverage, not a tinted tile.** The rasteriser already blends an A8
  mask with a colour — it is how glyphs are drawn — so a symbolic icon
  genuinely *is* a big glyph and gets the same blit, the same clipping
  and the same gamma. It also keeps the colour out of the key, which is
  what makes a scheme flip free.
* **Device px, not logical.** An icon on two outputs at different scales
  is two entries, exactly as a glyph at two sizes is.

**No eviction**, matching the glyph atlas, and for a sharper reason: the
set is *closed*. 47 icons at the four recommended sizes is 47 × (256 +
576 + 1 024 + 2 304) ≈ **190 KB** — the whole set, everywhere, at every
size a desktop lays out at. `IconEngine::MAX_BYTES` is 2 MiB, an order of
magnitude past that, and a raster that would cross it is refused rather
than evicting something: an LRU for a bounded set answers a question that
cannot be asked. `icon_refusals` in `stats` is how a server says that
reasoning was wrong; it has never been above zero.

`stats` reports `icons` (how many the set has), `icons_cached` (distinct
`(name, px)` pairs in use), `icon_renders`, `icon_bytes` and
`icon_refusals`. **`icon_renders` is the interesting one**: after a
settled desktop's first paint it must stop growing, and
`a_settled_desktop_re_rasterises_nothing` asserts exactly that across ten
repaints.

## Sizes

Icons are **square by contract**. One number is both the width and the
height, which is what lets the toolkit's `Icon` widget measure without a
round trip — a `Label` has to ask the server how wide its string is, an
icon never does.

**16, 24, 32 and 48** are the recommended sizes. The artwork is drawn on
a 16-unit grid, so an integer multiple puts every horizontal and vertical
stroke on a whole pixel boundary. Other sizes work and are properly
anti-aliased; they are simply slightly softer. The server clamps a
requested size into `4..=512` device pixels, and a non-finite or
non-positive one becomes 16 — an icon must never be able to kill a
client.

## Consumers

**`nitro-bar`.** The menu button's glyph is `icon("list")`, and the load
and memory readouts get `cpu` and `memory` in front of their numbers,
because `0.4  1.2/3.3G` says nothing about which is which. The `≡` this
replaced was a *character* in a label, so it came from whatever font on
the box happened to have U+2630 — a different weight and optical size
from everything beside it, and nothing at all on a box whose fonts lack
it.

The button keeps its **text** (`"Menu"`) as its accessible name, so
`hey nitro-bar list` and a screen reader are unaffected: the glyph is for
the eye, the word is for everything else.

Battery and wifi deliberately get no icon: the bar has a battery sensor
and no wifi one, and an icon in front of `87%+` would be redundant where
an icon in front of `0.4` is the only thing that makes it readable.
Adding a sensor is a different task.

The icons do not touch the bar's idle contract. They are static, and the
two things that could change them are the server's to act on, so the bar
sends one `SetIcon` per icon on its first paint and none afterwards —
`the_icons_are_painted_once_and_never_again` runs 120 s of sensor ticks
and asserts zero.

**`nitro-settings`.** Each section heading gets its icon at 16 px in a
row with the label, in `ColorRole::Text`: `display`, `keyboard`,
`speaker`, `palette`.

The heading row is deliberately **not** a `control_row`. A `control_row`
is `ROW_HEIGHT` (26 px) tall by contract, and a heading is as tall as its
own text (17.5 px at `HEADING_SIZE`); pinning the headings to 26 would
have added 8.5 px per section, 34 px over four, to a window whose height
is *measured* from its tree. As built, the icon is 16 px and the label's
line box is 17.5, so the row is 17.5 and the tree still occupies
**391.24 px** of a 440 px window — the same figure as before the icons,
which is why `WINDOW_SIZE` did not move.
`the_window_holds_its_tree_with_the_heading_icons` prints that number and
asserts the invariant.

Adding the heading rows did renumber the buttons row's path, which used
to be `window/container[4]`. It is `window/buttons` now: a path built out
of a sibling count changes whenever the tree above it does, which makes
every script that used it quietly wrong rather than loudly broken.

## Deferred to icons-B

Everything to do with **application** icons:

* the XDG icon-theme lookup (`hicolor`, theme inheritance, size
  directories, `index.theme`);
* PNG and the SVG subset those themes are drawn in — this depends on
  task #3711's PNG decoder;
* **full colour.** `SetIcon.role` already carries the sentinel
  `0xff` = `AS_COLOURED`, meaning "paint the icon's own colours, do not
  tint", and `IconRef::AS_COLOURED` carries it through the scene. It is
  accepted and draws nothing today, which is the door left open: the
  message and the cache can carry a full-colour icon without a wire
  change.
* per-icon user overrides, and a `server.conf` icon theme setting.

What is *not* deferred and not planned: client-supplied icon pixels.
An app that needs arbitrary artwork has `Image`, and pays the buffer for
it.
