# nitro-server

The display server. One thread, one epoll, a seat (`nitro-seat`), a KMS
backend (`nitro-kms`), a real scene graph (`nitro-scene`) painted with
`nitro-raster`, clients over `nitro-wire`, input from libinput and
xkbcommon, and — since M3 — **window management**: server-side
decorations, move/resize/focus/MRU, window states and multi-output
layout (`docs/wm.md`), plus a second, **privileged socket** the shell
connects to for layers, exclusive zones, anchors, global hotkeys, the
window list and output enumeration (`docs/shell.md`). `lib.rs` exposes
`run(Config)`; `main.rs` only
turns environment variables into a `Config`, so the integration tests
drive the whole loop in-process on the fake backend with a fake input
source — no seat, no DRM device and no evdev node anywhere. The M0 demo
is gone: the pixels the server paints for itself are the desktop
background under the clients and the frames around them.

## Sockets

Three, and they do different jobs.

The **wire socket** is the one clients use: `nitro-wire` v1, at
`$XDG_RUNTIME_DIR/nitro/wire.sock` unless `NITRO_SOCKET` says otherwise
(`/tmp/nitro-<uid>/wire.sock` when the runtime directory is unset or
relative). Client and server resolve that path through the *same*
function in `nitro-wire`, so a client started in the server's environment
finds it without being configured. A connection opens with `Hello`
(version plus a name for the logs) and is answered with `Welcome`
(version, capability bits, server name). Two capability bits are set:
`WM` (bit 4) **unconditionally** — the server always manages windows, so
a client may always send the M3 window ops — and `TEXT` (bit 1) only when
the startup font scan actually found a face, because that bit is a promise
that a `Text` node will draw something. Note what `TEXT` does *not* gate:
`Text` nodes and `SetText` are accepted either way, and a fontless server
shapes to an empty run and answers a well-formed `TextMetrics` of zero
width. Killing the connection over a missing font would make "no fonts
installed" a fatal error for every client on the box; the bit instead
answers the question a client can act on — is it worth laying out for text
at all? Direct scanout and dma-buf surfaces remain later work, and there a
zero bit *is* the protocol's way of saying "this does not exist yet": a
client that asks for a `Surface` node gets `WrongKind` and the connection
closes, rather than discovering at runtime that the feature silently did
nothing.

The **shell socket** — since M3-B — is the same protocol at
`$XDG_RUNTIME_DIR/nitro/shell.sock` (`NITRO_SHELL_SOCKET`), and a
connection accepted there gets `SHELL` (bit 5) on top of the usual bits.
That bit is the whole privilege model: the bar, the launcher and the
wallpaper are ordinary `nitro-ui` clients that are allowed to set layers,
reserve screen space, bind global hotkeys, grab the keyboard, list and
control other clients' windows and enumerate outputs *because of where
they connected*, not because of anything they sent. Same framing, same
handshake, same decoder, same event-loop arm; the difference is one `if`
against the epoll token range, in one place, before any shell op is looked
at. An unprivileged client sending one gets `Error { Protocol }` and is
disconnected. Several shell clients at once are fine (three processes),
and both sockets live in the same `0700` directory, so the grant is
precisely "a process running as this user" — `docs/shell.md` states what
that is worth, what it is not, and what a finer model would need.

The **control socket** is the v0 line protocol and stays as the server's
own test and debug channel — it is what `nitro-shot` and the integration
tests speak, and it deliberately has nothing to do with the client
protocol. Path: `NITRO_CONTROL`, else
`$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock`
with a warning. One request per line; the reply is a status line and an
optional body. Parsing and formatting live in `protocol.rs` with unit
tests.

| request              | reply                                                                 |
|----------------------|-----------------------------------------------------------------------|
| `shot [output-name]` | `ok <width> <height> <stride>\n` + `stride*height` bytes `XRGB8888`. Read from the output's **shadow buffer** when there is one — cheaper (no uncached reads back out of write-combined memory) and, if anything, more honest: the shadow is complete by construction, where the front buffer is complete only because the age-2 rule says so. It falls back to `read_front` under `NITRO_SHADOW=0` and for the moments before a shadow has been painted into. The two are byte-identical once a frame has settled, which `tests/shadow.rs` pins. |
| `shot-front [name]`  | The same, read off the **scanout** buffer whatever the shadow holds. Test-only: with the shadow on, an ordinary `shot` cannot see whether the copy out of the shadow put the right bytes where the display reads them, so a shadow test would be unable to fail. |
| `outputs`            | `ok\n`, one `name WxH@refresh_mhz scale=<s> pos=<x>,<y> primary=<0\|1>\n` per output, blank line. `pos` is the output's origin in **desktop** (logical) space — the number a window position on that output is relative to, which is what someone debugging a two-monitor layout is asking for; the device rectangle is `WxH` at `pos × scale`, so both spaces are recoverable from the one line. `scale` prints without a trailing `.0` (`scale=2`, `scale=1.25`), which is also how the configuration file that produced it spells it. |
| `stats`              | `ok\n`, one `key value` line per statistic (see below), blank line     |
| `quit`               | `ok\n`, then orderly shutdown                                          |
| `reload`             | `ok\n`; re-reads `server.conf` and applies it. Synchronous — the `ok` comes back after the reload was applied — which is what makes it the reload a test can use, where SIGHUP and the inotify watch are races against the loop noticing. `config_reloads` counts it. |
| `plug WxH`           | `ok\n`; fake backend only — hotplugs an output in, so a test can drive the "no output yet" state. Refused on DRM, where an output exists because a connector says so. |
| `unplug`             | `ok\n`; fake backend only — removes the last output, which is the half that matters to the window manager: removing an output orphans its windows, and migrating them is the behaviour under test. |
| `theme`              | `ok <scheme> <serial>\n`, one `role #rrggbb[aa]` line per colour role, blank line. Read-only, and the cheap way to answer "what colour is the desktop actually using" with no client and no screenshot: the palette is server state, so the server is the only thing that can say. The role names are the `server.conf` keys, so any line of the output is one `theme.` prefix away from being the config that pins it. See `docs/theme.md`. |
| `focus`              | `ok\n`; gives keyboard focus to the topmost window. Test-only, and it exists because focus otherwise *follows the click*: a toolkit test of Tab traversal would have to synthesise a click to get focus, which moves the focus to whatever widget was under the pointer — the very state it is about to assert on. `err no windows` when there are none. |
| anything else        | `err <message>\n`                                                     |

Several requests per connection are fine; a request line longer than 256
bytes without a newline drops the client. All three socket files are
unlinked on shutdown.

## Environment

| variable          | values                           | default                        |
|-------------------|----------------------------------|--------------------------------|
| `NITRO_BACKEND`   | `drm`, `fake`                    | `drm`                          |
| `NITRO_DRM_CARD`  | `/dev/dri/cardN`                 | first card with a connected output, else first that opens |
| `NITRO_FAKE_SIZE` | `WxH` (fake only)                | `1280x720`                     |
| `NITRO_CONTROL`   | socket path                      | `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock` (with a warning) |
| `NITRO_SOCKET`    | socket path                      | `$XDG_RUNTIME_DIR/nitro/wire.sock` (resolved by `nitro-wire`, so clients agree) |
| `NITRO_SHELL_SOCKET` | privileged socket path        | `$XDG_RUNTIME_DIR/nitro/shell.sock` (same resolution; see `docs/shell.md`) |
| `NITRO_INPUT`     | `off`                            | input enabled                  |
| `NITRO_INPUT_DIR` | directory scanned for `event*`   | `/dev/input`                   |
| `NITRO_SCALE`     | `<connector>=<f32>,…` per-output scale override, e.g. `HDMI-A-1=2` | `server.conf`'s `output.<c>.scale`, else EDID-derived: 2 at ≥ 192 dpi, else 1. See `docs/wm.md` and the precedence table below. |
| `NITRO_CONFIG`    | path of `server.conf`            | `$XDG_CONFIG_HOME/nitro/server.conf`, else `$HOME/.config/nitro/server.conf`. With neither variable set there is **no file and no watch** and the server runs on its defaults — the state a system service with an empty environment is in. See `docs/settings.md`. |
| `NITRO_SHADOW`    | `0` to paint straight into the scanout buffer | enabled: each output gets a heap shadow buffer (one scanout-sized allocation, ~8 MB at 1080p) that the rasterizer paints into, with only the damage rects streamed out to the write-combined dumb buffer. Worth 9.3× on the frame path and 8 MB of RSS per output (`docs/latency.md` §4.5, `docs/budget.md`); `0` is the A/B lever, not a supported configuration. |
| `NITRO_FONT_DIRS` | colon-separated font directories | `/usr/share/fonts:/usr/local/share/fonts:~/.local/share/fonts` (read by `nitro-text`) |
| `NITRO_FONT_CACHE_MB` | cap on resident font-file bytes | `8` (read by `nitro-text`; `0` keeps only the file currently in use — the file that overran the cap is never its own victim, so a too-small cap does not turn into one disk read per glyph) |
| `NITRO_FONT_INDEX_CACHE` | path of the font index cache, or `off` | `$XDG_CACHE_HOME/nitro/fonts.idx` (read by `nitro-text`) |
| `NITRO_LOG`       | `error`, `warn`, `info`, `debug` | `info`                         |

