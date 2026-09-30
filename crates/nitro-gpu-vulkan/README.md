# nitro-gpu-vulkan

The GPU helper process (#3920) is the `nitro_gpu::Backend` on Vulkan. It
is **the only crate in the workspace with GPU bindings**, and so the only
one with their `unsafe`. The protocol, event loop, validation, lifetimes
and sandbox are in `crates/nitro-gpu`. The design is in `docs/surfaces.md`
§ GPU helper. Every Vulkan call sequence follows
`docs/research/gpu-testbox/vk_probe.c`, which verified it on both test
boxes.

## What it does per frame

There is one render pass (`LOAD`/`STORE`) into the ring slot. For each
clip rect it sets a scissor and then draws, in order, every layer that
the rect touches. Each layer is a 4-vertex strip whose dst rect (NDC) and
src rect (normalised) come in as push constants. There is one fragment
shader for every texture kind: the YCbCr→RGB conversion lives in an
immutable sampler (`VkSamplerYcbcrConversion`), one per (matrix, range),
so at most six. Alpha is forced to 1 for XR24 and NV12. `PremulOver` is
`ONE, ONE_MINUS_SRC_ALPHA`, and scaling uses linear filtering.

- Acquire sync_files are imported as temporary `SYNC_FD` semaphores that
  the submit waits on.
- The completion semaphore is exported as a sync_file and returned **at
  once**.
- It is also attached to the slot's dma-buf with
  `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`, so an implicit-sync reader (KMS without
  `IN_FENCE_FD`) waits for it as well.
- Foreign images (client dma-bufs, the udmabuf shadow, ring slots) are
  acquired from `VK_QUEUE_FAMILY_FOREIGN_EXT` at the start of every frame
  and released back to it at the end, in `GENERAL` layout. A driver
  without `VK_EXT_queue_family_foreign` (v3dv) uses
  `VK_QUEUE_FAMILY_EXTERNAL` instead (`Gpu::foreign_family`).

A slot that was never drawn is cleared to opaque black inside its first
render pass. The first clip is the full output (buffer age), so nothing
undefined survives.

## The shadow buffer: two paths

- **udmabuf (zero copy)**: memfd → `UDMABUF_CREATE` → dma-buf → LINEAR
  `B8G8R8A8` import. The GPU reads the server's pages directly, and
  `UploadDamage` does nothing. The memfd needs `F_SEAL_SHRINK` (which
  the sealed-memfd contract guarantees) and a page-padded size.
- **staging (fallback)**: the memfd is mapped read-only with
  `nitro_shm::Mapping`. `UploadDamage` copies only the damaged rects into
  a persistent host-visible buffer, then `vkCmdCopyBufferToImage` copies
  them into an optimal-tiled image.

The helper uses udmabuf when it can open `/dev/udmabuf` and falls back to
staging otherwise. `NITRO_GPU_SHADOW=staging` forces staging.
`Stats.shadow_path` reports which path is in use. On box1 the seat ACL
gives the logged-in user `/dev/udmabuf`. On testhost2 only the GDM seat
user gets it, so a helper started by the session will use udmabuf there
and a helper started over ssh will use staging.

## Vendor-ICD selection (`icd.rs`)

1. Take the render node from `$NITRO_GPU_RENDER_NODE`, or the first
   `/dev/dri/renderD*`.
2. Find its kernel driver: `/sys/dev/char/<maj>:<min>/device/driver`.
3. Map that driver to manifest names. i915/xe → `intel_icd.json` (anv),
   then `intel_hasvk_icd.json`. amdgpu → radeon; nouveau, msm → freedreno;
   panfrost; v3d → broadcom; asahi; virtio. **Never lvp.**
4. Search `/etc/vulkan/icd.d`, `/usr/local/share/vulkan/icd.d`,
   `$XDG_DATA_DIRS/vulkan/icd.d` and `/usr/share/vulkan/icd.d`.
   Debian installs multiarch-suffixed manifests
   (`broadcom_icd.armv8l.json`, `intel_icd.x86_64.json`), so each name
   also matches `<stem>.<arch>.json` with exactly that stem. The first
   directory with any variant wins. In it the exact name comes first, then
   the suffix of the helper's own build arch (`arch_suffixes`: armv8l,
   armv7l, armhf for 32-bit arm), then the others, sorted.
   `NITRO_GPU_ICD=<json>` replaces the whole lookup.
5. For each candidate, set `VK_DRIVER_FILES=<json>` (in `main`, before
   any thread exists) and open the device. If the candidate has no usable
   physical device (anv on Haswell), tear its instance down and try the
   next one.

The physical device is matched to the render node through
`VK_EXT_physical_device_drm` when the driver has it. Otherwise it is the
first device with the dma-buf/sync_fd extension set, which with a single
vendor ICD is that vendor's device.

## Broadcom V3D (Raspberry Pi 5 / 500, #4000)

Measured on testhost3 (Pi 500, armhf userland on an aarch64 kernel, Mesa
24.2.8 v3dv, V3D 7.1.10.2, Vulkan 1.2.289):

- **Manifests are suffixed.** `/usr/share/vulkan/icd.d` only has
  `*_icd.armv8l.json`. Before the suffix match the helper found no
  candidate and exited ("no vendor Vulkan driver").
