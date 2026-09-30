# nitro-login

The login wire: greetd's IPC, hand-rolled, shared by both ends of nitro's
lock screen and (later) greeter.

- `ipc`: `Request` / `Response`, a JSON encoder and a decoder for exactly
  these seven flat messages, and `Framer`, which reassembles
  native-endian-`u32`-length-prefixed frames from partial non-blocking
  reads. Frames are capped at 64 KiB (`MAX_FRAME`).
- `owner()`: whose session this is (`$USER`, `$LOGNAME`, else the
  `getuid()` entry of `/etc/passwd`).

`nitro-auth` speaks this protocol on stdin/stdout; greetd speaks it on
`$GREETD_SOCK`. `nitro-greeter` uses the same codec for both, and so does
not link libpam: only the helper does.

The decoder reads one flat object whose values are strings or `null`
(arrays of strings only for `start_session`'s `cmd`/`env`). Numbers,
booleans, nested objects, lone surrogates, trailing bytes, duplicate keys:
all refused. Unknown keys are skipped. `Debug` on a request never prints
a response, and buffers that carried one are zeroed (best effort, as the
secret `TextField` does: `black_box`, no `unsafe`).

Dependencies: `std` and `rustix` (`process`, for `getuid`). No JSON crate;
the argument is `docs/greeter.md`, decision 3.
