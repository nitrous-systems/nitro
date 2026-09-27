# How nitro's numbers compare to other desktops

Until this page existed, no external number appeared anywhere in the tree.
[`latency.md`](latency.md), [`bench.md`](bench.md) and
[`budget.md`](budget.md) measure nitro carefully. They could not tell a
reader whether 9.3 ms median input-to-photon or a 2.4 MB `RssAnon` is good.
This page puts the published figures that exist next to ours, and it records
the places where no published figure exists.

**The governing rule: a comparison is only valid inside one metric class.**
Almost every published desktop number measures something different from
ours. Photon numbers include the mouse and the panel, `free -m` includes
the kernel, and x11perf counts requests. So every row below restates its
class, and every external figure carries its source, year and hardware.
Figures marked **[derived]** are arithmetic on a published number rather
than the number itself.

Our box is a Pentium G3240: Haswell, 2 cores, SSE4.2, no AVX2. It has
3.3 GB of RAM, an Intel HD HSW GT1 on `i915`, and HDMI-A-1 at 1920×1080@60
(`docs/testbox.md`). Nitro is a CPU compositor, with no GPU compositing
path. Every external rig quoted below is far stronger: a desktop GPU, a
120–500 Hz panel, or both. That gap works *against* us in every row, and
it needs stating once, here, rather than in every row.

The research behind this page was done on 2026-09-20. §5 lists what was
searched for and not found.

## 1. Latency

### Comparable: compositor-internal input → present

Our metric is defined in [`latency.md`](latency.md) §1. It starts at the
libinput event timestamp. The client view ends at `Presented.time_ns` for
the commit that answered the motion, which covers the whole round trip
through the client. The server view (`i2p_*`) ends at the vblank of the
frame that consumed the event. Neither view includes USB polling or panel
response.

Only one published measurement is in the same class. farnoy.dev (2026)
instrumented **KWin 6.6.4 on Wayland** from Chromium's input event to
present/pageflip, using `KWIN_LOG_PERFORMANCE_DATA`. That excludes USB,
the panel and pixel transition, like ours does. The rig is Zen 4, an RTX
Ada GPU and an LG C1 at **120 Hz** over HDMI, on NixOS.

