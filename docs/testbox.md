# Test box

`kaspar@192.168.1.204` (override with `NITRO_BOX`). Passwordless sudo,
ssh key access from the dev machine. Deliberately weak hardware — if it is
snappy here, it is snappy.

| | |
|---|---|
| OS / kernel | Ubuntu 26.04 LTS, kernel 7.0 |
| CPU / RAM | Pentium G3240 (Haswell, 2 cores, SSE4.2, **no AVX2**), 3.3 GB, **no swap** |
| GPU | Intel HD (HSW GT1), `i915`, `/dev/dri/card1`, `renderD128` |
| Outputs | HDMI-A-1 1920×1080 connected; VGA-1 unused |
| Seat | systemd-logind (seatd also present); user in `video`,`render`,`input` |
| Libs | libseat 0.9, libinput 1.31, libdrm 2.4.131, libxkbcommon 1.13 |
| Tools | `perf`, `ydotool`, `chvt`, rustup (`~/.cargo/bin`) |
| Repo | `~/src/ai/nitro` — git remote `box`, push with `just box-push` |
| Old stack | `~/src/ai/nitro-old`, `~/src/ai/gpui-solo` — reference only |

## Loop

```
just box-install   # once: systemd unit nitro-dev on tty2 (replaces getty@tty2)
just deploy        # build release → rsync ~/nitro-bin/ → restart nitro-dev
just shot          # front-buffer readback → tmp/shot.png
just box-log       # journalctl -f
just box-session   # what is running, from the session's own socket
just box-ps        # RSS + 60 s idle CPU for the whole tree
just box-chvt 1    # VT-switch survival test; `just box-chvt 2` to come back
just box-stop
```

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

```console
$ just box-session            # status
ok
nitro-server 217261
nitro-wallpaper 217278
nitro-bar 217279
nitro-launcher 217280

$ just box-session lock       # M4; refused, honestly
err lock is not implemented yet (M4: …)
$ just box-session suspend    # systemctl suspend, via the session
```

## Rules learned the hard way

- **No swap, 3.3 GB.** Never run overlapping `perf record`s; cap
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
