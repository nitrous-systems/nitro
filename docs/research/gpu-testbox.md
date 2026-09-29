# GPU / media capabilities of the two test boxes

These are the results of the #3903 capability spike, run on 2026-09-29.
The spike checked whether the nitro-gpu helper's Vulkan minimum holds, what
EGL/GLES costs by comparison, what VA-API decodes, what the KMS planes can
scan out, and which GPU path Chromium picks. The design summary is in
[`../surfaces.md`](../surfaces.md).

Everything below is **measured** unless it is marked **estimate**. Probes are
small C programs, built on each box and run on the render node only (no DRM
master; `AddFB2` works without master). RSS/PSS figures are deltas in MB,
median of 3 runs, read from `/proc/self/status` and `/proc/self/smaps_rollup`.
PSS is the fairer comparison, because shared libraries dominate RSS.

| | **testbox** ([`../testbox.md`](../testbox.md)) | **testhost2** |
|---|---|---|
| hardware | Haswell GT1 (8086:0402), 2 cores, 3.3 GB | Kaby Lake R i5-8250U, UHD 620 (Gen9), 24 GB, eDP 2560×1440 |
| kernel / distro | 7.0, Ubuntu | 7.2.2, Arch |
| Mesa | 26.0.8 | 26.2.3 |
| Vulkan driver | `hasvk` | `anv` |
| GL driver | crocus | iris |
| VA driver | i965 2.4.1 (iHD 26.1.2 refuses Gen7.5) | iHD 26.2.4, libva 2.24.1 |

## Verdict

- **The helper minimum is met on both boxes.** Every functional check passes
  on hasvk as well as on anv. The difference is conformance: anv reports
  1.4.0.0, hasvk reports 0.0.0.0 and warns that "Haswell Vulkan support is
  incomplete". KBL/anv is therefore the development target. HSW is a
  "works, verified narrow slice" tier, with the CPU path as the safety net.
- **ICD restriction is mandatory.** With the loader's default ICD discovery,
  llvmpipe maps libLLVM (46.7 MB), and the helper pays 5–7× its real cost.
- **Vulkan costs far less memory than EGL/GLES on both boxes:** 8–11 MB PSS
  against 38–65 MB PSS. There is no case for a GLES backend.
- **Planes differ decisively.** KBL scans out VA's Y-tiled NV12 directly. HSW
  cannot scan out NV12 at all. The `planes` module has to be driven by each
  plane's `IN_FORMATS`, not by assumptions.
- **VA-API is the Intel decode API.** Neither box has Vulkan Video: anv has
  none on Gen9, and hasvk has none at all.

## Vulkan

| | HSW hasvk | KBL anv |
|---|---|---|
| apiVersion / conformance | 1.2.335 / **0.0.0.0** | 1.4 / **1.4.0.0** |
| queue families | 1: GRAPHICS\|COMPUTE\|TRANSFER, no video | same, **no video queues** |
| memory | 1 heap, 1.5 GiB DEVICE_LOCAL | — |
| `VK_EXT_external_memory_dma_buf`, `VK_KHR_external_memory_fd`, `VK_EXT_image_drm_format_modifier`, `VK_KHR_external_semaphore_fd`, `VK_EXT_queue_family_foreign`, `samplerYcbcrConversion` | all ✔ | all ✔ |
| also | timeline semaphore, sync2, dynamic rendering, external_fence_fd; maxImageDimension2D 8192 | — |
| `VK_KHR_video_queue` / `video_decode_*` | absent | absent |
| SYNC_FD semaphore props | export ✔ import ✔ | export ✔ import ✔ |
| B8G8R8A8 modifiers | LINEAR, X, Y (sampled + color_attachment) | LINEAR, X, Y, **Y_CCS** (2 planes) |
| NV12 (G8_B8R8_2PLANE_420) modifiers | LINEAR, X, Y; sampled, ycbcr midpoint/cosited, linear filter | LINEAR, X, Y |
| YUYV (G8B8G8R8_422) modifiers | none | — |

