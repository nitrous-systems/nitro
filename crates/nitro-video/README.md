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
