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
#   dmabuf    --use-angle=${NITRO_BENCH_ANGLE:-vulkan} (+ the Vulkan features
#             when vulkan): GPU raster into GBM dma-bufs presented with
#             explicit fences (#3921). NITRO_BENCH_ANGLE=gl for ANGLE-GL
#             (box1's hasvk if VulkanFromANGLE import fails)
#   shm       no flag, NITRO_NO_DMABUF=1: the fallback the dmabuf arm
#             replaces, with GPU on (A/B)
#   video     the dmabuf flags on a page playing a 1080p H.264 clip
#             (<video autoplay loop muted>, 1280x720 CSS; NITRO_BENCH_VIDEO_FS=1
#             stretches it over a --start-fullscreen window) for 20 s, video
#             overlays on (#3944); NITRO_NO_OVERLAYS=1 in the environment is
#             the composited arm. Samples chrome-tree and nitro-server CPU and
#             the server's planes/paint/dma-buf counters over the window, and
#             says whether VA-API decoded (a libva driver mapped in the GPU
#             process). The clip is made once with ffmpeg (testsrc2, libx264)
#             and cached in ~/nitro-stage. NITRO_BENCH_FPS picks 30 or 60
#             (default 60), NITRO_BENCH_RES 1080 or 720 (default 1080; a
#             720p clip in the 1280x720 CSS box is upscaled, which a plane
#             does, where 1080p in it is downscaled, which it does not).
#             NITRO_BENCH_VIDEO_CSS=WxH changes the CSS box (1024x576 is
#             1280x720 device px at scale 1.25: a 720p clip at 1:1).
#
# For each run: starts chrome (a fresh profile under /tmp) on a 1500-row
# page, waits for it to settle, runs scroll-bench.py over the window
# (i2p, fps, server paint), samples the chrome tree's CPU over the scroll,
# and prints PSS/RSS of the browser and GPU processes, idle, before the
# scroll. Then closes chrome. Needs the session's control socket.
set -euo pipefail
mode=${1:?mode: inproc|oop|gpu|dmabuf|shm|video}
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
    dmabuf) angle=${NITRO_BENCH_ANGLE:-vulkan}
            flags+=(--use-angle="$angle")
            [[ $angle == vulkan ]] && flags+=(--enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE) ;;
    shm) envs+=(NITRO_NO_DMABUF=1) ;;
    video) flags+=(--use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE,AcceleratedVideoDecodeLinuxGL,AcceleratedVideoDecodeLinuxZeroCopyGL
                   --autoplay-policy=no-user-gesture-required)
           [[ ${NITRO_BENCH_VIDEO_FS:-0} == 1 ]] && flags+=(--start-fullscreen)
           [[ ${NITRO_NO_OVERLAYS:-0} == 1 ]] && envs+=(NITRO_NO_OVERLAYS=1) ;;
    *) echo "unknown mode $mode" >&2; exit 2 ;;
esac
# `env` with no assignments would run nothing extra; keep the array non-empty.
envs+=(NITRO_BENCH=1)

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

# One server stat, through scroll-bench.py's control-socket client.
stat() { python3 -c "import importlib.machinery as m; b=m.SourceFileLoader('sb','$here/scroll-bench.py').load_module(); print(b.stats().get('$1','-'))" 2>/dev/null || echo -; }

if [[ $mode == video ]]; then
    fps=${NITRO_BENCH_FPS:-60}
    res=${NITRO_BENCH_RES:-1080}
    case $res in 1080) dim=1920x1080 ;; 720) dim=1280x720 ;; *) echo "NITRO_BENCH_RES is 1080 or 720" >&2; exit 2 ;; esac
    clip=$HOME/nitro-stage/nitro-bench-${res}p${fps}.mp4
    if [[ ! -s $clip ]]; then
        mkdir -p "$(dirname "$clip")"
        ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=size=$dim:rate=$fps" \
            -t 30 -c:v libx264 -profile:v high -pix_fmt yuv420p -preset veryfast -b:v 8M "$clip"
    fi
    vpage=/tmp/nitro-chromium-bench-video.html
    if [[ ${NITRO_BENCH_VIDEO_FS:-0} == 1 ]]; then
        style='position:fixed;left:0;top:0;width:100vw;height:100vh;object-fit:contain;background:#000'
    else
        css=${NITRO_BENCH_VIDEO_CSS:-1280x720}
        style="width:${css%x*}px;height:${css#*x}px"
    fi
    cat > "$vpage" <<HTML
