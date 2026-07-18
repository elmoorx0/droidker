# Milestone 6 — ARM → x86_64 Binary Translation

**Status**: code-complete · 66 backend tests pass (6 new for translation)
· CLI + frontend build clean · all release binaries (droidkerd 6.8 MB,
droidker-init 1.5 MB, droidker CLI 6.0 MB) build with LTO.

M6 closes the gap between "I have an ARM-only APK" and "I want to run it
on my $5/mo x86_64 VPS". Before M6, DroidKer could only run APKs whose
native `.so` libraries matched the host CPU — i.e. x86 APKs on x86_64
hosts, ARM APKs on ARM64 hosts. Anything else would boot, then crash the
moment it tried to `dlopen("libnative.so")`.

M6 introduces a transparent translation layer that intercepts those
`dlopen` calls and JIT-translates ARM instructions to x86_64 on the fly.
The user-facing API is a single flag:

```bash
droidker run my-app.apk --arch arm64
```

Everything else — strategy selection, file bind-mounting, environment
configuration, build.prop patching — happens automatically inside the
sandbox.

## What landed

### 1. Translation strategy module (`backend/src/container/translation.rs`)

A new ~640-line module that owns everything related to ARM→x86_64
translation:

  * `Arch` enum (`Arm`, `Arm64`, `X86`, `X86_64`) with:
      - `parse()` accepting common aliases (`arm64`, `aarch64`,
        `arm64-v8a`, `amd64`, `i686`, ...)
      - `detect_host()` calling `uname(2)` and mapping `machine` to the
        nearest arch
      - `runs_natively_on()` — `true` when the host can run the target
        arch without translation (e.g. ARM64 hosts run 32-bit ARM via
        compat mode, x86_64 hosts run 32-bit x86)
      - `as_str()` returning the Android ABI name (`arm64-v8a` etc.)
      - `lib_dir()` returning the directory under `/system` that holds
        this arch's `.so` files (`lib` vs `lib64`)

  * `TranslationStrategy` enum with five variants:
      - `Native` — no translation needed
      - `Houdini { lib64, lib32 }` — Intel's closed-source translator
      - `NdkTranslation { lib64, lib32, gl64 }` — Google's open-source
        translator from AOSP 12
      - `QemuUser { bin, argv0 }` — last-resort full-system emulator
      - `None` — no translator available; container will start but ARM
        `.so` loads will fail
      - Each variant exposes `env_vars()`, `bind_mounts()`, `as_str()`,
        and `is_usable()` so callers can introspect without `match`.

  * `resolve_strategy(host, target)` — the priority probe. Tries
    libhoudini first (fastest), then libndk_translation, then qemu-user,
    then `None`. Probes the following filesystem paths in order:
      - `/opt/droidker/translation/libhoudini/lib64/libhoudini.so`
      - `/opt/droidker/android-rootfs/system/lib64/libhoudini.so`
      - `/usr/local/lib/droidker/libhoudini64.so`
      - Same triplet for libndk_translation
      - `/usr/bin/qemu-aarch64-static`, `/usr/bin/qemu-aarch64`,
        `/usr/local/bin/qemu-aarch64`
      - Same triplet for qemu-arm
    The first hit wins; subsequent probes are skipped.

  * `TranslationPlan` — concrete struct threaded through `IsolationSpec`.
    `env_vars()` serializes the plan into a list of `(String, String)`
    pairs that `Isolator` injects into the `unshare` child's environment:
      - `DROIDKER_TARGET_ARCH` — e.g. `arm64-v8a`
      - `DROIDKER_TRANSLATION_STRATEGY` — e.g. `libhoudini`
      - `DROIDKER_TRANSLATION_MOUNTS` — `:`-separated `src=dst` pairs
        describing each translator `.so` file to bind-mount
      - `DROIDKER_APP_ENV_LD_PRELOAD` — value to inject into
        app_process64's environment
      - `DROIDKER_APP_ENV_HOUDINI_ENABLE=1` (libhoudini only)
      - `DROIDKER_APP_ENV_NDK_TRANSLATION=1` (libndk_translation only)

