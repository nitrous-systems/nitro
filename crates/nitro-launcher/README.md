# nitro-launcher

The desktop's **application launcher**: a Super-tap overlay that searches
`.desktop` files and starts what you pick.

```text
                   ┌────────────────────────────────┐
                   │ calc                           │   query
                   ├────────────────────────────────┤
                   │ ▸ Calculator                   │   results/0
                   │   KCalc                        │   results/1
                   └────────────────────────────────┘
```

Like [`nitro-bar`](../nitro-bar) it is a `nitro-ui` app that connects to
`shell.sock` rather than `wire.sock` — which is what lets it live on the
`Overlay` layer, bind a server-global hotkey and read the keyboard
without taking focus. **The socket is the capability**; see
[`docs/shell.md`](../../docs/shell.md).

```console
$ cargo run -p nitro-launcher        # against a `just fake` server
```

`NITRO_LAUNCHER_DIRS` overrides the `.desktop` search path (colon
separated); `NITRO_SHELL_SOCKET` overrides the socket.

## The three decisions

**It is built once and hidden, never rebuilt.** The window, the query
field and the result rows exist from start-up, and showing or hiding the
launcher is one `SetVisible` on the window. A launcher that opened a
window on the Super tap would pay a `CreateWindow`, a measurement round
trip per string and a first paint *while the user is already typing* —
and would miss the first keystroke. Hiding is also a complete release:
the server drops a keyboard grab and an exclusive zone when a window
stops **showing**, so there is no second message to forget.

`showing_and_hiding_is_one_mutation_each` asserts that from the outside,
by counting mutations.

**The trigger is the server's.** A bare `Super` tap and `Super+Space` are
both `BindKey` bindings. A bound chord is not delivered to the focused
client at all, so the trigger cannot be swallowed by whatever the user is
typing into — and the bare tap is a server-side state machine, because
"Super pressed and released with nothing in between" is defined by what
did *not* happen, which a client cannot see. The same binding closes the
launcher, which is precisely why a keyboard grab deliberately does not
outrank the bindings.

**Keys arrive through the grab, not through focus.** The overlay is
`NO_FOCUS`: taking focus would make the window behind it look inactive
and would move the MRU order. `the_grab_is_what_delivers_keys_and_hiding_gives_it_back`
opens a second, ordinary client to hold the focus, so the test can tell
"the grab delivered it" from "the launcher happened to be focused".

## What closes it

Escape, a second Super tap (or `Super+Space`), a successful launch, and
**another window taking focus**.

That last one is the spec's "focus-loss hides it", and it has to be
written backwards: a `NO_FOCUS` overlay cannot *lose* focus, because it
never had any — no `Focus { focused: false }` is ever coming for this
window. So the observable event is somebody **else** gaining focus, which
the window-list subscription already reports as a `WindowInfo` with
`focused: true`. Without it the launcher sits on screen holding the
keyboard grab until Escape, which is the one failure mode a launcher
really must not have.

**The trigger is a change of window *identity*, not the `focused` flag.**
That distinction is the whole of the difficulty here. The server sends a
`WindowInfo` whenever **anything** about a window changes — `clients.rs`
relists on `SetWindowTitle` and `SetAppId`, `announce_state` on a
minimize or maximize, placement on a new window — and `focused` in it
carries the *current truth* rather than a transition. So the
already-focused window merely changing its title also arrives saying
`focused: true`.

Acting on the flag alone makes the launcher vanish mid-word for no
visible reason, and it is not an exotic case: a shell sets its terminal's
title on every prompt, a browser on every page load, a clock-in-title app
on a timer. The launcher therefore remembers which `WindowRef` holds
focus and acts only when that changes;
`the_focused_window_retitling_itself_is_not_a_focus_change` retitles the
focused window three times over an open launcher and asserts it is still
up with what was typed still in it.

Two windows are then ignored: our own (the server may report the overlay
itself, and hiding on that would close the launcher the moment it
opened), and everything while the launcher is already hidden — which is
every ordinary focus change on the desktop, and must cost nothing.
`an_ordinary_focus_change_costs_a_hidden_launcher_nothing` asserts that
by counting commits while two windows steal focus from each other.

The identity bookkeeping happens **before** both of those returns, and
the ordering is load-bearing: a focus change seen while hidden still has
to be recorded, or the first change after the next show is compared
against a stale id.
`focus_is_tracked_while_hidden_so_the_next_show_is_not_stale` fails if
the two are swapped.

