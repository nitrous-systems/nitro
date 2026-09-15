# nitro-png

A PNG decoder for icons. No dependencies, no `unsafe`, one entry point:

```rust
let img = nitro_png::decode(&bytes)?;   // img.data is [b, g, r, a] per pixel
```

Straight-alpha BGRA, rows top to bottom — the layout `nitro-ui`'s `Image`
widget, `nitro-raster`'s blitter and `nitro-wallpaper`'s PPM reader all
already use, so a decoded icon goes on screen without a conversion pass.

## Why this exists rather than the `png` crate

Because it was measured, not assumed. `DEPENDENCIES.md` carries the full
table under "`png` versus our own decoder, measured (#3711)"; the short
version is that the crate is **faster** (1.2–2.7× on the test box, the gap
growing with image size) and **lighter on peak RSS** (+1.5 MB vs +3.8 MB
decoding a 512×512), while this crate is **smaller in the binary** (+50 KB
vs +130 KB in `nitro-server`), **smaller in code we ship** (999 lines
against ~28 500 across the crate and its eight transitive dependencies) and
**zero new external crates against eight**.

At icon sizes the speed difference is 3–4 µs per file, which is the length
of a syscall. That is the trade this crate takes: it is slower at a scale
where slower does not matter, to keep eight crates and ~28 500 lines of
someone else's parser out of a process that runs for the whole session.

The one number that is a genuine cost rather than a rounding error is peak
RSS, and it is a design property: this decoder holds the whole filtered
raster *and* the output buffer at once, where the crate unfilters row by
row. For a 512×512 icon that is 2 MB of transient; for the 16–48 px icons a
theme is actually made of it is 3–10 KB. Should the server ever decode
something large, the fix is a streaming unfilter, not a dependency.

## What it does

| | |
|---|---|
| colour types | 0 (grey), 2 (RGB), 3 (palette), 4 (grey+alpha), 6 (RGBA) |
| bit depths | 1, 2, 4, 8, 16 (16 truncated to 8 by taking the high byte) |
| transparency | `tRNS` for colour types 0, 2 and 3 |
| filters | all five (none/sub/up/average/paeth) |
| compression | its own inflate: stored, fixed and dynamic Huffman, zlib wrapper, Adler-32 **checked** |
| chunk CRCs | checked; a mismatch is an error, not a warning |

Ancillary chunks are skipped, which means `gAMA`/`sRGB`/`iCCP` are
**ignored** (an icon does not carry a meaningful one) and an **APNG decodes
as its first still frame**, which is what an icon loader wants.

## What it refuses

**Adam7 interlacing**, with `Error::Interlaced`. Interlacing exists so a
picture can be shown progressively over a slow link; an icon is read from a
local file in microseconds. Supporting it is seven passes of the whole
unfilter-and-deinterleave path for a case that does not occur — and that
last part is checked rather than asserted: `tests/corpus.rs` surveys every
PNG installed on the machine and reports the interlaced count. It was **0
of 6455** on the dev box (all of `/usr` and `/opt`, not just icons) and 0 of
13 on the test box.

Everything else malformed returns an `Error`. It does not panic on any
input, and `tests/fuzz.rs` is the evidence: every corpus file is truncated
at twenty pseudo-random offsets, bit-flipped at twenty more, and bit-flipped
inside `IHDR` at twenty more *with the CRCs recomputed* so the mutation
reaches the decoder instead of being caught at the door. The seed is a
fixed function of the path, so a failure is reproducible.

## Tests

- `src/*.rs` — unit tests: fixtures, the Paeth predictor against the spec's
  arithmetic, sub-byte sample unpacking, stride/bpp for every colour type,
  round trips through a real compressor (python's zlib, if present).
- `tests/corpus.rs` — decodes every PNG under `/usr/share/icons` and
  `/usr/share/pixmaps`, asserting the buffer size matches the reported
  dimensions and that alpha is binary for the colour types that have none.
  **Skips, with a message, when no such files exist**, so a CI container
  without an icon theme does not fail. It also decodes each file a second
  time through `naive_unfilter` + `naive_expand` — the specification's §9.2
  and §7.2 pseudocode transcribed the slow, obvious way — and requires the
  two to be byte-identical. The real decoder is `const`-generic per `bpp`
  with a head/body split and a palette table hoisted out of the row loop;
  that is precisely where a transcription error hides from a test sharing
  its structure, and the naive version shares none of it.
- `tests/corpus.rs`'s synthetic half, which exists because **the installed
  corpus cannot exercise the filters**. A census of all 2602 scanlines of
  the 70 PNGs on the dev box: filter 0 ×2096, filter 1 ×88, filter 2 ×210,
  **filter 3 ×0**, filter 4 ×208. So the cross-check run on real files never
  executes the average filter at all, and its Paeth rows are flat enough
  that a tie-break mutation changes nothing. Thirty-five PNGs are therefore
  built in the test — every filter against every colour type, over
  deliberately non-smooth samples so `a`, `b` and `c` differ — and checked
  both against the bytes they were built from and against the naive path.
  Verified by mutation: rounding the average filter up, flipping the Paeth
  `b`/`c` tie, moving the row-head boundary by one, taking the low byte of a
  16-bit sample and swapping the palette's channel order are all **caught**;
  before the synthetic inputs, none of them were.
- `tests/fuzz.rs` — the no-panic property above.

The correctness evidence that mattered most is not in this repository: it
was a byte-for-byte pixel diff against the `png` crate over 6455 files,
which found a real bug (`tRNS` entries are 2 bytes big-endian at *every*
bit depth; reading the high byte made every depth-8 colour key compare as
zero, silently turning black pixels transparent). A test written from the
same misreading of the specification as the code would not have caught it.
The harness is recorded in the task thread rather than kept, because it
requires the dependency this crate exists to avoid.

## No consumer yet

Nothing in the workspace decodes a PNG today. This crate is groundwork for
the icon work — XDG icon themes are largely PNG — and it stays dependency-
free so that wiring it into the server costs exactly one path entry.
