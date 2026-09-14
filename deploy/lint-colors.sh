#!/usr/bin/env bash
# Fail on a hard-coded colour in an app or a widget.
#
# # Why this exists
#
# M4-F moved every colour on the desktop into one semantic table
# (`nitro_core::palette::Role`) so that `theme.scheme = dark` in
# `server.conf` changes the whole screen. That property is only as good
# as the weakest app: one `Color::rgb(0x33, 0x88, 0xff)` in a widget is
# one colour the user's switch cannot reach, and it will not look wrong
# until somebody flips the scheme — by which time it is five apps, not
# one. So the rule is enforced mechanically and at merge time, not
# remembered.
#
# The rule: **outside the allow-list below, no source file may construct
# a colour.** If a widget needs a colour it does not have a role for, the
# answer is to add a role (see `docs/theme.md`), not to write the value
# down.
#
# # What counts as constructing a colour
#
#   Color::rgb(      Color::rgba(      Color::from_u32(
#   a bare 0xRRGGBB literal (six hex digits)
#
# `Color::BLACK`, `Color::WHITE` and `Color::TRANSPARENT` are *not*
# colours in this sense: they are the identity values a compositing API
# needs (a fully transparent border, an opaque clear), they cannot drift
# from a scheme, and forbidding them would only produce a role called
# "black".
#
# # Who is allowed
#
# Three kinds of file, and each earns it for a different reason:
#
#   * `nitro-core/src/palette.rs` — the table itself. This is where the
#     colours live; that is the point.
#   * the rasteriser, the scene graph, the wire codec, the PPM/PNG
#     decoders — they *transport and blend* colours, they do not choose
#     any. A `Color::rgb(r, g, b)` there is arithmetic on somebody else's
#     value.
#   * `nitro-term/src/vt.rs` — the SGR tables. `38;5;n` and `38;2;r;g;b`
#     are **content**: the program running in the terminal chose that
#     colour and a desktop theme has no business overriding it.
#
# Test modules (`#[cfg(test)]`, `tests/`, `benches/`, `examples/`) are
# skipped: a test asserting on a specific pixel has to name one, and a
# test's colours never reach a user's screen.
#
# # The per-line escape hatch
#
# A single line — or the comment block directly above it — may carry
#
#     // lint-colors: allow — <reason>
#
# and is then skipped. It exists because whole-file allow-listing is too
# blunt for the two real cases in this tree: `nitro-term`'s widget
# resolves a truecolor `SGR 38;2;r;g;b` (content) in the same file that
# paints the cursor (chrome), and `nitro-wallpaper`'s `--color` parser
# turns a *user's own* six hex digits into a `Color` in the same file as
# the themed gradient. Allow-listing either file would stop the lint
# watching the part that matters. A reason is mandatory by convention
# and read at review — an unexplained pragma is the thing review is for.
#
# Usage:
#   deploy/lint-colors.sh            # the whole workspace
#   deploy/lint-colors.sh path ...   # specific files, for a quick check
#
# Exit status is 0 when clean, 1 with a `file:line: message` per finding.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# Files that may name colours. Matched against the repo-relative path as
# bash globs, so `crates/nitro-raster/src/*` covers the whole crate.
allow=(
    # The palette itself: the one place colours are written down.
    'crates/nitro-core/src/palette.rs'
    # Rasteriser and scene graph: blending, not choosing.
    'crates/nitro-raster/src/*'
    'crates/nitro-scene/src/*'
    # The wire codec's `Color` twin, and the doc examples in its lib.rs.
    'crates/nitro-wire/src/wire.rs'
    'crates/nitro-wire/src/msg.rs'
    'crates/nitro-wire/src/lib.rs'
    # Image decoding and screenshot encoding: pixels in, pixels out.
    'crates/nitro-wallpaper/src/ppm.rs'
    'crates/nitro-shot/src/png.rs'
    # The VT's SGR tables: a program's own colours are content, not
    # chrome. See the header.
    'crates/nitro-term/src/vt.rs'
    # The demo scene is a rendering test pattern: its colours are the
    # thing under test, and it is not a desktop app.
    'crates/nitro-demo/src/*'
    # The server's readback path converts XRGB words back to colours.
    'crates/nitro-server/src/render.rs'
    # The fake backend's synthetic test image.
    'crates/nitro-server/src/frame.rs'
)

allowed() {
    local path="$1" pattern
    for pattern in "${allow[@]}"; do
        # shellcheck disable=SC2053 # glob match is exactly what is wanted
        [[ $path == $pattern ]] && return 0
    done
    return 1
}

# The files to scan: what was asked for, or every `src/` file in the
# workspace. Tests, benches and examples are out of scope (see header).
if (($# > 0)); then
    mapfile -t files < <(printf '%s\n' "$@")
else
    mapfile -t files < <(find crates -type f -name '*.rs' -path '*/src/*' | sort)
fi

# Strip the `#[cfg(test)] mod tests { ... }` tail before scanning: it
# always runs to the end of the file in this tree, which makes "delete
# from the marker" exact rather than a brace-counting guess. A file with
# no such marker is scanned whole.
#
# `grep -n` on the trimmed text keeps the original line numbers, because
# the trim only ever removes a suffix.
findings=0
for f in "${files[@]}"; do
    [[ -f $f ]] || continue
    allowed "$f" && continue
    cut=$(grep -n '^#\[cfg(test)\]' "$f" | head -1 | cut -d: -f1 || true)
    if [[ -n ${cut:-} ]]; then
        body=$(head -n "$((cut - 1))" "$f")
    else
        body=$(cat "$f")
    fi
    mapfile -t lines < <(printf '%s\n' "$body")
    while IFS= read -r hit; do
        [[ -z $hit ]] && continue
        n=${hit%%:*}
        text=${hit#*:}
        # A comment is not code: a doc example is checked by the
        # doctest, and prose naming a colour is prose.
        [[ $text =~ ^[[:space:]]*(//|\*) ]] && continue
        # The pragma, on this line or anywhere in the contiguous comment
        # block directly above it. A block rather than a single line
        # because the reason is usually a sentence, and a reason that has
        # to fit on one line is a reason nobody writes.
        [[ $text == *"lint-colors: allow"* ]] && continue
        pragma=""
        k=$((n - 2))
        while ((k >= 0)); do
            line=${lines[$k]:-}
            [[ $line =~ ^[[:space:]]*(//|\*) ]] || break
            [[ $line == *"lint-colors: allow"* ]] && pragma=1 && break
            k=$((k - 1))
        done
        [[ -n $pragma ]] && continue
        echo "$f:$n:$text"
        findings=$((findings + 1))
    done < <(printf '%s\n' "$body" |
        grep -nE 'Color::rgba?\(|Color::from_u32\(|\b0x[0-9a-fA-F]{6}\b' || true)
done

if ((findings > 0)); then
    cat >&2 <<'EOF'

A colour is written down outside the palette.

Colours come from roles: `ui.color(ColorRole::Accent)`,
`cx.color(ColorRole::TextDim)`, `.color_role(ColorRole::Text)` on a
label. If the colour you need has no role yet, add one to
`crates/nitro-core/src/palette.rs` — both schemes, and a line in
`docs/theme.md`'s table. See docs/theme.md §Adding a role.

If this file genuinely transports colours rather than choosing them
(a rasteriser, a decoder, an SGR table), add it to `allow` in
deploy/lint-colors.sh with a sentence saying why — or, for one line,
put `// lint-colors: allow — <reason>` on it or in the comment above.
EOF
    exit 1
fi

echo "lint-colors: ${#files[@]} file(s), no hard-coded colours"
