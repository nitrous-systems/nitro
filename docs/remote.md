# Remote apps: the same wire over TCP

M4-E1. An app runs on machine **B** and connects to the nitro server on
machine **A**, sending the same mutation stream it sends over the Unix
socket. The window appears on A's screen, A's keyboard and pointer drive
it, and A's window manager decorates, focuses and `Alt+Tab`s it like any
other window. Nothing about the protocol changes.

```console
box$  echo 'remote.listen = 127.0.0.1:7700' >> ~/.config/nitro/server.conf
dev$  ssh -L 7700:127.0.0.1:7700 box -N &
dev$  NITRO_SOCKET=tcp://127.0.0.1:7700 nitro-calc
```

That is the whole feature. It is X-forwarding-shaped, and deliberately so:
the implementation is one endpoint parser, one extra listener, one
capability bit and a refusal. The pieces are
`crates/nitro-wire/src/endpoint.rs` (the `tcp://` form),
`crates/nitro-wire/src/io.rs` (the socket), `crates/nitro-server/src/remote.rs`
(the listener) and the `remote.listen` key in `docs/settings.md`.

> **No pixels cross the link.** An image's pixels ride on a file
> descriptor, and a descriptor cannot cross TCP. Everything a client
> *describes* — rects, text, layout, state — crosses fine, and that is
> most of a UI.

## Why this is barely any code

`docs/wire.md` §Transport has said since M1 that the framing does not
depend on the socket being local, and `caps::REMOTE` — "the link is
remote: buffers are expensive, text is cheap" — has been bit 3 since M1
as well. This milestone is the first time either claim was *tested*.
They held. The messages are little-endian `repr(C)` bodies behind an
8-byte header, with no pointers, no padding and no alignment
requirements, so a stream socket is a stream socket.

What did **not** hold for free is the descriptor channel, and the rest of
this document is mostly about that.

## The security model: loopback plus an SSH forward

**There is no authentication. None.** A process that can open the port
is a client, and a client can put windows on the user's screen, ask for
keyboard focus and read every key that goes to its own windows.

So the supported configuration is:

```text
remote.listen = 127.0.0.1:7700
```

plus `ssh -L 7700:127.0.0.1:7700 host`. Authentication happens in
**sshd**, which already has an answer to who you are, how you proved it
and what the transport encryption is. nitro's job is to be a display
server, and a display server that grows its own key exchange, its own
cipher negotiation and its own account model is a display server with
three new attack surfaces and no reviewers.

A non-loopback bind — `0.0.0.0:7700` — logs a `warn` **at every bind**,
saying exactly this. (At every bind, not literally once per process: a
reload that changes the address rebinds and warns again, which is what
you want — the warning is about the address in force, and the reload
that introduced it is the moment to say so.) It is for **measurement on
a trusted LAN**, which is how the numbers below were taken, and it
should not be left on. Nothing in the tree enables it, and
`remote.listen` is absent by default, so a desktop that does not want
remote clients has no TCP socket at all: no bind, no epoll registration,
no accept path.

If a future milestone wants real remote access without ssh, the shape is
TLS with a client certificate, or a token in the handshake — a new
capability bit and a new op, on a frozen v1. It is not this task.

## What does not work remotely, and why

| | works remotely? | why |
|---|---|---|
| rects, borders, corners, fills, transforms | **yes** | described in the message, no descriptors |
| text (`SetText`, `MeasureText`) | **yes** | the server owns the fonts; a string is bytes |
| window management, focus, `Alt+Tab`, decorations | **yes** | server-side, and the server does not care where a client is |
| input: pointer, keyboard, touch | **yes** | events go out on the same socket |
| `CreateBuffer` / `BufferDamage` / `Image` | **no** | a buffer *is* a file descriptor |
| the wallpaper | **no** | it is an `Image` and nothing else |
| the shell socket (`caps::SHELL`) | **no** | the privilege is having opened a `0700` path |
| `hey` against a remote app | **local to the app's machine** | the introspection socket is the app's, not the server's |

### Buffers

`CreateBuffer` passes a memfd over `SCM_RIGHTS`. There is no TCP
equivalent, and there is no honest fallback: streaming the pixels inline
would mean a 2048×2048 `AR24` image is 16 MiB on a link whose whole
point is that it is slow, and `MAX_PAYLOAD` is 16 MiB precisely so that
cannot happen by accident (`docs/wire.md`).

So it is refused, in three places, each doing a different job:

1. **`Connection::send`** returns `Error::RemoteNoFds` for an
   fd-carrying message on a remote link, **before encoding**. Nothing is
   queued and nothing is written, so the connection is exactly as it was.
