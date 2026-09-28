# nitro

A minimal, snappy desktop stack for Linux: a KMS compositor with a retained
scene graph, a small retained-mode toolkit, and a handful of shell apps.
Rust throughout. Remote and phone capable from the same code.

See [DESIGN.md](DESIGN.md) for the architecture sketch and milestones.

## Install

`sudo just install` builds a release and installs into `/usr/local`.
`just PREFIX=$HOME/.local install` does a user install. `PREFIX`, `DESTDIR`
and the other GNU directory variables work as a packager expects. See
[docs/install.md](docs/install.md), which also covers the optional
Chromium build, its runtime dependencies and the AppArmor profile. The
development and test-box recipes live in `deploy/dev.just`
([docs/testbox.md](docs/testbox.md)).
