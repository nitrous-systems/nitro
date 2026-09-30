# nitro-wallpaper

The desktop's **backdrop**: a `Background`-layer surface anchored to
every edge, painting a gradient, a colour or an image.

```console
$ nitro-wallpaper                    # the themed gradient (default)
$ nitro-wallpaper --color 202430     # one solid colour
$ nitro-wallpaper --image bg.ppm     # a picture (binary PPM)
```

It is the **smallest possible shell client**, and that is the interesting
part. A wallpaper has no input and no timers: it opens one window per
output on the `Background` layer, anchors each to all four edges, paints
once, and then never sends another byte until an output changes.

`a_settled_wallpaper_sends_nothing_at_all` asserts exactly that — not
even a timer is armed — because a program that sits on screen for the
whole session and costs nothing to be there is a claim worth checking
rather than assuming.

## One surface per output

One process, one shell connection, one `Ui` — and one backdrop window per
output, exactly as [`nitro-bar`](../nitro-bar) runs one panel per output.
The main window covers the output the server placed it on; every other
output gets `add_surface_window` with
`Surface::wallpaper().anchored(Anchor::fill().on(id))`
([`docs/shell.md`](../../docs/shell.md) §Anchors).

The wallpaper subscribes to `Outputs` and follows the bar's rule: **the
extra windows' outputs are the last snapshot minus the main window's
output**, reconciled at every `OutputsEnd` and whenever the server
re-places the main window. Plug an output and a window opens on it;
unplug it and that window closes (at the `OutputGone`, before the
snapshot that follows). `shell_clients` stays **1**.

## Sizes are still `Configure`-driven

The `Outputs` subscription decides *which* windows exist, never how big
they are. The server re-applies each anchor on every mode change, scale
change and hotplug and tells the window with a `Configure`; the toolkit
relayouts and repaints. Taking sizes from the snapshot instead would be
a second opinion about a window's size that has to agree with the
server's.

The gradient is expressed in the **node's own** space, from its top edge
to its bottom, so the server recomputes it when the node is resized and a
mode change costs the repaint the `Configure` already caused and nothing
more.

## An image is decoded once, scaled per output

`--image` is decoded **once**, at start-up, and the decoded pixels stay
in the state: an output plugged in an hour later needs its own copy, and
re-reading the file (which may have changed or gone) would be worse.
Each window gets a copy **scaled bilinearly to its own size in device
pixels**, so a 1920x1080 and a 2560x1440 screen are each sharp rather
than one stretched from the other. That copy moves into the window's
`Image` widget, goes to the server in a memfd and is dropped client-side
at the next paint; unplugging the output releases its buffer. A window
is scaled for only after it reaches its own output (it is first placed
on the primary at a placeholder size), and again only when its size
changes. The tests count both.

## Images: P6 PPM, and why only that

There is **no PNG or JPEG decoder anywhere in this tree**, on purpose.
`nitro-shot` and `nitro-hey` each carry a small stored-deflate *encoder*;
`DEPENDENCIES.md` lists `png` under Rejected. Adding a decoder would put
a parser for untrusted bytes into the dependency graph of a program that
runs for the entire session — which is precisely the exposure
`DEPENDENCIES.md` §"The untrusted-bytes note" is careful about for fonts.

PPM is the format whose decoder is seventy lines and readable in one
sitting: a magic number, three integers and a block of RGB triples.

```console
$ magick photo.jpg wallpaper.ppm      # or `pnmtoplainpnm`, or GIMP
$ nitro-wallpaper --image wallpaper.ppm
```

The parser refuses, with a message, everything it does not understand: a
`P3` (ASCII) file, a maxval that is not 255, a truncated pixel block, a
header without its separating whitespace, and a header claiming more than
64 megapixels. Refusing beats padding with black — a truncated image
padded out looks *almost* right, which is the worst possible answer to a
corrupt file.

