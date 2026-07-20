# Milestone 9 — Bundle extraction + multi-APK install + MP4 screenrecord

**Status:** complete
**Date:** 2026-07-19
**Build:** cargo check 0 errors · 126/126 tests pass · release builds OK
**Binaries:** droidkerd ~6.9 MB · droidker-init ~1.6 MB · droidker CLI ~6.0 MB

M9 closes two of the four "Outstanding (deferred to M9+)" items from
the M8 worklog:

* **M9.1** — Bundle extraction + multi-APK install. M8.2 *inspected*
  `.xapk` / `.apks` bundles but didn't actually install them. M9.1
  adds the extraction + install pipeline so `droidker run-bundle
  app.xapk` produces a running container with the base APK + the
  matching ABI split + any user-selected extra splits.

* **M9.2** — MP4 screenrecord capture. M5.4 captured MJPEG via the
  WebSocket screen stream. M9.2 invokes Android's real `screenrecord`
  binary inside the container's namespaces via `nsenter`, producing
  a proper H.264 MP4 file with audio + better compression + smaller
  size. Useful for product demos, app store trailers, and CI
  artifacts that need to play in standard video players.

Two M8-deferred items remain outstanding (full cryptographic APK
signature validation, Opus audio codec) — both still require native
crypto / codec deps we'd rather not pull into the 6.9 MB daemon
binary. They'll be revisited when we have a compelling reason to add
the weight.

## Goals

1. **M9.1 — Bundle extraction.** A new `apk::bundle::extract_bundle`
   function that pulls inner APK entries out of a `.xapk` / `.apks`
   bundle ZIP and writes them to `<data_dir>/apks/<bundle_sha>/`.
   Supports both STORED (method 0) and DEFLATED (method 8) entries
   via the pure-Rust `flate2` backend (no native zlib linked). The
   daemon exposes this via `POST /api/v1/apk/extract`, and the CLI
   surfaces it as `droidker run-bundle` (which chains upload +
   inspect + extract + create + start into one command).

2. **M9.1 — Multi-APK install.** `droidker-init` accepts a
   `:`-separated list of host paths via `DROIDKER_EXTRA_APKS` and
   copies each one into `/data/app/<package>/split_<n>.apk` so
   ART's PackageManagerScanner picks them up as split APKs during
   the boot scan. The naming convention matches what
   `pm install-multiple` writes, so existing tooling (`dumpsys
   package`, `pm list packages -f`) reports the splits correctly.

3. **M9.1 — Container model extension.** The `Container` model
   gains an `extra_apks: Vec<String>` field, persisted in
   `state.json`, so a `stop` + `start` cycle re-installs the same
   splits. `ContainerSummary` gains `extra_apks_count` so `droidker
   ps` can show "this is a bundle container with 3 splits".

4. **M9.2 — MP4 screenrecord capture.** A new
   `POST /api/v1/containers/{id}/screen/record-mp4` endpoint that
   nsenters into the container's PID + mount + IPC + net namespaces
   and runs `screenrecord --time-limit N --bit-rate RATE --size WxH
   /tmp/droidker-rec.mp4`. The resulting file is read from the
   host's view of the container's overlay upperdir and streamed back
   as `video/mp4`. The CLI surfaces this as
   `droidker mp4 <id> --duration 30 --bit-rate 8000000`.

## Design decisions

### Why extract on the daemon side, not the CLI side?

We considered two designs for M9.1:

* **Daemon-side extraction** — the daemon's `apk::bundle` module
  already had a hand-rolled ZIP central-directory walker. Extending
  it to actually pull bytes out of the file was straightforward. The
  CLI stays small (no `zip` or `flate2` dep on the client side).

* **CLI-side extraction** — extract locally, upload each split as a
  separate `POST /upload/apk` request, then `POST /containers` with
  the resulting filenames. More network traffic for large bundles,
  but the daemon stays tiny.

We went with daemon-side extraction because:

1. The daemon already had 90% of the ZIP parsing logic.
2. Multi-tenant web UIs (a future M10) can extract bundles without
   needing a CLI installed.
3. The `flate2` rust_backend adds only ~150 KB to the daemon binary
   (6.7 MB → 6.9 MB) — well within the 1-GB VPS budget.
4. Splitting a 100 MB bundle upload into 4 separate HTTP requests
   on the client side would just add latency.

### Why `split_<n>.apk` instead of the original split name?

When `pm install-multiple` writes split APKs to disk, it names them
`split_<n>.apk` (zero-indexed) under `/data/app/<package>/`. ART's
package scanner specifically looks for files matching `split_*.apk`
in that directory. If we kept the original `config.arm64_v8a.apk`
name, ART would treat them as separate standalone packages rather
than as splits of the base APK, and the app wouldn't find its own
native libs.

### Why nsenter for screenrecord, not exec?

The `POST /containers/{id}/exec` endpoint (M3) runs commands inside
the container's namespaces via `nsenter` already. We could have used
it to run `screenrecord` and then `cat /tmp/rec.mp4` to stream the
bytes back. But:

1. The exec endpoint streams stdout as text, not as a binary MP4.
2. screenrecord writes to a file, not to stdout — we'd need a second
   exec call to read the file.
3. The MP4 endpoint needs to handle the screenrecord exit code + the
   file read + the cleanup atomically. Doing this in one daemon
   endpoint is much cleaner than composing two exec calls from the
   CLI.

We still use `nsenter` (just like exec does), but the daemon owns
the full lifecycle: spawn screenrecord → wait → read file → cleanup
→ return bytes.

### Why a synchronous endpoint, not async + polling?

`screenrecord` can run for up to 3 minutes (its per-file hard cap).
A synchronous endpoint blocks the HTTP connection for that whole
time, which:

* Works fine in the CLI (we just show a spinner).
* Is awkward for browsers (a `fetch()` call would block the UI
  thread for 3 minutes).
* Could time out at intermediate reverse proxies (nginx default
  proxy_read_timeout is 60s).

An async + polling design would have been:

1. `POST /record-mp4` returns `{ "job_id": "..." }` immediately.
2. CLI polls `GET /record-mp4/jobs/<id>` every 1s.
3. When done, CLI fetches `GET /record-mp4/jobs/<id>/download`.

This adds 3 endpoints, a job state machine, and a tmpfile cleanup
story (what if the client never downloads?). For an MVP that's
mostly used from the CLI, the synchronous design is simpler. We'll
add the async variant in a future milestone if browser usage
justifies it.

For now, the daemon bumps the actix worker's per-request timeout
to `duration_sec + 10s` grace, and the CLI builds a one-off reqwest
client with `timeout = duration_sec + 30s` so the connection
doesn't drop mid-recording.

### Why flate2 with rust_backend, not libflate?

`flate2` is the de-facto Rust deflate crate, has been audited, and
its `rust_backend` feature uses the pure-Rust `miniz_oxide` crate
(no native zlib linked). `libflate` is also pure-Rust but less
battle-tested and slower on x86_64 (no SIMD path). The marginal
binary size cost is identical (~150 KB).

## API additions

### `POST /api/v1/apk/extract` (M9.1)

Request body:
```json
{
  "bundle": "<filename>",
  "zip_paths": ["splits/base.apk", "splits/config.arm64_v8a.apk"]
}
```

Response body:
```json
{
  "out_dir": "/var/lib/droidker/apks/abc123...",
  "format": "apks",
  "extracted": [
    {
      "zip_path": "splits/base.apk",
      "filename": "base.apk",
      "sha256": "deadbeef...",
      "size": 12345678,
      "kind": "base",
      "abi": null
    },
    {
      "zip_path": "splits/config.arm64_v8a.apk",
      "filename": "config.arm64_v8a.apk",
      "sha256": "feedface...",
      "size": 2345678,
      "kind": "abi",
      "abi": "arm64_v8a"
    }
  ],
  "total_bytes": 14691356
}
```

The `filename` field is relative to `<data_dir>/apks/` — i.e.
`<bundle_sha>/<filename>`. Pass it to `POST /containers` as either
`apk` (for the base) or an entry in `extra_apks` (for splits).

### `POST /api/v1/containers/{id}/screen/record-mp4` (M9.2)

Request body:
```json
{
  "duration_sec": 30,
  "bit_rate": 4000000,
  "width": 540,
  "height": 960,
  "rotate": false
}
```

Response: raw MP4 bytes with `Content-Type: video/mp4` and
`Content-Disposition: attachment; filename="droidker-<id>-<ts>.mp4"`.

### `POST /api/v1/containers` (extended, M9.1)

The `CreateContainerRequest` body now accepts an optional
`extra_apks: Vec<String>` field. Each entry is a path relative to
`<data_dir>/apks/` (typically `<bundle_sha>/<filename>`). The
manager validates each path before spawning the sandbox — a typo
fails fast with a 400 instead of producing a container that starts
but can't find its splits.

### `GET /api/v1/containers` (extended, M9.1)

The `ContainerSummary` response now includes `extra_apks_count:
usize` so callers can see "this is a bundle container with N
splits installed" without fetching the full container record.

## CLI additions

### `droidker run-bundle <bundle>` (M9.1)

One-shot bundle container lifecycle:

```
$ droidker run-bundle app.xapk --arch auto --name my-app -p 8080:80
• Uploading bundle...
• Inspecting bundle structure...
• Bundle has 4 APK entries (ABIs: arm64_v8a, x86_64)
• Auto-picked arch: arm64
• Extracting 2 APK(s) from bundle...
• Base: abc123/base.apk | Splits: abc123/config.arm64_v8a.apk
• Creating container...
• Package: com.example.app
• Starting container...

✓ Bundle container my-app is running (1 splits installed).
```

Flags:
* `--arch <ARCH>` — `arm`, `arm64`, `x86`, `x86_64`, or `auto`
  (picks the first available ABI split from the bundle).
* `--split <ZIP_PATH>` — extra split to install (e.g. `config.en.apk`
  for English locale). Can be repeated.