"—" means the item was not reported for that box.

### Helper minimum, checked end to end by `vk_probe full`

| requirement | HSW hasvk | KBL anv |
|---|---|---|
| dma-buf import with a DRM modifier: VA NV12 (Y_TILED, 2 planes, explicit modifier, dedicated import) → VkImage | PASS | PASS |
| NV12 sampling via `VkSamplerYcbcrConversion` (BT.709 narrow, Y=128 U=100 V=160) → render pass → readback | PASS, (188,119,71) exact | PASS, (188,119,71) exact |
| render into an exportable scanout image: modifier list {X_TILED, LINEAR} → driver picks X_TILED → KMS AddFB2 | PASS (X_TILED, pitch 7680) | PASS (X_TILED); a Y_TILED RT also passes |
| SYNC_FD import: `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` on the VA buffer → `vkImportSemaphoreFdKHR` → wait | PASS | PASS |
| SYNC_FD export: `vkGetSemaphoreFdKHR` → poll → `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` onto the RT | PASS | PASS |

hasvk logs `FINISHME: support YUV colorspace with DRM format modifiers`
(anv_formats.c), but the readback is exact.

### Cost

| | HSW hasvk | KBL anv |
|---|---|---|
| Δ start → first submit (instance, device, 1080p exported RT, clear), vendor ICD only | **+8.7 MB RSS / +6.6 MB PSS** | **+10.9 MB RSS / +8.4 MB PSS** |
| Δ full NV12 chain (from the VA baseline: import, YCbCr pipeline, draw) | +9.9 / +9.4 MB | +12.8 / +12.3 MB |
| same, default ICD discovery | instance alone: **63 MB RSS / 59 MB PSS** (lvp → libLLVM) | 32 MB / 27 MB (hasvk + anv + radeon loaded; lvp not installed) |
| instance / device / first submit, warm | 5.3 / 2.9 / 7.8 ms | 6.6 / 2.1 / 3.8 ms |
| instance, default ICD set | 26 ms warm, **169 ms cold** | — |
| NV12 pipeline creation | 2.5–3.4 ms | — |

On HSW, `libvulkan_intel_hasvk.so` accounts for 3.35 MB RSS, `libvulkan.so`
for 0.56 MB and memfd for 0.9 MB. The shader cache makes no measurable
difference. The #3894 estimate of "~50–200 ms" for instance + device creation
is only reached in the cold, all-ICD case. Restricted to the vendor ICD it
takes about 8 ms.

**ICD restriction.** Choose the ICD from the render node's kernel driver
(`drmGetVersion`: i915 → `intel_hasvk` on Gen7/7.5, `intel` (anv) on Gen8+)
and set `VK_LOADER_DRIVERS_SELECT` / `VK_DRIVER_FILES` before
`vkCreateInstance`, or dlopen the ICD and call `vk_icdGetInstanceProcAddr`
directly. The helper must never use llvmpipe anyway: CPU compositing is
`nitro-raster`'s job. On a distro that ships lvp, KBL would pay the same
~60 MB as HSW (**estimate**).

**Render-target modifiers.** Pass the target plane's `IN_FORMATS` modifiers
as the image's modifier list. On HSW that is X_TILED/LINEAR only (never
Y_TILED), which keeps the RT scanout-capable.

**radv / other hardware:** not measured. radv doesn't link LLVM by default
(ACO), so similar single-digit-MB costs are expected (**estimate**).

## EGL / GLES (comparison only)

