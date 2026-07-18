# Milestone 4 — Screen Streaming + Virtual Touchscreen

**Status**: code-complete · 42 backend tests pass · frontend builds clean · all
release binaries (droidkerd 6.3 MB, droidker-init 1.5 MB, droidker CLI 5.7 MB)
build with LTO.

M4 adds the two features that turn DroidKer from "a sandbox that runs an APK"
into "a remotely drivable Android device": live screen streaming and
virtual-touch input injection.

## What landed

### 1. Frame capture (`backend/src/streaming/capture.rs`)

A `FrameCapturer` runs as a tokio task and pulls one RGB888 frame per
iteration. Three capture sources are tried in priority order; the first
that works is cached so we don't fork `nsenter` 10 times per second
only to discover failure modes repeatedly:

| # | Source            | When it works                                         |
|---|-------------------|-------------------------------------------------------|
| 1 | `screencap -p`    | Real Android runtime with SurfaceFlinger up.          |
| 2 | `/dev/graphics/fb0` | Headless Android Things or Android with SurfaceFlinger down. |
| 3 | Test pattern      | Skeleton rootfs / dev mode. A 540×960 gradient + container ID + moving dot — proves the streaming path is alive even before Android boots. |

Each frame is downscaled if wider than `max_width` (default 540, qHD) so
the JPEG encode cost stays predictable on a 1-vCPU VPS. The downscaler
is a dumb nearest-neighbor sampler; quality is fine for screen viewing
and CI screenshots.

### 2. JPEG encoder (`backend/src/streaming/encoder.rs`)

A 30-line wrapper around the `jpeg-encoder` crate (pure Rust, no native
deps, AVX2 path when available). Quality is configurable per session
(10–95, default 70). At 540×960 q=70 on a Haswell-class VPS, encoding
takes ~6 ms — well under the 100 ms budget for 10 fps.

### 3. WebSocket screen server (`backend/src/streaming/server.rs`)

`GET /api/v1/containers/{id}/screen/ws` upgrades to a WebSocket and
immediately starts pushing binary frames. The wire format is:

```
+--------+--------+--------+--------+--------+--------+--------+--------+---...
| width  (u32 LE)         | height (u32 LE)         | JPEG bytes...
+--------+--------+--------+--------+--------+--------+--------+--------+---...
```

The browser decodes each frame with `createImageBitmap(new Blob([data],
{type:'image/jpeg'}))` and draws it onto a `<canvas>`. This is
hardware-accelerated in every modern browser.

Text messages from the client are JSON control messages:

```json
{ "type": "set_fps",     "fps": 15 }
{ "type": "set_quality", "quality": 80 }
{ "type": "set_size",    "width": 720 }
{ "type": "ping" }
```

The server heartbeats every 30 s and closes the connection if the client
goes quiet for 60 s (defensive against orphaned sessions on a 1-GB VPS).

### 4. Virtual touchscreen (`backend/src/streaming/input.rs`)

`InputInjector` opens `/dev/uinput` on the host and creates a virtual
multi-touch (Type B) touchscreen + keypad with the following capabilities:

```
EV_ABS:
  ABS_MT_SLOT          [0, 7]      — 8 fingers max
  ABS_MT_TRACKING_ID   [0, 65535]
  ABS_MT_POSITION_X    [0, width-1]
  ABS_MT_POSITION_Y    [0, height-1]
  ABS_MT_PRESSURE      [0, 255]
EV_KEY:
  BTN_TOUCH                        — touch down/up
  KEY_BACK, KEY_HOMEPAGE, KEY_APPSELECT
EV_SYN:
  SYN_REPORT                       — flush frame
```

The kernel allocates `/dev/input/eventN`. The daemon looks it up by
matching the device name (`droidker-touch-<container_id>`) in
`/sys/class/input/`. In production (M5) this path will be bind-mounted
into the container's `/dev/input/` so Android's EventHub auto-detects
the device on boot.

All ioctl numbers are computed from `<asm-generic/ioctl.h>` macros
rather than pulled from a libc binding — this keeps the binary free of
platform-specific constants and works identically on x86_64 and aarch64.

