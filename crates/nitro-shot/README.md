# nitro-shot

Screenshot client for `nitro-server`'s control socket. No dependencies:
the PNG encoder is ~120 lines of its own (8-bit RGB, filter 0 per row,
zlib stream of *stored* deflate blocks, CRC-32 table, Adler-32).

```text
nitro-shot [-o FILE] [--raw] [--output NAME] [--no-cursor] [-v]
                                                screenshot as PNG (stdout or FILE)
nitro-shot --raw                                XRGB8888 bytes exactly as the server sent them
nitro-shot --outputs                            list outputs: `name WxH@refresh_mhz scale=S pos=X,Y primary=0|1` (+` (custom)` for a modeline)
nitro-shot --modes                              list every mode each connector offers: `name WxH@Hz` (`*` preferred, `=` in use)
nitro-shot --stats                              frame counters
nitro-shot --quit                               orderly server shutdown
```

**What a shot shows (#3962).** What is on screen, Surfaces included:
video on a display plane, Surfaces the GPU helper composites and
Chromium's GPU windows come out of their buffers, not as holes. The
server draws CPU-readable buffers (shm, linear dma-bufs, its own scanout
buffers) itself and asks the GPU helper to composite tiled/compressed
ones (starting an on-demand helper for the shot). Only when neither can
(`gpu.helper = off`, a crashed helper, an unsupported format) is a
Surface grey (`0x808080`), and nitro-shot says so on stderr:
`nitro-shot: 1 surface(s) shown as placeholder (helper-off)`. `-v`
prints the server's whole metadata line (`surfaces= cpu= helper=
placeholder= reason=`). `--no-cursor` leaves the (software) cursor out.

Latency: a shot with no Surfaces is a copy of the shadow (a few ms at
1080p); a helper capture adds one GPU composite and a copy back, and a
helper started on demand adds its startup (see `docs/budget.md`). Nothing
stays allocated after the reply.

Socket: `$NITRO_CONTROL`, else `$XDG_RUNTIME_DIR/nitro/control.sock`,
else `/tmp/nitro-<uid>/control.sock`. Over ssh both sides see the same
`/run/user/<uid>`, so `just shot` (`ssh box ~/nitro-bin/nitro-shot >
tmp/shot.png`) just works; `just fake-shot` does the same against a local
`just fake`.

Exit status 1 with `nitro-shot: <reason>` on stderr when the socket is
missing or the server answers `err`.

The PNG output is uncompressed (stored blocks): a 1920×1080 shot is about
6 MB. Fine for `just shot`; pipe through `optipng`/`magick` if it matters.