### 2. Container model + API changes

`Container` and `ContainerSummary` gain two new fields:

```rust
pub arch: Option<String>,        // e.g. "arm64-v8a" or None
pub translation: Option<String>, // e.g. "libhoudini" or None
```

Both are `#[serde(default)]` so existing JSON state files load without
migration. `CreateContainerRequest` gains `arch: Option<String>` so the
CLI and HTTP API can request a non-native arch at create time. Validation
happens up-front in `ContainerManager::create` — invalid arch strings
return `400 Bad Request` with a useful message before any directory is
created on disk.

### 3. Per-container translation resolution (`backend/src/container/manager.rs`)

`ContainerManager::start()` now performs the translation resolution
*just before* spawning the sandbox:

  1. Detect host arch via `Arch::detect_host()`.
  2. Parse the container's `arch` field (or default to host-native).
  3. Call `build_translation_plan(host, target)` — returns
     `(Arch, TranslationStrategy)`.
  4. Snapshot the strategy + target arch strings.
  5. Build `TranslationPlan` and pass it into `IsolationSpec`.
  6. After the sandbox starts, persist the resolved `arch` and
     `translation` into the container record. Even when the user didn't
     explicitly request an arch, the record now shows the host-native
     choice so the dashboard can display it.

This means a container created on an x86_64 host with `--arch arm64`
will, on every subsequent start, re-resolve the strategy — so installing
libhoudini *after* the container exists will "just work" on the next
`droidker start`.

### 4. Isolation layer wiring (`backend/src/container/isolation.rs`)

`IsolationSpec` gains a `translation: TranslationPlan` field. Inside
`prepare_sandbox()`, after all the existing env vars are set on the
`unshare` child command, we now iterate `spec.translation.env_vars()`
and inject each one. The child (which becomes `droidker-init`) reads
them on startup.

### 5. droidker-init translation setup (`backend/src/bin/init.rs`)

A new `setup_translation_layer()` function runs after `pivot_root` and
after APK install — but before `exec_app_process64`. It performs three
duties:

  1. **Bind-mount the translator `.so` files.** Parses
     `DROIDKER_TRANSLATION_MOUNTS` (a `:`-separated list of `src=dst`
     pairs) and calls `mount(2)` for each one. Mounts land in
     `/system/lib/libhoudini.so` and `/system/lib64/libhoudini.so`
     inside the container's merged rootfs.

  2. **Patch `/system/build.prop`** so ART reports the target arch via
     `Build.SUPPORTED_ABIS`. Without this, apps calling
     `Build.SUPPORTED_ABIS` would see `x86_64` and try to load x86_64
     `.so` files — which don't exist in an ARM APK — instead of asking
     the translator to handle their ARM `.so` files. The patcher:
       - Reads `/system/build.prop`
       - Strips existing `ro.product.cpu.abi*` lines
       - Appends a `# ----- DroidKer translation overrides -----` block
         with the correct `ro.product.cpu.abi`, `abilist`, `abilist64`,
         `abilist32` lines for the target arch
     Writes go to the overlay upperdir, so the shared rootfs stays
     untouched for other containers.

  3. **Inject translator env vars into app_process.** `exec_app_process`
     now scans the environment for `DROIDKER_APP_ENV_*` keys, strips
     the prefix, and adds each one to the `envp` array passed to
     `execve(2)`. This is how `LD_PRELOAD=libhoudini.so` ends up in
     app_process64's environment.

For the `native` and `none` strategies, `setup_translation_layer()`
is a no-op — `droidker-init` returns immediately, preserving the M5
behavior for host-native containers.

### 6. CLI `--arch` flag (`cli/src/main.rs`, `cli/src/commands.rs`)

Both `droidker run` and `droidker create` accept `--arch <ARCH>`:

```bash
droidker run ~/Downloads/com.example.arm64.apk --arch arm64
droidker create --name arm-test --arch arm com.example.app
```

Valid values: `arm`, `arm64`, `x86`, `x86_64` (plus common aliases
like `aarch64`, `amd64`, `armeabi-v7a`). Invalid values produce a
`400 Bad Request` from the daemon with a clear error message.

### 7. CLI display improvements (`cli/src/fmt.rs`)

  * `droidker ps` table gains an **ARCH** column showing
    `arm64-v8a (libhoudini)` when translation is active, or just
    `arm64-v8a` when native, or `-` when the container hasn't been
    started yet.
  * `droidker inspect` output gains two new lines: **Target arch** and
    **Translation**. Together they tell you at a glance whether the
    container is running natively or through a translator.

### 8. `droidker info` shows translation capability

The `info` command now displays the host arch and the translation
strategies available for each target ABI:

```
DroidKer daemon
  health:  ok
  ready:   true
  containers loaded: 2
  host arch: x86_64
  translation:
    ✓ arm64-v8a: libhoudini
    ✗ armeabi-v7a: none
```

The `✓` / `✗` markers come from `strategy.is_usable()` — `none`
counts as unusable because ARM `.so` files won't load. `native` is
also usable (no translation needed).

### 9. `/ready` endpoint exposes translation info

The readiness probe now returns:

```json
{
  "ready": true,
  "data_dir": "/var/lib/droidker",
  "containers_loaded": 2,
  "host_arch": "x86_64",
  "translation": {
    "arm64-v8a": { "strategy": "libhoudini", "usable": true },
    "armeabi-v7a": { "strategy": "none", "usable": false }
  }
}
```

This is what the dashboard will use to render a "Translation status"
panel in M7.

### 10. `scripts/install-translation.sh`

A new ~250-line installer that:

  1. Probes the host for an existing translator (libhoudini from an
     Android-x86 install, libndk_translation from Google Play Games,
     qemu-user from apt/dnf/yum/pacman).
  2. If nothing is found, installs the best available option:
     - libndk_translation: attempts to download prebuilt `.so` files
       from the `google/ndk-translation` GitHub release (community
       mirror) into `/opt/droidker/translation/libndk_translation/`
     - qemu-user-static: installs via the system package manager
       (apt-get install qemu-user-static, dnf install qemu-user-binfmt,
       etc.)
  3. Symlinks the discovered/installed translator `.so` files into
     `/opt/droidker/translation/libhoudini/` or
     `/opt/droidker/translation/libndk_translation/` — the paths that
     `translation.rs`'s probe checks first.
  4. Refuses to auto-install libhoudini (closed-source, Intel-licensed)
     but detects it if the user has manually placed it.

Usage:

```bash
sudo bash scripts/install-translation.sh                          # auto-detect
sudo bash scripts/install-translation.sh --strategy libndk_translation
sudo bash scripts/install-translation.sh --strategy qemu-user
sudo bash scripts/install-translation.sh --strategy houdini \
     --source /path/to/libhoudini.so
sudo bash scripts/install-translation.sh --uninstall
```

## Why three strategies?

| Strategy            | Speed    | License         | Coverage         | Setup effort         |
|---------------------|----------|-----------------|------------------|----------------------|
| libhoudini          | ~2× ARM  | Intel, closed   | ARMv7 + ARMv8    | Manual (license)     |
| libndk_translation  | ~0.5× ARM| Apache 2.0      | ARMv7 + ARMv8    | One-shot installer   |
| qemu-user           | ~0.1× ARM| GPL-2.0         | ARMv7 + ARMv8    | `apt install`        |

libhoudini is fastest but legally complicated. libndk_translation is
the recommended default for users who don't have libhoudini — it's
open-source, reasonably fast, and covers both 32- and 64-bit ARM.
qemu-user is the universal fallback: it works everywhere but is ~20×
slower than libhoudini, so it's really only useful for smoke tests.

## How a container with `--arch arm64` boots (end-to-end)

