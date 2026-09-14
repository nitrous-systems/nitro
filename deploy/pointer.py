#!/usr/bin/env python3
"""Put the pointer where you asked, on a box whose libinput accelerates.

`docs/testbox.md` records the trap and a recipe that lands within ~3 %:
park the pointer at the top-left with a huge negative relative move, then
move by the target coordinates. It is not good enough for a *drag* — a
drag needs the press to land on a 27-pixel-high titlebar, and 3 % of 1080
is 32 pixels.

So this closes the loop instead of predicting it. The compositor draws a
software cursor (`docs/testbox.md`: the hardware plane is not in
screenshots, so the server paints one), which means **a screenshot says
where the pointer actually is**. Move, look, correct, repeat: three
iterations put it on the pixel, and no model of the acceleration curve is
needed at all.

    ./pointer.py move X Y          # land the pointer on X,Y
    ./pointer.py drag X1 Y1 X2 Y2  # press at X1,Y1, release at X2,Y2

The cursor is found as the darkest cluster that is *not* where the cursor
was in a reference frame, which is why every call takes its own reference
first: the desktop underneath may be anything.
"""

import os
import subprocess
import sys
import time

SHOT = os.path.expanduser("~/nitro-bin/nitro-shot")
SOCK = "/tmp/.ydotool_socket"


def ydotool(*args):
    subprocess.run(
        ["sudo", f"YDOTOOL_SOCKET={SOCK}", "ydotool", *args],
        check=False,
        capture_output=True,
    )


def shot():
    """The framebuffer as (w, h, XRGB8888 bytes).

    `nitro-shot --raw` hands back the readback unencoded, which is both
    faster than a PNG round trip and — more to the point — needs no image
    library on the box. The test box has no ImageMagick.
    """
    raw = subprocess.run([SHOT, "--raw"], check=True, capture_output=True).stdout
    out = subprocess.run([SHOT, "--outputs"], check=True, capture_output=True).stdout
    # One `NAME WxH@refresh_mhz` line per output; the first is ours.
    mode = out.decode().split()[1]
    w, h = (int(v) for v in mode.split("@")[0].split("x"))
    return w, h, raw


def cursor(before, after):
    """Centre of the pixels that changed between two frames.

    The cursor is the only thing moving, so "what changed" is the cursor
    at its new position plus the hole it left behind. Taking the centroid
    of the *new* frame's dark pixels among those is what separates the
    two; the arrow is drawn dark on a light border.
    """
    w, h, a = before
    _, _, b = after
    xs, ys = [], []
    stride = w * 4
    for y in range(0, h, 2):
        row = y * stride
        for x in range(0, w, 2):
            i = row + x * 4
            # XRGB8888 little-endian: [B, G, R, X].
            if a[i : i + 3] != b[i : i + 3] and b[i + 2] < 0x80:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    # The arrow's hotspot is its top-left tip, not its centroid.
    return min(xs), min(ys)


def locate():
    """Where is the pointer now? Nudge it and see what moved."""
    before = shot()
    ydotool("mousemove", "--", "4", "4")
    time.sleep(0.25)
    after = shot()
    pos = cursor(before, after)
    if pos is None:
        return None
    # Undo the nudge so the answer describes the pointer as we found it.
    ydotool("mousemove", "--", "-4", "-4")
    time.sleep(0.2)
    return pos


