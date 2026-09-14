#!/usr/bin/env bash
# The M3 desktop's process table: RSS, high-water mark and idle CPU for
# every process in the nitro-dev cgroup. Run on the box (`just box-ps`).
#
# Two things it is careful about, both learned the hard way:
#
#   * The processes are found by **walking down from the unit's MainPID**,
#     not by `pgrep nitro`. A pattern match would also catch a stray
#     `nitro-shot` from a screenshot, an orphan from yesterday, or an
#     editor with the word in its command line; what matters is "what
#     does the running desktop cost", which is exactly the session's own
#     children.
#
#     The unit's *cgroup* would have been the obvious source and is not
#     usable: `PAMName=login` makes pam_systemd move the whole tree into
#     the logind session scope (`user-1000.slice/session-N.scope`), so
#     `system.slice/nitro-dev.service/cgroup.procs` is **empty** while
#     the desktop runs. That is also why the unit's `MemoryMax=1G` does
#     not actually bind — see docs/testbox.md.
#   * Idle CPU is measured as **jiffies over an interval**, not as `top`'s
#     instantaneous percentage. The claim in DESIGN.md is "0.0 %", and the
#     only way to show that honestly is to read utime+stime before and
#     after a fixed wait and print the difference — a process that used
#     three jiffies in sixty seconds is not 0.0 % and should not be able
#     to round to it.
#
# Usage: box-ps.sh [SECONDS]   (default 60)
set -euo pipefail

secs="${1:-60}"
hz=$(getconf CLK_TCK)

main=$(systemctl show nitro-dev -p MainPID --value)
if [[ -z $main || $main == 0 ]]; then
    echo "nitro-dev is not running" >&2
    exit 1
fi
# The session and its children. One level is enough: the session starts
# the four pieces directly and nothing else in the tree forks.
pids=("$main")
for c in $(cat "/proc/$main/task/$main/children" 2>/dev/null); do
    # `(sd-pam)` is pam_systemd's own helper, forked into the session by
    # `PAMName=login`. It is systemd's process, not the desktop's, so it
    # is listed for honesty but left out of the total.
    pids+=("$c")
done

name_of() { tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null | awk '{print $1}' | xargs -r basename; }
field()   { awk -v k="$2:" '$1==k {print $2}' "/proc/$1/status" 2>/dev/null; }
# utime + stime, in jiffies. Field 14 and 15 of /proc/<pid>/stat, counted
# after the comm field — which can itself contain spaces, so the tail is
# taken from the last ')'.
jiffies() {
    local s
    s=$(< "/proc/$1/stat") || return 0
    s=${s#*\) }
    awk '{print $12 + $13}' <<< "$s"
}

declare -A before
for p in "${pids[@]}"; do before[$p]=$(jiffies "$p"); done

echo "== nitro-dev tree: measuring ${secs}s of idle =="
sleep "$secs"

printf '%-16s %8s %10s %10s %10s %8s\n' process pid VmRSS VmHWM threads "cpu%"
total_rss=0
for p in "${pids[@]}"; do
    [[ -d /proc/$p ]] || continue
    after=$(jiffies "$p")
    used=$(( after - ${before[$p]:-0} ))
    pct=$(awk -v j="$used" -v hz="$hz" -v s="$secs" 'BEGIN{printf "%.2f", 100*j/hz/s}')
    rss=$(field "$p" VmRSS)
    hwm=$(field "$p" VmHWM)
    if [[ $(name_of "$p") != "(sd-pam)" ]]; then
        total_rss=$(( total_rss + ${rss:-0} ))
    fi
    printf '%-16s %8s %8s kB %8s kB %10s %8s\n' \
        "$(name_of "$p")" "$p" "${rss:-?}" "${hwm:-?}" "$(field "$p" Threads)" "$pct"
done
printf '%-16s %8s %8s kB   (nitro processes only)\n' TOTAL "" "$total_rss"