2. **`Socket::send_once`** refuses again if one somehow got queued. A
   frame whose header declares descriptors that never arrive leaves the
   receiver waiting for bytes that do not exist, and a desynchronised
   stream is a dead connection. This is the backstop that makes the
   invariant "no half-frame ever reaches the wire" true rather than
   merely intended.
3. **The server** answers a remote buffer op with
   `Error { BadBuffer, "buffers are not available on a remote link" }`
   and **keeps the client**. A client that ignored `caps::REMOTE` is
   better served by an explanation than by a dead socket; only its image
   is missing.

**All three buffer ops, not just the one carrying a descriptor.** Only
`CreateBuffer` passes an fd; `BufferDamage` and `SetImage` merely *name*
a buffer — but a remote client can never have registered one, so all
three are equally impossible and all three get the same sentence. An app
does not send `CreateBuffer` alone: it sends the buffer, then the
`SetImage` naming it, then `BufferDamage` when the pixels change. If
only the first were survivable, a client would hear the clear
explanation and then be disconnected two messages later by `no buffer
with id 1` — the worse error, arriving after the recoverable one.

The one exception is `SetImage` naming `BufferId::NONE`, which is how an
image node is **cleared**: it names no buffer, needs none, and is the
one op in this group a remote app may legitimately send. The check looks
at the field, not just the op code.

A frame that *declares* descriptors on a remote link is a different
thing and stays fatal (`DecodeError::MissingFd`): the peer and the
receiver disagree about the byte stream, and there is no way back.

`nitro-ui` acts on the bit for you, and the shape of that is the point:
`Ui::is_remote()` is true, and `PaintCx::upload_image` returns **`None`,
not an error** — before it creates the memfd, so a remote app that
paints an image every frame does not allocate one every frame to throw
away. An `Image` widget already draws nothing for `None`, so the app
**carries on drawing everything else** and only loses its picture.

That distinction is load-bearing rather than stylistic. Routing the
refusal through `PaintCx::note` like an ordinary error would fail the
paint pass, and a failed paint pass ends `Ui::flush`, which ends
`App::run`: a remote app with one `Image` anywhere in its tree would
**exit on its first paint** instead of drawing the rest. "There is no
buffer here" is a permanent property of the connection, not something
that went wrong, and it is reported the way the toolkit already reports
the absence of a buffer. Every *other* failure — a memfd that will not
open, a broken socket — still goes through `note` and still fails the
pass. `a_remote_app_with_an_image_keeps_running_and_draws_the_rest` in
`crates/nitro-ui/tests/remote.rs` pins it, with a local control beside
it so the test means "remote" and not "images are broken".

`nitro-calc`, `nitro-term`, `nitro-files` and `nitro-settings` run
remotely **unmodified** — none of them draws an image.

`nitro-wallpaper` is the one app that cannot run remotely, and it is
honest about it rather than coming up blank: it *is* an image, drawn
edge to edge, and there is nothing left of it without one. It also needs
the shell socket, so it fails for two independent reasons.

### The shell socket is never TCP

`caps::SHELL` is granted because the client could open a `0700` path,
which proves it runs as the user (`docs/shell.md`). A TCP port proves
nothing of the sort, so `NITRO_SHELL_SOCKET=tcp://…` is an **error**,
not a connection, and a remote client never gets `SHELL` no matter what
it asks for. A remote bar, launcher or wallpaper is therefore not
possible today; a remote *application* is.

### `hey` is local to the app

The introspection socket (`docs/introspection.md`) lives in the **app's**
`$XDG_RUNTIME_DIR`, because it belongs to the app and not to the server.
So a calculator running on machine B is `hey`-addressable **on machine
B**, while its window is on machine A. That is the right split — it is
the app's widget tree being inspected, not the server's scene — and it
is also how the acceptance test read the answer back:

```console
box$  ydotool type '7*6'; ydotool key 28:1 28:0      # A's keyboard
dev$  hey nitro-calc get display                      # B's socket
value   42
```

`nitro-settings`' control-socket features will report "no server socket"
on the remote machine, and that is honest: the server it would configure
is on the other end of a TCP connection, and configuring a display from
a machine that does not have it is a different feature.

## The endpoint

`NITRO_SOCKET` takes `tcp://host:port`. Anything without that prefix is
a path, verbatim, exactly as before — including an absolute path with
colons in it, which is why the check is a prefix and not a "does this
look like host:port" guess.

