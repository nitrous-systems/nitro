# Introspection: every nitro app is scriptable from outside

A nitro app opens one extra Unix socket and answers a line-based text
protocol on it. Through that socket another process can **list** the
widget tree, **read** any widget's properties, **set** them, **invoke**
its actions, **watch** it change and **screenshot** it — with no
cooperation from the app's own code beyond having widgets at all.

That is goal 5 of `DESIGN.md`: BeOS-style first-class IPC to widgets. The
CLI in `crates/nitro-hey` (`hey`) is the reference consumer; an agent, a
test harness or an accessibility bridge is the same consumer wearing a
different hat.

(The columns below are aligned for reading; on the wire they are single
tabs.)

```text
$ hey                                   # which apps are up
calc          12873
hello-dialog  12904

$ hey hello list                        # tab-separated: path role name value bounds flags
window                          container  -        -             0,0,244,132    enabled
window/label[0]                 label      -        Hello, nitro  16,16,127,23   enabled
window/message                  label      message  Nothing yet.  16,51,86,16    enabled
window/container[0]             container  -        -             16,80,140,30   enabled
window/container[0]/spacer[0]   spacer     -        -             16,80,0,0      enabled
window/container[0]/cancel      button     cancel   Cancel        24,80,75,30    focusable,enabled
window/container[0]/ok          button     ok       OK            107,80,48,30   focusable,enabled

$ hey hello do window/container[0]/ok click
$ hey hello get window/message value
OK pressed.
$ hey hello shot -o dialog.png
```

## Why this is the same tree an AT-SPI bridge will read

There is **one** tree. `Ui::introspect` walks the same arena the layout
and paint passes walk, reading the same `Widget::role`, `accessible()`
and `WidgetState` every widget already has to provide. Nothing is
mirrored, nothing is registered, and there is no way for a widget to be
in the drawn tree but not the introspected one — which is exactly the
failure mode of every toolkit that bolts accessibility on afterwards.

The vocabulary was chosen to map onto AT-SPI without a translation
table:

| nitro | AT-SPI |
|---|---|
| `Role` | `AccessibleRole` (`PUSH_BUTTON`, `CHECK_BOX`, `SLIDER`, `ENTRY`, …) |
| `Access::name` | `Accessible::name` |
| `Access::value` | `Text::getText` / `Value::currentValue` |
| `Access::actions` | `Action::getName` / `doAction` |
| `Node::bounds` | `Component::getExtents` |
| `focused` / `focusable` | `StateSet` bits |
| `watch` | `object:state-changed` / `object:text-changed` signals |

An AT-SPI bridge is therefore a process that speaks D-Bus on one side
and this socket on the other, and it needs nothing new from the toolkit.
It is not in M2 because the D-Bus dependency is not, but the shape it
would consume is what is specified here.

And the same tree is what makes an app testable *from outside the
process*: `hey app do window/ok click` runs the real click path, real
callbacks, real invalidation — not a simulation of it.

## Where the socket is

```text
$XDG_RUNTIME_DIR/nitro/apps/<app-name>.<pid>.sock
```

The directory is created `0700`, so only the user who owns it can even
see the sockets, let alone connect. Without `XDG_RUNTIME_DIR` (or with a
relative one) the path falls back to `/tmp/nitro-<uid>/apps/`, the same
rule `nitro-server`'s control socket uses. `NITRO_APPS_DIR` overrides the
directory outright, which is what the tests use.

`<app-name>` is the name passed to `App::new`, with every character
outside `[A-Za-z0-9._-]` replaced by `_`; the pid disambiguates two
copies of the same app. The socket is unlinked when the app exits
normally.

### Sockets that outlive their app

