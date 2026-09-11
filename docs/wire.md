# The nitro wire protocol, version 1

This is the contract between a nitro client and the display server: the
scene-graph mutations, buffers, input events and frame timing that
everything else in the tree is built on. It is deliberately small and
boring. The implementation is `crates/nitro-wire`; the byte layout is
defined by the `#[repr(C)]` structs in `msg.rs` and `wire.rs`, and this
document describes them.

**Status.** v1, frozen at M2. Until then, changes are made here and in the
code together. After M2, v1 does not change: additions go in as new op
codes guarded by a capability bit, and only an incompatible change bumps
`VERSION`.

## Transport

A Unix `SOCK_STREAM` socket, by default
`$XDG_RUNTIME_DIR/nitro/wire.sock` (`NITRO_SOCKET` overrides the whole
path; `/tmp/nitro-<uid>/wire.sock` is the fallback when the runtime
directory is unset). Nothing in the framing depends on the socket being
local or on `SOCK_SEQPACKET` — the same bytes run over TCP or an SSH
channel for the remote case, where file-descriptor passing is simply not
available and clients must not use the buffer ops.

Both directions are non-blocking. A client that cannot write blocks
nobody: the unsent bytes stay queued and go out when the socket drains.

## Framing

Every message is one frame:

```text
0        4       6      7      8
+--------+-------+------+------+------------- ... -------------+
| len u32| op u16| fds u8|flag u8|         payload (len)         |
+--------+-------+------+------+------------- ... -------------+
```

| field | type | meaning |
|---|---|---|
| `len` | `u32` LE | payload bytes, header **excluded**. Max 16 MiB (`MAX_PAYLOAD`); larger is a fatal `Limit` error. |
| `op` | `u16` LE | which message. Top bit clear = client → server, set = server → client. |
| `fds` | `u8` | descriptors attached to this frame. Max 8 (`MAX_FDS`). |
| `flags` | `u8` | reserved, must be 0. A non-zero value is a fatal protocol error. |

The payload is the message's fields in declared order, with no padding and
no alignment: a message whose fields are all fixed-size *is* a packed
`repr(C)` struct of little-endian types.

### File descriptors

Descriptors travel as `SCM_RIGHTS` ancillary data on the `sendmsg` that
carries their frame's **header**. Two rules bind the sender:

1. One `sendmsg` may carry the descriptors of **at most one** frame.
2. That call must include the bytes of that frame's header.

`nitro-wire`'s `Socket` splits its writes accordingly. Note that the split
cannot be recovered by re-parsing the outgoing buffer: a short write may
stop *inside* a header, after which the buffer no longer starts on a frame
boundary. The `Writer` therefore tags each queued descriptor with the byte
offset at which its frame's header completes and shifts those tags as bytes
leave, so the rules survive any number of partial writes.

The receiver binds descriptors to frames by byte position, not by syscall:
an fd that arrived with a chunk belongs to the first frame in that chunk
which declares one. This is well defined even though a stream socket gives
no guarantee that a `recvmsg` boundary matches a frame boundary — a chunk
may carry several frames, or half of one.

A frame that declares more descriptors than arrived is a fatal protocol
error; so is a message that leaves an fd unclaimed.

## Primitives

All integers little-endian. No padding anywhere.

| name | size | layout |
|---|---|---|
| `u8` | 1 | |
| `u16` `u32` `u64` | 2, 4, 8 | LE |
| `i32` | 4 | LE two's complement |
| `f32` | 4 | LE IEEE 754 binary32 |
| `bool` | 1 | 0 or 1; **any other byte is a decode error** |
| `str` | 4 + n | `u32` byte length, then UTF-8. No NUL terminator; an embedded NUL is an error. |
| `bytes` | 4 + n | `u32` length, then the bytes. |
| `vec<T>` | 4 + n·k | `u32` item count, then the items back to back. |
| `Point` | 8 | `x: f32`, `y: f32` |
| `Size` | 8 | `w: f32`, `h: f32` |
| `Rect` | 16 | `x, y, w, h: f32` |
| `IRect` | 16 | `x, y, w, h: i32` |
| `Color` | 4 | `r, g, b, a: u8`, sRGB with straight (non-premultiplied) alpha |
| `Transform` | 24 | `a, b, c, d, e, f: f32`, mapping `(x,y)` to `(ax+cy+e, bx+dy+f)` |
| `NodeId` | 4 | `u32`; **0 = none** |
| `BufferId` | 4 | `u32`; **0 = none** |

A declared length larger than `MAX_PAYLOAD` is rejected before anything is
allocated, so a hostile count cannot make the peer reserve gigabytes.

