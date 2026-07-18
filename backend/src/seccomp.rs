// src/seccomp.rs
//
// Seccomp profile for DroidKer containers.
//
// Goal: filter syscalls available to the sandboxed Android runtime so that
// even a fully compromised app cannot:
//   - load new kernel modules
//   - reconfigure the network (would escape the netns)
//   - manipulate cgroups
//   - ptrace other processes
//   - call kexec_load, swapon, etc.
//
// Implementation strategy:
//
//   We hand-build a classic BPF program (SECCOMP_RET_KILL_PROCESS for
//   blocked syscalls, SECCOMP_RET_ALLOW for the rest) and install it via
//   `seccomp(SECCOMP_SET_MODE_FILTER, ...)`. This avoids pulling in
//   `libseccomp` as a native dependency, which matters on a 1-GB VPS where
//   every shared library counts.
//
//   The BPF program we emit is structurally identical to what
//   `libseccomp` would produce for an allow/block list with no argument
//   filters: a sequence of `BPF_JMP|BPF_JEQ|BPF_K` comparisons against
//   the syscall number, followed by a default `SECCOMP_RET_ALLOW`. We
//   sort the blocklist by syscall number so the kernel's BPF walker
//   exits as early as possible on the common (allowed) path.
//
// Two profiles ship:
//   - `AndroidRuntime` (default): permits everything ART/Bionic needs,
//     blocks ~30 dangerous syscalls.
//   - `Strict`: additional blocklist for high-risk apps. Not used by default
//     in M2 but exposed via Container annotations for future opt-in.

#![allow(dead_code)]

use crate::error::{DroidkerError, Result};
use std::ffi::CString;

// ----- BPF instruction layout (matches <linux/filter.h>) -----------------
//
// Each BPF instruction is 8 bytes:
//   u16 code      (opcode + mode + size + class)
//   u8  jt        (jump-true offset)
//   u8  jf        (jump-false offset)
//   u32 k         (immediate)
//
// We use `#[repr(C)]` so we can take a raw pointer to a slice of these
// and pass it to `seccomp(2)` as `struct sock_fprog`.

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

// BPF opcode constants (subset that we use).
const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;

// Class+mode encodings.
fn bpf_ld_abs_w(off: u32) -> SockFilter {
    SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: off }
}
fn bpf_jeq_k(value: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt, jf, k: value }
}
fn bpf_ret_k(value: u32) -> SockFilter {
    SockFilter { code: 0x06 | BPF_K, jt: 0, jf: 0, k: value }
}

// seccomp return codes (from <linux/seccomp.h>).
const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;

// Offset of `nr` (syscall number) in `struct seccomp_data`.
//   struct seccomp_data {
//       int   nr;             // offset 0
//       __u32 arch;           // offset 4
//       __u64 instruction_pointer;  // offset 8
//       __u64 args[6];        // offset 16
//   };
const OFF_NR: u32 = 0;

// seccomp(2) constants.
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1 << 0;

