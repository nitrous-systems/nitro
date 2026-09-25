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

61 icons, in seven groups:

| group | names |
|---|---|
| shell | `list` `window` `house` `search` `terminal` `calculator` |
| status | `battery` `battery-half` `battery-full` `battery-charging` `volume-up` `volume-down` `volume-mute` `wifi` `wifi-off` `cpu` `memory` `hdd` |
| settings | `display` `keyboard` `speaker` `palette` `sliders` `gear` |
| window controls | `x` `dash` `square` `arrows-angle-expand` |
| files | `folder-fill` `folder2-open` `file-earmark` `file-earmark-text` `file-earmark-image` `file-earmark-zip` `file-earmark-code` `file-earmark-font` `file-earmark-play` `file-earmark-music` `download` `headphones` `images` `film` `trash3` |
| general | `sun` `moon` `arrow-left` `arrow-up` `chevron-right` `recycle` `check` `circle-fill` `exclamation-triangle` `info-circle` `plus-lg` |
| player | `play-fill` `pause-fill` `stop-fill` `skip-start-fill` `skip-end-fill` `eject-fill` `repeat` |

**One substitution from the list the task named**, and it is worth
recording because it is a property of the upstream set rather than a
preference. `arrow-clockwise` mixes fill rules between its two paths —
the ring is `evenodd`, the arrowhead is `nonzero` — and the importer
refuses to flatten those into one path, because concatenating them would
silently draw the wrong shape. `arrow-counterclockwise` and
`arrow-repeat` are identically affected (95 of the 2 078 upstream icons
are). `recycle` carries the same "refresh, go round again" meaning in a
single nonzero path and is used instead.

Two more substitutions of the same kind, from the split-view work
(`docs/ui.md`, "Split view blueprint"). `music-note-beamed` — and
`music-note`, `music-note-list` — mix fill rules exactly as
`arrow-clockwise` does, so `nitro-files`' Music place uses
**`headphones`**. And `image` (singular) draws its frame 0.002 px past
the 16-unit grid, which `no_ink_is_clipped_by_the_box` refuses because
that sliver would be clipped at every size; **`images`** is drawn inside
the grid and reads the same at 16 px, so Pictures uses it.

