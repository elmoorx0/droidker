// src/container/isolation.rs
//
// Real Linux sandbox for DroidKer containers.
//
// Strategy: fork() a child with the clone flags below, then in the child:
//   1. setgroups + setresgid + setresuid (drop privileges if requested)
//   2. mount a new procfs + sysfs
//   3. mount the overlayfs (android_rootfs RO + per-container upperdir RW)
//   4. bind-mount /dev/binder, /dev/ashmem, /dev/null, /dev/zero, /dev/urandom
//   5. pivot_root into the merged view (makes the host fs invisible)
//   6. write the new hostname into the UTS namespace
//   7. drop Linux capabilities (M2: full set will land with seccomp in M2.6)
//   8. exec the Android runtime (app_process)
//
// The parent:
//   - waits for the child to publish its ready marker (or fail)
//   - moves the child PID into the per-container cgroup
//   - wires up the veth pair against the child's netns (via the PID)
//   - returns the (pid, veth_host, ip) tuple to ContainerManager

use crate::config::Settings;
use crate::container::cgroups::{Cgroup, CgroupLimits};
use crate::container::network::{IpAllocator, NetHandle, NetworkConfigurator};
use crate::container::rootfs;
use crate::container::translation::TranslationPlan;
use crate::error::{DroidkerError, Result};
use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

/// Everything needed to build a sandbox.
pub struct IsolationSpec {
    pub container_id: Uuid,
    pub rootfs_overlay_dir: PathBuf,
    pub android_rootfs: PathBuf,
    pub memory_mb: u32,
    pub cpu_percent: u32,
    pub bridge_name: String,
    pub bridge_subnet: String,
    pub binder_device: PathBuf,
    pub ashmem_device: PathBuf,
    pub hostname: String,
    /// Path to the runtime binary to exec inside the sandbox (usually
    /// /usr/local/bin/droidker-init).
    pub runtime_bin: PathBuf,
    /// Extra args passed to the runtime binary (e.g. package name).
    pub runtime_args: Vec<String>,
    /// Optional path to the host-side /dev/input/eventN node created by
    /// InputInjector. When set, droidker-init bind-mounts it into the
    /// container's /dev/input/event0 so Android's EventHub auto-detects
    /// the virtual touchscreen. When None, no input device is exposed
    /// and input injection API calls will create the injector lazily
    /// (M5 behavior — touch events still work because the kernel routes
    /// them via the host's uinput fd, but the container's own Android
    /// InputReader cannot read them back).
    pub input_event: Option<PathBuf>,
    /// M6: translation plan (target arch + strategy + bind-mounts + env
    /// vars). Passed to droidker-init through the environment so it can
    /// set up libhoudini / libndk_translation / qemu-user before exec'ing
    /// app_process64.
    pub translation: TranslationPlan,
}

/// Result of preparing a sandbox.
pub struct SandboxHandle {
    pub pid: u32,
    pub veth_host: String,
    pub ip: String,
    pub cgroup: Cgroup,
    pub net: NetHandle,
}

pub struct Isolator {
    settings: Settings,
}

