# Installing nitro

The install recipes are at the top of the `justfile`. They install from a
checkout into a local prefix, and they are written so that a Linux package
(deb, rpm, PKGBUILD, …) can wrap them. They use only `cargo`, `install`,
`sed` and `strip`: no rsync, no ssh, and no package manager.

```sh
just install                         # /usr/local; builds as you, sudo/doas only to copy

just PREFIX=$HOME/.local install     # a user install, no sudo
just DESTDIR=$PWD/pkg PREFIX=/usr install   # staging, what a package build does
just install install-chromium        # also the optional Chromium build
just uninstall                       # same variables as the install
```

Every recipe runs as you, so `cargo build` never runs as root. Only the
commands that write into the destination (`install`, `rm`, `rmdir`,
`apparmor_parser`) are prefixed, and only when the destination is not
writable: with `sudo` if it is on PATH, else with `doas`. `SUDO=doas`
forces doas, `SUDO=none` never escalates. `sudo just install` still works,
but then cargo builds as root; if an earlier one left `target/` owned by
root, `sudo chown -R $USER target` fixes it. Re-running an install
updates the files in place.


## Variables

| variable | default | |
|---|---|---|
| `PREFIX` | `/usr/local` | |
| `DESTDIR` | empty | staging root, prepended to every path, never written into a file |
| `BINDIR` | `$PREFIX/bin` | |
| `LIBDIR` | `$PREFIX/lib` | Chromium goes to `$LIBDIR/nitro/chromium` |
| `DATADIR` | `$PREFIX/share` | |
| `SYSCONFDIR` | `/etc` if `PREFIX=/usr`, else `$PREFIX/etc` | `install-apparmor`, and `pam.d/nitro-lock` from `install-bins` |
| `SUDO` | `auto` | privilege prefix for writes: `auto`, `sudo`, `doas`, or `none` |


Both `just PREFIX=/usr install` and `PREFIX=/usr just install` work. Write
`$HOME/.local`, not `~/.local`, in the first form: after `just PREFIX=`
the shell does not expand `~`.

## Recipes

| recipe | installs |
|---|---|
| `install` | `install-bins` then `install-desktop` |
| `install-bins` | `cargo build --release`, then the shipped binaries into `$BINDIR`, and `deploy/pam.d/nitro-lock` into `$SYSCONFDIR/pam.d` unless one exists |
| `install-desktop` | `deploy/*.desktop` and `deploy/nitro-mimeapps.list` into `$DATADIR/applications` |
| `install-chromium` | optional, see below |
| `install-apparmor` | optional, see below |
| `uninstall` | removes all of the above except the AppArmor profile |

**Binaries:** nitro-server, nitro-session, nitro-shot, nitro-calc,
nitro-amp, nitro-term, nitro-files, nitro-bar, nitro-launcher,
nitro-wallpaper, nitro-settings, nitro-video, nitro-greeter, nitro-auth,
hey. The test box also gets nitro-demo,
nitro-bench and the examples. Those are development and measurement
tools and are not installed.

**They are one set.** `nitro-session` starts the server, bar, launcher and
wallpaper from its own directory first and uses `$PATH` only as a
fallback (`crates/nitro-session/src/pieces.rs`). It also prepends that
directory to every child's `PATH`. That is why the `.desktop` files can
say `Exec=nitro-term`, and why `install-desktop` runs after `install-bins`:
an entry installed next to a `nitro-session` older than #3723 breaks
launching from the launcher. The long comment on `deploy-bins`
(`deploy/dev.just`) has the details.

`nitro-mimeapps.list` makes nitro-amp and nitro-video the default audio
and video handlers under `XDG_CURRENT_DESKTOP=nitro` (which nitro-session
sets for its children). It ranks below a user's `~/.config/mimeapps.list`,
which the install never touches (`docs/files.md`).

**The lock screen needs PAM.** `nitro-auth` (the lock screen's helper,
run by `nitro-greeter --lock`) links libpam, so the build needs its
development package: `libpam0g-dev` on Debian/Ubuntu, `pam-devel` on
Fedora, `pam` on Arch. It is the only binary that links it. At run time it
uses the PAM service `nitro-lock`; `install-bins` installs
`deploy/pam.d/nitro-lock` (`auth`/`account include login`) and never
overwrites an existing file, so an admin's edit survives a re-install.
`uninstall` removes it.

## Starting the session

`nitro-session` needs a VT and DRM master. It is neither a Wayland nor an X
compositor, so nothing is installed into `wayland-sessions/` or
`xsessions/`: a display manager would start it as the wrong kind of
session. Start it from tty1 (`exec nitro-session` in the login shell's
profile) or from greetd. See [greeter.md](greeter.md).

## Chromium (optional)

`install-chromium` installs a **release, non-component** build of the
`nitro-ozone` Chromium branch. It does not build Chromium. It reads the
build from `NITRO_CHROMIUM_OUT` (the `out/Nitro` directory) and exits
with an error if `chrome` is not there. It installs:

- `$LIBDIR/nitro/chromium/`: `chrome` (stripped) and its runtime files
- `$BINDIR/chromium-nitro`: the wrapper the launcher entry runs, with the
  `$LIBDIR` path substituted in at install time
- `$DATADIR/icons/hicolor/<n>x<n>/apps/chromium-nitro.png`
- `$DATADIR/applications/chromium-nitro.desktop`, installed last

**Runtime dependencies** are not installed for you. A packager should
declare Chromium's usual set (nss, nspr, cups, dbus, expat, gbm, drm,
xkbcommon, glib, pango, cairo, alsa, udev). A desktop-less system also
needs the accessibility libraries libatk1.0, libatk-bridge2.0 and
libatspi2.0; on Debian and Ubuntu these are `libatk1.0-0t64
libatk-bridge2.0-0t64 libatspi2.0-0t64`. To see what is missing, run
`ldd $LIBDIR/nitro/chromium/chrome | grep 'not found'`.

### AppArmor

Ubuntu sets `kernel.apparmor_restrict_unprivileged_userns=1`. Without a
profile that grants `userns`, a chrome at a custom path cannot start its
sandbox. `install-apparmor` renders `deploy/chromium/apparmor-chromium-nitro`
for `$LIBDIR/nitro/chromium/chrome` and installs it as
`$SYSCONFDIR/apparmor.d/chromium-nitro`. It loads the profile with
`apparmor_parser -r` only for a direct install (`DESTDIR` empty). A package
ships the file and runs `apparmor_parser -r` in its postinst. `uninstall`
leaves the profile in place.
