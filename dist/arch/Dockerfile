# Build container for the Arch Linux package (dist/dist.just).
ARG BASE=archlinux:latest
FROM ${BASE}

# Arch's rust tracks stable closely, so the distro toolchain is used.
# The dependencies are preinstalled here, so makepkg runs without -s.
RUN pacman -Syu --noconfirm --needed \
        base-devel git rust just pkgconf \
        seatd libinput libxkbcommon pam ffmpeg \
    && pacman -Scc --noconfirm

# makepkg refuses to run as root.
RUN useradd -m builder
