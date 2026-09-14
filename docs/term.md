# nitro-term — the terminal

`nitro-term` is the M4-A application: a terminal emulator written against
`nitro-ui`, and the app that makes the test box usable. It exists for two
reasons, and the second is the interesting one.

It is **the hardest test of the design's first claim**. `DESIGN.md` goal
1 says work is proportional to what changed and idle costs nothing. A
calculator changes one label per keypress and a bar changes a clock once
a minute; both are easy. A terminal is not, because the program on the
other end of the pty has never heard of a display server: `yes` writes a
line every microsecond, `htop` repaints eighty rows twice a second, and
`cat` of a binary file changes every cell on screen. Either the claim
survives contact with that or it was a claim about toy workloads.

```text
        ┌──────────────────────────────────────────┐
        │ $ ls --color                             │  TermGrid, named `grid`
        │ Cargo.toml  crates/  docs/               │
        │ $ █                                      │
        └──────────────────────────────────────────┘
             ▲                           │
       bytes │                           │ keys, xterm-encoded
             │                           ▼
        pty master ◄──────────── setsid --ctty $SHELL
```

## The model

Four pieces, each a module, each testable without the one above it.

| module | what it is |
|---|---|
| `pty` | the pseudoterminal and the child shell |
| `vt` | the escape-sequence parser: bytes → grid operations |
| `grid` | rows of cells, scrollback, alt screen, **damage** |
| `widget` | `TermGrid`: grid → scene nodes, and the damage strategy |

### The pty, and why there is no `unsafe`

A terminal's child needs two things that are normally done between the
`fork` and the `exec`: its own session (`setsid`) and the pty as its
controlling terminal (`TIOCSCTTY`). The standard library's door to that
is `CommandExt::pre_exec`, which is `unsafe` — and this tree denies
`unsafe` workspace-wide.

The answer is a program that already does exactly those two syscalls in
exactly that window: util-linux's **`setsid --ctty`**. The child is
spawned as `setsid --ctty $SHELL` with the pty slave as all three
standard descriptors, and the kernel work happens inside a process we did
not have to write.

This is the same shape of argument `nitro-launcher` makes in
`src/spawn.rs` — there the conclusion was that `process_group(0)` is the
safe half of `setsid` and the only half a launcher needs. A terminal
needs the other half too, so it pays for it with a dependency on a
binary rather than with an `unsafe` block.

**If `setsid` is missing**, the terminal still starts: a plain `Command`
with `process_group(0)`, a warning on stderr, and `has_job_control()`
answering `false`. What is lost is job control — Ctrl-C reaches nothing,
because there is no foreground process group to signal. That is a real
degradation and it is reported rather than hidden.

### The grid, and what damage means here

`Grid` is `Vec<Cell>` rows plus a scrollback ring (10 000 lines by
default, `--scrollback N`), an alternate screen with no scrollback, and
per-row damage recorded as a **column span**: every write widens
`(min_col, max_col)` for its row, and `clear_damage` resets it once the
damage has been drawn.

The span is not decoration. It is the contract between the model and the
widget: a byte stream marks spans, the widget reads them, and every
`SetText` on the wire is traceable back to a cell that actually changed.

## The damage strategy

This is the part worth reading, because it is where the milestone's claim
is either kept or lost. Three mechanisms, in the order they matter.

**1. A Text node per same-style run, in a stable slot.** A row is split
into maximal runs of cells sharing a style (`Grid::row_runs`). Each run
becomes one `SetText`; each run whose background is not the window's also
gets one rect. The slot number is `row * 64 + k` (32 runs, each with a rect and a text node), so a run keeps the same
scene node from frame to frame, and the toolkit's per-slot cache does the
diffing: **a run that produced the same bytes as last time costs nothing
on the wire**, even though `paint` walked over it.

A run with the *default* background gets no rect at all — the window's
backdrop is already that colour. Most of a normal screen is default
background, so this is most of the rects.

**2. A row that did not change is not walked.** `paint` asks
`Grid::row_dirty` first, and for a clean row calls `PaintCx::keep`, which
tells the framework the row's slots are unchanged without re-deriving
them. No run splitting, no string comparison, no allocation. On a
keystroke that is one row out of fifty.

`keep` is new in `nitro-ui` and is the one general thing this app needed
from the toolkit. Every built-in widget's slots are its *parts* — a
button that stops drawing a focus ring wants the ring destroyed, which is
what "a slot the paint did not emit is destroyed" gives it. A terminal's
slots are its *content*, and it needs a third answer: not "emit" and not
"omit" but "unchanged". See `docs/ui.md`.

**3. The scene is touched once per frame, not once per byte.** Bytes are
drained from the pty the moment they arrive — the child must never block
on a full pipe — but they go only into the grid, which marks the widget
dirty. The *scene* catches up in the app's frame callback
(`Ui::request_frame`, also new). So a hundred thousand lines cost one
commit per refresh, each showing the grid as it stood at that moment. The
intermediate states were never visible and did not need to be drawn.

And when nothing is happening, nothing is asked for: no frame is
requested when the grid has no damage, so the app sits in `epoll_wait`
with no timer and no callback pending.

### The trap in the third mechanism

