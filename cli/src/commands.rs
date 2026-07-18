// src/commands.rs
//
// Implementations of every CLI subcommand. Each function returns anyhow::Result
// so the dispatcher in main.rs can short-circuit on error.

use crate::client::DroidkerClient;
use crate::fmt;
use anyhow::{anyhow, Result};
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::path::Path;

pub async fn info(client: &DroidkerClient, json: bool) -> Result<()> {
    let health = client.health().await?;
    let ready = client.ready().await?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"health": health, "ready": ready}))?
        );
        return Ok(());
    }
    println!("{}", "DroidKer daemon".cyan().bold());
    println!("  health:  {}", "ok".green());
    println!("  ready:   {}", ready["ready"]);
    println!(
        "  containers loaded: {}",
        ready["containers_loaded"]
    );
    Ok(())
}

pub async fn ps(client: &DroidkerClient, json: bool) -> Result<()> {
    let containers = client.list_containers().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&containers)?);
        return Ok(());
    }
    fmt::print_container_table(&containers);
    Ok(())
}

pub async fn upload(client: &DroidkerClient, path: &Path, json: bool) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("APK file not found: {}", path.display()));
    }
    let result = client.upload_apk(path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "{} uploaded {} ({} bytes, sha256={})",
        "✓".green(),
        result["original_name"].as_str().unwrap_or("-"),
        result["size"].as_u64().unwrap_or(0),
        result["sha256"].as_str().unwrap_or("-"),
    );
    println!(
        "  stored as: {}",
        result["filename"].as_str().unwrap_or("-")
    );
    Ok(())
}

pub async fn run(
    client: &DroidkerClient,
    apk: &Path,
    name: Option<String>,
    memory: Option<u32>,
    cpu: Option<u32>,
    notes: Option<String>,
    ports: &[String],
    json: bool,
) -> Result<()> {
    // Step 1: upload the APK (the daemon dedups by SHA-256).
    println!("{} Uploading APK...", "•".cyan());
    let upload = client.upload_apk(apk).await?;
    let stored = upload["filename"].as_str().ok_or_else(|| anyhow!("missing filename in upload response"))?.to_string();

    // Step 2: create the container.
    println!("{} Creating container...", "•".cyan());
    let port_mappings = parse_ports(ports)?;
    let body = json!({
        "name": name,
        "apk": stored,
        "memory_mb": memory,
        "cpu_percent": cpu,
        "notes": notes,
        "ports": port_mappings,
    });
    let container = client.create_container(&body).await?;
    let id = container["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing id in create response"))?
        .to_string();

    // Step 3: start it.
    println!("{} Starting container...", "•".cyan());
    let started = client.start_container(&id).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&started)?);
    } else {
        fmt::print_container_detail(&started);
        println!(
            "\n{} Container {} is running.",
            "✓".green(),
            started["name"].as_str().unwrap_or(&id)
        );
    }
    Ok(())
}

pub async fn create(
    client: &DroidkerClient,
    apk: &str,
    name: Option<String>,
    memory: Option<u32>,
    cpu: Option<u32>,
    notes: Option<String>,
    ports: &[String],
    json: bool,
) -> Result<()> {
    let port_mappings = parse_ports(ports)?;
    let body = json!({
        "name": name,
        "apk": apk,
        "memory_mb": memory,
        "cpu_percent": cpu,
        "notes": notes,
        "ports": port_mappings,
    });
    let c = client.create_container(&body).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c)?);
    } else {
        fmt::print_container_detail(&c);
    }
    Ok(())
}

/// Parse `["8080:80", "9000:9000"]` into a JSON array of {host, container} objects.
/// Returns an error if any entry doesn't match `host:container`.
fn parse_ports(ports: &[String]) -> Result<serde_json::Value> {
    let mut arr = Vec::with_capacity(ports.len());
    for p in ports {
        let m = regex_lite(p)?;
        let host: u16 = m.0.parse().map_err(|_| anyhow!("bad host port: {}", m.0))?;
        let container: u16 = m.1.parse().map_err(|_| anyhow!("bad container port: {}", m.1))?;
        arr.push(json!({"host": host, "container": container}));
    }
    Ok(serde_json::Value::Array(arr))
}

