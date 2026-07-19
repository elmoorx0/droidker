# Milestone 8 — APK signature verification + split-APK bundles + audio VAD + multi-touch pinch-zoom

**Status:** complete
**Date:** 2026-07-19
**Build:** cargo check 0 errors · 119/119 tests pass · release builds OK
**Binaries:** droidkerd 6.7 MB · droidker-init 1.6 MB · droidker CLI 5.9 MB

M8 closes the four "Outstanding (deferred to M8+)" items from the M7
worklog and adds one bonus feature (multi-touch pinch-zoom) that
wasn't in the original M8 scope but turned out to be a natural fit
for the new `Humanizer` architecture.

## Goals

1. **M8.1 — APK signature verification.** A new `apk::verify` module
   that detects which signature scheme (v1 / v2 / v3 / v3.1) an APK
   uses and extracts the signer certificate's SHA-256 fingerprint —
   all without pulling in a native crypto library. The daemon exposes
   this via `GET/POST /api/v1/apk/verify`, and the CLI surfaces it
   as `droidker verify-apk <filename>`. Unsigned APKs trigger a
   warning so users can spot trojaned APKs before they execute on
   the VPS.

2. **M8.2 — Split-APK bundle support.** A new `apk::bundle` module
   that inspects `.xapk` and `.apks` archives (ZIP-of-APKs formats
   used by APKPure and Google's `bundletool` respectively). It
   enumerates the inner APKs (base + ABI / locale / density splits),
   classifies each, and recommends which ones to install for a given
   target arch. Exposed via `GET/POST /api/v1/apk/bundle` and the
   `droidker inspect-bundle <filename>` CLI subcommand. The upload
   endpoint now accepts `.xapk` and `.apks` extensions in addition
   to plain `.apk`.

3. **M8.3 — Audio Voice Activity Detection (VAD).** A pure-Rust
   silence detector that replaces silent audio chunks with 12-byte
   silence markers, cutting WebSocket bandwidth 10–50× for typical
   Android UI audio (mostly silence punctuated by short blips). The
   implementation avoids pulling in a native Opus encoder, keeping
   the daemon a single ~6 MB Rust binary as required for the 1-GB
   VPS target. Clients enable VAD via the `set_vad` control message
   on `/audio/ws`.

4. **M8.4 — Multi-touch pinch-zoom gestures.** A new `pinch_zoom`
   function in `humanizer::gestures` that uses two multitouch slots
   (0 and 1) to emit synchronized DOWN/MOVE/UP events along a
   Bezier-curve path, producing a natural pinch gesture that
   Android's `GestureDetector` recognizes as a zoom event. Exposed
   via `POST /api/v1/containers/{id}/screen/human/pinch` and the
   `droidker hpinch <id> <cx> <cy>` CLI subcommand. Also adds
   `ZoomIn` / `ZoomOut` action kinds to the `HumanizeAction` model
   for future dashboard integration.

5. **M8.5 — CLI subcommands for all M8 features.** Three new CLI
   subcommands (`verify-apk`, `inspect-bundle`, `hpinch`) expose the
   new daemon functionality. Each supports `--json` output for
   scripted use.

## Why these features

* **APK signature verification** was the top security ask from M7
  users: "How do I know the APK I just uploaded isn't a trojaned
  rebuild?" The answer is to check the signer certificate's SHA-256
  fingerprint against an out-of-band source of truth (e.g. the
  developer's website, or the Play Store listing). M8.1 doesn't do
  full cryptographic validation — that would require ~500 KB of
  native crypto deps — but it does extract the cert fingerprint,
  which is enough to spot the obvious "rebuilt from decompiled
  source" attack.

* **Split-APK bundles** are increasingly the only way to install
  modern Android apps. Google Play has been serving split APKs since
  2019, and third-party stores like APKPure mirror them as `.xapk`
  archives. Without bundle support, DroidKer users had to manually
  extract the base APK + ABI split from a `.xapk` before uploading
  — a friction point that drove the M8.2 work.

* **Audio VAD** is a pragmatic alternative to Opus. A native Opus
  encoder (`libopus` via the `opus` crate) would add ~500 KB of
  shared library to the daemon and break the "single 6 MB Rust
  binary" target. VAD gives most of the bandwidth benefit (10–50×
  for typical UI audio, which is mostly silence) with zero native
  deps. The wire format is extended with a `bits_per_sample=0`
  sentinel that the browser uses to generate digital silence locally.

* **Pinch-zoom** rounds out the gesture set. Without two-finger
  pinch, map apps, photo galleries, and zoomable PDF readers were
  effectively unusable from the dashboard — the user had to fall
  back to the on-screen zoom buttons, which many apps hide in
  gesture-first UIs. The implementation uses two multitouch slots
  in lockstep, each walking its own Bezier curve, so the gesture
  looks natural to bot detectors that analyse multi-touch motion.

## What's new in the binary

* **`apk/verify.rs`** (~570 LOC, 10 unit tests) — APK Signing Block
  parser. Locates the block by its magic ("APK Sig Block 42"),
  walks the value-pair sequence looking for v2 (0x7109871a) /
  v3 (0xf05368c0) / v3.1 (0x1b93ad61) IDs, then descends into the
  nested length-prefixed structure to extract the signer's DER
  certificate. Computes the SHA-256 of the cert and formats it as
  colon-separated hex (the same format `apksigner verify
  --print-certs` uses). Also detects v1 (JAR) signatures by
  scanning the central directory for `META-INF/*.SF` entries.

* **`apk/bundle.rs`** (~640 LOC, 17 unit tests) — Split-APK bundle
  inspector. Reuses the central-directory walker from `apk::inspect`
  to enumerate entries inside `.xapk` (manifest.json + APKs at root)
  and `.apks` (toc.json + APKs under `splits/`) archives. Classifies
  each APK as base / abi / locale / density / other, and recommends
  which to install for a given target arch. Also has a lightweight
  JSON field scanner that pulls the package name and version out of
  the manifest without pulling in `serde_json` for a single pair of
  string fields.

* **`streaming/audio.rs`** — VAD additions (~250 LOC, 11 unit
  tests). New `VadConfig` struct tracks per-connection state
  (enabled, threshold_db, hold_ms, silent_ms, emitting_silence).
  `compute_rms_db(pcm)` computes the RMS amplitude in dBFS — pure
  Rust, no native deps. `process_chunk()` returns `true` when the
  chunk should be sent as a silence marker (12-byte header with
  `bits_per_sample=0`, no payload). The AudioWs actor now carries
  a `vad: VadConfig` field that's mutated by the new `set_vad`
  control message.

* **`humanizer/gestures.rs`** — New `pinch_zoom`, `zoom_in`,
  `zoom_out` functions (~165 LOC, 3 unit tests). Two multitouch
  slots walk independent Bezier curves in lockstep. Each step emits
  a MOVE on slot 0 followed by a MOVE on slot 1 (matching the
  kernel's expected MT event framing). Inter-finger delays (5–15 ms
  Gaussian) prevent the "both fingers down in the same frame" pattern
  that some apps misclassify as a single-pointer event.

* **`models/container.rs`** — `HumanizeActionKind` extended with
  `PinchZoom`, `ZoomIn`, `ZoomOut` variants. `HumanizeAction` adds
  `start_distance`, `end_distance`, `angle_deg` fields for the
  pinch parameters.

* **`api/apk.rs`** — Four new endpoints: `GET/POST /apk/verify` and
  `GET/POST /apk/bundle`. Both accept the same path-resolution
  logic as the existing `/apk/inspect` endpoint, with the extension
  check widened to accept `.xapk` and `.apks` for the bundle path.

* **`api/screen.rs`** — New `POST /{id}/screen/human/pinch`
  endpoint and `HumanPinchRequest` struct.

* **`api/upload.rs`** — `is_apk_filename()` helper widens the
  accepted extensions to `.apk`, `.xapk`, `.apks`.

* **CLI** — Three new subcommands:
  * `droidker verify-apk <filename>` — calls `/apk/verify`, pretty-
    prints scheme + cert fingerprint + subject. Warns loudly when
    the APK is unsigned.
  * `droidker inspect-bundle <filename> [--arch <ARCH>]` — calls
    `/apk/bundle`, lists all inner APKs with classification, shows
    available ABIs and recommended install set.
  * `droidker hpinch <id> <cx> <cy> [--start-distance 30]
    [--end-distance 200] [--angle-deg 45]` — calls
    `/screen/human/pinch`, prints the gesture duration.

## Tests

* **Backend: 119/119 pass** (was 78 at M7; +41 new tests in M8).
  * `apk::verify::tests` — 10 tests covering v1/v2 detection, ASN.1
    parser, SHA-256 formatting, signing-block layout.
  * `apk::bundle::tests` — 17 tests covering format detection,
    entry classification (base/abi/locale/density), recommendation
    logic, manifest field scanning.
  * `streaming::audio::vad_tests` — 11 tests covering RMS
    computation (silence / full-scale / quiet sine), VAD state
    machine (hold period / reset on loud / resume after loud),
    silence-chunk header format.
  * `humanizer::gestures::tests` — 3 new pinch-zoom math tests
    (zoom-in increases distance, zoom-out decreases distance,
    fingers stay symmetric around center).

* **CLI: builds clean, 0 warnings-as-errors.**
* **Release builds: 6.7 MB / 1.6 MB / 5.9 MB** — within the
  1-GB VPS target budget.

## Files changed

* `backend/src/apk/verify.rs` — **new** (~570 LOC, 10 tests)
* `backend/src/apk/bundle.rs` — **new** (~640 LOC, 17 tests)
* `backend/src/apk/mod.rs` — registered verify + bundle modules
* `backend/src/apk/inspect.rs` — exported `EOCD_FIXED_SIZE`,
  `find_eocd_internal`, `EocdRecord` for reuse by verify + bundle
* `backend/src/api/apk.rs` — 4 new endpoints (verify × 2,
  bundle × 2), refactored path resolution into `resolve_apk_path`
* `backend/src/api/upload.rs` — accept `.xapk` / `.apks`
* `backend/src/api/screen.rs` — new `/screen/human/pinch`
  endpoint + `HumanPinchRequest` struct
* `backend/src/streaming/audio.rs` — `VadConfig`, `compute_rms_db`,
  `encode_silence_chunk`, `set_vad` control message (11 tests)
* `backend/src/humanizer/gestures.rs` — `pinch_zoom`, `zoom_in`,
  `zoom_out` functions (3 math tests)
* `backend/src/humanizer/mod.rs` — re-export pinch_zoom / zoom_in /
  zoom_out
* `backend/src/models/container.rs` — `PinchZoom` / `ZoomIn` /
  `ZoomOut` action kinds + pinch parameters on `HumanizeAction`
* `cli/src/main.rs` — 3 new subcommands (verify-apk, inspect-bundle,
  hpinch)
* `cli/src/commands.rs` — 3 new command handlers
* `cli/src/client.rs` — 3 new client methods (verify_apk,
  inspect_bundle, human_pinch)
* `docs/MILESTONE-8.md` — this file

## Outstanding (deferred to M9+)

* **Full APK signature cryptographic validation.** M8.1 only checks
  that a signature *exists* and extracts the cert fingerprint — it
  doesn't verify the signature against the APK contents. A full
  verifier would need RSA + EC + x509 parsers (~500 KB of native
  deps). Defer until we have a compelling reason to add the weight.

* **Opus audio codec.** VAD gives most of the bandwidth benefit at
  zero native-dep cost, but Opus would still be 2–5× better for
  sustained audio (e.g. music apps). Defer until libopus can be
  loaded dynamically only when needed.

* **Bundle extraction + multi-APK install.** M8.2 inspects bundles
  but doesn't actually extract and install the splits — the user
  still has to extract them manually and `droidker run` the base
  APK. A future M9 would add `droidker run-bundle <bundle>` that
  extracts, uploads each split, and `pm install-multiple`s them.

* **`nsenter screenrecord` for MP4 video capture.** The current
  `droidker record` captures MJPEG via WebSocket. An MP4 path via
  Android's `screenrecord` binary would have audio + better
  compression but requires `nsenter` access into the container's
  PID/mount namespace.

* **WebRTC screen streaming option.** Currently MJPEG over WS —
  fine for a single viewer on a 1-vCPU VPS, but a WebRTC path would
  give sub-100ms latency for interactive use.
