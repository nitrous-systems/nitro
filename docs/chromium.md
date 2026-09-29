# Chromium on nitro: the Ozone backend (#3778, #3865, #3919)

The backend lives in the Chromium tree, not in this repo: `ui/ozone/platform/nitro/` on branch `nitro-ozone` of the Chromium checkout (`/home/kaspar/src/ai/chromium/src` by default). How to build it, and the `NITRO_CHROMIUM_OUT` override the justfile uses, is in [chromium-build.md](chromium-build.md). Its companion wire client is `wire/`, from #3777.

- **Registration:** `build/config/ozone.gni` (`ozone_platform_nitro`) and `ui/ozone/BUILD.gn`.
- **GN args:** `out/Default/args.gn` has `ozone_platform_nitro = true`, with headless kept.
- **Run:**
  ```
  chrome --ozone-platform=nitro --disable-gpu
  ```
  The GPU process runs out of process, as on every other Chromium (#3919).
  `--in-process-gpu` still works and brings back the #3778 path, in which the
  browser presents.
- **Size:** about 3,900 lines across `ozone_platform_nitro`, `nitro_connection_host`, `nitro_gpu_connection`, `nitro_presenter.h`, `nitro_surface_factory` (`NitroCanvas`), `nitro_gl_readback` (measurement only), `nitro_window`, `nitro_window_manager`, `nitro_screen`, `nitro_event_source`, `client_native_pixmap_factory_nitro`, `mojom/nitro_gpu.mojom`, `BUILD.gn` and `DEPS`.

## Architecture

- **Two nitro connections: the browser's and the GPU process's** (#3919, option B of [B vs C](#out-of-process-gpu-b-vs-c-3904)).
  - **Browser** (`NitroConnectionHost`, on the UI thread). It owns every window: geometry, visibility, stacking, input, `Configure`, popups and the opaque region. It watches its fd with `WatchFileDescriptor` and routes server events by window/node to `NitroWindow` and outputs to `NitroScreen`. Each window's content is a **`Surface` node**. After the commit that creates the node, the browser sends `ExportSurface`, and `SurfaceExported` returns a token. The browser hands the token to the GPU process over `ui.nitro.mojom.NitroGpu`, a three-method interface (`SetTranslucent`, `SetSurfaceToken`, `RemoveSurface`) bound through `GpuPlatformSupportHost::OnGpuServiceLaunched` → `OzonePlatform::AddInterfaces`. A popup is re-created on every show, so each show brings a new node and a new token; the old import is revoked with its node.
  - **GPU process** (`NitroGpuConnection`). It connects in `InitializeForGPU`, which runs before the seccomp sandbox engages, the same way X11 Ozone opens its display. The connection runs on its own IO thread (`NitroGpu`, `kPresentation` priority). For each token it sends `ImportSurface` under an id of its own and feeds the node with `PresentSurface` (the vblank latch: no transaction, newest frame wins). The canvas's buffers are ordinary sealed-memfd `CreateBuffer`s, and an XR24/AR24 buffer may be presented on a Surface. `BufferReleased` and `Presented` come straight back to this connection. **The browser never sees a pixel, a release or a present**, so the busy UI thread is off the present path.
  - **`SurfaceRevoked`** (a stale token, a replaced popup node, a re-export) finishes the import's pending frames so viz never waits. The id stays bound until the next token replaces it. A frame drawn before the token arrives is kept as the widget's current buffer and shown as soon as an import exists.
  - **A GPU process that loses its connection exits.** The browser restarts it, the tokens are still valid (they live as long as the node), and the browser replays them. The newest import wins over the dead process's (`wire.md` § Surface sharing).
  - **Resize:** the browser resizes the root and Surface nodes when views changes the size (it no longer sees the frame that carries the new pixels). Until the GPU process's first frame at the new size, the latch scales the old buffer into the new rect, so a drag-resize shows one stretched frame at most. In-process, the resize rides the frame.
  - **Cost of the split:** `CreateBuffer` is registered at the next `Commit` while `PresentSurface` acts at receipt, so the GPU process commits once per new buffer (3 per window size), not per frame.
- **`--in-process-gpu`** keeps the #3778 path: one connection, an **Image** node per window, and `NitroCanvas` on the in-process viz thread posts `CreateBuffer`/`SetImage`/`BufferDamage`/`Commit` to the UI thread. Both paths share `NitroCanvas`; it talks to a `NitroPresenter`, which is either `NitroConnectionHost` or `NitroGpuConnection`.
- **Server requirement:** `SURFACE` and `SHARE` (#3897, #3904). An older server makes the out-of-process path fail at startup with a message naming the missing caps; use `--in-process-gpu` against it.
- **Canvas:**
  - 3 shm buffers (`UnsafeSharedMemoryRegion`), with Skia rastering straight into the mapping via `SkSurfaces::WrapPixels`.
  - Per-buffer `SkRegion` dirty tracking, with stale damage copied forward from the last attached buffer (the Wayland scheme).
  - `SupportsAsyncBufferSwap() = true`.
  - A buffer is reused only after its `BufferReleased`.
- **Formats:**
  - Opaque windows use `XR24`.
  - Translucent windows (menus, tooltips, bubbles, `kTranslucentWindow`) use `AR24` plus a CPU unpremultiply of the damage rect, because nitro's AR24 is straight alpha and Skia N32 is premultiplied.
- **Input:**
  - `ui::MouseEvent`, `MouseWheelEvent` and `KeyEvent` are dispatched through `PlatformEventSource`/`PlatformEventDispatcher`, following the `platform_window_cast` shape.
  - The §5.3 traps are honoured: the target is set, `changed_button_flags` is filled, a released button's flag stays in `flags`, and motion with a button becomes a drag.
  - `XkbKeyboardLayoutEngine` uses the keymap fd from `Keymap`. Key repeat is client-side, via `ui::EventAutoRepeatHandler` fed from `Keymap.rate_hz`/`delay_ms`.
- **Properties:**

  | property | value |
  |---|---|
  | `custom_frame_pref_default` | true |
  | `supports_server_side_window_decorations` | false |
  | `set_parent_for_non_top_level_windows` | true |
  | `supports_global_screen_coordinates` | false |
  | `platform_shows_drag_image` | false |
  | `IsWindowCompositingSupported()` | true |
- **Capabilities:**
  - `ClientCaps = desired & Welcome.caps`, where desired is POPUP, CURSOR, DRAG, OUTPUTS, KEYMAP, RELEASE and DATA. Missing caps are logged.
  - The WM-cap-gated ops (`SetWindowState`, `SetWindowLimits`, `SetAppId`) are sent only when `Welcome.caps` has WM.
- **Serials:** dropped, per #3767.

## What works

All of this was verified on a fake-backend nitro server (1280x720@60), driven by synthetic `FakeInput` from a small ad-hoc harness (not kept; the control socket's `input` op, #3834, replaces it).

- **Rendering:** a real page renders with the correct CSD frame, tabs, omnibox and webfonts.
- **Mouse:** clicks, focus, and `<select>` open and choose.
- **Popups:**
  - The `<select>` dropdown opens as a nitro popup and is correctly anchored.
  - The right-click context menu opens with translucent rounded corners and shadow. FLIP_Y constraint handling is correct when the menu does not fit below.
  - The omnibox suggestion dropdown opens.
  - The "Restore pages" bubble shows and follows its parent window when the window moves.
- **Keyboard:**
  - Typing works, including Shift.
  - **Ctrl+L** focuses the omnibox, so modifiers are delivered.
  - **Holding a key** repeats client-side: 1.2 s held gave one `a` plus about 16 repeats.
  - The server reports `key_repeats 0`, so **no doubled repeat** (#3782 behaves as intended for KEYMAP clients).
- **Wheel scrolling** is smooth (numbers below).
- **Titlebar drag** moves the window via `StartMove`. Edge resize works via `StartResize`, and the resulting `Configure` redraws at the new size.
  - The `PointerLeave` sent at the start of a client drag is swallowed, so the drag isn't cancelled.
- **Minimize** (titlebar button): the window unmaps, chrome stays alive, and the server's frame count stays flat, so there is no busy loop and no stall.
- **Clipboard** (#3943): copy and paste both ways with native nitro clients, on testhost2. See [Clipboard](#clipboard-3943).

## What doesn't / is not verified

- **GPU rendering** without a readback: the zero-copy dma-buf path is the follow-up task "Chromium Ozone nitro: GPU rendering via dma-buf". Until then GPU raster exists only as the `NITRO_GPU_READBACK=1` measurement knob (see [Out-of-process GPU measurements](#out-of-process-gpu-measurements-3919)).
- **Drag-resize** can show one stretched frame while the GPU process catches up (see Architecture).
- **HiDPI:** nitro logical = DIP, buffers = physical px, scale from `OutputInfo`. Since #3940 every logical size Chromium sends (`CreateWindow.size`, `CreatePopup.size`, the root/Image/Surface `SetBounds`) is the exact float `px / scale`, divided rather than multiplied by `1/scale` and not ceiled. At 1.25, `f32(px/1.25)*1.25 == px` for every integer px below 4000, so the server maps the buffer back to exactly its own size. The server snaps window roots to whole device pixels, so the buffer is drawn 1:1 (opaque copy, no resampling) at a fractional scale; see §testhost2 (#3940).
- **Popup types:** `kPopup`/`kBubble` windows are nitro popups too; there is no subsurface equivalent. They are placed by the server, and only `kMenu` grabs.
- **Drag and drop:** not wired. `platform_shows_drag_image=false`, and DnD start is a no-op.
- **Real hardware:** not tested on a real (non-fake) backend. All numbers are fake-backend.
- **Raster time:** not measured on the Chromium side. The server-side paint/copy cost is below.
- **Restore after minimize:** not exercised by the harness, which has no way to un-minimize.

## Clipboard (#3943)

`NitroClipboard` (`nitro_clipboard.{h,cc}`, Chromium `nitro-ozone` at `6bdd793e3c`) is the `PlatformClipboard`, over the DATA ops ([wire.md § Data transfer](wire.md#data-transfer-caps-data)). The backend README in the Chromium tree has the details.

- **Copy:** `SetSelection` with the `DataMap`'s types in this order: `text/plain;charset=utf-8`, `text/plain`, `UTF8_STRING`, `STRING`, `TEXT`, `text/html`, `text/uri-list`, `image/png`, then the rest (Chromium's own `chromium/x-*`). The first two are what nitro-ui clients read. The server needs keyboard focus. Without it the copy stays local and goes out on the next focus-in.
- **Serve:** a `SelectionRequest` gets a sealed memfd. It is built on the thread pool, so an 8 MB copy never touches the UI thread.
- **Paste:** `RequestSelection` for the mapped type (`text/plain` maps to the best offered alias, as on Wayland). The `SelectionData` fd is read on the thread pool, non-blocking, with a 5 s idle timeout and a 64 MiB cap.
- **Ownership:** a `SelectionOffer` that isn't our expected echo means another client took the selection. The backend drops its data and fires the clipboard-changed callback, which drives Chromium's `ClipboardMonitor` observers.
- **Primary selection:** the wire has none, so `IsSelectionBufferAvailable()` is false.
- **Drag and drop:** not wired.
- **Server fix found on the box:** a `SetSelection` that raced a focus-out (seen when copying 8 MB) used to be a fatal `Error { Protocol }`, which killed the browser. It is now dropped, like `SetCursor` ([wire.md § SetSelection](wire.md#setselection--0x0305)). The backend treats echoes still pending at focus-out as maybe-lost and re-offers on the next focus.

`just box=testhost2 chromium-clipboard` (`deploy/chromium-clipboard.sh`) runs two browsers, nitro-term and nitro-files in a live session, and prints PASS/FAIL. On testhost2, 2026-09-29, 3 runs, all 7 checks passed each time:

| check | result |
|---|---|
| Chromium → nitro-term (Ctrl+Shift+V) | text arrives |
| nitro-ui textfield → Chromium | `text/plain` |
| rich selection, browser A → browser B | `text/plain,text/html`, `<b>` kept |
| same selection → nitro-term | plain text, no markup |
| file copied in nitro-files → page | `Files`, `files[0].name` = the file (via `text/uri-list`) |
| 8 MB text, A → B | all 8 388 608 bytes. B's worst `requestAnimationFrame` gap during the paste was 33–65 ms |
| owner SIGKILLed after copying, then paste in nitro-term | nitro-term stays alive, gets an empty paste |

## Measurements

Fake backend, 60 Hz. The fake flip takes about 21 ms, which is the throughput ceiling. Test: 150 wheel events of +15 then 150 of −15, 16 ms apart, on a 60,000 px page. Source: `nitro-frame` traces (`NITRO_TRACE=1`) plus the control socket's `stats` (`nitro-shot --stats`).

| pacing | fps | frame interval p50 / p95 / max | submit→present p50 | input→photon p50 / p95 / max |
|---|---|---|---|---|
| ack on `Presented` (plan) | 7–40 | 24 ms / – / stalls 0.5–1.9 s | 21.5 ms | about 39 ms |
| ack on `BufferReleased` (**default**) | 44.3 | 21.0 / 22.4 / 38 ms | 28.8 ms | 34.7 / 48.3 / 51 ms (n=110) |
| same, rebuilt final binary | 45.8 | 22.0 / 23.4 / 153 ms (2 intervals >25 ms) | 31.4 ms | 36.7 / 51.1 / 54.6 ms (n=104) |

- **Server side, during the scroll:**
  - paint mean 5.2 ms
  - copy mean 0.23 ms
  - damage mean 735k px per frame, i.e. **the full window every frame** (see the gaps below)
  - server i2p mean 30.7 ms
- **Verdict:** at roughly 45 fps with about 35 ms input-to-photon on a slow fake flip, software rendering feels interactive. Scrolling is smooth with no stalls under release pacing. The bottlenecks are the backend's flip and nitro repainting the whole image each frame, not Skia raster.

### Why pacing is on BufferReleased, not Presented (this reverses the plan)

- Acking the swap on `Presented`, with viz's `MaxFramesPending=1`, meant viz missed every other BeginFrame. `Presented` for frame N lands after the next BeginFrame deadline, which caps throughput and caused 0.5–1.9 s stalls.
- Acking when the buffer that frame N *replaced* gets `BufferReleased` happens at the end of the server wakeup that processed the commit. That is the earliest point at which a buffer is guaranteed to be free, and it keeps a 3-buffer ring flowing.
- `Presented` still feeds vsync timing, and a second ack is harmless.
- `NITRO_ACK=present` restores the old mode, and `NITRO_MAX_PENDING` (1–2) is an experiment knob.
- Presented-pacing cannot deadlock either way: the server acks immediately for unplaced windows (`lib.rs:~7565`) and acks stale serials on outputs that painted nothing (`lib.rs:~2446`).

## nitro-side gaps the audit missed

1. **No cross-client buffer or node sharing.** Ids are per-connection, so a GPU process could not present into a browser window, which forced `--in-process-gpu`. **Fixed by #3904** (`ExportSurface`/`ImportSurface`, caps `SHARE`, `wire.md` § Surface sharing) **and used since #3919**: the GPU process presents into the browser's windows, and `--in-process-gpu` is gone from the wrapper. The one server change the backend needed was `SetOpaqueRegion` on a `Surface` node (#3919), which keeps #3877's opaque copy for the translucent CSD window.
2. **No premultiplied ARGB format.** `AR24` is straight alpha, while Skia (and most toolkits) produce premultiplied pixels. The workaround costs a CPU unpremultiply per translucent frame, and copying unpremultiplied pixels forward between buffers is a subtle blending hazard. Suggest adding a premultiplied fourcc or a per-buffer flag.
3. **`BufferDamage` was ignored on buffer swap.** `SetImage` with a new buffer marked the whole node dirty, so every frame repainted the full image (735k px per frame measured, even for a caret blink). **Fixed by #3833**: `BufferDamage` is honoured when a same-size buffer is swapped in (see the caret-blink idle number under Test box).
4. **No input-injection command on the control socket.** `FakeInput` was test-only, so this work needed a custom harness. **Fixed by #3834**: the control socket has `input` (`nitro-shot --input "wheel 0 15 count=3"`) and `samples`.
5. **`PointerAbsolute` is normalised 0..1, not pixels.** This isn't a bug, but it is undocumented at the injection level and cost a debugging cycle.
6. **WM ops are gated on the v1 `Welcome` WM bit, not `ClientCaps`.** This is easy to miss: sending `SetWindowState` without it is a protocol error. Worth a line in `wire.md` next to `ClientCaps`.
7. **No way to restore a minimized window from the client side,** and nothing in the harness to do it. Fine for a compositor with a taskbar, but worth checking that the shell path works for foreign clients.

## Out-of-process GPU: B vs C (#3904)

Two ways to let Chromium's GPU process draw into a window that the browser process's connection owns:

- **C: a browser-side relay over Mojo, no nitro change.** This is Wayland's shape: `WaylandBufferManagerHost` in the browser and `WaylandBufferManagerGpu` in the GPU process, a mojom between them, a buffer registry on the host side and a proxy on the GPU side.
  - Porting it is several thousand lines of Chromium code.
  - Every frame takes an extra hop, GPU → browser UI thread → nitro. Each fd is passed twice, and the busy browser UI thread stays on the present path. Taking the UI thread off the present path is half the point of an out-of-process GPU.
  - `Presented`/`BufferReleased` would also have to be relayed back.
- **B: token attach in nitro.** The browser exports a Surface node of its window and gets an unguessable 16-byte token (`ExportSurface`). It hands the token to the GPU process in a small per-widget message. The GPU process opens its **own** nitro connection before the sandbox engages, as X11 Ozone's GPU process does with its own display connection, and redeems the token (`ImportSurface`). It then presents with `PresentSurface` on its own buffers and gets `Presented`/`BufferReleased` directly.
  - About 600–900 lines in nitro, most of it tests, and very little on the Chromium side.
  - Its costs are a bearer-token security model (same uid, local links only) and a larger protocol.
  - It also serves other split-process producers later, such as a decoder process presenting into a player window.

C is not cheaper, so **B**. Implemented in #3919; see Architecture. The browser keeps owning geometry, visibility, input and `Frame`/`Configure`. The GPU process only feeds the Surface's vblank latch (#3897): newest frame wins, whoever sent it. The exact rules (lifetime, revocation, interleaving, event routing) are in `wire.md` § Surface sharing.

One limit applies to both options: an out-of-process GPU does not reduce CPU on a 2-core box (see the next levers above). It is about Chromium's normal process model and sandboxing.

## Out-of-process GPU measurements (#3919)

`just chromium-bench MODE [RUNS]` (`deploy/chromium-bench.sh`) runs on the
box against the running session. It starts a fresh-profile chrome on a
1 500-row page, waits 12 s, samples PSS/RSS of the browser and GPU
processes while idle, runs `scroll-bench.py` (150 + 150 wheel events,
16 ms apart) and samples the chrome tree's CPU over the scroll. The arms:

| arm | flags |
|---|---|
| `inproc` | `--disable-gpu --in-process-gpu` (the #3877 baseline) |
| `oop` | `--disable-gpu` (**the default now**) |
| `gpu` | `NITRO_GPU_READBACK=1 --use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE`: ANGLE-on-Vulkan raster, then `glReadPixels` of the whole frame into the same shm canvas. **Measurement only**, and it runs out of process |

Each arm ran twice. All arms used the same server (this branch) and the
same `out/Nitro` chrome. The `gpu` arm's renderer, confirmed through CDP
`SystemInfo`, was `ANGLE (Intel, Vulkan 1.2 (HSW GT1), Mesa 26.0.8)` on box1,
hardware hasvk and not SwiftShader.

Chromium tree: `nitro-ozone` at `b06f29bbd7` (#3919's three commits on top of `8b9e445b34`).

### box1: Pentium G3240, HSW GT1, 1920×1080 **@60** (the panel was at 60 that day), window ~1050×830 device px

| arm | fps | frame p50/p95/max ms | i2p p50/p95/max ms | server paint mean | browser PSS / RSS | GPU proc PSS / RSS | tree PSS | chrome CPU |
|---|---|---|---|---|---|---|---|---|
| inproc | 61.6–61.7 | 16.7/16.7/16.7 | 25.1/32.4–32.5/33.1 | 1.50–1.53 ms | 135–138 / 291–293 MB | – | 487–488 MB | 60 % |
| **oop** | 61.9 | 16.7/16.7/16.7 | 24.9–25.7/32.2–32.6/32.9–33.3 | 1.44–1.48 ms | 123–140 / 274–290 MB | 30–31 / 113–116 MB | 491–505 MB | 62–73 % |
| gpu (readback) | 52.7–53.3 | 16.7/33.3/33.3 | 23.0–23.5/31.6–35.9/38–40 | 3.6–3.9 ms | 120–124 / 271–273 MB | 103–111 / 198–202 MB | 560–567 MB | 80 % |

Caret-blink idle with oop (focused `<textarea>`, 20 s): chrome tree **2 % CPU**,
**4 frames/s**, damage 14.7k px mean (the stats window still carried the
page load), paint 26 µs. That matches the in-process 1.8 % / 4 frames/s.

### testhost2: i5-8250U, UHD 620 (KBL), eDP 2560×1440@60

**Scale 1.25 (the box's usual setting), #3940.** Before #3940, every arm
was limited by the server rather than by Chromium. At a fractional
scale, a window at a logical position that is not a multiple of 4 had a
fractional device origin. On top of that, the out-of-process path
ceiled its logical size, so `ceil(px/1.25)*1.25 != px`. Either one
meant the ~1.4 Mpx Surface/Image was not drawn 1:1: #3877's opaque copy
never applied, and the whole window was blended and scaled (21–26 ms of
paint per frame, 14–16 fps in every arm). #3940 fixes both. The server
rounds each window root's device origin to a whole pixel
(`Scene::root_placement`), and Chromium sends `px/scale` exactly (see
HiDPI above). The same scroll, before and after (temporary nitro-dev
unit, `NITRO_SCALE=eDP-1=1.25`, 2 runs each):

| arm (scale 1.25) | fps | frame p50/p95/max ms | i2p p50/p95/max ms | server paint mean | server i2p mean |
|---|---|---|---|---|---|
| inproc, before | 16.2 | 33.3/33.3/33.3 | 42.1–42.7/48.8–49.5/62.7–70.1 | 20.9–21.2 ms | 42.3 ms |
| oop, before | 15.2–15.4 | 33.3/50.0/50.0 | 41.4–43.3/57.9–58.0/67.2–69.4 | 20.9–21.8 ms | 42.7–42.9 ms |
| **inproc, after** | 31.0–31.2 | 16.7/16.7/16.7–33.3 | 24.6–24.8/31.9–32.1/32.8–33.9 | 1.12 ms | 25.0–25.3 ms |
| **oop, after** | 31.5–31.7 | 16.7/16.7/16.7 | 24.7–25.5/32.0–32.8/32.7–38.2 | 1.00–1.02 ms | 25.1 ms |
| new server, old chrome: inproc | 31.4 | 16.7/16.7/16.7 | 24.7/32.0/33.2 | 1.11 ms | 25.1 ms |
| new server, old chrome: oop | 15.8 | 33.3/50.0/– | 41.1/59.1/73.1 | 17.1 ms | 36.3 ms |

(One oop run's frame max, 1.6 s, is the idle gap before the scroll.) Both
parts are needed. The in-process path already sent exact `px/scale`
bounds, so the server snap alone fixes it. The out-of-process path also
needs the Chromium change. After the fix, 1.25 matches scale 1: ~31 fps,
which is the event rate, as at scale 1 below. Memory and CPU are
unchanged. At **scale 1** after #3940: inproc 31.4 fps, paint 1.22 ms,
i2p p50 25.1; oop 31.5 fps, paint 1.02 ms, i2p p50 25.1. That is the
same as the table below, so there is no regression. Text at 1.25 is
Chromium's own device-resolution raster, copied 1:1, so it is exactly
as crisp as Chromium drew it. A `nitro-shot` crop is sharp. Against the
old chrome's near-1:1 resample (a buffer about 1 px off), the difference
is too small to see at 8× zoom: the Laplacian means are 0.210 and 0.213.
So the screenshot shows the text is crisp, but it does not prove the
copy path. The paint time does: 1 ms for a copy against 17–21 ms for a
scaled blend.

The pre-#3940 arms were also run at **scale 1**, which `server.conf`
restored afterwards:

| arm (scale 1) | fps | frame p50/p95/max ms | i2p p50/p95/max ms | server paint mean | browser PSS / RSS | GPU proc PSS / RSS | tree PSS | chrome CPU |
|---|---|---|---|---|---|---|---|---|
| inproc | 31.4 | 16.7/16.7/16.7–33.3 | 24.6–24.7/31.9–32.1/32.6–34.0 | 1.12–1.21 ms | 153–155 / 315–316 MB | – | 515–519 MB | 44–47 % |
| **oop** | 31.2 | 16.7/16.7/16.7 | 25.1–25.3/32.1–32.4/34.6–35.7 | 0.97–1.05 ms | 136–138 / 286–290 MB | 34 / 123 MB | 520–522 MB | 40–55 % |
| gpu (readback) | 29.2–30.4 | 16.7/16.7–33.3/33.3 | 17.2–19.0/30.4–32.7/35–41 | 1.62 ms | 134–149 / 283–301 MB | 81–82 / 182 MB | 556–574 MB | 57–63 % |

At scale 1 the window is 1.63 Mpx of damage per frame, and every arm
delivers ~31 fps on 60 Hz with 16.7 ms frame intervals. Chromium produces
a frame every other vblank, so the event rate, not the present path, is
what the fps column shows there. i2p is the same for inproc and oop.

### Reading

- **Out of process costs nothing measurable on the present path.** fps,
  frame intervals, i2p and server paint are within run-to-run noise of
  in-process on both boxes. The extra hop (GPU process → its own socket)
  replaces the old one (viz thread → UI thread task → socket).
- **Memory:** the GPU process is +30–34 MB PSS (~115–125 MB RSS, most of
  it the shared binary). The browser drops by 12–17 MB PSS because viz
  moved out, so the whole tree is +3–15 MB PSS. On box1's 3.3 GB that is
  noise.
- **CPU:** the chrome tree's CPU during the scroll is the same within
  noise (60 % vs 62–73 % on box1). On a 2-core box an out-of-process GPU
  does not save CPU, as predicted, but it does not cost any measurable
  CPU either.
- **GPU raster with a readback is not worth shipping.** On box1 it
  *loses* fps (53 vs 62): the whole-frame `glReadPixels` on HSW GT1 plus
  whole-frame damage (a readback has none; server paint 1.5 → 3.7 ms)
  cost more than Skia software raster saves. On testhost2 it trades about
  1 fps for a 6–8 ms better i2p p50 (17–19 vs 25 ms: raster finishes
  sooner, the copy comes later) and +45–50 MB of GPU-process memory. The
  benefit of GPU raster only shows once the readback **and** the
  whole-frame damage go away, which is exactly what the dma-buf follow-up
  removes. Expect it on testhost2 (KBL, full anv) far more than on box1,
  where hasvk is "incomplete" and the GPU is GT1.

## Other gotchas

- `headless_shell` forces `--ozone-platform=headless` (`headless_content_main_delegate.cc:272`). Use it as a compile check only, and run `chrome` for real tests.
- `--enable-logging=stderr` is needed to see the `nitro-frame`/`nitro-trace` output. Out of process, `nitro-frame` lines come from the GPU process and carry no i2p (the input is the browser's); use the server's `samples i2p` or `scroll-bench.py`.
- `NITRO_GPU_READBACK=1` is a measurement knob, not a feature: without it the platform offers no GL, and Chromium rasters in software whatever `--use-angle` says.
- The DBus, BlueZ and GCM errors in the log are environmental noise.

## Reproducing

- **Build:** [chromium-build.md](chromium-build.md), `-j 48` via `cr-env.sh`; §11 is the shippable `out/Nitro` build.
- **Deploy:** `just deploy-chromium` (box) or `just install-chromium` (local); see `docs/testbox.md` §Chromium.
- **Input:** `nitro-shot --input ARGS` injects input server-side through the control socket (`wheel`, keys, pointer), so no harness is needed.
- **Scroll benchmark:** `deploy/scroll-bench.py` injects the wheel test above against whatever client is under the pointer and prints a row in the format of the Measurements tables. `just chromium-bench inproc|oop|gpu [RUNS]` wraps it with a chrome launch and memory/CPU sampling (#3919).

## Test box (#3865)

Tested on the real box: `kaspar@192.168.1.204`, Pentium G3240 (2 cores, no AVX2), 3.3 GB RAM, Intel HD on i915/KMS, 1920×1080. The build is release and non-component (`out/Nitro`, see [chromium-build.md](chromium-build.md) §11), deployed with `just deploy-chromium` and launched from nitro-launcher's "Chromium (nitro)" entry. nitro was main `1ae7761`, with the launcher change from this task.

### What works on real hardware
- **Launch from the launcher.** The entry appears after `chromium-nitro` is added to `NATIVE_PROGRAMS`. Without that, the launcher's native-only filter (task 3842) hides it.
- **Icon.** The bar's window entry shows Chromium's logo. It comes from `--class=chromium-nitro` → `SetAppId` → the user hicolor `chromium-nitro.png`. The title bar is Chromium's own CSD, so there is no server icon there.
- **First run.** The ToS dialog, the welcome page and the Google new-tab page all render. The box has network, so a real website loaded.
- **Local page** `file://` renders.
- **Input.** Typing into a `<textarea>` works; the text read "tzped" for "typed" because the box's layout is German and the injector types US. Button clicks register (the page's counter reached "clicked 2"). A `<select>` opens as a popup and choosing "gamma" works. The right-click context menu shows with rounded corners.
- **Window management.** A titlebar drag moves the window. A right-edge drag resizes it: +240 px, and the page reflowed.
- **Sandbox is ON.** Ubuntu's `apparmor_restrict_unprivileged_userns=1` would deny the userns sandbox to a chrome at `~/nitro-bin`. The fix is an AppArmor profile (`deploy/chromium/apparmor-chromium-nitro`, Ubuntu's `chrome` shape) at our path. With it, renderers and utility processes run in their own user namespace with `Seccomp: 2`. No `--no-sandbox` and no SUID helper are needed.

### What broke / caveats
- **Missing system libs.** The first launch died with `libatk-1.0.so.0: cannot open shared object file`. The box had no ATK/AT-SPI; the recipe now apt-installs `libatk1.0-0t64 libatk-bridge2.0-0t64 libatspi2.0-0t64`.
- **Old deployed server.** The box's server (`5e4b02e`) predated #3834's `input`/`samples` control ops, so main was deployed first.
- **Server `reload` needed after first install.** The server's desktop index doesn't pick up the new `.desktop` until then. The launcher rescans by itself.
- **zram.** The box has 4 GB of zram (compressed RAM, no disk swap); `docs/testbox.md` records it.

### Memory (one tab, idle, caret page)
| | |
|---|---|
| processes | 10 (browser, 3 zygote, 2 utility, 4 renderer incl. spare/extension) |
| PSS sum | **439 MB** (fresh) / 496 MB (after the benches, with 2 tabs of history) |
| RssAnon sum | 234 / 264 MB |
| VmRSS sum | 1 468 MB. This double-counts the shared 346 MB binary; don't quote it |
| `free` used | +170 MB relative to the desktop without chrome (726 → 900 MB) |

On a 3.3 GB box that fits comfortably next to the desktop's ~730 MB. Each additional site process will add roughly 20–50 MB PSS.

### Measurements

These run on the real box with the real scan-out: `deploy/scroll-bench.py`, driven through the control socket's `input wheel` over the page. The page is 1 500 rows. The window was about 1180×1000 device px, and the whole content area repaints while scrolling.

| pacing | fps | frame interval p50 / p95 / max | submit→present p50 | input→photon p50 / p95 / max |
|---|---|---|---|---|
| box 60 Hz, 16 ms events #1 | 60.5 | 16.7 / 16.7 / 50.0 ms | – | 25.4 / 32.8 / 48.1 ms (n=282) |
| box 60 Hz, 16 ms events #2 | 60.4 | 16.7 / 16.7 / 33.3 ms | – | 25.6 / 32.8 / 46.1 ms (n=280) |
| box 120 Hz, 16 ms events #1 | 61.8 | 16.7 / 16.7 / 108.4 ms | – | 24.9 / 32.3 / 44.5 ms (n=285) |
| box 120 Hz, 16 ms events #2 | 61.2 | 16.7 / 16.7 / 91.7 ms | – | 24.8 / 32.5 / 48.7 ms (n=282) |
| box 120 Hz, 8 ms events #1 | 61.4 | 16.7 / 16.7 / 33.3 ms | – | 21.1 / 24.8 / 38.2 ms (n=286) |
| box 120 Hz, 8 ms events #2 | 61.0 | 16.7 / 16.7 / 33.4 ms | – | 21.3 / 25.0 / 34.6 ms (n=284) |

| arm | server paint mean | copy mean | damage/frame | chrome tree CPU | nitro-server CPU |
|---|---|---|---|---|---|
| 60 Hz | 9.9–10.7 ms | 1.3–1.5 ms | **1 102 800 px** (the whole window) | 60–63 % of one core | 62 % |
| 120 Hz | 10.7–11.1 ms | 1.4 ms | 1 102 800 px | 64–66 % | 66 % |

CPU is the utime+stime jiffies of the whole chrome tree over the run. There are 2 cores, so 200 % is the ceiling. Total load is about 130 % of 200.

**Caret-blink idle** (focused `<textarea>`, 30 s, 60 Hz): chrome tree **1.8 % CPU**, server **4.0 frames/s**, **damage 15 px/frame**, paint 9 µs. #3833 holds on the box. A second 20 s sample at 120 Hz after the benches read 2.9 % CPU and 4.1 frames/s. Its `damage_px_mean` of 294k is the stats window of 100 still carrying bench frames, so it is not a caret number.

### Verdict

**Usable, and at 60 Hz genuinely smooth.** Scrolling holds a flat 60 fps (p95 16.7 ms) with input→photon p50 about 25 ms, about 21 ms with denser input. That beats the fake-backend dev numbers (45 fps / 35 ms), because the real flip is a real vblank and not a 21 ms fake. Idle is quiet. Memory is fine for 3.3 GB.

**It does not use 120 Hz.** At 120 Hz Chromium still delivers about 61 fps; the frame interval p50 stays at exactly 16.7 ms. The cause is the server paint of the full 1.1 Mpx damage every frame, at about 11 ms per frame. That is already above the 8.3 ms budget before Chromium's own raster runs on the other core. The two processes together use about 130 % of the 2 cores.

### Next levers (in order of expected payoff)
1. **Server paint cost per px: done in #3877**, see §Paint cost below. With the SWAR blend plus `SetOpaqueRegion`, paint is ~1.8 ns/px and 120 Hz holds.
2. **Chromium side:** check `--num-raster-threads=1` against the default on 2 cores, and test a smaller window/tile size. An out-of-process GPU (option B/C above) would not help CPU on a 2-core box.
3. **Launcher:** `NATIVE_PROGRAMS` is a hardcoded list. A `X-Nitro-Native=true` key in `.desktop` would stop each foreign nitro client from needing a launcher change.

## Paint cost (#3877)

**Cause, confirmed:** the main browser window's canvas is `AR24`
(`nitro: canvas widget=1 format=AR24` in the log), because the frame is
always `kTranslucent` on Linux. So nitro blended all 1.1 Mpx of every
scroll frame with the per-pixel straight-alpha loop, and it painted the
background under the window as well, since an alpha image cannot occlude.

**What changed:**
- **B (server):** `nitro-raster` SWAR straight-alpha blit: 2 px per `u64`,
  with 255/255 → copy and 0/0 → skip. It is bit-exact. See `docs/bench.md`
  §7.10c.
- **A (wire + server + backend):** `SetOpaqueRegion` (`0x030c`, cap bit 15
  `OPAQUE_REGION`, `docs/wire.md`).
  - `NitroWindow::SetOpaqueRegion` stores Chromium's region, and
    `PresentFrame` sends it before the next `Commit`, so it is atomic with
    the pixels.
  - Chromium calls it for the CSD frame via
    `BrowserDesktopWindowTreeHostLinux` (`GetRestoredOpaqueRegion`). On the
    box it sent 2 rects for a ~1185×1000 window (first `8,0 1169x8`): the
    top strip between the rounded corners, and the body.
  - The server paints the region with the opaque copy, and only the
    corners and the shadow ring with the blend. The region also occludes
    what lies under it.
  - The server falls back to a full blend whenever the item is not 1:1,
    pixel-aligned and at opacity 1 (overview thumbnails, scaled windows).
- **Knobs (debug/bench only):**
  - `NITRO_FORCE_OPAQUE=1` makes toplevels `XR24`. This loses the corner
    transparency and the shadow.
  - `NITRO_NO_OPAQUE_REGION=1` leaves the cap out of `ClientCaps`, which
    gives the "B only" arm.
- **Unpremultiply:** not skipped. It is already a load plus a branch per
  pixel at a == 255, and the chrome-side CPU did not move measurably.

**Box numbers:** `deploy/scroll-bench.py` on a 1500-row page, window about
1180×1000, 1.10 Mpx damage per frame, 2 runs per arm. CPU is utime+stime
over the run, as a percentage of one core (2 cores in total).

| arm | fps | frame interval p50/p95/max ms | i2p p50/p95/max ms | paint mean | ns/px | chrome CPU | server CPU |
|---|---|---|---|---|---|---|---|
| baseline 60 Hz | 58.0–59.6 | 16.7/16.7–33.3/33–50 | 25.5–25.7/33.0–35.4/47–53 | 10.6–11.1 ms | 9.6–10.1 | 51 % | 61–62 % |
| D forced-opaque 60 Hz | 61.6–62.1 | 16.7/16.7/16.7 | 25.1–25.2/32.4–32.5/33 | 2.2–2.4 ms | 2.0–2.1 | 53–55 % | 21 % |
| B only 60 Hz | 61.6 | 16.7/16.7/16.7 | 25.3–25.4/32.2–32.6/33 | 5.4–5.5 ms | 4.9–5.0 | 57–58 % | 38 % |
| **B+A 60 Hz** | 61.6–61.9 | 16.7/16.7/16.7 | 24.8–24.9/32.1–32.4/33 | **2.0 ms** | **1.8** | 58–61 % | **20 %** |
| baseline 120 Hz | 60.4–61.0 | 16.7/16.7/25–33 | 25.7–26.0/32.6–32.8/41–50 | 10.8 ms | 9.8 | 65 % | 63–64 % |
| D forced-opaque 120 Hz | 122.2–122.6 | 8.3/8.3/8.3–16.7 | 13.0–13.1/16.7–16.8/17–23 | 1.9–2.4 ms | 1.7–2.2 | 78–82 % | 38–40 % |
| B only 120 Hz | 71.0–75.4 | 16.7/16.7/16.7 | 22.0–23.1/31.2–31.3/33 | 5.9 ms | 5.3–5.4 | 91 % | 45–46 % |
| **B+A 120 Hz** | **120.6** | **8.3/8.3/16.7** | **13.0–13.2/16.9–17.1/25–27** | **1.9–2.3 ms** | **1.7–2.1** | 87–92 % | 41–42 % |

**Verdict: 120 Hz is now sustained.** Scrolling runs at 120.6 fps, with an
8.3 ms p50/p95 frame interval and i2p p50 halved (26 → 13 ms). That
matches the forced-opaque upper bound while keeping the rounded corners
and the shadow. B alone halves paint but does not reach 120 Hz: the
leftover is the background painted under the window, which only A's
occlusion removes. At 120 Hz Chrome now uses about 90 % of a core, which
leaves about 70 % of the two cores free.

**Visual check (B+A, box):**
- The CSD corners are rounded, with no black or garbage pixels.
- The right-click context menu still renders with rounded corners and a
  shadow over the page. It is `AR24` with no region, so it takes the blend.

**Pointer at startup:** the server re-derives pointer focus whenever a
window maps, unmaps, restacks or moves under a still pointer (#3886), so
a chrome started under a parked pointer gets its `PointerEnter` with its
first frame and the wheel scrolls at once. No motion is needed before
`scroll-bench.py`.

