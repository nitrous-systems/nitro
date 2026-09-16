# nitro-session

The process that turns "the box booted" into "there is a desktop", and
the one that takes it away again. **M3-E, and M3's exit.**

```text
              nitro-session                 ← systemd runs this, on tty2
                    │
      ┌─────────────┼──────────────┬──────────────┐
      ▼             ▼              ▼              ▼
 nitro-server  nitro-wallpaper  nitro-bar   nitro-launcher
   (the VT,      Background       Top layer    Overlay layer
    DRM master)   layer           + zone       + Super hotkey
```

It starts them in that order, waits for the compositor to really be
answering before starting anything that talks to it, restarts a shell
piece that dies, ends the whole session when the *server* exits, and
tears down in reverse order on `SIGTERM`. It also answers the power
actions — `suspend`, `poweroff`, `reboot`, `logout`, and an M4 `lock` —
on `$XDG_RUNTIME_DIR/nitro/session.sock`.

```console
$ nitro-session                              # the whole desktop
$ NITRO_SESSION_PIECES= nitro-session        # the compositor alone
$ NITRO_SESSION_PIECES=nitro-bar nitro-session   # …and just the bar
$ printf 'status\n' | nc -U $XDG_RUNTIME_DIR/nitro/session.sock
ok
nitro-server 1841
nitro-wallpaper 1855
nitro-bar 1856
nitro-launcher 1857
```

## The five decisions

### 1. Readiness is a handshake, not a file

The obvious "is the server up?" test is *does `wire.sock` exist*, and it
is wrong twice. `bind(2)` creates the file and `listen(2)` is what makes
a `connect(2)` succeed, so there is a window where the file is there and
clients are refused. Worse, a **stale** socket file from a session that
was `SIGKILL`ed exists forever and refuses connections forever — the file
test would report "ready" for a server that is not running at all, and
the three shell pieces would then die one by one in the first second with
three different-looking errors.

So the check is a real `nitro-wire` client connection with a real
handshake, against **both** sockets (they are bound at different moments,
and the shell pieces need the second one). It is the same code path every
shell piece is about to take, which is the property worth having: the
session cannot conclude "ready" through a path the pieces do not use.

`a_stale_socket_file_is_not_mistaken_for_a_server` is the test, and it
exists because a file check would have passed every other test in
`src/wait.rs`.

The probe is bounded by 500 ms of its own (`PROBE_TIMEOUT`), because
`nitro-wire`'s handshake waits for the `Welcome` with an unbounded
`poll`: a compositor that has `listen`ed but wedged before its accept
loop would otherwise hang the *session*, which is strictly worse than a
session that reports a timeout.

### 2. A child's exit is a descriptor

One `poll(2)` over the session socket, the signal pipe and **one pidfd
per running child**. A pidfd becomes readable when the process it names
exits, so "the bar died" arrives the same way "a client connected" does,
and the session is asleep in the kernel in between. The steady state is
**zero wakeups**, which is the only acceptable idle cost for the
longest-lived process in the desktop.

`SIGCHLD` would have worked and is worse here: it says *a* child changed
state, and the `waitpid(-1)` that follows cannot tell "the piece I
supervise exited" from "something reparented onto me exited". A poll
timer would have worked and is worse for the obvious reason — a wakeup
per interval, forever, on a box whose whole idle claim is 0.0 % with zero
context switches.

### 3. The server is never restarted

A shell piece can be replaced because its state is *derived*: the bar
re-reads the window list, the wallpaper repaints, the launcher rebuilds
its index. The server's state **is the desktop** — every window of every
application is a client connection to it. A server that exited has taken
all of them with it, and a "restarted" desktop with an empty screen is
not a recovery, it is a data-loss event dressed as one.

So the server's exit ends the session, with the server's exit code, and
what to do next belongs to whatever started it. `systemd` has a
`Restart=` line for exactly that judgement, and on the test box it is
deliberately `no`.

### 4. Backoff resets on evidence, not on a timer

1 s, doubling, capped at 30 s; reset by **a run that outlived the cap**.
No delay at all turns a bar with a missing font into a fork bomb — on a
3.3 GB box with two cores and no swap, the difference between "the bar is
missing" and "the box is gone". A delay that only ever grows turns one
bad afternoon into a desktop that takes half a minute to put its bar back
a week later.

The reset condition is the one fact the supervisor actually has: how long
the last run lasted. A run longer than the cap is evidence the delay we
imposed was already enough. There is deliberately **no give-up**: a
desktop with no bar cannot be fixed from inside, 30 s of patience costs
one wakeup per 30 s, and it is what lets a `just deploy` of a fixed
binary be picked up by the running session.

### 5. Teardown is reverse order with one shared deadline

