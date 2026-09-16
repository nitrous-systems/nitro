#!/usr/bin/env bash
# The benchmark matrix, run on the box, appended to tmp/bench/<sha>.jsonl.
#
# # Why a script and not a `for` loop in the binary
#
# Every run is a fresh process with a fresh connection, and that is load
# bearing rather than convenient. The CPU column is a `/proc` delta over
# the run, so a second scenario in the same process would inherit the
# first one's page cache, its heap and its already-warmed socket. The
# server's `stats` windows are the last N frames, so back-to-back runs in
# one process would bleed one scenario's paint times into the next one's
# `paint_us_mean`. One process per row is the only shape in which the
# before/after pair means what the column header says.
#
# # What it runs
#
#   1. `bandwidth` once — the denominator every pixel-path verdict needs.
#   2. The x11perf sweeps: rects/text at 10, 100, 500, 1000, 2000 nodes,
#      putimage at 100, 250, 500, 1080 px.
#   3. The demo effects at 640x480 (period-correct) and fullscreen.
#   4. The pairs: boing/boing-node, starfield/starfield-nodes,
#      balls/balls-nodes, text/text-static — each arm back to back, since
#      this box's numbers drift by the day and a comparison across two
#      evenings is not a comparison (chat nitro-testbox, #3702).
#   5. One `rects` control point with the shell killed, because the bar
#      and the launcher are up for every other row and a reader is
#      entitled to know what they cost.
#
# # The refresh sweep
#
# With `--modes "60 120"` the whole matrix runs twice, writing
# `output.<connector>.mode = 1920x1080@<Hz>` into the human's config and
# restarting the unit between arms. Two rules, both non-negotiable:
#
#   * The human's `server.conf` is **backed up and restored**, never
#     rewritten from scratch: it carries his dark scheme and his `de`
#     layout and those are his, not ours.
#   * After each restart the script **confirms `outputs` reports the rate
#     it asked for** before running a single scenario. Otherwise the 120
#     Hz column is a measurement of a switch that never happened — the
#     #3704 rule this room learned the hard way.
#
# Usage, on the box:
#   deploy/bench.sh [--seconds N] [--out FILE] [--modes "60 120"]
#                   [--connector HDMI-A-1] [--quick]
set -euo pipefail

seconds=10
out=""
modes=""
connector="HDMI-A-1"
quick=0
bin="${NITRO_BENCH_BIN:-$HOME/nitro-bin/nitro-bench}"

while (($#)); do
    case "$1" in
    --seconds) seconds="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --modes) modes="$2"; shift 2 ;;
    --connector) connector="$2"; shift 2 ;;
    --quick) quick=1; seconds=3; shift ;;
    -h | --help) sed -n '2,45p' "$0"; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done

: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"
export XDG_RUNTIME_DIR

sha="$(git -C "$HOME/src/ai/nitro" rev-parse --short HEAD 2>/dev/null || echo unknown)"
export NITRO_BENCH_SHA="$sha"
export NITRO_BENCH_HOST="$(hostname)"

if [[ -z $out ]]; then
    mkdir -p "$HOME/tmp/bench"
    out="$HOME/tmp/bench/$sha.jsonl"
fi
mkdir -p "$(dirname "$out")"

conf="$HOME/.config/nitro/server.conf"
backup="$(mktemp)"
restored=0

# The human's configuration is his. Restore it whatever happens — a
# `mode` line left behind would silently change every measurement anyone
# takes on this box afterwards, which is exactly the debris this room has
# spent five windows learning not to leave.
restore_conf() {
    ((restored)) && return 0
    restored=1
    if [[ -s $backup ]]; then
        install -m 644 "$backup" "$conf"
    elif [[ -n ${had_conf:-} ]]; then
        : # there was a file and it was empty; leave what is there
    else
        rm -f "$conf"
    fi
    rm -f "$backup"
}
trap restore_conf EXIT

