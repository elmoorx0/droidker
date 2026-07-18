// src/container/translation.rs
//
// ARM → x86_64 translation strategy (M6).
//
// Most Android APKs ship native `.so` libraries compiled for ARM (armeabi-v7a
// or arm64-v8a). On an x86_64 host those won't load directly into ART — we
// need a binary-translation layer that intercepts `dlopen("libfoo.so")` and
// JIT-translates the ARM instructions to x86_64 as they execute.
//
// Three strategies, in order of preference:
//
//   1. **libhoudini** — closed-source, shipped with Android-x86 and Intel's
//      "Android on Intel" builds. ~2× native ARM speed, supports both ARMv7
//      and ARMv8. Located at `/system/lib/libhoudini.so` and
//      `/system/lib64/libhoudini.so` on a real Android-x86 system image.
//
//   2. **libndk_translation** — Google's open-source ARM translator from
//      AOSP 12 (used by "Google Play Games on PC"). Slower than libhoudini
//      but freely redistributable. Two parts:
//        libndk_translation.so  — JNI bridge (NDK apps)
//        libndk_translation_gl.so — GL→TLS bridge
//
//   3. **qemu-user** — last-resort fallback. Slowest but works everywhere.
//      We invoke `qemu-aarch64` / `qemu-arm` as the runtime interpreter and
//      pass app_process64 as its argv[1]. This bypasses ART entirely and is
//      only useful for smoke-testing.
//
// Detection is *best-effort*: if the host is aarch64/arm, no translation is
// needed (the strategy is `Native`). If the host is x86_64 and no translator
// is found, the strategy is `None` — containers will still start but any APK
// with native ARM libs will crash on `dlopen`.

use crate::error::{DroidkerError, Result};
use std::path::{Path, PathBuf};

/// CPU architecture a container should *appear* to be running on. This is
/// what gets reported to apps via `Build.SUPPORTED_ABIS` and the
/// `ro.product.cpu.abi` system property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    /// 32-bit ARM (armeabi-v7a).
    Arm,
    /// 64-bit ARM (arm64-v8a).
    Arm64,
    /// 64-bit x86 (x86_64).
    X86_64,
    /// 32-bit x86 (x86 / i686).
    X86,
}

