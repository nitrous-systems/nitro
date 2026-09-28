set shell := ["bash", "-euo", "pipefail", "-c"]

# The development and test-box recipes (fake, icons-import, size, deploy*,
# box-*, shot, bench*) live in deploy/dev.just. Their names are unchanged
# (`just deploy`, `just bench`, …), and they run from this directory.
import 'deploy/dev.just'

# A bare `just` runs the checks (the recipes are at the end of this file).
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

# The shipped binaries. This is `box_bins` (deploy/dev.just) minus
# nitro-demo and nitro-bench, and without the examples. Those are
# development and measurement tools: nitro-bench restarts and drives a
# live server, and nitro-demo is a damage/frame test client. The
# launcher lists nitro-demo only when it is present, so leaving it out
# costs nothing. nitro-session finds the rest next to itself in $BINDIR
# (crates/nitro-session/src/pieces.rs), so they are installed as one set.
install_bins := "nitro-server nitro-session nitro-shot nitro-calc nitro-amp nitro-term nitro-files nitro-bar nitro-launcher nitro-wallpaper nitro-settings hey"

# Chromium on nitro (#3865). A **release, non-component** build of the
# `nitro-ozone` branch in the Chromium checkout. Neither install-chromium
# nor deploy-chromium builds it (that needs `cr-env.sh` and about an hour;
# see tmp/chromium-build.md), and both fail clearly if it is missing.
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
    cargo build --release --workspace --bins
    for b in {{install_bins}}; do
        install -Dm755 "target/release/$b" '{{DESTDIR}}{{BINDIR}}'"/$b"
    done

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
[doc("Install deploy/*.desktop into $DESTDIR$DATADIR/applications (after install-bins)")]
install-desktop:
    #!/usr/bin/env bash
    set -euo pipefail
    for f in deploy/*.desktop; do
        install -Dm644 "$f" '{{DESTDIR}}{{DATADIR}}/applications/'"$(basename "$f")"
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
    out='{{chromium_out}}'
    if [[ ! -x $out/chrome ]]; then
        echo "install-chromium: no $out/chrome — build nitro-ozone Chromium first (set NITRO_CHROMIUM_OUT; see tmp/chromium-build.md)" >&2
        exit 1
    fi
    src=$(cd "$out/../.." && pwd)
    lib='{{DESTDIR}}{{LIBDIR}}/nitro/chromium'
    bindir='{{DESTDIR}}{{BINDIR}}'
    datadir='{{DESTDIR}}{{DATADIR}}'
    # `symbol_level=0` still leaves a ~200 MB `.symtab`/`.strtab`; strip a
    # copy rather than touching the build (shared with deploy-chromium).
    stage=target/chromium-stage
    mkdir -p "$stage"
    if [[ ! $stage/chrome -nt $out/chrome ]]; then
        strip -o "$stage/chrome" "$out/chrome"
    fi
    install -Dm755 "$stage/chrome" "$lib/chrome"
    # The directories are replaced as a whole, so a file dropped from the
    # build goes with them. `*.info` are build-time translation manifests.
    rm -rf "$lib/locales" "$lib/resources"
    for f in {{chromium_files}}; do
        case $f in
            locales|resources)
                (cd "$out" && find "$f" -type f ! -name '*.info' -exec install -Dm644 {} "$lib/{}" \;) ;;
            chrome_crashpad_handler|*.so|*.so.*)
                install -Dm755 "$out/$f" "$lib/$f" ;;
            *)
                install -Dm644 "$out/$f" "$lib/$f" ;;
        esac
    done
    # The wrapper gets the installed path, without DESTDIR.
    mkdir -p "$bindir"
    sed 's|@CHROMIUM_DIR@|{{LIBDIR}}/nitro/chromium|' deploy/chromium/chromium-nitro > "$stage/chromium-nitro.install"
    install -Dm755 "$stage/chromium-nitro.install" "$bindir/chromium-nitro"
    for n in 16 24 48 64 128 256; do
        install -Dm644 "$src/chrome/app/theme/chromium/product_logo_$n.png" \
            "$datadir/icons/hicolor/${n}x${n}/apps/chromium-nitro.png"
    done
    # The entry goes **last**: a failure above leaves no launcher entry
    # pointing at a half-installed browser.
    install -Dm644 deploy/chromium/chromium-nitro.desktop "$datadir/applications/chromium-nitro.desktop"

# The AppArmor profile that lets the installed chrome use its sandbox,
# → $DESTDIR$SYSCONFDIR/apparmor.d/chromium-nitro. Opt-in. It loads the
# profile only for a direct install (DESTDIR empty); a package does
# `apparmor_parser -r` in its postinst.
[doc("Install (and, without DESTDIR, load) the Chromium AppArmor profile into $SYSCONFDIR")]
install-apparmor:
    #!/usr/bin/env bash
    set -euo pipefail
    dest='{{DESTDIR}}{{SYSCONFDIR}}/apparmor.d/chromium-nitro'
    mkdir -p target/chromium-stage
    sed 's|@CHROME@|{{LIBDIR}}/nitro/chromium/chrome|' deploy/chromium/apparmor-chromium-nitro > target/chromium-stage/apparmor-chromium-nitro.install
    install -Dm644 target/chromium-stage/apparmor-chromium-nitro.install "$dest"
    if [[ -z '{{DESTDIR}}' ]] && command -v apparmor_parser >/dev/null; then
        apparmor_parser -r "$dest"
    else
        echo "install-apparmor: installed $dest; not loaded (run: apparmor_parser -r {{SYSCONFDIR}}/apparmor.d/chromium-nitro)"
    fi

# Remove what the install targets put down. The AppArmor profile stays;
# remove {{SYSCONFDIR}}/apparmor.d/chromium-nitro by hand if you want it gone.
[doc("Remove what the install recipes installed (except the AppArmor profile)")]
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail
    for b in {{install_bins}} chromium-nitro; do
        rm -f '{{DESTDIR}}{{BINDIR}}'"/$b"
    done
    for f in deploy/*.desktop deploy/chromium/chromium-nitro.desktop; do
        rm -f '{{DESTDIR}}{{DATADIR}}/applications/'"$(basename "$f")"
    done
    for n in 16 24 48 64 128 256; do
        rm -f '{{DESTDIR}}{{DATADIR}}'"/icons/hicolor/${n}x${n}/apps/chromium-nitro.png"
    done
    rm -rf '{{DESTDIR}}{{LIBDIR}}/nitro/chromium'
    if [[ -d '{{DESTDIR}}{{LIBDIR}}/nitro' ]]; then
        rmdir --ignore-fail-on-non-empty '{{DESTDIR}}{{LIBDIR}}/nitro'
    fi

# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

[group('check')]
fmt:
    cargo fmt --all -- --check

[group('check')]
build:
    cargo build --workspace --all-targets

[group('check')]
clippy: lint-colors
    cargo clippy --workspace --all-targets -- -D warnings

# Fail on a hard-coded colour outside the palette. See the script's
# header and docs/theme.md: colours come from roles, so that one
# `theme.scheme` switch moves the whole desktop.
[group('check')]
lint-colors:
    bash deploy/lint-colors.sh

# Everything a merge checks that is not a compile or a test.
[group('check')]
lint: lint-colors clippy

[group('check')]
test:
    cargo test --workspace
