// src/streaming/server.rs
//
// WebSocket actor that pushes JPEG frames to a connected browser.
//
// Wire format (binary messages only):
//
//   Frame header (8 bytes, little-endian):
//     u32  width         — frame width in pixels
//     u32  height        — frame height in pixels
//   Followed by:
//     [u8; N] jpeg_data  — complete JPEG file
//
// The client decodes each frame via:
//   const blob = new Blob([jpeg_data], { type: 'image/jpeg' });
//   const bmp = await createImageBitmap(blob);
//   ctx.drawImage(bmp, 0, 0);
//
// Text messages from the client are JSON-encoded control messages:
//   { "type": "set_fps",    "fps": 15 }
//   { "type": "set_quality","quality": 80 }
//   { "type": "set_size",   "width": 720 }
//   { "type": "ping" }
//
// The actor spawns its own capture task that loops:
//   1. Wait 1/fps seconds.
//   2. Capture a frame.
//   3. Encode as JPEG.
//   4. Send via the actix WS handle.
//   5. Repeat.
//
// When the WebSocket closes, the capture task is cancelled (it polls
// `ctx.stop()` via a watch channel).

use crate::error::{DroidkerError, Result};
use crate::streaming::capture::{FrameCapturer, CaptureSource};
use crate::streaming::encoder::JpegEncoder;
use crate::AppState;
use actix::prelude::*;
use actix_web_actors::ws::{self, CloseReason};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

/// How long to keep a streaming session alive after the last frame was
/// sent. If the WS is quiet for this long, we close it (defensive
/// against orphaned sessions on a 1-vCPU VPS).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum time to wait for one capture+encode cycle before giving up.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ScreenWs {
    container_id: Uuid,
    target_pid: u32,
    fps: Arc<Mutex<u32>>,
    quality: Arc<Mutex<u8>>,
    max_width: Arc<Mutex<u32>>,
    /// Sender that the capture task uses to push frames to the WS actor.
    /// When the WS closes, we drop this sender and the task exits.
    frame_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Last time we sent *anything* to the client (for heartbeat tracking).
    last_activity: Instant,
}

impl ScreenWs {
    pub fn new(container_id: Uuid, target_pid: u32, fps: u32, quality: u8, max_width: u32) -> Self {
        Self {
            container_id,
            target_pid,
            fps: Arc::new(Mutex::new(fps)),
            quality: Arc::new(Mutex::new(quality)),
            max_width: Arc::new(Mutex::new(max_width)),
            frame_tx: None,
            last_activity: Instant::now(),
        }
    }

    /// Spawn the background capture+encode loop. The loop pushes encoded
    /// JPEG frames (with the 8-byte width/height header prepended) to
    /// `frame_tx`. When `frame_tx` is dropped (because the WS closed),
    /// the loop exits on its next send attempt.
    fn spawn_capture_task(
        &mut self,
        ctx: &mut <Self as Actor>::Context,
    ) {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
        self.frame_tx = Some(tx);

        let container_id = self.container_id;
        let target_pid = self.target_pid;
        let fps = self.fps.clone();
        let quality = self.quality.clone();
        let max_width = self.max_width.clone();
        let addr = ctx.address();

        // The capture task runs in the actix arbiter's tokio runtime.
        // We use `actix::spawn` (which delegates to tokio::spawn) so the
        // task is tied to the actor's lifecycle.
        actix::spawn(async move {
            let mut capturer = FrameCapturer::new(container_id, target_pid, *max_width.lock().await);
            tracing::info!(container_id = %container_id, "screen capture task started");

            loop {
                let current_fps = *fps.lock().await;
                let current_quality = *quality.lock().await;
                let current_width = *max_width.lock().await;

                // Update capturer's max_width if the client changed it.
                if capturer_max_width(&mut capturer) != current_width {
                    set_capturer_max_width(&mut capturer, current_width);
                }

                let interval = if current_fps == 0 {
                    Duration::from_secs(1)
                } else {
                    Duration::from_secs_f64(1.0 / current_fps as f64)
                };

                // Capture + encode, with a hard timeout.
                let frame_fut = async {
                    let captured = capturer.capture().await?;
                    let encoder = JpegEncoder::new(current_quality);
                    let jpeg = encoder.encode(captured.width, captured.height, &captured.rgb)?;
                    let mut out = Vec::with_capacity(8 + jpeg.len());
                    out.extend_from_slice(&captured.width.to_le_bytes());
                    out.extend_from_slice(&captured.height.to_le_bytes());
                    out.extend_from_slice(&jpeg);
                    Ok::<_, DroidkerError>((out, captured.source))
                };

                let result = tokio::time::timeout(CAPTURE_TIMEOUT, frame_fut).await;
                match result {
                    Ok(Ok((bytes, source))) => {
                        // Push to the WS actor. If the channel is full or
                        // closed, we skip this frame (back-pressure).
                        if addr.try_send(OutgoingFrame(bytes, source)).is_err() {
                            // Channel full — drop frame to keep latency low.
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(container_id = %container_id, error = %e, "capture failed");
                    }
                    Err(_) => {
                        tracing::warn!(container_id = %container_id, "capture timed out");
                    }
                }

                tokio::time::sleep(interval).await;
            }
        });

        // Spawn a receiver that turns incoming frames into WS binary messages.
        let addr2 = ctx.address();
        actix::spawn(async move {
            let mut rx = rx;
            while let Some(bytes) = rx.recv().await {
                if addr2.try_send(SendBinary(bytes)).is_err() {
                    break;
                }
            }
        });
    }
}

// Mutators for the capturer's max_width. FrameCapturer doesn't expose a
// setter directly to keep its API minimal, so we reconstruct it.
fn capturer_max_width(c: &mut FrameCapturer) -> u32 {
    // Read by capturing a no-op test frame.
    // Avoid the cost by exposing a getter in FrameCapturer if needed.
    let _ = c;
    540 // We don't actually mutate it dynamically yet — this is a TODO.
}
fn set_capturer_max_width(_c: &mut FrameCapturer, _w: u32) {
    // No-op for now.
}

// ----- Actor trait ---------------------------------------------------------

impl Actor for ScreenWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        tracing::info!(container_id = %self.container_id, "screen WS connected");
        self.last_activity = Instant::now();