| form | meaning |
|---|---|
| `tcp://127.0.0.1:7700` | IPv4 literal |
| `tcp://[::1]:7700` | IPv6 literal, **bracketed** |
| `tcp://box.local:7700` | a name, through `ToSocketAddrs`; every address is tried in order, first that connects wins |
| `/run/user/1000/nitro/wire.sock` | a path, as always |

An unbracketed IPv6 literal (`tcp://::1:7700`) is refused rather than
mangled: splitting at the last colon would make it "work" by accident,
and `::1:2:3` has no defensible reading. A value that claims to be TCP
and is not usable is an error at connect time; a value that is not TCP
is a path, and a bad path fails as `No such file or directory`, which is
what the user can act on.

## TCP options, and what they are worth

**`TCP_NODELAY`, both ends.** Set in `Socket::from_tcp`, so a client
connecting and a server accepting both get it.

**`SO_KEEPALIVE` with `TCP_KEEPIDLE 10`, `TCP_KEEPINTVL 5`,
`TCP_KEEPCNT 3`**, on the server's accepted sockets only. The server is
the side holding resources — windows on a screen — for a peer that may
never speak again, and without keepalive a yanked cable leaves them
there until the server restarts: TCP itself never notices, because
neither end sends anything to an idle app.

**`SO_REUSEADDR`** on the listener, so a server restarted while a
previous connection sits in `TIME_WAIT` comes back up instead of failing
with `EADDRINUSE`. It does **not** let two servers share a port; that
would need `SO_REUSEPORT`, which is deliberately not set.

### The Nagle measurement is a negative result

`TCP_NODELAY` is a latency claim, and a latency claim with no
measurement behind it is a comment. `NITRO_TCP_NODELAY=0` turns the
option off so the A/B is the **same binary, same window, same machine
pair, one sockopt flipped** — not "TCP vs Unix", which differs in far
more than one option.

Five interleaved pairs on the LAN (dev box → test box, 120 real pointer
moves each, `nitro-demo --follow`):

| | i2p median, five runs | p95 |
|---|---|---|
| `TCP_NODELAY` on | 9 232 / 9 496 / 9 802 / 10 205 / 10 629 µs | 15.6–18.8 ms |
| Nagle on | 10 253 / 10 300 / 10 790 / 11 424 µs | 15.6–17.6 ms |
| local, same box, same evening | **8 664 µs** | 17.3 ms |

**Mixed signs. Not resolvable in this workload**, and the first pair had
Nagle *faster* — which is exactly how a 10 % "win" gets reported off one
run. The mechanism says why: `--follow` commits about 4.7 times a
second, so every write is alone on the wire with nothing to coalesce and
the previous one long since acknowledged. Nagle holds a *second* small
write pending an ACK; a protocol that sends one and waits never meets
it.

The option stays on. It costs nothing, and the traffic it does protect
is real — the one burst in the demo's life is the handshake, and there
it shows:

| | connect → first `Presented` |
|---|---|
| `TCP_NODELAY` on | **8.7, 9.2, 12.3, 21.6, 31.3 ms** |
| Nagle on | 18.8, 19.0, 20.1, 25.7, 31.3 ms |

Same direction in four of five pairs, and the mechanism fits: the
handshake and the first transaction are back-to-back small writes, which
is Nagle's actual target.

## The numbers

Test box (Pentium G3240, 1920×1080@60, `docs/testbox.md`) as machine A;
the dev box (128 cores) as machine B; 1 Gb LAN.

| | measured |
|---|---|
| i2p median, remote over LAN | **9.2–10.6 ms** (local, same evening: 8.7 ms) |
| `nitro-term`, `seq 1 1000000`: **bytes on the wire** | **43 185 B** out, 37 862 B back, for **6 888 896 B** of terminal output — **0.63 %** |
| `nitro-files`, first paint of `/usr/share` | remote **9.2 ms** (local on the box: 10.3 / 18.4 / 22.7 ms) |
| server RSS, listener idle vs no listener | **below this box's resolution** |
| `nitro-server` binary growth | 2 199 080 → **2 219 336 B (+19.8 KB, +0.92 %)** |
| remote app `SIGKILL`ed | window gone within one loop turn |
| link blackholed (a yanked cable) | **17 s** to declare the peer dead |

**The wire-byte figure is the one that says something about the link.**
A million lines of terminal output become about 30 frames of `SetText`,
not 6.9 MB of pixels — the mutation stream describes what the screen
should look like, and a screen that is overwritten 33 000 times only has
to be described once per frame. That ratio is why this design forwards
usefully over a link where pixels would not.