| | HSW crocus | KBL iris |
|---|---|---|
| version | GLES 3.2, GL 4.6 core | — |
| EGL: `EXT_image_dma_buf_import(_modifiers)`, `ANDROID_native_fence_sync`, `KHR_fence_sync`/`wait_sync`, `MESA_image_dma_buf_export`, `KHR_surfaceless_context`, `KHR_no_config_context` (surfaceless and GBM) | all ✔ | — |
| GL: `OES_EGL_image_external(_essl3)`, `EXT_EGL_image_storage`, `EXT_memory_object_fd`, `EXT_semaphore_fd` | ✔ (`EXT_YUV_target` ✗) | — |
| NV12 VA dma-buf → EGLImage → external OES draw; native fence fd | PASS, exact | PASS |
| Δ start → first draw | **+67 MB RSS / +65 MB PSS** (surfaceless and GBM alike) | **+41 MB / +38 MB** |
| Δ full NV12 chain | +73 / +72 MB | — |
| eglInitialize / context | 23–26 ms / 2.7 ms | — |

On HSW, crocus lives in libgallium (11.9 MB), which links libLLVM (46.7 MB).
The EGL path is functionally complete, but it costs about 7× the Vulkan path.

## KMS planes

| | HSW (`drm_info`, card1) | KBL (`modetest -p`) |
|---|---|---|
| per CRTC | primary, 1 sprite, cursor | 2 NV12-capable planes per pipe (primary + sprite); pipe C has no NV12 |
| primary formats | C8, RG16, XR24, XB24, XR30, XB30, XB4H; LINEAR, X_TILED | includes NV12 |
| sprite formats | XR24, XB24, XR30, XB30, XR4H, XB4H, **YUYV, YVYU, UYVY, VYUY**; LINEAR, X_TILED. **No NV12, no Y_TILED.** COLOR_ENCODING 601/709, COLOR_RANGE limited/full, rotation 0/180 | NV12, XYUV, YUYV family, AR24 |
| cursor | AR24, LINEAR | — |
| **AddFB2 of VA's NV12 (Y_TILED)** | **FAIL, EINVAL** | **PASS** |

On Gen9+ the zero-GPU "decoder dma-buf → plane" path works. On HSW, video on
a plane has to be packed 4:2:2 (YUYV/UYVY, linear or X-tiled; the display
engine still does YUV→RGB and scaling, while the client or CPU packs
NV12→YUYV) or XRGB (for example, helper output in an X-tiled RT). hasvk has no
YUYV format with modifiers, so the helper cannot *write* YUYV; its output is
XRGB. **Untested:** VA VPP (the `VideoProc` entrypoint) converting NV12 → YUY2
into an X-tiled/linear surface, which would be the zero-3D, zero-CPU video
path on HSW.

