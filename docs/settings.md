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
output.HDMI-A-1.mode     = 1920x1080@120
output.VGA-1.position    = 1920,0

keyboard.layout  = de
keyboard.variant =
keyboard.options = ctrl:nocaps

theme.scheme = dark
theme.accent = #6ca8f0
theme.icons  = hicolor
```

Save it and the desktop re-lays out and re-colours. No restart, no `systemctl`, no
logout — the server watches the file and applies what changed.

## The format

One assignment per line. `#` starts a comment at the beginning of a line
or after whitespace, so a value may contain a `#` without being
truncated. Whitespace around the `=` is insignificant. The **last**
assignment to a key wins, which makes appending a line a working way to
override one.

One exception, and it is there for the colours: a `#` followed by **six
or eight hex digits and then whitespace** is a value, not a comment, so
`theme.accent = #6ca8f0` means what it looks like. A trailing comment
after a colour still works (`# blue` is not hex digits). The cost is a
comment whose entire text is six or eight hex characters —
`scale = 2 #beefed` keeps the `#beefed`, which then fails to parse and is
warned about.

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
| `output.<connector>.mode` | `<W>x<H>[@<Hz>]`, or `max`, or `fastest` — which of the connector's modes to run | the connector's preferred mode |
| `output.<connector>.modeline` | raw timings the monitor never advertised; see below | none |
| `output.<connector>.position` | `x,y` — this output's top-left corner in **desktop (logical)** coordinates; may be negative | placed after the last positioned output, in connector order |
| `output.<connector>.primary` | `true`/`yes`/`on`/`1` (and the negatives) | the first connector |
| `keyboard.layout` | an xkb layout, e.g. `us`, `de`, `us,de` | `us` |
| `keyboard.variant` | an xkb variant, e.g. `nodeadkeys` | none |
| `keyboard.options` | xkb options, e.g. `ctrl:nocaps` | none |
| `theme.scheme` | `light` or `dark` — the desktop's colour scheme | `light` |
| `theme.<role>` | `#rrggbb` or `#rrggbbaa`, overriding one role on top of the scheme | the scheme's value |
| `theme.icons` | the **XDG icon theme** application icons come from, e.g. `hicolor`, `Adwaita` | `hicolor` |
| `remote.listen` | `<addr>:<port>` — bind a **TCP** listener for remote apps | absent: no TCP socket at all |

`<connector>` is the name the kernel gives the connector — `HDMI-A-1`,
`VGA-1`, `DP-1.2` — which is exactly what `nitro-shot --outputs` prints.
A connector name may itself contain a dot (a DisplayPort MST branch
reports `DP-1.2`), so the field is taken after the **last** dot.

An explicit empty value is not the same as an absent key:
`keyboard.variant =` means "no variant", where saying nothing lets
`XKB_DEFAULT_VARIANT` or xkbcommon's own default decide.

### `output.<connector>.mode`

```text
output.HDMI-A-1.mode = 1920x1080@120
```

**Which of the connector's modes to run.** With no such line nitro takes
the connector's *preferred* mode — the kernel's own choice, the one the
EDID marks — which is what it did for its whole life before this key
existed, and on every panel anyone has plugged in that has meant 60 Hz.

Four spellings, all matched against the list the connector actually
reports:

| value | means |
|---|---|
| `1920x1080@120` | exactly that size, and the listed rate nearest 120 Hz |
| `1920x1080` | exactly that size, **highest** rate available at it |
| `fastest` | the preferred *size*, highest rate at it |
| `max` | largest area, then highest rate — ignoring the preferred flag |

The rate may be fractional: `@59.94` is a real mode and picks it.

**Matching.** The size must match **exactly** — there is no scaling here,
this key chooses a mode and nothing else. The rate is matched to the
**nearest** listed mode within **0.5 Hz**, which is the rule that makes
`@60` unambiguous: an HDMI connector lists 60.000 *and* 59.940, 60 mHz
apart, and a threshold rule would have to pick one of them arbitrarily.
Nearest picks the one you asked for, both ways round. **Interlaced modes
are never selected** by a `mode` line: the default rule tolerates one as a
last resort, because a connector that lists nothing else still has to
light up, but answering an explicit `1920x1080@60` with a 1080i mode would
put a different picture on screen than the one that was asked for.

