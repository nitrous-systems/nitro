#!/bin/sh
# Smoke test, run by `just dist-test-<distro>` in a fresh container after
# the distro's install.sh has installed the package. POSIX sh: Alpine has
# no bash. NITRO_BINS is the justfile's install_bins.
set -eu

fail() { echo "smoke: FAIL: $*" >&2; exit 1; }

[ -n "${NITRO_BINS:-}" ] || fail "NITRO_BINS not set"
for b in $NITRO_BINS; do
    [ -x "/usr/bin/$b" ] || fail "/usr/bin/$b missing"
done
[ -f /etc/pam.d/nitro-lock ] || fail "/etc/pam.d/nitro-lock missing"
for f in nitro-term.desktop nitro-mimeapps.list; do
    [ -f "/usr/share/applications/$f" ] || fail "/usr/share/applications/$f missing"
done

# Every shared library the binaries link is installed: this is what
# proves the package's runtime dependencies are complete.
missing=$(for b in $NITRO_BINS; do ldd "/usr/bin/$b" 2>&1 | grep 'not found' | sed "s|^|$b: |"; done || true)
[ -z "$missing" ] || fail "unresolved libraries:
$missing"

# `hey` with no running apps prints its usage and exits 1. It must get
# as far as printing the usage, which proves the binary loads and runs.
out=$(/usr/bin/hey --help 2>&1 || true)
case $out in
    *"usage: hey"*) ;;
    *) fail "hey --help did not print its usage: $out" ;;
esac

echo "smoke: ok: $(echo $NITRO_BINS | wc -w) binaries, pam.d, desktop entries, all libraries resolved"