## Ids and transactions

**Ids are client-allocated.** The server keys nodes by `(client, id)`;
there is no id negotiation, no round trip to create anything, and two
clients may use the same numbers.

**Every mutation is buffered.** The server accumulates what a client sends
and applies the whole batch atomically at the next `Commit { serial }`.
Nothing is visible before the commit and the server never renders a
half-applied batch. The serial comes back in `Presented` when the frame
containing the transaction reached the screen.

## Handshake

1. The client sends `Hello { version, name }` as its **first** message.
   Any other message first is a fatal protocol error.
2. The server answers `Welcome { version, caps, name }` if it speaks that
   version, or `Error { code: Version }` followed by a close if not.
3. A second `Hello` is a fatal protocol error.

`caps` is a bitmask; a zero bit means the client must not use the feature.

| bit | name | meaning |
|---|---|---|
| 0 | `DIRECT_SCANOUT` | the server can scan a client buffer out without compositing |
| 1 | `TEXT` | `Text` nodes are accepted (M2) |
| 2 | `DMABUF` | `Surface` nodes backed by dma-bufs are accepted (M5) |
| 3 | `REMOTE` | the link is remote: buffers are expensive, text is cheap |

## Errors

**Every error is fatal.** The server sends `Error { serial, code, msg }`
and closes the connection; there is no per-request error and no recovery.
`serial` is the transaction being applied, or 0 outside one. `msg` is for
logs and is never parsed.

| code | value | when |
|---|---|---|
| `Protocol` | 1 | malformed frame, unknown op, message in the wrong state |
| `UnknownNode` | 2 | node id that does not exist, or belongs to another client |
| `WrongKind` | 3 | operation does not apply to this node kind |
| `BadParent` | 4 | cycle, wrong parent kind, or a `before` that is not a child of the parent |
| `BadBuffer` | 5 | unknown buffer, or fd/size/stride inconsistent with the declared geometry |
| `Limit` | 6 | a protocol limit was exceeded |
| `Version` | 7 | the client asked for a version the server does not speak |

## Enumerations

| type | values |
|---|---|
| `Layer` | `Background` 0, `Normal` 1, `Top` 2, `Overlay` 3 |
| `NodeKind` | `Group` 1, `Rect` 2, `Image` 3, `Text` 4 *(reserved)*, `Surface` 5 *(reserved)* |
| `ButtonState` | `Released` 0, `Pressed` 1 |
| `AxisSource` | `Wheel` 0, `Finger` 1, `Continuous` 2, `WheelTilt` 3 |
| `TouchPhase` | `Down` 0, `Move` 1, `Up` 2, `Cancel` 3 |
| `Fill` tag | `None` 0, `Solid` 1, `Linear` 2 |

A value outside the list is a decode error, not a silently-ignored
unknown. `Text` and `Surface` decode but are rejected by a v1 server
unless the matching capability bit is set.

`window_flags`: `UNDECORATED` 1, `FULLSCREEN` 2, `OPAQUE` 4. Unknown bits
are reserved and must be zero.

Buffer formats are DRM fourcc codes: `XR24` (`0x34325258`, 32 bpp
`[b,g,r,x]`) and `AR24` (`0x34325241`, `[b,g,r,a]`, straight alpha).
Unknown formats earn a `BadBuffer` error from the server, not a decode
error.

## Op codes

Assigned in blocks of 0x100 so a block can grow without renumbering.

### Client → server

| op | message | block |
|---|---|---|
| `0x0001` | `Hello` | session |
| `0x0002` | `Commit` | session |
| `0x0010` | `CreateWindow` | session |
| `0x0011` | `SetWindowTitle` | session |
| `0x0012` | `RequestFrame` | session |
| `0x0101` | `CreateNode` | tree |
| `0x0102` | `DestroyNode` | tree |
| `0x0103` | `Reparent` | tree |
| `0x0104` | `SetBounds` | tree |
| `0x0105` | `SetTransform` | tree |
| `0x0106` | `SetVisible` | tree |
| `0x0201` | `SetOpacity` | style |
| `0x0202` | `SetClip` | style |
| `0x0203` | `SetFill` | style |
| `0x0204` | `SetCorners` | style |
| `0x0205` | `SetBorder` | style |
| `0x0301` | `CreateBuffer` | buffers |
| `0x0302` | `DestroyBuffer` | buffers |
| `0x0303` | `BufferDamage` | buffers |
| `0x0304` | `SetImage` | buffers |

### Server → client

