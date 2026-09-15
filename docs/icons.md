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

47 icons, in six groups:

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
the eye, the word is for everything else. It is also the fallback:
without `caps::ICONS` an icon button paints that label instead, which is
why the text is not optional. A button that emitted its icon regardless
would send `CreateNode { kind: Icon }` to a server that rejects the kind
as a decode error — so the bar would have *killed itself* against an
older server rather than showing a plain button. Both `measure` and
`paint` therefore ask the capability, and they have to give the same
answer: an icon box is square and a label box is not.

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

## Measured on the box

Test box (Pentium G3240, HDMI-A-1 1920×1080), bar and settings open, my
build of server *and* every client (the wire changed, so a mixed tree
would be a protocol mismatch rather than a test).

**Crispness, settled on pixels.** An icon crop always contains ink, so
"I can see a gear" proves nothing. The discriminating control is a **2×
nearest-neighbour upscale of the scale-1 crop** — literally what the
server would put on screen if it blitted a scaled tile — compared against
the scale-2 crop. Same icon, same colour, same position, one variable:

| | 16 device px | a 2× NN blit of it | 32 device px (scale 2) |
|---|---|---|---|
| bar `list`: edge px | 12 | **48** (= 4 × 12, by construction) | **12** |
| bar `list`: distinct colours | 3 | 3 | **4** |
| settings `display`: edge px | 30 | **120** | **50** |
| settings `display`: distinct colours | 14 | 14 | **27** |

Neither number matches the blit, and the second is the one that closes
the case: a nearest-neighbour upscale **cannot invent a colour its source
lacks**. The scale-1 `list` tile's entire palette is
`{7b7b7f, cacace, e4e4e8}`; at scale 2 the box shows `#121216` — the true
`button_text` — plus `4f4f53` and `515155`. Three colours that are not in
the 1× tile at all, so it cannot be a scaled 1× tile.

`icon_bytes` corroborates from the other side: 7 × 16² = 1 792 at scale
1, and 5 888 at scale 2, i.e. exactly +4 × 32² for the four masks
re-rasterised at the device size.

**The scheme flip.** `theme.scheme = dark` into the watched file, nothing
restarted (`config_reloads` checked to have advanced *first*, so this is
not a measurement of a switch that never happened):

| | measured |
|---|---|
| icon pixels changed | **256/256** in both the bar's and settings' 16×16 boxes |
| whole screen | 100.0 % (230 352 of 230 400 sampled px) |
| **`icon_renders`** | **11 → 11 — unmoved** |
| `icons_cached` | 11 → 11 |
| frames across the switch | 7 |

`icon_renders` not moving *is* the coverage-not-tinted-pixels decision:
every icon on screen changed colour and the server rasterised nothing.

**Idle**, 45 s windows inside one minute, with @3700's app-absent control:

| arm | frames | bar CPU ticks | `icon_renders` |
|---|---|---|---|
| bar + settings open | **2** | 0 | 11 → 11 |
| settings killed (control) | **2** | 0 | 11 → 11 |

Identical in both arms — the 2 frames are the bar's 30 s sensor poll, not
the icons. My first pass read 4 in one arm, and the fault was my own
instrument: the start gate allowed a `:32` start, and `:32 + 45 s`
crosses the minute boundary and picks up the clock tick. Gating on `:02`
only puts both arms inside one minute and the difference vanishes.

**Remote**, the point of by-name: `nitro-settings` running on the dev box
over `ssh -L` to `remote.listen = 127.0.0.1:7712`, rendering on the
box's screen. Its `display` heading icon is **256/256 pixels identical**
to the local one — the same artwork, from the same server, because what
crossed the link was the string `"display"` and not a pixel. An `Image`
could not have made the trip at all.

**Sizes and memory.** `docs/budget.md` has the tables; the two numbers
worth repeating here are that the server's +268 KB is **84 % zeno's
rasteriser** (measured by rebuilding with the table cut to one icon) and
only 16 % artwork at **935 B/icon**, and that the cache is **1 792
bytes** for the seven icons on screen — 0.17 % of the glyph atlas beside
it.

## Application icons

Everything above is artwork the server **owns**. An application icon is
artwork it does not: `firefox` is a PNG the distribution installed under
`/usr/share/icons`, and the only reason the server touches it at all is
that it is the process that knows the output scale and the one that has
to blit the pixels.

```rust,ignore
icon("firefox").coloured().size(24.0).fallback("window")
```

