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

The search path is rescanned on **show**, and only when a directory's
mtime moved. Re-reading a few hundred files on every keystroke would be
hundreds of syscalls per character; never re-reading them would mean
restarting the launcher after every install.

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
* **No history, no frecency, no modes.** A launcher that learns what you
  use is a good feature and a stateful one; a calculator mode is
  `nitro-calc`'s job. Neither is M3.

## Tests

`src/desktop.rs`, `src/search.rs` and `src/spawn.rs` unit-test the parts
that need no server at all: the parser (field codes, quoting, localized
names, action groups, `NoDisplay`/`Hidden`, directory precedence, the
mtime fingerprint), the ranking (subsequence, prefix order, word starts,
tight runs, the limit), and the spawn (the environment subtraction, the
process group, a real child writing a real marker file, and reaping).

`tests/launcher.rs` drives the tree the binary builds through a real
server on the shell socket, 19 cases: the bare-Super tap showing and
hiding it; the grab delivering keys past a focused second client and
releasing on hide; show and hide costing one mutation each; Escape;
typing narrowing the list; the arrows wrapping and costing two `SetText`s;
Enter and a real click each launching a real process (asserted with a
marker file, and with row 0's *callback* following its label after a
keystroke); a query that matches nothing saying so; a terminal entry
refused; a failed launch coming back with the reason; the query being
cleared on reopen; an application installed since start-up appearing; a
built-in launchable with no `.desktop` files at all; every part
addressable for `hey`; idle silence both hidden and shown; and the
centred anchor leaving the window its own size.
