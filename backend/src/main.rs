// src/main.rs
//
// DroidKer Backend Daemon (droidkerd)
// Entry point for the API server that manages Android APK micro-containers.
//
// Architecture overview:
//   - actix-web HTTP API exposes CRUD operations on containers
//   - ContainerManager owns the in-memory state + spawns child processes
//   - Each container runs in its own Linux namespace/cgroup sandbox
//   - The Android runtime (ART + Bionic + microG) is bind-mounted from a
//     shared rootfs to keep per-container disk usage near zero

use actix_cors::Cors;
use actix_web::{middleware, web, App, HttpServer};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod api;
mod config;
mod container;
mod error;
mod exec;
mod humanizer;
mod logs;
mod models;
mod seccomp;
mod stats;
mod streaming;
mod ws;

use crate::config::Settings;
use crate::container::ContainerManager;

/// Shared application state passed to every actix handler.
/// Cheap to clone (Arc inside) so we can hand it to worker threads.
#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<ContainerManager>,
    pub settings: Arc<Settings>,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // --- Load .env and initialize structured logging -----------------------
    let _ = dotenvy::dotenv();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,droidker_backend=debug"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_target(true))
        .init();

    tracing::info!("DroidKer backend daemon starting up...");

    // --- Load configuration --------------------------------------------------
    let settings = Settings::load().unwrap_or_else(|e| {
        tracing::warn!("Failed to load config ({}), using defaults", e);
        Settings::default()
    });

    let bind_addr = format!("{}:{}", settings.host, settings.port);
    tracing::info!("API server will bind to {}", bind_addr);
    tracing::info!("Data directory: {}", settings.data_dir.display());
    tracing::info!("Android rootfs: {}", settings.android_rootfs.display());

    // --- Initialize the container manager -----------------------------------
    let manager = Arc::new(
        ContainerManager::new(settings.clone())
            .expect("Failed to initialize container manager"),
    );

    // --- Validate the Android rootfs (warn-only mode so the daemon still
    //     starts on a fresh host before `build-rootfs.sh` has been run).
    if let Err(e) = crate::container::rootfs::validate_android_rootfs(&settings) {
        tracing::warn!(
            error = %e,
            "Android rootfs validation failed — containers will not start until this is resolved. \
             Run: sudo bash scripts/build-rootfs.sh"
        );
    }

    let state = AppState {
        manager,
        settings: Arc::new(settings),
    };

    // --- Start the HTTP server ----------------------------------------------
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .wrap(middleware::Logger::default())
            .wrap(middleware::NormalizePath::trim())
            .wrap(
                Cors::permissive()
                    .allowed_methods(vec!["GET", "POST", "DELETE", "PUT", "PATCH"])
                    .max_age(3600),
            )
            .configure(api::configure_routes)
    })
    .bind(&bind_addr)?
    .workers(2) // Keep worker count low — this targets 1-vCPU VPS hosts
    .run()
    .await
}
