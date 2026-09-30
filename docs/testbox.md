# Test boxes

There are two boxes. Every box recipe in `deploy/dev.just` targets
**box1** by default and either box with `box=` or `NITRO_BOX`:

```
just deploy                          # box1
just box=testhost2 deploy            # testbox2 (Kaby Lake laptop)
NITRO_BOX=testhost2 just footprint   # the same, from the environment
```

The box's **profile** (`box_profile`) says how it runs nitro. It is
guessed from the host name (`testhost2` / `192.168.1.193` → `greetd`,
anything else → `unit`), and `NITRO_BOX_PROFILE` forces it.
`NITRO_BOX_BINDIR` overrides where the binaries live. Both boxes keep
their clone at `~/src/ai/nitro`, and `just box-push` pushes `main` to it.

| recipe | `unit` (box1) | `greetd` (testbox2) |
|---|---|---|
| `deploy-bins` | rsync → `~/nitro-bin`, entries → `~/.local/share/applications` | stage in `~/nitro-stage`, `sudo install` → `/usr/local/bin`, entries → `/usr/local/share/applications`; examples → `~/nitro-bin` |
| `deploy` | + restart `nitro-dev` | install only (no autologin: a restart would leave the greeter) |
| `box-restart` | restart `nitro-dev` | restart `greetd` (ends the session, greeter) |
| `box-status` / `box-log` / `box-stop` | the `nitro-dev` unit | `loginctl` + `pgrep`, the journal by `_COMM` (incl. `nitro-greeter`, `greetd`), stop `greetd` |
| `box-install` | install the unit | make the clone pushable, check greetd is enabled |
| `box-ps`, `footprint` | tree under the unit's MainPID | tree under the oldest `nitro-session` |
| `shot`, `bench-bandwidth` | `~/nitro-bin/…` | `/usr/local/bin/…` |
| `bench` | runs | **refuses** (no unit to restart between arms) |
| `deploy-chromium` | apt + AppArmor profile | pacman `at-spi2-core`, no AppArmor step |
| `box-greetd` / `box-greetd-rollback` | greetd on tty1 from `~/nitro-bin` (nitro-dev stays on tty2) | greetd as *the* display manager from `/usr/local/bin`, gdm disabled |

## box1: Pentium G3240 (Haswell)

`kaspar@192.168.1.204` (the default `box`). Passwordless sudo,
ssh key access from the dev machine. Deliberately weak hardware — if it is
snappy here, it is snappy.

| | |
|---|---|
| OS / kernel | Ubuntu 26.04 LTS, kernel 7.0 |
| CPU / RAM | Pentium G3240 (Haswell, 2 cores, SSE4.2, **no AVX2**), 3.3 GB, **no disk swap** (4 GB zram since ~Sep 2026, i.e. compressed RAM) |
| GPU | Intel HD (HSW GT1), `i915`, `/dev/dri/card1`, `renderD128` |
| Outputs | HDMI-A-1 1920×1080 connected, **running at 120 Hz** (see below); VGA-1 unused |
| Seat | systemd-logind (seatd also present); user in `video`,`render`,`input` |
| Libs | libseat 0.9, libinput 1.31, libdrm 2.4.131, libxkbcommon 1.13 |
| Tools | `perf`, `ydotool`, `chvt`, rustup (`~/.cargo/bin`) |
| Repo | `~/src/ai/nitro` — push with `just box-push` (the `box` git remote is the same URL) |
| Login | **greetd on tty1** (#3951): the nitro greeter as `_greetd`, from `~/nitro-bin`; getty@tty1 disabled. `nitro-dev` stays on tty2 as the measurement harness |

## Loop

These recipes are defined in `deploy/dev.just`, which the `justfile`
imports. They run from the repository root. For a local install, not the
box, see [install.md](install.md).

```
just box-install   # once: systemd unit nitro-dev on tty2 (replaces getty@tty2)
just deploy        # build release → rsync ~/nitro-bin/ + ~/.local/share/applications/ → restart nitro-dev
just shot          # front-buffer readback → tmp/shot.png
just box-log       # journalctl -f
just box-session   # what is running, from the session's own socket
just box-ps        # RSS + 60 s idle CPU for the whole tree
just footprint     # binary sizes + dep count + box idle RSS (docs/budget.md baseline)
just bench         # the throughput matrix; see docs/bench.md
just bench-report  # that ledger as the markdown docs/bench.md carries
just bench-bandwidth box   # the box's memcpy rate: copy 3.61 GB/s
just box-chvt 1    # VT-switch survival test; `just box-chvt 2` to come back
just box-gpu-test  # GPU helper pixel tests (+ footprint) on the box's render node, NITRO_GPU_TEST=require
just box-stop
```

`just bench` is the long one — 54 runs, about twelve minutes at six
seconds each, and it restarts `nitro-dev`. The three-rate sweep
(`just bench "1920x1080@60 1920x1080@120 720p240" 6`) is 140 runs and
about twenty minutes, and it **puts the screen at 1280×720 for the
240 Hz arm**, so that arm is a reduced matrix and the script logs when it
enters and leaves it. **Announce it in the `nitro-testbox` room before
starting**, as with anything that takes the box for a window. It backs up
and restores the human's `~/.config/nitro/server.conf` (a refresh sweep
writes a `mode` or `modeline` line into it and must hand back exactly
what it found, his `mode = 1920x1080@120` included), and it reads the
control socket through a small Python client rather than `nc`, for the
reason in the rules below.

