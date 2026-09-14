# Settings: `server.conf` and `nitro-settings`

Two halves of one feature. The **file** is the contract — a plain text
file the display server reads, watches and re-applies without being
restarted — and `nitro-settings` is one editor for it. The file is
primary: everything below works with `$EDITOR`, and the app exists
because a user should not need one.

```text
$XDG_CONFIG_HOME/nitro/server.conf      (default ~/.config/nitro/server.conf)
```

```text
# nitro server configuration
output.HDMI-A-1.scale    = 2
output.HDMI-A-1.position = 0,0
output.HDMI-A-1.primary  = true
output.VGA-1.position    = 1920,0

keyboard.layout  = de
keyboard.variant =
keyboard.options = ctrl:nocaps
```

Save it and the desktop re-lays out. No restart, no `systemctl`, no
logout — the server watches the file and applies what changed.

## The format

One assignment per line. `#` starts a comment at the beginning of a line
or after whitespace, so a value may contain a `#` without being
truncated. Whitespace around the `=` is insignificant. The **last**
assignment to a key wins, which makes appending a line a working way to
override one.

That is the whole grammar. It is deliberately not TOML: there are no
tables, no arrays and no types beyond a float, a pair of integers and a
string, so a parser dependency would buy nothing. The key *is* the path —
`output.<connector>.scale` is what a table would have spelled anyway —
and a flat file survives `sed`, a settings app, and a person with a
broken desktop and a text console.

### Keys

| key | value | default |
|---|---|---|
| `output.<connector>.scale` | `0.5`–`8`; the logical-to-device factor | EDID: 2 at ≥ 192 dpi, else 1 |
| `output.<connector>.position` | `x,y` — this output's top-left corner in **desktop (logical)** coordinates; may be negative | placed after the last positioned output, in connector order |
| `output.<connector>.primary` | `true`/`yes`/`on`/`1` (and the negatives) | the first connector |
| `keyboard.layout` | an xkb layout, e.g. `us`, `de`, `us,de` | `us` |
| `keyboard.variant` | an xkb variant, e.g. `nodeadkeys` | none |
| `keyboard.options` | xkb options, e.g. `ctrl:nocaps` | none |

`<connector>` is the name the kernel gives the connector — `HDMI-A-1`,
`VGA-1`, `DP-1.2` — which is exactly what `nitro-shot --outputs` prints.
A connector name may itself contain a dot (a DisplayPort MST branch
reports `DP-1.2`), so the field is taken after the **last** dot.

An explicit empty value is not the same as an absent key:
`keyboard.variant =` means "no variant", where saying nothing lets
`XKB_DEFAULT_VARIANT` or xkbcommon's own default decide.

### `keyboard.repeat` is deliberately absent

It is the key a reader most expects to find, so the parser names it
explicitly and warns rather than letting it fall into "unknown key".

**Nothing in nitro repeats keys yet.** libinput reports a press and a
release, the server forwards both, and no client synthesises a repeat in
between. A `keyboard.repeat = 300,25` would therefore be a promise with
nothing behind it — a setting that appears to work, changes nothing, and
costs a user an afternoon. When key repeat is implemented this key is
where it goes.

## Nothing in this file can fail

A configuration file is user input that arrives **while the compositor is
running**, so there is no useful sense in which parsing it can fail: a
bad line must not be able to take a desktop down.

Every unusable line is warned about in the log and skipped, and
everything else is applied. An unknown key, an unparseable scale, a
missing `=`, a scale of `-3`, a file of binary garbage — all of them are
warnings, and the previous configuration stays in force for anything the
file no longer says. A file that does not exist is not an error either;
it is the state every fresh installation is in.

Scales are clamped to `0.5`–`8` rather than trusted. A typo in the other
direction (`scale = 20`) would render the desktop at twenty times and
leave nothing clickable with which to fix it — including this app.

## Precedence: environment, then file, then the hardware

| setting | wins | then | then |
|---|---|---|---|
| output scale | `NITRO_SCALE=<c>=<f32>` | `output.<c>.scale` | EDID dpi step |
| output position | — | `output.<c>.position` | left-to-right in connector order |
| primary output | — | `output.<c>.primary` | the first connector |
| keyboard | `XKB_DEFAULT_*` | `keyboard.*` | the `us` layout |

The environment wins because it is the **development** channel: a
`NITRO_SCALE=HDMI-A-1=2 just fake` must not be silently overridden by
whatever the box's own config happens to say. The file wins over the EDID
because it is the user's explicit answer to the EDID's guess.

## How a reload happens

Three doors, one `Server::reload_config`:

| door | when to use it |
|---|---|
| **inotify** | automatic; saving the file is enough |
| **SIGHUP** | scripts, and a session manager that edited the file |
| **`reload` on the control socket** | tests, because it is **synchronous** — the `ok` comes back after the reload was applied, where the other two are races against the loop noticing |

`stats` counts completed reloads in `config_reloads`, whichever door they
came through: what a caller wants to know is "did the server pick my edit
up", not which mechanism told it.

