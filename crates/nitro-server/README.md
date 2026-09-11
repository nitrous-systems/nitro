# nitro-server

The display server. M0 shape: one thread, one epoll, a seat (`nitro-seat`),
a KMS backend (`nitro-kms`), a placeholder scene and a line-based control
socket. `lib.rs` exposes `run(Config)`; `main.rs` only turns environment
variables into a `Config`, so tests drive the whole loop in-process on the
fake backend.

## Environment

| variable          | values                         | default                        |
|-------------------|--------------------------------|--------------------------------|
| `NITRO_BACKEND`   | `drm`, `fake`                  | `drm`                          |
| `NITRO_DRM_CARD`  | `/dev/dri/cardN`               | first card with a connected output, else first that opens |
| `NITRO_FAKE_SIZE` | `WxH` (fake only)              | `1280x720`                     |
| `NITRO_DEMO`      | `static`, `moving`             | `static` (bar stops after 3 s) |
| `NITRO_CONTROL`   | socket path                    | `$XDG_RUNTIME_DIR/nitro/control.sock`, else `/tmp/nitro-<uid>/control.sock` (with a warning) |
| `NITRO_LOG`       | `error`, `warn`, `info`, `debug` | `info`                       |

`fake` needs no seat at all: `just fake` runs it locally, `just fake-shot`
grabs a PNG from it.

## Event loop

Level-triggered epoll, no timers. An idle server never wakes (`top` shows
0.0 %, `voluntary_ctxt_switches` stops counting).

| fd                        | on readable                                                              |
|---------------------------|--------------------------------------------------------------------------|
| seat                      | `Seat::dispatch`; `Disable` → `backend.pause()` then `ack_disable()`; `Enable` → `backend.resume()`, full repaint |
| backend `poll_fds()`      | `Backend::dispatch`; `Flipped` → paint the next frame; `Hotplug` → `rescan()`, re-register fds, paint new outputs |
| signal self-pipe          | SIGTERM/SIGINT → orderly shutdown (`signal-hook`'s `low_level::pipe` on a `UnixDatagram` pair) |
| control listener          | accept, register the client                                              |
| control client            | read lines, answer, drop on hangup; `OUT` interest only while a reply is queued |

`epoll_wait` is retried on `EINTR` (the signal handler interrupts it and
the kernel never restarts `epoll_wait`); the self-pipe is what ends the
loop.

Startup on DRM: open the seat, `dispatch()` once (libseat queues the
initial `Enable` inside `open_seat` without making the fd readable), wait
for `is_active()`, then open the card through the seat. The backend gets a
`dup` of the seat's fd (same open file description, hence DRM master) so it
can be `'static`; the seat closes its own fd after the backend is gone.

Rendering (`render.rs`): a vertical gradient, a 4-px white frame, and a
40-px orange bar that advances 8 px per flip and wraps. Frames are painted
**only** in response to `Flipped`, never while `flip_pending`, never while
paused. The two buffers alternate strictly, so the buffer painted at frame
`n` holds frame `n-2`: the scene restores the background under that bar,
paints the new one, and reports as damage the on-screen bar (`n-1`) plus
the new one. After a resume or a new output the next two frames repaint
fully with full damage. In `static` mode the bar stops after 3 s and no
further commits happen — that is the idle state.

Shutdown order is the `Server` struct's field order: control clients,
socket file, backend (destroys FBs/dumb buffers, releases master with the
fd), the seat's `Device` (closes through the seat), the seat. Exit code 0.

## VT-switch contract

On `SeatEvent::Disable` the server stops touching the device (pause), then
acks; libseat holds the switch until then. On `Enable` it calls
`resume()` (one blocking `ALLOW_MODESET` commit) and invalidates every
scene. Ten round-trips on the test box: no warnings, +2 frames per resume,
fd count constant.

## Control protocol v0

Unix stream socket, one request per line, reply is a status line and an
optional body. Replaced by `nitro-wire` in M1; parsing/formatting lives in
`protocol.rs` with unit tests.

| request              | reply                                                                 |
|----------------------|-----------------------------------------------------------------------|
| `shot [output-name]` | `ok <width> <height> <stride>\n` + `stride*height` bytes `XRGB8888` (`read_front`) |
| `outputs`            | `ok\n`, one `name WxH@refresh_mhz\n` per output, blank line            |
| `stats`              | `ok\n`, `frames n`, `flips_pending n`, `uptime_ms n`, `active 0|1`, `flip_interval_{mean,min,max}_us n`, blank line |
| `quit`               | `ok\n`, then orderly shutdown                                          |
| anything else        | `err <message>\n`                                                     |

Several requests per connection are fine; a request line longer than 256
bytes without a newline drops the client.

## Testing

- Unit tests: protocol, renderer (age-2 damage), control buffering,
  logging, signals (skipped when the sandbox blocks SIGTERM).
- `tests/fake_loop.rs`: runs `run(Config::fake(..))` on a thread, checks
  `outputs`, exact `shot` pixels against `render::expected_color`, `stats`,
  a moving bar, many clients with partial lines, and that `quit` stops the
  thread and removes the socket.
- Hardware: `just deploy`, `just shot`, `just box-chvt 1|2`, `just box-stop`
  (see `docs/testbox.md`).
