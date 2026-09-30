set shell := ["bash", "-euo", "pipefail", "-c"]

# The development and test-box recipes (fake, icons-import, size,
# footprint, deploy*, box-*, shot, bench*) live in deploy/dev.just. Their
# names are unchanged (`just deploy`, `just bench`, …), and they run from
# this directory.
import 'deploy/dev.just'

# A bare `just` runs the checks (the recipes are at the end of this file).
[doc("Run the standard checks: fmt, build, test, lint-colors")]
[group('check')]
default: fmt build test lint-colors

# ---------------------------------------------------------------------------
# Install (see docs/install.md)
# ---------------------------------------------------------------------------
# These follow the GNU/packager conventions. Each variable can be set on
# the command line (`just PREFIX=$HOME/.local install`) or in the
# environment (`PREFIX=/usr just install`). DESTDIR is the staging root
# that package builders use; it is empty for a direct install, and it is
# never written into an installed file.
PREFIX     := env_var_or_default("PREFIX", "/usr/local")
DESTDIR    := env_var_or_default("DESTDIR", "")
BINDIR     := env_var_or_default("BINDIR", PREFIX / "bin")
LIBDIR     := env_var_or_default("LIBDIR", PREFIX / "lib")
DATADIR    := env_var_or_default("DATADIR", PREFIX / "share")
SYSCONFDIR := env_var_or_default("SYSCONFDIR", if PREFIX == "/usr" { "/etc" } else { PREFIX / "etc" })
# How to get write access to the destination. cargo always runs as you;
# only the commands that write under $DESTDIR$PREFIX (install, rm, …) are
# prefixed. `auto`: no prefix when the destination is writable (or you are
# root), otherwise `sudo` if it is on PATH, else `doas`, else an error.
# `SUDO=doas` / `SUDO=sudo` forces one; `SUDO=none` (or empty) never
# escalates.
SUDO       := env_var_or_default("SUDO", "auto")

# Shared bash for the install recipes, pasted in as `{{_asroot}}`.
# `asroot_for DIR` sets the array SU to the prefix needed to write under
# DIR (checked on its first existing ancestor); `asroot_force` picks one
# whenever we are not root (for apparmor_parser). Use as "${SU[@]}" cmd.
_asroot := "asroot_mode='" + SUDO + "'\n" + '''
asroot_force() {
    SU=()
    [[ $(id -u) -eq 0 ]] && return
    case $asroot_mode in
        ''|none) ;;
        auto)
            if command -v sudo >/dev/null; then SU=(sudo)
            elif command -v doas >/dev/null; then SU=(doas)
            else echo "just: $1 needs root; run as root, set PREFIX to a directory you own, or install sudo/doas" >&2; exit 1
            fi ;;
        *) SU=("$asroot_mode") ;;
    esac
}
asroot_for() {
    local d=$1
    while [[ ! -e $d ]]; do d=$(dirname "$d"); done
    if [[ -w $d ]]; then SU=(); else asroot_force "$1"; fi
}
'''

# The shipped binaries. This is `box_bins` (deploy/dev.just) minus
# nitro-demo and nitro-bench, and without the examples. Those are
# development and measurement tools: nitro-bench restarts and drives a
# live server, and nitro-demo is a damage/frame test client. The
# launcher lists nitro-demo only when it is present, so leaving it out
# costs nothing. nitro-session finds the rest next to itself in $BINDIR
# (crates/nitro-session/src/pieces.rs), so they are installed as one set.
install_bins := "nitro-server nitro-gpu-vulkan nitro-session nitro-shot nitro-calc nitro-amp nitro-term nitro-files nitro-bar nitro-launcher nitro-wallpaper nitro-settings nitro-video nitro-greeter nitro-auth hey"