impl Arch {
    pub fn as_str(&self) -> &'static str {
        match self {
            Arch::Arm => "armeabi-v7a",
            Arch::Arm64 => "arm64-v8a",
            Arch::X86_64 => "x86_64",
            Arch::X86 => "x86",
        }
    }

    /// Directory name inside `/system/lib*` that holds this arch's `.so`
    /// files on a real Android system.
    pub fn lib_dir(&self) -> &'static str {
        match self {
            Arch::Arm => "lib",
            Arch::Arm64 => "lib64",
            Arch::X86_64 => "lib64",
            Arch::X86 => "lib",
        }
    }

    /// Alternate ABI name returned by `Build.CPU_ABI` (legacy 32-bit name).
    pub fn alt_abi(&self) -> &'static str {
        match self {
            Arch::Arm => "armeabi",
            Arch::Arm64 => "arm64-v8a",
            Arch::X86_64 => "x86_64",
            Arch::X86 => "x86",
        }
    }

    /// Parse an arch string from the CLI/API. Accepts common aliases.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "arm" | "armeabi" | "armeabi-v7a" | "armv7" => Ok(Arch::Arm),
            "arm64" | "aarch64" | "arm64-v8a" | "armv8" => Ok(Arch::Arm64),
            "x86_64" | "x86-64" | "amd64" | "x64" => Ok(Arch::X86_64),
            "x86" | "i686" | "i386" => Ok(Arch::X86),
            other => Err(format!(
                "unknown arch '{}' (expected: arm, arm64, x86_64, x86)",
                other
            )),
        }
    }

    /// Detect the running host's architecture from `uname(2)`.
    pub fn detect_host() -> Self {
        let uts = match nix::sys::utsname::uname() {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "uname() failed; assuming x86_64");
                return Arch::X86_64;
            }
        };
        let machine = uts.machine().to_string_lossy().to_string();
        match machine.as_str() {
            "x86_64" | "amd64" => Arch::X86_64,
            "aarch64" | "arm64" => Arch::Arm64,
            "armv7l" | "armv6l" | "armhf" => Arch::Arm,
            "i686" | "i386" => Arch::X86,
            _ => {
                tracing::warn!(machine = %machine, "unknown host arch; assuming x86_64");
                Arch::X86_64
            }
        }
    }

    /// Returns `true` if the host can run this arch natively (i.e. without
    /// binary translation). ARM64 hosts can run both ARM and ARM64; x86_64
    /// hosts can run x86 and x86_64.
    pub fn runs_natively_on(&self, host: Arch) -> bool {
        matches!(
            (self, host),
            (Arch::Arm, Arch::Arm)
                | (Arch::Arm, Arch::Arm64)
                | (Arch::Arm64, Arch::Arm64)
                | (Arch::X86, Arch::X86)
                | (Arch::X86, Arch::X86_64)
                | (Arch::X86_64, Arch::X86_64)
        )
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which translation strategy to use for a given (host, target) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationStrategy {
    /// No translation needed — host runs the target arch natively.
    Native,
    /// libhoudini (Intel's closed-source translator).
    Houdini {
        /// Path to the 64-bit libhoudini.so on the host (bind-mounted into
        /// /system/lib64/libhoudini.so inside the container).
        lib64: PathBuf,
        /// Path to the 32-bit libhoudini.so (may be absent if the host only
        /// has 64-bit translation; we'll fall back to qemu for ARMv7).
        lib32: Option<PathBuf>,
    },
    /// libndk_translation (Google's open-source translator).
    NdkTranslation {
        lib64: PathBuf,
        lib32: Option<PathBuf>,
        /// Path to libndk_translation_gl.so (optional — only needed for
        /// GL-based apps).
        gl64: Option<PathBuf>,
    },
    /// qemu-user fallback. Slowest but always available.
    QemuUser {
        /// Path to the qemu-<arch> binary on the host.
        bin: PathBuf,
        /// argv[0] to pass to qemu (so it logs under the right name).
        argv0: String,
    },
    /// No translator available. The container will start but any APK with
    /// ARM-only `.so` files will crash on `dlopen`.
    None,
}

impl TranslationStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranslationStrategy::Native => "native",
            TranslationStrategy::Houdini { .. } => "libhoudini",
            TranslationStrategy::NdkTranslation { .. } => "libndk_translation",
            TranslationStrategy::QemuUser { .. } => "qemu-user",
            TranslationStrategy::None => "none",
        }
    }

    /// Returns `true` if this strategy can actually run ARM code on x86_64.
    pub fn is_usable(&self) -> bool {
        !matches!(self, TranslationStrategy::None)
    }

    /// Environment variables to inject into the container's environment
    /// block when this strategy is active. The variables configure both
    /// ART and the translator itself.
    pub fn env_vars(&self, target: Arch) -> Vec<(String, String)> {
        let mut v = Vec::new();
        match self {
            TranslationStrategy::Native => {
                // No overrides needed — ART picks up the real host ABI.
            }
            TranslationStrategy::Houdini { .. } => {
                // libhoudini reads these to know which ARM variant to
                // dispatch to. Setting both lets the same rootfs run arm
                // and arm64 apps simultaneously.
                v.push(("HOUDINI_ENABLE".to_string(), "1".to_string()));
                v.push(("HOUDINI_ABI".to_string(), target.as_str().to_string()));
                // Bionic's linker respects this: it tells ld-android to
                // try /system/lib/libhoudini.so as a fallback when an
                // ARM .so is requested.
                v.push((
                    "LD_PRELOAD".to_string(),
                    "/system/lib/libhoudini.so:/system/lib64/libhoudini.so".to_string(),
                ));
            }
            TranslationStrategy::NdkTranslation { .. } => {
                v.push(("NDK_TRANSLATION".to_string(), "1".to_string()));
                v.push((
                    "LD_PRELOAD".to_string(),
                    "/system/lib/libndk_translation.so:/system/lib64/libndk_translation.so"
                        .to_string(),
                ));
            }
            TranslationStrategy::QemuUser { .. } => {
                // qemu-user sets these itself; we just hint the target ABI
                // so QEMU_LOG can pick it up if tracing is enabled.
                v.push(("QEMU_GUEST_BASE_ABI".to_string(), target.as_str().to_string()));
            }
            TranslationStrategy::None => {
                // Nothing to set — the container will simply fail to load
                // ARM .so files at runtime.
            }
        }
        v
    }

    /// Bind-mount entries that `droidker-init` must create *before*
    /// `pivot_root` so they're visible from inside the container.
    ///
    /// Each tuple is `(host_source, container_destination)`. The destination
    /// is relative to the merged rootfs.
    pub fn bind_mounts(&self) -> Vec<(PathBuf, PathBuf)> {
        let mut mounts = Vec::new();
        match self {
            TranslationStrategy::Native | TranslationStrategy::None => {
                // No extra mounts.
            }
            TranslationStrategy::Houdini { lib64, lib32 } => {
                mounts.push((lib64.clone(), PathBuf::from("system/lib64/libhoudini.so")));
                if let Some(lib32) = lib32 {
                    mounts.push((lib32.clone(), PathBuf::from("system/lib/libhoudini.so")));
                }
            }
            TranslationStrategy::NdkTranslation { lib64, lib32, gl64 } => {
                mounts.push((lib64.clone(), PathBuf::from("system/lib64/libndk_translation.so")));
                if let Some(lib32) = lib32 {
                    mounts.push((lib32.clone(), PathBuf::from("system/lib/libndk_translation.so")));
                }
                if let Some(gl) = gl64 {
                    mounts.push((gl.clone(), PathBuf::from("system/lib64/libndk_translation_gl.so")));
                }
            }
            TranslationStrategy::QemuUser { bin, .. } => {
                mounts.push((bin.clone(), PathBuf::from("system/bin/qemu-translation")));
            }
        }
        mounts
    }
}