| system | statistic | value | endpoint | refresh | hardware |
|---|---|---|---|---|---|
| **nitro**, 202 samples ([`latency.md`](latency.md) headline, §4.1) | **median / p95 / min / max** | **9.3 / 17.1 / 1.3 / 19.2 ms** | input → client `Presented` | 60 Hz | Pentium G3240, HD GT1 |
| **nitro**, three interleaved pairs ([`latency.md`](latency.md) §5, #3718) | **mean** of `i2p_mean_us` | **6 362 / 6 312 / 6 181 µs ≈ 6.3 ms** (60 Hz arm: 9 928 / 10 299 / 10 576) | input → vblank (server view) | **120 Hz** | same |
| KWin 6.6.4, idle desktop | one instrumented frame ("about") | **9.78 ms** | input → present | 120 Hz | Zen 4, RTX Ada, LG C1 |
| KWin 6.6.4, busy background client | one instrumented frame | **14.74 ms** (a missed pageflip, i.e. one extra 8.3 ms frame) | input → present | 120 Hz | same |
| KWin, scheduling slack removed | theoretical floor / patched minimum | **3.07 ms** / **≈3 ms** | input → present | 120 Hz | same |
| KWin, GPU compositing work alone | p50 / p95 | **0.36 / 1.01 ms** | render time, not latency | — | same |

How to read it, honestly:

- **At the same refresh rate, ours is lower**: ≈6.3 ms against 9.78 ms,
  both at 120 Hz, on a machine that is weaker in every respect. The
  statistics differ, though. Ours is a mean over interleaved runs with a
  different pacing from the 202-sample headline. KWin's figure is a single
  instrumented frame, which the author calls "about 9.78 ms". So the
  comparison supports "the same regime, and not worse". It does not
  support "N% faster". farnoy.dev's figures span ≈3 ms (patched KWin) to
  9.78 ms (stock) at 120 Hz, and nitro lands inside that range.
- **Most of the difference between our two rows is the refresh rate, and
  we measured it.** The dominant term is the wait for the next vblank.
  Halving the frame period (16.7 → 8.3 ms) moved our mean by **−3 983 µs**,
  against a predicted half-frame of 4 167 µs, 3/3 pairs the same sign
  ([`latency.md`](latency.md) §5). So the 60 Hz headline of 9.3 ms and
  KWin's 9.78 ms at 120 Hz are *not* like for like. The 120 Hz row is the
  one to hold against KWin.
- **Our p95 of 17.1 ms is a whole extra frame at 60 Hz.** KWin's
  busy-client 14.74 ms is the same phenomenon at 120 Hz, one missed
  pageflip. A missed frame costs 16.7 ms for us and 8.3 ms for them, so
  tails quoted in milliseconds across different refresh rates are not
  comparable either.
- **The endpoints differ slightly.** Our headline ends at the client's
  `Presented`, which includes the client's reaction and two socket hops.
  KWin's ends at the compositor's present. Our 120 Hz row is the server
  view, ending at vblank, which is the closer match.
- KWin's GPU work is 0.36 ms p50. Our CPU rasteriser's paint + copy is
  **669 µs mean** per frame for a 275 418-pixel damage
  ([`latency.md`](latency.md) §4.5), and a `paint_us_mean` of **17 µs** for
  pointer motion at either rate (§5). In both systems the rendering is a small fraction
  of the latency. The rest is scheduling.

### Not comparable: click-to-photon

Everything else published measures **click-to-photon** or
**keypress-to-photon**. Those numbers include the input device (scan,
debounce, USB poll), scanout and the panel's pixel response. Pavel Fatin's
component budget (2015) puts keyboard + monitor at ≈26 ms. At 60 Hz, a
photon figure is roughly **our kind of number + 20–25 ms**, and at
120 Hz roughly + 8–10 ms. [`latency.md`](latency.md) §1 says the same from
our side ("plus roughly 10–25 ms of hardware neither process can see").
Do not hold any of these against 9.3 ms.

The one result from this class worth quoting is the **X11 compositor
tax**, because it compares two stacks on one rig by one method.
zamundaaa (Xaver Hugl, KWin), 2021, used an MCU-emulated click and a
brightness sensor, on a Ryzen 5800X, an RX 6800 XT and a Samsung C49RG94
at 120 Hz. With fifo present, median click-to-photon was **59 ms with KWin
X11 compositing** and **41 ms with no compositor**. The author attributes
one frame of that to the X11 protocol: the compositor receives the
client's image too late for the current frame. The author could not
account for the second frame.

Other photon studies are examples of the non-comparable class only.
They include marco-nett.de (2025/26, 4.79 ms X11 vs 4.93 ms Wayland on a
500 Hz OLED with tearing, game HUD flash), davidjusto.com (2025, ≈7 ms at
360 Hz, CS2 muzzle flash) and Dan Luu (2017, 90 ms+, measured from when
the key *starts moving* until the panel finishes transitioning).

### Not found

**No published latency measurement exists, in any class, for
GNOME/mutter, XFCE/xfwm4, weston or sway.** Mutter's latency work is
discussed in MRs and release notes, but no primary source with a table of
measured milliseconds was found. Recent instrumented Linux latency work is
KWin-centric and gaming-centric.

## 2. Memory

### Nothing external in our class

We report `RssAnon` / `RssFile` / `RssShmem` from `/proc/<pid>/status`
([`budget.md`](budget.md), "Resident memory"). **No public source reports
`RssAnon`, `RssFile` or PSS for any compositor**: not Xorg, gnome-shell,
kwin, sway, weston or labwc. Every public number is `ps rss`, `top RES`
or `free -m`. So there is no external figure to compare with ours, and the
nearest ones follow with their caveats.

### Per-process RSS (loosely comparable, RSS to RSS only)

| process | RSS | how measured | setup | year | source |
|---|---|---|---|---|---|
| gnome-shell 42.1, healthy | **147 MB** | per-process figure reported in a forum thread | Manjaro | 2022 | discourse.gnome.org |
| gnome-shell | **~276 MB** [derived] | `top %MEM` 6.9 % × 4 GB, ±10 % | Fedora 31 VM | 2019 | Fedora Magazine |
| kwin_x11 | **~144 MB** [derived] | `%MEM` 3.6 % × 4 GB | Fedora 31 KDE VM | 2019 | same |
| xfwm4 | **~80 MB** [derived] | `%MEM` 2.0 % × 4 GB | Fedora 31 XFCE VM | 2019 | same |
| Xorg | **~88–100 MB** [derived] | `%MEM` 2.2–2.5 % × 4 GB | Fedora 31 VMs | 2019 | same |
| **`nitro-server`**, M3 desktop, shadow on | **19 620 kB** VmRSS (12 444 anon / 7 176 file) | `/proc/<pid>/status` | box, 1080p, real KMS | 2026 | [`budget.md`](budget.md), "The M3 desktop" |
| **`nitro-server`**, audited, `NITRO_SHADOW=0` | **2 424 kB** `RssAnon` at 0 windows, **3 980 kB** at 5; `RssFile` **7 252 kB** | same | same | 2026 | [`budget.md`](budget.md), "Test box" |
| **whole nitro desktop** (server, session, wallpaper, bar, launcher) | **30 700 kB** VmRSS (13 372 anon / 17 328 file) | sum of five `status` samples | same | 2026 | [`budget.md`](budget.md), "The M3 desktop" |

Caveats that belong beside the table:

- The [derived] rows are `top %MEM` to two significant figures against a
  nominal 4 GB, so they carry roughly ±10 % error. They are order of
  magnitude only.
- RSS counts shared-library pages, so each RSS is a different mix of
  private and shared memory. Only RSS against RSS is even loosely
  meaningful. `RssAnon` against RSS is not meaningful.
- **Ours includes the framebuffer and theirs does not.** Nitro's 8.1 MB
  shadow buffer is heap, so it is in our RSS
  ([`budget.md`](budget.md), "The 8 MB the shadow buffer costs"). A GPU
  compositor keeps its equivalents in VRAM or GEM/dma-buf allocations,
  which do not appear in any process's RSS. The difference in magnitude
  is real, but this accounting difference makes ours look *larger*, not
  smaller.
- The external runs are VMs with software rendering. llvmpipe puts
  framebuffers into system RAM, which inflates Xorg's figure. Bare metal
  would read differently.
- gnome-shell has well-known pathologies of **3–9.5 GB**, from extensions
  and from the pre-3.30 GJS toggle-ref/GC issue. They measure the user's
  extensions, not GNOME, and are quoted here only as a warning never to
  use a gnome-shell figure that does not state "extensions disabled".

### Whole-session `free -m`: not comparable at all

The following figures are what every "GNOME uses N MB" review measures.
**They cannot be compared to a per-process number, not even roughly.**
Whole-system "used" memory includes the kernel and its slab caches,
systemd, journald, NetworkManager, the display manager and each distro's
update daemon. The desktop is only part of it. Going from them to our
figures involves three changes of definition at once: whole-system to
per-process, shared-inclusive to anon-only, and kernel-plus-services to
user process only.

With that said: Fedora Magazine (2019, Fedora 31 spins, KVM VM,
1 vCPU/4 GB, 5 min after login, 3 terminals) measured GNOME **612 MB**,
KDE Plasma **733 MB**, XFCE **448 MB** and LXQt **391 MB**. A 2022
itvision measurement on Fedora 37 (VirtualBox, 4 GB) put the operating
system alone, with no desktop environment (IceWM only), at **271 MB**. So
most of any such figure is the distro rather than the desktop. The M3
desktop's 30.7 MB VmRSS is not a point on this scale.

## 3. Throughput

### Why no valid comparison exists

[`bench.md`](bench.md) §2 declines to report an x11perf operations-per-
second figure, and transposes the question to mutations per frame
sustained at refresh. There is also nothing modern to compare such a
figure with. **Absolute x11perf results are essentially unpublished after
about 2010.** Phoronix-era work reports percentage deltas on single tests
(for example GLAMOR's +759 % on `-f8text`, 2014), which says nothing about
absolute speed. openbenchmarking.org refuses scripted fetches, and Keith
Packard's 2014 GLAMOR post, which carried Brix x11perf figures, is now a
404.

The historic absolutes, with their fill rates for scale: `-rect500` is
500×500 = 250 000 pixels per op.

| `x11perf -rect500` | ≈ fill rate [derived] | hardware | year | source |
|---|---|---|---|---|
| **438** ops/s | **≈110 Mpx/s** | Sun SPARCstation LX, Solaris 2.2 / OpenWindows 3.2 | 1993 | Beebe, ftp.math.utah.edu |
| **163** ops/s | **≈41 Mpx/s** | ATI Radeon r100, NoAccel (software) | ~2006 | Carl Worth, cworth.org |
| **501–772** ops/s | **≈125–193 Mpx/s** | ATI Radeon r100, EXA | ~2006 | same |
| **~3 400** ops/s | **≈850 Mpx/s** | Intel 965 (Worth: 843 Mpx/s) and dual-head Intel (Vignatti: 3 385–3 400) | 2007–08 | two independent sources agreeing |

The 2006 software figure is *slower* than the 1993 workstation, because
NoAccel wrote into uncached video memory across AGP. The memory path
matters more than the year of the hardware. Nitro's §4.5 shadow buffer
exists for the same reason ([`latency.md`](latency.md) §4.5).

### x11perf sends one rectangle per request

This is the primary source that confirms [`bench.md`](bench.md) §2.
Carl Worth chased an implausible "30 billion pixels/sec" result and
explained it with `xtrace`: *"my test was sending rectangles in batches of
256 per request while x11perf was sending only 1 per request."* On
identical hardware drawing the identical primitive, his batched version
ran **about 200× faster**, mostly because EXA culled the near-total
overlap. (cworth.org/exa/mystery_solved/, 2006–07.) The variable was the
protocol's shape, not the drawing. That is why an ops/s figure from a
retained, once-per-vblank server would measure the socket and nothing
else.

### The nearest per-frame analogues

- **Copy elimination.** In 2026, KWin made `wl_shm` avoid one CPU-side
  copy (udmabuf + `VK_EXT_external_memory_host`). KWin's CPU while
  scrolling in KDevelop went from **80–90 % of one core to 20 %**, on an
  integrated GPU (zamundaaa, 2026-05-06). **[derived at 60 fps; the post
  does not state the frame rate]**: that is ≈13–15 ms → ≈3.3 ms of CPU per
  frame. This is the closest published analogue to our pixel-vs-node
  headline ([`bench.md`](bench.md) §7.7: 20 833 → 111 µs of client CPU
  per frame at 1080p). Both results point the same way: the per-frame
  cost that matters is bytes moved, not drawing.
- **Composite time.** On a healthy desktop GPU, KWin can composite a frame
  in *"as little as a few hundred microseconds"* (zamundaaa, 2024, on
  fixing KWin performance on old hardware). The same post reports old
  Intel iGPUs with 4K screens missing the 16.67 ms deadline outright.
  No in-tree figure is exactly "~0.4 ms of damage per frame". The nearest
  real figure is [`latency.md`](latency.md) §4.5: the default shadow-buffer
  server paints a 275 418-pixel damage (13 % of the screen) in **418 µs
  mean**, and **669 µs** including the copy to scanout. That is on a CPU
  rasteriser on the G3240, the same order as KWin's GPU on a far stronger
  machine.

### A finding: nobody has measured what damage tracking saves

**No published, quantified "damage tracking / partial repaint saves N %"
exists for any compositor.** The nearest items are the copy elimination
above (not damage) and Worth's 200× (an accidental overlap cull inside an
immediate-mode stack, which was treated as a benchmark artefact at the
time). [`bench.md`](bench.md) §7.7 measures it directly: the same ball
damages 8.1 % of the screen instead of 100 %, for 187× less client CPU.
As far as this search could establish, that makes nitro a primary source
for the figure.

## 4. Memory bandwidth against STREAM

[`bench.md`](bench.md) §5 measures `copy` at **3.61 GB/s** on the box. The
§9 sitting measured the same box at **3.43 GB/s** (3.56 on the §7.10b
sitting). So the figure drifts by a few percent between days, and it is
not a fixed property of the machine ([`bench.md`](bench.md) §11).

**It counts destination bytes only.** `nitro-bench bandwidth`'s `copy`
reports `bytes × rounds` for a `copy_from_slice` of `bytes`
(`crates/nitro-bench/src/bandwidth.rs`). STREAM COPY counts both the read
and the write, 16 bytes per 8-byte element. In STREAM's convention our
figures are therefore **≈7.2 GB/s** (3.61) and **≈6.9 GB/s** (3.43).

For placement, the official STREAM table
(cs.virginia.edu/stream/by_date/Bandwidth.html, revised 2017) has no
G3240 or any Haswell desktop. The nearest client part is an Intel Core
i7-2600 (Sandy Bridge, 2011) at **12 373 MB/s** COPY single-threaded. Ours
is ≈58 % of that (≈56 % at 3.43). That is plausible for a 2-core Pentium
with AVX fused off, and unremarkable. Holding the raw 3.61 against a STREAM
table instead suggests a machine **~3.4× slower** than the i7-2600, which
is wrong by the factor of two in the convention.

The `write` and `read` rows count one pass each, and those match STREAM's
per-byte convention for a one-way operation. [`bench.md`](bench.md) §5's
"~3 passes" arithmetic is internally consistent: it counts payload bytes
and divides by a payload-byte bandwidth.

## 5. Sources

Latency:
- farnoy.dev, "Linux latency", 2026 — https://farnoy.dev/posts/linux-latency
- zamundaaa (Xaver Hugl), "About gaming on Wayland", 2021-12-14 — https://zamundaaa.github.io/wayland/2021/12/14/about-gaming-on-wayland.html
- marco-nett.de, 2025/26 — https://marco-nett.de/blog/measuring-input-latency-on-linux-x11-vs-wayland-vrr-dxvk/
- davidjusto.com, 2025 — https://davidjusto.com/articles/m2p-latency/
- Dan Luu, "Computer latency", 2017 — https://danluu.com/input-lag/
- Pavel Fatin, "Typing with pleasure", 2015 — https://pavelfatin.com/typing-with-pleasure/

Memory:
- Fedora Magazine, "Fedora desktops – memory footprints", 2019 — https://fedoramagazine.org/fedora-desktops-memory-footprints/
- itvision, "Linux desktop environments system usage", 2022 — https://itvision.altervista.org/linux-desktop-environments-system-usage.html
- GNOME Discourse thread, 2022 — https://discourse.gnome.org/t/gnome-shell-used-me-lots-of-random-acess-memory-is-it-normal/9974
- fsck.sh, gnome-shell leak debugging, 2025 — https://fsck.sh/en/blog/gnome-memory-leak-debugging/
- Launchpad bug 1856838, 2019 — https://bugs.launchpad.net/ubuntu/+source/gnome-shell/+bug/1856838
- feaneron, "The infamous GNOME Shell memory leak", 2018 — https://feaneron.com/2018/04/20/the-infamous-gnome-shell-memory-leak/

Throughput and bandwidth:
- Nelson H. F. Beebe, x11perf results, 1993 — https://ftp.math.utah.edu/pub/benchmarks/x11perf/x11perf-direct-xlib.msg
- Carl Worth — https://cworth.org/exa/mystery_solved/ and https://cworth.org/exa/corrected_rectangles/
- Tiago Vignatti, "Benchmarking it all", 2008-02-21 — https://vignatti.com/posts/benchmarking-it-all/
- Phoronix, GLAMOR x11perf deltas, 2014 — https://www.phoronix.com/news/MTYyODU
- zamundaaa, "Fixing KWin's performance on old hardware", 2024-06-25 — https://zamundaaa.github.io/wayland/2024/06/25/fixing-kwin-perf-on-old-hardware.html
- zamundaaa, "Making wl_shm fast", 2026-05-06 — https://zamundaaa.github.io/wayland/2026/05/06/making-wl-shm-fast.html
- STREAM results table — https://www.cs.virginia.edu/stream/by_date/Bandwidth.html; counting rules — https://www.cs.virginia.edu/stream/ref.html

Searched for and **not found**:
- any latency measurement, in any class, for GNOME/mutter, XFCE/xfwm4, weston or sway;
- `RssAnon` / `RssFile` / PSS for any compositor process;
- a verifiable weston / sway / wlroots / labwc RSS figure (a 2025 Reddit comparison exists but could not be fetched, so it is not quoted);
- a modern absolute x11perf result (`-rect100`, `-putimage500`, text) on current Xorg;
- a per-frame microsecond figure for mutter, weston or sway;
- any quantified "damage tracking saves N %" for any compositor.
