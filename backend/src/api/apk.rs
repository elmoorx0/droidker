// src/api/apk.rs
//
// APK inspection endpoint (M7.1).
//
//   GET  /api/v1/apk/inspect?path=<filename>
//   POST /api/v1/apk/inspect   { "apk": "<filename>" }
//
// Both forms look up the APK under `<data_dir>/apks/<filename>` and return
// the inspect result (list of native ABIs + recommended arch). The CLI
// uses this to implement `droidker run --arch auto`: it uploads the APK,
// then calls /apk/inspect to pick the best arch before creating the
// container.

use crate::apk::{inspect_apk, InspectResult};
use crate::error::{DroidkerError, Result};
use crate::AppState;
use actix_web::{get, post, web, HttpResponse, Responder};
use serde::Deserialize;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(inspect_by_query).service(inspect_by_body);
}

/// GET /api/v1/apk/inspect?path=<filename>
///
/// `filename` is the value returned by `POST /api/v1/upload/apk`
/// (typically `<sha256>.apk`). It must not contain path separators —
/// we reject anything that looks like a traversal attempt.
#[get("/apk/inspect")]
async fn inspect_by_query(
    state: web::Data<AppState>,
    query: web::Query<InspectQuery>,
) -> Result<impl Responder> {
    let filename = &query.path;
    let result = perform_inspect(&state, filename)?;
    Ok(HttpResponse::Ok().json(result))
}

/// POST /api/v1/apk/inspect  { "apk": "<filename>" }
///
/// Same semantics as the GET form, but accepts a JSON body for clients
/// that prefer POST (e.g. when the filename is very long or contains
/// query-string-reserved characters).
#[post("/apk/inspect")]
async fn inspect_by_body(
    state: web::Data<AppState>,
    body: web::Json<InspectBody>,
) -> Result<impl Responder> {
    let filename = &body.apk;
    let result = perform_inspect(&state, filename)?;
    Ok(HttpResponse::Ok().json(result))
}

#[derive(Deserialize)]
struct InspectQuery {
    path: String,
}

#[derive(Deserialize)]
struct InspectBody {
    apk: String,
}

fn perform_inspect(state: &AppState, filename: &str) -> Result<InspectResult> {
    // Reject path separators — the filename must be a bare name under
    // <data_dir>/apks/, never an arbitrary host path.
    if filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.is_empty()
    {
        return Err(DroidkerError::BadRequest(format!(
            "invalid apk filename: {:?} (must be a bare name, no path separators)",
            filename
        )));
    }
    if !filename.to_lowercase().ends_with(".apk") {
        return Err(DroidkerError::BadRequest(
            "apk filename must end with .apk".into(),
        ));
    }
    let path = state.settings.data_dir.join("apks").join(filename);
    let result = inspect_apk(&path).map_err(|e| DroidkerError::Internal(format!("{e}")))?;
    Ok(result)
}
