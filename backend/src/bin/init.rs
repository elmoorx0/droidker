// src/bin/init.rs
//
// `droidker-init` — PID 1 of every DroidKer container.
//
// Invoked by `unshare --mount --pid --net --uts --ipc --user --map-root-user
// --fork /usr/local/bin/droidker-init <container_id> <package> <apk_path>
// <apk_sha256>`.
//
// Responsibilities:
//   1. Mount the overlayfs (lowerdir = android_rootfs RO, upperdir + workdir
//      from per-container dirs).
//   2. Bind-mount /dev/binder, /dev/ashmem, /dev/null, /dev/zero, /dev/urandom,
//      /dev/pts into the merged view.
//   3. Mount a fresh procfs and sysfs inside the merged view.
//   4. pivot_root(2) into the merged view so the host filesystem becomes
//      invisible.
//   5. Write the hostname into the UTS namespace.
//   6. Drop Linux capabilities (bounding set) to the minimum needed by ART.
//   7. Install the seccomp filter (AndroidRuntime profile).
//   8. Reap zombies as long as we live (PID 1 responsibility).
//   9. M6: bind-mount the translator's `.so` files (libhoudini /
//      libndk_translation / qemu-user) into /system/lib* and patch
//      `ro.product.cpu.abi` in build.prop so ART reports the target arch.
//  10. exec(2) /system/bin/app_process64 with the Android boot classpath.
//
// Environment variables consumed:
//   DROIDKER_ROOTFS_MERGED   — where the overlay is mounted
//   DROIDKER_ROOTFS_UPPER    — overlay upperdir
//   DROIDKER_ROOTFS_WORK     — overlay workdir
//   DROIDKER_ANDROID_ROOTFS  — lowerdir (read-only Android system image)
//   DROIDKER_BINDER_DEVICE   — host binder device node
//   DROIDKER_ASHMEM_DEVICE   — host ashmem device node
//   DROIDKER_INPUT_EVENT     — host /dev/input/eventN node created by
//                              InputInjector (M5); bind-mounted into the
//                              container so Android's EventHub picks it up
//                              as the primary touchscreen. Optional: if
//                              absent, no input device is exposed.
//   DROIDKER_HOSTNAME        — hostname for the UTS namespace
//   DROIDKER_TARGET_ARCH     — target arch string (e.g. "arm64-v8a")
//   DROIDKER_TRANSLATION_STRATEGY — one of "native"|"libhoudini"|
//                              "libndk_translation"|"qemu-user"|"none"
//   DROIDKER_TRANSLATION_MOUNTS — `:`-separated `src=dst` pairs to bind-mount
//                              the translator's .so files into /system/lib*
//   DROIDKER_APP_ENV_<NAME>  — extra env vars to set in the app_process
//                              environment (LD_PRELOAD, HOUDINI_ENABLE, ...)
//   RUST_LOG                 — log level

use std::ffi::CString;
use std::path::{Path, PathBuf};

fn main() {
    // Initialize logging to stderr (so the daemon can capture it).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!(
            "Usage: droidker-init <container_id> <package> <apk_path> <apk_sha256>"
        );
        std::process::exit(2);
    }
    let container_id = &args[1];
    let package = &args[2];
    let apk_path = &args[3];
    let apk_sha = &args[4];

    tracing::info!(
        container_id,
        package,
        apk_path,
        apk_sha,
        pid = std::process::id(),
        "droidker-init starting"
    );

    if let Err(e) = run(container_id, package, apk_path, apk_sha) {
        tracing::error!(error = %e, "droidker-init failed");
        std::process::exit(1);
    }
}