And one from `nitro-amp`'s transport. Upstream `shuffle` mixes fill
rules like the rest of this family, so the player's shuffle control is a
labelled checkbox rather than an icon button; `repeat` is a single
nonzero path and is imported as-is. `plus-lg` is the one new icon that is
`evenodd` upstream, and `even_odd_matches_upstream` lists it.

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
rx ry φ f1 f2 x y` with the command letter written once. 13 of the original 47
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
set is *closed*. 61 icons at the four recommended sizes is 61 × (256 +
576 + 1 024 + 2 304) ≈ **254 KB** — the whole set, everywhere, at every
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

**`nitro-files`.** Every row of the listing carries a **type** icon at
16 px in `ColorRole::Text`: `folder-fill` for a directory and for a symlink
pointing at one, the `file-earmark-*` family by MIME type
(`-text`/`-code`/`-image`/`-music`/`-play`/`-font`/`-zip`, plain
`file-earmark` for anything unclaimed), and `hdd` for a fifo, socket,
device node or an entry whose `stat` failed. The map is
`mime::icon_for`; `docs/files.md` has the table and the argument for why
source is matched before `text/*`.

It is the first consumer where the icon is **per row of a virtualised
list** rather than per widget, which is the case the cache's arithmetic was
waiting for: a screenful of a mixed directory is one entry per *distinct*
name at one size, so ten rows showing three types cost three masks.
`nitro-ui`'s `List` grew the row-icon API for it (`docs/ui.md`), and the
row's diff compares against the last `SetIcon` it **requested** — so a
`set_rows` over an unchanged listing costs zero and twenty whole-window
scrolls over a model whose icons alternate cost zero against 400
`SetText`s.

Measured on the box with a directory holding one of each type:
`icons_cached` **3 → 13** for ten rows showing nine distinct names,
`icon_bytes` **3 328 = 13 × 16²**, `icon_refusals` **0**, and
`icon_renders` unmoved across twenty full-window scrolls of a 1 860-row
listing. The server binary was **byte-identical** to the one already
deployed — a per-type icon column on a thousand-row directory added no
bytes at all to the process that draws it, which is this document's
opening argument in arithmetic. `docs/files.md` has the pixel census and
the rename control that settle "ten distinguishable icons" on numbers
rather than on a description.

It also found the map defect worth repeating here, because it is about
*names* and this document is about naming things: the box's
`shared-mime-info` spells Rust `text/rust` where our built-in table spells
it `text/x-rust`, so a `.rs` drew the wrong icon on the box and the right
one in every test. A name is only as portable as the table that produces
it, and a table tested against a fixture is a test of the fixture.

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

### The lookup: three steps, in this order

An `AS_COLOURED` name is resolved by asking three questions, and the
first one that answers wins:

| # | where | added |
|---|---|---|
| 1 | the machine's **XDG icon theme** — a PNG on disk | #3714 |
| 2 | **`<name>.desktop`'s `Icon=`**, one hop, no recursion | #3715 |
| 3 | that `Icon=` value through the **normal** symbolic-then-theme lookup | #3715 |

Step 1 is the rest of this section. Steps 2 and 3 are the `.desktop`
indirection below, and the short version is that they are what turns
`nitro-calc` into `calculator`.

### Step 1: the XDG spec, the honest subset

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

### The `.desktop` indirection

`crates/nitro-server/src/desktop_index.rs`.

A name that resolves in neither the symbolic set nor the icon theme is
looked up as a **desktop-entry basename**, and its `Icon=` value goes
back through the normal resolution. `nitro-calc` → `calculator` (ours,
symbolic); `firefox` → `firefox` (the theme's PNG); an absolute path is
honoured exactly as in step 1.

**Why this exists at all** is the paragraph the previous version of this
document ended with. `nitro-bar`'s window list asks for an icon by a
window's `app_id`, on the freedesktop convention that a desktop file is
named after its app id and its `Icon=` usually matches. On the test box
that convention falls back for **every one of our own applications**: the
bar showed the generic `window` glyph for the calculator, the settings
window and the terminal alike, because `nitro-calc` is an app id and not
an icon name. The fact that ties the two together was written down all
along — `Icon=calculator` in `deploy/nitro-calc.desktop` — and nothing
read it.

**One hop, and no recursion.** An `Icon=` that resolves nowhere is a
`BadIcon`, not another `.desktop` lookup. A chain would be a cycle
waiting to happen and answers no question the single hop does not; it is
a property of the call graph rather than a rule to remember, because step
3 calls the theme lookup directly and cannot reach the index.

**The symbolic set is not step 0**, and that is the selector rule holding
rather than bending. A palette role means the symbolic set and never
looks at anything else; `AS_COLOURED` never consults the symbolic set for
the name it was *given*. Trying it first would be the one-namespace
design this document rejects two sections up — `icon("list").coloured()`
would quietly mean the desktop's own glyph on every box, shadowing
whatever a theme installs.

The symbolic set **is** consulted for the **indirected** name, and that
is a different question: the `.desktop` file is the application's own
statement about which shape it wants, so there is nothing to shadow.
`Icon=calculator` in a file we ship means our `calculator`, deliberately.

**A symbolic answer to a coloured request is drawn tinted**, in
`Role::Text`. It has to be: a symbolic icon is an A8 coverage mask with
no colours of its own, so a node that kept `AS_COLOURED` would look the
handle up in the application cache, find nothing and draw an empty box.
`IconEngine::lookup_app` therefore answers with an `AppIcon` saying which
set replied, and `clients.rs` stores the role byte that implies. This is
the same **mixed-tint rule** the toolkit's own fallback already applies
client-side — `nitro-ui`'s `icon_fallback_tinted` sends a coloured icon's
fallback with a palette role — and it is now the server's too, which is
what makes it work for the bar, whose name *is* an app id and whose
fallback therefore never fired. A tinted request whose target is a
coloured PNG is unchanged from #3714: refused, for the reason the
role-byte section gives.

**The search path** is `$XDG_DATA_HOME/applications`,
`~/.local/share/applications`, and each `$XDG_DATA_DIRS` entry plus
`/applications` — the same directory precedence the icon path uses, minus
the flat `/usr/share/pixmaps`, which is an icon directory and holds no
desktop entries. Earlier wins, so a user's own entry overrides a
packaged one.

**The index is built once**, at start and on every `reload`, as
`basename -> Icon=` and nothing else. Eagerly rather than on the first
miss, because the alternative is a `read_dir` on the commit path — the
call a client's first-paint latency waits on — for a staleness guarantee
it would not actually provide: nothing here watches the filesystem either
way. `reload` is the moment the user says "look again" after installing
something, and it re-resolves every decorated window's frame icon with
it. The scan is bounded at 4 096 entries and 64 KiB per file, because
`$XDG_DATA_DIRS` is user-controlled and a directory with a million files
in it must not make the server's start unbounded.

**An indirected answer is memoised under the name that was asked for.**
That is not free bookkeeping, it is what makes the "one hash lookup per
row and no filesystem at all" promise true for the case this feature
exists for. The theme's own cache keys on the name that *resolved*, which
for an indirected icon is the `Icon=` target and never the `app_id` the
caller asked about — so without the memo the requested name stays
unknown and every repeat re-walks the theme directories and re-takes the
hop. It is also what makes `app_icon_indirections` count **distinct
names** rather than calls, which is what a reader needs it to mean: a bar
redrawing its window list must not make the number climb. Both a
`theme.icons` change and a `reload`'s re-scan drop it, since those are
the two things that can change the answer.

**Why the parser is not the launcher's.**
`crates/nitro-launcher/src/desktop.rs` already reads these files, and
this duplicates the thirty lines it needs. Three reasons, in order of
weight: the **dependency edge runs backwards** — the launcher is a
*client*, linking `nitro-wire` and `nitro-ui` and spawning processes, and
a compositor that linked its launcher to reuse a `split('=')` would pull
an application's tree into the process that owns the screen; the two want
**different answers** — the launcher needs `Name`, `Exec`, `Terminal`,
`NoDisplay`, `Hidden`, `Type` and the `%f` field codes and has to *skip*
entries, where this needs one key from files it never filters, because an
icon for a `NoDisplay=true` entry is still the right icon for that
application's window; and this is an **index**, not a parse — the
artefact is a map built once, not a `Vec<Entry>` that is then sorted and
filtered.

What *is* shared is the format's two traps, which is why it is a scan
rather than a `split('=')`: only the `[Desktop Entry]` group counts (a
later `[Desktop Action …]` has its own `Icon=`), and a localized key is
not the key (`Icon[de]=` must never overwrite `Icon=`).

`stats` gains `desktop_entries` (how many the index holds) and
`app_icon_indirections` (how many distinct names were answered through
the hop rather than by the theme directly). The second is the one that
says whether the index is earning its read.

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

**Two failures, and only one of them reaches the client.** The
distinction falls out of the lazy decode and it is worth being exact
about, because the obvious reading is wrong.

*Resolution* fails at **commit** time — the theme has no such name — and
that is a `BadIcon`: the node is cleared, the client is told, and its
`.fallback(…)` fires inside the same interaction. That is the common
case on a thin theme and it is the one the fallback exists for.

*Decoding* fails at **paint** time — the name resolved, the file was
there at commit, and it turns out to be truncated, unreadable, or not a
PNG. There is no client in the paint path and no transaction to attach an
error to, so what happens is: a one-line `warn`, a `Missing` mark so the
next frame does not try again, and an **empty icon box** for that node.
The widget's fallback does *not* fire. The client only learns anything if
it re-sends the same name later, at which point `lookup_app` sees the
mark and refuses with a `BadIcon` as usual.

That is a deliberate consequence rather than an oversight, but it is a
real gap and it is written down here rather than left to be discovered:
reporting it properly means remembering the refusal and attaching it to
the next commit from that client, which is a queue and an ownership
question for a case that needs a *corrupt file in an installed icon
theme*. `app_icon_misses` in `stats` is how a server says it is
happening; if it is ever above zero on a real box, this is the paragraph
to come back to.

The `Missing` mark matters for cost as well as for correctness: a missing
icon is the common case on a thin theme, and a launcher redrawing forty
rows must not walk the search path forty times a frame.

**A theme change re-asks every name, and keeps every handle.**
`set_theme` (on `reload`) drops every resolved path and every decoded
tile but keeps the **handles**, because the scene is holding those
indices and an index that changed meaning would be a far worse bug than a
re-resolve. The next `lookup_app` for a name whose path was dropped
resolves it again against the new theme — at *commit* time, which is what
matters: a name the new theme does not have is refused there with a
`BadIcon`, so the client's `.fallback(…)` fires, rather than the node
silently drawing nothing.

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

**`nitro-launcher`** puts a 24 px icon in front of every row, falling
back to `window`. A **built-in** entry's icon is symbolic — a built-in
exists precisely on the box with no icon theme installed, so its icon has
to come from the set compiled into the server. A **`.desktop`** entry's
is an application icon, asked for by the entry's **app id** (its file's
basename) rather than by its `Icon=` value.

That last point is the launcher's half of the indirection, and it is
worth stating because the obvious reading is backwards. Sending `Icon=`
directly looks like one lookup saved and is one *namespace* lost: an
`AS_COLOURED` name goes to the machine's icon theme, then to
`<name>.desktop`, then to `BadIcon` — and never to the symbolic set, by
the rule two sections up. Our own files name symbolic shapes
(`Icon=calculator`), which no icon theme has, so the direct spelling
resolves nowhere on exactly the boxes this desktop runs on and every row
falls back to `window`. Sending `nitro-calc` takes the hop, reads that
same `Icon=calculator`, finds it in the symbolic set and draws it tinted
— the identical path `nitro-bar`'s window list takes. For a third-party
application the two spellings agree (`firefox.desktop` has
`Icon=firefox`), so nothing changes for it; the hop only matters where
the basename and the `Icon=` differ, which is all of ours.

(An entry naming **no** `Icon=` still shows `window` directly rather than
asking for its app id first: that would put a `BadIcon` round trip on the
first-paint path for every such row, which is what `first_paint.rs`'s
control arm measures — and it caught exactly that regression when this
rule was first written.)

The idle and latency contracts hold, and the second one is measured
rather than argued. The structural argument is that `SetIcon` is
one-way, so a row costs no round trip, and the decode is the server's and
lazy — but "no round trip" is a claim about a protocol and "it opens as
fast" is a claim about a clock, and the second does not follow from the
first for free: a row's icon is still a node, a mutation and a blit.

`crates/nitro-launcher/tests/first_paint.rs` times the show — the tap,
through layout, paint and flush, until the server goes quiet — over 40
`.desktop` entries filling the 20-row list, with one variable: every
entry names an icon, or none does. The icon arm deliberately names icons
**no theme has**, so it is the expensive case rather than the cheap one:
the server walks its whole search path per row, answers `BadIcon`, and
the widget sends its fallback. Twelve timed shows per arm, alternating,
median:

| arm | first paint, 20 rows |
|---|---|
| every row names a theme icon (all 20 fall back) | **247.4 ms** |
| no row names one (control) | **247.6 ms** |

**×1.00.** The number to watch is not the few tenths of a millisecond,
which is noise on a shared machine — it is that a synchronous round trip
per row would put twenty of them on this path and show up as a multiple.
The test asserts only that loose bound for exactly that reason: a tight
one would be a flaky test that readers learn to re-run, which is worse
than no test. The absolute figures are the dev box's and are dominated by
the per-row `SetText` measurement round trips the launcher already made
before this task.

**`nitro-bar`**'s window list uses a window's `app_id` **directly as an
icon name**, falling back to `window`. That is the freedesktop
convention — an application's `.desktop` file is usually named after its
app id and its `Icon=` usually matches — and it is right often enough to
be worth one string. It is also honestly limited: an application whose
app id and icon name differ (`org.gnome.Nautilus` vs `nautilus`) gets the
fallback.

Since #3715 the bar's rule is unchanged and its **limitation has shrunk**,
because the server took the second step for it. The name the bar sends is
still the raw `app_id` and it still holds no index and reads no files;
what changed is that the server, failing to find that name in the theme,
now looks for `<app_id>.desktop` and resolves its `Icon=`. So the case
that falls back is no longer "an app whose app id is not an icon name" —
which is all of ours — but the narrower "an app whose `.desktop`
**basename** differs from its app id". `org.gnome.Nautilus` ships
`org.gnome.Nautilus.desktop`, so it now works; an application that
registers one app id and installs a differently named entry does not.

The alternative that was weighed here — "have the *server* resolve
`app_id` → `.desktop` → `Icon=`; strictly better and strictly bigger" —
is the thing that shipped, and the evidence the paragraph asked for is
what drove it: the fallback icon *was* showing up on applications people
actually run, namely every application this desktop ships. The index, its
search path and its invalidation are in the compositor after all, and
they are 200 lines. Our own applications keep the convention regardless:
the `.desktop` files under `deploy/` are named after their app ids.

#### What still falls back to `window`, and on purpose

The hop only fires for an app id that has a `.desktop` file, and since
#3723 `just deploy` installs ours — all four of them — into
`~/.local/share/applications` on the box. So the list of things that
still show the generic `window` glyph is short and deliberate:

| | icon |
|---|---|
| `nitro-calc`, `nitro-files`, `nitro-settings`, `nitro-term` | `calculator`, `folder-fill`, `gear`, `terminal`, through the hop |
| **`nitro-bar`, `nitro-launcher`, `nitro-wallpaper`** | `window` — **no `.desktop`, on purpose** |
| **`nitro-demo`, `nitro-bench`** | `window` — **no `.desktop`, on purpose** |
| `nitro-shot`, `hey` | not windows at all |

The shell surfaces are the first group's reason: a bar, a launcher and a
wallpaper are not applications and must not be listed *in* the launcher,
and they have no window-list button either (`docs/shell.md`: only
`Normal` windows are applications). An entry for them would be a button
that starts a second bar. The second group is tools — a demo and a
benchmark are things a developer runs from a shell with arguments, not
things a user picks off a list — and `nitro-demo` keeps the launcher's
**built-in** entry, which is the right amount of discoverability for it:
present on a box where the binaries are, absent from anybody's
application menu.

So the `window` glyph on one of those is the fallback working, not a
defect, and the file to *not* write is part of the design rather than an
oversight somebody should fix.

**The window decorations** are the other consumer, and the one with no
client at all. A decorated window's title bar carries its application's
icon at 16 px and three symbolic button glyphs; all four are nodes the
*server* owns, so they are set through the engine's internal path rather
than over the wire, and the `window` fallback fires synchronously instead
of as a `BadIcon` somebody has to answer. `docs/wm.md` has the frame
anatomy.

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
three showed the symbolic `window`, because our app ids (`nitro-calc`,
`nitro-settings`, `nitro-term`) are in no icon theme on the box. That was
rule (a) working as documented rather than failing — the fallback is what
keeps the list readable — and it is exactly the finding that made #3715's
indirection worth building. The measurement above is icons-B's and is
kept as it was taken; #3715's own box numbers are in its section below.

Sizes and memory are in `docs/budget.md`; the two numbers worth repeating
here are that the server grew **+97 368 B (+3.9 %)**, a third of what the
symbolic set cost, and that the largest icon actually loaded decoded in
**214–510 µs**.

### Measured on the box (#3715, the `.desktop` hop)

Test box, dark scheme, scale 1, my build of server *and* every client,
the human's own `server.conf` restored afterwards. **No fixtures**: the
four `.desktop` files are `deploy/nitro-{calc,files,settings,term}.desktop`
from this repository, copied to `~/.local/share/applications` — which is
exactly the box state #3714's writeup says would fix the bar.

**The headline, as an A/B with one variable.** Same binary, same three
apps, fresh server and fresh clients in both arms; the only difference is
whether the entries are installed. The numbers are the bar's window-list
icon boxes compared against each other, pixel for pixel:

| bar window-list icons | calc vs settings | calc vs term | settings vs term |
|---|---|---|---|
| **before** — no entries | **0/256** | **0/256** | **0/256** |
| **after** — entries installed | **129/256** | **117/256** | **124/256** |

Zero differing pixels *is* #3714's finding reproduced: three identical
`window` glyphs. `app_icon_indirections` went 0 → 9 across the same pair.

A false start worth recording, because it looks like the feature failing:
the first control removed the files and sent one `reload`, and the icons
**did not change**. The bar sends one `SetIcon` per window when the
window appears, and the scene nodes still hold resolved handles — so a
before-arm needs a restart. A control that shares state with the arm is
not a control.

**The defect this run found, which the whole suite missed.**
`desktop_entries` read **0 on a fresh server with twelve `.desktop` files
on disk**, and **12** after a `reload`. `rescan_desktop` was called only
from `set_desktop_dirs`, which only a *test* calls: every fixture had an
index and a real server had an empty one until the user happened to
reload. Every test passed, because every test supplies its own
directories — the construction path the tests take was not the one the
product takes. Both constructors scan now, and
`a_default_engine_has_already_scanned_for_desktop_entries` asserts the
constructor rather than the fixture.

**The frame's own icons**, `nitro-calc`, focused (bar `#2c3e55`):

| | measured |
|---|---|
| app icon box, ink | **92/256 px**, of which `f0f4f8` × 50 — `title_text_active`, i.e. tinted |
| close / maximize / minimize ink | **28 / 26 / 12** of 196 — glyphs, not discs |
| every button's corner at rest | **bar colour**: no disc |
| `title_close` in the resting title bar | **0 px** |
| `title_maximize` in the resting title bar | **0 px** |

The last two are the censuses that would catch a regression to the old
look: the red and the green are not on screen at all until the pointer
arrives.

**Hover**, pointer at each button's centre:

| hovered | px changed | `title_close` | `title_button_hover` | the other two |
|---|---|---|---|---|
| close | 160/196 | **75** | 0 | 0, 0 |
| maximize | 160/196 | 0 | **79** | 0, 0 |
| minimize | 160/196 | 0 | **91** | 0, 0 |
| after leaving | — | **0** | **0** | 0, 0 |

`text_layouts` 142 → 142 and `icon_renders` 7 → 7 across all of it: a
hover shapes no text and rasterises nothing.

**Scale, and the arithmetic that makes it a re-raster rather than a
blit.** `icon_bytes` is **1 324** at scale 1, and that figure closes
exactly: 3 × 10² (the button glyphs) + 16² (the frame icon) + 3 × 16²
(the bar's own icons). At scale 2 it is **5 420**, a delta of **+4 096 =
four 32² masks** — the four 16-logical icons re-rasterised at the device
size. Back at scale 1, `icon_renders` stays at 11: the 16 px masks were
kept, because an output can change scale back.

**The rest**, on pixels or on counters rather than on description:

| claim | measured |
|---|---|
| minimize button | `minimized` 0 → **1**; `Alt+Tab` → **0** |
| maximize button | bounds → **0,0,1918,1019** = the work area exactly |
| close button | `windows` 4 → 3, `decorated` 1 → 0, process gone |
| drag, 30 steps | **91 frames**, `text_layouts` **+0**, `icon_renders` **+0** |
| idle, 45 s gated to `:02` | arm **+4**, control **+2**, arm **+4**, control **+4** |
| `nitro-server` binary | 2 599 008 → **2 613 488 (+14 480 B, +0.56 %)** |

The idle arms are run **twice each** because a single pair is publishable
in either direction: the app arm is never *above* the control, and the
one reading of 2 is the bar's 30 s poll landing differently in the
window, not a cost of the icons.

### Measured on the box (#3723, the entries actually installed)

Test box at 1080p@119982, dark, scale 1, my build of every binary
(md5-checked at entry **and** at exit, unchanged — a run whose binaries
move under it is two builds interleaved). The human's `server.conf`
restored `diff`-identical. **No fixtures**: the entries are
`deploy/nitro-{calc,files,settings,term}.desktop` from this repository,
rsynced by the `just deploy-bins` recipe this task adds.

#3715 measured this feature with the files copied in by hand and then
removed for hygiene. This is the same measurement with them **deployed**,
and with the two things that copy could not show: a fifth window as a
control, and whether launching from the launcher still works.

**The headline, one variable.** Both arms are the same script — restart,
then the same five clients in the same order, then one full-screen
readback — so the server places the windows identically and the same
coordinates name the same button in both. Fresh server *and* fresh
clients each arm, because the bar sends one `SetIcon` per window at
creation and the scene nodes hold resolved handles: a "control" that
shares state with the arm is no control (#3715's false start).

| bar window-list icon boxes, of 256 px | before | after |
|---|---|---|
| every one of the **10** pairs among 5 buttons | **0** | **159–198** |
| distinct icon boxes on screen | **1** | **5** |

One distinct box across five buttons *is* #3714's finding in its sharpest
form: not "they look similar", but literally one glyph drawn five times.

**The control is what makes the arm attributable.** `hello_dialog` has no
`.desktop` file and no theme icon, so it must not move:

| button, before → after | changed |
|---|---|
| Calculator | **162**/256 |
| Settings | **159**/256 |
| Terminal | **34**/256 |
| Files | **173**/256 |
| **`hello-dialog` (control)** | **0**/256 |