**The `seq` wall times are deliberately not quoted as a comparison.**
The VT parsing happens on whichever machine the *app* runs on, so remote
(0.35 s, 128-core dev box) against local (0.55 s, Pentium G3240)
measures the two CPUs and says nothing about TCP.

**Server RSS is "below resolution", not a number.** Three interleaved
restart pairs gave −700, −20 and +32 kB against an off-side spread of
764 kB. Two of three are inside ±35 kB, which is what the mechanism
predicts — one listening socket and a parsed `SocketAddr` — but the
spread is larger than the effect, so quoting a mean would be inventing
precision. An idle listener costs one fd and no wakeups, on the same
terms as the config watch and the defer timer.

**`nitro-files` first paint appears *faster* remotely**, and that is not
a link result either: reading `/usr/share` happens on the client's
machine, and the dev box's page cache and 128 cores beat the Pentium's.
The honest reading is that the remote path adds nothing measurable to a
first paint dominated by a directory read.

### Disconnection

Three ways a remote client goes away, and they are not the same:

| | how long |
|---|---|
| the app exits or is `SIGKILL`ed | one loop turn — the kernel sends FIN/RST and the socket is readable at once |
| the ssh forward is stopped | **0.6 s** — also a clean FIN, so this is the close path again |
| the link blackholes (cable, NAT timeout, crashed router) | **17 s** — keepalive, and the only case that needs it |

The middle row is worth stating because it is the test one is tempted to
call a "yank": killing the forward looks like pulling a cable and is
nothing of the sort. To actually measure keepalive the packets have to
be *dropped*, so the acceptance run blackholed port 7700 in both
directions with `iptables -j DROP` and timed it: **17 s**, against a
theoretical 10 + 2×5 = 20 s worst case, because the idle timer had
already partly run. The harness test covers the RST/close case, which is
deterministic; the keepalive window is what this measurement is for.

Either way the outcome is identical to a Unix hangup: the client's
windows are destroyed, focus moves, `clients` and `remote_clients`
decrement. A remote client is not a special case in `disconnect`.

## Operating it

`remote.listen` is applied at startup **and on every reload** (see
`docs/settings.md`), so turning remote apps on does not need a restart.

- **Adding the key** binds the listener and logs `remote wire socket at tcp://…`.
- **Changing it** rebinds.
- **Removing it** closes the listener. Clients **already connected keep
  working**: a connection lives on the socket it was accepted on, and
  "no listener" means "no new connections".
- **An unchanged value does not rebind**, which is what keeps a reload
  that moved a monitor from disturbing a connected remote app.
- **A bad value** — a hostname, a bad port, an address already in use —
  is a `warn` and no listener. A typo in a config file must not cost
  someone their desktop.

`stats` on the control socket reports two things:

```console
$ printf 'stats\n' | nc -U $XDG_RUNTIME_DIR/nitro/control.sock | grep remote
remote_clients 1
remote_listen 127.0.0.1:7700
```

`remote_listen` is the **bound** address, with the port the kernel chose
for a configured `:0`. That is not a test affordance: someone who wrote
`remote.listen = 127.0.0.1:0` has exactly the same question, and "the
address in the file" would be a useless answer to it. It is `off` when
there is no listener.

## Endianness

The wire is little-endian everywhere — every field is a `zerocopy`
byteorder type, so a mixed x86-64/aarch64 pair is fine. That claim is
now **pinned rather than assumed**:
`the_wire_image_is_little_endian_and_pinned` in
`crates/nitro-wire/tests/tcp.rs` encodes a `SetBounds` and compares it
against a byte literal, so a field that ever becomes native-endian — or
is reordered, or padded — fails on the machine that built it rather than
on someone's ARM laptop talking to an x86 box.

## Two traps from the acceptance run

**`nc -q1 -U` drops replies.** On the test box a large fraction of
control-socket requests through `nc` returned *nothing at all*, which
reads exactly like "the key is absent" — the first reading after
enabling the listener looked like a broken reload, while `ss -ltnp`
showed the port LISTENing at that very moment. A twelve-line Python
client got 5/5 and `config_reloads 2`. If you are asserting that a key
is **missing** from `stats`, do not use `nc`.

**Dropping a node from a scene is two edits, not one.** `nitro-demo`
could not connect remotely at all (it builds a `CreateBuffer` for its
procedural image), and once that was fixed it died on the **first
`Configure`** with `UnknownNode`, because `reconfigure` still laid out
the image node the build had never created. The second edit lives in a
path that only runs when the window is resized, so it survived 61
passing tests and the first minute of the box run. Both are pinned now.
