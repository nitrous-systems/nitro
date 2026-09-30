#!/usr/bin/env bash
# Chromium full-screen sharing on nitro, end to end, run ON a box (#676 D).
# `just chromium-share` copies it over ssh and runs it inside the
# logged-in session. Needs `deploy-chromium`, a server with caps::CAPTURE
# that grants this client (NITRO_CAPTURE_ALLOW=1 in its environment, or
# the capture permission of #676 C), and the session's control socket.
#
#   chromium-share.sh [SECONDS] [FPS]
#
# Load: nitro-video --synthetic (a 720p30 clip, VA-API, on a plane when the
# box has one) and the share page itself in a GPU (ANGLE-Vulkan) Chromium
# window with a spinning canvas. The page calls getDisplayMedia (the picker
# auto-selects the screen, --auto-select-screen-capture-source), sends the
# track through an RTCPeerConnection loopback, draws what it receives to a
# canvas and reports: received fps (requestVideoFrameCallback), the size,
# and whether the picture has content (luma spread over a 16x16 grid of
# samples; a grey/black/blank picture fails).
#
# Measured over SECONDS (default 20) three times: sharing off, the server
# recording to nitro-shot --record (no browser consumer), and sharing on:
# display fps (server `frames`), server/helper/Chromium CPU, the server's
# capture counters, dma-buf objects (/sys/kernel/debug/dma_buf/bufinfo,
# via sudo) while recording and after the browser closed. Prints PASS/FAIL
# for "received stream has content", "frames keep arriving" and "display
# fps did not drop while recording" (off vs nitro-shot: with Chromium as
# the consumer its VP8 encode competes with the page for CPU, so the
# page's own frame rate, and with it the display's, can drop).
set -uo pipefail
secs=${1:-20}
fps=${2:-30}
chrome=${NITRO_CHROME:-$HOME/nitro-bin/chromium/chrome}
video=${NITRO_VIDEO:-$(command -v nitro-video || echo "$HOME/nitro-bin/nitro-video")}
here=$(cd "$(dirname "$0")" && pwd)
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
[[ -S $XDG_RUNTIME_DIR/nitro/wire.sock ]] || { echo "no nitro session" >&2; exit 1; }
work=$(mktemp -d /tmp/nitro-share.XXXXXX)
port=9344
fails=0
pids=()
cleanup() {
    for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
    sleep 1
    pkill -9 -f -- "--user-data-dir=$work/prof" 2>/dev/null
    [[ -n ${KEEP:-} ]] && echo "kept $work" || rm -rf "$work"
}
trap cleanup EXIT