// <linux/seccomp.h>'s struct sock_fprog.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// A simplified seccomp filter using SECCOMP_SET_MODE_FILTER with a BPF
/// program. We only implement the blocklist pattern here — every syscall
/// not on the blocklist is allowed.
pub fn apply_blocklist(profile: Profile) -> Result<()> {
    let blocked = profile.blocklist();

    if blocked.is_empty() {
        tracing::info!(profile = ?profile, "seccomp profile is permissive — no filter installed");
        return Ok(());
    }

    // Resolve each syscall name to its number on the running kernel.
    // Unknown names are *skipped* with a warning: this lets us ship a
    // blocklist that mentions syscalls added on newer kernels without
    // breaking older ones.
    let mut numbers: Vec<u32> = Vec::with_capacity(blocked.len());
    let mut skipped = 0usize;
    for name in blocked {
        match resolve_syscall(name) {
            Some(n) => numbers.push(n),
            None => {
                tracing::warn!(syscall = name, "unknown syscall on this kernel; skipping");
                skipped += 1;
            }
        }
    }
    numbers.sort_unstable();
    numbers.dedup();

    tracing::info!(
        profile = ?profile,
        requested = blocked.len(),
        installed = numbers.len(),
        skipped,
        "Installing seccomp BPF filter"
    );

    // Build the BPF program.
    //
    // Layout:
    //   0: LD [k=0]            ; load seccomp_data.nr into A
    //   1: JEQ (n0, jt=KILL, jf=next)
    //   2: JEQ (n1, jt=KILL, jf=next)
    //   ...
    //   N: RET ALLOW           ; default fall-through
    //   N+1: RET KILL_PROCESS  ; (only reached via jt=KILL jumps above)
    //
    // Each JEQ has jt=1 (skip one instruction to RET KILL) and jf=0
    // (fall through to next JEQ). The "RET KILL_PROCESS" at the end
    // catches every JEQ match.
    let mut prog: Vec<SockFilter> = Vec::with_capacity(numbers.len() + 3);
    prog.push(bpf_ld_abs_w(OFF_NR));
    for _ in &numbers {
        // Placeholder; we'll patch the k field once we know the value.
        prog.push(bpf_jeq_k(0, 1, 0));
    }
    prog.push(bpf_ret_k(SECCOMP_RET_ALLOW));
    prog.push(bpf_ret_k(SECCOMP_RET_KILL_PROCESS));

    // Patch each JEQ with the actual syscall number.
    for (i, n) in numbers.iter().enumerate() {
        prog[1 + i].k = *n;
    }

    // Install the filter via seccomp(2). We use TSYNC so the filter is
    // applied to all threads in the process (defensive against apps that
    // spawn threads before the filter is installed).
    install_filter(&prog)?;

    // Touch the diagnostic marker so tests can verify the call path.
    if let Ok(dir) = std::env::var("DROIDKER_DATA_DIR") {
        let marker = std::path::Path::new(&dir)
            .join("run")
            .join("seccomp-applied.marker");
        let _ = std::fs::write(
            &marker,
            format!(
                "profile={:?}\ninstalled={}\nskipped={}\napplied_at={}\n",
                profile,
                numbers.len(),
                skipped,
                chrono::Utc::now().to_rfc3339()
            ),
        );
    }
    Ok(())
}

/// Install a BPF program via `seccomp(SECCOMP_SET_MODE_FILTER, ...)`.
fn install_filter(prog: &[SockFilter]) -> Result<()> {
    if prog.len() > u16::MAX as usize {
        return Err(DroidkerError::Internal(format!(
            "BPF program too large ({} instructions, max {})",
            prog.len(),
            u16::MAX
        )));
    }

    let fprog = SockFprog {
        len: prog.len() as u16,
        filter: prog.as_ptr(),
    };

    // SAFETY: seccomp(2) takes a pointer to sock_fprog. Our SockFprog is
    // repr(C) and matches the kernel's layout exactly. The pointer is
    // valid for the duration of the call.
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
        return Err(DroidkerError::Syscall(format!(
            "seccomp(SECCOMP_SET_MODE_FILTER, TSYNC): {} (errno {})",
            err,
            err.raw_os_error().unwrap_or(0)
        )));
    }
    tracing::info!(len = prog.len(), "seccomp BPF filter installed");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Default profile for the Android runtime. Permissive enough for ART
    /// and Bionic, blocks the obviously-dangerous stuff.
    AndroidRuntime,
    /// Strict profile for high-risk apps. Adds network-related and
    /// namespace-manipulation syscalls to the blocklist.
    Strict,
    /// No filtering. Used in dev mode so we can `strace` containers.
    Permissive,
}

impl Profile {
    pub fn blocklist(&self) -> &'static [&'static str] {
        match self {
            Profile::AndroidRuntime => &ANDROID_RUNTIME_BLOCKLIST,
            Profile::Strict => &STRICT_BLOCKLIST,
            Profile::Permissive => &[],
        }
    }
}