# Chromium on nitro (#3865). A **release, non-component** build of the
# `nitro-ozone` branch in the Chromium checkout. Neither install-chromium
# nor deploy-chromium builds it (that needs `cr-env.sh` and about an hour;
# see docs/chromium-build.md), and both fail clearly if it is missing.
chromium_out := env_var_or_default("NITRO_CHROMIUM_OUT", "/home/kaspar/src/ai/chromium/src/out/Nitro")
# What `chrome` needs at run time, from `gn desc out/Nitro //chrome:chrome
# runtime_deps` filtered to what the software path loads. libEGL/libGLESv2
# and SwiftShader are listed because `--disable-gpu` still probes them
# at startup; without them chrome logs errors but runs.
# `chrome` itself is copied separately, stripped.
chromium_files := "chrome_crashpad_handler chrome_100_percent.pak chrome_200_percent.pak resources.pak icudtl.dat v8_context_snapshot.bin snapshot_blob.bin libEGL.so libGLESv2.so libvk_swiftshader.so vk_swiftshader_icd.json libvulkan.so.1 locales resources"

# Binaries, then launcher entries. Chromium is optional and external, so
# it is not included: run `just install install-chromium` for it.
#
# There is no display-manager session entry (wayland-sessions/ or
# xsessions/). nitro is neither a Wayland nor an X compositor, and it
# needs a VT and DRM master. Start it from greetd or from tty1 with
# `exec nitro-session` (docs/greeter.md, docs/install.md).
[doc("Install binaries and launcher entries (PREFIX, DESTDIR, BINDIR, DATADIR)")]
install: install-bins install-desktop

# Release-build the shipped binaries and install them into $DESTDIR$BINDIR.
[doc("Release-build the shipped binaries and install them into $DESTDIR$BINDIR")]
install-bins:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_asroot}}
    if [[ $(id -u) -eq 0 && -n ${SUDO_USER:-} ]]; then
        echo "install-bins: note: cargo runs as root; plain \`just install\` builds as you and uses sudo/doas only to copy" >&2
    fi
    cargo build --release --workspace --bins
    asroot_for '{{DESTDIR}}{{BINDIR}}'
    for b in {{install_bins}}; do
        "${SU[@]}" install -Dm755 "target/release/$b" '{{DESTDIR}}{{BINDIR}}'"/$b"
    done
    # The lock screen's PAM service (deploy/pam.d/nitro-lock). An existing
    # one is the admin's and is left alone.
    pam='{{DESTDIR}}{{SYSCONFDIR}}/pam.d/nitro-lock'
    if [[ -e $pam ]]; then
        echo "install-bins: keeping existing $pam"
    else
        asroot_for '{{DESTDIR}}{{SYSCONFDIR}}'
        "${SU[@]}" install -Dm644 deploy/pam.d/nitro-lock "$pam"
    fi

