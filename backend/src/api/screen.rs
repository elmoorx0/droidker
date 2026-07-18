// src/api/screen.rs
//
// REST + WebSocket endpoints for screen streaming + input injection.
//
// Routes (mounted under /api/v1/containers):
//   GET  /{id}/screen/ws              — upgrade to WebSocket; streams JPEG frames
//   POST /{id}/screen/touch           — inject a single raw touch event
//   POST /{id}/screen/key             — inject a key event (home/back/recent)
//   GET  /{id}/screen/info            — query streaming capabilities + state
//   POST /{id}/screen/human/tap       — humanized tap (M5)
//   POST /{id}/screen/human/swipe     — humanized swipe along a Bezier path (M5)
//   POST /{id}/screen/human/longpress — humanized long-press (M5)

use crate::error::{DroidkerError, Result};
use crate::humanizer::{self, GestureConfig, HumanizerEngine};
use crate::streaming::audio::{self, AudioFormat};
use crate::streaming::input::{InputInjector, KeyEvent, TouchEvent};
use crate::streaming::server::upgrade as ws_upgrade;
use crate::AppState;
use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
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
            .service(screen_info)
            .service(screen_human_tap)
            .service(screen_human_swipe)
            .service(screen_human_longpress)
            .service(audio_ws),
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

// ----- M5: humanized gesture endpoints --------------------------------------
//
// These endpoints take a high-level intent ("tap here", "swipe from A to B",
// "long-press here for 800ms") and expand it into a sequence of low-level
// touch events with Bezier-curve paths and Gaussian-jittered timings.
//
// The blocking gesture functions live in `humanizer::gestures`. Because
// they sleep (real wall-clock time) for tens of milliseconds per gesture,
// we run them on `tokio::task::spawn_blocking` so the actix worker thread
// is not tied up. Each request gets its own `HumanizerEngine` instance
// seeded from the container_id + current time, so successive calls
// produce uncorrelated jitter.

/// Request body for `POST /screen/human/tap`.
#[derive(Debug, Deserialize)]
pub struct HumanTapRequest {
    pub x: i32,
    pub y: i32,
    /// Optional override of the default gesture config.
    #[serde(default)]
    pub config: Option<GestureConfig>,
}

/// Request body for `POST /screen/human/swipe`.
#[derive(Debug, Deserialize)]
pub struct HumanSwipeRequest {
    pub start_x: i32,
    pub start_y: i32,
    pub end_x: i32,
    pub end_y: i32,
    #[serde(default)]
    pub config: Option<GestureConfig>,
}

/// Request body for `POST /screen/human/longpress`.
#[derive(Debug, Deserialize)]
pub struct HumanLongPressRequest {
    pub x: i32,
    pub y: i32,
    /// How long to hold the press, in milliseconds.
    pub hold_ms: u32,
    #[serde(default)]
    pub config: Option<GestureConfig>,
}

/// Generic response from any humanized gesture endpoint.
#[derive(Debug, Serialize)]
pub struct HumanGestureResponse {
    pub container_id: String,
    pub gesture: &'static str,
    /// Total wall-clock milliseconds spent sleeping inside the gesture.
    /// Useful for clients that want to schedule the next action without
    /// racing the previous one's tail.
    pub duration_ms: u32,
}

/// POST /api/v1/containers/{id}/screen/human/tap
#[post("/{id}/screen/human/tap")]
async fn screen_human_tap(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<HumanTapRequest>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    let req = body.into_inner();
    let cfg = req.config.unwrap_or_default();

    let injector = get_or_create_injector(&state, id).await?;
    // Hold the injector lock across the entire gesture — we don't want
    // a concurrent raw /screen/touch call to interleave events.
    let inj_arc = injector.clone();
    let duration_ms = tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut inj = inj_arc.blocking_lock();
        let mut h = HumanizerEngine::new(seed_for(id));
        humanizer::tap(&mut inj, &mut h, req.x, req.y, &cfg)
    })
    .await
    .map_err(|e| DroidkerError::Syscall(format!("gesture task join: {e}")))??;

    Ok(HttpResponse::Ok().json(HumanGestureResponse {
        container_id: id.to_string(),
        gesture: "tap",
        duration_ms,
    }))
}

