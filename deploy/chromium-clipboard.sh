#!/usr/bin/env bash
# Chromium clipboard over nitro's DATA ops, end to end, run ON a box
# (#3943). `just chromium-clipboard` copies it over ssh and runs it inside
# the logged-in session. Needs `deploy-chromium` and a running session
# with nitro-term and nitro-files installed; leaves nothing but /tmp files.
#
# Two browsers with separate profiles are two nitro clients, so A -> B is
# a real cross-client transfer (SelectionRequest / SendSelection /
# SelectionData), not ClipboardOzone's own cache. Pages report what a
# paste delivered in document.title, read back over the DevTools /json
# endpoint. Input goes through the control socket's `input` request
# (evdev codes; C/V/A sit in the same place on the de and us layouts).
#
# Checks, each printing PASS/FAIL:
#   text-chromium-to-term   Ctrl+C in A, Ctrl+Shift+V in nitro-term
#   text-native-to-chromium Ctrl+C in a nitro-ui textfield, Ctrl+V in A
#   html-chromium-to-chromium  rich selection in A pastes into B with text/html
#   html-chromium-to-term   the same selection pastes into nitro-term as plain text
#   uri-files-to-chromium   a file copied in nitro-files pastes into A as
#                           text/uri-list / Files
#   large-chromium-to-chromium  an 8 MB textarea from A into B, with B's
#                           requestAnimationFrame gap during the read
#   owner-quits             A copies and is SIGKILLed; nitro-term pastes,
#                           stays alive, and B sees the selection cleared
set -uo pipefail
chrome=${NITRO_CHROME:-$HOME/nitro-bin/chromium/chrome}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
ctl=$XDG_RUNTIME_DIR/nitro/control.sock
[[ -S $ctl ]] || { echo "no nitro session" >&2; exit 1; }
work=$(mktemp -d /tmp/nitro-clip.XXXXXX)
fails=0