| op | message | block |
|---|---|---|
| `0x8001` | `Welcome` | session |
| `0x8002` | `Error` | session |
| `0x8003` | `Presented` | session |
| `0x8101` | `Configure` | windows |
| `0x8102` | `Frame` | windows |
| `0x8103` | `Focus` | windows |
| `0x8104` | `Closed` | windows |
| `0x8201` | `PointerEnter` | input |
| `0x8202` | `PointerLeave` | input |
| `0x8203` | `PointerMotion` | input |
| `0x8204` | `PointerButton` | input |
| `0x8205` | `PointerAxis` | input |
| `0x8206` | `Key` | input |
| `0x8207` | `Touch` | input |

## Messages, client → server

### `Hello` — 0x0001

| field | type | meaning |
|---|---|---|
| `version` | `u32` | protocol version the client speaks (1) |
| `name` | `str` | client name for logs and the window list; not unique |

Must be the first message. A version mismatch is fatal.

### `Commit` — 0x0002

| field | type | meaning |
|---|---|---|
| `serial` | `u32` | client-chosen, monotonically increasing |

Applies everything sent since the last commit, atomically. The serial is
echoed in `Presented`.

### `CreateWindow` — 0x0010

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | node id for the window's root group |
| `size` | `Size` | requested logical size; the server answers with `Configure` |
| `layer` | `Layer` | stacking layer |
| `flags` | `u32` | `window_flags` bits |
| `title` | `str` | window title |

The window is a normal node in the client's id space; its children are
created with `CreateNode { parent: id }`.

### `SetWindowTitle` — 0x0011

| field | type |
|---|---|
| `window` | `NodeId` |
| `title` | `str` |

### `RequestFrame` — 0x0012

| field | type |
|---|---|
| `window` | `NodeId` |

The server answers with one `Frame` carrying the deadline for the next
flip. One request, one answer: there are no free-running render loops.

### `CreateNode` — 0x0101

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | new id; must be unused and non-zero |
| `kind` | `NodeKind` | what it draws |
| `parent` | `NodeId` | parent; `NONE` is an error (use `CreateWindow` for a root) |
| `before` | `NodeId` | insert before this sibling; `NONE` appends |

### `DestroyNode` — 0x0102

| field | type |
|---|---|
| `id` | `NodeId` |

Recursive. Destroying a window's root group closes the window.

### `Reparent` — 0x0103

| field | type |
|---|---|
| `id` | `NodeId` |
| `parent` | `NodeId` |
| `before` | `NodeId` (`NONE` appends) |

### `SetBounds` — 0x0104

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | |
| `rect` | `Rect` | bounds in the parent's coordinate space |

### `SetTransform` — 0x0105

| field | type |
|---|---|
| `id` | `NodeId` |
| `transform` | `Transform` |

`Group` nodes only; `WrongKind` otherwise.

### `SetVisible` — 0x0106

| field | type |
|---|---|
| `id` | `NodeId` |
| `visible` | `bool` |

Hides the node and its whole subtree.

### `SetOpacity` — 0x0201

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | |
| `opacity` | `f32` | `0.0..=1.0`, clamped by the server; multiplied down the subtree |

### `SetClip` — 0x0202

| field | type |
|---|---|
| `id` | `NodeId` |
| `clip` | `bool` |

`Group` nodes only: clips children to the group's bounds.

### `SetFill` — 0x0203

| field | type |
|---|---|
| `id` | `NodeId` |
| `fill` | `Fill` (tagged, below) |

`Fill` is a `u8` tag followed by its payload:

| tag | variant | payload |
|---|---|---|
| 0 | `None` | — |
| 1 | `Solid` | `Color` |
| 2 | `Linear` | `start: Point`, `end: Point`, `c0: Color`, `c1: Color` |

`Linear` interpolates `c0` at `start` to `c1` at `end`, in the node's local
space. Applies to `Rect` nodes.

### `SetCorners` — 0x0204

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | |
| `radius` | `f32` | corner radius in logical pixels; 0 = square |

### `SetBorder` — 0x0205

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | |
| `width` | `f32` | border width, drawn inside the bounds; 0 = none |
| `color` | `Color` | |

### `CreateBuffer` — 0x0301 — **carries 1 fd**

| field | type | meaning |
|---|---|---|
| `id` | `BufferId` | client-allocated |
| `width` | `u32` | pixels |
| `height` | `u32` | pixels |
| `stride` | `u32` | bytes per row; at least `width * 4` |
| `format` | `u32` | DRM fourcc |
| `size` | `u32` | mapping size in bytes; at least `stride * height` |
| *(fd)* | `SCM_RIGHTS` | memfd or shm descriptor |

The server maps the descriptor **read-only**. The client keeps writing
into it and announces changes with `BufferDamage`. An inconsistent
geometry, size or descriptor is a `BadBuffer` error.

