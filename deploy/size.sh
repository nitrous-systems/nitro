#!/usr/bin/env bash
# Binary sizes and resident memory of every shipped binary, for docs/budget.md.
#
# Two halves, and the second is the interesting one:
#
#   sizes  — `stat -c %s` of the release binaries. They are already
#            stripped: `[profile.release] strip = true` in Cargo.toml, so
#            there is nothing left for a `strip(1)` pass to remove and
#            running one would only measure whether the tool is installed.
#   RSS    — a real server on the fake backend with a real client
#            connected, read out of /proc/<pid>/status. VmRSS is what is
#            resident now, VmHWM the high-water mark: the peak is what
#            matters on a 3.3 GB box with no swap, and it is the number
#            a steady-state `top` never shows you.
#
# Both processes are stopped cleanly at the end (`nitro-shot --quit`, then
# SIGTERM), so a failed run cannot leave a server holding the socket.
#
# Usage: just size [WINDOWS]   (default 1; pass 5 for the multi-window row)
set -euo pipefail

windows="${1:-1}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

runtime="$(mktemp -d)"
cleanup() {
    [[ -n "${calc_pid:-}" ]] && kill -KILL "$calc_pid" 2>/dev/null || true
    [[ -n "${demo_pid:-}" ]] && kill "$demo_pid" 2>/dev/null || true
    [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$runtime"
}
trap cleanup EXIT

cargo build --release --workspace --bins --examples >&2

echo "== binary sizes (release, stripped) =="
printf '%-14s %10s\n' binary bytes
for b in nitro-server nitro-shot nitro-demo nitro-calc nitro-term nitro-bar nitro-launcher nitro-wallpaper nitro-settings hey; do
    printf '%-14s %10s\n' "$b" "$(stat -c %s "target/release/$b")"
done
printf '%-14s %10s\n' hello_client "$(stat -c %s target/release/examples/hello_client)"

# A private runtime dir keeps the sockets out of the developer's session:
# this must not disturb a `just fake` that happens to be running.
export XDG_RUNTIME_DIR="$runtime"
export NITRO_BACKEND=fake
export NITRO_FAKE_SIZE=1280x720
export NITRO_LOG=warn

target/release/nitro-server &
server_pid=$!
for _ in $(seq 100); do
    [[ -S "$runtime/nitro/wire.sock" ]] && break
    sleep 0.1
done
if [[ ! -S "$runtime/nitro/wire.sock" ]]; then
    echo "server never created its wire socket" >&2
    exit 1
fi

target/release/nitro-demo --follow --windows "$windows" --seconds 3 >/dev/null &
demo_pid=$!
# The calculator is the app half of the budget: a real toolkit client,
# with its widget tree built and its introspection socket bound. It is
# given its own apps dir so `hey` here cannot find the developer's.
NITRO_APPS_DIR="$runtime/apps" target/release/nitro-calc >/dev/null 2>&1 &
calc_pid=$!
sleep 2

field() { grep -m1 "^$2:" "/proc/$1/status" 2>/dev/null | awk '{print $2 " " $3}'; }

echo
echo "== resident memory (fake backend, ${windows} window(s), after 2 s) =="
printf '%-14s %12s %12s\n' process VmRSS VmHWM
printf '%-14s %12s %12s\n' nitro-server "$(field "$server_pid" VmRSS)" "$(field "$server_pid" VmHWM)"
printf '%-14s %12s %12s\n' nitro-demo "$(field "$demo_pid" VmRSS)" "$(field "$demo_pid" VmHWM)"
printf '%-14s %12s %12s\n' nitro-calc "$(field "$calc_pid" VmRSS)" "$(field "$calc_pid" VmHWM)"

echo
echo "== wire cost of the first frame =="
target/release/nitro-demo --follow --stats --seconds 1 | grep -E '^wire:' || true

# Stop the calculator through its own introspection socket, the way
# `nitro-shot --quit` stops the server, with a hard kill as the fallback.
# A plain SIGTERM is not enough on its own: a signal mask is inherited,
# so a shell that blocks SIGTERM hands the block to everything it starts
# and the `wait` below never returns.
NITRO_APPS_DIR="$runtime/apps" target/release/hey nitro-calc quit >/dev/null 2>&1 || true
kill -KILL "$calc_pid" 2>/dev/null || true
wait "$calc_pid" 2>/dev/null || true
calc_pid=
wait "$demo_pid" 2>/dev/null || true
demo_pid=
target/release/nitro-shot --quit >/dev/null 2>&1 || kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
server_pid=
