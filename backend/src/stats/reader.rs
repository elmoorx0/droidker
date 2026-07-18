// src/stats/reader.rs
//
// Concrete reader that walks cgroup v2 control files + /proc/<pid>/ to
// produce a single snapshot of a container's resource usage.

use crate::error::{DroidkerError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerStats {
    pub container_id: Uuid,
    pub sampled_at: DateTime<Utc>,
    pub pid: u32,
    pub memory: MemoryStat,
    pub cpu: CpuStat,
    pub pids: PidStat,
    pub io: Vec<IoStat>,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStat {
    /// Current RSS in bytes (memory.current).
    pub current: u64,
    /// High-water mark since boot (memory.peak).
    pub peak: u64,
    /// Hard limit (memory.max). 0 = unlimited.
    pub max: u64,
    /// Soft limit (memory.high). 0 = unlimited.
    pub high: u64,
    /// Number of OOM events since boot.
    pub oom: u64,
    /// Number of OOM-killed tasks since boot.
    pub oom_kill: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuStat {
    /// Microseconds of CPU time consumed.
    pub usage_usec: u64,
    /// Microseconds of CPU time spent throttled by cpu.max.
    pub throttled_usec: u64,
    /// Number of throttling events.
    pub nr_periods: u64,
    pub nr_throttled: u64,
    /// Quota / period (e.g. 50000/100000 = 50% of one core).
    pub quota: u64,
    pub period: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidStat {
    pub current: u64,
    pub peak: u64,
    pub max: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IoStat {
    /// Device major:minor, e.g. "8:0".
    pub device: String,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    /// PID *inside the container's PID namespace* (what the app sees).
    pub container_pid: u32,
    /// PID on the host (what the daemon sees).
    pub host_pid: u32,
    pub name: String,
    pub state: String,
    /// RSS in bytes (from /proc/<pid>/status → VmRSS).
    pub rss_kb: u64,
    /// CPU time in seconds (user + system).
    pub cpu_time_sec: f64,
    pub user: String,
}

pub struct StatsReader {
    cgroup_root: PathBuf,
}

impl StatsReader {
    pub fn new() -> Self {
        Self {
            cgroup_root: PathBuf::from("/sys/fs/cgroup/droidker"),
        }
    }

    /// Snapshot one container. Returns an error if the cgroup is gone.
    pub fn snapshot(&self, container_id: Uuid, host_pid: u32) -> Result<ContainerStats> {
        let cg_path = self.cgroup_root.join(format!("container-{}", container_id));
        if !cg_path.exists() {
            return Err(DroidkerError::NotFound(format!(
                "cgroup for container {} (looked at {})",
                container_id,
                cg_path.display()
            )));
        }

        let memory = read_memory(&cg_path)?;
        let cpu = read_cpu(&cg_path)?;
        let pids = read_pids(&cg_path)?;
        let io = read_io(&cg_path)?;
        let processes = read_processes(host_pid)?;

        Ok(ContainerStats {
            container_id,
            sampled_at: Utc::now(),
            pid: host_pid,
            memory,
            cpu,
            pids,
            io,
            processes,
        })
    }

    /// Snapshot every running container. Returns (id, stats) pairs.
    /// Failures on individual containers are logged and skipped.
    pub fn snapshot_all(
        &self,
        containers: &[(Uuid, u32)],
    ) -> Vec<(Uuid, Result<ContainerStats>)> {
        containers
            .iter()
            .map(|(id, pid)| (*id, self.snapshot(*id, *pid)))
            .collect()
    }
}

impl Default for StatsReader {
    fn default() -> Self {
        Self::new()
    }
}

// ----- Per-resource readers ----------------------------------------------

fn read_memory(cg: &Path) -> Result<MemoryStat> {
    let current = read_u64(cg.join("memory.current"))?;
    let peak = read_u64(cg.join("memory.peak"))?;
    let max_raw = read_string(&cg.join("memory.max"))?;
    let high_raw = read_string(&cg.join("memory.high"))?;
    let max = parse_max_or_unlimited(&max_raw);
    let high = parse_max_or_unlimited(&high_raw);

    let (oom, oom_kill) = read_oom_events(&cg.join("memory.events"));

    Ok(MemoryStat {
        current,
        peak,
        max,
        high,
        oom,
        oom_kill,
    })
}

fn read_cpu(cg: &Path) -> Result<CpuStat> {
    let stat = fs::read_to_string(cg.join("cpu.stat"))?;
    let mut usage_usec = 0u64;
    let mut throttled_usec = 0u64;
    let mut nr_periods = 0u64;
    let mut nr_throttled = 0u64;

    for line in stat.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("usage_usec") => usage_usec = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            Some("throttled_usec") => {
                throttled_usec = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0)
            }
            Some("nr_periods") => nr_periods = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            Some("nr_throttled") => {
                nr_throttled = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0)
            }
            _ => {}
        }
    }

    let max_raw = read_string(&cg.join("cpu.max"))?;
    let (quota, period) = parse_cpu_max(&max_raw);

    Ok(CpuStat {
        usage_usec,
        throttled_usec,
        nr_periods,
        nr_throttled,
        quota,
        period,
    })
}

fn read_pids(cg: &Path) -> Result<PidStat> {
    let current = read_u64(cg.join("pids.current"))?;
    let peak = read_u64(cg.join("pids.peak"))?;
    let max_raw = read_string(&cg.join("pids.max"))?;
    let max = parse_max_or_unlimited(&max_raw);
    Ok(PidStat {
        current,
        peak,
        max,
    })
}

fn read_io(cg: &Path) -> Result<Vec<IoStat>> {
    // io.stat format: "8:0 rbytes=123 wbytes=456 rios=78 wios=90"
    let raw = fs::read_to_string(cg.join("io.stat")).unwrap_or_default();
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        let device = match parts.next() {
            Some(d) => d.to_string(),
            None => continue,
        };
        let mut stat = IoStat {
            device,
            read_bytes: 0,
            write_bytes: 0,
            read_ios: 0,
            write_ios: 0,
        };
        for kv in parts {
            if let Some((k, v)) = kv.split_once('=') {
                let val: u64 = v.parse().unwrap_or(0);
                match k {
                    "rbytes" => stat.read_bytes = val,
                    "wbytes" => stat.write_bytes = val,
                    "rios" => stat.read_ios = val,
                    "wios" => stat.write_ios = val,
                    _ => {}
                }
            }
        }
        out.push(stat);
    }
    Ok(out)
}

