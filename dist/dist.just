# Distribution packages, built in containers (docs/packaging.md). Imported
# from the root justfile, so the recipes run from the repository root.
# Output: dist/out/<distro>/. The engine is podman or docker
# (DIST_ENGINE overrides the auto-detection).

# Base images. The Ubuntu `latest` tag is the current LTS, `rolling` the
# newest release (the same image between an LTS and the next interim).
dist_ubuntu_lts := env_var_or_default("DIST_UBUNTU_LTS", "docker.io/library/ubuntu:latest")
dist_ubuntu     := env_var_or_default("DIST_UBUNTU", "docker.io/library/ubuntu:rolling")
dist_debian     := env_var_or_default("DIST_DEBIAN", "docker.io/library/debian:stable")
dist_arch       := env_var_or_default("DIST_ARCH", "docker.io/library/archlinux:latest")
dist_alpine     := env_var_or_default("DIST_ALPINE", "docker.io/library/alpine:latest")

[doc("Build the .deb for the current Ubuntu LTS into dist/out/ubuntu-lts")]
[group('dist')]
dist-deb-ubuntu-lts:
    bash dist/run.sh build ubuntu-lts deb '{{dist_ubuntu_lts}}'

[doc("Build the .deb for the latest Ubuntu release into dist/out/ubuntu")]
[group('dist')]
dist-deb-ubuntu:
    bash dist/run.sh build ubuntu deb '{{dist_ubuntu}}'

[doc("Build the .deb for Debian stable into dist/out/debian")]
[group('dist')]
dist-deb-debian:
    bash dist/run.sh build debian deb '{{dist_debian}}'

[doc("Build the Arch Linux package into dist/out/arch")]
[group('dist')]
dist-arch:
    bash dist/run.sh build arch arch '{{dist_arch}}'

[doc("Build the Alpine package into dist/out/alpine")]
[group('dist')]
dist-alpine:
    bash dist/run.sh build alpine alpine '{{dist_alpine}}'

[doc("Build every distribution package")]
[group('dist')]
dist-all: dist-deb-ubuntu-lts dist-deb-ubuntu dist-deb-debian dist-arch dist-alpine

[doc("Write the source tarball (git archive) into dist/out/src")]
[group('dist')]
dist-src:
    bash dist/run.sh src

[doc("Install the Ubuntu LTS .deb in a fresh container and smoke-test it")]
[group('dist')]
dist-test-ubuntu-lts:
    bash dist/run.sh test ubuntu-lts deb '{{dist_ubuntu_lts}}'

[doc("Install the latest-Ubuntu .deb in a fresh container and smoke-test it")]
[group('dist')]
dist-test-ubuntu:
    bash dist/run.sh test ubuntu deb '{{dist_ubuntu}}'

[doc("Install the Debian .deb in a fresh container and smoke-test it")]
[group('dist')]
dist-test-debian:
    bash dist/run.sh test debian deb '{{dist_debian}}'

[doc("Install the Arch package in a fresh container and smoke-test it")]
[group('dist')]
dist-test-arch:
    bash dist/run.sh test arch arch '{{dist_arch}}'

[doc("Install the Alpine package in a fresh container and smoke-test it")]
[group('dist')]
dist-test-alpine:
    bash dist/run.sh test alpine alpine '{{dist_alpine}}'

[doc("Smoke-test every built package")]
[group('dist')]
dist-test-all: dist-test-ubuntu-lts dist-test-ubuntu dist-test-debian dist-test-arch dist-test-alpine

[doc("Remove dist/out")]
[group('dist')]
dist-clean:
    rm -rf dist/out
