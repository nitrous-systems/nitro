#!/bin/sh
# Install the built .deb in a fresh container (just dist-test-*).
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq /out/nitro_*.deb
dpkg -L nitro
