// src/api/stats.rs
//
// REST + WebSocket endpoints for container resource stats.
//
// Routes (mounted under /api/v1/containers):
//   GET  /{id}/stats         — one-shot snapshot (JSON)
//   GET  /{id}/stats/ws      — live stats stream (WebSocket, M3)

use crate::error::{DroidkerError, Result};
use crate::stats::StatsReader;
use crate::ws::stats::{parse_interval_ms, StatsWs};
use crate::AppState;
use actix_web::{get, web, HttpRequest, HttpResponse, Responder};
use actix_web_actors::ws as actix_ws;
use uuid::Uuid;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(get_stats).service(get_stats_ws);
}

/// GET /api/v1/containers/{id}/stats — one-shot snapshot.
#[get("/{id}/stats")]
async fn get_stats(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    if c.pid == 0 {
        return Err(DroidkerError::InvalidState(
            "container is not running".into(),
        ));
    }
    let reader = StatsReader::new();
    let stats = reader.snapshot(c.id, c.pid)?;
    Ok(HttpResponse::Ok().json(stats))
}

/// GET /api/v1/containers/{id}/stats/ws?interval=1000 — live stats stream.
#[get("/{id}/stats/ws")]
async fn get_stats_ws(
    state: web::Data<AppState>,
    path: web::Path<String>,
    req: HttpRequest,
    stream: web::Payload,
    query: web::Query<StatsWsQuery>,
) -> Result<HttpResponse> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    let interval = parse_interval_ms(query.interval.as_deref());
    let actor = StatsWs::new(c.id, c.pid, interval);
    actix_ws::start(actor, &req, stream).map_err(|e| {
        DroidkerError::Internal(format!("ws upgrade failed: {e}"))
    })
}

#[derive(Debug, serde::Deserialize)]
pub struct StatsWsQuery {
    pub interval: Option<String>,
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
