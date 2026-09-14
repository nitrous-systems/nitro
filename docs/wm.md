# Window management

How the server places, decorates, moves, focuses and stacks top-level
windows. M3-A. The implementation is `crates/nitro-server/src/wm.rs`
(policy, all of it unit-testable without a server) plus the window half of
`crates/nitro-scene` (the frame group, the state, the limits); the wire
side is the `WM` capability bit in `docs/wire.md`.

Two decisions shape everything below, and both were made deliberately:

* **Decorations are server-side, and opt-out.** The server draws the title
  bar, the border and the buttons, as scene nodes it owns. A client that
  wants its own passes `UNDECORATED` and gets a bare group.
* **The server moves and resizes windows with zero client round-trips.** A
  drag is one scene mutation per motion event plus one *one-way*
  `Configure` telling the client where it now is; nothing is ever asked of
  the client and nothing is waited for. The frame scheduler already
  throttles those `Configure`s to one per frame.

## Frame groups

A window is a *content* group owned by its client. When the window is
decorated the server wraps that group in a **frame group** it owns under
`ClientId::SERVER`:

```
frame group  (server)          ← Window::root(),  the whole window
├── background rect (server)   ← border colour, rounded top corners
├── title bar rect  (server)
├── title text      (server)   ← elided with "…"
├── close button    (server)
├── maximize button (server)   ← absent when FIXED_SIZE
└── content group   (client)   ← Window::content(), offset by the insets
```

The client's `NodeId` names the **content** group, before and after
framing: `Scene::frame_window` mints a new root *above* the existing node
and leaves that node's key alone, so every `CreateNode { parent: window }`
still lands inside the client's own group and a client can never create a
node on top of its own title bar. The decorations are the content group's
*siblings*, inserted before it, so the client always paints over its own
frame's background and never over the bar.

Geometry, in logical units:

| | |
|---|---|
| title bar height | 28 |
| border | 1, left/right/bottom |
| top corner radius | 6 |
| button | 14 × 14, 8 apart, close rightmost |
| resize grab band | 6, **outside** the frame edge only |

The insets are `(1, 28, 1, 1)`. `Window::size()` and every `Configure` are
the **content's** size; `Window::frame_size()` adds the insets.
`Configure.position` is the content's origin, so a client holding a
screenshot of the whole output can still crop it to exactly itself.

### Why the resize band only reaches outwards

The obvious implementation grabs six pixels either side of the frame edge.
It is also wrong: with a 1-px border that steals the outermost six pixels
of the *client's* content, and a button flush against the window edge
becomes unclickable. So the band reaches inwards only as far as the
frame's own border (or the title bar, at the top) and outwards the full
six pixels. Six pixels of slop outside the window is what makes a 1-px
border grabbable, and it costs the client nothing, because those pixels
are not its.

### Hit regions

Front to back through the z-order, skipping minimized windows:

| region | pointer action |
|---|---|
| title bar | press-drag moves; double click toggles maximize |
| close button | `Closed` to the client on release *inside the button* |
| maximize button | toggle maximize on release inside the button |
| edge / corner band | press-drag resizes those edges |
| content | the client's, routed as `PointerButton` / `PointerMotion` |

A button fires on **release inside itself**, which is what lets a user
change their mind by sliding off it before letting go.

Cursor *shapes* are M4: the arrow does not change over a resize band in
M3.

## Interaction

### Pointer

* Click anywhere in a window raises it (within the `Normal` layer only —
  a click must not pull a panel out from under a menu) and focuses it.
* A click on nothing changes nothing: a press that hit-tests to no window
  — bare desktop, or a `NO_FOCUS` one that cannot take the keyboard —
  leaves the focus and the MRU exactly where they were. Focus is only ever
  handed on, never dropped on the floor.
* `Super` + left-drag moves, `Super` + right-drag resizes from the nearest
  corner — on any window, decorated or not. That is the whole of what an
  undecorated window loses by opting out.
