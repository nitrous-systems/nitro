# nitro-greeter

The lock screen and greetd's greeter: one nitro-ui app that renders
PAM's conversation (whatever it asks: password, one-time code, "touch your
key") instead of a hard-coded password box. See `docs/greeter.md`,
decisions 4 and 6.

```text
┌──────────────────────────────┐
│            14:05             │  clock
│  [ alice                  ]  │  user     (Enter submits)
│  Password:                   │  prompt   (PAM's text)
│  [ ••••••                 ]  │  answer   (masked for `secret`)
│  Authentication failure      │  message  (notices and errors)
│  Checking…                   │  status   (while waiting)
│  [ Log out ]                 │  logout   (another user's name only)
│  [ Nitro ]                   │  session  (greeter: click cycles)
│ [Suspend][Restart][Power off]│  power    (greeter only)
└──────────────────────────────┘
```

## Greeter mode

```console
$ nitro-greeter          # under greetd, via `nitro-session --greeter`
```

The greeter piece (`Role::Primary`) of `nitro-session --greeter`, which
greetd runs as its greeter user (`deploy/greetd/`). It speaks greetd's
IPC on `$GREETD_SOCK` (`src/greetd.rs`; the codec is `nitro-login`'s).
Without the variable it exits 1. Same window as lock mode
(`Surface::lock()`), no `Lock`. On start it sends `cancel_session`, since
a crashed predecessor may have left greetd mid-conversation. Then it
prefills the remembered user and asks for them at once, or focuses the
name field. After `success` it sends `start_session` with the chosen
session's command and `XDG_SESSION_DESKTOP`/`XDG_CURRENT_DESKTOP`/
`XDG_SESSION_TYPE`. greetd's `success` to that saves the user and
session to `/var/cache/nitro-greeter/state` (`NITRO_GREETER_STATE`), and
the greeter exits 0, which ends its session so greetd starts the user's.
An error returns to the name field with the reason, and cancels. A wrong
password asks again for the same name.

- **Sessions** (`src/sessions.rs`): the built-in *Nitro* (`nitro-session`
  next to this binary, under `systemd-cat` so its log reaches the
  journal), then `/usr/share/wayland-sessions/*.desktop` read with
  `nitro-launcher`'s parser. A `nitro.desktop` that runs nitro-session is
  skipped as a duplicate. The `session` button shows the current session,
  and a click moves to the next one.
- **Power**: `suspend`, `reboot`, `poweroff` on the greeter session's own
  `session.sock`. A failure shows on the message line.

## Lock mode

```console
$ nitro-greeter --lock
```

What `nitro-session --locked` and the bar's Lock action (and Super+L)
run: the session spawns it as its on-demand lock piece, after locking at
the server itself, restarts it if it crashes and leaves it be when it
exits 0. It connects to the **shell** socket, opens `Surface::lock()` (an
undecorated, focusable overlay covering the output), sends `Lock`
(`Ui::lock_session`), which takes over an ownerless lock, and starts a
conversation for the session's owner, so the password prompt has the
keyboard at once. A wrong password shows PAM's reason and asks again for
the same name. A failure the user typed nothing towards (a locked or
expired account) is not retried by itself, which would spin PAM
transactions; the reason stays up and Enter in the name field retries. Success sends `Unlock`, closes the helper and exits 0.

A different name starts **no** conversation: another user never types a
password into this session. The screen says "Only alice can unlock this
session. Log out to sign in as bob — unsaved work will be lost." and
offers `Log out` (`session.sock`'s `logout`).

## Pieces

- `src/conv.rs`: the pure state machine (`User → Waiting → Prompt →
  Authenticated`). No I/O. It tracks which requests are unanswered, so the
  `success` that answers a `cancel_session` (and late answers of an
  abandoned conversation) are not taken for an authentication. Tested
  against scripted conversations.
- `src/backend.rs`: the `Backend` trait and `AuthHelper`, which spawns
  `nitro-auth` (`$NITRO_AUTH`, else next to this executable, else `PATH`)
  with piped stdio, reads it non-blocking through `ui.add_fd`, and
  respawns it on the next attempt if it died.
- `src/greetd.rs`: the greetd `Backend`: a `UnixStream` to `$GREETD_SOCK`,
  connected lazily and again after a loss, blocking writes,
  `recv(DONTWAIT)` reads.
- `src/sessions.rs`, `src/state.rs`: the session list and the remembered
  defaults.
- `src/lib.rs`: the tree, and `on_response`, the single entry the fd hook
  and the tests use.

## Trying it

Against `just fake` with a locked server:

```console
$ NITRO_LOCKED=1 just fake          # another terminal
$ cargo build -p nitro-auth && cargo run -p nitro-greeter -- --lock
```

`hey nitro-greeter get window/message value` reads the message line;
`window/answer`'s value is always the mask. Names: `clock`, `user`,
`prompt`, `answer`, `message`, `status`, `logout`, `session`, `suspend`,
`reboot`, `poweroff`. A text field is driven with `set … value` and
`do … submit`.

## Not yet

A user list, a real session list widget (the picker is a cycling
button), and multiple outputs: other outputs show only the
background, because the server draws nothing but the lock owner's
windows.
