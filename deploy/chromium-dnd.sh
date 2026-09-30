#!/usr/bin/env bash
# Chromium drag and drop over nitro's DATA ops, end to end, run ON a box
# (#3965). `just chromium-dnd` copies it over ssh and runs it inside the
# logged-in session. Needs `deploy-chromium` and a running session with
# nitro-term installed; leaves nothing but /tmp files.
#
# Two browsers with separate profiles are two nitro clients, tiled side by
# side (Super+Right / Super+Left): A on the right, B on the left. Drags are
# driven with the control socket's `input` (button down, motion steps,
# button up: the server routes DragEnter/Motion from real motion). Pages
# report drops in document.title and `dragend` in a log, read back over
# DevTools. Positions are found in a `shot` by the pages' colours, so the
# script does not care about the output's size or scale.
#
# Checks, each printing PASS/FAIL:
#   text-a-to-b         selected text dragged from A into B's drop zone
#   link-a-to-b         a link dragged A -> B arrives as text/uri-list
#   text-within-a       text dragged into A's own drop zone (one client)
#   text-on-term        selected text dropped on nitro-term is pasted at
#                       its prompt (dragend copy, text in the grid)
#   link-on-term        a link dropped on nitro-term pastes its URL
#   drop-on-bar         a drop on the bar (no DATA: no target) is rejected
#   escape              Escape mid-drag cancels; A and B still respond
#   source-killed       A SIGKILLed mid-drag: B gets DragLeave, the grab
#                       goes, B still takes a click
#   tab-drag (INFO)     a tab dragged out of A's strip: reported only, the
#                       move loop is not wired (docs/chromium.md)
set -uo pipefail
chrome=${NITRO_CHROME:-$HOME/nitro-bin/chromium/chrome}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
ctl=$XDG_RUNTIME_DIR/nitro/control.sock
[[ -S $ctl ]] || { echo "no nitro session" >&2; exit 1; }
work=$(mktemp -d /tmp/nitro-dnd.XXXXXX)
fails=0

# ------------------------------------------------------------------ tools
cat > "$work/lib.py" <<'EOF'
import base64, json, os, socket, sys, urllib.parse, urllib.request
CTL = os.environ["XDG_RUNTIME_DIR"] + "/nitro/control.sock"

def say(line):
    s = socket.socket(socket.AF_UNIX); s.connect(CTL)
    s.sendall((line + "\n").encode()); s.settimeout(3)
    f = s.makefile("rb"); out = [f.readline().decode()]
    if line in ("stats",):
        while True:
            l = f.readline().decode()
            if not l.strip(): break
            out.append(l)
    return "".join(out)

def js(port, expr):
    ts = json.load(urllib.request.urlopen("http://127.0.0.1:%d/json" % port, timeout=3))
    ws = next(t["webSocketDebuggerUrl"] for t in ts
              if t.get("type") == "page" and "dnd.html" in t.get("url", ""))
    u = urllib.parse.urlparse(ws)
    s = socket.create_connection((u.hostname, u.port)); s.settimeout(5)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall((f"GET {u.path} HTTP/1.1\r\nHost: {u.netloc}\r\nUpgrade: websocket\r\n"
               f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
               "Sec-WebSocket-Version: 13\r\n\r\n").encode())
    buf = b""
    while b"\r\n\r\n" not in buf: buf += s.recv(4096)
    buf = buf.split(b"\r\n\r\n", 1)[1]
    msg = json.dumps({"id": 1, "method": "Runtime.evaluate",
                      "params": {"expression": expr, "returnByValue": True}}).encode()
    mask = os.urandom(4); hdr = bytearray([0x81]); n = len(msg)
    if n < 126: hdr.append(0x80 | n)
    else: hdr += bytes([0x80 | 126]) + n.to_bytes(2, "big")
    s.sendall(bytes(hdr) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(msg)))
    def need(k):
        nonlocal buf
        while len(buf) < k: buf += s.recv(65536)
    need(2); ln = buf[1] & 0x7f; off = 2
    if ln == 126: need(4); ln = int.from_bytes(buf[2:4], "big"); off = 4
    elif ln == 127: need(10); ln = int.from_bytes(buf[2:10], "big"); off = 10
    need(off + ln)
    v = json.loads(buf[off:off + ln]).get("result", {}).get("result", {}).get("value")
    return "" if v is None else str(v)

