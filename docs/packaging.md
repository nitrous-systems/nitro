# Distribution packages

`dist/` builds native packages of nitro in containers, one per target:

| recipe | base image (override) | output |
|---|---|---|
| `just dist-deb-ubuntu-lts` | `ubuntu:latest` (`DIST_UBUNTU_LTS`) | `dist/out/ubuntu-lts/nitro_<ver>_amd64.deb` |
| `just dist-deb-ubuntu` | `ubuntu:rolling` (`DIST_UBUNTU`) | `dist/out/ubuntu/nitro_<ver>_amd64.deb` |
| `just dist-deb-debian` | `debian:stable` (`DIST_DEBIAN`) | `dist/out/debian/nitro_<ver>_amd64.deb` |
| `just dist-arch` | `archlinux:latest` (`DIST_ARCH`) | `dist/out/arch/nitro-<ver>-1-x86_64.pkg.tar.zst` |
| `just dist-alpine` | `alpine:latest` (`DIST_ALPINE`) | `dist/out/alpine/build/x86_64/nitro-<ver>-r0.apk` and the signing key `dist/out/alpine/*.rsa.pub` |
| `just dist-all` | | all of the above |
| `just dist-test-<distro>`, `dist-test-all` | the same image, fresh | installs the package and smoke-tests it |
| `just dist-src` | | `dist/out/src/nitro-<ver>.tar.gz` (git archive) |
| `just dist-clean` | | removes `dist/out` |

`<distro>` is `ubuntu-lts`, `ubuntu`, `debian`, `arch` or `alpine`.
Images are given fully qualified (`docker.io/library/…`), so podman does
not ask which registry to use. Between an Ubuntu LTS and the next interim
release, `ubuntu:latest` and `ubuntu:rolling` are the same image, and the
two packages are identical.

## Prerequisites

podman (rootless is fine) or docker. `DIST_ENGINE=docker` or
`DIST_ENGINE=podman` overrides the auto-detection, which picks podman
first. The build containers need **network access**: they install the
distro's build dependencies, rustup (deb) and just (deb), and cargo
fetches the crates. Nothing is cached between builds; a build is a full
release build (fat LTO, `codegen-units = 1`) and takes a few minutes per
distro.

The containers cannot run inside a sandbox that forbids nested user
namespaces (the tau worker sandbox, for one). Build on a normal host.

## How a build works

`dist/run.sh` does all of it; the recipes only name the distro.

1. It computes the version (below) and writes a `git archive` of `HEAD`
   to `dist/out/src/`. Only **committed** files are packaged.
   `DIST_DIRTY=1` also includes uncommitted changes to tracked files
   (through `git stash create`); untracked files are never included.
2. It builds the image from `dist/<deb|arch|alpine>/Dockerfile`, with the
   base image as the build argument `BASE`.
3. It runs `dist/<dir>/build.sh` in that image. The tarball, the
   packaging directory and `dist/out/<distro>` are bind-mounted. The
   script builds with the distro's own tool:
   - **deb**: `dpkg-buildpackage -b` on `dist/deb/debian/`, with the
     changelog generated from `dist/deb/changelog.in`.
   - **Arch**: `makepkg` on `dist/arch/PKGBUILD` as an unprivileged user.
   - **Alpine**: `abuild -r` on `dist/alpine/APKBUILD` as a user in group
     `abuild`, with a key generated in the image.
4. Every format installs with the justfile's own recipe:
   `just PREFIX=/usr DESTDIR=<staging> SUDO=none install`. The file list
   is therefore `install_bins` plus what `install-bins` and
   `install-desktop` put down ([install.md](install.md)); no packaging file
   repeats it.

Under rootful docker the output is chowned back to the invoking user.

## What a package contains

- the 15 binaries in `install_bins` (`/usr/bin`)
- `/etc/pam.d/nitro-lock`, a conffile (deb) / `backup` (Arch). Alpine
  has no equivalent; apk keeps a modified file as `.apk-new`.
