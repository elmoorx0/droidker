// src/container/rootfs.rs
//
// Filesystem preparation for a container.
//
// Layer model (mirrors Docker's overlayfs scheme):
//
//   ┌─── read-only bind from /opt/droidker/android-rootfs ───┐
//   │                          (lowerdir)                     │
//   ├─────────────────────────────────────────────────────────┤
//   │   /var/lib/droidker/overlays/<id>/upper    (upperdir)   │
//   │   /var/lib/droidker/overlays/<id>/work     (workdir)    │
//   └─────────────────────────────────────────────────────────┘
//                              ▼
//                  pivot_root into merged view
//
// Plus per-container bind mounts (all writable so the app can change them):
//   /dev/binder  -> /dev/binderfs/<container-id>  (binder context isolation)
//   /dev/ashmem  -> /dev/ashmem                   (shared; M3 will isolate)
//   /dev/pts, /dev/null, /dev/zero, /dev/random, /dev/urandom
//   /proc (new procfs instance inside pid namespace)
//   /sys (read-only sysfs bind)
//   /data (writable, lives in the overlay upperdir)

use crate::config::Settings;
use crate::error::{DroidkerError, Result};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub struct RootfsSpec {
    pub container_id: Uuid,
    pub android_rootfs: PathBuf,
    pub overlay_dir: PathBuf,
    pub binder_device: PathBuf,
    pub ashmem_device: PathBuf,
}

pub struct PreparedRootfs {
    /// The merged view that will become the container's new root.
    pub merged: PathBuf,
    pub upper: PathBuf,
    pub work: PathBuf,
}

/// Create the overlay directory structure on disk. The actual `mount -t
/// overlay` happens inside the child's mount namespace (see `isolation.rs`).
pub fn prepare_layout(spec: &RootfsSpec) -> Result<PreparedRootfs> {
    let id_str = spec.container_id.to_string();
    let base = spec.overlay_dir.join(&id_str);
    let merged = base.join("merged");
    let upper = base.join("upper");
    let work = base.join("work");

    for d in [&merged, &upper, &work] {
        fs::create_dir_all(d).map_err(|e| {
            DroidkerError::Io(std::io::Error::new(
                e.kind(),
                format!("create overlay dir {}: {e}", d.display()),
            ))
        })?;
    }

    // Pre-create the writable subdirs the Android runtime expects to find
    // on first boot. ART will populate these as the app starts.
    for sub in [
        "data/data",
        "data/app",
        "data/local/tmp",
        "data/media",
        "data/dalvik-cache",
        "cache",
        "acct",
        "dev",
        "proc",
        "sys",
        "system/etc/permissions",
    ] {
        fs::create_dir_all(upper.join(sub))?;
    }

    tracing::debug!(container_id = %spec.container_id, merged = %merged.display(), "Rootfs layout prepared");
    Ok(PreparedRootfs {
        merged,
        upper,
        work,
    })
}

/// Tear down the overlay directory for a container (called on delete).
pub fn remove_layout(overlay_dir: &Path, container_id: Uuid) -> Result<()> {
    let base = overlay_dir.join(container_id.to_string());
    if base.exists() {
        fs::remove_dir_all(&base).map_err(|e| {
            DroidkerError::Io(std::io::Error::new(
                e.kind(),
                format!("remove overlay {}: {e}", base.display()),
            ))
        })?;
    }
    Ok(())
}

/// Validate that the shared Android rootfs has the minimum required pieces.
/// Called once at daemon startup so we fail fast on a misconfigured host.
pub fn validate_android_rootfs(settings: &Settings) -> Result<()> {
    let r = &settings.android_rootfs;

    let required = [
        ("system/lib", false),        // 32-bit Bionic/ART (may be absent on arm64-only)
        ("system/lib64", true),       // 64-bit Bionic/ART
        ("system/framework", true),   // ART boot jars (services.jar, framework.jar, ...)
        ("system/bin/app_process64", true), // ART entry point
        ("system/etc", true),
        ("system/build.prop", true),
    ];

    let mut missing_strict = Vec::new();
    let mut missing_optional = Vec::new();

    for (rel, strict) in required {
        let p = r.join(rel);
        if !p.exists() {
            if strict {
                missing_strict.push(rel.to_string());
            } else {
                missing_optional.push(rel.to_string());
            }
        }
    }

    if !missing_strict.is_empty() {
        return Err(DroidkerError::Internal(format!(
            "Android rootfs at {} is missing required paths: {}\n\
             Run scripts/build-rootfs.sh to build a rootfs first.",
            r.display(),
            missing_strict.join(", ")
        )));
    }

    if !missing_optional.is_empty() {
        tracing::warn!(
            rootfs = %r.display(),
            missing = ?missing_optional,
            "Android rootfs is missing some optional paths (32-bit support may be absent)"
        );
    }

    Ok(())
}

/// Bind-mount a path. Must be called from inside the container's mount
/// namespace.
pub fn bind_mount_ro(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    mount_bind(src, dst, true)
}

pub fn bind_mount_rw(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    mount_bind(src, dst, false)
}

fn mount_bind(src: &Path, dst: &Path, read_only: bool) -> Result<()> {
    use std::ffi::CString;

    let src_c = CString::new(src.as_os_str().as_encoded_bytes())
        .map_err(|e| DroidkerError::Syscall(format!("CString: {e}")))?;
    let dst_c = CString::new(dst.as_os_str().as_encoded_bytes())
        .map_err(|e| DroidkerError::Syscall(format!("CString: {e}")))?;

    let mut flags = libc::MS_BIND | libc::MS_REC;
    if read_only {
        flags |= libc::MS_RDONLY;
    }

    // First mount (bind), then remount to apply MS_RDONLY if requested.
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            dst_c.as_ptr(),
            std::ptr::null(),
            flags & !libc::MS_RDONLY, // initial bind is rw
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(DroidkerError::Syscall(format!(
            "mount({} -> {}): errno {}",
            src.display(),
            dst.display(),
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        )));
    }

    if read_only {
        let rc = unsafe {
            libc::mount(
                src_c.as_ptr(),
                dst_c.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            tracing::warn!(
                src = %src.display(),
                dst = %dst.display(),
                "remount RO failed (non-fatal)"
            );
        }
    }

    Ok(())
}

/// Symlink helper that won't error if the target already exists.
pub fn ensure_symlink(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(());
    }
    symlink(src, dst).map_err(|e| {
        DroidkerError::Syscall(format!(
            "symlink {} -> {}: {e}",
            src.display(),
            dst.display()
        ))
    })?;
    Ok(())
}