def shot():
    s = socket.socket(socket.AF_UNIX); s.connect(CTL); s.sendall(b"shot\n")
    f = s.makefile("rb"); h = f.readline().split()
    w, hh, st = int(h[1]), int(h[2]), int(h[3])
    return w, hh, st, f.read(st * hh)

def locate():
    """Centres (device px) of: A's and B's source span, link and drop zone,
    A's tab strip, and a point on nitro-term over B's half."""
    w, h, st, d = shot()
    def px(x, y): o = y * st + 4 * x; return d[o + 2], d[o + 1], d[o]
    def box(pred, x0, x1, y0=0, y1=None, step=2):
        xs, ys = [], []
        for y in range(y0, y1 or h, step):
            for x in range(x0, x1, step):
                if pred(px(x, y)): xs.append(x); ys.append(y)
        return (min(xs), min(ys), max(xs), max(ys)) if xs else None
    mid = w // 2
    out = {}
    for side, (x0, x1) in (("b", (0, mid)), ("a", (mid, w))):
        src = box(lambda p: p == (0xff, 0xff, 0xdd), x0, x1)
        dz = box(lambda p: p == (0xdd, 0xff, 0xdd), x0, x1, step=8)
        if not src or not dz:
            continue
        out[side + "_src"] = ((src[0] + src[2]) // 2, (src[1] + src[3]) // 2)
        # The link sits one line-height and a bit under the span.
        out[side + "_lnk"] = (src[0] + 20, src[3] + (src[3] - src[1]) * 2)
        out[side + "_dz"] = ((dz[0] + dz[2]) // 2, (dz[1] * 3 + dz[3]) // 4)
        out[side + "_tabs"] = (x0 + (x1 - x0) * 3 // 8, max(8, src[1] - (src[3] - src[1]) * 3))
    # nitro-term: the rows with a long dark run (its background; page
    # text is short runs), over B's half.
    rows = []
    for y in range(h // 5, h, 8):
        run = best = 0
        for x in range(0, mid, 4):
            run = run + 1 if sum(px(x, y)) < 90 else 0
            best = max(best, run)
        if best * 4 >= 300: rows.append(y)
    if rows:
        y = rows[len(rows) // 2]
        xs = [x for x in range(0, mid, 4) if sum(px(x, y)) < 90]
        out["term"] = ((min(xs) + max(xs)) // 2, y)
    else:
        out["term"] = None
    out["bar"] = (mid + mid // 2, 6)
    return out

if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "js": print(js(int(sys.argv[2]), sys.argv[3]))
    elif cmd == "locate":
        for k, v in locate().items():
            print("%s=%s" % (k, "" if v is None else "%d,%d" % v))
    elif cmd == "stat":
        for l in say("stats").splitlines():
            if l.split(" ")[0] == sys.argv[2]: print(l.split()[1])
    else: say(" ".join(sys.argv[1:]))
EOF
L() { python3 "$work/lib.py" "$@"; }
S() { L "$@" >/dev/null; }
check() { # name ok detail
    if [[ $2 == 1 ]]; then echo "PASS $1  $3"; else echo "FAIL $1  $3"; fails=$((fails+1))
        grep -h "nitro: " "$work/a.log" | grep -v "Focus\|canvas\|GBM" | tail -4 | sed 's/^/  a: /'; fi
}
chord() { S input key 125 down; S input key "$1" tap; S input key 125 up; sleep 1; }
# drag X0,Y0 X1,Y1 [release=1]: press, twelve motion steps, release.
drag() {
    local x0=${1%,*} y0=${1#*,} x1=${2%,*} y1=${2#*,}
    S input motion "$x0" "$y0"; sleep 0.2
    S input button left down; sleep 0.2
    for i in $(seq 1 12); do
        S input motion $((x0 + (x1 - x0) * i / 12)) $((y0 + (y1 - y0) * i / 12)); sleep 0.08
    done
    sleep 0.5
    if [[ ${3:-1} == 1 ]]; then S input button left up; sleep 1; fi
}
selA() { click "$a_dz"; L js 9501 "log=[];sel()" >/dev/null; }
click() { S input motion "${1%,*}" "${1#*,}"; S input button left click; sleep 0.5; }
reset() { L js "$1" "log=[];document.title=location.hash.slice(1)+'|ready';getSelection().removeAllRanges()" >/dev/null; }

# ------------------------------------------------------------------ page
cat > "$work/dnd.html" <<'EOF'
<html><head><title>T|ready</title><style>
body{margin:0;font:28px sans-serif}
#src{position:absolute;left:30px;top:20px;background:#ffd}
#lnk{position:absolute;left:30px;top:90px}
#dz{position:absolute;left:0;top:260px;right:0;bottom:0;background:#dfd}
</style></head><body>
<span id=src>dragtext-4711</span><a id=lnk href="https://example.com/nitro-link-9">link-9</a>
<div id=dz>drop here</div>
<script>
const tag = location.hash.slice(1);
let log = [];
document.title = tag + '|ready';
function sel() { const r = document.createRange(); r.selectNodeContents(src);
  const s = getSelection(); s.removeAllRanges(); s.addRange(r); }
dz.addEventListener('dragover', e => e.preventDefault());
dz.addEventListener('drop', e => { e.preventDefault(); const d = e.dataTransfer;
  document.title = tag + '|drop|' + [...d.types].join(',') + '|' +
    d.getData('text/plain').slice(0, 60) + '|uri=' + d.getData('text/uri-list').slice(0, 60); });
addEventListener('dragstart', e => log.push('start'));
addEventListener('dragend', e => log.push('end:' + e.dataTransfer.dropEffect));
addEventListener('click', e => log.push('click'));
</script></body></html>
EOF

flags=(--ozone-platform=nitro --disable-gpu --no-first-run --no-default-browser-check
       --password-store=basic --disable-features=Translate --enable-logging=stderr
       --vmodule=nitro_data_drag=1,nitro_connection_host=1)
start_chrome() { # tag port [extra urls]
    local tag=$1 port=$2; shift 2
    setsid "$chrome" "${flags[@]}" --user-data-dir="$work/prof-$tag" --remote-debugging-port="$port" \
        "file://$work/dnd.html#$tag" "$@" >> "$work/$tag.log" 2>&1 < /dev/null &
    echo $!
}
cleanup() {
    kill "${pa:-}" "${pb:-}" 2>/dev/null
    [[ -n ${term_started:-} ]] && hey nitro-term quit >/dev/null 2>&1
    sleep 1
    pkill -9 -f -- "--user-data-dir=$work/prof" 2>/dev/null
    if [[ -n ${KEEP:-} ]]; then echo "kept $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

pa=$(start_chrome a 9501 "file://$work/dnd.html#a2"); sleep 7; chord 106   # A right
pb=$(start_chrome b 9502); sleep 7; chord 105                              # B left
eval "$(L locate)"
echo "  A src=$a_src dz=$a_dz  B dz=$b_dz"

# ------------------------------------------------------------------ checks
# 1. Text, A -> B.
reset 9501; reset 9502; selA
drag "$a_src" "$b_dz"
t=$(L js 9502 document.title); e=$(L js 9501 'log.join()')
[[ $t == *'|drop|'*text/plain*'|dragtext-4711|'* && $e == *end:copy* ]] && ok=1 || ok=0
check text-a-to-b $ok "$t  A:$e"

# 2. A link, A -> B.
reset 9501; reset 9502
drag "$a_lnk" "$b_dz"
t=$(L js 9502 document.title)
[[ $t == *text/uri-list*'uri=https://example.com/nitro-link-9'* ]] && ok=1 || ok=0
check link-a-to-b $ok "$t"

# 3. Within one window: source and target are the same client.
reset 9501; selA
drag "$a_src" "$a_dz"
t=$(L js 9501 document.title); e=$(L js 9501 'log.join()')
[[ $t == *'|drop|'*dragtext-4711* && $e == *end:copy* ]] && ok=1 || ok=0
check text-within-a $ok "$t  A:$e"

# 4. A drop on the bar: no DATA, so no target at all.
# Carried inside the page, then straight up: crossing A's tab strip on the
# way would make it a drop candidate (Chrome's own strip drop indicator).
reset 9501; selA
drag "$a_src" "$a_dz" 0
S input motion "${bar%,*}" "${bar#*,}"; sleep 0.3
S input motion "$(( ${bar%,*} + 4 ))" "${bar#*,}"; sleep 0.5
S input button left up; sleep 1
click "$a_dz"
e=$(L js 9501 'log.join()')
[[ $e == *end:none,click ]] && ok=1 || ok=0
check drop-on-bar $ok "A:$e"

# 5. Drops on nitro-term (#3966): text and a link are pasted at its
# prompt. It is tiled over B's half for the checks, and quit after them.
hey nitro-term quit >/dev/null 2>&1; sleep 0.5
setsid nitro-term >/dev/null 2>&1 < /dev/null & term_started=1
sleep 2; chord 105
eval "$(L locate 2>/dev/null | grep '^term=')"
grid() { hey nitro-term get grid value 2>/dev/null; }
if [[ -n ${term:-} ]]; then
    hey nitro-term do grid send '\x15' >/dev/null 2>&1; sleep 0.3   # ^U: clear the line
    reset 9501; selA
    drag "$a_src" "$term"
    e=$(L js 9501 'log.join()'); g=$(grid)
    [[ $e == *end:copy* && $g == *dragtext-4711* ]] && ok=1 || ok=0
    check text-on-term $ok "A:$e grid-has-text=$([[ $g == *dragtext-4711* ]] && echo 1 || echo 0)"
    hey nitro-term do grid send '\x15' >/dev/null 2>&1; sleep 0.3
    reset 9501
    drag "$a_lnk" "$term"
    g=$(grid); alive=0; pgrep -x nitro-term >/dev/null && alive=1
    [[ $g == *https://example.com/nitro-link-9* && $alive == 1 ]] && ok=1 || ok=0
    check link-on-term $ok "term-alive=$alive grid-has-link=$([[ $g == *nitro-link-9* ]] && echo 1 || echo 0)"
    hey nitro-term do grid send '\x15' >/dev/null 2>&1
else
    check text-on-term 0 "nitro-term not found on screen"
    check link-on-term 0 "nitro-term not found on screen"
fi
hey nitro-term quit >/dev/null 2>&1; term_started=; sleep 1
click "$a_dz"; click "$b_dz"

# 6. Escape mid-drag, over B's drop zone.
reset 9501; reset 9502; selA
drag "$a_src" "$b_dz" 0
S input key 1 tap; sleep 0.5; S input button left up; sleep 1
click "$b_dz"; click "$a_dz"
e=$(L js 9501 'log.join()'); eb=$(L js 9502 'log.join()'); tb=$(L js 9502 document.title)
[[ $e == *end:none,click && $eb == click && $tb == b\|ready ]] && ok=1 || ok=0
check escape $ok "A:$e B:$eb $tb"

# 7. The source is SIGKILLed mid-drag.
reset 9502; selA
leaves=$(grep -c "nitro: DragLeave" "$work/b.log")
drag "$a_src" "$b_dz" 0
kill -9 "$pa"; pa=; sleep 1.5
grab=$(L stat dnd_grab)
S input button left up; sleep 0.5
click "$b_dz"
eb=$(L js 9502 'log.join()'); tb=$(L js 9502 document.title)
leave=0; (( $(grep -c "nitro: DragLeave" "$work/b.log") > leaves )) && leave=1
[[ $grab == 0 && $eb == click && $tb == b\|ready && $leave == 1 ]] && ok=1 || ok=0
check source-killed $ok "grab=$grab B:$eb $tb leave=$leave"

# 8. A tab dragged out of a strip (INFO): the move loop is not wired.
pa=$(start_chrome a 9501 "file://$work/dnd.html#a2"); sleep 7
pages() { curl -s "http://127.0.0.1:$1/json" | python3 -c 'import json,sys; print(sum(t["type"]=="page" for t in json.load(sys.stdin)))'; }
before=$(grep -c "nitro: StartDrag" "$work/a.log")
drag "$a_tabs" "$b_dz"; sleep 1
starts=$(( $(grep -c "nitro: StartDrag" "$work/a.log") - before ))
echo "INFO tab-drag  StartDrag=$starts A-pages=$(pages 9501) B-pages=$(pages 9502) (tab detach is DnD with chromium/x-window-drag; see docs/chromium.md)"

grep -hE "FATAL|nitro: connection lost|never started|refused by the server" "$work"/*.log |
    sort | uniq -c | head -8 | sed 's/^/  log: /'
echo "$fails failed"
exit $((fails > 0))
