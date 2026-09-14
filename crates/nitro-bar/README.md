# nitro-bar

The desktop's **top bar**, and the first program written against the
[shell socket](../../docs/shell.md).

```text
┌────────────────────────────────────────────────────────────────────────┐
│ ☰  ▸ Calculator │ hello-dialog      09:41        87%+  0.42  1.2/3.3G  │
└────────────────────────────────────────────────────────────────────────┘
  launcher   ─── window list ───      clock       battery load  memory
```

It is a `nitro-ui` app like any other — a state struct, a tree built
once, a callback per button — with exactly one difference: it connects to
`shell.sock` rather than `wire.sock`. That is what lets it live on the
`Top` layer, span the top edge of its output, and reserve 32 px of screen
space the rest of the desktop may not use. **The socket is the
capability**: the bar is privileged because of where it connected, not
because of anything it sent.

Run it against a server (`just fake` in another terminal):

```console
$ cargo run -p nitro-bar
```

`NITRO_BAR_HEIGHT` overrides the 32 px height (and the strip it
reserves). `NITRO_SHELL_SOCKET` overrides the socket path.

## What each section is

| section | name | what it does |
|---|---|---|
| launcher | `launcher` | fires the launcher's trigger (M3-D) |
| window list | `windows` | one button per window: click focuses, **middle-click closes** |
| clock | `clock` | local `HH:MM`, one update per minute |
| battery | `battery` | `/sys/class/power_supply/*`, e.g. `87%+` |
| load | `load` | the 1-minute average from `/proc/loadavg` |
| memory | `mem` | used/total from `/proc/meminfo`, e.g. `1.2/3.3G` |

The window list is **subscribed, not polled**: `WindowList` is answered
once and afterwards the server sends a `WindowInfo` whenever a window
appears, is retitled, changes state or takes focus, and a `WindowGone`
when one closes. The focused window's entry is marked `▸`.

Middle-click sends `CloseWindow`, which is a *request*: the owning client
is told and decides, so unsaved work survives a misclick. Both it and the
click are **silently refused** by the server when they cannot be honoured
— a stale window id names nothing — because a task list must not be able
to wedge the keyboard by naming the wrong row.

## Idle costs nothing

With nothing changing, the bar puts **zero bytes on the wire between
clock ticks**, and the clock ticks once a minute. Three mechanisms:

* the window list is subscribed rather than polled, so nothing is sent
  to ask "has anything changed?";
* the clock's timer is **aligned to the minute boundary** and re-armed
  from inside its own callback, so it fires at `:00` rather than every
  second to check whether the minute rolled over — and it cannot drift,
  because the interval is recomputed each time;
* the sensors are polled every 5 s but written to the tree **only when
  the formatted string differs**. A `Label`'s setter returns early on an
  unchanged string, so an unchanged reading costs no mutation and no
  commit.

`a_settled_bar_is_silent_while_nothing_changes` asserts that from the
outside, over a window containing a dozen sensor polls, and
`a_sensor_that_changes_costs_one_set_text` asserts the other half: when a
reading does change, it costs exactly one `SetText`.

## Driving it with `hey`

Nothing below cooperates with the bar's code — `hey` is a separate binary
talking to the introspection socket every nitro app opens.

```console
$ hey nitro-bar list                       # the whole bar, one line per widget
$ hey nitro-bar get clock value            # 09:41
$ hey nitro-bar get load value             # 0.42
$ hey nitro-bar do launcher click
$ hey nitro-bar do windows/win3 click      # focus window 3
$ hey nitro-bar do windows/win3 alt_click  # ask it to close
```

Window-list entries are named `win<N>` after the server's own
`WindowRef`, which is never reused — so the path a script holds names the
same window for that window's whole life, and a stale one resolves to
nothing rather than to somebody else's window.

## One bar, on the primary output

The bar opens **one window, on whichever output the server places it**.
"One bar per output, following hotplug" is not implementable on today's
protocol, and the bar deliberately does not pretend otherwise:

* `CreateWindow` carries no output, and the server places every new
  window on the primary one;
* `SetAnchor` anchors to whichever output the window is *already* on;
* nothing moves a window between outputs but a user's drag.

So N bar windows would all land on the primary output — N overlapping
bars and N×32 px of exclusive zone on one screen, which is worse than one
bar. `docs/shell.md` §Deferred records the gap under **"Per-output shell
surfaces"**, and the fix is an `output` field on `SetAnchor`.

The bar is structured so that adding it is small: all of the per-bar
state lives in one `Bar` behind one tree, so a second output means a
second `Ui` (one window each — a `Ui` owns exactly one window) and no
change to the layout, the window list or the sensors.

## Layout

The three groups are one `row()` with a `spacer().grow(1.0)` on each side
of the clock. That is what centres the clock **on the bar** rather than
in whatever space the window list happens to leave over — a clock that
slid sideways as windows opened would be the obvious wrong answer.

Window-list buttons are capped at 180 px and their labels elided at 22
characters, by character count rather than by byte, so a title in a
script that is not Latin is cut at the same visual length instead of
mid-codepoint. A window with neither title nor app id gets `(untitled)`
rather than an empty button nothing can be clicked on.

## Reading the sensors

`src/sensors.rs` is split into **pure formatters** that take the file's
text and **thin readers** that only open the file. That split is what
makes the parsers testable: a test that needed a real battery would only
ever run on somebody's laptop.

Every reading is `Option<String>`, and `None` means *leave the widget
empty* rather than draw a zero. A bar that said `0%` on a desktop would
send the user looking for a fault in their machine instead of in the bar.

Memory is `MemTotal - MemAvailable`, not `MemTotal - MemFree`: `MemFree`
counts the page cache as used, which is the classic wrong answer and
makes a healthy Linux box look permanently out of memory.

`src/clock.rs` parses `/etc/localtime` (TZif, RFC 8536) itself, because
the crate's dependencies are `nitro-ui` and `rustix` and nothing else. It
never needs the date — only the time of day — so there is no calendar
arithmetic in it at all.

## Tests

`tests/bar.rs` drives the tree the binary builds, through a real server
on the shell socket:

* the exclusive zone really shrinks the desktop — a second, ordinary
  client that maximizes lands below the bar and is shorter by its strip;
* the window list follows a window appearing, being retitled, and
  closing, and the bar does **not** list itself;
* a real click on an entry focuses that window, and a real middle click
  makes the owning client receive `Closed`;
* the clock updates **exactly once** at the minute boundary and not at
  all inside the minute, costing one `SetText`;
* idle silence, with the sensors polling throughout;
* every section resolves by name, so `hey nitro-bar list` finds it.

The wall clock is faked (`Bar::with_fake_time_ms`) and so are the sensor
readings (`Bar::with_sensors`) — the latter because the idle claim is "a
poll that finds the same numbers costs nothing", and polling the real
`/proc` would instead be asserting that this machine's load average held
still, which is not the claim and is not reliably true.