See also the #3895 `TEST_ONLY` inventory in
[`../../crates/nitro-kms/README.md`](../../crates/nitro-kms/README.md#planes-discovery-test_only-and-the-hsw-gt1-inventory-measured).

## VA-API decode

| | HSW i965 2.4.1 | KBL iHD 26.2.4 |
|---|---|---|
| VLD profiles | H.264 CBP/Main/High (+MVC/Stereo High), MPEG-2 Simple/Main, VC-1 S/M/A, JPEG baseline | H.264, MPEG-2, VC-1, JPEG, **VP8, HEVC Main/Main10, VP9 Profile 0/2** |
| missing | VP8, VP9, HEVC, AV1 | AV1 |
| other entrypoints | EncSlice (H.264, MPEG-2), VideoProc | 11 encode entrypoints |
| `vaExportSurfaceHandle` DRM_PRIME_2 (composed, 1080p NV12) | 1 object, 3 133 440 B, modifier `0x0100000000000002` = I915_FORMAT_MOD_Y_TILED; Y pitch 1920 off 0, UV pitch 1920 off 2 088 960 (height aligned to 1088) | NV12, Y_TILED |
| 1080p30 H.264 High 8 Mb/s, real time (`-re`, 10 s): SW (`-threads 2`) vs VA-API | **≈60% vs ≈5%** of one core | ≈20% vs ≈2.5% |
| same, as fast as possible | SW ≈215 fps, VA-API ≈570 fps | — |
| 1080p30 VP9 6 Mb/s, real time: SW vs VA-API | ≈65% vs n/a (no profile) | ≈29% vs ≈2.4% |

AV1 is software on both boxes. AV1 1080p via dav1d on the HSW Pentium was not
measured; it is likely marginal on 2 cores without AVX2 (**estimate**).

Consequence for `nitro-media`: VA-API is the primary hardware backend on
Intel Gen7–11, Vulkan Video applies to newer hardware, and software is the
fallback. Without VA-API the reference box would play H.264 at 12× the CPU
cost.

## Chromium

| | HSW, chromium 156.0.8067.0 (`~/nitro-bin/chromium`) | KBL, Arch chromium 153 |
|---|---|---|
| default, `--headless=new --ozone-platform=headless` | ANGLE on **SwiftShader**; compositing disabled_software, WebGL unavailable_software. GPU process 75.8 MB RSS / 28.0 MB PSS | SwiftShader |
| `--ignore-gpu-blocklist` | same as default | — |
| `--use-angle=gl-egl` | **fails**: "Initialization of all (0) EGL display types failed" | — |
| `--use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE` | hardware: "ANGLE (Intel, Vulkan 1.2.335 (HSW GT1), Mesa 26.0.8)", Skia GaneshVulkan, compositing/raster/WebGL/WebGPU on; not blocklisted when forced. GPU process **144 MB RSS / 88 MB PSS** (all ICDs) | ANGLE Vulkan on anv, compositing on, **video_decode enabled**, Skia GaneshGL |
| same + `VK_DRIVER_FILES=…/intel_hasvk_icd.json` | same result, **90 MB RSS / 35 MB PSS** | — |
| video_decode | disabled_software in every configuration | enabled with the Vulkan flags |

chrome://gpu was read over CDP (`SystemInfo.getInfo`, see `cdp.py`). The
nitro Chromium wrapper should export a restricted ICD selection whenever it
enables Vulkan.

## Reference probes

The sources live in [`gpu-testbox/`](gpu-testbox/). They are copied verbatim
from the #3903 task messages, and `vk_probe.c` is the reference for #3901.

| file | what |
|---|---|
| `probe.h` | RSS/PSS checkpoints (`cp()`), timing, `RESULT` lines, expected BT.709 colour |
| `va_nv12.h` | creates a filled 1080p NV12 VA surface and exports it as DRM_PRIME_2 |
| `vk_probe.c` | the helper minimum: VA NV12 import, YCbCr sample, render into an exported scanout-modifier RT, AddFB2, SYNC_FD both ways |
| `tri.vert`, `nv12.frag` | full-screen triangle; the fragment shader samples through the immutable YCbCr sampler |
| `egl_probe.c` | EGL/GLES comparison: dma-buf → external OES, native fence fd |
| `cdp.py` | dependency-free CDP client that prints chrome://gpu data |

Build on the box (needs the Vulkan, EGL, GLES, GBM, libva and libdrm -dev
packages, plus glslang):

```sh
glslangValidator -V tri.vert -o tri.vert.spv
glslangValidator -V nv12.frag -o nv12.frag.spv
gcc -O2 vk_probe.c -o vk_probe -I/usr/include/libdrm -lvulkan -lva -lva-drm -ldrm
gcc -O2 egl_probe.c -o egl_probe -I/usr/include/libdrm -lEGL -lGLESv2 -lgbm -lva -lva-drm -ldrm
```

Run:

```sh
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/intel_hasvk_icd.json ./vk_probe rss   # or: full
PLAT=surfaceless ./egl_probe full                                            # or: PLAT=gbm
```

- On KBL, use `intel_icd.json`.
- `RT_MODS=x|linear` restricts the RT modifier list.
- `HOLD=1` sleeps 3 s at the end, so per-library RSS can be read from
  `/proc/<pid>/smaps`.
- The probes open `/dev/dri/renderD128` and `/dev/dri/card1`. Adjust these
  paths on other machines.
- For chrome://gpu, start chrome with `--remote-debugging-port=9333` and run
  `python3 cdp.py`.
