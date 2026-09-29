# nitro-gpu

The GPU helper minus the GPU (#3920). Design: `docs/surfaces.md` §
GPU helper. The Vulkan backend is `crates/nitro-gpu-vulkan`. This crate
is `#![forbid(unsafe_code)]` and links no GPU API, so nitro-server can
use its protocol and client end as they are.

## Process model

The server starts `nitro-gpu-vulkan` with the helper socket as **fd 0**.
The helper:

1. refuses to start if it inherited a DRM primary node or an input device
   (`sandbox::check_fds`);
2. takes the socket off fd 0 and puts `/dev/null` there;
3. drops privileges (`sandbox::apply`): `no_new_privs`, non-dumpable,
   `RLIMIT_CORE = 0`, `RLIMIT_NOFILE ≤ 4096`, `chdir("/")`;
4. opens the render node (backend), then checks the fds again: the
   driver may only have opened `renderD*`;
5. runs `event_loop::run` until EOF, `Shutdown` or idle exit.

**Lifecycle.** Always-on is the default (`Config::idle_exit = None`).
On-demand mode sets `idle_exit`: the helper exits once it holds no
texture, has no frame in flight and has heard nothing for that long.
(`NITRO_GPU_IDLE_EXIT=<secs>` in the binary.)

**seccomp is deferred.** rustix has no seccomp, and installing a BPF
filter needs `prctl(PR_SET_SECCOMP, …, ptr)`, i.e. `unsafe` or libc. It is
the next hardening step. The allowlist is small: ioctl on the render node,
mmap/munmap, futex, memfd_create, poll, sendmsg/recvmsg, close, exit.

## Protocol

These are `nitro-wire` frames: an 8-byte header, and fds on the header's
`sendmsg`, at most 8 per frame. The helper uses its own op namespace on
its own socket. Every request that fails gets an `Error` with the op, the
id or serial, and a code, and the helper carries on. Only a broken
stream (bad framing) ends it.

| op | server → helper | fds | reply |
|---|---|---|---|
| 0x01 | `Hello{version}` | 0 | `HelloReply{version, device, driver, sampleable[], render[]}` |
| 0x02 | `ImportDmabuf{id, w, h, fourcc, modifier, encoding, range, planes[]{offset, pitch}}` | 1 per plane | `Imported{id}` |
| 0x03 | `ImportShadow{id, w, h, stride, fourcc XR24/AR24}` | 1 sealed memfd | `Imported{id}` |
| 0x04 | `UploadDamage{id, rects[]}` | 0 | none (staging path copies; udmabuf path no-op) |
| 0x05 | `AllocOutputRing{n ≤ 4, w, h, fourcc, modifiers[]}` | 0 | `OutputRing{w, h, fourcc, modifier, slots[]{offset, pitch, size}}` + n dma-bufs |
| 0x06 | `Composite{serial, out_idx, damage[], layers[]{tex, src f32×4, dst IRect, blend}, fence_mask}` | popcount(mask) sync_files | `Composited{serial}` + 1 sync_file, sent **right after submit** |
| 0x07 | `Release{id}` | 0 | `Released{id}` once every frame sampling it signalled |
| 0x08 | `ReadBack{out_idx}` (debug/test; waits for the GPU) | 0 | `ReadBackReply{w, h, stride}` + sealed memfd |
| 0x09 | `GetStats` | 0 | `Stats{frames, imports, errors, textures_live, in_flight, submit µs avg/max, shadow_path, drm_total, drm_resident, rss, pss}` |
| 0x0a | `Shutdown` | 0 | — |

Limits: 16 layers, 64 rects, 4 planes, 32 modifiers, 16384 px edges.
Encode checks the fd count against the message on the sending side too,
so the server cannot build a frame the helper would reject for its fds.

**Damage** is what changed since the *previous frame*. The helper adds
buffer-age damage itself (`ring::DamageRing`): the clip for slot *s* is
this frame's damage ∪ every frame's since *s* was last drawn. A slot that
was never drawn, or was reallocated, gets the full output.

**Slots.** The server must not reuse a slot whose last `Composited`
fence has not signalled. The helper refuses it with `Busy` rather than
block.

**Fences.** The helper keeps a dup of each frame's sync_file and polls it
next to the socket. When the fence signals, that frame's texture
references drop and its slot is free. Nothing in the helper waits on the
GPU except `ReadBack`, a re-alloc of the ring, and teardown.

## Modules

`proto` (codec), `client` (server end: `Conn`), `backend` (the trait:
`import_dmabuf`, `import_shadow`, `upload_damage`, `alloc_output_ring`,
`composite`, `release`, `readback`), `event_loop`, `ring`, `validate`,
`lifetime`, `sandbox`, `stats` (DRM fdinfo `drm-total-*`/`drm-resident-*`
per client id, VmRSS, Pss), `fake` (a GPU-less backend whose fences are
pipes the test signals).

## Tests

`cargo test -p nitro-gpu` runs the unit tests plus `tests/fake_transport.rs`,
which drives the loop over a real socketpair with the fake backend. It
covers round trips of every message, encode-side limits, hostile input
(truncated payloads, unknown ops, extra/missing fds, unsealed memfd,
unknown ids, out-of-bounds rects, no ring, bad slot, duplicate ids,
unsupported format) each answered with the right `Error` code, deferred
release ordering, busy slots, ring damage accumulation, EOF and idle exit,
and version mismatch.
