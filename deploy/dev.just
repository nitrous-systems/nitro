# ---------------------------------------------------------------------------
# Headless (the CI-able path)
# ---------------------------------------------------------------------------
fake_size := env_var_or_default("NITRO_FAKE_SIZE", "1280x720")

# Run the server locally on the fake backend (Ctrl-C to stop).
[doc("Run the server locally on the fake backend (Ctrl-C to stop)")]
[group('dev')]
fake:
    NITRO_BACKEND=fake NITRO_FAKE_SIZE={{fake_size}} cargo run -p nitro-server

# Grab a PNG from a locally running `just fake` server.
[doc("Grab a PNG from a locally running `just fake` server")]
[group('dev')]
fake-shot out="tmp/fake-shot.png":
    mkdir -p tmp && cargo run -q -p nitro-shot -- -o {{out}} && echo "wrote {{out}}"

# ---------------------------------------------------------------------------
# Icons (see docs/icons.md)
# ---------------------------------------------------------------------------

# Regenerate crates/nitro-icons/src/set.rs from the pinned upstream commit.
# `just icons-import gear house` also adds those two names to icons.txt.
# Idempotent: a run with no arguments must leave set.rs byte-identical.
[doc("Regenerate nitro-icons set.rs; names given are added to icons.txt")]
[group('dev')]
icons-import *names:
    bash deploy/icons-import.sh {{names}}

# ---------------------------------------------------------------------------
# Budget (see docs/budget.md)
# ---------------------------------------------------------------------------

# Binary sizes and RSS/HWM of every shipped binary, release build.
# `just size 5` measures the server with five windows open.
[doc("Binary sizes and RSS/HWM of the shipped binaries (arg: window count)")]
[group('dev')]
size windows="1":
    bash deploy/size.sh {{windows}}

# Every surface-tagged task pastes its delta against the baseline in
# docs/budget.md. `NITRO_FOOTPRINT_NO_BOX=1 just footprint` skips the box.
#
# Binary sizes, dep count and box idle RSS: the docs/budget.md baseline.
[doc("Binary sizes, dep count and box idle RSS: the docs/budget.md baseline")]
[group('dev')]
footprint secs="10":
    FOOTPRINT_BINS="{{box_bins}}" NITRO_BOX={{box}} NITRO_BOX_BINDIR='{{box_bindir}}' bash deploy/footprint.sh {{secs}}

# ---------------------------------------------------------------------------
# Test box (see docs/testbox.md)
# ---------------------------------------------------------------------------
#
# Two boxes (docs/testbox.md). `box` picks one, `box_profile` says how it
# runs nitro, and every box recipe follows both:
#
#   just deploy                          # box1, kaspar@192.168.1.204
#   just box=testhost2 deploy            # box2, the Kaby Lake laptop
#   NITRO_BOX=testhost2 just footprint   # the same, from the environment
#
# Profiles:
#   unit — box1. nitro runs as the `nitro-dev` system unit from
#          ~/nitro-bin, and `deploy` restarts it.
#   greetd — testhost2. greetd is the display manager (`box-greetd`,
#          docs/testbox.md "Login (greetd)"): nitro-greeter on tty1, and
#          the human logs in to /usr/local/bin/nitro-session. Binaries go
#          to /usr/local/bin through sudo; `deploy` does not restart
#          anything, because a restart ends the logged-in session.
# box1 (`unit`) runs greetd on tty1 as well, next to nitro-dev on tty2.
# The profile is guessed from the host name and can be forced with
# NITRO_BOX_PROFILE; NITRO_BOX_BINDIR overrides where the binaries are.
# Both boxes keep their clone at ~/src/ai/nitro.
box := env_var_or_default("NITRO_BOX", "kaspar@192.168.1.204")
box_profile := env_var_or_default("NITRO_BOX_PROFILE", if box =~ '(^|@)(testhost2|192\.168\.1\.193)$' { "greetd" } else { "unit" })
box_bindir := env_var_or_default("NITRO_BOX_BINDIR", if box_profile == "greetd" { "/usr/local/bin" } else { "~/nitro-bin" })
# Everything the box needs for a desktop. `nitro-session` is what the
# unit runs; it finds the other four next to itself in ~/nitro-bin, which
# is why they are deployed together and in one rsync — a session that
# started yesterday's bar next to today's server is the failure mode the
# sibling lookup exists to prevent.
box_bins := "nitro-server nitro-gpu-vulkan nitro-session nitro-shot nitro-demo nitro-bench nitro-calc nitro-amp nitro-term nitro-files nitro-bar nitro-launcher nitro-wallpaper nitro-settings nitro-video nitro-greeter nitro-auth hey"
box_examples := "hello_client hello_dialog shell_probe"