fn read_processes(host_pid: u32) -> Result<Vec<ProcessInfo>> {
    // Walk the host PID subtree rooted at `host_pid` (the container's
    // init). /proc/<pid>/task/<tid>/children gives us the direct children,
    // and we recurse.
    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    walk_proc_tree(host_pid, host_pid, &mut visited, &mut out, 0)?;
    out.sort_by_key(|p| p.host_pid);
    Ok(out)
}

fn walk_proc_tree(
    root_pid: u32,
    cur_pid: u32,
    visited: &mut std::collections::HashSet<u32>,
    out: &mut Vec<ProcessInfo>,
    depth: u32,
) -> Result<()> {
    if depth > 32 || !visited.insert(cur_pid) {
        return Ok(()); // prevent cycles / runaway recursion
    }
    if let Some(info) = read_process_info(cur_pid, root_pid) {
        out.push(info);
    }
    let children_path = format!("/proc/{}/task/{}/children", cur_pid, cur_pid);
    if let Ok(children) = fs::read_to_string(&children_path) {
        for child_pid_str in children.split_whitespace() {
            if let Ok(child_pid) = child_pid_str.parse::<u32>() {
                let _ = walk_proc_tree(root_pid, child_pid, visited, out, depth + 1);
            }
        }
    }
    Ok(())
}

fn read_process_info(host_pid: u32, root_pid: u32) -> Option<ProcessInfo> {
    let status = fs::read_to_string(format!("/proc/{}/status", host_pid)).ok()?;
    let stat = fs::read_to_string(format!("/proc/{}/stat", host_pid)).ok()?;
    let comm = fs::read_to_string(format!("/proc/{}/comm", host_pid))
        .ok()?
        .trim_end()
        .to_string();

    let mut rss_kb = 0u64;
    let mut user = String::from("?");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            rss_kb = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Uid:") {
            // Real UID is the first number after "Uid:".
            if let Some(uid_str) = rest.split_whitespace().next() {
                if let Ok(uid) = uid_str.parse::<u32>() {
                    user = uid_to_name(uid);
                }
            }
        }
    }

    // Parse /proc/<pid>/stat to extract state + utime + stime.
    // Format: pid (comm) state ppid ... utime stime ...
    // The comm field can contain spaces and parens, so we extract state
    // as the first char after the closing paren.
    let close_paren = stat.rfind(')')?;
    let after = &stat[close_paren + 1..];
    let mut fields = after.split_whitespace();
    let state = fields.next().unwrap_or("?").to_string();
    // Skip ppid, pgrp, sid, tty, tpgid, flags, minflt, cminflt, majflt,
    // cmajflt — that's 10 fields before utime.
    let mut skipped = 0;
    let mut utime = 0u64;
    let mut stime = 0u64;
    for f in fields {
        if skipped < 10 {
            skipped += 1;
            continue;
        }
        if skipped == 10 {
            utime = f.parse().unwrap_or(0);
            skipped += 1;
        } else if skipped == 11 {
            stime = f.parse().unwrap_or(0);
            break;
        }
    }
    // CLK_TCK is usually 100 on Linux; convert to seconds.
    let cpu_time_sec = (utime + stime) as f64 / 100.0;

    // Map host PID → container PID (relative to the init).
    // The init itself is PID 1 inside the namespace.
    let container_pid = if host_pid == root_pid {
        1
    } else {
        // We don't actually have a way to know the in-namespace PID without
        // reading /proc/<pid>/status → NSpid. Use NSpid[1] if available.
        nspid_of(host_pid).unwrap_or(host_pid)
    };

    Some(ProcessInfo {
        container_pid,
        host_pid,
        name: comm,
        state,
        rss_kb,
        cpu_time_sec,
        user,
    })
}

