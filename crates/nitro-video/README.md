# nitro-video

A native video player (#3906): the system **FFmpeg** demuxes and decodes,
a ring of NV12 shared-memory buffers carries the frames, and a `Surface`
node shows them, with nitro-ui controls drawn over the video.

```
nitro-video FILE [--fullscreen] [--frames N] [--stats]
nitro-video --synthetic ...        # generated 720p30 stream, no file
```

Keys: Space play/pause · Left/Right ±5 s · F / F11 / double-click
fullscreen · Esc leave fullscreen · Q quit. The controls hide after 3 s
without pointer motion while playing.

## Design

| file | role |
|---|---|
| `src/shim.c` | the only code that sees FFmpeg: `nv_open/info/seek/next/close` over libavformat + libavcodec, 4:2:0 → NV12 without swscale |
| `src/ffmpeg.rs` | `LibavDecoder`, the five `extern "C"` calls (the crate's `unsafe` exception, see `DEPENDENCIES.md`) |
| `src/decode.rs` | the `Decoder` trait, `StreamInfo`, `FrameBuf` (the seam for a VA-API `DmaBuf` variant), `SyntheticDecoder` |
| `src/player.rs` | decode thread, 4-buffer NV12 ring, pacing against frame callbacks, `PresentSurface`, stats |
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

## Limitations (v1)

- No audio, so no A/V sync.
- 8-bit 4:2:0 only (`yuv420p`, `yuvj420p`, `nv12`); others are refused by name.
- Software decode only; VA-API through FFmpeg's hwaccel is the follow-up.
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