def move_to(x, y, tries=4):
    """Land the pointer on (x, y), correcting from what the screen says."""
    # Park first: from a known corner the first guess is close.
    ydotool("mousemove", "--", "-10000", "-10000")
    time.sleep(0.25)
    ydotool("mousemove", "--", str(x // 2), str(y // 2))
    time.sleep(0.3)
    for _ in range(tries):
        pos = locate()
        if pos is None:
            print("cannot see the cursor", file=sys.stderr)
            return None
        dx, dy = x - pos[0], y - pos[1]
        if abs(dx) <= 2 and abs(dy) <= 2:
            return pos
        # Relative moves are accelerated, so correct by the *observed*
        # error rather than by a predicted one; it converges in two or
        # three passes because the error shrinks each time.
        ydotool("mousemove", "--", str(dx), str(dy))
        time.sleep(0.3)
    return locate()


def window_origin():
    """Top-left of the topmost light (window) region on screen.

    A window's content is near-white and the desktop is a dark gradient,
    so the bounding box of light pixels is the windows' extent. It is a
    blunt instrument and exactly right for one job: telling whether a
    drag moved something, and by how much.
    """
    w, h, d = shot()
    stride = w * 4
    best = None
    for y in range(40, h, 2):
        row = y * stride
        for x in range(0, w, 2):
            i = row + x * 4
            if d[i] > 0xC0 and d[i + 1] > 0xC0 and d[i + 2] > 0xC0:
                if best is None or (y, x) < (best[1], best[0]):
                    best = (x, y)
        if best is not None:
            return best
    return best


def app_size(app):
    """A nitro app's own view of its window size, via `hey`.

    The authority on "did the window resize", and the reason this is not
    done by looking at pixels: the compositor draws a **software cursor**
    (`docs/testbox.md` — the hardware plane is not in screenshots), the
    cursor is light, and the cursor moves with the drag. A bounding box
    of light pixels therefore grows exactly as far as the pointer does,
    which makes a resize loop that closes on it report a perfect success
    while the window has not moved a pixel. It did, for three runs.
    """
    out = subprocess.run(
        [os.path.expanduser("~/nitro-bin/hey"), app, "get", "window", "bounds"],
        check=False,
        capture_output=True,
    ).stdout.decode()
    try:
        x, y, w, h = (int(v) for v in out.strip().split(","))
    except ValueError:
        return None
    return w, h


def window_box():
    """Bounding box of the light (window) pixels: (x0, y0, x1, y1).

    **Includes the software cursor**, which is light too. Use it only for
    "is anything on screen", never for measuring a window during a drag;
    `app_size` is the honest answer to that.
    """
    w, h, d = shot()
    stride = w * 4
    xs, ys = [], []
    for y in range(40, h, 2):
        row = y * stride
        for x in range(0, w, 2):
            i = row + x * 4
            if d[i] > 0xC0 and d[i + 1] > 0xC0 and d[i + 2] > 0xC0:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    return min(xs), min(ys), max(xs), max(ys)


def main():
    if len(sys.argv) >= 4 and sys.argv[1] == "move":
        x, y = int(sys.argv[2]), int(sys.argv[3])
        got = move_to(x, y)
        print(f"asked {x},{y} landed {got}")
    elif len(sys.argv) >= 6 and sys.argv[1] == "drag":
        x1, y1, x2, y2 = (int(v) for v in sys.argv[2:6])
        got = move_to(x1, y1)
        print(f"press at {x1},{y1} (landed {got})")
        ydotool("click", "0x40")
        time.sleep(0.3)
        # Closing the loop on the **window**, not on the cursor. While a
        # drag is in flight the "what changed between two frames" trick
        # cannot see the pointer any more: the window moves with it, so
        # everything the window covers changes too. But the window's own
        # top-left is exactly what a drag is supposed to move, so track
        # that instead and stop when it has gone far enough.
        #
        # Iterating also absorbs the thing that made the first attempt
        # undershoot by 10x: libinput **decelerates** small slow relative
        # moves (`docs/testbox.md` records the acceleration curve as
        # non-linear; the slow end is the half nobody hits with a mouse).
        # A single -200 request moved the window 38 pixels. Asking again
        # from the error each time converges anyway, and needs no model
        # of the curve.
        start = window_origin()
        want = (x2 - x1, y2 - y1)
        for _ in range(12):
            now = window_origin()
            if now is None or start is None:
                break
            got = (now[0] - start[0], now[1] - start[1])
            err = (want[0] - got[0], want[1] - got[1])
            if abs(err[0]) <= 4 and abs(err[1]) <= 4:
                break
            ydotool("mousemove", "--", str(err[0]), str(err[1]))
            time.sleep(0.25)
        ydotool("click", "0x80")
        time.sleep(0.5)
        end = window_origin()
        if start and end:
            print(f"window moved by {end[0] - start[0]},{end[1] - start[1]} "
                  f"(asked {want[0]},{want[1]})")
    elif sys.argv[1:2] == ["box"]:
        print(window_box())
    elif len(sys.argv) >= 7 and sys.argv[1] == "resize":
        # Drag a window's bottom-right corner, closing the loop on the
        # **bounding box** rather than on its origin: a resize moves the
        # far edge and leaves the near one where it was, so the origin —
        # what `drag` tracks — never changes and the loop would give up
        # having decided nothing happened.
        app, x1, y1, dx, dy = (
            sys.argv[2],
            *(int(v) for v in sys.argv[3:7]),
        )
        got = move_to(x1, y1)
        print(f"press at {x1},{y1} (landed {got})")
        start = app_size(app)
        ydotool("click", "0x40")
        time.sleep(0.3)
        for _ in range(12):
            now = app_size(app)
            if now is None or start is None:
                break
            grew = (now[0] - start[0], now[1] - start[1])
            err = (dx - grew[0], dy - grew[1])
            if abs(err[0]) <= 4 and abs(err[1]) <= 4:
                break
            ydotool("mousemove", "--", str(err[0]), str(err[1]))
            time.sleep(0.3)
        ydotool("click", "0x80")
        time.sleep(0.5)
        end = app_size(app)
        print(f"{app} {start} -> {end} (asked +{dx},+{dy})")
    else:
        print(__doc__)
        sys.exit(2)


if __name__ == "__main__":
    main()
