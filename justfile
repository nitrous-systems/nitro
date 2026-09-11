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
