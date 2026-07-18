// src/streaming/mod.rs
//
// Screen streaming + virtual input for DroidKer containers (Milestone 4).
//
// Architecture
// ------------
// The daemon captures the container's framebuffer and pushes JPEG frames
// to every connected browser over a WebSocket (`/screen/ws`). The browser
// renders each frame onto a `<canvas>` and translates pointer/touch events
// into JSON messages that are POSTed to `/screen/touch`. The daemon then
// writes those events to a `/dev/uinput` virtual touchscreen that is
// bind-mounted into the container, so Android's InputReader picks them up
// like real touches.
//
// Frame capture strategy (in priority order):
//   1. `screencap -p` inside the container — emits a PNG of the current
//      SurfaceFlinger output. We decode it to RGB888 and re-encode as JPEG.
//      This is the only path that works on a fully-booted Android runtime.
//   2. `/dev/graphics/fb0` mmap — raw framebuffer, BGRA8888 or RGB565.
//      Used when `screencap` is unavailable but the kernel exposes fb0.
//   3. Test pattern — a synthetic 540x960 gradient with the container ID
//      overlaid. Used in dev mode and on the skeleton rootfs.
//
// Why MJPEG over WebSocket (not WebRTC)?
//   - WebRTC requires an SFU or P2P NAT traversal — overkill for a single
//     viewer on a 1-vCPU VPS.
//   - MJPEG is just JPEG frames sent as WebSocket binary messages; the
//     browser decodes them with `createImageBitmap(new Blob([data]))`,
//     which is hardware-accelerated in every modern browser.
//   - CPU cost: ~5-10% of one core at 10 FPS, 540x960, q=70 on a Haswell
//     class VPS. Acceptable.
//
// Why uinput and not an evdev socket?
//   - uinput lets us create a *real* input device that Android's
//     InputReader discovers via `/dev/input/event*`. No code changes are
//     needed in the Android runtime — it just sees a touchscreen.
//   - The alternative (a binder call to InputManager.injectInputEvent)
//     would require knowing the binder transaction codes for the Android
//     version on the host, which is fragile.

pub mod audio;
pub mod capture;
pub mod encoder;
pub mod input;
pub mod server;

pub use audio::{AudioCapturer, AudioFormat, AudioWs, AacEncoder};
pub use capture::{CapturedFrame, FrameCapturer, CaptureSource};
pub use encoder::JpegEncoder;
pub use input::{InputInjector, TouchEvent, TouchPhase, KeyEvent, KeyCode};
pub use server::ScreenWs;

use serde::{Deserialize, Serialize};

/// Configuration for a single streaming session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    /// Target frame rate (frames per second). The capturer may emit fewer
    /// frames if the source is slower. Default 10.
    pub fps: u32,
    /// JPEG quality (1-100, higher = bigger files). Default 70.
    pub quality: u8,
    /// Maximum width. Frames are downscaled if the source is wider.
    /// Default 540 (qHD; small enough for 1-vCPU).
    pub max_width: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            fps: 10,
            quality: 70,
            max_width: 540,
        }
    }
}