One subtlety with a test on it: exactly **one** whitespace byte ends the
header. A parser that skipped all whitespace there would eat a first
pixel whose red channel happens to be `0x20` or `0x0a`, shifting every
pixel in the image by one byte and turning the picture into colourful
noise.

## Everything refuses rather than falling back

A bad `--color`, a missing `--image` file, an image that does not decode,
an unknown flag: each exits with the reason instead of quietly painting
the default gradient. A wallpaper that ignored `--image` would look
exactly like one whose file was wrong, and the user would go looking in
the wrong place.

## Driving it with `hey`

```console
$ hey nitro-wallpaper list
$ hey nitro-wallpaper get backdrop bounds     # 0,0,1920,1080
```

Which on a box showing a black screen is exactly the question: is the
wallpaper running, and how big does it think it is?

The backdrop hangs under a container rather than *being* the root, and
the reason is addressing: `introspect::path_of` names the root `window`
whatever else it is called, so a named root is a name nothing can
resolve. One `Flex` that paints nothing costs a scene group and a
`SetBounds` per resize, and buys the same `window/<name>` addressing
every other nitro app has.

## Limitations

* **P6 PPM only**, argued above.
* **The image is stretched**, not letterboxed or tiled. Aspect-ratio
  modes are a flag and a rectangle calculation, and nothing in M3 needs
  one; a `--scale fit|fill|tile` is the shape it would take.
* **No live reload.** Changing the wallpaper means restarting it. The
  real answer is a `nitro-wallpaperctl` over the introspection socket
  (`set backdrop …` already exists as a mechanism), and that wants a
  settings story rather than a flag.
* **`Background` is a layer, not a window manager concept.** A window
  that asks to be `Background` and then wants to be clicked will not be:
  the scene's layer ordering puts every `Normal` window above it. That is
  correct for a wallpaper and would be surprising for anything else.

## Measured

On the test box (Pentium G3240, 1920x1080, `i915`), release, stripped,
running alongside the bar, the launcher and `nitro-calc`:

| | value |
|---|---|
| binary | **521 880 bytes** (522 KB) |
| RSS / HWM | **2 608 kB** / 2 608 kB |
| idle CPU over 30 s | **0.00 %** (zero jiffies) |
| timers armed | none |

It is the smallest of the four shipped `nitro-ui` binaries, and within a
kilobyte of `hello_dialog` — which is the honest summary of what a
wallpaper is: the toolkit, one widget, and no application.

The RSS figure is worth a paragraph, because writing this table is what
caught the one real bug in the one-window crate: it kept the whole
`Paint` as the app's state, so an image wallpaper held a second 8 MB copy
that nothing ever read. Per-output surfaces changed that trade on
purpose. The decoded source is now kept (`w * h * 4` anon bytes for the
session: 8 MB for a 1920x1080 picture), because a later output needs it;
`the_state_keeps_exactly_one_copy_of_the_pixels` pins it at exactly one.
Each output's scaled copy is transient in this process and lives on as
the server's mapping of its memfd — `docs/budget.md` has the per-output
arithmetic. A gradient or a colour keeps no pixels at all.

## Tests

`src/ppm.rs` unit-tests the decoder against the cases that produce a
*wrong picture* rather than an error: the whitespace byte, comments in
the header, `[b, g, r, a]` ordering, truncation, a hostile size, an ASCII
`P3`, and a dozen malformed headers that must be errors and not panics.
`src/lib.rs` tests the command line and the fill each `Paint` produces.

`tests/wallpaper.rs` drives the tree the binary builds through a real
server on the shell socket, on real pixels: the surface covering the
whole output with every corner inside the gradient; the gradient really
being one, lighter at the top; a solid colour painted *exactly*; a 2×2
image blown up to the output with each quadrant in the right place (which
is what catches a byte-order bug a "it is not black" assertion would
miss); zero traffic while idle; a mode change resizing it — followed by
silence again; a second output getting its own surface on the same
connection, losing it on unplug and getting one again on plug; and an
image decoded once and scaled once per output, to each output's own
size, with the unplugged output's buffer released.