say() { python3 - "$ctl" "$*" <<'EOF'
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
s.sendall((sys.argv[2] + "\n").encode()); s.settimeout(3)
try: s.recv(4096)
except OSError: pass
EOF
}
# chord KEY... : press in order, release in reverse.
chord() {
    local k ks=("$@")
    for k in "${ks[@]}"; do say input key "$k" down; done
    for ((i=${#ks[@]}-1; i>=0; i--)); do say input key "${ks[i]}" up; done
    sleep 0.3
}
CTRL=29 SHIFT=42 KA=30 KC=46 KV=47
check() { # name ok detail
    if [[ $2 == 1 ]]; then echo "PASS $1  $3"; else echo "FAIL $1  $3"; fails=$((fails+1)); fi
}
# Focus the window whose bar entry's title matches $1 (fixed string).
focus() {
    local id
    id=$(hey nitro-bar tree | grep -E "window/windows/win[0-9]+\s" | grep -F -- "$1" | head -1 | awk '{print $1}')
    [[ -n $id ]] || { echo "  (no window matching $1)"; return 1; }
    # `▸` marks the focused window; clicking that entry would minimize it.
    hey nitro-bar get "$id" value | grep -q '^▸' && return 0
    hey nitro-bar do "$id" click >/dev/null
    sleep 0.8
}
# The title of the page at $2 in the browser on port $1.
title() { curl -s "http://127.0.0.1:$1/json" | python3 -c '
import json, sys
for t in json.load(sys.stdin):
    if t.get("type") == "page" and sys.argv[1] in t.get("url", ""):
        print(t["title"]); break' "$2"; }
term_text() { hey nitro-term get grid text; }
# Run JS in the page at $2 of the browser on port $1 (DevTools
# Runtime.evaluate over a minimal websocket client; no extra packages).
eval_js() { # port page js — runs via DevTools Runtime.evaluate
    local ws
    ws=$(curl -s "http://127.0.0.1:$1/json" | python3 -c '
import json, sys
for t in json.load(sys.stdin):
    if t.get("type") == "page" and sys.argv[1] in t.get("url", ""):
        print(t["webSocketDebuggerUrl"]); break' "$2")
    python3 - "$ws" "$3" <<'EOF'
import base64, json, os, socket, sys, urllib.parse
u = urllib.parse.urlparse(sys.argv[1])
s = socket.create_connection((u.hostname, u.port))
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((f"GET {u.path} HTTP/1.1\r\nHost: {u.netloc}\r\nUpgrade: websocket\r\n"
           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
buf = b""
while b"\r\n\r\n" not in buf: buf += s.recv(4096)
msg = json.dumps({"id": 1, "method": "Runtime.evaluate",
                  "params": {"expression": sys.argv[2]}}).encode()
mask = os.urandom(4)
hdr = bytearray([0x81])
n = len(msg)
if n < 126: hdr.append(0x80 | n)
elif n < 65536: hdr += bytes([0x80 | 126]) + n.to_bytes(2, "big")
else: hdr += bytes([0x80 | 127]) + n.to_bytes(8, "big")
s.sendall(bytes(hdr) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(msg)))
s.settimeout(5)
try: s.recv(65536)
except OSError: pass
EOF
}

# ---------------------------------------------------------------- pages
cat > "$work/a.html" <<'EOF'
<html><head><title>A|ready</title></head><body>
<textarea id=t rows=4 cols=60>clip-from-chromium-4711</textarea>
<div id=rich contenteditable><b>bold-5150</b> and <i>italic</i></div>
<script>
const t = document.getElementById('t');
function selText() { t.focus(); t.select(); }
function selRich() { const r = document.createRange();
  r.selectNodeContents(document.getElementById('rich'));
  const s = getSelection(); s.removeAllRanges(); s.addRange(r); }
function big(n) { t.value = 'L'.repeat(n - 4) + 'TAIL'; selText(); }
selText();
document.addEventListener('paste', e => { e.preventDefault();
  const d = e.clipboardData, p = d.getData('text/plain');
  document.title = 'A|pasted|' + [...d.types].join(',') + '|' +
    p.slice(0, 80).replace(/\s+/g, ' ') + '|files=' + d.files.length +
    (d.files.length ? ':' + d.files[0].name : '');
});
</script></body></html>
EOF
cat > "$work/b.html" <<'EOF'
<html><head><title>B|ready</title></head><body>
<div id=box contenteditable style="height:200px;border:1px solid">x</div>
<script>
document.getElementById('box').focus();
let last = performance.now(), gap = 0;
(function tick(now) { gap = Math.max(gap, now - last); last = now;
  requestAnimationFrame(tick); })(last);
function arm() { gap = 0; }
document.addEventListener('paste', e => { e.preventDefault();
  // Read everything now: the DataTransfer is unreadable after the event.
  const d = e.clipboardData, p = d.getData('text/plain'), h = d.getData('text/html');
  const types = [...d.types].join(',');
  const rich = /<b[ >]/.test(h) && h.indexOf('bold-5150') >= 0;
  setTimeout(() => { document.title = 'B|pasted|' + types +
    '|len=' + p.length + '|tail=' + p.slice(-4) + '|html=' + rich +
    '|maxgap=' + Math.round(gap); }, 1500);
});
</script></body></html>
EOF

flags=(--ozone-platform=nitro --disable-gpu --no-first-run --no-default-browser-check
       --password-store=basic --disable-features=Translate --enable-logging=stderr
       --vmodule=nitro_clipboard=1,nitro_connection_host=1)
start_chrome() { # tag port page
    "$chrome" "${flags[@]}" --user-data-dir="$work/prof-$1" --remote-debugging-port="$2" \
        "file://$work/$3" > "$work/$1.log" 2>&1 &
    echo $!
}
cleanup() {
    kill "${pa:-}" "${pb:-}" 2>/dev/null
    [[ -n ${term_started:-} ]] && hey nitro-term quit >/dev/null 2>&1
    [[ -n ${files_started:-} ]] && hey nitro-files quit >/dev/null 2>&1
    sleep 1
    pkill -9 -f -- "--user-data-dir=$work/prof" 2>/dev/null
    if [[ -n ${KEEP:-} ]]; then echo "kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

hey nitro-term >/dev/null 2>&1 || { setsid nitro-term >/dev/null 2>&1 & term_started=1; }
mkdir -p "$work/clipdir" && echo hello > "$work/clipdir/clip-file-9.txt"
hey nitro-files >/dev/null 2>&1 && hey nitro-files quit >/dev/null 2>&1
sleep 0.5; setsid nitro-files >/dev/null 2>&1 & files_started=1
pa=$(start_chrome a 9301 a.html)
pb=$(start_chrome b 9302 b.html)
sleep 8

# ---------------------------------------------------------------- checks
term_prompt_clear() { focus nitro-term && chord $CTRL $KC; }

# 1. Chromium -> nitro-term.
focus 'A|' && chord $CTRL $KA && chord $CTRL $KC
term_prompt_clear; chord $CTRL $SHIFT $KV; sleep 0.5
if term_text | grep -q clip-from-chromium-4711; then ok=1; else ok=0; fi
check text-chromium-to-term $ok ""
chord $CTRL $KC

# 2. nitro-ui textfield -> Chromium.
P=window/content/content_header/path
focus "$(basename "$HOME")"   # nitro-files starts in ~, titled after it
hey nitro-files set $P value native-to-chromium-0815 >/dev/null
hey nitro-files do $P focus >/dev/null
chord $CTRL $KA; chord $CTRL $KC
focus 'A|'; chord $CTRL $KV; sleep 1
t=$(title 9301 a.html)
[[ $t == *native-to-chromium-0815* ]] && ok=1 || ok=0
check text-native-to-chromium $ok "$t"
hey nitro-files set $P value "$work/clipdir" >/dev/null; hey nitro-files do $P submit >/dev/null; sleep 0.5

# 3. HTML: A's rich selection into B, and into nitro-term.
focus 'A|'
eval_js 9301 a.html 'selRich()'
chord $CTRL $KC
focus 'B|'; chord $CTRL $KV; sleep 2.5
t=$(title 9302 b.html)
[[ $t == *text/html* && $t == *html=true* ]] && ok=1 || ok=0
check html-chromium-to-chromium $ok "$t"
term_prompt_clear; chord $CTRL $SHIFT $KV; sleep 0.5
if term_text | grep -q "bold-5150 and italic" && ! term_text | grep -q "<b>"; then ok=1; else ok=0; fi
check html-chromium-to-term $ok ""
chord $CTRL $KC

# 4. nitro-files -> Chromium (text/uri-list).
focus clipdir
hey nitro-files do window/content/content_body/list focus >/dev/null
hey nitro-files do window/content/content_body/list select 0 >/dev/null
chord $CTRL $KC
focus 'A|'; eval_js 9301 a.html 'selText()'; chord $CTRL $KV; sleep 1
t=$(title 9301 a.html)
[[ $t == *clip-file-9.txt* ]] && ok=1 || ok=0
check uri-files-to-chromium $ok "$t"

# 5. 8 MB of text, A -> B; B's UI thread must keep producing frames.
eval_js 9301 a.html 'big(8*1024*1024)'
focus 'A|'; eval_js 9301 a.html 'selText()'; chord $CTRL $KC
focus 'B|'; eval_js 9302 b.html 'arm()'; chord $CTRL $KV; sleep 4
t=$(title 9302 b.html)
gap=$(sed -n 's/.*maxgap=\([0-9]*\).*/\1/p' <<<"$t")
[[ $t == *len=8388608* && $t == *tail=TAIL* && ${gap:-9999} -lt 250 ]] && ok=1 || ok=0
check large-chromium-to-chromium $ok "$t"

# 6. The owner quits: A copies, is killed, nitro-term pastes.
eval_js 9301 a.html 't.value="owner-quits-77"; selText()'
focus 'A|'; chord $CTRL $KC; sleep 0.5
kill -9 "$pa"; pa=; sleep 1
tpid=$(pgrep -x nitro-term | head -1)
term_prompt_clear; chord $CTRL $SHIFT $KV; sleep 1
alive=0; kill -0 "$tpid" 2>/dev/null && hey nitro-term get grid text >/dev/null && alive=1
check owner-quits $alive "nitro-term pid $tpid alive=$alive"
chord $CTRL $KC

grep -hE "FATAL|nitro: connection lost|nitro: clipboard" "$work"/*.log | sort | uniq -c | head -8 | sed 's/^/  log: /'
echo "$fails failed"
exit $((fails > 0))
