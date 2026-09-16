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
# With `--modes "1920x1080@60 1920x1080@120 720p240"` the matrix runs once
# per arm, writing `output.<connector>.mode` (or `.modeline`, for the
# modeline arm) into the human's config and restarting the unit between
# arms. Three rules, all non-negotiable:
#
#   * The human's `server.conf` is **backed up and restored**, never
#     rewritten from scratch: it carries his dark scheme and his `de`
#     layout and those are his, not ours. And what he wants left behind
#     is `mode = 1920x1080@120` — so the restore is a file copy of what
#     was found, not a reconstruction, and it runs on every exit path
#     including a crash and a Ctrl-C.
#   * After each restart the script **confirms `outputs` reports the size
#     and the rate it asked for** — the size because the 720p arm changes
#     both and a 1080p fallback at 240 would be a *plausible* wrong
#     answer; the rate to within half a hertz. Otherwise the 120 Hz
#     column is a measurement of a switch that never happened — the #3704
#     rule this room learned the hard way. A mode line that matches
#     nothing is a *warning plus the default*, so the server comes back
#     happily at 60 and a fallen-back arm is indistinguishable from
#     "120 Hz bought nothing".
#
#     The tolerance is not slack: a real mode's refresh is almost never
#     the round number. This panel's "120" is clock 285 500 over
#     2080x1144 = **119.982 Hz**, reported as `@119982`, and its 85 is
#     84.904; the 720p modeline's "240" is **239.840**, because CVT-RB
#     rounds the pixel clock down to a 0.25 MHz step. An equality check
#     against `hz * 1000` would skip every arm for ever — which is
#     exactly the quiet failure the guard exists to catch, and is what
#     the first version of it did.
#   * The 720p@240 arm is **short and announced**. It changes what the
#     human sees on his own screen, so it runs a reduced matrix (§ the
#     `matrix_720p240` function) and hands the panel back within ten
#     minutes. The modeline is removed as soon as the arm ends, not at
#     the end of the script.
#
# # Naming an arm
#
# An arm is either `WxH@Hz` (a mode the connector lists) or a nickname
# for a modeline this script knows: `720p240`. A bare `60` still means
# `1920x1080@60`, so the old `--modes "60 120"` spelling keeps working —
# `docs/bench.md` §10 and the justfile both use it.
#
# Usage, on the box:
#   deploy/bench.sh [--seconds N] [--out FILE]
#                   [--modes "1920x1080@60 1920x1080@120 720p240"]
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
    -h | --help) sed -n '2,72p' "$0"; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done

: "${XDG_RUNTIME_DIR:=/run/user/$(id -u)}"
export XDG_RUNTIME_DIR

say() { printf '### %s\n' "$*" >&2; }

# The sha the ledger stamps on every row.
#
# **Taken from the caller, not from the box's checkout**, and that
# distinction cost this task a first run. `git -C ~/src/ai/nitro
# rev-parse HEAD` reads the box's *clone*, which is whatever `just
# box-push` last put there — on this box, `329faf3`, seventeen commits
# behind the `c25170d` binaries actually in `~/nitro-bin`. Every record
# of that run carried a sha naming a tree that did not build them: the
# `md5sum`-the-binary rule (`docs/testbox.md`) one level up, where the
# instrument agrees with itself and names the wrong source.
#
# `just bench` exports `NITRO_BENCH_SHA` from the dev machine, which is
# the tree that ran `cargo build --release`. The clone is the fallback
# and says so, because a wrong sha is worse than `unknown`: `unknown`
# sends a reader to ask, and a plausible sha sends them to read the
# wrong diff.
sha="${NITRO_BENCH_SHA:-}"
if [[ -z $sha ]]; then
    sha="$(git -C "$HOME/src/ai/nitro" rev-parse --short HEAD 2>/dev/null || echo unknown)-box-checkout"
    say "WARNING: no NITRO_BENCH_SHA from the caller; stamping rows '$sha'"
    say "  (that is the box's clone, which is not necessarily what built ~/nitro-bin)"
fi
export NITRO_BENCH_SHA="$sha"
export NITRO_BENCH_HOST="$(hostname)"

