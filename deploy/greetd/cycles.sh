# N greetd cycles (deploy/greetd/cycle.sh), one docs/testbox.md table row each:
# | n | greetd pid before→after | greeter first_frame_ms | user first_frame_ms |
# | handoff ms (greeter exit → user server ready) | greeter session end | DRM errors | status |
N=${1:-10}
for n in $(seq $N); do
  since=$(date '+%Y-%m-%d %H:%M:%S.%N')
  out=$(bash ~/nitro-stage/cycle.sh 2>&1 | tail -1)
  sleep 1
  j=$(journalctl -b --since "$since" --no-pager -o short-unix SYSLOG_IDENTIFIER=nitro-session + SYSLOG_IDENTIFIER=nitro-greeter-session)
  ho=$(echo "$j" | grep 'nitro-greeter-session.*handed off' | head -1 | awk '{print $1}')
  ur=$(echo "$j" | grep 'nitro-session\[.*server ready in' | head -1 | awk '{print $1}')
  uff=$(echo "$j" | grep 'nitro-session\[.*first frame' | head -1 | sed 's/.*first frame \([0-9]*\) ms.*/\1/')
  gff=$(echo "$j" | grep 'nitro-greeter-session\[.*first frame' | tail -1 | sed 's/.*first frame \([0-9]*\) ms.*/\1/')
  gend=$(echo "$j" | grep 'nitro-greeter-session.*session ended' | head -1 | sed 's/.*session ended: //')
  drm=$(echo "$j" | grep -iE 'drm master|libseat.*(fail|error)|permission denied' | grep -vc mesa_shader_cache)
  hms=$( [ -n "$ho" ] && [ -n "$ur" ] && echo "$ur $ho" | awk '{printf "%.0f", ($1-$2)*1000}')
  echo "| $n | $(echo $out | awk '{print $2"→"$3}') | ${gff:-?} | ${uff:-?} | ${hms:-?} | ${gend:-?} | $drm | $(echo "$out" | grep -q 'ok nitro-server' && echo ok || echo FAIL) |"
done