Same message, same node kind, no new op: `SetIcon` with `role = 0xff`
(`AS_COLOURED`). The door M4-G left open, walked through.

### The role byte is the selector, not a search order

A palette role means the **symbolic** set compiled into the server.
`AS_COLOURED` means the machine's **icon theme**. Nothing falls back from
one to the other, in either direction.

The obvious alternative — one namespace, symbolic first — was rejected,
and the reason is worth stating because it looks like the friendlier
design. It makes the meaning of `icon("list")` depend on what the box
has installed: today no theme on either of our machines ships a `list`,
so the desktop's own glyph wins; the day one does, the shadowing is the
only thing between the menu button and somebody else's artwork, and
nothing anywhere would say so. Shadowing is invisible by construction.
With the role deciding, the call site says which set it means, there is
no collision to resolve, and `icon("list")` is the same eleven pixels on
every machine.

The corollary is a combination that is **refused** rather than
implemented: a palette role with a theme-only name earns `BadIcon`. It
would have been easy to find the file and tint its alpha — "a monochrome
theme icon used symbolically" — but a theme icon is a picture, not a
coverage mask. Tinting one throws the artwork away and keeps the
silhouette, which is a rendering bug on every icon that is not already
monochrome, and the client cannot know which those are.
`a_symbolic_name_is_not_an_application_name_and_the_reverse` asserts both
directions through the real server.

### The lookup: the XDG spec, the honest subset

`crates/nitro-server/src/icon_theme.rs`, and it is filesystem and parsing
only — it returns a path and decodes nothing.

**Search path**, in order: `$XDG_DATA_HOME/icons`, `~/.icons`, each
`$XDG_DATA_DIRS` entry plus `/icons` (default
`/usr/local/share:/usr/share`), and the flat, unthemed
`/usr/share/pixmaps` **last**. `NITRO_ICON_PATH` replaces the whole list,
`/usr/share/pixmaps` included — a partial override is not a fixture,
because whatever the box has installed still leaks into the answer.

**Theme chain**: `theme.icons` from `server.conf` (default `hicolor`,
see `docs/settings.md`), then its `Inherits=` transitively, breadth-first,
each theme once, with **`hicolor` always last** whether or not anything
named it. Cycles are guarded and the chain is capped at 16, because an
`index.theme` is a file any package may drop.

**Size matching** is the spec's: each theme's `index.theme` gives
`Directories`, and each directory group its `Size`, `Scale`, `Type`
(`Fixed`/`Scalable`/`Threshold`), `MinSize`, `MaxSize` and `Threshold`.
An exact match at the requested **scale** wins; failing that, the
smallest size distance, tie-broken towards the requested scale and then
towards the **larger** source, because downscaling a 48 into a 24 is
sharper than doubling a 16. Failing *that*, a bounded scan of the theme's
directories and then the flat ones, which is what makes a lone
`/usr/share/pixmaps/foo.png` work.

One deliberate deviation: the exact-match pass runs over the **whole
chain** before the closest-size pass, where the spec exhausts both passes
per theme before recursing to the parent. The difference shows up when a
child theme ships one odd size of an icon and `hicolor` ships the
requested one — the spec takes the child's mis-sized file, we take
hicolor's exact one. A display server resamples every icon it draws, so a
correctly sized file from the fallback theme is visibly better than a
resampled one from the preferred theme, and the preferred theme still
wins for every icon it ships at a normal size, which is all of them.

**PNG only.** `.svg` and `.xpm` are skipped. That is not laziness about
formats, it is the #3711 survey's finding: 14 of 20 Adwaita application
SVGs need gradients, which `nitro-raster` does not do, and rasterising
them properly is a renderer rather than a feature. Half an SVG renderer
shows up as *silently wrong artwork* in a user's launcher, which is worse
than a missing icon — and a missing icon already has a fallback. The
survey's other finding is why this is liveable: Adwaita is SVG-only on
both our boxes, `/usr/share/icons` holds 67 PNGs on the dev box and 13 on
the test box, and what actually exists in practice is `hicolor` PNGs
installed by the applications themselves — which is exactly what
`theme.icons = hicolor` finds.

An **absolute path** in the name is honoured, as the desktop-entry spec
allows in `Icon=`, when it ends in `.png` and names a readable file. A
name containing `/`, `..` or a NUL is refused outright.

### The cache: pixels, and an LRU

Two differences from the symbolic cache, both forced.

