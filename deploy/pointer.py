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

    ./pointer.py move X Y              # land the pointer on X,Y
    ./pointer.py drag X1 Y1 X2 Y2 [app] # press at X1,Y1, release at X2,Y2
    ./pointer.py calibrate             # measure ABS_SCALE on a new box

Placement is **absolute** (`ydotool mousemove -a`), which skips the
acceleration curve entirely and lands on the pixel in one call; the
closed loop below is now only a check. Pass `app` to `drag` and the
window is tracked by `hey <app> get window bounds` rather than by light
pixels — the pixel heuristic assumes a dark desktop and the default
scheme is light, so it reports the wallpaper.

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
# The absolute device's coordinate space, as a multiple of the mode. Two on
# this box: `mousemove -a -x X/2 -y Y/2` lands at (X, Y). See `calibrate`.
ABS_SCALE = 2.0


def ydotool(*args):
    subprocess.run(
        ["sudo", f"YDOTOOL_SOCKET={SOCK}", "ydotool", *args],
        check=False,
        capture_output=True,
    )


def move_abs(x, y):
    """Put the pointer at (x, y) in **absolute** mode.

    `ydotool mousemove -a` bypasses libinput's acceleration entirely, so
    there is no curve to model and no loop to close: one call lands on the
    pixel. The catch is the factor of two — on this box the absolute
    device's coordinate space is twice the mode's, so `-x X/2 -y Y/2`
    lands at (X, Y). `docs/testbox.md` records the measurement; `ABS_SCALE`
    is where to change it if another box disagrees, and `calibrate()`
    below is how to find out.
    """
    ydotool("mousemove", "-a", "-x", str(int(x // ABS_SCALE)),
            "-y", str(int(y // ABS_SCALE)))
    time.sleep(0.25)


def calibrate():
    """Measure ABS_SCALE: ask for (200, 200) absolute and look.

    Run it once on a new box. A box where the absolute space *is* the
    mode reports 1.0; this one reports 2.0.
    """
    ydotool("mousemove", "-a", "-x", "100", "-y", "100")
    time.sleep(0.4)
    pos = locate()
    if pos is None:
        print("cannot see the cursor", file=sys.stderr)
        return None
    # Asked for 100,100 in the device's space; where did it land?
    scale = (pos[0] / 100.0, pos[1] / 100.0)
    print(f"asked 100,100 landed {pos} -> ABS_SCALE ~ {scale}")
    return scale


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
    """Land the pointer on (x, y).

    **Absolute mode first**, which is exact and needs no screenshot at
    all; the closed loop below is only a check, and only runs while the
    cursor can still be seen. That ordering is the fix for the thing that
    made this script unreliable once the default scheme became *light*:
    the locator looks for a dark arrow among changed pixels, and on a
    light desktop it often finds nothing and returns `None`. A `None` used
    to mean "give up"; now it means "the absolute move already did the
    job", which it did.
    """
    move_abs(x, y)
    for _ in range(tries):
        pos = locate()
        if pos is None:
            # Cannot see it, so cannot correct it — and after an absolute
            # move there is nothing to correct. Trust the placement.
            return (x, y)
        dx, dy = x - pos[0], y - pos[1]
        if abs(dx) <= 2 and abs(dy) <= 2:
            return pos
        # Correct absolutely too: a relative nudge would put the
        # acceleration curve back in the picture, which is the whole thing
        # absolute mode is here to avoid.
        move_abs(x + dx * ABS_SCALE, y + dy * ABS_SCALE)
    return locate() or (x, y)


def app_origin(app):
    """A nitro app's own view of where its window is, via `hey`.

    The honest answer, and the one to use: the app is asked, so no pixel
    heuristic can be fooled. `hey <app> get window bounds` prints
    `x,y,w,h`.
    """
    b = app_bounds(app)
    return None if b is None else (b[0], b[1])


def app_bounds(app):
    """`hey <app> get window bounds` as (x, y, w, h), or None."""
    out = subprocess.run(
        [os.path.expanduser("~/nitro-bin/hey"), app, "get", "window", "bounds"],
        check=False,
        capture_output=True,
    ).stdout.decode()
    try:
        return tuple(int(v) for v in out.strip().split(","))
    except ValueError:
        return None


def window_origin():
    """Top-left of the topmost light (window) region on screen.

    **Deprecated; prefer `app_origin`.** It assumes "light pixels on a
    dark desktop", which stopped being true when the default scheme became
    *light* — the desktop gradient is now light too, so this finds the
    wallpaper and reports the same origin whatever the window does. Kept
    only for a window belonging to no nitro app (nothing to ask), and the
    caller must know that is what it is getting.
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
    b = app_bounds(app)
    return None if b is None else (b[2], b[3])


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
    elif sys.argv[1:2] == ["calibrate"]:
        calibrate()
    elif len(sys.argv) >= 6 and sys.argv[1] == "drag":
        x1, y1, x2, y2 = (int(v) for v in sys.argv[2:6])
        # Optional trailing app name: track *it* rather than light pixels.
        app = sys.argv[6] if len(sys.argv) >= 7 else None
        track = (lambda: app_origin(app)) if app else window_origin
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
        start = track()
        want = (x2 - x1, y2 - y1)
        for _ in range(12):
            now = track()
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
        end = track()
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