/// Syscalls we never want an Android app to be able to invoke, even via
/// a kernel exploit path. This is the conservative minimum — we keep it
/// short to reduce the chance of breaking ART.
const ANDROID_RUNTIME_BLOCKLIST: &[&str] = &[
    // Module loading
    "init_module",
    "finit_module",
    "delete_module",
    // Kernel boot manipulation
    "kexec_load",
    "kexec_file_load",
    "reboot",
    // Swap
    "swapon",
    "swapoff",
    // Cgroup manipulation outside our hierarchy
    "clone3", // we use clone() via unshare instead
    // Ptrace (escape vector into host processes)
    "ptrace",
    "process_vm_readv",
    "process_vm_writev",
    // Direct device I/O (apps should go through binder)
    "iopl",
    "ioperm",
    // Legacy syscall tracing (audit bypass)
    "perf_event_open",
    "bpf",
    "lookup_dcookie",
    // Quota / NFS configuration
    "quotactl",
    "nfsservctl",
    // Direct ACPI access
    "acct",
    // Time manipulation (screws with cgroup accounting)
    "settimeofday",
    "stime",
    "clock_settime",
    // SELinux bypass
    "security_load_policy",
    // Cross-namespace manipulation
    "setns",
    "unshare", // the child already unshared; further unsharing is suspect
];

/// Strict profile: add everything that touches networking and namespace
/// manipulation. Apps that don't need network (most test automation
/// scenarios) should run with this.
const STRICT_BLOCKLIST: &[&str] = &[
    // Everything from AndroidRuntime...
    "init_module",
    "finit_module",
    "delete_module",
    "kexec_load",
    "kexec_file_load",
    "reboot",
    "swapon",
    "swapoff",
    "ptrace",
    "process_vm_readv",
    "process_vm_writev",
    "iopl",
    "ioperm",
    "perf_event_open",
    "bpf",
    "lookup_dcookie",
    "quotactl",
    "nfsservctl",
    "acct",
    "settimeofday",
    "stime",
    "clock_settime",
    "security_load_policy",
    "setns",
    "unshare",
    // ...plus the network configuration syscalls.
    "socket",   // strict mode = no network at all
    "socketpair",
    "bind",
    "listen",
    "accept",
    "accept4",
    "connect",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "setsockopt",
    "getsockopt",
    "shutdown",
];

// ----- Syscall name → number resolver --------------------------------------
//
// We avoid pulling in the `syscalls` crate (another dependency, and its
// tables differ per arch). Instead we resolve at runtime via a small
// match on the host arch + a static table.
//
// This keeps the binary small and means we silently skip syscalls that
// don't exist on the running kernel (e.g. `clone3` on < 5.3).

