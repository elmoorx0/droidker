// src/fmt.rs
//
// Pretty-printing helpers for terminal output.

use chrono::{DateTime, Utc};
use colored::Colorize;
use comfy_table::{Cell, ContentArrangement, Row, Table};
use serde_json::Value;

/// Render a JSON value with indentation.
pub fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

/// Render a list of containers as a table.
pub fn print_container_table(containers: &[Value]) {
    if containers.is_empty() {
        println!("{}", "No containers found.".yellow());
        return;
    }
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "ID",
            "NAME",
            "PACKAGE",
            "STATUS",
            "ARCH",
            "PID",
            "IP",
            "CREATED",
        ]);

    for c in containers {
        let id = c["id"].as_str().unwrap_or("-");
        let short_id = if id.len() >= 8 { &id[..8] } else { id };
        let status = c["status"].as_str().unwrap_or("-");
        let status_colored = match status {
            "running" => status.green().to_string(),
            "stopped" | "exited" => status.red().to_string(),
            "created" | "paused" => status.yellow().to_string(),
            _ => status.to_string(),
        };

        let created = c["created_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc).to_rfc3339())
            .unwrap_or_else(|| "-".to_string());

        // M6: show arch + translation strategy as a combined cell like
        // "arm64-v8a (libhoudini)". When translation is `native` or
        // null we just show the arch.
        let arch = c["arch"].as_str().unwrap_or("-");
        let translation = c["translation"].as_str().unwrap_or("");
        let arch_cell = match (arch, translation) {
            ("-", _) => "-".to_string(),
            (a, "native") | (a, "") => a.to_string(),
            (a, t) => format!("{} ({})", a, t),
        };

        table.add_row(Row::from(vec![
            Cell::new(short_id),
            Cell::new(c["name"].as_str().unwrap_or("-")),
            Cell::new(c["package"].as_str().unwrap_or("-")),
            Cell::new(status_colored),
            Cell::new(arch_cell),
            Cell::new(c["pid"].as_u64().unwrap_or(0)),
            Cell::new(c["ip"].as_str().unwrap_or("-")),
            Cell::new(created),
        ]));
    }
    println!("{table}");
}

/// Render a single container as a labelled key/value block.
pub fn print_container_detail(c: &Value) {
    let lines = vec![
        ("ID", c["id"].as_str().unwrap_or("-").to_string()),
        ("Name", c["name"].as_str().unwrap_or("-").to_string()),
        ("Package", c["package"].as_str().unwrap_or("-").to_string()),
        ("Status", c["status"].as_str().unwrap_or("-").to_string()),
        ("PID", c["pid"].as_u64().unwrap_or(0).to_string()),
        ("IP", c["ip"].as_str().unwrap_or("-").to_string()),
        (
            "Memory (MB)",
            c["memory_mb"].as_u64().unwrap_or(0).to_string(),
        ),
        (
            "CPU (%)",
            c["cpu_percent"].as_u64().unwrap_or(0).to_string(),
        ),
        (
            "Rootfs",
            c["rootfs"].as_str().unwrap_or("-").to_string(),
        ),
        (
            "APK SHA256",
            c["apk_sha256"].as_str().unwrap_or("-").to_string(),
        ),
        // M6: arch + translation strategy.
        (
            "Target arch",
            c["arch"].as_str().unwrap_or("host-native").to_string(),
        ),
        (
            "Translation",
            c["translation"].as_str().unwrap_or("-").to_string(),
        ),
        // M7.2: per-container translation_strategy override (empty when
        // the manager is auto-resolving).
        (
            "Strategy override",
            c["translation_strategy"].as_str().unwrap_or("(auto)").to_string(),
        ),
        (
            "Created",
            c["created_at"].as_str().unwrap_or("-").to_string(),
        ),
        (
            "Updated",
            c["updated_at"].as_str().unwrap_or("-").to_string(),
        ),
    ];

    for (k, v) in lines {
        println!("{}: {}", k.cyan().bold(), v);
    }
}