# The fingerprint of what is actually being measured, taken at the start
# and re-checked at the end.
#
# **This is the check that this task learned the hard way, and it is the
# only one that could have caught what happened.** A 141-run sweep was
# taken here against binaries that a *different task deployed six minutes
# after this script's caller rsynced its own* — `just deploy` from
# another branch, landing between the deploy and the first arm. Every
# other instrument agreed: `outputs` reported the right mode, the
# refresh came back from two independent sources, the ledger's `sha`
# column named the caller's tree, and the numbers were plausible. The
# run measured someone else's build for forty minutes and said so
# nowhere.
#
# An md5 taken *once, before the run* does not catch it either — that is
# the shape #3718 used, and it answers "is this mine now". The question
# a long run has to answer is "was it mine **throughout**", which needs
# the pair. So the fingerprint is recorded in the ledger as a comment at
# both ends, and a mismatch is a loud failure rather than a footnote: a
# ledger whose binaries changed under it is not a slightly-flawed
# measurement, it is two measurements interleaved and unlabelled.
fingerprint() {
    md5sum "$HOME"/nitro-bin/nitro-server "$HOME"/nitro-bin/nitro-bench 2>/dev/null |
        awk '{print $1}' | tr '\n' ' '
}
BINS_BEFORE="$(fingerprint)"

# Re-check the binaries and say so in the ledger, whatever the answer.
check_bins() {
    local now
    now="$(fingerprint)"
    if [[ $now == "$BINS_BEFORE" ]]; then
        printf '# binaries unchanged across the whole run: %s\n' "$now" >>"$out"
        say "binaries unchanged across the run ($now)"
        return 0
    fi
    printf '# INVALID: the binaries changed DURING this run: %s -> %s\n' \
        "$BINS_BEFORE" "$now" >>"$out"
    say "*** INVALID RUN: nitro-server/nitro-bench changed under it"
    say "***   before: $BINS_BEFORE"
    say "***   after:  $now"
    say "*** Someone deployed to this shared box mid-sweep. The ledger is"
    say "*** two builds interleaved and cannot be read as one measurement."
    return 1
}

if [[ -z $out ]]; then
    mkdir -p "$HOME/tmp/bench"
    out="$HOME/tmp/bench/$sha.jsonl"
fi
mkdir -p "$(dirname "$out")"

conf="$HOME/.config/nitro/server.conf"
backup="$(mktemp)"
restored=0
# The mode `outputs` reported for the arm now running: the rate in
# millihertz and the size as `WxH`. Set by `set_mode`, read by the matrix
# note — a column labelled with the *requested* rate would be the request
# marking its own homework, and on this panel "120" is really 119 982 and
# "240" is 239 840.
MEASURED_MHZ=""
MEASURED_SIZE=""

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

# The one modeline this script knows, by nickname.
#
# CVT-RB 1280x720@240 = 279.75 MHz, inside HDMI 1.4's ~300 MHz TMDS
# ceiling where 1080p@240's 606.5 MHz is not. #3718 set it, the kernel
# took it, `outputs` reported `1280x720@239840 (custom)` — and the human
# looked at the panel and said "720p 240hz!", which is the only
# instrument that can answer that question (`just shot` reads the shadow
# buffer and returns a perfect frame at a dark panel).
#
# CVT rounds the pixel clock down to a 0.25 MHz step, so the arm asks for
# 240 and the hardware gives **239.840** — which the 0.5 Hz tolerance in
# `set_mode` absorbs, and which is why that tolerance is a rule rather
# than slack.
MODELINE_720P240="279750 1280 1328 1360 1440 720 723 727 810 +hsync -vsync"

# Expand an arm name into `WIDTH HEIGHT MHZ KIND VALUE`.
#
# `KIND` is `mode` or `modeline`, `VALUE` the text that goes after the
# `=`. A bare number is 1080p at that rate, so `--modes "60 120"` — the
# spelling in the justfile and in `docs/bench.md` §10 — keeps working.
arm_spec() {
    case "$1" in
    720p240)
        printf '1280 720 240000 modeline %s\n' "$MODELINE_720P240"
        ;;
    *x*@*)
        local wh="${1%@*}" hz="${1#*@}"
        printf '%s %s %s mode %s\n' "${wh%x*}" "${wh#*x}" "$((hz * 1000))" "$1"
        ;;
    '' | *[!0-9]*)
        return 1
        ;;
    *)
        printf '1920 1080 %s mode 1920x1080@%s\n' "$(($1 * 1000))" "$1"
        ;;
    esac
}

