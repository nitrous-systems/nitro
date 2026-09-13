# Size and memory budget

Goal 2 of `DESIGN.md` is "small": low memory, few dependencies, short
build. This is the table that keeps it honest. Regenerate the dev-machine
rows with `just size` (and `just size 5` for the five-window row); the box
rows come from the same binaries running on the test box, which is the
number that actually matters — 3.3 GB and **no swap**.

All figures are release builds, measured after this branch was rebased
onto M2-pre (`nitro-text`). That matters: text moved almost every number
on this page. The server was **2.5× over its RSS budget** because of it;
the lazy font loading of issue #528 has since brought it back inside. See
the resident-memory section for both numbers.

The `nitro-calc` rows and section are the M2 exit measurement (#3684),
taken on the box against the server as it stood at #528. The app numbers
do not move with the server's: a client's own RSS and binary size are
independent of how the compositor stores its fonts.

`[profile.release]` sets `strip = true`, so the binaries are already
stripped and a separate `strip(1)` pass would measure nothing but whether
the tool is installed.

## Binaries

| binary | bytes | budget | verdict |
|---|---|---|---|
| `nitro-calc` | 560 360 | ≤ 1 MB (client) | **ok**, 56 % of budget |
| `nitro-demo` | 481 240 | ≤ 1 MB (client) | **ok**, 48 % |
| `hello_client` | 393 344 | ≤ 1 MB (client) | **ok**, 39 % |
| `hey` | 368 008 | — | ok |
| `nitro-shot` | 330 160 | — | ok |
| `nitro-server` | 2 047 016 | — | see below |

Identical on the dev machine and the box (same artefact, `rsync`ed, and
`sha256sum`-verified on both sides for the M2 row below).

**`nitro-calc` is the number M2 exists to produce**: a complete
application — two labels, twenty buttons, a state machine, a formatter
and a scriptable introspection socket — in **560 KB**, against a 1 MB
client budget. It carries no font library, no rasterizer and no
compositor: a label is a string on the wire and the server owns the
glyphs. Of that, ~68 KB is the introspection protocol, monomorphised per
app-state type (`docs/ui.md`, *Measured*); the de-monomorphisation that
would share one copy across a program is M3.

It is 79 KB above `nitro-demo`, and that gap is the toolkit's own cost:
the arena, eleven widgets, the flex solver, the passes and the socket,
against `nitro-demo` building its scene by hand. Buying a widget toolkit
for 79 KB over talking to the wire directly is the trade goal 2 asks
for, and it is the reason the toolkit exists.

The server tripled, from 737 736 bytes before M2-pre to 2 047 016 after:
that is `swash` and its shaping tables, and it buys text end to end. There
is no stated binary-size budget for the server — it is one process on a
desktop, not a per-app cost — so this is recorded rather than flagged. The
*client* number is the one with a budget, because it is paid once per
running app, and it did not move: `nitro-demo` grew 3 280 bytes across the
rebase and neither client links the text engine at all. That split is
working exactly as intended — shaping lives in the server, so a client
sends a string and pays nothing for the machinery that draws it.

(The server grew 49 864 bytes against the same tree without this change —
1 997 152 → 2 047 016 — which is the lazy font index, its LRU and the
hand-written cache format. It bought 11.4 MB of RSS back, a trade worth
making twice. The 1 988 608 figure this page carried before is from an
earlier commit on this branch.)

The two clients remain within a hundred kilobytes of each other despite
`nitro-demo` doing considerably more, which is the useful signal: nearly
all of a client's size is `nitro-wire` plus the Rust runtime and panic
machinery, not its own code. A toolkit client starts from about the same
floor.

## Resident memory

`VmRSS` is what is resident at the sample; `VmHWM` is the high-water mark
— the peak is what matters on a box with no swap, and it is the number a
steady-state `top` never shows you.

### Test box (real KMS, 1920×1080@60)

| process | windows | VmRSS | VmHWM | budget | verdict |
|---|---|---|---|---|---|
| `nitro-server` | 1 | 8 240 kB | 8 240 kB | ≤ 8 MB with 5 windows | over by 1 % — see below |
| `nitro-server` | 5 | 8 344 kB | 8 344 kB | ≤ 8 MB with 5 windows | over by 2 % — see below |
| `nitro-calc` | 1 | **2 752 kB** | **2 752 kB** | ≤ 3 MB (client) | **ok**, 92 % |
| `nitro-demo` | 1 | 3 132 kB | 3 132 kB | ≤ 3 MB (client) | over by 4 % |
| `nitro-demo` | 5 | 3 224 kB | 3 224 kB | ≤ 3 MB (client) | over by 7 % |

**The first app is inside the client budget**, at 2 752 kB against 3 MB,
and it stays there: 2 776 kB after 120 keypresses, so typing allocates
nothing that is not freed. It is *lighter* than `nitro-demo` despite
being a real application with a widget tree, because `nitro-demo` uploads
a 16 kB image and keeps two frames of scratch. One thread, and no second
one anywhere — the introspection socket is served by the app's own
`epoll` loop between events, which is what makes "scriptable" cost
neither a thread nor a lock.

**The server is back inside its budget**, within rounding: 19 676 → 8 240 kB
with one window, 19 804 → 8 344 kB with five. The fix is #528's: `nitro-text`
no longer reads every font file at scan time and keeps it. It indexes the
faces (family, attributes, `(path, face index)`), reads a file only when a
face is actually shaped or rasterized with, caps what is resident with
`NITRO_FONT_CACHE_MB` (default 8 MB), and hands the bytes back when the event
loop next goes idle — which is exactly the state this table is measured in.
The atlas keeps the rendered masks, so a release costs one re-read the next
time a *new* glyph appears (53 µs against 20 µs for a warm line) and never a
redraw. On the box: 47 faces indexed, **0 bytes** of font data resident in the
settled state, 1.9 MB at the moment a label is being painted.

The 48–152 kB over 8 192 is flagged rather than declared a pass. It is not
fonts: with `NITRO_FONT_DIRS` pointed at nothing the same binary sits at
8 012 kB, so the floor is the binary's own text and data (1.9 MB resident of
a 2.0 MB image), libc, libinput/libudev/libglib pulled in by the seat, and one
1 MiB atlas page. Those are the things to attack next if 8 MB is to be a hard
ceiling; the per-process font cost, which is what blew the budget, is gone.

