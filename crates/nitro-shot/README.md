# nitro-shot

Screenshot client for `nitro-server`'s control socket. No dependencies:
the PNG encoder is ~120 lines of its own (8-bit RGB, filter 0 per row,
zlib stream of *stored* deflate blocks, CRC-32 table, Adler-32).

```text
nitro-shot [-o FILE] [--raw] [--output NAME]   screenshot as PNG (stdout or FILE)
nitro-shot --raw                                XRGB8888 bytes exactly as the server sent them
nitro-shot --outputs                            list outputs: `name WxH@refresh_mhz scale=S pos=X,Y primary=0|1` (+` (custom)` for a modeline)
nitro-shot --modes                              list every mode each connector offers: `name WxH@Hz` (`*` preferred, `=` in use)
nitro-shot --stats                              frame counters
nitro-shot --quit                               orderly server shutdown
```

Socket: `$NITRO_CONTROL`, else `$XDG_RUNTIME_DIR/nitro/control.sock`,
else `/tmp/nitro-<uid>/control.sock`. Over ssh both sides see the same
`/run/user/<uid>`, so `just shot` (`ssh box ~/nitro-bin/nitro-shot >
tmp/shot.png`) just works; `just fake-shot` does the same against a local
`just fake`.

Exit status 1 with `nitro-shot: <reason>` on stderr when the socket is
missing or the server answers `err`.

The PNG output is uncompressed (stored blocks): a 1920×1080 shot is about
6 MB. Fine for `just shot`; pipe through `optipng`/`magick` if it matters.
