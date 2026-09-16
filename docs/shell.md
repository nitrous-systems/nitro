# The shell socket

How the bar, the launcher and the wallpaper get to do things an ordinary
client may not. M3-B. The implementation is
`crates/nitro-server/src/shell.rs` (policy: zones, anchors, the hotkey
table and the tap state machine, all unit-testable without a server) plus
the shell arms of `crates/nitro-server/src/lib.rs`; the wire side is the
"Shell (caps `SHELL`)" section of `docs/wire.md`, and the window model it
builds on is `docs/wm.md`.

One decision shapes everything below:

> **The socket is the capability.** A client is privileged because of
> *where it connected*, not because of anything it sent, asked for or was
> configured with.

## The privilege model

The server listens on two `nitro-wire` sockets:

| socket | default path | `Welcome` caps |
|---|---|---|
| wire | `$XDG_RUNTIME_DIR/nitro/wire.sock` (`NITRO_SOCKET`) | `WM`, `TEXT` if there are fonts |
| shell | `$XDG_RUNTIME_DIR/nitro/shell.sock` (`NITRO_SHELL_SOCKET`) | the same, **plus `SHELL`** |

Same framing, same handshake, same decoder, same `ClientStream`, same
event-loop arm. The `Listener::bind_shell_default` next to
`bind_default` in `nitro-wire` is deliberately a one-line difference, and
`Server::on_shell_accept` is deliberately `on_wire_accept` with a different
token base: a second, subtly different transport is how a privileged path
acquires a bug the unprivileged one does not have.

The whole check is one `if` in `Server::handle_shell_msg`, against the
token range, before any op is looked at:

```rust
if !Self::is_shell(token) {
    self.disconnect(token, Some((0, ErrorCode::Protocol, …)));
    return false;
}
```

Three things follow from putting it there.

**The token range *is* the fact.** Shell clients are allocated tokens from
`TOK_SHELL_BASE`, so "is this client privileged?" is answered by arithmetic
on a number the epoll loop already has. A per-client boolean would be a
second copy of the same fact, and two copies of a security-relevant fact
drift.

**One check, not eleven.** Spreading the test over eleven message handlers
is how a capability check gets forgotten in the twelfth. `is_shell_op` is a
`match` over the shell variants rather than an op-code range test, so
adding an op to the `0x_4xx` block without classifying it stops compiling —
which is exactly when a privilege check must not keep compiling.

**Refusal is fatal.** An unprivileged client that sends a shell op gets
`Error { Protocol }` and the connection closes, like every other error in
this protocol. A client that asked for something the protocol says it may
not have has misunderstood its own situation, and everything it sends
afterwards is guesswork. `tests/shell.rs` asserts this for **every one of
the eleven ops**, one connection each, because a check that covered ten of
them would look exactly like a working one until someone found the
eleventh. It also asserts it *without a commit*, which matters for the four
buffered ops below: the check is on receipt, so a client that never commits
is still disconnected rather than sitting there having spoken an op it may
not use.

### Buffered or immediate

The shell ops do **not** all take effect at the same moment, and the split
follows what each one is:

| ops | when | why |
|---|---|---|
| `SetLayer`, `SetExclusiveZone`, `SetAnchor`, `GrabKeyboard` | at the sender's `Commit` | they name the sender's **own** window, and a bar sends `CreateWindow` and `SetAnchor` in one transaction |
| `BindKey`, `UnbindKey`, `WindowList`, `Outputs`, `FocusWindow`, `CloseWindow`, `SetWindowStateFor` | on receipt | questions and registrations, or ops on *another* client's window — none of which the sender's commit has anything to do with |

The first row is the hardware probe's finding, and worth recording because
the first implementation got it wrong in a way no unit test caught: the
four ops were answered on receipt, `shell_probe` sent the obvious
transaction, and `SetAnchor` got `UnknownNode` for a window `Commit` had
not created yet. `clients::apply_msg` now resolves the window (so a bad
`NodeId` or a reserved bit still aborts the batch, fatally) and pushes a
`shell::WindowOp` into `ApplyOutcome`; `Server::apply_shell_op` applies it
at the commit. `a_bar_can_create_anchor_and_reserve_in_one_transaction` is
the regression test.