# Build release, rsync binaries to the box, restart the dev session
# (on a `greetd` box: install only; log out/in or `just box-restart`).
[doc("Build release, deploy to the box and restart the session (greetd: install only)")]
[group('dev')]
deploy: (deploy-bins)
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ {{box_profile}} == greetd ]]; then
        echo "installed on {{box}}; log out and in (or \`just box={{box}} box-restart\`) to pick it up"
    else
        ssh {{box}} 'sudo systemctl restart nitro-dev' && just box={{box}} box-status
    fi

# Restart the box's session. On a `greetd` box this restarts greetd,
# which ends the logged-in session and leaves the greeter.
[doc("Restart the box session (greetd box: restarts greetd, back to the greeter)")]
[group('dev')]
box-restart:
    ssh {{box}} {{ if box_profile == "greetd" { "'sudo systemctl restart greetd'" } else { "'sudo systemctl restart nitro-dev'" } }}

[doc("Build release and copy binaries + launcher entries to the box")]
[group('dev')]
deploy-bins:
    # `--examples` on its own does not build the binaries, so ask for both.
    cargo build --release --workspace --bins --examples
    just box={{box}} _deploy-bins-{{box_profile}}

# box1: everything into ~/nitro-bin, entries into ~/.local.
_deploy-bins-unit:
    cd target/release && rsync -az {{box_bins}} {{box}}:nitro-bin/
    cd target/release/examples && rsync -az {{box_examples}} {{box}}:nitro-bin/
    # `deploy/*.desktop` are installed too, and are part of the deployed
    # set exactly like `~/nitro-bin` — a worker who "restores the box as
    # found" must leave them (docs/testbox.md).
    #
    # They were deliberately *not* installed until #3723, and the reason
    # is worth keeping because it is what had to be fixed rather than
    # worked around: their `Exec=` is a bare name, as the freedesktop
    # spec asks and a packager needs, and `~/nitro-bin` is on nobody's
    # `PATH` — so `Exec=nitro-term` was an `execvp` that could only fail,
    # and because a `.desktop` file *shadows* the launcher's built-in
    # entry for the same program, installing one replaced a working
    # launcher entry with "spawn: No such file or directory".
    #
    # `nitro-session` now prepends its own executable's directory to the
    # `PATH` every child inherits (`crates/nitro-session/src/pieces.rs`),
    # which is the same sibling lookup it already used to *find* the
    # pieces, stated to the processes it starts. So the bare name
    # resolves, the shadowing is now the behaviour we want — one Terminal
    # entry, the packaged one — and the box gets what #3715 needs: an
    # `<app_id>.desktop` for the server to resolve `nitro-calc` →
    # `Icon=calculator` through, so the bar's window list and the title
    # bars show real application icons instead of the generic `window`.
    #
    # Idempotent: rsync over the same four basenames, and the launcher
    # and the server both key their indexes on the basename.
    #
    # **The files go last, and the order is deliberate.** A failure here
    # leaves the box with new binaries and no entries, which is the
    # benign half of the coupling below — the launcher falls back to its
    # built-ins and everything still runs. The other order could leave
    # entries next to old binaries, which is the half that breaks.
    #
    # ⚠ The files and the `PATH` prepend are **coupled, and the coupling
    # is one-directional**: measured on the box during #3723, these four
    # files installed next to a session that does *not* prepend (any
    # build before #3723) reproduce the original regression exactly —
    # launching Terminal from the launcher spawns nothing, because the
    # packaged entry shadows the built-in and its bare `Exec=` does not
    # resolve. The two halves therefore ship in one commit and must land
    # together; installing the files from this recipe while an older
    # `nitro-session` is deployed is the one way to get the old failure
    # back.
    #
    # `deploy/nitro-mimeapps.list` rides along: nitro's default
    # applications (audio → nitro-amp, video → nitro-video), read by
    # nitro-files only under XDG_CURRENT_DESKTOP=nitro and below the
    # user's own ~/.config/mimeapps.list, which is never touched.
    just box={{box}} _deploy-pam
    ssh {{box}} 'mkdir -p ~/.local/share/applications'
    rsync -az deploy/*.desktop deploy/nitro-mimeapps.list {{box}}:.local/share/applications/

# The lock screen's PAM service. Installed only when absent, the same
# never-overwrite rule as `just install`: a box whose admin edited
# /etc/pam.d/nitro-lock keeps the edit. Without the file PAM falls back
# to `other`, which on Debian/Ubuntu is deny-all, so the lock screen
# could not unlock. Part of the deployed set (docs/testbox.md).
_deploy-pam:
    ssh {{box}} 'mkdir -p ~/nitro-stage'
    rsync -az deploy/pam.d/nitro-lock {{box}}:nitro-stage/nitro-lock
    ssh {{box}} 'test -e /etc/pam.d/nitro-lock && echo "keeping existing /etc/pam.d/nitro-lock" || sudo install -Dm644 ~/nitro-stage/nitro-lock /etc/pam.d/nitro-lock'

# testhost2: the session runs from /usr/local/bin (root-owned), so the
# binaries are staged in ~/nitro-stage and `sudo install`ed; the
# examples (probes, not part of the session) go to ~/nitro-bin. The
# .desktop entries go last into /usr/local/share/applications, for the
# reason spelled out above.
_deploy-bins-greetd:
    ssh {{box}} 'mkdir -p ~/nitro-stage ~/nitro-bin'
    cd target/release && rsync -az {{box_bins}} {{box}}:nitro-stage/
    cd target/release/examples && rsync -az {{box_examples}} planes_probe {{box}}:nitro-bin/
    ssh {{box}} 'cd ~/nitro-stage && sudo install -m 755 {{box_bins}} {{box_bindir}}/'
    just box={{box}} _deploy-pam
    rsync -az deploy/*.desktop deploy/nitro-mimeapps.list {{box}}:nitro-stage/
    ssh {{box}} 'sudo install -d /usr/local/share/applications && sudo install -m 644 ~/nitro-stage/*.desktop ~/nitro-stage/nitro-mimeapps.list /usr/local/share/applications/'

# Chromium on nitro (#3865). `chromium_out` and `chromium_files` are
# defined in the root justfile, because `install-chromium` uses them too.
# This recipe does not build Chromium and fails clearly if it is missing.

# It does not restart nitro-dev and touches nothing in ~/nitro-bin but
# `chromium/` and the `chromium-nitro` wrapper; see docs/testbox.md.
#
# Rsync the Chromium build + its launcher entry to the box.
[doc("Rsync the Chromium build and its launcher entry to the box")]
[group('dev')]
deploy-chromium:
    #!/usr/bin/env bash
    set -euo pipefail
    out='{{chromium_out}}'
    if [[ ! -x $out/chrome ]]; then
        echo "deploy-chromium: no $out/chrome — build it first (docs/chromium-build.md, target chrome in out/Nitro)" >&2
        exit 1
    fi
    src=$(cd "$out/../.." && pwd)
    ssh {{box}} 'mkdir -p ~/nitro-bin/chromium ~/.local/share/applications'
    # `chrome` links ATK/AT-SPI (accessibility) and a desktop-less box
    # does not have them: without these three it dies in the loader with
    # `libatk-1.0.so.0: cannot open shared object file`. Idempotent.
    # On the Arch box (`greetd` profile) the same libraries are `at-spi2-core`.
    if [[ {{box_profile}} == greetd ]]; then
        ssh {{box}} 'pacman -Q at-spi2-core >/dev/null 2>&1 || sudo pacman -S --needed --noconfirm at-spi2-core'
    else
        ssh {{box}} 'dpkg -s libatk1.0-0t64 libatk-bridge2.0-0t64 libatspi2.0-0t64 >/dev/null 2>&1 || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libatk1.0-0t64 libatk-bridge2.0-0t64 libatspi2.0-0t64'
    fi
    # `--delete` is scoped to ~/nitro-bin/chromium/ — the directory is
    # ours alone, so a file dropped from the list goes with it.
    # `symbol_level=0` still leaves a ~200 MB `.symtab`/`.strtab` in a
    # 519 MB binary; strip a copy rather than touching the build.
    stage=target/chromium-stage
    mkdir -p "$stage"
    if [[ ! $stage/chrome -nt $out/chrome ]]; then
        strip -o "$stage/chrome" "$out/chrome"
    fi
    # `P /chrome` protects the stripped binary from this `--delete`; it
    # has its own rsync right after. `*.info` are build-time translation
    # manifests, not loaded at run time (70 MB of the locales dir).
    (cd "$out" && rsync -az --delete --exclude '*.info' --filter 'P /chrome' \
        {{chromium_files}} {{box}}:nitro-bin/chromium/)
    rsync -az "$stage/chrome" {{box}}:nitro-bin/chromium/chrome
    # The wrapper and the profile are templates (the install targets
    # render them with $LIBDIR). For the box, `$HOME` stays literal so
    # the wrapper's shell expands it at run time, and the profile matches
    # any user's ~/nitro-bin.
    sed 's|@CHROMIUM_DIR@|$HOME/nitro-bin/chromium|' deploy/chromium/chromium-nitro > "$stage/chromium-nitro"
    sed 's|@CHROME@|/home/*/nitro-bin/chromium/chrome|' deploy/chromium/apparmor-chromium-nitro > "$stage/apparmor-chromium-nitro"
    rsync -az --chmod=F755 "$stage/chromium-nitro" {{box}}:nitro-bin/chromium-nitro
    # The sandbox: an AppArmor profile granting `userns` to this path,
    # because `kernel.apparmor_restrict_unprivileged_userns=1` denies the
    # namespace sandbox to unconfined programs. See the profile's header.
    # A box without AppArmor (Arch) does not restrict userns, so there
    # is nothing to grant and the step is skipped.
    if ssh {{box}} 'command -v apparmor_parser >/dev/null'; then
        scp -q "$stage/apparmor-chromium-nitro" {{box}}:/tmp/apparmor-chromium-nitro
        ssh {{box}} 'sudo install -m 644 /tmp/apparmor-chromium-nitro /etc/apparmor.d/chromium-nitro && sudo apparmor_parser -r /etc/apparmor.d/chromium-nitro && rm /tmp/apparmor-chromium-nitro'
    else
        echo "no apparmor_parser on {{box}}; sandbox profile skipped"
    fi
    # Chromium's own logo, as `chromium-nitro` in the user's hicolor, so
    # the launcher row, the bar and the title bar resolve it by app id.
    for n in 16 24 48 64 128 256; do
        ssh {{box}} "mkdir -p ~/.local/share/icons/hicolor/${n}x${n}/apps"
        rsync -az "$src/chrome/app/theme/chromium/product_logo_$n.png" \
            {{box}}:.local/share/icons/hicolor/${n}x${n}/apps/chromium-nitro.png
    done
    # The entry goes **last**, for the reason in `deploy-bins`: a failure
    # above leaves no launcher entry pointing at a half-installed browser.
    rsync -az deploy/chromium/chromium-nitro.desktop {{box}}:.local/share/applications/
    ssh {{box}} 'md5sum ~/nitro-bin/chromium/chrome'

