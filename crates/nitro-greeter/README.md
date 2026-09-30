# nitro-greeter

The lock screen, and later greetd's greeter: one nitro-ui app that renders
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
└──────────────────────────────┘
```

## Lock mode

```console
$ nitro-greeter --lock
```

What `nitro-session --locked` and the bar's Lock action are to run (next
tasks). It connects to the **shell** socket, opens `Surface::lock()` (an
undecorated, focusable overlay covering the output), sends `Lock`
(`Ui::lock_session`), which takes over an ownerless lock, and starts a
conversation for the session's owner, so the password prompt has the
keyboard at once. A wrong password shows PAM's reason and asks again for
the same name. Success sends `Unlock`, closes the helper and exits 0.

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
  respawns it on the next attempt if it died. greetd (step 5) will be a
  second `Backend` over `$GREETD_SOCK`, with the same codec
  (`nitro-login`).
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
`prompt`, `answer`, `message`, `status`, `logout`.

## Not yet

greetd mode (no flag prints that and exits 2), a session list, power
buttons, a user list, and multiple outputs: other outputs show only the
background, because the server draws nothing but the lock owner's
windows.