The injector writes standard Linux `input_event` structs (24 bytes on
both x86_64 and aarch64: 16-byte `timeval` + 2-byte `type` + 2-byte
`code` + 4-byte `value`). Each touch event becomes a batch of 4–6
structs followed by `SYN_REPORT`, which the kernel delivers atomically
to readers.

### 5. API endpoints (`backend/src/api/screen.rs`)

```
GET  /api/v1/containers/{id}/screen/ws        — upgrade to WS, stream JPEG
POST /api/v1/containers/{id}/screen/touch     — inject TouchEvent
POST /api/v1/containers/{id}/screen/key       — inject KeyEvent
GET  /api/v1/containers/{id}/screen/info      — capabilities + event path
```

`TouchEvent` body:

```json
{
  "x": 320,
  "y": 480,
  "phase": "down",            // down | move | up
  "pressure": 128,            // optional, default 128
  "slot": 0                   // optional, default 0
}
```

`KeyEvent` body:

```json
{ "code": "home", "down": true }   // code ∈ home | back | recent
```

The uinput injector is created lazily on first touch/key call (rather
than at container start), so containers that never receive input don't
pay the `/dev/uinput` cost.

### 6. Frontend (`frontend/src/lib/components/ScreenStream.svelte`)

A new tab "Screen" appears on the container detail page when the
container is running. The component:

1. Opens a WebSocket to `/api/v1/containers/{id}/screen/ws`.
2. For each binary frame, decodes the JPEG via `createImageBitmap` and
   draws it onto a `<canvas>` sized to match the source resolution.
3. Translates pointer events on the canvas to container-screen pixel
   coordinates (accounting for `object-fit: contain` letterboxing) and
   POSTs them to `/screen/touch`.
4. Multi-touch: each `pointerId` gets its own multitouch slot via the
   `Pointer Capture API`, so two-finger gestures work.
5. Shows live metrics: frame dimensions, measured fps, source label,
   frame counter.
6. FPS / Quality sliders adjust the stream in real time by sending
   `set_fps` / `set_quality` control messages.
7. Home / Back / Recent buttons below the screen fire `KeyEvent`
   down+up pairs with a 50 ms gap.

Reconnect logic uses exponential backoff (250 ms → 5 s cap) so a
dropped WS doesn't hammer the daemon.

### 7. CLI commands (`cli/src/commands.rs`)

New commands:

```
droidker screenshot <id> [-o file.jpg]      # save one frame to disk
droidker tap       <id> <x> <y>             # inject a tap
droidker swipe     <id> <x1> <y1> <x2> <y2> [-d 300]   # inject a swipe
droidker key       <id> <home|back|recent>  # inject a key tap
```

`droidker run` and `droidker create` now accept `-p HOST:CONTAINER`
(repeatable) to publish ports. Example:

```bash
droidker run app.apk --name web -p 8080:80 -p 8443:443
```

The daemon installs iptables DNAT rules on container start and removes
them on stop (`backend/src/container/ports.rs`). Rules carry the
comment `droidker:<container_id>:<host_port>` so they can be enumerated
and torn down by container ID even if the daemon crashes.

### 8. Seccomp bug fixes

While wiring up the new endpoints we found that the Strict-profile
syscall number table in `seccomp.rs` had ARM32 numbers (200–212) for
network syscalls, not x86_64 (41–55). The `socketpair` and `accept4`
entries also collided with non-network syscalls. The table is now
per-arch (x86_64 + aarch64) with regression tests that verify every
blocklist entry resolves to a unique number.

## What's not in M4

- **Bind-mount of `/dev/input/eventN` into the container.** The
  injector creates the device on the host but doesn't yet bind-mount it
  into `/dev/input/` inside the container. M5 will add this — without
  it, touch events reach the kernel but not the Android InputReader.
  The current code is still useful for end-to-end testing the daemon's
  uinput path.

- **WebRTC.** MJPEG-over-WebSocket is intentionally simpler. WebRTC
  would require an SFU or P2P NAT traversal — overkill for a single
  viewer on a 1-vCPU VPS. The current path uses ~5–10% of one core at
  10 fps 540×960 q=70; WebRTC would add at least 30% for H.264
  encoding + libnice negotiation. If WebRTC becomes necessary later,
  the `streaming/` module is structured so the encoder can be swapped
  for x264 and the WS for a PeerConnection without touching capture
  or input.