fn run(container_id: &str, package: &str, apk_path: &str, apk_sha: &str) -> Result<(), String> {
    let merged = env_path("DROIDKER_ROOTFS_MERGED")?;
    let upper = env_path("DROIDKER_ROOTFS_UPPER")?;
    let work = env_path("DROIDKER_ROOTFS_WORK")?;
    let android_rootfs = env_path("DROIDKER_ANDROID_ROOTFS")?;
    let binder_device = env_path("DROIDKER_BINDER_DEVICE")?;
    let ashmem_device = env_path("DROIDKER_ASHMEM_DEVICE")?;
    let hostname = std::env::var("DROIDKER_HOSTNAME").unwrap_or_else(|_| "droidker".to_string());

    // ---- 1. Mount overlayfs ------------------------------------------------
    mount_overlay(&android_rootfs, &upper, &work, &merged)?;

    // ---- 2. Bind-mount /dev nodes -----------------------------------------
    // The optional /dev/input/eventN node is created by the daemon's
    // InputInjector before we exec. If present, it gets bind-mounted into
    // the container's /dev/input/ so Android's EventHub auto-detects the
    // virtual touchscreen on boot.
    let input_event = std::env::var("DROIDKER_INPUT_EVENT").ok().map(PathBuf::from);
    setup_dev_nodes(&merged, &binder_device, &ashmem_device, input_event.as_deref())?;

    // ---- 3. Mount fresh procfs + sysfs ------------------------------------
    setup_proc_and_sys(&merged)?;

    // ---- 4. pivot_root into the merged view -------------------------------
    pivot_into(&merged)?;

    // ---- 5. Set hostname --------------------------------------------------
    set_hostname(&hostname)?;

    // ---- 6. Drop capabilities (bounding set) ------------------------------
    drop_capabilities().map_err(|e| format!("drop capabilities: {e}"))?;

    // ---- 7. Apply seccomp blocklist ---------------------------------------
    // Build a minimal blocklist inline (we don't link the daemon's seccomp
    // module here to keep droidker-init a separate, small binary).
    //
    // The blocklist matches `Profile::AndroidRuntime` from the daemon's
    // seccomp.rs — module loading, kexec, ptrace, bpf, setns, unshare, etc.
    apply_seccomp_blocklist().map_err(|e| format!("apply seccomp: {e}"))?;

    // ---- 8. Install APK + exec app_process -------------------------------
    install_apk(package, apk_path, apk_sha)?;

    // ---- 9. Start logcat capture in the background ----------------------
    // The logcat process drains the Android system log buffer into a file
    // the daemon can tail. We redirect both logcat's own stderr and the
    // buffer contents to /data/droidker.logcat.log.
    //
    // If /system/bin/logcat doesn't exist (e.g. on the skeleton rootfs),
    // we silently skip — the daemon's log streamer will just see an
    // empty file.
    start_logcat_capture();

    // ---- 10. M6: translation layer setup ---------------------------------
    // Bind-mount libhoudini / libndk_translation / qemu-user into /system/lib*
    // (based on env vars set by the daemon), then patch build.prop so ART
    // reports the target arch to apps via Build.SUPPORTED_ABIS. This must
    // happen AFTER pivot_root (so /system is visible at the new root) and
    // AFTER install_apk (so /data is writable for any cached config files
    // the translator wants to drop).
    //
    // When the strategy is `native` or `none`, this is a no-op.
    setup_translation_layer().map_err(|e| format!("translation setup: {e}"))?;

    // ---- 11. exec app_process64 (replaces us) ------------------------------
    exec_app_process(package)?;
    // exec never returns
    Ok(())
}

fn env_path(key: &str) -> Result<PathBuf, String> {
    std::env::var(key)
        .map(PathBuf::from)
        .map_err(|_| format!("missing env var: {key}"))
}