Terminal's 34 is smaller because `terminal`'s glyph shares most of its
box with `window`'s — two rectangles — which is exactly why the *pairwise*
table above is the headline: 34 px is unambiguous against a control that
moved 0.

**The frames**, measured one window at a time so nothing can be mistaken
for the frame, and located by the `title_bar` colour rather than by "a
band wider than N" — the first attempt at that found a 765 px gradient
band **in the wallpaper** and reported a confident 0/256:

| title-bar icon box, 16×16 | before | after | changed |
|---|---|---|---|
| `nitro-calc` | 55 px ink | **91** | **136**/256 |
| `nitro-files` | 55 px ink | **151** | **160**/256 |

55 px of ink in both before-arms is the same `window` glyph twice.

**The counters**, and the launcher:

| | before | after |
|---|---|---|
| `desktop_entries` | **8** (system files only) | **12** |
| `app_icon_indirections` | **0** | **4** |
| launcher entries named Terminal / Calculator / Files / Settings | 1 each (built-ins) | **1 each** (the files') |
| `hey nitro-launcher do results/0 click` on Terminal → `pgrep -c nitro-term` | 1 → 2 | **1 → 2**, argv `nitro-term` |

That last row is the regression the old "deliberately NOT installed"
comment guarded, and it is the one an icon screenshot cannot see: an
entry that is present and dead looks identical to a working one. The
spawned process's argv is the bare `nitro-term`, not the built-in's
absolute path — so what ran is the packaged `Exec=`, resolved through the
inherited `PATH`. A second `just deploy` of the same four files left
`desktop_entries` at 12 and the entry list byte-identical.

**The prepend, from the outside**, which is the whole fix in two lines:

```text
nitro-session : PATH=/usr/local/sbin:/usr/local/bin:...      ← the unit's
nitro-launcher: PATH=/home/kaspar/nitro-bin:/usr/local/sbin:...
nitro-bar     : PATH=/home/kaspar/nitro-bin:/usr/local/sbin:...
```

**The finding that changes a deployment rule.** The files and the
`PATH`-prepending session are **coupled**, and it was worth measuring
rather than assuming: with the four files installed next to *main's*
session (no prepend), launching Terminal from the launcher spawns
**nothing** — `pgrep -c nitro-term` 0 → 0 — because the packaged entry
shadows the built-in and its bare `Exec=` does not resolve. Removing the
files restores the built-in and the launch works again (0 → 1, argv
`/home/kaspar/nitro-bin/nitro-term`), which is the control that makes the
first arm mean what it says. So the two halves ship in one commit;
`docs/testbox.md` carries the rollback warning.

#### The third consumer, which the first run did not census

The run above measured the bar's window list, the two frames, the entry
count and the launch — and **not** `results/N`. Review caught that the
same commit broke the launcher's own rows, and the deduction was right:
installing the files flips those entries from `Source::Builtin` to
`Source::Desktop`, `row_icon` keyed the *namespace* off the source, and a
row therefore sent `SetIcon("calculator", AS_COLOURED)` — which step 1
misses (no theme has it), step 2 misses (`calculator.desktop` does not
exist) and the symbolic set is forbidden to answer. All four rows fell
back to one `window` glyph.

Measured, on the box, one variable — two launcher binaries built from the
same tree differing only in `row_icon`, each verified by md5 **after** the
copy:

| launcher row icon boxes, of 576 px (24×24) | `Icon=` (before) | app id (after) |
|---|---|---|
| all 6 pairs among our four rows | **0** | **293–371** |
| distinct boxes among our four | **1** | **4** |
| `app_icon_indirections` | **0** | **7** |

And the control that makes it attributable — per row, before vs after, at
the same screen coordinates:

| row | changed |
|---|---|
| Calculator / Files / Settings | **329 / 361 / 281** of 576 |
| Terminal | **81** |
| **Foot, Foot Client, Foot Server, Hello Dialog, Htop, Nitro Demo, TeXInfo, Vim** | **0** each |

Eight rows of 576 px moved **zero**. `Foot` is the load-bearing one: it
has a real theme PNG, so its row proves the ordinary third-party path is
untouched — for an application whose basename and `Icon=` agree, both
spellings resolve to the same file.

**Three instrument failures on this one measurement**, all of which
returned a confident number first, and all three are the same shape as
traps already in `docs/testbox.md`:

* **The launcher is never rebuilt, only hidden**, so `hey … list` returns
  row bounds whether or not it is showing. Censusing those coordinates
  read the **wallpaper**, which — being a gradient — obligingly reported
  `576/576 differing` for every pair, with 2–4 colours per box. The show
  is now proved in pixels (~240 000 for a 600×400 overlay) before
  anything is measured.
* **A widget's bounds are window-local.** `hey` puts row 0 at `(12,74)`;
  the launcher's origin on screen was `(660,340)`. Adding the two is the
  difference between measuring icons and measuring wallpaper — the same
  shape as `hey <app> get window bounds` returning `0,0,w,h` for the
  frame census earlier in this task. The origin is now derived from the
  pixels the tap changed, not from any reported coordinate.
* **`hey nitro-launcher do launcher click` changed 0 pixels**, because a
  scripted `click` runs the widget's callback — the input path's
  *destination* — and the launcher's trigger is a server-side tap state
  machine. It takes a real `ydotool key 125:1 125:0`. (`nitro-bar`'s
  README already says this about focus; it is equally true of the tap.)

A fourth was caught before it ran: the tap **toggles**, so "240 000 px
changed" is equally consistent with showing and with hiding, and the
first version of the guard could not tell them apart.

### Still deferred

* **SVG application icons**, and the gradients they need.
* **Per-icon user overrides** — pinning one name to one file. That is a
  desktop-settings feature, not a path resolver's.
* **`Context=`, localized theme names, `.icon` metadata.** All of it
  exists for an icon *chooser*; we are given a name and asked for a file.
* **Watching the `.desktop` directories.** The index is rebuilt at start
  and on `reload`, so an application installed while the desktop is
  running gets its icon on the next `reload` and not before. A watch
  would be an inotify descriptor per directory in `$XDG_DATA_DIRS` for a
  change that happens when a user installs software — and `reload` is
  already the gesture for "I changed something, look again".

`.desktop`-based resolution for the bar is no longer on this list: #3715
built it, and the section above is what it does.

What is *not* deferred and not planned: client-supplied icon pixels.
An app that needs arbitrary artwork has `Image`, and pays the buffer for
it.