* A drag in flight owns every motion: the window follows the pointer and
  the client is never consulted. Both a move and a resize send one
  one-way `Configure` per motion — a move because `Configure.position` is
  what a client crops a screenshot with, so a pure move still changes what
  it must be told. Neither blocks on a reply.
* Dragging a maximized window by its title bar restores it first, under
  the cursor, so the restore rectangle is not silently discarded.

### Keyboard

Server-global, and deliberately confined to three chord families nothing
else can reasonably claim: `Ctrl+Alt` (the console escape hatches every
Linux user already knows), `Alt+Tab` (which no application may have,
because it is how you leave one) and `Super` (reserved for the desktop by
convention).

| chord | action |
|---|---|
| `Ctrl+Alt+Backspace` | quit the server (development safety valve) |
| `Ctrl+Alt+F1`…`F12` | switch VT |
| `Alt+Tab` / `Alt+Shift+Tab` | cycle focus in MRU order |
| `Super+Q` | close the focused window |
| `Super+M` | toggle maximize |
| `Super+F` | toggle fullscreen |
| `Super+H` | minimize |
| `Super+←` / `Super+→` | tile to that half of the work area |

`Super+Enter` was **reserved** here in M3-A for "the launcher", and is no
longer a compositor chord: M3-B's shell socket lets the launcher claim it
with `BindKey`, and a chord this table still claimed could never reach the
shell. A shell client's bindings sit between this table and the focused
client — see `docs/shell.md`.

`Ctrl+Super+…` is deliberately *not* a window-management chord: an
application may reasonably want it, and the `Super` table must not swallow
it.

### Focus and MRU

Click-to-focus; focus follows the raise. The focused window gets
`Focus { true }` and the previous one `Focus { false }`; keyboard input
goes to the focused window and nowhere else. A `NO_FOCUS` window never
takes focus (this is what the launcher and the bar are for), and neither
does a minimized one.

Focus moves on its own in exactly three places, all of them "the keyboard
must not be dropped on the floor": a new window on the `Normal` layer
takes it, minimizing the focused window hands it to the next focusable
entry in the MRU list, and closing the focused window does the same.

The **MRU list** is every window, most-recently-used first, minimized ones
included — that is precisely what lets `Alt+Tab` bring a minimized window
back. A new window joins at the *back*: it exists but has never been used,
so `Alt+Tab` reaches it last.

`Alt+Tab` is a *cycle*, not a step. While `Alt` is held the focus moves
but the MRU list is **not** reordered, so `Alt+Tab+Tab` reaches the third
window rather than bouncing between two; the list is reordered and the
window raised when `Alt` comes up. The first press of a cycle lands on the
second entry, which is what makes a single `Alt+Tab` a toggle between the
last two windows.

## States

`Normal`, `Maximized`, `Fullscreen`, `Minimized` — set by the client with
`SetWindowState`, by the user with a shortcut or a button, and reported
back with a `WindowState` event whenever it actually changed.

| state | geometry | decorations |
|---|---|---|
| `Normal` | the remembered rectangle | shown |
| `Maximized` | the output's work area | shown |
| `Fullscreen` | the whole output | hidden |
| `Minimized` | unchanged | — (the window is hidden) |

Entering `Maximized` or `Fullscreen` from `Normal` remembers the frame
position and content size; leaving puts them back, clamped into the work
area in case the output changed underneath. `Minimized` does not move
anything, so un-minimizing lands where the window was.

Fullscreen **hides** the decorations rather than destroying them: the
frame group stays, its insets go to zero, and leaving fullscreen puts them
back. Adding or removing a frame for real would restructure the tree under
a live client.

A `FIXED_SIZE` window silently refuses `Maximized` and `Fullscreen`, and
gets neither resize bands nor a maximize button. Silently, because the
protocol has no per-request error: every error is fatal, and killing a
connection over "you cannot maximize this" would be absurd.

