# Milestone 7 — APK arch auto-detection + per-container strategy override + qemu-user exec wrapper + dashboard translation panel

**Status:** complete
**Date:** 2026-07-18
**Build:** cargo check 0 errors · 78/78 tests pass · release builds OK
**Binaries:** droidkerd 6.6 MB · droidker-init 1.6 MB · droidker CLI 5.8 MB

M7 closes the four "Outstanding (deferred to M7+)" items from M6 and adds a
dashboard panel so users can see translation capability at a glance.

## Goals

1. **M7.1 — APK arch auto-detection.** A `droidker run --arch auto` mode
   that uploads the APK, inspects its `lib/<abi>/*.so` entries, and
   picks the best target arch automatically. No more guessing whether
   the user's APK is arm64-v8a or armeabi-v7a-only.

2. **M7.2 — Per-container translation_strategy override.** A new
   `translation_strategy` field on `Container` +
   `CreateContainerRequest` + `--translation-strategy` CLI flag. When
   set, the manager uses the named strategy verbatim instead of
   probing the host. Useful for apps that crash under libhoudini but
   work fine under qemu-user, or for reproducible benchmark runs.

3. **M7.3 — qemu-user exec wrapper.** When the strategy resolves to
   `qemu-user`, `droidker-init` now rewrites the `execve` argv to
   `["qemu-<arch>", "/system/bin/app_process64", "app_process", ...]`
   so qemu-user actually interprets the ARM binary. Without this
   rewrite, app_process64 exec'd natively and immediately SIGSEGV'd
   on the first ARM instruction.

4. **M7.4 — Dashboard translation panel.** A new Svelte component
   `TranslationPanel.svelte` that polls `GET /api/v1/ready` every
   10 seconds and renders the host's per-ABI capability table
   (native / libhoudini / libndk_translation / qemu-user / none)
   on the containers list page.

## Why these features

* **APK auto-detection** was the #1 friction point reported by M6
  users: "I uploaded my APK, ran `droidker run`, and it crashed —
  turned out my APK was arm64-v8a-only and my host is x86_64." With
  `--arch auto`, the CLI inspects the APK's `lib/` subdirs before
  creating the container and picks the right arch automatically.