fn nspid_of(host_pid: u32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{}/status", host_pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("NSpid:") {
            // Format: "NSpid:\t<host_pid>\t<in_ns_pid>\t..."
            let mut parts = rest.split_whitespace();
            parts.next()?; // skip the host PID
            return parts.next().and_then(|s| s.parse().ok());
        }
    }
    None
}

fn uid_to_name(uid: u32) -> String {
    // Cheap lookup via /etc/passwd. Avoid the `users` crate to keep the
    // binary small.
    if let Ok(passwd) = fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let mut parts = line.split(':');
            let name = parts.next().unwrap_or("?");
            parts.next(); // x
            parts.next(); // skip
            if let Some(uid_str) = parts.next() {
                if uid_str.parse::<u32>().ok() == Some(uid) {
                    return name.to_string();
                }
            }
        }
    }
    format!("uid={}", uid)
}

fn read_oom_events(path: &Path) -> (u64, u64) {
    let raw = fs::read_to_string(path).unwrap_or_default();
    let mut oom = 0u64;
    let mut oom_kill = 0u64;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("oom ") {
            oom = rest.parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("oom_kill ") {
            oom_kill = rest.parse().unwrap_or(0);
        }
    }
    (oom, oom_kill)
}

// ----- Low-level helpers -------------------------------------------------

fn read_u64(path: PathBuf) -> Result<u64> {
    let s = read_string(&path)?;
    Ok(s.trim().parse().unwrap_or(0))
}

fn read_string(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .map_err(|e| DroidkerError::Io(std::io::Error::new(e.kind(), format!("{}: {}", path.display(), e))))
}

/// "max" → 0 (means unlimited in our schema); a number → that number.
fn parse_max_or_unlimited(s: &str) -> u64 {
    let t = s.trim();
    if t == "max" || t.is_empty() {
        0
    } else {
        t.parse().unwrap_or(0)
    }
}

/// cpu.max format: "$quota $period" or "max $period".
fn parse_cpu_max(s: &str) -> (u64, u64) {
    let mut parts = s.trim().split_whitespace();
    let q = parts.next().unwrap_or("max");
    let p = parts.next().unwrap_or("100000");
    let quota = if q == "max" { 0 } else { q.parse().unwrap_or(0) };
    let period = p.parse().unwrap_or(100_000);
    (quota, period)
}

/// Convert a `Duration` to a human-readable short string.
#[allow(dead_code)]
pub fn fmt_duration(d: Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms < 1000 {
        format!("{}ms", total_ms)
    } else if total_ms < 60_000 {
        format!("{:.1}s", total_ms as f64 / 1000.0)
    } else if total_ms < 3_600_000 {
        format!("{:.1}m", total_ms as f64 / 60_000.0)
    } else {
        format!("{:.1}h", total_ms as f64 / 3_600_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_max_keyword_as_zero() {
        assert_eq!(parse_max_or_unlimited("max"), 0);
        assert_eq!(parse_max_or_unlimited(""), 0);
        assert_eq!(parse_max_or_unlimited("12345"), 12345);
    }

    #[test]
    fn parses_cpu_max_correctly() {
        assert_eq!(parse_cpu_max("50000 100000"), (50000, 100000));
        assert_eq!(parse_cpu_max("max 100000"), (0, 100000));
        // Default period when missing
        assert_eq!(parse_cpu_max("max"), (0, 100_000));
    }
}
