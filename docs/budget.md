# Size and memory budget

Goal 2 of `DESIGN.md` is "small": low memory, few dependencies, short
build. This is the table that keeps it honest. Regenerate the dev-machine
rows with `just size` (and `just size 5` for the five-window row); the box
rows come from the same binaries running on the test box, which is the
number that actually matters — 3.3 GB and **no swap**.

All figures are release builds. `[profile.release]` sets `strip = true`,
so the binaries are already stripped and a separate `strip(1)` pass would
measure nothing but whether the tool is installed.

## Binaries

| binary | bytes | budget | verdict |
|---|---|---|---|
| `nitro-demo` | 476 992 | ≤ 1 MB (client) | **ok**, 47 % of budget |
| `hello_client` | 387 584 | ≤ 1 MB (client) | **ok**, 38 % |
| `nitro-shot` | 330 160 | — | ok |
| `nitro-server` | 737 736 | — | ok |

Identical on the dev machine and the box (same artefact, `rsync`ed).

The two clients are within a hundred kilobytes of each other despite
`nitro-demo` doing considerably more, which is the useful signal: nearly
all of a client's size is `nitro-wire` plus the Rust runtime and panic
machinery, not its own code. A toolkit client will start from about the
same floor.

## Resident memory

`VmRSS` is what is resident at the sample; `VmHWM` is the high-water mark
— the peak is what matters on a box with no swap, and it is the number a
steady-state `top` never shows you.

### Test box (real KMS, 1920×1080@60)

| process | windows | VmRSS | VmHWM | budget | verdict |
|---|---|---|---|---|---|
| `nitro-server` | 1 | 7 468 kB | 7 468 kB | ≤ 8 MB with 5 windows | **ok** |
| `nitro-server` | 5 | 7 592 kB | 7 592 kB | ≤ 8 MB with 5 windows | **ok**, 93 % of budget |
| `nitro-demo` | 1 | 3 168 kB | 3 164 kB | ≤ 3 MB (client) | **over by 5 %** |
| `nitro-demo` | 5 | 3 164 kB | 3 164 kB | ≤ 3 MB (client) | **over by 5 %** |

Five windows cost the server **124 kB** over one — 25 kB per window, which
is the scene nodes (18 per window) and the per-client id maps, and nothing
that scales with pixels. The server's framebuffers are not in RSS: they
are dumb buffers owned by the GPU and mapped, not anonymous memory.

The client is over its 3 MB budget by about 160 kB, and is flagged here
rather than quietly rounded down. It is worth noting *what* it is not: the
figure does not move between one and five windows (3 164 kB either way),
so this is fixed overhead — the Rust runtime, the wire buffers, the 16 kB
image the demo uploads — and not a leak or a per-window cost. A client
that opens five windows for the price of one is the property worth having;
the constant is the thing to attack if 3 MB is a real limit, and the first
place to look is the 64 KiB `recv` scratch buffer each `Socket` allocates.

### Dev machine (fake backend, 1280×720)

| process | windows | VmRSS | VmHWM |
|---|---|---|---|
| `nitro-server` | 1 | 13 312 kB | 13 312 kB |
| `nitro-demo` | 1 | 2 968 kB | 2 968 kB |

The server is 5.8 MB heavier here than on the box, which is the fake
backend, not a regression: `FakeBackend` keeps its "framebuffers" as
ordinary heap allocations (two 1280×720×4 buffers = 7 MB), while on real
KMS those live in GPU-owned dumb buffers outside RSS. It is a useful
reminder that the fake backend is not a memory model, and that the box row
is the one to quote.

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
| `connect()` → first `Presented` | 24.1 ms | 18.6 ms |

**Under two kilobytes** to put a window with a gradient, four bordered
rounded rects, an image and ten more nodes on screen — 18 scene nodes for
1 755 bytes, under 100 bytes a node. That is the number that matters for
goal 4 (remote from the same code): a full first frame fits in a single
TCP segment, and subsequent frames are far smaller still (a pointer-follow
commit is ~180 bytes).

The two machines differ by a couple of hundred bytes because the window is
configured to a different size on a 1280×720 fake output than on
1920×1080, so the `SetBounds` payloads differ slightly — there is no
per-machine overhead in the protocol.

`connect()` → first `Presented` is dominated by waiting for a vblank
(16.7 ms of it is one refresh), not by work: the handshake, the
transaction and the first paint together are well under a millisecond.

## Dependency count

`cargo tree -e normal --prefix none | sort -u | wc -l` = **48**.
`nitro-demo` adds **zero** new dependencies: it uses `nitro-wire`,
`nitro-core`, `rustix` and `signal-hook`, all of which the tree already
carried. Its PNG writer for `--save-small` is thirty lines of deflate
rather than the `png` crate, for the same reason `nitro-shot` has one.
