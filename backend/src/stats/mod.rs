// src/stats/mod.rs
//
// Live resource statistics for running containers, sourced directly from
// cgroup v2 control files. No agents, no /proc scraping inside the
// container — everything we need is exposed by the host kernel under
// /sys/fs/cgroup/droidker/container-<id>/.
//
// What we report:
//   - memory.current (bytes currently used)
//   - memory.peak (high-water mark since boot)
//   - memory.events (oom, oom_kill)
//   - cpu.stat (usec consumed, throttled)
//   - pids.current / pids.peak
//   - io.stat (bytes read/written per device)
//   - process list (PIDs visible inside the PID namespace)

pub mod reader;

pub use reader::{ContainerStats, CpuStat, IoStat, MemoryStat, ProcessInfo, StatsReader};