**A size or rate the connector does not have is a warning, not a
failure.** The output comes up on its default mode and the log names every
mode the connector *does* list — which is exactly the text you need in
order to write a line that works:

```text
[warn] HDMI-A-1: no mode matches `2560x1440@144`; this connector lists
       1920x1080@120, 1920x1080@85, 1920x1080@60, 1920x1080@60,
       1920x1080@50, 1920x1080@24, 3840x2160@30, 3840x2160@25,
       3840x2160@24. Using the default mode.
```

The same list without waiting for a mistake: **`modes` on the control
socket** (`crates/nitro-server/README.md`), which marks the preferred mode
`*` and the one in use `=`. `nitro-settings` shows the short version on
each display row — `1920×1080 @ 60 Hz (also 120, 85, 50, 24)` — and
deliberately offers no mode *picker*: a control that blanks the screen and
can leave a panel dark is not something to put behind a combo box.

**`outputs` reports the truth, not the request.** A `mode` line that did
not take shows as the mode that did, which is the only way to tell a
modeset that happened from one that silently did not:

```console
$ nitro-shot --outputs
HDMI-A-1 1920x1080@120000 scale=1 pos=0,0 primary=1
```

#### When it takes effect

**On reload, live.** A mode set is a *full modeset* — the CRTC is retimed
and the panel blanks for a moment — not an atomic property flip like
`scale` or `position`, so it is applied only when the resolved mode
actually changed. A reload that moved a colour, or one that repeats the
same `mode` line, costs nothing at all: the comparison happens before the
hardware is touched.

A **refresh** change at the same size (1080p60 → 1080p120) is the cheap
case and the one this key is mostly for: no buffer changes size, so the
shadow and both scanout buffers stay exactly as they are. A **size**
change reallocates them and re-lays-out the desktop, which is the same
path a hotplug already takes — it works, and it is a visibly bigger
event.

The key is also read **before the first modeset**, not after it: the
server reads `server.conf` before it opens the card, so a box configured
for 120 Hz comes up at 120 Hz rather than lighting at 60 and retiming a
moment later.

#### `NITRO_MODE`, for a measurement run

```console
$ NITRO_MODE=HDMI-A-1=1920x1080@120 nitro-server
$ NITRO_MODE=HDMI-A-1=1920x1080@120,DP-1=max nitro-server
```

Comma-separated, `<connector>=<mode>`, exactly `NITRO_SCALE`'s shape and
for the same reason: the environment is the **development** channel, so a
sweep or a one-off comparison can override the box's own file without
editing it. It beats `output.<c>.mode`.

#### `output.<connector>.modeline`, and how to get your screen back

```text
output.HDMI-A-1.modeline = 279750 1280 1328 1360 1440 720 723 727 810 +hsync -vsync
```