- **No `VK_EXT_queue_family_foreign`.** v3dv has every other required
  extension (dma_buf, drm_format_modifier, external_memory_fd,
  external_semaphore_fd, and physical_device_drm, so the DRM-node match
  works). The extension is optional now, and ownership transfers use
  `VK_QUEUE_FAMILY_EXTERNAL`.
- **Render-only GPU, separate display.** `renderD128` and `card0` are
  `v3d`, and scanout is `card1` (`vc4-drm`). Ring slots are exported from
  v3d and imported into vc4 as dma-bufs. The helper advertises 4 render
  format/modifier pairs, and pixel test case 8 picks LINEAR
  (`ring modifier 0x0`). A live ring into the vc4 primary was **not**
  exercised: an idle desktop has no helper-able surface (`gpu_state 2`,
  `gpu_ring_slots 0`), and a LINEAR client dma-buf went straight to a
  plane. v3dv does not
  sample LINEAR NV12, so NV12 surfaces are not given to the helper there
  (they stay on the CPU/plane path).
- **v3dv's BO cache** keeps freed BOs (`V3DV_MAX_BO_CACHE_SIZE`), and
  they count in `drm_total`. Pixel test case 9 therefore warms up at
  1080p before sampling.
- `/dev/udmabuf` is `root:kvm 0660` there, so the shadow takes the
  staging path.
- Footprint: the helper starts in ~7 ms. Idle RSS is 7.5 MiB (PSS 3.6
  MiB). With a 3-slot 1080p ring, the 1080p shadow and a first frame it is
  15.5 MiB RSS and 41 MiB `drm_total`. The idle figure is within the
  vendor-ICD-only budget of #3903 (8.7–10.9 MB).
- All 11 pixel tests pass there with `NITRO_GPU_TEST=require`: cross-build
  them with `cargo test --no-run`, and put the helper at its build-time
  path on the box (`docs/testbox.md` §testbox3).

## `unsafe` inventory

The exception is crate-wide, and `#![deny(clippy::undocumented_unsafe_blocks)]`
means every block carries a `// SAFETY:` comment. It covers:

- **Vulkan FFI** (`device.rs`, `pipeline.rs`, `backend.rs`): every `ash`
  call. The invariants that recur: every create-info outlives its call;
  objects are destroyed only once their last submit's fence has signalled
  (the event loop keeps texture references until the frame's sync_file is
  POLLIN, and a slot is reused only after its `VkFence`); fds handed to
  `vkAllocateMemory`/`vkImportSemaphoreFdKHR` are transferred on success
  and closed by us on failure; fds from `vkGetMemoryFdKHR`/
  `vkGetSemaphoreFdKHR` are new and owned.
- **Mapped staging memory**: one `slice::from_raw_parts_mut` per upload,
  over our own persistently mapped host-coherent buffer, after waiting on
  its fence. Also one `from_raw_parts` in `readback`.
- **`sys.rs`**: `UDMABUF_CREATE` (an `Ioctl` impl whose output is the new
  fd) and `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` (`Setter`), each argument
  struct size-asserted against the kernel header.
- **`main.rs`**: one `env::set_var("VK_DRIVER_FILES")`, before any
  thread exists and before libvulkan is loaded.

## Shaders

`shaders/quad.vert` and `shaders/rgba.frag` are the GLSL sources. The
`.spv` next to them is checked in and loaded with `include_bytes!` plus
`ash::util::read_spv`, so the build needs no shader compiler.
Regenerate with `just gpu-shaders` (glslangValidator, run locally or on
the box).

## Tests

- `cargo test -p nitro-gpu-vulkan`: ICD unit tests (temp-dir trees) and
  `tests/pixels.rs`. Without a render node or a usable ICD, the pixel
  tests **skip with a printed note**; `NITRO_GPU_TEST=require` makes that
  a failure instead. NV12 cases also need `/dev/udmabuf` to build a
  LINEAR NV12 dma-buf from a memfd, and skip that part with a note when it
  is missing (`NITRO_GPU_TEST_NV12=require` fails instead).
- `just box-gpu-test` (and `just box=testhost2 box-gpu-test`) rsyncs the
  working tree to `~/tmp/nitro-gpu-test` on the box and runs everything,
  including the ignored `footprint` measurement, with
  `NITRO_GPU_TEST=require`. It uses only the render node, so the running
  session is untouched.

The pixel cases:

1. The shadow alone equals the shadow, on the udmabuf path and on the
   forced staging path.
2. A premultiplied AR24 shadow with a transparent hole over an NV12
   Surface. The hole shows NV12 (Y 128, U 100, V 160, BT.709 narrow →
   (188, 119, 71) ±2). The opaque shadow covers the rest of the NV12 quad.
   A 50 % red pixel blends as `src + (1 − a)·dst`.
3. 64×64 → 160×96 scaled with a colour split, which checks orientation
   and that the area outside dst is untouched.
4. Damage clip and ring age with two slots: frame 3 into slot 0 includes
   frame 2's damage, and pixels changed but not reported stay unchanged.
5. The returned sync_file becomes POLLIN.
6. `Release` of an in-flight 1080p texture answers only after the frame
   signalled.
7. A bad texture id, an out-of-bounds dst or an unknown modifier each get
   `Error`, and the next frame still renders correctly.
8. A ring modifier list {X_TILED, LINEAR}: the reply is one of them.