/// Render a stats snapshot as a labelled block.
///
/// The `v` argument is the JSON returned by `GET /containers/{id}/stats`.
pub fn print_stats(v: &Value) {
    let mem = &v["memory"];
    let cpu = &v["cpu"];
    let pids = &v["pids"];

    let mem_cur = fmt_bytes(mem["current"].as_u64().unwrap_or(0));
    let mem_peak = fmt_bytes(mem["peak"].as_u64().unwrap_or(0));
    let mem_max = {
        let m = mem["max"].as_u64().unwrap_or(0);
        if m == 0 {
            "unlimited".to_string()
        } else {
            fmt_bytes(m)
        }
    };
    let oom = mem["oom"].as_u64().unwrap_or(0);
    let oom_kill = mem["oom_kill"].as_u64().unwrap_or(0);

    let cpu_usage_sec = cpu["usage_usec"].as_u64().unwrap_or(0) as f64 / 1_000_000.0;
    let cpu_throttled_sec = cpu["throttled_usec"].as_u64().unwrap_or(0) as f64 / 1_000_000.0;
    let cpu_quota = cpu["quota"].as_u64().unwrap_or(0);
    let cpu_period = cpu["period"].as_u64().unwrap_or(0);
    let cpu_pct = if cpu_period > 0 && cpu_quota > 0 {
        format!("{:.1}% of one core", (cpu_quota as f64 / cpu_period as f64) * 100.0)
    } else {
        "unlimited".to_string()
    };

    let pid_cur = pids["current"].as_u64().unwrap_or(0);
    let pid_peak = pids["peak"].as_u64().unwrap_or(0);
    let pid_max = {
        let m = pids["max"].as_u64().unwrap_or(0);
        if m == 0 {
            "unlimited".to_string()
        } else {
            m.to_string()
        }
    };

    println!("{}", "Memory".cyan().bold());
    println!("  current:  {}", mem_cur);
    println!("  peak:     {}", mem_peak);
    println!("  limit:    {}", mem_max);
    println!("  OOM events: {} ({} killed)", oom, oom_kill);
    println!();
    println!("{}", "CPU".cyan().bold());
    println!("  used:        {:.2}s", cpu_usage_sec);
    println!("  throttled:   {:.2}s", cpu_throttled_sec);
    println!("  quota:       {}", cpu_pct);
    println!();
    println!("{}", "Processes".cyan().bold());
    println!("  current:  {}", pid_cur);
    println!("  peak:     {}", pid_peak);
    println!("  limit:    {}", pid_max);

    if let Some(procs) = v["processes"].as_array() {
        if !procs.is_empty() {
            println!();
            println!("{}", "Top processes".cyan().bold());
            let mut table = Table::new();
            table
                .load_preset(comfy_table::presets::UTF8_FULL)
                .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
                .set_content_arrangement(ContentArrangement::Dynamic)
                .set_header(vec!["PID", "NAME", "RSS", "CPU(s)", "USER"]);
            // Sort by RSS descending and show top 10.
            let mut sorted: Vec<&Value> = procs.iter().collect();
            sorted.sort_by_key(|p| {
                std::cmp::Reverse(p["rss_kb"].as_u64().unwrap_or(0))
            });
            for p in sorted.iter().take(10) {
                table.add_row(Row::from(vec![
                    Cell::new(p["container_pid"].as_u64().unwrap_or(0)),
                    Cell::new(p["name"].as_str().unwrap_or("-")),
                    Cell::new(fmt_bytes(
                        p["rss_kb"].as_u64().unwrap_or(0) * 1024,
                    )),
                    Cell::new(format!(
                        "{:.2}",
                        p["cpu_time_sec"].as_f64().unwrap_or(0.0)
                    )),
                    Cell::new(p["user"].as_str().unwrap_or("-")),
                ]));
            }
            println!("{table}");
        }
    }
}

/// Format a byte count as a human-readable string (B / KiB / MiB / GiB).
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}
