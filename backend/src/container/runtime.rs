// src/container/runtime.rs
//
// Android runtime launcher.
//
// In Milestone 2 the runtime is invoked via a small Rust binary called
// `droidker-init`, which:
//   1. mount(2) the overlayfs (lowerdir = android_rootfs RO, upperdir + workdir
//      from per-container overlay dir)
//   2. bind-mount /dev/binder, /dev/ashmem, /dev/pts, /dev/null, ...
//   3. pivot_root(2) into the merged overlay
//   4. write the hostname into /etc/hostname (UTS ns)
//   5. exec /system/bin/app_process64 with the Android boot classpath
//
// `droidker-init` lives in the same Cargo workspace as the daemon and ships
// as a separate binary so it can be statically linked and drop all the
// daemon's HTTP machinery.
//
// This file (runtime.rs in the daemon) just resolves the runtime binary path
// and the boot args, and hands them to `Isolator` via `RuntimeSpec`.

use crate::config::Settings;
use crate::error::Result;
use std::path::PathBuf;
use uuid::Uuid;

pub struct RuntimeSpec {
    pub container_id: Uuid,
    pub package: String,
    pub apk_sha256: String,
    pub rootfs_overlay_dir: PathBuf,
    pub apk_path: PathBuf,
}

pub struct AndroidRuntime {
    settings: Settings,
}

impl AndroidRuntime {
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }

    /// Resolve the runtime binary + args for this container.
    ///
    /// Returns a tuple of (binary_path, args).
    pub fn build_invocation(&self, spec: &RuntimeSpec) -> Result<(PathBuf, Vec<String>)> {
        // The init binary ships alongside droidkerd; look it up relative
        // to the current executable, then fall back to /usr/local/bin.
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("/usr/local/bin"));

        let candidate_local = exe_dir.join("droidker-init");
        let candidate_global = PathBuf::from("/usr/local/bin/droidker-init");

        let bin = if candidate_local.exists() {
            candidate_local
        } else if candidate_global.exists() {
            candidate_global
        } else {
            tracing::warn!(
                local = %candidate_local.display(),
                global = %candidate_global.display(),
                "droidker-init not found; falling back to placeholder"
            );
            // Milestone 1 compatibility: if droidker-init is missing we
            // fall back to /bin/sh so the sandbox still starts. Useful for
            // local dev where you haven't run `cargo build --bin droidker-init`.
            PathBuf::from("/bin/sh")
        };

        let mut args = Vec::new();
        if bin.file_name().and_then(|s| s.to_str()) == Some("droidker-init") {
            args.push(spec.container_id.to_string());
            args.push(spec.package.clone());
            args.push(spec.apk_path.to_string_lossy().into_owned());
            args.push(spec.apk_sha256.clone());
        } else {
            // Placeholder fallback: sleep forever so the parent can wire
            // networking + cgroups up against a live PID.
            args.push("-c".to_string());
            args.push(format!(
                "echo '[droidker] sandbox placeholder for {} ({})' ; sleep 3600",
                spec.container_id, spec.package
            ));
        }

        tracing::info!(
            container_id = %spec.container_id,
            bin = %bin.display(),
            args = ?args,
            "Runtime invocation built"
        );
        Ok((bin, args))
    }

    /// Legacy entry point kept for compatibility with manager.rs.
    /// In M2 the actual launch happens inside Isolator::prepare_sandbox
    /// (which forks unshare -> droidker-init), so this is a no-op marker.
    pub async fn launch(&self, spec: &RuntimeSpec) -> Result<()> {
        let (bin, args) = self.build_invocation(spec)?;
        let marker = spec
            .rootfs_overlay_dir
            .join(spec.container_id.to_string())
            .join("droidker.runtime.marker");
        let body = format!(
            "container_id={}\npackage={}\napk_sha256={}\nruntime_bin={}\nruntime_args={:?}\nlaunched_at={}\n",
            spec.container_id,
            spec.package,
            spec.apk_sha256,
            bin.display(),
            args,
            chrono::Utc::now().to_rfc3339()
        );
        tokio::fs::create_dir_all(marker.parent().unwrap()).await.ok();
        tokio::fs::write(&marker, body).await?;
        Ok(())
    }
}
