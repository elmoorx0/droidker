// src/config/settings.rs
//
// Concrete `Settings` struct + loader. The defaults are deliberately tuned
// for very low-resource VPS hosts (1 GB RAM / 1 vCPU).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Host the HTTP API binds to. Default `0.0.0.0` for VPS access.
    pub host: String,

    /// Port for the HTTP API.
    pub port: u16,

    /// Directory where container metadata, rootfs overlays, and uploaded APKs
    /// are stored. Must be on a filesystem that supports overlayfs.
    pub data_dir: PathBuf,

    /// Path to the shared Android rootfs (ART, Bionic, microG, etc.).
    /// This is bind-mounted read-only into every container.
    pub android_rootfs: PathBuf,

    /// Maximum number of concurrent containers. Capped to keep RAM usage low.
    pub max_containers: usize,

    /// Memory limit (in MB) applied to each container via cgroups v2.
    pub container_memory_mb: u32,

    /// CPU quota (percentage of one core, 1-100). 50 = half a vCPU.
    pub container_cpu_percent: u32,

    /// Path to the `binder` device node (created by setup.sh).
    pub binder_device: PathBuf,

    /// Path to the `ashmem` device node.
    pub ashmem_device: PathBuf,

    /// Network bridge name used for container networking.
    pub bridge_name: String,

    /// Subnet (CIDR) for container veth pairs.
    pub bridge_subnet: String,

    /// Path to the WebRTC signaling socket (for the streaming module).
    pub signaling_socket: PathBuf,

    /// Whether the host is ARM (no translation needed) or x86_64 (uses libhoudini/libndk).
    pub host_arch: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            data_dir: PathBuf::from("/var/lib/droidker"),
            android_rootfs: PathBuf::from("/opt/droidker/android-rootfs"),
            max_containers: 8,
            container_memory_mb: 128,
            container_cpu_percent: 50,
            binder_device: PathBuf::from("/dev/binder"),
            ashmem_device: PathBuf::from("/dev/ashmem"),
            bridge_name: "droidker0".to_string(),
            bridge_subnet: "10.244.0.0/16".to_string(),
            signaling_socket: PathBuf::from("/var/run/droidker/signaling.sock"),
            host_arch: detect_arch().to_string(),
        }
    }
}

impl Settings {
    /// Load settings from env vars and (optionally) a config file.
    ///
    /// Priority: built-in defaults < TOML file < env vars.
    pub fn load() -> Result<Self, anyhow::Error> {
        // 1. Serialize the built-in defaults to JSON and use that as the
        //    initial source. `config::Config::try_deserialize::<Settings>`
        //    used to accept a value in 0.13 but in 0.14 it takes no args,
        //    so we go through a JSON string instead.
        let defaults_json = serde_json::to_string(&Settings::default())?;

        let mut cfg_builder = config::Config::builder()
            .add_source(config::File::from_str(&defaults_json, config::FileFormat::Json));

        // 2. Override from /etc/droidker/config.toml (optional).
        let config_path = Path::new("/etc/droidker/config.toml");
        if config_path.exists() {
            cfg_builder = cfg_builder.add_source(config::File::from(config_path));
        }

        // 3. Override from env vars: DROIDKER_HOST, DROIDKER_PORT, DROIDKER__DATA_DIR, ...
        cfg_builder = cfg_builder.add_source(
            config::Environment::with_prefix("DROIDKER").separator("__"),
        );

        let settings: Settings = cfg_builder.build()?.try_deserialize()?;
        Ok(settings)
    }

    /// Ensure all required directories exist on disk.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for sub in ["containers", "apks", "overlays", "logs", "run"] {
            std::fs::create_dir_all(self.data_dir.join(sub))?;
        }
        std::fs::create_dir_all(
            self.signaling_socket
                .parent()
                .unwrap_or_else(|| Path::new("/var/run/droidker")),
        )?;
        Ok(())
    }
}

/// Detect host architecture so we know whether ARM translation is needed.
fn detect_arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        return "x86_64";
    }
    #[cfg(target_arch = "aarch64")]
    {
        return "aarch64";
    }
    #[cfg(target_arch = "arm")]
    {
        return "arm";
    }
    #[allow(unreachable_code)]
    {
        return "unknown";
    }
}
