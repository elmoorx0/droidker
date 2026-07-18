// src/container/cgroups.rs
//
// cgroup v2 management for DroidKer containers.
//
// Layout (under the unified hierarchy /sys/fs/cgroup):
//
//   /sys/fs/cgroup/droidker/
//   ├── container-<uuid>/
//   │   ├── cgroup.procs      (PIDs admitted to this cgroup)
//   │   ├── memory.max        (hard RSS limit in bytes)
//   │   ├── memory.high       (soft throttle threshold)
//   │   ├── memory.swap.max   (swap limit; 0 = no swap)
//   │   ├── cpu.max           ("$quota $period" — e.g. "50000 100000" = 50%)
//   │   ├── cpu.weight        (1..10000, relative share)
//   │   ├── pids.max          (max processes)
//   │   ├── cgroup.freeze     (write 1 to freeze, 0 to thaw)
//   │   └── cgroup.events     (pollable: notifies on OOM)
//
// Why cgroup v2 (not v1)?
//   - Unified hierarchy: no more per-controller mounts to juggle.
//   - Better delegation: a subtree can be owned by a non-root user.
//   - Modern kernels (>= 5.4) ship it as default, which is what every
//     current VPS provider installs.

use crate::error::{DroidkerError, Result};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Top-level cgroup hierarchy for all DroidKer containers.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Subtree under which per-container cgroups live.
const DROIDKER_SUBTREE: &str = "droidker";

/// Resource limits applied to a single container.
#[derive(Debug, Clone)]
pub struct CgroupLimits {
    /// Memory ceiling in bytes (memory.max).
    pub memory_max: u64,
    /// Soft throttle threshold (memory.high). Usually ~90% of memory_max.
    pub memory_high: u64,
    /// Swap allowance in bytes; 0 disables swap entirely.
    pub swap_max: u64,
    /// CPU quota in microseconds per period (cpu.max "$quota $period").
    /// Use `None` for "no limit" (max).
    pub cpu_quota_us: Option<u64>,
    /// CPU period in microseconds (usually 100000 = 100ms).
    pub cpu_period_us: u64,
    /// Relative CPU weight 1..10000 (cpu.weight).
    pub cpu_weight: u32,
    /// Maximum number of processes (pids.max). 0 = no limit.
    pub pids_max: u64,
}

impl CgroupLimits {
    /// Build limits from the high-level settings (memory MB + cpu %).
    pub fn from_settings(memory_mb: u32, cpu_percent: u32) -> Self {
        let memory_max = (memory_mb as u64) * 1024 * 1024;
        let memory_high = memory_max * 9 / 10;
        // cpu.max uses microseconds per period. 100% of one core = 100000/100000.
        // 50% of one core = 50000/100000.
        let quota = ((cpu_percent as u64) * 1000).min(10_000_000);
        Self {
            memory_max,
            memory_high,
            swap_max: 0, // No swap on a low-RAM VPS.
            cpu_quota_us: Some(quota),
            cpu_period_us: 100_000,
            cpu_weight: (cpu_percent as u32).max(1).min(10000),
            pids_max: 256, // Plenty for one Android app.
        }
    }
}

/// Handle to a per-container cgroup. Dropping it removes the cgroup (if empty).
pub struct Cgroup {
    pub path: PathBuf,
    pub container_id: Uuid,
}

impl Cgroup {
    /// Create the per-container cgroup and apply limits.
    pub fn create(container_id: Uuid, limits: &CgroupLimits) -> Result<Self> {
        ensure_root_subtree()?;

        let cg_name = format!("container-{}", container_id);
        let path = Path::new(CGROUP_ROOT)
            .join(DROIDKER_SUBTREE)
            .join(&cg_name);

        fs::create_dir_all(&path).map_err(|e| {
            DroidkerError::Syscall(format!(
                "create cgroup {} (are you root? is cgroup v2 mounted?): {}",
                path.display(),
                e
            ))
        })?;

        let cg = Self {
            path,
            container_id,
        };
        cg.apply_limits(limits)?;
        Ok(cg)
    }

    /// Write the limit files. Writes are best-effort for the optional ones
    /// (some kernels don't expose every controller).
    pub fn apply_limits(&self, limits: &CgroupLimits) -> Result<()> {
        write_control(&self.path.join("memory.max"), &limits.memory_max.to_string())?;
        write_control(
            &self.path.join("memory.high"),
            &limits.memory_high.to_string(),
        )?;
        write_control(&self.path.join("memory.swap.max"), &limits.swap_max.to_string())?;
        write_control(&self.path.join("pids.max"), &limits.pids_max.to_string())?;
        write_control(&self.path.join("cpu.weight"), &limits.cpu_weight.to_string())?;

        if let Some(quota) = limits.cpu_quota_us {
            // Format: "$quota $period" — "max" disables the limit.
            let val = format!("{} {}", quota, limits.cpu_period_us);
            write_control(&self.path.join("cpu.max"), &val)?;
        } else {
            write_control(&self.path.join("cpu.max"), "max")?;
        }

        // Enable memory + pids + cpu controllers on this subtree (idempotent).
        // On cgroup v2 the controllers must be enabled on the parent before
        // children can use them. We do this in `ensure_root_subtree`.
        Ok(())
    }