had_conf=""
if [[ -f $conf ]]; then
    had_conf=1
    cp "$conf" "$backup"
fi

say() { printf '### %s\n' "$*" >&2; }

# One control request, through Python rather than `nc`: on this box `nc
# -q1 -U` silently returns nothing for a fraction of requests, and an
# empty reply reads exactly like the failure you would most plausibly
# believe about your own change (chat nitro-testbox, #3707/#3706).
control() {
    python3 - "$1" <<'PY'
import socket, sys
p = f"/run/user/{__import__('os').getuid()}/nitro/control.sock"
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2)
try:
    s.connect(p)
    s.sendall((sys.argv[1] + "\n").encode())
    buf = b""
    while True:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        buf += chunk
        if buf.endswith(b"\n\n"):
            break
    sys.stdout.write(buf.decode("utf-8", "replace"))
except Exception as e:
    print(f"err {e}", file=sys.stderr)
    sys.exit(1)
PY
}

# Run one scenario and append its JSON object. A failure is recorded as a
# comment rather than aborting the matrix: a table of thirty-nine runs
# beats an error about the fortieth.
bench() {
    local name="$1"; shift
    say "$name $*"
    if ! "$bin" "$name" --seconds "$seconds" --json "$@" >>"$out" 2>/tmp/bench-err.$$; then
        printf '# FAILED %s %s: %s\n' "$name" "$*" "$(tr '\n' ' ' </tmp/bench-err.$$)" >>"$out"
    fi
    rm -f "/tmp/bench-err.$$"
    # A breath between runs: the server's stats windows are the last N
    # frames and a back-to-back start would read the previous scenario's
    # tail as its own baseline.
    sleep 1
}

# Set the output's mode and prove it took. Returns non-zero when the
# server did not come back at the rate asked for, so the caller can skip
# that arm rather than record a mislabelled column.
set_mode() {
    local hz="$1"
    say "switching $connector to ${hz} Hz"
    mkdir -p "$(dirname "$conf")"
    # Rewrite only the mode line; everything else in the human's file
    # stays exactly as it was.
    local tmp
    tmp="$(mktemp)"
    grep -v "^output\.$connector\.mode" "$conf" 2>/dev/null >"$tmp" || true
    printf 'output.%s.mode = 1920x1080@%s\n' "$connector" "$hz" >>"$tmp"
    install -m 644 "$tmp" "$conf"
    rm -f "$tmp"
    sudo systemctl restart nitro-dev
    # Wait for the socket, then for the rate. Both bounded: a box that
    # did not come back is a failure to report, not a hang.
    local i
    for i in $(seq 1 60); do
        [[ -S $XDG_RUNTIME_DIR/nitro/control.sock ]] && break
        sleep 1
    done
    sleep 3
    local got
    got="$(control outputs | grep -o '@[0-9]*' | head -1 | tr -d '@')"
    local want=$((hz * 1000))
    if [[ $got != "$want" ]]; then
        say "REFUSING the ${hz} Hz arm: outputs reports @${got:-nothing}, not @$want"
        return 1
    fi
    say "confirmed: outputs reports @$got"
    return 0
}