### `DestroyBuffer` — 0x0302

| field | type |
|---|---|
| `id` | `BufferId` |

The server drops its mapping; the id may be reused after the next
`Commit`.

### `BufferDamage` — 0x0303

| field | type | meaning |
|---|---|---|
| `id` | `BufferId` | |
| `rects` | `vec<IRect>` | changed regions in buffer pixel coordinates |

An empty vector means "nothing changed" and is legal.

### `SetImage` — 0x0304

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | the `Image` node |
| `buffer` | `BufferId` | `NONE` detaches |
| `src` | `IRect` | source rectangle in buffer pixels |

## Messages, server → client

### `Welcome` — 0x8001

| field | type |
|---|---|
| `version` | `u32` |
| `caps` | `u32` |
| `name` | `str` |

### `Error` — 0x8002

| field | type |
|---|---|
| `serial` | `u32` |
| `code` | `u16` (`ErrorCode`) |
| `msg` | `str` |

Fatal: the connection closes after it.

### `Presented` — 0x8003

| field | type | meaning |
|---|---|---|
| `serial` | `u32` | the `Commit` that reached the screen |
| `output` | `u32` | which output |
| `time_ns` | `u64` | presentation time, `CLOCK_MONOTONIC` |
| `seq` | `u64` | output frame sequence (vblank count) |

### `Configure` — 0x8101

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `size` | `Size` | logical size the server gave it |
| `scale` | `f32` | output scale factor |
| `output` | `u32` | which output |

Sent when a window is placed, resized or rescaled. The client lays out for
`size` and commits; the new size takes effect at that commit.

### `Frame` — 0x8102

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `deadline_ns` | `u64` | target presentation time, `CLOCK_MONOTONIC` |
| `refresh_ns` | `u32` | output refresh interval |

The answer to one `RequestFrame`. Commit before `deadline_ns`.

### `Focus` — 0x8103

| field | type |
|---|---|
| `window` | `NodeId` |
| `focused` | `bool` |

### `Closed` — 0x8104

| field | type |
|---|---|
| `window` | `NodeId` |

The window is gone; its node id is invalid from here on.

### `PointerEnter` — 0x8201

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `node` | `NodeId` | node under the pointer, or `NONE` |
| `pos` | `Point` | in the window's coordinate space |
| `time_ns` | `u64` | `CLOCK_MONOTONIC` |

### `PointerLeave` — 0x8202

| field | type |
|---|---|
| `window` | `NodeId` |
| `time_ns` | `u64` |

### `PointerMotion` — 0x8203

Same fields as `PointerEnter`.

### `PointerButton` — 0x8204

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `button` | `u32` | Linux evdev code (`BTN_LEFT` = 0x110) |
| `state` | `ButtonState` | |
| `time_ns` | `u64` | |

### `PointerAxis` — 0x8205

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `dx` | `f32` | horizontal scroll, logical pixels |
| `dy` | `f32` | vertical scroll, logical pixels |
| `source` | `AxisSource` | |
| `time_ns` | `u64` | |

### `Key` — 0x8206

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `keycode` | `u32` | Linux evdev code |
| `state` | `ButtonState` | |
| `mods` | `u32` | xkb modifier mask |
| `keysym` | `u32` | resolved keysym, or 0 |
| `time_ns` | `u64` | |
| `utf8` | `str` | text produced; empty for non-printing keys |

### `Touch` — 0x8207

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `id` | `i32` | touch point id, stable from `Down` to `Up`/`Cancel` |
| `phase` | `TouchPhase` | |
| `pos` | `Point` | |
| `time_ns` | `u64` | |

## Deviations from the M1 sketch

The task's sketch is followed except where a fixed-size head had to come
first, or where a name was ambiguous. Every difference:

1. **Variable-length fields move to the end.** `CreateWindow` is
   `{id, size, layer, flags, title}` rather than
   `{id, title, size, layer, flags}`; `Welcome` is
   `{version, caps, name}` rather than `{version, name, caps}`; `Key` puts
   `utf8` last. This lets the fixed part of every message be one packed
   `repr(C)` struct decoded in place, with at most one variable tail.
2. **`Presented` orders `{serial, output, time_ns, seq}`** — the same
   fields, with the 8-byte values after the 4-byte ones.
3. **Field names.** `SetTransform.t` → `transform`; `SetOpacity`'s unnamed
   `f32` → `opacity`; `SetClip`'s bool → `clip`; `SetVisible`'s →
   `visible`; `Focus.focused` and `SetVisible.visible` are `bool`
   (byte 0/1, other values rejected) rather than raw `u8`;
   `SetWindowTitle.id` → `window`, matching the other window ops;
   `CreateBuffer`'s `w, h` → `width, height`.