# Chromium scroll/i2p/memory benchmark on the box (#3919), against the
# running session: `just chromium-bench oop`, `just box=testhost2
# chromium-bench inproc 3`. MODE is inproc | oop | gpu | dmabuf | shm | video
# (see the script; `dmabuf` is #3921's GPU path, `NITRO_BENCH_ANGLE=gl` on the
# box switches its ANGLE backend; `video` plays an H.264 clip for #3944's
# overlays, steered by NITRO_NO_OVERLAYS and NITRO_BENCH_{RES,FPS,VIDEO_FS,VIDEO_CSS}).
# Needs `deploy-chromium` first; leaves nothing behind but /tmp files.
[doc("Chromium scroll/i2p/memory benchmark on the box (mode: inproc|oop|gpu|dmabuf|shm|video)")]
[group('dev')]
chromium-bench mode="oop" runs="2":
    ssh {{box}} 'mkdir -p ~/nitro-stage'
    rsync -az deploy/chromium-bench.sh deploy/scroll-bench.py {{box}}:nitro-stage/
    ssh {{box}} "NITRO_BENCH_ANGLE=${NITRO_BENCH_ANGLE:-vulkan} NITRO_NO_OVERLAYS=${NITRO_NO_OVERLAYS:-0} NITRO_BENCH_RES=${NITRO_BENCH_RES:-1080} NITRO_BENCH_FPS=${NITRO_BENCH_FPS:-60} NITRO_BENCH_VIDEO_FS=${NITRO_BENCH_VIDEO_FS:-0} NITRO_BENCH_VIDEO_CSS=${NITRO_BENCH_VIDEO_CSS:-1280x720} bash ~/nitro-stage/chromium-bench.sh {{mode}} {{runs}}"

