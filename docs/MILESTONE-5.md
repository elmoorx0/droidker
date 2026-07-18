# Milestone 5 — Humanized Input + Audio + Recording

**Status**: code-complete · 51 backend tests pass · CLI + frontend build
clean · all release binaries (droidkerd 6.5 MB, droidker-init 1.5 MB,
droidker CLI 5.8 MB) build with LTO.

M5 turns DroidKer from "a sandbox with a screen stream" into a fully
drivable, automatable Android device. Three capabilities land in this
milestone:

  1. The container can finally *receive* the touches the user sends —
     the InputInjector's `/dev/input/eventN` node is bind-mounted into
     the container so Android's EventHub auto-detects it.
  2. The Humanizer engine from M1 (Bezier curves + Box-Muller Gaussian
     jitter) is wired to the InputInjector, so a single API call
     produces a human-looking tap, swipe, or long-press instead of an
     instant down+up that bot detectors flag.
  3. Audio streaming over WebSocket + screen recording to MJPEG for
     CI artifacts round out the automation story.

## What landed

### 1. /dev/input bind-mount (`backend/src/bin/init.rs`, `backend/src/container/isolation.rs`)

Before M5, the InputInjector created a virtual touchscreen on the host
(`/dev/input/eventN`), wrote touch events to it, but the container
*couldn't see them* because its `/dev/input/` was either empty or held
only the host's devices. The kernel delivered events to the host's
InputReader, not the container's.

M5 fixes this with a three-step wiring:

  1. `ContainerManager::start()` creates the `InputInjector` *before*
     spawning the sandbox and waits up to 500 ms for the kernel to
     allocate `/dev/input/eventN` (via the new
     `InputInjector::wait_for_event_path()` poll loop).
  2. The discovered eventN path is passed to `droidker-init` via the
     `DROIDKER_INPUT_EVENT` env var.
  3. `setup_dev_nodes()` in `droidker-init` bind-mounts that single
     event node into the container's `/dev/input/event0`.

Only the touchscreen event node is exposed — not the whole `/dev/input/`
directory — so the container cannot read other host input devices
(keyboard, mouse, host touchscreen). Inside the container the device
always appears as `event0` so Android's EventHub assigns it the primary
touchscreen slot during InputReader initialization.

### 2. Humanizer gesture engine (`backend/src/humanizer/gestures.rs`, new ~300 lines)

The math primitives from M1 (`BezierPath`, `HumanizerEngine` with
xorshift64 + Box-Muller) are now wrapped by three high-level gesture
functions:

| Function | What it does | Event count |
|---|---|---|
| `tap(x, y)` | Pre-delay (finger approaching) → DOWN → Gaussian hold → UP | 2 + 2 sleeps |
| `swipe(start, end)` | Build Bezier curve, walk it in N steps, emit one MOVE per step | 2 + N (N derived from distance + random human speed) |
| `long_press(x, y, ms)` | DOWN → periodic MOVE with ±2 px drift → UP | 2 + (ms/50) |

Each gesture uses:
  - **Bezier paths** for swipes (start → randomized control → end),
    not straight lines. Curvature is randomized 20–60 px perpendicular
    to the swipe direction.
  - **Box-Muller Gaussian jitter** on inter-sample delays (base ± stddev).
    Default sample interval: 16 ± 4 ms (~60 Hz with natural variance).
  - **Variable pressure** clamped to [40, 255] — Android drivers treat
    <40 as "no touch".
  - **Position drift** during long-press (±2 px) so Android's
    InputReader doesn't classify the touch as stationary and suppress
    the long-press callback.

`HumanizerEngine` exposes two new public methods:
  - `next_uniform()` → `[0, 1)` — used by the gesture layer to derive
    symmetric jitter.
  - `next_signed()` → `[-1, 1)` — convenience wrapper for the drift
    math.

### 3. Humanized gesture API (`backend/src/api/screen.rs`)

Three new REST endpoints:

```
POST /api/v1/containers/{id}/screen/human/tap
  Body: { "x": 270, "y": 480, "config": { ... } }   # config optional
  Resp: { "container_id": "...", "gesture": "tap", "duration_ms": 142 }

POST /api/v1/containers/{id}/screen/human/swipe
  Body: { "start_x": 100, "start_y": 800, "end_x": 100, "end_y": 200 }
  Resp: { "container_id": "...", "gesture": "swipe", "duration_ms": 423 }

POST /api/v1/containers/{id}/screen/human/longpress
  Body: { "x": 270, "y": 480, "hold_ms": 800 }
  Resp: { "container_id": "...", "gesture": "longpress", "duration_ms": 905 }
```

Because gestures sleep for real wall-clock time (tens of ms), each
handler runs the blocking gesture on `tokio::task::spawn_blocking` so
the actix worker thread isn't tied up. The `InputInjector` is locked
across the entire gesture via `blocking_lock()` — concurrent raw
`/screen/touch` calls will queue rather than interleave events.

Each request gets its own `HumanizerEngine` seeded from
`container_uuid_low_64_bits XOR nanos`, so successive gestures on the
same container produce uncorrelated jitter patterns.

### 4. CLI subcommands (`cli/src/main.rs`, `cli/src/commands.rs`)

```
droidker htap    <id|name> <x> <y>
droidker hswipe  <id|name> <x1> <y1> <x2> <y2>
droidker hlongpress <id|name> <x> <y> <hold_ms>
droidker record  <id|name> [--out FILE] [--duration SEC]
                 [--fps 1..30] [--quality 10..95]
```

The `record` subcommand opens a WebSocket to `/screen/ws`, negotiates
the requested fps + quality via the existing control-message protocol,
and captures frames until the deadline.

Output format (`MJP1`):
```
+---------+---------------------+
| 'MJP1'  | u32 LE frame_count |     8-byte file header
+---------+---------------------+
Per frame:
+--------+--------+--------+--------+-----------+
| width  | height | n_bytes| ts_ms  | JPEG data |
| u32 LE | u32 LE | u32 LE | u32 LE |  N bytes  |
+--------+--------+--------+--------+-----------+
```

A 30-second recording at 5 FPS q=70 540×960 is ~3 MB. The format is
trivially parseable by any binary tool; a Python post-processor can
extract individual JPEGs or transcode to MP4 with Pillow/ffmpeg if
needed. We deliberately avoided standard .avi/.mp4 to keep the CLI free
of native codec dependencies.

Progress reporting: every 5 frames the CLI prints elapsed time, total
frames, and total KB. On WS close mid-recording, the frame count is
patched in the header and the file is closed cleanly.

### 5. Audio streaming (`backend/src/streaming/audio.rs`, new ~600 lines)

A new `AudioWs` actor mirrors `ScreenWs`. The daemon captures audio from
the container and pushes raw PCM chunks to the browser, which decodes
them with the Web Audio API. No native AAC encoder is required — the
daemon stays a 6.5 MB Rust binary.

**Endpoint**: `GET /api/v1/containers/{id}/audio/ws?sample_rate=8000&channels=1`

**Wire format** (binary WebSocket frames):
```
+----------+----------+----------+----------+
| rate u32 | chan u16 | bits u16 | count u32|   12-byte header
+----------+----------+----------+----------+
| raw s16le PCM samples ...                            |
+----------------------------------------------------+
```

Text control messages from the client:
```json
{ "type": "set_format", "sample_rate": 16000, "channels": 1 }
{ "type": "set_volume", "volume": 0.8 }
{ "type": "ping" }
```

**Capture source priority**:

| # | Source | When it works |
|---|---|---|
| 1 | ALSA loopback (`arecord -D hw:1,0`) | Host has `snd-aloop` loaded; AudioFlinger routed to loopback. |
| 2 | `screenrecord --output-format=wav` | Stub for M6 — requires nsenter into the container. |
| 3 | Test tone (440 Hz sine, -14 dBFS) | Dev mode, skeleton rootfs, no ALSA. Proves the streaming path. |