4. **`flags` on the frame header is validated**, not merely reserved: a
   non-zero value is a fatal protocol error, so a future flag cannot be
   silently ignored by an old peer.
5. **`bool` is strict.** Any byte but 0 or 1 in a bool field is a decode
   error. Same for every tagged enumeration.
6. **`MAX_FDS` is 8** (no v1 message carries more than one; the room is
   for batching later), and the receive ancillary buffer is sized for 16
   as the task asked.
7. **`Fill` is `PartialEq` but not `Eq`**, because it contains `Point`
   (`f32`).
8. **Op codes leave gaps**: `0x0003..0x000f` inside the session block,
   `0x0013..0x00ff` after the window ops, and so on, so each block can
   grow.
9. **No separate `Sender` type.** The sketch asked for `ClientStream` plus
   a `Sender` for `ServerMsg`; sending is instead `ClientStream::send` /
   `flush` on the same object. A `Sender` would have to own or share the
   socket *and* the outgoing buffer, which is precisely what `ClientStream`
   already is; splitting them buys a second borrow to juggle in the epoll
   loop and no separation. If the server ever needs to hand a send handle
   to another component, that is the point to introduce one.
10. **`Writer::put_str` strips embedded NULs** rather than encoding them.
    `Reader::get_str` rejects a NUL as a protocol error, so encoding one
    would let a client kill itself by putting a NUL in a window title;
    stripping keeps the failure local.

## Receive-side limits

The decode path is a hostile boundary, so the receiver bounds every
resource a peer can make it hold:

| limit | value | what it stops |
|---|---|---|
| `MAX_PAYLOAD` | 16 MiB | one oversize frame |
| `MAX_FDS` | 8 | descriptors declared by one frame |
| `MAX_PENDING_FDS` | 64 | **unclaimed** descriptors held by the framer |
| `READ_BUDGET` | 1 MiB | bytes one `read`/`poll` call takes before yielding |

`MAX_PENDING_FDS` is the subtle one. Descriptors are bound to frames by
byte position, so a receiver legitimately holds a few before the frame
claiming them is complete. Without a cap, a peer that attaches an
`SCM_RIGHTS` descriptor to every `sendmsg` while declaring `fds: 0` in
every header parks one open descriptor per call forever — every frame
decodes fine and nothing errors, until the process hits `EMFILE` and, on a
server, takes every other client down with it. Exceeding the cap is a
fatal `UnexpectedFd`.

`READ_BUDGET` stops one busy client from monopolising a single-threaded
event loop: a receive loop that ran until `EAGAIN` would let a peer that
keeps writing hold the loop indefinitely while the framer's buffer grows.
The socket stays readable, so the next epoll wakeup simply continues.

A hangup is reported only once nothing decodable is left, so a loop that
handles one message per wakeup does not lose what already arrived. The
"is anything left?" test is exact rather than a byte count — a peer that
dies mid-frame leaves a partial trailing frame, and counting bytes would
spin forever on it.

## Allocation

`Frame::payload` is a fresh `Vec<u8>` per frame. The task offered either a
borrowed slice or a `Vec`, and v1 takes the `Vec` deliberately: it makes
frame lifetimes independent of the framer's buffer, which is what lets a
receiver hold a decoded message across further reads without fighting the
borrow checker. It is the one allocation per message, and mutations arrive
in the thousands per frame, so it is the first thing to revisit if
profiling says so. The borrow path stays open — `next_frame` could gain a
sibling returning a borrowed frame over the framer's buffer without any
wire change, because the byte layout does not depend on it. Everything
else on the hot path reuses buffers: the `Writer`'s bytes, the `Framer`'s
receive buffer, and the caller-owned `Vec<ServerMsg>` that `poll` appends
to.

## Versioning policy

* v1 is **frozen at M2**. Once the toolkit builds on it, the byte layout of
  every op listed here is fixed.
* Additions after that are **new op codes**, in the gaps or in a new block,
  guarded by a **new capability bit** in `Welcome`. A client that does not
  see the bit must not send the op; a server that receives an op it does
  not know answers `Error { Protocol }`.
* `VERSION` is bumped only for a change that is not expressible that way —
  a different framing, a changed field, a removed op. A version mismatch is
  fatal at handshake: there is no negotiation and no compatibility shim.
* The `payload_layouts_are_frozen` test in
  `crates/nitro-wire/tests/messages.rs` holds golden byte strings; it is
  the tripwire for an accidental layout change.