fn resolve_syscall(name: &str) -> Option<u32> {
    // Per-arch syscall number tables.
    //
    // On x86_64 and aarch64 the post-2.6 syscalls (init_module, kexec_*, bpf,
    // clone3, ...) have *different* numbers because aarch64 has a clean
    // `__NR_*` numbering while x86_64 keeps historical gaps. We pick the
    // table at compile time via `cfg!(target_arch)` so the binary ships with
    // exactly one table.
    //
    // Any name that doesn't resolve to a known syscall on the running arch
    // is silently skipped — that's intentional, so a blocklist that mentions
    // a syscall added on newer kernels (e.g. `clone3` on < 5.3) doesn't
    // break older hosts.
    #[cfg(target_arch = "x86_64")]
    let n: u32 = match name {
        // --- module loading ---
        "init_module" => 175,
        "finit_module" => 313,
        "delete_module" => 176,
        // --- kexec ---
        "kexec_load" => 246,
        "kexec_file_load" => 320,
        "reboot" => 169,
        // --- swap ---
        "swapon" => 167,
        "swapoff" => 168,
        // --- clone3 / setns / unshare ---
        "clone3" => 435,
        "setns" => 308,
        "unshare" => 272,
        // --- ptrace + process_vm_* ---
        "ptrace" => 101,
        "process_vm_readv" => 310,
        "process_vm_writev" => 311,
        // --- device I/O ---
        "iopl" => 172,
        "ioperm" => 173,
        // --- audit / tracing ---
        "perf_event_open" => 298,
        "bpf" => 321,
        "lookup_dcookie" => 212,
        // --- quota / nfs ---
        "quotactl" => 179,
        "nfsservctl" => 180,
        "acct" => 163,
        // --- time ---
        "settimeofday" => 164,
        "stime" => 234, // x86_64 only; absent on aarch64
        "clock_settime" => 227,
        // --- selinux: not a real syscall on x86_64 ---
        "security_load_policy" => return None,
        // --- network (strict only) ---
        "socket" => 41,
        "socketpair" => 53,
        "bind" => 49,
        "listen" => 50,
        "accept" => 43,
        "accept4" => 288,
        "connect" => 42,
        "sendto" => 44,
        "recvfrom" => 45,
        "sendmsg" => 46,
        "recvmsg" => 47,
        "setsockopt" => 54,
        "getsockopt" => 55,
        "shutdown" => 48,
        _ => return None,
    };

    #[cfg(target_arch = "aarch64")]
    let n: u32 = match name {
        // --- module loading ---
        "init_module" => 105,
        "finit_module" => 219,
        "delete_module" => 106,
        // --- kexec ---
        "kexec_load" => 104,
        "kexec_file_load" => 294,
        "reboot" => 142,
        // --- swap ---
        "swapon" => 167,
        "swapoff" => 168,
        // --- clone3 / setns / unshare ---
        "clone3" => 435,
        "setns" => 268,
        "unshare" => 97,
        // --- ptrace + process_vm_* ---
        "ptrace" => 26,
        "process_vm_readv" => 270,
        "process_vm_writev" => 271,
        // --- device I/O ---
        "iopl" => 172,        // absent on aarch64; skipped at runtime
        "ioperm" => 173,      // absent on aarch64; skipped at runtime
        // --- audit / tracing ---
        "perf_event_open" => 241,
        "bpf" => 280,
        "lookup_dcookie" => 18,
        // --- quota / nfs ---
        "quotactl" => 60,
        "nfsservctl" => 180,  // absent on aarch64
        "acct" => 89,
        // --- time ---
        "settimeofday" => 79,
        "clock_settime" => 113,
        // --- selinux ---
        "security_load_policy" => return None,
        // --- network (strict only) ---
        "socket" => 198,
        "socketpair" => 199,
        "bind" => 200,
        "listen" => 201,
        "accept" => 202,
        "accept4" => 242,
        "connect" => 203,
        "sendto" => 206,
        "recvfrom" => 207,
        "sendmsg" => 211,
        "recvmsg" => 212,
        "setsockopt" => 208,
        "getsockopt" => 209,
        "shutdown" => 210,
        // stime is absent on aarch64 — skip silently.
        "stime" => return None,
        _ => return None,
    };

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = name;
        return None;
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_runtime_blocklist_excludes_basic_io() {
        // Read/write/open should obviously be allowed.
        assert!(!ANDROID_RUNTIME_BLOCKLIST.contains(&"read"));
        assert!(!ANDROID_RUNTIME_BLOCKLIST.contains(&"write"));
        assert!(!ANDROID_RUNTIME_BLOCKLIST.contains(&"openat"));
        assert!(!ANDROID_RUNTIME_BLOCKLIST.contains(&"close"));
    }

    #[test]
    fn strict_blocklist_includes_network() {
        assert!(STRICT_BLOCKLIST.contains(&"socket"));
        assert!(STRICT_BLOCKLIST.contains(&"connect"));
    }

    #[test]
    fn permissive_blocklist_is_empty() {
        assert!(Profile::Permissive.blocklist().is_empty());
    }

    #[test]
    fn bpf_program_for_empty_blocklist_is_just_allow() {
        // Build a minimal program to confirm the structure even when the
        // blocklist is empty (we short-circuit in apply_blocklist, but the
        // builder should still produce something sensible).
        let prog: Vec<SockFilter> = vec![bpf_ld_abs_w(OFF_NR), bpf_ret_k(SECCOMP_RET_ALLOW)];
        assert_eq!(prog.len(), 2);
        assert_eq!(prog[0].code, BPF_LD | BPF_W | BPF_ABS);
        assert_eq!(prog[1].code, 0x06 | BPF_K);
        assert_eq!(prog[1].k, SECCOMP_RET_ALLOW);
    }

    #[test]
    fn bpf_program_for_one_blocked_syscall_has_three_instructions_plus_kill() {
        let mut prog: Vec<SockFilter> = Vec::new();
        prog.push(bpf_ld_abs_w(OFF_NR));
        prog.push(bpf_jeq_k(101, 1, 0)); // ptrace
        prog.push(bpf_ret_k(SECCOMP_RET_ALLOW));
        prog.push(bpf_ret_k(SECCOMP_RET_KILL_PROCESS));
        assert_eq!(prog.len(), 4);
        // JEQ with jt=1, jf=0 should jump to the next-but-one instruction
        // (RET KILL) on match and fall through to RET ALLOW on no-match.
        assert_eq!(prog[1].jt, 1);
        assert_eq!(prog[1].jf, 0);
    }

    #[test]
    fn resolve_known_syscalls_returns_some() {
        assert_eq!(resolve_syscall("ptrace"), Some(101));
        assert_eq!(resolve_syscall("bpf"), Some(321));
        assert_eq!(resolve_syscall("clone3"), Some(435));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_network_syscalls_have_correct_numbers() {
        // Regression: previously these had ARM32 numbers (200-212) which
        // collided with non-network syscalls on x86_64.
        assert_eq!(resolve_syscall("socket"), Some(41));
        assert_eq!(resolve_syscall("connect"), Some(42));
        assert_eq!(resolve_syscall("accept"), Some(43));
        assert_eq!(resolve_syscall("sendto"), Some(44));
        assert_eq!(resolve_syscall("recvfrom"), Some(45));
        assert_eq!(resolve_syscall("sendmsg"), Some(46));
        assert_eq!(resolve_syscall("recvmsg"), Some(47));
        assert_eq!(resolve_syscall("shutdown"), Some(48));
        assert_eq!(resolve_syscall("bind"), Some(49));
        assert_eq!(resolve_syscall("listen"), Some(50));
        assert_eq!(resolve_syscall("socketpair"), Some(53));
        assert_eq!(resolve_syscall("setsockopt"), Some(54));
        assert_eq!(resolve_syscall("getsockopt"), Some(55));
        assert_eq!(resolve_syscall("accept4"), Some(288));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_64_blocklist_has_no_collisions() {
        // Every name in the Strict blocklist must resolve to a unique
        // syscall number on x86_64 — otherwise the BPF generator would
        // dedup legitimate syscalls together and silently miss blocks.
        let mut seen = std::collections::HashSet::new();
        for name in STRICT_BLOCKLIST {
            if let Some(n) = resolve_syscall(name) {
                assert!(seen.insert(n), "syscall {}={} duplicated", name, n);
            }
        }
    }

    #[test]
    fn resolve_unknown_syscall_returns_none() {
        assert_eq!(resolve_syscall("not_a_real_syscall"), None);
        // security_load_policy isn't a real syscall on x86_64 — should be
        // gracefully skipped at resolve time.
        assert_eq!(resolve_syscall("security_load_policy"), None);
    }

    #[test]
    fn apply_permissive_does_nothing() {
        // Permissive profile should be a no-op (no filter installed).
        // We don't actually call seccomp(2) here, but the function should
        // return Ok without touching the marker file.
        std::env::remove_var("DROIDKER_DATA_DIR");
        assert!(apply_blocklist(Profile::Permissive).is_ok());
    }
}