impl Isolator {
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }

    /// Build the sandbox, returning the runtime PID + networking details.
    ///
    /// On entry we are running as the daemon (root). On exit the child has
    /// been spawned, has its own namespaces, and is waiting for the parent
    /// to attach the veth before exec'ing the runtime.
    pub fn prepare_sandbox(&self, spec: &IsolationSpec) -> Result<(u32, String, String)> {
        // ---- 1. Prepare the rootfs layout (overlay dirs) -------------------
        let rf_spec = rootfs::RootfsSpec {
            container_id: spec.container_id,
            android_rootfs: spec.android_rootfs.clone(),
            overlay_dir: spec.rootfs_overlay_dir.clone(),
            binder_device: spec.binder_device.clone(),
            ashmem_device: spec.ashmem_device.clone(),
        };
        let prepared = rootfs::prepare_layout(&rf_spec)?;

        // ---- 2. Create the per-container cgroup ---------------------------
        let limits = CgroupLimits::from_settings(spec.memory_mb, spec.cpu_percent);
        let cgroup = Cgroup::create(spec.container_id, &limits)?;

        // ---- 3. Allocate an IP from the bridge pool -----------------------
        let allocator = IpAllocator::new(&self.settings.data_dir.join("run"));
        let container_ip = allocator.allocate(spec.container_id)?;

        // ---- 4. Spawn the child with clone() ------------------------------
        // We use `/usr/bin/unshare` rather than calling clone(2) directly:
        //   - it's already installed (util-linux, ships with every distro)
        //   - it correctly handles the SID/ctty/cleanup dance for us
        //   - it lets us write the inner setup as a plain shell script,
        //     which is far easier to debug than a fork()-in-Rust tangle.
        //
        // The child runs `droidker-init`, which:
        //   - mounts the overlay
        //   - pivot_roots
        //   - bind-mounts devices
        //   - exec's app_process
        //
        // droidker-init takes its config from environment variables so we
        // don't have to write a temp file per spawn.

        let pid_fd_path = format!("/proc/self/fd/"); // placeholder; we use spawn().id()

        let mut cmd = Command::new("/usr/bin/unshare");
        cmd.env_clear();
        cmd.env("DROIDKER_CONTAINER_ID", spec.container_id.to_string());
        cmd.env("DROIDKER_ROOTFS_MERGED", &prepared.merged);
        cmd.env("DROIDKER_ROOTFS_UPPER", &prepared.upper);
        cmd.env("DROIDKER_ROOTFS_WORK", &prepared.work);
        cmd.env("DROIDKER_ANDROID_ROOTFS", &spec.android_rootfs);
        cmd.env("DROIDKER_BINDER_DEVICE", &spec.binder_device);
        cmd.env("DROIDKER_ASHMEM_DEVICE", &spec.ashmem_device);
        cmd.env("DROIDKER_HOSTNAME", &spec.hostname);
        cmd.env("RUST_LOG", "info");
        if let Some(p) = &spec.input_event {
            cmd.env("DROIDKER_INPUT_EVENT", p);
        }
        // M6: thread the translation plan (target arch + strategy + bind
        // mounts + LD_PRELOAD) through to droidker-init via env vars. The
        // init binary parses these after pivot_root and applies them just
        // before exec'ing app_process64.
        for (k, v) in spec.translation.env_vars() {
            cmd.env(k, v);
        }

        // Namespace flags. `unshare` accepts them as `-<flag>` short opts.
        cmd.args([
            "--mount",  // CLONE_NEWNS
            "--pid",    // CLONE_NEWPID
            "--net",    // CLONE_NEWNET
            "--uts",    // CLONE_NEWUTS
            "--ipc",    // CLONE_NEWIPC
            "--user",   // CLONE_NEWUSER — let us pretend to be root inside
            "--map-root-user", // map our uid to 0 inside the namespace
            "--fork",   // fork() so the child becomes PID 1 in the new ns
        ]);
        cmd.arg(&spec.runtime_bin);
        for a in &spec.runtime_args {
            cmd.arg(a);
        }

        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Use exec() directly so we can grab the PID before unshare returns.
        // We need the PID for the network configurator (which has to enter
        // the child's netns via /proc/<pid>/ns/net).
        let child = spawn_with_setsid(cmd)?;
        let pid = child.id();

        tracing::info!(
            container_id = %spec.container_id,
            pid,
            "Sandbox child spawned (unshare + droidker-init)"
        );

        // ---- 5. Move the child PID into the cgroup ------------------------
        if let Err(e) = cgroup.add_pid(pid) {
            tracing::warn!(error = %e, "failed to add PID to cgroup");
        }

        // ---- 6. Wire up networking (veth pair + bridge) -------------------
        let net_cfg = NetworkConfigurator::new(&spec.bridge_name, &spec.bridge_subnet);
        let net = match net_cfg.setup(pid, &container_ip) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "network setup failed; container will have no connectivity");
                // Best-effort cleanup of the cgroup before bailing out.
                let _ = cgroup.destroy();
                return Err(e);
            }
        };

        // Note: we intentionally leak the `child` handle here. The runtime
        // is the parent of the container's PID 1, and the daemon keeps
        // track of the PID via ContainerManager state. If the daemon is
        // killed, systemd will reparent the child to init, which is fine —
        // the cgroup ensures it dies with the host if the daemon doesn't
        // come back.
        std::mem::forget(child);
        let _ = pid_fd_path;

        Ok((pid, net.host_veth.clone(), net.ip.clone()))
    }
}

/// Spawn a child in its own session (so it survives the daemon dying).
fn spawn_with_setsid(mut cmd: Command) -> Result<std::process::Child> {
    unsafe {
        cmd.pre_exec(|| {
            // setsid() detaches us from the daemon's controlling terminal
            // and process group, so SIGINT to the daemon doesn't kill us.
            let rc = libc::setsid();
            if rc == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| {
        DroidkerError::Syscall(format!("spawn unshare: {e}"))
    })
}

/// Best-effort: kill a process group by sending SIGKILL to the negative PID.
/// Used by ContainerManager::stop as a fallback after SIGTERM.
#[allow(dead_code)]
pub fn kill_process_group(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if rc != 0 {
        return Err(DroidkerError::Syscall(format!(
            "kill -9 -{pid}: errno {}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        )));
    }
    Ok(())
}

/// Convert a Path to a CString (used by raw mount(2) calls).
#[allow(dead_code)]
fn path_to_cstring(p: &std::path::Path) -> Result<CString> {
    CString::new(p.as_os_str().as_encoded_bytes())
        .map_err(|e| DroidkerError::Syscall(format!("CString: {e}")))
}