Raw timings, X-style, with the clock in **kHz** (the unit the kernel's own
`i915_display_info` prints, not X's MHz):
`<clock_khz> <hdisp> <hsync_start> <hsync_end> <htotal> <vdisp>
<vsync_start> <vsync_end> <vtotal> [+hsync|-hsync] [+vsync|-vsync]`.
Polarity defaults to `+hsync -vsync`, which is what CVT reduced blanking
wants. `NITRO_MODELINE=<connector>=<timings>` is the environment form —
one connector per variable, because a modeline has spaces in it and could
not share `NITRO_MODE`'s comma-separated list without a quoting rule.

> **This bypasses the monitor's advertised list entirely.** The EDID is
> not consulted; the kernel is asked whether the timings are *drivable*
> (an atomic `TEST_ONLY` commit before anything is applied, so a rejected
> modeline is a warning and the default mode, never a server that will not
> start) but nothing checks whether the **panel** will sync to them. A
> panel that will not shows black until you change the line back.

**Recovering from a black screen.** ssh in — the server is running fine,
it is the monitor that is unhappy — remove or fix the line, and restart:

```console
$ ssh box
$ sed -i '/modeline/d' ~/.config/nitro/server.conf
$ sudo systemctl restart nitro-dev
```

`just shot` keeps working throughout, and is **not** evidence either way:
it reads the shadow buffer, which is the picture the server *composed*.
Whether that picture reached the glass is a question only a person looking
at the panel can answer. (The #3705 lesson, in a new costume: the
instrument was measuring the layer below the broken one.)

`outputs` marks a custom mode so the caveat travels with the numbers:

```console
HDMI-A-1 1280x720@239840 scale=1 pos=0,0 primary=1 (custom)
```

#### The test box's modes, and the 240 Hz question

The human asked for 240 Hz — "the monitor can do it". On this source it
cannot, and the evidence is the connector's own list. `modes` on the
control socket (`nitro-shot --modes`), HDMI-A-1 — **45 modes**, of which
the interesting head is:

| mode | clock | note |
|---|---|---|
| **1920x1080@60** | 148 500 kHz | `PREFERRED`, and what the box ran before this key existed |
| 3840x2160@30 / 29.97 / 25 / 24 / 23.976 | | the largest *area*, which is why `max` and `fastest` differ here |
| **1920x1080@119.982** | 285 500 kHz | the top of the list |
| 1920x1080@84.904 | | |
| 1920x1080@60 / 59.94 / 50 / 24 / 23.976 | | |
| 1680x1050@59.883, 1280x1024@75.024, … | | 30 more, down to 720x400@70.081 |

Two things in that table are worth more than they look. **The 120 Hz mode
is 119.982 Hz, not 120** — 285 500 kHz over 2080×1144 — so `outputs`
reports `@119982`, and a rule that required an exact match would have
failed on the one mode this key exists to reach. You still write
`mode = 1920x1080@120`; that is what the 0.5 Hz nearest-match rule is for,
and `@85` likewise finds the listed 84.904. **A script that greps
`outputs` for the literal `@120000` will never match** and will skip the
arm silently — grep `1920x1080@` and parse.

And the list is **45 modes long**, not the handful `i915_display_info`
shows: the EDID's real list runs all the way down to 720x400@70. That is
why a "no such mode" warning truncates at twelve and points here — printed
whole it is a 1 400-character log line nobody reads.

**120 Hz is the top of the list and nothing above it exists.** The reason
is the link, not the panel: this is HDMI 1.4 on Haswell, whose TMDS clock
tops out near **300 MHz**. 1080p@120 at 285.5 MHz is just under it — which
is why it is the last mode the EDID offers — and 1080p@240 needs
**606.5 MHz** with CVT-RB v1 (583.2 with RBv2), twice the ceiling. Not
close. 1080p@144 (346.5 MHz) and 1080p@165 (401.0 MHz) do not fit either,
which is worth saying with the numbers rather than by trying: 346.5 > 300,
so there is nothing to attempt.

**240 Hz is a DisplayPort or HDMI 2.x question for another source**, and
that is the honest end of it for this box at 1080p.

What *does* fit under 300 MHz is a **smaller** mode at 240 Hz, and KMS
accepts an arbitrary mode blob whether or not the EDID lists it — which is
what `modeline` is for. CVT 1.2 reduced blanking for 1280×720@240:

```text
RBv1: H blanking is a constant 160 px (48 front, 32 sync, 80 back), so
      htotal = 1280 + 160 = 1440.
      V blanking is the fewest lines covering RB_MIN_V_BLANK = 460 us:
      h_period = (1e6/240 - 460) / 720 = 4.1944 us, so
      vbi = floor(460 / 4.1944) + 1 = 90 lines, vtotal = 810.
      vsync is 4 lines after a 3-line front porch: 723, 727.
      clock = 1440 * 810 * 240 = 279.936 MHz, rounded DOWN to CVT's
      0.25 MHz step = 279.750 MHz.
      Actual refresh = 279 750 000 / (1440 * 810) = 239.84 Hz.

RBv2: H blanking 80 px (8 front, 32 sync, 40 back), htotal 1360;
      same 810 lines; clock = 1360 * 810 * 240 = 264.384 MHz.
```

**279.75 MHz is inside the 300 MHz limit** — and so is RBv2's 264.4, with
more headroom. The formula is checkable against a mode the box already
has: run it for 1920×1080@120 and it produces **285 500 kHz**, the exact
clock the kernel reports for that connector's 120 Hz mode. That is the
reason to trust the 279 750.

```text
output.HDMI-A-1.modeline = 279750 1280 1328 1360 1440 720 723 727 810 +hsync -vsync
```

**Tried on the box, and the kernel took it.** `outputs` reported

```console
HDMI-A-1 1280x720@239840 scale=1 pos=0,0 primary=1 (custom)
```

and the CRTC really was scanning out at that rate: 380 flips in 1.7 s =
**219 flips/s**, `flip_interval_mean_us` **4 491** against a 4 167 µs
period. Note the reported **239 840** rather than 240 000 — CVT rounds the
clock down to its 0.25 MHz step, so the timings really are 239.84 Hz and
the server reports what the timings produce rather than what was asked
for. The desktop laid out at 720p without squashing anything (bar
1280×32, launcher 600×400, settings 560×440 with its buttons row inside
the window).

**Whether the panel syncs to it is unverified.** That is a different
question from whether the link carries it, and the only instrument for it
is a person looking at the screen — nobody was at the box. `nitro-shot`
returned a perfect 3 686 400-byte 1280×720 frame throughout, and that is
**not evidence**: it reads the shadow buffer, which is the picture the
server composed, not the picture the glass received. Recorded here as
*kernel accepted, panel sync unobserved* rather than as a working 240 Hz
mode. `docs/testbox.md` says how to retry it.

### `remote.listen`

```text
remote.listen = 127.0.0.1:7700
```

Binds a second wire listener over TCP, so an app on another machine can
put a window on this screen (`docs/remote.md`). **Absent by default, and
absent means no socket at all** — no bind, no epoll registration, no
accept path — so a desktop that does not want remote clients pays
nothing for the feature existing.

The value is an **IP literal** and a port: `127.0.0.1:7700`,
`[::1]:7700`, `0.0.0.0:7700`. Not a hostname — a listener resolved
through DNS is a foot-gun, because the name may move and the address the
server bound is then not the one the file names. Port `0` is legal and
means "ask the kernel"; the port it chose is reported by `stats` as
`remote_listen`.

> **There is no authentication.** Anyone who can reach the port can put
> windows on your screen. The supported configuration is the loopback
> address plus an SSH port-forward (`ssh -L 7700:127.0.0.1:7700 host`),
> which puts authentication in sshd where there already is some. A
> non-loopback bind logs a `warn` saying exactly this and is for
> measurement on a trusted LAN.

An empty value (`remote.listen =`) means "off", like the other keys: it
is how a settings app disables the listener without deleting the line.

On **reload** (below), the key behaves as you would want:

| the file now says | what happens |
|---|---|
| the same address | **nothing** — no rebind, and connected remote clients are undisturbed |
| a different address | rebind |
| nothing (key removed) | the listener closes; **clients already connected keep working**, because a connection lives on the socket it was accepted on |
| something unusable | a `warn` and no listener — a typo must not cost you your desktop |

### `keyboard.repeat` is deliberately absent

It is the key a reader most expects to find, so the parser names it
explicitly and warns rather than letting it fall into "unknown key".

**Nothing in nitro repeats keys yet.** libinput reports a press and a
release, the server forwards both, and no client synthesises a repeat in
between. A `keyboard.repeat = 300,25` would therefore be a promise with
nothing behind it — a setting that appears to work, changes nothing, and
costs a user an afternoon. When key repeat is implemented this key is
where it goes.

### Colours: `theme.scheme` and `theme.<role>`

`theme.scheme` picks one of two built-in palettes; each `theme.<role>`
line overrides exactly that one role on top of it. The role names are
`window_background`, `accent`, `title_bar_active`, `ansi1` — the full
table is in **`docs/theme.md`**, which is also where the wire op, the
lint rule and "how to add a role" live.

```text
theme.scheme = dark
theme.accent = #6ca8f0
theme.terminal_background = #141418
```

A reload re-derives the palette and the server pushes it to every
connected client, so the whole desktop — decorations, bar, terminal,
wallpaper, dialogs — changes colour within a frame, with nothing
restarted. A reload that leaves the palette unchanged sends nothing.

The default is **light**, deliberately: every screenshot in `docs/` was
taken on it, and a desktop that changes its appearance because a file is
missing is one that cannot be supported over the phone.

A key naming no role, or a value that is not six or eight hex digits, is
warned about and skipped like any other bad line.

### Application icons: `theme.icons`

```text
theme.icons = Adwaita
```

The **name of a directory** in the XDG icon search path
(`$XDG_DATA_HOME/icons`, `~/.icons`, `$XDG_DATA_DIRS/icons`, plus the
flat `/usr/share/pixmaps` last), not a path: a value with a `/` in it is
warned about and ignored, because a theme name is joined onto a search
directory and a separator would escape it. An empty value means the
default, like `keyboard.variant =` does.

It decides where the server looks for the **coloured application icons**
a launcher or a task list asks for by name — `firefox`, `nitro-calc`. It
has nothing to do with the desktop's own symbolic icons (the gear, the
list, the battery), which are compiled into the server and follow
`theme.scheme` like the text does. `docs/icons.md` has both halves and
the reason they are separate namespaces.