- `deploy/*.desktop` and `nitro-mimeapps.list` in `/usr/share/applications`

**Not included:**

- `install-chromium`: the nitro-ozone Chromium build is external and
  takes an hour to build ([chromium-build.md](chromium-build.md)).
- `install-apparmor`: its profile only covers that Chromium binary.
- `nitro-gpu-vulkan`: it is not in `install_bins` (only in the test box's
  `box_bins`). The packages follow `install_bins`; whether it should ship
  is a separate question.

## Versions

The version is `[workspace.package] version` in the root `Cargo.toml`. To
bump it, change it there; nothing in `dist/` holds a version. A snapshot
(the default) adds the commit date and revision, spelled the way each
format allows:

| format | snapshot | `DIST_RELEASE=1` |
|---|---|---|
| deb | `0.0.1+git20260930.58e69d1` | `0.0.1` |
| Arch | `0.0.1.r<commits>.g58e69d1`, `pkgrel=1` | `0.0.1` |
| Alpine | `0.0.1_git20260930`, `pkgrel=0`; the rev is in `pkgdesc` | `0.0.1` |

Snapshot versions sort by date (deb, Alpine) or by commit count (Arch),
so a later commit upgrades an earlier one. Two snapshots from the same
day get the same Alpine version; bump `pkgrel` by hand if that matters.
`DIST_RELEASE=1` is for a tagged release commit.

## Dependencies

Build:

| | deb | Arch | Alpine |
|---|---|---|---|
| toolchain | build-essential, rustup stable | base-devel, rust | alpine-sdk, cargo |
| just | release binary (≥ 1.27 needed; some distro versions are older) | just | just |
| libseat | libseat-dev | seatd | libseat-dev |
| libinput | libinput-dev | libinput | libinput-dev |
| xkbcommon | libxkbcommon-dev | libxkbcommon | libxkbcommon-dev |
| PAM | libpam0g-dev | pam | linux-pam-dev |
| FFmpeg | libavformat-dev libavcodec-dev libavutil-dev | ffmpeg | ffmpeg-dev |
| other | pkg-config, debhelper | pkgconf | pkgconf, linux-headers |

Vulkan is not a build dependency: the shaders are prebuilt and `ash`
dlopens libvulkan.

Runtime: the shared libraries above plus libc and libgcc_s. The deb
takes them from `dh_shlibdeps` (`${shlibs:Depends}`) and Alpine from
abuild's `so:` tracing. The PKGBUILD lists them by hand:
`glibc gcc-libs seatd libinput libxkbcommon pam ffmpeg`. greetd is a
suggestion (deb `Suggests`, Arch `optdepends`), not a dependency.

The deb family uses rustup because the workspace is edition 2024 on
stable (`rust-toolchain.toml`) and the distros' rustc lags. Arch and
Alpine use their own rust. On Alpine that is required: rustup's musl
target defaults to `crt-static`, which cannot link libseat, libinput and
FFmpeg as shared libraries. Alpine's rust links musl dynamically.

## Smoke test

`just dist-test-<distro>` starts a fresh container of the same base image,
installs the package with the distro's package manager (so the declared
runtime dependencies are pulled in), and runs `dist/smoke.sh`. The script
checks:

- every binary in `install_bins` is in `/usr/bin`
- `/etc/pam.d/nitro-lock` and the desktop entries are installed
- `ldd` resolves every library of every binary, which proves the
  dependencies are complete
- `hey --help` runs and prints its usage (it exits 1 with no apps running)

Nothing is started beyond `hey`: the other binaries need a VT, DRM, or a
running server.

## musl (Alpine)

The workspace builds on musl with Alpine's rust, with no source changes.
The places at risk were checked: rustix (linux_raw backend, no libc),
nonstick (`-lpam` against linux-pam), libseat-sys and input-sys
(pkg-config), and nitro-video's C shim against ffmpeg-dev.
