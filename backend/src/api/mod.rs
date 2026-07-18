// src/api/mod.rs
//
// Public HTTP API surface of the DroidKer backend.
//
// All routes are mounted under `/api/v1/*` so we can ship breaking changes
// under `/api/v2/*` later without touching existing clients (CLI, dashboard).

pub mod apk;
pub mod containers;
pub mod exec;
pub mod health;
pub mod logs;
pub mod screen;
pub mod stats;
pub mod upload;

use actix_web::web;

pub fn configure_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1")
            .configure(health::configure)
            .configure(containers::configure)
            .configure(upload::configure)
            .configure(stats::configure)
            .configure(logs::configure)
            .configure(exec::configure)
            .configure(apk::configure),
    );
}
