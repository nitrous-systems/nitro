# Test box

`kaspar@192.168.1.204` (override with `NITRO_BOX`). Passwordless sudo,
ssh key access from the dev machine. Deliberately weak hardware — if it is
snappy here, it is snappy.

| | |
|---|---|
| OS / kernel | Ubuntu 26.04 LTS, kernel 7.0 |
| CPU / RAM | Pentium G3240 (Haswell, 2 cores, SSE4.2, **no AVX2**), 3.3 GB, **no swap** |
| GPU | Intel HD (HSW GT1), `i915`, `/dev/dri/card1`, `renderD128` |
| Outputs | HDMI-A-1 1920×1080 connected, **running at 120 Hz** (see below); VGA-1 unused |
| Seat | systemd-logind (seatd also present); user in `video`,`render`,`input` |
| Libs | libseat 0.9, libinput 1.31, libdrm 2.4.131, libxkbcommon 1.13 |
| Tools | `perf`, `ydotool`, `chvt`, rustup (`~/.cargo/bin`) |
| Repo | `~/src/ai/nitro` — git remote `box`, push with `just box-push` |
| Old stack | `~/src/ai/nitro-old`, `~/src/ai/gpui-solo` — reference only |

## Loop

```
just box-install   # once: systemd unit nitro-dev on tty2 (replaces getty@tty2)
just deploy        # build release → rsync ~/nitro-bin/ + ~/.local/share/applications/ → restart nitro-dev
just shot          # front-buffer readback → tmp/shot.png
just box-log       # journalctl -f
just box-session   # what is running, from the session's own socket
just box-ps        # RSS + 60 s idle CPU for the whole tree
just bench         # the throughput matrix; see docs/bench.md
just bench-report  # that ledger as the markdown docs/bench.md carries
just bench-bandwidth box   # the box's memcpy rate: copy 3.61 GB/s
just box-chvt 1    # VT-switch survival test; `just box-chvt 2` to come back
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

## The deployed set is two directories, not one

`just deploy` writes **`~/nitro-bin/`** and
**`~/.local/share/applications/`**, and both are part of the deployed
state. A worker who restores the box "as found" must leave the four
`.desktop` files in place: they are `deploy/nitro-{calc,files,settings,
term}.desktop` from this repository, they are re-written by every
`just deploy`, and deleting them is not tidying up — it is undeploying
half the desktop's icons.

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
nitro-wallpaper 217278
nitro-bar 217279
nitro-launcher 217280

$ just box-session lock       # M4; refused, honestly
err lock is not implemented yet (M4: …)
$ just box-session suspend    # systemctl suspend, via the session
```

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
