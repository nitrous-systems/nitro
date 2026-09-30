# greetd for nitro

`config.toml` makes greetd run the nitro greeter: `nitro-session
--greeter` (a `nitro-server` plus `nitro-greeter`) as the greeter user,
on VT 1. The design is `docs/greeter.md`; the boxes are
`docs/testbox.md` "Login (greetd)".

- **Install**: `pacman -S greetd` / `apt install greetd`, then
  `just install install-greetd`. The config is rendered with `$BINDIR` and
  the greeter user. An existing config is kept once as
  `config.toml.pre-nitro`. `just uninstall` puts it back.
- **Greeter user**: the package creates it. Arch: `greeter`. Debian/Ubuntu:
  `_greetd`. It needs to exec the binaries (a `$BINDIR` under a 0750 home
  needs an ACL `x`), and it gets DRM and input through logind like any
  seat session. If libseat refuses, check the session is class `greeter`
  on the seat (`loginctl`), and the `video`/`render` groups.
- **PAM**: the package ships `/etc/pam.d/greetd` and
  `greetd-greeter`. Nothing of ours is needed. (The lock screen's
  `nitro-lock` is separate: `just install`.)
- **`/var/cache/nitro-greeter`**: owned by the greeter user. It holds the
  remembered user and session. `XDG_CACHE_HOME` points there too, for
  Mesa's shader cache.
- **Server config**: the greeter's server has no
  `~/.config/nitro/server.conf`. For a keymap or scale, point it at a
  system one: `env NITRO_CONFIG=/etc/nitro/server.conf …` in `command`
  (`just box-greetd` does this).
- **Debian's unit** conflicts with `getty@tty7`, not tty1. With `vt = 1`,
  disable `getty@tty1`, or add a drop-in with
  `Conflicts=getty@tty1.service`.
- **Sessions**: nitro is built in (`nitro-session` next to the greeter),
  and nothing is installed into `/usr/share/wayland-sessions`. Other
  entries there are listed after it. A `nitro.desktop` whose `Exec` is
  nitro-session is skipped.
- **Autologin, locked** (decision 6): uncomment `[initial_session]`.
- **Rollback**: `systemctl disable --now greetd`, restore
  `config.toml.pre-nitro`, and re-enable the previous display manager (or
  `getty@tty1`). On a test box: `just box-greetd-rollback`.

`cycle.sh` / `cycles.sh` are the ten-cycle login test from
`docs/testbox.md`.
