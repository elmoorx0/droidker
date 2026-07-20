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
use std::io::Write;
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
    // M6: show host arch + translation capability.
    if let Some(host_arch) = ready["host_arch"].as_str() {
        println!("  host arch: {}", host_arch);
    }
    if let Some(t) = ready["translation"].as_object() {
        if !t.is_empty() {
            println!("  translation:");
            for (abi, info) in t {
                let strat = info["strategy"].as_str().unwrap_or("?");
                let usable = info["usable"].as_bool().unwrap_or(false);
                let marker = if usable { "✓".green() } else { "✗".red() };
                println!("    {} {}: {}", marker, abi, strat);
            }
        }
    }
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

/// `droidker inspect-apk <filename>` (M7.1).
///
/// Calls `POST /api/v1/apk/inspect` and pretty-prints the result: which
/// `lib/<abi>/*.so` directories the APK ships, how many `.so` files per
/// ABI, total uncompressed bytes, and the recommended target arch.
pub async fn inspect_apk(client: &DroidkerClient, apk: &str, json: bool) -> Result<()> {
    let result = client.inspect_apk(apk).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!("{}", "APK native-ABI manifest".cyan().bold());
    println!("  file:           {}", result["path"].as_str().unwrap_or("-"));
    println!("  zip entries:    {}", result["zip_entry_count"].as_u64().unwrap_or(0));
    let has_no_native = result["has_no_native_libs"].as_bool().unwrap_or(false);
    if has_no_native {
        println!(
            "  native libs:    {} (pure-Java/Kotlin app; host arch is fine)",
            "none".yellow()
        );
    } else {
        println!("  native libs:    {} ABI(s) shipped", "found".green());
    }
    if let Some(abis) = result["abis"].as_array() {
        for abi in abis {
            let name = abi["abi"].as_str().unwrap_or("-");
            let count = abi["so_count"].as_u64().unwrap_or(0);
            let bytes = abi["total_uncompressed_bytes"].as_u64().unwrap_or(0);
            println!(
                "    {name:<16} {count:>3} .so   {bytes:>10} bytes",
            );
        }
    }
    if let Some(rec) = result["recommended_arch"].as_str() {
        println!("  recommended:    {} ({})", rec.green(), "use --arch <ARCH>");
    } else {
        println!(
            "  recommended:    {}",
            "(none — use host arch)".yellow()
        );
    }
    Ok(())
}

/// M8.1: `droidker verify-apk <filename>` — checks an APK's signature.
pub async fn verify_apk(client: &DroidkerClient, apk: &str, json: bool) -> Result<()> {
    let result = client.verify_apk(apk).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!("{}", "APK signature info".cyan().bold());
    println!("  file:           {}", result["path"].as_str().unwrap_or("-"));
    let signed = result["signed"].as_bool().unwrap_or(false);
    let scheme = result["scheme"].as_str().unwrap_or("none");
    if signed {
        println!("  signed:         {} ({})", "yes".green(), scheme);
    } else {
        println!("  signed:         {}", "NO".red().bold());
    }
    if let Some(fp) = result["cert_sha256"].as_str() {
        println!("  cert SHA-256:   {}", fp.cyan());
    }
    if let Some(subject) = result["cert_subject"].as_str() {
        println!("  cert subject:   {}", subject);
    }
    if !signed {
        println!();
        println!(
            "{}",
            "WARNING: this APK is unsigned. Running untrusted APKs is a security risk —"
                .yellow()
        );
        println!(
            "{}",
            "any code in the APK can execute on your VPS with the container's privileges."
                .yellow()
        );
    }
    Ok(())
}