* **Strategy override** is for power users. Some apps have known
  incompatibilities with specific translators (libhoudini's
  proprietary JIT crashes on certain DRM checks; libndk_translation
  doesn't ship with GL bridge; qemu-user is slow but always works).
  An override lets the user pin a strategy without editing global
  config.

* **qemu-user exec wrapper** was a real correctness bug. M6's
  `setup_translation_layer` bind-mounted `qemu-translation` into
  the container but `exec_app_process` still called
  `execve("/system/bin/app_process64", ...)` natively. The fix is
  a 2-line branch — but it's the difference between "ARM APKs run"
  and "ARM APKs SIGSEGV immediately".

* **Dashboard panel** is a 1-screen feature that saves the user
  from having to SSH in and run `droidker info` to see if their
  host can run ARM. The panel surfaces the same info inline.

## Design: APK inspection without `zip` crate

The obvious approach to APK inspection is to add the `zip` crate
and call `ZipArchive::new()`. We didn't, for two reasons:

1. On a 1-GB VPS, every shared lib counts. `zip` + `flate2` add
   ~200 KB to the binary, and we only need to enumerate file names
   — not decompress contents.

2. The ZIP central directory format is well-defined and stable
   (PKZIP 2.04g, 1993). A 150-LOC parser is plenty.

So `apk/inspect.rs` ships a hand-rolled central directory walker:

```text
1. Seek to (file_len - 65 KB) and read the tail into memory.
2. Scan backwards for the EOCD signature (0x06054b50).
3. Read cd_offset + cd_size from the EOCD.
4. Seek to cd_offset, read cd_size bytes into a buffer.
5. Walk CD entries sequentially:
     - Validate signature (0x02014b50).
     - Read name_len + extra_len + comment_len + uncompressed_size.
     - Read name_len bytes as the entry name.
     - Parse `lib/<abi>/*.so` and aggregate per-ABI counts + sizes.
6. Sort ABIs by KNOWN_ABIS priority (arm64-v8a first, then
   armeabi-v7a, x86_64, x86, etc.).
7. Return recommended_arch = abis[0] mapped to a CLI arch token
   (arm64-v8a → "arm64", armeabi-v7a → "arm", etc.).
```

This is enough to handle every APK on Google Play (which all use
standard ZIP). It does NOT handle:

* **APK Signature Scheme v3** — we don't validate signatures, so
  a tampered APK could still be inspected. That's fine; the daemon
  only reads file names, never executes APK content.
* **AAB (Android App Bundle)** — `.aab` files use a different
  on-disk format. Users must run `bundletool build-apks` first,
  which is what everyone does anyway.
* **Split APKs** — each split is a separate APK; the user must
  inspect + create a container per split. Future M8 may add
  `droidker run --split base config.arm64_v8a`.

The parser is safe: it rejects path traversal, validates ABI names
(alphanumeric + `-_` only), and bails on any malformed entry
instead of trying to recover.

## Design: per-container strategy override

Two parts:

1. **Model:** `Container.translation_strategy: Option<String>` +
   `CreateContainerRequest.translation_strategy: Option<String>`.
   Both default to `None` (auto-resolve). On `Container::start`,
   the manager calls `build_translation_plan_with_override()` which
   tries the override first; if it's `None` or unrecognized, falls
   back to the standard `build_translation_plan()` path.

2. **CLI:** `--translation-strategy <STRATEGY>` flag on both
   `droidker run` and `droidker create`. Accepted tokens:
   `native`, `houdini`, `ndk_translation`, `qemu-user` (also
   `qemu`, `libhoudini`, `libndk_translation`, `ndk` as aliases).

The override is best-effort: if the requested strategy's files
aren't installed on the host, the manager logs a warning and falls
back to auto-resolve rather than failing the container start.
This matches Docker's `--platform` behavior — it tries the
requested platform but doesn't hard-fail if unavailable.

## Design: qemu-user exec wrapper

The fix is in `exec_app_process()` in `backend/src/bin/init.rs`:

```text
if DROIDKER_TRANSLATION_STRATEGY == "qemu-user"
   AND /system/bin/qemu-translation exists:
    exec_target = "/system/bin/qemu-translation"
    argv = ["qemu-<target_arch>",        // qemu's argv[0]
            "/system/bin/app_process64", // guest binary
            "app_process",                // guest argv[0]
            "/system/bin",
            "--nice-name", package,
            "android.app.ActivityThread"]
else:
    exec_target = "/system/bin/app_process64"
    argv = ["app_process", "/system/bin", "--nice-name",
            package, "android.app.ActivityThread"]
```

qemu-user's argv[0] is cosmetic (just for `ps`), argv[1] is the
guest binary path, and argv[2..] becomes the guest's argv[0..].
We pass `"app_process"` as the guest argv[0] so ART's process
name introspection sees the same value as under native execution.

The wrapper also degrades gracefully: if `qemu-translation` is
missing (e.g. the bind-mount failed), it logs an error and falls
through to the native exec path. The container still starts in a
degraded mode and the user sees an explanatory error in the logs.

## Design: dashboard translation panel

`TranslationPanel.svelte` is a 130-line component that:

1. Polls `GET /api/v1/ready` on mount and every 10 seconds.
2. Renders the host arch + containers-loaded count.
3. Renders a table of per-ABI strategy info:
   - ABI directory name (`arm64-v8a`, `armeabi-v7a`)
   - Strategy label (Native / libhoudini / libndk_translation /
     qemu-user / Unavailable), color-coded
   - Usable column (✓ / ✗)
4. If `arm64-v8a` is unusable, shows a yellow warning banner with
   the install command.

Mounted in a 2/3 + 1/3 grid layout on the containers list page
(`frontend/src/routes/containers/+page.svelte`). The 1/3 column
also leaves room for future panels (per-container stats, recent
events, etc.).

## Tests

12 new tests in `apk::inspect::tests` (78 total, up from 66):

  - `inspect_finds_all_four_common_abis` — full APK with all 4 ABIs.
  - `inspect_handles_no_lib_directory` — pure-Java APK (no `lib/`).
  - `inspect_ignores_non_so_files_under_lib` — `lib/<abi>/README.txt`
    doesn't count.
  - `inspect_aggregates_multiple_so_per_abi` — multiple `.so` per ABI.
  - `inspect_rejects_too_small_file` — file < EOCD size.
  - `inspect_rejects_missing_eocd` — file with no EOCD signature.
  - `inspect_recommends_x86_64_when_only_x86_64_present`
  - `inspect_recommends_arm_when_only_armv7_present`
  - `inspect_handles_legacy_libs_prefix` — old `libs/<abi>/` spelling.
  - `parse_lib_entry_rejects_garbage` — non-lib paths, empty ABIs,
    non-`.so` files.
  - `map_abi_to_arch_token_known` — ABI name → CLI arch token.
  - `map_abi_to_arch_token_unknown_passthrough` — unknown ABIs
    pass through unchanged.

The tests build a minimal in-memory ZIP central directory (no
compression — we only enumerate file names) and write it to a temp
file, then call `inspect_apk()` on it. This makes the tests fast
(<1 ms each) and self-contained (no fixture files on disk).

## API changes

New endpoint:

```text
POST /api/v1/apk/inspect   { "apk": "<filename>" }
GET  /api/v1/apk/inspect?path=<filename>

→ 200 OK
  {
    "path": "/var/lib/droidker/apks/<sha256>.apk",
    "zip_entry_count": 142,
    "abis": [
      { "abi": "arm64-v8a",           "so_count": 3, "total_uncompressed_bytes": 4_521_984 },
      { "abi": "armeabi-v7a",         "so_count": 3, "total_uncompressed_bytes": 3_102_720 }
    ],
    "has_no_native_libs": false,
    "recommended_arch": "arm64"
  }
```

Both forms accept the same `filename` value returned by
`POST /api/v1/upload/apk` (typically `<sha256>.apk`). Path
separators are rejected to prevent traversal.

`POST /api/v1/containers` body gains a new optional field:

```json
{
  "name": "my-app",
  "apk": "abc123.apk",
  "arch": "arm64",
  "translation_strategy": "qemu-user"
}
```

`GET /api/v1/containers/{id}` and `GET /api/v1/containers` now
include `translation_strategy` in the response.

## CLI changes

```text
droidker run app.apk --arch auto                       # M7.1
droidker run app.apk --translation-strategy qemu-user  # M7.2
droidker create app.apk --translation-strategy native  # M7.2

droidker inspect-apk <filename>                        # M7.1 (new subcommand)
  → APK native-ABI manifest
      file:           /var/lib/droidker/apks/abc.apk
      zip entries:    142
      native libs:    found 2 ABI(s) shipped
        arm64-v8a           3 .so    4521984 bytes
        armeabi-v7a         3 .so    3102720 bytes
      recommended:    arm64 (use --arch <ARCH>)
```

`droidker inspect <id>` output gains a new line:

```text
Target arch:       arm64-v8a
Translation:       libhoudini
Strategy override: (auto)        ← new (M7.2)
```

## Outstanding (deferred to M8+)

  * **APK signature verification** — currently we trust any APK the
    user uploads. Adding APK Signature Scheme v3 verification
    (apksig library) would let multi-tenant deployments reject
    tampered APKs at upload time.

  * **AAB support** — inspect `.aab` files directly by running
    `bundletool` server-side. Probably not worth it; users should
    run `bundletool build-apks` locally.

  * **Split APKs** — `droidker run --split base,config.arm64_v8a`
    would install multiple APKs into one container. Requires
    changing the container model to hold a list of APKs instead
    of one.

  * **Translation stats** — surface how many ARM instructions
    have been translated, JIT cache hit rate, etc. via
    `/containers/{id}/stats`. Requires the translator to expose
    a stats interface (libhoudini has `Houdini_DumpStats`;
    libndk_translation does not).

  * **WebRTC screen streaming** — for sub-100ms screen latency,
    offer WebRTC as an alternative to MJPEG-over-WS. Still
    optional — the WS path stays the default for single-viewer
    scenarios.

  * **Opus audio** — swap the PCM pass-through for a real Opus
    encoder so we can ship 16 kHz stereo at the same bitrate as
    8 kHz mono PCM today.

## Files touched

  * `backend/src/apk/mod.rs` — **new** (15 LOC, module root)
  * `backend/src/apk/inspect.rs` — **new** (~470 LOC, 12 tests)
  * `backend/src/main.rs` — added `mod apk;`
  * `backend/src/api/mod.rs` — registered `apk::configure`
  * `backend/src/api/apk.rs` — **new** (~95 LOC, GET + POST
    `/apk/inspect`)
  * `backend/src/models/container.rs` — added `translation_strategy`
    field on `Container`, `ContainerSummary`, `CreateContainerRequest`
  * `backend/src/container/translation.rs` — added
    `parse_strategy_override()` + `build_translation_plan_with_override()`
  * `backend/src/container/manager.rs` — uses
    `build_translation_plan_with_override()` in `start()`;
    persists `translation_strategy` on `create()`
  * `backend/src/bin/init.rs` — `exec_app_process()` rewritten to
    branch on qemu-user strategy and rewrite argv; warning in
    `setup_translation_layer` updated
  * `cli/src/main.rs` — `--translation-strategy` flag on `run` +
    `create`; new `InspectApk` subcommand
  * `cli/src/commands.rs` — `inspect_apk()` function; `run()` +
    `create()` updated to thread `translation_strategy` through
    and resolve `--arch auto` via the inspect endpoint
  * `cli/src/client.rs` — `inspect_apk()` method
  * `cli/src/fmt.rs` — "Strategy override" line in
    `print_container_detail()`
  * `frontend/src/lib/api/api.ts` — `ReadyResponse`,
    `TranslationStrategySummary`, `ApkAbiInfo`, `ApkInspectResult`
    types; `inspectApk()` method
  * `frontend/src/lib/components/TranslationPanel.svelte` — **new**
    (~130 LOC)
  * `frontend/src/routes/containers/+page.svelte` — mount
    TranslationPanel in a 2/3 + 1/3 grid
  * `docs/MILESTONE-7.md` — this file
