// src/ws/logs.rs
//
// WebSocket actor that streams a container's log file (init / runtime /
// logcat) to a client, similar to `tail -f`.
//
// Wire protocol (JSON over text frames, server -> client only):
//
//   { "type": "data",  "kind": "runtime", "data": "<base64>" }
//   { "type": "rotated", "kind": "runtime" }
//   { "type": "error", "message": "..." }
//
// The client opens the WS with a query string like:
//   /api/v1/containers/{id}/logs/ws?kind=runtime&follow_from_start=false

use crate::error::Result;
use crate::logs::{LogKind, LogStreamer, LogTailRequest};
use actix::prelude::*;
use actix_web_actors::ws;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);

pub struct LogWs {
    pub container_id: Uuid,
    pub overlay_dir: std::path::PathBuf,
    pub kind: LogKind,
    pub follow_from_start: bool,
    pub last_pong: Instant,
    /// Receiver for byte chunks pushed by the background tail task.
    pub rx: Option<mpsc::Receiver<Vec<u8>>>,
}

impl LogWs {
    pub fn new(
        container_id: Uuid,
        overlay_dir: std::path::PathBuf,
        kind: LogKind,
        follow_from_start: bool,
    ) -> Self {
        Self {
            container_id,
            overlay_dir,
            kind,
            follow_from_start,
            last_pong: Instant::now(),
            rx: None,
        }
    }
}

impl Actor for LogWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.last_pong) > CLIENT_TIMEOUT {
                tracing::warn!("logs WS client timeout, closing");
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });

        // Spawn the tailer.
        let streamer = LogStreamer::new(self.container_id, self.overlay_dir.clone());
        let req = LogTailRequest {
            kind: self.kind,
            follow_from_start: self.follow_from_start,
        };
        let addr = ctx.address().clone();
        // We need to call `streamer.tail()` which is async. We use
        // `actix::spawn` to run it on the actix-rt tokio runtime.
        actix::spawn(async move {
            match streamer.tail(req).await {
                Ok(mut rx) => {
                    while let Some(chunk) = rx.recv().await {
                        let _ = addr.try_send(LogChunk(chunk));
                    }
                    let _ = addr.try_send(LogChunk(Vec::new())); // signal EOF
                }
                Err(e) => {
                    let _ = addr.try_send(LogError(format!("tail failed: {e}")));
                }
            }
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        tracing::info!(container_id = %self.container_id, kind = ?self.kind, "logs WS closed");
    }
}

#[derive(actix::Message)]
#[rtype(result = "()")]
struct LogChunk(Vec<u8>);

#[derive(actix::Message)]
#[rtype(result = "()")]
struct LogError(String);

impl Handler<LogChunk> for LogWs {
    type Result = ();
    fn handle(&mut self, msg: LogChunk, ctx: &mut Self::Context) {
        if msg.0.is_empty() {
            // EOF from the tailer (receiver dropped).
            // Don't close — the client may still want to issue a fresh tail
            // via reconnect. We just stop sending data.
            return;
        }
        let b64 = base64_encode(&msg.0);
        let frame = serde_json::json!({
            "type": "data",
            "kind": LogKind::filename(&self.kind),
            "data": b64,
        });
        ctx.text(frame.to_string());
    }
}

impl Handler<LogError> for LogWs {
    type Result = ();
    fn handle(&mut self, msg: LogError, ctx: &mut Self::Context) {
        let frame = serde_json::json!({
            "type": "error",
            "message": msg.0,
        });
        ctx.text(frame.to_string());
    }
}

impl StreamHandler<std::result::Result<ws::Message, ws::ProtocolError>> for LogWs {
    fn handle(&mut self, item: std::result::Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "logs WS protocol error");
                ctx.stop();
                return;
            }
        };
        match msg {
            ws::Message::Ping(m) => {
                self.last_pong = Instant::now();
                ctx.pong(&m);
            }
            ws::Message::Pong(_) => {
                self.last_pong = Instant::now();
            }
            ws::Message::Close(reason) => {
                ctx.close(reason);
                ctx.stop();
            }
            _ => {}
        }
    }
}

/// Cheap base64 encoder (mirrors `exec::session::base64_encode`).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Resolve query params into a `LogKind`.
pub fn parse_kind(s: &str) -> LogKind {
    LogKind::from_str_lossy(s)
}

/// Convenience used by the route handler: build a LogWs from path + query.
pub fn build_actor(
    container_id: Uuid,
    overlay_dir: std::path::PathBuf,
    kind_str: &str,
    follow_from_start: bool,
) -> LogWs {
    LogWs::new(container_id, overlay_dir, parse_kind(kind_str), follow_from_start)
}

/// Result type alias for route handler convenience.
#[allow(dead_code)]
pub type TailResult = Result<()>;