With a text-heavy client (`hello_client`, three labels in two faces) the same
server measures 8 804 kB settled and peaks at 10 632 kB (`VmHWM`) while the
faces are loaded and the glyphs rasterized — the peak is the honest number for
a box with no swap, and it is the one the cap bounds.

Five windows cost the server **104 kB** over one — 21 kB per window, which is
the scene nodes (18 per window) and the per-client id maps, and nothing that
scales with pixels. The server's framebuffers are not in RSS: they are dumb
buffers owned by the GPU and mapped, not anonymous memory.

The scan also stopped costing a 12 MB read per boot: the index is cached in
`$XDG_CACHE_HOME/nitro/fonts.idx`, validated against the directory walk (paths,
sizes, mtimes) and discarded on any mismatch. Box: **5.2 ms cold, 0.4 ms
warm**, 47 faces, 5 520-byte cache file.

The client is over its 3 MB budget by 4–7 %, and is flagged rather than
quietly rounded down. It is worth noting what it is *not*: five windows
cost it 92 kB, so this is fixed overhead — the Rust runtime, the wire
buffers, the 16 kB image it uploads — not a leak or a per-window cost. A
client that opens five windows for the price of one is the property worth
having; the constant is what to attack if 3 MB is a hard limit, and the
first place to look is the 64 KiB `recv` scratch buffer each `Socket`
allocates.

### Dev machine (fake backend, 1280×720)

| process | windows | VmRSS | VmHWM |
|---|---|---|---|
| `nitro-server` | 1 | 13 824 kB | 13 824 kB |
| `nitro-server` | 5 | 14 092 kB | 14 092 kB |
| `nitro-calc` | 1 | 2 632 kB | 2 632 kB |
| `nitro-demo` | 1 | 2 904 kB | 2 904 kB |
| `nitro-demo` | 5 | 3 040 kB | 3 040 kB |

The server is 5.5 MB *heavier* here than on the box, and the reason is the
fake backend, not the fonts: it keeps its "framebuffers" as ordinary heap
allocations (two 1280×720×4 buffers = 7 MB) where real KMS puts them in
GPU-owned dumb buffers outside RSS. `nitro-demo` on the fake backend draws
no text, so no font file is ever loaded on this row — which is why the two
machines no longer differ by what they happen to have installed. The box
row is the one to quote; it is the one with a budget attached.

## First frame on the wire

What a client spends to get its first pixels on screen, counted by
`nitro-demo --stats` (`wire:` line). `tx_bytes` is every byte of every
message it sent, framing included; the image pixels are **not** in it —
they travel through a memfd passed by `SCM_RIGHTS`, which is the whole
point of the buffer design.

| | box | dev machine |
|---|---|---|
| `tx_bytes` (client → server, first frame) | 1 755 | 1 972 |
| `rx_bytes` (server → client) | 120 | 152 |
| `connect()` → first `Presented` | 31.6 ms | 18.5 ms |

