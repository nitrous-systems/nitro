#!/usr/bin/env bash
# Driver for the dist-* recipes (dist/dist.just, docs/packaging.md).
#
#   dist/run.sh src                         write the source tarball, print its path
#   dist/run.sh build <distro> <dir> <image>  build a package in a container
#   dist/run.sh test  <distro> <dir> <image>  install it in a fresh container
#
# <distro> names the output directory dist/out/<distro>/; <dir> is the
# packaging directory under dist/ (deb, arch, alpine); <image> is the base
# image. It runs from the repository root, as just recipes do.
#
# Environment:
#   DIST_ENGINE   podman or docker (default: podman if found, else docker)
#   DIST_RELEASE  1 = plain workspace version, no git snapshot suffix
#   DIST_DIRTY    1 = include uncommitted changes to tracked files
set -euo pipefail

die() { echo "dist: $*" >&2; exit 1; }

engine() {
    if [[ -n ${DIST_ENGINE:-} ]]; then
        command -v "$DIST_ENGINE" >/dev/null || die "DIST_ENGINE=$DIST_ENGINE not found"
        echo "$DIST_ENGINE"
    elif command -v podman >/dev/null; then echo podman
    elif command -v docker >/dev/null; then echo docker
    else die "neither podman nor docker found; install one or set DIST_ENGINE"
    fi
}

# Which uid owns the files the container writes to dist/out. Under a
# rootless engine container root *is* the invoking user, so it stays 0.
# Under rootful docker the files would be root's, so they are chowned
# to the invoking user.
out_owner() {
    local e=$1 rootless=false
    case $(basename "$e") in
        podman) rootless=$("$e" info --format '{{.Host.Security.Rootless}}' 2>/dev/null || echo true) ;;
        docker) "$e" info --format '{{.SecurityOptions}}' 2>/dev/null | grep -q rootless && rootless=true ;;
    esac
    if [[ $rootless == true ]]; then echo "0:0"; else echo "$(id -u):$(id -g)"; fi
}

# The version, from [workspace.package] in Cargo.toml. A snapshot adds
# the commit date and short rev; each packaging format spells that its
# own way (see docs/packaging.md).
version() {
    base=$(sed -n '/^\[workspace\.package\]/,/^\[/ s/^version *= *"\(.*\)"/\1/p' Cargo.toml)
    [[ -n $base ]] || die "no version in [workspace.package] of Cargo.toml"
    rev=$(git rev-parse --short HEAD)
    date=$(git log -1 --format=%cd --date=format:%Y%m%d)
    count=$(git rev-list --count HEAD)
    if [[ ${DIST_RELEASE:-} == 1 ]]; then
        src_version=$base deb_version=$base arch_pkgver=$base apk_pkgver=$base
    else
        src_version="$base+git$date.$rev"
        deb_version="$base+git$date.$rev"
        arch_pkgver="$base.r$count.g$rev"
        apk_pkgver="${base}_git$date"
    fi
}

# git archive of HEAD (or, with DIST_DIRTY=1, of HEAD plus the working
# tree's changes to tracked files) with the prefix nitro/.
make_src() {
    version
    local tree=HEAD
    if [[ ${DIST_DIRTY:-} == 1 ]]; then
        tree=$(git stash create) || true
        [[ -n $tree ]] || tree=HEAD
    fi
    mkdir -p dist/out/src
    src="dist/out/src/nitro-$src_version.tar.gz"
    git archive --format=tar.gz --prefix=nitro/ -o "$src" "$tree"
}

cmd=${1:-}
case $cmd in
    src)
        make_src
        echo "$src"
        ;;
    build)
        [[ $# -eq 4 ]] || die "usage: $0 build <distro> <dir> <image>"
        distro=$2 dir=$3 image=$4
        e=$(engine)
        make_src
        out="dist/out/$distro"
        rm -rf "$out"; mkdir -p "$out"
        tag="nitro-dist-$distro"
        echo "dist: $distro: building image $tag from $image with $e"
        "$e" build --pull=always -t "$tag" --build-arg "BASE=$image" "dist/$dir"
        echo "dist: $distro: building nitro $src_version"
        "$e" run --rm \
            -v "$PWD/$src:/src/nitro.tar.gz:ro,z" \
            -v "$PWD/dist/$dir:/pkg:ro,z" \
            -v "$PWD/$out:/out:z" \
            -e "DIST_DEB_VERSION=$deb_version" \
            -e "DIST_ARCH_PKGVER=$arch_pkgver" \
            -e "DIST_APK_PKGVER=$apk_pkgver" \
            -e "DIST_REV=$rev" \
            -e "DIST_OUT_OWNER=$(out_owner "$e")" \
            "$tag" bash /pkg/build.sh
        echo "dist: $distro: done:"
        find "$out" -type f | sort
        ;;
    test)
        [[ $# -eq 4 ]] || die "usage: $0 test <distro> <dir> <image>"
        distro=$2 dir=$3 image=$4
        e=$(engine)
        out="dist/out/$distro"
        [[ -d $out ]] || die "no $out; run the dist build for $distro first"
        bins=$("${JUST:-just}" --evaluate install_bins)
        echo "dist: $distro: installing into a fresh $image"
        "$e" run --rm --pull=always \
            -e "NITRO_BINS=$bins" \
            -v "$PWD/$out:/out:ro,z" \
            -v "$PWD/dist/$dir/install.sh:/install.sh:ro,z" \
            -v "$PWD/dist/smoke.sh:/smoke.sh:ro,z" \
            "$image" sh -c 'sh /install.sh && sh /smoke.sh'
        echo "dist: $distro: smoke test passed"
        ;;
    *)
        die "usage: $0 src | build <distro> <dir> <image> | test <distro> <dir> <image>"
        ;;
esac
