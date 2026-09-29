#!/usr/bin/env bash
# Chromium-on-nitro benchmark, run ON a box (#3919). `just chromium-bench`
# pipes it over ssh together with scroll-bench.py.
#
#   chromium-bench.sh MODE [RUNS]
#
# MODE:
#   inproc    --disable-gpu --in-process-gpu   (the #3778/#3877 baseline)
#   oop       --disable-gpu                    (GPU process presents via
#                                               ExportSurface/ImportSurface)
#   gpu       NITRO_GPU_READBACK=1 --use-angle=vulkan
#             --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE
#             (GPU raster, glReadPixels into the shm canvas; measurement only)
#
# For each run: starts chrome (a fresh profile under /tmp) on a 1500-row
# page, waits for it to settle, runs scroll-bench.py over the window
# (i2p, fps, server paint), samples the chrome tree's CPU over the scroll,
# and prints PSS/RSS of the browser and GPU processes, idle, before the
# scroll. Then closes chrome. Needs the session's control socket.
set -euo pipefail
mode=${1:?mode: inproc|oop|gpu}
runs=${2:-2}
here=$(cd "$(dirname "$0")" && pwd)
chrome=${NITRO_CHROME:-$HOME/nitro-bin/chromium/chrome}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
[[ -S $XDG_RUNTIME_DIR/nitro/wire.sock ]] || { echo "no nitro session" >&2; exit 1; }

page=/tmp/nitro-chromium-bench.html
{
    echo '<html><body style="margin:0;font:16px sans-serif">'
    for i in $(seq 0 1499); do echo "<div style=\"padding:2px 8px;background:hsl($((i*7%360)),60%,90%)\">row $i lorem ipsum dolor sit amet, consectetur adipiscing elit</div>"; done
    echo '</body></html>'
} > "$page"

flags=(--ozone-platform=nitro --enable-logging=stderr --no-first-run --no-default-browser-check
       --disable-features=Translate --password-store=basic)
envs=()
case $mode in
    inproc) flags+=(--disable-gpu --in-process-gpu) ;;
    oop) flags+=(--disable-gpu) ;;
    gpu) flags+=(--use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE)
         envs+=(NITRO_GPU_READBACK=1) ;;
    *) echo "unknown mode $mode" >&2; exit 2 ;;
esac

# kB of a field in /proc/PID/smaps_rollup (PSS) or status (VmRSS).
pss() { awk '/^Pss:/{print $2}' "/proc/$1/smaps_rollup" 2>/dev/null || echo 0; }
rss() { awk '/^VmRSS:/{print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }
# utime+stime jiffies of every process whose cmdline starts with $chrome.
tree_jiffies() {
    local t=0 p
    for p in $(pgrep -f "^$chrome"); do
        t=$((t + $(awk '{print $14+$15}' "/proc/$p/stat" 2>/dev/null || echo 0)))
    done
    echo $t
}

for run in $(seq 1 "$runs"); do
    prof=$(mktemp -d /tmp/nitro-chromium-bench.XXXXXX)
    env "${envs[@]}" "$chrome" "${flags[@]}" --user-data-dir="$prof" \
        "file://$page" > "$prof/log" 2>&1 &
    pid=$!
    sleep 12
    gpu=$(pgrep -f -- "^$chrome.*--type=gpu-process" | head -1 || true)
    tree_pss=0
    for p in $(pgrep -f "^$chrome"); do tree_pss=$((tree_pss + $(pss "$p"))); done
    b_pss=$(pss $pid); b_rss=$(rss $pid)
    g_pss=0; g_rss=0
    [[ -n $gpu ]] && { g_pss=$(pss "$gpu"); g_rss=$(rss "$gpu"); }
    j0=$(tree_jiffies); t0=$(date +%s.%N)
    row=$(python3 "$here/scroll-bench.py" --label "$mode #$run" 2>&1 | tail -3)
    j1=$(tree_jiffies); t1=$(date +%s.%N)
    cpu=$(python3 -c "print(round(($j1-$j0)/$(getconf CLK_TCK)/($t1-$t0)*100))")
    echo "$row"
    echo "  mem idle: browser PSS $((b_pss/1024)) MB (RSS $((b_rss/1024))), gpu PSS $((g_pss/1024)) MB (RSS $((g_rss/1024))), tree PSS $((tree_pss/1024)) MB; chrome CPU over scroll ${cpu}% of one core"
    grep -m4 -E "NITRO_GPU|the server lacks|FATAL|GPU process has crashed|GL_RENDERER|ANGLE" "$prof/log" | cut -c1-200 | sed 's/^/  log: /' || true
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    # The browser's children may still be writing the profile.
    for _ in 1 2 3 4 5; do pgrep -f -- "--user-data-dir=$prof" >/dev/null || break; sleep 1; done
    pkill -9 -f -- "--user-data-dir=$prof" 2>/dev/null || true
    rm -rf "$prof" 2>/dev/null || true
    sleep 2
done