On reload the server re-applies the scales (a change sends a `Configure`
to that output's windows and repaints it in full), re-lays out the
desktop positions, and rebuilds the xkb keymap — resetting the key state,
because a keymap swap invalidates whatever modifiers were being held.

### Why the watch is on the directory

The safe way to write a configuration file is *write a temp file and
rename it over the target*, which is what `nitro-settings` does, what
`sed -i` does, and what every editor with a crash-safe save does. A
rename **replaces the inode**, so a watch on the file itself follows the
old inode into oblivion and never fires again. Watching the directory and
filtering on the file name sees the rename, the plain overwrite, and the
first creation of a file that was not there before.

That last case is not hypothetical, and it is where this shipped broken
once: on a machine whose `~/.config/nitro` did not exist, the watch could
not be placed at all (inotify needs an existing directory), nothing
retried it, and so the very first file a settings app wrote — the one
that creates it — was the single event guaranteed to be missed. Every
fresh installation is in that state. The server now creates the directory
it intends to watch.

An idle desktop pays nothing for the watch: an inotify fd with no queued
event is not readable, so it never wakes the event loop. Measured on the
test box, 30 s with the watch armed and nothing happening is **0 CPU
ticks**.

## `nitro-settings`

```console
$ nitro-settings
```

Three sections in one decorated window: **Displays** (one row per output
— name, mode, a scale slider, a `primary` checkbox, and x/y position
fields), **Keyboard** (layout, variant, options, and a field to type in
afterwards), **Audio** (volume and mute). **Apply** writes the file;
**Revert** re-reads it.

The output list comes from the shell socket, so the rows are the outputs
the compositor actually has, with their live scales and positions — not
whatever the file last said. With no shell socket the app falls back to
the file alone and says so in the window rather than showing an empty
Displays section.

**Apply reports what the server did, not what the app hoped.** After
writing, it watches `config_reloads` on the control socket: `applied`
when it moves, `server rejected: see log` when it does not. The app does
not validate layouts itself — xkbcommon is the authority on whether `de`
compiles, and a second opinion in the client would be a second thing to
be wrong.

### Everything is `hey`-addressable

```console
$ hey nitro-settings set displays/HDMI-A-1/scale value 2
$ hey nitro-settings set keyboard/layout value de
$ hey nitro-settings do apply click
$ hey nitro-settings get status value
applied
```

Rows are named by connector, so `displays/HDMI-A-1/primary` is stable
across reboots and hotplugs. The captions in front of the fields are
deliberately unnamed: a label named `layout` beside the field named
`layout` would make the short path ambiguous.

### Three limitations, stated rather than discovered

**Apply rewrites the file wholesale.** Hand-written comments and keys the
app does not know about are not preserved. If you maintain the file by
hand and value its comments, do not press Apply.

What Apply does *not* do is invent opinions. A connector the file says
nothing about gets no `scale` line unless you actually move its slider:
the slider is seeded from the live scale, and writing that back would
freeze today's EDID answer into the file (so a replaced monitor would
stop being measured) and would make a `NITRO_SCALE=…` meant for one dev
run permanent.

**Positions are typed, not dragged.** Drag-arrange of a monitor layout is
not in M4. The fields are desktop-space logical pixels, which is the same
space `nitro-shot --outputs` prints in, so the numbers can be read off
and typed back.

**Mixed scales with explicit positions can overlap.** A position is
logical, and the device rectangle the pointer is clamped to is that
position times *that output's own* scale — so the two layouts match
exactly when the outputs share a scale, and can come apart when they do
not. A 1920-wide output at 2× is 960 logical units across, so a
neighbour placed at `960,0` at 1× overlaps it in device pixels, and a
pointer in the shared strip belongs to whichever output is found first.
The fix is to choose device positions rather than derive them, which
belongs with drag-arrange; until then a typed position is taken at face
value. See `docs/wm.md`.

### Audio

Volume and mute shell out to `wpctl` (PipeWire), falling back to `pactl`,
and the section says **no audio backend found** when neither is on
`PATH`. That is the honest state of the test box, which has no sound
server under nitro — the section reports it rather than showing a slider
that does nothing.

No daemon, no D-Bus and no polling timer: the values are read when the
window opens and written when you move the control.

## Measured

On the test box (Pentium G3240, HDMI-A-1 1920×1080), against the file
written by the app itself:

| | measured | budget |
|---|---|---|
| `nitro-settings` RSS | **2 872 kB** | ≤ 3.5 MB |
| binary | **719 240 B** | ≤ 700 KB — **2.7 % over** |
| idle, 30 s | **0 CPU ticks, 0 commits** | 0 |
| server idle with the watch armed, 30 s | **0 CPU ticks** | 0 |
| server RSS cost of the config + watch | **below this box's resolution** | ~0 |

The binary is **over its budget by 19 KB** and that is recorded rather
than rounded away. The app is three sections against `nitro-calc`'s one
keypad (573 664 B) and carries a config renderer, a control-socket
client and a subprocess audio backend; the budget was set before any of
those were specified. It is a number to bring down, not a reason to
pretend: the obvious lever is the introspection protocol, which
`docs/budget.md` already records as ~68 KB monomorphised per app-state
type and which a de-monomorphisation would share across every app at
once.

The server-RSS row deserves its hedge. Three interleaved A/B pairs
(config absent versus config present and watched) gave **+68, +744 and
+36 kB** against an A-side spread of ~110 kB in that series and 668 kB in
an earlier one. Two of the three agree with what the mechanism predicts —
one fd, one 4 KiB drain buffer and a struct of three `Option`s — but the
744 kB outlier is larger than the effect being measured, so the honest
statement is "at or below what a 3.3 GB box with no swap can resolve"
rather than a mean that would imply a precision this does not have.

The scale claim is settled on **pixels**: the bar's strip measures 32
device rows at scale 1, 64 at scale 2, and 32 again on the way back, read
out of raw framebuffer dumps. `stats` and `outputs` corroborate it. They
cannot decide it — a config that says 2 while the screen never re-lays
out would satisfy both, and `hey nitro-calc get window bounds` reports
the same `223,334` at either scale, because logical geometry is
scale-invariant by design.

The keyboard claim has a control: the same evdev keycode 21 produces `z`
under `keyboard.layout = de` and `y` under `us`, both by reload with no
restart, both typed through `ydotool` into a real terminal. Without the
second leg the test could not tell "the layout applied" from "that key is
z".
