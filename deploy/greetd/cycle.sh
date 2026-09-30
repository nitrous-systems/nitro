# One greetd login/logout cycle as `nitrotest` (docs/testbox.md "Login (greetd)").
# Copied to ~/nitro-stage on the box and run there as the box user; needs
# ~/nitro-stage/nitrotest.pw (never committed).
set -u
H=$(dirname "$(pgrep -a -x nitro-greeter | tail -1 | cut -d" " -f2)")/hey
G=$(getent passwd greeter >/dev/null && echo greeter || echo _greetd)
GU=$(id -u $G); TU=$(id -u nitrotest)
gp() { pgrep -u $G -x nitro-greeter | tail -1; }
g() { sudo -u $G env XDG_RUNTIME_DIR=/run/user/$GU $H nitro-greeter.$(gp) "$@"; }
sudo install -o $G -m 600 ~/nitro-stage/nitrotest.pw /run/user/$GU/.pw
pid0=$(systemctl show -p MainPID --value greetd)
# wait for greeter
for i in $(seq 100); do g get window/user value >/dev/null 2>&1 && break; sleep 0.1; done
g set window/user value nitrotest >/dev/null; g do window/user submit >/dev/null
for i in $(seq 50); do [ "$(g get window/prompt value 2>/dev/null)" != "-" ] && break; sleep 0.1; done
t0=$(date +%s.%N)
sudo -u $G sh -c "env XDG_RUNTIME_DIR=/run/user/$GU $H nitro-greeter.$(gp) set window/answer value \"\$(cat /run/user/$GU/.pw)\"" >/dev/null
g do window/answer submit >/dev/null
sudo rm -f /run/user/$GU/.pw
s=/run/user/$TU/nitro/session.sock
for i in $(seq 200); do sudo -u nitrotest test -S $s 2>/dev/null && break; sleep 0.05; done
st=$(printf 'status\n' | sudo -u nitrotest nc -q1 -U $s 2>&1 | tr '\n' ' ')
sleep 1
printf 'logout\n' | sudo -u nitrotest nc -q1 -U $s >/dev/null 2>&1
for i in $(seq 100); do g get window/user value >/dev/null 2>&1 && break; sleep 0.1; done
pid1=$(systemctl show -p MainPID --value greetd)
echo "greetd $pid0 $pid1 | $st"
