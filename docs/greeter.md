# Login: greetd + `nitro-greeter`

Status: **sketch**. Nothing here is implemented. It argues where the line
goes, what the new pieces are, and what each one costs, so the first
commit can be small.

Today the desktop starts from `deploy/nitro-dev.service`: a systemd unit
with `User=kaspar` and `PAMName=login` that puts `nitro-session` on tty2.
That is a dev harness, not a login. The user is fixed in the unit file,
there is no password, and a second user means a second unit. This page is
about getting from "the box booted" to "*someone* logged in and got a
nitro session" without GDM, SDDM or LightDM, and without pulling any of
their dependencies into our tree.

## The shape

```text
 systemd
   └─ greetd                          root, owns VT 1, runs PAM   (system package)
        │
        ├─ [greeter session]          user `greeter`, PAM service `greetd`
        │    nitro-session --greeter
        │      ├─ nitro-server        DRM master via libseat/logind, as today
        │      └─ nitro-greeter       nitro-ui app; speaks greetd IPC on $GREETD_SOCK
        │                             ── exits 0 after `start_session` → whole tree exits
        │
        └─ [user session]             user `alice`, started by greetd once the greeter is gone
             nitro-session            exactly today's desktop, unchanged
               ├─ nitro-server
               ├─ nitro-wallpaper · nitro-bar · nitro-launcher
```

The compositor runs **twice**, once per session, as two different users.
Nothing is handed across the boundary except a command line. That is the
whole design: the privileged part is someone else's small daemon, and
the part we write is an ordinary unprivileged nitro-ui client.

## Decision 1: greetd, not our own login daemon

There are three options. The first and third define the edges.

| option | what we write | what runs as root | new crates in our tree |
|---|---|---|---|
| **A. getty + profile** | nothing | `agetty`, `login` (already there) | 0 |
| **B. greetd + `nitro-greeter`** | one nitro-ui app, ~150 lines of IPC | `greetd` (system package) | **0** |
| C. `nitro-logind` of our own | PAM conversation, `setuid`/`initgroups`, session registration, VT allocation | **ours** | `pam` bindings (FFI) + an `unsafe` exception |

**A is the baseline, and it should be documented as the supported
zero-install path.** Log in on tty1 with the kernel console, and put this
in `~/.bash_profile`:

```sh
[ "$(tty)" = /dev/tty1 ] && [ -z "$NITRO_SOCKET" ] && exec nitro-session
```

`login` has already opened a logind session through `pam_systemd`, so
libseat gets the seat exactly as it does under `nitro-dev.service`. It
costs nothing. It is also not a login screen, which is what was asked
for.