The default is `hicolor` because the icon-theme specification requires
every theme to fall back to it and every application to install its own
icon there, so it is the one theme that is *about* the applications on
the box rather than about a look. Naming a theme that is not installed
is not an error: its inheritance chain ends at `hicolor` anyway.

Only `.png` files are candidates today. A theme that ships its
application icons as SVG only — Adwaita is one — will therefore find
nothing through this key and fall back to whatever `hicolor` has;
`docs/icons.md` records why SVG is deferred.

A reload re-reads the theme **only when this key changed**, because doing
so walks the whole search path and throws away every decoded icon; a
reload that moved a monitor must not cost the launcher its icons.

`NITRO_ICON_PATH` (colon-separated absolute directories) replaces the
entire search path, `/usr/share/pixmaps` included. It exists so a test or
a demo can be hermetic; a partial override is not a fixture, because
whatever the box has installed still leaks into the answer.

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
| output mode | `NITRO_MODE=<c>=<mode>` / `NITRO_MODELINE=<c>=<timings>` | `output.<c>.mode` / `.modeline` | the connector's preferred mode |
| output position | — | `output.<c>.position` | left-to-right in connector order |
| primary output | — | `output.<c>.primary` | the first connector |
| keyboard | `XKB_DEFAULT_*` | `keyboard.*` | the `us` layout |
| colour scheme | — | `theme.scheme` | `light` |
| one colour | — | `theme.<role>` | the scheme's value |
| icon theme | `NITRO_ICON_PATH` (the search path, not the name) | `theme.icons` | `hicolor` |

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