# deploy/*.desktop → $DESTDIR$DATADIR/applications/.
#
# Run it *after* install-bins (as `install` does). The entries' `Exec=` is
# a bare name. It resolves because nitro-session prepends its own
# directory ($BINDIR) to every child's PATH, and an entry shadows the
# launcher's built-in one for the same program. So entries next to old
# binaries (a pre-#3723 nitro-session) break launching, while new
# binaries without entries are harmless. See the long comment in
# `deploy-bins` (deploy/dev.just). The glob does not include
# deploy/chromium/chromium-nitro.desktop; install-chromium installs that.
# deploy/nitro-mimeapps.list (audio → nitro-amp, video → nitro-video,
# honoured under XDG_CURRENT_DESKTOP=nitro) goes next to the entries; a
# user's ~/.config/mimeapps.list still outranks it and is never touched.
[doc("Install deploy/*.desktop and nitro-mimeapps.list into $DESTDIR$DATADIR/applications (after install-bins)")]
install-desktop:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_asroot}}
    asroot_for '{{DESTDIR}}{{DATADIR}}'
    for f in deploy/*.desktop deploy/nitro-mimeapps.list; do
        "${SU[@]}" install -Dm644 "$f" '{{DESTDIR}}{{DATADIR}}/applications/'"$(basename "$f")"
    done

# The nitro-ozone Chromium build → $LIBDIR/nitro/chromium/, plus the
# `chromium-nitro` wrapper in $BINDIR, the icons and the launcher entry.
#
# No packages are installed from here. The runtime deps for packagers
# are the usual Chromium set (nss, nspr, cups, dbus, expat, gbm, drm,
# xkbcommon, glib, pango/cairo, alsa, udev) plus the accessibility libs
# that a desktop-less system lacks: libatk1.0, libatk-bridge2.0,
# libatspi2.0 (Debian: libatk1.0-0t64 libatk-bridge2.0-0t64
# libatspi2.0-0t64). `ldd chrome | grep 'not found'` lists what is missing.
# The sandbox needs the AppArmor profile on Ubuntu: `just install-apparmor`.
[doc("Install the nitro-ozone Chromium build (NITRO_CHROMIUM_OUT) into $LIBDIR/nitro/chromium")]
install-chromium:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_asroot}}
    out='{{chromium_out}}'
    if [[ ! -x $out/chrome ]]; then
        echo "install-chromium: no $out/chrome — build nitro-ozone Chromium first (set NITRO_CHROMIUM_OUT; see docs/chromium-build.md)" >&2
        exit 1
    fi
    src=$(cd "$out/../.." && pwd)
    lib='{{DESTDIR}}{{LIBDIR}}/nitro/chromium'
    bindir='{{DESTDIR}}{{BINDIR}}'
    datadir='{{DESTDIR}}{{DATADIR}}'
    asroot_for "$lib"; su_lib=("${SU[@]}")
    asroot_for "$bindir"; su_bin=("${SU[@]}")
    asroot_for "$datadir"; su_data=("${SU[@]}")
    # `symbol_level=0` still leaves a ~200 MB `.symtab`/`.strtab`; strip a
    # copy rather than touching the build (shared with deploy-chromium).
    stage=target/chromium-stage
    mkdir -p "$stage"
    if [[ ! $stage/chrome -nt $out/chrome ]]; then
        strip -o "$stage/chrome" "$out/chrome"
    fi
    "${su_lib[@]}" install -Dm755 "$stage/chrome" "$lib/chrome"
    # The directories are replaced as a whole, so a file dropped from the
    # build goes with them. `*.info` are build-time translation manifests.
    "${su_lib[@]}" rm -rf "$lib/locales" "$lib/resources"
    for f in {{chromium_files}}; do
        case $f in
            locales|resources)
                (cd "$out" && "${su_lib[@]}" find "$f" -type f ! -name '*.info' -exec install -Dm644 {} "$lib/{}" \;) ;;
            chrome_crashpad_handler|*.so|*.so.*)
                "${su_lib[@]}" install -Dm755 "$out/$f" "$lib/$f" ;;
            *)
                "${su_lib[@]}" install -Dm644 "$out/$f" "$lib/$f" ;;
        esac
    done
    # The wrapper gets the installed path, without DESTDIR.
    sed 's|@CHROMIUM_DIR@|{{LIBDIR}}/nitro/chromium|' deploy/chromium/chromium-nitro > "$stage/chromium-nitro.install"
    "${su_bin[@]}" install -Dm755 "$stage/chromium-nitro.install" "$bindir/chromium-nitro"
    for n in 16 24 48 64 128 256; do
        "${su_data[@]}" install -Dm644 "$src/chrome/app/theme/chromium/product_logo_$n.png" \
            "$datadir/icons/hicolor/${n}x${n}/apps/chromium-nitro.png"
    done
    # The entry goes **last**: a failure above leaves no launcher entry
    # pointing at a half-installed browser.
    "${su_data[@]}" install -Dm644 deploy/chromium/chromium-nitro.desktop "$datadir/applications/chromium-nitro.desktop"

# The AppArmor profile that lets the installed chrome use its sandbox,
# → $DESTDIR$SYSCONFDIR/apparmor.d/chromium-nitro. Opt-in. It loads the
# profile only for a direct install (DESTDIR empty); a package does
# `apparmor_parser -r` in its postinst.
[doc("Install (and, without DESTDIR, load) the Chromium AppArmor profile into $SYSCONFDIR")]
install-apparmor:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_asroot}}
    dest='{{DESTDIR}}{{SYSCONFDIR}}/apparmor.d/chromium-nitro'
    asroot_for "$dest"
    mkdir -p target/chromium-stage
    sed 's|@CHROME@|{{LIBDIR}}/nitro/chromium/chrome|' deploy/chromium/apparmor-chromium-nitro > target/chromium-stage/apparmor-chromium-nitro.install
    "${SU[@]}" install -Dm644 target/chromium-stage/apparmor-chromium-nitro.install "$dest"
    if [[ -z '{{DESTDIR}}' ]] && command -v apparmor_parser >/dev/null; then
        asroot_force "$dest"
        "${SU[@]}" apparmor_parser -r "$dest"
    else
        echo "install-apparmor: installed $dest; not loaded (run: apparmor_parser -r {{SYSCONFDIR}}/apparmor.d/chromium-nitro)"
    fi

# Remove what the install targets put down. The AppArmor profile stays;
# remove {{SYSCONFDIR}}/apparmor.d/chromium-nitro by hand if you want it gone.
[doc("Remove what the install recipes installed (except the AppArmor profile)")]
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail
    {{_asroot}}
    asroot_for '{{DESTDIR}}{{BINDIR}}'; su_bin=("${SU[@]}")
    asroot_for '{{DESTDIR}}{{DATADIR}}'; su_data=("${SU[@]}")
    asroot_for '{{DESTDIR}}{{LIBDIR}}'; su_lib=("${SU[@]}")
    for b in {{install_bins}} chromium-nitro; do
        "${su_bin[@]}" rm -f '{{DESTDIR}}{{BINDIR}}'"/$b"
    done
    asroot_for '{{DESTDIR}}{{SYSCONFDIR}}'; "${SU[@]}" rm -f '{{DESTDIR}}{{SYSCONFDIR}}/pam.d/nitro-lock'
    for f in deploy/*.desktop deploy/nitro-mimeapps.list deploy/chromium/chromium-nitro.desktop; do
        "${su_data[@]}" rm -f '{{DESTDIR}}{{DATADIR}}/applications/'"$(basename "$f")"
    done
    for n in 16 24 48 64 128 256; do
        "${su_data[@]}" rm -f '{{DESTDIR}}{{DATADIR}}'"/icons/hicolor/${n}x${n}/apps/chromium-nitro.png"
    done
    "${su_lib[@]}" rm -rf '{{DESTDIR}}{{LIBDIR}}/nitro/chromium'
    if [[ -d '{{DESTDIR}}{{LIBDIR}}/nitro' ]]; then
        "${su_lib[@]}" rmdir --ignore-fail-on-non-empty '{{DESTDIR}}{{LIBDIR}}/nitro'
    fi

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

[doc("Check formatting (cargo fmt --check)")]
[group('check')]
fmt:
    cargo fmt --all -- --check

[doc("Build the whole workspace, all targets")]
[group('check')]
build:
    cargo build --workspace --all-targets

[doc("Run clippy on the workspace with warnings as errors")]
[group('check')]
clippy: lint-colors
    cargo clippy --workspace --all-targets -- -D warnings

# Fail on a hard-coded colour outside the palette. See the script's
# header and docs/theme.md: colours come from roles, so that one
# `theme.scheme` switch moves the whole desktop.
[doc("Fail on hard-coded colours outside the theme palette")]
[group('check')]
lint-colors:
    bash deploy/lint-colors.sh

# Everything a merge checks that is not a compile or a test.
[doc("Everything a merge checks that is not a compile or a test")]
[group('check')]
lint: lint-colors clippy

[doc("Run the workspace tests")]
[group('check')]
test:
    cargo test --workspace