Within a commit the four run after every ordinary mutation and *before* the
batch's state requests. An anchor decides a window's whole rectangle, so it
has to win over the client's own `SetBounds` in that batch — and a
`Maximized` asked for in the same batch has to win over the anchor, which is
the shell deliberately handing its window to the window manager.

### What this model is worth, and what it is not

Both sockets sit in the same `0700` directory. So the grant is precisely
**"a process running as this user"** — no finer. A malicious program the
user runs can open `shell.sock` and take the bar's hotkeys.

That is a real limit, and it is the right one for M3 for two reasons.
First, the alternative that actually helps needs an identity for the peer
that the peer cannot forge, and on a Unix socket the only such identity is
`SO_PEERCRED`'s pid/uid/gid — a pid is racy and a uid is what we already
have. Second, and more to the point: on a single-user desktop a program
running as the user can already read `~/.ssh`, `LD_PRELOAD` the browser and
write to `~/.config/autostart`. A capability check that stops it from
drawing a bar while leaving all of that open buys the appearance of
security, not security.

The honest claim is therefore narrow and true: **`SHELL` keeps the ops out
of the reach of ordinary applications, which connect to the wire socket
because that is the path `Connection::connect_default` resolves.** A
browser cannot accidentally — or as part of a compromise of the renderer
sandbox, which does *not* get the runtime directory — start reserving screen
space or reading every keystroke. That is worth having on its own.

