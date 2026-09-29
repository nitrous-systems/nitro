#!/usr/bin/env bash
# The footprint report: one block to paste against the baseline table in
# docs/budget.md ("Footprint baseline and the surface delta rule").
#
# Three halves:
#
#   binaries — `stat -c %s` of every shipped binary, release build. They
#              are already stripped (`[profile.release] strip = true`).
#              The list comes from `just` (FOOTPRINT_BINS, built from
#              `box_bins` in deploy/dev.just) so there is one source of
#              truth; a name that is not built prints `absent` rather
#              than failing. `nitro-gpu-vulkan` (#3920) is the GPU
#              helper; it is built but not yet installed on the box.
#   deps     — the two `cargo tree` numbers docs/budget.md's "Dependency
#              count" section defines: sorted unique lines, and distinct
#              external crate names.
#   box RSS  — `deploy/box-ps.sh` on the test box: the idle desktop tree.
#              Before measuring, the local nitro-server's md5 is compared
#              with the box's; a mismatch is a WARNING (the box is not
#              running this build — `just deploy` first), and the numbers
#              are printed anyway. NITRO_FOOTPRINT_NO_BOX=1 skips this
#              half; an unreachable box skips it with a note.
#
# Usage: just footprint [SECS]   (idle window for box-ps, default 10)
set -euo pipefail

secs="${1:-10}"
box="${NITRO_BOX:-kaspar@192.168.1.204}"
# Where the box keeps the binaries: ~/nitro-bin (box1, `unit` profile) or
# /usr/local/bin (testhost2, `gdm`). Expanded by the remote shell.
bindir="${NITRO_BOX_BINDIR:-~/nitro-bin}"
bins="${FOOTPRINT_BINS:-nitro-server nitro-gpu-vulkan nitro-video}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# The same build `just deploy-bins` ships. `--examples` is not optional:
# with the examples in the build cargo unifies features differently, and
# a `--bins`-only nitro-calc/nitro-bar differ from the deployed ones by a
# kilobyte and the md5 check below always fails.
cargo build --release --workspace --bins --examples >&2

sha=$(git rev-parse --short HEAD)
[[ -n $(git status --porcelain) ]] && sha="$sha (dirty)"
echo "footprint at $sha, $(date -u +%Y-%m-%dT%H:%MZ)"

echo
echo "== binaries (release, stripped) =="
printf '%-16s %10s\n' binary bytes
sum=0
for b in $bins; do
    f="target/release/$b"
    if [[ -f $f ]]; then
        n=$(stat -c %s "$f")
        sum=$(( sum + n ))
        printf '%-16s %10s\n' "$b" "$n"
    else
        printf '%-16s %10s\n' "$b" absent
    fi
done
printf '%-16s %10s\n' sum "$sum"

echo
echo "== dependencies =="
tree=$(cargo tree -e normal --prefix none)
printf '%-24s %4s\n' "cargo tree lines" "$(sort -u <<< "$tree" | wc -l)"
printf '%-24s %4s\n' "distinct external names" \
    "$(awk 'NF{print $1}' <<< "$tree" | grep -v '^nitro-' | sort -u | wc -l)"

echo
echo "== box idle RSS ($box) =="
if [[ ${NITRO_FOOTPRINT_NO_BOX:-0} == 1 ]]; then
    echo "skipped (NITRO_FOOTPRINT_NO_BOX=1)"
    exit 0
fi
if ! remote=$(ssh -o ConnectTimeout=5 -o BatchMode=yes "$box" \
        "md5sum $bindir/nitro-server" 2>/dev/null); then
    echo "box unreachable, skipped"
    exit 0
fi
local_md5=$(md5sum target/release/nitro-server | awk '{print $1}')
if [[ ${remote%% *} != "$local_md5" ]]; then
    echo "WARNING: box is not running this build's nitro-server; run \`just box=$box deploy\` first"
fi
ssh "$box" 'bash -s' "$secs" < deploy/box-ps.sh
