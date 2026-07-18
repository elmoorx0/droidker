// src/models/container.rs
//
// Data model describing a DroidKer micro-container.
// A container is essentially a sandboxed Android runtime process plus metadata.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Lifecycle state of a container.
/// Mirrors a subset of OCI states for familiarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerStatus {
    /// Created but not yet started.
    Created,
    /// Process is running.
    Running,
    /// Process is paused (cgroup freezer).
    Paused,
    /// Process exited cleanly or was stopped.
    Stopped,
    /// Process died unexpectedly.
    Exited,
    /// Being created or destroyed.
    Creating,
}

impl ContainerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContainerStatus::Created => "created",
            ContainerStatus::Running => "running",
            ContainerStatus::Paused => "paused",
            ContainerStatus::Stopped => "stopped",
            ContainerStatus::Exited => "exited",
            ContainerStatus::Creating => "creating",
        }
    }
}

/// Full container record (stored on disk + held in memory).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Container {
    /// Random UUID v4 generated at create time.
    pub id: Uuid,
    /// Human-friendly name (must be unique).
    pub name: String,
    /// Package name of the installed APK (e.g. `com.example.app`).
    pub package: String,
    /// SHA-256 of the source APK file (for integrity / dedup).
    pub apk_sha256: String,
    /// Current lifecycle state.
    pub status: ContainerStatus,
    /// PID of the runtime process (0 if not running).
    pub pid: u32,
    /// Memory limit in MB.
    pub memory_mb: u32,
    /// CPU quota (% of one core).
    pub cpu_percent: u32,
    /// On-disk path of the container's writable overlay.
    pub rootfs: PathBuf,
    /// Internal IP on the bridge network.
    pub ip: Option<String>,
    /// veth pair name on the host side.
    pub veth_host: Option<String>,
    /// Port mappings `host:container` (TCP). Empty when no ports are
    /// published. We only support TCP for now — Android apps very rarely
    /// need UDP from outside the sandbox.
    #[serde(default)]
    pub ports: Vec<PortMapping>,
    /// Target CPU architecture for the container (M6). When `None`, the
    /// container runs on the host's native arch (no translation). When set
    /// to `arm` or `arm64` on an x86_64 host, DroidKer transparently
    /// invokes the configured translator (libhoudini / libndk_translation
    /// / qemu-user) so the APK's native `.so` libraries load correctly.
    #[serde(default)]
    pub arch: Option<String>,
    /// Translation strategy that was *actually used* the last time the
    /// container started. Populated by `ContainerManager::start` after the
    /// strategy has been resolved from `arch` + the host. Empty when the
    /// container has never been started.
    #[serde(default)]
    pub translation: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last state transition timestamp.
    pub updated_at: DateTime<Utc>,
    /// Optional human-readable notes.
    pub notes: Option<String>,
}

/// A `host:container` TCP port mapping, e.g. `8080:80` forwards host
/// port 8080 to the container's port 80.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PortMapping {
    /// Port on the host (what the user connects to).
    pub host: u16,
    /// Port inside the container (where the app listens).
    pub container: u16,
}

/// Lightweight projection used in `GET /containers` listings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSummary {
    pub id: Uuid,
    pub name: String,
    pub package: String,
    pub status: ContainerStatus,
    pub pid: u32,
    pub ip: Option<String>,
    pub arch: Option<String>,
    pub translation: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<&Container> for ContainerSummary {
    fn from(c: &Container) -> Self {
        Self {
            id: c.id,
            name: c.name.clone(),
            package: c.package.clone(),
            status: c.status,
            pid: c.pid,
            ip: c.ip.clone(),
            arch: c.arch.clone(),
            translation: c.translation.clone(),
            created_at: c.created_at,
        }
    }
}

/// Payload accepted by `POST /containers`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateContainerRequest {
    /// Friendly name. If omitted, a random one is generated.
    pub name: Option<String>,
    /// Path (relative to the data_dir/apks) of the uploaded APK to install.
    pub apk: String,
    /// Override the per-container memory limit (MB).
    pub memory_mb: Option<u32>,
    /// Override the per-container CPU quota (%).
    pub cpu_percent: Option<u32>,
    /// Free-form notes.
    pub notes: Option<String>,
    /// Port mappings to publish. Each entry forwards `host` → `container`
    /// via an iptables DNAT rule on the host. Defaults to none.
    #[serde(default)]
    pub ports: Vec<PortMapping>,
    /// Target CPU architecture for this container (M6). Accepted values:
    /// `arm`, `arm64`, `x86`, `x86_64`. When omitted, the container runs
    /// on the host's native arch.
    #[serde(default)]
    pub arch: Option<String>,
}

/// Payload for `POST /containers/{id}/humanize` — drives the Humanizer engine.
#[derive(Debug, Clone, Deserialize)]
pub struct HumanizeAction {
    /// What kind of action to perform.
    pub action: HumanizeActionKind,
    /// X coordinate (0..width) for taps/swipes.
    pub x: Option<i32>,
    /// Y coordinate (0..height) for taps/swipes.
    pub y: Option<i32>,
    /// End X for swipe gestures.
    pub x2: Option<i32>,
    /// End Y for swipe gestures.
    pub y2: Option<i32>,
    /// Text to type (for `type_text`).
    pub text: Option<String>,
    /// Duration in ms for swipe gestures.
    pub duration_ms: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanizeActionKind {
    Tap,
    DoubleTap,
    LongPress,
    Swipe,
    TypeText,
    Home,
    Back,
    Recent,
}