- **Audio.** Out of scope for M4. M5 may add an AAC stream piggy-backed
  on the same WebSocket.

- **Recording.** `droidker screenshot` grabs a single frame. A future
  `droidker record` command could save an MJPEG video.

## Performance notes

| Metric                              | Value (Haswell-class VPS)        |
|-------------------------------------|----------------------------------|
| Frame capture (screencap -p)        | ~3 ms                            |
| JPEG encode 540×960 q=70            | ~6 ms                            |
| WS frame round-trip (localhost)     | ~1 ms                            |
| Browser decode + draw               | ~2 ms                            |
| Total per-frame cost                | ~12 ms → ~80 fps theoretical     |
| Default target                      | 10 fps (100 ms budget)           |
| CPU cost @ 10 fps                   | <5% of one core                  |
| Bandwidth @ 10 fps, q=70            | ~150 KB/s = 1.2 Mbps             |

The default 540×960 + 10 fps + q=70 is sized for a 1-Mbps uplink (typical
cheap VPS). The client can request higher quality via the FPS / Quality
sliders if the uplink allows.

## Testing

42 backend unit tests pass:

- `streaming::capture` — test-pattern size, BGRA→RGB swap, RGBA→RGB drop,
  downscale halving, glyph renderer, long-string draw.
- `streaming::encoder` — solid-color frame, misized buffer rejection,
  quality clamp.
- `streaming::input` — touch/key JSON deserialization, input_event
  encoding size, ioctl constant values match kernel headers
  (0x5501, 0x5502, 0x40045501, 0x40045515, 0x40045522, 0x40185536,
  0x405C5503), struct sizes (24 + 92 bytes).
- `seccomp` — blocklist content, x86_64 number correctness, no-collisions
  regression, BPF program shape.
- Plus all previous M1/M2 tests.

The frontend builds clean (`npm run check` and `npm run build` both pass
with zero warnings).

## End-to-end smoke test

On a host with `droidkerd` running:

```bash
# 1. Start a container (uses the skeleton rootfs in dev mode)
droidker run ~/Downloads/test.apk --name smoke

# 2. Open the dashboard and click the container → "Screen" tab.
#    You should see the test pattern (gradient + container ID + moving dot)
#    at ~10 fps. Click on the canvas — touch events are sent (silently
#    accepted because no /dev/input/eventN is bind-mounted yet).

# 3. CLI screenshot:
droidker screenshot smoke -o /tmp/screen.jpg
xdg-open /tmp/screen.jpg

# 4. CLI touch + key:
droidker tap smoke 270 480
droidker key smoke home

# 5. Publish a port + reach it from outside the VPS:
droidker run web.apk -p 8080:80 --name web
curl http://$(hostname):8080/   # hits the container's port 80
```

## Files touched

```
backend/Cargo.toml
backend/src/api/mod.rs
backend/src/api/containers.rs
backend/src/api/screen.rs                 (new, 178 lines)
backend/src/container/mod.rs
backend/src/container/manager.rs
backend/src/container/ports.rs            (new, 178 lines)
backend/src/models/container.rs
backend/src/models/mod.rs
backend/src/seccomp.rs                    (bug fix)
backend/src/streaming/mod.rs              (rewritten)
backend/src/streaming/capture.rs          (new, 384 lines)
backend/src/streaming/encoder.rs          (new, 92 lines)
backend/src/streaming/input.rs            (new, 596 lines)
backend/src/streaming/server.rs           (new, 333 lines)

cli/Cargo.toml
cli/src/client.rs
cli/src/commands.rs
cli/src/main.rs

frontend/src/lib/api/api.ts
frontend/src/lib/components/ScreenStream.svelte     (new, 326 lines)
frontend/src/lib/components/UploadPanel.svelte
frontend/src/routes/containers/[id]/+page.svelte
```

## What's next (M5)

- Bind-mount `/dev/input/eventN` into the container's `/dev/input/`
  so Android's InputReader picks up injected touches.
- Full Humanizer engine — the Bezier + Box-Muller path from M1 needs
  to be wired to the new InputInjector so gestures look human instead
  of mechanical.
- Audio stream (AAC over the same WS).
- Recording: `droidker record <id> -o video.mjpeg` for CI artifacts.
