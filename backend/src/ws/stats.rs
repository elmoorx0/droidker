// src/ws/stats.rs
//
// WebSocket actor that pushes live cgroup-v2 stats for a container at a
// fixed interval. Used by the dashboard to render the live CPU/memory
// graphs.
//
// Wire protocol (JSON over text frames, server -> client only):
//
//   { "type": "stats", "data": { ...ContainerStats... } }
//   { "type": "error", "message": "..." }
//
// The client opens the WS with a query string like:
//   /api/v1/containers/{id}/stats/ws?interval=1000
// `interval` is in milliseconds (default 1000, min 200, max 10000).

use crate::error::{DroidkerError, Result};
use crate::stats::StatsReader;
use actix::prelude::*;
use actix_web_actors::ws;
use std::time::{Duration, Instant};
use uuid::Uuid;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_INTERVAL_MS: u64 = 1000;
const MIN_INTERVAL_MS: u64 = 200;
const MAX_INTERVAL_MS: u64 = 10_000;

pub struct StatsWs {
    pub container_id: Uuid,
    pub host_pid: u32,
    pub interval: Duration,
    pub last_pong: Instant,
    pub reader: StatsReader,
}

impl StatsWs {
    pub fn new(container_id: Uuid, host_pid: u32, interval_ms: u64) -> Self {
        let clamped = interval_ms.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS);
        Self {
            container_id,
            host_pid,
            interval: Duration::from_millis(clamped),
            last_pong: Instant::now(),
            reader: StatsReader::new(),
        }
    }
}

impl Actor for StatsWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Heartbeat.
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.last_pong) > CLIENT_TIMEOUT {
                tracing::warn!("stats WS client timeout, closing");
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });

        // Snapshot pump. We sample synchronously inside the interval callback
        // because the snapshot is a quick filesystem read (no I/O blocking
        // long enough to stall the actor).
        let interval = self.interval;
        ctx.run_interval(interval, |act, ctx| {
            if act.host_pid == 0 {
                ctx.text(
                    serde_json::json!({
                        "type": "error",
                        "message": "container is not running"
                    })
                    .to_string(),
                );
                ctx.stop();
                return;
            }
            match act.reader.snapshot(act.container_id, act.host_pid) {
                Ok(stats) => {
                    let frame = serde_json::json!({
                        "type": "stats",
                        "data": stats,
                    });
                    ctx.text(frame.to_string());
                }
                Err(DroidkerError::NotFound(_)) => {
                    // Cgroup is gone — container has been stopped/deleted.
                    ctx.text(
                        serde_json::json!({
                            "type": "error",
                            "message": "container cgroup no longer exists"
                        })
                        .to_string(),
                    );
                    ctx.stop();
                }
                Err(e) => {
                    ctx.text(
                        serde_json::json!({
                            "type": "error",
                            "message": format!("snapshot failed: {e}")
                        })
                        .to_string(),
                    );
                }
            }
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        tracing::info!(container_id = %self.container_id, "stats WS closed");
    }
}

impl StreamHandler<std::result::Result<ws::Message, ws::ProtocolError>> for StatsWs {
    fn handle(&mut self, item: std::result::Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "stats WS protocol error");
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

/// Parse the `interval` query parameter, falling back to the default.
pub fn parse_interval_ms(s: Option<&str>) -> u64 {
    s.and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_INTERVAL_MS)
}
