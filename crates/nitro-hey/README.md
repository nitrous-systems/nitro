# nitro-hey

`hey` — drive any nitro app from the command line.

Every nitro app opens an introspection socket and answers a line protocol
on it (see [`docs/introspection.md`](../../docs/introspection.md)). `hey`
is the thing that speaks it, which makes "click the OK button" a shell
command and makes every app scriptable and testable without the app
knowing anything about it.

```text
hey                                    list running apps
hey <app> list [path]                  the widget tree
hey <app> tree [path]                  the same, indented for humans
hey <app> get <path> [prop]            one widget's properties
hey <app> set <path> <prop> <value>    change one
hey <app> do <path> <action> [arg]     invoke an action
hey <app> watch [path|*]               stream changes until Ctrl-C
hey <app> shot [-o FILE] [--raw]       screenshot that window
hey <app> quit                         ask the app to exit
```

```console
$ hey
calc          12873
hello-dialog  12904

$ hey hello list
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

The columns are single tabs on the wire; they are aligned above for
reading. The fields are path, role, addressing name, value, `x,y,w,h`
and flags.

`<app>` matches by **pid**, by **exact name**, or by a **unique name
prefix** — an ambiguous prefix is an error rather than a guess, because a
tool that silently picked one of two apps would be worse than one that
refused.

Exit codes: **0** ok, **1** the app answered `err` (its message goes to
stderr), **2** no such app.

## Dependencies

`std` and `rustix`, and nothing else — not even `nitro-ui`, whose
`introspect` module it could have shared a path resolver and an escaping
function with. This is the tool you reach for when something is already
wrong, so it should build and run when as little as possible is working.
Two dozen duplicated lines is the price; the tests on both sides pin the
shared format.

Its PNG writer is a copy of `nitro-shot`'s stored-deflate encoder for the
same reason: moving those 120 lines into `nitro-core` would put a PNG
encoder in the dependency graph of the server, the toolkit and every app,
to save two CLIs a copy each. Revisit at a third consumer.

## Where the sockets are

`$NITRO_APPS_DIR`, else `$XDG_RUNTIME_DIR/nitro/apps`, else
`/tmp/nitro-<uid>/apps` — the same rule `nitro-server`'s control socket
follows, one level deeper, so listing the apps is reading one directory.
The directory is `0700`, which is the whole of M2's security model: any
process of the same user can drive any app completely, exactly as with
the X11 socket. A per-app allow policy is M3+.

## Testing

`tests/end_to_end.rs` runs a real app on a real server (the `nitro-ui`
harness, fake backend, in-process) and drives it with the **real `hey`
binary as a child process**: `do … click` flips a label and `get` proves
it, `set` changes a text field, `watch` sees a change another client
made, and `shot` writes a PNG whose IHDR dimensions equal the window's.
Nothing in that test reaches into the app — every assertion is made by
asking `hey`, which is the claim the socket exists to support.
