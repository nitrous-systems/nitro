# The BeOS Media Kit, and what `nitro-media` takes from it

Source: the Be Book, as hosted by Haiku
(`https://www.haiku-os.org/legacy-docs/bebook/`, pages
`TheMediaKit_Overview_Introduction`, `…_ReadingWriting`, `BMediaNode`,
`BMediaRoster`, `BBufferProducer`, `BBufferConsumer`, `BBuffer`,
`BBufferGroup`, `BTimeSource`, `BMediaEventLooper`, `BMediaFile`,
`BMediaTrack`, `BSoundPlayer`, `BControllable`, `BMediaAddOn`). Statements
marked **(recollection)** are not from those pages.

The design that follows from this is [`docs/media.md`](../media.md).

## Two levels

The Be Book splits the kit in two.

**High level: play or record.** `BMediaFile` opens a file or a `BDataIO`,
identifies the container by sniffing, and hands out one `BMediaTrack` per
stream. A track offers:

- `DecodedFormat(&fmt)`: the app proposes an output format with
  wildcards, and the codec fills in the rest or refuses;
- `ReadFrames()` for decoded data, `ReadChunk()` for undecoded packets;
- `SeekToTime()`/`SeekToFrame()`, which return where they actually landed
  (a keyframe).

Writing is symmetric: the Be Book's transcoder example is one short loop.
`BSoundPlayer` is the easy audio path, a `PlayBuffer(cookie, buf, size,
format)` callback the system pulls from.

**Low level: a graph of nodes.**

- **Node kinds**, combinable: `BBufferProducer`, `BBufferConsumer` (both =
  *filter*), `BTimeSource`, `BControllable`, plus flags for physical
  input/output and the system mixer.
- **Nodes are handles, not pointers.** Apps name a node by a small
  `media_node` value because nodes are shared across protected address
  spaces. They live in the app or in add-ons (`BMediaAddOn`) loaded by
  `media_addon_server`. Every node has a control port that it receives
  messages on.
- **Endpoints**: `media_source`/`media_destination` are the small "jacks"
  used on the real-time path; `media_output`/`media_input` add name and
  format for UIs.
- **Formats**: `media_format` is a type (raw/encoded × audio/video) plus a
  union of details. Wildcards mean "I'm flexible". `AcceptFormat` fills
  them in, and the Be Book warns that a consumer calling back into the
  producer inside `AcceptFormat` deadlocks. The full connect sequence
  (`FormatProposal` → `AcceptFormat` → `PrepareToConnect` → `Connected` →
  `Connect`) is a **(recollection)**.
- **Buffers**: a `BBuffer` is a header (start time, size used, per-type
  info) over shared-memory areas. Nodes pass references, never copies.
  `BBufferGroup` is a pool, three buffers by default, in one area.
  Consumers recycle buffers to it.
- **Time**, all in µs: *media time* (position in the file), *real time*
  (system clock), *performance time* (when the final output must play
  it). A `BTimeSource` keeps publishing `(performance, real, drift)` with
  `PublishTime()`. Every node is slaved to one source, normally the
  sound card. A seek is `BroadcastTimeWarp()`.
- **Latency is explicit**: algorithmic, processing, scheduling, downstream.
  A producer starts a buffer at *performance time − downstream latency*.
- **Lateness**: a consumer that gets a late buffer calls
  `NotifyLateProducer()`, and the producer's `LateNoticeReceived()`
  reacts according to its **run mode**:
  - `B_DROP_DATA` drops buffers;
  - `B_DECREASE_PRECISION` does cheaper work;
  - `B_INCREASE_LATENCY` gives itself more time;
  - `B_RECORDING` means timestamps are always in the past;
  - `B_OFFLINE` means correctness over timing (rendering to disk).
- `BMediaEventLooper`: a control thread per node, a time-ordered event
  queue, `DispatchEvent(event, lateness)`.
- `BControllable` + `BParameterWeb`: nodes publish parameters, and the
  Media preferences app builds their UI generically.
- `BMediaRoster`: the app's one handle to the system. It creates, connects,
  starts, stops and seeks nodes. `GetAudioMixer()` returns the system
  mixer, which does mixing, format conversion and resampling for every
  app.

The Be Book's own warning: "No portion of the media node protocol is
optional". **(recollection)** Writing a correct node was notoriously
hard. A `media_server` or `media_addon_server` crash silenced every app.
Haiku reimplemented the kit and uses FFmpeg as its codec plugin.

## PipeWire is the same design, current

**(recollection, not verified against PipeWire's source in this
research)** PipeWire has nodes, ports and links; formats are negotiated by
intersecting `EnumFormat` params; buffers are `spa_data` of type MemFd,
DmaBuf or MemPtr, shared by fd; one *driver* node per graph publishes
`spa_io_position` (clock nsec, rate, position, delay) every quantum;
latency is a param; properties (volume, mute) are the `Props` param; the
registry plays `BMediaRoster`. Two differences matter: nodes are pulled
once per quantum by the driver and must not block, and links are usually
made by the session manager (WirePlumber), not the app. `nitro-media`
therefore adopts PipeWire's vocabulary rather than Be's names.

## What nitro does with each trait

| # | Media Kit trait | PipeWire | in nitro |
|---|---|---|---|
| 1 | Two levels: file/track API for apps, node graph underneath | node graph only; `pw_stream` is the easy path | **yes**: `MediaFile` is the API apps use; nodes are the implementation |
| 2 | Nodes are handles, usable across processes | yes, globals by id | **yes**: a node lives in the app, in a decode helper, or in the PipeWire graph; one set of types |
| 3 | `DecodedFormat` with wildcards | `EnumFormat` intersection | **yes**: the app proposes, the helper fills in or refuses by name |
| 4 | Buffers in shared memory, pooled, recycled | MemFd / DmaBuf `spa_data` | **yes**: already built (NV12 memfd ring, VA dma-bufs with `release(key)`); now they cross the helper boundary as fds, sent once |
| 5 | Media / real / performance time; a time source publishes `(perf, real, drift)`; seek = time warp | driver clock, `spa_io_position` | **yes**: the helper's PipeWire stream is the time source; its clock is a memfd page the app reads; seek carries a generation number |
| 6 | Explicit latency | `Latency` param, stream delay | **yes**: PipeWire's own delay figure, replacing `pw-cat`'s estimate |
| 7 | Late notices and run modes | xruns reported, no run modes | **approximated**: the app drops late video frames (`B_DROP_DATA`); `Offline` is the one other mode, for transcoding and capture |
| 8 | System-wide node graph, `media_server`, add-on server, shared nodes | the PipeWire daemon | **via PipeWire**: we run no media server of our own. Decode is one file per helper at a time, helpers pooled per app (pre-warmed, reused after a clean close); a crash loses one playback |
| 9 | System mixer | yes (with WirePlumber) | **PipeWire's**: the helper's stream is one of its clients |
| 10 | `BControllable` / generic parameter UIs | `Props` param | **later**: volume/mute as stream `Props` first |
| 11 | `BMediaEventLooper` per node | data loop per node | **no**: the helper is a request/reply loop plus PipeWire's real-time callback; video timing lives in the app |