**It holds a BGRA tile, not coverage.** A coloured icon has no tint to
resolve, so there is nothing to keep out of the key. The entry is the
tile the blitter takes, already resampled to the device size, so the
per-frame path is an integer-aligned one-to-one copy. A scheme flip does
not touch it, which is correct: a Firefox logo is not part of the
palette.

**It evicts.** The symbolic set is *closed*, which is what lets that
cache refuse rather than evict; the set of applications on a machine is
not. `IconEngine::APP_MAX_BYTES` is 4 MiB with LRU behind it — 64 tiles
of 128² or about a thousand at 32², where a launcher showing twenty 24 px
rows and a bar showing ten 16 px buttons costs 41 KB. "Refuse the next
one" would mean a launcher whose last rows are blank for the rest of the
session.

**The decode is lazy and happens once.** Resolution (walking the theme,
`stat`) happens at *commit* time, because that is the call that decides
whether the client earns a `BadIcon` and therefore whether its
`.fallback(…)` fires inside the same interaction. Reading and decoding
the file happens on the **first paint** that needs it, once per `(name,
device px)`: it costs milliseconds, and it depends on a device size the
commit does not know yet. A 256 px PNG is ~2 ms on the Pentium (#3711),
which is a frame — per-frame would be a bug, and per-commit would put it
on the client's first-paint latency.

A decode failure is a `BadIcon`, one line in the log, and a `Missing`
mark so the *next* frame does not try again. A missing icon is the common
case on a thin theme, and a launcher redrawing forty rows must not walk
the search path forty times a frame.

`stats` gains `app_icons_cached`, `app_icon_bytes`, `app_icon_loads`,
`app_icon_misses`, `app_icon_evictions` and `app_icon_decode_us_max`.
`app_icon_loads` is the interesting one, the way `icon_renders` is for
the symbolic half: on a settled desktop it must stop growing.

### Resampling, and why it is not the raster's blitter

The tile is built by `square_tile`/`resample` in `icons.rs` rather than
by `Canvas::blit`, and that is not duplication. A `Canvas` is a *screen*:
its pixels are `XRGB8888` and its blitter writes a zero into every fourth
byte, because a framebuffer has no alpha to keep. Running an icon through
it produces a **fully transparent tile** — which is what the first
version of this code did, and what
`an_application_icon_is_decoded_once_and_blitted_in_its_own_colours`
caught. An icon tile is a source image, not a destination.

Within that function: the mix is **premultiplied**, because averaging
straight-alpha samples drags a transparent pixel's colour into its
neighbour and every icon drawn on transparent black — which is most of
them — comes back with a dark halo. Downscaling takes the **area
average** of every source pixel the destination covers (48 → 16 is nine
samples; a 4-tap bilinear would miss five ninths of the artwork, which is
how thin strokes vanish); upscaling is bilinear, because there is nothing
to average and nearest-neighbour is the blocky doubled tile this document
spends a section arguing against. A non-square source is **letterboxed**,
not stretched.

### The consumers

**`nitro-launcher`** parses `Icon=` (`desktop.rs`, `Entry::icon`) and
puts a 24 px icon in front of every row, falling back to `window`. A
`.desktop` entry's icon is coloured (it names the theme); a **built-in**
entry's is symbolic, because a built-in exists precisely on the box with
no icon theme installed, so its icon has to come from the set compiled
into the server.

The idle and latency contracts are untouched, and the reason is
structural rather than measured: `SetIcon` is one-way, so a row costs no
round trip, and the decode is the server's and lazy. The launcher's
first-paint measurement with 40 entries is in `docs/latency.md`.

**`nitro-bar`**'s window list uses a window's `app_id` **directly as an
icon name**, falling back to `window`. That is the freedesktop
convention — an application's `.desktop` file is usually named after its
app id and its `Icon=` usually matches — and it is right often enough to
be worth one string. It is also honestly limited: an application whose
app id and icon name differ (`org.gnome.Nautilus` vs `nautilus`) gets the
fallback.

The alternative was to have the *server* resolve `app_id` → `.desktop` →
`Icon=`, which is strictly better and strictly bigger: it puts a
`.desktop` index, its search path and its invalidation into the
compositor, for a case the convention already covers. Rule (a) is what
shipped; the note is here so the next person knows what they are
choosing between. Our own applications keep the convention: the
`.desktop` files under `deploy/` are named after their app ids.

### Measured on the box

Test box (Pentium G3240, HDMI-A-1 1920×1080), my build of server *and*
every client, the human's own `server.conf` restored afterwards.

**What the box actually has**, because that is the finding the whole
design has to survive: `/usr/share/icons` holds **17 PNGs**, of which the
application ones are `foot`, `gvim`, `apport` and `org.freedesktop.fwupd`
(plus `htop` in `/usr/share/pixmaps`). Adwaita is not installed at all.
So `hicolor` PNGs installed by applications really are what exists, the
symbolic fallback really is the usual answer, and no fixture had to be
installed to take the screenshot below — the icons in it are the box's.

**The launcher**, opened on the bare Super tap, 20 entries:

| row | icon it asked for | what was drawn |
|---|---|---|
| Foot | `foot`, coloured | the theme's PNG — 23 non-grey px in the icon box |
| Htop | `htop`, coloured | the theme's PNG — 343 non-grey px |
| Calculator, Files, Settings, Terminal | `calculator`, `folder-fill`, `gear`, `terminal`, symbolic | tinted, 0 non-grey px |
| Vim | `gvim`, coloured | fell back — `gvim` is only under `locolor` |

"Non-grey" (max channel − min channel > 24) is the discriminator rather
than "there is ink": a symbolic icon is tinted from the palette and so is
grey by construction, while a real application icon cannot be. Foot's
cream `(249, 239, 198)` and Htop's green `(110, 193, 112)` are colours no
palette role in either scheme contains.

**Lazy decode, from the outside.** `app_icon_loads` was **0** until the
launcher was first shown — the rows existed, the names were resolved, and
not a byte had been read. On the first paint it went to 5: the icons that
resolved, not the 20 rows. Scrolling the list to rows the first paint did
not reach left it at 5, because those rows' names do not resolve; every
row that *does* resolve costs exactly one decode, ever.

**Scale 2, and the 48 px source.** `theme.icons` untouched,
`output.HDMI-A-1.scale = 2` written into the watched file and `reload`ed,
nothing restarted:

| | `app_icons_cached` | `app_icon_bytes` |
|---|---|---|
| scale 1 | 2 | **4 608** = 2 × 24² × 4 |
| scale 2 | 4 | **23 040** = 4 608 + 2 × 48² × 4 |

The delta is **+18 432 bytes, exactly two 48×48 tiles**. A 24-logical
icon on a 2× output is 48 device px, and the cache grew by precisely the
two new tiles at that size rather than by a scaled copy of the old ones —
which also shows the 24 px entries were kept, since an output can change
scale back. That is the arithmetic proof the task asked for, and it is
stronger than a crop: a doubled 24 px tile would occupy the same 9 216
bytes each, so bytes alone would not discriminate — but the *theme* has
no 24 px `foot`, so the scale-1 tile is itself a downscale of the 48 and
the scale-2 one is the file untouched.

**Idle**, 45 s windows gated to start at `:02` so neither crosses a
minute boundary (the instrument error `docs/icons.md` already records),
launcher closed, with an `NITRO_ICON_PATH=/tmp/no-such-icons` control arm
that makes every application icon unresolvable:

| arm | `app_icon_loads` | frames / 45 s | `icon_renders` | bar CPU ticks |
|---|---|---|---|---|
| icons resolve | 5 | **2** | 7 → 7 | 0 |
| control, nothing resolves | 0 | **2** | 7 → 7 | 0 |

Identical, and identical to icons-A's figure: the 2 frames are the bar's
30 s sensor poll. Application icons add nothing to idle, which is the
claim — `SetIcon` is one-way, the decode is the server's and already
done, and a cached tile is an integer-aligned blit.

**The bar's window list**, with calc, settings and a terminal open: all
three show the symbolic `window`, because our app ids (`nitro-calc`,
`nitro-settings`, `nitro-term`) are in no icon theme on the box. That is
rule (a) working as documented rather than failing — the fallback is what
keeps the list readable — and it is exactly what the `.desktop` files
under `deploy/` would fix on a box where they were installed.

Sizes and memory are in `docs/budget.md`; the two numbers worth repeating
here are that the server grew **+97 368 B (+3.9 %)**, a third of what the
symbolic set cost, and that the largest icon actually loaded decoded in
**214–510 µs**.

### Still deferred

* **SVG application icons**, and the gradients they need.
* **`.desktop`-based resolution for the bar**, above.
* **Per-icon user overrides** — pinning one name to one file. That is a
  desktop-settings feature, not a path resolver's.
* **`Context=`, localized theme names, `.icon` metadata.** All of it
  exists for an icon *chooser*; we are given a name and asked for a file.

What is *not* deferred and not planned: client-supplied icon pixels.
An app that needs arbitrary artwork has `Image`, and pays the buffer for
it.
