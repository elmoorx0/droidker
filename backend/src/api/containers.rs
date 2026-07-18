// src/api/containers.rs
//
// REST endpoints for container lifecycle management.

use crate::error::{DroidkerError, Result};
use crate::models::{ContainerSummary, CreateContainerRequest};
use crate::AppState;
use actix_web::{delete, get, post, web, HttpResponse, Responder};
use uuid::Uuid;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/containers")
            .service(list_containers)
            .service(create_container)
            .service(get_container)
            .service(start_container)
            .service(stop_container)
            .service(delete_container)
            .configure(crate::api::screen::configure),
    );
}

/// GET /api/v1/containers — list all containers (lightweight summaries).
#[get("")]
async fn list_containers(state: web::Data<AppState>) -> impl Responder {
    let summaries: Vec<ContainerSummary> = state.manager.list();
    HttpResponse::Ok().json(summaries)
}

/// POST /api/v1/containers — create (but do not start) a container.
#[post("")]
async fn create_container(
    state: web::Data<AppState>,
    body: web::Json<CreateContainerRequest>,
) -> Result<impl Responder> {
    let container = state.manager.create(body.into_inner()).await?;
    Ok(HttpResponse::Created().json(container))
}

/// GET /api/v1/containers/{id} — full container record.
/// Accepts either a UUID or a container name.
#[get("/{id}")]
async fn get_container(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    Ok(HttpResponse::Ok().json(c))
}

/// POST /api/v1/containers/{id}/start — start the sandbox.
#[post("/{id}/start")]
async fn start_container(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let resolved = resolve_container(&state, &key)?;
    let started = state.manager.start(resolved.id).await?;
    Ok(HttpResponse::Ok().json(started))
}

/// POST /api/v1/containers/{id}/stop — gracefully stop the sandbox.
#[post("/{id}/stop")]
async fn stop_container(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    let stopped = state.manager.stop(c.id).await?;
    Ok(HttpResponse::Ok().json(stopped))
}

/// DELETE /api/v1/containers/{id} — remove the container record + overlay.
#[delete("/{id}")]
async fn delete_container(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    state.manager.delete(c.id).await?;
    Ok(HttpResponse::NoContent())
}

/// Resolve a path parameter to a container record by trying UUID first,
/// then falling back to name lookup.
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