# Drop this connector's mode and modeline lines from the human's file.
#
# Called at the end of the 720p arm rather than only at exit: that arm
# changes what is on his screen, and "ten minutes" is a promise made in
# the `nitro-testbox` room. `restore_conf` still puts the *original* file
# back on every exit path — this only stops one arm's line leaking into
# the next.
clear_mode() {
    local tmp
    tmp="$(mktemp)"
    grep -vE "^output\.$connector\.mode(line)? " "$conf" 2>/dev/null >"$tmp" || true
    install -m 644 "$tmp" "$conf"
    rm -f "$tmp"
}

# Set the output's mode and prove it took. Returns non-zero when the
# server did not come back at the size *and* rate asked for, so the
# caller can skip that arm rather than record a mislabelled column.
set_mode() {
    local arm="$1" spec
    if ! spec="$(arm_spec "$arm")"; then
        say "REFUSING the $arm arm: not a WxH@Hz mode or a known modeline nickname"
        return 1
    fi
    local want_w want_h want_mhz kind value
    read -r want_w want_h want_mhz kind value <<<"$spec"
    say "switching $connector to $arm (${want_w}x${want_h} @ ${want_mhz} mHz, via $kind)"
    mkdir -p "$(dirname "$conf")"
    # Rewrite only the mode/modeline lines; everything else in the
    # human's file stays exactly as it was. Both keys are dropped first,
    # because a leftover `mode` beside a new `modeline` is two answers to
    # one question.
    clear_mode
    printf 'output.%s.%s = %s\n' "$connector" "$kind" "$value" >>"$conf"
    sudo systemctl restart nitro-dev
    # Wait for the socket, then for the rate. Both bounded: a box that
    # did not come back is a failure to report, not a hang.
    local i
    for i in $(seq 1 60); do
        [[ -S $XDG_RUNTIME_DIR/nitro/control.sock ]] && break
        sleep 1
    done
    sleep 3
    # Confirm the mode, and **parse it rather than matching a literal**.
    #
    # A real mode's refresh is almost never the round number you asked
    # for. The box's "120 Hz" mode is clock 285 500 over 2080x1144, i.e.
    # **119.982 Hz**, and `outputs` reports `@119982`; its 85 is 84.904;
    # the 720p modeline's 240 is 239.840. The first version of this check
    # compared against `hz * 1000` and so would have skipped the 120 Hz
    # arm *for ever* — silently, with a tidy `# SKIPPED` line in the
    # ledger — which is precisely the quiet failure this guard exists to
    # catch, committed inside the guard itself. (#3718, who owns the key,
    # caught it.)
    #
    # The config line still asks for the round number: matching is
    # nearest-within-0.5 Hz exactly so a person writes `@120`. So the
    # check is "did we land within half a hertz of what we asked for",
    # which is the same rule the server applies, rather than string
    # equality against a number no connector actually offers.
    #
    # **The size is checked too**, and that is new with the 720p arm. A
    # rejected modeline is a warning plus the *default* mode, which here
    # is 1920x1080@60 — so a size-blind guard that asked for 720p@240 and
    # got 1080p@60 would refuse it for the right reason by luck, while an
    # arm asking for 1080p@60 with a typo'd modeline would pass the rate
    # check and silently measure the wrong screen. Size and rate are one
    # question here, so they are one check.
    local line
    # Anchored on **this connector's line**, not on the first `@` in the
    # whole reply: `outputs` prints one line per output, so a second
    # monitor (or a `(custom)` modeline row) would otherwise silently
    # supply the number this guard then blesses. Reviewer's catch, and it
    # is the same failure mode as the literal below — a guard reading the
    # wrong field is worse than no guard, because it reports success.
    line="$(control outputs | grep -E "^${connector} " | head -1)"
    local got_w got_h got
    got_w="$(sed -nE 's/^[^ ]+ ([0-9]+)x([0-9]+)@([0-9]+).*/\1/p' <<<"$line")"
    got_h="$(sed -nE 's/^[^ ]+ ([0-9]+)x([0-9]+)@([0-9]+).*/\2/p' <<<"$line")"
    got="$(sed -nE 's/^[^ ]+ ([0-9]+)x([0-9]+)@([0-9]+).*/\3/p' <<<"$line")"
    if [[ -z ${got:-} ]]; then
        say "REFUSING the $arm arm: outputs has no WxH@Hz for $connector"
        control outputs | sed 's/^/    /' >&2
        return 1
    fi
    if [[ $got_w != "$want_w" || $got_h != "$want_h" ]]; then
        say "REFUSING the $arm arm: outputs reports ${got_w}x${got_h}, asked ${want_w}x${want_h}"
        say "  (a mode line that matches nothing is a warning plus the default)"
        return 1
    fi
    local delta=$((got - want_mhz))
    ((delta < 0)) && delta=$((-delta))
    if ((delta > 500)); then
        say "REFUSING the $arm arm: outputs reports @${got}, ${delta} mHz from @${want_mhz}"
        say "  (\`nitro-shot --modes\` lists what this connector really offers)"
        return 1
    fi
    say "confirmed: outputs reports ${got_w}x${got_h}@${got}, within ${delta} mHz of $arm"
    # Hand the measured mode back so the ledger's note carries the truth
    # rather than the request.
    MEASURED_MHZ="$got"
    MEASURED_SIZE="${got_w}x${got_h}"
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
    #
    # Every pair runs at **both** sizes, because the spec asked for the
    # native resolution and the period-correct 640x480 and the two answer
    # different questions: fullscreen is where the pixel arm hits the
    # bandwidth wall, and VGA is where it does not, so a node arm that
    # wins at both wins for a reason other than the wall.
    bench boing --note "$tag; 640x480, pixel arm of the pair"
    bench boing-node --note "$tag; 640x480, node arm of the pair"
    bench boing --fullscreen --note "$tag; pixel arm of the pair"
    bench boing-node --fullscreen --note "$tag; node arm of the pair"
    local sn
    for sn in 100 500 2000; do
        ((quick)) && [[ $sn == 2000 ]] && continue
        bench starfield --n "$sn" --note "$tag; 640x480, pixel arm"
        bench starfield-nodes --n "$sn" --note "$tag; 640x480, node arm"
        bench starfield --n "$sn" --fullscreen --note "$tag; pixel arm"
        bench starfield-nodes --n "$sn" --fullscreen --note "$tag; node arm"
    done
    bench balls --n 32 --note "$tag; 640x480, pixel arm"
    bench balls-nodes --n 32 --note "$tag; 640x480, node arm"
    bench balls --n 32 --fullscreen --note "$tag; pixel arm"
    bench balls-nodes --n 32 --fullscreen --note "$tag; node arm"
}

