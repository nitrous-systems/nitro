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
#     not actually bind: it applies to that empty cgroup while the scope
#     the processes are in inherits `max`. See docs/testbox.md and the
#     comment beside the setting in deploy/nitro-dev.service.
#   * Idle CPU is measured as **jiffies over an interval**, not as `top`'s
#     instantaneous percentage. The claim in DESIGN.md is "0.0 %", and the
#     only way to show that honestly is to read utime+stime before and
#     after a fixed wait and print the difference — a process that used
#     three jiffies in sixty seconds is not 0.0 % and should not be able
#     to round to it.
#   * **RssAnon and RssFile are split out** (#538), because only one of
#     them is the server's doing. `RssFile` is the binary's own text and
#     rodata plus libc, libinput, libxkbcommon, libglib and friends —
#     page-cache pages the kernel maps, shared between every process that
#     maps the same file, and reclaimable under pressure. It is a constant
#     of what the binary links, it does not move when a window opens, and
#     summing it across the tree double-counts the shared pages.
#     `RssAnon` is the heap and the shadow buffer: private, unreclaimable
#     on a box with no swap, and the only half that scales with what the
#     user is doing. A budget written against `VmRSS` alone cannot tell
#     "we allocated a megabyte" from "we linked another library".
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

printf '%-16s %8s %10s %10s %10s %10s %10s %8s\n' \
    process pid VmRSS RssAnon RssFile VmHWM threads "cpu%"
total_rss=0
total_anon=0
total_file=0
for p in "${pids[@]}"; do
    [[ -d /proc/$p ]] || continue
    after=$(jiffies "$p")
    used=$(( after - ${before[$p]:-0} ))
    pct=$(awk -v j="$used" -v hz="$hz" -v s="$secs" 'BEGIN{printf "%.2f", 100*j/hz/s}')
    rss=$(field "$p" VmRSS)
    anon=$(field "$p" RssAnon)
    file=$(field "$p" RssFile)
    hwm=$(field "$p" VmHWM)
    if [[ $(name_of "$p") != "(sd-pam)" ]]; then
        total_rss=$(( total_rss + ${rss:-0} ))
        total_anon=$(( total_anon + ${anon:-0} ))
        total_file=$(( total_file + ${file:-0} ))
    fi
    printf '%-16s %8s %8s kB %8s kB %8s kB %8s kB %10s %8s\n' \
        "$(name_of "$p")" "$p" "${rss:-?}" "${anon:-?}" "${file:-?}" \
        "${hwm:-?}" "$(field "$p" Threads)" "$pct"
done
printf '%-16s %8s %8s kB %8s kB %8s kB   (nitro processes only)\n' \
    TOTAL "" "$total_rss" "$total_anon" "$total_file"

# The file-backed half is shared: the five processes map the same libc,
# and `nitro-bar`/`nitro-launcher`/`nitro-wallpaper` are three copies of
# very nearly the same `nitro-ui` text. Summing RssFile over the tree
# therefore counts those pages once per process, so the TOTAL above is an
# upper bound on what the desktop actually costs the box. It is printed
# anyway because the per-process split is what the budget is written
# against, and because the *anon* column — the half that really is
# private, and the only half that scales with what the user is doing — is
# additive and is the number `docs/budget.md` builds its line from.
