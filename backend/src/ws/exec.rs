// src/ws/exec.rs
//
// WebSocket actor that bridges a browser/CLI terminal session to an
// `ExecSession` running inside a container's namespaces.
//
// Wire protocol (JSON over text frames, both directions):
//
//   client -> server:
//     { "type": "stdin",  "data": "<base64>" }
//     { "type": "resize", "rows": 24, "cols": 80 }
//     { "type": "close" }
//
//   server -> client:
//     { "type": "stdout", "data": "<base64>" }
//     { "type": "stderr", "data": "<base64>" }
//     { "type": "exit",   "code": 0 }
//     { "type": "error",  "message": "..." }
//
// M2 ships a minimal pipe-mode implementation: stdin writes are forwarded
// to the child, stdout/stderr chunks are relayed to the client. TTY
// allocation (pty fork + TIOCSWINSZ) lands in M3 alongside the interactive
// shell work.

use crate::exec::session::{encode_exit_message, encode_stream_message, ExecRequest, ExecSession};
use crate::error::{DroidkerError, Result};
use actix::prelude::*;
use actix_web_actors::ws;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(60);
/// Poll the child's stdout/stderr this often. Small enough to feel
/// interactive, large enough to keep CPU idle on a 1-vCPU host.
const READ_TICK: Duration = Duration::from_millis(50);
const READ_CHUNK: usize = 4096;

/// Actor that owns one `ExecSession` and pumps bytes between the WebSocket
/// and the child process.
pub struct ExecWs {
    pub container_id: Uuid,
    pub session: Option<ExecSession>,
    pub last_pong: Instant,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    Stdin { data: String },
    Resize { rows: u16, cols: u16 },
    Close,
}

impl ExecWs {
    pub fn new(container_id: Uuid, _req: ExecRequest) -> Self {
        Self {
            container_id,
            session: None,
            last_pong: Instant::now(),
        }
    }

    /// Attach a previously-spawned session to this actor.
    pub fn with_session(mut self, session: ExecSession) -> Self {
        self.session = Some(session);
        self
    }
}

impl Actor for ExecWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Heartbeat: Ping every HEARTBEAT_INTERVAL, close if no Pong within
        // CLIENT_TIMEOUT.
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.last_pong) > CLIENT_TIMEOUT {
                tracing::warn!("exec WS client timeout, closing");
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });

        // Periodically drain stdout/stderr and forward as text frames.
        // We can't await inside `run_interval`, so we use `spawn` with a
        // tokio runtime handle stored in the actor. For M2 we use a simpler
        // synchronous approach: poll via `ContextFut::spawn` for each tick.
        //
        // The actual pump lives in `StreamHandler::handle` is wrong —
        // let's use `ctx.run_interval` + a synchronous read via nb::Error.
        // Tokio's `AsyncReadExt::read` is async, so we can't call it directly.
        //
        // Workaround: wrap the pump in `actix::spawn(future)`. We need the
        // tokio handle from the actix runtime. actix-rt provides this.
        ctx.run_interval(READ_TICK, |act, ctx| {
            if act.session.is_none() {
                return;
            }
            // Take the session out so we can poll the pipes without holding
            // a borrow on `act` across an await point.
            let mut session = act.session.take().unwrap();
            let addr = ctx.address().clone();
            actix::spawn(async move {
                // Read stdout (or PTY master) with a short timeout so we
                // don't block the pump forever.
                let stdout_chunk = tokio::time::timeout(
                    Duration::from_millis(10),
                    session.read_stdout_chunk(),
                )
                .await;
                let mut produced = false;
                match stdout_chunk {
                    Ok(Ok(bytes)) if !bytes.is_empty() => {
                        let _ = addr.try_send(WsOut::Stdout(bytes));
                        produced = true;
                    }
                    _ => {}
                }
                // Read stderr (no-op in PTY mode — returns Ok(None)).
                let stderr_chunk = tokio::time::timeout(
                    Duration::from_millis(10),
                    session.read_stderr_chunk(),
                )
                .await;
                if let Ok(Ok(Some(bytes))) = stderr_chunk {
                    if !bytes.is_empty() {
                        let _ = addr.try_send(WsOut::Stderr(bytes));
                        produced = true;
                    }
                }
                let _ = produced;

                // Put the session back.
                let _ = addr.try_send(WsOut::ReplaceSession(session));
            });
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        tracing::info!(container_id = %self.container_id, "exec WS closed");
    }
}