A `SIGTERM` or a `SIGKILL` does not run the app's cleanup, so the socket
file stays. `nitro-session` *restarts* a shell piece that dies, so those
leftovers used to accumulate by themselves, and the directory would end
up holding one live bar and seven ghosts — at which point `hey nitro-bar
list` refused to run, because the name was "ambiguous" between apps that
no longer existed (#536, #544). The debugging tool you reach for when the
bar misbehaves must not be the tool that stops working once it has.

A socket is a **live app** when both halves hold:

| test | catches |
|---|---|
| `/proc/<pid>` exists | the ordinary case: the app is gone |
| `connect()` succeeds | the app is gone and its pid was reused |

Neither alone is enough. The pid is in the file name, so it is the cheap
first filter; but a pid is reused, and then only a `connect` tells a live
app from a dead one's namesake. `connect` is not a timeout, either:
`listen(2)` queues the connection in the kernel whether or not the app is
currently in `accept`, so a busy app is never mistaken for a dead one,
and `ECONNREFUSED` means there is no listener at all.

Leftovers are pruned at the two moments that matter:

* **`nitro_ui::introspect::Socket::bind`** sweeps the sibling
  `<name>.*.sock` whose pid is dead before binding its own. A restarted
  app therefore buries its own corpse, and the box recovers without
  anyone running anything. Only the pid is consulted here — there is no
  listener to ask yet — and only sockets of the same name, because
  another app's leftovers are not this app's business.
* **`hey`**, on every invocation, before it decides anything: before a
  name is judged ambiguous, and before a listing is printed, since a
  listing that names dead apps is the same lie in a friendlier voice.
  Both halves of the test run here, and a socket that fails either is
  unlinked (best effort, silently) and dropped from the set.
  `nitro_ui::introspect::list_apps` does the same for in-process
  readers.

So a name is only **ambiguous** when two apps of that name are genuinely
running, and then `hey` says which selector picks one:

```text
$ hey nitro-bar list
hey: `nitro-bar` is ambiguous: nitro-bar.217436, nitro-bar.218864; name one, e.g. `hey nitro-bar.217436 …`
$ hey nitro-bar.218864 list
```

`<name>.<pid>` — the socket's own file name without `.sock` — is accepted
wherever an app is named, alongside a bare pid, an exact name and a
unique name prefix.

The toolkit installs **no signal handler**: an app killed with `SIGKILL`
can never clean up after itself, so the reader has to be robust anyway,
and once it is, a handler would only make the tidy case tidier at the
cost of a signal handler in every app that links the toolkit. The
leftover of a killed app is removed by that app's next start, or by the
next `hey`, whichever comes first.

**Security in M2 is the directory mode and nothing else**: any process of
the same user that can open the socket can drive the app completely. That
is the same trust boundary as the X11 socket, `$XDG_RUNTIME_DIR/wayland-0`
or the server's own control socket, and it is deliberately not more:
a per-app allow policy (a capability token handed to the launcher, an
`accept`-time credential check with a rule file) is M3+, and belongs with
the session manager that would issue the tokens. An app that wants no
socket at all calls `App::introspect(false)`.

## The loop it runs on

The **listener** is registered in the app's own `epoll` set, and requests
are executed **between events, on the app thread**, by the same loop that
runs the app's callbacks. Nothing is locked, nothing is cloned, no
request can observe a half-laid-out tree, and a callback triggered by
`do` runs with exactly the `&mut S` and `&mut Ui<S>` a real click would
give it.

That is the BeOS property: IPC and the application share one message
loop, so "scriptable" costs neither a thread nor a lock.

After a batch of requests the app flushes as usual, so a `set` or a `do`
produces exactly the mutations the equivalent user input would.

**Connected clients are polled, not registered**, and that is a wart
worth stating plainly. Only the listener has an `epoll` token; while at
least one client is connected the loop clamps its `epoll_wait` timeout to
10 ms and calls `Socket::serve` on each turn. So an app with a `hey
watch` attached wakes **100 times a second** — which is in direct tension
with the "blocks in `epoll_wait` and no bytes move" property `docs/ui.md`
claims a few sections away, and the claim holds only while nothing is
connected. An idle app with no client registers nothing extra, waits
exactly as before and wakes not at all.

The reason is bookkeeping rather than principle: clients come and go
constantly, and each would need an `epoll` token allocated, tracked and
deleted, in a loop whose token space is currently three constants and a
raw fd. Registering each client stream (and reverting the timeout clamp)
is a contained change and the right one; it is M3.

## The protocol

One request per line, ASCII, `\n`-terminated. Every reply begins with a
status line — `ok`, `ok <args>` or `err <message>` — and every reply
except `shot`'s pixel body and `watch`'s stream is terminated by an
**empty line**. Lines are at most 4096 bytes; a longer one drops the
connection. Fields inside a line are separated by tabs; a tab, newline or
backslash inside a value is escaped `\t`, `\n`, `\\`.

| request | reply |
|---|---|
| `list [path]` | `ok\n` + one line per widget + blank line |
| `get <path> [prop]` | `ok\n` + `prop<TAB>value` lines (or the bare value) + blank line |
| `set <path> <prop> <value>` | `ok\n\n` or `err <msg>\n\n` |
| `do <path> <action> [arg]` | `ok\n\n` or `err <msg>\n\n` |
| `watch <path\|*>` | `ok\n` then `event …` lines until the connection closes |
| `shot` | `ok <w> <h> <stride>\n` + `stride*h` bytes of `XRGB8888` |
| `tree` | as `list`, but indented by depth — for humans |
| `quit` | `ok\n\n`, then the app exits |

### `list [path]`

One line per widget, in tree order, rooted at `path` (default: the whole
tree):

```text
<path>\t<role>\t<name or ->\t<value or ->\t<x,y,w,h>\t<flags>
```

`flags` is a comma-separated subset of `focused`, `hovered`, `enabled`,
or `-` when none apply. Bounds are logical pixels in window coordinates,
rounded to integers.

### `get <path> [prop]`

Without a property, every property of the widget, one `prop<TAB>value`
line each:

| prop | meaning |
|---|---|
| `path` | the widget's own path |
| `role` | `button`, `label`, `textfield`, `checkbox`, `scroll`, `slider`, `separator`, `image`, `container`, `spacer`, `other` |
| `name` | addressing/accessible name, or `-` |
| `value` | the widget's value as text, or `-` |
| `text` | `value` for widgets whose value *is* their text (label, button, text field) |
| `bounds` | `x,y,w,h` in window coordinates |
| `enabled`, `focused`, `hovered`, `focusable` | `true`/`false` |
| `children` | number of children |
| `actions` | comma-separated action names |

With a property, the bare value on one line — which is what makes
`hey app get window/ok value` usable in a shell.

### `set <path> <prop> <value>`

Applied through the **same `WidgetMut` setter the app itself would
call**, so invalidation is identical: `set window/msg text Hi` marks
layout dirty exactly as `set_text` does from a callback. `set <prop>` is
the action `set_<prop>`, so anything `set` can do `do` can do too.

Framework-level properties (`name`, `focusable`, `focused`) are handled
by the framework; everything else is the widget's own.

### `do <path> <action> [arg]`

Runs the action with the app's state and the whole tree available, i.e.
the callbacks fire:

| role | actions |
|---|---|
| any focusable | `focus` |
| `button` | `click` (alias `activate`) |
| `checkbox` | `toggle`, `set_value <true\|false>` |
| `textfield` | `set_value <text>`, `submit`, `clear` |
| `slider` | `set_value <number>` |
| `scroll` | `scroll_to <offset>`, `scroll_by <delta>` |
| any | `set_name <text>` |

An unknown action is `err unknown action`, never a panic, and a `do`
without both a path and an action says what one looks like:

```text
$ hey calc do 7
do needs a path and an action, e.g. `hey calc do window/container[1]/7 click`
```

### `watch <path|*>`

Streams one line per change until the peer closes the connection:

```text
event <path> <kind> <value>
```

`kind` is `value` (the widget's value changed, however it changed),
`focus` (`true`/`false`), or `click` (with an empty value). `*` watches
the whole tree; a path watches that widget and its descendants.

`value` and `focus` are detected by **diffing the introspection snapshot**
before and after each batch, which is why a change made by the app's own
code, by real input, or by another client's `set` all report identically
— there is no way for a widget to change quietly. The cost of that is
that a value which changes and changes back within one loop turn is not
reported, and the diff is O(widgets) per turn *while a watcher is
connected*; an unwatched app takes no snapshot and pays nothing.

`click` cannot work that way, because a button that ran its callback
looks exactly like one that did not. A widget therefore **announces** its
own activation (`EventCx::report_activation`), and the socket turns those
into `click` events. One click on a button with a callback produces two
events in one batch: the `click`, then whatever `value` the callback
changed.

### A note on bounds

`bounds` is the rectangle the widget is **drawn** at, in window
coordinates: ancestors contribute their origin *and* their content
transform, so a widget inside a scrolled viewport reports where it
currently is, not where it would be unscrolled. That is what makes
`bounds` usable for "point at this widget" and what lets it stand in for
AT-SPI's `Component::getExtents`. A widget scrolled out of its viewport
reports a rectangle outside the viewport rather than being hidden — the
clip is a paint-time fact, and `list` does not filter by visibility.

### `shot`

`ok <w> <h> <stride>` and then `stride × h` bytes of `XRGB8888`: the
server's control `shot` of the whole output, **cropped to this window**.
The crop needs the window's position, which the app keeps from the
`Configure` message the server sends when it places or moves the window.

`shot` is the one request that talks to the server's control socket, and
it is also the one that can fail for a reason that is nobody's fault
(no server control socket in the environment) — then it is
`err shot: <reason>`.

## Paths

```text
path    := segment ( "/" segment )*
segment := name | role "[" index "]"
```

The root widget is always `window`. Below it, a widget with an
addressable **name** (set with `.name("ok")` or `WidgetMut::set_name`) is
addressed by that name; a widget without one is `role[i]`, where `i`
counts from 0 **among the siblings of the same role**. So the third
button of a row is `button[2]` whether or not there are labels between
them, and adding a label to that row does not renumber it.

A name is addressable when it is non-empty and contains no whitespace,
no `/` and no `[`; a name that is not (a label's text, say, which is its
*accessible* name) is ignored for addressing and the widget keeps its
`role[i]` segment. Names are matched before roles, so `window/ok` finds
the widget named `ok` even if some sibling would also answer to
`button[0]`.

Paths are resolved against the live tree on every request; nothing caches
them. A widget that has been removed is simply `err no such widget`.

## Cost

The introspection code is compiled into every app, so its size matters.
Measured on `examples/hello_dialog.rs`, release, stripped:

| build | binary | delta |
|---|---|---|
| with the socket (the default) | 522 112 | — |
| `App::run` never binding it | 514 952 | −7 160 |
| the `introspect` and `shot` modules removed | 453 600 | −68 512 |

So the socket costs **+77 504 bytes (+17 %)** over the toolkit without
it, and **+76 kB of RSS** — which is resident text, not data: nothing is
allocated until a client connects, and the `watch` snapshot is freed when
the last watcher goes.

That is bigger than "small", and the reason is that the protocol is
**monomorphised per app-state type**: every function that holds a `&mut
Ui<S>` — `Socket::serve`, `list`, `get`, `invoke`, `path_of` — is
instantiated afresh for each app, and `Socket::serve` alone is 24 770
bytes, the largest single symbol in the binary. Routing the protocol
through a `&mut dyn IntrospectTree` would collapse it to one copy for the
whole program at the cost of one virtual call per request, on a request
rate measured in tens per second. That is the M3 change and it is
mechanical: the protocol already touches the tree through eight methods.

The new widgets, by contrast, cost almost nothing: `nitro_ui::widgets::*`
is 10 960 bytes for all eleven, only 558 more than the five that were
there before — `TextField`, `Checkbox`, `Slider`, `Scroll`, `Separator`
and `Image` share the paint, measure and theme helpers the earlier
widgets already had.

It is on by default anyway, and that is a deliberate trade: an app that
has to opt in to being scriptable is an app that nothing can drive, and
the point of goal 5 is that *every* nitro app is scriptable through one
mechanism. `App::introspect(false)` is there for the app that disagrees.

The full argument, with the symbol-level attribution, is under *Measured*
in `docs/ui.md`.