<html><body style="margin:0;background:#fff;font:16px sans-serif">
<video src="file://$clip" autoplay loop muted style="$style"></video>
<p>nitro #3944 video overlay bench</p></body></html>
HTML
    server=$(pgrep -x nitro-server | head -1)
    counters=(planes_mode planes_in_use planes_obscured plane_flips frames gpu_frames dmabuf_buffers dmabuf_kms_imported dmabuf_placeholder_paints planes_fallbacks buffers damage_px_mean)
    for run in $(seq 1 "$runs"); do
        prof=$(mktemp -d /tmp/nitro-chromium-bench.XXXXXX)
        env "${envs[@]}" "$chrome" "${flags[@]}" --user-data-dir="$prof" \
            --vmodule='*vaapi*=1,*nitro*=2' "file://$vpage" > "$prof/log" 2>&1 &
        pid=$!
        sleep 10
        gpu=$(pgrep -f -- "^$chrome.*--type=gpu-process" | head -1 || true)
        declare -A c0=()
        for k in "${counters[@]}"; do c0[$k]=$(stat "$k"); done
        j0=$(tree_jiffies); s0=$(awk '{print $14+$15}' "/proc/$server/stat"); t0=$(date +%s.%N)
        sleep 20
        j1=$(tree_jiffies); s1=$(awk '{print $14+$15}' "/proc/$server/stat"); t1=$(date +%s.%N)
        pct() { python3 -c "print(round(($2-$1)/$(getconf CLK_TCK)/($t1-$t0)*100,1))"; }
        line="video ${res}p$fps fs=${NITRO_BENCH_VIDEO_FS:-0} overlays=$([[ ${NITRO_NO_OVERLAYS:-0} == 1 ]] && echo off || echo on) #$run: chrome CPU $(pct "$j0" "$j1")% server CPU $(pct "$s0" "$s1")% (of one core)"
        for k in "${counters[@]}"; do
            v=$(stat "$k")
            case $k in
                planes_mode|planes_in_use|dmabuf_buffers|dmabuf_kms_imported|buffers|damage_px_mean) line+=" $k=$v" ;;
                *) line+=" $k+=$(( ${v/-/0} - ${c0[$k]/-/0} ))" ;;
            esac
        done
        echo "$line"
        va=none
        [[ -n $gpu ]] && va=$(grep -oE '(iHD|i965)_drv_video\.so' "/proc/$gpu/maps" | sort -u | tr '\n' ' ')
        echo "  libva driver mapped in the GPU process: ${va:-none}"
        grep -m8 -E "VaapiVideoDecoder|vaapi_wrapper|nitro: (widget .* overlay|overlay)|FATAL|GPU process has crashed|Limit" "$prof/log" | cut -c1-200 | sed 's/^/  log: /' || true
        kill $pid 2>/dev/null || true
        wait $pid 2>/dev/null || true
        for _ in 1 2 3 4 5; do pgrep -f -- "--user-data-dir=$prof" >/dev/null || break; sleep 1; done
        pkill -9 -f -- "--user-data-dir=$prof" 2>/dev/null || true
        [[ -n ${NITRO_BENCH_KEEP_LOG:-} ]] && cp "$prof/log" "$NITRO_BENCH_KEEP_LOG.$run"
        rm -rf "$prof" 2>/dev/null || true
        sleep 2
    done
    exit 0
fi

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
    s0="$(stat dmabuf_buffers) $(stat fence_waits) $(stat dmabuf_placeholder_paints)"
    srv=$(pgrep -x nitro-server | head -1 || true)
    sj() { [[ -n $srv ]] && awk '{print $14+$15}' "/proc/$srv/stat" 2>/dev/null || echo 0; }
    j0=$(tree_jiffies); s_j0=$(sj); t0=$(date +%s.%N)
    row=$(python3 "$here/scroll-bench.py" --label "$mode #$run" 2>&1 | tail -3)
    j1=$(tree_jiffies); s_j1=$(sj); t1=$(date +%s.%N)
    cpu=$(python3 -c "print(round(($j1-$j0)/$(getconf CLK_TCK)/($t1-$t0)*100))")
    scpu=$(python3 -c "print(round(($s_j1-$s_j0)/$(getconf CLK_TCK)/($t1-$t0)*100))")
    echo "$row"
    echo "  server: dmabuf_buffers/fence_waits/placeholder_paints before $s0, after $(stat dmabuf_buffers) $(stat fence_waits) $(stat dmabuf_placeholder_paints); planes_mode $(stat planes_mode), gpu_frames $(stat gpu_frames), gpu_translucent_approx $(stat gpu_translucent_approx)"
    echo "  mem idle: browser PSS $((b_pss/1024)) MB (RSS $((b_rss/1024))), gpu PSS $((g_pss/1024)) MB (RSS $((g_rss/1024))), tree PSS $((tree_pss/1024)) MB; chrome CPU over scroll ${cpu}% of one core, nitro-server ${scpu}%"
    grep -m6 -E "NITRO_GPU|nitro: GBM|dma-buf|GPU raster|the server lacks|FATAL|GPU process has crashed|GL_RENDERER|ANGLE" "$prof/log" | cut -c1-200 | sed 's/^/  log: /' || true
    kill $pid 2>/dev/null || true
    wait $pid 2>/dev/null || true
    # The browser's children may still be writing the profile.
    for _ in 1 2 3 4 5; do pgrep -f -- "--user-data-dir=$prof" >/dev/null || break; sleep 1; done
    pkill -9 -f -- "--user-data-dir=$prof" 2>/dev/null || true
    rm -rf "$prof" 2>/dev/null || true
    sleep 2
done