**Why raw PCM and not AAC?**
  - AAC encoders (`fdk-aac`, `ffmpeg`) pull in native dependencies that
    bloat the daemon and conflict with the "single 6 MB Rust binary"
    target for 1-GB VPS hosts.
  - At 8 kHz mono s16le, raw PCM is 16 KB/s. A 60-second session is
    ~1 MB of WebSocket traffic. Acceptable.
  - Higher quality (Opus, RFC 6716) is M6 territory — the pure-Rust
    `opus` crate has a decoder; we'd pair it with the reference encoder.

**Heartbeat**: ping every 30 s, close after 60 s of client silence
(matching `ScreenWs` behavior).

## Files changed in M5

```
backend/src/api/screen.rs               (extended — 3 gesture endpoints + audio WS)
backend/src/bin/init.rs                 (extended — /dev/input bind-mount)
backend/src/container/isolation.rs      (extended — DROIDKER_INPUT_EVENT env)
backend/src/container/manager.rs        (extended — InputInjector created at start)
backend/src/humanizer/gestures.rs       (new, ~300 lines)
backend/src/humanizer/input.rs          (extended — public next_uniform, next_signed)
backend/src/humanizer/mod.rs            (extended — re-exports)
backend/src/streaming/audio.rs          (new, ~600 lines)
backend/src/streaming/input.rs          (extended — wait_for_event_path)
backend/src/streaming/mod.rs            (extended — audio module)

cli/src/client.rs                       (extended — human_tap/swipe/long_press)
cli/src/commands.rs                     (extended — htap/hswipe/hlongpress/record)
cli/src/main.rs                         (extended — 4 new subcommands)
```

## Test coverage

Backend tests: **51 pass / 0 fail** (was 45 at end of M4).

New tests added in M5:
  - `humanizer::gestures::tests::gesture_config_defaults_are_reasonable`
  - `humanizer::gestures::tests::sample_pressure_stays_in_bounds`
  - `humanizer::gestures::tests::next_signed_is_bounded`
  - `streaming::audio::tests::audio_format_bytes_per_second`
  - `streaming::audio::tests::audio_format_bytes_for_ms`
  - `streaming::audio::tests::chunk_header_packs_correctly`
  - `streaming::audio::tests::encode_chunk_round_trips`
  - `streaming::audio::tests::test_tone_produces_correct_byte_count`
  - `streaming::audio::tests::aac_encoder_is_passthrough`

The CLI has no unit tests (it's a thin shell over the HTTP client); the
backend tests cover the wire format and math that the CLI depends on.

## Release binaries

| Binary | Size | Notes |
|---|---|---|
| `droidkerd` | 6.5 MB | Daemon. LTO + strip. |
| `droidker-init` | 1.5 MB | PID 1. LTO + strip. |
| `droidker` | 5.8 MB | CLI. LTO + strip. |

All three build with `cargo build --release --bins` and have zero
native dependencies beyond glibc.

## What's next (M6)

- **Opus audio** — swap the PCM pass-through for a real Opus encoder so
  we can ship 16 kHz stereo at the same bitrate as 8 kHz mono PCM today.
- **nsenter-based screenrecord** — capture audio via Android's own
  `screenrecord --output-format=wav` when ALSA loopback isn't available.
- **Pinch-zoom + two-finger gestures** — extend the gesture engine with
  multi-slot InputInjector sequences.
- **ARM→x86_64 translation** — integrate libhoudini/libndk so DroidKer
  can run ARM APKs on x86_64 VPS hosts (the vast majority of cheap
  1-vCPU VPS offerings are x86_64).
- **WebRTC option** — for sub-100ms screen latency, offer WebRTC as an
  alternative to MJPEG-over-WS. Still optional — the WS path stays the
  default for single-viewer scenarios.
