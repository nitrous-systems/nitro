# nitro-system

The desktop's remote controls, shared by `nitro-settings` and
`nitro-bar`'s quick-settings menu. Library only; depends on `nitro-core`
alone and adds no external crate.

| module | what it drives |
|---|---|
| `audio` | `wpctl` (PipeWire), falling back to `pactl`: volume, mute, the sink list (`Backend::sinks`) and the default output (`Backend::set_default_sink`) |
| `conf` | `server.conf`: parse, render, atomic write, and `set_scheme` (change `theme.scheme` only) |
| `session` | `nitro-session`'s socket: `request(path, Action)` sends `logout`/`suspend`/`reboot`/`poweroff`/`lock` and reads `ok` / `err <reason>` with a 2 s timeout |

No background activity: every call runs a subprocess or a socket round
trip when it is made and returns. Output formats of `wpctl`/`pactl` are
parsed by shape, not column, and an unreadable answer is `None` rather
than a confident zero.

`session` is a client of the protocol in `crates/nitro-session/README.md`
("The protocol"). `nitro-session` does **not** depend on this crate; the
two share a two-line protocol, not code, and the tests here pin the client
side against a fake listener.

`conf` is a second implementation of `nitro_server::config`'s format; the
round-trip test that keeps them honest is
`the_server_parser_reads_back_what_we_write` in
`crates/nitro-settings/tests/settings.rs`.
