# nitro-wire

The client↔server protocol: framing, messages, file-descriptor passing and
non-blocking socket io. Everything above it — toolkit, apps, the remote
view, the Wayland adapter — is layered on this crate, so it is small,
versioned and hand-written.

Two dependencies: `rustix` (sockets, `SCM_RIGHTS`, `poll`, `memfd`) and
`zerocopy` (`repr(C)` layout with validated, zero-copy decode). No serde,
no `unsafe`, no allocation per message beyond growing the reusable
buffers.

The full specification — transport, framing, every message with its field
table, error codes and the versioning policy — is in
[`docs/wire.md`](../../docs/wire.md).

## Shape

* A **frame** is an 8-byte header (`len u32`, `op u16`, `fds u8`,
  `flags u8`) plus a payload of fixed-layout fields.
* Clients send **mutations** on a tree of nodes with **client-allocated
  ids**; nothing is visible until `Commit`, which applies the batch
  atomically.
* The server sends configuration, input and frame timing back.
* Errors are **fatal**: the server sends `Error` and closes.

## Client

```rust,no_run
use nitro_core::{Color, Rect, Size};
use nitro_wire::client::Connection;
use nitro_wire::types::{Layer, NodeId};

# fn main() -> Result<(), nitro_wire::Error> {
// Connect and handshake (NITRO_SOCKET, or $XDG_RUNTIME_DIR/nitro/wire.sock).
let mut conn = Connection::connect_default("demo")?;

let win = NodeId(1);
let boxy = NodeId(2);

// One transaction, written straight into the send buffer.
conn.tx()
    .create_window(win, "demo", Size::new(400.0, 300.0), Layer::Normal)
    .create_rect(boxy, win, Rect::new(10.0, 10.0, 100.0, 50.0))
    .fill_solid(boxy, Color::rgb(0x33, 0x88, 0xff))
    .corners(boxy, 4.0)
    .request_frame(win)
    .commit(1)?;
conn.flush()?;

// Non-blocking receive: register `conn.as_fd()` with epoll, then drain.
let mut events = Vec::new();
conn.poll(&mut events)?;
for e in &events {
    println!("{}", e.name());
}
# Ok(()) }
```

`Connection::flush` returns `false` when the socket filled up; wait for
writability and call it again. `poll` appends into a caller-owned `Vec`,
so the event loop owns all the allocation.

## Server

```rust,no_run
use nitro_wire::msg::ClientMsg;
use nitro_wire::server::{self, ClientStream, Listener};

# fn main() -> Result<(), nitro_wire::Error> {
let listener = Listener::bind_default()?;
// Register `listener.as_fd()` with epoll; on readable:
while let Some(mut client) = listener.accept()? {
    // Register `client.as_fd()` too; on readable:
    client.read()?;
    while let Some(msg) = client.next_msg()? {
        match msg {
            ClientMsg::Hello(_) => client.welcome("nitro", 0)?,
            ClientMsg::Commit(c) => {
                // apply the accumulated transaction, atomically
                let _ = c.serial;
            }
            other => {
                // buffer the mutation until the next Commit
                let _ = other;
            }
        }
    }
    // On writable, or opportunistically:
    client.flush()?;
}
# Ok(()) }
```

`ClientStream` handles the handshake state machine itself: the first
message must be `Hello` with a matching version. Any error it returns is
fatal — answer with `client.fail(serial, server::code_for(&err), "…")` and
drop the client.

Sending goes through the same object (`send`/`flush`) rather than a
separate `Sender`: it already owns the socket and the outgoing buffer, so
splitting them would only add a second borrow to juggle in the epoll loop.
This is a deliberate deviation from the M1 sketch, recorded in
[`docs/wire.md`](../../docs/wire.md#deviations-from-the-m1-sketch).

`read()` is bounded (`READ_BUDGET`) so one busy client cannot starve the
event loop, and a hangup is only reported once nothing decodable is left —
the socket stays readable, so the next wakeup continues where it stopped.

## Buffers

Pixels never cross the stream. A client creates a memfd, passes it once
with `CreateBuffer` (one `SCM_RIGHTS` descriptor on that frame), points an
`Image` node at a region with `SetImage`, and announces changes with
`BufferDamage`. The server maps the descriptor read-only.

## Layout and tests

The byte layout is defined by the `#[repr(C)]` structs in `src/msg.rs` and
`src/wire.rs` — layout *is* the wire format, so there is no separate
serializer to drift. Tests cover: every message round-tripping, every
proper prefix of every message failing to decode rather than panicking,
100k pseudo-random frames through both decoders, byte-at-a-time framing,
fd binding across chunk boundaries, a memfd surviving a `socketpair`
round trip (verified by inode), the partial-write path with a 4 KiB
`SO_SNDBUF`, and a real handshake over a Unix socket including the
version-mismatch path.

The hostile-peer cases have their own tests, each verified to fail when
the guard is removed: unclaimed descriptors cannot accumulate
(`MAX_PENDING_FDS`, or the server leaks fds to `EMFILE`), one client
cannot monopolise a `read` (`READ_BUDGET`), a hangup does not discard
already-received messages, and a peer that dies mid-frame terminates the
loop instead of spinning.
