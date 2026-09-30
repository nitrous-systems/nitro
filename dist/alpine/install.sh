#!/bin/sh
# Install the built package in a fresh container (just dist-test-alpine).
set -eu
cp /out/*.rsa.pub /etc/apk/keys/
apk add --no-cache "$(find /out -name 'nitro-[0-9]*.apk' | head -n1)"
apk info -L nitro