/// Tiny regex-free parser for `host:container` strings.
fn regex_lite(s: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(anyhow!("bad port mapping: \"{}\" (expected host:container)", s));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

pub async fn start(client: &DroidkerClient, id_or_name: &str, json: bool) -> Result<()> {
    let c = client.start_container(id_or_name).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c)?);
    } else {
        fmt::print_container_detail(&c);
    }
    Ok(())
}

pub async fn stop(client: &DroidkerClient, id_or_name: &str, json: bool) -> Result<()> {
    let c = client.stop_container(id_or_name).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c)?);
    } else {
        fmt::print_container_detail(&c);
    }
    Ok(())
}

pub async fn restart(client: &DroidkerClient, id_or_name: &str, json: bool) -> Result<()> {
    // Stop is best-effort; the container may already be stopped.
    let _ = client.stop_container(id_or_name).await;
    let c = client.start_container(id_or_name).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c)?);
    } else {
        fmt::print_container_detail(&c);
    }
    Ok(())
}

pub async fn rm(client: &DroidkerClient, id_or_name: &str, json: bool) -> Result<()> {
    client.delete_container(id_or_name).await?;
    if !json {
        println!("{} removed container {}", "✓".green(), id_or_name);
    }
    Ok(())
}

pub async fn inspect(client: &DroidkerClient, id_or_name: &str, json: bool) -> Result<()> {
    let c = client.get_container(id_or_name).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c)?);
    } else {
        fmt::print_container_detail(&c);
    }
    Ok(())
}

pub async fn logs(client: &DroidkerClient, id_or_name: &str, kind: &str) -> Result<()> {
    let text = client.get_logs(id_or_name, kind).await?;
    if text.is_empty() {
        println!(
            "{} no {} logs for container {} yet",
            "•".yellow(),
            kind,
            id_or_name
        );
    } else {
        print!("{}", text);
        if !text.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}

/// Pretty-print a stats snapshot.
pub async fn stats(client: &DroidkerClient, id_or_name: &str, json: bool) -> Result<()> {
    let s = client.get_stats(id_or_name).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(());
    }
    fmt::print_stats(&s);
    Ok(())
}

/// One-shot exec. Joins stdout/stderr and prints to the terminal.
pub async fn exec(
    client: &DroidkerClient,
    id_or_name: &str,
    cmd: &[String],
    cwd: Option<&str>,
    json: bool,
) -> Result<()> {
    let result = client.exec(id_or_name, cmd, cwd, false).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let stdout = result["stdout"].as_str().unwrap_or("");
    let stderr = result["stderr"].as_str().unwrap_or("");
    let code = result["exit_code"].as_i64().unwrap_or(-1);
    let timed_out = result["timed_out"].as_bool().unwrap_or(false);

    if !stdout.is_empty() {
        print!("{}", stdout);
    }
    if !stderr.is_empty() {
        eprint!("{}", stderr);
    }
    if timed_out {
        eprintln!(
            "{} command timed out after 30s",
            "!".yellow().bold()
        );
    }
    if code != 0 {
        eprintln!("{} exit code: {}", "!".red().bold(), code);
    }
    Ok(())
}