# Chromium clipboard end to end on the box (#3943): two browsers,
# nitro-term and nitro-files copy and paste to each other through the DATA
# ops; prints PASS/FAIL per check (see the script) and fails if any fail.
# Needs `deploy-chromium` and a logged-in session; `KEEP=1` on the box
# keeps the logs.
[doc("Chromium clipboard end-to-end check on the box (PASS/FAIL per check)")]
[group('dev')]
chromium-clipboard:
    ssh {{box}} 'mkdir -p ~/nitro-stage'
    rsync -az deploy/chromium-clipboard.sh {{box}}:nitro-stage/
    ssh {{box}} 'bash ~/nitro-stage/chromium-clipboard.sh'

# Chromium drag and drop end to end on the box (#3965): two tiled browsers
# drag text and links to each other, to themselves and onto nitro-term;
# drops on the bar, Escape and a source SIGKILLed mid-drag must end cleanly.
# Prints PASS/FAIL per check (see the script). Needs `deploy-chromium` and a
# logged-in session; `KEEP=1` on the box keeps the logs.
[doc("Chromium drag-and-drop end-to-end check on the box (PASS/FAIL per check)")]
[group('dev')]
chromium-dnd:
    ssh {{box}} 'mkdir -p ~/nitro-stage'
    rsync -az deploy/chromium-dnd.sh {{box}}:nitro-stage/
    ssh {{box}} 'bash ~/nitro-stage/chromium-dnd.sh'