This is the one place the launcher subscribes to anything. It is a
subscription rather than a poll, and a failure to establish it is not
fatal: a launcher that cannot watch the window list still opens,
searches and launches, it just keeps the overlay up until Escape.

## Driving it with `hey`

Nothing below cooperates with the launcher's code — `hey` is a separate
binary talking to the introspection socket every nitro app opens.

```console
$ hey nitro-launcher list
$ hey nitro-launcher set query value calc
$ hey nitro-launcher get results/0 value     # ▸ Calculator
$ hey nitro-launcher do results/0 click      # launches it
```

That is the agentic path, and it is the *same* path a key press takes:
`set query value` runs the field's `on_change`, which re-ranks and
rewrites the rows, and `do results/0 click` runs the row's callback,
which spawns. Nothing special-cases being driven from outside, which is
the only way a scripted path stays honest.

Rows are named by **position** (`results/0`, `results/1`, …), not by the
application they show: a script asks for "the first match", which is what
a user pressing Enter gets. A row named after its current application
would change its path on every keystroke.

## Where the entries come from

| source | what |
|---|---|
| `$XDG_DATA_DIRS` + `$XDG_DATA_HOME`, each `/applications` | `.desktop` files, parsed by hand (`src/desktop.rs`) |
| the launcher's own directory | `nitro-calc`, `hello_dialog`, `nitro-demo` if present |

The built-ins are what makes the **test box** work: a freshly rsynced
`~/nitro-bin` has no `.desktop` files anywhere, and a launcher with an
empty list cannot be tested at all. A real `.desktop` file naming the
same program replaces its built-in rather than appearing beside it.

**Since #3723 the box has both**, and the shadowing above is what keeps
the list honest: `just deploy` installs `deploy/*.desktop` into
`~/.local/share/applications`, so Calculator, Files, Settings and
Terminal come from the files and appear **once** each — the packaged
entry, not the packaged entry plus the fallback. `nitro-demo` ships no
`.desktop` (a tool, not an application) and keeps its built-in.

Those files carry a **bare `Exec=`**, which is the spec's form and a
packager's, and it works on the box because `nitro-session` prepends its
own executable's directory to the `PATH` every child inherits. This
crate needs nothing for that: `Command::new("nitro-term")` is `execvp`
and the kernel does the search. Before it, installing those files was a
regression — a shadowed built-in replaced by a command that could not
run. `tests/deployed.rs` pins the shadowing, the bare-name resolution and
its control against the repository's real files.

One consequence of that shadowing is worth a pointer, because it bit this
crate once: when a file replaces a built-in, the row's icon changes
*namespace* too. A row for a scanned entry asks the server for the
entry's **app id**, not for its `Icon=` value — `row_icon`'s rustdoc has
the argument, and it is the difference between our four applications
showing their own glyphs and all four showing the generic `window`.

The search path is rescanned on **show**, and only when a directory's
mtime moved. Re-reading a few hundred files on every keystroke would be
hundreds of syscalls per character; never re-reading them would mean
restarting the launcher after every install.

### Precedence, which is reversed on the way in