/// M8.2: `droidker inspect-bundle <filename>` — lists an .xapk/.apks bundle's splits.
pub async fn inspect_bundle(
    client: &DroidkerClient,
    bundle: &str,
    arch: Option<&str>,
    json: bool,
) -> Result<()> {
    let result = client.inspect_bundle(bundle, arch).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    let format = result["format"].as_str().unwrap_or("?");
    println!("{}", "Split-APK bundle manifest".cyan().bold());
    println!("  file:           {}", result["path"].as_str().unwrap_or("-"));
    println!("  format:         {}", format);
    if let Some(pkg) = result["package"].as_str() {
        println!("  package:        {}", pkg);
    }
    if let Some(ver) = result["version_name"].as_str() {
        println!("  version:        {}", ver);
    }
    println!(
        "  zip entries:    {}",
        result["zip_entry_count"].as_u64().unwrap_or(0)
    );

    if let Some(abis) = result["available_abis"].as_array() {
        if !abis.is_empty() {
            let abi_strs: Vec<String> = abis
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            println!("  available ABIs: {}", abi_strs.join(", "));
        }
    }

    if let Some(entries) = result["entries"].as_array() {
        println!();
        println!("  entries:");
        for entry in entries {
            let zip_path = entry["zip_path"].as_str().unwrap_or("-");
            let kind = entry["kind"].as_str().unwrap_or("other");
            let size = entry["uncompressed_size"].as_u64().unwrap_or(0);
            let mut detail = String::new();
            if let Some(abi) = entry["abi"].as_str() {
                detail.push_str(&format!("abi={}", abi));
            }
            if let Some(loc) = entry["locale"].as_str() {
                if !detail.is_empty() {
                    detail.push_str(", ");
                }
                detail.push_str(&format!("locale={}", loc));
            }
            if let Some(d) = entry["density"].as_str() {
                if !detail.is_empty() {
                    detail.push_str(", ");
                }
                detail.push_str(&format!("density={}", d));
            }
            if detail.is_empty() {
                detail.push('-');
            }
            println!("    {:<40} {:<10} {:>10} bytes  ({})", zip_path, kind, size, detail);
        }
    }

    if let Some(rec) = result["recommended_install"].as_array() {
        if !rec.is_empty() {
            println!();
            println!("  recommended install:");
            for r in rec {
                if let Some(s) = r.as_str() {
                    println!("    • {}", s.green());
                }
            }
        }
    }
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
    arch: Option<String>,
    translation_strategy: Option<String>,
    json: bool,
) -> Result<()> {
    // Step 1: upload the APK (the daemon dedups by SHA-256).
    println!("{} Uploading APK...", "•".cyan());
    let upload = client.upload_apk(apk).await?;
    let stored = upload["filename"].as_str().ok_or_else(|| anyhow!("missing filename in upload response"))?.to_string();

    // Step 2: resolve `--arch auto` by inspecting the uploaded APK's
    // native-ABI manifest (M7.1). For an explicit `--arch <TOKEN>` we
    // pass it through unchanged; for `None` we let the daemon default
    // to the host arch.
    let resolved_arch = if let Some(a) = arch.as_deref() {
        if a.eq_ignore_ascii_case("auto") {
            println!("{} Inspecting APK native ABIs...", "•".cyan());
            let inspect = client.inspect_apk(&stored).await?;
            let recommended = inspect["recommended_arch"]
                .as_str()
                .map(|s| s.to_string());
            match recommended {
                Some(picked) => {
                    let abis = inspect["abis"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x["abi"].as_str().map(|s| s.to_string()))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    println!(
                        "{} APK ships {} ABI(s): {}; picked {}",
                        "•".cyan(),
                        abis.len(),
                        abis.join(", "),
                        picked.green()
                    );
                    Some(picked)
                }
                None => {
                    println!(
                        "{} APK has no native libs (pure-Java); using host arch",
                        "•".cyan()
                    );
                    None
                }
            }
        } else {
            Some(a.to_string())
        }
    } else {
        None
    };

    // Step 3: create the container.
    println!("{} Creating container...", "•".cyan());
    let port_mappings = parse_ports(ports)?;
    let body = json!({
        "name": name,
        "apk": stored,
        "memory_mb": memory,
        "cpu_percent": cpu,
        "notes": notes,
        "ports": port_mappings,
        "arch": resolved_arch,
        "translation_strategy": translation_strategy,
    });
    let container = client.create_container(&body).await?;
    let id = container["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing id in create response"))?
        .to_string();

    // Step 4: start it.
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
    arch: Option<String>,
    translation_strategy: Option<String>,
    json: bool,
) -> Result<()> {
    // `--arch auto` is not supported on `create` because the APK must
    // already have been uploaded — but the user can still call
    // `droidker inspect-apk <filename>` first and pass the result here.
    if let Some(a) = arch.as_deref() {
        if a.eq_ignore_ascii_case("auto") {
            return Err(anyhow!(
                "--arch auto is not supported on `create` (the APK must already \
                 be uploaded). Use `droidker run --arch auto` instead, or run \
                 `droidker inspect-apk <filename>` to pre-resolve the arch."
            ));
        }
    }
    let port_mappings = parse_ports(ports)?;
    let body = json!({
        "name": name,
        "apk": apk,
        "memory_mb": memory,
        "cpu_percent": cpu,
        "notes": notes,
        "ports": port_mappings,
        "arch": arch,
        "translation_strategy": translation_strategy,
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

/// Humanized tap (M5): Bezier-jittered down+up with Gaussian pressure.
pub async fn htap(client: &DroidkerClient, id_or_name: &str, x: i32, y: i32) -> Result<()> {
    let resp = client.human_tap(id_or_name, x, y).await?;
    let dur = resp.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    println!(
        "{} humanized tap ({}, {}) — {}ms total",
        "✓".green(),
        x,
        y,
        dur
    );
    Ok(())
}

/// Humanized swipe (M5): curved Bezier path with Gaussian-jittered delays.
pub async fn hswipe(
    client: &DroidkerClient,
    id_or_name: &str,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
) -> Result<()> {
    let resp = client.human_swipe(id_or_name, x1, y1, x2, y2).await?;
    let dur = resp.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    println!(
        "{} humanized swipe ({}, {}) -> ({}, {}) — {}ms total",
        "✓".green(),
        x1,
        y1,
        x2,
        y2,
        dur
    );
    Ok(())
}

/// Humanized long-press (M5): holds with small position drift.
pub async fn hlongpress(
    client: &DroidkerClient,
    id_or_name: &str,
    x: i32,
    y: i32,
    hold_ms: u32,
) -> Result<()> {
    let resp = client.human_long_press(id_or_name, x, y, hold_ms).await?;
    let dur = resp.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    println!(
        "{} humanized long-press ({}, {}) hold={}ms — {}ms total",
        "✓".green(),
        x,
        y,
        hold_ms,
        dur
    );
    Ok(())
}

/// Humanized pinch-zoom gesture (M8.4).
///
/// Sends a two-finger pinch from `start_distance` to `end_distance`
/// at the given center point. When `end_distance > start_distance`,
/// it's a zoom-in; otherwise zoom-out.
pub async fn hpinch(
    client: &DroidkerClient,
    id_or_name: &str,
    center_x: i32,
    center_y: i32,
    start_distance: f64,
    end_distance: f64,
    angle_deg: f64,
) -> Result<()> {
    let resp = client
        .human_pinch(
            id_or_name,
            center_x,
            center_y,
            start_distance,
            end_distance,
            angle_deg,
        )
        .await?;
    let dur = resp.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let direction = if end_distance > start_distance {
        "zoom-in"
    } else {
        "zoom-out"
    };
    println!(
        "{} humanized pinch {} ({}, {}) {} -> {} px @ {:.0}° — {}ms total",
        "✓".green(),
        direction,
        center_x,
        center_y,
        start_distance,
        end_distance,
        angle_deg,
        dur
    );
    Ok(())
}

/// Record a container's screen stream to an MJPEG file (M5).
///
/// The output file format is a simple concatenation:
///   - 4-byte magic: b"MJP1" (MJPEG v1)
///   - 4-byte LE: frame count
///   - For each frame:
///       4-byte LE: width
///       4-byte LE: height
///       4-byte LE: jpeg byte count
///       4-byte LE: timestamp_ms since recording start
///       N bytes: JPEG data
///
/// This is parseable by any tool that can read sequential binary; we
/// picked a custom format over standard .avi/.mp4 to avoid pulling in
/// ffmpeg as a native dependency. A 30-second recording at 5 FPS, q=70,
/// 540x960 is ~3 MB.
pub async fn record(
    client: &DroidkerClient,
    id_or_name: &str,
    out: Option<&std::path::Path>,
    duration_sec: u64,
    fps: u32,
    quality: u8,
) -> Result<()> {
    let c = client.get_container(id_or_name).await?;
    let id = c["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing id in container response"))?;

    let fps = fps.clamp(1, 30);
    let quality = quality.clamp(10, 95);

    let out_path = match out {
        Some(p) => p.to_path_buf(),
        None => std::path::PathBuf::from(format!("{}-record.mjpeg", &id[..8])),
    };

    println!(
        "{} recording {} for {}s at {}fps q={} -> {}",
        "•".cyan(),
        id,
        duration_sec,
        fps,
        quality,
        out_path.display()
    );

    let ws_url = client.ws_url(&format!("/api/v1/containers/{}/screen/ws", id))?;
    let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| anyhow!("WS connect failed: {e}"))?;

    use tokio_tungstenite::tungstenite::Message;

    // Tell the server to send us frames at the requested fps + quality.
    let set_fps = format!(r#"{{"type":"set_fps","fps":{}}}"#, fps);
    let set_quality = format!(r#"{{"type":"set_quality","quality":{}}}"#, quality);
    ws.send(Message::Text(set_fps.into())).await.ok();
    ws.send(Message::Text(set_quality.into())).await.ok();

    // Open the output file and write the header placeholder. We'll come
    // back and patch the frame count when we're done.
    let mut file = std::fs::File::create(&out_path)?;
    file.write_all(b"MJP1")?;
    // Placeholder frame count — we'll overwrite at the end.
    file.write_all(&0u32.to_le_bytes())?;

    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(duration_sec);
    let mut frame_count: u32 = 0;
    let mut total_bytes: u64 = 0;

    while std::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(
            std::time::Duration::from_millis(2000),
            ws.next(),
        )
        .await;
        match msg {
            Ok(Some(Ok(Message::Binary(buf)))) if buf.len() > 8 => {
                let width = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let height = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                let jpeg = &buf[8..];
                let ts_ms = start.elapsed().as_millis() as u32;

                // Per-frame header.
                file.write_all(&width.to_le_bytes())?;
                file.write_all(&height.to_le_bytes())?;
                file.write_all(&(jpeg.len() as u32).to_le_bytes())?;
                file.write_all(&ts_ms.to_le_bytes())?;
                file.write_all(jpeg)?;

                frame_count += 1;
                total_bytes += jpeg.len() as u64;

                if frame_count % 5 == 0 {
                    let elapsed = start.elapsed().as_secs();
                    println!(
                        "{}  captured {} frames ({}s / {}s, {} KB)",
                        "•".cyan(),
                        frame_count,
                        elapsed,
                        duration_sec,
                        total_bytes / 1024
                    );
                }
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                return Err(anyhow!("WS error after {} frames: {}", frame_count, e))
            }
            Ok(None) => {
                println!("{} WS closed by server after {} frames", "!".yellow(), frame_count);
                break;
            }
            Err(_) => {
                // Timeout — keep going until the deadline.
                continue;
            }
        }
    }

    // Patch the frame count in the header.
    use std::io::{Seek, Write};
    file.seek(std::io::SeekFrom::Start(4))?;
    file.write_all(&frame_count.to_le_bytes())?;
    file.sync_all()?;

    let elapsed = start.elapsed().as_secs();
    let out_size = std::fs::metadata(&out_path)?.len();
    println!(
        "{} recorded {} frames in {}s ({} KB, avg {} KB/frame) -> {}",
        "✓".green(),
        frame_count,
        elapsed,
        out_size / 1024,
        if frame_count > 0 { total_bytes / frame_count as u64 / 1024 } else { 0 },
        out_path.display()
    );
    Ok(())
}

/// `droidker run-bundle` — extract + create + start a split-APK bundle
/// container in one shot (M9.1).
///
/// Flow:
///   1. Upload the `.xapk` / `.apks` bundle (dedup'd by SHA-256).
///   2. Inspect the bundle to enumerate inner APKs + their kinds.
///   3. Resolve the target arch (auto via the bundle's ABI splits, or
///      the user's `--arch`).
///   4. Extract the recommended install set (base + matching ABI split,
///      plus any extras the user listed via `--split`).
///   5. Create a container with the base APK as `apk` and the splits as
///      `extra_apks`.
///   6. Start the container.
pub async fn run_bundle(
    client: &DroidkerClient,
    bundle: &Path,
    name: Option<String>,
    memory: Option<u32>,
    cpu: Option<u32>,
    notes: Option<String>,
    ports: &[String],
    arch: Option<String>,
    translation_strategy: Option<String>,
    extra_splits: &[String],
    json: bool,
) -> Result<()> {
    // Step 1: upload the bundle (the daemon dedups by SHA-256).
    println!("{} Uploading bundle...", "•".cyan());
    let upload = client.upload_apk(bundle).await?;
    let stored = upload["filename"]
        .as_str()
        .ok_or_else(|| anyhow!("missing filename in upload response"))?
        .to_string();

    // Step 2: inspect the bundle to enumerate inner APKs + kinds.
    println!("{} Inspecting bundle structure...", "•".cyan());
    let inspect = client.inspect_bundle(&stored, None).await?;
    let entries = inspect["entries"]
        .as_array()
        .ok_or_else(|| anyhow!("bundle inspect response missing 'entries' array"))?;
    let available_abis: Vec<String> = inspect["available_abis"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let package = inspect["package"].as_str().map(|s| s.to_string());

    println!(
        "{} Bundle has {} APK entries (ABIs: {})",
        "•".cyan(),
        entries.len(),
        if available_abis.is_empty() {
            "none".to_string()
        } else {
            available_abis.join(", ")
        }
    );

    // Step 3: resolve the target arch.
    //   --arch auto  → pick the first available ABI from the bundle
    //                  (sorted by KNOWN_BUNDLE_ABIS priority).
    //   --arch <X>   → use X verbatim.
    //   (no flag)    → None (host native; base APK must contain the libs).
    let resolved_arch = if let Some(a) = arch.as_deref() {
        if a.eq_ignore_ascii_case("auto") {
            if let Some(picked) = available_abis.first() {
                // Map `arm64_v8a` → `arm64`, `armeabi_v7a` → `arm`, etc.
                let cli_arch = map_bundle_abi_to_cli(picked);
                println!("{} Auto-picked arch: {}", "•".cyan(), cli_arch.green());
                Some(cli_arch)
            } else {
                println!(
                    "{} Bundle has no ABI splits; using host arch",
                    "•".cyan()
                );
                None
            }
        } else {
            Some(a.to_string())
        }
    } else {
        None
    };

    // Step 4: build the list of zip_paths to extract.
    //   - Always include the base APK.
    //   - Include the ABI split matching `resolved_arch` (when one exists).
    //   - Include any user-supplied `--split <zip_path>` entries.
    let mut zip_paths: Vec<String> = Vec::new();
    let mut base_zip_path: Option<String> = None;
    let mut matching_abi_zip_path: Option<String> = None;

    for entry in entries {
        let zip_path = entry["zip_path"]
            .as_str()
            .ok_or_else(|| anyhow!("bundle entry missing 'zip_path'"))?
            .to_string();
        let kind = entry["kind"].as_str().unwrap_or("other");
        if kind == "base" && base_zip_path.is_none() {
            base_zip_path = Some(zip_path.clone());
        }
        if kind == "abi" {
            if let Some(wanted) = &resolved_arch {
                let entry_abi = entry["abi"].as_str().unwrap_or("");
                let cli_abi = map_bundle_abi_to_cli(entry_abi);
                if &cli_abi == wanted {
                    matching_abi_zip_path = Some(zip_path.clone());
                }
            }
        }
    }

    if let Some(p) = &base_zip_path {
        zip_paths.push(p.clone());
    } else {
        return Err(anyhow!(
            "bundle has no base APK entry (all entries are splits?)"
        ));
    }
    if let Some(p) = &matching_abi_zip_path {
        zip_paths.push(p.clone());
    }
    for s in extra_splits {
        if !zip_paths.contains(s) {
            zip_paths.push(s.clone());
        }
    }

    println!(
        "{} Extracting {} APK(s) from bundle...",
        "•".cyan(),
        zip_paths.len()
    );
    let extract = client.extract_bundle(&stored, &zip_paths).await?;
    let extracted = extract["extracted"]
        .as_array()
        .ok_or_else(|| anyhow!("extract response missing 'extracted' array"))?;

    // The extraction directory name is the bundle's SHA-256 (added by
    // the daemon). The daemon returns the absolute out_dir, but we need
    // paths relative to <data_dir>/apks/ for the create request.
    let out_dir = extract["out_dir"]
        .as_str()
        .ok_or_else(|| anyhow!("extract response missing 'out_dir'"))?;
    let apks_prefix = "/apks/";
    let bundle_sha_dir = if let Some(idx) = out_dir.find(apks_prefix) {
        out_dir[idx + apks_prefix.len()..].to_string()
    } else {
        // Fallback: use the last path segment.
        out_dir
            .split('/')
            .next_back()
            .unwrap_or("")
            .to_string()
    };

    // Find the base APK's filename + collect splits' relative paths.
    let mut base_rel: Option<String> = None;
    let mut extra_apks_rel: Vec<String> = Vec::new();
    for entry in extracted {
        let filename = entry["filename"]
            .as_str()
            .ok_or_else(|| anyhow!("extracted entry missing 'filename'"))?
            .to_string();
        let rel = format!("{bundle_sha_dir}/{filename}");
        let kind = entry["kind"].as_str().unwrap_or("other");
        if kind == "base" {
            base_rel = Some(rel);
        } else {
            extra_apks_rel.push(rel);
        }
    }
    let base_apk_rel = base_rel.ok_or_else(|| anyhow!("no base APK in extracted set"))?;

    println!(
        "{} Base: {} | Splits: {}",
        "•".cyan(),
        base_apk_rel,
        if extra_apks_rel.is_empty() {
            "(none)".to_string()
        } else {
            extra_apks_rel.join(", ")
        }
    );

    // Step 5: create the container with the base APK + extra splits.
    println!("{} Creating container...", "•".cyan());
    let port_mappings = parse_ports(ports)?;
    let body = json!({
        "name": name,
        "apk": base_apk_rel,
        "memory_mb": memory,
        "cpu_percent": cpu,
        "notes": notes,
        "ports": port_mappings,
        "arch": resolved_arch,
        "translation_strategy": translation_strategy,
        "extra_apks": extra_apks_rel,
    });
    let container = client.create_container(&body).await?;
    let id = container["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing id in create response"))?
        .to_string();

    // Show the resolved package name if the bundle manifest supplied one.
    if let Some(pkg) = package {
        println!("{} Package: {}", "•".cyan(), pkg.green());
    }

    // Step 6: start it.
    println!("{} Starting container...", "•".cyan());
    let started = client.start_container(&id).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&started)?);
    } else {
        fmt::print_container_detail(&started);
        println!(
            "\n{} Bundle container {} is running ({} splits installed).",
            "✓".green(),
            started["name"].as_str().unwrap_or(&id),
            extra_apks_rel.len()
        );
    }
    Ok(())
}