/// Save one screen frame to a JPEG file. We open a WebSocket to the
/// screen stream, wait for the first binary frame, strip the 8-byte
/// width/height header, and write the JPEG bytes to disk.
pub async fn screenshot(
    client: &DroidkerClient,
    id_or_name: &str,
    out: Option<&std::path::Path>,
) -> Result<()> {
    // Resolve the container ID first.
    let c = client.get_container(id_or_name).await?;
    let id = c["id"].as_str().ok_or_else(|| anyhow!("missing id in container response"))?;

    // Build the WS URL. The host comes from the client's base URL.
    let ws_url = client.ws_url(&format!("/api/v1/containers/{}/screen/ws", id))?;
    println!("{} connecting to {}", "•".cyan(), ws_url);

    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| anyhow!("WS connect failed: {e}"))?;

    // Send a low-fps control message so we don't get a flood of frames.
    use tokio_tungstenite::tungstenite::Message;
    ws.send(Message::Text(r#"{"type":"set_fps","fps":1}"#.into()))
        .await
        .ok();

    // Wait for the first binary frame.
    let mut frame: Option<Vec<u8>> = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), ws.next()).await;
        match msg {
            Ok(Some(Ok(Message::Binary(buf)))) => {
                if buf.len() > 8 {
                    frame = Some(buf.to_vec());
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => return Err(anyhow!("WS error: {e}")),
            Ok(None) => return Err(anyhow!("WS closed before first frame")),
            Err(_) => continue,
        }
    }
    let frame = frame.ok_or_else(|| anyhow!("timed out waiting for screen frame"))?;

    // Strip the 8-byte width/height header.
    let jpeg = &frame[8..];
    let width = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
    let height = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
    let out_path = match out {
        Some(p) => p.to_path_buf(),
        None => std::path::PathBuf::from(format!("{}-screenshot.jpg", &id[..8])),
    };
    std::fs::write(&out_path, jpeg)?;
    println!(
        "{} saved {}x{} screenshot to {} ({} bytes)",
        "✓".green(),
        width,
        height,
        out_path.display(),
        jpeg.len()
    );
    Ok(())
}

/// Inject a tap (down + up) at (x, y).
pub async fn tap(client: &DroidkerClient, id_or_name: &str, x: i32, y: i32) -> Result<()> {
    client.send_touch(id_or_name, x, y, "down", 128, 0).await?;
    // Brief pause to let the input reader pick up the down event.
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    client.send_touch(id_or_name, x, y, "up", 0, 0).await?;
    println!("{} tapped ({}, {})", "✓".green(), x, y);
    Ok(())
}

/// Inject a swipe from (x1,y1) to (x2,y2) over `duration_ms`.
/// We interpolate ~16 steps along the path with small sleeps between.
pub async fn swipe(
    client: &DroidkerClient,
    id_or_name: &str,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    duration_ms: u32,
) -> Result<()> {
    let steps = 16u32;
    let step_ms = duration_ms / steps;
    client.send_touch(id_or_name, x1, y1, "down", 128, 0).await?;
    for i in 1..steps {
        let t = i as f32 / steps as f32;
        let x = (x1 as f32 + (x2 as f32 - x1 as f32) * t).round() as i32;
        let y = (y1 as f32 + (y2 as f32 - y1 as f32) * t).round() as i32;
        client.send_touch(id_or_name, x, y, "move", 128, 0).await?;
        tokio::time::sleep(std::time::Duration::from_millis(step_ms as u64)).await;
    }
    client.send_touch(id_or_name, x2, y2, "up", 0, 0).await?;
    println!(
        "{} swiped ({}, {}) -> ({}, {}) in {}ms",
        "✓".green(),
        x1,
        y1,
        x2,
        y2,
        duration_ms
    );
    Ok(())
}

/// Inject a Home / Back / Recent key tap.
pub async fn key(client: &DroidkerClient, id_or_name: &str, key: &str) -> Result<()> {
    let normalized = match key.to_lowercase().as_str() {
        "home" | "homepage" => "home",
        "back" => "back",
        "recent" | "recents" | "appselect" => "recent",
        other => return Err(anyhow!("unknown key: {} (use home|back|recent)", other)),
    };
    client.send_key(id_or_name, normalized, true).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    client.send_key(id_or_name, normalized, false).await?;
    println!("{} sent key {}", "✓".green(), normalized);
    Ok(())
}
