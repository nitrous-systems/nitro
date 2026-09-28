# Chromium build runbook

How to build Chromium with nitro's Ozone backend (#3778, #3865). The backend
itself is described in [chromium.md](chromium.md). Everything below was built
and verified on the dev build host (§1) and is replayable without rediscovery.

**Paths are this host's defaults.** The checkout is
`/home/kaspar/src/ai/chromium/src` and the build wrapper is
`/home/kaspar/src/ai/chromium/cr-env.sh` (§6). The justfile reads the
shippable build from `NITRO_CHROMIUM_OUT`, default
`/home/kaspar/src/ai/chromium/src/out/Nitro` (§11); set it if your checkout
lives elsewhere. `just install-chromium` and `just deploy-chromium` do not
build; they fail clearly if `$NITRO_CHROMIUM_OUT/chrome` is missing.

**Status: WORKING.** Both `headless_shell` and full `chrome` build and render a
real page. Incremental rebuild on an `ui/ozone/` change is **~15 s** — #3778's
edit cycle will be cheap.

---

## 0. TL;DR — the numbers

| | |
|---|---|
| **Checkout path** | `/home/kaspar/src/ai/chromium/src` (default; see §3) |
| Chromium HEAD | `f741c8c27fd492b2f3fae0cfe0d698170ffaebec` |
| clang | `24.0.0git` (from `third_party/llvm-build`) |
| `gclient sync --nohooks` | **2 min 4 s** |
| `gclient runhooks` | **33 s** |
| `install-build-deps.sh` | **59 s** |
| `gn gen` | **2.2 s** |
| `headless_shell` full build | **~27 min** wall at `-j 48` (18 099 targets) |
| `chrome` on top of that | **~16 min** (17 142 further targets) |
| **no-op rebuild** | **12 s** (`headless_shell`) / **13 s** (`chrome`) |
| **1-file `ui/ozone/` change** | **13–15 s** (`headless_shell`) / **15 s** (`chrome`) |
| Disk: checkout `src` | 34 GB (incl. `.git`, all deps, `out/`) |
| Disk: `out/Default` | 8.1 GB (both binaries) |
| Free disk after | 188 GB of 676 GB — **never got tight** |
| Binaries | `chrome` 178 MB, `headless_shell` 134 KB (component build) |

Two environment gotchas cost real time. Both are in §6. Read §6 before you build.

### Recommendation for backend work

**Iterate on `headless_shell`, not `chrome`.** It links `ui/ozone`, so it fully
exercises an Ozone platform backend, and its edit cycle is ~13 s vs ~15 s — but
more importantly its *full* build is 27 min vs 43 min, so a from-scratch or
wide-header rebuild is far cheaper. Build `chrome` only when you need the full
browser. The ~12 s no-op floor is ninja's graph scan and is unavoidable.

---

## 1. Build host

128 cores, 125 GB RAM, Ubuntu 26.04 (resolute), everything on one bcachefs
filesystem (`/` and `/home/kaspar/src/ai` are the **same** device).

> **Do NOT build at the default `-j`.** `autoninja` picks `-j` from core count
> (128 here). 128 concurrent `clang++` at ~0.7 GB each ≈ **90 GB resident**, and
> the first attempt was **OOM-killed** at target 17539/35637.
> **Use `-j 48`.** At `-j 48` the build peaked around 47 GB used / 50 GB free
> and completed with zero failures. This box has other tenants — leave headroom.

---

## 2. depot_tools

```bash
git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git ~/depot_tools
export PATH="$HOME/depot_tools:$PATH"
~/depot_tools/ensure_bootstrap        # REQUIRED, see below
export DEPOT_TOOLS_UPDATE=0           # set this only AFTER ensure_bootstrap
```

**Gotcha:** setting `DEPOT_TOOLS_UPDATE=0` *before* first use makes `gn` die with

```
python3_bin_reldir.txt not found. need to initialize depot_tools by
running gclient, update_depot_tools or ensure_bootstrap.
```

Run `ensure_bootstrap` once, then pin `DEPOT_TOOLS_UPDATE=0` to stop mid-build
self-updates. depot_tools is 886 MB after bootstrap.

## 3. Checkout layout

gclient requires the solution dir to be literally named `src` (every DEPS path
is `src/...`; `use_relative_paths` is not set). The original clone was at
`/home/kaspar/src/ai/chromium-nitro`, which cannot work. It was **moved**:

```bash
mkdir -p /home/kaspar/src/ai/chromium
mv /home/kaspar/src/ai/chromium-nitro /home/kaspar/src/ai/chromium/src
```

