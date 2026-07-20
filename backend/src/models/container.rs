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
    /// Per-container translation strategy override (M7.2). When set to
    /// one of `houdini` | `ndk_translation` | `qemu-user` | `native`, the
    /// manager uses this strategy verbatim instead of probing the host.
    /// Useful for apps that crash under libhoudini but work fine under
    /// qemu-user, or for reproducible benchmark runs. Empty / `None`
    /// means "auto-resolve" (the M6 default).
    #[serde(default)]
    pub translation_strategy: Option<String>,
    /// M9.1: extra split APKs to install alongside the base APK. Each
    /// entry is a path relative to `<data_dir>/apks/` — typically
    /// `<bundle_sha>/config.arm64_v8a.apk`. Persisted so subsequent
    /// `start` calls after a `stop` re-bind-mount the same splits.
    #[serde(default)]
    pub extra_apks: Vec<String>,
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
    #[serde(default)]
    pub translation_strategy: Option<String>,
    /// M9.1: number of extra split APKs attached to this container.
    /// Zero for plain (non-bundle) containers.
    #[serde(default)]
    pub extra_apks_count: usize,
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
            translation_strategy: c.translation_strategy.clone(),
            extra_apks_count: c.extra_apks.len(),
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
    /// Translation strategy override (M7.2). Accepted values:
    /// `houdini`, `ndk_translation`, `qemu-user`, `native`. When omitted,
    /// the manager auto-resolves the strategy based on the host and the
    /// requested `arch`. Useful for apps that crash under libhoudini but
    /// work fine under qemu-user.
    #[serde(default)]
    pub translation_strategy: Option<String>,
    /// M9.1: extra split APKs to install alongside the base APK (for
    /// `.xapk` / `.apks` bundle support). Each entry is a filename
    /// relative to `<data_dir>/apks/` (NOT a host path) — typically
    /// `<bundle_sha>/config.arm64_v8a.apk`. The manager resolves these
    /// to absolute paths before passing them to `droidker-init` via
    /// `DROIDKER_EXTRA_APKS`.
    ///
    /// Empty for plain (non-bundle) APK containers.
    #[serde(default)]
    pub extra_apks: Vec<String>,
}

/// Payload for `POST /containers/{id}/humanize` — drives the Humanizer engine.
#[derive(Debug, Clone, Deserialize)]
pub struct HumanizeAction {
    /// What kind of action to perform.
    pub action: HumanizeActionKind,
    /// X coordinate (0..width) for taps/swipes. For pinch-zoom gestures
    /// this is the X of the gesture's center point.
    pub x: Option<i32>,
    /// Y coordinate (0..height) for taps/swipes. For pinch-zoom gestures
    /// this is the Y of the gesture's center point.
    pub y: Option<i32>,
    /// End X for swipe gestures.
    pub x2: Option<i32>,
    /// End Y for swipe gestures.
    pub y2: Option<i32>,
    /// Text to type (for `type_text`).
    pub text: Option<String>,
    /// Duration in ms for swipe gestures.
    pub duration_ms: Option<u32>,
    /// Initial distance between the two fingers for pinch-zoom gestures
    /// (M8.4). Defaults to 30 px (fingers close together) when omitted.
    pub start_distance: Option<f64>,
    /// Final distance between the two fingers for pinch-zoom gestures
    /// (M8.4). Defaults to 200 px (zoom-in) or 30 px (zoom-out) when
    /// omitted, depending on the action.
    pub end_distance: Option<f64>,
    /// Orientation of the pinch line in degrees (M8.4). 0° = horizontal,
    /// 90° = vertical, 45° = diagonal (the human-default). Defaults to
    /// 45° when omitted.
    pub angle_deg: Option<f64>,
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
    /// Two-finger pinch gesture (M8.4). Use `start_distance`,
    /// `end_distance`, and `angle_deg` to control the gesture. When
    /// `end_distance > start_distance`, it's a zoom-in; otherwise
    /// zoom-out.
    PinchZoom,
    /// Convenience alias for `PinchZoom` with `end_distance > start_distance`.
    ZoomIn,
    /// Convenience alias for `PinchZoom` with `end_distance < start_distance`.
    ZoomOut,
}