stat() { python3 -c "import importlib.machinery as m; b=m.SourceFileLoader('sb','$here/scroll-bench.py').load_module(); print(b.stats().get('$1','-'))" 2>/dev/null || echo -; }
jiffies() { local t=0 p; for p in "$@"; do t=$((t + $(awk '{print $14+$15}' "/proc/$p/stat" 2>/dev/null || echo 0))); done; echo $t; }
chrome_pids() { pgrep -f -- "--user-data-dir=$work/prof" | tr '\n' ' '; }
bufinfo() { # objects bytes
    sudo -n cat /sys/kernel/debug/dma_buf/bufinfo 2>/dev/null | awk '/^Total .* objects/ {print $2 " objects " $4 " bytes"; f=1} END {if (!f) print "- (no sudo)"}'
}
check() { if [[ $2 == 1 ]]; then echo "PASS $1  $3"; else echo "FAIL $1  $3"; fails=$((fails+1)); fi; }
title() { curl -s "http://127.0.0.1:$port/json" | python3 -c '
import json, sys
for t in json.load(sys.stdin):
    if t.get("type") == "page" and ("share.html" in t.get("url", "") or "spin.html" in t.get("url", "")):
        print(t["title"]); break' 2>/dev/null; }

cat > "$work/share.html" <<EOF
<html><head><title>share|idle</title></head>
<body style="margin:0;background:#fff;font:16px sans-serif">
<canvas id=spin width=300 height=300 style="float:right"></canvas>
<p>nitro #676 share test — <span id=st>idle</span></p>
<canvas id=out width=640 height=360 style="border:1px solid #888"></canvas>
<video id=rx autoplay muted playsinline width=320 height=180></video>
<script>
// A busy GPU canvas, so the shared screen changes every frame.
const sp = document.getElementById('spin').getContext('2d');
let rafN = 0, rafT0 = performance.now(), rafFps = 0;
(function spin(t) { if (++rafN === 120) { rafFps = 120000 / (t - rafT0); rafT0 = t; rafN = 0; }
  sp.fillStyle = 'hsl(' + (t / 10 % 360) + ',80%,50%)';
  sp.fillRect(0, 0, 300, 300); sp.fillStyle = '#000';
  sp.save(); sp.translate(150, 150); sp.rotate(t / 300);
  sp.fillRect(-100, -10, 200, 20); sp.restore();
  requestAnimationFrame(spin); })(0);
const st = document.getElementById('st');
let frames = 0, t0 = 0, w = 0, h = 0, spread = 0, state = 'idle';
function report() {
  const f = t0 ? frames / ((performance.now() - t0) / 1000) : 0;
  document.title = 'share|' + state + '|fps=' + f.toFixed(1) + '|size=' + w + 'x' + h +
    '|spread=' + spread.toFixed(0) + '|frames=' + frames + '|raf=' + rafFps.toFixed(1);
  st.textContent = document.title;
}
setInterval(report, 500);
async function share() {
  state = 'asking'; report();
  let s;
  try {
    s = await navigator.mediaDevices.getDisplayMedia({video: {frameRate: $fps}, audio: false});
  } catch (e) { state = 'error:' + e.name; report(); return; }
  const a = new RTCPeerConnection(), b = new RTCPeerConnection();
  a.onicecandidate = e => e.candidate && b.addIceCandidate(e.candidate);
  b.onicecandidate = e => e.candidate && a.addIceCandidate(e.candidate);
  const rx = document.getElementById('rx');
  b.ontrack = e => { rx.srcObject = new MediaStream([e.track]); };
  s.getTracks().forEach(t => a.addTrack(t, s));
  await a.setLocalDescription(await a.createOffer());
  await b.setRemoteDescription(a.localDescription);
  await b.setLocalDescription(await b.createAnswer());
  await a.setRemoteDescription(b.localDescription);
  state = 'sharing';
  const out = document.getElementById('out').getContext('2d', {willReadFrequently: true});
  const onFrame = () => {
    if (!t0) t0 = performance.now(); else frames++;
    w = rx.videoWidth; h = rx.videoHeight;
    out.drawImage(rx, 0, 0, 640, 360);
    if (frames % 15 === 0) {
      const d = out.getImageData(0, 0, 640, 360).data;
      let lo = 255, hi = 0;
      for (let y = 0; y < 16; y++) for (let x = 0; x < 16; x++) {
        const i = ((y * 22 + 10) * 640 + (x * 40 + 10)) * 4;
        const l = (d[i] * 3 + d[i + 1] * 6 + d[i + 2]) / 10;
        lo = Math.min(lo, l); hi = Math.max(hi, l);
      }
      spread = hi - lo;
    }
    rx.requestVideoFrameCallback(onFrame);
  };
  rx.requestVideoFrameCallback(onFrame);
  window.stopShare = () => { s.getTracks().forEach(t => t.stop()); a.close(); b.close(); state = 'stopped'; };
}
share();
</script></body></html>
EOF

server=$(pgrep -x nitro-server -u "$(id -u)" | tail -1)
helper=$(pgrep -f "nitro-gpu-vulkan" -u "$(id -u)" | tail -1)
[[ -n $server ]] || { echo "no nitro-server of this user" >&2; exit 1; }
echo "server $server helper ${helper:-none}; bufinfo before: $(bufinfo)"

# The load: a video on a plane, and the GPU Chromium window (the page).
if [[ -x $video ]]; then
    "$video" --synthetic > "$work/video.log" 2>&1 &
    pids+=($!)
fi
sleep 3

window() { # label — sample display fps and CPU over $secs
    local f0 f1 s0 s1 h0 h1 c0 c1 t0 t1 pc
    pc=$(chrome_pids)
    f0=$(stat frames); s0=$(jiffies "$server"); h0=$(jiffies ${helper:-}); c0=$(jiffies $pc); t0=$(date +%s.%N)
    local cf0; cf0=$(stat capture_frames)
    sleep "$secs"
    f1=$(stat frames); s1=$(jiffies "$server"); h1=$(jiffies ${helper:-}); c1=$(jiffies $pc); t1=$(date +%s.%N)
    local cf1; cf1=$(stat capture_frames)
    echo "$1: planes_mode $(stat planes_mode) planes_in_use $(stat planes_in_use) damage_px_mean $(stat damage_px_mean)" >&2
    python3 - "$1" "$f0" "$f1" "$s0" "$s1" "$h0" "$h1" "$c0" "$c1" "$t0" "$t1" "$(getconf CLK_TCK)" "$cf0" "$cf1" <<'PY'
import sys
l, f0, f1, s0, s1, h0, h1, c0, c1, t0, t1, hz, cf0, cf1 = sys.argv[1:]
dt = float(t1) - float(t0); hz = float(hz)
num = lambda v: float(v) if v not in ("-", "") else 0.0
pct = lambda a, b: (num(b) - num(a)) / hz / dt * 100
print(f"{l}: display_fps {(num(f1)-num(f0))/dt:.1f} capture_fps {(num(cf1)-num(cf0))/dt:.1f} "
      f"server_cpu {pct(s0,s1):.1f}% helper_cpu {pct(h0,h1):.1f}% chrome_cpu {pct(c0,c1):.1f}% (of one core)")
print(f"DISPLAY_FPS {(num(f1)-num(f0))/dt:.2f}")
PY
}

flags=(--ozone-platform=nitro --enable-logging=stderr --no-first-run --no-default-browser-check
       --disable-features=Translate --password-store=basic
       --use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE,AcceleratedVideoDecodeLinuxGL
       --auto-select-screen-capture-source --remote-debugging-port=$port
       --vmodule='nitro_desktop_capturer=1')

# Off: the same load (the page's spinner in a GPU window, the video), no
# sharing: the page with its share() call removed.
sed 's/^share();$//' "$work/share.html" > "$work/spin.html"
"$chrome" "${flags[@]}" --user-data-dir="$work/prof" "file://$work/spin.html" > "$work/off.log" 2>&1 &
pids+=($!)
sleep 10
off=$(window "sharing off" | tee /dev/stderr | awk '/^DISPLAY_FPS/ {print $2}')
echo "page (off): $(title)"
# The same load with the server recording but no Chromium consumer:
# nitro-shot --record, which only maps and releases the slots. Separates
# the capture's cost to the display from the browser's (encode) CPU.
shot=${NITRO_SHOT:-$(command -v nitro-shot || echo "$HOME/nitro-bin/nitro-shot")}
"$shot" --record 100000 --fps "$fps" > "$work/record.log" 2>&1 &
rec=$!
sleep 2
mid=$(window "recording, nitro-shot" | tee /dev/stderr | awk '/^DISPLAY_FPS/ {print $2}')
echo "page (recording): $(title)"
kill "$rec" 2>/dev/null; wait "$rec" 2>/dev/null
sleep 1
kill "${pids[-1]}" 2>/dev/null; wait "${pids[-1]}" 2>/dev/null
for _ in 1 2 3 4 5; do pgrep -f -- "--user-data-dir=$work/prof" >/dev/null || break; sleep 1; done
pkill -9 -f -- "--user-data-dir=$work/prof" 2>/dev/null
sleep 2

# On.
"$chrome" "${flags[@]}" --user-data-dir="$work/prof" "file://$work/share.html" > "$work/on.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 30); do
    t=$(title); [[ $t == share\|sharing* || $t == share\|error* ]] && break; sleep 1
done
sleep 4
echo "page: $(title)"
echo "bufinfo while recording: $(bufinfo)"
command -v nitro-shot >/dev/null && nitro-shot -o "$work/screen.png" 2>/dev/null ||
    "$HOME/nitro-bin/nitro-shot" -o "$work/screen.png" 2>/dev/null || true
on=$(window "sharing on " | tee /dev/stderr | awk '/^DISPLAY_FPS/ {print $2}')
t=$(title)
echo "page: $t"
for k in capture_active capture_frames capture_drops capture_gpu_us capture_gpu_us_max capture_copy_us capture_rings_bytes capture_snapshot_bytes planes_mode planes_in_use; do
    printf '%s=%s ' "$k" "$(stat $k)"
done; echo
grep -E "nitro-capture" "$work/on.log" | tail -4 | cut -c1-220 | sed 's/^/  log: /'
rxfps=$(sed -n 's/.*|fps=\([0-9.]*\).*/\1/p' <<<"$t")
spread=$(sed -n 's/.*|spread=\([0-9]*\).*/\1/p' <<<"$t")
check received-content "$([[ ${spread:-0} -ge 40 ]] && echo 1 || echo 0)" "luma spread ${spread:-?} over 256 samples, $t"
# The receiver's rate is bounded by the software VP8 encode of the
# loopback (1080p on box1's Pentium: 8–10 fps; 1440p on testhost2: 15–21); the check is that frames
# keep flowing, the number is the measurement.
check received-fps "$(python3 -c "print(1 if float('${rxfps:-0}') >= 5 else 0)")" "${rxfps:-?} fps received, asked $fps"
check display-fps "$(python3 -c "print(1 if float('${mid:-0}') >= float('${off:-0}') - 0.5 else 0)")" "off ${off:-?}, recording (nitro-shot) ${mid:-?}; sharing through Chromium ${on:-?} (the browser's own frames slow down under its encoder)"

kill "${pids[-1]}" 2>/dev/null; wait "${pids[-1]}" 2>/dev/null
for _ in 1 2 3 4 5; do pgrep -f -- "--user-data-dir=$work/prof" >/dev/null || break; sleep 1; done
pkill -9 -f -- "--user-data-dir=$work/prof" 2>/dev/null
sleep 2
echo "after the browser closed: capture_active=$(stat capture_active) capture_rings_bytes=$(stat capture_rings_bytes) capture_snapshot_bytes=$(stat capture_snapshot_bytes) bufinfo $(bufinfo)"
[[ $fails == 0 ]] && echo "chromium-share: all passed" || echo "chromium-share: $fails failed"
exit $((fails > 0))
