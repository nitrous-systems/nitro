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
just box-chvt 1    # VT-switch survival test; `just box-chvt 2` to come back
just box-stop
```

The unit (`deploy/nitro-dev.service`) runs the server as the developer
user inside a logind session on tty2 (`PAMName=login`, `TTYPath`), so
libseat's logind backend grants DRM master and input without root.
`ExecStartPre=+chvt 2` makes the session active immediately.

## Rules learned the hard way

- **No swap, 3.3 GB.** Never run overlapping `perf record`s; cap
  `--call-graph=dwarf,N` at N ≤ 8192 or use `fp`. The unit has
  `MemoryMax=1G`. The box once needed a power cycle after a perf pile-up.
- **HW cursor is not in screenshots** (composited at scan-out). If a test
  needs the cursor, draw a software cursor in a debug mode.
- **libinput pointer acceleration is non-linear** for `ydotool`. Recipe
  that lands within ~3%: `ydotool mousemove -- -10000 -10000; sleep 0.1;
  ydotool mousemove -- $((X/2)) $((Y/2)); ydotool click 0xC0`.
- tty1 keeps a getty for rescue. If the box stops answering the server,
  `sudo systemctl stop nitro-dev` from ssh restores tty1.
