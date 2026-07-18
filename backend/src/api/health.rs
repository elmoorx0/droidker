// src/api/health.rs
//
// Liveness + readiness probes. Used by systemd and the dashboard's status
// indicator.

use actix_web::{get, web, HttpResponse, Responder};
use serde_json::json;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(health).service(readiness);
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().json(json!({ "status": "ok" }))
}

#[get("/ready")]
async fn readiness(state: web::Data<crate::AppState>) -> impl Responder {
    // Confirm we can reach the data directory.
    let data_dir_exists = state.settings.data_dir.exists();
    HttpResponse::Ok().json(json!({
        "ready": data_dir_exists,
        "data_dir": state.settings.data_dir.display().to_string(),
        "containers_loaded": state.manager.list().len(),
    }))
}