The keyboard layout comes from the `XKB_DEFAULT_{RULES,MODEL,LAYOUT,VARIANT,OPTIONS}`
variables — the same ones every other libinput-based compositor and
`setxkbmap` use — then `server.conf`'s `keyboard.*` section, with a
fallback to the `us` layout.

## Configuration: `server.conf`

The persistent half of the display configuration is one plain text file,
`$XDG_CONFIG_HOME/nitro/server.conf`, parsed by `src/config.rs` (which is
also where the file format and the "nothing here fails" rule are argued).
It is read at startup and re-read on every reload; `docs/settings.md` is
the user-facing description.

```text
output.HDMI-A-1.scale    = 2
output.HDMI-A-1.position = 0,0
output.HDMI-A-1.primary  = true
output.VGA-1.position    = 1920,0

keyboard.layout  = de
keyboard.options = ctrl:nocaps

theme.scheme = dark
theme.accent = #6ca8f0
```

### Precedence

Environment beats file beats EDID/default, and nothing else is ever
consulted. The environment wins because it is the *development* channel —
a `NITRO_SCALE=HDMI-A-1=2 just fake` must not be silently overridden by
whatever the box's own config says — and the file wins over the EDID
because it is the user's explicit answer to the EDID's guess.

| setting | wins | then | then |
|---|---|---|---|
| output scale | `NITRO_SCALE=<c>=<f32>` | `output.<c>.scale` | EDID dpi step: 2 at ≥ 192 dpi, else 1 |
| output position | — | `output.<c>.position`, in **desktop** (logical) units | placed after the last positioned output, in connector order |
| primary output | — | `output.<c>.primary = true` | the first connector |
| keyboard | `XKB_DEFAULT_{RULES,MODEL,LAYOUT,VARIANT,OPTIONS}` | `keyboard.layout\|variant\|options` | the `us` layout |
| colour scheme | — | `theme.scheme` (`light`\|`dark`) | `light` |
| one colour | — | `theme.<role>` = `#rrggbb[aa]` | the scheme's value |

The scale rule lives in one function (`resolve_scale`) so it cannot drift
between startup, a reload and a hotplug — all three go through
`sync_outputs`, and a replugged monitor coming back a different size from
the one the user configured is exactly the bug a second copy of the rule
would produce. `primary_output()` is the same idea for the primary: every
"the first output" in the window manager reads it.

### Position: one layout, two spaces

Every output has a **device** rect (scanout pixels; what the pointer is
clamped to and what `output_at` hit-tests) and a **desktop** origin
(logical units; what every window rectangle in the window manager is
relative to). A configured `position` is a *logical* one, so it is
multiplied back by the output's scale to give the device rect, and both
spaces are computed in one pass in `sync_outputs`.

Doing only half of that would be worse than doing neither: with the
desktop layout following the file and the device layout still in connector
order, the pointer would cross between screens at a different place from
where a dragged window does. `desktop_origin` therefore does nothing but
read the table `sync_outputs` left behind — which also keeps it cheap, and
it is called from every hit test, every drag motion and every clamp.

### Reloading

Three doors, one `Server::reload_config`:

* **inotify** on the *directory* the file is in. A settings app writes a
  temp file and renames it over the top, which replaces the inode, so a
  watch on the file itself would follow the old one into oblivion and
  never fire again; events are filtered to the file's own name. A watch
  that cannot be created is a warning, not a failure — the same rule the
  input-hotplug uevent socket follows.
* **SIGHUP**, on its own self-pipe with its own epoll token, so a reload
  can never be mistaken for a request to shut the desktop down.
* **`reload`** on the control socket, which is synchronous and therefore
  the one a test uses.

The watch asks for `CLOSE_WRITE`, `MOVED_TO`, `CREATE`, `DELETE` and
`MOVED_FROM`. The last two were missing until issue #558, on the theory
that a file which goes away should leave the last configuration in force
so a half-finished `mv` does not flicker the desktop — which was wrong:
`rm server.conf` is the documented way back to defaults, and without them
a `theme.scheme` from a file that no longer existed stayed in force. A
`mv`'s intermediate state is answered by the reload path reading whatever
is on disk *now*, and the pair of events arrives in one drain, so it
costs one reload rather than two.

A reload re-applies everything rather than diffing, with two exceptions
that earn it: the work is one file read, one `sync_outputs` and (only
when the `keyboard.*` section actually changed) one keymap compile, all
of which startup already does. The **palette** is the other exception,
and for a different reason — applying it is not idempotent from the
outside, since it restyles every decoration, repaints every client and
puts a `Theme` on every socket. An unchanged palette is therefore
silence, so a reload that only moved `keyboard.layout` costs nothing. A keymap
swap resets the xkb state and the hotkey table, because the modifiers a
user is holding belong to keys that no longer mean what they did — the
same reasoning the VT-switch and input-hotplug paths use. A scale change
sends every window on that output a `Configure` (whose `scale` field is
how a client learns how many device pixels its logical rectangle is worth)
and repaints the output in full.

`fake` needs no seat at all and never opens input devices: `just fake`
runs it locally, `just fake-shot` grabs a PNG from it.

## Event loop

Level-triggered epoll, and exactly one timer. An idle server never
wakes: with nothing changing there is no damage, so no frame is painted,
so no flip completes, so nothing becomes readable. `top` shows 0.0 % and
`voluntary_ctxt_switches` stops counting — including with a client
connected and its window on screen. The one timer is the deferred-flip
deadline below, and it is armed **only** while a flip is actually being
held, so it does not cost the idle case anything: measured on the box,
five seconds of idle after a deferral is 0 frames, 0 CPU ticks and
`voluntary_ctxt_switches` flat.

