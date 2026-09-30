#!/usr/bin/env bash
# Runs inside the dist/arch container: makepkg as the builder user.
set -euo pipefail
work=/home/builder/build
install -d -o builder "$work" /out/pkg
sed "s|@PKGVER@|$DIST_ARCH_PKGVER|" /pkg/PKGBUILD > "$work/PKGBUILD"
cp /src/nitro.tar.gz "$work/"
chown -R builder "$work" /out
su builder -c "cd $work && PKGDEST=/out makepkg --noconfirm --nodeps"
rmdir /out/pkg
chown -R "$DIST_OUT_OWNER" /out