matrix() {
    local tag="$1"

    # x11perf: sweep N until something breaks. The sweep is the benchmark;
    # a single point would only say "it was fine at 100".
    local ns="10 100 500 1000 2000"
    ((quick)) && ns="10 500"
    local n
    for n in $ns; do
        bench rects --n "$n" --note "$tag; x11perf -rect100 recolour"
        bench rects-move --n "$n" --note "$tag; x11perf -rect100 move"
    done
    # Text is the expensive one, so its sweep stops earlier: 2000 shaped
    # runs a frame is not a number anybody needs to see twice.
    local tns="10 100 500"
    ((quick)) && tns="10 100"
    for n in $tns; do
        # Back to back, because the whole claim is the difference.
        bench text --n "$n" --note "$tag; x11perf -ftext"
        bench text-static --n "$n" --note "$tag; the retained arm"
        bench text --n "$n" --size 24 --note "$tag; x11perf -f24text"
    done

    # The pixel path, swept by buffer edge. 1080 is the fullscreen case
    # expressed as a square, so the sweep and the fullscreen runs share a
    # denominator.
    local sizes="100 250 500 1080"
    ((quick)) && sizes="100 500"
    local s
    for s in $sizes; do
        bench putimage --size "$s" --note "$tag; x11perf -putimage$s"
    done

    bench scroll --n 500 --note "$tag; x11perf -scroll500"
    bench create --n 50 --note "$tag; x11perf -create"

    # The effects, at the period-correct size and then fullscreen. Both,
    # because 640x480 is what they were written for and 1920x1080 is what
    # they have to survive.
    local e
    for e in plasma fire rotozoom boing; do
        bench "$e" --note "$tag; 640x480, period-correct"
        bench "$e" --fullscreen --note "$tag; fullscreen"
    done

    # The pairs. Each arm immediately after its twin: this box's numbers
    # drift by the day, so a comparison assembled from two evenings is not
    # a comparison.
    bench boing --fullscreen --note "$tag; pixel arm of the pair"
    bench boing-node --fullscreen --note "$tag; node arm of the pair"
    local sn
    for sn in 100 500 2000; do
        ((quick)) && [[ $sn == 2000 ]] && continue
        bench starfield --n "$sn" --fullscreen --note "$tag; pixel arm"
        bench starfield-nodes --n "$sn" --fullscreen --note "$tag; node arm"
    done
    bench balls --n 32 --fullscreen --note "$tag; pixel arm"
    bench balls-nodes --n 32 --fullscreen --note "$tag; node arm"
}

say "sha $sha, writing $out"
printf '# nitro-bench matrix, sha %s, host %s, %s\n' \
    "$sha" "$NITRO_BENCH_HOST" "$(date -Is)" >>"$out"

"$bin" bandwidth --json >>"$out"

if [[ -z $modes ]]; then
    matrix "bar+launcher up"
else
    for hz in $modes; do
        if set_mode "$hz"; then
            matrix "bar+launcher up; ${hz} Hz"
        else
            printf '# SKIPPED the %s Hz arm: outputs did not report it\n' "$hz" >>"$out"
        fi
    done
fi

# The control arm. Every row above ran with the bar and the launcher up,
# which is the realistic baseline and is stated as such — but a reader is
# entitled to know what they cost, and the only way to say is to take them
# away for one point.
#
# The shell clients are killed, **not** the unit: stopping `nitro-dev`
# takes the server down with them and there would be nothing left to
# benchmark. `nitro-session` supervises the four and repairs a kill within
# about a second, so the control point is taken in the window a
# `systemctl kill`-free pkill opens — which is why it is one short run and
# why the note says so.
say "control: shell clients down"
sudo systemctl stop nitro-dev >/dev/null 2>&1 || true
sleep 2
sudo systemctl set-environment NITRO_BENCH_CONTROL=1 >/dev/null 2>&1 || true
sudo systemctl start nitro-dev
sleep 5
# Kill the three shell clients; the session restarts them, so the run has
# to start immediately and be short. Its note says exactly that, because a
# control arm whose control quietly came back is worse than no control.
pkill -f nitro-wallpaper || true
pkill -f nitro-bar || true
pkill -f nitro-launcher || true
before_procs="$(pgrep -c nitro || echo 0)"
bench rects --n 500 \
    --seconds 3 \
    --note "control: shell clients killed ($before_procs procs at start; the session restarts them within ~1 s, so this arm is 3 s)"
sudo systemctl unset-environment NITRO_BENCH_CONTROL >/dev/null 2>&1 || true

restore_conf
sudo systemctl restart nitro-dev || true
say "done: $out"
printf 'wrote %s (%s runs)\n' "$out" "$(grep -c '^{' "$out" || echo 0)"