What a stronger model would need is in [Deferred](#deferred).

## Exclusive zones

A shell window reserves space along an output edge with
`SetExclusiveZone { window, edge, px }`. The reservation comes off that
output's **work area**, which `docs/wm.md` defines as what `Maximized`
fills and what new windows are placed into. A 32-px top zone makes every
maximized window 32 px shorter and starts it 32 px lower.

The subtraction happens in exactly one place, `Server::local_work_area`:

```
wm::work_area(scene, output)      ← the output's logical rectangle
  → Zones::work_area(area, …)     ← minus every zone held on that output
```

`wm::work_area` stays a pure function of the scene, and every window-manager
call site goes through `local_work_area` or `desktop_area`. Putting the
subtraction in `wm::work_area` would have made a pure geometry function
need the shell's state and the scene's window table; putting it at each
call site would have let one forget.

**Zones add, per edge.** Two bars docked to the top each get their own
strip. Addition is the only rule that composes: `max` would silently make
the second bar's reservation depend on the first's, and a shell cannot see
the other shell's zone to work around it.

**A zone is released by more than `px: 0`.** Also whenever the window stops
**showing** — hidden with `SetVisible(false)`, `Minimized`, closed, or its
client gone. This matters more than it looks: a panel that hides itself on
a keystroke must hand its strip back, and requiring it to send
`SetExclusiveZone { px: 0 }` first means one forgotten message leaves the
desktop permanently short — and a *crashed* bar leaves it short with no
message coming at all.

"Showing" is one predicate, `Server::showing`: alive, not `Minimized`, and
its content subtree visible. Both this and the keyboard grab hang on it,
and that is deliberate — they used to test it *differently* (the zone
checked only `Minimized`, the grab checked node visibility too), which is
exactly how two copies of one rule drift. A hidden bar's zone is skipped
when the work area is computed rather than deleted, so unhiding restores it
without the shell re-sending anything; `forget_window` is the permanent
drop, for a window that is gone.

Both halves need a reflow, and they arrive differently: a zone change goes
through `apply_shell_op`, a `Minimized` through `set_state`, and a
`SetVisible` through `ApplyOutcome::visibility_changed` — which exists only
because `clients.rs` cannot know whether the window it just hid holds a
zone.

**The reflow is immediate and proportional.** When a zone changes,
`reflow_work_area` re-applies the geometry of every `Maximized` window —
and only those. A floating window is where the user put it, and a
fullscreen one covers the output regardless. So the cost of a zone change
is the number of maximized windows, not the number of windows, and a
desktop with no shell running pays one `Zones::is_empty` per state change.

**Zones are keyed by window, not by output.** A bar that moves between
outputs takes its reservation with it; the output is looked up from the
scene at the moment the work area is computed. Keying by output would
leave a stale strip reserved on the screen a bar used to be on.

## Anchors

`SetAnchor { window, edges, margin }` sticks a window to its output's
edges. Opposite edges together mean "span that axis" — so the window is
**resized** — and neither means "centre on it", rounded to a whole logical
pixel so a bar's text does not land between pixels.

| shell surface | `edges` |
|---|---|
| bar | `TOP\|LEFT\|RIGHT` |
| dock | `BOTTOM\|LEFT\|RIGHT` |
| sidebar | `LEFT\|TOP\|BOTTOM` |
| launcher (centred overlay) | `0` |

**Anchors are against the output's full rectangle, not its work area.** Two
bugs fall out of the other choice: a bar anchored into the work area is
pushed off the screen by its own exclusive zone, and a launcher centred in
the work area jumps every time a panel appears or hides. A shell surface
is *outside* the work-area contract — it is what shapes it.

`Server::apply_anchor` sets the frame rectangle and the client gets a
`Configure`, like any other geometry the server decided. Anchors are
re-applied from `sync_outputs`, so a bar keeps spanning across a mode
change, a scale change and a hotplug — unconditionally, because
`set_frame_rect` is idempotent and an anchor that silently stopped holding
is the harder bug to notice.

## Hotkeys

`BindKey { id, mods, keysym }` claims a server-global chord. While bound it
is **not** delivered to the focused client: a global hotkey the focused
application could also see would be a keylogger and an ambiguity at once.
It arrives as `HotKey { id, pressed, time_ns }` — on the press and again
on the release, so a shell can implement press-and-hold.

Three priorities, in this order, in `Server::key`:

1. **The compositor's own chords.** `Ctrl+Alt+*`, `Alt+Tab` and the
   `Super` window-management table (`docs/wm.md`). Not bindable: `BindKey`
   on one is `Error { Protocol }`. `Ctrl+Alt+F2` must switch VT with a
   wedged shell, and `Alt+Tab` is how you leave an application that has
   taken the keyboard.
2. **The shell's bindings.**
3. **The focused client**, or the grab holder — a grab replaces focus at
   *this* step, which is why it does not outrank step 2. See
   [Keyboard grabs](#keyboard-grabs).

`mods` is a `mod_mask` bitmask (`SHIFT`/`CTRL`/`ALT`/`SUPER`), *not* the
xkb mask `Key.mods` carries. xkb's serialized mask is an opaque bitmap
whose positions depend on the compiled keymap, so it cannot be compared
against a constant and a shell could not express "Super+Return" in it at
all. `Mods::mask`/`Mods::from_mask` convert at the one boundary.

**One chord, one owner.** A chord another client holds is
`Error { Protocol }`; re-binding your own `id` replaces it. A client's
bindings are all released when it disconnects, so a launcher that crashed
does not leave `Super+Return` swallowed for the rest of the session.
`UnbindKey` on an id that is not bound is a deliberate **no-op** — a shell
shutting down should not have to remember what it managed to bind.

### The bare-modifier tap

`keysym: 0` binds the *tap*: the modifier named in `mods` pressed and
released with nothing else in between. It is the launcher's Super trigger,
and it is the one binding that needs state rather than a lookup.

`HotKeys::key` is fed **every** key event, hotkey or not, because a tap is
defined by what did *not* happen while the modifier was held. A candidate
is armed when a lone modifier goes down with nothing else held, and
cancelled by:

* any other key going down (so `Super+Q` is a window close and not also a
  launcher toggle);
* a second modifier going down (`Super+Shift` tapped has no meaning — there
  is no single press to detect, which is also why `BindKey` refuses a tap
  naming more than one modifier);
* a bound chord firing under the same modifier;
* a non-modifier release, which means the key was pressed while the
  modifier was held;
* `HotKeys::reset`, after a VT switch or a keyboard being unplugged: the
  release that would have completed the tap was never seen, so firing later
  would be a guess.

The tap fires **once, on the release**, with `pressed: false`. There is no
press event to report: until the release the server cannot know it was a
tap rather than the start of a chord.

A pointer button does not go through `HotKeys::key`, so `Server::pointer_button`
calls `HotKeys::cancel_tap` on every press. That is what keeps `Super`-drag
and the launcher's bare-Super trigger from being the same gesture: a drag
ends with `Super` released and no key in between, which is exactly a tap's
shape, and the button is the only thing that distinguishes them.
`super_drag_still_moves_a_window_with_the_shell_connected` in
`tests/shell.rs` asserts both halves — the window moves, and no `HotKey`
fires.

## Keyboard grabs

A launcher is `NO_FOCUS` and `Overlay`: it must never take focus, because
taking focus would make the window behind it look inactive and would move
the MRU order. So it reads the keyboard through
`GrabKeyboard { window, on }` instead.

A grab replaces **focus** as the destination of key events: while it is
held, keys go to the grabbing window rather than to the focused one. The
focused window is never told it lost anything — it stays focused, keeps its
active frame, and simply stops receiving keys.

**A grab does not outrank the bindings.** The priority list above is the
whole truth: compositor chords, then the shell's `BindKey` bindings, then
the grab holder or the focused window. A chord that fires is reported as a
`HotKey` and is *not* also delivered as a `Key` to the grab holder.

That is the behaviour a launcher actually needs, which is why it is this way
round rather than the other: a launcher opened by a bare-Super tap has to be
closable by a second tap *while it holds the grab*, and if the grab
outranked bindings the second tap would arrive as an ordinary key event and
the launcher would have to reimplement tap detection itself. The cost is one
rule a shell has to know, stated in `docs/wire.md` too: **do not bind a
chord you also want delivered as a key to your grabbing window.** Escape,
the key a launcher most wants, is nobody's chord, so this is not a
constraint in practice.
`a_bound_chord_under_a_grab_fires_as_a_hotkey_not_a_key` pins the order
down.

Released by `on: false`, by the window ceasing to **show**, by closing it,
or by the client disconnecting. "Ceasing to show" is `Server::showing`,
checked on each key rather than watched for: lazily, because the scene does
not report visibility changes and polling one node per key is cheaper than
inspecting every commit — and checked at all because the launcher hides
itself on Escape, so requiring an explicit release as well would mean one
forgotten message swallows the keyboard for the whole session.

One grab at a time: a second replaces the first, whose owner is simply no
longer receiving keys.

## The window list

`WindowList` is answered on receipt — it is a question, like `MeasureText` —
with one `WindowInfo` per window and a `WindowListEnd`, and it
**subscribes** the connection. Afterwards a change produces another
`WindowInfo` and a window that goes produces a `WindowGone`, so a bar never
polls.

`WindowInfo` is emitted from the places that change something in it:
`place_new_window` (a new entry), `set_focus` (**both** windows, so a bar
can un-highlight the old one), `announce_state`, and the commit path for a
retitle or an app id. The last two are why `ApplyOutcome` grew a
`relisted` list next to `retitled`: they answer different questions —
`retitled` is "reshape the title bar", `relisted` is "tell the bar its
entry moved" — and an app id change is the second without the first.

### Only `Normal` windows are applications

The list is **every** window, shell surfaces included: a wallpaper, a dock
and a launcher are windows like any other as far as the server is
concerned. So `WindowInfo` carries the window's `layer`, and a task list
filters on `layer == Normal`.

That is not a detail a consumer can skip. `nitro-bar` filtered only on its
own app id, which hid the bar and nothing else, and a desktop running the
wallpaper and the launcher showed `nitro-wallpaper` and `nitro-launcher`
as entries in the task list — buttons that focus `NO_FOCUS` surfaces and
therefore do nothing.

The filtering is the **consumer's**, not the server's, because the layer
is information a future dock or pager wants: "which bars are up" is a
reasonable question, and a server that answered `WindowList` with only
applications could not be asked it. A bar that lists a window whose layer
later changes must also *remove* it, so the filter belongs on the same
upsert path as everything else rather than at the point a window is first
seen.

### The list is the part of the bar that yields

Since #561 a widget is never laid out below the size it measured unless
it says it can, so a bar whose sections do not fit **overflows** rather
than squashing them. The window list is the one section whose content
count has no ceiling, so it is the one that takes the `Zero` shrink floor
(on the row *and* on its buttons — narrowing the row alone would only
make its children overflow it instead).

Measured on the box at 1920: twelve windows need no shrinking at all
(buttons at their natural 104 px, well under the `MAX_BUTTON_W` cap of
180, the row ending at 1379). At twenty-four the row is squeezed to 1708
and the buttons fall to 65 px, with the clock, battery, load and memory
still on the strip. Without the opt-out that row would be laid out at its
intrinsic ~2700 px and push all four off the end of the bar — including
the clock that is supposed to be centred *on the bar*. A squeezed title
is the better failure, because the sections it would otherwise displace
are the ones the user did not open and cannot close; `MAX_LABEL_CHARS`
and `MAX_BUTTON_W` are what keep a squeezed title from being the usual
case.

### The icon is the app id, and that is the whole rule

Each window-list button carries a 16 px **coloured** application icon,
and the name it asks for is the window's `app_id` itself, with `window`
as the fallback. `nitro-bar` does no `.desktop` parsing, holds no index,
and reads no files.

That works because of a convention rather than a specification: an
application's desktop file is usually named after its app id, and its
`Icon=` key usually matches both. `firefox` has app id `firefox` and icon
`firefox`. Our own applications keep it deliberately — the `.desktop`
files under `deploy/` are named after the `App::new` name each binary
registers, which *is* the app id the shell reports.

**The limitation shrank in #3715 without the rule changing.** The bar
still sends the raw app id and still reads nothing; what changed is that
the *server*, failing to find that name in the icon theme, now looks for
`<app_id>.desktop` and resolves its `Icon=` (`docs/icons.md`). So the
case that falls back is no longer "an application whose app id is not an
icon name" — which was every application this desktop ships, and is why
the box showed the generic `window` glyph for the calculator — but the
narrower **"an application whose `.desktop` basename differs from its app
id"**. `org.gnome.Nautilus` ships `org.gnome.Nautilus.desktop` and now
resolves; an application that registers one app id and installs a
differently named entry still gets the fallback, and nothing says why.

**And the hop needs the file to be installed, which is what #3723 did.**
A rule that reads `<app_id>.desktop` is worth nothing on a box where
nobody wrote one, and until #3723 `just deploy` deliberately did not:
`deploy/*.desktop` carry a bare `Exec=` (the spec's form, and a
packager's), the box's binaries live in `~/nitro-bin`, and that
directory is on nobody's `PATH` — so installing them handed the launcher
a command it could not run, *and* shadowed the built-in entry that could.

The fix is a `PATH`, in the one process that knows the answer.
`nitro-session` already finds its own pieces by looking next to its
executable (`crates/nitro-session/src/pieces.rs`, "the sibling lookup");
it now **prepends that directory to the `PATH` every child inherits**, so
the same fact is available to everything its children go on to start.
The launcher needs no change for it — `Command::new("nitro-term")` is
`execvp`, and the kernel does the search — which is the argument for
putting it there rather than teaching the launcher about deployment
layouts. Prepended rather than appended, for the same reason the sibling
lookup wins over `$PATH`: a stale `/usr/local/bin/nitro-term` must lose
to the binary deployed beside the running session. An installed
`/usr/bin/nitro-session` contributes `/usr/bin`, which is already there,
so the change is a no-op off the box.

With the files installed the shadowing becomes the behaviour we want: the
launcher shows **one** Terminal entry, the packaged one, because
`Launcher::rescan` drops a built-in whose program's *file name* matches a
scanned entry's. `crates/nitro-launcher/tests/deployed.rs` pins all of it
against the repository's own files rather than fixtures.

That is the bigger alternative this section used to weigh — "have the
server resolve `app_id` → `.desktop` → `Icon=`, strictly better and
strictly bigger" — taken, on the evidence it asked for: the fallback icon
was showing up on the applications people on this box actually run. The
index is 200 lines in the compositor, the bar is unchanged, and the
small rule is still the one the bar implements.

The icons do not touch the bar's idle contract: an app id is fixed for a
window's life, so each button sends exactly one `SetIcon` when it is
created and none afterwards — the same property the static icons have,
and asserted over 120 s of sensor ticks by
`the_icons_are_painted_once_and_never_again`.

### Server-global window ids

`WindowInfo.window` is a `WindowRef`, a dense `u32` the server mints. It is
a **third** id space, and both of the existing ones were unusable:

* a scene `WindowKey` is generational and internal; handing one out would
  leak the scene's allocation strategy onto the wire;
* a client's `NodeId` is namespaced *per connection*, so two clients may
  both own `NodeId(1)` and a shell could not tell them apart.

Ids are **never reused**. A shell holding a stale `WindowRef` gets "no such
window" — silently, since there is no per-request error — rather than
someone else's window, which is the one failure it could not detect.
`WindowRefs::next` saturates rather than wrapping for the same reason.

The list is ordered by scene key, not by z-order: a bar's task list should
not reshuffle itself every time the user raises a window. A shell that
wants stacking order can ask for it when there is a reason to.

### Acting on another client's window

`FocusWindow`, `CloseWindow` and `SetWindowStateFor` take a `WindowRef`.
All three are **silently refused** when they cannot be honoured — a
`NO_FOCUS` window cannot take focus, a `FIXED_SIZE` window cannot
maximize, a stale ref names nothing — on exactly the terms a click or a
`SetWindowState` from the owning client would be. There is no
per-request error in this protocol, and a bar's window list must not be
able to wedge the keyboard by clicking the wrong row.

**`FocusWindow` restores a minimized window first** (#3724). It used to
be refused for one, because the server's `focusable()` excludes
`Minimized` — so clicking a minimized entry in the bar did nothing at
all, silently, which is what the test box reported: *"if a window is
minimized, clicking it in the bar should re-open it"*. The server now
un-minimizes it through the **same** `set_state(Normal)` that `Alt+Tab`
uses and then raises and focuses it. Two reasons it is the server's job
and not the bar's: "restore then focus" is one act, so a bar sending two
messages could have the second refused for the state the first had just
changed; and every shell gets the behaviour rather than each one
reimplementing it. The `NO_FOCUS` refusal is unchanged and is checked
*before* the restore, so a bar cannot un-minimize a panel it could never
focus.

`CloseWindow` is a *request*: the owning client gets `Closed` and decides,
so unsaved work survives a misclick in a task list.

### What a click on a window-list entry does

The rule a task list follows, and `nitro-bar` implements it in
`upsert`'s `on_click`. It needs no new message: the bar knows each
entry's focus and state from the `WindowInfo` it already subscribes to.

| the entry's window | click | what the bar sends |
|---|---|---|
| unfocused, on screen | focus and raise it | `FocusWindow` |
| minimized | restore, raise and focus it | `FocusWindow` (the server restores) |
| focused, on screen | minimize it | `SetWindowStateFor { Minimized }` |
| any | **middle** click: ask it to close | `CloseWindow` |

The third row is the taskbar toggle every desktop has, and before #3724
the bar had no equivalent: focusing an already-focused window is a no-op,
so the row for the window you were looking at was a button that did
nothing. It goes out as the `SetWindowStateFor` the bar could already
send, so the wire is untouched.

A **minimized** entry is rendered differently, because eight identical
rows for eight windows say nothing about which of them are on screen: the
label is bracketed (`[Calculator]`) *and* dimmed to `text_dim`. Both,
not either — colour alone is an affordance a colour-blind user does not
get, and a marker alone is easy to miss in a long row. What it is **not**
is `set_enabled(false)`, the obvious way to grey a button: a disabled
button ignores clicks, and clicking a minimized entry is precisely what
has to work.

The ops that act on the sender's **own** windows (`SetLayer`,
`SetExclusiveZone`, `SetAnchor`, `GrabKeyboard`) take a `NodeId` instead and
answer `Error { UnknownNode }` for a window the sender does not own —
fatally, because a shell that named a window it does not own has lost track
of its own tree.

## Layers

`SetLayer` moves one of the sender's own windows to `Background`
(wallpaper), `Top` (bar, dock) or `Overlay` (launcher, menu).

`Normal` is `Error { Protocol }`. A shell surface asking to be an ordinary
window has misunderstood the op, and obliging silently would put a bar into
the window-management z-order, where a click could raise a document over it
— the scene's layer ordering is what makes a panel un-coverable, and
`docs/wm.md` relies on it (a left click raises within the `Normal` layer
only, precisely so a click cannot pull a panel out from under a menu).

## Outputs

`Outputs` is answered on receipt with one `OutputInfo` per connected output
and an `OutputsEnd`, and subscribes to hotplug. A hotplug sends
`OutputGone` for whatever vanished and then the **whole** remaining list
rather than a diff: outputs are few, their positions are relative to each
other (unplugging the left one moves every other), and a diff a shell had
to reassemble would be a second source of truth about the layout.

`refresh_mhz` and `name` come from the backend's own mode description
rather than from `OutputState.refresh_ns`, which stores a period: round-
tripping it through nanoseconds would report a refresh rate slightly
different from the mode the kernel actually set.

## Statistics

Four keys in `stats`, and the first is the one to look at when a bar "is
not working":

| key | meaning |
|---|---|
| `shell_clients` | connections on the **privileged** socket. Zero means the shell never got there — wrong `XDG_RUNTIME_DIR`, or a server too old to have the socket. |
| `hotkeys` | live `BindKey` bindings. |
| `exclusive_zones` | windows reserving space. |
| `grabbed` | 1 while a keyboard grab is held. A 1 with no launcher on screen is a stuck grab. |

## Testing

* `src/shell.rs` unit-tests the policy with no server at all: zone
  arithmetic (per-edge addition, opposite edges, another output's zone,
  release, an absurd zone collapsing rather than inverting), anchor
  rectangles (span, margin, centring, a second output's origin), the bind
  table (reserved bits, malformed taps, compositor chords, contested
  chords, re-binding an id, unbind and disconnect) and the tap state
  machine (fires once on release; cancelled by another key, a second
  modifier, a bound chord, a pointer button and a reset).
* `src/lib.rs` unit-tests the two things the privilege check is made of:
  that only shell tokens read as privileged, and that `is_shell_op`
  classifies every shell op and no ordinary one.
* `tests/shell.rs` drives 28 cases through the real event loop on the fake
  backend: the two sockets' capability bits and three shell clients at
  once; **every** shell op refused on the wire socket, one connection
  each, and refused *without a commit*; a bar creating, anchoring and
  reserving in **one transaction** (the probe's regression); a 32-px top
  zone shortening a maximized window's `Configure` by
  exactly 32 and offsetting it by 32, released by `px: 0`, by minimizing
  the bar and by hiding it with `SetVisible(false)` — and taken back when
  it shows again, since a hidden zone is skipped rather than forgotten; a
  zone moving a newly *placed* window, asserted
  against `wm::place` on the shrunken area; a `Top` bar painted over a
  maximized window in a screenshot; `SetLayer{Normal}`, reserved anchor
  bits and a foreign `NodeId` each closing the connection with the right
  code; centred and margin-inset anchors; the window list over three
  windows following a retitle, a focus change, a state change and a close,
  with `WindowGone` and a retired ref; `FocusWindow`/`SetWindowStateFor`/
  `CloseWindow` on another client's window, including that a stale ref is
  not fatal; `Super+Return` firing `HotKey` twice and reaching the client
  *never*, while unbound `Super+A` still does; the bare-Super tap firing
  once and cancelled by another key and by a second modifier; unbind and
  disconnect both giving the chord back; a compositor chord refused; two
  clients contesting a chord; a grab routing keys to a `NO_FOCUS` overlay
  and back without a `Focus` event, released by hiding the window, and
  **not** outranking a bound chord (which arrives as a `HotKey`, while the
  tap still fires under the grab); `Super`-drag still working with a shell
  connected and not looking like a
  tap; outputs listed, hotplugged and unplugged; and an anchored bar
  re-spanning after a hotplug.
* `examples/shell_probe.rs` is the hardware probe: a `Top` bar with a 32-px
  exclusive zone, `WindowInfo` events printed as they arrive, and
  `Super+Return` bound. Throwaway, not a shipped binary. The numbers it
  produced are in the server README's "M3-B: the shell socket" table, and
  it found two real bugs — the buffered-op ordering above, and that a bar
  which ignores the `Configure` its own anchor produces paints its original
  width.
* The **consumers** test the model from the other side, and that is where
  the remaining surprises live. `crates/nitro-launcher` (M3-D) is the
  first client of the grab and of the bare-modifier tap, and its
  `tests/launcher.rs` pins down two things this document asserts but
  `tests/shell.rs` cannot show: that a bare-Super tap opens an overlay
  and a **second one closes it while the overlay holds the keyboard** —
  the whole reason a grab does not outrank the bindings — and that keys
  reach a `NO_FOCUS` overlay *past a focused ordinary client*, which
  needs a second real client to be visible at all. It also exercises the
  lazy release: a grab whose window stops showing is dropped on the next
  key rather than at the commit, so `stats.grabbed` reads 1 until
  something is typed. That is correct and documented above, and it is
  exactly the kind of thing a consumer's test discovers by waiting for a
  statistic that was never going to move.

## Deferred

**Per-app allow lists.** The grant is "this user", not "this program".
Making it finer needs an identity for the peer, and the candidates all have
problems worth stating rather than picking one now: `SO_PEERCRED` gives a
racy pid; matching the executable path is `/proc`-dependent and defeated by
a copy; a token handed out at spawn time needs something trusted to do the
spawning, which is the session manager — and there is not one yet. The
session manager is where this belongs, because it is the component that
*starts* the shell and can therefore hand it a secret nothing else has.

**Multi-seat.** Everything here is single-seat: one keyboard, one grab, one
focus, one hotkey table. A second seat needs a seat id on `HotKey`,
`GrabKeyboard` and `WindowInfo.focused`, and a per-seat `HotKeys`. The
protocol has room (a new field is a new op code and a new capability bit),
but the server's input path assumes one seat throughout and that is the
work.

**Per-output shell surfaces.** A bar currently anchors to whichever output
its window is on. "One bar per output" is a shell-side decision today (open
one window per output); an `output` field on `SetAnchor` would let the
server place it, and is worth adding when a shell actually wants it.

One now does, and it turns out "open one window per output" is not
actually available to a shell. `crates/nitro-bar` (M3-C) therefore opens
**one** bar, on whichever output the server placed its window: a client
cannot choose the output, because `CreateWindow` carries none and every
new window is placed on the primary one, and nothing moves a window
between outputs afterwards but a user's drag. N bar windows would all
land on the same output — N overlapping bars and N×32 px of zone on one
screen, which is worse than one bar. Closing this needs the `output`
field on `SetAnchor` *and* a way to place the window there to begin with;
the bar's README states the limitation where a reader of the bar will
find it.

`crates/nitro-wallpaper` (M3-D) hits the same wall and answers it the
same way, which is worth recording because a wallpaper is the surface
where "one per output" is most obviously wanted: N wallpaper windows
would all land on the primary output — N stacked backdrops on one screen
and none on the others. So it opens one, and its README says plainly that
the primary output is covered and a second output shows the compositor's
own background.

**Stacking order in the window list.** The list is ordered by window
identity. A shell that wants z-order, or the MRU order for a task
switcher, needs another op or a field; neither has a consumer yet.

**Input regions and click-through.** A bar with a shadow, or a launcher
dimming the desktop behind it, wants to say "this part of me is not
clickable". Nothing in M3 needs it.