/// Where on the host we look for translation runtimes. Order matters — the
/// first hit wins.
const HOUDINI_SEARCH_PATHS: &[&str] = &[
    // Standard install location from install-translation.sh
    "/opt/droidker/translation/libhoudini",
    // Android-x86 system images ship libhoudini here
    "/opt/droidker/android-rootfs/system/lib/libhoudini.so",
    // Manual install
    "/usr/local/lib/droidker/libhoudini.so",
];

const HOUDINI64_SEARCH_PATHS: &[&str] = &[
    "/opt/droidker/translation/libhoudini/lib64/libhoudini.so",
    "/opt/droidker/android-rootfs/system/lib64/libhoudini.so",
    "/usr/local/lib/droidker/libhoudini64.so",
];

const NDK_TRANSLATION_SEARCH_PATHS: &[&str] = &[
    "/opt/droidker/translation/libndk_translation/lib/libndk_translation.so",
    "/opt/droidker/android-rootfs/system/lib/libndk_translation.so",
];

const NDK_TRANSLATION64_SEARCH_PATHS: &[&str] = &[
    "/opt/droidker/translation/libndk_translation/lib64/libndk_translation.so",
    "/opt/droidker/android-rootfs/system/lib64/libndk_translation.so",
];

const NDK_TRANSLATION_GL64_PATH: &str =
    "/opt/droidker/translation/libndk_translation/lib64/libndk_translation_gl.so";

const QEMU_AARCH64_PATHS: &[&str] = &[
    "/usr/bin/qemu-aarch64-static",
    "/usr/bin/qemu-aarch64",
    "/usr/local/bin/qemu-aarch64",
];

const QEMU_ARM_PATHS: &[&str] = &[
    "/usr/bin/qemu-arm-static",
    "/usr/bin/qemu-arm",
    "/usr/local/bin/qemu-arm",
];

