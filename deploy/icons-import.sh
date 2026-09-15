#!/usr/bin/env bash
# Regenerate crates/nitro-icons/src/set.rs from Bootstrap Icons.
#
# Run it directly -- there is deliberately no `just` recipe, because this is
# not part of the build: `set.rs` is committed, and importing is a rare,
# reviewed, network-touching act.
#
#   bash deploy/icons-import.sh                 # re-import exactly icons.txt
#   bash deploy/icons-import.sh gear house      # add those names, then re-import
#
# With arguments the names are appended to `crates/nitro-icons/icons.txt`
# (deduplicated, sorted) and the whole list is regenerated, so adding an icon
# is one command and leaves the curated list and the generated file in step.
#
# The run is idempotent: the output depends only on `icons.txt` and the pinned
# upstream commit, so running it twice produces a byte-identical `set.rs`.
#
# ---------------------------------------------------------------------------
# Why the paths are rewritten rather than copied verbatim
# ---------------------------------------------------------------------------
# zeno 0.3.3 implements SVG elliptical arcs, but its path parser does *not*
# accept an **implicit repeated arc argument set** -- the SVG 1.1 8.3.2 form
# where one `a` letter is followed by several 7-argument groups:
#
#     M11.5 2a1.5 1.5 0 1 0 0 3 1.5 1.5 0 0 0 0-3
#                              ^ second group, no repeated 'a'
#
# `svg_parser.rs`'s `arc_rest_arguments` starts reading `ry` without first
# skipping the separator, so the parse stops mid-path. zeno reports this via
# `validate_svg`, but `Mask::render_into` does not: it silently renders the
# prefix it managed to parse, which is how you get an icon that is a stray
# fragment instead of an error. 13 of the curated icons hit this.
#
# So the importer normalises every path to absolute, arc-free commands
# (M/L/C/Q/Z): arcs become cubic Béziers via the exact SVG F.6.5/F.6.6
# endpoint-parameterisation formulae. That also sidesteps a rounding bug in
# zeno's own arc code (`TAU: f32 = 3.141579 * 2.` in `path_builder.rs` -- note
# the transposed digits), so the emitted curves are marginally *more* accurate
# than what zeno would have produced from the original arcs. Verified against
# the 33 curated icons zeno can parse natively: worst per-pixel alpha
# difference is 38/255 at the antialiased edge, and no fully-opaque pixel ever
# flips to fully-clear at 16/32/64/128 px.
#
# The generated `d` strings always carry an explicit command letter per
# segment, so they can never re-enter the implicit-repeat case.
#
# ---------------------------------------------------------------------------
# What is rejected
# ---------------------------------------------------------------------------
# Hard errors, naming the icon, because each would silently produce a wrong
# glyph if waved through:
#
#   * a viewBox that is not `0 0 16 16` -- the whole crate assumes a 16-unit
#     grid, and a 20px icon would quietly render at 80% scale;
#   * any drawing element other than `<path>`, with one sanctioned exception
#     below;
#   * two paths in one icon that disagree on `fill-rule`. The generator
#     concatenates an icon's paths into a single `d` and stores one flag, so a
#     disagreement cannot be represented; filling both under either rule draws
#     the wrong shape. Upstream `arrow-clockwise`, `arrow-counterclockwise` and
#     `arrow-repeat` are all like this -- see icons.txt for the substitution.
#
# The sanctioned exception: `circle-fill` is upstream a bare
# `<circle cx cy r>`, and it is too useful (status dots, unread markers) to
# drop over its element type. A circle is converted here to the equivalent
# two-arc path, which then goes through the same arc->cubic normaliser as
# everything else. `<rect>` is handled the same way (upstream `align-top` and
# `align-bottom` use one) though no curated icon currently needs it.
set -euo pipefail

# Bootstrap Icons, MIT. Pinned so that a re-import is reproducible and a
# deliberate upstream bump shows up as a one-line diff here.
# Exported rather than `readonly` because the generator below reads them from
# the environment.
export SHA=6945b7006285d444cc17ff2e22c7691719229526
export UPSTREAM=https://github.com/twbs/icons

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly crate="$root/crates/nitro-icons"
readonly list="$crate/icons.txt"
readonly out="$crate/src/set.rs"

[[ -f "$list" ]] || { echo "icons-import: missing $list" >&2; exit 1; }