# The **reduced** matrix, for the 720p@240 arm.
#
# Reduced because that arm is on the human's own screen: it drops him to
# 1280x720 and he uses the box interactively, so the promise made in the
# `nitro-testbox` room is ten minutes. The full matrix is 54 runs and
# would be forty.
#
# What survives the cut is chosen by which §9 question a row answers, not
# by what is cheap:
#
#   * `rects` at 100/500/2000 — "how many mutations fit in 4.2 ms", and
#     the sweep's three informative points (n=10 was never near a limit,
#     and n=1000 sits between two points that already bracket it).
#   * `text`/`text-static` at n=100 — the reshape ratio, one point,
#     because the ratio is what carries across rates, not its N curve.
#   * `putimage` at 100/500/720 — 720 and not 1080: the fullscreen case
#     expressed as a square is the *screen's* short edge, so the sweep
#     and the fullscreen rows keep sharing a denominator at this size.
#   * `scroll`, the boing pair, the starfield pairs at 100/500/2000 and
#     the three effects — the crossover (§7.8) and the fixed-cost
#     question (#568) are the two measurements this arm exists for.
#
# Every fullscreen row here is **1280x720**, which is what "fullscreen"
# means at this mode; the rows label themselves with their geometry, so
# a table cannot confuse it with the 1080p arms' 1920x1080.
matrix_720p240() {
    local tag="$1"

    local n
    for n in 100 500 2000; do
        bench rects --n "$n" --note "$tag; x11perf -rect100 recolour"
    done
    bench text --n 100 --note "$tag; x11perf -ftext"
    bench text-static --n 100 --note "$tag; the retained arm"

    local s
    for s in 100 500 720; do
        bench putimage --size "$s" --note "$tag; x11perf -putimage$s"
    done
    bench scroll --n 500 --note "$tag; x11perf -scroll500"

    local e
    for e in plasma fire rotozoom; do
        bench "$e" --note "$tag; 640x480, period-correct"
        bench "$e" --fullscreen --note "$tag; fullscreen (1280x720 here)"
    done

    bench boing --note "$tag; 640x480, pixel arm of the pair"
    bench boing-node --note "$tag; 640x480, node arm of the pair"
    bench boing --fullscreen --note "$tag; pixel arm of the pair"
    bench boing-node --fullscreen --note "$tag; node arm of the pair"
    local sn
    for sn in 100 500 2000; do
        bench starfield --n "$sn" --note "$tag; 640x480, pixel arm"
        bench starfield-nodes --n "$sn" --note "$tag; 640x480, node arm"
        bench starfield --n "$sn" --fullscreen --note "$tag; pixel arm"
        bench starfield-nodes --n "$sn" --fullscreen --note "$tag; node arm"
    done
}