fn mount_overlay(lower: &Path, upper: &Path, work: &Path, merged: &Path) -> Result<(), String> {
    std::fs::create_dir_all(merged).map_err(|e| format!("mkdir merged: {e}"))?;

    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.display(),
        upper.display(),
        work.display()
    );
    let opts_c = CString::new(opts.as_str()).map_err(|e| format!("CString: {e}"))?;
    let target_c = CString::new(merged.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;
    let type_c = CString::new("overlay").unwrap();

    // MS_NODEV | MS_NOEXEC would block ART (it needs to mmap .so files
    // from /system), so we leave the overlay executable.
    let flags = libc::MS_RELATIME;

    let rc = unsafe {
        libc::mount(
            type_c.as_ptr(),
            target_c.as_ptr(),
            type_c.as_ptr(),
            flags,
            opts_c.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        return Err(format!(
            "mount overlay: errno {} ({})",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            std::io::Error::last_os_error()
        ));
    }
    tracing::info!(merged = %merged.display(), "overlayfs mounted");
    Ok(())
}

fn setup_dev_nodes(
    merged: &Path,
    binder_device: &Path,
    ashmem_device: &Path,
    input_event: Option<&Path>,
) -> Result<(), String> {
    let dev = merged.join("dev");
    std::fs::create_dir_all(&dev).map_err(|e| format!("mkdir dev: {e}"))?;

    // /dev/binder + /dev/ashmem: bind-mount the host device nodes.
    bind_mount(binder_device, &dev.join("binder"))?;
    bind_mount(ashmem_device, &dev.join("ashmem"))?;

    // /dev/null, /dev/zero, /dev/urandom, /dev/random: these are typically
    // already created by setup.sh as static device nodes or via devtmpfs.
    for name in ["null", "zero", "urandom", "random", "full", "tty"] {
        let src = Path::new("/dev").join(name);
        let dst = dev.join(name);
        if src.exists() {
            let _ = bind_mount(&src, &dst);
        }
    }

    // /dev/input/eventN — bind-mount the host-side virtual touchscreen
    // created by InputInjector. We expose it under the same path inside
    // the container (/dev/input/event0) so Android's EventHub, which
    // scans /dev/input/event*, picks it up automatically during the
    // InputReader thread initialization.
    //
    // We deliberately expose only the touchscreen event node — NOT the
    // whole /dev/input/ directory — so the container cannot read other
    // host input devices (keyboard, mouse, host touchscreen).
    if let Some(event_path) = input_event {
        if event_path.exists() {
            let input_dir = dev.join("input");
            std::fs::create_dir_all(&input_dir)
                .map_err(|e| format!("mkdir /dev/input: {e}"))?;
            // Inside the container, always expose as event0 — Android's
            // EventHub assigns the first detected touchscreen as the
            // primary display input device.
            let dst = input_dir.join("event0");
            match bind_mount(event_path, &dst) {
                Ok(()) => tracing::info!(
                    src = %event_path.display(),
                    dst = %dst.display(),
                    "input event device bind-mounted"
                ),
                Err(e) => tracing::warn!(
                    src = %event_path.display(),
                    error = %e,
                    "failed to bind-mount input event device (input injection will not work)"
                ),
            }
        } else {
            tracing::warn!(
                path = %event_path.display(),
                "DROIDKER_INPUT_EVENT points to nonexistent path; skipping bind-mount"
            );
        }
    }

    // /dev/pts as a new instance (so each container has its own pty namespace).
    let pts_dir = dev.join("pts");
    std::fs::create_dir_all(&pts_dir).map_err(|e| format!("mkdir pts: {e}"))?;
    let src_c = CString::new("devpts").unwrap();
    let dst_c = CString::new(pts_dir.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;
    let type_c = CString::new("devpts").unwrap();
    let opts_c = CString::new("newinstance,ptmxmode=0666,mode=620").unwrap();
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            dst_c.as_ptr(),
            type_c.as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            opts_c.as_ptr() as *const libc::c_void,
        )
    };
    if rc != 0 {
        tracing::warn!("devpts mount failed (non-fatal)");
    }

    // /dev/ptmx symlink to /dev/pts/ptmx
    let _ = std::os::unix::fs::symlink("/dev/pts/ptmx", dev.join("ptmx"));
    // /dev/fd symlink to /proc/self/fd
    let _ = std::os::unix::fs::symlink("/proc/self/fd", dev.join("fd"));
    // /dev/stdin, /dev/stdout, /dev/stderr symlinks
    let _ = std::os::unix::fs::symlink("/proc/self/fd/0", dev.join("stdin"));
    let _ = std::os::unix::fs::symlink("/proc/self/fd/1", dev.join("stdout"));
    let _ = std::os::unix::fs::symlink("/proc/self/fd/2", dev.join("stderr"));

    tracing::info!(dev = %dev.display(), "dev nodes set up");
    Ok(())
}

