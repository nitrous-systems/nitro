# The nitro wire protocol, version 1

This is the contract between a nitro client and the display server: the
scene-graph mutations, buffers, input events and frame timing that
everything else in the tree is built on. It is deliberately small and
boring. The implementation is `crates/nitro-wire`; the byte layout is
defined by the `#[repr(C)]` structs in `msg.rs` and `wire.rs`, and this
document describes them.

**Status.** v1, **frozen** (M2 done, commit 84f7ed3, 2026-09-13). v1 does
not change: additions go in as new op codes guarded by a capability bit,
and only an incompatible change bumps `VERSION`. The last in-place change
was `Configure.position` (task #3683).

## Transport

A Unix `SOCK_STREAM` socket, by default
`$XDG_RUNTIME_DIR/nitro/wire.sock` (`NITRO_SOCKET` overrides the whole
path; `/tmp/nitro-<uid>/wire.sock` is the fallback when the runtime
directory is unset). Nothing in the framing depends on the socket being
local or on `SOCK_SEQPACKET` — the same bytes run over TCP or an SSH
channel for the remote case, where file-descriptor passing is simply not
available and clients must not use the buffer ops.

There is a **second** socket, `$XDG_RUNTIME_DIR/nitro/shell.sock`
(`NITRO_SHELL_SOCKET`), with identical framing and handshake. The only
difference is that a connection accepted there is granted the `SHELL`
capability and may send the ops in the [Shell](#shell-caps-shell)
section. **The socket is the privilege**: see that section and
`docs/shell.md`.

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
| `CursorPos` | 8 | `offset: u32` (byte offset into the text), `x: f32` (logical pixels from the text's left edge) |

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
| 1 | `TEXT` | the server has fonts, so `Text` nodes will actually draw (M2) |
| 2 | `DMABUF` | `Surface` nodes backed by dma-bufs are accepted (M5) |
| 3 | `REMOTE` | the link is remote: buffers are expensive, text is cheap |
| 4 | `WM` | the server manages windows: decorations, states, limits, app ids (M3) |
| 5 | `SHELL` | the connection arrived on the **shell socket** and may send the shell ops (M3) |

`SHELL` is bit 5, not bit 3: bit 3 is `REMOTE` and was taken in M1. It is
*reported*, never negotiated — a client cannot ask for it. See
[Shell](#shell-caps-shell).

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
| `NodeKind` | `Group` 1, `Rect` 2, `Image` 3, `Text` 4, `Surface` 5 *(reserved)* |
| `ButtonState` | `Released` 0, `Pressed` 1 |
| `AxisSource` | `Wheel` 0, `Finger` 1, `Continuous` 2, `WheelTilt` 3 |
| `TouchPhase` | `Down` 0, `Move` 1, `Up` 2, `Cancel` 3 |
| `Align` | `Left` 0, `Center` 1, `Right` 2 |
| `WindowState` | `Normal` 0, `Maximized` 1, `Fullscreen` 2, `Minimized` 3 |
| `Edge` | `Top` 0, `Bottom` 1, `Left` 2, `Right` 3 *(M3, shell)* |
| `Fill` tag | `None` 0, `Solid` 1, `Linear` 2 |

A value outside the list is a decode error, not a silently-ignored
unknown. `Surface` decodes but is rejected by the server, which
advertises no `DMABUF` capability. `Text` is **no longer reserved**: the
node kind is accepted unconditionally and `SetText` always applies. What
the `TEXT` capability bit reports is whether the server found a font to
draw with — a client that ignores the bit gets working, well-formed
metrics for an empty run and paints nothing, rather than a dead
connection. Check the bit before you rely on text being *visible*.

`window_flags`: `UNDECORATED` 1 (the server draws no frame around this
window), `FIXED_SIZE` 2 (the window is not user-resizable: no resize
bands, no maximize), `NO_FOCUS` 4 (the window never takes keyboard focus —
a launcher, a bar). Unknown bits are reserved and must be zero.

`mod_mask` (M3, shell): `SHIFT` 1, `CTRL` 2, `ALT` 4, `SUPER` 8. Used by
`BindKey`, and deliberately **not** the same thing as `Key.mods`: that is
xkb's serialized mask, whose bit positions depend on the compiled keymap,
so it cannot be compared against a constant and a shell could not express
"Super+Return" in it at all. Unknown bits are reserved and must be zero.

`anchor` (M3, shell): `TOP` 1, `BOTTOM` 2, `LEFT` 4, `RIGHT` 8. Used by
`SetAnchor`. Opposite edges together mean "span that axis"; neither means
"centre on it". Unknown bits are reserved and must be zero.

**Errata (M3).** Bits 1 and 2 used to be documented as `FULLSCREEN` and
`OPAQUE`. Both were placeholders that no implementation ever honoured:
fullscreen is now a window *state* (`SetWindowState`, `WindowState`) rather
than a creation flag, and the opacity hint is deferred until there is a
compositor optimisation to feed it to. No byte layout changed — only the
meaning of two bits nothing read — so `VERSION` stays 1.

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
| `0x0013` | `SetWindowState` | session (see `WM`) |
| `0x0014` | `SetWindowLimits` | session (see `WM`) |
| `0x0015` | `SetAppId` | session (see `WM`) |
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
| `0x0206` | `SetText` | style (see `TEXT`) |
| `0x0207` | `MeasureText` | style (see `TEXT`) |
| `0x0301` | `CreateBuffer` | buffers |
| `0x0302` | `DestroyBuffer` | buffers |
| `0x0303` | `BufferDamage` | buffers |
| `0x0304` | `SetImage` | buffers |
| `0x0401` | `SetLayer` | shell (see `SHELL`) |
| `0x0402` | `SetExclusiveZone` | shell (see `SHELL`) |
| `0x0403` | `SetAnchor` | shell (see `SHELL`) |
| `0x0404` | `BindKey` | shell (see `SHELL`) |
| `0x0405` | `UnbindKey` | shell (see `SHELL`) |
| `0x0406` | `GrabKeyboard` | shell (see `SHELL`) |
| `0x0407` | `WindowList` | shell (see `SHELL`) |
| `0x0408` | `FocusWindow` | shell (see `SHELL`) |
| `0x0409` | `CloseWindow` | shell (see `SHELL`) |
| `0x040a` | `SetWindowStateFor` | shell (see `SHELL`) |
| `0x040b` | `Outputs` | shell (see `SHELL`) |

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
| `0x8105` | `WindowState` | windows (see `WM`) |
| `0x8201` | `PointerEnter` | input |
| `0x8202` | `PointerLeave` | input |
| `0x8203` | `PointerMotion` | input |
| `0x8204` | `PointerButton` | input |
| `0x8205` | `PointerAxis` | input |
| `0x8206` | `Key` | input |
| `0x8207` | `Touch` | input |
| `0x8301` | `TextMetrics` | text |
| `0x8302` | `TextMeasured` | text |
| `0x8401` | `HotKey` | shell (see `SHELL`) |
| `0x8402` | `WindowInfo` | shell (see `SHELL`) |
| `0x8403` | `WindowListEnd` | shell (see `SHELL`) |
| `0x8404` | `WindowGone` | shell (see `SHELL`) |
| `0x8405` | `OutputInfo` | shell (see `SHELL`) |
| `0x8406` | `OutputsEnd` | shell (see `SHELL`) |
| `0x8407` | `OutputGone` | shell (see `SHELL`) |

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

### `SetWindowState` — 0x0013

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window |
| `state` | `WindowState` | state to put it in |

Asks the server to put the window into that state. The server answers with
a `WindowState` event once it has. It may refuse — a window created with
`FIXED_SIZE` cannot maximize, for instance — in which case **no event is
sent** and nothing changes; there is no per-request error.

`Configure` carries the resulting size and position, as always: a client
lays out for what `Configure` gives it, not for what it asked for.

`Minimized` keeps the window alive, in the window list and in the
focus-cycling order; it is not a soft close. Only `Closed` ends a window.

Requires the `WM` capability bit.

### `SetWindowLimits` — 0x0014

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window |
| `min` | `Size` | smallest logical content size |
| `max` | `Size` | largest logical content size |

The minimum and maximum logical content size the server will resize this
window to. A zero component means **no limit** on that axis, so
`min = {0, 0}`, `max = {0, 0}` is "resize me freely" — the default.

A `max` below `min` is clamped by the server, never an error: limits are a
hint the window manager applies, not a request that can fail.

Requires the `WM` capability bit.

### `SetAppId` — 0x0015

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window |
| `app_id` | `str` | application identifier |

A stable identifier for the window's *application* (`"org.nitro.calc"`), as
opposed to `SetWindowTitle`, which names the document. The shell uses it
for its window list and for icon lookup. It is free-form: the server does
not validate or interpret it.

Fixed head 4 bytes (`window`), then the string — the same shape as
`SetWindowTitle`.

Requires the `WM` capability bit.

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

### `SetText` — 0x0206

| field | type | meaning |
|---|---|---|
| `node` | `NodeId` | the `Text` node |
| `size_px` | `f32` | font size in logical pixels |
| `weight` | `u16` | CSS-style weight (400 regular, 700 bold) |
| `italic` | `bool` | select an italic face |
| `max_width` | `f32` | wrapping/alignment width; **0 = no limit** |
| `wrap` | `bool` | wrap lines at `max_width`; no effect when `max_width` is 0 |
| `align` | `Align` | horizontal alignment of the lines |
| `color` | `Color` | text colour |
| `family` | `str` | family name, or `sans` / `serif` / `mono` |
| `text` | `str` | the string, UTF-8 |

Fixed head 21 bytes (`4+4+2+1+4+1+1+4`), then `family`, then `text`.

Applies at the next `Commit`, like every other mutation. The server does
the shaping — clients send strings and style, never glyph pixels — and
answers with one `TextMetrics` for **each node it (re)shaped in that
commit**. On a node that is not a `Text` node it is a `WrongKind` error.

The `TEXT` capability bit does **not** gate acceptance: a server with no
fonts still applies `SetText` and still answers `TextMetrics`, for an
empty run of zero width. The bit tells a client whether text will be
*visible*, which is the question a client can actually act on.

Text is **LTR-only in M2**: no bidi, no rich text, one style per node.

### `MeasureText` — 0x0207

| field | type | meaning |
|---|---|---|
| `request` | `u32` | client-chosen; echoed back in `TextMeasured` |
| `size_px` | `f32` | font size in logical pixels |
| `weight` | `u16` | CSS-style weight |
| `italic` | `bool` | select an italic face |
| `max_width` | `f32` | wrapping width; **0 = no limit** |
| `wrap` | `bool` | wrap lines at `max_width`; no effect when `max_width` is 0 |
| `family` | `str` | family name, or `sans` / `serif` / `mono` |
| `text` | `str` | the string to measure, UTF-8 |

Fixed head 16 bytes (`4+4+2+1+4+1`), then `family`, then `text`.

Answered **immediately on receipt**, not at the next commit, with a
`TextMeasured` carrying the same `request`. This is the one
request/response pair in the protocol: a text field needs a measurement
before it can lay itself out, so making it wait for a commit would
deadlock the layout it is part of. It creates no node and mutates
nothing. Answered whether or not the `TEXT` bit is set; without fonts the
answer is an empty measurement rather than an error.

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
| `position` | `Point` | top-left corner on the output, logical units |
| `scale` | `f32` | output scale factor |
| `output` | `u32` | which output |

Sent when a window is placed, resized or rescaled. The client lays out for
`size` and commits; the new size takes effect at that commit.

`position` is the window's content origin in the output's logical
coordinate space, so a client holding a screenshot of the whole output can
crop it to `size` at `position` to get just itself.

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

### `WindowState` — 0x8105

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window |
| `state` | `WindowState` | the state it is now in |

The window's state actually changed — either because the client asked with
`SetWindowState` or because the user did: a shortcut, the maximize button,
a double-click on the title bar. A request the server refuses produces no
event, so receiving one is the only confirmation a state took effect.

`Configure` carries the resulting size and position, as always.

Sent only to clients that were told the `WM` capability bit.

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

### `TextMetrics` — 0x8301

| field | type | meaning |
|---|---|---|
| `node` | `NodeId` | the `Text` node that was shaped |
| `width` | `f32` | width of the longest line, logical pixels |
| `height` | `f32` | total height of all lines |
| `ascent` | `f32` | ascent of the first line above its baseline |
| `descent` | `f32` | descent of the last line below its baseline |
| `line_count` | `u32` | number of laid-out lines |

Sent for every node the server (re)shaped in a commit — that is, one per
`SetText` that was applied. Nothing else triggers a reshape in M2: the
wrap width comes only from `SetText`, so a later `SetBounds` re-*places*
the existing block (and re-aligns it inside the new box) without
re-shaping it, and an output scale change re-rasterizes the glyphs at the
new device size without changing the layout. Reshaping on a bounds or
scale change is a later decision, and would need this sentence to change
with it.

### `TextMeasured` — 0x8302

| field | type | meaning |
|---|---|---|
| `request` | `u32` | the `request` of the `MeasureText` this answers |
| `width` | `f32` | width of the longest line, logical pixels |
| `height` | `f32` | total height of all lines |
| `ascent` | `f32` | ascent of the first line above its baseline |
| `descent` | `f32` | descent of the last line below its baseline |
| `line_count` | `u32` | number of laid-out lines |
| `cursor_x` | `vec<CursorPos>` | cursor positions, in increasing `offset` order |

Fixed head 24 bytes, then the vector. Sent on receipt of the
`MeasureText`, not at a commit. `cursor_x` may be empty.

## Shell (caps `SHELL`)

The bar, the launcher and the wallpaper are ordinary `nitro-ui` clients.
They are not privileged because of anything they *send* — they are
privileged because of **where they connected**.

### The socket rule

* The server listens on **two** sockets with identical framing, handshake
  and op decoding: `$XDG_RUNTIME_DIR/nitro/wire.sock` and
  `$XDG_RUNTIME_DIR/nitro/shell.sock` (`NITRO_SOCKET` /
  `NITRO_SHELL_SOCKET`; `/tmp/nitro-<uid>/…` is the fallback for both).
  Both live in a `0700` directory.
* A connection accepted on the shell socket is answered with
  `Welcome { caps: … | WM | SHELL }`. One accepted on the wire socket never
  has `SHELL` set.
* Every op in this section requires that bit. Sending one without it is
  `Error { Protocol }` and the connection closes — like every other error
  in this protocol, and for the same reason: a client that asked for
  something it may not have has misunderstood its own situation.
* Several shell clients are allowed at once (bar, launcher, wallpaper are
  three processes), and they are all the same user.
* There is no `Spawn` op and no way to acquire the bit at runtime. A shell
  client forks and execs on its own.

What this is *not*: a per-message capability negotiation, a per-app allow
list, or an authentication protocol. `docs/shell.md` states the model, the
threat it does and does not address, and what is deferred.

### Server-global window ids

The ops that act on *another client's* window take a `WindowRef`, a `u32`
id the **server** allocates. It is neither a scene key (internal,
generational) nor a `NodeId` (namespaced per client, so two clients may
both own `NodeId(1)`). A shell only ever learns one from a `WindowInfo`,
and ids are **never reused** — a stale `WindowRef` names nothing, rather
than naming somebody else's window. `WindowRef(0)` is "no window".

The ops that act on one of the sender's *own* windows (`SetLayer`,
`SetExclusiveZone`, `SetAnchor`, `GrabKeyboard`) take the sender's own
`NodeId`, like every other window op. Naming a window the sender does not
own is `Error { UnknownNode }`.

### When a shell op takes effect

The split follows what each op *is*, and it is not uniform:

* The four that name the sender's own window — `SetLayer`,
  `SetExclusiveZone`, `SetAnchor`, `GrabKeyboard` — are **buffered and
  applied at the sender's `Commit`**, exactly like `SetBounds` or
  `SetWindowState`. They have to be: a bar sends `CreateWindow` and
  `SetAnchor` in one transaction, and an anchor applied on receipt would be
  looking for a window the commit has not created yet.
* The other seven are answered **on receipt**. `WindowList` and `Outputs`
  are questions, like `MeasureText`; `BindKey`/`UnbindKey` are
  registrations; and the three `WindowRef` ops act on *another* client's
  window, which the sender's own commit has nothing to do with.

The **privilege check is always on receipt**, whichever group an op is in:
an unprivileged client is disconnected whether or not it ever commits.

Within a commit the four run after every ordinary mutation and *before* the
batch's `SetWindowState` requests, for the same reason state requests come
last: an anchor decides a window's whole rectangle, so it must win over the
client's own `SetBounds` in that batch — and a `Maximized` asked for in the
same batch must win over the anchor, which is the shell deliberately handing
its window to the window manager.

### `SetLayer` — 0x0401

| field | type | meaning |
|---|---|---|
| `window` | `u32` (`NodeId`) | one of the sender's own windows |
| `layer` | `u8` (`Layer`) | `Background`, `Top` or `Overlay` |

`Normal` is `Error { Protocol }`: a shell surface asking to be an ordinary
window has misunderstood the op, and obliging silently would put a bar into
the window-management z-order where a click could raise a document over it.

### `SetExclusiveZone` — 0x0402

| field | type | meaning |
|---|---|---|
| `window` | `u32` (`NodeId`) | one of the sender's own windows |
| `edge` | `u8` (`Edge`) | which edge of the output the space comes off |
| `px` | `u32` | logical pixels to reserve; 0 releases |

The reservation comes off that output's **work area** — what `Maximized`
fills and what new windows are placed into. A 32-px top zone makes every
maximized window 32 px shorter and moves it 32 px down, *immediately*: a
maximized window is re-sized when the zone changes, not at its next
maximize.

Zones on one edge **add** (two bars docked to the top each get a strip),
and a zone larger than the area collapses that axis to zero rather than
going negative. A zone is released by `px: 0`, and also whenever the window
stops **showing** — hidden with `SetVisible { false }`, `Minimized`,
closed, or its client gone — so a bar that toggles itself off hands the
strip back without sending `px: 0` first, and a crashed bar cannot leave the
desktop permanently short. Unhiding restores the zone: it is skipped while
hidden, not forgotten.

The server does **not** place the window for you; `SetAnchor` does. A
shell may legitimately want a zone larger or smaller than the window it
belongs to (a bar with a shadow, a dock that only reserves its resting
height).

### `SetAnchor` — 0x0403

| field | type | meaning |
|---|---|---|
| `window` | `u32` (`NodeId`) | one of the sender's own windows |
| `edges` | `u8` | bitmask from `anchor` |
| `margin` | `u32` | gap in logical pixels on each anchored edge |

Opposite edges together mean "span that axis", so the window is **resized**
to fit; neither means "centre on it", rounded to a whole logical pixel. A
bar is `TOP|LEFT|RIGHT`, a dock `BOTTOM|LEFT|RIGHT`, a centred launcher
`edges: 0`. A reserved bit is `Error { Protocol }`.

Anchoring is against the output's **full** logical rectangle, not its work
area: a bar anchored into the work area would be pushed off the screen by
its own exclusive zone, and a launcher centred in the work area would jump
every time a panel appeared. Anchors are re-applied on every output
change, so a bar keeps spanning after a mode change, a scale change or a
hotplug.

The answer is a `Configure`, like any other server-decided geometry.

### `BindKey` — 0x0404

| field | type | meaning |
|---|---|---|
| `id` | `u32` | the shell's own id for this binding, echoed in `HotKey` |
| `mods` | `u32` | bitmask from `mod_mask` |
| `keysym` | `u32` | X11 keysym, or 0 for a bare-modifier tap |

While bound, the chord is **not** delivered to the focused client as a
`Key` — a global hotkey the focused application could also see would be a
keylogger and an ambiguity at once. It arrives as a `HotKey`.

`keysym: 0` is the **bare-modifier tap**: the binding fires when the
modifier named in `mods` is pressed and released with *nothing else pressed
in between*, which is the launcher's Super trigger. It must name exactly
one modifier — "Super+Shift tapped" has no single press to detect — and
anything else is `Error { Protocol }`.

Refused with `Error { Protocol }`:

* a reserved bit in `mods`;
* a malformed tap (zero or several modifiers with `keysym: 0`);
* one of the **compositor's own** chords (`Ctrl+Alt+*`, `Alt+Tab`,
  `Super+Q/M/F/H/←/→` — `docs/wm.md` has the table). Those are not
  negotiable: `Ctrl+Alt+F2` must switch VT with a wedged shell, and
  `Alt+Tab` is how you leave an application that took the keyboard;
* a chord **another** shell client already holds.

Re-binding the same `id` from the same client replaces it. `Super+Return`
is *not* a compositor chord in M3-B — the launcher binds it here, which is
why `keyboard::hotkey` no longer claims it.

### `UnbindKey` — 0x0405

| field | type |
|---|---|
| `id` | `u32` |

Unbinding an `id` that is not bound is a **no-op**, not an error: a shell
shutting down should not have to remember what it managed to bind. Every
binding of a client is released when it disconnects.

### `GrabKeyboard` — 0x0406

| field | type | meaning |
|---|---|---|
| `window` | `u32` (`NodeId`) | one of the sender's own windows |
| `on` | `bool` | whether to hold the grab |

A grab replaces **focus** as the destination of key events: while it is
held, keys go to this window rather than to the focused one. It is how a
`NO_FOCUS` `Overlay` reads the keyboard without taking focus — the window
that was focused stays focused, keeps its active frame, and is never told
it lost anything. It simply stops receiving keys.

A grab does **not** outrank the bindings that run before delivery. The
full order in the server is: the compositor's own chords, then any
`BindKey` binding, then the grab holder or the focused window. So a chord
that fires is reported as a `HotKey` and is *not* delivered as a `Key` to
the grab holder — which is what lets a launcher opened by a bare-Super tap
be closed by a second tap while it holds the grab. The consequence for a
shell is concrete: **do not bind a chord you also want delivered as a key
to your grabbing window**, because you will get the `HotKey` and not the
`Key`.

Released by `on: false`, by the window ceasing to show (`SetVisible
{ false }` or `Minimized`), by closing it, or by the client disconnecting.
One grab at a time: a second replaces the first.

### `WindowList` — 0x0407

Empty payload. Answered **on receipt**, not at a commit: it is a question,
like `MeasureText`. One `WindowInfo` per window the server knows about,
then one `WindowListEnd`.

It also **subscribes** the connection: afterwards every change produces
another `WindowInfo` and every window that goes a `WindowGone`, so a bar
never polls. Asking again re-sends the snapshot; the subscription is
idempotent.

### `FocusWindow` — 0x0408

| field | type |
|---|---|
| `window` | `u32` (`WindowRef`) |

Raises and focuses another client's window. **Silently refused** for a
window that cannot take focus — `NO_FOCUS`, minimized, unplaced — on
exactly the terms a click on it would be, and for a stale `WindowRef`.
There is no per-request error in this protocol, and a shell's window list
must not be able to wedge the keyboard.

### `CloseWindow` — 0x0409

| field | type |
|---|---|
| `window` | `u32` (`WindowRef`) |

The same *request* the title bar's close button makes: the owning client
gets `Closed` and decides. The server does not tear the window down, so a
client with unsaved work can still refuse.

### `SetWindowStateFor` — 0x040a

| field | type |
|---|---|
| `window` | `u32` (`WindowRef`) |
| `state` | `u8` (`WindowState`) |

`SetWindowState` for someone else's window — what a bar's window list needs
to minimize and restore an entry. Refused silently on the same terms: a
`FIXED_SIZE` window still cannot maximize. The owning client is told with
a `WindowState` event, so it learns what actually happened.

### `Outputs` — 0x040b

Empty payload. Answered on receipt with one `OutputInfo` per connected
output and an `OutputsEnd`, and subscribes the connection to hotplug.

### `HotKey` — 0x8401

| field | type | meaning |
|---|---|---|
| `id` | `u32` | the `id` given to `BindKey` |
| `pressed` | `bool` | the chord went down (`true`) or came up (`false`) |
| `time_ns` | `u64` | event time, `CLOCK_MONOTONIC` |

A chord fires on its press and again on its release, so a shell can
implement press-and-hold. A **bare-modifier tap** is reported *once*, with
`pressed: false`, when the modifier comes back up: until the release the
server cannot know it was a tap rather than the start of a chord, so there
is no press event to report.

### `WindowInfo` — 0x8402

| field | type | meaning |
|---|---|---|
| `window` | `u32` (`WindowRef`) | server-global window id |
| `state` | `u8` (`WindowState`) | what the window is doing |
| `focused` | `bool` | whether it holds keyboard focus |
| `output` | `u32` | the output it is on, or `u32::MAX` for none |
| `layer` | `u8` (`Layer`) | the stacking layer it was created on |
| `app_id` | `str` | from `SetAppId`; empty when the client set none |
| `title` | `str` | window title |

Fixed head 11 bytes, then the two strings. `output` is `u32::MAX` rather
than 0 for "nowhere": output 0 is a real output, and a shell must be able
to tell an unplaced window from one on the primary screen.

`layer` is what separates applications from furniture. A wallpaper, a dock
and a launcher are windows like any other as far as the server is
concerned, so a **task list must filter on `layer == Normal`** or it lists
the shell itself — which is exactly what `nitro-bar` did before this field
existed. It is carried rather than filtered server-side because a pager or
a dock wants the full picture; the server reports every window and the
consumer decides.

Sent for each window in answer to `WindowList`, and again whenever
anything in it changes — a retitle, an app id, a focus change (on **both**
windows), a state change, or a new window being placed.

### `WindowListEnd` — 0x8403

Empty payload. Every window of the snapshot has been sent.

### `WindowGone` — 0x8404

| field | type |
|---|---|
| `window` | `u32` (`WindowRef`) |

The id is retired and will never be issued again.

### `OutputInfo` — 0x8405

| field | type | meaning |
|---|---|---|
| `id` | `u32` | the output id `Configure.output` and `Presented.output` carry |
| `w` | `u32` | width in device pixels |
| `h` | `u32` | height in device pixels |
| `scale` | `f32` | scale factor |
| `x` | `i32` | left edge in the global device-pixel space |
| `y` | `i32` | top edge in the global device-pixel space |
| `refresh_mhz` | `u32` | refresh rate in millihertz (60 000 = 60 Hz) |
| `name` | `str` | connector name (`"HDMI-A-1"`) |

Fixed head 28 bytes, then the name. Outputs are reported in device-x
order, which is the left-to-right connector order the server lays them out
in (`docs/wm.md`).

### `OutputsEnd` — 0x8406

Empty payload. Every output of the snapshot has been sent.

### `OutputGone` — 0x8407

| field | type |
|---|---|
| `id` | `u32` |

A hotplug sends `OutputGone` for whatever vanished and then the **whole**
remaining list, rather than a diff: outputs are few, their positions are
relative to each other (unplugging the left one moves every other), and a
diff a shell had to reassemble would be a second source of truth about the
layout.

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
   `0x0016..0x00ff` after the window ops (M3 took `0x0013..0x0015`), and
   so on, so each block can grow. M3-B added a whole new block,
   `0x_4xx`/`0x84xx`, for the shell ops.
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
11. **The text ops use strict types, not raw bytes.** `SetText.italic`,
    `SetText.wrap` and `MeasureText.italic`, `MeasureText.wrap` are
    `bool`s, and `SetText.align` is an `Align` tag, rather than the `u8`s
    the sketch had: any other byte is a decode error, matching the rest
    of v1.
12. **`SetText` and `MeasureText` have two variable tails.** Their fixed
    heads (21 and 16 bytes) come first as one packed `repr(C)` struct,
    then `family`, then `text`, in that order. The "at most one variable
    tail" rule becomes "the head is still one `repr(C)` struct"; the
    strings are read back to back after it.

## Receive-side limits

The decode path is a hostile boundary, so the receiver bounds every
resource a peer can make it hold:

| limit | value | what it stops |
|---|---|---|
| `MAX_PAYLOAD` | 16 MiB | one oversize frame |
| `MAX_FDS` | 8 | descriptors declared by one frame |
| `MAX_PENDING_FDS` | 64 | **unclaimed** descriptors held by the framer |
| `READ_BUDGET` | 1 MiB | bytes one `read`/`poll` call takes before yielding |

`MAX_PENDING_FDS` is the subtle one, and the two receive limits are
**coupled** — anyone tuning either needs to know why.

Descriptors are claimed only when the frame declaring them is decoded, so
what this cap really bounds is *how many descriptors can arrive between
two drains*, not how many fit in one `recvmsg`. The byte budget does not
bound them: the kernel does not coalesce skbs carrying `SCM_RIGHTS`, so
each `recvmsg` returns exactly one `sendmsg`'s worth. A batch of 65
`CreateBuffer`s is 65 reads of ~32 bytes — about 2 KB, nowhere near a
megabyte, yet 65 unclaimed descriptors.

So a receive loop that reads without draining must **yield** when the
pending count reaches the cap, letting the caller decode and continue on
the next wakeup. It must not read on and let the framer hit the cap
internally, because that is fatal. The check belongs *before* each
`recvmsg`: once the overflow has been seen the connection is already
poisoned.

Yielding alone is not enough either. Yielding is progress only while the
caller has something to drain; at the cap with *nothing decodable left*
the pending descriptors belong to no frame and never will, so a loop that
kept yielding would spin at 100% CPU and never report the attack — worse
than the kill it was meant to avoid. Both halves are needed.

Together they are the only honest test: **do pending descriptors survive a
drain?** A glyph atlas, a tiled surface or a toolkit re-uploading
per-window buffers on an output change all send far more than 64 buffers
in a batch, and all drop to zero pending the moment frames are decoded —
so the receiver yields, the caller drains, and the batch arrives intact. A
peer attaching a descriptor to every `sendmsg` while declaring `fds: 0`
never does: it parks one open descriptor per call forever — every frame
decodes fine and nothing errors, until the process hits `EMFILE` and, on a
server, takes every other client down with it. That case, and only that
case, is a fatal `UnexpectedFd`.

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
* The M2 text ops — `SetText` 0x0206, `MeasureText` 0x0207,
  `TextMetrics` 0x8301, `TextMeasured` 0x8302 — are exactly that
  sanctioned path: four **new op codes** in a gap and a new block,
  guarded by the **existing `TEXT` capability bit** (bit 1). No field of
  any pre-existing message changed, so `VERSION` stays **1**.
* The M3 window-management ops — `SetWindowState` 0x0013,
  `SetWindowLimits` 0x0014, `SetAppId` 0x0015 and `WindowState` 0x8105 —
  are the same sanctioned path again: four **new op codes** in the gaps
  after the existing window ops, guarded by the **new `WM` capability
  bit** (bit 4). No field of any pre-existing message changed, so
  `VERSION` stays **1**.
* The M3-B shell ops — `0x0401..0x040b` and `0x8401..0x8407` — are the
  path once more, in a **new block** of their own and guarded by the **new
  `SHELL` capability bit** (bit 5). Two things about them are worth
  spelling out: the bit is granted by *which socket* a client connected to
  rather than asked for, and the block is separate so the unprivileged
  protocol can keep growing in `0x_0xx..0x_3xx` without ever colliding
  with a privileged op. `VERSION` stays **1**.
* M3-B also **removes a compositor chord**: `Super+Return` was reserved
  for "the launcher" in M3-A and is now bindable through `BindKey`,
  because the launcher exists and binds it. Nothing on the wire changed —
  the chord was never a message — but a client could observe the
  difference, so it is recorded here.
* The one thing M3 does change is the *meaning* of `window_flags` bits 1
  and 2, documented as `FULLSCREEN` and `OPAQUE` and now `FIXED_SIZE` and
  `NO_FOCUS` (see the errata under `window_flags`). No byte layout moved
  and no implementation ever honoured either bit, so this too leaves
  `VERSION` at **1**. A renaming like that is only safe while nothing
  reads the bit; once the toolkit does, the bits are frozen with
  everything else.
* `VERSION` is bumped only for a change that is not expressible that way —
  a different framing, a changed field, a removed op. A version mismatch is
  fatal at handshake: there is no negotiation and no compatibility shim.
* The `payload_layouts_are_frozen` test in
  `crates/nitro-wire/tests/messages.rs` holds golden byte strings; it is
  the tripwire for an accidental layout change.
