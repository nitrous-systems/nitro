# nitro-term

A **terminal emulator** on `nitro-ui`, and M4's first application. It is
the app that makes the test box usable, and the one that stresses text
and damage hardest — see [`docs/term.md`](../../docs/term.md) for the
model, the damage strategy and the measured numbers.

```text
┌──────────────────────────────────────────┐
│ $ ls --color                             │   TermGrid, named `grid`
│ Cargo.toml  crates/  docs/               │
│ $ █                                      │
└──────────────────────────────────────────┘
```

Run it against a server (`just fake` in another terminal):

```console
$ cargo run -p nitro-term
$ cargo run -p nitro-term -- --scrollback 50000
```

`Ctrl+Shift+Q` quits — **not** `Ctrl+Q`, which is XON and belongs to the
program inside. That is the general rule here: a terminal must not steal
a chord its child might want, so the only two bindings it keeps for
itself are that one and `Shift+PgUp`/`PgDn` for the scrollback.

## What it supports

Keys are encoded the way `xterm` encodes them, because that is what
`$TERM=xterm-256color` promises: arrows and Home/End in both normal and
application-cursor mode, F1–F12, Insert/Delete/PgUp/PgDn, `Ctrl+letter` →
C0, `Alt+x` → `ESC x`, and the xterm modifier-parameter form so
`Ctrl+Left` is a word jump.

On the output side: C0 controls, cursor movement, ED/EL, ICH/DCH/IL/DL,
SGR (bold, italic, underline, inverse, 16/256/truecolor foreground and
background, with background-colour erase), DECSTBM scroll regions, the alternate screen (1049 and the
older 47/1047/1048), cursor visibility, bracketed paste, application
cursor keys, OSC 0/2 window titles, DECSC/DECRC, and DSR/DA replies.
Anything unrecognised is ignored rather than displayed, and nothing in
the parser can panic on hostile bytes — `ten_kilobytes_of_noise_never_panic`
feeds it exactly that.

## `setsid` is a dependency

The child needs its own session and the pty as its controlling terminal,
and this tree denies `unsafe`, so `pre_exec` is not available. Instead
the shell is started as **`setsid --ctty $SHELL`** (util-linux), which
does those two syscalls in the one window where they can be done.

Without `setsid` on `$PATH` the terminal still runs, prints a warning,
and reports `has_job_control() == false` — what you lose is job control,
so `Ctrl+C` reaches nothing. The reasoning, and why this is the same
argument `nitro-launcher` makes in `src/spawn.rs` and reaches the
opposite conclusion, is in [`docs/term.md`](../../docs/term.md).

## Driving it with `hey`

The terminal is scriptable like every nitro app, and here it is more than
a demonstration: it is how the tests and the box acceptance drive it
without a keyboard.

```console
$ hey nitro-term get grid text          # the whole screen, as text
$ hey nitro-term get grid value         # the same thing
$ hey nitro-term set grid value 'ls\n'  # type it into the pty
$ hey nitro-term do grid send 'ls\n'    # the same thing, spelled as a verb
$ hey nitro-term do grid scroll_to_bottom
$ hey nitro-term shot -o tmp/term.png
```

The `\n` is a **literal backslash-n**, and it matters: the value's
C-style escapes (`\n`, `\r`, `\t`, `\e`, `\0`, `\\`) are interpreted by
the terminal, because a control character cannot be written on a command
line any other way and a terminal's scripted input is mostly control
characters. An unknown escape keeps both of its characters.

The bytes are **typed, not pasted** — even when the program has asked
for bracketed paste. bash 5.1 and later ask by default, and the entire
purpose of those markers is to tell readline that what follows is data
rather than keystrokes, so a bracketed `ls\n` sits on the prompt unrun.
A script driving a terminal is a keyboard, not a clipboard;
`do grid paste_text` is the bracketed form, for the day there is a real
clipboard.

`set grid value` and `do grid send` feed the **pty**, not the grid.
Writing into the screen behind the program's back would desynchronise the
two immediately: the shell would not know a command had been typed, and
the next thing it printed would overwrite it.

Reading the screen needs no font, no screenshot and no OCR, which is why
the integration tests assert on `get grid text` rather than on pixels.

## Environment

| | |
|---|---|
| `$SHELL` | what to run; `/bin/sh` if unset |
| `TERM` | set to `xterm-256color` for the child |
| `COLORTERM` | set to `truecolor` |
| `--scrollback N` | lines of history; default 10 000 |

`LINES` and `COLUMNS` are deliberately **removed** from the child's
environment: the kernel's `winsize` is the truth, and a stale copy in the
environment is how a resized terminal ends up drawing at the old size.

## Limitations

No clipboard, no scrollback rewrap, no mouse reporting, no sixel or
inline images, no bidi. Each is argued in
[`docs/term.md`](../../docs/term.md) rather than listed as a regret —
most of them want a server-side concept that nitro does not have yet, and
the bracketed-paste plumbing is already in place for the day the
clipboard arrives.