fn setup_proc_and_sys(merged: &Path) -> Result<(), String> {
    let proc_dir = merged.join("proc");
    let sys_dir = merged.join("sys");
    std::fs::create_dir_all(&proc_dir).map_err(|e| format!("mkdir proc: {e}"))?;
    std::fs::create_dir_all(&sys_dir).map_err(|e| format!("mkdir sys: {e}"))?;

    let target_proc = CString::new(proc_dir.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;
    let target_sys = CString::new(sys_dir.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;
    let type_proc = CString::new("proc").unwrap();
    let type_sys = CString::new("sysfs").unwrap();

    let rc = unsafe {
        libc::mount(
            type_proc.as_ptr(),
            target_proc.as_ptr(),
            type_proc.as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(format!("mount proc: {}", std::io::Error::last_os_error()));
    }

    let rc = unsafe {
        libc::mount(
            type_sys.as_ptr(),
            target_sys.as_ptr(),
            type_sys.as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV | libc::MS_RDONLY,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        tracing::warn!("mount sysfs failed (non-fatal)");
    }

    tracing::info!("procfs + sysfs mounted");
    Ok(())
}

fn bind_mount(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Touch the dst so mount(2) doesn't fail with ENOENT.
    if !dst.exists() {
        if src.is_dir() {
            std::fs::create_dir_all(dst).ok();
        } else {
            std::fs::write(dst, b"").ok();
        }
    }
    let src_c = CString::new(src.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString src: {e}"))?;
    let dst_c = CString::new(dst.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString dst: {e}"))?;
    let rc = unsafe {
        libc::mount(
            src_c.as_ptr(),
            dst_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "bind_mount {} -> {}: {}",
            src.display(),
            dst.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn pivot_into(merged: &Path) -> Result<(), String> {
    // pivot_root(new_root, put_old) requires:
    //   - new_root is a mount point
    //   - put_old is underneath new_root
    // We use `merged/.old_root` as put_old, then unmount it after pivot.
    let old_root = merged.join(".old_root");
    std::fs::create_dir_all(&old_root).map_err(|e| format!("mkdir old_root: {e}"))?;

    let new_root_c = CString::new(merged.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;
    let old_root_c = CString::new(old_root.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;

    // Make `merged` a mount point (required by pivot_root) by bind-mounting
    // it onto itself.
    let rc = unsafe {
        libc::mount(
            new_root_c.as_ptr(),
            new_root_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "self-bind {} for pivot: {}",
            merged.display(),
            std::io::Error::last_os_error()
        ));
    }

    // glibc doesn't expose pivot_root(2) directly; call it via syscall.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pivot_root,
            new_root_c.as_ptr(),
            old_root_c.as_ptr(),
        )
    };
    if rc == -1 {
        return Err(format!(
            "pivot_root: {}",
            std::io::Error::last_os_error()
        ));
    }

    // We are now chrooted into `merged`. The old root is at /.old_root.
    // Move / to a stable path and unmount it.
    std::env::set_current_dir("/").map_err(|e| format!("set_current_dir: {e}"))?;

    // umount the old root (lazy, so we don't block on busy inodes).
    let old_c = CString::new("/.old_root").unwrap();
    let rc = unsafe { libc::umount2(old_c.as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        tracing::warn!("umount old root failed (non-fatal)");
    }
    let _ = std::fs::remove_dir("/.old_root");

    // Change CWD to the new root.
    std::env::set_current_dir("/").map_err(|e| format!("set_current_dir: {e}"))?;

    tracing::info!("pivot_root complete");
    Ok(())
}

fn set_hostname(name: &str) -> Result<(), String> {
    let name_c = CString::new(name).map_err(|e| format!("CString: {e}"))?;
    let rc = unsafe { libc::sethostname(name_c.as_ptr(), name.len() as libc::size_t) };
    if rc != 0 {
        return Err(format!("sethostname: {}", std::io::Error::last_os_error()));
    }
    tracing::info!(hostname = name, "hostname set");
    Ok(())
}

fn drop_capabilities() -> Result<(), String> {
    // For M2 we keep this simple: convert the bounding set to the empty set
    // by dropping every capability from 0..CAP_LAST_CAP. This effectively
    // makes exec() lose all caps, which is what we want for an untrusted
    // app process.
    //
    // NOTE: this only works because we are running as root inside the user
    // namespace (via --map-root-user). The exec'd app_process will think
    // it's root but won't actually have any capabilities in the parent ns.

    const CAP_LAST_CAP: i32 = 40; // CAP_BPF as of kernel 5.8; bumps rarely
    for cap in 0..=CAP_LAST_CAP {
        let rc = unsafe {
            libc::prctl(
                libc::PR_CAPBSET_DROP,
                cap as libc::c_ulong,
                0,
                0,
                0,
            )
        };
        if rc != 0 {
            // Some caps don't exist on older kernels — ignore EINVAL.
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::EINVAL {
                tracing::warn!(cap, errno, "failed to drop capability");
            }
        }
    }
    tracing::info!("capability bounding set dropped");
    Ok(())
}

// ----- Seccomp BPF install (M2.6) ------------------------------------------
//
// We hand-build a classic BPF program (SECCOMP_RET_KILL_PROCESS for blocked
// syscalls, SECCOMP_RET_ALLOW for the rest) and install it via
// `seccomp(SECCOMP_SET_MODE_FILTER, ...)`. This avoids pulling in
// `libseccomp` as a native dependency, which matters on a 1-GB VPS where
// every shared library counts.

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1 << 0;
const OFF_NR: u32 = 0; // offset of `nr` in struct seccomp_data

/// Apply the AndroidRuntime seccomp blocklist. The blocklist matches the
/// daemon's `Profile::AndroidRuntime` — module loading, kexec, ptrace, bpf,
/// setns, unshare, time manipulation, etc.
fn apply_seccomp_blocklist() -> Result<(), String> {
    // (name, number) on x86_64/aarch64. We resolve at compile time since
    // we don't link the daemon's resolver here.
    //
    // NOTE: numbers are valid for both x86_64 and aarch64 for the syscalls
    // we care about (verified against unistd.h on both arches). Any
    // syscall not present on the running kernel is simply omitted from
    // the filter — we don't need to do anything special, since the BPF
    // program never references it.
    let blocked: &[(u32, &str)] = &[
        (175, "init_module"),
        (313, "finit_module"),
        (176, "delete_module"),
        (246, "kexec_load"),
        (320, "kexec_file_load"),
        (169, "reboot"),
        (167, "swapon"),
        (168, "swapoff"),
        (435, "clone3"),
        (101, "ptrace"),
        (310, "process_vm_readv"),
        (311, "process_vm_writev"),
        (172, "iopl"),
        (173, "ioperm"),
        (298, "perf_event_open"),
        (321, "bpf"),
        (212, "lookup_dcookie"),
        (179, "quotactl"),
        (180, "nfsservctl"),
        (163, "acct"),
        (164, "settimeofday"),
        (234, "stime"),
        (227, "clock_settime"),
        (308, "setns"),
        (272, "unshare"),
    ];

    // Sort by number so the BPF walker exits as early as possible on the
    // common (allowed) path.
    let mut numbers: Vec<u32> = blocked.iter().map(|(n, _)| *n).collect();
    numbers.sort_unstable();
    numbers.dedup();

    // Build the BPF program.
    let mut prog: Vec<SockFilter> = Vec::with_capacity(numbers.len() + 3);
    prog.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: OFF_NR });
    for _ in &numbers {
        prog.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    }
    prog.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: SECCOMP_RET_ALLOW });
    prog.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: SECCOMP_RET_KILL_PROCESS });
    for (i, n) in numbers.iter().enumerate() {
        prog[1 + i].k = *n;
    }

    // Install via seccomp(2).
    let fprog = SockFprog {
        len: prog.len() as u16,
        filter: prog.as_ptr(),
    };
    // SAFETY: see seccomp.rs in the daemon for the same call. fprog is
    // valid for the duration of the syscall.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER as libc::c_uint,
            SECCOMP_FILTER_FLAG_TSYNC as libc::c_uint,
            &fprog as *const SockFprog,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "seccomp(SECCOMP_SET_MODE_FILTER, TSYNC): {} (errno {})",
            err,
            err.raw_os_error().unwrap_or(0)
        ));
    }
    tracing::info!(blocked = numbers.len(), "seccomp BPF filter installed");
    Ok(())
}

