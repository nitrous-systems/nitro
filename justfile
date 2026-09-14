set shell := ["bash", "-euo", "pipefail", "-c"]

default: fmt build test

fmt:
    cargo fmt --all -- --check

build:
    cargo build --workspace --all-targets

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

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
box_bins := "nitro-server nitro-session nitro-shot nitro-demo nitro-calc nitro-term nitro-bar nitro-launcher nitro-wallpaper nitro-settings hey"
box_examples := "hello_client hello_dialog shell_probe"

# Build release, rsync binaries to the box, restart the dev session.
deploy: (deploy-bins) 
    ssh {{box}} 'sudo systemctl restart nitro-dev' && just box-status

deploy-bins:
    # `--examples` on its own does not build the binaries, so ask for both.
    cargo build --release --workspace --bins --examples
    cd target/release && rsync -az {{box_bins}} {{box}}:nitro-bin/
    cd target/release/examples && rsync -az {{box_examples}} {{box}}:nitro-bin/
    # `deploy/nitro-term.desktop` is deliberately NOT installed here.
    # The launcher's built-in entry spawns `~/nitro-bin/nitro-term` by
    # absolute path; a `.desktop` file shadows that built-in, and its
    # `Exec=nitro-term` is a bare name that the session's `PATH` does not
    # resolve — so installing it replaced a working entry with
    # "spawn: No such file or directory". The file is for a packager who
    # puts the binary in `/usr/bin`; the box is covered by the built-in.

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