`/home/kaspar/src/ai/chromium/.gclient`:

```python
solutions = [
  { "name": "src",
    "url": "https://chromium.googlesource.com/chromium/src.git",
    "managed": False,
    "custom_deps": {},
    "custom_vars": {
      "checkout_configuration": "small",
    },
  },
]
target_os = ["linux"]
```

`"managed": False` keeps gclient from moving HEAD on this shallow, `main`-only
clone. `checkout_configuration: "small"` skips GPU-meet-effects / press-benchmark
/ instrumented-libraries deps — pure disk win, no build risk for headless.

## 4. Sync

The shallow + `--filter=blob:none` clone synced **fine** — the feared re-`fetch`
was never needed.

```bash
cd /home/kaspar/src/ai/chromium
gclient sync --no-history --shallow --nohooks -j 32   # 2m04s
gclient runhooks                                      # 33s
```

Split intentionally: a network/submodule failure stays distinguishable from a
toolchain-download failure across the 61-entry hook list.

Verify all of these exist afterwards (all were **missing** pre-sync):

```
third_party/llvm-build/Release+Asserts/bin/clang
buildtools/linux64/gn
third_party/rust-toolchain/bin/rustc
build/linux/debian_bullseye_amd64-sysroot
build/config/gclient_args.gni      # gn gen CANNOT run before this exists
build/config/siso/.sisoenv
third_party/node/linux
```

Note `git submodule status` still shows all 273 deps with a `-` prefix
("uninitialized") *after* a successful sync — gclient manages them outside git's
submodule view. **That is normal, not a failure.** Check for real content
(`ls third_party/skia/BUILD.gn`) instead.

## 5. Build deps

```bash
cd /home/kaspar/src/ai/chromium/src
./build/install-build-deps.sh --quick-check                          # see the gap
./build/install-build-deps.sh --no-prompt --no-arm --no-chromeos-fonts
```

Ubuntu 26.04 "resolute" is explicitly supported; no `--unsupported` needed.
~40 packages were missing (gperf, libnss3-dev, libcups2-dev, …) — it is required.
Needs passwordless sudo (present). The `sysctl: ... Read-only file system`
warnings it prints are harmless container noise.

---

## 6. The two environment gotchas ⚠️

Both come from **stray files in `/home/kaspar` that have nothing to do with
Chromium** — an unrelated `bun` install (`bun.lock`, `pi-coding-agent`):

```
/home/kaspar/node_modules    236 packages, incl. a COMPLETE undici-types
/home/kaspar/package.json    0 bytes, root-owned  -> invalid JSON
```

They sit in node's/TypeScript's module walk-up path **above** the checkout, and
break two separate build steps:

1. **devtools-frontend esbuild** —
   `✘ [ERROR] Unexpected end of file in JSON  ../../../../../../package.json:1:0`
2. **`ui/webui` `ts_library.py`** —
   `AssertionError: Undeclared dependencies to definition files ... //../../../../node_modules/undici-types/*.d.ts`

**The checkout is not at fault.** I verified this by downloading the upstream
tarball (`gs://chromium-nodejs/44f845cd5bd9d805cb1c73a98d5726b38ed8662a`):
upstream **deliberately ships `undici-types` stripped** to `package.json` +
`LICENSE` (3 entries, no `.d.ts`) — it is the only stripped package of 189. On a
clean machine that resolution simply *fails* and both steps succeed. Here, node
walks up and finds the full copy in `$HOME`. Confirmed directly:

```
require.resolve("undici-types/utility.d.ts", {paths:[".../node_modules/@types/node"]})
  -> /home/kaspar/node_modules/undici-types/utility.d.ts     # the stray one
```

### The fix: `chromium/cr-env.sh`

Those files belong to the human's tooling, so they are **not** modified. Instead
the build runs in a **private mount namespace** that masks them; nothing outside
the namespace sees any change. `/home/kaspar/src/ai/chromium/cr-env.sh`:

```bash
#!/bin/bash
set -euo pipefail
CR=/home/kaspar/src/ai/chromium
exec sudo -n unshare -m bash -c '
  mount --bind "$0/probe/empty"              /home/kaspar/node_modules
  mount --bind "$0/probe/empty-package.json" /home/kaspar/package.json
  exec setpriv --reuid=1000 --regid=1000 --init-groups -- \
    env HOME=/home/kaspar USER=kaspar LOGNAME=kaspar "$@"
' "$CR" "$@"
```

with `probe/empty/` an empty dir and `probe/empty-package.json` containing `{}`.

