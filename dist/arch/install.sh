#!/bin/sh
# Install the built package in a fresh container (just dist-test-arch).
set -eu
pacman -Syu --noconfirm >/dev/null
pacman -U --noconfirm /out/nitro-*.pkg.tar.zst
pacman -Ql nitro
