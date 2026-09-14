# nitro-wallpaper

The desktop's **backdrop**: a `Background`-layer surface anchored to
every edge, painting a gradient, a colour or an image.

```console
$ nitro-wallpaper                    # the themed gradient (default)
$ nitro-wallpaper --color 202430     # one solid colour
$ nitro-wallpaper --image bg.ppm     # a picture (binary PPM)
```

It is the **smallest possible shell client**, and that is the interesting
part. A wallpaper has no input, no timers and nothing to subscribe to: it
opens one window on the `Background` layer, anchors it to all four edges,
paints once, and then never sends another byte until an output changes
size.

`a_settled_wallpaper_sends_nothing_at_all` asserts exactly that — not
even a timer is armed — because a program that sits on screen for the
whole session and costs nothing to be there is a claim worth checking
rather than assuming.

## Hotplug is followed without watching anything

The wallpaper does **not** subscribe to `Outputs`, and does not need to.
The server re-applies an anchor from `sync_outputs` on every mode change,
scale change and hotplug, and tells the client the only way it ever tells
a client about its own geometry — a `Configure`. The toolkit turns that
into a relayout and one repaint.

The obvious implementation (ask for `Outputs`, react to `OutputInfo`)
would be strictly worse: it subscribes to a stream of events to learn
something the `Configure` already said, and it creates a second opinion
about the window's size that has to agree with the server's.

The gradient is expressed in the **node's own** space, from its top edge
to its bottom, so the server recomputes it when the node is resized and a
mode change costs the repaint the `Configure` already caused and nothing
more.

## One window, and what "per output" really costs

The spec asks for one surface per output. That is **not implementable on
this protocol** — the same gap [`nitro-bar`](../nitro-bar) records and
[`docs/shell.md`](../../docs/shell.md) §Deferred states under *Per-output
shell surfaces*:

* `CreateWindow` carries no output, and the server places every new
  window on the primary one;
* `SetAnchor` anchors to whichever output the window is *already* on;
* nothing moves a window between outputs but a user's drag.

N wallpaper windows would therefore all land on the primary output: N
stacked backdrops on one screen and none on the others, which is worse
than one. So the wallpaper opens **one** window, and the honest
description is: the primary output is covered, and a second output shows
the compositor's own background. The fix is the `output` field on
`SetAnchor`.

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

* **One output**, argued above.
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
caught the one real bug in the crate. A 1920x1080 `--image` wallpaper is
8 MB of pixels; they reach the server in a memfd and the `Image` widget
drops its copy at the first paint, so the resident cost is the server's
mapping rather than a picture in both processes. But the first version
also handed the whole `Paint` to `App::run` as the app's *state* — which
lives for the session — so an image wallpaper kept a second 8 MB copy
that nothing ever read, because a wallpaper has no callbacks to read it
with.

The state is now `Kind`: `Gradient`, `Solid`, or `Image(w, h)`. It is
`Copy`, and `the_state_does_not_keep_a_copy_of_the_pixels` asserts that —
a type holding a `Vec` cannot be `Copy`, so the test stops compiling the
moment somebody puts the pixels back.

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
miss); zero traffic while idle; and a mode change resizing it — followed
by silence again.
