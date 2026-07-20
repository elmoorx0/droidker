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
            .service(screen_human_pinch)
            .service(audio_ws)
            .service(screen_record_mp4),
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

/// Request body for `POST /screen/human/pinch` (M8.4).
#[derive(Debug, Deserialize)]
pub struct HumanPinchRequest {
    /// X coordinate of the pinch center.
    pub center_x: i32,
    /// Y coordinate of the pinch center.
    pub center_y: i32,
    /// Initial distance between the two fingers, in pixels. Typical
    /// zoom-in starting distance: 30 px (fingers close together).
    pub start_distance: f64,
    /// Final distance between the two fingers, in pixels. For a zoom-in
    /// this is larger than `start_distance`; for a zoom-out, smaller.
    pub end_distance: f64,
    /// Orientation of the pinch line in degrees. 0° = horizontal,
    /// 90° = vertical, 45° = diagonal (the human default).
    #[serde(default = "default_pinch_angle")]
    pub angle_deg: f64,
    #[serde(default)]
    pub config: Option<GestureConfig>,
}

fn default_pinch_angle() -> f64 {
    45.0
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

/// POST /api/v1/containers/{id}/screen/human/pinch
///
/// Performs a two-finger pinch-zoom gesture (M8.4). The two fingers
/// start at `start_distance` apart and end at `end_distance` apart,
/// oriented along `angle_deg` (0° = horizontal, 90° = vertical,
/// 45° = diagonal which is the human default).
///
/// When `end_distance > start_distance`, the gesture is a zoom-in.
/// When `end_distance < start_distance`, it's a zoom-out. Both
/// directions use the same endpoint — the direction is implicit in
/// the relationship between the two distances.
#[post("/{id}/screen/human/pinch")]
async fn screen_human_pinch(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<HumanPinchRequest>,
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
        humanizer::pinch_zoom(
            &mut inj,
            &mut h,
            (req.center_x, req.center_y),
            req.start_distance,
            req.end_distance,
            req.angle_deg,
            &cfg,
        )
    })
    .await
    .map_err(|e| DroidkerError::Syscall(format!("gesture task join: {e}")))??;

    Ok(HttpResponse::Ok().json(HumanGestureResponse {
        container_id: id.to_string(),
        gesture: "pinch",
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

// ----- M9.2: MP4 screenrecord capture ----------------------------------

/// Body for `POST /api/v1/containers/{id}/screen/record-mp4`.
///
/// Runs Android's `screenrecord` binary inside the container's
/// namespaces via `nsenter` and returns the resulting MP4 bytes as
/// the response body. The recording is synchronous — the request
/// blocks until `screenrecord` exits (either because `duration_sec`
/// elapsed or because `screenrecord` hit its 3-minute hard cap).
#[derive(Debug, Deserialize)]
struct RecordMp4Request {
    /// Recording duration in seconds. Capped at 180 (the Android
    /// `screenrecord` hard limit per-file). Required.
    duration_sec: u32,
    /// Video bit rate in bits per second. Higher = better quality +
    /// bigger file. Default 4 Mbps is fine for most app demos; bump to
    /// 8 Mbps for game captures with rapid motion.
    #[serde(default = "default_bit_rate")]
    bit_rate: u32,
    /// Capture width in pixels. Defaults to 540 (qHD), matching the
    /// screen streamer's resolution. Pass the container's actual
    /// framebuffer width if you know it (e.g. 1080 for FHD devices).
    #[serde(default = "default_width")]
    width: u32,
    /// Capture height in pixels. Defaults to 960.
    #[serde(default = "default_height")]
    height: u32,
    /// Rotate the recording 90 degrees. Useful for portrait apps being
    /// recorded in landscape orientation. Default false.
    #[serde(default)]
    rotate: bool,
}

fn default_bit_rate() -> u32 {
    4_000_000
}
fn default_width() -> u32 {
    540
}
fn default_height() -> u32 {
    960
}

/// POST /api/v1/containers/{id}/screen/record-mp4  (M9.2)
///
/// Captures the container's screen to an MP4 file via Android's
/// `screenrecord` binary, then streams the file back to the caller.
///
/// The recording runs synchronously: this endpoint blocks for
/// `duration_sec` seconds (or until `screenrecord` exits on its own),
/// then returns the MP4 bytes as `video/mp4`. The caller (CLI) is
/// expected to set a long enough HTTP timeout — we bump actix's default
/// via the request's keep-alive, but if there's a reverse proxy in
/// front, make sure it allows at least `duration_sec + 5` seconds.
///
/// Implementation:
///   1. Resolve the container's PID.
///   2. `nsenter --target=PID --pid --mount --ipc --net -- /system/bin/screenrecord
///       --time-limit N --bit-rate RATE --size WxH [--rotate] /tmp/droidker-rec.mp4`
///   3. Read the resulting file from the host's view of the container's
///      overlay upperdir: `<data_dir>/overlays/<id>/upper/tmp/droidker-rec.mp4`.
///   4. Return the bytes; delete the temp file.
///
/// On a 1-vCPU VPS, `screenrecord` at 540x960 + 4 Mbps takes ~50% CPU.
/// For longer / higher-quality captures, run the daemon on a 2-vCPU box.
#[post("/{id}/screen/record-mp4")]
async fn screen_record_mp4(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<RecordMp4Request>,
) -> Result<impl Responder> {
    use std::process::Stdio;
    use tokio::process::Command;

    let key = path.into_inner();
    let id = resolve_id(&state, &key)?;
    let req = body.into_inner();

    // Clamp duration_sec to screenrecord's 3-minute cap.
    let duration = req.duration_sec.clamp(1, 180);
    let bit_rate = req.bit_rate.max(1_000_000); // 1 Mbps minimum sanity floor
    let size = format!("{}x{}", req.width.max(64), req.height.max(64));

    let container = state
        .manager
        .get(id)
        .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?;
    if container.pid == 0 {
        return Err(DroidkerError::InvalidState(
            "container is not running".into(),
        ));
    }

    // Pick a unique filename so concurrent recordings don't collide.
    let rec_filename = format!("droidker-rec-{}.mp4", uuid::Uuid::new_v4());
    let container_tmp_path = format!("/tmp/{rec_filename}");

    // Build the screenrecord invocation. We nsenter into the container's
    // PID + mount + IPC + net namespaces so screenrecord sees the same
    // /system/bin/screenrecord, talks to SurfaceFlinger via binder (IPC ns),
    // and can resolve /tmp via the merged mount table.
    let mut cmd = Command::new("nsenter");
    cmd.arg(format!("--target={}", container.pid))
        .args(["--pid", "--mount", "--ipc", "--net", "--"])
        .args(["/system/bin/screenrecord"])
        .args([
            "--time-limit",
            &duration.to_string(),
            "--bit-rate",
            &bit_rate.to_string(),
            "--size",
            &size,
        ]);
    if req.rotate {
        cmd.arg("--rotate");
    }
    cmd.arg(&container_tmp_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    tracing::info!(
        container_id = %id,
        pid = container.pid,
        duration_sec = duration,
        bit_rate,
        size = %size,
        rotate = req.rotate,
        path = %container_tmp_path,
        "starting screenrecord"
    );

    // Run synchronously, capped at duration + 10s grace. screenrecord
    // sometimes hangs for a few seconds after the time limit while it
    // flushes the encoder.
    let timeout_dur = std::time::Duration::from_secs((duration + 10) as u64);
    let outcome = tokio::time::timeout(timeout_dur, cmd.output()).await;
    let output = match outcome {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(DroidkerError::Internal(format!(
                "screenrecord spawn failed: {e}"
            )));
        }
        Err(_) => {
            return Err(DroidkerError::Internal(format!(
                "screenrecord timed out after {timeout_dur:?}"
            )));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DroidkerError::Internal(format!(
            "screenrecord exited with status {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    // Read the resulting MP4 from the host's view of the container's
    // overlay upperdir. The overlay is mounted as
    //   lowerdir=<android_rootfs> : upperdir=<data_dir>/overlays/<id>/upper
    //   workdir=<data_dir>/overlays/<id>/work
    // so any write to /tmp/<file> inside the container lands in
    // <data_dir>/overlays/<id>/upper/tmp/<file> on the host.
    let host_mp4_path = state
        .settings
        .data_dir
        .join("overlays")
        .join(id.to_string())
        .join("upper")
        .join("tmp")
        .join(&rec_filename);

    let mp4_bytes = tokio::fs::read(&host_mp4_path).await.map_err(|e| {
        DroidkerError::Internal(format!(
            "screenrecord completed but MP4 file not found at {}: {e}",
            host_mp4_path.display()
        ))
    })?;

    // Best-effort cleanup of the temp file inside the container.
    let _ = tokio::fs::remove_file(&host_mp4_path).await;

    tracing::info!(
        container_id = %id,
        size_bytes = mp4_bytes.len(),
        "screenrecord MP4 ready"
    );

    // Stream the bytes back to the caller with the right content type.
    Ok(HttpResponse::Ok()
        .content_type("video/mp4")
        .append_header(("Content-Length", mp4_bytes.len().to_string()))
        .append_header((
            "Content-Disposition",
            format!(
                "attachment; filename=\"droidker-{}-{}.mp4\"",
                id,
                chrono::Utc::now().timestamp()
            ),
        ))
        .body(mp4_bytes))
}