Reverse, because **a piece must not outlive the thing it talks to**.
Stopping the server first would leave three toolkit clients blocking on a
socket that has gone, each logging a fatal-looking connection error
during what is supposed to be a clean shutdown.

One deadline (3 s) rather than one per piece, because four pieces × 5 s
is 20 s and the unit's `TimeoutStopSec` is 5: systemd would `SIGKILL` the
session mid-teardown and the compositor would be left holding DRM master
with nothing to release it. The *signalling* is strictly ordered; the
*waiting* is not, and what matters is that the launcher is asked to stop
before the server is.

## Power actions: `systemctl`, not D-Bus

`DESIGN.md` says session policy that talks to logind lives in one side
daemon and that it "is the only place a D-Bus client is allowed". This is
that daemon, and it still does not speak D-Bus. The permission was
spent, not the requirement:

1. **`zbus` is ~40 crates.** The tree is 37 distinct external crates
   today, and was 34 when this argument was written — it was never 35.
   That figure came from counting with a bare
   `awk '{print $1}'` over `cargo tree`, which turns the blank line
   separating each root's subtree into an empty string that `sort -u`
   then keeps: one too many. The right command is
   `cargo tree -e normal --prefix none | awk 'NF{print $1}' | grep -v '^nitro-' | sort -u | wc -l`
   (`docs/budget.md` has the full decomposition). Either way, doubling the
   tree to send four method calls a user makes twice a day is the worst
   dependency trade available in this repo — "~40 against 37" is the same
   argument.
2. **`systemctl` is already there**, and it *is* a logind call:
   `systemctl suspend` goes to `org.freedesktop.login1.Manager` with the
   same polkit check, the same inhibitor handling and the same "another
   session is active" refusal. We are not avoiding logind; we are letting
   the tool that ships with it do the IPC.
3. **What D-Bus would buy is not wanted yet.** What a real client gets
   over `systemctl` is *events*: `PrepareForSleep`, `Lock`/`Unlock`,
   idle hints, and an inhibitor fd held across a suspend so a lock screen
   can paint before the machine goes down. Every one of those is M4.

The honest cost: a `systemctl suspend` is a fork, an exec and a D-Bus
round trip inside someone else's process (~20 ms rather than ~2 ms), it
can fail for reasons we can only report as text, and the session cannot
be told the machine is *about to* sleep.

**When it gets revisited:** the first requirement `systemctl` genuinely
cannot meet is a lock screen that must paint before suspend. That is the
same milestone in which `lock` stops returning `err`, and that is not a
coincidence — `lock` is the one action that needs the event, which is why
it is the one action not implemented here.

## The protocol

One request per line, ASCII, `\n`-terminated; the reply is `ok\n` or
`err <reason>\n`. It is `nitro-server`'s control socket again, down to
the `MAX_LINE` overflow rule, because the bar will eventually speak both
and a desktop with two hand-rolled line protocols that differ in their
details is a desktop with one of them written wrong.

| request | effect |
|---|---|
| `lock` | M4. `err not implemented …` today. |
| `suspend` | `systemctl suspend` |
| `poweroff` | `systemctl poweroff` |
| `reboot` | `systemctl reboot` |
| `logout` | orderly teardown, exit 0 |
| `status` | `ok` + one `name pid` line per piece + a blank line |

A verb with an argument is **refused**, not truncated to the verb:
`poweroff now` from a client that thinks this is a shell must not power
the box off. An unknown verb closes the connection, like every other
protocol error in this tree.

The socket lives in the `0700` runtime directory, and that directory is
the whole access control. It is enough, and the reason is worth stating
plainly: a process that can reach this socket can also run `systemctl
poweroff` itself. The session adds no privilege — it adds a *name* for
the action, so a bar does not have to carry a process spawn.

## Environment

| variable | effect |
|---|---|
| `NITRO_SOCKET` / `NITRO_SHELL_SOCKET` | the sockets waited for; the same variables the server binds and clients connect to |
| `NITRO_SESSION_SOCKET` | overrides the session socket path |
| `NITRO_SESSION_BIN_DIR` | where the pieces are looked for before `$PATH`, and what is prepended to the children's `PATH` |
| `NITRO_SESSION_PIECES` | which shell pieces to start (comma separated; empty = the server alone) |
| `NITRO_LOG` | `error\|warn\|info\|debug`, same levels and format as the server |

Everything else a piece needs it reads itself: the session passes its
environment on unchanged, so `NITRO_BACKEND=fake` in the unit reaches the
server and `NITRO_FONT_DIRS` reaches the text stack without this crate
knowing either variable exists.

`NITRO_SESSION_PIECES` is a **subset selector**, not a way to have the
session start an arbitrary program: a name that is not one of the three
shell pieces is warned about and ignored. The session runs at the same
privilege as the compositor, and "supervise anything the environment
names" is not a property a process in that position should have.