# Chromium full-screen sharing end to end on the box (#676 D):
# getDisplayMedia + RTCPeerConnection loopback while a video plays and a
# GPU Chromium window is open; prints the received fps and a content
# check, display fps sharing off vs on, CPU, the server's capture counters
# and dma-buf memory while recording and after (see the script). Needs
# `deploy-chromium` and a session whose server grants capture
# (NITRO_CAPTURE_ALLOW=1 until the prompt of #676 C). `just chromium-share
# 30 60` shares for 30 s at 60 fps; KEEP=1 on the box keeps the logs.
[doc("Chromium screen sharing (getDisplayMedia) end to end on the box: PASS/FAIL + measurements")]
[group('dev')]
chromium-share secs="20" fps="30":
    ssh {{box}} 'mkdir -p ~/nitro-stage'
    rsync -az deploy/chromium-share.sh deploy/scroll-bench.py {{box}}:nitro-stage/
    ssh {{box}} "KEEP=${KEEP:-} NITRO_SHOT=${NITRO_SHOT:-} bash ~/nitro-stage/chromium-share.sh {{secs}} {{fps}}"

# Install/refresh the systemd unit on the box (needs sudo there). On a
# `greetd` box there is no unit: this makes the clone pushable and
# checks that greetd is the display manager (`box-greetd` sets it up).
[doc("Install/refresh the nitro-dev systemd unit on the box (greetd: check setup)")]
[group('dev')]
box-install:
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ {{box_profile}} == greetd ]]; then
        ssh {{box}} 'git -C ~/src/ai/nitro config receive.denyCurrentBranch updateInstead && systemctl is-enabled greetd && echo "greetd is the display manager"'
        exit
    fi
    # `daemon-reload` picks up a changed unit; disabling getty@tty2 frees
    # the VT the session takes. tty1 keeps its getty for rescue.
    scp deploy/nitro-dev.service {{box}}:/tmp/nitro-dev.service
    ssh {{box}} 'sudo install -m 644 /tmp/nitro-dev.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl disable --now getty@tty2 2>/dev/null; true'

