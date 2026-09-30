# nitro-media

The media library behind `nitro-video` (and, later, `nitro-amp`). The design,
decisions and phases are in [`docs/media.md`](../../docs/media.md).

This is a library only. It links no C and has no `unsafe`. It depends on
`nitro-wire` (framing), `nitro-shm` (sealed memfds) and `rustix`, and not on
`nitro-ui`.

| module | what |
|---|---|
| `frame.rs` | `Nv12Layout`, `Matrix`, `StreamInfo`, `FrameBuf::{Shm, DmaBuf}`, `Dmabuf{Plane,Desc,Frame}`, `HwInfo`, `HwDec` |
| `source.rs` | `VideoSource`, the trait a video backend implements (info, seek, next frame, dma-bufs, VPP scaling, run mode) |
| `fake.rs` | `SyntheticDecoder`: made-up frames and an emulated VA surface pool. It is a plain module, not a feature, because `nitro-video --synthetic` ships it |
| `node.rs` | the vocabulary: `Node` (local / helper / graph), `Port`, `LinkRequest` → `LinkOutcome`, `Format` with `Choice::Any` wildcards, `Buffers`, `ClockPoint`, `Clock`, `RunMode` |
| `proto.rs` | app ↔ decode-helper messages (`ToHelper` / `FromHelper`) on `nitro-wire` framing, with fd counts checked both ways |
| `validate.rs` | helper replies checked as hostile input: slot range, fd counts, dma-buf sizes against their planes, monotonic generations, a sane clock |

## Status

**Phase 1 is done (#3988).** `nitro-video` uses these types and the trait, and
still runs FFmpeg in-process: `ffmpeg.rs`, `shim.c` and `build.rs` stay in
`nitro-video` until the helper binary takes them over in phase 2. The protocol
and the validation are defined and unit-tested, but no process speaks them
yet.

Presentation policy stays in `nitro-video`: `choose_output`, `scale_target`,
the ring, and pacing.