    /// Move a process (and its future children) into this cgroup.
    pub fn add_pid(&self, pid: u32) -> Result<()> {
        write_control(&self.path.join("cgroup.procs"), &pid.to_string())?;
        tracing::debug!(pid, cgroup = %self.path.display(), "PID added to cgroup");
        Ok(())
    }

    /// Freeze every task in the cgroup (used for pause).
    pub fn freeze(&self) -> Result<()> {
        write_control(&self.path.join("cgroup.freeze"), "1")
    }

    /// Resume every task in the cgroup.
    pub fn thaw(&self) -> Result<()> {
        write_control(&self.path.join("cgroup.freeze"), "0")
    }

    /// Read the OOM counter (memory.events → oom). Returns 0 if unavailable.
    pub fn oom_count(&self) -> u64 {
        let events = self.path.join("memory.events");
        match fs::read_to_string(&events) {
            Ok(s) => s
                .lines()
                .find_map(|l| {
                    let mut parts = l.split_whitespace();
                    if parts.next() == Some("oom") {
                        parts.next().and_then(|v| v.parse().ok())
                    } else {
                        None
                    }
                })
                .unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Remove the cgroup directory. Fails if there are still live processes.
    pub fn destroy(self) -> Result<()> {
        match fs::remove_dir(&self.path) {
            Ok(()) => {
                tracing::debug!(cgroup = %self.path.display(), "cgroup destroyed");
                Ok(())
            }
            Err(e) => {
                // If the directory is busy (live procs), try one more time
                // after freezing; otherwise log and move on.
                tracing::warn!(
                    cgroup = %self.path.display(),
                    error = %e,
                    "failed to destroy cgroup (live processes?)"
                );
                Err(DroidkerError::Syscall(format!("destroy cgroup: {e}")))
            }
        }
    }
}

/// Make sure /sys/fs/cgroup/droidker exists and has the memory + cpu + pids
/// controllers enabled on it.
fn ensure_root_subtree() -> Result<()> {
    let root = Path::new(CGROUP_ROOT);

    if !root.exists() {
        return Err(DroidkerError::Syscall(
            "/sys/fs/cgroup not found — is cgroup v2 mounted?".into(),
        ));
    }

    // Sanity: confirm we're actually on cgroup v2 (unified hierarchy).
    let controllers_file = root.join("cgroup.controllers");
    if !controllers_file.exists() {
        return Err(DroidkerError::Syscall(
            "cgroup.controllers file missing — host is not on cgroup v2".into(),
        ));
    }

    let subtree = root.join(DROIDKER_SUBTREE);
    if !subtree.exists() {
        fs::create_dir_all(&subtree).map_err(|e| {
            DroidkerError::Syscall(format!("create droidker subtree: {e}"))
        })?;
    }

    // Enable controllers at the root by writing to cgroup.subtree_control.
    // This must happen at the *parent* level — i.e. /sys/fs/cgroup itself —
    // so the droidker subtree inherits them.
    let parent_ctrl = root.join("cgroup.subtree_control");
    for controller in ["memory", "cpu", "pids", "io"] {
        let _ = fs::write(&parent_ctrl, format!("+{controller}"));
    }

    // And enable them on the droidker subtree too, so per-container cgroups
    // under it can use them.
    let our_ctrl = subtree.join("cgroup.subtree_control");
    for controller in ["memory", "cpu", "pids", "io"] {
        let _ = fs::write(&our_ctrl, format!("+{controller}"));
    }

    Ok(())
}

/// Write a value to a cgroup control file. Errors are wrapped as Syscall.
fn write_control(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value.as_bytes()).map_err(|e| {
        DroidkerError::Syscall(format!("write {}: {e}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_from_settings_basic() {
        let l = CgroupLimits::from_settings(128, 50);
        assert_eq!(l.memory_max, 128 * 1024 * 1024);
        assert_eq!(l.memory_high, l.memory_max * 9 / 10);
        assert_eq!(l.cpu_quota_us, Some(50_000));
        assert_eq!(l.cpu_period_us, 100_000);
        assert_eq!(l.pids_max, 256);
        assert_eq!(l.swap_max, 0);
    }

    #[test]
    fn cpu_quota_caps_at_safe_max() {
        let l = CgroupLimits::from_settings(128, 1000); // 1000% — absurd
        assert!(l.cpu_quota_us.unwrap() <= 10_000_000);
    }
}
