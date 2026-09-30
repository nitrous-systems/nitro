# nitro-video

A native video player (#3906): the system **FFmpeg** demuxes and decodes
(on VA-API when the hardware takes the stream, #3923), NV12 frames reach a
`Surface` node as shared-memory buffers or as the decoder's own dma-bufs,
and nitro-ui controls are drawn over the video.

```
nitro-video FILE [--fullscreen] [--frames N] [--stats]
                 [--hwdec auto|dmabuf|download|off] [--vaapi-device PATH]
nitro-video --synthetic ...        # generated 720p30 stream, no file
```

Keys: Space play/pause · Left/Right ±5 s · F / F11 / double-click
fullscreen · Esc leave fullscreen · Q quit. The controls hide after 3 s
without pointer motion while playing.

## Design

| file | role |
|---|---|
| `src/shim.c` | the only code that sees FFmpeg: `nv_open/info/hw_info/seek/next/next_hw/release/close` over libavformat + libavcodec + libavutil's hwcontext, 4:2:0 → NV12 without swscale |
| `src/ffmpeg.rs` | `LibavDecoder` and `open` (VA-API first, software fallback), the eight `extern "C"` calls (the crate's `unsafe` exception, see `DEPENDENCIES.md`) |
| `src/decode.rs` | the `Decoder` trait, `StreamInfo`, `FrameBuf::{Shm, DmaBuf}`, the output policy `choose_output`, `SyntheticDecoder` (with an emulated VA pool for tests) |
| `src/player.rs` | decode thread, NV12 shm ring or registered dma-bufs, pacing against frame callbacks, `PresentSurface`, stats |
| `src/pacing.rs` | pure clock + frame picking |
| `src/controls.rs` | the nitro-ui overlay (`SurfaceView` + play, seek slider, time, fullscreen) |
| `build.rs` | `pkg-config` + `cc` + `ar` by command, no build crates |

**Threads.** A decode thread owns the decoder and maps the ring's memfds
itself; it decodes into a free slot, sends `(slot, pts)` over a channel
and pokes a wake pipe the UI loop watches (`Ui::add_fd`). A full ring
blocks it — that is the back-pressure. The UI thread never touches pixels.
Up to 1080p libavcodec decodes on the decode thread alone; frame threads
(one per core, up to four) only above that (see *Memory* below).

**Ring.** 4 sealed memfds (one on screen, one latched, two ahead),
≈ 5.5 MB at 720p and 12.4 MB at 1080p. A slot is *decoder* → *ready* →
*queued* (`PresentSurface`) → back to the decoder at `BufferReleased`.

**Pacing.** While playing, each `Frame` callback shows the newest ready
frame due by the flip (nearest-vblank rounding); older due frames are
skipped. The clock is anchored at the first frame and re-calibrated on its
`Presented` time, so latency in the server's first answer does not skew
the whole run. A callback with nothing due sets a timer for the next due
frame instead of spinning. Pause/seek stop the clock; a seek restarts the
decoder at the keyframe before the target and drops the frames before it
(drags coalesce: one seek in flight, the newest waits). The overlay
updates once a second; video frames never touch the widget tree.

**Colour.** The stream's matrix/range when it states them; otherwise
BT.709 from 720 lines up, BT.601 below, limited range.

## Hardware decode (#3923)

**Backend.** `ffmpeg::open` tries VA-API first unless `--hwdec off`. The
shim creates an `AV_HWDEVICE_TYPE_VAAPI` device on the render node
(`--vaapi-device`, default `/dev/dri/renderD128`), lets libavcodec's
hwaccel decode into VA surfaces on one thread, and **decodes the first
frame in `nv_open`**, keeping it pending. Only a frame proves the driver
takes the stream. It falls back to the software decoder (#3924's thread
policy) with a reason on stderr and in `--stats` (`fallback="…"`) when:

- there is no device, or it does not open;
- FFmpeg has no VA hwaccel for the codec;
- the driver refuses the profile (FFmpeg's `get_format` is then offered
  no `vaapi` and falls back itself);
- the surfaces are not NV12 (10-bit HEVC Main10 / VP9 profile 2 → P010).

| box | driver | VA-API | software |
|---|---|---|---|
| box1 (Haswell) | i965 | H.264 | HEVC, VP8/9, AV1 |
| testhost2 (Kaby Lake) | iHD 26.2 | H.264, HEVC Main, VP8, VP9 profile 0 | AV1; 10-bit (P010 is refused, not NV12) |

**Two outputs**, chosen once the window is open (`decode::choose_output`,
`decode=` in `--stats`):

- `vaapi-dmabuf`: each VA surface is exported with `av_hwframe_map` →
  DRM PRIME (FFmpeg's separate R8 + GR88 layers are composed into NV12
  plane 0/1), registered once with `CreateDmabufBuffer`, and presented
  with `PresentSurface`. There is no copy at all. The export's
  `vaSyncSurface` makes a frame complete before it is presented, and the
  server snapshots the buffer's implicit fences at `PresentSurface`, so
  no `PresentSurfaceFenced` is needed. The frame's `AVFrame` ref is held
  until `BufferReleased` and then goes back to the decoder's pool.
- `vaapi-download`: `av_hwframe_transfer_data` into NV12 and then the
  shm ring, as software does. Decode is off the CPU, the copy is not.

`--hwdec auto` (default) picks dma-bufs only when the server **shows**
them as they are: the default `DmabufFeedback` lists NV12 with the
surfaces' modifier as `CPU` (linear), or as `SCANOUT` while the server
advertises `caps::DIRECT_SCANOUT`. Otherwise it swaps the VA decoder for
the **software** one before the first frame (the stream is re-opened; a
seek made meanwhile is replayed), because downloading was measured to
cost more than it saves (below). `--hwdec dmabuf` takes anything the
feedback lists as `IMPORT`, and `--hwdec download` always downloads. Both
check the feedback before sending `CreateDmabufBuffer`, since an unlisted
pair is a fatal `BadBuffer`, and download otherwise. The player waits up
to 200 ms for the feedback.

**What the server does with a tiled VA buffer (#3938).** VA on Intel
decodes into Y-tiled NV12 (`I915_FORMAT_MOD_Y_TILED`), which the CPU
cannot convert. Where a plane lists that pair, the server advertises
`DIRECT_SCANOUT`, KMS-imports every VA surface once (AddFB2 with the
modifier) and **scans it out on a plane**:

- **testhost2 (KBL):** `auto` picks `vaapi-dmabuf` and shows the real
  picture. The CRTC CRC changes frame to frame, and
  `i915_display_info` shows the NV12 Y-tiled framebuffer on the plane.
  - Fullscreen: direct scanout on the primary (`planes_mode 3`, primary
    `NV12 0x100000000000002 1920x1080 → 2560x1440`).
  - Fullscreen with the pointer or controls over it: underlay by primary
    swap (video on the primary, the UI as AR24 on the overlay,
    `planes_mode 1`).
  - Windowed 720p (1600×900 device px at scale 1.25): overlay above the
    UI (`planes_mode 1`).
  - A **1080p clip in its default window** is downscaled to 1600×900
    (0.83×). That is under KBL's 0.94× floor, so it cannot go on a
    plane. Since #3922 the GPU helper composites it (`planes_mode 2`).
    Measured in #3953 (temporary `nitro-dev` unit, 450 frames): a real
    picture (the CRTC CRC changes 62 times in 2 s), 0 dropped, 1 late,
    `gpu_frames` +456, `gpu_composite_us` avg 756 / max 992,
    `gpu_fallbacks` 0, `dmabuf_placeholder_paints` +1 (the first frame
    only, not +452 as before). Server CPU 3.0 % (was 27–31 % with the
    placeholder), player 5.3 %, helper RSS 13.9 MB. `nitro-shot` still
    shows grey there: it reads the server's shadow, where the video is a
    hole that `shot` fills with the placeholder colour (#3897). A
    server started before the #3922 build was installed (a GDM login
    from earlier) has no helper and still paints the placeholder
    (`planes_mode 0`, 31 % CPU).
- **box1 (HSW, i965):** no plane lists NV12, so the server does not
  import Y-tiled NV12 at all. `auto` decodes in software (`fallback="the
  server does not import NV12 with modifier 0x100000000000002"`), and
  `--hwdec dmabuf` falls back to `vaapi-download` for the same reason.
  Neither case shows a placeholder.

**Scaling to the plane hint (#3956).** The server sends
`SurfacePlaneHint`: the Surface's device-pixel size and the plane's
downscale floor (94 % on KBL, 100 % where planes do not scale, 0 without
planes). When the stream is larger than a plane can downscale to that
size (`decode::scale_target`), the decode thread scales every VA frame on
the GPU's fixed-function video engine (VPP: one
`VAProcPipelineParameterBuffer` pass in the shim, libva directly, same
matrix and range in and out) into a fixed pool of `SCALE_POOL` (5)
surfaces at the hinted size, keeping the stream's aspect. The plane then
takes the frame 1:1. It never downscales what a plane can take and never
upscales (the plane does that). Hysteresis: the current size is kept
while the plane can take it to the new hint (down to the floor, up to
110 %). A rescale waits 250 ms after the last hint, so a window drag
costs one reallocation per settle rather than one per step. A new pool is
a new key generation: the old pool's registrations are destroyed as they
come back from the server, not left to the LRU. If VPP cannot start or
fails mid-stream (no VideoProc entrypoint, e.g. an old i965), the
player logs one line and stays native. `--no-scale` turns it off for
measurement. `--stats` adds `scale=WxH|native rescales=N min_scale=P
scale_limited=N`; `scale_limited` counts hints in which the server said
the Surface was off the planes because of the downscale limit, which is
how that fallback shows up without the control socket.

Measured on testhost2 (#3956, temporary `nitro-dev` unit with
`NITRO_GPU=0`, scale 1.25, 450 frames, CPU over 6 s of steady state):

| run (1080p clip, default 1600×900-device-px window, helper **off**) | layout | player CPU | player RSS | player GEM | server CPU | presented / dropped / late |
|---|---|---|---|---|---|---|
| VPP to the hint (default) | overlay, `NV12 Y-tiled 1600x900` (`planes_mode 1`) | 4.7–4.8 % | 56.8–59.5 MB | 53–54 MB | **2.3 %** | 450 / 0 / 0–1 |
| `--no-scale` (before #3956) | composite, **placeholder** (`planes_mode 0`) | 4.3 % | 54.5 MB | 28.2 MB | 25.5 % | 450 / 0 / 1 |
| `--hwdec off` (software, shm) | composite, CPU scaled blend | 24.6 % | 71.2 MB | — | 49.4 % | 450 / 0 / 23 |
| 720p clip, software, same window (≈ a pre-scaled shm frame) | composite | 12.5 % | — | — | 42.8 % | 450 / 0 / 8 |

The plane path costs the player ~0.5 % CPU (the VPP pass) and ~25 MB
more GEM (5 scaled surfaces at 1600×900 plus the decoder's pool, which
FFmpeg keeps at its own size), and saves the server 23 points of a core
against the placeholder, and 47 against software decode. With the helper
on, mode 2 was 3.0 % server CPU (#3953); VPP-to-plane is 2.3 % and the
helper process (13.9 MB RSS) is not needed for it. Fullscreen (2560×1440,
the plane upscales) and 720p in its window (fits) stay native
(`rescales=0`): `planes_mode 3` and `1`, server 2.3 %.

**Software decode is not scaled (#3956).** The server's CPU composite
costs about the same whether it scales: the 720p software clip in the
same window (a frame already smaller than its rect, as a swscale'd one
would be) still costs the server 43 % against 49 % for 1080p, and the
player would add swscale's per-frame cost and a new mapped library. Not
taken; software decode stays native.

**box1 (HSW)** has no plane scaler and no plane that takes Y-tiled NV12,
so VA-API there falls back to software (above) and the hint buys nothing:
a shm Surface is composited whatever its size. A producer that does
render dma-bufs a Haswell plane takes (linear YUYV on the overlay) would
get `min_scale_pct 100` and render exactly at the node's size.

**Buffer pool.** FFmpeg's VA pool is dynamic (VA-API ≥ 1), so it grows
to the decoder's references plus what is in flight; `extra_hw_frames = 4`
sizes a fixed pool the same way. The decode thread keeps at most `RING`
(4) surfaces out of the pool: on screen, latched, two ahead. The player
registers at most 24 dma-bufs (the server allows 32 per client). If
the decoder cycles through more surfaces than that, the least recently
used idle one is destroyed and re-registered. Surface keys carry a
frames-pool generation, so a mid-stream re-init (a VP9/HEVC resolution
change) never reuses a stale registration; a size change is an error, as
in software.

### Measurements (#3923)

x264 High / x265 30 fps `testsrc2` clips, `--frames 300 --stats`, CPU =
(utime + stime) / wall over the first 7–10 s, RSS at 7–10 s.

**testhost2** (i5-8250U, iHD 26.2, FFmpeg 9), real session: a temporary
`nitro-dev` unit on the deployed server (`76d438e`-era main, eDP
2560×1440, scale 1.25). `server` is the whole server's CPU; `vaapi-dmabuf`
is forced (`--hwdec dmabuf`) and paints the grey **placeholder**, so its
`paint_us` is the placeholder fill, not the picture:

| run | decode | player CPU | RSS | server CPU | `paint_us` mean | presented / dropped / late |
|---|---|---|---|---|---|---|
| 720p windowed | software | 13 % | 52.1 MB | 46 % | 6.5 ms | 300 / 0 / 24 |
| | vaapi-download | 13 % | 51.3 MB | 47 % | 6.6 ms | 300 / 0 / 35 |
| | vaapi-dmabuf | **5 %** | **45.9 MB** | 31 % | 3.1 ms | 300 / 0 / 0 |
| 1080p windowed | software | 23 % | 70.9 MB | 47 % | 6.8 ms | 300 / 0 / 33 |
| | vaapi-download | 20 % | 61.9 MB | 50 % | 10.2 ms | 300 / 0 / 76 |
| | vaapi-dmabuf | **5 %** | **49.8 MB** | 31 % | 3.2 ms | 300 / 0 / 2 |
| 1440p fullscreen | software (4 threads) | 40 % | 136.7 MB | 48 % | 6.0 ms | 300 / 0 / 2 |
| | vaapi-download | 26 % | 77.0 MB | 52 % | 10.2 ms | 300 / 0 / 132 |
| | vaapi-dmabuf | **5 %** | **54.8 MB** | 31 % | 1.4 ms | 300 / 0 / 0 |
| HEVC 1080p | auto → download (pre-swap build) | 20 % | 64.0 MB | 50 % | 8.4 ms | 300 / 0 / 68 |

The dma-buf runs registered 7 buffers (`dmabuf_buffers 7`, all
`dmabuf_kms_imported`), `buffers=7` in `--stats`: the VA pool as FFmpeg
grew it for x264 (refs + 4 in flight), well under the 24 cap.

**testhost2 with planes (#3938)**: temporary `nitro-dev` unit on this
branch (eDP 2560×1440, scale 1.25), `--hwdec auto` → `vaapi-dmabuf`, a
**real picture**, 450 frames. CPU is over 6 s in steady state. Every
frame is a plane-only flip (`plane_flips` +438…441 of 450), and the
server rasterizes nothing, so `paint_us` does not apply (no paints
after the first frames). The table is 0 dropped, 0 late:

| run | layout | player CPU | player RSS | player GEM | server CPU | server RSS |
|---|---|---|---|---|---|---|
| 1080p fullscreen | direct (primary, scaled) / underlay with the pointer | 3.5–3.7 % | 56.7–60.7 MB | 28.2–29.0 MB | **2.3–2.5 %** | 26.7 MB |
| 1440p fullscreen | underlay (primary swap) | 4.3–4.5 % | 56.1–57.2 MB | 54.3–55.9 MB | **2.7 %** | 26.7 MB |
| 720p windowed | overlay above | 3.2 % | 55.5 MB | 17.4 MB | 2.7 % | 26.7 MB |
| 1080p in a 1600×900 window | composite (0.83× downscale): **placeholder** | 3.8 % | 60.4 MB | 28.2 MB | 27 % | 26.7 MB |

Against the placeholder runs below, the server drops from 31 % to 2.5 %
of a core: it no longer touches the frame. `plane_fences` stays 0
because VA frames arrive complete (the export's `vaSyncSurface`), so no
fence is pending at receipt (`fence_waits` 0).

**box1** (Pentium G3240, i965 2.4.1: **H.264 only**, FFmpeg 8), the live
`nitro-dev` session, HDMI 1080p. i965 exports Y-tiled NV12 too, which a
fake-backend server does not import, and the deployed live server
predates #3918 (it did not grant `DMABUF`), so only software and download
compare. The dma-buf path on box1 still needs a re-check after a deploy:

| run | decode | player CPU | RSS | server CPU | `paint_us` mean | late (of 300) |
|---|---|---|---|---|---|---|
| 720p | software | 33 % | 56.1 MB | — | 5.6 ms | 1 |
| | vaapi-download | **16 %** | 59.2 MB | — | 5.9 ms | 1–3 |
| 1080p | software | 34–37 % | 75.4 MB | 39–41 % | 5.7–6.3 ms | 50–64 |
| | vaapi-download | **23 %** | 75.0 MB | **56–58 %** | **16.3–17.8 ms** | **272–288** |
| HEVC 720p | auto → software | 37 % | 65.4 MB | — | 8.2 ms | fallback: "the VA-API driver does not decode this hevc profile" |

0 dropped in every run. **Why `auto` does not download:** the player's
CPU halves, but the server's full-frame NV12 paint of a downloaded frame
takes ~3× as long at 1080p on box1 (and ~1.5× on testhost2), with 4–5×
the late frames. The server did the same work (`perf stat`: 8.3 G vs
8.4 G instructions), so the extra time is memory stalls. The likeliest
cause is the downloaded frames themselves: `av_hwframe_transfer_data`
reads VA's tiled, uncached surface through a mapping, and the copy
competes with the server's paint for the memory bus on both iGPUs. It is
not isolated further here. Download is kept as `--hwdec download` for
measurement.

**Memory.** With dma-bufs the 4-slot NV12 shm ring (12.2 MB at 1080p,
22 MB at 1440p) is never created, and the software decoder's DPB and
pools (~24 MB at 1080p) move to VA surfaces: player RSS **−21 MB at
1080p** (70.9 → 49.8 MB) and **−82 MB at 1440p fullscreen** (136.7 →
54.8 MB, where software needs frame threads). The surfaces are GEM
objects in system RAM on these iGPUs, outside RSS but not free: `drm-total-system0`
in the player's `/proc/<pid>/fdinfo` is **22.8 MB at 1080p** and 42.0 MB at
1440p. So the net memory saving at 1080p is ≈ 0, and the win is CPU:
**5 % instead of 23 %** of a core at 1080p, 5 % instead of 40 % at 1440p.

Binary: stripped `nitro-video` 845 KB (was 809 KB, +36 KB), no new crate.
AV1 (`libdav1d`) has no VA hwaccel in FFmpeg and decodes in software on
both boxes ("FFmpeg's libdav1d decoder has no VA-API hwaccel").

## Limitations (v1)

- No audio, so no A/V sync.
- 8-bit 4:2:0 only (`yuv420p`, `yuvj420p`, `nv12`); others are refused by name.
- VA-API output is 8-bit NV12 only; 10-bit streams decode in software and are then refused (8-bit 4:2:0 only).
- Needs `libavformat`/`libavcodec`/`libavutil` (`.so.62`/`.so.60`) at runtime and their `-dev` packages to build.

## Measurements

box1 (Pentium G3240, HDMI 1080p60), nitro-dev on main `76d438e`,
x264 High 30 fps `testsrc2` clips, 450 frames per run, `--stats`:

| run | player CPU (decode + UI) | RSS | libav* resident | server `paint_us` mean | presented / dropped |
|---|---|---|---|---|---|
| 1280×720 windowed | 38 % of one core | 63 MB | 8.8 MB | ≈ 5.8 ms (max 12.2) | 450 / 0 |
| 1920×1080 windowed | 53 % | 87 MB | 8.9 MB | ≈ 10.7 ms (max 19.6) | 450 / 0 |
| 1920×1080 fullscreen | 51 % | 87 MB | 8.9 MB | ≈ 10.1 ms (max 19.2) | 450 / 0 |

`paint_us` is the server's full-frame NV12 conversion (damage is the
whole picture every frame) on the CPU composite path; at 1080p the paint
approaches a 60 Hz frame, and the planes path (#3899) is what removes it.
"late" (presented > 20 ms after due) is ~2 at 720p and about half the
frames at 1080p, for that reason.

Binary: stripped `nitro-video` **805 KB** (release). The libav* files it
maps: libavcodec 28.3 MB, libavformat 3.2 MB, libavutil 1.2 MB and
(transitively) libswresample 0.2 MB on disk, shared with any other FFmpeg
user; ≈ 9 MB of them is resident while playing.

## Memory (#3924)

Where the 87 MB went at 1080p, and what was cut. `/proc/<pid>/smaps_rollup`
and per-mapping `Rss` from `smaps`, sampled 10 s into a 450-frame run,
x264 High 30 fps `testsrc2`, `--stats`. CPU is (utime + stime) / wall of the
whole process, as a share of one core.

box1 (Pentium G3240, 2 cores, FFmpeg 8.0.1, real session on HDMI 1080p):

| 1080p windowed | before (`d286a1f`) | after |
|---|---|---|
| RSS | 87.1 MB | **75.2 MB** |
| anon: libavcodec mmaps (DPB, pools, contexts) | 33.9 MB | 23.8 MB |
| anon: `[heap]` | 3.2 MB | 1.7 MB |
| anon: dirty pages of libraries, stacks | 4.6 MB | 4.6 MB |
| shm: NV12 ring (`memfd:nitro-video`, 4 × 3.1 MB) | 12.2 MB | 12.2 MB |
| file: libav* (`libavcodec` 5.2, `libavformat` 2.8, `libavutil` 0.7) | 8.8 MB | 8.8 MB |
| file: other libraries, the binary | 24.4 MB | 24.2 MB |
| threads | 4 | 2 |
| CPU | 51 % | **35 %** |
| dropped / late (of 450) | 0 / 164–188 | 0 / 98–127 |

| run | RSS before → after | CPU before → after | dropped | late before → after |
|---|---|---|---|---|
| 720p windowed | 62.4 → **56.2 MB** | 40 % → 34 % | 0 | 1–3 → 2–3 |
| 1080p fullscreen | 87.1 → **75.3 MB** | 49 % → 33 % | 0 | 253–277 → 31–53 |

testhost2 (i5-8250U, 8 threads, FFmpeg 9.0.2; a fake-backend 1080p server,
so "late" means nothing there and is not shown):

| run | RSS before → after | threads | CPU before → after | dropped |
|---|---|---|---|---|
| 1080p | 96.3 → **71.2 MB** | 6 → 2 | 30 % → 36 % | 0 |
| 720p | 63.9 → **51.7 MB** | 6 → 2 | 30 % → 26 % | 0 |

What each lever was worth (1080p, box1 unless noted):

- **Decoder frame threads: the cut.** Each frame thread keeps its own H.264
  context and a picture in flight: +10.5–11.6 MB per thread at 1080p,
  +6.3 MB at 720p. On box1 (2 threads) the frame-threaded decoder also cost
  *more* CPU than one thread (51 % vs 35 %) and more late frames, because it
  competes with the UI thread and the server for two cores. One thread
  decodes 1080p30 comfortably on the weakest box, so up to 1080p the shim
  forces `thread_count = 1`; above it, `min(cores, 4)` as before. On
  testhost2 the price is +6 % of one core at 1080p for −25 MB.
- **Slice threads**: no alternative. x264 writes one slice per frame by
  default, so `FF_THREAD_SLICE` adds a thread and saves nothing over one
  thread (box1 1080p: 75.5 MB, same as one thread).
- **What remains in anon (≈ 24 MB at 1080p, 12 MB at 720p)** is the
  single-threaded H.264 decoder: the DPB (up to 16 references plus the
  current picture, sized by the stream's level) and libavcodec's
  frame/buffer pool. Frames are `av_frame_unref`ed right after the copy,
  so the pool is libavcodec's own and there is no knob for it short of a
  decoder that allocates less; `malloc_trim(0)` after `nv_open` (the
  probe decoder of `avformat_find_stream_info` is freed, and trimming
  moved nothing: 29.9 vs 30.1 MB anon) was not taken.
- **malloc arenas**: `mallopt(M_ARENA_MAX, 1/2)` changed RSS by ≤ 0.2 MB on
  both boxes; with one or two threads glibc's per-thread arenas are not
  where the memory is. Not taken.
- **Ring 4 → 3**: saves one frame (3.0 MB at 1080p, 1.4 MB at 720p) with 0
  dropped, but box1 showed more late frames at 1080p windowed (136–139 vs
  113–127 with one decoder thread). Kept at 4.
- **Per-frame copies**: exactly one, decoder frame → the shm NV12 slot in
  `to_nv12`, no per-frame allocation. The zero-copy path is VA-API +
  `DmaBuf`.
- **File-backed (≈ 33 MB, 24 of it not libav*)**: the system FFmpeg pulls
  in its whole codec/protocol world (libcrypto, libstdc++, librsvg,
  libopenmpt, gnutls, libx265, …, each 0.2–1.9 MB resident from
  relocation and init). Those pages are clean and shared with any other
  user of the libraries (`Pss` 59.9 MB vs `Rss` 75.2 MB); cutting them
  needs a narrower FFmpeg build, not a change here.
