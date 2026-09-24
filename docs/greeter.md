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

## Decision 5: `TextField` grows a secret mode, and introspection respects it

This is the one change the greeter forces on the toolkit, and it is a
security fix before it is a feature. Today `TextField::accessible()`
returns `value: Some(self.text.clone())`. So `nitro-hey get
window/password value` would print the password, and the introspection
socket is a documented, scripted interface (goal 5). Three things
change for `TextField::secret(true)`:

1. **The wire carries bullets, not the text.** The server shapes text, so
   whatever string the field sends is in the server's scene, in a
   `nitro-shot` readback, and on the remote link. The field keeps the
   real bytes client-side and sends `"•"` repeated to the character count.
2. **Introspection reports length, not value.** `get … value` answers
   `<secret, 8 chars>`. `set_value` stays allowed, so tests and `hey` can
   still type into it, because writing a secret is not reading one. `watch`
   events for the field carry no value.
3. **No copy.** Copy and cut are no-ops. Paste is allowed.

The buffer is zeroed on drop and on submit with `fill(0)` followed by
`std::hint::black_box`. That is not a guarantee against the optimiser
eliding the write. The guaranteed version is `write_volatile`, which is
`unsafe`, and the tree has one sanctioned `unsafe` exception. The honest
position: the password also passes through a `String` in the IPC encoder
and through greetd's own memory, so the zeroing is hygiene, not a
boundary. The introspection and wire changes above are the actual
boundary.

The greeter's own introspection socket lives in `greeter`'s `0700`
runtime directory, so only root and `greeter` can reach it anyway.
Redaction is still the right default because the same widget will be used
in-session (Wi-Fi passphrases, `sudo` prompts), where the socket is the
user's.

## What this does *not* do

- **Lock screen.** This is related but a different mechanism. A lock
  screen authenticates *inside* the user's session. It cannot go through
  greetd, which only creates new sessions. It needs either PAM in-process
  (the FFI that option C refuses) or a helper such as `unix_chkpwd`,
  which is setuid and already installed, but checks only `pam_unix`
  passwords, not fingerprints. That is M4's `lock` and needs its own
  sketch. What it *can* share with this one is the conversation
  widget from decision 4 and the secret field from decision 5.
- **Multi-seat, remote login, user switching.** greetd supports one seat
  per instance, and nothing here precludes a second instance on `seat1`
  later.
- **Wayland sessions under our greeter.** These work as a side effect of
  decision 4's session list. `cmd` is any program, and greetd starts it
  after our compositor is gone.

## Plan

In order. Each step can land on its own:

1. **`TextField::secret`** in nitro-ui, with the wire and introspection
   redaction and tests that assert over *every* place the text could leak
   (scene string, `get`, `watch`, a `shot`), per the
   "a green test is evidence only about what it looked at" rule.
2. **`nitro-greeter` crate**: `greetd.rs` (codec + tests), `flow.rs`
   (the state machine, pure, tested with a scripted fake greetd over a
   `socketpair`: password; OTP after password; an info line; a wrong
   password; `start_session` refused), and `main.rs` (the UI). The
   dependencies are `nitro-ui` and `rustix`, both already in the tree.
3. **`nitro-session --greeter`**: the profile, `Role::Primary`, and the
   two exit-code tests.
4. **`deploy/greetd/config.toml`** plus a `docs/testbox.md` section:
   install greetd from the distro, create the `greeter` user, disable
   `nitro-dev.service`'s tty2 conflict (or move greetd to tty1 and keep
   dev on tty2). On the box, measure the time from greetd start to first
   greeter frame, the RSS of the greeter session (target: under the M3
   desktop's ~31 MB, since it is two processes instead of five), and ten
   login/logout cycles with the VT coming back each time.
5. **Docs**: README option A (getty + profile) as the zero-install path,
   and `DEPENDENCIES.md` gains a short "system components" note that
   greetd is a runtime requirement of the login path only, not a crate.

The crate count does not move.