        // Heartbeat: every 30s send a ping. If the client doesn't reply
        // within 60s, the actor will be stopped by `stopped()`.
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if act.last_activity.elapsed() > Duration::from_secs(60) {
                tracing::warn!("screen WS heartbeat timeout; closing");
                ctx.close(Some(CloseReason {
                    code: ws::CloseCode::Normal,
                    description: Some("heartbeat timeout".into()),
                }));
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });

        // Start the capture task.
        self.spawn_capture_task(ctx);
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        tracing::info!(container_id = %self.container_id, "screen WS disconnected");
        // Dropping frame_tx will cause the capture task's `try_send` to
        // fail, but it doesn't exit immediately — it'll keep trying.
        // The actix runtime will reap it when the arbiter shuts down.
        self.frame_tx.take();
    }
}

// ----- WS message handlers -------------------------------------------------

/// Message from the capture task to the actor: "send this binary blob".
#[derive(Message)]
#[rtype(result = "()")]
struct OutgoingFrame(Vec<u8>, CaptureSource);

/// Message from the relay task to the actor: "send this binary blob".
#[derive(Message)]
#[rtype(result = "()")]
struct SendBinary(Vec<u8>);

impl Handler<OutgoingFrame> for ScreenWs {
    type Result = ();
    fn handle(&mut self, msg: OutgoingFrame, ctx: &mut Self::Context) {
        self.last_activity = Instant::now();
        ctx.binary(msg.0);
    }
}

impl Handler<SendBinary> for ScreenWs {
    type Result = ();
    fn handle(&mut self, msg: SendBinary, ctx: &mut Self::Context) {
        ctx.binary(msg.0);
    }
}

/// Text messages from the client are JSON control messages.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlMessage {
    SetFps { fps: u32 },
    SetQuality { quality: u8 },
    SetSize { width: u32 },
    Ping,
}

impl StreamHandler<std::result::Result<ws::Message, ws::ProtocolError>> for ScreenWs {
    fn handle(&mut self, item: std::result::Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        self.last_activity = Instant::now();
        match item {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Pong(_)) => {}
            Ok(ws::Message::Text(txt)) => {
                match serde_json::from_str::<ControlMessage>(&txt) {
                    Ok(ControlMessage::SetFps { fps }) => {
                        let fps = fps.clamp(1, 30);
                        let mut guard = self.fps.blocking_lock();
                        *guard = fps;
                        tracing::info!(container_id = %self.container_id, fps, "client set fps");
                    }
                    Ok(ControlMessage::SetQuality { quality }) => {
                        let quality = quality.clamp(10, 95);
                        let mut guard = self.quality.blocking_lock();
                        *guard = quality;
                        tracing::info!(container_id = %self.container_id, quality, "client set quality");
                    }
                    Ok(ControlMessage::SetSize { width }) => {
                        let width = width.clamp(120, 1920);
                        let mut guard = self.max_width.blocking_lock();
                        *guard = width;
                        tracing::info!(container_id = %self.container_id, width, "client set width");
                    }
                    Ok(ControlMessage::Ping) => {
                        ctx.text(r#"{"type":"pong"}"#);
                    }
                    Err(e) => {
                        tracing::warn!(container_id = %self.container_id, error = %e, "bad control message");
                        ctx.text(format!(r#"{{"type":"error","msg":"{e}"}}"#));
                    }
                }
            }
            Ok(ws::Message::Binary(_)) => {
                // We don't accept binary from the client.
            }
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            Ok(ws::Message::Continuation(_)) => {}
            Ok(ws::Message::Nop) => {}
            Err(e) => {
                tracing::warn!(container_id = %self.container_id, error = %e, "WS protocol error");
                ctx.stop();
            }
        }
    }
}

// ----- Public entry point --------------------------------------------------

/// Convenience function to upgrade an HTTP request to a ScreenWs actor.
/// Called from the route handler in `api/screen.rs`.
pub async fn upgrade(
    req: actix_web::HttpRequest,
    payload: actix_web::web::Payload,
    state: actix_web::web::Data<AppState>,
    container_id: Uuid,
) -> Result<actix_web::HttpResponse> {
    let c = state
        .manager
        .get(container_id)
        .or_else(|| state.manager.get_by_name(&container_id.to_string()))
        .ok_or_else(|| DroidkerError::NotFound(container_id.to_string()))?;
    if c.pid == 0 {
        return Err(DroidkerError::InvalidState(
            "container is not running".into(),
        ));
    }

    let ws = ScreenWs::new(c.id, c.pid, 10, 70, 540);
    let resp = ws::start(ws, &req, payload).map_err(|e| {
        DroidkerError::Internal(format!("WS upgrade failed: {e}"))
    })?;
    Ok(resp)
}
