# nitro

A minimal, snappy desktop stack for Linux: a KMS compositor with a retained
scene graph, a small retained-mode toolkit, and a handful of shell apps.
Rust throughout. Remote and phone capable from the same code.

See [DESIGN.md](DESIGN.md) for the architecture sketch and milestones.

## Install

`just install` builds a release and installs into `/usr/local`, using sudo
or doas only for the copy.

`just PREFIX=$HOME/.local install` does a user install. `PREFIX`, `DESTDIR`
and the other GNU directory variables work as a packager expects. See
[docs/install.md](docs/install.md), which also covers the optional
Chromium build, its runtime dependencies and the AppArmor profile. The
development and test-box recipes live in `deploy/dev.just`
([docs/testbox.md](docs/testbox.md)).

## Starting a session

`nitro-session` needs a VT and DRM master. There are two supported ways
to get there:

**A. getty + profile (zero install).** Log in on tty1 at the text
console and put this in `~/.bash_profile`:

```sh
[ "$(tty)" = /dev/tty1 ] && [ -z "$NITRO_SOCKET" ] && exec nitro-session
```

`login` has already opened a logind session, so the compositor gets the
seat as it would under any display manager. Logging out of nitro drops
back to the getty.

**B. greetd + `nitro-greeter` (a login screen).** Install greetd from
your distribution (`pacman -S greetd`, `apt install greetd`), then
`just install install-greetd`, and switch display managers with the two
commands it prints. The greeter is a nitro-ui app running on its own
nitro-server as the greeter user. It renders whatever PAM asks, offers
nitro and the machine's `/usr/share/wayland-sessions` entries, and has
suspend/restart/power-off buttons. See [docs/greeter.md](docs/greeter.md)
and [deploy/greetd/README.md](deploy/greetd/README.md).
