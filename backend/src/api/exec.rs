// src/api/exec.rs
//
// REST + WebSocket endpoints for running commands inside a running container.
//
// Routes (mounted under /api/v1/containers):
//   POST /{id}/exec         — one-shot exec, returns collected stdout/stderr
//   GET  /{id}/exec/ws      — interactive exec, bidirectional bytes over WS

use crate::error::{DroidkerError, Result};
use crate::exec::{ExecRequest, ExecSession};
use crate::ws::exec::{spawn_session_for, ExecWs};
use crate::AppState;
use actix_web::{get, post, web, HttpRequest, HttpResponse, Responder};
use actix_web_actors::ws as actix_ws;
use serde::Serialize;
use std::time::Duration;
use uuid::Uuid;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(exec_one_shot).service(exec_ws);
}

/// POST /api/v1/containers/{id}/exec
///
/// Runs a command inside the container, waits up to `timeout_ms` for it to
/// finish, and returns the combined stdout/stderr + exit code.
///
/// Request body: `ExecRequest` (cmd, env, cwd, tty, user).
#[post("/{id}/exec")]
async fn exec_one_shot(
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<ExecRequest>,
) -> Result<impl Responder> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    if c.pid == 0 {
        return Err(DroidkerError::InvalidState(
            "container is not running".into(),
        ));
    }
    let req = body.into_inner();
    let session = ExecSession::spawn(c.pid, req).await?;
    let pid = session.child.id();

    // Drive the session to completion, draining stdout + stderr into buffers.
    // In PTY mode there's no separate stderr (it's merged into stdout via
    // the pty), so we only read from one stream.
    let (mut child, mut stdout, mut stderr) = (
        session.child,
        session.stdout,
        session.stderr,
    );
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    let timeout = Duration::from_secs(30);
    let read_looped = tokio::time::timeout(timeout, async {
        use tokio::io::AsyncReadExt;
        let mut tmp_out = vec![0u8; 4096];
        let mut tmp_err = vec![0u8; 4096];
        loop {
            tokio::select! {
                n = async {
                    match stdout.as_mut() {
                        Some(s) => s.read(&mut tmp_out).await,
                        None => std::future::pending::<std::io::Result<usize>>().await,
                    }
                } => {
                    match n {
                        Ok(0) => break,
                        Ok(n) => stdout_buf.extend_from_slice(&tmp_out[..n]),
                        Err(_) => break,
                    }
                }
                n = async {
                    match stderr.as_mut() {
                        Some(s) => s.read(&mut tmp_err).await,
                        None => std::future::pending::<std::io::Result<usize>>().await,
                    }
                } => {
                    match n {
                        Ok(0) => break,
                        Err(_) => break,
                        Ok(n) => stderr_buf.extend_from_slice(&tmp_err[..n]),
                    }
                }
                _ = child.wait() => break,
            }
        }
    })
    .await;

    let timed_out = read_looped.is_err();
    if timed_out {
        // Best-effort kill.
        let _ = child.start_kill();
    }

    let exit_code = match child.try_wait() {
        Ok(Some(status)) => status.code().unwrap_or(-1),
        Ok(None) => {
            // Still running after timeout — wait a bit more then kill.
            let _ = child.kill().await;
            -1
        }
        Err(_) => -1,
    };

    Ok(HttpResponse::Ok().json(ExecResult {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
        pid: pid.unwrap_or(0),
        timed_out,
    }))
}

/// GET /api/v1/containers/{id}/exec/ws
///
/// Upgrades to a WebSocket and runs an interactive exec session. The request
/// body is supplied as a JSON query parameter because WebSocket upgrade
/// requests don't carry a body.
#[get("/{id}/exec/ws")]
async fn exec_ws(
    state: web::Data<AppState>,
    path: web::Path<String>,
    req: HttpRequest,
    stream: web::Payload,
    query: web::Query<ExecWsQuery>,
) -> Result<HttpResponse> {
    let key = path.into_inner();
    let c = resolve_container(&state, &key)?;
    let req_payload = ExecRequest {
        cmd: query
            .cmd
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        cwd: query.cwd.clone(),
        env: None,
        tty: query.tty.unwrap_or(false),
        user: None,
        rows: query.rows.unwrap_or(24),
        cols: query.cols.unwrap_or(80),
    };
    let session = spawn_session_for(state.get_ref(), c.id, req_payload).await?;
    let actor = ExecWs::new(c.id, ExecRequest {
        cmd: vec![],
        cwd: None,
        env: None,
        tty: false,
        user: None,
        rows: 24,
        cols: 80,
    })
    .with_session(session);
    actix_ws::start(actor, &req, stream)
        .map_err(|e| DroidkerError::Internal(format!("ws upgrade failed: {e}")))
}

#[derive(Debug, serde::Deserialize)]
pub struct ExecWsQuery {
    /// Command to run, URL-encoded multiple times: `?cmd=/bin/sh&cmd=-c&cmd=ls`
    #[serde(default)]
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub tty: Option<bool>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub pid: u32,
    pub timed_out: bool,
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