## Where the binaries come from

Next to `nitro-session` itself (via `current_exe`), then `$PATH`. The
test box's `~/nitro-bin` is a flat rsync target that is not on `$PATH`,
and `just deploy` replaces all of them at once; a session that searched
`$PATH` first could start yesterday's installed bar next to today's
server. A sibling that is not an executable *file* falls through to
`$PATH` rather than becoming an `EACCES` at spawn time, which is the
half-rsynced-file case.

## …and the children get that directory on their `PATH`

The same directory is **prepended to the `PATH` every child inherits**
(`pieces::path_with_bin_dir`). The lookup above answers "where is the
bar?" for the session; this answers it for everything the session's
children go on to start, and the case that forced it is the launcher's.

A `.desktop` file's `Exec=` is a bare program name — the spec's form, and
what a packager ships, because on an ordinary system the binary is in
`/usr/bin`. On the box it is in `~/nitro-bin`, so `Exec=nitro-term` was
an `execvp` that could only fail; and since a `.desktop` file shadows the
launcher's built-in entry for the same program, installing
`deploy/nitro-term.desktop` replaced a working launcher entry with
`spawn: No such file or directory`. That is why `just deploy` refused to
install the files at all, and why the box showed the generic `window`
icon for every one of our applications even after the server learned to
resolve `app_id` → `<app_id>.desktop` → `Icon=` (#3715).

The session is the process that knows where the desktop's binaries are,
so it is the process that says so. **Prepended**, not appended, for the
same reason the sibling lookup wins over `$PATH`: a stale
`/usr/local/bin/nitro-term` must lose to the binary deployed beside the
running session. And because it is the session's *own* directory, an
installed `/usr/bin/nitro-session` contributes `/usr/bin` — already
there, so the rewrite is skipped entirely and the environment a packaged
desktop's children see is untouched.

Nothing else about the environment is edited; `NITRO_SESSION_BIN_DIR`
moves both halves together, since it is the directory the session looks
in.

## stdio

stdout and stderr are inherited by every piece, so the whole desktop
writes to one journal in the order it happened: `journalctl -u nitro-dev
-f` is the entire debugging story on the box. The session does **not**
own a pipe per child — a supervisor blocked in `poll` while a child fills
a pipe it never drains is a deadlock waiting for a verbose client.

stdin is inherited **only by the server**, and that is load-bearing: the
unit hands the session `/dev/tty2` (`StandardInput=tty-force`) and it is
the compositor that needs to be on a VT. A shell client with that tty on
stdin would be a toolkit app that can be Ctrl-C'd from a keyboard the
compositor also owns.

## Dependencies

`rustix`, `nitro-wire`, `signal-hook` — and **no new crate in the tree**:
all three are already here. The spec allowed `rustix` + `nitro-wire`
only; `signal-hook` is the one addition, for SIGTERM → a descriptor
without `unsafe`. The alternative inside `rustix` is
`runtime::kernel_sigaction`, which is `unsafe`, `doc(hidden)` and
documented as unusable in a process that has a libc — which this one
does.

The logging module is `nitro-server`'s, copied. The alternative is a
dependency on `nitro-server` from the process that *starts*
`nitro-server`, which would link libseat, libinput, xkbcommon and the
whole compositor into a supervisor whose job is `fork`, `exec` and
`poll`. Sixty lines of `writeln!` is the cheaper half of that trade.

## Tests

`src/` unit-tests the parts that are pure: the backoff sequence and its
reset boundary, the start/teardown order, path resolution (sibling vs
`$PATH`, including the non-executable and the directory cases), the
`PATH` the children inherit (prepended, the empty and already-first
no-ops) and a real child proving a **bare name resolves** through it with
a control that shows it does not without, the readiness timeout (with the
stale-socket and wedged-listener traps), the command parser, and the line
buffering.

`tests/session.rs` runs a **real** `Session` — real fork/exec, real
pidfds, the real poll loop, the real handshake probe — against
`examples/stub_child`, a supervisable stand-in that marks when it starts
and when it is asked to stop and crashes on request. Eight tests: start
order, restart with a growing delay, reverse teardown order, a piece that
ignores `SIGTERM` being killed inside the deadline, the server's exit
ending the session with its code, the socket's `status`/`lock`/`logout`
answers, a server that never becomes ready failing the start with
nothing left running, and a server that dies mid-startup being noticed
at once instead of at the timeout.

One environment note recorded there: a **signal mask is inherited across
fork and exec**, so in a sandbox that blocks `SIGTERM` the stubs are born
unable to receive it. Teardown still completes (through the `SIGKILL` at
the deadline) but the `term` marks never appear, so the order assertions
detect that case by actually signalling a stub and report themselves
skipped rather than passing vacuously.