# The control point: one `rects n=500` with the shell clients down.
#
# Every other row runs with the bar and the launcher up, which is the
# realistic baseline and is stated as such — but a reader is entitled to
# know what they cost, and the only way to say is to take them away for
# one point. Taken **once per rate**, because what the shell costs is a
# per-frame figure and the whole question of this sweep is which
# per-frame figures move with the rate.
#
# The shell clients are killed, **not** the unit: stopping `nitro-dev`
# takes the server down with them and there would be nothing left to
# benchmark. `nitro-session` supervises the four and repairs a kill within
# about a second, so the control point is taken in the window a
# `systemctl kill`-free pkill opens — which is why it is one short run and
# why the note says so.
control_arm() {
    local tag="$1"
    say "control: shell clients down ($tag)"
    pkill -f nitro-wallpaper || true
    pkill -f nitro-bar || true
    pkill -f nitro-launcher || true
    local before_procs
    before_procs="$(pgrep -c nitro || echo 0)"
    bench rects --n 500 \
        --seconds 3 \
        --note "control: shell clients killed; $tag ($before_procs procs at start; the session restarts them within ~1 s, so this arm is 3 s)"
}

say "sha $sha, writing $out"
printf '# nitro-bench matrix, sha %s, host %s, %s\n' \
    "$sha" "$NITRO_BENCH_HOST" "$(date -Is)" >>"$out"
printf '# binaries at start (nitro-server nitro-bench): %s\n' "$BINS_BEFORE" >>"$out"
say "binaries under test: $BINS_BEFORE"

"$bin" bandwidth --json >>"$out"

if [[ -z $modes ]]; then
    matrix "bar+launcher up"
    control_arm "1 arm, no mode set"
else
    for arm in $modes; do
        MEASURED_MHZ=""
        MEASURED_SIZE=""
        if set_mode "$arm"; then
            # The note carries the mode the server **reported**, not the
            # one asked for: 120 is really 119.982 on this panel and 240
            # is 239.840, and a column labelled with the request would be
            # the request marking its own homework. `refresh_mhz` in each
            # record is the client's own reading of the frame callback, so
            # the two are independent and a reader can check them against
            # each other.
            tag="bar+launcher up; asked $arm, outputs says ${MEASURED_SIZE}@${MEASURED_MHZ}"
            # The 720p arm is the reduced one, and it is reduced for the
            # human's sake rather than the machine's — see
            # `matrix_720p240`. The test is on the size `outputs`
            # reported, not on the arm's name: a nickname is a request and
            # the reported size is the fact.
            if [[ $MEASURED_SIZE == 1280x720 ]]; then
                say "ENTERING the 720p@240 arm — the human's screen changes now"
                matrix_720p240 "$tag"
                control_arm "$tag"
                say "LEAVING the 720p@240 arm — restoring the human's mode"
                clear_mode
            else
                matrix "$tag"
                control_arm "$tag"
            fi
        else
            printf '# SKIPPED the %s arm: outputs did not confirm the mode: %s\n' \
                "$arm" "$(control outputs | tr '\n' ' ')" >>"$out"
        fi
    done
fi

# The control arm ran inside each rate arm above, because what the shell
# costs per frame is exactly the kind of figure this sweep asks about.
# `NITRO_BENCH_CONTROL` used to be set here for a session-wide restart;
# it is not needed now that the control is a pkill inside a live arm, and
# a stale drop-in on a shared box is debris.

restore_conf
sudo systemctl restart nitro-dev || true
say "done: $out"
printf 'wrote %s (%s runs)\n' "$out" "$(grep -c '^{' "$out" || echo 0)"
# Last, and load-bearing: did anyone deploy over this run while it ran?
# A non-zero exit here is the script saying the ledger it just wrote
# cannot be read as one measurement.
check_bins
