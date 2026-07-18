// src/api/logs.rs
//
// REST + WebSocket endpoints for container logs.
//
// Routes (mounted under /api/v1/containers):
//   GET  /{id}/logs?kind=runtime      — one-shot fetch (plain text)
//   GET  /{id}/logs/ws?kind=runtime   — live tail (WebSocket)

use crate::error::{DroidkerError, Result};
use crate::logs::{LogKind, LogStreamer};
use crate::ws::logs::build_actor;
use crate::AppState;
use actix_web::{get, web, HttpRequest, HttpResponse, Responder};
use actix_web_actors::ws as actix_ws;
use uuid::Uuid;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(get_logs).service(get_logs_ws);
}

/// GET /api/v1/containers/{id}/logs?kind=runtime&follow_from_start=false
///
/// Returns the full content of the requested log file as `text/plain`.
/// `kind` is one of `init`, `runtime`, `logcat` (defaults to `runtime`).
#[get("/{id}/logs")]
async fn get_logs(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    let kind = LogKind::from_str_lossy(&query.kind);
    let streamer = LogStreamer::new(c.id, state.settings.data_dir.join("overlays"));
    let bytes = streamer.snapshot(kind).await?;
    Ok(HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .body(bytes))
}

/// GET /api/v1/containers/{id}/logs/ws?kind=runtime&follow_from_start=false
///
/// Upgrades to a WebSocket and streams new log lines as they arrive.
#[get("/{id}/logs/ws")]
async fn get_logs_ws(
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<LogsQuery>,
    req: HttpRequest,
    stream: web::Payload,
) -> Result<HttpResponse> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    let kind_str = query.kind.clone();
    let follow_from_start = query.follow_from_start.unwrap_or(false);
    let actor = build_actor(
        c.id,
        state.settings.data_dir.join("overlays"),
        &kind_str,
        follow_from_start,
    );
    actix_ws::start(actor, &req, stream).map_err(|e| {
        DroidkerError::Internal(format!("ws upgrade failed: {e}"))
    })
}

#[derive(Debug, serde::Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_kind")]
    pub kind: String,
    pub follow_from_start: Option<bool>,
}

fn default_kind() -> String {
    "runtime".to_string()
}

/// Resolve a path parameter to a container record (UUID first, then name).
fn resolve_container(state: &AppState, key: &str) -> Result<crate::models::Container> {
    if let Ok(u) = Uuid::parse_str(key) {
        if let Some(c) = state.manager.get(u) {
            return Ok(c);
        }
    }
    state
        .manager
        .get_by_name(key)
        .ok_or_else(|| DroidkerError::NotFound(key.to_string()))
}