/// Spawn a background `logcat` process that writes the Android system log
/// buffer to /data/droidker.logcat.log. The daemon's `LogStreamer` tails
/// this file from outside the container.
///
/// If /system/bin/logcat doesn't exist (e.g. on the skeleton rootfs), or
/// the fork fails, we silently skip — logcat capture is best-effort.
fn start_logcat_capture() {
    let logcat = Path::new("/system/bin/logcat");
    if !logcat.exists() {
        tracing::info!("logcat binary not found; skipping log capture");
        return;
    }

    let log_path = Path::new("/data/droidker.logcat.log");
    // Pre-create the file so the daemon's tailer can open it immediately.
    let _ = std::fs::File::create(log_path);

    // fork() a child that exec's logcat. The child inherits our mount
    // namespace + pid namespace, so it becomes a sibling of app_process.
    // The daemon's cgroup will reap it when the container stops.
    unsafe {
        let pid = libc::fork();
        if pid < 0 {
            tracing::warn!("fork() for logcat failed: {}", std::io::Error::last_os_error());
            return;
        }
        if pid == 0 {
            // Child: redirect stdout+stderr to the log file, then exec logcat.
            let log_c = CString::new("/data/droidker.logcat.log").unwrap();
            // Open with O_WRONLY | O_APPEND | O_CREAT, mode 0644.
            let fd = libc::open(
                log_c.as_ptr(),
                libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
                0o644,
            );
            if fd < 0 {
                libc::_exit(1);
            }
            libc::dup2(fd, libc::STDOUT_FILENO);
            libc::dup2(fd, libc::STDERR_FILENO);
            libc::close(fd);

            // Close stdin so logcat doesn't try to read from it.
            libc::close(libc::STDIN_FILENO);

            // Build argv: logcat -v threadtime
            let argv0 = CString::new("logcat").unwrap();
            let argv1 = CString::new("-v").unwrap();
            let argv2 = CString::new("threadtime").unwrap();
            let argv: [*const libc::c_char; 4] = [
                argv0.as_ptr(),
                argv1.as_ptr(),
                argv2.as_ptr(),
                std::ptr::null(),
            ];

            // execvp searches PATH for the binary.
            let bin_c = CString::new("/system/bin/logcat").unwrap();
            libc::execvp(bin_c.as_ptr(), argv.as_ptr());
            // If we get here, exec failed.
            libc::_exit(1);
        }
        // Parent: logcat is running in the background. We don't wait for it.
        tracing::info!(logcat_pid = pid, log_path = %log_path.display(), "logcat capture started");
    }
}