The unit (`deploy/nitro-dev.service`) runs **`nitro-session`** as the
developer user inside a logind session on tty2 (`PAMName=login`,
`TTYPath`), so libseat's logind backend grants DRM master and input
without root. `ExecStartPre=+chvt 2` makes the session active
immediately.

Since M3-E the unit starts the *session*, not the server: `nitro-session`
starts `nitro-server`, waits for its two sockets to accept a handshaken
client, then starts the wallpaper, the bar and the launcher, and
supervises all four. So `pgrep nitro` shows five processes, a
`pkill nitro-bar` is repaired within about a second, and
`systemctl stop nitro-dev` takes the whole desktop down in order. See
[`crates/nitro-session/README.md`](../crates/nitro-session/README.md).

## testbox2: i5-8250U (Kaby Lake R, Gen9), `testhost2`

`ssh testhost2` (user kaspar, passwordless sudo, key access; host name
`ng`, 192.168.1.193). A laptop on which greetd (since #3951; GDM
before) runs the nitro greeter, and the human logs in to nitro from
`/usr/local/bin`. The human uses it now and then but has released it for
measurements (#3917). It is here for what box1 cannot do:
Gen9 display planes with NV12 and scalers, full anv Vulkan, and VA-API
on iHD.

| | |
|---|---|
| OS / kernel | Arch Linux, kernel 7.2.2 |
| CPU / RAM | i5-8250U (Kaby Lake R, 4c/8t), 23.9 GB |
| GPU | Intel UHD 620 (KBL GT2, 8086:5917), `i915`, `/dev/dri/card1`, `renderD128`; display version 9, cdclk 337.5 MHz (max 675) |
| Power | **suspend disabled** (#3917, at the human's request, after the box went to sleep at the greeter): `sleep`/`suspend`/`hibernate`/`hybrid-sleep`/`suspend-then-hibernate.target` masked, `/etc/systemd/logind.conf.d/nitro-no-sleep.conf` ignores lid and idle. cpufreq `intel_pstate` (active), governor `powersave`, EPP `balance_performance`, left as found for every measurement |
| Outputs | eDP-1 2560×1440@60 (`2560x1440@59997`) connected (scale 1.25 in `~/.config/nitro/server.conf`, `de` keymap); DP-1, HDMI-A-1, DP-2 disconnected |
| Planes | per pipe: primary + 1 overlay + cursor; 2 scalers on pipes A/B, 1 on C. Measured in [`crates/nitro-kms/README.md`](../crates/nitro-kms/README.md#testbox2-intel-uhd-620-kaby-lake-r-gen9-i915-kernel-722-25601440-edp--2026-09-29) |
| Seat | greetd + logind (greeter and user session on seat0/tty1); gdm installed but **disabled**; seatd 0.9.3 present |
| Libs | Mesa 26.2.3, vulkan-intel (anv) 26.2.3, intel-media-driver (iHD) 26.2.4 + libva-intel-driver, libva 2.24.1, libinput 1.32, libxkbcommon 1.13.2, libdrm 2.4.134 |
| Tools | `perf`, `chvt`, `sqlite3`, `python3`, `vulkaninfo` (vulkan-tools), `vainfo` (libva-utils), rustc 1.95 |
| Repo | `~/src/ai/nitro` (moved from `~/src/nitro` in #3911; `receive.denyCurrentBranch=updateInstead`) |

Vulkan (`vulkaninfo --summary`): Intel(R) UHD Graphics 620 (KBL GT2),
Mesa 26.2.3 anv, API 1.4.354, conformance 1.4.0.0.

VA-API (`vainfo --display drm`): iHD 26.2.4. Decode (VLD): MPEG-2, H.264
(CB/Main/High), VC-1, JPEG, VP8, HEVC Main and **Main10**, VP9 Profile 0
and **Profile 2** (10-bit). Encode: MPEG-2, H.264 (incl. low-power),
JPEG, VP8, HEVC Main/Main10. Plus VideoProc (scaling/CSC).

Rules for this box:

- **Free for measurements** (the human, 2026-09-29): stopping and
  restarting greetd and nitro is fine. `just box=testhost2 deploy`
  installs into `/usr/local/bin` and does **not** restart anything; the
  new build runs from the next login. There is no autologin, so
  `box-restart` / `box-stop` leave the greeter.
- **Leave a working session behind**: `sudo systemctl start greetd` at the
  end (greetd is the display manager; do not re-enable gdm), with a build that runs, and say in `nitro-testbox` what is
  deployed.
- There is no *permanent* `nitro-dev` unit (greetd does not use tty2, but a login there would be a second seat user),
  and `just bench` refuses on this box. For a measurement window, #3917
  installed box1's unit **temporarily** and ran the box1 scripts
  directly, which gives the same shape as box1 (a logind session on tty2,
  `MALLOC_MMAP_THRESHOLD_=131072`), so the numbers compare:

  ```console
  $ sed 's#/home/kaspar/nitro-bin/nitro-session#/usr/local/bin/nitro-session#' \
      deploy/nitro-dev.service | ssh testhost2 'sudo tee /etc/systemd/system/nitro-dev.service >/dev/null &&
      sudo systemctl daemon-reload && sudo systemctl stop greetd && sudo systemctl start nitro-dev'
  $ ssh testhost2 'ln -sf /usr/local/bin/nitro-server /usr/local/bin/nitro-bench ~/nitro-bin/'  # bench.sh fingerprints ~/nitro-bin
  $ ssh testhost2 'NITRO_BENCH_SHA='$(git rev-parse --short HEAD)' bash -s' -- --seconds 6 < deploy/bench.sh
  $ just box=testhost2 footprint 60      # box-ps finds the tree under the oldest nitro-session
  # afterwards: stop nitro-dev, rm the unit and the two symlinks, daemon-reload, start greetd
  ```

  Remove the unit afterwards, and start greetd again: stopped, the box
  has no login screen.
- The box has **no `ydotool`**. Input goes through the control socket's
  `input` request (`docs/latency.md` §7), which is the better instrument
  anyway.
- The session's control socket is in `/run/user/1000/nitro/`, so `shot`
  and `box-session` work over ssh as the same user while he is logged in.
- `/sys/kernel/debug` needs sudo: `sudo cat /sys/kernel/debug/dri/1/i915_display_info`.

## testbox3: Raspberry Pi 500 (BCM2712), `testhost3`

`ssh testhost3` (user kaspar, uid 1001, passwordless sudo; host name
`max`). An ARM box with a render-only GPU and a separate display
controller (#4000).

| | |
|---|---|
| OS / kernel | Raspberry Pi OS bookworm, **armhf (32-bit) userland** on an aarch64 kernel 6.12.62+rpt-rpi-v8 (`getconf LONG_BIT` = 32) |
| RAM | 8 GB |
| GPU | V3D 7.1 (`v3d`): `card0` and `renderD128`. Vulkan: Mesa 24.2.8 v3dv, API 1.2.289, manifest `broadcom_icd.armv8l.json` |
| Display | `card1` = `vc4-drm`; HDMI-A-2 connected 1920×1080@60 |
| Seat | the `nitro-dev` unit on tty2 (profile `unit`, like box1); `/dev/udmabuf` is root:kvm 0660, so the GPU shadow uses staging |
| Tools | `kmsprint`, `vulkaninfo`; no cargo |

The binaries are cross-built on the dev machine. **`just deploy` builds
x86, so do not use it here.**

```console
$ source tmp/xarm/env.sh     # clang + lld against tmp/pi-sysroot
$ cargo build --release --target armv7-unknown-linux-gnueabihf --workspace --bins
$ cd target/xarm/armv7-unknown-linux-gnueabihf/release && rsync -az nitro-server nitro-gpu-vulkan nitro-session nitro-shot nitro-bar nitro-launcher nitro-wallpaper testhost3:nitro-bin/
$ just box=testhost3 box-restart; just box=testhost3 box-log; just box=testhost3 shot
```

`tmp/xarm` and `tmp/pi-sysroot` are untracked scratch. To recreate the
sysroot, rsync `/usr/include`, `/usr/lib/arm-linux-gnueabihf`,
`/usr/lib/gcc`, `/usr/lib/linux` and `/usr/share/pkgconfig` from the Pi,
then make the absolute symlinks relative.

GPU pixel tests: there is no cargo on the box. Run
`cargo test --release --target armv7-unknown-linux-gnueabihf -p nitro-gpu-vulkan --no-run`,
then copy the `pixels-*` binary over. Put `nitro-gpu-vulkan` at the same
absolute path as on the dev machine
(`…/target/xarm/armv7-unknown-linux-gnueabihf/release/`), because the
test spawns `CARGO_BIN_EXE_nitro-gpu-vulkan`. Then run it with
`NITRO_GPU_TEST=require … --include-ignored --test-threads=1`.

## The deployed set is two directories, not one

`just deploy` writes **`~/nitro-bin/`** and
**`~/.local/share/applications/`**, and both are part of the deployed
state. A worker who restores the box "as found" must leave the four
`.desktop` files in place: they are `deploy/nitro-{calc,files,settings,
term}.desktop` from this repository, they are re-written by every
`just deploy`, and deleting them is not tidying up — it is undeploying
half the desktop's icons. `nitro-mimeapps.list` sits beside them (on
testhost2 in `/usr/local/share/applications`) and is part of the set too:
it is what makes nitro-files open audio in nitro-amp and video in
nitro-video.

They matter because of #3715: the server resolves an `app_id` it cannot
find in the icon theme through `<app_id>.desktop`'s `Icon=`, so
`nitro-calc` becomes the `calculator` glyph in the bar's window list and
in the window's own title bar. With the files absent, every one of our
applications shows the generic `window` shape — which is exactly what the
box looked like before #3723 and looks like again the moment somebody
"restores" the directory away. `stats desktop_entries` is the check: 8 on
a box with only the system files, **12** with ours installed.

Installing them used to be actively harmful, and the fix is worth knowing
because it changed a rule: their `Exec=` is a bare program name (the spec
asks for one, and a packager needs one), `~/nitro-bin` is on nobody's
`PATH`, and a `.desktop` file *shadows* the launcher's built-in entry for
the same program — so installing `nitro-term.desktop` replaced a working
launcher entry with `spawn: No such file or directory`. Since #3723
`nitro-session` prepends its own executable's directory to the `PATH`
every child inherits, so the bare name resolves and the shadowing is the
behaviour we want: **one** Terminal entry in the launcher, the packaged
one. If the launcher ever shows a Terminal entry that does not start,
that `PATH` prepend is the first thing to check — and it needs `sudo`,
since the session's children are not the ssh user's processes for
`/proc` purposes:

```console
$ pid=$(pgrep -x nitro-launcher)
$ sudo sh -c "tr '\0' '\n' < /proc/$pid/environ" | grep ^PATH=
PATH=/home/kaspar/nitro-bin:/usr/local/sbin:/usr/local/bin:...
```

The session's own `PATH` is the unit's and does **not** start with
`~/nitro-bin` — only its children's does, which is the change. Seeing
both is the cheapest confirmation that the prepend is live.

> **⚠ The files and the deployed `nitro-session` are coupled, and only
> one order is safe.** Measured on the box during #3723 rather than
> assumed: these four files installed next to a session that does *not*
> prepend (anything before #3723) reproduce the original regression
> exactly — `hey nitro-launcher do results/0 click` on Terminal spawns
> **nothing**, `pgrep -c nitro-term` stays 0, because the packaged entry
> shadows the built-in and its bare `Exec=` does not resolve. Removing
> the files brings the built-in back and the launch works again (the
> control for that arm). So: a rollback of the binaries to a pre-#3723
> build must take the four files with it, and a box deployed from such a
> build must not be left with them. `just deploy` from a tree containing
> #3723 is always consistent, because it writes both halves.

```console
$ just box-session            # status
ok
nitro-server 217261
nitro-greeter -
nitro-wallpaper 217278
nitro-bar 217279
nitro-launcher 217280

$ just box-session lock       # lock at the server, start nitro-greeter --lock
ok
$ just box-session suspend    # lock, then systemctl suspend
```

## Lock screen

`just deploy` (both profiles) also installs `/etc/pam.d/nitro-lock`, the
lock screen's PAM service, **when it is absent** (never overwriting). It
is part of the deployed set: "restore as found" leaves it in place.

To test on a box:

1. Lock: `hey nitro-bar do 'window[1]/lock' click` with the menu open,
   `just box-session lock`, or Super+L. `just shot` shows only the lock
   screen; `box-session` lists `nitro-greeter <pid>`.
2. A wrong password: `hey nitro-greeter get window/message value` shows
   PAM's reason, and the screen stays locked. **At most two in a row**:
   `pam_faillock` locks the account after a few.
3. The right one (`hey nitro-greeter set window/answer value …`, then
   `hey nitro-greeter do window/answer submit` (a text field's action is `submit`), or typed with ydotool
   on box1): the desktop comes back, and `box-session` shows
   `nitro-greeter -` (exit 0, not restarted).
4. `kill -9` the greeter while locked: `just shot` still shows no
   desktop, the journal says the session is restarting it, and the new
   one takes the lock over (server log: "lock taken over").

A `--locked` start, temporarily: on box1,
`sudo systemctl edit nitro-dev` with
`[Service]` / `ExecStart=` / `ExecStart=/home/kaspar/nitro-bin/nitro-session --locked`,
restart, and remove the drop-in (`sudo systemctl revert nitro-dev`)
afterwards. On testbox2, greetd starts the user's session as
`/usr/local/bin/nitro-session` with no flag; test `--locked` with a
temporary `[initial_session]` block in `/etc/greetd/config.toml`
(`command = "/usr/local/bin/nitro-session --locked"`, `user = "kaspar"`),
`systemctl restart greetd`, and remove the block afterwards.

## Login (greetd)

Since #3951 both boxes log in through greetd with `nitro-greeter` as
the greeter (`docs/greeter.md`). Set up, idempotently, with:

```console
$ just box-greetd                   # box1: greetd on tty1, nitro-dev keeps tty2
$ just box=testhost2 box-greetd     # testbox2: greetd replaces gdm
$ just box=testhost2 box-greetd-rollback   # undo: greetd off, gdm (or getty@tty1) back
```

`box-greetd` installs the package (apt or pacman), renders
`deploy/greetd/config.toml` with the box's bin dir and greeter user,
backs up the package's config once as `/etc/greetd/config.toml.pre-nitro`,
creates `/var/cache/nitro-greeter` (the remembered user), seeds
`/etc/nitro/server.conf` from the box user's (the greeter's keymap and
scale: testbox2's `de` and 1.25), and switches the display manager.
Details that bit on the way:

- **Greeter users differ**: `_greetd` (uid 110) on Ubuntu, `greeter`
  (uid 912, home `/`) on Arch. The config sets `XDG_CACHE_HOME` for
  Mesa's shader cache, which otherwise fails on `//.cache`.
- **Ubuntu's `greetd.service` conflicts with `getty@tty7`**, its default
  VT, not tty1. Our config uses VT 1, so on box1 getty@tty1 and greetd
  fought over it (greetd lost at the first login and systemd restarted
  it with a new PID). `box-greetd` adds the drop-in
  `greetd.service.d/nitro-vt1.conf` (`Conflicts=getty@tty1`) and
  disables getty@tty1; the rollback removes both.
- **box1 runs `~/nitro-bin`**, and `/home/kaspar` is 0750: `box-greetd`
  gives the greeter user an ACL `x` on it (`setfacl -m u:_greetd:x`,
  installing `acl` if needed). Any other user logging in on box1 needs
  the same to reach `nitro-session`.
- **Logs**: greetd gives the session its VT as stdio, so the config runs
  it under `systemd-cat`: the greeter's tree logs as
  `nitro-greeter-session`, a user session (the built-in Nitro entry) as
  `nitro-session`. `just box-log` follows both.
- On box1 the idle greeter's compositor lives on VT 1 while `nitro-dev`
  is on tty2: it adds its RSS (a server, the GPU helper, the greeter) to
  the box's memory, but not to the `nitro-dev` tree that `box-ps` and
  `footprint` measure. `chvt 1` shows it, and nitro-dev's
  `ExecStopPost=chvt 1` now lands on it.

**Driving the greeter** (as the greeter user, whose runtime dir holds
the app's introspection socket; name the pid when a killed greeter left
a stale socket):

```console
$ g() { sudo -u _greetd env XDG_RUNTIME_DIR=/run/user/110 ~/nitro-bin/hey nitro-greeter "$@"; }
$ g set window/user value nitrotest; g do window/user submit
$ g get window/prompt value          # Password:
$ g set window/answer value …; g do window/answer submit
$ g do window/session click; g do window[1]/sway click   # pick a session from the menu
$ g get window/session-name value
```

Do not put a password on a `sudo` command line: sudo logs it to the
journal. `deploy/greetd/cycle.sh` reads it from a 0600 file instead.

**The ten-cycle test.** A throwaway user `nitrotest` with a random
password (generated locally in `./tmp`, never committed) was created on
each box, and **deleted afterwards** (`userdel -r`, and its ACL entry on
box1), so no test account stays on either box. `deploy/greetd/cycles.sh 10`
(run on the box from `~/nitro-stage`, with `cycle.sh` beside it) logs in
through the greeter with `hey`, checks the user session answers `status`
on its `session.sock`, sends `logout`, waits for the greeter to be back,
and prints one row per cycle from the journal. Handoff is greeter exit
("handed off") → the user session's "server ready". 2026-09-30, build
from task-3951:

box1 (HSW, HDMI 1080p; nitro-dev running on tty2 throughout):

| cycle | greetd pid | greeter `first_frame_ms` | user `first_frame_ms` | handoff ms | greeter session | DRM/libseat errors | user session |
|---|---|---|---|---|---|---|---|
| 1 | 572621 | 230 | 221 | 871 | Stopped | 0 | ok |
| 2 | 572621 | 227 | 168 | 528 | Stopped | 0 | ok |
| 3 | 572621 | 218 | 222 | 547 | Stopped | 0 | ok |
| 4 | 572621 | 217 | 228 | 569 | Stopped | 0 | ok |
| 5 | 572621 | 214 | 206 | 521 | Stopped | 0 | ok |
| 6 | 572621 | 183 | 195 | 520 | Stopped | 0 | ok |
| 7 | 572621 | 194 | 180 | 525 | Stopped | 0 | ok |
| 8 | 572621 | 205 | 180 | 531 | Stopped | 0 | ok |
| 9 | 572621 | 212 | 197 | 537 | Stopped | 0 | ok |
| 10 | 572621 | 205 | 235 | 572 | Stopped | 0 | ok |

testbox2 (KBL, eDP 2560×1440 at 1.25):

| cycle | greetd pid | greeter `first_frame_ms` | user `first_frame_ms` | handoff ms | greeter session | DRM/libseat errors | user session |
|---|---|---|---|---|---|---|---|
| 1 | 460647 | 278 | 304 | 776 | Stopped | 0 | ok |
| 2 | 460647 | 283 | 264 | 548 | Stopped | 0 | ok |
| 3 | 460647 | 297 | 244 | 525 | Stopped | 0 | ok |
| 4 | 460647 | 312 | 269 | 551 | Stopped | 0 | ok |
| 5 | 460647 | 271 | 272 | 551 | Stopped | 0 | ok |
| 6 | 460647 | 270 | 256 | 538 | Stopped | 0 | ok |
| 7 | 460647 | 296 | 267 | 556 | Stopped | 0 | ok |
| 8 | 460647 | 274 | 243 | 539 | Stopped | 0 | ok |
| 9 | 460647 | 283 | 258 | 542 | Stopped | 0 | ok |
| 10 | 460647 | 283 | 258 | 535 | Stopped | 0 | ok |

The greetd PID never changed, and every greeter tree ended with
`session ended: Stopped` (exit 0 after the hand-off). The second server
always got DRM master; there was not one libseat or DRM-master error.
The handoff (~530 ms, ~870 ms on the first, cold login) is greetd's PAM
session setup plus the user's systemd manager starting; the user's
`first_frame_ms` comes after it. Also checked on both: `kill -9` of
nitro-greeter at the greeter is restarted after the 1 s backoff and the
new one logs in; killed *mid-conversation*, its successor at first
showed greetd's "a session is already being configured", which is why
the greeter now sends `cancel_session` when it starts.

## Chromium (#3865)

`just deploy-chromium` puts Chromium on nitro's own Ozone backend on the
box. It is **not** part of `just deploy`, and it does not restart
`nitro-dev`. It does not build anything either: it rsyncs a release,
non-component build from `out/Nitro` in the Chromium checkout
(`NITRO_CHROMIUM_OUT` overrides; `docs/chromium-build.md` §11 has the args). It
writes:

| where | what |
|---|---|
| `~/nitro-bin/chromium/` | `chrome` (stripped, 346 MB) and its runtime files, 429 MB in all; `--delete` is scoped to this directory |
| `~/nitro-bin/chromium-nitro` | the wrapper named by `Exec=`: flags, `--class`, and the profile `~/.config/chromium-nitro` |
| `~/.local/share/icons/hicolor/*/apps/chromium-nitro.png` | Chromium's logo, found by app id |
| `~/.local/share/applications/chromium-nitro.desktop` | "Chromium (nitro)" in the launcher, installed **last** |
| `/etc/apparmor.d/chromium-nitro` | `userns` for this path, so the sandbox is on (below) |
| apt: `libatk1.0-0t64 libatk-bridge2.0-0t64 libatspi2.0-0t64` | chrome links them, and the box did not have them |

The wrapper and the profile in `deploy/chromium/` are templates
(`@CHROMIUM_DIR@`, `@CHROME@`). For the box, `deploy-chromium` renders them
into `target/chromium-stage/` with `$HOME/nitro-bin/chromium`, kept literal
so the wrapper's shell expands it, and `/home/*/nitro-bin/chromium/chrome`.
The deployed files are the same as before the templating.

These are part of the deployed set now. `stats desktop_entries` is **14**
with it and `nitro-amp` installed. The launcher lists it only because
`chromium-nitro` is in its `NATIVE_PROGRAMS`. Add a `reload` on the
control socket after a first install so the server indexes the new
`.desktop` file (the launcher rescans on its own).

**The sandbox works; do not add `--no-sandbox`.** Ubuntu's
`kernel.apparmor_restrict_unprivileged_userns=1` denies user namespaces to
unconfined programs, and a chrome at a custom path is one. The profile,
Ubuntu's own `chrome` shape at our path, gives it `userns`. Renderers
run in their own user namespace with seccomp-bpf (`Seccomp: 2`). No SUID
`chrome-sandbox` is shipped.

**Memory:** one tab idle is 10 processes, **~440–500 MB PSS** and
~235–265 MB `RssAnon`. The `VmRSS` sum (~1.5 GB) double-counts the shared
binary and is not the number to quote. `free` "used" moved +170 to +250 MB.

Figures and the verdict are in `docs/chromium.md` §Test box.

**Out-of-process GPU (#3919).** The wrapper no longer passes
`--in-process-gpu`: the GPU process presents into the browser's windows
through `ExportSurface`/`ImportSurface`, so the box's server must have
`SURFACE` and `SHARE` (#3897/#3904). Deploy the server first. `just
chromium-bench inproc|oop|gpu [RUNS]` (either box, `box=testhost2`) runs
the scroll/i2p/memory comparison against the running session and leaves
only `/tmp` files behind. On testhost2 it needs a session, so bring up the
temporary `nitro-dev` unit above first. Numbers are in `docs/chromium.md`
§Out-of-process GPU measurements.

## The panel runs at 120 Hz

**By the human's choice, and the line is in his `server.conf` to stay**
(#3718):

```text
output.HDMI-A-1.mode = 1920x1080@120
```

> **The line only does anything with a build that knows the key.** An
> older `nitro-server` answers it with `unknown output key \`mode\`` and
> comes up at 60 — a warning, the rest of the file still applied, the
> desktop fine. So between #3718 landing and whatever is deployed on the
> box, "the config says 120" and "the box is at 60" can both be true at
> once. **Read `outputs`, not the file.**

So `nitro-shot --outputs` says `1920x1080@**119982**` — not `@120000`:
this connector's 120 Hz mode is 285 500 kHz over 2080×1144, which is
119.982 Hz. You still *write* `@120` (matching is nearest-within-0.5 Hz
so a person writes the round number), but **a script that greps for the
literal `@120000` will never match and will skip the arm silently.** Grep
`1920x1080@` and parse. `flip_interval_mean_us` is ~8 333 under
continuous motion, not ~16 667.

**Anything measured here is a 120 Hz number unless it says otherwise**,
and every figure in `docs/latency.md` §1–§4 and `docs/budget.md` predates
the key and is a 60 Hz number. `docs/latency.md` §5 has the conversion,
the measured 60-vs-120 pairs, and the two thresholds that move with the
rate.

To take a 60 Hz comparison, **rewrite the line rather than deleting it** —
deleting it gives the connector's preferred mode, which *is* 60 here, but
leaves nothing behind saying the box is meant to be at 120:

```console
$ ssh box "sed -i 's/^output.HDMI-A-1.mode.*/output.HDMI-A-1.mode = 1920x1080@60/' ~/.config/nitro/server.conf"
$ ssh box 'sudo systemctl restart nitro-dev'
$ ssh box '~/nitro-bin/nitro-shot --outputs'    # confirm before measuring
... and put `1920x1080@120` back when you release the box.
```

A restart is not strictly needed for a rate change — a same-size retime is
live on `reload` — but a restart is what makes the arms comparable: it
gives each one a fresh server, which is what every A/B in this room has
done.

**Check `outputs` at the start of every arm.** A `mode` line that matches
nothing is a warning in the log and the *default* mode on screen, so an
arm that silently ran at 60 while labelled 120 looks exactly like "120 Hz
bought nothing". `~/nitro-bin/nitro-shot --modes` lists what the connector
offers, which is also the answer to "what may I write here".

> **And check the md5 of the binary you are measuring, every time — at
> the start of the run *and at the end*.** This has now cost two
> measurements, in two different ways, and the second is why the rule has
> a second half.
>
> #3718: a retime experiment was run against what was assumed to be the
> new build, produced a clean-looking result, and was in fact main's
> server — the key was inert, nothing set had any effect, and the numbers
> were a measurement of nothing. The log said `unknown output key 'mode'`
> the whole time.
>
> #3722: a 141-run benchmark sweep was measured against binaries **a
> different task deployed six minutes after the sweep's own `rsync` and
> six minutes before its first arm**. The pre-run md5 check passed —
> it was taken before the other deploy landed. Everything else agreed
> too: `outputs` reported the right mode at every arm, the client's own
> `refresh_mhz` agreed with `outputs`, and the ledger's `sha` column
> faithfully named the tree the *caller* had built from. Forty minutes of
> numbers, nothing anywhere saying they were somebody else's build.
>
> So: **an md5 before a run answers "is this mine now". A long run has to
> answer "was it mine *throughout*", and only a pair of fingerprints
> answers that.** `deploy/bench.sh` now records both in the ledger and
> fails with `# INVALID: the binaries changed DURING this run` if they
> differ; a clean run carries `# binaries unchanged across the whole
> run`. Anything else that holds the box for more than a few minutes
> should do the same, because a `just deploy` from another branch does
> not read the room and will not warn you.
>
> The related trap, same family: **do not take the sha from the box's
> git clone.** `~/src/ai/nitro` is whatever `just box-push` last pushed
> there, which on this box was seventeen commits behind the binaries in
> `~/nitro-bin`. A provenance field has to be derived from the artefact
> it describes, not from something that is usually the same.

> **And `md5sum -c` your backup against the box *before* restoring it,
> not only after.** The rule above has a third form that cost a deploy
> during #3723. A worker backed up `~/nitro-bin` at 07:58, the
> orchestrator deployed main at 08:01 — three minutes into the window —
> and the worker's restore at 08:30 put the 07:58 snapshot back and
> reported a clean **26/26 OK**. That is what a correct restore looks
> like, and it was a correct restore *to the wrong point in time*: it
> silently reverted the orchestrator's deploy.
>
> The three forms, because they answer different questions and only the
> last one covers this:
>
> | check | answers |
> |---|---|
> | md5 before a run (#3718) | is this binary mine **now**? |
> | md5 before *and* after (#3722) | was it mine **throughout**? |
> | **md5 -c before restoring (#3723)** | did anything land **while I held the box**? |
>
> A `md5sum -c` after the restore compares the box to *your backup*, so
> it passes by construction whenever the restore worked — it cannot see
> that the backup itself went stale. Run it **first**, and treat a
> mismatch as "somebody deployed during my window; find out what before
> overwriting it" rather than as a reason to re-copy harder. The `ls -l`
> mtimes on `~/nitro-bin` are the cheap corroboration: a file stamped
> inside your window that you did not put there is the whole story.

### 240 Hz at 1080p is out of reach — but 720p@240 works

The human asked. The connector's own list tops out at 1080p@119.982, and
the reason is the link rather than the panel: HDMI 1.4 on Haswell caps the
TMDS clock near **300 MHz**, 1080p@120 is 285.5 MHz (just under), and
1080p@240 needs 606.5 MHz with CVT-RB. 1080p@144 (346.5) and @165 (401.0)
do not fit either. `docs/settings.md` has the full mode table and the
arithmetic.

**A smaller mode does fit, and it was tried — successfully.** With

```text
output.HDMI-A-1.modeline = 279750 1280 1328 1360 1440 720 723 727 810 +hsync -vsync
```

(CVT-RB 1280×720@240, 279.75 MHz) the kernel accepted the mode, the CRTC
scanned out at 240 — `outputs` reported `1280x720@239840 … (custom)`, 380
flips in 1.7 s = **219 flips/s**, `flip_interval_mean_us` 4 491 against a
4 167 µs period — **and the panel syncs**: asked of the one instrument
that can answer it, a person at the monitor, who reported a stable picture
(*"720p 240hz!"*) on a mode this display never advertised.

So **this box can be measured at 240 Hz**, at 720p. That makes a 4 167 µs
frame a real target rather than a hypothetical one, and it makes the
pixel-path arms of a benchmark sweep interesting at a size where the frame
is 3 686 400 B instead of 8 294 400.

**If you set a modeline and the screen goes black, ssh still works**: the
server is fine, the monitor is not. Remove the line and restart. `just
shot` keeps working the whole time and proves **nothing** about sync — it
reads the shadow buffer, which is the picture the server composed, not the
picture the glass received. It returned a perfect 3 686 400-byte 1280×720
frame throughout the run above, and would have returned the identical
bytes had the panel stayed dark. **Only a person looking at the screen
closes that question**, which is why it took two attempts hours apart to
get an answer.

## Rules learned the hard way

- **No disk swap, 3.3 GB** (the 4 GB of zram is compressed RAM and does not make memory free). Never run overlapping `perf record`s; cap
  `--call-graph=dwarf,N` at N ≤ 8192 or use `fp`. The unit has
  `MemoryMax=1G`. The box once needed a power cycle after a perf pile-up.
- **The unit sets `MALLOC_MMAP_THRESHOLD_=131072`, and that is a
  box-only mitigation rather than a product fix** (issue #547). Without it
  glibc's dynamic mmap threshold ratchets up when the second font file is
  freed, and ~2 MB of released font bytes stay resident in the server for
  the life of the process. **A server you start by hand does not get it**,
  so a `RssAnon` measured outside the unit is ~2 MB higher than one taken
  under it, and the two are not comparable — say which you measured. The
  effect is also what makes a decorated window *look* like it costs
  ~740 kB and then nothing: pinned, it costs ~86 kB every time. The real
  fix is file-backed font bytes in `nitro-text`, which needs an `unsafe`
  exception this tree does not grant; `docs/budget.md` has the numbers and
  the argument.
- **HW cursor is not in screenshots** (composited at scan-out). If a test
  needs the cursor, draw a software cursor in a debug mode.
- **libinput pointer acceleration is non-linear** for `ydotool`, in
  *both* directions. The old recipe — park with a huge negative move,
  then ask for half the target — lands within ~3 %, which is fine for a
  click and useless for a drag: 3 % of 1080 is 32 px and a titlebar is
  27 px high. Worse, small slow moves are **decelerated**: a single
  `mousemove -- -200` moved a dragged window 38 px.

  Better still, **do not use relative mode for placement at all**.
  `ydotool mousemove -a` is absolute and the acceleration is not applied
  to it, so one call lands on the pixel — with one trap worth writing
  down, because it costs an hour to rediscover: on this box the absolute
  device's coordinate space is **twice** the mode's, so

  ```console
  $ sudo YDOTOOL_SOCKET=/tmp/.ydotool_socket ydotool mousemove -a -x 550 -y 308
  ```

  lands the pointer at **(1100, 616)** on a 1920×1080 screen. Halve the
  coordinate you want. `deploy/pointer.py calibrate` measures the factor
  on a box that disagrees, and the script's `ABS_SCALE` is where it
  lives.

  `deploy/pointer.py` therefore places absolutely and only *checks* with
  a screenshot; the closed loop below is the fallback, not the method.

  ```console
  $ python3 /tmp/pointer.py move 1100 617       # lands within 1 px, ~1.3 s
  $ python3 /tmp/pointer.py drag 1060 540 660 380
  window moved by -400,-158 (asked -400,-160)
  $ python3 /tmp/pointer.py resize nitro-calc 1188 848 160 120
  ```

  It works because the compositor draws a **software** cursor, so a
  screenshot says where the pointer is. Three traps it records in its own
  comments, all of which produced a confidently wrong answer first:
  **the default scheme is now `light`**, so "a window is light pixels on
  a dark desktop" finds the *wallpaper* — `window_origin()` is deprecated
  for exactly that, and `drag` takes an optional app name so it can track
  the window by `hey <app> get window bounds` instead. The cursor
  locator, which looks for a dark arrow among the changed pixels, fails
  the same way and now returns "trust the absolute placement" rather than
  giving up. And:
  during a drag the "what changed between two frames" trick sees the
  *window* as well as the cursor, so a drag closes the loop on the
  window's origin; and a resize does not move the origin at all, so it
  closes the loop on the app's own `hey … get window bounds`. A
  bounding box of light pixels is **not** a window — the cursor is light
  too, and it moves with the drag, which made three runs report a
  perfect resize of a window that had not moved.
- **A colour run is not an edge when the shape is allowed to have
  content** (#3724). Locating a window's frame by "the widest contiguous
  run of title-bar colour" is a defensible boundary right up until you
  remember the title's *glyphs interrupt the run* — so the row it finds
  is the first one **below** the text, 20 px down on a 28-px bar. Two
  failures followed, and the second is the one worth the entry:

  1. "The pixel beside the title bar" was sampled below the bar, in the
     client's area, and read bar colour where border was expected. Nearly
     filed as a regression in the feature under test.
  2. A title-bar press at `frame.y + 14` measured from that wrong top
     landed **on the client's content**, `dragging` stayed 0, and the
     move cursor never appeared — producing a perfectly legible crop of
     *the arrow* during what looked like a title drag. The obvious
     reading was "the drag's cursor shape does not work".

  **The instrument's defect was indistinguishable from the feature's**,
  and a better crop could not have separated them. What did was a
  **sweep**: the same press at five heights down the bar, which turns
  "it does not work" into `+4 → dragging=1, +8 → 0, +14 → 0`, and a
  boundary at +4 is a fact about the coordinates rather than about the
  code. The fix for the locator is to find the run and then *walk up*
  from it at a column the content cannot reach (3 px in from the border,
  left of the app icon). Same family as the entry above — @3705's "a
  bounding box of light pixels is not a window" — one step in: a bounding
  box fails because other things are bright, a colour run fails because
  the thing itself is not uniform.

- **A diff against a pre-drag shot cannot show the cursor during a drag**
  (#3724), because the drag moves the *window*: the diff is a rectangle
  of translated title bar with the pointer somewhere inside it, and it is
  confidently unreadable. Measure inside a patch of one **role colour**
  instead — the blank stretch of title bar right of the text and left of
  the buttons — where "not that colour" means "the cursor" with no
  reference frame at all.

- **`PAMName=login` moves the tree out of the unit's cgroup.**
  pam_systemd puts the session in `user-1000.slice/session-N.scope`, so
  `system.slice/nitro-dev.service/cgroup.procs` is **empty** while the
  desktop runs. Two consequences: `journalctl -u nitro-dev` shows only
  systemd's own lines (use `journalctl -t nitro-session` or
  `_PID=`), and the unit's `MemoryMax=1G` **does not actually bind** the
  processes — it applies to an empty cgroup while the scope they are in
  inherits `max`. `deploy/box-ps.sh` therefore walks down from the unit's
  `MainPID` instead of reading the cgroup, and `deploy/nitro-dev.service`
  carries the same warning next to the setting. To make the limit real,
  put it where the processes are:

  ```console
  $ sudo systemctl set-property user-1000.slice MemoryMax=1G
  ```

  which the session scope inherits. That is a box-wide policy — it caps
  your ssh session too — so it is a note rather than something the
  repo installs.
- **No ImageMagick on the box.** `nitro-shot --raw` gives the readback
  unencoded, which is what the scripts here parse; `magick` is only
  available on the dev machine, for looking at the PNGs afterwards.
- **A signal mask is inherited across `fork` and `exec`.** A shell that
  blocks `SIGTERM` hands the block to everything it starts, so a
  `kill`-based test can silently do nothing. `deploy/size.sh` has a
  comment about it and `crates/nitro-session/tests/session.rs` detects
  it at run time rather than assuming either way.
- tty1 keeps a getty for rescue. If the box stops answering the server,
  `sudo systemctl stop nitro-dev` from ssh restores tty1.