| fd                        | on readable                                                              |
|---------------------------|--------------------------------------------------------------------------|
| seat                      | `Seat::dispatch`; `Disable` → suspend input, `backend.pause()`, `ack_disable()`; `Enable` → resume backend and input, reset xkb, full repaint |
| backend `poll_fds()`      | `Backend::dispatch`; `Flipped` → `Presented`/`Frame` to clients, then paint the next frame; `Hotplug` → `rescan()`, re-register fds, place unplaced windows, repaint |
| libinput                  | dispatch, convert to `InputEvent`, route, then update the scene and paint if anything moved |
| input uevent socket       | a device appeared or went away: rescan `NITRO_INPUT_DIR`, add/remove libinput paths, register any new fd, reset xkb if a device left |
| defer timer               | a held cursor-only flip's deadline passed: count a `defer_timeouts` and paint without the client's answer |
| config inotify            | something changed in the directory `server.conf` lives in: drain the queue, and if our file was named, reload the configuration. An inotify fd with nothing queued is simply not readable, so a desktop nobody is configuring pays one fd in the set and **zero wakeups** — the same bargain the defer timerfd and the uevent socket make |
| signal self-pipe          | SIGTERM/SIGINT → orderly shutdown (`signal-hook`'s `low_level::pipe` on a `UnixDatagram` pair) |
| SIGHUP self-pipe          | re-read and apply `server.conf`. A **second** datagram pair with its own fd and token, because a self-pipe carries no payload: with one pair the loop would learn that *a* signal arrived and could not tell "reload" from "shut down" |
| control listener          | accept, register the client                                              |
| wire listener             | accept, allocate a `ClientId`, register the client                       |
| shell listener            | the same, from the privileged token range: the accepted client's `Welcome` gets `caps::SHELL` |
| wire client               | read, decode, buffer mutations, apply on `Commit`; `OUT` interest only while bytes are queued |
| control client            | read lines, answer, drop on hangup; `OUT` interest only while a reply is queued |

Every wakeup does a bounded amount of work — a client read is capped by
`nitro-wire`'s read budget, a frame's paint is proportional to its damage,
nothing blocks — which is why one thread is still enough. Fanning
rasterization out to workers is a decision for the day a frame's damage
stops fitting in a vblank, and that day has not arrived (see the measured
numbers below).

`epoll_wait` is retried on `EINTR` (the signal handler interrupts it and
the kernel never restarts `epoll_wait`); the self-pipe is what ends the
loop.

Startup on DRM: open the seat, `dispatch()` once (libseat queues the
initial `Enable` inside `open_seat` without making the fd readable), wait
for `is_active()`, then open the card through the seat. The backend gets a
`dup` of the seat's fd (same open file description, hence DRM master) so it
can be `'static`; the seat closes its own fd after the backend is gone.

Shutdown order is the `Server` struct's field order: wire clients, control
clients, sockets, input (whose device fds belong to the seat), backend
(destroys FBs/dumb buffers, releases master with the fd), the seat's
`Device`, the seat. Exit code 0.

## Clients

**Id translation.** A client allocates its own `NodeId`s and `BufferId`s
and never learns the scene's keys. The server keeps a map per client in
*both* directions: ids to keys, to look a mutation up, and keys back to
ids, to name a node in an input event. The forward map is also the
isolation mechanism — a client simply cannot name another client's node,
which is cheaper and harder to get wrong than an ownership check on every
call. The reverse map is safe because scene keys are generational: a
stale key can never be confused with a recycled one.

A window's id names the client's **content** group, which is what the
server's decorations are wrapped *around*: framing a window mints a new
root above that node and leaves the node itself alone, so a client can
never create a node on top of its own title bar, and every coordinate it
is given or sends is in its own content's space.

**Transactions.** Nothing a client sends touches the scene until its
`Commit`. Mutations are buffered in arrival order and applied in one
pass, so a frame never shows half a batch. The one exception is
`CreateBuffer`, whose pixels are read when the message arrives rather than
at commit: the client may legitimately reuse the memfd for its next frame
as soon as it has sent the message, and reading late would tear its own
image. The descriptors ride along with the transaction and move into the
server's buffer map only once the scene has minted keys for them.

**Errors are fatal.** A mutation the scene refuses aborts the whole batch:
the client gets one `Error` carrying the commit serial and a code, and the
connection closes. A client that named a bad id has lost track of its own
tree, and everything it sends afterwards is guesswork. Disconnect — for
any reason, including the socket simply going away — destroys everything
the client owned: its windows (and with them their node trees) and its
buffers. The client's own maps are the authority, so that is a walk of
one hash map rather than a search of the scene, and the scene damages the
pixels those windows covered as it removes them, so the area repaints
without them on the next frame.

**The buffer copy path.** `CreateBuffer` validates the description
(format, stride, a 64 MiB cap that fits a 4K ARGB frame with room to
spare) and `pread`s the pixels out of the client's memfd into scene-owned
memory. They are **copied, not mapped**, and that is the M1 answer rather
than an oversight: a client can shrink a memfd under a live mapping and
turn the server's next read into SIGBUS, so a safe mapping needs either
enforced `F_SEAL_SHRINK` or a signal handler — and `mmap` is `unsafe`,
which this tree does not use. The copy costs one pass over the pixels per
update, and `BufferDamage` keeps that pass proportional to what actually
changed: only the damaged *rows* are re-read, one `pread` per row band,
because a row is contiguous and damage rectangles are usually wide.
Revisit with sealing when a client pushes video.

`DestroyBuffer` releases the descriptor as well as the pixels. The scene
forgets the bytes on its own, but the fd is the server's, and a client
that cycles buffers — create, damage, destroy, once per frame, which is
the obvious way to push changing images — would otherwise leak one
descriptor per frame until it hit the process limit.

**Placement and decoration.** A new window is **decorated** unless it
passed `UNDECORATED`: the server wraps its group in a frame group it owns
and draws a title bar, a border and two buttons into it. It is then
placed **centred-cascade** in the primary output's work area — the first
window centred, each later one 28 px down and right, clamped inside the
area and rounded to whole logical pixels. The client is told what it got
with `Configure` (content size, content position, scale, output); a later
resize the server or the client's own `SetBounds` decides on produces
another, and so does a pure *move*, because `position` is what a client
crops a screenshot with. `docs/wm.md` is the whole model: hit regions,
states, focus, MRU, shortcuts, multi-output policy and scale.

A window created while there is *no* output — every connector unplugged,
or a hotplug still in flight — is held in a pending list instead. It is a
real window owning real nodes; it simply has nowhere to be, and it is
placed and configured the moment an output appears. No `Configure` is
sent before then: an unplaced window has no size, scale or output to
report, and naming output 0 for it would be a lie the client cannot
detect.

**Presentation and frame callbacks.** A commit is reported with
`Presented` when the frame carrying it reaches the screen, and a
`RequestFrame` is answered with `Frame` *after* the flip, because the
deadline it carries is only meaningful once the vblank it is extrapolated
from has actually happened.

Both of those need a second path, though, and the reason is the design
itself: **the server only flips when something changed**, so "wait for the
next flip" is not a promise it can keep to a client that changed nothing.
Two cases would otherwise hang:

* A commit that damages no pixel — a hidden subtree, a property set to the
  value it already had, a bare `RequestFrame`. The serial is the client's
  flow control, so it is acknowledged from the last vblank we saw rather
  than held for a frame that will never carry it. Holding it would stall
  any client that waits for `Presented` before sending the next frame, and
  would grow the pending list without bound. There are two shapes of this:
  a client with no *placed* window — nothing it commits can reach a screen
  — is answered inline at commit time, and one whose windows are placed
  but whose transaction produced no damage is answered once the update
  pass has confirmed there is nothing to paint.
* A `RequestFrame` from a quiescent desktop — which is exactly how a
  client *starts* an animation. If the answer waited for a flip, and the
  flip waited for damage, and the damage was going to be the client's
  response to the answer, nothing would ever happen. It is answered
  immediately from the extrapolated vblank clock, which
  `frame::frame_deadline` computes with or without a flip having happened.

A commit is stamped only onto the outputs its own windows are on, so the
same serial is never reported twice on a multi-output desktop, and frame
callbacks are answered only by the vblank of the output the requesting
window is actually on.

## Frame path

One pass, for each output, every time an event could have changed
anything:

1. `scene.update()` runs the scene's layout/damage pass and folds each
   output's damage (global device pixels, shifted back to the output's own
   origin) into that output's `OutputState`. Any `Configure` it produces
   is sent to the owning client.
2. `repaint_region()` computes the region the *scanout buffer* is behind
   by. The backend owns two buffers per output and alternates them
   strictly, so the buffer handed out at frame `n` is the one that was on
   screen at frame `n - 2`. Bringing only this frame's damage up to date
   would leave the previous frame's changes stale in it, so the region is
   **`damage(n) ∪ damage(n-1)`** — the *age-2 rule*. What is carried
   forward is the previous frame's **damage**, not the region it painted:
   the painted region was itself a union of two frames' damage, and
   feeding it back would make every frame at least as large as the one
   before and never shrink again. One consequence worth stating: after a
   resume, a modeset or a new output, `invalidate()` simply sets the
   damage to the whole output, and the rule produces the two full frames
   both unknown buffers need, with no separate "repaint fully for N
   frames" counter to get subtly wrong when a client commits between them.
