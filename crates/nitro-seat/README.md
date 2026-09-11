# nitro-seat

Safe wrapper over [libseat](https://sr.ht/~kennylevinsen/seatd/) (the
`libseat` crate, `default-features = false`). The server's ownership root
for every privileged fd: it opens the session (logind, seatd or builtin —
libseat decides at runtime, `LIBSEAT_BACKEND` overrides) and hands out
`Device` fds for `/dev/dri/cardN` and `/dev/input/event*`. No `unsafe`, no
logging: the crate returns data, the server logs.

## Contract

- **Event loop.** `Seat` is `AsFd`; register it level-triggered readable
  and call `Seat::dispatch()` when it fires. Events come back in order.
- **Disable / ack.** On `SeatEvent::Disable` stop touching every device
  (no DRM commits, no evdev reads), *then* call `Seat::ack_disable()`.
  libseat will not let the VT switch complete until the disable is
  acknowledged, and the crate never acks on your behalf — the server knows
  when its devices are quiescent, the wrapper does not. On
  `SeatEvent::Enable` devices are usable again (re-modeset).
- **Drop order.** `Seat` is created first and dropped last. A `Device`
  closes itself through its seat when dropped (`Seat::close_device` does
  the same but reports the libseat error). Dropping a `Seat` with live
  devices panics in debug builds; in release builds the orphaned devices
  can only leak their fd. The check is a runtime assertion rather than a
  `Device<'seat>` lifetime because a borrow would forbid calling
  `Seat::dispatch(&mut self)` while any device exists, which is the normal
  state of the server.
- **`is_active`** is libseat's latest word (last `Enable`/`Disable`),
  updated even before the event has been drained by `dispatch`.

## Testing

Unit tests cover what needs no seat. `tests/noop_backend.rs` runs the whole
API against libseat's `noop` backend (re-executing the test binary with
`LIBSEAT_BACKEND=noop`), so `just test` exercises open, dispatch,
open/close device, drop and the drop-order assertion in CI.

The real thing needs a logind session on a VT — the test box
(`docs/testbox.md`):

```sh
cargo build --release --example seat_probe \
  && rsync target/release/examples/seat_probe kaspar@192.168.1.204:nitro-bin/ \
  && ssh kaspar@192.168.1.204 'sudo systemd-run --wait --pipe -p PAMName=login -p TTYPath=/dev/tty2 --uid=kaspar ~/nitro-bin/seat_probe'
```

`seat_probe [DEVICE] [SECONDS]` prints the seat name, every event for
three seconds (acking disables), then opens `/dev/dri/card1` and closes
it. Use `--pipe`, not `--pty`: with `TTYPath` set, `--pty` sends the
process's stdout to tty2 itself. To watch a VT switch, give it a longer
window and `sudo chvt 1; sudo chvt 2` from another ssh while it runs.
Expected output:

```
seat "seat0" opened, active=false, poll fd=BorrowedFd { fd: 4 }
event Enable (active=true)
event Disable (active=false)
acknowledged disable
event Enable (active=true)
opened /dev/dri/card1 as DeviceId(0), fd=BorrowedFd { fd: 5 }
closed device; open_devices=0
seat closed
```