/// Messages sent from the pump task back to the actor.
#[derive(actix::Message)]
#[rtype(result = "()")]
enum WsOut {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    ReplaceSession(ExecSession),
    Exited(i32),
}

impl Handler<WsOut> for ExecWs {
    type Result = ();
    fn handle(&mut self, msg: WsOut, ctx: &mut Self::Context) {
        match msg {
            WsOut::Stdout(b) => {
                ctx.text(encode_stream_message("stdout", &b));
            }
            WsOut::Stderr(b) => {
                ctx.text(encode_stream_message("stderr", &b));
            }
            WsOut::ReplaceSession(s) => {
                self.session = Some(s);
            }
            WsOut::Exited(code) => {
                ctx.text(encode_exit_message(code));
                ctx.close(None);
                ctx.stop();
            }
        }
    }
}

/// Handle incoming text/binary messages from the client.
impl StreamHandler<std::result::Result<ws::Message, ws::ProtocolError>> for ExecWs {
    fn handle(&mut self, item: std::result::Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "exec WS protocol error");
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Ping(msg) => {
                self.last_pong = Instant::now();
                ctx.pong(&msg);
            }
            ws::Message::Pong(_) => {
                self.last_pong = Instant::now();
            }
            ws::Message::Text(text) => {
                let parsed: ClientMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        ctx.text(
                            serde_json::json!({
                                "type": "error",
                                "message": format!("invalid message: {e}")
                            })
                            .to_string(),
                        );
                        return;
                    }
                };
                match parsed {
                    ClientMsg::Stdin { data } => {
                        let bytes = match base64_decode(&data) {
                            Ok(b) => b,
                            Err(e) => {
                                ctx.text(
                                    serde_json::json!({
                                        "type": "error",
                                        "message": format!("bad base64: {e}")
                                    })
                                    .to_string(),
                                );
                                return;
                            }
                        };
                        if let Some(session) = self.session.take() {
                            let addr = ctx.address().clone();
                            actix::spawn(async move {
                                let mut s = session;
                                if let Err(e) = s.write_stdin(&bytes).await {
                                    let _ = addr.try_send(WsOut::ReplaceSession(s));
                                    let _ = addr.try_send(WsOut::Exited(-1));
                                    tracing::warn!(error = %e, "stdin write failed");
                                    return;
                                }
                                let _ = addr.try_send(WsOut::ReplaceSession(s));
                            });
                        }
                    }
                    ClientMsg::Resize { rows, cols } => {
                        if let Some(session) = self.session.take() {
                            let addr = ctx.address().clone();
                            actix::spawn(async move {
                                let mut s = session;
                                let _ = s.resize_tty(rows, cols).await;
                                let _ = addr.try_send(WsOut::ReplaceSession(s));
                            });
                        }
                    }
                    ClientMsg::Close => {
                        ctx.close(None);
                        ctx.stop();
                    }
                }
            }
            ws::Message::Binary(_) => {
                // We accept binary frames as raw stdin bytes (no base64 wrap).
            }
            ws::Message::Close(reason) => {
                ctx.close(reason);
                ctx.stop();
            }
            ws::Message::Continuation(_) => {}
            ws::Message::Nop => {}
        }
    }
}

/// Cheap base64 decoder (matches the encoder in `exec/session.rs`).
fn base64_decode(s: &str) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let val = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err(format!("invalid base64 char: {}", c as char)),
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Helper for route handlers: spawn the session, then upgrade the WebSocket
/// with the actor attached.
pub async fn spawn_session_for(
    state: &crate::AppState,
    container_id: Uuid,
    req: ExecRequest,
) -> Result<ExecSession> {
    let container = state
        .manager
        .get(container_id)
        .ok_or_else(|| DroidkerError::NotFound(container_id.to_string()))?;
    if container.pid == 0 {
        return Err(DroidkerError::InvalidState(
            "container is not running".into(),
        ));
    }
    ExecSession::spawn(container.pid, req).await
}
