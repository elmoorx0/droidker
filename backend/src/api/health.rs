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

    // M6: surface the host arch + translation capability so the dashboard
    // and CLI can warn the user when their APK's target arch isn't
    // supported on this host.
    let host_arch = crate::container::translation::Arch::detect_host();
    // Probe the host for libhoudini / libndk_translation / qemu-user.
    let arm64_strategy = crate::container::translation::resolve_strategy(
        host_arch,
        crate::container::translation::Arch::Arm64,
    );
    let arm_strategy = crate::container::translation::resolve_strategy(
        host_arch,
        crate::container::translation::Arch::Arm,
    );

    HttpResponse::Ok().json(json!({
        "ready": data_dir_exists,
        "data_dir": state.settings.data_dir.display().to_string(),
        "containers_loaded": state.manager.list().len(),
        "host_arch": host_arch.as_str(),
        "translation": {
            "arm64-v8a": crate::container::translation::strategy_summary(&arm64_strategy),
            "armeabi-v7a": crate::container::translation::strategy_summary(&arm_strategy),
        },
    }))
}