"Read until `WouldBlock`, then take a frame" is wrong, and the first
throughput measurement is what caught it. `WouldBlock` never arrives
while the writer is faster than the reader, so `cat` of a 5 MB file was
consumed in **one** drain and produced **one** commit. By the letter of
the pacing claim that is a perfect score; what it actually describes is a
terminal that showed nothing for two and a half seconds and then jumped
to the end.

So a drain reads at most 256 KiB — about four screens of dense output —
and hands the loop back. The pacing argument is untouched (a frame still
has more than it can show), but the screen now keeps up with the stream
instead of waiting for it to finish.
`a_fast_writer_does_not_starve_the_screen` asserts it, and it is the one
test in the file that argues for *more* work rather than less.

## What a keystroke costs

Measured from outside, by counting mutations through the harness's tap
(`one_keystroke_costs_two_mutations`):

| | mutations |
|---|---|
| the **first** key on a row | 4 — `CreateNode`, `SetBounds`, `SetText`, and the cursor's `SetBounds` |
| **every key after it** | **2** — `SetText` for the run, `SetBounds` for the cursor |

The first key on a row is where that row's text node comes into
existence; it happens once per row rather than once per key.

Getting the steady state to two took one non-obvious decision. A text
node's box runs to the **end of the row**, not to the end of its run. The
width is not visible — a run is drawn left-aligned and unwrapped, so the
glyphs stop where the string does whatever box they sit in — but it *is*
diffed, and a box sized to the run grows by one cell per typed character,
which put a `SetBounds` next to every `SetText`. Sized to the row, the
box is identical on every repaint and only the string moves.

## The numbers

Measured on the test box (Pentium G3240, `docs/testbox.md`).

> **Pending the box run.** The box was claimed by task 3702 when this
> branch was finished; the numbers below are filled in by the acceptance
> run and this note is removed with them. Everything above is asserted by
> the test suite on the dev machine and does not depend on them.

| | nitro-term | Linux console (reference) |
|---|---|---|
| `time seq 1 1000000` | — | — |
| `time cat` (5 MB) | — | — |
| frames during it | — | — |
| `paint_us_mean` | — | — |
| `damage_px_mean` | — | — |

| | |
|---|---|
| keystroke i2p, median / p95 | — |
| idle, 30 s at a prompt | — frames, — ticks |
| `htop` running | — frames/s, `damage_px` — |
| RSS, 10 000 lines of scrollback | — |
| binary | **646 664 bytes** (dev machine), budget ≤ 900 KB |
| server RSS before / after | — |
| `atlas_pages` | — |

The binary is the one number that is a property of the code rather than
the hardware, so it is quoted now: **646 KB against a 900 KB budget**,
with a VT parser, a grid model, a scrollback ring and a pty in it. The
comparison worth making is `nitro-calc` at 560 KB — a terminal is 86 KB
more application than a calculator, because both are "a widget tree and a
state struct" and neither contains a font, a rasterizer or a compositor.

## Dependencies

`vte` (+ `arrayvec`, `memchr`) and `rustix`, on top of `nitro-ui`. The
argument for `vte` is in `DEPENDENCIES.md`; the short form is that it is
the state machine and not the terminal — it assigns no meaning to what it
parses, so every escape sequence's *effect* is ours and is tested here.

## Limitations

Each of these is a real feature rather than a missing case, and each is
recorded because the spec asks for them rather than because they are
regrets.

* **No clipboard.** `Ctrl+Shift+C`/`V` are deliberately unbound. A
  clipboard is a server-side concept — a selection owner, and a protocol
  for offering and requesting types — and nitro does not have one yet.
  The *bracketed paste* plumbing is in place and tested (`keys::paste`,
  DECSET 2004), so the day there is a selection owner the terminal side
  is one call.
* **No scrollback rewrap.** Resizing reflows the screen but not the
  history: a narrowed window truncates old lines rather than re-wrapping
  them. Rewrap means re-deciding where every historical line broke, which
  needs the scrollback to store *logical* lines rather than rows — a
  different data structure, not a missing loop.
* **No mouse reporting.** A program that asks for mouse events (DECSET
  1000/1002/1006) does not get them; the wheel scrolls the scrollback
  instead. The sequences are parsed and ignored, so nothing breaks.
* **No sixel, no inline images, no ligatures, no bidi.** The first two
  need a wire op; the last two are the server's text layer rather than
  the terminal's.
* **32 style runs per row.** A row with more distinct style runs than
  that draws the first 32. It takes a colour-test pattern to reach — real
  output is a handful of runs per row — and the alternative, allocating
  slots dynamically, would make one row's node numbering depend on
  another's, which is exactly what the per-slot diff cannot tolerate.
* **A wide character is width 2 from a hand-rolled table.** East Asian
  Wide/Fullwidth plus the emoji blocks, taken whole; a rare dingbat may
  be one column wrong. The alternative is a Unicode-width dependency
  whose table we would have to trust anyway.
* **Combining marks are dropped** rather than composed onto the previous
  cell. They take no cell, so nothing shifts; the mark is simply not
  drawn.
* **No reflow of the cursor's logical line on resize**, and no
  `SIGWINCH`-time redraw beyond what the child chooses to send.
