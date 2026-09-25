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
was `WindowInfo.layer` (task #3697), in the `SHELL` block; before that,
`Configure.position` (task #3683). The last *addition* is `SetIcon`
(0x0208) behind the new `ICONS` bit (task #3712, M4-G); before that,
`Theme` (0x8004) behind the `THEME` bit (task #3706, M4).

## Transport

A Unix `SOCK_STREAM` socket, by default
`$XDG_RUNTIME_DIR/nitro/wire.sock` (`NITRO_SOCKET` overrides the whole
path; `/tmp/nitro-<uid>/wire.sock` is the fallback when the runtime
directory is unset). Nothing in the framing depends on the socket being
local or on `SOCK_SEQPACKET`.

Since M4-E1 that is demonstrated rather than asserted: `NITRO_SOCKET`
also takes **`tcp://host:port`**, and the same bytes run over TCP to a
server on another machine. A connection accepted there is granted
`REMOTE` (bit 3) and never `SHELL`, and **file descriptors cannot be
passed on it** — see [caps](#handshake) and `docs/remote.md` for the
model, the security model (loopback plus an SSH forward; there is no
authentication) and the measured numbers.

The endpoint forms are:

| `NITRO_SOCKET` | meaning |
|---|---|
| anything without a `tcp://` prefix | a **path**, verbatim — including one containing colons |
| `tcp://127.0.0.1:7700` | IPv4 literal |
| `tcp://[::1]:7700` | IPv6 literal, **bracketed**; unbracketed is an error, not a guess |
| `tcp://box.local:7700` | a name, resolved with the platform resolver; every address is tried in order |

### Descriptors on a remote link

Sending a frame that carries descriptors on a remote socket is a
**client-side error** (`Error::RemoteNoFds`), raised before the message
is encoded: nothing is queued and no bytes reach the wire. Receiving a
frame that *declares* descriptors on a remote socket is a protocol error
that disconnects — the descriptors can never arrive, so the two ends
disagree about the byte stream, and a desynchronised stream cannot be
resynchronised.

In v1 exactly one message carries a descriptor, `CreateBuffer`, so the
rule in practice is: **no buffers, and therefore no `Image` content, on
a remote link.** The server answers a remote **buffer op** with
`Error { BadBuffer }` but keeps the client connected, which is the one
place an error is not fatal to the connection.

That covers all three of `CreateBuffer`, `BufferDamage` and `SetImage`,
not only the one carrying the fd: the other two merely *name* a buffer,
but a remote client can never have registered one, so all three are
equally impossible and get the same message. Refusing only the first
would leave a client disconnected by the `SetImage` that follows it,
with the worse error arriving after the recoverable one. `SetImage`
naming `BufferId::NONE` is exempt — it *clears* an image node, names no
buffer, and is the one op here a remote client may legitimately send.

A remote receive does `recvmsg` with **no ancillary buffer** at all: a
TCP socket cannot produce an `SCM_RIGHTS` cmsg, so asking for one would
be asking a question with one possible answer.

### Endianness

Every field on the wire is an explicit little-endian type
(`zerocopy::byteorder`), with no padding and no alignment requirement,
so a **mixed x86-64/aarch64 pair is fine**. This is pinned by
`the_wire_image_is_little_endian_and_pinned` in
`crates/nitro-wire/tests/tcp.rs`, which compares an encoded `SetBounds`
against a byte literal — a field that ever became native-endian, or was
reordered or padded, fails there on the machine that built it rather
than on someone's ARM laptop.

There is a **second** socket, `$XDG_RUNTIME_DIR/nitro/shell.sock`
(`NITRO_SHELL_SOCKET`), with identical framing and handshake. The only
difference is that a connection accepted there is granted the `SHELL`
capability and may send the ops in the [Shell](#shell-caps-shell)
section. **The socket is the privilege**: see that section and
`docs/shell.md`. It is therefore **never TCP** — a `tcp://` in
`NITRO_SHELL_SOCKET` is an error, because a port cannot prove what a
`0700` path proves.

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

**Both directions carry descriptors.** Until M5-A only the client did
(`CreateBuffer`); the server now sends `Keymap` (0x8208) and
`SelectionData` (0x8502), and the client also sends `SendSelection`
(0x0307). The two rules above are unchanged and apply to the server's
writes exactly as to the client's — the `Writer` and `Socket::send_all`
that implement the split are the same code in both directions. Three
consequences are worth naming, because three places used to assume the
server never sent one:

* `needs_fd` is **direction-agnostic**. Ops are numerically disjoint
  between the directions, so one function classifies both.
* The server's `send` refuses an fd-carrying message on a **remote** (TCP)
  link before encoding it, exactly as the client's has since M4-E1. A
  frame whose header promises descriptors that can never arrive would
  desynchronise the peer's stream, which is worse than the feature not
  working.
* The client's receive loop applies the same `MAX_PENDING_FDS` yield rule
  the server's has; see [Receive-side limits](#receive-side-limits).

Who creates and who closes each descriptor is per message; the data
transfer table is under
[Descriptor ownership](#descriptor-ownership-per-message), and `Keymap`
and `CreateBuffer` state theirs in their own sections.

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
| `vec<T>` | 4 + n·k | `u32` item count, then the items back to back. Fixed-size items only. |
| `vec<str>` | 4 + … | `u32` item count, then that many `str` fields back to back. |
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

`vec<str>` needs its own guard, because its items have no fixed size and
the `count × item_size` check cannot be made. Every `str` carries at least
its own 4-byte length prefix, so a count needing more than `4 × count`
remaining bytes is `Truncated` **before** anything is reserved — the same
shape of check, adapted.

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
| 6 | `THEME` | the server owns the colour palette and pushes it (M4); see [`Theme`](#theme--0x8004) |
| 7 | `ICONS` | the server has the symbolic icon set, so `Icon` nodes will actually draw (M4-G); see [`SetIcon`](#seticon--0x0208) |
| 8 | `POPUP` | `CreatePopup` / `RepositionPopup` / `PopupDone` are accepted (M5-A) |
| 9 | `CURSOR` | `SetCursor` — a client may ask for a named cursor shape (M5-A) |
| 10 | `DRAG` | `StartMove` / `StartResize` — client-initiated **window** drag, not DnD (M5-A) |
| 11 | `OUTPUTS` | `ListOutputs` — an unprivileged client may enumerate outputs (M5-A) |
| 12 | `KEYMAP` | the server sends the xkb `Keymap` and `Modifiers` (M5-A) |
| 13 | `RELEASE` | the server sends `BufferReleased` (M5-A) |
| 14 | `DATA` | clipboard **and** drag-and-drop; see [Data transfer](#data-transfer-caps-data) (M5-A) |

Bits 8–14 together are `caps::CAPS_M5_MASK`, the range
[`ClientCaps`](#capability-opt-in-clientcaps) governs.

`DATA` is **one** bit for two features because they are one mechanism: the
offer, the MIME list and the descriptor relay are shared, and
`RequestSelection` names which of the two it means with a one-byte
`DataSource`. Two bits would mean two copies of the same four messages.

**None of bits 8–14 is advertised yet.** M5-A froze the protocol surface
ahead of the behaviour, deliberately, so that the eight follow-up tasks
implement against bytes nobody can still change. Until each lands, the
server advertises no bit above 7 and refuses every M5-A client op with
`Error { Protocol }` — which is the correct answer rather than a stub,
because no conformant client sends one without the bit.

`SHELL` is bit 5, not bit 3: bit 3 is `REMOTE` and was taken in M1. It is
*reported*, never negotiated — a client cannot ask for it. See
[Shell](#shell-caps-shell).

`THEME` is set unconditionally by the current server, for the reason `WM`
is: the server always owns a palette and always pushes it. It is still a
bit rather than an assumption, because it is what tells a client whether
to *wait* for colours or fall back to its built-in ones — and because a
future non-desktop server (a remote view, a test fixture) may honestly
not have a palette to push.

`ICONS` has the shape of `TEXT` rather than of `THEME`: it says the
server has artwork to draw with, the way `TEXT` says it found a font. The
current server compiles the set in, so it always sets the bit — but it is
asked rather than asserted, because a stripped or fixture server honestly
may not have one, and because a client that checks the bit lays out
identically either way (an `Icon` node measures a square box whatever the
answer, and simply paints nothing without it). See `docs/icons.md`.

`REMOTE` is **granted since M4-E1**, for and only for a connection
accepted on the TCP listener (`remote.listen` in `server.conf`). It is
reported on the same terms: the fact is which socket the client reached,
and the bit is how the server says so. A client that sees it must not
use `CreateBuffer`, `BufferDamage` or `SetImage` — see
[Descriptors on a remote link](#descriptors-on-a-remote-link) and
`docs/remote.md`. `REMOTE` and `SHELL` never appear together: a TCP port
cannot prove what a `0700` path proves.

### Capability opt-in: `ClientCaps`

A capability bit alone cannot carry a **server → client** addition, and
M5-A is the first change to make that matter.

The mechanics: `ServerMsg::decode` answers `UnknownOp` for an op it does
not know, and the client's `poll` treats that as fatal. So *any* new
server→client message pushed unprompted kills an older client. "Sending
the request proves knowledge" does not save the M5-A ops either: a drop
target never sent a `DATA` op, yet the whole `DragEnter…DragDrop` side is
pushed at it. The `THEME` precedent (0x8004, pushed to everyone right
after `Welcome`) got away with it only because every client ships from
this tree; Route A ends that, because the Chromium backend is built out of
tree and versioned independently.

So a client names the subset it is ready to receive:

```text
ClientCaps = 0x0003   { caps: u32 }
```

The rules:

1. **The server must not send a message belonging to a bit the client did
   not list.** No `ClientCaps` = 0 = a pre-M5-A client, which sees exactly
   the v1 message set.
2. It gates the **server → client** direction only. Client → server ops
   stay gated on the server's `Welcome` bits, as before. The two are
   orthogonal: a client may send `ListOutputs` only because the server
   advertised `OUTPUTS`, and will be *answered* only because it listed
   `OUTPUTS` in its `ClientCaps`.
3. Sending a client op whose bit was not listed is `Error { Protocol }`.
   A client that asks for outputs while claiming not to understand the
   answer is confused, and this is the cheap place to say so.
4. Listing a bit means "I know every message this document ties to that
   bit **at this `VERSION`**". That is what lets
   [`IconRefused`](#iconrefused--0x8303) ride the existing `ICONS` bit.
5. A second `ClientCaps` replaces the first — a client may narrow or
   widen — and is not an error.

**Discovery.** `ClientCaps` is deliberately behind no capability bit of
its own, which raises the obvious question: how does a client know the
server understands 0x0003? The rule is

> send `ClientCaps` **only if** `Welcome.caps` carried at least one bit
> ≥ 8 (`caps::CAPS_M5_MASK`).

A server advertising any bit in that range necessarily knows the op,
because the bits and the op arrived in the same change. A server
advertising none has nothing to opt in to, so sending it there is both
useless and fatal — the exact failure `ClientCaps` exists to prevent,
running backwards. `Connection::client_caps` *enforces* this rather than
merely documenting it: it returns without sending when no such bit was
advertised, so the footgun is unreachable through the helper. A
hand-rolled client that sends the frame anyway still dies, which is
correct.

**Grandfather clause.** `ClientCaps` governs **bits 8 and above**, plus
the single named exception of `IconRefused` under the existing `ICONS`
bit. The v1-era unconditional pushes — `Theme` under `THEME`, and every
message of the frozen v1 set — are unchanged and are **not** subject to
it. The mechanism exists because M5-A is the first change to add
server→client messages a client may not know; it does not reach backwards,
and reading rule 1 as retiring the unconditional `Theme` would be a
behaviour change of `VERSION`-bump weight.

## Errors

**Every error is fatal, with two named exceptions.** The server sends
`Error { serial, code, msg }` and closes the connection; there is no
per-request error and no recovery. `serial` is the transaction being
applied, or 0 outside one. `msg` is for logs and is never parsed.

The exceptions are `BadBuffer` for a **remote** client's buffer op (see
[Descriptors on a remote link](#descriptors-on-a-remote-link)) and
`BadIcon` for an unknown icon name. Both share a shape: the thing the
client asked for is permanently unavailable *to it*, the frame was
consumed whole so the stream is still in step, and killing the connection
would cost an application rather than a feature. A desktop must not lose
an app because one of its widgets named an icon a newer set has.

| code | value | when |
|---|---|---|
| `Protocol` | 1 | malformed frame, unknown op, message in the wrong state |
| `UnknownNode` | 2 | node id that does not exist, or belongs to another client |
| `WrongKind` | 3 | operation does not apply to this node kind |
| `BadParent` | 4 | cycle, wrong parent kind, or a `before` that is not a child of the parent |
| `BadBuffer` | 5 | unknown buffer, or fd/size/stride inconsistent with the declared geometry |
| `Limit` | 6 | a protocol limit was exceeded |
| `Version` | 7 | the client asked for a version the server does not speak |
| `BadIcon` | 8 | `SetIcon` named an icon the server does not have — **not fatal** |

## Enumerations

| type | values |
|---|---|
| `Layer` | `Background` 0, `Normal` 1, `Top` 2, `Overlay` 3 |
| `NodeKind` | `Group` 1, `Rect` 2, `Image` 3, `Text` 4, `Surface` 5 *(reserved)*, `Icon` 6 |
| `ButtonState` | `Released` 0, `Pressed` 1 |
| `AxisSource` | `Wheel` 0, `Finger` 1, `Continuous` 2, `WheelTilt` 3 |
| `TouchPhase` | `Down` 0, `Move` 1, `Up` 2, `Cancel` 3 |
| `Align` | `Left` 0, `Center` 1, `Right` 2 |
| `WindowState` | `Normal` 0, `Maximized` 1, `Fullscreen` 2, `Minimized` 3 |
| `Edge` | `Top` 0, `Bottom` 1, `Left` 2, `Right` 3 *(M3, shell)* |
| `Fill` tag | `None` 0, `Solid` 1, `Linear` 2 |
| `PopupAnchor` | `None` 0, `Top` 1, `Bottom` 2, `Left` 3, `Right` 4, `TopLeft` 5, `BottomLeft` 6, `TopRight` 7, `BottomRight` 8 *(M5-A)* |
| `PopupGravity` | the same nine values, same numbering *(M5-A)* |
| `DragAction` | `None` 0, `Copy` 1, `Move` 2, `Link` 3 *(M5-A)* |
| `KeymapFormat` | `XkbV1` 1 *(M5-A)* |
| `DataSource` | `Clipboard` 0, `Drag` 1 *(M5-A)* |
| `CursorShape` (`u16`) | `None` 0, then `wp_cursor_shape_device_v1` 1–34 — see below *(M5-A)* |

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

`CursorShape` (M5-A) is a **`u16`**, not a byte, and its values 1–34 are
`wp_cursor_shape_device_v1`'s **verbatim**:

| value | name | value | name | value | name |
|---|---|---|---|---|---|
| 0 | `None` (hide) | 12 | `Copy` | 24 | `SwResize` |
| 1 | `Default` | 13 | `Move` | 25 | `WResize` |
| 2 | `ContextMenu` | 14 | `NoDrop` | 26 | `EwResize` |
| 3 | `Help` | 15 | `NotAllowed` | 27 | `NsResize` |
| 4 | `Pointer` | 16 | `Grab` | 28 | `NeswResize` |
| 5 | `Progress` | 17 | `Grabbing` | 29 | `NwseResize` |
| 6 | `Wait` | 18 | `EResize` | 30 | `ColResize` |
| 7 | `Cell` | 19 | `NResize` | 31 | `RowResize` |
| 8 | `Crosshair` | 20 | `NeResize` | 32 | `AllScroll` |
| 9 | `Text` | 21 | `NwResize` | 33 | `ZoomIn` |
| 10 | `VerticalText` | 22 | `SResize` | 34 | `ZoomOut` |
| 11 | `Alias` | 23 | `SeResize` | | |

Borrowing an externally validated list makes a future Wayland adapter a
cast and stops this enum being re-litigated every time a new cursor is
wanted. It covers every `ui::mojom::CursorType` a Chromium backend needs:
the panning family maps to `AllScroll`, the `*NoResize` family to
`NotAllowed`, and the `kDnd*` family to `NoDrop`/`Move`/`Copy`/`Alias`.
`CursorType::kCustom` is deliberately unsupported — see the Versioning
policy.

`constraint_adjust` (M5-A): `SLIDE_X` 1, `SLIDE_Y` 2, `FLIP_X` 4,
`FLIP_Y` 8, `RESIZE_X` 16, `RESIZE_Y` 32. Used by `CreatePopup` and
`RepositionPopup`; the values match `ui::OwnedWindowConstraintAdjustment`.
Unknown bits are reserved and must be zero.

> **`RESIZE_Y` is spelled correctly here.** Chromium's constant is
> `kAdjustmentRezizeY` — a typo upstream, at the same bit. It is
> deliberately not copied; do not "fix" ours to match when comparing the
> two files.

`popup_flags` (M5-A): `GRAB` 1 — take the pointer grab, so a click
outside the popup chain dismisses the whole chain and is **consumed**
rather than delivered. Unknown bits are reserved and must be zero.

`resize_edges` (M5-A): `TOP` 1, `BOTTOM` 2, `LEFT` 4, `RIGHT` 8. Used by
`StartResize`. **The same bit positions as `anchor`, by design**, so a
toolkit holding one edge set can hand it to either — but a separate module
in the code, because a resize edge set and a window anchor are not the
same idea and a shared name would invite them to drift into one. Two bits
are a corner; 0 means "the server picks". Unknown bits are reserved and
must be zero.

`drag_actions` (M5-A): `COPY` 1, `MOVE` 2, `LINK` 4 — the *set* a drag
source offers, on `StartDrag` and `DragEnter`. The destination picks one
and names it as a `DragAction` in `AcceptDrop`. Without this field
copy-versus-move (the Ctrl/Shift behaviour every file manager has) could
not be expressed at all, and `AcceptDrop.action` would have nothing to be
chosen from. Unknown bits are reserved and must be zero.

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
| `0x0003` | `ClientCaps` | session (M5-A; **no bit of its own**) |
| `0x0010` | `CreateWindow` | session |
| `0x0011` | `SetWindowTitle` | session |
| `0x0012` | `RequestFrame` | session |
| `0x0013` | `SetWindowState` | session (see `WM`) |
| `0x0014` | `SetWindowLimits` | session (see `WM`) |
| `0x0015` | `SetAppId` | session (see `WM`) |
| `0x0016` | `CreatePopup` | session (see `POPUP`) |
| `0x0017` | `RepositionPopup` | session (see `POPUP`) |
| `0x0018` | `SetCursor` | session (see `CURSOR`) |
| `0x0019` | `StartMove` | session (see `DRAG`) |
| `0x001a` | `StartResize` | session (see `DRAG`) |
| `0x001b` | `ListOutputs` | session (see `OUTPUTS`) — **answered in `0x84xx`** |
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
| `0x0208` | `SetIcon` | style (see `ICONS`) |
| `0x0301` | `CreateBuffer` | buffers |
| `0x0302` | `DestroyBuffer` | buffers |
| `0x0303` | `BufferDamage` | buffers |
| `0x0304` | `SetImage` | buffers |
| `0x0305` | `SetSelection` | data transfer (see `DATA`) |
| `0x0306` | `RequestSelection` | data transfer (see `DATA`) |
| `0x0307` | `SendSelection` | data transfer (see `DATA`) — **carries 1 fd** |
| `0x0308` | `StartDrag` | data transfer (see `DATA`) |
| `0x0309` | `AcceptDrop` | data transfer (see `DATA`) |
| `0x030a` | `FinishDrag` | data transfer (see `DATA`) |
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
| `0x040c` | `Lock` | shell (see `SHELL`) |
| `0x040d` | `Unlock` | shell (see `SHELL`) |

### Server → client

| op | message | block |
|---|---|---|
| `0x8001` | `Welcome` | session |
| `0x8002` | `Error` | session |
| `0x8003` | `Presented` | session |
| `0x8004` | `Theme` | session (see `THEME`) |
| `0x8101` | `Configure` | windows |
| `0x8102` | `Frame` | windows |
| `0x8103` | `Focus` | windows |
| `0x8104` | `Closed` | windows |
| `0x8105` | `WindowState` | windows (see `WM`) |
| `0x8106` | `PopupDone` | windows (see `POPUP`) |
| `0x8201` | `PointerEnter` | input |
| `0x8202` | `PointerLeave` | input |
| `0x8203` | `PointerMotion` | input |
| `0x8204` | `PointerButton` | input |
| `0x8205` | `PointerAxis` | input |
| `0x8206` | `Key` | input |
| `0x8207` | `Touch` | input |
| `0x8208` | `Keymap` | input (see `KEYMAP`) — **carries 1 fd** |
| `0x8209` | `Modifiers` | input (see `KEYMAP`) |
| `0x8301` | `TextMetrics` | replies about content |
| `0x8302` | `TextMeasured` | replies about content |
| `0x8303` | `IconRefused` | replies about content (see `ICONS`) |
| `0x8305` | `BufferReleased` | replies about content (see `RELEASE`) |
| `0x8401` | `HotKey` | shell (see `SHELL`) |
| `0x8402` | `WindowInfo` | shell (see `SHELL`) |
| `0x8403` | `WindowListEnd` | shell (see `SHELL`) |
| `0x8404` | `WindowGone` | shell (see `SHELL`) |
| `0x8405` | `OutputInfo` | shell (see `SHELL`) |
| `0x8406` | `OutputsEnd` | shell (see `SHELL`) |
| `0x8407` | `OutputGone` | shell (see `SHELL` **or** `OUTPUTS`) |
| `0x8408` | `OutputWorkArea` | shell (see `SHELL` **or** `OUTPUTS`) |
| `0x8501` | `SelectionOffer` | data transfer (see `DATA`) |
| `0x8502` | `SelectionData` | data transfer (see `DATA`) — **carries 1 fd** |
| `0x8503` | `SelectionRequest` | data transfer (see `DATA`) |
| `0x8504` | `DragEnter` | data transfer (see `DATA`) |
| `0x8505` | `DragMotion` | data transfer (see `DATA`) |
| `0x8506` | `DragLeave` | data transfer (see `DATA`) |
| `0x8507` | `DragDrop` | data transfer (see `DATA`) |
| `0x8508` | `DragFinished` | data transfer (see `DATA`) |

Two block notes, both warts kept deliberately.

**Server block 3 is "replies about content", not only text.** The module
table calls client `0x_3xx` *buffers* and server `0x83xx` *text*, because
M2 put `TextMetrics`/`TextMeasured` there first. `IconRefused` at `0x8303`
and `BufferReleased` at `0x8305` restore the block number's original
meaning — one block for the server's replies about a client's content —
rather than opening a block per subject. **`0x8304` is deliberately
free**: it is not a gap to be filled by the next thing that needs a
number, it is simply unassigned.

**`ListOutputs` is an unprivileged op answered in the shell block.**
`0x001b` is answered by `OutputInfo` `0x8405`, `OutputWorkArea` `0x8408`
and `OutputsEnd` `0x8406`, with later `OutputGone` `0x8407`s. Duplicating
four messages into a new block to keep the numbering tidy would be worse:
two encoders, two decoders and two things to keep in step forever. The
access rule, stated once here and again under
[Shell](#shell-caps-shell): **those four messages are sent to a client
holding either `SHELL` or `OUTPUTS`.**

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

### `ClientCaps` — 0x0003

| field | type | meaning |
|---|---|---|
| `caps` | `u32` | bits whose **server → client** messages this client understands |

Fixed head 4 bytes. Behind no capability bit of its own; see
[Capability opt-in](#capability-opt-in-clientcaps) for the five rules, the
discovery rule (send it only when `Welcome.caps` carried a bit ≥ 8) and
the grandfather clause (it governs bits 8 and above, plus `IconRefused`
under `ICONS`).

`caps` must be a subset of what `Welcome` advertised: claiming to
understand messages the server never offered is `Error { Protocol }`.

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

### `CreatePopup` — 0x0016

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | new id for the popup's root group |
| `parent` | `NodeId` | the parent window, or a parent popup for a submenu chain |
| `anchor_rect` | `IRect` | rectangle in the parent's logical space to anchor against |
| `anchor` | `u8` (`PopupAnchor`) | which point of `anchor_rect` the popup hangs off |
| `gravity` | `u8` (`PopupGravity`) | which way it grows from that point |
| `constraint` | `u32` | `constraint_adjust` bitmask |
| `size` | `Size` | requested size in logical pixels |
| `flags` | `u32` | `popup_flags` bitmask |

Fixed head **42 bytes**; every field is fixed, so the payload is the head.
Requires `POPUP`.

A popup is a menu or a tooltip — what Wayland calls a popup, and what
Chromium calls `kMenu`/`kTooltip`. Chromium's `kPopup`/`kBubble` are
subsurfaces and want ordinary nodes in the parent window instead; they do
**not** need this op.

**The answer is an ordinary `Configure`.** A popup is a window, so the
server reports the geometry it actually gave — after any sliding,
flipping or shrinking `constraint` allowed — through
[`Configure`](#configure--0x8101), and there is deliberately no
`PopupConfigure`. `Configure.position` keeps its existing meaning,
**output-global logical coordinates**, and is *not* parent-relative the
way `xdg_popup.configure` is: nitro has no reason to hide global
coordinates (`Configure` has always carried them, which is what lets a
client crop a screenshot of its own window), and a backend that needs
parent-relative subtracts the parent's `Configure.position`, which it
already holds.

**Why `anchor_rect` is an `IRect`.** It is the one integer rectangle in a
logical-pixel space — everywhere else in the unprivileged blocks, logical
geometry is `f32` (`SetBounds.rect`, `CreateWindow.size`,
`Configure.position`), and `IRect` otherwise appears only in *buffer
pixel* coordinates. The argument for keeping it integer: both ends of the
only conversation this field has — `xdg_positioner.set_anchor_rect` and
`ui::OwnedWindowAnchor::anchor_rect`, which is a `gfx::Rect` of integer
DIP — are integer, so an `f32` here would be a widening on the way in and
a rounding on the way out, with the rounding landing inside the
flip/slide/resize arithmetic. A menu that jitters by a subpixel when its
parent moves is what that buys. And no client has a fractional anchor to
express: an anchor rect is a widget's box, not a transform. (Both types
are 16 bytes, so the head is 42 either way.)

`flags` bit `GRAB` takes the pointer grab: a click outside the popup chain
dismisses the whole chain and is **consumed**, not delivered. Dismissal is
reported with [`PopupDone`](#popupdone--0x8106), after the popup has
already been unmapped — the unmap-then-notify ordering Chromium expects.

Authorized by **owning the parent window**, not by an input serial; see
the Versioning policy.

### `RepositionPopup` — 0x0017

| field | type | meaning |
|---|---|---|
| `id` | `NodeId` | the popup, by the sender's own node id |
| `anchor_rect` | `IRect` | new anchor rectangle in the parent's logical space |
| `anchor` | `u8` (`PopupAnchor`) | which point of it the popup hangs off |
| `gravity` | `u8` (`PopupGravity`) | which way it grows |
| `constraint` | `u32` | `constraint_adjust` bitmask |

Fixed head **26 bytes**. Requires `POPUP`. Answered with another
`Configure`, exactly as `CreatePopup` is.

**There is deliberately no reposition token.** `xdg_positioner` hands one
back in `xdg_popup.repositioned` so a client can tell which request a
configure answers — but Chromium's `XdgPopup::OnRepositioned` is
`NOTIMPLEMENTED_LOG_ONCE()`
(`ui/ozone/platform/wayland/host/xdg_popup.cc:360-362`), so the token is
never matched. A token here would be bytes on every reposition serving one
call site that does nothing. Noted so a Wayland-literate reader sees a
decision rather than an oversight.

### `SetCursor` — 0x0018

| field | type | meaning |
|---|---|---|
| `shape` | `u16` (`CursorShape`) | the shape wanted; `None` (0) hides the cursor |

Fixed head **2 bytes**. Requires `CURSOR`.

**Names no window, on purpose.** The cursor is a property of the
*pointer*, not of a surface — `wl_pointer.set_cursor` names no target
window either — and the authorization is "does this client hold pointer
focus **now**", which the server answers itself. A client that does not is
**silently ignored**, not disconnected: focus can legitimately leave
between the send and the receive, and killing a client for losing that
race would be killing it for being correct.

A **named shape, never a bitmap**. The compositor knows better how to draw
a cursor at the output's scale, which is the argument `wp_cursor_shape_v1`
itself makes, and it is why the enum is that protocol's list verbatim. See
[Enumerations](#enumerations) for the table and the Versioning policy for
why `CursorType::kCustom` is unsupported.

### `StartMove` — 0x0019

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | one of the sender's own windows |

Fixed head **4 bytes**. Requires `DRAG`.

What a client-side-decorated window needs to be draggable by its own title
bar — without it, a client that draws its own decorations cannot be moved
at all. The server drives the drag from there exactly as it does for one
begun on a server-drawn frame.

Authorized by pointer focus **and a button actually being down**; ignored
otherwise, on the same terms as `SetCursor`.

### `StartResize` — 0x001a

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | one of the sender's own windows |
| `edges` | `u8` | `resize_edges` bitmask: what the user grabbed |

Fixed head **5 bytes**. Requires `DRAG`. Two bits are a corner; `0` lets
the server choose. Same authorization as `StartMove`.

### `ListOutputs` — 0x001b

No fields; head **0 bytes**. Requires `OUTPUTS`.

Exactly [`Outputs`](#outputs--0x040b) without the shell socket. Answered
at once with one `OutputInfo` and one `OutputWorkArea` per connected
output, then an `OutputsEnd`; the connection is then **subscribed** to
hotplug, so a mode, scale, position or work-area change produces another
message and an unplug an `OutputGone`. Asking twice re-sends the snapshot;
the subscription is idempotent.

The snapshot is **complete at `OutputsEnd`** — that terminator is what
makes it safe for the work area to arrive in a second message rather than
as a field on `OutputInfo`; see
[`OutputWorkArea`](#outputworkarea--0x8408).

The answers live in the `0x84xx` shell block. That wart is deliberate: see
the note under the op-code tables, and the access rule under
[Shell](#shell-caps-shell).

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

**A window's own group always clips, and this cannot turn it off.** The
node a `CreateWindow` binds — the window's content group — is created
clipping, so a client's nodes are bounded by its content rectangle
whatever it sends: overflow is cut off at the window's edge rather than
painted on the desktop beside the frame, a click outside the window
reaches nothing, and damage never leaves it. `clip: false` on that node
is refused with `BadParent`, like any other attempt to mutate what the
window owns; `clip: true` on it is the no-op it already was, so a client
that re-asserts the flag on its own window node is not disconnected for a
redundant request. Naming your own window node in a `SetClip` at all is a
raw-wire thing to do — `examples/overflow_client` is the one that does it
— and the refusal is about *clearing* the flag, not about the message.

A toolkit's own clip is a **different node** and is unaffected either
way: `nitro-ui` clips its root widget's group, which hangs *under* the
window's content group rather than being it, at the same rectangle. The
two are redundant, not one — see `docs/ui.md` §A window's content is
clipped to the window.

Every other group is the client's to clip or not — a scroll view needs
it, a shadow must not have it. See `docs/wm.md` §What a client can and
cannot draw.

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

### `SetIcon` — 0x0208

| field | type | meaning |
|---|---|---|
| `node` | `NodeId` | the `Icon` node |
| `size` | `f32` | the icon's square side in **logical** pixels |
| `role` | `u8` | palette `Role` index, or `0xff` = the **application** icon set |
| `name` | `str` | icon name (`"gear"`); **empty clears the node** |

Fixed head 9 bytes (`4+4+1`), then the name — the same shape as
`SetWindowTitle`. Requires the `ICONS` capability; applies at the next
`Commit`, like every other mutation. On a node that is not an `Icon` node
it is a `WrongKind` error.

The client sends a **name**, never pixels. `docs/icons.md` makes the case
in full; the three-line version is that it is the only form that survives
a remote link (a buffer is a descriptor and cannot cross TCP), a scheme
flip (the server resolves the role at paint time, so `theme.scheme`
recolours every icon with no client message) and a scale change (the
server rasterises at `round(size × output scale)`, so a 2× screen gets a
real 2× icon rather than a doubled tile).

**`size` is one number because icons are square by contract.** Every icon
in the set is drawn on a 16-unit grid, which is what lets a toolkit
measure one without a round trip. 16, 24, 32 and 48 are the recommended
sizes; the server clamps into `4..=512` device pixels and substitutes 16
for a non-finite or non-positive one rather than erroring, because an
icon must never be able to kill a client.

`role` is a palette index (`docs/theme.md`), resolved by the server at
*paint* time. An index past the last role this server knows falls back to
`Text` rather than vanishing — a client one release ahead gets a visible
icon in the wrong colour, not a silent gap.

**`0xff` (`AS_COLOURED`) selects the other icon set.** Since M4-H it does
not mean "this name, untinted": it means the name is an **application**
icon, looked up in the machine's XDG icon theme (`theme.icons` in
`server.conf`) and painted in the file's own colours. A palette role
means the server's own symbolic set, and *nothing falls back between the
two*.

That is a decision rather than an implementation detail, so it is written
down here as well as in `docs/icons.md`: one namespace searched
"symbolic first" would make `SetIcon { name: "list" }` mean the desktop's
own list glyph on a bare box and somebody else's artwork on a box with a
theme that happens to ship a `list` — invisibly, and differently per
machine. With the role as the selector the *caller* says which set it
means, and there is no collision to reason about.

The corollary is the combination that is **refused**: a palette role with
a name only the icon theme has earns `BadIcon`, rather than finding the
file and tinting its alpha. A theme icon is a picture, not a coverage
mask — tinting one throws the artwork away and keeps the silhouette,
which looks like a rendering bug on every icon that is not already
monochrome. A client that wants a tinted icon names one from the
symbolic set, which is what that set is for.

The scale argument above holds for both, by different means: a symbolic
icon is *rasterised* at the device size, and an application icon is read
from the theme directory that matches the device size (a 48 px file for a
24-logical icon on a 2× output) and resampled once into a cached tile.
Neither is a doubled 16 px bitmap.

An **unknown name** earns `Error { BadIcon }` and the node draws nothing;
the connection survives. It covers both sets: a symbolic name the server
does not have and an application name the machine's theme does not have
are the same answer, because from the client's side they are the same
fact — the icon it asked for cannot be drawn, and it should send its
fallback. See [Errors](#errors) for why this and the remote buffer op are
the only two non-fatal errors in the protocol.

### `CreateBuffer` — 0x0301 — **carries 1 fd**

| field | type | meaning |
|---|---|---|
| `id` | `BufferId` | client-allocated |
| `width` | `u32` | pixels |
| `height` | `u32` | pixels |
| `stride` | `u32` | bytes per row; at least `width * 4` |
| `format` | `u32` | DRM fourcc |
| `size` | `u32` | mapping size in bytes; at least `stride * height`, and no larger than the file |
| *(fd)* | `SCM_RIGHTS` | **sealed** memfd (see below) |

The descriptor **must be a sealed memfd**: created with
`memfd_create(name, MFD_ALLOW_SEALING)` and then sealed with
`fcntl(F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL)`. The server
verifies this with `fcntl(F_GET_SEALS)` **before** it maps anything, and a
descriptor missing any of the three earns `BadBuffer` — there is no
fallback to copying. An inconsistent geometry or size is `BadBuffer` too,
as is a file shorter than the declared `size`.

The reason is not ceremony. The server **maps** the descriptor read-only
rather than copying the pixels out of it, so if a client could shrink the
file under that live mapping, the server's next read of the vanished pages
would be a `SIGBUS` — any client could kill the compositor. `F_SEAL_SHRINK`
is what makes that impossible; `F_SEAL_SEAL` is what stops the seal set
being changed afterwards, so what the server checked is what holds for the
buffer's life; `F_SEAL_GROW` fixes the size so the declared `size` can be
checked once and relied on. Sealing does **not** restrict writing: the
client goes on writing its frames into the same pages, which is the whole
point. See `crates/nitro-shm/README.md` for the full argument and its
residuals.

The client keeps writing into the buffer and announces changes with
`BufferDamage`, which is now purely "repaint the nodes that sample these
rows": the server re-reads nothing, because it never had a copy.

**The tearing contract.** Because the mapping is live, the bytes the server
blits are whatever is in the buffer at the moment it paints. A client that
wants a coherent frame writes the next one only after the server has
finished the previous one — i.e. after the `Frame` callback (or
`Presented`) for it. The toolkit and the benchmark harness already work
this way: they render on `Frame`, which the server sends after the paint.
A client that ignores this tears its own window and nothing else.

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

### `SetSelection` — 0x0305

| field | type | meaning |
|---|---|---|
| `mimes` | `vec<str>` | MIME types offered, most preferred first; empty clears |

Head **0 bytes** — the whole payload is the vector. Requires `DATA`.

Declares what this client *can* serve, never any bytes. A paste elsewhere
earns a [`SelectionRequest`](#selectionrequest--0x8503), answered with
[`SendSelection`](#sendselection--0x0307--carries-1-fd). An **empty** `mimes` clears the
selection, and the server then pushes `SelectionOffer { mimes: [] }` to
everyone.

Authorized by **keyboard focus**, not by an input serial; a client without
it is `Error { Protocol }`. See [Data transfer](#data-transfer-caps-data).

### `RequestSelection` — 0x0306

| field | type | meaning |
|---|---|---|
| `request` | `u32` | the **requester's** own id, echoed in `SelectionData` |
| `source` | `u8` (`DataSource`) | `Clipboard` or the drag offer currently over this client |
| `mime` | `str` | the type wanted, from the offer's list |

Fixed head **5 bytes**, then the string. Requires `DATA`.

A `Drag` request is valid **only while this client is the current drop
target** — it has had a `DragEnter` and neither a `DragLeave` nor a
completed drag. Outside that window it is `Error { Protocol }`, on the
same footing as the focus checks that replace input serials.

**Every accepted request is answered exactly once**, and a failure is an
already-at-EOF descriptor rather than a silence — so the requester needs
one code path and no timeout. Reusing a `request` that is still
outstanding is `Error { Protocol }`. See
[Data transfer](#data-transfer-caps-data) for the two id spaces, the
failure path and the per-connection bound.

### `SendSelection` — 0x0307 — **carries 1 fd**

| field | type | meaning |
|---|---|---|
| `request` | `u32` | the **server's** id, from the `SelectionRequest` being answered |

Fixed head **4 bytes**, plus one descriptor. Requires `DATA`.

**The owner supplies the descriptor** — a readable one: a sealed memfd, or
the read end of a pipe it writes at its leisure. The server relays it to
the requester as [`SelectionData`](#selectiondata--0x8502--carries-1-fd) and never reads
it. A descriptor already at EOF means "I cannot serve that MIME type".

An unknown or stale `request` is **not** an error; the server closes the
descriptor and drops the message. See
[Data transfer](#data-transfer-caps-data) for why, and for the
fd-ownership rules.

### `StartDrag` — 0x0308

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window the drag starts from |
| `icon` | `NodeId` | node to drag under the pointer; `NONE` for no icon |
| `actions` | `u32` | `drag_actions` bitmask: what this source allows |
| `mimes` | `vec<str>` | MIME types offered, most preferred first |

Fixed head **12 bytes**, then the vector. Requires `DATA`.

`mimes` is last because it is the variable field, which moves it against
the M5-A sketch — the same house rule that reordered `SetAppId`. The
`actions` field is an addition to the sketch: without it copy-versus-move
cannot be expressed and `AcceptDrop.action` has nothing to be chosen from.

Authorized by pointer focus **and a button actually being down**.

### `AcceptDrop` — 0x0309

| field | type | meaning |
|---|---|---|
| `action` | `u8` (`DragAction`) | what this target would do; `None` rejects |
| `mime` | `str` | the type it would read; empty rejects |

Fixed head **1 byte**, then the string — `mime` moved last for the same
reason `StartDrag.mimes` did. Requires `DATA`.

Sent by the **destination** while a drag is over it, and again whenever
the answer changes (the pointer moved to another widget, a modifier went
down). `action` must be one of the actions `DragEnter` advertised.

### `FinishDrag` — 0x030a

No fields; head **0 bytes**. Requires `DATA`.

Sent by the drag **source** after its
[`DragFinished`](#dragfinished--0x8508): it releases the offer and the
server drops the drag icon. A source that disconnects instead is
equivalent.

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

### `Theme` — 0x8004

The desktop's colour palette. Requires `THEME`.

| field | type | meaning |
|---|---|---|
| `serial` | `u32` | increments on every change |
| `colors` | `vec<Color>` | one colour per **role**, in role order |

Sent **immediately after `Welcome`**, on both the wire and the shell
socket, and again whenever the server's palette changes. A client does
not ask for it and cannot refuse it: the server owns the scheme
(`theme.scheme` in `server.conf`) and the per-role overrides, and the
client is told. See `docs/theme.md` for the role table and the
configuration keys.

Sending it before anything else is deliberate. A client's first paint
happens before its first `Configure` comes back, so a palette that
arrived one round trip later would mean every app on the desktop shows
one frame of its built-in defaults and then flashes.

**Why the count is on the wire.** `colors` is a `vec<Color>`, so the
length is a `u32` in the payload rather than a constant both sides must
agree on. The roles are a dense, append-only enumeration
(`nitro_core::palette::Role`), and the count is `Role::COUNT` *at the
sender's build time*. That makes appending a role a compatible change in
both directions:

* a **newer server, older client** sends more colours than the client
  knows roles; the extra tail is dropped;
* an **older server, newer client** sends fewer; the roles it did not
  carry keep their value from the client's own built-in default.

Neither is an error, and neither closes the connection. This is the same
"grow in place within the block" rule `WindowInfo` follows — appending a
role does not need a new op, a new bit or a version bump, and a desktop
running a mixed set of binaries during an upgrade renders rather than
refusing to start.

Roles are therefore **only ever appended**: a role's position is its wire
index, so inserting one in the middle would silently renumber every
colour after it. `keys_and_indices_round_trip` in
`crates/nitro-core/src/palette.rs` pins the order.

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

### `PopupDone` — 0x8106

| field | type | meaning |
|---|---|---|
| `popup` | `NodeId` | the popup, by the owning client's own node id |

Fixed head **4 bytes**. Requires `POPUP`.

The popup was dismissed — an outside click, Escape, or the parent going
away. The whole chain below the dismissed popup goes with it, each
reported separately. The server has **already unmapped** it by the time
this arrives, which is the unmap-then-notify ordering Chromium expects;
the client should destroy the node.

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

### `Keymap` — 0x8208 — **carries 1 fd**

| field | type | meaning |
|---|---|---|
| `format` | `u8` (`KeymapFormat`) | `XkbV1` (1) |
| `size` | `u32` | bytes to map, **including** the trailing NUL |
| `rate_hz` | `u32` | advisory repeat rate, repeats per second; 0 = none |
| `delay_ms` | `u32` | advisory delay before the first repeat; 0 = none |

Fixed head **13 bytes**, plus one descriptor. Requires `KEYMAP`.

For a client that owns its own `xkb_state` and must not have the server
resolve for it. The existing `Key` fields do not change: a toolkit that
uses `keysym`/`utf8` never negotiates `KEYMAP` and never sees this. Sent
after the handshake and again on every layout change.

**The descriptor.** A **sealed memfd** — `memfd_create(MFD_ALLOW_SEALING)`
plus `F_ADD_SEALS` with `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`, as
`CreateBuffer` demands and for the same `SIGBUS` reason — to be mapped
`PROT_READ | MAP_PRIVATE`. It holds the `XKB_KEYMAP_FORMAT_TEXT_V1`
string **NUL-terminated**, with `size` counting the NUL. That is Wayland's
convention, so an adapter is a pass-through. The **server creates it**;
the **client owns it** once it has decoded the message, and closes it. One
frame's descriptors per `sendmsg`, header included, as always.

**`rate_hz` and `delay_ms` are advisory.** They describe the *user's
preference*, not a server behaviour: nitro synthesises no repeats, so a
client that wants them repeats itself — exactly `wl_keyboard.repeat_info`,
which is advisory for the same reason, and exactly what a Chromium backend
wants since it repeats on its own. They ride here rather than on a message
of their own because the server recompiles the keymap on config reload
anyway, so the two always change together. **Until `keyboard.repeat`
exists in `server.conf` the server sends `0, 0`** — do not read the fields
as a promise; `docs/settings.md` § `keyboard.repeat` is the other half of
this note.

### `Modifiers` — 0x8209

| field | type | meaning |
|---|---|---|
| `depressed` | `u32` | modifiers currently held down |
| `latched` | `u32` | modifiers latched for the next key |
| `locked` | `u32` | modifiers locked (caps lock, num lock) |
| `group` | `u32` | effective layout (group) index |

Fixed head **16 bytes**. Requires `KEYMAP`.

The four masks `xkb_state_serialize_mods`/`_layout` produce, to be fed
straight into the client's own `xkb_state_update_mask`. Sent whenever any
of them changes, and after every `Keymap`. **Meaningful only against the
keymap that message carried** — the bit positions depend on it, which is
why `BindKey` uses `mod_mask` names instead.

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

### `IconRefused` — 0x8303

| field | type | meaning |
|---|---|---|
| `serial` | `u32` | the transaction being applied, or 0 outside one |
| `node` | `NodeId` | the `Icon` node whose name was refused |
| `name` | `str` | the name that was not found |

Fixed head **8 bytes**, then the string. Requires the **existing** `ICONS`
bit, and a `ClientCaps` listing it.

The same fact as `Error { BadIcon }` — which is retained verbatim for a
client that does not know this message — but carrying the **node id**, so
a toolkit can route the failure to the widget that asked. `Error.msg` is
documented as being for logs and never parsed; before this message existed
the toolkit had no choice but to parse the icon name out of it, and this
is what makes that documentation true again.

A **new op code behind an existing capability bit**: the M2 text ops set
that precedent, and `ClientCaps` makes it safe here — a client that lists
`ICONS` is by construction new enough to know this message, and one that
lists nothing keeps getting `Error { BadIcon }`, prose and all. It is the
single named exception to "`ClientCaps` governs bits 8 and above".

Non-fatal, exactly as `Error { BadIcon }` is: the node draws nothing and
the connection stays up.

**Not yet sent by the server.** M5-A ships the message, its layout and
this section; switching `report_bad_icons` over to it, and deleting the
toolkit's parser of `Error.msg`, is task **#3786**. Until that lands every
client still receives `Error { BadIcon }`, whatever its `ClientCaps` says.

*(0x8304 is deliberately unassigned.)*

### `BufferReleased` — 0x8305

| field | type | meaning |
|---|---|---|
| `id` | `BufferId` | the buffer whose pixels are free again |

Fixed head **4 bytes**. Requires `RELEASE`.

The server maps a client's pages and reads them at paint time, so a client
that redraws into a buffer still being composited tears. `Presented` is a
usable but **conservative** substitute — a buffer is free once blitted
into the shadow, well before scanout — and this is the exact answer, which
is what lets a two-buffer client avoid a frame of latency.

It does **not** release the id: that is still `DestroyBuffer`. It says
only that the pixels may be overwritten.

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

### The output messages are also reachable with `OUTPUTS`

One exception to "every op in this section requires `SHELL`", added in
M5-A. `OutputInfo` (0x8405), `OutputsEnd` (0x8406), `OutputGone` (0x8407)
and `OutputWorkArea` (0x8408) are sent to a client holding **either**
`SHELL` **or** `OUTPUTS`, because an ordinary client needs the output
layout for the same reason a shell does — to place a window, to pick a
maximize geometry, to know a scale before it allocates.

The *request* differs: a shell asks with `Outputs` (0x040b, `SHELL`), an
ordinary client with `ListOutputs` (0x001b, `OUTPUTS`). The **answers are
the same four messages**. That leaves an unprivileged `0x00xx` op answered
in the privileged `0x84xx` block, which is a wart and is kept knowingly:
duplicating four messages into a new block to tidy the numbering would
mean two encoders, two decoders and two things to keep in step forever.

Nothing else in this section is reachable without `SHELL`. In particular
the output messages carry nothing about *other clients' windows*, which is
what the privilege actually protects.

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
* The other nine are answered **on receipt**. `WindowList` and `Outputs`
  are questions, like `MeasureText`; `BindKey`/`UnbindKey` are
  registrations; the three `WindowRef` ops act on *another* client's
  window, which the sender's own commit has nothing to do with; and
  `Lock`/`Unlock` change the whole session, which a lock screen must be
  able to do before its own window exists.

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

### `Lock` — 0x040c

Empty payload. Applied on receipt: the session is **locked** and the
sender is its **owner**. While locked, only the owner's windows are drawn
and hit-tested, and only they receive input (keys, pointer, scroll,
touch, focus). Shell bindings do not fire, and the only compositor chord
is the VT switch. Other clients keep their windows and are still sent
`Configure`, `WindowState` and `Closed`; they are simply not shown and
not given input.

- Sent while already the owner: nothing happens.
- Sent while the lock has **no owner** (its owner disconnected, or the
  server was started with `NITRO_LOCKED=1`): the sender takes it over.
- Sent while **another connection** owns the lock: `Error { Protocol }`.

An owner that disconnects leaves the session **locked with no owner**:
nothing but the background is drawn until the next `Lock`. There is no
reply; a refusal is the error. The model and its reasons are in
`docs/shell.md`, "The session lock".

### `Unlock` — 0x040d

Empty payload. Applied on receipt, from the lock owner only: the session
is unlocked, every window is drawn again, and the window that had focus
when the lock was taken gets it back. From anyone else, or when the
session is not locked, it is `Error { Protocol }`.

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

### `OutputWorkArea` — 0x8408

| field | type | meaning |
|---|---|---|
| `id` | `u32` | the output, the same id `OutputInfo` carries |
| `area` | `IRect` | work area in **global device pixels** |

Fixed head **20 bytes**. Sent to a client holding `SHELL` **or**
`OUTPUTS`, like the other four output messages.

The output's rectangle with every exclusive zone subtracted: what
`Maximized` fills and what new windows are placed into. Sent for each
output between its `OutputInfo` and the `OutputsEnd` that terminates the
snapshot, and again whenever the work area alone changes.

`area` is in **device pixels in the global space**, exactly like
`OutputInfo.x/y/w/h` — consistency inside one subject beats matching a
consumer's DIP convention, which a backend already has to divide for.

**Why a message rather than a field on `OutputInfo`.** The argument is
**cadence**, not compatibility. The work area moves when an *exclusive
zone* changes (`SetExclusiveZone`, a bar hiding itself), which touches
none of the output's mode, position, scale or name. Folding it into
`OutputInfo` would force a whole output list to be re-sent on every zone
change, or leave the field stale — and stale here is a client computing
maximize geometry wrong. A separate message is both the snapshot carrier
and the update channel.

The objection that sank a parallel `WindowLayer` in #3697 — "an output is
briefly listed with an unknown work area" — does not apply, because the
output list has a **terminator**: the snapshot is complete at
`OutputsEnd`, and a client reads the whole snapshot before acting on it. A
window list has no per-window terminator, which is why that case went the
other way. The Versioning policy has the rest of the argument, including
why #3697's exemption cannot simply be reused here.

## Data transfer (caps `DATA`)

Clipboard and drag-and-drop are **one capability bit** because they are
one mechanism: an offer (a list of MIME types), a request naming one type,
and a file descriptor the bytes travel over. `RequestSelection.source`
names which of the two a request is about; nothing else differs.

### The clipboard sequence

```text
owner      →  SetSelection { mimes }                            (0x0305)
server     →  SelectionOffer { mimes }          to everyone     (0x8501)

requester  →  RequestSelection { request, source, mime }        (0x0306)
server     →  SelectionRequest { request', source, mime }  to owner  (0x8503)
owner      →  SendSelection { request' } + 1 fd                 (0x0307)
server     →  SelectionData { request } + the same fd  to requester (0x8502)
```

**The owner supplies the descriptor**, which is the inversion Wayland does
not make, and it is deliberate. A Wayland client serving a large clipboard
payload needs a *writer state machine* in its event loop, because a pipe
holds 64 KiB and the compositor hands it the write end. Here an owner that
already has the bytes hands over a **sealed memfd** and is done — no
partial writes, no re-entry — and the server relays a descriptor instead
of copying bytes. An owner that prefers a pipe still may: it creates one,
sends the read end, and writes at its leisure.

### The drag-and-drop sequence

```text
source       →  StartDrag { window, icon, actions, mimes }      (0x0308)

server       →  DragEnter { window, pos, actions, mimes }  to target  (0x8504)
server       →  DragMotion { window, pos, time_ns }        to target  (0x8505)
target       →  AcceptDrop { action, mime }                    (0x0309)
target       →  RequestSelection { request, Drag, mime }       (0x0306)  — reads the data
server       →  DragLeave { window }                      to target  (0x8506)
  … or …
server       →  DragDrop { window }                       to target  (0x8507)
server       →  DragFinished { accepted, action }          to source  (0x8508)
source       →  FinishDrag                                     (0x030a)
```

The destination reads the dragged bytes with an ordinary
`RequestSelection` carrying `source: Drag`, answered by the same
`SelectionRequest`/`SendSelection`/`SelectionData` machinery. That request
is valid only between a `DragEnter` and the matching `DragLeave` or the
end of the drop; outside that window it is `Error { Protocol }`.

**The source learns the action only at `DragFinished`.** There is no
mid-drag action message — Wayland's `wl_data_source.action` — and that is
a decision, not an omission: it maps to
`WmDragHandler::LocationDelegate::OnDragOperationChanged`, which has
exactly one caller in the whole Chromium tree
(`ui/ozone/platform/x11/x11_window.cc:1738`) and **zero under
`ui/ozone/platform/wayland/`**. The Wayland backend never calls it, so a
nitro backend modelled on it needs nothing here. If cursor feedback ever
wants it, it is a new op behind the existing `DATA` bit — cheap, and not
bytes spent on speculation now.

### The two `request` id spaces

They are **different spaces and never meet**; the server maps between
them.

* `RequestSelection.request` / `SelectionData.request` — the
  **requester's** id. Client-allocated, namespaced per connection exactly
  like a `NodeId`. It exists so a client with several reads in flight can
  match answers. The server echoes it and never interprets it. The
  `MeasureText`/`TextMeasured` pair is the existing precedent for a
  client-allocated request id.
* `SelectionRequest.request` / `SendSelection.request` — the **server's**
  id, handed to the owner and echoed back. Server-allocated and
  server-global. An owner must not assume any relationship to the other
  space.

A requester reusing an id that is still outstanding is
`Error { Protocol }`: it has made its own answers ambiguous, and unlike
the owner-side race below that is entirely within its control.

### The failure path, and why there is no timeout

**Every accepted `RequestSelection` is answered by exactly one
`SelectionData`.** When the server cannot get a descriptor from the owner
it creates a pipe, closes the write end, and sends the read end —
byte-identical to the owner's own way of saying "I cannot serve that MIME
type". So the requester needs exactly one code path and no timeout logic.

It fires when:

* the owner disconnects;
* the owner answers with an unknown or stale id;
* the selection owner changes before the `SendSelection` arrives;
* the per-connection cap below is hit.

The remaining rules:

* **An unknown or stale `request` in `SendSelection` is not a protocol
  error.** The owner is racing a selection change it has not yet been told
  about, which is legitimate and unavoidable; killing a correct client for
  losing that race would be wrong. The server closes the descriptor and
  drops the message. The original requester has already been answered with
  an EOF descriptor by the rule above.
* **If the owner changes between `RequestSelection` and `SendSelection`**,
  the request is answered from the owner it was *sent to*. A late
  `SendSelection` from the old owner is the stale case above; the new
  owner is never asked about an old request.
* **`MAX_PENDING_SELECTIONS = 16` outstanding requests per connection**,
  counted as requests a client has *made*. The 17th is answered
  immediately with an EOF descriptor rather than refused — same single
  code path, no new error, and the client that is misbehaving is the one
  that feels it. See [Receive-side limits](#receive-side-limits).
* A client that never reads its end costs one descriptor and one pipe
  until it disconnects, which the same cap bounds.

### Two things a client must not do

1. **Do not block on the read.** A hostile or merely slow owner can hand
   over a descriptor that never reaches EOF — true of Wayland too. Read
   non-blocking, from the event loop.
2. **Do not assume the owner writes.** A descriptor already at EOF with no
   bytes is a documented, expected answer, not a bug.

### Descriptor ownership, per message

| message | who creates the fd | who closes it | what EOF means |
|---|---|---|---|
| `Keymap` 0x8208 | the server (sealed memfd) | the client, after mapping | n/a — `size` is exact |
| `SendSelection` 0x0307 | the selection **owner** | the server, once relayed (or at once if stale) | "I cannot serve that MIME type" |
| `SelectionData` 0x8502 | relayed, not created | the **requester** | end of the data, or the refusal above |
| `SelectionRequest` 0x8503 | — **no fd** | — | — |

The two `sendmsg` rules under [File descriptors](#file-descriptors) are
unchanged and apply in both directions: one frame's descriptors per call,
and that call includes the frame's header.

### Clearing the selection

`SetSelection { mimes: [] }` clears it, and the server then pushes
`SelectionOffer { mimes: [] }` to **everyone** rather than saying nothing.
A client holding a stale offer must be told it is stale, or a paste button
stays enabled forever after the owning app exits; and an empty list is the
natural "there is no selection", so both ends of the protocol encode the
same fact the same way.

### `SelectionOffer` — 0x8501

| field | type | meaning |
|---|---|---|
| `mimes` | `vec<str>` | types offered, most preferred first; empty = no selection |

Head **0 bytes** — the payload is the vector. Pushed to every client
holding `DATA` whenever the selection changes.

### `SelectionData` — 0x8502 — **carries 1 fd**

| field | type | meaning |
|---|---|---|
| `request` | `u32` | the requester's own id, echoed back |

Fixed head **4 bytes**, plus one descriptor — the one the owner supplied,
relayed rather than copied. Read it non-blocking, to EOF; see the two
rules above.

### `SelectionRequest` — 0x8503

| field | type | meaning |
|---|---|---|
| `request` | `u32` | the **server's** id, echoed in `SendSelection` |
| `source` | `u8` (`DataSource`) | clipboard, or a drag offer |
| `mime` | `str` | the type wanted, from this client's own offer |

Fixed head **5 bytes**, then the string. **Carries no descriptor**: the
owner supplies one with `SendSelection`.

### `DragEnter` — 0x8504

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window the drag is over |
| `pos` | `Point` | pointer position in that window's coordinate space |
| `actions` | `u32` | `drag_actions` bitmask the source offers |
| `mimes` | `vec<str>` | types offered, most preferred first |

Fixed head **16 bytes**, then the vector.

### `DragMotion` — 0x8505

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | |
| `pos` | `Point` | |
| `time_ns` | `u64` | `CLOCK_MONOTONIC` nanoseconds |

Fixed head **20 bytes**.

### `DragLeave` — 0x8506

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window the drag left |

Fixed head **4 bytes**. The offer is gone: a `RequestSelection` with
`source: Drag` after this is `Error { Protocol }`.

### `DragDrop` — 0x8507

| field | type | meaning |
|---|---|---|
| `window` | `NodeId` | the window dropped on |

Fixed head **4 bytes**. The destination may still read the data — the
drag stays open until the transfer finishes — and should have said what it
would do with `AcceptDrop` before now.

### `DragFinished` — 0x8508

| field | type | meaning |
|---|---|---|
| `accepted` | `bool` | whether the offer was taken |
| `action` | `u8` (`DragAction`) | the action the destination chose |

Fixed head **2 bytes**. Sent to the drag **source**, and the only point at
which it learns the action. A rejected or cancelled drag is
`accepted: false` with `action: None`. The source answers `FinishDrag`.

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
13. **M5-A: `SendSelection` carries the fd, `SelectionRequest` does not.**
    The sketch gave a descriptor to `SelectionRequest` *and* to
    `SendSelection` *and* to `SelectionData`, which describes two
    different protocols at once (a server-made pipe and an owner-made fd)
    and leaves `SendSelection` with no role in the first. Settled: **the
    owner supplies the fd**, `SelectionRequest` carries none, and the
    server relays a descriptor instead of copying bytes. The argument is
    in [Data transfer](#data-transfer-caps-data): an owner that already
    has the bytes hands over a sealed memfd and is done, with no writer
    state machine in its event loop.
14. **M5-A: `StartDrag` and `DragEnter` gained an `actions` field.** The
    sketch has none. Without it copy-versus-move — the Ctrl/Shift
    behaviour every file manager has — cannot be expressed at all, and
    `AcceptDrop.action` would have nothing to be chosen from.
15. **M5-A: `RequestSelection` and `SelectionRequest` gained a
    `DataSource` byte**, moving both heads from 4 to 5. The sketch gives a
    drop target a `mimes` list in `DragEnter` and then no op with which to
    ask for the bytes; one byte on the existing request pair is what keeps
    "the offer/mime/fd machinery is shared" true, and so keeps `DATA` one
    capability bit rather than two.
16. **M5-A reorders two messages so the variable field is last**, the same
    house rule that moved `SetAppId`'s string: `StartDrag` is
    `{window, icon, actions, mimes}` and `AcceptDrop` is
    `{action, mime}`.
17. **M5-A adds `ClientCaps` (0x0003), which is not in the sketch's op
    list at all.** It is the one addition outside it, and it is what makes
    every other one safe: without it a pushed server→client message kills
    a client that does not know the op. See
    [Capability opt-in](#capability-opt-in-clientcaps).

## Receive-side limits

The decode path is a hostile boundary, so the receiver bounds every
resource a peer can make it hold:

| limit | value | what it stops |
|---|---|---|
| `MAX_PAYLOAD` | 16 MiB | one oversize frame |
| `MAX_FDS` | 8 | descriptors declared by one frame |
| `MAX_PENDING_FDS` | 64 | **unclaimed** descriptors held by the framer |
| `MAX_PENDING_SELECTIONS` | 16 | outstanding selection requests **one connection has made** |
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

`MAX_PENDING_SELECTIONS` is the clipboard's peer of `MAX_PENDING_FDS`, and
bounds a different resource: server *state*, not descriptors. A
`RequestSelection` parks a mapping from the requester's id to the owner's
until the owner answers, and an owner is under no obligation to be quick
— so a client that fires requests and never reads the answers would grow
that state without bound. The 17th outstanding request is **answered
immediately with an EOF descriptor** rather than refused: the requester
already has exactly one code path for "I cannot serve that", so the bound
costs no new error and no new client code, and the client that feels it is
the one misbehaving. See
[the failure path](#the-failure-path-and-why-there-is-no-timeout).

Since M5-A the `MAX_PENDING_FDS` rule runs in **both** directions. A
client can now receive descriptors (`Keymap`, `SelectionData`), so a buggy
or hostile server could park them in a client's framer exactly as a client
could in the server's, and the client's `poll` therefore has the same two
halves: yield at the cap **if there is a frame to drain**, and be fatal if
there is not.

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
* `Lock` (`0x040c`) and `Unlock` (`0x040d`) join that block, for the
  session lock. They are new ops under the existing `SHELL` bit, so no
  new capability bit is needed: an older server refuses them as unknown
  ops, which is the right answer for a server that cannot lock. `VERSION`
  stays **1**.
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
* M3 adds a field **in place** to an existing message: `WindowInfo`
  (0x8402) grew `layer`, moving its fixed head from 10 to 11 bytes
  (task #3697). This is the one deviation on this list that really does
  move bytes in a message that already existed, so it needs its own
  argument rather than the "new op code" one above.

  It is acceptable **here** because `WindowInfo` is a post-M2-freeze
  M3-B op living in the `SHELL` block, and that block is reachable only
  through `shell.sock`: the capability is granted by which socket a
  client connected to, so no unprivileged client can send `WindowList`,
  can receive a `WindowInfo`, or can observe the layout at all. The
  readers are shell clients — the bar, the launcher, `shell_probe` —
  which are built from this tree and ship with the server. There is no
  client that can see the old layout and the new server at once.

  The reason it could not be a new op code: a task list must filter
  *every* window it is told about, so the layer has to be on the message
  that reports a window, not on a second message a shell might not ask
  for. A parallel `WindowLayer` op would mean a window is briefly listed
  with an unknown layer, which is the bug being fixed.

  `VERSION` therefore stays **1**. Had `WindowInfo` been an unprivileged
  op, or had any non-tree client existed, this would have been a bump.
  The same reasoning does **not** extend to the `0x_0xx..0x_3xx` blocks:
  a field added in place to any message an ordinary client can receive
  is a `VERSION` bump.
* The M4-G icon op — `SetIcon` 0x0208 — is the sanctioned path again:
  **one new op code** in the gap after the text ops, guarded by a **new
  `ICONS` capability bit** (bit 7), plus one new value in each of two
  enumerations (`NodeKind::Icon` = 6 and `ErrorCode::BadIcon` = 8). No
  field of any pre-existing message changed and no byte moved, so
  `VERSION` stays **1**.

  The two enum values need their own note, because appending to an
  enumeration is not automatically free: a value outside the list is a
  *decode error* in this protocol, not a silently-ignored unknown. So an
  old client that somehow received `NodeKind::Icon` or
  `ErrorCode::BadIcon` would disconnect rather than misread. It cannot:
  `NodeKind` only ever travels **client → server** (in `CreateNode`), and
  a client that does not know the kind cannot send it; and `BadIcon` is
  only ever sent in answer to a `SetIcon`, which a client without the
  `ICONS` bit must not send. Both are reachable only by a peer that
  already knows about them, which is the property that makes appending
  to a strict enumeration compatible here and would not make it so for,
  say, a new `Fill` tag the server could push unprompted.
* The M4-H application icons change **no bytes at all**: `SetIcon`'s
  reserved `role = 0xff` went from "accepted, draws nothing" to "this
  name is an application icon". That is the door M4-G left open being
  used, and it is the cheapest kind of change on this list — no op code,
  no capability bit, no field. `VERSION` stays **1**.

  It is worth one paragraph anyway, because a *meaning* changing is
  exactly what the versioning policy is nervous about. It is safe here
  because no client could have been relying on the old behaviour: the old
  behaviour was "nothing is drawn", which is indistinguishable from a
  node the client never created, and `docs/icons.md` and this document
  both said in as many words that the value was reserved for this. The
  observable difference for an old client is that a name it never had a
  reason to send now draws an icon.
* **`SetCursor` was deferred to M5, and landed in M5-A** as `0x0018`
  behind the new `CURSOR` bit. The paragraph below is the M4-era
  reasoning, kept because the decision it records is still in force for
  the half that did *not* ship; what changed is recorded after it.

  #3724 gave the *server* five cursor shapes beyond the arrow — the four
  resize diagonals and the move cross — chosen from the frame region under
  the pointer (`docs/wm.md`). A **client** still could not ask for one: a
  text widget could not show an I-beam and a link could not show a hand.

  It was deferred rather than added because the shape is only half of it.
  A useful `SetCursor` is a named shape *and* a client-supplied bitmap
  with a hotspot, which is a buffer op, a lifetime question (the cursor
  outlives the frame that set it) and a re-entrancy one (the pointer is
  over a node whose client has gone). Shipping the enum half then would
  freeze a message that has to grow the other half later, which is
  precisely what this policy exists to prevent.

  **What M5-A changed: the named-shape half is now judged sufficient on
  its own**, so the message does not have to grow and freezing it is
  safe. `wp_cursor_shape_v1` makes the argument for us — the compositor
  knows better how to draw a cursor at the output's scale, and a named
  shape is what lets it. Every `ui::mojom::CursorType` a browser needs
  maps onto the list except `kCustom` (CSS `cursor: url(…)`), which is
  **deliberately unsupported** and falls back to `Pointer`. That is the
  bitmap half, with all three of its problems, and declining it is what
  makes the enum half a finished message rather than a down payment. If a
  bitmap cursor is ever wanted it is a *new op* behind a new bit, not a
  field on this one.

  Two details of the prediction were wrong and are corrected here rather
  than quietly: the op landed in `0x001x`, not the `0x_02x` gap the
  paragraph guessed, and the shape is a `u16` rather than a byte, because
  the borrowed `wp_cursor_shape_device_v1` numbering is worth more than
  the byte. `VERSION` stays **1**, as promised.

* **M5-A** — the wire surface for a Chromium Ozone backend: `ClientCaps`
  `0x0003`, `CreatePopup`…`ListOutputs` `0x0016..0x001b`,
  `SetSelection`…`FinishDrag` `0x0305..0x030a`, `PopupDone` `0x8106`,
  `Keymap`/`Modifiers` `0x8208`–`0x8209`, `IconRefused` `0x8303`,
  `BufferReleased` `0x8305`, `OutputWorkArea` `0x8408`, and the new
  `0x85xx` data-transfer block. **`VERSION` stays 1.**

  The ordinary part of the argument first: every one of these is a **new
  op code behind a new capability bit** (bits 8–14: `POPUP`, `CURSOR`,
  `DRAG`, `OUTPUTS`, `KEYMAP`, `RELEASE`, `DATA`), which is the sanctioned
  path this list has taken four times already. **No field of any
  pre-existing message changed and no byte moved** — the
  `payload_layouts_are_frozen` goldens are untouched, which is the
  tripwire that says so mechanically. Three things need more than that.

  **1. `IconRefused` is a new op code behind an *existing* bit.** It rides
  `ICONS` (bit 7) rather than taking a bit of its own, exactly as the M2
  text ops rode `TEXT`. What makes it safe is `ClientCaps`: a client that
  lists `ICONS` is by construction new enough to know the message, and one
  that lists nothing keeps receiving `Error { BadIcon }` verbatim, prose
  and all. (The alternative — adding a node id to `Error` in place — would
  have been a bump: `Error` is an unprivileged message any ordinary client
  receives.)

  **2. `ClientCaps` is the one addition outside the sketch's op list, and
  it is load-bearing.** A capability bit cannot by itself carry a
  server→client addition, because `ServerMsg::decode` answers `UnknownOp`
  for an op it does not know and the client's `poll` treats that as fatal.
  The `THEME` precedent — 0x8004 pushed to everyone right after `Welcome`
  — got away with it only because every client ships from this tree. Route
  A ends that: the Chromium backend is built out of tree and versioned
  independently. `ClientCaps` is the four bytes that make every pushed
  M5-A message safe; the rules, the discovery rule and the grandfather
  clause are under
  [Capability opt-in](#capability-opt-in-clientcaps). It governs bits 8
  and above plus the `IconRefused` exception, and does **not** reach
  backwards: the v1-era unconditional `Theme` push is unchanged, because
  retiring it would itself be a change of bump weight.

  **3. `work_area` is a new message, not a field on `OutputInfo`.** This
  is the decision most likely to be second-guessed, so the reasoning is
  recorded in full.

  Adding it in place would have moved bytes in `OutputInfo`, and the
  tempting move was to reuse #3697's argument (the `layer` field on
  `WindowInfo`, further up this list). **That argument cannot be reused
  here, and saying so is the point.** It has two legs — the block is
  reachable only through `shell.sock`, and every reader ships from this
  tree — and `ListOutputs` (0x001b) saws off the first one *by design*: it
  exists precisely to make `OutputInfo` reachable by unprivileged clients.
  Re-using an argument after removing its premise is exactly what this
  policy exists to stop. A parallel `OutputInfo2` was rejected too: two
  messages carrying the same six fields, forever.

  So `OutputWorkArea` (0x8408) is a message of its own — and the decisive
  argument for it is not compatibility at all but **cadence**. The work
  area moves when an *exclusive zone* changes (`SetExclusiveZone`, a bar
  hiding itself), which touches none of the output's mode, position, scale
  or name. Folding it in would force a whole output list to be re-sent on
  every zone change, or leave the field stale — and stale here is a client
  computing maximize geometry wrong. A separate message is both the
  snapshot carrier and the update channel.

  The objection that sank a parallel `WindowLayer` in #3697 — "an output
  is briefly listed with an unknown work area" — does not apply, because
  the output list has a **terminator**: the snapshot is complete at
  `OutputsEnd`. A window list has no per-window terminator, which is why
  that case went the other way. No pre-existing byte moves, so this needs
  no exemption at all.

  **4. The new strict enums are safe to append to, by the M4-G rule.**
  `CursorShape`, `PopupAnchor`, `PopupGravity`, `DragAction`,
  `KeymapFormat` and `DataSource` all decode strictly — an unlisted value
  is a `BadValue` decode error, as everywhere here — so the question the
  `NodeKind::Icon`/`BadIcon` paragraph asks applies to each: could a peer
  receive a value it does not know? It cannot. Every one of these values
  travels only in a message tied to a bit its receiver must have named:
  the client→server enums ride ops a client may send only because the
  server advertised the bit, and the server→client ones ride messages the
  server may send only because the client listed the bit in `ClientCaps`.
  Both directions are reachable only by a peer that already knows about
  them, which is the property that makes appending to a strict enumeration
  compatible.

* **M5-A deliberately adds no input serials, and that is a decision worth
  its own entry.** Wayland stamps a serial on every input event, and
  `set_cursor`, `xdg_popup.grab`, `start_drag` and `set_selection` all
  require one. nitro's `Key`, `PointerButton` and `PointerEnter` carry
  none, and M5-A does not add any.

  Adding one **in place** would be a `VERSION` bump by this document's own
  rule ("a field added in place to any message an ordinary client can
  receive is a bump"). And it would buy nothing, because the serial's two
  jobs are both empty here:

  | job | does nitro need it? |
  |---|---|
  | anti-spoofing: prove the request follows a real input event this client received | **No.** nitro has no authentication at all — any process with the user's uid may connect (`docs/shell.md`: the grant is "a process running as this user", no finer). A serial check protects nothing that `connect(2)` does not already give away. |
  | race resolution: reject a request naming a stale focus or grab | **No.** The server already knows who holds pointer and keyboard focus, and processes one client at a time on one thread. "Do you hold focus **now**" is strictly more accurate than "did you at serial N". |

  So every op Wayland would gate on a serial is gated on a **server-side
  focus check** instead. This is the contract the M5 follow-up tasks
  implement, so it is stated here rather than left to each of them:

  | op | authorization | on failure |
  |---|---|---|
  | `SetCursor` | holds **pointer focus** | silently ignored — focus can legitimately leave between send and receive |
  | `CreatePopup` / `RepositionPopup` | **owns the parent** window or popup | `Error { UnknownNode }` |
  | `StartMove` / `StartResize` | holds pointer focus **and** a button is down | silently ignored, as `SetCursor` |
  | `SetSelection` | holds **keyboard focus** | `Error { Protocol }` |
  | `StartDrag` | holds pointer focus **and** a button is down | silently ignored |
  | `RequestSelection { source: Drag }` | is the **current drop target** (had a `DragEnter`, no `DragLeave`, drag not finished) | `Error { Protocol }` |

  The split between "ignored" and "`Protocol`" follows whether the client
  could plausibly be *racing* rather than lying: pointer focus changes
  under a client's feet, so losing it is not a protocol violation;
  asking for a drag offer that never existed is.

  **Consequence for a Chromium backend:** Chromium hands the platform
  serials it expects forwarded. The backend drops them. Nothing upstream
  inspects what it does with them.

* `VERSION` is bumped only for a change that is not expressible that way —
  a different framing, a changed field, a removed op. A version mismatch is
  fatal at handshake: there is no negotiation and no compatibility shim.
* The `payload_layouts_are_frozen` test in
  `crates/nitro-wire/tests/messages.rs` holds golden byte strings; it is
  the tripwire for an accidental layout change.