**Under two kilobytes** to put a window with a gradient, four bordered
rounded rects, an image and ten more nodes on screen — 18 scene nodes for
1 755 bytes, under 100 bytes a node. That is the number that matters for
goal 4 (remote from the same code): a full first frame fits in a single
TCP segment, and subsequent frames are far smaller (a pointer-follow
commit is ~180 bytes).

The two machines differ by a couple of hundred bytes because the window is
configured to a different size on a 1280×720 fake output than on
1920×1080, so the `SetBounds` payloads differ slightly — there is no
per-machine overhead in the protocol.

`connect()` → first `Presented` is dominated by waiting for a vblank, not
by work: the handshake, the transaction and the first paint together are
well under a millisecond. It varies run to run by roughly a frame
depending on where in the refresh cycle the client happens to connect.

## The first app: `nitro-calc`

M2's exit criterion, measured on the box against the real KMS server with
`sha256sum` verified on both sides before the run.

| what | value |
|---|---|
| app source, excluding tests | **489 lines** (292 engine + 197 UI) |
| binary, release, stripped | **560 360 bytes** (560 KB) |
| RSS / HWM on the box | **2 752 kB**, and 2 776 kB after 120 keypresses |
| threads | 1 |
| idle CPU over 10 s, app *and* server | **0.0 %**, 0 voluntary context switches each |
| one keypress on the wire | **2 mutations: `SetText`, `Commit`** |
| keypress-to-photon (server `i2p_*`, 3 runs of 60 presses) | **min 1.9 ms, mean 11.7–13.3 ms, max 36.6–43.7 ms** |

![nitro-calc on the test box](calc-box.png)

That screenshot is `hey nitro-calc shot`, downscaled 2× — the app
screenshotting itself over its own introspection socket, from an ssh
session, with no cooperation from its code. The PNG is 223×334, which is
exactly the window's `bounds` (`0,0,223,334`), not the 1920×1080 output.

**One keypress is one `SetText` and the `Commit` that carries it.** Not
"about one" — the mutation tap records exactly `["SetText", "Commit"]`,
asserted by `crates/nitro-calc/tests/calc.rs::one_keypress_is_one_set_text`.
The twenty buttons do not repaint, nothing is re-measured but the label,
and the history line stays silent because its string did not change.
That is the whole retained-tree claim, checked from outside the process.

**Idle is zero, not low.** Ten seconds with the app on screen and the
server running: no CPU time and no voluntary context switches in either
process. Both block in `epoll_wait` and no bytes move. (An app with a
`hey watch` attached is the documented exception, waking 100×/s; see
`docs/introspection.md`.)

### On the latency number

The i2p figures are the **server's** view (libinput timestamp → vblank of
the frame that consumed it), which is the only view available here:
`nitro-demo` measures the client half by instrumenting itself, and
`nitro-calc` is an ordinary app with no stopwatch in it. Three runs of 60
`ydotool` keypresses at ~4/s agree to within 1.6 ms of mean — the third
after the #528 font rework, which did not move the app's numbers.

The mean sits above the 9.3 ms `nitro-demo` reports in `docs/latency.md`,
and the difference is real rather than noise: a keypress makes the client
**re-measure a new string** (`MeasureText` is a synchronous round trip in
M2; `docs/ui.md`) and only then commit, where a pointer-follow frame
commits from state the client already has. Every digit is a string the
cache has not seen, so every keypress pays one turnaround. The cache is
why the number is a per-*string* cost and not a per-frame one, and the
async measurement path is M3 — this is the first measurement that puts a
price on that decision.

The run was checked against the trap `docs/latency.md` section 5 warns
about: **1.7 flips/s** over the typing run, nowhere near the 60.0 that
would mean the queue was full and the number was backlog rather than
latency.

## Dependency count

`cargo tree -e normal --prefix none | sort -u | wc -l` = **64**, matching
`DEPENDENCIES.md`. Several of those lines are cargo's `(*)` markers for
already-printed subtrees and several more are our own workspace crates;
**distinct external crate names are still 35**.

`nitro-calc` adds **zero** of them, which is the point worth recording:
the first real application on the toolkit needed nothing that was not
already in the tree. Its only dependency is `nitro-ui`, and the four
lines the count rose by are the new `nitro-calc` and `nitro-ui (*)`
entries, not new crates. `hey` likewise depends on `std` and `rustix`
alone.

`nitro-demo` adds **zero** of them: it uses `nitro-wire`, `nitro-core`,
`rustix` and `signal-hook`, all of which the tree already carried. Its PNG
writer for `--save-small` is a small deflate encoder rather than the `png`
crate, for the same reason `nitro-shot` has one. The rise from 48 at M1 to
60 is `swash` and its seven transitive crates (M2-pre text), plus the
`nitro-demo` and second `signal-hook (*)` lines that branch added;
`DEPENDENCIES.md` has the per-crate justification for each.