**C is refused.** A login daemon runs as root, drives PAM (a C API built
around a callback that receives an array of pointers) and switches user
identity. That breaks two rules in `DESIGN.md` at once. It needs a second
sanctioned `unsafe` boundary (PAM's conversation function), and it puts
root code in a tree whose one C dependency today is libseat. What you get
for that is a daemon greetd already is.

**B is the proposal.** greetd was written for this split. The daemon
does PAM, VT and session setup, and knows nothing about pixels. A greeter
is any program that speaks a four-message protocol over a Unix socket. It
is by the same author as seatd and libseat, which we already depend on, so
the seat model underneath is the one we already use. It is a Rust daemon,
and the tree we ship does not link it. It is a system package, like
systemd and logind are. Our `Cargo.lock` does not change.

What greetd gives us for free:

- **Autologin.** An `[initial_session]` block in `config.toml` runs the
  user's command once at boot, without a greeter. This covers the kiosk
  and phone case with no nitro code at all.
- **Arbitrary PAM stacks.** Fingerprint, TOTP, `pam_faillock` messages and
  password expiry arrive as generic prompts (see decision 4). A greeter
  that renders prompts instead of a hard-coded password box supports all
  of them without knowing any of them exist.
- **A text fallback.** `agreety`, shipped with greetd, is the greeter for
  a box whose GPU or font setup is broken. You switch one line of
  config, and there is no "cannot log in to fix the desktop" trap.

## Decision 2: the greeter is a nitro-ui app hosted by `nitro-session`

greetd runs *one command* as the `greeter` user and waits for it to exit.
That command has to bring up a compositor and a UI, then go away cleanly.
`nitro-session` already does the first two and most of the third, so the
command is:

```toml
# /etc/greetd/config.toml
[terminal]
vt = 1

[default_session]
command = "nitro-session --greeter"
user = "greeter"
```

`--greeter` is a second **profile** of the session, not a second
supervisor. There are two differences from the desktop profile:

1. **The piece list is `[nitro-server, nitro-greeter]`.** No bar, no
   launcher, no wallpaper process. The greeter paints its own background,
   because one surface is cheaper than a second client. This is a new
   `const` next to `pieces::PIECES`, chosen by the flag. It is *not*
   `NITRO_SESSION_PIECES`: that variable is a subset selector over the
   desktop's three pieces, and the README's reasons for refusing arbitrary
   programs there still hold.
2. **The greeter is the *primary* piece.** Today only the server's exit
   ends a session. A shell piece that exits is restarted with backoff. In
   the greeter profile:
   - `nitro-greeter` exits **0** means it has handed off (`start_session`
     succeeded). The session runs its normal reverse-order teardown and
     exits 0. greetd sees its child exit and starts the user's session.
   - `nitro-greeter` exits **non-zero** means it crashed, so it is
     restarted with the existing backoff. A greeter that cannot come back
     means nobody can log in, so it keeps retrying, like the bar. It has
     no give-up path.
   - The server's exit still ends everything, as today. greetd then
     restarts the greeter session. That is the `Restart=` decision
     `nitro-session`'s README leaves to "whatever started it", and greetd
     makes it.

That is one new `Role::Primary` in `pieces.rs`, one branch in the exit
handler, and tests for both exit codes against `examples/stub_child`.
Nothing about readiness, pidfds, signals or the teardown deadline
changes.

**The VT handoff has to be clean.** greetd starts the user session on the
*same* VT as soon as the greeter's process tree is gone. The second
`nitro-server` can only get DRM master if the first one has really
released it. That is the same property `systemctl stop nitro-dev` is
measured on today (`DESIGN.md` M3: "exits 0, tty1 back"). The greeter
profile adds a test for it on the box: log in, log out, log in again, ten
times, on the same pids of greetd.

There is one visible cost: between the two compositors the panel shows
the text console for a moment. Four things happen in order when the
greeter's `nitro-server` exits and the user's starts:

1. **The exiting process's framebuffers are destroyed.** The kernel
   removes every framebuffer the process created, and a plane that was
   showing one is switched off.
2. **The kernel's console takes the display back.** When the last DRM
   fd closes, fbdev emulation restores its own buffer and mode.
3. **logind switches the VT back to text mode.** When the greeter's
   session ends, logind puts the VT back into `KD_TEXT`, and fbcon
   redraws.
4. **The user's `nitro-server` starts**, takes the VT into graphics
   mode, sets its mode and paints.

| step | fix | who controls it |
|---|---|---|
| 1 | `DRM_IOCTL_MODE_CLOSEFB` (Linux 6.8): drop the handle, keep the plane scanning it. `drm-sys` 0.8 has the struct, but `drm-ffi` 0.9 has no wrapper. Upstream it (five lines in `drm-ffi`) rather than take a third `unsafe` exception: `DEPENDENCIES.md` already lists two, in `nitro-seat` and `nitro-shm`. The shim itself is about fifteen lines over `rustix::ioctl`. | us, after drm-rs |
| 2 | someone keeps the device open during the gap, or fbdev emulation is off | not us, under greetd |
| 3 | fbcon deferred takeover and a quiet boot (Fedora's flicker-free setup): text mode then draws nothing | distro or box config |
| 4 | the first commit carries a finished frame, and the mode already lit is kept when it is as good, so there is no monitor resync | **us: done** (`nitro-kms` README, "The first picture is a finished frame") |

Step 4 is in: `DrmBackend::open` no longer commits a black buffer. The
first `commit` is the initial modeset, with the server's first real
frame, and the probe keeps the mode the CRTC is already scanning out
when `select::keep_on_screen` says it answers the configuration as well.
`stats` reports `first_frame_ms`, the length of the gap from the new
server's side.

With steps 1, 3 and 4, the handover is: greeter frame, possibly one
black frame, desktop. It shows no text and needs no resync. **Fully
seamless needs overlap**: the next compositor has the device open
before the last one closes it, so step 2 never happens. GDM does this by
starting the user's session on a fresh VT, waiting for its first frame
and then switching VTs. greetd runs the session on the greeter's VT
after the greeter has exited, so this is a feature request upstream,
not something to patch locally. A persistent display holder (a root
daemon that owns DRM master and leases outputs to each session) would
also give overlap, and is refused for the same reason as option C.

### Deferred: a keeper process

Recorded here because it is the cheapest route to steps 1 and 2, and
it is **not being built now**.

Framebuffers and DRM master belong to the open file, not to the
process. While any fd to that file is open, the old framebuffers
survive, the plane keeps showing the last frame, and the console never
gets the last close. So the exiting server does not need to hand over
master (logind grants that per session anyway), only to keep the file
open until someone else has committed:

1. After `start_session` succeeds, the greeter's `nitro-server` forks and
   execs a small keeper with a dup of the DRM fd and its framebuffer
   ids, detached with `setsid`, then exits normally.
2. The keeper polls every 100 ms: `GETPLANE` on every plane, and exit
   once none shows one of its framebuffers. That covers the successor
   flipping onto the same plane, driving the output from another CRTC,
   and fbcon having taken the plane back first. It works for any
   successor, not only nitro. It gives up after 10 s.
3. Exit timing is invisible: the successor's frame is already on screen,
   and exiting only frees the old buffers.

It needs no root, no new crates and no `unsafe`, and it works on any
kernel. It would make `CLOSEFB` unnecessary. The same mechanism serves
logout as well. Step 3 (text mode during the gap) still needs fbcon
deferred takeover.

Unverified, and the first things to check on the box: that greetd and
logind leave a detached process of the greeter's session alive until it
exits (systemd's default `KillUserProcesses=no` should), and that
`GETPLANE` reports another file's framebuffer to a client that is not
master (as far as I know only `GETFB`, which hands out buffer handles,
is restricted).

Rejected alternative: exec from the greeter into the user session with
the fd inherited. The switch from `greeter` to the user needs root,
which is option C.

## Decision 3: the IPC is hand-rolled, not `greetd_ipc`

The protocol is a native-endian `u32` length followed by a JSON object
tagged by `"type"`:

| direction | `type` | fields |
|---|---|---|
| → | `create_session` | `username` |
| → | `post_auth_message_response` | `response`: string or `null` |
| → | `start_session` | `cmd`: `[string]`, `env`: `[string]` (`KEY=VALUE`) |
| → | `cancel_session` | — |
| ← | `success` | — |
| ← | `error` | `error_type`: `auth_error` \| `error`, `description` |
| ← | `auth_message` | `auth_message_type`: `visible` \| `secret` \| `info` \| `error`, `auth_message` |

The upstream `greetd_ipc` crate is correct and small, but it is
`serde` + `serde_json`. That is the tree's first JSON dependency and a
second proc-macro stack, bought to encode four shapes and decode three.
`nitro-term` went through the same trade with `vte`, and the line drawn
in `DEPENDENCIES.md` applies here. `vte` earned its place because a state
table is easy to get wrong in ways that surface months later. This is
the opposite case: a closed set of seven flat messages, each testable
exhaustively.

So `nitro-greeter/src/greetd.rs`:

- **Encode**: `format!` over a string escaper for `"`, `\`, control
  characters as `\u00XX`. The only untrusted input going out is the
  username and the responses the user typed, and the escaper is what
  keeps a `"` in a password from ending the string.
- **Decode**: a reader for *one flat object whose values are strings or
  `null`*. Any other value (a number, a nested object, an array) is a
  protocol error, not something to be tolerant about. `\uXXXX` includes
  surrogate pairs, because PAM messages are localised.
- **Framing**: the length is bounded at 64 KiB. greetd is root and
  trusted, but every other decoder in this tree bounds its input
  (`MAX_LINE`, the wire's frame cap). A greeter blocked allocating 4 GiB is
  a login screen that does not come up.
- The socket is **non-blocking**, registered with `Ui::add_fd` the way
  `nitro-term` registers its pty, and replies are reassembled from
  partial reads. It is never read in a blocking call, because greetd answers `create_session` only after PAM
  asks its first question, which on a fingerprint stack can take
  seconds. The UI must keep painting during that time (a spinner, and
  Cancel still works).

Estimate: ~150 lines plus a test table that round-trips every message and
refuses the malformed ones. No new crates. `rustix` is already in the tree
for the socket.

## Decision 4: the UI renders PAM's conversation, not a password box

The state machine is small, and pure, so it can be unit-tested without a
server:

```text
          ┌──────────── cancel / auth_error ─────────────┐
          ▼                                              │
     ┌─────────┐ create_session  ┌──────────┐  auth_message  ┌────────────┐
     │  User   │────────────────▶│ Waiting  │───────────────▶│  Prompt    │
     │ (entry) │                 │ (spinner)│◀───────────────│ (k, text)  │
     └─────────┘                 └──────────┘  post_response └────────────┘
                                     │ success
                                     ▼
                                ┌──────────┐ start_session ┌──────────┐
                                │ Session  │──────────────▶│ exit(0)  │
                                │ (choose) │   success     └──────────┘
                                └──────────┘
```

- The prompt **text** comes from PAM, and nothing is hard-coded. `secret`
  shows a masked field, `visible` a plain one. `info` and `error` show a
  line and are answered with `response: null` straight away. They still
  need a reply before the conversation continues, which is the classic
  greeter bug.
- `error{auth_error}` returns to User with the description shown and the
  username kept, so a mistyped password costs one field, not two.
  `error{error}` does the same, with a different tone.
- Before `create_session` for a new attempt, the greeter sends
  `cancel_session` if a conversation is in flight. greetd refuses a second
  `create_session` otherwise.

**Session choice.** There is one entry by default: `nitro-session`. Other
entries come from `/usr/share/wayland-sessions/*.desktop`, parsed with
`nitro-launcher`'s existing `.desktop` reader (moved into a small shared
module rather than copied). `Exec=` becomes `cmd`, split on whitespace.
We do not support the full shell quoting of the spec, just as the launcher
does not. The last user and session are remembered in
`/var/cache/nitro-greeter/state`, which must be writable by `greeter`.
Losing that file only loses a default.

**Power.** Suspend, reboot and power off buttons use the session socket
that `nitro-session` already serves: `suspend`, `poweroff`, `reboot` over
`$XDG_RUNTIME_DIR/nitro/session.sock`. The greeter is an active local
logind session, so polkit's default rules allow these without a password.
That is the same path the bar will use, and no new code is needed.

**Layout.** One centred panel with a clock, a username field, the current
prompt, a message line, a session picker and power buttons. It is built
from existing nitro-ui widgets (`Label`, `TextField`, `Button`, `Flex`,
`Panel`), plus one list for users if we choose to list them (below).

**Users are typed, not listed, at first.** Listing means reading
`/etc/passwd` and deciding which UIDs are "human" (≥ 1000, a real shell,
not `nobody`), and every distro disagrees on that. A typed username is
correct everywhere. A list is a later option, off by default.

## Decision 5: `TextField` grows a secret mode, and introspection respects it (done)

This is the one change the greeter forces on the toolkit, and it was a
security fix before it was a feature: `TextField::accessible()` returned
the text, so `nitro-hey get window/password value` would have printed a
password. **Built**; the full account is in `docs/ui.md` under "A secret
`TextField`". In short:

- **Every string the field sends the server is the mask**, one `•` per
  character: the `SetText` it paints, and the strings it asks to have
  measured and caret-positioned. So the text is not in the scene, a
  `nitro-shot` readback, the remote link, the measure requests or the
  measure cache. The field translates caret offsets between the text
  and the mask.
- **Introspection reports the mask** as `value` and `text` (the sketch
  said `<secret, N chars>`; the mask is what AT-SPI does, and it is
  what `watch` would carry anyway). `set_value` still writes, so tests
  and `hey` can type a password. **No action unmasks**; only the app
  can, with `set_secret(false)`, which exists for a conversation whose
  one field asks a visible question, then a secret one.
- **Copy and cut** need no change: the field has no clipboard yet. When it
  gets one, a secret field must refuse both.
- **Wiping**: zeroed on `clear`, on replacement and on drop, spare
  capacity scrubbed after every edit, growth done by hand so no
  outgrown buffer is freed unzeroed, callbacks handed a borrow. It uses
  `black_box`, not `write_volatile`, because the latter is `unsafe`
  (the tree has two sanctioned exceptions, and this is not worth a
  third). It is hygiene, not a boundary: the password also passes
  through the input events, through the IPC encoder and through greetd.

`crates/nitro-ui/tests/secret.rs` checks against every byte the client
wrote to the socket, with a plain field typed in the same window as the
control. Each masking path was checked to fail its test when disabled.

## Decision 6: the owner boots into their own session, locked

On a device with one user (most laptops, phones and workstations), the
fastest login is not a greeter at all. greetd's `[initial_session]`
starts the owner's session at boot without authentication, and the
session comes up **locked**:

```toml
[initial_session]            # once per boot
command = "nitro-session --locked"
user = "alice"

[default_session]            # after any logout: the greeter
command = "nitro-session --greeter"
user = "greeter"
```

Unlocking costs about a frame. No second compositor starts and none is
torn down, so the common path has no handover at all.

- **One app, two backends.** The lock screen is `nitro-greeter` in a
  second mode: same layout, same conversation state machine, same secret
  field. As the greeter it talks to greetd. As the lock screen it talks to
  `nitro-auth`, a small helper that runs PAM for the session's own user
  and speaks the same prompt and answer messages. It is the only binary
  that links libpam: our second deliberate C dependency, with the FFI
  `unsafe` inside the binding crate (the `xkbcommon` precedent). Which
  crate is still to be measured. Piping to `unix_chkpwd` needs no
  library, but supports passwords only.
- **The lock is the server's, from the first frame.** A lock client
  started after the desktop would race it. The server starts locked,
  composites only the lock surface (other windows are not drawn at all,
  rather than covered) and routes input only to it. Only the connection
  that took the lock can unlock. A crashed lock client leaves the screen
  locked, and its restart takes the lock over. These are
  `ext-session-lock` semantics; suspend and idle locking need the same
  thing. Built: see plan step 2.
- **Nothing acts for the user before unlock.** `--locked` starts only
  what paints (server, wallpaper, bar, lock screen, the launcher's
  index). Autostart, when nitro has one, waits for the unlock.
- **Where it does not apply.** A home or keyring encrypted with the login
  password (fscrypt, systemd-homed, `pam_mount`) cannot open without the
  password, and an autologin has none. Full-disk encryption is the
  natural companion: the disk passphrase already authenticated the owner.
- **Another user never types a password into the owner's session.** The
  owner's session can only check the owner's password, and anything
  running as the owner could read what is typed there. So the lock screen
  asks for the name first. A different name goes to the greeter, where
  it is chosen again from a list: no channel carries it across.
  - Before the owner has unlocked, their session has nothing in it, and
    it logs out.
  - After they have used it: warn and log out, until multi-session.
- **Multi-session is deferred.** nitro already survives being a background
  session, since that is the VT-switch path M3 tests. greetd runs one
  session per instance, so a second login needs a second instance on
  another VT, started on demand (a polkit rule for that one unit) or
  standing by. Switching VTs between two live compositors is also the
  seamless handover, as a side effect. A session that goes to the
  background locks.

## What this does *not* do

- **Multi-seat and remote login.** greetd supports one seat per
  instance, and nothing here precludes a second instance on `seat1`
  later. User switching is decision 6.
- **Wayland sessions under our greeter.** These work as a side effect of
  decision 4's session list. `cmd` is any program, and greetd starts it
  after our compositor is gone.

## Plan

In order. Each step can land on its own:

1. **`TextField::secret`**: done (decision 5).
2. **The server's lock state**: done. `Lock`/`Unlock` shell ops,
   `NITRO_LOCKED=1`, the scene's `Admit` filter, and a gate on every input
   path; `docs/shell.md` §The session lock. One difference from decision
   6's sketch: the lock client is identified by being the connection that
   sent `Lock` (the `ext-session-lock` rule), not by a socket handed over
   by `nitro-session`. That needs no new fd plumbing, and an ownerless
   lock still cannot be unlocked by anyone. What it does not stop is
   another shell client of the same user taking over an *ownerless* lock,
   which is the same boundary the shell socket already has (`docs/shell.md`,
   "What this model is worth"). `nitro-session --locked` starts the lock
   screen first, so in practice it is the first to ask.
3. **`nitro-auth` and the conversation**: the helper, and in
   `nitro-greeter` the pure state machine (tested against scripted
   conversations: password; one-time code after password; an info line;
   a wrong password) with the lock-screen UI on top.
4. **`nitro-session --locked`** and the greetd config. On the box,
   measure boot to the lock screen's first frame (`first_frame_ms`) and
   unlock to desktop.
5. **The greeter**: `greetd.rs` (codec and tests), the same state
   machine as the greetd backend, and `nitro-session --greeter` with
   `Role::Primary`. Then `deploy/greetd/`, and a `docs/testbox.md`
   section with ten login/logout cycles.
6. **Docs**: README option A (getty + profile) as the zero-install path,
   and a `DEPENDENCIES.md` note that greetd is a runtime requirement of
   the login path, not a crate.

The crate count moves once, for the PAM binding in step 3.