/// Resolve the best translation strategy for running `target` arch on the
/// given `host` arch. Probes the filesystem in priority order and returns
/// the first strategy whose files actually exist.
pub fn resolve_strategy(host: Arch, target: Arch) -> TranslationStrategy {
    if target.runs_natively_on(host) {
        return TranslationStrategy::Native;
    }

    // Only x86_64 → ARM translation is supported. Other pairs are treated
    // as "no translator available".
    if !matches!(host, Arch::X86_64 | Arch::X86) || !matches!(target, Arch::Arm | Arch::Arm64) {
        return TranslationStrategy::None;
    }

    // 1. Try libhoudini.
    if let Some(lib64) = find_first(HOUDINI64_SEARCH_PATHS) {
        let lib32 = find_first(HOUDINI_SEARCH_PATHS);
        tracing::info!(
            lib64 = %lib64.display(),
            lib32 = lib32.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".into()),
            "translation strategy: libhoudini"
        );
        return TranslationStrategy::Houdini { lib64, lib32 };
    }

    // 2. Try libndk_translation.
    if let Some(lib64) = find_first(NDK_TRANSLATION64_SEARCH_PATHS) {
        let lib32 = find_first(NDK_TRANSLATION_SEARCH_PATHS);
        let gl64 = Path::new(NDK_TRANSLATION_GL64_PATH)
            .exists()
            .then(|| PathBuf::from(NDK_TRANSLATION_GL64_PATH));
        tracing::info!(
            lib64 = %lib64.display(),
            lib32 = lib32.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<none>".into()),
            gl64 = gl64.is_some(),
            "translation strategy: libndk_translation"
        );
        return TranslationStrategy::NdkTranslation { lib64, lib32, gl64 };
    }

    // 3. Fall back to qemu-user.
    let qemu_paths: &[&str] = match target {
        Arch::Arm64 => QEMU_AARCH64_PATHS,
        Arch::Arm => QEMU_ARM_PATHS,
        _ => return TranslationStrategy::None,
    };
    if let Some(bin) = find_first(qemu_paths) {
        tracing::info!(bin = %bin.display(), target = %target, "translation strategy: qemu-user");
        return TranslationStrategy::QemuUser {
            bin,
            argv0: format!("qemu-{}", target.as_str()),
        };
    }

    tracing::warn!(
        host = %host,
        target = %target,
        "no ARM translator found; APKs with native ARM libs will crash. \
         Run: sudo bash scripts/install-translation.sh"
    );
    TranslationStrategy::None
}

/// Probe a list of candidate paths; return the first that exists as a
/// regular file (so directories with the same name don't shadow).
fn find_first(paths: &[&str]) -> Option<PathBuf> {
    for p in paths {
        let path = Path::new(p);
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    None
}

/// Top-level entry: resolve translation strategy + produce the bind-mount
/// list and env-vars that `droidker-init` will consume.
///
/// `requested_arch` is what the user asked for (from the container model);
/// if `None`, the host arch is used (no translation).
pub fn build_translation_plan(
    host: Arch,
    requested_arch: Option<Arch>,
) -> (Arch, TranslationStrategy) {
    let target = requested_arch.unwrap_or(host);
    let strategy = resolve_strategy(host, target);
    (target, strategy)
}

/// Concrete plan passed to `droidker-init` via `IsolationSpec`. This wraps
/// the resolved target arch + strategy so the init binary doesn't have to
/// re-probe the host.
#[derive(Debug, Clone)]
pub struct TranslationPlan {
    pub target_arch: Arch,
    pub strategy: TranslationStrategy,
}

impl TranslationPlan {
    /// Default plan for a host-native container (no translation). Useful
    /// when callers want to skip the resolution step.
    pub fn native(host: Arch) -> Self {
        Self {
            target_arch: host,
            strategy: TranslationStrategy::Native,
        }
    }

    /// Serialize the plan into a list of environment variables that
    /// `droidker-init` reads on startup. The init binary uses these to:
    ///   1. Pick which `.so` files to bind-mount (via DROIDKER_TRANSLATION_MOUNTS).
    ///   2. Set the LD_PRELOAD / HOUDINI_ENABLE vars in the app_process env.
    ///   3. Patch `ro.product.cpu.abi` in build.prop before ART starts.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        let mut v = vec![(
            "DROIDKER_TARGET_ARCH".to_string(),
            self.target_arch.as_str().to_string(),
        )];
        v.push((
            "DROIDKER_TRANSLATION_STRATEGY".to_string(),
            self.strategy.as_str().to_string(),
        ));
        // Serialize the bind-mounts as a `:`-separated list of `src=dst`
        // pairs. droidker-init parses this and calls mount(2) for each.
        let mounts: Vec<String> = self
            .strategy
            .bind_mounts()
            .into_iter()
            .map(|(src, dst)| format!("{}={}", src.display(), dst.display()))
            .collect();
        if !mounts.is_empty() {
            v.push((
                "DROIDKER_TRANSLATION_MOUNTS".to_string(),
                mounts.join(":"),
            ));
        }
        // Extra env vars for the app_process environment (LD_PRELOAD etc).
        for (k, val) in self.strategy.env_vars(self.target_arch) {
            v.push((format!("DROIDKER_APP_ENV_{}", k), val));
        }
        v
    }
}

