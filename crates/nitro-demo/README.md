# nitro-demo

The M1 measurement client. `hello_client` proves a client can draw; this
one is the instrument that produced `docs/latency.md` and `docs/budget.md`.

It does three jobs that a toolkit will one day do for real:

- **Draws a realistic scene.** One 800×500 `Normal`-layer window with a
  gradient backdrop, four bordered rounded rects, a memfd-backed ARGB
  image, and a 24×24 "follower" rect that tracks the pointer with a 2-px
  ghost of its previous position. That shape matters: a pointer move must
  produce *client* pixels, or the latency being measured is only the
  server's cursor.
- **Measures input-to-photon from the client's side.** For every
  `PointerMotion` it remembers `serial → input time_ns`, and on
  `Presented{serial, time_ns}` it records the difference. Both stamps are
  the server's `CLOCK_MONOTONIC`, so the subtraction is immune to clock
  skew between the processes.
- **Cross-checks against the server.** `--stats` reads the v0 control
  socket and prints the server's own `i2p_*` next to the client's, with a
  one-frame agreement check. A measurement only the server takes is the
  server marking its own homework.

## Running

```sh
nitro-demo --follow                  # default: commit only on input
nitro-demo --animate                 # one commit per Frame callback
nitro-demo --follow --windows 5      # cascade/z-order/focus
nitro-demo --follow --stats          # + the server's view and the cross-check
nitro-demo --damage                  # outline the damage rects
nitro-demo --save-small docs/x.png   # downscaled screenshot, no ImageMagick
```

On the box: `ssh box 'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-demo
--follow --stats'`. `NITRO_SOCKET` and `NITRO_CONTROL` override the socket
paths; `NITRO_DEMO_SHOW_DAMAGE=1` presets `--damage`.

Keys (matched on **evdev keycodes**, not keysyms, so the demo behaves the
same on any layout and on a server running without a compiled keymap):
`q` quits, `d` toggles damage outlines, `Esc` closes the window, `n`/`p`
select the next/previous window.

## The two modes

**`--follow`** commits only in response to input, so an idle demo costs
*zero* frames — the client-side half of the property the whole design
exists for, and the mode the latency numbers are taken in. A burst of
motions arriving in one wakeup collapses into a single commit carrying the
latest position: answering each separately would send more commits than
there are frames, and the extra serials would land on the same vblank and
flatter the histogram with duplicate samples.

**`--animate`** moves a rect 4 px per `Frame` callback and asks for the
next callback in the same transaction — one request, one answer, one
commit, no free-running render loop. The pacing check is on the
*increments* between marks, not the totals: the build transaction is a
commit no callback asked for and there is always exactly one
`RequestFrame` in flight, so the totals differ by a small constant
forever. Comparing them would report that constant as a violation.

## Layout

| module | what |
|---|---|
| `args` | the command line |
| `latency` | the serial ledger, the histogram, nearest-rank percentiles |
| `geom` | follower/trail geometry and damage-outline rects |
| `scene` | ids, layout, the mutation batches, the procedural image |
| `control` | the v0 control socket (`stats`, `shot`) |
| `png` | a size-conscious PNG writer for `--save-small` |
| `app` | the event loop |

`scene` returns `Vec<ClientMsg>` rather than driving the `Transaction`
builder, for two reasons: the demo reports the bytes its first frame costs
(`docs/budget.md`), which needs the messages as values it can weigh before
sending; and it lets `tests/against_server.rs` feed the binary's *own*
batches to a real server instead of a reimplementation that could drift.

Node ids are arithmetic in the window index (`Ids::for_window`), not a
counter: window 3's follower is always `3 * STRIDE + 8`, so a log line
names a node without a table and `--windows N` needs no bookkeeping.

## Why a second PNG encoder

`nitro-shot` writes **stored** (uncompressed) deflate — the right trade
for a debugging dump piped over ssh, where encode speed is everything and
size is irrelevant. `--save-small` exists to produce a file that goes in
`docs/`, under a 200 KB budget against a 1920×1080 screenshot's 8 MB, so
`png.rs` does the two things that actually buy compression on a UI
screenshot: the `Up` row filter, and back-references at distance 1 (flat
fills) or 4 (a gradient's repeating per-pixel delta). No hash chains, no
dynamic Huffman, no general match search — those two distances are where
all the win is on this input and are the cases that are easy to get
provably right. About 40× on the demo's own screenshot.

The downscale is nearest-neighbour on purpose: a box filter would blur the
one-pixel damage outlines into invisibility, and showing where the damage
was is the entire reason for the screenshot.

## Tests

`cargo test -p nitro-demo`. Unit tests cover the percentile maths, the
ledger's bounded eviction, the outline geometry, the id space and the PNG
encoder (including the awkward run lengths 3, 258 and 259).

`tests/against_server.rs` runs the real `nitro_server::run` on a thread
with a `FakeBackend` and a `FakeInput` — no seat, no DRM device, no evdev
node — and drives the real `App` over a real socket: the demo's scene is
accepted and painted where it asked, synthetic motion produces a genuine
end-to-end latency sample, `--animate` commits exactly once per callback,
the damage outlines appear in a screenshot, `--windows 3` cascades, and an
idle demo leaves the server flipping nothing.