/// POST /api/v1/containers/{id}/screen/human/swipe
#[post("/{id}/screen/human/swipe")]
async fn screen_human_swipe(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<HumanSwipeRequest>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    let req = body.into_inner();
    let cfg = req.config.unwrap_or_default();

    let injector = get_or_create_injector(&state, id).await?;
    let inj_arc = injector.clone();
    let duration_ms = tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut inj = inj_arc.blocking_lock();
        let mut h = HumanizerEngine::new(seed_for(id));
        humanizer::swipe(
            &mut inj,
            &mut h,
            (req.start_x, req.start_y),
            (req.end_x, req.end_y),
            &cfg,
        )
    })
    .await
    .map_err(|e| DroidkerError::Syscall(format!("gesture task join: {e}")))??;

    Ok(HttpResponse::Ok().json(HumanGestureResponse {
        container_id: id.to_string(),
        gesture: "swipe",
        duration_ms,
    }))
}

/// POST /api/v1/containers/{id}/screen/human/longpress
#[post("/{id}/screen/human/longpress")]
async fn screen_human_longpress(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<HumanLongPressRequest>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    let req = body.into_inner();
    let cfg = req.config.unwrap_or_default();

    let injector = get_or_create_injector(&state, id).await?;
    let inj_arc = injector.clone();
    let duration_ms = tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut inj = inj_arc.blocking_lock();
        let mut h = HumanizerEngine::new(seed_for(id));
        humanizer::long_press(&mut inj, &mut h, req.x, req.y, req.hold_ms, &cfg)
    })
    .await
    .map_err(|e| DroidkerError::Syscall(format!("gesture task join: {e}")))??;

    Ok(HttpResponse::Ok().json(HumanGestureResponse {
        container_id: id.to_string(),
        gesture: "longpress",
        duration_ms,
    }))
}

/// Build a per-request RNG seed from the container ID and the current
/// nanosecond count. This ensures successive gestures on the same
/// container produce different jitter patterns.
fn seed_for(id: Uuid) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // XOR the nanos with the container UUID's low 64 bits. The xorshift64
    // state must be nonzero, and UUIDs are random enough to guarantee that.
    let id_low = u64::from_be_bytes(id.as_bytes()[..8].try_into().unwrap_or([0u8; 8]));
    nanos ^ id_low
}

// ----- M5: audio streaming endpoint -----------------------------------------

/// GET /api/v1/containers/{id}/audio/ws
///
/// Upgrade to a WebSocket. The server pushes raw PCM audio chunks
/// (12-byte header + s16le samples). The browser decodes them with
/// the Web Audio API. See `streaming/audio.rs` for the wire format.
///
/// Query parameters:
///   sample_rate (default 8000) — 8000 | 16000 | 22050 | 44100
///   channels    (default 1)    — 1 (mono) | 2 (stereo)
#[get("/{id}/audio/ws")]
async fn audio_ws(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
    path: web::Path<String>,
    query: web::Query<std::collections::HashMap<String, String>>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;

    let sample_rate = query
        .get("sample_rate")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(8000)
        .clamp(4000, 48000);
    let channels = query
        .get("channels")
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(1)
        .clamp(1, 2);

    let format = AudioFormat {
        sample_rate,
        channels,
        bits_per_sample: 16,
    };

    tracing::info!(
        container_id = %id,
        format = ?format,
        "audio WS upgrade requested"
    );

    audio::upgrade(req, payload, id, format).await
}

// GestureConfig needs Deserialize for the API request bodies.
// We `#[derive(Deserialize)]` it here rather than in humanizer/gestures.rs
// so the humanizer module stays free of serde dependencies (it's used
// from contexts where serde isn't available, e.g. the test suite).
//
// Actually, serde IS available throughout the backend — but keeping the
// derive at the API boundary is cleaner. We re-export GestureConfig from
// humanizer::gestures already, so we just need Deserialize on the struct.
// To avoid editing gestures.rs, we wrap it: the request body's `config`
// field uses `GestureConfig` directly, which requires Deserialize. We
// add it via a derive in the humanizer module.

// Note: GestureConfig already derives Deserialize via `serde::Deserialize`
// imported in gestures.rs. If that import is missing, the build will
// fail here and we'll fix it.