`XDG_DATA_DIRS` is most-important-**first** ("the first directory listed
is the most important"), and the Desktop Entry spec resolves a
desktop-file ID to the *first* file found along it. `scan` implements
precedence the other way round — it walks the list overwriting as it
goes, so the **last** directory wins — which means the list handed to it
has to be reversed. `dirs_from` does that with one `.rev()`, and
`$XDG_DATA_HOME` is appended *after* the reversal so a user's own file
outranks every system one.

Getting this backwards is silent and wrong in a way nobody reports:
`/usr/share/applications` would shadow `/usr/local/share/applications`,
so a locally installed program is hidden by the distribution's copy of
the same file. `the_search_path_is_most_specific_last` pins the order
down *and* checks it against `scan` itself with real files on disk, so
the two halves cannot drift apart. It is a separate pure function
precisely so it can be tested: `search_dirs` reads the process
environment, and a test that set it would race every other test in the
binary.

### The parser, and what it honours

`Name`, `Exec` (with the `%f`/`%u`/… field codes stripped and the spec's
quoting honoured), `NoDisplay`, `Hidden`, `Terminal`, `Type`. Everything
else is read past. Two rules are easy to get wrong and both have tests:

* **only the `[Desktop Entry]` group counts** — a browser's
  `[Desktop Action NewPrivate]` has its own `Name` and `Exec`, and a
  parser that took the last one in the file would open a private window
  every time you searched for the browser;
* **`Name[de]` is not `Name`** — splitting on `=` and trimming
  overwrites the name with whichever translation came last.

Field codes are *removed* rather than passed on, because `firefox %U`
launched with the code intact opens a file called `%U`.

## Matching

Case-insensitive **subsequence** with a score: `fx` finds Firefox. A
substring match is strictly worse for the same code — it cannot find
`Text Editor` from `txed` — and the false positives a subsequence admits
are pushed to the bottom by the scoring rather than removed, which is the
right trade when a missing match reads as "the launcher does not work".

The bonuses, largest first: an exact name, the query as a prefix, a
character starting a word, consecutive characters. The penalties: skipped
characters, and name left over after the last match. The property that
matters more than any constant is that **an exact prefix always wins**,
because that is the case a user can predict; the rest is tuned by the
tests.

## Launching

`Command::spawn` with stdio on `/dev/null`, the environment inherited,
and `process_group(0)` so a signal aimed at the launcher's group does not
reach the application.

The child runs in **`$HOME`**, or in the directory the entry's `Path=`
names — the desktop-entry spec's default and its override. Before this it
inherited the launcher's cwd, which under the session unit is `/`, so a
terminal launched from here opened at `kaspar@ubuntu:/` (issue #571). A
home that is unset or not a directory falls back to inheriting rather than
failing every launch on the `chdir`.

`NITRO_SHELL_SOCKET` is **removed** from the child's environment: it
names the privileged socket, and an application started from the launcher
is an ordinary application. The directory is `0700` and a determined
program can still construct the default path, so this is defence in depth
rather than a boundary — but a launcher should not be the thing that
spreads it.

## Limitations

Each is a decision, not an oversight.

* **No `setsid`, only a new process group.** The spec asks for
  `fork` + `setsid` + `execvp`; the hook that would run `setsid` between
  fork and exec (`CommandExt::pre_exec`) is `unsafe`, which this tree
  denies. `process_group(0)` is the safe subset, and the difference is a
  *controlling terminal* — which the launcher does not have to pass on,
  since its own stdio is the compositor unit's journal and the child's is
  `/dev/null`.
* **A launched process is reaped by the launcher, not by init.** Without
  the double fork above it is not orphaned while the launcher lives, so
  `Children::reap` runs a non-blocking `try_wait` before every spawn and
  the launcher's exit hands the rest to init. The cost of the gap is a
  zombie — a task-table entry — for a program that exits between two
  launches.
* **`Terminal=true` entries are shown and refused.** There is no
  `nitro-term` yet. Hiding them would be "htop is missing"; running them
  with stdio on `/dev/null` would produce a process the user can neither
  see nor type at, which looks exactly like a launcher that did nothing.
  Showing them marked `(terminal)` and saying why on Enter is the honest
  third option.
* **No icons.** The toolkit has an `Image` widget but no icon *theme*
  lookup, and that is a whole feature (a theme index, inheritance, sizes,
  a fallback chain). Half of one is worse than none.
* **`Name` is shown, not `Name[<locale>]`.** Picking the right
  translation has a `LANG`-parsing tail on it; the C-locale name is
  correct in the C locale and predictable everywhere else.
* **A `.desktop` file edited in place is not noticed** until some other
  file in the directory changes: the rescan trigger is the *directory's*
  mtime, and editing a file does not move it. Packages replace files
  rather than editing them, so this self-corrects on the next install.
* **Only the top level of each `applications` directory is read.** The
  Desktop Entry spec allows nested entries, whose desktop-file ID is
  `subdir-name.desktop`; `scan` does not recurse, so those are invisible.
  Rare in practice, and the fix is a `read_dir` recursion plus the `-`
  joining rule for the ID.
* **Focus-loss hiding is "somebody else took focus"**, not a focus event
  of our own — see *What closes it*. The difference shows if a focused
  window closes without anything taking focus after it: the launcher
  stays up, because no `WindowInfo { focused: true }` arrives. Escape and
  a second tap still close it.
* **No history, no frecency, no modes.** A launcher that learns what you
  use is a good feature and a stateful one; a calculator mode is
  `nitro-calc`'s job. Neither is M3.

## Measured

On the test box (Pentium G3240, 1920x1080, `i915`), release, stripped,
with the wallpaper, the bar, the launcher and `nitro-calc` all running:

| | binary | RSS | HWM | idle CPU over 30 s |
|---|---|---|---|---|
| `nitro-launcher` | 704 544 | 2 928 kB | 2 928 kB | **0.00 %** |
| `nitro-wallpaper` | 521 880 | 2 608 kB | 2 608 kB | **0.00 %** |
| `nitro-bar` | 601 152 | 2 780 kB | 2 780 kB | **0.00 %** |
| `nitro-calc` | 566 984 | 2 756 kB | 2 756 kB | **0.00 %** |
| `nitro-server` | 2 164 024 | 19 200 kB | 35 308 kB | **0.00 %** |

Zero jiffies of CPU across thirty seconds for every one of the five,
and the server's frame counter is flat over ten seconds of idle (the
only two frames in the window are the two screenshots' own readbacks).
That is the whole point of the epoll loops: four programs on screen and
nothing running.

The server's row is bigger than the M3-C table said because `main` grew
a heap shadow buffer per output (#539) in between; none of the four
clients moved. The launcher is ~103 KB bigger than the bar, and the attribution is
mundane: `std::process::Command` and its `posix_spawn` path, plus the
`.desktop` parser and the sort. Nothing here links a font library or a
rasterizer, which is the asymmetry `docs/ui.md` describes.

Its dependency list is **`nitro-ui` and nothing else**, like
`nitro-calc`'s. The spec allowed `rustix` for `fork`/`setsid`; it turned
out not to be needed, because `Command::process_group(0)` is the safe
half of `setsid` and the half a launcher actually needs. `rustix` is a
dev-dependency, for the one test that checks a launched process really
left the launcher's process group.

### The box path, end to end

```console
$ ydotool key 125:1 125:0          # bare Super tap -> grabbed 1
$ ydotool key 46:1 46:0 30:1 30:0 38:1 38:0 46:1 46:0   # c a l c
$ hey nitro-launcher get results/0 value
▸ Calculator
$ ydotool key 28:1 28:0            # Enter -> nitro-calc starts
```

and the agentic path, which is the same path with no keyboard:

```console
$ hey nitro-launcher set query value calc
$ hey nitro-launcher do results/0 click     # a new nitro-calc pid
```

Both were run on the box against real `.desktop` files (the launcher
found `Foot`, `Foot Client` and the rest of what is installed alongside
its own built-ins).

## Tests

`src/desktop.rs`, `src/search.rs` and `src/spawn.rs` unit-test the parts
that need no server at all: the parser (field codes, quoting, localized
names, action groups, `NoDisplay`/`Hidden`, the mtime fingerprint), the
**search-path precedence** (most-specific last, the home directory
outranking everything, ragged and empty `XDG_DATA_DIRS`, and the order
checked against `scan` with real files), the ranking (subsequence, prefix
order, word starts, tight runs, the limit), and the spawn (the
environment subtraction, the process group, a real child writing a real
marker file, and reaping).

`tests/launcher.rs` drives the tree the binary builds through a real
server on the shell socket, 24 cases: the bare-Super tap showing and
hiding it; the grab delivering keys past a focused second client and
releasing on hide; **another window taking focus hiding it**, the focused window
*retitling* itself **not** hiding it, focus tracked while hidden so the
next show is not stale, and an ordinary focus change costing a hidden
launcher nothing; show and hide
costing one mutation each; Escape; typing narrowing the list; a keystroke
sending nothing for a row whose text did not change; the arrows wrapping
and costing two `SetText`s; Enter and a real click each launching a real
process (asserted with a marker file, and with row 0's *callback*
following its label after a keystroke); a query that matches nothing
saying so; a terminal entry refused; a failed launch coming back with the
reason; the query being cleared on reopen; an application installed since
start-up appearing; a built-in launchable with no `.desktop` files at
all; every part addressable for `hey`; idle silence both hidden and
shown; and the centred anchor leaving the window its own size.

`tests/deployed.rs` is the fourth file, and it is deliberately **not**
fixture-based: it reads `deploy/*.desktop` from this repository and
asserts what the rest of the desktop assumes of them — the file name, the
app id, `StartupWMClass` and `Icon=` all lining up (#3714/#3715), every
`Exec=` being a bare name, that bare name resolving against a `PATH` that
contains the binary and failing against one that does not, and the
installed files shadowing the built-ins into exactly one entry each,
idempotently across a repeated deploy.