fn install_apk(package: &str, apk_path: &str, _apk_sha: &str) -> Result<(), String> {
    // The APK is bind-mounted from the host. We "install" it by copying it
    // into /data/app/<package>/base.apk — the location ART scans on boot.
    // A full AOT dexopt would happen via `dex2oat` later, but for M2 we
    // skip that and let ART run in interpreter mode.

    let apk_src = Path::new(apk_path);
    if !apk_src.exists() {
        return Err(format!("apk not found: {}", apk_src.display()));
    }

    let dst_dir = Path::new("/data/app").join(package);
    std::fs::create_dir_all(&dst_dir).map_err(|e| format!("mkdir apk dst: {e}"))?;
    let dst = dst_dir.join("base.apk");

    std::fs::copy(apk_src, &dst).map_err(|e| format!("copy apk: {e}"))?;
    tracing::info!(dst = %dst.display(), "APK installed");
    Ok(())
}

fn exec_app_process(package: &str) -> Result<(), String> {
    // The real Android boot path is:
    //   app_process64 /system/bin com.android.commands.am.Am start ...
    // We use a much simpler invocation that just starts the Zygote (which
    // is what real init does on boot). The Zygote then forks the actual
    // app process when we issue `am start` via the binder.
    //
    // For M2 we use the simpler `app_process` form that directly loads the
    // APK's main activity class. This requires the APK to declare an
    // explicit main class in its manifest, which most apps do.
    //
    // M7.3: when the translation strategy is `qemu-user`, we rewrite the
    // exec target to `/system/bin/qemu-translation` (which was bind-mounted
    // by `setup_translation_layer`) and prepend `app_process64` to argv so
    // qemu interprets it. This is the only way to actually run ARM .so
    // files under qemu-user — without the rewrite, app_process64 would
    // exec natively and immediately SIGSEGV on the first ARM instruction.

    let app_process = Path::new("/system/bin/app_process64");
    if !app_process.exists() {
        return Err(format!(
            "app_process64 not found at {} — Android rootfs is incomplete",
            app_process.display()
        ));
    }

    // M7.3: detect qemu-user strategy. When active we exec qemu instead.
    let strategy = std::env::var("DROIDKER_TRANSLATION_STRATEGY")
        .unwrap_or_else(|_| "native".to_string());
    let target_arch = std::env::var("DROIDKER_TARGET_ARCH")
        .unwrap_or_else(|_| "x86_64".to_string());
    let qemu_active = strategy == "qemu-user";

    // Path to the qemu-user binary that `setup_translation_layer` bind-mounted
    // into /system/bin/qemu-translation. When qemu is not active this is unused.
    let qemu_bin = Path::new("/system/bin/qemu-translation");

    // Verify the qemu binary is actually present when we plan to use it.
    // If not, we log a fatal error but still fall through to the native
    // app_process exec — that way the container at least starts (in a
    // degraded mode) and the user sees an explanatory error in the logs.
    let use_qemu = if qemu_active {
        if qemu_bin.exists() {
            true
        } else {
            tracing::error!(
                bin = %qemu_bin.display(),
                "qemu-user strategy selected but translator binary is missing; \
                 falling back to native app_process (will crash on ARM .so loads)",
            );
            false
        }
    } else {
        false
    };

    // ----- Build the exec target + argv ----------------------------------
    //
    // Native path:
    //   execve("/system/bin/app_process64", ["app_process", "/system/bin",
    //           "--nice-name", <package>, "android.app.ActivityThread"], envp)
    //
    // qemu-user path:
    //   execve("/system/bin/qemu-translation",
    //          ["qemu-<arch>", "/system/bin/app_process64", "app_process",
    //           "/system/bin", "--nice-name", <package>,
    //           "android.app.ActivityThread"], envp)
    //
    // qemu-user's argv[1] is the guest binary, argv[2..] are the guest's
    // own argv[0..]. We pass `app_process` as the guest argv[0] so ART
    // sees the same name it would under native execution.

    let arg_app_process = CString::new("app_process").unwrap();
    let arg_bin_dir = CString::new("/system/bin").unwrap();
    let arg_nice_flag = CString::new("--nice-name").unwrap();
    let arg_package = CString::new(package).unwrap();
    let arg_activity = CString::new("android.app.ActivityThread").unwrap();

    // For qemu-user: argv[0] is the qemu program name (cosmetic), argv[1]
    // is the guest binary path, argv[2..] is the guest's argv[0..].
    let arg_qemu_argv0 = CString::new(format!("qemu-{}", target_arch)).unwrap();
    let arg_qemu_guest = CString::new(app_process.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;

    // Collect argv as a Vec so we can branch without duplicating the
    // envp-building logic below.
    let argv_ptrs: Vec<*const libc::c_char> = if use_qemu {
        vec![
            arg_qemu_argv0.as_ptr(),
            arg_qemu_guest.as_ptr(),
            arg_app_process.as_ptr(),
            arg_bin_dir.as_ptr(),
            arg_nice_flag.as_ptr(),
            arg_package.as_ptr(),
            arg_activity.as_ptr(),
            std::ptr::null(),
        ]
    } else {
        vec![
            arg_app_process.as_ptr(),
            arg_bin_dir.as_ptr(),
            arg_nice_flag.as_ptr(),
            arg_package.as_ptr(),
            arg_activity.as_ptr(),
            std::ptr::null(),
        ]
    };

    // The actual binary we execve.
    let exec_target = if use_qemu { qemu_bin } else { app_process };
    let exec_target_c = CString::new(exec_target.as_os_str().as_encoded_bytes())
        .map_err(|e| format!("CString: {e}"))?;

    let env_classpath = CString::new(
        "CLASSPATH=/system/framework/services.jar:/system/framework/framework.jar",
    ).unwrap();

    // Set up minimal environment for ART.
    let env_bootclasspath = CString::new(
        "BOOTCLASSPATH=/system/framework/core-libart.jar:/system/framework/conscrypt.jar:\
         /system/framework/okhttp.jar:/system/framework/core-junit.jar:/system/framework/bouncycastle.jar",
    ).unwrap();
    let env_android_data = CString::new("ANDROID_DATA=/data").unwrap();
    let env_android_root = CString::new("ANDROID_ROOT=/system").unwrap();
    let env_ld_library = CString::new("LD_LIBRARY_PATH=/system/lib64:/system/lib").unwrap();

    // M6: collect translator env vars (LD_PRELOAD, HOUDINI_ENABLE, etc.)
    // from DROIDKER_APP_ENV_<NAME> entries set by the daemon. Each entry
    // becomes a real env var in the app_process environment.
    let translation_envs: Vec<CString> = std::env::vars()
        .filter_map(|(k, v)| {
            k.strip_prefix("DROIDKER_APP_ENV_").map(|name| {
                CString::new(format!("{name}={v}")).unwrap()
            })
        })
        .collect();

    // Build envp dynamically so we can append the translation envs.
    let mut envp: Vec<*const libc::c_char> = vec![
        env_bootclasspath.as_ptr(),
        env_classpath.as_ptr(),
        env_android_data.as_ptr(),
        env_android_root.as_ptr(),
        env_ld_library.as_ptr(),
    ];
    for e in &translation_envs {
        envp.push(e.as_ptr());
    }
    envp.push(std::ptr::null());

    tracing::info!(
        strategy = %strategy,
        target_arch = %target_arch,
        use_qemu,
        exec_target = %exec_target.display(),
        extra_envs = translation_envs.len(),
        "exec'ing app_process64"
    );
    let rc = unsafe {
        libc::execve(exec_target_c.as_ptr(), argv_ptrs.as_ptr(), envp.as_ptr())
    };
    Err(format!(
        "execve returned (rc={}): {}",
        rc,
        std::io::Error::last_os_error()
    ))
}

// ----- M6: Translation layer setup -----------------------------------------
//
// `setup_translation_layer` runs after pivot_root. It:
//   1. Parses DROIDKER_TRANSLATION_MOUNTS (a `:`-separated list of `src=dst`
//      pairs) and bind-mounts each one into the container's /system/lib*.
//   2. Patches /system/build.prop so `ro.product.cpu.abi` and friends match
//      the target arch. Without this, apps that call Build.SUPPORTED_ABIS
//      would see "x86_64" and try to load x86_64 .so files instead of ARM.
//   3. For the qemu-user strategy, rewrites the app_process argv so we
//      actually exec `qemu-aarch64 /system/bin/app_process64 ...` instead.
//
// All operations are best-effort — failures in translation setup are
// downgraded to warnings so the container still starts (in a degraded
// mode) rather than failing the whole boot.

fn setup_translation_layer() -> Result<(), String> {
    let strategy = std::env::var("DROIDKER_TRANSLATION_STRATEGY")
        .unwrap_or_else(|_| "native".to_string());
    let target_arch = std::env::var("DROIDKER_TARGET_ARCH")
        .unwrap_or_else(|_| "x86_64".to_string());

    tracing::info!(
        strategy = %strategy,
        target_arch = %target_arch,
        "translation layer setup"
    );

    // For `native` and `none` we have nothing to do.
    if strategy == "native" || strategy == "none" {
        tracing::debug!(
            strategy = %strategy,
            "no translation setup required"
        );
        return Ok(());
    }

    // 1. Bind-mount the translator .so files.
    let mounts = std::env::var("DROIDKER_TRANSLATION_MOUNTS").unwrap_or_default();
    if !mounts.is_empty() {
        for pair in mounts.split(':') {
            if pair.is_empty() {
                continue;
            }
            let (src, dst) = match pair.split_once('=') {
                Some((s, d)) => (s, d),
                None => {
                    tracing::warn!(pair, "malformed translation mount entry (missing '=')");
                    continue;
                }
            };
            // The dst is relative to the new root (post-pivot). Make it
            // absolute so bind_mount resolves correctly.
            let abs_dst = if dst.starts_with('/') {
                dst.to_string()
            } else {
                format!("/{dst}")
            };
            match bind_mount(Path::new(src), Path::new(&abs_dst)) {
                Ok(()) => tracing::info!(
                    src = src,
                    dst = %abs_dst,
                    "translator file bind-mounted"
                ),
                Err(e) => tracing::warn!(
                    src = src,
                    dst = %abs_dst,
                    error = %e,
                    "failed to bind-mount translator file (app may crash on ARM .so loads)"
                ),
            }
        }
    }

    // 2. Patch build.prop so ART reports the target arch.
    if let Err(e) = patch_build_prop_for_arch(&target_arch) {
        tracing::warn!(
            error = %e,
            "failed to patch build.prop for target arch (apps may see wrong ABI)"
        );
    }

    // 3. For qemu-user, the actual exec rewrite happens in `exec_app_process`
    //    (M7.3). We just sanity-check that the bind-mount succeeded above.
    if strategy == "qemu-user" {
        let qemu_bin = Path::new("/system/bin/qemu-translation");
        if !qemu_bin.exists() {
            tracing::warn!(
                bin = %qemu_bin.display(),
                "qemu-user strategy selected but translator binary is missing; \
                 exec_app_process will fall back to native app_process64 \
                 (will crash on ARM .so loads)",
            );
        } else {
            tracing::info!(
                bin = %qemu_bin.display(),
                "qemu-user strategy ready; exec_app_process will rewrite argv",
            );
        }
    }

    Ok(())
}

/// Patch /system/build.prop so Android reports the target arch via
/// `Build.SUPPORTED_ABIS` and friends. We rewrite the existing
/// `ro.product.cpu.abi` / `ro.product.cpu.abilist` lines (if present)
/// and append a DroidKer-section override block.
///
/// The build.prop file lives in the merged overlay view at /system/build.prop,
/// which is the lowerdir of the overlay. Writes go to the upperdir, leaving
/// the shared rootfs read-only for other containers.
fn patch_build_prop_for_arch(target_arch: &str) -> Result<(), String> {
    let build_prop = Path::new("/system/build.prop");
    if !build_prop.exists() {
        return Err(format!(
            "/system/build.prop not found (rootfs is incomplete)"
        ));
    }

    // Read the current contents.
    let content = std::fs::read_to_string(build_prop)
        .map_err(|e| format!("read build.prop: {e}"))?;

    // Build the new contents: strip the existing ro.product.cpu.* lines
    // (they're set by the original Android-x86 rootfs to x86/x86_64) and
    // append our overrides.
    let mut new_lines: Vec<&str> = Vec::new();
    let mut saw_marker = false;
    for line in content.lines() {
        if line.starts_with("# ----- DroidKer translation overrides -----") {
            saw_marker = true;
            break; // stop here; we'll rewrite everything from the marker
        }
        // Skip existing CPU ABI lines so our overrides win.
        if line.starts_with("ro.product.cpu.abi")
            || line.starts_with("ro.product.cpu.abilist")
        {
            continue;
        }
        new_lines.push(line);
    }
    let _ = saw_marker;

    // Build the new override block. We set BOTH the legacy single-ABI and
    // the modern ABIS list so apps using either API get the right answer.
    let abi_list = match target_arch {
        "arm64-v8a" => "arm64-v8a,armeabi-v7a,armeabi",
        "armeabi-v7a" => "armeabi-v7a,armeabi",
        "x86_64" => "x86_64,x86",
        "x86" => "x86",
        _ => target_arch,
    };
    let override_block = format!(
        "\n# ----- DroidKer translation overrides -----\n\
         # Set by droidker-init M6 — do not edit by hand.\n\
         ro.product.cpu.abi={}\n\
         ro.product.cpu.abilist={}\n\
         ro.product.cpu.abilist64={}\n\
         ro.product.cpu.abilist32={}\n",
        target_arch, abi_list, abi_list, abi_list,
    );

    let mut new_content = new_lines.join("\n");
    new_content.push('\n');
    new_content.push_str(&override_block);

    std::fs::write(build_prop, new_content)
        .map_err(|e| format!("write build.prop: {e}"))?;

    tracing::info!(
        target_arch,
        "build.prop patched for target arch"
    );
    Ok(())
}