**A missing file means defaults.** Deleting `server.conf` is a reload
like any other, and everything it used to say falls back to the
environment, the EDID or the built-in default — a `theme.scheme = dark`
in a file you just removed does not stay in force. The watch asks for
`DELETE` and `MOVED_FROM` as well as the write and rename events, which
it did not until issue #558: `rm server.conf` was silently not an event,
so a stale setting survived until something else triggered a reload. A
half-finished `mv` is answered by the reload path reading whatever is on
disk *now* — the `MOVED_TO` of the replacement arrives in the same drain
as the `MOVED_FROM` of the original, so the pair costs one reload, not
two.

On reload the server re-applies the scales (a change sends a `Configure`
to that output's windows and repaints it in full), re-lays out the
desktop positions, and rebuilds the xkb keymap — resetting the key state,
because a keymap swap invalidates whatever modifiers were being held.

A changed `output.<c>.mode` is applied **first**, because it decides what
the layout is laying out. It is the one key here whose application is a
full modeset rather than a property flip, so it is done only when the
resolved mode actually moved — see the key's own section above.

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

Four sections in one decorated window: **Displays** (one row per output
— name, mode, a scale slider, a `primary` checkbox, and x/y position
fields), **Keyboard** (layout, variant, options, and a field to type in
afterwards), **Audio** (volume and mute) and **Appearance** (the dark
scheme). **Apply** writes the file; **Revert** re-reads it.

### The window is 560×440, and both numbers are measured

The widest thing in it is a display row — 76 px of connector name, 136 of
mode string, a slider at its 64 px minimum, 30 for the scale value, 79
for the `primary` checkbox, two 54 px position fields and six 6 px gaps,
≈ 529 — so the inner width is 540.

The height is not reasoned about, it is measured: the built tree at that
width, with the height unbounded, comes to **391.2 px for one output and
exactly `ROW_HEIGHT + GAP` = 32 px more per output after it**.

| outputs | the tree needs | fits in 440 |
|---|---|---|
| 1 | 391.2 | yes, 49 px spare |
| 2 | 423.2 | yes, 17 px spare |
| 3 | 455.2 | **no** — 15 px short |

440 is the smallest round number that holds the two-monitor case, which
is what a laptop-plus-external-screen desktop is. The first draft of this
fix said 400 and claimed two outputs fit — 400 is the *one*-output figure
rounded up, and the claim was false by 23 px. That is why the table is
here and why a test asserts the two-output case rather than prose
claiming it.

This is written down because getting it wrong is not a cosmetic bug. The
window was 440×320 while its tree measured ~400 px tall, and a flex
container whose children do not fit **takes the overflow back out of
them**, weighted by size (`flex_shrink`, CSS's rule). So every direct
child of the root column was laid out smaller than it had measured:
section headings at 11.8 px instead of 17.5, which cut the descenders off
"Displays" and "Keyboard"; the two-line notes in 20 px of a needed 30;
`no audio backend found` in 10.2 px; every control row at 17.5 instead of
26. Horizontally the same arithmetic shrank the keyboard captions to
26/28/30 px — "Layout" rendering as "Layc" — and pushed the display row's
`y` field out to x=452 in a 440-wide window.

Every widget *measured* correctly throughout; each was then laid out
smaller than it measured, which is why eighteen passing tests never saw
it. Two things stop it recurring, and the first is no longer this app's
doing:

- **The toolkit makes a widget's measured size its floor** (#561). A
  child is never laid out below what it measured unless it says it can,
  and a container's measured size already sums its children — so a
  column cannot end before the rows inside it either. This dialog used
  to spell both halves itself: `shrink(0.0)` on the headings, captions,
  notes and most of a display row, *and* on the `displays` and
  `keyboard` columns, because a container of `min_height` rows that
  could itself be shrunk was laid out shorter than the rows inside it
  and drew the last one over `displays_note` — 0.8 px of overlap with
  two outputs, 15.6 px with three. Those calls are gone; the behaviour
  is the default. What is left is the small print the floor does not
  cover: the slider is the one control that reads correctly at any
  width (the toolkit gives it the `Zero` floor, and this app gives it a
  `min_width`), and the `x`/`y` position fields spell `shrink(0.0)`
  because a text field opts out too and a box too narrow for "1920" is
  not a smaller version of itself.
- The dialog declares 560×440 as the window's **minimum** via
  `SetWindowLimits`, so the server refuses a drag that would put the tree
  back into less space than it needs. There is no maximum.

`no_widget_is_laid_out_smaller_than_it_measures` and
`two_outputs_fit_the_window_and_a_third_clips_rather_than_overlaps` in
`crates/nitro-settings/tests/settings.rs` pin all of it, reading the same
bounds `hey nitro-settings list` prints.

**It does not grow for a third monitor.** A client cannot ask the server
to resize it — `Ui::resize` only re-lays the client's own tree out inside
whatever size the server gave, which is the behaviour the explicit size
exists to get (a settings dialog that resized itself when you unplugged a
monitor would be the worse bug). With three or more outputs the tree runs
past the bottom edge and is **clipped**, not squashed and not overlapped:
the rows keep their full height and the Apply/Revert row is what goes
out of view. Drag the window taller once and it stays there — the
minimum only stops it shrinking.

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
hand and value its comments, do not press Apply. The one carve-out is the
`theme.*` block, which is carried over from disk verbatim — see
*Appearance* below.

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

### Appearance

One checkbox, **Dark**, writing `theme.scheme`. Unlike every other
control in the window it does not wait for Apply: it saves on the spot,
and the desktop changes colour within a frame. A colour scheme is the one
setting you judge by looking at it, so asking the user to confirm
something they can already see would be theatre.

Per-role overrides (`theme.accent`) have no widget — thirty-odd colour
pickers for a thing done once, where the file is the better interface —
but they are **preserved**: Apply carries the whole `theme.*` block over
from disk rather than rendering it from the widgets. That is the one
exception to "Apply rewrites the file wholesale" above, and it earns it,
because the alternative is a save button that silently deletes a colour
the user hand-picked.

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
