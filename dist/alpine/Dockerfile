# Build container for the Alpine package (dist/dist.just).
ARG BASE=alpine:latest
FROM ${BASE}

# Alpine's own rust links musl dynamically. rustup's musl target defaults
# to crt-static, which cannot link the C libraries (libseat, libinput,
# FFmpeg, …) as shared objects, so the distro toolchain is used.
RUN apk add --no-cache \
        alpine-sdk bash sudo cargo just pkgconf linux-headers \
        libseat-dev libinput-dev libxkbcommon-dev linux-pam-dev ffmpeg-dev

# abuild runs as a user in group abuild, with a signing key.
RUN adduser -D builder && addgroup builder abuild \
    && su builder -c 'abuild-keygen -a -n'
