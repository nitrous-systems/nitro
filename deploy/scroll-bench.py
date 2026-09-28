#!/usr/bin/env python3
"""Reproducible scroll benchmark against a running nitro server (#3834).

Reproduces #3778's scroll test on whatever client is under the pointer:
N wheel events of +STEP, then N of -STEP, EVERY ms apart, injected
server-side through the control socket's `input` request (the real input
path, so focus, i2p accounting etc. are exactly as for a mouse). Reports
one markdown row in the format of docs/chromium.md §Measurements:

  | label | fps | frame interval p50 / p95 / max | submit→present | i2p p50 / p95 / max (n) |

submit→present is client-side only and printed as `–`. Pure stdlib.

  deploy/scroll-bench.py --label "chromium, release pacing"
  deploy/scroll-bench.py --x 400 --y 300 --count 150 --step 15 --every 16
"""

import argparse
import os
import socket
import sys
import time


def socket_path():
    p = os.environ.get("NITRO_CONTROL")
    if p:
        return p
    d = os.environ.get("XDG_RUNTIME_DIR")
    if d and os.path.isabs(d):
        return os.path.join(d, "nitro", "control.sock")
    return "/tmp/nitro-%d/control.sock" % os.getuid()


def request(line, body):
    """Send one request; return (status, body lines). `body` says whether
    the reply carries a blank-line-terminated body."""
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(10)
    s.connect(socket_path())
    s.sendall((line + "\n").encode())
    f = s.makefile("r")
    status = f.readline().rstrip("\n")
    lines = []
    if body and status.startswith("ok"):
        for l in f:
            l = l.rstrip("\n")
            if not l:
                break
            lines.append(l)
    s.close()
    if not status.startswith("ok"):
        sys.exit("nitro: %s -> %s" % (line, status))
    return status, lines


def stats():
    out = {}
    for l in request("stats", True)[1]:
        k, _, v = l.partition(" ")
        try:
            out[k] = int(v)
        except ValueError:
            out[k] = v
    return out


def samples(kind):
    status, lines = request("samples " + kind, True)
    return int(status.split()[1]), [int(v) for v in lines]


def pct(values, p):
    if not values:
        return float("nan")
    v = sorted(values)
    i = min(len(v) - 1, max(0, int(round(p / 100.0 * (len(v) - 1)))))
    return v[i]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--x", type=float, help="device px; default: centre of the first output")
    ap.add_argument("--y", type=float)
    ap.add_argument("--count", type=int, default=150)
    ap.add_argument("--step", type=float, default=15)
    ap.add_argument("--every", type=float, default=16, help="ms between events")
    ap.add_argument("--label", default="scroll-bench")
    ap.add_argument("--settle", type=float, default=0.3, help="s to wait after the last event")
    a = ap.parse_args()

    if a.x is None or a.y is None:
        first = request("outputs", True)[1][0].split()
        w, h = first[1].split("@")[0].split("x")
        a.x = a.x if a.x is not None else int(w) / 2
        a.y = a.y if a.y is not None else int(h) / 2

    request("input motion %g %g" % (a.x, a.y), False)
    time.sleep(0.2)

    s0 = stats()
    i2p0, _ = samples("i2p")
    flip0, _ = samples("flip")
    t0 = time.monotonic()
    span = a.count * a.every / 1000.0
    request("input wheel 0 %g count=%d every=%g" % (a.step, a.count, a.every), False)
    request("input wheel 0 %g count=%d every=%g after=%g"
            % (-a.step, a.count, a.every, a.count * a.every), False)
    time.sleep(2 * span)
    while stats().get("input_inject_pending", 0) > 0:
        time.sleep(0.02)
    t_last = time.monotonic()
    time.sleep(a.settle)
    s1 = stats()
    i2p1, i2p_all = samples("i2p")
    flip1, flip_all = samples("flip")

    wall = t_last - t0
    frames = s1["frames"] - s0["frames"]
    fps = frames / wall if wall > 0 else float("nan")
    new_i2p = i2p_all[len(i2p_all) - min(len(i2p_all), i2p1 - i2p0):]
    new_flip = flip_all[len(flip_all) - min(len(flip_all), flip1 - flip0):]
    new_flip = new_flip[1:]  # the first spans the idle before the run
    ms = lambda us: us / 1000.0

    print("| pacing | fps | frame interval p50 / p95 / max | submit→present p50 | input→photon p50 / p95 / max |")
    print("|---|---|---|---|---|")
    print("| %s | %.1f | %.1f / %.1f / %.1f ms | – | %.1f / %.1f / %.1f ms (n=%d) |" % (
        a.label, fps,
        ms(pct(new_flip, 50)), ms(pct(new_flip, 95)), ms(max(new_flip) if new_flip else float("nan")),
        ms(pct(new_i2p, 50)), ms(pct(new_i2p, 95)), ms(max(new_i2p) if new_i2p else float("nan")),
        len(new_i2p)))
    print()
    print("server: %d events, %d frames in %.2f s; paint mean %.2f ms, copy mean %.2f ms, "
          "damage mean %d px, i2p mean %.1f ms (window of 100)" % (
              s1.get("input_injected", 0) - s0.get("input_injected", 0), frames, wall,
              ms(s1.get("paint_us_mean", 0)), ms(s1.get("copy_us_mean", 0)),
              s1.get("damage_px_mean", 0), ms(s1.get("i2p_mean_us", 0))))
    if not new_i2p:
        print("warning: no i2p samples - is a client window under (%g, %g) answering scrolls?"
              % (a.x, a.y), file=sys.stderr)


if __name__ == "__main__":
    main()
