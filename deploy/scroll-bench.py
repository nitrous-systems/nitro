#!/usr/bin/env python3
"""Reproducible scroll benchmark against a running nitro server (#3834).

Reproduces #3778's scroll test on whatever client is under the pointer:
N wheel events of +STEP, then N of -STEP, EVERY ms apart, injected
server-side through the control socket's `input` request (the real input
path, so focus, i2p accounting etc. are exactly as for a mouse). Reports
one markdown row in the format of docs/chromium.md §Measurements:

  | label | fps | frame interval p50 / p95 / max | submit→present | i2p p50 / p95 / max (n) |

submit→present is client-side only and printed as `–`. Pure stdlib.

fps is frames / the whole scroll's wall time, so events that produce no
frame (scrolling past the top or bottom of the page) halve it. The server
applies `pointer.natural_scroll` to injected events too; the bench reads
the `pointer_natural_scroll` stat and negates STEP, so the first phase
always scrolls content down from the top of the page (#3953: with it on
and not compensated, the first 150 events scrolled up at the top, did
nothing, and testhost2 measured 31 fps instead of 62).

Input patterns (#3974), `--pattern`:
  even    COUNT events EVERY ms apart, then COUNT back (the original run).
          Evenly spaced input can phase-lock with vblank.
  random  the same 2*COUNT events at intervals uniform on 4..25 ms from
          `--seed`, so input lands at every phase of the frame.
  burst   flicks of 5..10 events 8 ms apart separated by 100..300 ms
          idle gaps (seeded), 2*COUNT events in all: the first frame
          after an idle gap is reported on its own row.
Every event is scheduled server-side with an absolute `t=` stamp
(CLOCK_MONOTONIC, which is Python's time.CLOCK_MONOTONIC too), so the
pacing is the server's timer's, not Python's. When the server has the
`timeline` request, the bench turns it on, clears it, and prints a
per-stage table (p50 / p95 of each step, ms) after the i2p row; it
leaves the timeline as it found it. `--timeline-out FILE` keeps the raw
records.

  deploy/scroll-bench.py --label "chromium, release pacing"
  deploy/scroll-bench.py --x 400 --y 300 --count 150 --step 15 --every 16
  deploy/scroll-bench.py --pattern burst --seed 3974
"""

import argparse
import os
import random
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


def natural_scroll(st):
    """The server's `pointer.natural_scroll`: its stat, or for a server
    older than #3953 the config file it reads (last assignment wins)."""
    if "pointer_natural_scroll" in st:
        return int(st["pointer_natural_scroll"]) == 1
    d = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    on = False
    try:
        with open(os.path.join(d, "nitro", "server.conf")) as f:
            for l in f:
                k, eq, v = l.split("#")[0].partition("=")
                if eq and k.strip() == "pointer.natural_scroll":
                    on = v.strip().lower() in ("true", "yes", "on", "1")
    except OSError:
        pass
    return on


def samples(kind):
    status, lines = request("samples " + kind, True)
    return int(status.split()[1]), [int(v) for v in lines]


def request_soft(line, body):
    """`request`, but None instead of exiting on an `err` reply."""
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
    return (status, lines) if status.startswith("ok") else None


def schedule(pattern, count, every, seed):
    """Offsets (ms from the start) and direction (+1/-1) of each event."""
    rnd = random.Random(seed)
    n = 2 * count
    out = []
    if pattern == "even":
        for i in range(n):
            out.append((i * every, 1 if i < count else -1))
    elif pattern == "random":
        t = 0.0
        for i in range(n):
            out.append((t, 1 if i < count else -1))
            t += rnd.uniform(4.0, 25.0)
    elif pattern == "burst":
        t = 0.0
        i = 0
        while i < n:
            flick = rnd.randint(5, 10)
            for _ in range(flick):
                if i >= n:
                    break
                out.append((t, 1 if i < count else -1))
                i += 1
                t += 8.0
            t += rnd.uniform(100.0, 300.0) - 8.0
    else:
        raise ValueError(pattern)
    return out


# The per-stage deltas the timeline table reports: (label, from, to).
STAGES = [
    ("input→rx", "input", "rx"),
    ("rx→sent", "rx", "sent"),
    ("sent→present (client)", "sent", "present"),
    ("present→fence", "present", "fence"),
    ("fence→latch", "fence", "latch"),
    ("latch→commit", "latch", "commit"),
    ("commit→vblank", "commit", "vblank"),
    ("input→vblank (earliest)", "input", "vblank"),
    ("input→vblank (newest)", "input_last", "vblank"),
]


def timeline_rows(lines):
    header = lines[0].split()
    return [dict(zip(header, map(int, l.split()))) for l in lines[1:]]