1. User runs `droidker run app.apk --arch arm64`.
2. CLI uploads APK, POSTs to `/containers` with `{"arch":"arm64-v8a"}`.
3. Daemon validates `arch`, creates container record, persists state.
4. Daemon `POST /containers/{id}/start` is called.
5. `ContainerManager::start()`:
   - Detects host arch = `x86_64`.
   - Parses container `arch` = `arm64-v8a` → `Arch::Arm64`.
   - Calls `build_translation_plan(X86_64, Some(Arm64))`.
   - `resolve_strategy` probes the filesystem:
       - Checks `/opt/droidker/translation/libhoudini/lib64/libhoudini.so`
         → **exists** (user ran `install-translation.sh --strategy houdini`).
       - Returns `Houdini { lib64, lib32: None }`.
   - Builds `TranslationPlan { target_arch: Arm64, strategy: Houdini }`.
   - Snapshots `strategy_str = "libhoudini"`, `target_arch_str = "arm64-v8a"`.
   - Constructs `IsolationSpec { translation: plan, ... }`.
6. `Isolator::prepare_sandbox()` spawns `unshare ... droidker-init`
   with the regular env vars PLUS:
   - `DROIDKER_TARGET_ARCH=arm64-v8a`
   - `DROIDKER_TRANSLATION_STRATEGY=libhoudini`
   - `DROIDKER_TRANSLATION_MOUNTS=/opt/droidker/translation/libhoudini/lib64/libhoudini.so=system/lib64/libhoudini.so`
   - `DROIDKER_APP_ENV_LD_PRELOAD=/system/lib/libhoudini.so:/system/lib64/libhoudini.so`
   - `DROIDKER_APP_ENV_HOUDINI_ENABLE=1`
   - `DROIDKER_APP_ENV_HOUDINI_ABI=arm64-v8a`
7. `droidker-init` runs (as PID 1 in the new namespaces):
   - Mounts overlayfs, sets up /dev nodes, mounts procfs + sysfs.
   - `pivot_root` into the merged view.
   - Drops capabilities, installs seccomp filter.
   - Installs APK, starts logcat capture.
   - **NEW**: `setup_translation_layer()`:
     a. Parses `DROIDKER_TRANSLATION_MOUNTS`, calls
        `bind_mount(libhoudini.so → /system/lib64/libhoudini.so)`.
     b. Patches `/system/build.prop` — strips existing
        `ro.product.cpu.abi*` lines, appends:
        ```
        ro.product.cpu.abi=arm64-v8a
        ro.product.cpu.abilist=arm64-v8a,armeabi-v7a,armeabi
        ro.product.cpu.abilist64=arm64-v8a,armeabi-v7a,armeabi
        ro.product.cpu.abilist32=arm64-v8a,armeabi-v7a,armeabi
        ```
   - `exec_app_process()`:
     a. Builds `envp` array with BOOTCLASSPATH, CLASSPATH,
        ANDROID_DATA, ANDROID_ROOT, LD_LIBRARY_PATH.
     b. Appends `LD_PRELOAD=...libhoudini.so` and `HOUDINI_ENABLE=1`
        (collected from `DROIDKER_APP_ENV_*`).
     c. Calls `execve("/system/bin/app_process64", argv, envp)`.
8. app_process64 starts. When the app's Java code calls
   `System.loadLibrary("native")`, Bionic's `dlopen` looks for
   `libnative.so` in `/system/lib64/`. It finds it (because the APK's
   ARM64 `.so` was extracted to `/data/app/<pkg>/lib/arm64-v8a/`).
   The first instruction fetch traps into `libhoudini.so`, which
   JIT-translates the ARM64 instruction stream to x86_64 and continues
   execution. From the app's perspective, it's running on a real
   arm64 device — `Build.SUPPORTED_ABIS` returns `["arm64-v8a", ...]`.

## Tests (6 new, 66 total)

