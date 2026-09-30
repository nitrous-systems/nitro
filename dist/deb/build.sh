#!/usr/bin/env bash
# Runs inside the dist/deb container: unpack the source, add the debian/
# directory, generate the changelog and build the binary package into /out.
set -euo pipefail
. /etc/os-release
work=/build
mkdir -p "$work"
tar -xzf /src/nitro.tar.gz -C "$work"
cd "$work/nitro"
cp -r /pkg/debian debian
# The version lives in Cargo.toml; the changelog is generated from it.
sed -e "s|@VERSION@|$DIST_DEB_VERSION|" \
    -e "s|@REV@|$DIST_REV|" \
    -e "s|@DIST@|${VERSION_CODENAME:-unstable}|" \
    -e "s|@DATE@|$(date -R)|" \
    /pkg/changelog.in > debian/changelog
dpkg-buildpackage -b -us -uc
cp ../*.deb ../*.buildinfo ../*.changes /out/
chown -R "$DIST_OUT_OWNER" /out
