# `nitro-media`: nodes, out-of-process decode, PipeWire

Status: **phase 1 built** (#3988): `crates/nitro-media` (library only);
phases 2–5 are design. Research behind it:
[`research/media-kit.md`](research/media-kit.md) (the BeOS Media Kit, and
how PipeWire maps onto it).

Today `nitro-video` links the system FFmpeg (`shim.c` + `ffmpeg.rs`, the
tree's third `unsafe` exception, see `DEPENDENCIES.md`). `nitro-amp` runs
the `ffmpeg` CLI as a child process and plays through
`pw-cat`/`paplay`/`aplay`. Both parse untrusted media with C code, and
neither knows when a sample is actually heard. This page moves demux,
decode and audio output into **sandboxed helper processes, one open file
each**, behind a Rust library whose vocabulary is PipeWire's.

## Decisions

1. **One file per helper at a time, helpers reusable and pre-warmed.** The
   app spawns helpers as children (the `nitro-auth` model), not a system
   media server. A malformed file crashes one playback, not everyone's
   audio. When the app dies, the socket closes and the helper exits. A
   helper that closed its file cleanly goes back to the app's pool for the
   next file; see "Helper lifecycle".
2. **Demux in the helper too.** FFmpeg's demuxers are the mature part.
   The app never sees container bytes, only decoded buffers and metadata.
3. **PipeWire's model is nitro-media's vocabulary**: node, port, link,
   format (with wildcards), buffers, clock. A node is a handle; it may
   live in the app (WAV, synthetic, tests), in a decode helper, or in the
   PipeWire graph (a device, another app's stream). One set of types for
   all three.
4. **The decode helper is the PipeWire client.** Decoded audio goes from
   the helper straight into a PipeWire stream; it never passes through
   the app. The app keeps video presentation, and presents against the
   PipeWire clock the helper publishes to it.
5. **libpipewire is approved** (the project's human, this design) as a
   deliberate C dependency, **linked only by the helper binary**, beside
   libav* and libva. Apps link no C.

## Shape

```text
 nitro-video / nitro-amp              (no C, no unsafe)
   └─ nitro-media (lib)               Node/Port/Link/Format/Buffers/Clock, MediaFile,
        │                             proto + validate, in-process nodes, DSP, fake
        │  socketpair (nitro-wire framing) + SCM_RIGHTS
        ▼
 nitro-media-helper (bin, one file)   shim.c + libavformat/libavcodec/libavutil/libva
        │                             + libpipewire; sandboxed
        │  PipeWire native protocol (fd the app passed in)
        ▼
 pipewire / wireplumber               (system) → ALSA, Bluetooth, …
```

The same split as `nitro-gpu` / `nitro-gpu-vulkan` and `nitro-login` /
`nitro-auth`: the library has the protocol, the validation and a fake
backend; only the helper binary links C. The binary is named for its
role, not for FFmpeg, now that it links two C stacks.

## Vocabulary

| nitro-media | PipeWire | Media Kit |
|---|---|---|
| `Node` (handle; kind: local, helper, graph) | `pw_node`, `media.class` | `media_node`, node kind |
| `Port` (in/out, one format) | `pw_port` | `media_input` / `media_output` |
| `Link` | `pw_link` (often created by WirePlumber) | `BMediaRoster::Connect` |
| `Format`, fields may be `Any` | `EnumFormat` / `Format` params | `media_format` + wildcards |
| `Buffers`: memfd ring or dma-buf pool, recycled | `Buffers` param, `spa_data` MemFd / DmaBuf | `BBufferGroup` |
| `Clock`: `(nsec, position, rate, delay)` | driver, `spa_io_position` | `BTimeSource::PublishTime` |
| `Latency` per port | `SPA_PARAM_Latency` | explicit latency |
| `Props` (volume, mute; later) | `Props` param | `BControllable` |
| `Roster`: devices and nodes | registry / globals | `BMediaRoster` |

Links are **requests**: in PipeWire the session manager decides routing,
so `Link` to a graph node can be refused or rerouted, and the API says so.

## API sketch

The high level is what apps use; the node types sit under it.

```rust
let pool = HelperPool::new(Helpers::default());          // pre-warms one spare
let file = MediaFile::open(&pool, path)?;                 // fd into a warm helper
for t in file.tracks() { /* kind, codec, duration, size / rate */ }

// Audio: the helper's stream node, linked to the default sink by WirePlumber.
let audio = file.play_audio(0, AudioOut { target: Target::Default, dsp: Some(eq) })?;
// Video: buffers into the app; presented against audio.clock().
let mut video = file.video(0, VideoFormat { pixel: Nv12, size: Any, hw: Prefer })?;

pub trait Source: Send {                                   // a node with one output port
    fn format(&self) -> &Format;                          // negotiated; wildcards filled
    fn seek(&mut self, t: Micros, generation: u32) -> Result<Micros, Error>; // where it landed
    fn next(&mut self) -> Result<Option<Buffer>, Error>;   // slot or dma-buf key + pts + generation
    fn release(&mut self, key: BufferKey);                 // recycle
    fn set_mode(&mut self, _: RunMode) {}                  // Realtime (drop allowed) | Offline
}

pub trait Clock {                                          // read-only view, shared memory
    fn now(&self) -> ClockPoint;                           // {mono_ns, media_us, rate, delay_ns}
}
```

The video-specific extras that exist today (`set_scale` for VA-API VPP,
`hw()` pool info) stay on the video source.

## Protocol (app ↔ helper)

Today's C boundary (`nv_open`/`nv_info`/`nv_hw_info`/`nv_seek`/`nv_next`/
`nv_next_hw`/`nv_release`/`nv_set_scale`/`nv_close`), made into messages,
plus lifecycle and audio output:

| app → helper | helper → app |
|---|---|
| `Warm { render fd?, pipewire fd? }` | `Warmed` |
| `Open { file fd }` | `Info { tracks[] }` or `Error { text }` |
| `SelectVideo { track, format }` | `Selected { format }` or refusal by name |
| `Pool { track, memfd, slots, layout }` (software video) | — |
| `Next { track }` | `Buffer { track, slot \| key, pts, generation }`, dma-buf fds on first use of a key |
| `Release { track, key }` | — |
| `PlayAudio { track, target, dsp }` | `Playing { node id, clock memfd }` |
| `Props { volume, mute, eq bands }` | — |
| `Tap { memfd }` (visualiser) | — |
| `Play` / `Pause` / `Seek { t, generation }` | `Seeked { landed_at, generation }` |
| `SetScale { w, h }` | `Scaled` / `ScaleError { text }` |
| `Close` | `Closed` |

Rules:

- **Heavy data only moves as fds.** Video rings are sealed memfds
  (`nitro-shm`) the app allocates and hands over once; VA surfaces are
  DRM-PRIME dma-bufs, each sent once. The clock is one memfd page the
  helper writes under a sequence counter and the app only reads. Audio
  never crosses this socket.
- **The app validates every reply** as hostile input (slot range, fd
  count, dma-buf sizes against the layout, monotonic generations, a sane
  clock), the way `nitro-gpu/validate.rs` checks requests.
- **The helper validates every request** too.

## Audio path and the clock

Inside the helper:

```text
 demux ─▶ audio decode thread ─▶ DSP (EQ, Rust) ─▶ SPSC ring ─▶ pw_stream process callback ─▶ PipeWire
                                               └─▶ tap ring (visualiser, app reads)
```

- The **process callback** (PipeWire's real-time thread) only copies from
  the ring: no decode, no allocation, no locks.
- The stream's format is whatever the decoder produces; PipeWire's
  adapter converts rate, sample format and channel layout to the device
  **(to confirm when building: rate conversion in the adapter, and planar
  `F32P` input)**. That removes the resampling question, and a playlist
  of mixed rates no longer means reopening an output.
- Each cycle the helper writes the driver's `spa_io_position` clock and
  the stream's delay into the clock page. The app maps it to "media time
  heard now" and presents video frames against it. This replaces
  `Sink::latency_frames()`'s estimate with PipeWire's own figure.
- nitro-amp's EQ (`dsp.rs`) moves into `nitro-media` and runs in the
  helper; volume and mute are stream `Props`, so any mixer shows and
  controls the app's stream.

## Helper lifecycle

Startup is paid once per helper, not once per file:

```text
 spawn ─▶ Warm ──Open──▶ Busy ──Close──▶ Warm ── …   (reuse)
            │                │
            │                └─ crash / bad reply / timeout ─▶ killed, never reused
            └─ idle past the app's limit ─▶ exits
```

- **Warm** means the helper has already paid for everything that does not
  depend on the file: exec and dynamic linking, `vaInitialize` on the
  render node (loading the i965/iHD driver), connecting to PipeWire, and
  the privilege drop. What remains for `Open` is probing the container,
  opening the codecs and creating the stream.
- **Pre-warm**: the app spawns one spare at startup, or when it knows a
  file is coming. nitro-amp opens the next playlist entry in the spare
  before the current one ends; two streams briefly overlap in PipeWire,
  which is the route to gapless playback and crossfade. nitro-video warms
  one at launch, while the window maps.
- **Reuse** happens only after a clean `Close`: every track released, the
  demuxer, codecs and stream freed, every pool fd dropped. The helper
  confirms with `Closed`, and the app checks that no dma-buf or pool fd
  is still outstanding. After a crash, a protocol violation or a timeout,
  the helper is killed and never reused.
- **Scope**: helpers are pooled **per app**, never shared between apps. A
  helper only ever receives fds its app already has. The residual risk is
  that a helper compromised by file A corrupts the app's decode of file
  B; that stays inside one app, and an app can cap reuse.
- **Pool size**: at most one warm spare by default. Each warm helper costs
  its RSS, the VA driver's memory and a PipeWire client (to be measured,
  `docs/budget.md`); idle spares exit after a timeout.

## Latency

The helper must not cause **underruns**. Rules:

1. **Audio never waits behind video.** One decode thread per track, and a
   demux thread feeding per-stream packet queues bounded by time
   (seconds), not by count. A full video pool must not stall demux.
2. **The real-time callback only copies.** Decode fills the ring ahead to
   a target depth; the callback never blocks.
3. **Priority**: the PipeWire data thread gets real-time priority the
   usual way (RTKit / `module-rt`); the audio decode thread gets raised
   priority too. The ring depth absorbs the rest.
4. **Warm helpers** keep open and track-change cost off the audible path.
   A crash respawn still costs a gap; acceptable when rare.

## Sandbox

The helper receives: the file fd, the render-node fd (only if VA-API is
wanted), and a **connected PipeWire socket** (the app connects and passes
it; libpipewire can adopt an fd, `pw_context_connect_fd`). It closes
everything else and drops privileges; no filesystem access is needed.
Share `nitro-gpu/sandbox.rs`'s render-node-only fd check. seccomp later.

The PipeWire socket is the weak point: a plain client can usually list
the graph and **open capture nodes** (the microphone). A compromised
helper could record. **Accepted for v1** (the project's human): the
helper runs as an ordinary PipeWire client. **v2** lowers its
permissions: the app restricts the client before handing the fd over
(PipeWire's per-client permission model, as portals use it), or the
helper runs under an access policy that only allows playback.

## Failure

Helper exits or sends garbage → the source reports an error; its
PipeWire stream disappears with the process. The player may respawn the
helper once and seek to the last presented pts. A second failure is
shown to the user and not retried. **A running PipeWire daemon is
assumed** (the project's human): there is no ALSA or `aplay` fallback.
If the connection is refused anyway, playback fails with a plain
message.

## Phases

1. **Library.** Create `crates/nitro-media` with the frame types, `Format`,
   the node vocabulary, the `Source` trait, `SyntheticDecoder` as the
   fake, and the protocol + validation. `nitro-video` depends on it and
   still runs FFmpeg in-process behind the trait. No behaviour change.
   **Done (#3988).** The trait is `VideoSource` (today's decoder seam
   plus `set_mode`; the generation-carrying `seek`/`next` arrive with the
   remote source). `SyntheticDecoder` is a plain module, not a
   `test-support` feature, because `nitro-video --synthetic` ships it.
   Presentation policy (`choose_output`, `scale_target`, the ring,
   pacing) stays in `nitro-video`.
2. **Helper, video.** Create `crates/nitro-media-helper` (`shim.c`,
   `build.rs`, `ffmpeg.rs` move here), a remote source, and the per-app
   helper pool (warm, reuse after `Closed`, kill on failure).
   `nitro-video` goes remote and the in-process path is deleted. Tests:
   kill the helper mid-stream (respawn + seek), hostile replies
   (refused), a reused helper holds no fd from the previous file.
   Measured on the HSW box, into `docs/budget.md`: helper RSS warm and
   busy, cold vs warm time to first frame, per-frame round trip.
3. **Helper, audio.** Link libpipewire; audio decode → DSP → ring → stream;
   the clock page; stream `Props`; the tap ring. `nitro-amp` switches
   from the `ffmpeg` CLI and `sink.rs` to the helper (`wav.rs` and
   `dsp.rs` move into `nitro-media`; `sink.rs` and its
   `pw-cat`/`paplay`/`aplay` detection are deleted). Measured: underruns under load on
   the HSW box, helper RSS with a stream, the clock's delay against
   `pw-top`.
4. **A/V.** `nitro-video` plays sound and presents against the clock page.
5. **Roster (later).** List PipeWire devices and nodes through the same
   types: output selection in nitro-settings, cameras as `Video/Source`,
   nitro-shot recording published as a node.

## Open questions

- **v2: PipeWire permissions for the helper.** Restrict the client
  before passing the fd; the mechanism without a portal is to be worked
  out then.
- **Docs to update as phases land**: `DEPENDENCIES.md` (the `unsafe`
  exception moves crate; `nitro-video` loses its C link; libpipewire
  added with this approval as its record), `docs/surfaces.md` (the
  "future `nitro-media`" wording), `docs/budget.md`, `docs/install.md`
  (`libpipewire-0.3-dev` to build).