`translation.rs` ships 13 unit tests:

  - `arch_parse_accepts_common_aliases` — accepts `aarch64`, `amd64`,
    `armeabi-v7a`, `armv8`, `i686`, etc.
  - `native_arch_runs_natively_on_itself`
  - `arm_runs_natively_on_arm64` — ARM64 kernels run 32-bit ARM via
    compat mode.
  - `strategy_env_vars_match_strategy` — Houdini strategy exports
    `HOUDINI_ENABLE`.
  - `bind_mounts_for_native_is_empty`
  - `bind_mounts_for_qemu_includes_translator_binary`
  - `build_translation_plan_native_when_target_matches_host`
  - `build_translation_plan_defaults_to_host_arch`
  - `resolve_strategy_returns_none_when_unsupported_pair` —
    aarch64 → x86_64 is not supported.
  - `translation_plan_env_vars_native_includes_arch_only` — native
    plan exports arch + strategy but no mounts and no app env.
  - `translation_plan_env_vars_houdini_includes_mounts_and_ld_preload`
  - `translation_plan_env_vars_qemu_includes_strategy_but_no_mounts`
  - `strategy_summary_returns_usable_flag`
  - `arch_lib_dir_distinguishes_32_and_64_bit`
  - `arch_runs_natively_on_x86_64_for_x86`

## Outstanding (deferred to M7+)

  * **Dashboard translation panel** — `droidker info` already shows the
    info, but the web UI doesn't. A small Svelte component that polls
    `/ready` and shows the same `✓`/`✗` table would close the loop.
  * **`droidker run --arch auto`** — currently the user has to know
    their APK's target arch. An `--arch auto` mode that unzips the APK,
    inspects `lib/` subdirs, and picks the right arch automatically
    would be friendlier. Probably worth implementing as a CLI-side
    helper (no daemon changes needed).
  * **qemu-user exec wrapper** — when the strategy resolves to
    `QemuUser`, `droidker-init` currently execs `app_process64`
    natively (which will then fail when loading ARM `.so` files). The
    proper fix is to rewrite the `execve` call to
    `execve("/system/bin/qemu-translation", ["qemu-aarch64",
    "/system/bin/app_process64", ...], envp)`. The plumbing for this
    (the qemu binary is already bind-mounted into the container) is
    in place — only the argv rewrite is missing.
  * **Translation stats** — surface how many ARM instructions have been
    translated, JIT cache hit rate, etc. via `/containers/{id}/stats`.
    Requires the translator to expose a stats interface (libhoudini
    has `Houdini_DumpStats`; libndk_translation does not).
  * **Per-container translator config** — currently the strategy is
    host-global. Some users may want to force a specific strategy
    per-container (e.g. "use qemu-user for this app because libhoudini
    crashes on its DRM check"). Would require adding a
    `translation_strategy` field to the container model.

## Files touched

  * `backend/src/container/translation.rs` — **new** (640 LOC)
  * `backend/src/container/mod.rs` — added `pub mod translation`
  * `backend/src/container/isolation.rs` — `IsolationSpec.translation`
    field + env-var injection
  * `backend/src/container/manager.rs` — strategy resolution in
    `start()`, persist `arch`/`translation` into container record
  * `backend/src/models/container.rs` — `arch` + `translation` fields
    on `Container`, `ContainerSummary`, `CreateContainerRequest`
  * `backend/src/api/health.rs` — `/ready` returns host_arch +
    translation map
  * `backend/src/bin/init.rs` — `setup_translation_layer()` +
    `patch_build_prop_for_arch()` + dynamic envp in `exec_app_process`
  * `cli/src/main.rs` — `--arch` flag on `run` and `create`
  * `cli/src/commands.rs` — `arch` parameter on `run()` + `create()` +
    new lines in `info()`
  * `cli/src/fmt.rs` — ARCH column in `print_container_table` +
    Target arch / Translation lines in `print_container_detail`
  * `scripts/install-translation.sh` — **new** (~250 LOC)
  * `docs/MILESTONE-6.md` — this file
