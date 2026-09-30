# Build container for the .deb packages (dist/dist.just). One file for
# every Debian-family target; BASE picks the release (ubuntu:latest,
# ubuntu:rolling, debian:stable, …).
ARG BASE=debian:stable
FROM ${BASE}

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential debhelper dpkg-dev fakeroot pkg-config \
        ca-certificates curl git \
        libseat-dev libinput-dev libxkbcommon-dev libpam0g-dev \
        libavformat-dev libavcodec-dev libavutil-dev \
    && rm -rf /var/lib/apt/lists/*

# The workspace is edition 2024 and follows stable (rust-toolchain.toml).
# The distros' rustc lags behind that, so the toolchain comes from rustup.
ENV RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo
ENV PATH=/opt/rust/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path \
        --profile minimal --default-toolchain stable \
    && chmod -R a+rwX /opt/rust

# The justfile needs a just with `import`, `[doc]` and `[group]`
# (>= 1.27). The distro's is too old on some releases, so take the
# prebuilt release binary.
RUN curl -sSf https://just.systems/install.sh | bash -s -- --to /usr/local/bin \
    && just --version
