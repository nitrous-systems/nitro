set shell := ["bash", "-euo", "pipefail", "-c"]

default: fmt build test lint-colors

fmt:
    cargo fmt --all -- --check

build:
    cargo build --workspace --all-targets

clippy: lint-colors
    cargo clippy --workspace --all-targets -- -D warnings

# Fail on a hard-coded colour outside the palette. See the script's
# header and docs/theme.md: colours come from roles, so that one
# `theme.scheme` switch moves the whole desktop.
lint-colors:
    bash deploy/lint-colors.sh

# Everything a merge checks that is not a compile or a test.
lint: lint-colors clippy

test:
    cargo test --workspace

# ---------------------------------------------------------------------------
# Headless (the CI-able path)
# ---------------------------------------------------------------------------
fake_size := env_var_or_default("NITRO_FAKE_SIZE", "1280x720")

# Run the server locally on the fake backend (Ctrl-C to stop).
fake:
    NITRO_BACKEND=fake NITRO_FAKE_SIZE={{fake_size}} cargo run -p nitro-server

# Grab a PNG from a locally running `just fake` server.
fake-shot out="tmp/fake-shot.png":
    mkdir -p tmp && cargo run -q -p nitro-shot -- -o {{out}} && echo "wrote {{out}}"

# ---------------------------------------------------------------------------
# Icons (see docs/icons.md)
# ---------------------------------------------------------------------------

# Regenerate crates/nitro-icons/src/set.rs from the pinned upstream commit.
# `just icons-import gear house` also adds those two names to icons.txt.
# Idempotent: a run with no arguments must leave set.rs byte-identical.
icons-import *names:
    bash deploy/icons-import.sh {{names}}

# ---------------------------------------------------------------------------
# Budget (see docs/budget.md)
# ---------------------------------------------------------------------------

# Binary sizes and RSS/HWM of every shipped binary, release build.
# `just size 5` measures the server with five windows open.
size windows="1":
    bash deploy/size.sh {{windows}}

# ---------------------------------------------------------------------------
# Test box (see docs/testbox.md)
# ---------------------------------------------------------------------------
box := env_var_or_default("NITRO_BOX", "kaspar@192.168.1.204")
# Everything the box needs for a desktop. `nitro-session` is what the
# unit runs; it finds the other four next to itself in ~/nitro-bin, which
# is why they are deployed together and in one rsync — a session that
# started yesterday's bar next to today's server is the failure mode the
# sibling lookup exists to prevent.
box_bins := "nitro-server nitro-session nitro-shot nitro-demo nitro-bench nitro-calc nitro-term nitro-files nitro-bar nitro-launcher nitro-wallpaper nitro-settings hey"
box_examples := "hello_client hello_dialog shell_probe"

# Build release, rsync binaries to the box, restart the dev session.
deploy: (deploy-bins) 
    ssh {{box}} 'sudo systemctl restart nitro-dev' && just box-status

deploy-bins:
    # `--examples` on its own does not build the binaries, so ask for both.
    cargo build --release --workspace --bins --examples
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
    ssh {{box}} 'mkdir -p ~/.local/share/applications'
    rsync -az deploy/*.desktop {{box}}:.local/share/applications/

# Install/refresh the systemd unit on the box (needs sudo there).
box-install:
    # `daemon-reload` picks up a changed unit; disabling getty@tty2 frees
    # the VT the session takes. tty1 keeps its getty for rescue.
    scp deploy/nitro-dev.service {{box}}:/tmp/nitro-dev.service
    ssh {{box}} 'sudo install -m 644 /tmp/nitro-dev.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl disable --now getty@tty2 2>/dev/null; true'

box-status:
    ssh {{box}} 'systemctl status nitro-dev --no-pager -n 20 || true'

# Talk to the running session: `status`, `lock`, `suspend`, `logout`.
box-session cmd="status":
    # `-q1`: the session keeps the connection open after most replies, so
    # nc is told to leave one second after its own stdin ends.
    ssh {{box}} 'printf "{{cmd}}\n" | nc -q1 -U /run/user/$(id -u)/nitro/session.sock'

# RSS/idle-CPU table for the whole desktop tree, for docs/budget.md.
box-ps:
    ssh {{box}} 'bash -s' < deploy/box-ps.sh

box-log:
    ssh {{box}} 'journalctl -u nitro-dev -f -o cat'

box-stop:
    ssh {{box}} 'sudo systemctl stop nitro-dev'

# Screenshot of the live server, written to tmp/shot.png.
shot out="tmp/shot.png":
    mkdir -p tmp && ssh {{box}} '~/nitro-bin/nitro-shot' > {{out}} && echo "wrote {{out}}"

# Switch the box's foreground VT (vt-switch survival test).
box-chvt n:
    ssh {{box}} 'sudo chvt {{n}}'

# Push the repo to the box's clone (~/src/ai/nitro).
box-push:
    git push box main

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
bench modes="" seconds="10":
    ssh {{box}} 'NITRO_BENCH_SHA='"$(git rev-parse --short HEAD)"' bash -s' -- --seconds {{seconds}} {{ if modes == "" { "" } else { "--modes '" + modes + "'" } }} < deploy/bench.sh
    mkdir -p tmp/bench
    ssh {{box}} 'cat ~/tmp/bench/*.jsonl' > tmp/bench/box.jsonl
    @echo "wrote tmp/bench/box.jsonl"

# The markdown for docs/bench.md, from a ledger.
bench-report file="tmp/bench/box.jsonl":
    cargo run -q -p nitro-bench -- report {{file}}

# This machine's memcpy bandwidth — the denominator the pixel-path
# verdicts need. `just bench-bandwidth box` measures the box instead.
bench-bandwidth where="here":
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "{{where}}" == box ]]; then
        ssh {{box}} '~/nitro-bin/nitro-bench bandwidth'
    else
        cargo run -q --release -p nitro-bench -- bandwidth
    fi