[doc("Show the status of nitro on the box")]
[group('dev')]
box-status:
    ssh {{box}} {{ if box_profile == "greetd" { "'loginctl list-sessions --no-pager; pgrep -a nitro || echo no nitro processes; systemctl is-active greetd || true'" } else { "'systemctl status nitro-dev --no-pager -n 20 || true'" } }}

# greetd as the box's display manager (docs/testbox.md "Login (greetd)").
# Idempotent. Installs the package, renders deploy/greetd/config.toml
# with the box's bin dir and greeter user (`greeter` on Arch, `_greetd`
# on Debian/Ubuntu), backs up the original config once as
# config.toml.pre-nitro, and switches the display manager: gdm is
# disabled and stopped if present, greetd enabled and started. On box1
# the greeter runs ~/nitro-bin, so the greeter user gets an ACL `x` on
# the home directory to reach it. Undo with `box-greetd-rollback`.
[doc("Install greetd on the box and make it the display manager (drops gdm)")]
[group('dev')]
box-greetd:
    #!/usr/bin/env bash
    set -euo pipefail
    ssh {{box}} 'command -v greetd >/dev/null || { if command -v pacman >/dev/null; then sudo pacman -S --needed --noconfirm greetd; else sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends greetd; fi; }'
    greeter=$(ssh {{box}} 'getent passwd greeter >/dev/null && echo greeter || echo _greetd')
    bindir=$(ssh {{box}} 'echo {{box_bindir}}')
    mkdir -p tmp
    # The greeter's server reads /etc/nitro/server.conf (seeded once from
    # the box user's, so the greeter has the same keymap and scale).
    ssh {{box}} 'test -e /etc/nitro/server.conf || { test -e ~/.config/nitro/server.conf && sudo install -Dm644 ~/.config/nitro/server.conf /etc/nitro/server.conf; } || true'
    sed -e "s|@BINDIR@|$bindir|g" -e "s|@GREETER_USER@|$greeter|g" \
        -e 's|^command = "env |command = "env NITRO_CONFIG=/etc/nitro/server.conf |' \
        deploy/greetd/config.toml > tmp/greetd-config.toml
    ssh {{box}} 'mkdir -p ~/nitro-stage'
    rsync -az tmp/greetd-config.toml {{box}}:nitro-stage/greetd-config.toml
    ssh {{box}} "set -e
        test -e /etc/greetd/config.toml.pre-nitro || sudo cp -a /etc/greetd/config.toml /etc/greetd/config.toml.pre-nitro
        sudo install -m 644 ~/nitro-stage/greetd-config.toml /etc/greetd/config.toml
        sudo install -d -o $greeter -m 755 /var/cache/nitro-greeter
        case $bindir in /home/*)
            command -v setfacl >/dev/null || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends acl
            sudo setfacl -m u:$greeter:x \$HOME ;;
        esac
        if systemctl cat gdm >/dev/null 2>&1; then sudo systemctl disable gdm 2>/dev/null || true; sudo systemctl stop gdm || true; fi
        # Debian's unit conflicts with getty@tty7 only (its default
        # VT); ours is VT 1, so the tty1 getty must yield to it.
        sudo install -d /etc/systemd/system/greetd.service.d
        printf '[Unit]\\nConflicts=getty@tty1.service\\nAfter=getty@tty1.service\\n' | sudo tee /etc/systemd/system/greetd.service.d/nitro-vt1.conf >/dev/null
        sudo systemctl daemon-reload
        sudo systemctl disable getty@tty1 2>/dev/null || true
        sudo systemctl enable greetd
        sudo systemctl restart greetd
        sleep 2; systemctl is-active greetd"

# Undo `box-greetd`: greetd off, the original config back, and gdm (or
# the tty1 getty) back on.
[doc("Undo box-greetd: disable greetd, restore gdm or getty@tty1")]
[group('dev')]
box-greetd-rollback:
    ssh {{box}} 'set -e; sudo systemctl disable --now greetd; sudo rm -f /etc/systemd/system/greetd.service.d/nitro-vt1.conf; sudo systemctl daemon-reload; sudo systemctl enable getty@tty1 2>/dev/null || true; if test -e /etc/greetd/config.toml.pre-nitro; then sudo mv /etc/greetd/config.toml.pre-nitro /etc/greetd/config.toml; fi; if systemctl cat gdm >/dev/null 2>&1; then sudo systemctl enable --now gdm; else sudo systemctl start getty@tty1; fi'

# Talk to the running session: `status`, `lock`, `suspend`, `logout`.
[doc("Send a command to the running session: status, lock, suspend, logout")]
[group('dev')]
box-session cmd="status":
    # `-q1`: the session keeps the connection open after most replies, so
    # nc is told to leave one second after its own stdin ends.
    ssh {{box}} 'printf "{{cmd}}\n" | nc -q1 -U /run/user/$(id -u)/nitro/session.sock'

# RSS/idle-CPU table for the whole desktop tree, for docs/budget.md.
[doc("RSS/idle-CPU table for the whole desktop tree on the box")]
[group('dev')]
box-ps:
    ssh {{box}} 'bash -s' < deploy/box-ps.sh

[doc("Follow the box's nitro logs (journalctl -f)")]
[group('dev')]
box-log:
    ssh {{box}} {{ if box_profile == "greetd" { "'journalctl -f -o cat _COMM=nitro-server + _COMM=nitro-session + _COMM=nitro-greeter + _COMM=nitro-bar + _COMM=nitro-launcher + _COMM=nitro-wallpaper + _COMM=greetd'" } else { "'journalctl -u nitro-dev -f -o cat'" } }}

# On a `greetd` box this stops greetd (the human's standing OK; start it
# again with `just box=testhost2 box-restart`).
[doc("Stop nitro on the box (greetd box: stops greetd)")]
[group('dev')]
box-stop:
    ssh {{box}} {{ if box_profile == "greetd" { "'sudo systemctl stop greetd'" } else { "'sudo systemctl stop nitro-dev'" } }}

# Screenshot of the live server, written to tmp/shot.png.
[doc("Screenshot the live server on the box into tmp/shot.png")]
[group('dev')]
shot out="tmp/shot.png":
    mkdir -p tmp && ssh {{box}} '{{box_bindir}}/nitro-shot' > {{out}} && echo "wrote {{out}}"

# Switch the box's foreground VT (vt-switch survival test).
[doc("Switch the box's foreground VT (vt-switch survival test)")]
[group('dev')]
box-chvt n:
    ssh {{box}} 'sudo chvt {{n}}'

# GPU helper pixel tests (crates/nitro-gpu-vulkan/tests/pixels.rs) on the
# box's render node, footprint included. The working tree (not a commit)
# is rsynced to ~/tmp/nitro-gpu-test there, so the box's clone and the
# running session are untouched. `NITRO_GPU_TEST=require` turns "no GPU"
# skips into failures. NV12 cases need /dev/udmabuf access (box1: yes via
# the seat ACL; testhost2: only for the GDM seat user).
[doc("Run the GPU helper pixel tests on the box's render node")]
[group('dev')]
box-gpu-test:
    rsync -az --delete --exclude target --exclude .git --exclude tmp ./ {{box}}:tmp/nitro-gpu-test/
    ssh {{box}} 'cd ~/tmp/nitro-gpu-test && NITRO_GPU_TEST=require ~/.cargo/bin/cargo test --release -p nitro-gpu-vulkan -- --include-ignored --test-threads=1 --nocapture'

# Regenerate the helper's SPIR-V from the GLSL next to it. Needs
# glslangValidator (installed on both boxes; `just box=... gpu-shaders`
# compiles there when it is missing locally). The .spv files are checked
# in, so a normal build needs no shader compiler.
[doc("Regenerate the GPU helper SPIR-V from its GLSL")]
[group('dev')]
gpu-shaders:
    #!/usr/bin/env bash
    set -euo pipefail
    cd crates/nitro-gpu-vulkan/shaders
    if command -v glslangValidator >/dev/null; then
        for s in quad.vert rgba.frag; do glslangValidator -V --target-env vulkan1.2 -o $s.spv $s; done
    else
        ssh {{box}} 'mkdir -p ~/tmp/nitro-shaders'
        scp -q quad.vert rgba.frag {{box}}:tmp/nitro-shaders/
        ssh {{box}} 'cd ~/tmp/nitro-shaders && for s in quad.vert rgba.frag; do glslangValidator -V --target-env vulkan1.2 -o $s.spv $s; done'
        scp -q {{box}}:tmp/nitro-shaders/quad.vert.spv {{box}}:tmp/nitro-shaders/rgba.frag.spv .
    fi

# Push the repo to the box's clone (~/src/ai/nitro on both boxes).
[doc("Push main to the box's clone (~/src/ai/nitro)")]
[group('dev')]
box-push:
    git push {{box}}:src/ai/nitro main

# ---------------------------------------------------------------------------
# Benchmarks (see docs/bench.md)
# ---------------------------------------------------------------------------

# The whole throughput matrix on the box, written to ~/tmp/bench/<sha>.jsonl
# there and fetched to tmp/bench/ here. `just bench "1920x1080@60
# 1920x1080@120 720p240"` sweeps the refresh rate; a bare `60` still means
# 1080p at that rate, and `720p240` is the CVT-RB modeline #3718 verified
# on the panel.
#
# It restarts `nitro-dev` and, in the refresh sweep, writes a `mode` or
# `modeline` line into the human's `server.conf` — backed up and restored
# by the script. Announce in the `nitro-testbox` room before running it:
# the box is shared, and the 720p arm changes what is on his screen.
#
# `NITRO_BENCH_SHA` is exported **from here**, not read from the box's
# clone, because the clone is whatever `just box-push` last put there and
# the binaries are whatever `just deploy` last built: a sweep stamped
# with the clone's sha names a tree that did not build what it measured.
# (#3722 lost a run to exactly that.)
#
# The modes string is shell-quoted **twice** (`quote(quote(modes))`) on
# purpose: ssh does not preserve argv, it joins its arguments into one
# command string and hands that to the remote login shell, which splits
# it again. One layer of quoting is eaten locally, the second by the
# remote shell, so the multi-arm form arrives as a single `--modes`
# argument. Simplify it back to `'{{modes}}'` and every arm after the
# first becomes a stray positional and `deploy/bench.sh` exits 2 — which
# is what #618/#3843 found, after the spelling below had been documented
# and unrunnable since `c4f51f1`. `crates/nitro-bench/tests/bench_recipe.rs`
# pins it.
#
# A `greetd` box has no nitro-dev unit to restart between arms, so bench
# refuses there.
[doc("Run the throughput benchmark matrix on the box (restarts nitro-dev)")]
[group('dev')]
bench modes="" seconds="10":
    {{ if box_profile == "greetd" { "echo 'bench: " + box + " runs nitro under greetd, with no nitro-dev unit to restart between arms (docs/testbox.md)' >&2; exit 1" } else { "" } }}
    ssh {{box}} 'NITRO_BENCH_SHA='"$(git rev-parse --short HEAD)"' bash -s' -- --seconds {{seconds}} {{ if modes == "" { "" } else { "--modes " + quote(quote(modes)) } }} < deploy/bench.sh
    mkdir -p tmp/bench
    ssh {{box}} 'cat ~/tmp/bench/*.jsonl' > tmp/bench/box.jsonl
    @echo "wrote tmp/bench/box.jsonl"

# The markdown for docs/bench.md, from a ledger.
[doc("Render the docs/bench.md markdown from a bench ledger")]
[group('dev')]
bench-report file="tmp/bench/box.jsonl":
    cargo run -q -p nitro-bench -- report {{file}}

# This machine's memcpy bandwidth — the denominator the pixel-path
# verdicts need. `just bench-bandwidth box` measures the box instead.
[doc("Measure memcpy bandwidth here (or on the box with `box`)")]
[group('dev')]
bench-bandwidth where="here":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "{{where}}" == box ]]; then
        ssh {{box}} '{{box_bindir}}/nitro-bench bandwidth'
    else
        cargo run -q --release -p nitro-bench -- bandwidth
    fi
