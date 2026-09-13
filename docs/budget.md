# Size and memory budget

Goal 2 of `DESIGN.md` is "small": low memory, few dependencies, short
build. This is the table that keeps it honest. Regenerate the dev-machine
rows with `just size` (and `just size 5` for the five-window row); the box
rows come from the same binaries running on the test box, which is the
number that actually matters — 3.3 GB and **no swap**.

All figures are release builds, measured after this branch was rebased
onto M2-pre (`nitro-text`). That matters: text moved almost every number
on this page, and the server is now **well over its RSS budget** because
of it. See the flagged row below and issue #528.

`[profile.release]` sets `strip = true`, so the binaries are already
stripped and a separate `strip(1)` pass would measure nothing but whether
the tool is installed.

## Binaries

| binary | bytes | budget | verdict |
|---|---|---|---|
| `nitro-demo` | 480 272 | ≤ 1 MB (client) | **ok**, 48 % of budget |
| `hello_client` | 393 264 | ≤ 1 MB (client) | **ok**, 39 % |
| `nitro-shot` | 330 160 | — | ok |
| `nitro-server` | 1 988 608 | — | see below |

Identical on the dev machine and the box (same artefact, `rsync`ed).

The server tripled, from 737 736 bytes before M2-pre to 1 988 608 after:
that is `swash` and its shaping tables, and it buys text end to end. There
is no stated binary-size budget for the server — it is one process on a
desktop, not a per-app cost — so this is recorded rather than flagged. The
*client* number is the one with a budget, because it is paid once per
running app, and it did not move: `nitro-demo` grew 3 280 bytes across the
rebase and neither client links the text engine at all. That split is
working exactly as intended — shaping lives in the server, so a client
sends a string and pays nothing for the machinery that draws it.

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
| `nitro-server` | 1 | 19 676 kB | 19 676 kB | ≤ 8 MB with 5 windows | **OVER, 2.5×** — see #528 |
| `nitro-server` | 5 | 19 804 kB | 19 804 kB | ≤ 8 MB with 5 windows | **OVER, 2.5×** — see #528 |
| `nitro-demo` | 1 | 3 132 kB | 3 132 kB | ≤ 3 MB (client) | over by 4 % |
| `nitro-demo` | 5 | 3 224 kB | 3 224 kB | ≤ 3 MB (client) | over by 7 % |

**The server is the headline failure and it is not this crate's doing.**
Before M2-pre the same measurement on the same box was 7 468 kB — inside
budget at 93 %. `nitro-text`'s `FontDb` reads every font file's bytes at
scan time and holds them for the process lifetime (47 faces on this box),
which is +12.2 MB and is exactly what issue **#528** tracks, with a fix
proposed: index lazily, load a face's bytes on first use. Nothing about
the window count touches it — one window and five differ by 128 kB — so
the budget line as written (≤ 8 MB *with 5 windows*) is not the thing that
broke; the per-process floor is.

Recorded here rather than quietly left at the old figure, because a budget
document whose numbers predate the commit that blew the budget is worse
than no budget document. The M1-era 7.5 MB figure still appears in
`crates/nitro-server/README.md` and `DESIGN.md`'s M1 bullet; the latter is
a historical milestone record and correct as such.

Five windows cost the server **128 kB** over one — 26 kB per window, which
is the scene nodes (18 per window) and the per-client id maps, and nothing
that scales with pixels. The server's framebuffers are not in RSS: they
are dumb buffers owned by the GPU and mapped, not anonymous memory.

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
| `nitro-server` | 1 | 17 244 kB | 17 244 kB |
| `nitro-server` | 5 | 17 440 kB | 17 440 kB |
| `nitro-demo` | 1 | 2 916 kB | 2 916 kB |
| `nitro-demo` | 5 | 2 996 kB | 2 996 kB |

The server is 2.4 MB *lighter* here than on the box, which is two effects
cancelling and is worth stating so nobody reads it as the box being
pessimistic. The fake backend keeps its "framebuffers" as ordinary heap
allocations (two 1280×720×4 buffers = 7 MB) where real KMS puts them in
GPU-owned dumb buffers outside RSS — that pushes the dev figure *up*. The
dev machine also has a different, smaller set of installed fonts for
`FontDb` to slurp, which pushes it *down* by rather more. The box row is
the one to quote.

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

## Dependency count

`cargo tree -e normal --prefix none | sort -u | wc -l` = **60**, matching
`DEPENDENCIES.md`. Thirteen of those lines are cargo's `(*)` markers for
already-printed subtrees and several more are our own workspace crates;
**distinct external crate names are 35**.

`nitro-demo` adds **zero** of them: it uses `nitro-wire`, `nitro-core`,
`rustix` and `signal-hook`, all of which the tree already carried. Its PNG
writer for `--save-small` is a small deflate encoder rather than the `png`
crate, for the same reason `nitro-shot` has one. The rise from 48 at M1 to
60 is `swash` and its seven transitive crates (M2-pre text), plus the
`nitro-demo` and second `signal-hook (*)` lines this branch adds;
`DEPENDENCIES.md` has the per-crate justification for each.