/// Convert a `TranslationStrategy` into a JSON-serializable summary for the
/// `GET /info` endpoint. Keeps the wire format small and free of internal
/// path information.
pub fn strategy_summary(strategy: &TranslationStrategy) -> serde_json::Value {
    serde_json::json!({
        "strategy": strategy.as_str(),
        "usable": strategy.is_usable(),
    })
}

/// Validate that a host path exists and is readable, returning a `Result`
/// so callers can short-circuit with a useful error message.
pub fn ensure_path_exists(p: &Path, label: &str) -> Result<()> {
    if !p.exists() {
        return Err(DroidkerError::Internal(format!(
            "{} not found at {} — run scripts/install-translation.sh",
            label,
            p.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_parse_accepts_common_aliases() {
        assert_eq!(Arch::parse("arm").unwrap(), Arch::Arm);
        assert_eq!(Arch::parse("armeabi-v7a").unwrap(), Arch::Arm);
        assert_eq!(Arch::parse("aarch64").unwrap(), Arch::Arm64);
        assert_eq!(Arch::parse("arm64").unwrap(), Arch::Arm64);
        assert_eq!(Arch::parse("x86_64").unwrap(), Arch::X86_64);
        assert_eq!(Arch::parse("amd64").unwrap(), Arch::X86_64);
        assert_eq!(Arch::parse("x86").unwrap(), Arch::X86);
        assert!(Arch::parse("mips").is_err());
    }

    #[test]
    fn native_arch_runs_natively_on_itself() {
        assert!(Arch::Arm64.runs_natively_on(Arch::Arm64));
        assert!(Arch::X86_64.runs_natively_on(Arch::X86_64));
        assert!(!Arch::Arm64.runs_natively_on(Arch::X86_64));
        assert!(!Arch::X86_64.runs_natively_on(Arch::Arm64));
    }

    #[test]
    fn arm_runs_natively_on_arm64() {
        // 64-bit ARM kernels can run 32-bit ARM binaries via compat mode.
        assert!(Arch::Arm.runs_natively_on(Arch::Arm64));
    }

    #[test]
    fn strategy_env_vars_match_strategy() {
        let s = TranslationStrategy::Houdini {
            lib64: PathBuf::from("/dev/null"),
            lib32: None,
        };
        let env = s.env_vars(Arch::Arm64);
        assert!(env.iter().any(|(k, _)| k == "HOUDINI_ENABLE"));
    }

    #[test]
    fn bind_mounts_for_native_is_empty() {
        let s = TranslationStrategy::Native;
        assert!(s.bind_mounts().is_empty());
    }

    #[test]
    fn bind_mounts_for_qemu_includes_translator_binary() {
        let s = TranslationStrategy::QemuUser {
            bin: PathBuf::from("/usr/bin/qemu-aarch64-static"),
            argv0: "qemu-aarch64".to_string(),
        };
        let mounts = s.bind_mounts();
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].1.ends_with("qemu-translation"));
    }

    #[test]
    fn build_translation_plan_native_when_target_matches_host() {
        let (target, strategy) = build_translation_plan(Arch::Arm64, Some(Arch::Arm64));
        assert_eq!(target, Arch::Arm64);
        assert_eq!(strategy, TranslationStrategy::Native);
    }

    #[test]
    fn build_translation_plan_defaults_to_host_arch() {
        let (target, _) = build_translation_plan(Arch::X86_64, None);
        assert_eq!(target, Arch::X86_64);
    }

    #[test]
    fn resolve_strategy_returns_none_when_unsupported_pair() {
        // x86_64 → x86 is not translation (it's native-ish), but aarch64 → x86_64
        // is unsupported.
        let s = resolve_strategy(Arch::Arm64, Arch::X86_64);
        assert_eq!(s, TranslationStrategy::None);
    }

    #[test]
    fn translation_plan_env_vars_native_includes_arch_only() {
        let plan = TranslationPlan::native(Arch::X86_64);
        let env = plan.env_vars();
        // Should include target_arch + strategy, but no bind-mounts and
        // no DROIDKER_APP_ENV_* entries (native strategy has none).
        assert!(env.iter().any(|(k, v)| k == "DROIDKER_TARGET_ARCH" && v == "x86_64"));
        assert!(env.iter().any(|(k, v)| k == "DROIDKER_TRANSLATION_STRATEGY" && v == "native"));
        assert!(!env.iter().any(|(k, _)| k == "DROIDKER_TRANSLATION_MOUNTS"));
        assert!(!env.iter().any(|(k, _)| k.starts_with("DROIDKER_APP_ENV_")));
    }

    #[test]
    fn translation_plan_env_vars_houdini_includes_mounts_and_ld_preload() {
        let plan = TranslationPlan {
            target_arch: Arch::Arm64,
            strategy: TranslationStrategy::Houdini {
                lib64: PathBuf::from("/opt/droidker/translation/libhoudini/lib64/libhoudini.so"),
                lib32: None,
            },
        };
        let env = plan.env_vars();
        let mounts = env
            .iter()
            .find(|(k, _)| k == "DROIDKER_TRANSLATION_MOUNTS")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(mounts.contains("libhoudini.so"));
        assert!(mounts.contains("system/lib64/libhoudini.so"));
        // LD_PRELOAD must be present in the app env.
        let ld_preload = env
            .iter()
            .find(|(k, _)| k == "DROIDKER_APP_ENV_LD_PRELOAD")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(ld_preload.contains("libhoudini.so"));
    }

    #[test]
    fn translation_plan_env_vars_qemu_includes_strategy_but_no_mounts() {
        let plan = TranslationPlan {
            target_arch: Arch::Arm64,
            strategy: TranslationStrategy::QemuUser {
                bin: PathBuf::from("/usr/bin/qemu-aarch64-static"),
                argv0: "qemu-aarch64".to_string(),
            },
        };
        let env = plan.env_vars();
        assert!(env.iter().any(|(k, v)| k == "DROIDKER_TRANSLATION_STRATEGY" && v == "qemu-user"));
        // qemu-user mounts its binary, not a .so.
        let mounts = env
            .iter()
            .find(|(k, _)| k == "DROIDKER_TRANSLATION_MOUNTS")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        assert!(mounts.contains("qemu-translation"));
    }

    #[test]
    fn strategy_summary_returns_usable_flag() {
        let v = strategy_summary(&TranslationStrategy::Native);
        assert_eq!(v["strategy"], "native");
        assert_eq!(v["usable"], true);

        let v = strategy_summary(&TranslationStrategy::None);
        assert_eq!(v["strategy"], "none");
        assert_eq!(v["usable"], false);
    }

    #[test]
    fn arch_lib_dir_distinguishes_32_and_64_bit() {
        assert_eq!(Arch::Arm.lib_dir(), "lib");
        assert_eq!(Arch::Arm64.lib_dir(), "lib64");
        assert_eq!(Arch::X86.lib_dir(), "lib");
        assert_eq!(Arch::X86_64.lib_dir(), "lib64");
    }

    #[test]
    fn arch_runs_natively_on_x86_64_for_x86() {
        // x86_64 hosts run 32-bit x86 binaries via compat mode.
        assert!(Arch::X86.runs_natively_on(Arch::X86_64));
    }
}
