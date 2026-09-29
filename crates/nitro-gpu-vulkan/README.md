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
  and released back to it at the end, in `GENERAL` layout.

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
   `NITRO_GPU_ICD=<json>` replaces the whole lookup.
5. For each candidate, set `VK_DRIVER_FILES=<json>` (in `main`, before
   any thread exists) and open the device. If the candidate has no usable
   physical device (anv on Haswell), tear its instance down and try the
   next one.

The physical device is matched to the render node through
`VK_EXT_physical_device_drm` when the driver has it. Otherwise it is the
first device with the dma-buf/sync_fd extension set, which with a single
vendor ICD is that vendor's device.

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
