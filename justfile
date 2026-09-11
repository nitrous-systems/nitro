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
# Test box (see docs/testbox.md)
# ---------------------------------------------------------------------------
box := env_var_or_default("NITRO_BOX", "kaspar@192.168.1.204")
box_bins := "nitro-server nitro-shot"

# Build release, rsync binaries to the box, restart the dev server.
deploy: (deploy-bins) 
    ssh {{box}} 'sudo systemctl restart nitro-dev' && just box-status

deploy-bins:
    cargo build --release --workspace
    cd target/release && rsync -az {{box_bins}} {{box}}:nitro-bin/

# Install/refresh the systemd unit on the box (needs sudo there).
box-install:
    scp deploy/nitro-dev.service {{box}}:/tmp/nitro-dev.service
    ssh {{box}} 'sudo install -m 644 /tmp/nitro-dev.service /etc/systemd/system/ && sudo systemctl daemon-reload && sudo systemctl disable --now getty@tty2 2>/dev/null; true'

box-status:
    ssh {{box}} 'systemctl status nitro-dev --no-pager -n 20 || true'

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