# --- extend the curated list, if asked -------------------------------------
# Comments and blank lines are preserved as a header block; the names below
# them are rewritten deduplicated and sorted. `LC_ALL=C` so the sort matches
# Rust's byte-order `str` comparison, which is what the binary search in
# `lookup` relies on.
if (( $# > 0 )); then
    header="$(sed -n '/^[[:space:]]*$/!{/^[[:space:]]*#/!q;}; p' "$list")"
    names="$(
        { grep -v '^[[:space:]]*#' "$list" | grep -v '^[[:space:]]*$' || true
          printf '%s\n' "$@"
        } | LC_ALL=C sort -u
    )"
    printf '%s\n%s\n' "$header" "$names" > "$list"
    echo "icons-import: icons.txt now has $(wc -l <<< "$names") names"
fi

mapfile -t icons < <(grep -v '^[[:space:]]*#' "$list" | grep -v '^[[:space:]]*$' | LC_ALL=C sort -u)
(( ${#icons[@]} > 0 )) || { echo "icons-import: $list lists no icons" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "icons-import: fetching ${#icons[@]} icons from $UPSTREAM @ ${SHA:0:12}"
for name in "${icons[@]}"; do
    url="https://raw.githubusercontent.com/twbs/icons/$SHA/icons/$name.svg"
    if ! curl -sfL --retry 3 -o "$work/$name.svg" "$url"; then
        echo "icons-import: no such icon upstream at ${SHA:0:12}: '$name' ($url)" >&2
        exit 1
    fi
done

# The upstream licence travels with the paths; refresh it from the same commit
# so it can never drift from the art it covers.
curl -sfL --retry 3 -o "$crate/LICENSE.bootstrap-icons" \
    "https://raw.githubusercontent.com/twbs/icons/$SHA/LICENSE"

SVG_DIR="$work" OUT="$out" python3 - "${icons[@]}" <<'PYTHON'
"""Parse the fetched SVGs and emit `set.rs`.

Kept in one heredoc rather than a separate file because it is meaningless
outside this script: it hard-codes this crate's assumptions (16-unit grid, one
path and one fill rule per icon, arc-free output).
"""
import math
import os
import re
import sys

SHA = os.environ["SHA"]
UPSTREAM = os.environ["UPSTREAM"]
SVG_DIR = os.environ["SVG_DIR"]
OUT = os.environ["OUT"]

NUMBER = re.compile(r"[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?")
ELEMENT = re.compile(r"<([a-zA-Z][a-zA-Z0-9]*)\b([^>]*)>")
ATTR = re.compile(r"([a-zA-Z-]+)\s*=\s*\"([^\"]*)\"")

# Elements that draw. Anything here that is not `path` is converted to a path
# below; anything *not* here and not in IGNORED is a hard error.
DRAWING = {"path", "circle", "rect"}
IGNORED = {"svg", "title", "desc", "defs", "style", "metadata"}


class IconError(Exception):
    """A malformed or unrepresentable icon, reported with the icon's name."""


def attrs(text):
    return dict(ATTR.findall(text))


def tokenise(d):
    """Yield (command, [numbers]) groups, expanding implicit argument repeats.

    Implicit repeats are the whole reason this exists: SVG lets one command
    letter be followed by several argument sets, and after `M`/`m` the repeat
    is an implicit `L`/`l` (SVG 1.1 8.3.2) -- for the repeats only, never for
    the first set.
    """
    arity = {"l": 2, "h": 1, "v": 1, "c": 6, "s": 4, "q": 4, "t": 2, "a": 7, "m": 2}
    out, i, n = [], 0, len(d)
    cmd, first = None, True
    while i < n:
        ch = d[i]
        if ch.isspace() or ch == ",":
            i += 1
            continue
        if ch.isalpha():
            cmd, first = ch, True
            i += 1
            if cmd in "zZ":
                out.append((cmd, []))
                cmd = None
            continue
        if cmd is None:
            raise IconError(f"number before any command at byte {i}")
        effective = cmd if first else {"M": "L", "m": "l"}.get(cmd, cmd)
        first = False
        if effective.lower() not in arity:
            raise IconError(f"unknown path command {effective!r}")
        args = []
        for _ in range(arity[effective.lower()]):
            while i < n and (d[i].isspace() or d[i] == ","):
                i += 1
            if i >= n:
                raise IconError(f"path ended mid-argument for {effective!r}")
            # Arc flags are single '0'/'1' digits that may be glued to the next
            # number ("0 1 0 0 3" and "010 0 3" are the same), so they cannot
            # go through the general number scanner.
            if effective.lower() == "a" and len(args) in (3, 4):
                if d[i] not in "01":
                    raise IconError(f"bad arc flag {d[i]!r} at byte {i}")
                args.append(float(d[i]))
                i += 1
                continue
            m = NUMBER.match(d, i)
            if not m:
                raise IconError(f"expected a number at byte {i}: {d[i:i + 12]!r}")
            args.append(float(m.group()))
            i = m.end()
        out.append((effective, args))
    return out


def arc_to_cubics(x0, y0, rx, ry, phi_deg, large, sweep, x1, y1):
    """Endpoint-parameterised elliptical arc -> cubic segments (SVG F.6.5/F.6.6)."""
    if x0 == x1 and y0 == y1:
        return []                      # F.6.2: zero-length arc is a no-op
    if rx == 0 or ry == 0:
        return [("L", [x1, y1])]       # F.6.2: a zero radius degenerates to a line
    rx, ry = abs(rx), abs(ry)
    phi = math.radians(phi_deg % 360.0)
    cos_p, sin_p = math.cos(phi), math.sin(phi)
    dx2, dy2 = (x0 - x1) / 2.0, (y0 - y1) / 2.0
    x1p = cos_p * dx2 + sin_p * dy2
    y1p = -sin_p * dx2 + cos_p * dy2
    # F.6.6: scale up radii that are too small to span the endpoints.
    lam = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry)
    if lam > 1.0:
        scale = math.sqrt(lam)
        rx, ry = rx * scale, ry * scale
    num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p
    den = rx * rx * y1p * y1p + ry * ry * x1p * x1p
    coef = math.sqrt(max(num / den, 0.0))
    if large == sweep:
        coef = -coef
    cxp, cyp = coef * rx * y1p / ry, -coef * ry * x1p / rx
    cx = cos_p * cxp - sin_p * cyp + (x0 + x1) / 2.0
    cy = sin_p * cxp + cos_p * cyp + (y0 + y1) / 2.0

    def angle(ux, uy, vx, vy):
        norm = math.hypot(ux, uy) * math.hypot(vx, vy)
        cosine = 0.0 if norm == 0 else max(-1.0, min(1.0, (ux * vx + uy * vy) / norm))
        a = math.acos(cosine)
        return -a if ux * vy - uy * vx < 0 else a

    ux, uy = (x1p - cxp) / rx, (y1p - cyp) / ry
    vx, vy = (-x1p - cxp) / rx, (-y1p - cyp) / ry
    theta = angle(1.0, 0.0, ux, uy)
    sweep_angle = angle(ux, uy, vx, vy)
    if not sweep and sweep_angle > 0:
        sweep_angle -= 2.0 * math.pi
    elif sweep and sweep_angle < 0:
        sweep_angle += 2.0 * math.pi

    # One cubic per quarter turn or less: that is the standard bound at which
    # the Bézier approximation of a circular arc stays well under a
    # thousandth of the radius, far below a pixel on a 16-unit grid.
    count = max(1, math.ceil(abs(sweep_angle) / (math.pi / 2.0) - 1e-9))
    delta = sweep_angle / count
    kappa = 4.0 / 3.0 * math.tan(delta / 4.0)

    def point(c, s):
        return (cos_p * rx * c - sin_p * ry * s + cx,
                sin_p * rx * c + cos_p * ry * s + cy)

    def deriv(c, s):
        return (-cos_p * rx * s - sin_p * ry * c,
                -sin_p * rx * s + cos_p * ry * c)

    out, px, py = [], x0, y0
    for _ in range(count):
        nxt = theta + delta
        c1, s1 = math.cos(theta), math.sin(theta)
        c2, s2 = math.cos(nxt), math.sin(nxt)
        ex, ey = point(c2, s2)
        d1x, d1y = deriv(c1, s1)
        d2x, d2y = deriv(c2, s2)
        out.append(("C", [px + kappa * d1x, py + kappa * d1y,
                          ex - kappa * d2x, ey - kappa * d2y, ex, ey]))
        px, py, theta = ex, ey, nxt
    return out


def normalise(d):
    """Absolute, arc-free command list: only M, L, C, Q and Z survive."""
    cx = cy = sx = sy = 0.0
    cubic_c2 = quad_c = None
    out = []
    for cmd, a in tokenise(d):
        upper, rel = cmd.upper(), cmd.islower()
        if upper == "Z":
            out.append(("Z", []))
            cx, cy = sx, sy
            cubic_c2 = quad_c = None
        elif upper == "M":
            x, y = (a[0] + cx, a[1] + cy) if rel else (a[0], a[1])
            out.append(("M", [x, y]))
            cx = sx = x
            cy = sy = y
            cubic_c2 = quad_c = None
        elif upper == "L":
            x, y = (a[0] + cx, a[1] + cy) if rel else (a[0], a[1])
            out.append(("L", [x, y]))
            cx, cy = x, y
            cubic_c2 = quad_c = None
        elif upper == "H":
            x = a[0] + cx if rel else a[0]
            out.append(("L", [x, cy]))
            cx = x
            cubic_c2 = quad_c = None
        elif upper == "V":
            y = a[0] + cy if rel else a[0]
            out.append(("L", [cx, y]))
            cy = y
            cubic_c2 = quad_c = None
        elif upper == "C":
            p = [a[0] + cx, a[1] + cy, a[2] + cx, a[3] + cy,
                 a[4] + cx, a[5] + cy] if rel else list(a)
            out.append(("C", p))
            cubic_c2, quad_c = (p[2], p[3]), None
            cx, cy = p[4], p[5]
        elif upper == "S":
            # Reflect the previous cubic's second control point; with no
            # preceding cubic the reflection is the current point (SVG 8.3.6).
            c1 = (2 * cx - cubic_c2[0], 2 * cy - cubic_c2[1]) if cubic_c2 else (cx, cy)
            r = [a[0] + cx, a[1] + cy, a[2] + cx, a[3] + cy] if rel else list(a)
            p = [c1[0], c1[1], r[0], r[1], r[2], r[3]]
            out.append(("C", p))
            cubic_c2, quad_c = (p[2], p[3]), None
            cx, cy = p[4], p[5]
        elif upper == "Q":
            p = [a[0] + cx, a[1] + cy, a[2] + cx, a[3] + cy] if rel else list(a)
            out.append(("Q", p))
            quad_c, cubic_c2 = (p[0], p[1]), None
            cx, cy = p[2], p[3]
        elif upper == "T":
            c = (2 * cx - quad_c[0], 2 * cy - quad_c[1]) if quad_c else (cx, cy)
            r = [a[0] + cx, a[1] + cy] if rel else list(a)
            out.append(("Q", [c[0], c[1], r[0], r[1]]))
            quad_c, cubic_c2 = c, None
            cx, cy = r[0], r[1]
        elif upper == "A":
            x, y = (a[5] + cx, a[6] + cy) if rel else (a[5], a[6])
            for seg in arc_to_cubics(cx, cy, a[0], a[1], a[2],
                                     bool(a[3]), bool(a[4]), x, y):
                out.append(seg)
                if seg[0] == "C":
                    cubic_c2 = (seg[1][2], seg[1][3])
            cx, cy = x, y
            quad_c = None
        else:
            raise IconError(f"unhandled path command {cmd!r}")
    return out


def number(v):
    """Shortest round-trippable-enough decimal.

    Four places is ~1/1000 of a pixel at 16 px and well under a thousandth at
    512 px, so it is invisible, and it keeps `set.rs` small. `-0` is folded to
    `0` so the output cannot depend on the sign of a zero.
    """
    s = f"{v:.4f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "", "-") else s


def emit_path(cmds):
    """Serialise with an explicit letter per segment.

    Never emits an implicit argument repeat: that is exactly the construct
    zeno 0.3.3 mis-parses, and the point of this importer is that `set.rs`
    cannot contain one.
    """
    parts = []
    for cmd, args in cmds:
        s = cmd
        for v in args:
            t = number(v)
            if len(s) > 1 and not t.startswith("-"):
                s += " "
            s += t
        parts.append(s)
    return "".join(parts)


def circle_to_path(a):
    """`<circle>` -> two half-arcs, the usual lossless spelling."""
    cx, cy, r = float(a.get("cx", 0)), float(a.get("cy", 0)), float(a["r"])
    return (f"M{number(cx - r)} {number(cy)}"
            f"A{number(r)} {number(r)} 0 1 1 {number(cx + r)} {number(cy)}"
            f"A{number(r)} {number(r)} 0 1 1 {number(cx - r)} {number(cy)}Z")


def rect_to_path(a):
    """`<rect>` -> an explicit rectangle. Rounded corners are not supported."""
    if a.get("rx") or a.get("ry"):
        raise IconError("rounded <rect> is not supported")
    x, y = float(a.get("x", 0)), float(a.get("y", 0))
    w, h = float(a["width"]), float(a["height"])
    return (f"M{number(x)} {number(y)}H{number(x + w)}V{number(y + h)}"
            f"H{number(x)}Z")


def parse_icon(name, svg):
    """(concatenated arc-free `d`, even_odd) for one icon, or raise IconError."""
    root = ELEMENT.search(svg)
    if root is None or root.group(1) != "svg":
        raise IconError("no <svg> root element")
    box = attrs(root.group(2)).get("viewBox", "").split()
    if box != ["0", "0", "16", "16"]:
        raise IconError(
            f"viewBox is {' '.join(box) or '<missing>'!r}, expected '0 0 16 16'; "
            "nitro-icons assumes a 16-unit grid"
        )

    pieces, rules = [], set()
    for tag, raw in ELEMENT.findall(svg):
        if tag in IGNORED or tag.startswith("/"):
            continue
        a = attrs(raw)
        if tag not in DRAWING:
            raise IconError(
                f"unsupported element <{tag}>; this importer understands only "
                f"{', '.join('<' + t + '>' for t in sorted(DRAWING))}"
            )
        if tag == "path":
            d = a.get("d")
            if not d:
                raise IconError("<path> without a 'd' attribute")
        elif tag == "circle":
            d = circle_to_path(a)
        else:
            d = rect_to_path(a)
        # A missing fill-rule is `nonzero` (SVG default). Only `<path>` can
        # carry one in this set; the converted shapes are simple and closed,
        # so either rule fills them identically.
        rules.add(a.get("fill-rule", "nonzero"))
        pieces.append(d)

    if not pieces:
        raise IconError("no drawing elements")
    if len(rules) > 1:
        raise IconError(
            f"paths disagree on fill-rule ({', '.join(sorted(rules))}); they "
            "cannot be concatenated into one path -- pick a different icon"
        )
    rule = rules.pop()
    if rule not in ("nonzero", "evenodd"):
        raise IconError(f"unknown fill-rule {rule!r}")
    return "".join(emit_path(normalise(d)) for d in pieces), rule == "evenodd"


names = sorted(sys.argv[1:])
entries, failures = [], []
for name in names:
    with open(os.path.join(SVG_DIR, f"{name}.svg"), encoding="utf-8") as fh:
        svg = fh.read()
    try:
        path, even_odd = parse_icon(name, svg)
    except IconError as exc:
        failures.append(f"  {name}: {exc}")
        continue
    if not path:
        failures.append(f"  {name}: produced an empty path")
        continue
    entries.append((name, path, even_odd))

if failures:
    sys.stderr.write("icons-import: refusing to generate set.rs:\n")
    sys.stderr.write("\n".join(failures) + "\n")
    sys.exit(1)

header = f'''//! The generated icon table. **Do not edit by hand.**
//!
//! Source:    <{UPSTREAM}> (Bootstrap Icons)
//! Licence:   MIT -- see `LICENSE.bootstrap-icons` beside this crate's manifest
//! Pinned at: {SHA}
//! Generated: `bash deploy/icons-import.sh`
//!
//! Paths are normalised to absolute, arc-free commands (`M`/`L`/`C`/`Q`/`Z`)
//! with an explicit command letter per segment, because zeno 0.3.3 mis-parses
//! implicit repeated arc argument sets. The importer explains why at length.
//!
//! Entries are sorted by name so `lookup` can binary search.

use crate::Icon;

/// Every curated icon, sorted by `name`.
pub(crate) static ICONS: &[Icon] = &[
'''

body = "".join(
    f'    Icon {{\n'
    f'        name: "{name}",\n'
    f'        path: "{path}",\n'
    f'        even_odd: {"true" if even_odd else "false"},\n'
    f'    }},\n'
    for name, path, even_odd in entries
)

with open(OUT, "w", encoding="utf-8") as fh:
    fh.write(header + body + "];\n")

total = sum(len(p) for _, p, _ in entries)
print(f"icons-import: wrote {len(entries)} icons, {total} bytes of path data -> {OUT}")
PYTHON

echo "icons-import: done; review the diff and run 'cargo test -p nitro-icons'"
