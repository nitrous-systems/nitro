# nitro-settings

**Displays, keyboard and audio**, in one decorated window. It is the app
that writes the compositor's own configuration file — `server.conf`, the
one `nitro-server` watches with inotify and reloads — and so the first
nitro app whose output something else reads back.

```text
┌──────────────┬───────────────────────────────────────────────────┐
│ Settings     │ Displays                                          │
│              ├───────────────────────────────────────────────────┤
│ ▣ Displays   │  Outputs                                          │
│   Keyboard   │ ┌───────────────────────────────────────────────┐ │
│   Audio      │ │ HDMI-A-1 1920×1080 @ 60 Hz [==o==] 2 ☑ primary │ │
│   Appearance │ │    position [0   ] [0   ]                     │ │
│              │ └───────────────────────────────────────────────┘ │
│              │  Positions are typed; drag-arrange is not in M4.  │
│              ├───────────────────────────────────────────────────┤
│              │ [Apply] [Revert]                          applied │
└──────────────┴───────────────────────────────────────────────────┘
```

A split view (`docs/ui.md`): a sidebar row per category, a page per
row. `hey nitro-settings do sidebar/nav_keyboard click` switches pages.

Run it against a server (`just fake` in another terminal):

```console
$ cargo run -p nitro-settings
$ NITRO_CONFIG=/tmp/server.conf cargo run -p nitro-settings
```

## What Apply writes

The whole file, rendered from the widgets and `rename(2)`d into place:

```text
# nitro server configuration — written by nitro-settings.
# Plain `key = value` lines; see docs/settings.md.

output.HDMI-A-1.scale = 2
output.HDMI-A-1.position = 0,0
output.HDMI-A-1.primary = true

output.VGA-1.scale = 1
output.VGA-1.position = 1920,0

keyboard.layout = de
keyboard.variant =
keyboard.options = ctrl:nocaps
```

Outputs sorted by connector, `scale`/`position`/`primary` within each,
`primary` only when true, a blank line between blocks, and the three
keyboard lines always. `apply_writes_exactly_the_expected_file` compares
the whole string, because a file format nothing pins is a file format
that drifts.

The **rename is the point**. The server watches the config *directory*,
so a rename makes the new file appear atomically: a reader sees the old
file or the new one, never a half-written one. Writing in place would let
the compositor reload three lines, apply a scale of 1 to the primary
output and re-apply the real one a millisecond later.

## Limitations, and they are real

**The file is rewritten wholesale.** Comments you typed and keys this app
does not know about are **lost** on Apply. Round-tripping them would mean
keeping the whole file's token stream; the answer for now is: edit the
file *or* use the app, not both.

It does not, however, invent settings. A connector the file never
mentioned gets no `scale` line unless you move its slider: the slider is
seeded from the *live* scale, and persisting that would pin today's EDID
answer (so a replaced monitor stops being measured) and would make a
`NITRO_SCALE=…` dev override permanent.

**No drag-arrange.** Output positions are typed into two fields, in
desktop logical pixels. A drag-to-arrange canvas needs a widget
`nitro-ui` does not have and a preview the server cannot render yet; the
dialog says so in a label rather than pretending.

A typed position is logical, and the server turns it into device pixels
with that output's own scale — so the two agree exactly when the outputs
share a scale, and can overlap in device space when they do not (a
1920-wide output at 2× is 960 logical units wide, so a neighbour at
`960,0` at 1× sits inside it). Choosing device positions instead of
deriving them is drag-arrange's job. See `docs/settings.md`.

**The scale slider offers 1–3 in steps of 0.25.** The *file* allows
0.5–8 and the server enforces that, but the panels that exist live in
1–3, and a knob whose useful travel is its leftmost eighth is a worse
control than one that cannot express a scale nobody has. A file outside
the range is clamped into it visibly — and reversibly, by not pressing
Apply.

**The volume is read once.** No daemon, no D-Bus, no polling timer: a
volume changed by a media key while the window is open is stale until you
press Revert. That is what the idle contract costs, and it is why
`nothing_is_sent_while_it_sits_there` can assert that a settled dialog
leaves no timer armed at all.

## Validation is the server's job

Apply does not check your keyboard layout, and that is deliberate: the
compositor owns xkbcommon, and a settings app that re-implemented the
check would eventually disagree with the thing it configures. So Apply
writes the file and then **asks** — the control socket's `stats` carries
a `config_reloads` counter, and the status line reports what it did:

| status | means |
|---|---|
| `applied` | the counter moved: the server read the file and took it |
| `server rejected: see log` | it did not move within 1.5 s; the compositor's log says why |
| `saved to …` | there is no server to ask. Written and correct, confirmed by nothing |

The third is not the second. A file nothing confirmed is not a file
something refused, and drawing them the same way would teach the user to
ignore the message.

**"Test here"** is a scratch text field for exactly this: after Apply,
type in it and see whether the new layout took.

## "Caps Lock is Ctrl"

The Keyboard page's one switch, and the only xkb option with a control
instead of a spelling to remember. It adds and removes exactly
`ctrl:nocaps` in the `options` field beside it — which is what
libxkbcommon has always done with that token, LED and lock state and
modifier interactions included. There is no bespoke remap path here: the
control writes a string, and the compositor's keymap does the work.