Details that matter, each learned the hard way:
- The masking `package.json` must be **valid JSON (`{}`)**. An empty file
  reproduces the *exact same* esbuild error.
- `setpriv --reuid=1000` — build as the real user, so outputs are owned by
  `kaspar`, not root.
- `env HOME=/home/kaspar` — without it `sudo` leaves `HOME=/root` and depot_tools
  dies with `PermissionError: '/root/.config/depot_tools'`.
- Unprivileged `unshare -Urm` also masks correctly, **but** files it creates
  appear as `root`-owned inside, so the `sudo`+`setpriv` form is the right one.

**Prefix every build/run command with `./cr-env.sh`.** If you get a clean box
without those `$HOME` strays, you can drop the wrapper entirely.

---

## 7. gn args

```bash
cd /home/kaspar/src/ai/chromium/src
gn gen out/Default --args='is_debug=false is_component_build=true symbol_level=0
  blink_symbol_level=0 use_ozone=true ozone_auto_platforms=false
  ozone_platform_headless=true ozone_platform="headless" use_remoteexec=false
  use_siso=false treat_warnings_as_errors=false dcheck_always_on=false
  is_official_build=false'
```
(one line in practice)

Authoritative `gn args out/Default --list --short --overrides-only`:

```
blink_symbol_level = 0
dcheck_always_on = false
is_component_build = true
is_debug = false
is_official_build = false
ozone_auto_platforms = false
ozone_platform = "headless"
ozone_platform_headless = true
symbol_level = 0
treat_warnings_as_errors = false
use_ozone = true
use_remoteexec = false
use_siso = false
```
(plus defaults gn echoes back: angle/vulkan dirs, clang plugin configs, crashpad,
`devtools_visibility`, `rtc_common_public_deps`, `v8_enable_gdbjit=false`, …)

Why each:
- **`enable_nacl=false` is NOT in the list — it is an invalid arg in this tree and
  `gn gen` hard-errors on it.** NaCl is fully deleted. The original spec's arg
  line is wrong; drop it.
- `is_component_build=true` + `symbol_level=0` + `blink_symbol_level=0` — the
  three that actually matter for build speed and incremental relink.
- `use_siso=false` — the `configure_siso` hook writes `build/config/siso/.sisoenv`
  during sync, which silently flips `use_siso` on. Pin it off.
- `use_remoteexec=false` — no RBE here.
- `treat_warnings_as_errors=false` — defaults **true**; cheap insurance against a
  new-toolchain warning killing a long build near the end.
- `is_official_build=false` — keeps PGO / CFI / ThinLTO off. Do not set true.
- Do **not** import `build/args/headless.gn` for the full `chrome` target: it sets
  `use_glib=false use_gtk=false`, which desktop `chrome` needs.

## 8. Build

```bash
cd /home/kaspar/src/ai/chromium
./cr-env.sh env PATH="/home/kaspar/depot_tools:/usr/local/bin:/usr/bin:/bin" \
  DEPOT_TOOLS_UPDATE=0 \
  autoninja -j 48 -C /home/kaspar/src/ai/chromium/src/out/Default headless_shell
```

`headless_shell` (18 099 targets) is the fail-fast target and took **~27 min**.
Full `chrome` adds 17 142 further targets and took a further **~16 min**
(~43 min from scratch for both). Build `chrome` by swapping the target name.

## 8b. Incremental rebuild — the number that governs #3778

Measured on this checkout, `-j 48`, after a complete build:

| scenario | `headless_shell` | `chrome` |
|---|---|---|
| no-op (ninja graph scan only) | **12 s** | **13 s** |
| touch `ui/ozone/platform/headless/headless_window.cc` | **13–15 s** (121 targets) | **15 s** (196 targets) |
| touch `ui/ozone/platform/headless/headless_window.h` | **13 s** (125 targets) | — |

Each number is the mean of two consecutive runs; they were stable to ±2 s.

**Read:** an `ui/ozone/` edit costs ~15 s, of which ~12 s is the fixed no-op
graph scan — i.e. the actual compile+relink is only ~2–3 s. `is_component_build=true`
is what makes the relink that cheap (`libheadless_headless_shell_lib.so` instead
of relinking a 178 MB binary). **#3778's edit cycle is not a bottleneck.**
A header change is no worse than a .cc change at this point in the graph.

## 9. Verify — it renders