/// Map a bundle-internal ABI segment (`arm64_v8a`, `armeabi_v7a`,
/// `x86_64`, `x86`) back to the CLI arch token (`arm64`, `arm`,
/// `x86_64`, `x86`). Used by `run_bundle --arch auto` so the CLI
/// can pass a clean arch token to `POST /containers`.
fn map_bundle_abi_to_cli(abi: &str) -> String {
    match abi {
        "arm64_v8a" => "arm64".to_string(),
        "armeabi_v7a" => "arm".to_string(),
        "x86_64" => "x86_64".to_string(),
        "x86" => "x86".to_string(),
        other => other.to_string(),
    }
}

/// `droidker mp4` — capture an MP4 video via Android's `screenrecord`
/// binary (M9.2).
///
/// Flow:
///   1. Resolve the container id_or_name to a UUID.
///   2. POST /containers/{id}/screen/record-mp4 with the duration,
///      bit_rate, size, and rotate flag.
///   3. The daemon blocks synchronously while `screenrecord` runs
///      inside the container's namespaces.
///   4. Write the response body (raw MP4 bytes) to the output file.
///
/// We print a small spinner while waiting so the user knows the
/// recording is in progress (the request can take up to 3 minutes).
pub async fn mp4(
    client: &DroidkerClient,
    id_or_name: &str,
    out: Option<&std::path::Path>,
    duration_sec: u32,
    bit_rate: u32,
    width: u32,
    height: u32,
    rotate: bool,
) -> Result<()> {
    let c = client.get_container(id_or_name).await?;
    let id = c["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing id in container response"))?;

    // Clamp duration to screenrecord's 3-minute hard cap.
    let duration = duration_sec.clamp(1, 180);

    let out_path = match out {
        Some(p) => p.to_path_buf(),
        None => std::path::PathBuf::from(format!(
            "{}-{}.mp4",
            &id[..8],
            chrono::Utc::now().timestamp()
        )),
    };

    println!(
        "{} recording MP4 of {} for {}s @ {}bps ({}x{}) -> {}",
        "•".cyan(),
        id,
        duration,
        bit_rate,
        width,
        height,
        out_path.display()
    );
    if rotate {
        println!("{} rotate=on (90°)", "•".cyan());
    }
    println!(
        "{} this blocks until recording finishes — keep the terminal open",
        "•".dimmed()
    );

    let start = std::time::Instant::now();
    let bytes = client
        .record_mp4(id, duration, bit_rate, width, height, rotate)
        .await?;
    let elapsed = start.elapsed().as_secs();

    std::fs::write(&out_path, &bytes)?;
    let size_kb = bytes.len() / 1024;

    println!(
        "{} captured {} KB in {}s -> {}",
        "✓".green(),
        size_kb,
        elapsed,
        out_path.display()
    );
    println!(
        "{} avg bitrate: {} kbps",
        "•".dimmed(),
        if elapsed > 0 {
            (bytes.len() * 8 / 1000) as u64 / elapsed
        } else {
            0
        }
    );
    Ok(())
}