It **edits the list rather than replacing it**. `keyboard.options` is
comma-separated and may well have been typed by hand
(`grp:alt_shift_toggle,compose:ralt`), so the switch adds or drops its
own token and leaves every other entry, in its original order, alone —
`conf::with_option`, round-tripped in a unit test and again through the
real file in `the_caps_lock_switch_edits_only_its_own_option`. A control
that overwrote the field would delete a configuration from a switch the
user flipped to get one thing.

The switch and the field show one value and each follows the other:
typing `ctrl:nocaps` into the field ticks the switch, flipping the
switch rewrites the field. That cannot loop, because a *setter*
(`set_text`, `set_checked`) is the app changing its own mind and does
not run the user callback — only an *action* does.

There is deliberately no control for `ctrl:swapcaps` (swap rather than
replace) or for any other option: one switch for the common case, and
the **file remains the escape hatch for the full xkb vocabulary**.

## Where the display list comes from

The **shell socket**. `App::shell` connects to `shell.sock`,
`Ui::outputs()` subscribes, and each output arrives as a `ShellEvent`.
It is a subscription, not a poll — plug a monitor in and a row appears.

The window is nevertheless an **ordinary decorated window**: the app
never calls `App::surface`, and a shell connection with no shell surface
is exactly that. The socket *is* the capability (`docs/shell.md`), so the
privilege buys the output list and costs nothing else;
`a_shell_connection_still_opens_an_ordinary_window` asserts the window is
decorated and focusable rather than a `NO_FOCUS` panel.

Without a shell socket — an older server, or a client started outside
the session — the dialog falls back to the ordinary socket, builds its
rows from `server.conf` alone, and **says so in the note under the
rows**. A dialog showing two of your four monitors with no explanation is
worse than one that admits what it is working from.

## Audio is a remote control

There is no audio in nitro and there is not going to be: PipeWire is the
sound server, and a display server that also owned the mixer would be two
daemons in one process. So the audio section runs `wpctl`, falls back to
`pactl`, and shows **"no audio backend found"** when neither is on
`PATH`, with the slider and checkbox disabled rather than absent.

| | read | write |
|---|---|---|
| `wpctl` | `get-volume @DEFAULT_AUDIO_SINK@` | `set-volume … 65%`, `set-mute … 1\|0` |
| `pactl` | `get-sink-volume @DEFAULT_SINK@`, `get-sink-mute` | `set-sink-volume`, `set-sink-mute` |

`wpctl`'s output format is **not a contract**, so it is parsed by shape:
a decimal fraction anywhere in the text, and `MUTED` anywhere in it. Not
"the first number" — the first number in `Sink 42 volume: 0.50` is the
sink id, and reading it as a volume would set the machine to maximum on a
format this parser was meant to tolerate.

## Driving it with `hey`

Every widget carries a `.name()`, and the names are published as
`nitro_settings::names` constants. A display row is named by its
**connector**, which is what the server, the file and the monitor's EDID
all call it:

```console
$ hey nitro-settings list
$ hey nitro-settings do displays/HDMI-A-1/primary click
$ hey nitro-settings set displays/HDMI-A-1/scale value 2
$ hey nitro-settings set displays/HDMI-A-1/x value 0
$ hey nitro-settings get displays/HDMI-A-1/scale_value value   # 2
$ hey nitro-settings set keyboard/layout value de
$ hey nitro-settings do keyboard/nocaps toggle                 # Caps Lock is Ctrl
$ hey nitro-settings do apply click
$ hey nitro-settings get status value                          # applied
```

The short paths work because a segment naming no direct child is looked
for by name in the subtree, and refused if it is not unique — which is
why the captions in front of the fields are deliberately **unnamed**: a
label called `layout` beside the field called `layout` would make
`keyboard/layout` ambiguous and break every command above.

That rule is also what kept these commands working when a display row
became **two lines** in #3725 (`docs/settings.md`). The row is now a
column of a `top` and a `bottom` line, so the canonical path `list`
prints is `displays/HDMI-A-1/top/scale`; `displays/HDMI-A-1/scale` still
resolves, because the segment names no direct child and is unique in the
subtree. The second line adds `mode`'s companion `modes` (the
connector's other refresh rates, read-only, absent on a connector that
offers none) and `position` for the caption in front of `x` and `y`.

## Why there is a second copy of the config parser

`src/conf.rs` renders and parses the same format as
`nitro_server::config`, and does **not** depend on it. Depending on the
compositor crate would link libinput, drm, xkbcommon and the rasterizer
into a binary whose job is to render nine lines of text, for the benefit
of forty lines of parser.

What keeps the two honest is a test, not a comment:
`the_server_parser_reads_back_what_we_write` feeds this crate's output to
the server's real parser — through a dev-dependency, where the compositor
may be linked — and asserts that every value survives with no warnings.
A divergence in either direction fails there, which is the only place it
can be caught without shipping the compositor to the user.

## Environment

| | |
|---|---|
| `$NITRO_CONFIG` | the file to read and write; else `$XDG_CONFIG_HOME/nitro/server.conf`, else `$HOME/.config/nitro/server.conf` |
| `$NITRO_CONTROL` | the server's control socket, for the `applied` verdict; resolved exactly as `nitro-shot` does |
| `$PATH` | where `wpctl` and `pactl` are looked for |