```bash
cd /home/kaspar/src/ai/chromium
./cr-env.sh env HOME=/home/kaspar src/out/Default/headless_shell \
  --ozone-platform=headless --disable-gpu --no-sandbox \
  --screenshot=$PWD/shots/local.png --window-size=1280,800 \
  'data:text/html,<html style="background:%23204080"><body><h1 style="color:%23ffcc00;font:700 90px sans-serif">NITRO M5-J0</h1></body></html>'
```

→ `24364 bytes written to ... local.png`, and the PNG contains the **real
rendered page**: blue background, yellow 90px heading, correct text layout.
Not a blank image. **Ozone headless works.**

Same check against the full `chrome` binary (add `--headless`):

```bash
./cr-env.sh env HOME=/home/kaspar src/out/Default/chrome --headless \
  --ozone-platform=headless --disable-gpu --no-sandbox \
  --screenshot=$PWD/shots/chrome.png --window-size=1280,800 'data:text/html,...'
```

→ `31200 bytes written`, likewise a correctly rendered page. Screenshots kept at
`/home/kaspar/src/ai/chromium/shots/{local,chrome}.png`.

The `dbus`/`bluez` `ERROR:` lines and
`InitializeSandbox() called with multiple threads` are expected noise in this
container — the screenshot still succeeds. `--no-sandbox` is needed here.

---

## 10. If you have to start over

```bash
rm -rf /home/kaspar/src/ai/chromium/src/out/Default        # just the build
gclient sync --no-history --shallow --nohooks -j 32 && gclient runhooks
```

Full reset: delete `/home/kaspar/src/ai/chromium` and
`fetch --nohooks --no-history chromium` into a fresh dir — but note the plain
shallow clone synced fine, so this was never necessary.

Keep watching `df -h /home/kaspar/src/ai`; `/` is the same filesystem, so
filling it takes the box down, not just the build.

## Source branch

The nitro Ozone backend is committed on branch `nitro-ozone` in the Chromium checkout (`/home/kaspar/src/ai/chromium/src` by default; commit `455459e465`, on top of main `f741c8c27f`). Check that branch out before building with `ozone_platform_nitro = true`.

## 11. Shippable build: `out/Nitro` (#3865)

Release, **non-component**, for the test box. `out/Default` is left alone.
This is the directory `NITRO_CHROMIUM_OUT` points at by default.

```bash
cd /home/kaspar/src/ai/chromium
./cr-env.sh env PATH=/home/kaspar/depot_tools:/usr/local/bin:/usr/bin:/bin DEPOT_TOOLS_UPDATE=0 bash -c 'cd src && gn gen out/Nitro --args="is_debug=false is_component_build=false symbol_level=0 blink_symbol_level=0 use_ozone=true ozone_auto_platforms=false ozone_platform_nitro=true ozone_platform_headless=true ozone_platform=\"nitro\" use_remoteexec=false use_siso=false treat_warnings_as_errors=false dcheck_always_on=false is_official_build=false"'
./cr-env.sh env PATH=/home/kaspar/depot_tools:/usr/local/bin:/usr/bin:/bin DEPOT_TOOLS_UPDATE=0 \
  autoninja -j 48 -C /home/kaspar/src/ai/chromium/src/out/Nitro chrome chrome_sandbox
```

| | |
|---|---|
| branch | `nitro-ozone` @ `455459e465` |
| targets | 56 777 |
| wall time | **2 961 s (49 min 21 s)** at `-j 48`, from scratch |
| `chrome` | 519 MB unstripped. `symbol_level=0` still leaves about 200 MB of `.symtab`/`.strtab`. **346 MB stripped** (`just deploy-chromium` strips a copy into `target/chromium-stage/`) |
| `out/Nitro` | 9.7 GB |
| codegen | default x86-64, with no `-mavx*` in the args. The box has no AVX2, and chrome runs there |
| glibc | highest symbol version required is `GLIBC_2.25` (bullseye sysroot). The box has 2.43 |
| deployed set | 429 MB: `chrome`, `chrome_crashpad_handler`, `chrome_{100,200}_percent.pak`, `resources.pak`, `icudtl.dat`, `v8_context_snapshot.bin`, `snapshot_blob.bin`, `libEGL.so`, `libGLESv2.so`, `libvk_swiftshader.so`, `vk_swiftshader_icd.json`, `libvulkan.so.1`, `locales/*.pak` (without the 70 MB of `*.info`), `resources/` |

**Runtime system deps on a desktop-less box:** chrome `NEEDED`s `libatk-1.0`, `libatk-bridge-2.0` and `libatspi`, which the box lacked. `just deploy-chromium` apt-installs them. The alternative is rebuilding with `use_atk=false`.