* `-p, --port HOST:CONTAINER` — TCP port mapping. Can be repeated.
* `-m, --memory <MB>` — memory limit.
* `-c, --cpu <%>` — CPU quota.
* `--name <NAME>` — container name.
* `--notes <TEXT>` — free-form notes.
* `--translation-strategy <STRATEGY>` — `houdini`, `ndk_translation`,
  `qemu-user`, or `native`.

### `droidker mp4 <id>` (M9.2)

```
$ droidker mp4 my-app --duration 30 --bit-rate 8000000
• recording MP4 of abc12345-... for 30s @ 8000000bps (540x960) -> abc12345-1718900000.mp4
• this blocks until recording finishes — keep the terminal open
✓ captured 12450 KB in 31s -> abc12345-1718900000.mp4
• avg bitrate: 3212 kbps
```

Flags:
* `-d, --duration <SEC>` — recording duration (1..=180, default 10).
* `-b, --bit-rate <BPS>` — video bitrate (default 4 Mbps).
* `--width <PX>` — capture width (default 540).
* `--height <PX>` — capture height (default 960).
* `--rotate` — rotate 90° (for portrait→landscape captures).
* `-o, --out <PATH>` — output file path (default
  `<id-8>-<timestamp>.mp4`).

## Tests

* **Backend: 126/126 pass** (was 119 at M8; +7 new tests in M9.1).
  * `apk::bundle::tests::extract_bundle_*` — 6 new tests covering:
    * STORED entries are extracted with correct content + SHA-256.
    * `zip_paths` filter is honored (only requested entries are written).
    * `out_dir` is created recursively when missing.
    * Malicious entry names (`../../etc/passwd.apk`) are sanitized so
      they land inside `out_dir` (no path traversal).
    * Missing bundle file → error.
    * No matching entries → error.
  * `apk::bundle::tests::crc32_matches_known_vectors` — sanity check
    for the hand-rolled CRC-32 implementation used by the test ZIP
    builder (matches zlib's `crc32("")` = 0 and `crc32("123456789")`
    = 0xCBF43926).

* **CLI: builds clean.**
* **Release builds: ~6.9 MB / ~1.6 MB / ~6.0 MB** — within the
  1-GB VPS target budget (the `flate2` rust_backend added ~150 KB
  to the daemon).

## Files changed

* `backend/Cargo.toml` — added `flate2` with `rust_backend` feature.
* `backend/src/apk/bundle.rs` — added `extract_bundle` +
  `ExtractSpec` + `BundleExtractResult` + `ExtractedApk` types
  (~290 LOC, 7 new tests + 1 helper test).
* `backend/src/apk/mod.rs` — re-exported the new types.
* `backend/src/api/apk.rs` — new `POST /apk/extract` endpoint.
* `backend/src/api/screen.rs` — new `POST /containers/{id}/screen/record-mp4`
  endpoint + `RecordMp4Request` struct.
* `backend/src/container/isolation.rs` — added `extra_apks` field to
  `IsolationSpec` + `DROIDKER_EXTRA_APKS` env var threading.
* `backend/src/container/manager.rs` — added `resolve_extra_apks`
  helper + populate `iso_spec.extra_apks` in `start()`.
* `backend/src/models/container.rs` — added `extra_apks: Vec<String>`
  to `Container` + `CreateContainerRequest`; added
  `extra_apks_count: usize` to `ContainerSummary`.
* `backend/src/bin/init.rs` — added `install_extra_apks` function
  (reads `DROIDKER_EXTRA_APKS`, copies each split to
  `/data/app/<pkg>/split_<n>.apk`).
* `cli/Cargo.toml` — added `bytes` dep (for `record_mp4` response).
* `cli/src/client.rs` — added `extract_bundle` + `record_mp4` methods.
* `cli/src/commands.rs` — added `run_bundle` + `mp4` command handlers
  + `map_bundle_abi_to_cli` helper.
* `cli/src/main.rs` — added `RunBundle` + `Mp4` subcommands.
* `docs/MILESTONE-9.md` — this file.

## Outstanding (deferred to M10+)

* **Full APK signature cryptographic validation.** M8.1 only checks
  that a signature *exists* and extracts the cert fingerprint — it
  doesn't verify the signature against the APK contents. A full
  verifier would need RSA + EC + x509 parsers (~500 KB of native
  deps). Defer until we have a compelling reason to add the weight.

* **Opus audio codec.** VAD (M8.3) gives most of the bandwidth
  benefit at zero native-dep cost, but Opus would still be 2–5×
  better for sustained audio (e.g. music apps). Defer until libopus
  can be loaded dynamically only when needed.

* **Async MP4 capture job API.** The current `POST /record-mp4`
  endpoint is synchronous, which works for CLI usage but is awkward
  for browsers. A future async variant would return a job_id
  immediately and expose `GET /record-mp4/jobs/<id>` for polling +
  `GET /record-mp4/jobs/<id>/download` for fetching the result.

* **WebRTC screen streaming option.** Currently MJPEG over WS —
  fine for a single viewer on a 1-vCPU VPS, but a WebRTC path would
  give sub-100ms latency for interactive use. Deferred to M10+
  because a WebRTC SFU on a 1-GB VPS is a tight fit.
