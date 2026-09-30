#!/usr/bin/env bash
# Runs inside the dist/alpine container: abuild as the builder user.
set -euo pipefail
# Hand /out back to the invoking user even when the build fails.
trap 'chown -R "$DIST_OUT_OWNER" /out' EXIT
work=/home/builder/build
mkdir -p "$work"
sed -e "s|@PKGVER@|$DIST_APK_PKGVER|" -e "s|@REV@|$DIST_REV|" /pkg/APKBUILD > "$work/APKBUILD"
cp /src/nitro.tar.gz "$work/"
chown -R builder "$work" /out
su builder -c "cd $work && abuild checksum && REPODEST=/out abuild -r -d"
# The public key the packages are signed with, for `apk add --keys-dir`.
cp /home/builder/.abuild/*.rsa.pub /out/