`Minimized` is not a soft close: the window keeps its geometry, its place
in the z-order and its place in the focus-cycling order. Only `Closed`
ends a window — and the server does not tear a window down itself when the
user presses the close button. It sends `Closed` and the window goes when
the client destroys its root, because a `Closed` the client has not acted
on is a chance to save.

### Limits

`SetWindowLimits { min, max }` bounds the **content** size. A zero
component means "no limit" on that axis; a `max` below `min` is clamped up
rather than refused. Every server-initiated resize goes through the clamp,
and a drag additionally cannot go below a 64 × 32 content floor, so a
client that declares nothing still cannot be resized into nothing.

## Work area and placement

The **work area** is a per-output rectangle in logical units: everything a
maximized or newly placed window may use. `wm::work_area` gives the
output's own logical rectangle; since M3-B the shell's **exclusive zones**
are subtracted from it, in exactly one place — `Server::local_work_area`,
which every window-manager call site goes through. `wm::work_area` stays a
pure function of the scene, and `docs/shell.md` has the zone rules (they
add per edge, and a hidden or dead bar's zone is released).

New windows are placed **centred-cascade**: the first window's frame is
centred in the work area, each later one steps 28 px down and right, and
the walk starts over once it would leave the area. Every result is clamped
inside the work area and rounded to whole logical pixels — a window at a
half pixel puts every edge and glyph in it between device pixels, which is
both blurrier and more expensive to rasterize than it is worth.

The cascade length adapts: it walks only as many steps as actually fit
between the centre and the bottom-right corner, because past that point
every window clamps to the same place and a longer walk would just pile
them up against the edge.

## Multi-output

Outputs are laid out **left to right in connector order** unless the user
says otherwise. A row is the arrangement that needs no policy, and
connector order is the only ordering the kernel offers, so it is the
default; since M4-C `output.<connector>.position` in `server.conf`
overrides it per connector, and an output the file does not mention is
placed after the last positioned one, in connector order. See
`docs/settings.md`.

A configured position moves the output in **both** spaces — the desktop
(logical) layout windows are placed in, and the device-pixel rectangle
the pointer is clamped to and hit-tested against. Half-doing it would be
worse than not doing it at all: with the desktop layout following the
file while the device layout stayed in connector order, the pointer would
cross between screens somewhere other than a dragged window does. Both
are computed in one pass so they cannot diverge.

* The pointer moves across freely: it is clamped to the *union* of the
  outputs, not to one of them.
* A window belongs to the output containing its **centre**. That is the
  rule a user can predict: a window is on the screen it mostly is on, and
  dragging it more than halfway across hands it over. `Configure.output`
  updates when it does.
* A dragged window may leave its own output. The clamp during a drag is to
  the union, with at least a title bar's width kept on screen, so a window
  can always be grabbed again.

### Scale

Per output, and a **step function**: 1× normally, 2× when the EDID
physical size works out to 192 dpi or more. Fractional scaling is not
offered, because every rectangle in the tree would land between device
pixels and the whole damage contract is built on exact device rects.

Two things override it, in this order: `NITRO_SCALE=<connector>=<f32>,…`
(`NITRO_SCALE=HDMI-A-1=2`) and then `output.<connector>.scale` in
`server.conf`. The environment stays on top because it is the
*development* channel — a `NITRO_SCALE=… just fake` must not be silently
overridden by whatever the box's own config says — and the file beats the
EDID because it is the user's explicit answer to the EDID's guess.

The file is what M3 deferred and M4-C delivered: the persistent output
layout is no longer an environment variable that is obviously temporary.
`NITRO_SCALE` remains, demoted from stop-gap to dev override.

A window's logical geometry does not change with the scale; the output
scale lives in the window root's transform, so the rasterizer never needs
to know about it and a 2× output simply gets twice the device pixels for
the same logical rectangle.

### Hotplug

* **A new output** takes the position `server.conf` gives it, or is
  appended to the right of the row when the file does not name it.
* **A removed output** orphans its windows — the scene unplaces them — so
  they are **migrated onto the primary output** (`output.<c>.primary`,
  else the first connector), clamped into its work area, and
  re-`Configure`d. A maximized or fullscreen window has its
  geometry re-derived for the new output. Leaving them unplaced would be
  much worse than it sounds: an unplaced window is in no z-order at all,
  so no click and no `Alt+Tab` could ever get it back.
* The pointer is re-clamped to whatever outputs remain.

Driven in tests through the fake backend's `simulate_plug` / `unplug`,
which queue an `Event::Hotplug` *and* make the poll fd readable, so an
idle server actually wakes for it.

### Input-device hotplug

Deferred from M1, and done here with the **same kernel uevent socket** the
DRM backend already uses, on the `input` subsystem. libinput's *path*
backend has no idea devices come and go — that is what its udev backend is
for, and udev is a dependency this tree deliberately does not have — so
the server rescans `/dev/input` itself on an `add`/`remove` uevent and
tells libinput which paths appeared and disappeared. A new device's fd
joins the epoll set; a keyboard going away resets the xkb state, because
it may have been holding a modifier whose release will never arrive.

Failure to open the socket is not fatal: a sandbox with no netlink loses
hotplug, not the keyboard it already has.

## Damage

Unchanged, and the reason a drag is cheap: moving a window damages **old ∪
new bounds** and nothing else, so dragging a small window across a 1080p
desktop costs about twice the window's area per frame, not a full-screen
repaint. The frame group's own nodes ride the same rule — they are
ordinary scene nodes, so a title bar that did not change contributes no
damage, and a focus change repaints the bar and the border and nothing
else.

`Minimized` is `visible = false` on the frame group, which is one
`INHERIT` mutation and damages exactly the rectangle the window covered.

## Measured

On the test box (Pentium G3240, i915, HDMI-A-1 1920x1080@60), against
`nitro-calc` and `hello_dialog`, both decorated, driven with `ydotool`.
The `nitro-server` README has the full table; the three numbers that
matter to this document:

* **Idle stays zero.** Six decorated windows, 0 frames and 0 CPU ticks
  over 5 s. Decoration costs the idle case nothing, because a title bar
  that did not change contributes no damage.
* **A drag damages the window, not the screen.** 40 motions dragging a
  225 x 363 frame: `damage_px_mean` 125 785, which is 1.5x the window's
  own 81 675 pixels (consecutive motions inside one frame period
  coalesce) and 6 % of the 2 073 600-pixel screen.
* **Drag input-to-photon is 14.5 ms mean**, inside one 16.7 ms refresh.

RSS with five windows is 11 336 kB against 10 604 kB for the same test on
`main`: window management costs **+732 kB** — the frame nodes, their
shaped titles, and the atlas pages those pull in.

The box has VGA-1 **disconnected**, so the multi-output paths are covered
by the fake backend's `plug`/`unplug` in `crates/nitro-server/tests/wm.rs`
rather than on real hardware.

## What is deferred

* **Workspaces / virtual desktops.** Not in M3 at all. The MRU list and
  the z-order are per *output*, and nothing in the model assumes there is
  only ever one set of them, so this is an addition rather than a rework.
* **Cursor shapes.** The arrow stays an arrow over a resize band and a
  title bar. M4, together with the cursor theme.
* **Rotation.** Position, scale and the primary flag per connector are
  persistent since M4-C (`server.conf`, `docs/settings.md`); rotation is
  not, because nothing in the scene applies one yet.
* **Drag-arranging the monitor layout.** Positions are persistent and
  editable, but they are *typed* — in `nitro-settings` or in the file.
  Dragging a monitor rectangle into place is not in M4.
* **Window snapping / edge tiling by drag.** `Super+←`/`→` tile; dragging
  a window to a screen edge does not.
* **Per-window opacity and shadows.** The scene supports opacity; the
  frame does not use it.
