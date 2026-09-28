# Chromium on nitro: the Ozone backend (#3778, #3865)

The backend lives in the Chromium tree, not in this repo: `ui/ozone/platform/nitro/` on branch `nitro-ozone` of the Chromium checkout (`/home/kaspar/src/ai/chromium/src` by default). How to build it, and the `NITRO_CHROMIUM_OUT` override the justfile uses, is in [chromium-build.md](chromium-build.md). Its companion wire client is `wire/`, from #3777.

- **Registration:** `build/config/ozone.gni` (`ozone_platform_nitro`) and `ui/ozone/BUILD.gn`.
- **GN args:** `out/Default/args.gn` has `ozone_platform_nitro = true`, with headless kept.
- **Run:**
  ```
  chrome --ozone-platform=nitro --disable-gpu --in-process-gpu
  ```
- **Size:** about 3,150 lines across `ozone_platform_nitro`, `nitro_connection_host`, `nitro_surface_factory` (`NitroCanvas`), `nitro_window`, `nitro_window_manager`, `nitro_screen`, `nitro_event_source`, `client_native_pixmap_factory_nitro`, `BUILD.gn` and `DEPS`.

## Architecture

- **One nitro connection, owned by the browser UI thread** (`NitroConnectionHost`). It watches the fd with `WatchFileDescriptor` and routes server events:
  - by window/node to `NitroWindow`;
  - outputs to `NitroScreen`;
  - `BufferReleased`/`Presented` to a buffer-id registry that posts back to the owning canvas's sequence.

  All wire sends happen on the UI thread. `NitroCanvas`, on the viz thread, posts its `CreateBuffer`/`SetImage`/`BufferDamage`/`Commit` work there.
- **`--in-process-gpu` is required.** The spec wanted the GPU process to open its own connection, but that cannot work, because nitro scopes node, buffer and window ids per client connection (`clients.rs`; `wire.md:207`). A second connection could create buffers, but it could never `SetImage` them onto the browser's window. The human chose option A (ask#419).
  - If the GPU process is out of process, `InitializeForGPU` logs an error and `CreateCanvasForWidget` returns nullptr, so it fails clearly.
  - Follow-ups if out-of-process GPU is ever wanted:
    - **B:** a nitro "embed/share buffer across clients" op, e.g. a token that a second connection can attach to.
    - **C:** a Mojo host/gpu split like Wayland's, where the GPU side sends shm fds to the browser.
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

## What doesn't / is not verified

- **No out-of-process GPU** (see above).
- **HiDPI:** only scale 1 was exercised. The model is nitro logical = DIP, buffers = physical px, and scale comes from `OutputInfo`.
- **Popup types:** `kPopup`/`kBubble` windows are nitro popups too; there is no subsurface equivalent. They are placed by the server, and only `kMenu` grabs.
- **Drag and drop:** not wired. `platform_shows_drag_image=false`, and DnD start is a no-op.
- **Clipboard:** not verified end to end.
- **Real hardware:** not tested on a real (non-fake) backend. All numbers are fake-backend.
- **Raster time:** not measured on the Chromium side. The server-side paint/copy cost is below.
- **Restore after minimize:** not exercised by the harness, which has no way to un-minimize.

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

1. **No cross-client buffer or node sharing.** Ids are per-connection, so a GPU process cannot present into a browser window. This forces `--in-process-gpu`. A nitro embed/share op would unblock Chromium's normal process model.
2. **No premultiplied ARGB format.** `AR24` is straight alpha, while Skia (and most toolkits) produce premultiplied pixels. The workaround costs a CPU unpremultiply per translucent frame, and copying unpremultiplied pixels forward between buffers is a subtle blending hazard. Suggest adding a premultiplied fourcc or a per-buffer flag.
3. **`BufferDamage` was ignored on buffer swap.** `SetImage` with a new buffer marked the whole node dirty, so every frame repainted the full image (735k px per frame measured, even for a caret blink). **Fixed by #3833**: `BufferDamage` is honoured when a same-size buffer is swapped in (see the caret-blink idle number under Test box).
4. **No input-injection command on the control socket.** `FakeInput` was test-only, so this work needed a custom harness. **Fixed by #3834**: the control socket has `input` (`nitro-shot --input "wheel 0 15 count=3"`) and `samples`.
5. **`PointerAbsolute` is normalised 0..1, not pixels.** This isn't a bug, but it is undocumented at the injection level and cost a debugging cycle.
6. **WM ops are gated on the v1 `Welcome` WM bit, not `ClientCaps`.** This is easy to miss: sending `SetWindowState` without it is a protocol error. Worth a line in `wire.md` next to `ClientCaps`.
7. **No way to restore a minimized window from the client side,** and nothing in the harness to do it. Fine for a compositor with a taskbar, but worth checking that the shell path works for foreign clients.

## Other gotchas

- `headless_shell` forces `--ozone-platform=headless` (`headless_content_main_delegate.cc:272`). Use it as a compile check only, and run `chrome` for real tests.
- `--enable-logging=stderr` is needed to see the `nitro-frame`/`nitro-trace` output.
- The DBus, BlueZ and GCM errors in the log are environmental noise.

## Reproducing

- **Build:** [chromium-build.md](chromium-build.md), `-j 48` via `cr-env.sh`; §11 is the shippable `out/Nitro` build.
- **Deploy:** `just deploy-chromium` (box) or `just install-chromium` (local); see `docs/testbox.md` §Chromium.
- **Input:** `nitro-shot --input ARGS` injects input server-side through the control socket (`wheel`, keys, pointer), so no harness is needed.
- **Scroll benchmark:** `deploy/scroll-bench.py` injects the wheel test above against whatever client is under the pointer and prints a row in the format of the Measurements tables.

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
1. **Server paint cost per px: #3877.** Honouring `BufferDamage` on swap (#3833) was already in the tested build; the damage is still the whole window because the content moves. A scroll-blit hint would not help much: nitro's existing scroll blit (#3780, `docs/bench.md` §7.5) does not apply to scrolling inside one buffer, and a blit costs about what a copy costs. The suspected cause of the ~10 ns/px is the AR24 straight-alpha blend; #3877 tries an opaque region plus a SWAR blend.
2. **Chromium side:** check `--num-raster-threads=1` against the default on 2 cores, and test a smaller window/tile size. An out-of-process GPU (option B/C above) would not help CPU on a 2-core box.
3. **Launcher:** `NATIVE_PROGRAMS` is a hardcoded list. A `X-Nitro-Native=true` key in `.desktop` would stop each foreign nitro client from needing a launcher change.
