# nitro-auth

The lock screen's PAM helper. It authenticates **the session's own user**
and nobody else, and it is the only binary in the tree that links libpam.

```console
$ nitro-auth [--service NAME]     # default service: nitro-lock
```

## Protocol

greetd's IPC, verbatim, on stdin (requests) and stdout (responses): a
native-endian `u32` length and one flat JSON object. The codec is
`nitro-login`. So `nitro-greeter` runs one conversation state machine
against this helper or against greetd.

- `create_session{username}`: refused with `error{error, "only <owner> can
  unlock this session"}` unless `username` is the owner
  (`nitro_login::owner()`); otherwise one PAM transaction
  (`pam_authenticate` + `pam_acct_mgmt`). Every PAM message becomes an
  `auth_message` and waits for `post_auth_message_response`. The end is
  `success`, `error{auth_error, <PAM's text>}` (wrong password, locked
  account, …) or `error{error, …}` (a broken stack).
- A new `create_session` after an attempt ended is a new transaction:
  that is the retry after a wrong password. One while a question is open
  is refused, and the question still stands (greetd's rule).
- `cancel_session` mid-conversation fails the conversation, abandons the
  transaction and is answered `success`.
- `start_session` is `error{error, "not supported by nitro-auth"}`.
- End of input: exit 0. A malformed frame or a broken pipe: exit 1.
  Bad arguments: exit 2. stderr is for diagnostics only.

The loop (`src/serve.rs`) is generic over an `Authenticator` trait and is
tested against scripted ones: tests never touch real PAM, which could trip
`pam_faillock` on the machine running them.

## PAM

The service file is `deploy/pam.d/nitro-lock` (`auth` and `account`
`include login`, swaylock's shape); `just install` puts it in
`$SYSCONFDIR/pam.d/nitro-lock` unless one exists. Without it, Linux-PAM
falls back to `/etc/pam.d/other`, which usually denies.

As a non-root process, `pam_unix` checks the password through the setuid
`unix_chkpwd`, which only verifies **the caller's own** password. That is
exactly this helper's job, and another reason it refuses other users.

Not done: `pam_setcred`. The binding (`nonstick` 0.1) has no call for it
yet, so an unlock does not refresh credentials (Kerberos tickets and the
like). A lock screen can live without it.

## Binding

`nonstick` 0.1 with `default-features = false, features = ["link"]`: +4
crate names (`nonstick`, `libpam-sys`, `libpam-sys-impls`,
`libpam-sys-helpers`), no bindgen, no headers at build, just `-lpam`. The
FFI `unsafe` lives inside it; this crate has none. The comparison is in
`DEPENDENCIES.md`.