3. The rasterizer is pointed at the output's **shadow buffer** — a
   heap-resident, scanout-sized `XRGB8888` copy of what the output must
   show — and given `rasterize_region()`, which is `damage(n)` *alone*.
   The shadow is never stale, so nothing older needs re-drawing; the age-2
   union applies to the copy in step 5, not to the paint. Under
   `NITRO_SHADOW=0` there is no shadow, the rasterizer writes the scanout
   mapping directly and is given the union for both.

   The reason for the indirection is the destination. A dumb buffer is
   mapped write-combined — writes coalesce, reads are uncached — and
   source-over reads every destination pixel it blends, which measured out
   at ~87 % of `paint_us` on the test box. Painting into cached heap and
   streaming the result back is 9.3× on the whole frame path, for one
   scanout-sized allocation per output (`docs/latency.md` §4.5, #539).
4. For each rect of the rasterize region: the server's background first —
   the vertical gradient inside its 4-px frame, one `fill_irect` per row —
   unless an opaque client rect covers the whole clip, which the scene
   promises conservatively; then the scene's paint list clipped to the
   rect, one `nitro-raster` call per item; then the software cursor last.
   The rasterizer never writes outside the clip it was given, so one rect
   cannot smear into another.
5. The age-2 region is streamed out of the shadow into the back buffer,
   one `copy_from_slice` per row (one for the whole block when the region
   spans full rows): sequential and **write-only**, which is what
   write-combined memory wants. Nothing reads the mapping.
6. `commit` with the same region as `FB_DAMAGE_CLIPS` — it is exactly the
   set of pixels that differ between what this buffer holds and what must
   be on screen — and the paint time, the copy time and the damage area go
   into the rolling statistics.

There is no hardware cursor plane. A KMS cursor is composited by the
display engine rather than into the framebuffer, so it does not appear in
a screenshot taken by dumping the back buffer; every visual test run over
SSH would lose the pointer exactly when it matters. The cursor is a 24×24
ARGB image blitted like anything else, inside the frame's damage clip —
into the shadow, like everything else — and a pointer move damages the old
and the new cursor rect.

It is drawn only once a pointer device has actually reported something,
not merely because an output exists. Whether there is a pointer on this
desk is a question libinput's capability bits answer badly — a machine can
have a disabled touchpad, or a "pointer" that is really a lid switch — and
the honest test is whether one has moved. A keyboard-only box therefore
shows no arrow, which is also what a headless server's screenshots should
show.

A commit that fails keeps its damage and sets a retry flag, so the next
event repaints the same region instead of stranding the output until the
next resume or hotplug (issue #519). The paint itself is not wasted — the
back buffer holds it — but the buffers did not swap, so the next paint
must cover the same region again. The matching fix on the other side is
that `nitro-kms::resume()` now clears flip-pending for every output
(issue #520): a flip abandoned by a VT switch is never completed, and an
output that still believed a flip was in flight would never paint again.

### Deferring a cursor-only flip (issue #529)

Step 1 hides a scheduling decision that is worth a frame of latency, and
for a while cost one. **A pointer move damages the cursor immediately** —
before the client under the pointer has heard anything — so the obvious
schedule paints and flips a cursor-only frame at once. The client's
answering commit arrives 0.12 ms later, by which time a flip is in flight
and `paint` cannot start another, so the client's pixels ride the
*following* vblank: content always one whole refresh behind the arrow
that provoked it. Measured end to end that was a median input-to-photon
of 25.2 ms against a one-frame budget, with 0.3 ms of work in it.

So when the damage on a wakeup is **cursor-only** and an input was just
routed to a client, the flip waits (`Server::paint_or_defer`). It is
released by whichever comes first:

* **the client's commit** — the next wakeup, usually — which adds content
  damage, so one flip carries cursor *and* content;
* **the deadline**, `frame::frame_deadline`: the next expected vblank
  minus `FRAME_MARGIN_NS`. A client that never answers therefore costs
  nothing at all, because 2 ms is ten paint passes on the test box — the
  cursor still reaches that same vblank.

Four conditions all have to hold, and each of the last three is a way of
saying "nobody is already waiting for these pixels":

* a client was told about an input and has not answered. Cursor movement
  over the bare **desktop** has nobody to wait for and stays on the
  untouched fast path — measured, 2 flips per isolated move either way;
* something is paintable *now*. An output whose flip is still in flight
  is not being deferred, it is simply not being painted; that wakeup
  comes back through `on_flip`, which goes through the same decision;
* every output that wants a frame wants it for the cursor alone
  (`OutputState::cursor_only`). Content damage or a commit retry both
  disqualify it. The **age-2 carry deliberately does not**: those pixels
  are already on screen and the repaint only brings the *other* buffer up
  to date, so nobody is waiting for them. Counting them would be actively
  wrong — a pointer over a client that answers every motion produces
  content damage on alternate frames, so every second frame would refuse
  to wait and put the next answer a flip behind again;
* the timer arms. If it will not, the server paints: a frame nothing
  would ever wake it for is far worse than a frame one refresh early.

The deadline is one `CLOCK_MONOTONIC` timerfd in the epoll set, armed
only while a flip is held and disarmed on the way into every paint —
including the paths that never consult the deferral (a resume, a hotplug,
startup), which is what makes "idle is zero wakeups" unconditionally
true. `flips_deferred` and `defer_timeouts` in `stats` are the two
numbers to look at: a `defer_timeouts` that tracks `flips_deferred` is a
client that is not answering, and the cursor is fine — it is reaching
every vblank — but nothing is riding with it.

One subtlety in the bookkeeping: disarming the timer and forgetting *who*
is being waited for are separate steps. A wakeup that wanted to paint and
could not (a flip still in flight) keeps the wait alive for the wakeup
that can; clearing it there would lose the answer in exactly the
saturating-input case, where every motion arrives mid-flip.

The measured effect, and the one number that matters, is in
`docs/latency.md` §4: **median 25.1 ms → 9.3 ms** over 202 samples, with
the minimum falling from 17.5 ms (a whole frame — the signature of the
structural flip) to 1.3 ms.

**How this generalises.** Nothing in the mechanism assumes the trigger
was an input event: `DeferredFlip` holds a set of clients and a deadline,
and the release condition is "one of them committed, or the deadline
passed". To hold a flip briefly for a client that is *known to be
responding* — an animator that has committed on each of the last N frames
— only the predicate that populates that set would change, from "was just
sent an input" to "has a commit streak". The deadline, the timer, the
counters and the cursor-only guard all stay as they are. What should not
change is the bound: it must remain the vblank the frame was going to
reach anyway, so that a wrong guess about a client costs nothing rather
than a dropped frame.

## Text

**Clients send strings, the server draws glyphs.** That is the whole
shape of it, and it is a deliberate asymmetry: a label is a few dozen
bytes on the wire whatever the font, an app binary carries no font
library, and the remote case costs exactly what the local one does. The
price is that the server owns a font database, a shaper and a glyph
cache, which is what `nitro-text` is; `text.rs` assembles them into the
one `TextEngine` the event loop holds.

**Startup.** `FontDb::scan()` walks `NITRO_FONT_DIRS` (or the three
default directories) once and logs the face count, how long it took and
whether it came off the index cache. Fonts are not hot-reloaded: one
installed while the server runs is picked up at the next restart. A box
with no fonts is a warning, not a failure — the server runs, the `TEXT`
capability bit stays clear, and text nodes draw nothing.

**No font file's bytes are resident until a face is drawn with**, and they
are handed back when the loop next goes idle. The scan builds an index —
family, attributes, `(path, face index)` — and `FontDb::face` reads the
file on demand into a cache capped by `NITRO_FONT_CACHE_MB` (default 8
MB). Releasing is free because the atlas keeps the rendered masks: a face
is needed to shape a run and to rasterize a glyph the atlas has not seen,
neither of which a settled desktop does, so the cost is one `read(2)` the
next time a genuinely new glyph appears. This is what got the server from
20.4 MB back under its 8 MB RSS budget on the box; `stats`' `fonts_loaded`
and `font_bytes` are the counters, and `crates/nitro-text/README.md` has
the measurements. The cap is a backstop for a frame whose faces do not fit,
not the mechanism that keeps the steady state small — `nitro-text` keeps
separate `evictions` (cap) and `releases` (idle) counters, and a non-zero
`evictions` means a single frame's working set genuinely overran
`NITRO_FONT_CACHE_MB`.

**`SetText`** is an ordinary mutation: buffered, applied at the client's
`Commit`, shaped there, and the resulting run stored under the client's
id. The scene gets a `TextRef` — an opaque `u32` key plus the measured
block size, first baseline, colour and alignment — so `nitro-scene` never
sees a glyph or a font, and `paint_list` can place a centred block without
consulting the store. Every node the commit (re)shaped is answered with a
`TextMetrics` carrying its measured size, which is how a toolkit lays a
label out without a separate round trip.

**`MeasureText`** is the one exception to "everything waits for the
commit", and it is answered the moment it is decoded. A text field cannot
lay itself out until it knows how wide its content is, and making it wait
a frame for that would put a round trip in the middle of every keystroke.
It stores nothing and mutates nothing: the answer, a `TextMeasured`,
carries the block metrics plus one `(byte offset, x)` pair per cluster
boundary, which is exactly what a caret needs.

**Painting.** `PaintKind::Text` reaches `TextEngine::paint`, which
resolves each glyph to an atlas mask (rasterizing on a miss) and blits it
with `Canvas::blit_mask` — A8 coverage times the node's colour,
source-over, inside the damage clip like every other item, **and inside
the node's bounds**, which is the one rule text adds. Every other kind's
geometry is its bounds and so cannot escape them; a run's is whatever the
shaper produced. Damage is computed from the bounds, so a glyph pixel
outside them is one nothing will ever repaint — it would outlive the next
`SetText`, the node and the window, as a ghost. Glyphs are
rasterized at the **device** size: the engine reads the scale out of the
paint item's world transform, so a 2x output gets real 2x glyphs rather
than a magnified 1x bitmap, and neither the scene nor the rasterizer has
to know that text has a resolution at all. The pen's fractional x is not
thrown away: it selects one of four subpixel buckets in the cache key, so
a run's glyphs land where shaping put them.

**Lifetime.** A shaped run outlives the message that made it, so
something has to free it. Three places do, and between them they cover
every way a run can become unreachable: re-`SetText` on a node releases
the run it replaces, destroying a node (or a window, or a subtree)
releases every run under it, and a disconnect drops everything the client
owned by owner id. A shell that restarts its clients would otherwise leak
a glyph vector per label per restart.

**Hostile input stops at the boundary.** A non-finite size or wrap width
is replaced rather than passed to the shaper (a NaN would poison the line
breaker), sizes are clamped to 1..=256 px so one glyph cannot ask for an
atlas page of its own, and a string longer than 64 KiB is truncated at a
char boundary rather than killing the connection — an over-long label is a
client bug, and a label that is merely cut off is a far more debuggable
symptom than a disconnect.

Limitations are `nitro-text`'s and are listed in its README: LTR only, no
bidi, no rich text, per-run font fallback, no colour emoji.

## Input

libinput is created with `new_from_path`, not `new_with_udev`: the udev
backend would pull `libudev` into the dependency tree for something a
`read_dir` of `/dev/input` does (`event*` nodes only — `mouse*` and `js*`
are legacy interfaces for the same hardware, sorted numerically so
`event10` follows `event9`). Devices are opened through `nitro-seat`,
which is the only thing in the process allowed to open `/dev/input/*`, and
that is what lets the server run without root. They are also **closed back
through the seat**, which matters more than it looks: libinput hands back
only the raw descriptor, so the `Device` it belongs to must be findable
from that number, and libseat refuses to reopen a device it still has
open — leaving one behind is exactly what makes `Libinput::resume` fail
after a VT switch.

**Device hotplug** (deferred from M1, landed in M3) rides the *same*
kernel uevent socket `nitro-kms` uses for DRM, on the `input` subsystem.
On an `add`/`remove` the server re-scans `NITRO_INPUT_DIR`, diffs it
against the paths libinput already has, and adds or removes the
difference — the path backend has no idea devices come and go, so the
scan is ours. A new fd joins the epoll set; a device leaving resets the
xkb state, because it may have been holding a modifier whose release will
never arrive. Failure to open the socket is logged, not fatal: a sandbox
with no netlink loses hotplug, not the keyboard it already has.

Everything above `input.rs` speaks `InputEvent`, a small enum with no
libinput in it. That `InputSource` seam is what makes the whole input
path — hit-testing, focus, enter/leave bookkeeping, the input-to-photon
clock — testable on the fake backend: `FakeSource` is a queue a test
pushes into, woken through an `eventfd` so the loop sees the same "fd
readable, then dispatch" shape it sees from libinput.

Keys are translated by xkbcommon against the `XKB_DEFAULT_*` keymap, with
the evdev-to-XKB `+ 8` offset applied at every boundary and each event
resolved against the state *before* it is applied (so pressing Shift does
not retroactively shift itself). If no keymap compiles the server logs a
warning and runs on: the evdev keycode still reaches the focused client,
without keysym or text, which is a far better failure mode for a display
server than refusing to start. The chords that never reach a client are
the compositor's, and `docs/wm.md` has the whole table: **Ctrl+Alt** for
the console escape hatches (Backspace quits, F1..F12 switch VT),
**Alt+Tab** for the MRU focus cycle, and **Super** for the
window-management shortcuts (`Q` close, `M` maximize, `F` fullscreen,
`H` minimize, `←`/`→` tile). `Super+Enter` was reserved for the launcher
in M3-A and is no longer a compositor chord: a shell client binds it with
`BindKey` on the shell socket, which is why the table had to give it up
(`docs/shell.md`).

A **shell** client's bindings sit between those and the focused client:
after the compositor's, which are not negotiable, and before any
application's, because a global hotkey the focused application could also
see would be a keylogger and an ambiguity at once. A `GrabKeyboard` from a
shell client replaces the **focus** as the destination of key events, but
does *not* outrank either binding table: a bound chord pressed under a grab
arrives as a `HotKey`, not as a `Key` to the grabbing window. That is what
lets a launcher opened by a bare-Super tap be closed by a second tap while
it holds the grab — see `docs/shell.md`.

Routing:

* The pointer has one position in device pixels, clamped to the **union**
  of the outputs, so it can cross a gap between them in a single motion.
  Acceleration is whatever libinput applied. Absolute devices report in
  their own unit square and are scaled onto the first output; mapping a
  tablet or touchscreen to the right output is a settings question, and
  settings are M4.
* Every motion hit-tests the scene. The window under the pointer gets
  `PointerMotion` with **window-local** coordinates (not node-local: a
  client compares the position against the layout it sent, whatever
  transforms the intervening groups apply) and the node that was hit; a
  change of window sends `PointerLeave` to the old one and `PointerEnter`
  to the new.
* Buttons and scroll go to the window the pointer is **over**, not the
  focused one — focus follows the click, not the other way round. A left
  press raises that window (within the `Normal` layer only, so a click
  cannot pull a panel out from under a menu) and focuses it; a press that
  hits no window at all — bare desktop, or a `NO_FOCUS` window — leaves
  the focus where it was, because focus is only ever handed on.
* A press that lands on a window's **frame** — title bar, button, resize
  band — or anywhere at all with `Super` held is the *server's*, and the
  client hears nothing about it: it starts a move or resize drag, or arms
  a title-bar button. While a drag is in flight every motion drives the
  window and no pointer event reaches any client. See `docs/wm.md`.
* A touch sequence belongs to the window it started on: the finger may
  wander off while dragging and the client still owns the gesture until it
  lifts.

Every input event's timestamp is remembered, and handed to whichever
outputs are about to paint at the *next scene update* — which is the first
moment the damage it caused is visible to the server. That indirection
matters: a pointer move damages the cursor immediately, but a click or a
key does not. The pixels answering those are the client's, and they arrive
in a later wakeup as a commit. Stamping only what was already dirty would
quietly reduce the histogram to a cursor-motion histogram; carrying the
timestamp until a frame actually consumes it measures the thing the metric
is named after, click-to-photon included.

The carry is bounded to 200 ms. An input nothing ever responds to must not
sit waiting to be reported as a multi-second latency by some unrelated
frame later on — which is precisely what an earlier version did, once
reporting 33 seconds.

## Statistics

`stats` returns these keys, in this order. The paint and latency windows
are short on purpose: an average over an hour hides the stutter a test is
looking for.

| key                      | meaning                                                                 |
|--------------------------|-------------------------------------------------------------------------|
| `frames`                 | Page flips completed since startup.                                      |
| `flips_pending`          | Outputs with a flip in flight right now (0 when idle).                   |
| `uptime_ms`              | Milliseconds since `run()` started.                                      |
| `active`                 | 1 unless the session is paused by a VT switch.                           |
| `flip_interval_mean_us`  | Mean interval between flips, in microseconds.                            |
| `flip_interval_min_us`   | Shortest interval seen.                                                  |
| `flip_interval_max_us`   | Longest interval seen. Intervals longer than four refresh periods are **not counted**: an idle server deliberately stops flipping, and that gap is the absence of frames, not a slow one — counting it would measure how long the desktop sat still. |
| `flips_deferred`         | Cursor-only flips held back for a client's answer, counted once per episode (a burst of motion inside one frame period is one). See the frame path above. |
| `defer_timeouts`         | How many of those ended at the deadline instead of at a commit — the client did not answer. Tracking `flips_deferred` means a wedged client: the cursor is still reaching every vblank, but nothing is riding with it. |
| `paint_us_min`           | Rasterization time per frame, over the last 120 frames (two seconds at 60 Hz). Paint only — not the copy to the scanout buffer (`copy_us_*`) and not the commit. With the shadow on, a min of 0 is normal and correct: the age-2 carry frame has no new damage to draw, only pixels to copy. |
| `paint_us_mean`          | Mean of the same window.                                                 |
| `paint_us_max`           | Max of the same window — in practice a full-screen repaint.              |
| `copy_us_min`            | Microseconds spent streaming the shadow buffer's damage rects into the scanout buffer, same 120-frame window. Always 0 under `NITRO_SHADOW=0`, where there is no copy. Kept apart from `paint_us` because the two measure different hardware — cached heap versus the write-combined mapping — and respond to different changes. |
| `copy_us_mean`           | Mean of the same window.                                                 |
| `copy_us_max`            | Max of the same window.                                                  |
| `damage_px_mean`         | Mean damaged device pixels repainted per frame, same 120-frame window.   |
| `i2p_min_us`             | Input-to-photon latency, over the last 100 samples: from the libinput event timestamp to the vblank of the frame carrying its effect. |
| `i2p_mean_us`            | Mean of the same window.                                                 |
| `i2p_max_us`             | Max of the same window.                                                  |
| `fonts`                  | Font faces the startup scan indexed. 0 means no `TEXT` capability.        |
| `fonts_loaded`           | Font **files** whose bytes are resident right now — not faces indexed. The db loads a file on first use and releases it when the loop next goes idle, so a settled desktop reports 0 with its glyphs still on screen. |
| `font_bytes`             | Total size of those files. This is the number `docs/budget.md` cares about; it was the whole of the server's RSS overrun (#528). Capped by `NITRO_FONT_CACHE_MB` (default 8 MB). |
| `font_loads`             | Font files read from disk since startup — the miss counter. |
| `font_releases`          | Files handed back by the **idle sweep** since startup. `font_bytes` is an instant and cannot tell "the sweep is working" from "no face was ever loaded": both settle at 0. `font_releases` tracking `font_loads` is the sweep doing its job, and a `font_loads` far ahead of it is the leak the sweep exists to prevent (#538). |
| `font_evictions`         | Files dropped by the **cap** (`NITRO_FONT_CACHE_MB`), never by the sweep. Non-zero means one frame's working set genuinely overran the cap — a distinct and more alarming fact than an ordinary idle release. |
| `glyphs_cached`          | Distinct glyph masks in the atlas (font, glyph, quantized size, subpixel bucket). |
| `glyph_renders`          | Masks actually rasterized since startup. It stops rising once a UI's glyphs are all cached; a number that keeps climbing on a static screen means the cache key is churning. |
| `atlas_pages`            | 1024x1024 A8 pages allocated, 1 MiB each.                                |
| `atlas_bytes`            | What those pages cost the resident set: `atlas_pages × 1 MiB`. A page is allocated whole and never shrinks, so this is the real cost whatever fraction is packed. Reported rather than left as a multiplication for the reader of `docs/budget.md`. |
| `text_runs`              | Shaped runs held in the text store: one per text node with content. A `MeasureText` stores nothing, so it never moves this. |
| `shape_us_mean`          | Mean microseconds per shaping call, over the last 120 (shapes *and* measurements — they run the same layout). |
| `clients`                | Connected wire clients.                                                  |
| `windows`                | Windows in the scene.                                                    |
| `nodes`                  | Nodes in the scene, across every window — the server's own frame nodes included. A decorated window costs the server **six** of them (the frame group, the background, the title bar, the title text and two buttons; five on a `FIXED_SIZE` window, which has no maximize), pinned by `tests/wm.rs::a_frame_costs_six_scene_nodes_and_a_fixed_window_five`. Everything above that in the count is the client's own tree. |
| `outputs`                | Outputs currently connected.                                             |
| `shadow_bytes`           | Heap held by the shadow buffers, summed over the outputs: one scanout-sized `XRGB8888` buffer each (8 294 400 bytes at 1080p), 0 under `NITRO_SHADOW=0`. It is the server's one allocation proportional to pixels rather than to work, and `docs/budget.md` argues for it explicitly rather than leaving it to be inferred from `outputs`. |
| `decorated`              | Windows carrying a server-drawn frame. `windows - decorated` is how many opted out with `UNDECORATED`. |
| `minimized`              | Windows hidden by `Minimized`. They are still in `windows` and still in the `Alt+Tab` order. |
| `dragging`               | 1 while a move or resize drag is in flight. A drag that is still 1 with nothing on the desk is a stuck grab. |
| `focused`                | 1 when some window has keyboard focus. 0 with windows on screen means every one of them is `NO_FOCUS` or minimized — or that the focus was dropped and not handed on, which is a bug. |
| `shell_clients`          | Connections on the **privileged** shell socket. The first key to look at when a bar "is not working": zero means it never got there. |
| `hotkeys`                | Live `BindKey` bindings held by shell clients. |
| `exclusive_zones`        | Windows reserving screen space off an output edge. |
| `grabbed`                | 1 while a shell client holds a keyboard grab. A 1 with no launcher on screen is a stuck grab. |
| `config_reloads`         | Completed `server.conf` reloads since startup, whatever triggered them — the `reload` request, SIGHUP and the inotify watch all land in this one counter, because what a caller wants to know is "did the server pick my edit up", not which of the three doors it came through. A reload of a file that will not parse still counts: the file *was* re-read, and every line it could not use was warned about and skipped. |

The key naming is inconsistent on purpose — `paint_us_min` but
`i2p_min_us` — because that is what the protocol spec says, and the wire
format outranks tidiness. There is no histogram and no percentile:
percentiles over 120 samples are mostly noise, and a real latency
distribution needs a real sampling story, which is a later decision.

## VT-switch contract

On `SeatEvent::Disable`, in this order: **input is suspended first**
(libinput must have let go of its device fds before the ack, or the switch
hangs), then the backend is paused, then `ack_disable()` — libseat holds
the switch until that ack. On `Enable`: `resume()` the backend (one
blocking `ALLOW_MODESET` commit, which also clears any flip that was
abandoned), resume input, **reset the xkb state** (key releases that
happened on the other VT were never seen, so the modifier state is a
guess and dropping it is the only honest option), invalidate every output
and repaint fully. Three round trips on the test box with a client
connected: clean, and input is still routed to the client afterwards.

One known wart: each round trip leaks one descriptor per input device,
because `libseat_close_device` reports success without closing the fd it
handed out. The fix belongs in `nitro-seat`, which owns that value; issue
#525 tracks it. Bounded and slow — five fds per switch against a default
limit of 1024 — but real.

## Testing

- Unit tests per module: protocol parsing and replies, the `OutputState`
  age-2 rule and deadline maths, the statistics windows, control
  buffering, buffer validation, pointer clamping and hit-testing,
  keyboard resolution and hotkeys, the cursor bitmap, logging, signals
  (skipped when the sandbox blocks SIGTERM), and — in `wm.rs` — the
  whole of the window-management *policy*: frame hit regions, resize
  arithmetic against a client's limits, centred-cascade placement,
  tiling, the MRU order and the `Alt+Tab` cycle, double-click detection.
  Policy is pure functions on purpose, so it is testable without a
  server at all.
- `tests/fake_loop.rs` runs `run(Config::fake(..))` on a thread with a
  fake input source — the real event loop, no seat and no evdev node —
  and covers fourteen things:
  1. `outputs`, exact `shot` pixels against `render::background_color`,
     `shot` by name and the error for an unknown one, `err` for a bad
     request, that an idle server stops flipping entirely, and that `quit`
     stops the thread and removes both socket files.
  2. A client window: `Configure` with the size, scale and *content*
     position it got, `Presented` for its commit serial, the window's
     pixels where the window manager put it, and non-zero
     `paint_us`/`damage_px`. Placement policy itself — the first window
     centred, each later one a step down and right, all of them on screen
     — is asserted against `wm::place` rather than re-derived.
  3. Pointer motion: `PointerEnter` naming the rect node with
     window-local coordinates, a second move inside producing
     `PointerMotion` rather than another enter, `PointerLeave` on the way
     out, the software cursor visible in the screenshot, and a non-zero
     `i2p_max_us`.
  4. A left click on the lower of two overlapping windows: `Focus` and
     `PointerButton` to that client, and the overlap repainted in its
     colour because the click raised it.
  5. Disconnect: windows and nodes drop to zero and the area the client
     covered is repainted with the desktop underneath.
  6. A bad message (a node whose parent does not exist — something only
     the server's id map can catch) gets `Error` with the commit serial
     and `UnknownNode`, and the connection closes.
  7. A client buffer written to a memfd, uploaded with `CreateBuffer`,
     shown by an `Image` node, and checked pixel-for-pixel in a
     screenshot.
  8. Eight control clients sending partial lines, and two requests on one
     connection.
  9. The desktop frame still painted under everything.
  10. A client cycling 64 buffers (create, damage, destroy) leaks no
      descriptors, counted from `/proc/self/fd` — the server runs on a
      thread of the test process, so its leaks are the test's to see.
  11. A commit that changes no pixel is still `Presented`, four times in a
      row, so a client using the serial as flow control cannot stall.
  12. A `RequestFrame` from a settled, idle server is answered with a
      `Frame` carrying a usable deadline, without waiting for a flip that
      would never come.
  13. A window created before any output exists: its commit is still
      acknowledged with `Presented` (nothing it drew can reach a screen,
      so no frame will ever carry that serial), it gets no `Configure`
      while there is nowhere to put it, and it is placed and configured
      when an output is hotplugged in (`plug WxH`), appearing at the
      cascade's first position.
  14. No cursor is drawn until a pointer device reports something: a bare
      desktop is background everywhere, and the arrow appears on the first
      motion.
  15. Deferred flips, four ways: a client that answers a motion rides the
      *same* flip as the cursor (8 flips for 4 isolated answered motions,
      2 per move — 12 without the deferral, which is the bug); a client
      that never answers still gets a flip, with `defer_timeouts`
      climbing; cursor movement over the desktop is never deferred and
      still costs exactly 2 flips per move; and a settled server after a
      deferral makes no further frames, so the timer is really disarmed.
- `tests/shadow.rs` covers the #539 heap shadow buffer through the same
  real loop, five cases, all of them whole-buffer comparisons rather than
  spot checks — a shadow that drifts from what a full repaint would
  produce can drift anywhere: five successive partial damages leave the
  *scanout* buffer byte-identical to the same server forced to repaint
  everything; the age-2 carry still reaches the second buffer, checked
  through `shot-front` because an ordinary `shot` reads the shadow and so
  could not fail; `NITRO_SHADOW=0` and the default paint the same pixels
  (two servers, one process, compared pixel for pixel — which is what
  makes the A/B on the box a measurement of speed and nothing else), and
  `shot` agrees with the scanout buffer either way; `shadow_bytes` appears
  with an output, doubles with a second, and goes to 0 when the last is
  unplugged; and `copy_us` is non-zero with a shadow and exactly zero
  without one.

  On the fake backend the shadow buys nothing — that backend's buffers are
  heap already, so it is a pure extra `memcpy` per frame. It is left
  enabled by default there anyway, so the tests exercise the path that
  ships; the alternative is a test suite that never runs the production
  code.
- `tests/wm.rs` drives the M3 window manager through the same real loop:
  a decorated window's title bar painted above its content and an
  undecorated one with no frame at all; a title-bar drag moving the frame
  by exactly the drag delta while the damage stays proportional to the
  window; an edge drag resizing it and `Configure`-ing the client, and
  the client's own `SetWindowLimits` clamping both ends of it; the close
  and maximize buttons; `Super`-drag moving and resizing an *undecorated*
  window; `Alt+Tab` walking three windows in MRU order and holding its
  place across repeated Tabs; a minimized window gone from the screen and
  from the hit test but still reachable with `Alt+Tab`; `Super+Q`/`M`/
  arrows; a client asking for `Fullscreen` and being told what it got; a
  `FIXED_SIZE` window silently refusing to maximize; a second output via
  `plug`, a window dragged onto it changing `Configure.output`, and
  `unplug` migrating it back; and a scale-2 output drawing twice the
  device pixels for the same logical window.
- `tests/shell.rs` drives the M3-B shell socket through the same real loop,
  28 cases: the two sockets' capability bits and three shell clients at
  once; **every one of the eleven shell ops** refused with `Protocol` on the
  ordinary socket, one connection each and *without a commit* — the
  privilege check is on receipt (a check that covered ten would look
  exactly like a working one until someone found the eleventh); a bar
  creating, anchoring and reserving in **one transaction**, which is what
  the hardware probe caught the first implementation getting wrong; a 32-px
  top exclusive zone shortening a maximized window's `Configure` by exactly
  32
  and offsetting it by 32, released by `px: 0`, by minimizing the bar and by
  hiding it with `SetVisible(false)` — and taken back when it shows again,
  since a hidden zone is skipped rather than forgotten; a
  zone moving a newly *placed* window, asserted against `wm::place` on the
  shrunken area; a `Top` bar painted over a maximized window in a
  screenshot; `SetLayer{Normal}`, reserved anchor bits and a foreign
  `NodeId` each closing the connection with the right code; centred and
  margin-inset anchors; the window list over three windows following a
  retitle, a focus change, a state change and a close, with `WindowGone`
  and a retired ref; `FocusWindow`/`SetWindowStateFor`/`CloseWindow` on
  another client's window, and a stale ref being silently ignored rather
  than fatal; `Super+Return` firing `HotKey` twice and reaching the focused
  client *never*, while unbound `Super+A` still does; the bare-Super tap
  firing once and cancelled by another key and by a second modifier; unbind
  and disconnect both giving a chord back; a compositor chord refused; two
  clients contesting a chord; a grab routing keys to a `NO_FOCUS` overlay
  and back with no `Focus` event either way, released by hiding the window,
  and **not** outranking a bound chord — which arrives as a `HotKey` rather
  than a `Key`, while the bare-Super tap still fires under the grab, so a
  launcher can close itself the way it opened; `Super`-drag still moving a window with a shell connected and not
  looking like a tap; outputs listed, hotplugged and unplugged; an anchored
  bar re-spanning after a hotplug.
- `tests/config.rs` drives `server.conf` through the same real loop, with
  each harness owning a configuration directory of its own (the
  environment is process-global and these run in threads of one process,
  which is also why `Config::fake` leaves `config_path` at `None` — a test
  must never read the developer's own `~/.config/nitro/server.conf`).
  Thirteen cases: a startup `output.Virtual-1.scale = 2` reaching the client
  as `Configure.scale` *and* the `outputs` reply as `scale=2`; the whole
  precedence ladder, env > file > EDID, in both directions; a `reload`
  over the control socket applying `scale = 1`, incrementing
  `config_reloads` and actually repainting the output; the same edit
  picked up by the **inotify** path with no request at all — written the
  way a settings app writes it, temp file plus rename, which is the write
  the directory watch exists for — polled to a deadline because it is
  asynchronous; two outputs with explicit `position`s laying the desktop
  out against connector order (the second one placed to the *left*, so
  connector order could not produce the result) and a window dragged onto
  it; a scaled output's device and desktop origins agreeing, checked by
  driving the pointer in device pixels across an edge the desktop
  coordinates place elsewhere; `output.Virtual-2.primary = true` taking
  the orphans when the output they were on is unplugged; a `de` layout
  from the file turning evdev 21 into keysym `z` (and a reload from `us`
  to `de` doing it live), skipped with a message where the box has no
  `xkeyboard-config` data, the way the pixel tests skip on `has_fonts()`;
  and a file of pure garbage leaving the server running, answering and
  correctly configured, followed by a mostly-garbage file whose one good
  line still applies. Plus the idle claim the watch makes: an unrelated
  file written *into the watched directory* is drained and ignored, with
  the frame counter and `config_reloads` both still where they were —
  draining is what keeps the level-triggered fd from spinning, ignoring
  is what keeps an editor's swap file from reloading the desktop.
- `src/test_support.rs`, behind the **`test-support`** feature, is
  `tests/fake_loop.rs`'s harness factored out so another crate can use it:
  `TestServer::start` runs the real loop on a thread with a fake backend
  and a fake input source, and offers control requests, `stat`, `shot`,
  `settle`, `focus_window` and synthetic input. `nitro-ui`'s test harness
  is the consumer. It is a feature because it pulls a control-socket
  client and a thread into anything that links it, and no shipped binary
  wants either.
- Hardware: `just deploy`, `just shot`, `just box-chvt 1|2`, `just
  box-stop` (see `docs/testbox.md`), plus three clients:
  `hello_client`, which opens a gradient-and-rounded-rects window with an
  image node and prints every `ServerMsg` it receives; **`shell_probe`**,
  the M3-B probe for the privileged socket — a `Top` bar with a 32-px
  exclusive zone and a `TOP|LEFT|RIGHT` anchor, `Super+Return` and the
  bare-Super tap bound, and every `WindowInfo`/`OutputInfo`/`HotKey`
  printed as it arrives (`ssh box 'XDG_RUNTIME_DIR=/run/user/1000
  ~/nitro-bin/shell_probe'`); and **`nitro-demo`**
  (`crates/nitro-demo/README.md`), which is the measurement instrument:
  it follows the pointer, keeps its own input-to-photon histogram from
  `PointerMotion` to `Presented`, and cross-checks it against this
  server's `i2p_*` over the control socket. `ssh box
  'XDG_RUNTIME_DIR=/run/user/1000 ~/nitro-bin/nitro-demo --follow --stats'`.

## Measured on the test box

Pentium G3240, i915, 1920×1080@60, with `nitro-demo --follow` connected
and the pointer being driven at ~28 moves/s — the fastest pacing a 60 Hz
display can actually service, which is the rate the latency figures below
are taken at and why (see `docs/latency.md` §2 and §4). Sizes and RSS in
`docs/budget.md`.

| what                     | value                                                     |
|--------------------------|-----------------------------------------------------------|
| idle CPU                 | 0.0 %, zero frames in 5 s with a client connected and visible — including 5 s after a deferral, with `voluntary_ctxt_switches` flat |
| CPU under load           | 1.5 % animating at 60 Hz, 2.2 % under 100 Hz pointer input |
| RSS                      | server **8.2 MB** (8.3 MB with 5 windows), `nitro-demo` 3.1 MB. Was 19.7 MB: `FontDb` used to hold every indexed face's bytes, which is the whole of the +12.2 MB over the pre-M2 7.5 MB. Lazy loading (issue #528) gives it back — 0 font bytes resident in the settled state, 8.8 MB with a text-heavy client on screen and a 10.6 MB `VmHWM` peak while it paints |
| flip interval            | mean 16 666 µs, min 16 654, max 16 675                    |
| `paint_us`               | min 142, mean 201, max 323 per pointer-move frame (13 459 on a full repaint) |
| `damage_px_mean`         | 1 560 of 2 073 600 in steady state                        |
| server input-to-photon   | min 1 287 µs, mean 9 459 µs, max 17 788 µs                 |
| client input-to-photon   | **median 9 290 µs, p95 17 134 µs, min 1 287 µs** over 202 samples — inside one refresh, so the `DESIGN.md` budget is met (was median 25 132, min 17 502 before the deferred-flip fix) |
| cursor over the desktop  | 2 flips per isolated move, unchanged, and nothing deferred |
| wedged client (SIGSTOP)  | cursor still 60.0 flips/s; `defer_timeouts` climbs         |
| VT switches              | 3 round trips with a client connected: clean, input still routed |

### M3-A: window management

Measured on the same box against `nitro-calc` and `hello_dialog`, both
server-decorated, driven with `ydotool`.

| what | value |
|---|---|
| idle CPU, 6 decorated windows | **0 frames and 0 CPU ticks in 5 s** — decoration costs the idle case nothing, because a frame that does not change contributes no damage |
| RSS, 5 `nitro-calc` windows | **11 336 kB**, against **10 604 kB** for the same test on `main`: window management costs **+732 kB**. Both are over the 8.5 MB line, which `main` was already missing; see below |
| title-bar drag, 40 motions | `dragging 1` throughout, `damage_px_mean` **125 785** against a 225 × 363 frame (81 675 px), i.e. **1.5× the window** and 6 % of the 2 073 600-pixel screen |
| drag input-to-photon | min 6 518 µs, **mean 14 533 µs**, max 22 568 µs, `paint_us_mean` 4 441 |
| `Super+M`, `Super+H`, `Alt+Tab` | all act on the focused window; `Alt+Tab` after `Super+H` brings the minimized window back |
| focus styling | focused and unfocused frames measurably differ in the screenshot (bar and border colours) |
| VGA-1 | **disconnected on the box**, so the two-monitor case is covered by the fake backend's `plug`/`unplug` in `tests/wm.rs`, not on real hardware |

Three of those need reading carefully.

**The drag damage is the whole point of the scene's damage contract.** A
move damages *old ∪ new bounds*, so dragging a window across a 1080p
desktop costs about twice its own area per frame — measured at 1.5×,
because consecutive motions inside one frame period coalesce. A
compositor that repainted the screen per motion would report 2 073 600.

**Drag latency is above the 9.3 ms pointer figure, and honestly so.** A
resize sends a `Configure` per motion and the client answers it, so the
number spans a full round trip including the toolkit's relayout; a move,
which sends nothing at all, is the fast path. 14.5 ms is still inside one
refresh at 60 Hz, which is the budget.

**RSS is over, and was already over.** The A/B above is the useful number:
window management adds 732 kB (the frame nodes, their shaped titles and
the atlas pages those pull in), on top of a baseline that had already
drifted from the 8 240 kB `docs/budget.md` records. Attributing the rest
is a budget task, not this one.

Two more things need reading carefully.

**Latency is only measurable below the display's own rate.** Above about
30 moves/s the demand for flips (two per move, the age-2 cursor cost)
exceeds the 60 Hz the hardware can retire, and the server pins at exactly
60.0 flips/s. Both this build and the one before the deferred-flip fix do
— measured, 60.0 vs 59.9 flips/s at 65 moves/s — so at that pacing the
number reports queue depth, not input-to-photon, and both builds report
~25 ms for that reason. `docs/latency.md` §4 has the rate sweep.

**The two views agree now, which they did not before.** The server's
figure ends at the vblank of the frame that consumed the input; the
client's spans the whole round trip. They used to differ by a whole frame
because a pointer move damaged the cursor and flipped before the client
had answered (issue #529). With that flip deferred, both land at ~9.3 ms
and the gap between them is the client's own reaction time, which is what
it should have been measuring all along.

### M3-B: the shell socket

Measured on the same box with `examples/shell_probe` (a `Top` bar, a 32-px
top exclusive zone, a `TOP|LEFT|RIGHT` anchor, `Super+Return` and the
bare-Super tap bound) and two `nitro-calc` windows, driven with `ydotool`.

| what | value |
|---|---|
| sockets | `wire.sock`, `shell.sock` and `control.sock` in one `0700` directory; the probe's `Welcome` is `caps=0x32` (`TEXT|WM|SHELL`) and an ordinary client's is `0x12` |
| anchor | asked for 400 px wide, `Configure`d to **1920×32 at (0,0)** and re-`Configure`d after answering — a bar that ignores that `Configure` paints its original width, which is how the probe found the bug below |
| exclusive zone | a maximized `nitro-calc`'s own title bar starts at **y=33**, with the bar owning y=0..31; the strip is **given back** when the probe is killed (title bar back at y=0..27) |
| layers | the bar's pixels win over the maximized window's throughout its strip, which is the `Top`-over-`Normal` ordering |
| hotkeys | `Super+Return` → `HotKey id=1` press *and* release; bare Super tap → one `HotKey id=2 pressed=false`; **`Super+M` still maximizes** (`WindowInfo state=Maximized`) rather than reaching the shell |
| window list | live: a second `nitro-calc` produced a new `WindowInfo`, and the focus change produced one for *each* window |
| outputs | `OutputInfo id=1 1920x1080@60.000Hz scale=1 at 0,0 name="HDMI-A-1"` |
| disconnect | `pkill shell_probe` → `shell_clients 0`, `hotkeys 0`, `exclusive_zones 0`; nothing leaks |
| idle CPU, bar + 2 windows | **0 frames, 0 CPU ticks and +2 voluntary context switches in 5 s**, `top` 0.0 % |
| RSS | **10 992 kB** with the bar and two `nitro-calc` windows (`VmHWM` 24 908 kB), against the 11 336 kB M3-A measured for five `nitro-calc` windows — the shell state is a handful of hash maps |

Two of those need reading carefully.

**A zone costs the idle case nothing, and that is not an accident.** The
work-area subtraction happens only when something *asks* for the work area
(a maximize, a placement), and a zone change reflows only `Maximized`
windows. So the bar's strip is not a per-frame cost, and the 0-frames-in-5s
result holds with it up.

**The probe earned its keep twice.** First, the four window-targeting shell
ops were originally answered on receipt, and the probe sent the transaction
a real bar sends — `CreateWindow`, `SetAnchor`, `SetExclusiveZone` in one
commit — and got `UnknownNode` for a window `Commit` had not created yet.
They are buffered now (`docs/shell.md`), with
`a_bar_can_create_anchor_and_reserve_in_one_transaction` as the regression
test. Second, the probe itself did not answer the `Configure` its anchor
produced, so the bar painted 400 of its 1920 px and the desktop showed
through the rest — a client-side bug, but exactly the one a real bar makes,
so the probe now answers it and says why.

### #539: the heap shadow buffer

Measured on the same box, same binary throughout, `NITRO_SHADOW=0`
against the default through a systemd drop-in; #3693's protocol (30
`hello_client` cycles filling the 120-frame window), six interleaved pairs
with the order flipped for pairs 4–6, every run valid at `frames=122 ∧
damage_px_mean=275418`.

| what | `NITRO_SHADOW=0` | shadow (default) |
|---|---|---|
| `paint_us_mean` | 6 233 µs | **418 µs** |
| `paint_us_min` / `_max` | 119 / 15 815 µs | 0 / 4 349 µs |
| `copy_us_mean` | 0 (no copy) | 251 µs |
| **paint + copy** | **6 233 µs** | **669 µs — 9.3×** |
| `nitro-calc` keypress i2p, mean | 12.0 / 13.9 / 13.5 ms | 12.9 / 12.4 / 13.1 ms |
|          max | 36.7 / 39.2 / 22.2 ms | 21.4 / 22.5 / 22.4 ms |
| idle, 2 decorated windows | 0 frames, 0 CPU ticks in 5 s | 0 frames, 0 CPU ticks in 5 s |
| RSS, no client | 7 800 / 7 688 kB | 15 888 / 15 844 kB |
| `shadow_bytes` | 0 | 8 294 400 |
| `cargo bench -p nitro-raster` | unchanged (zero diff in that crate) | |

The paired difference on `paint_us` is 5 815 µs with a standard deviation
of 73 µs over the six pairs (t(5) = 196) — a result that needs reporting
rather than statistics. Three of those rows need reading carefully.

**`paint_us_min` of 0 is correct, not a broken counter.** With the shadow
the age-2 carry frame rasterizes *nothing*: it has no new damage, the
shadow already holds the previous frame's, and all that is left is the
copy. Every such frame used to repaint the union.

**The i2p median did not move, and should not have.** Latency is set by
which vblank a frame catches, not by how much of the interval it uses:
6.2 ms of paint already fitted inside a 16.7 ms refresh. What moved is the
tail — two of the three `NITRO_SHADOW=0` runs show a max near 37–39 ms, a
missed frame, and none of the shadow runs does. #539 buys **margin, not
median**, and margin is what keeps the median inside budget as scenes get
more expensive.

**The 8 MB is real and is in RSS**, unlike the dumb buffers, which the GPU
owns. It roughly doubles the server's resident set on this box, it is
exactly `1920 × 1080 × 4` per output, and it does not move with the number
of windows (the two-window delta is 2.6 MB either way). `docs/budget.md`
argues the trade rather than hiding it.