def stage_table(label, recs, idle_starts):
    """Markdown rows: p50 / p95 per stage for every client-answered frame,
    and for burst, the frames answering the first input after a gap."""
    ans = [r for r in recs if r["sent"] and r["present"] and r["vblank"]]
    first = [r for r in ans if r["input"] in idle_starts]
    rows = [(label, ans)]
    if idle_starts:
        rows.append((label + ", first after gap", first))
    out = []
    for name, rs in rows:
        cells = []
        for _, a, b in STAGES:
            d = [(r[b] - r[a]) / 1e6 for r in rs if r[a] and r[b] and r[b] >= r[a]]
            cells.append("%.1f / %.1f" % (pct(d, 50), pct(d, 95)) if d else "–")
        unp = [r["unpresented"] for r in rs if r["latch"]]
        defer = sum(r["deferred"] for r in rs)
        out.append("| %s | %s | %s | %d | %d |" % (
            name, " | ".join(cells), "%.1f" % (sum(unp) / len(unp)) if unp else "–",
            defer, len(rs)))
    return out


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
    ap.add_argument("--pattern", choices=("even", "random", "burst"), default="even")
    ap.add_argument("--seed", type=int, default=3974)
    ap.add_argument("--timeline-out", help="write the raw timeline records here")
    a = ap.parse_args()

    if a.x is None or a.y is None:
        first = request("outputs", True)[1][0].split()
        w, h = first[1].split("@")[0].split("x")
        a.x = a.x if a.x is not None else int(w) / 2
        a.y = a.y if a.y is not None else int(h) / 2

    request("input motion %g %g" % (a.x, a.y), False)
    time.sleep(0.2)

    s0 = stats()
    # Positive dy scrolls content down; natural scroll inverts it server-side.
    natural = natural_scroll(s0)
    step = -a.step if natural else a.step
    tl = request_soft("timeline", True)
    tl_was_on = tl is not None and tl[0].split()[2:3] == ["1"]
    if tl is not None:
        request("timeline on", False)
        request("timeline clear", False)
    i2p0, _ = samples("i2p")
    flip0, _ = samples("flip")
    t0 = time.monotonic()
    plan = schedule(a.pattern, a.count, a.every, a.seed)
    idle_starts = set()
    if a.pattern == "even":
        span = a.count * a.every / 1000.0
        request("input wheel 0 %g count=%d every=%g" % (step, a.count, a.every), False)
        request("input wheel 0 %g count=%d every=%g after=%g"
                % (-step, a.count, a.every, a.count * a.every), False)
        time.sleep(2 * span)
    else:
        # Absolute stamps on the server's clock, far enough ahead that
        # every request is in before the first is due.
        base = time.clock_gettime_ns(time.CLOCK_MONOTONIC) + 300_000_000
        prev = None
        for off, sign in plan:
            t = base + int(off * 1e6)
            if prev is None or off - prev >= 100.0:
                idle_starts.add(t)
            prev = off
            request("input wheel 0 %g t=%d" % (sign * step, t), False)
        end = base + int(plan[-1][0] * 1e6)
        time.sleep(max(0.0, (end - time.clock_gettime_ns(time.CLOCK_MONOTONIC)) / 1e9))
    while stats().get("input_inject_pending", 0) > 0:
        time.sleep(0.02)
    t_last = time.monotonic()
    time.sleep(a.settle)
    s1 = stats()
    recs = []
    if tl is not None:
        _, lines = request("timeline", True)
        recs = timeline_rows(lines)
        if a.timeline_out:
            with open(a.timeline_out, "w") as f:
                f.write("\n".join(lines) + "\n")
        if not tl_was_on:
            request("timeline off", False)
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
    if recs:
        print("| %s stages (ms, p50 / p95) | %s | unpresented at latch | deferred | n |" % (
            a.pattern, " | ".join(s[0] for s in STAGES)))
        print("|---|" + "---|" * (len(STAGES) + 3))
        for row in stage_table("%s %s" % (a.label, a.pattern), recs, idle_starts):
            print(row)
        print()
    events = s1.get("input_injected", 0) - s0.get("input_injected", 0)
    print("server: %d events%s, %d frames in %.2f s; paint mean %.2f ms, copy mean %.2f ms, "
          "damage mean %d px, i2p mean %.1f ms (window of 100)" % (
              events, " (natural_scroll inverted)" if natural else "", frames, wall,
              ms(s1.get("paint_us_mean", 0)), ms(s1.get("copy_us_mean", 0)),
              s1.get("damage_px_mean", 0), ms(s1.get("i2p_mean_us", 0))))
    if a.pattern == "even" and a.every >= 16 and frames < 0.8 * events:
        print("warning: %d frames for %d events - is the page at an edge, or the scroll "
              "direction inverted? fps counts the idle time too" % (frames, events),
              file=sys.stderr)
    if not new_i2p:
        print("warning: no i2p samples - is a client window under (%g, %g) answering scrolls?"
              % (a.x, a.y), file=sys.stderr)


if __name__ == "__main__":
    main()
