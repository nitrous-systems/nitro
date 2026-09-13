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
# Test box (see docs/testbox.md)
# ---------------------------------------------------------------------------
box := env_var_or_default("NITRO_BOX", "kaspar@192.168.1.204")
box_bins := "nitro-server nitro-shot hello_client"

# Build release, rsync binaries to the box, restart the dev server.
deploy: (deploy-bins) 
    ssh {{box}} 'sudo systemctl restart nitro-dev' && just box-status

deploy-bins:
    # `--examples` on its own does not build the binaries, so ask for both.
    cargo build --release --workspace --bins --examples
    cd target/release && rsync -az nitro-server nitro-shot {{box}}:nitro-bin/
    cd target/release/examples && rsync -az hello_client {{box}}:nitro-bin/

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
