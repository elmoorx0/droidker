// src/api/screen.rs
//
// REST + WebSocket endpoints for screen streaming + input injection.
//
// Routes (mounted under /api/v1/containers):
//   GET  /{id}/screen/ws       — upgrade to WebSocket; streams JPEG frames
//   POST /{id}/screen/touch    — inject a touch event
//   POST /{id}/screen/key      — inject a key event (home/back/recent)
//   GET  /{id}/screen/info     — query streaming capabilities + state

use crate::error::{DroidkerError, Result};
use crate::streaming::input::{InputInjector, KeyEvent, TouchEvent};
use crate::streaming::server::upgrade as ws_upgrade;
use crate::AppState;
use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Per-app shared state: a map of container_id -> Arc<tokio::sync::Mutex<InputInjector>>.
/// The outer map is `std::sync::Mutex` because we only hold it briefly to
/// look up or insert. The inner is `tokio::sync::Mutex` because input
/// injection is async and we want `await` points to release the lock.
pub static INJECTORS: once_cell::sync::Lazy<Arc<StdMutex<HashMap<Uuid, Arc<Mutex<InputInjector>>>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(StdMutex::new(HashMap::new())));

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("")
            .service(screen_ws)
            .service(screen_touch)
            .service(screen_key)
            .service(screen_info),
    );
}

/// GET /api/v1/containers/{id}/screen/ws
///
/// Upgrade to a WebSocket. The server immediately starts pushing JPEG
/// frames (with an 8-byte width/height header). See `streaming/server.rs`
/// for the wire format.
#[get("/{id}/screen/ws")]
async fn screen_ws(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    ws_upgrade(req, payload, state, id).await
}

/// POST /api/v1/containers/{id}/screen/touch
///
/// Inject a touch event into the container's virtual touchscreen.
/// Body: `TouchEvent` JSON.
#[post("/{id}/screen/touch")]
async fn screen_touch(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<TouchEvent>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    let ev = body.into_inner();
    let injector = get_or_create_injector(&state, id).await?;
    let mut inj = injector.lock().await;
    inj.inject_touch(&ev)?;
    Ok(HttpResponse::NoContent())
}

/// POST /api/v1/containers/{id}/screen/key
///
/// Inject a key event (home/back/recent). Body: `KeyEvent` JSON.
#[post("/{id}/screen/key")]
async fn screen_key(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<KeyEvent>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    let ev = body.into_inner();
    let injector = get_or_create_injector(&state, id).await?;
    let mut inj = injector.lock().await;
    inj.inject_key(&ev)?;
    Ok(HttpResponse::NoContent())
}

/// GET /api/v1/containers/{id}/screen/info
///
/// Return streaming capabilities + the uinput event path (if any).
#[get("/{id}/screen/info")]
async fn screen_info(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;

    let injectors = INJECTORS.lock().unwrap();
    let event_path = match injectors.get(&id) {
        Some(inj_arc) => {
            // Try to lock and read the path without blocking.
            match inj_arc.try_lock() {
                Ok(inj) => inj.find_event_path().map(|p| p.to_string_lossy().into_owned()),
                Err(_) => None, // injector busy — skip
            }
        }
        None => None,
    };
    drop(injectors);

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "container_id": id.to_string(),
        "streaming": true,
        "input_injector_active": event_path.is_some(),
        "event_path": event_path,
        "default_fps": 10,
        "default_quality": 70,
        "default_max_width": 540,
    })))
}

// ----- helpers --------------------------------------------------------------

/// Resolve a container ID or name to a Uuid.
fn resolve_id(state: &AppState, key: &str) -> Result<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(key) {
        if state.manager.get(uuid).is_some() {
            return Ok(uuid);
        }
    }
    state
        .manager
        .get_by_name(key)
        .map(|c| c.id)
        .ok_or_else(|| DroidkerError::NotFound(key.to_string()))
}

/// Get or create the uinput injector for a container. The injector is
/// created lazily on first touch/key call rather than at container start,
/// so containers that never receive input don't pay the /dev/uinput cost.
async fn get_or_create_injector(
    state: &AppState,
    id: Uuid,
) -> Result<Arc<tokio::sync::Mutex<InputInjector>>> {
    // Fast path: injector already exists.
    {
        let injectors = INJECTORS.lock().unwrap();
        if let Some(inj) = injectors.get(&id) {
            return Ok(inj.clone());
        }
    }

    // Slow path: create a new injector.
    let c = state
        .manager
        .get(id)
        .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?;
    if c.pid == 0 {
        return Err(DroidkerError::InvalidState(
            "container is not running".into(),
        ));
    }
    // Default screen size: 540x960 (qHD). In production we'd read this from
    // SurfaceFlinger's DisplayInfo once Android is up.
    let injector = InputInjector::new(id, 540, 960)?;
    let arc = Arc::new(tokio::sync::Mutex::new(injector));
    {
        let mut injectors = INJECTORS.lock().unwrap();
        injectors.insert(id, arc.clone());
    }
    Ok(arc)
}
