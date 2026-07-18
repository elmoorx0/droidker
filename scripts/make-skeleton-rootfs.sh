#!/usr/bin/env bash
#
# make-skeleton-rootfs.sh — Build a minimal Android-like rootfs skeleton
#                            for DroidKer dev/CI hosts.
#
# This is NOT a real Android rootfs. It produces just enough of the
# /system layout for `droidkerd` to pass `validate_android_rootfs()` and
# for `droidker-init` to successfully pivot_root into a working chroot
# where it can run a tiny "fake app_process" stub for smoke-testing the
# namespace+cgroup+network pipeline.
#
# For a real Android runtime, run `build-rootfs.sh` instead.
#
# Layout produced:
#   /opt/droidker/android-rootfs/
#   ├── system/
#   │   ├── bin/
#   │   │   ├── app_process64            (shell shim: prints args, sleeps)
#   │   │   ├── sh -> /bin/busybox        (only if busybox present)
#   │   │   └── am                        (no-op shim)
#   │   ├── lib64/                        (empty dir; no real .so files)
#   │   ├── framework/                    (empty dir)
#   │   ├── etc/
#   │   │   ├── permissions/              (empty)
#   │   │   └── build.prop                (minimal DroidKer-only props)
#   │   └── build.prop                    (symlink to etc/build.prop)
#   ├── data/                             (empty; overlay fills this in)
#   ├── cache/                            (empty)
#   ├── dev/                              (empty; bind-mounted at runtime)
#   ├── proc/                             (empty; mountpoint at runtime)
#   ├── sys/                              (empty; mountpoint at runtime)
#   ├── acct/                             (empty)
#   ├── etc -> system/etc
#   └── vendor -> system/vendor           (only if system/vendor exists)

set -euo pipefail

ROOTFS_DIR="${1:-/opt/droidker/android-rootfs}"

log()  { printf "\033[0;36m[*]\033[0m %s\n" "$*"; }
ok()   { printf "\033[0;32m[✓]\033[0m %s\n" "$*"; }
err()  { printf "\033[0;31m[✗]\033[0m %s\n" "$*" >&2; }

if [[ $EUID -ne 0 ]]; then
  err "This script must be run as root (use sudo)."
  exit 1
fi

log "Building skeleton Android rootfs at $ROOTFS_DIR"
mkdir -p "$ROOTFS_DIR"

# ---- Top-level directories --------------------------------------------------
for d in system/bin system/lib64 system/framework system/etc/permissions \
         system/vendor data cache dev proc sys acct; do
  mkdir -p "$ROOTFS_DIR/$d"
done

# ---- /system/bin/app_process64 stub ----------------------------------------
# This is what droidker-init execs. We make it a shell script that logs its
# args and then sleeps forever — that way the daemon's "container is running"
# state stays valid and we can poke at the sandbox via the cgroup + netns.
cat > "$ROOTFS_DIR/system/bin/app_process64" <<'SH'
#!/system/bin/sh
# DroidKer skeleton app_process stub.
echo "[droidker-app_process] args: $*" >&2
echo "[droidker-app_process] container ready; pid=$$; sleeping forever" >&2
# Tail /dev/null so PID 1 stays alive waiting for signals.
exec sleep 86400
SH
chmod +x "$ROOTFS_DIR/system/bin/app_process64"

# ---- /system/bin/sh (needed by the stub above) -----------------------------
# In a real rootfs this is a Bionic-backed mksh binary. In the skeleton we
# symlink to /bin/sh on the host — which is fine because droidker-init
# pivot_roots into the merged view, so /bin/sh on the host is *not* visible.
# Instead we install busybox if available, else fall back to a minimal
# script that uses the host's /bin/bash via an absolute path.
if command -v busybox >/dev/null 2>&1; then
  cp "$(command -v busybox)" "$ROOTFS_DIR/system/bin/sh"
  chmod +x "$ROOTFS_DIR/system/bin/sh"
else
  # No busybox — install a tiny C-built /system/bin/sh stub at next boot via
  # the daemon. For now, install a symlink that *will* be broken inside the
  # chroot, but the app_process64 script above doesn't actually need /bin/sh
  # because it's interpreted by the kernel via the #! line — wait, that
  # needs /system/bin/sh. So install a statically-compiled dash if available.
  if [[ -x /bin/dash ]]; then
    cp /bin/dash "$ROOTFS_DIR/system/bin/sh"
  else
    err "Neither busybox nor dash is installed; /system/bin/sh will be missing."
    err "Install one of: apt-get install -y busybox-static"
    exit 1
  fi
fi

# ---- /system/bin/am stub ----------------------------------------------------
cat > "$ROOTFS_DIR/system/bin/am" <<'SH'
#!/system/bin/sh
# DroidKer skeleton `am` (Activity Manager) stub.
echo "[droidker-am] (stub) args: $*" >&2
exit 0
SH
chmod +x "$ROOTFS_DIR/system/bin/am"

# ---- /system/etc/build.prop ------------------------------------------------
cat > "$ROOTFS_DIR/system/etc/build.prop" <<'PROP'
# ----- DroidKer skeleton build.prop -----
# These are the minimum properties ART/Bionic probe at boot. Real values
# would come from the Android system image; the skeleton just supplies
# enough to satisfy any code that reads ro.build.* before exec'ing
# app_process.

ro.build.version.release=10
ro.build.version.sdk=29
ro.build.version.incremental=droidker-skeleton
ro.product.model=DroidKer-Skeleton
ro.product.brand=generic
ro.product.name=droidker
ro.product.device=droidker
ro.product.manufacturer=DroidKer
ro.product.cpu.abi=x86_64
ro.hardware=droidker
ro.bootmode=unknown
ro.boot.hardware=droidker

# DroidKer-specific flags.
ro.droidker=true
ro.droidker.version=0.1.0
ro.droidker.skeleton=true

# Headless mode (no SurfaceFlinger).
debug.sf.nobootanimation=1
ro.kernel.qemu=1
ro.kernel.qemu.gles=0

# ART in interpreter-only mode (no dex2oat).
dalvik.vm.dex2oat-filter=interpret-only
pm.dexopt.install=interpret-only
pm.dexopt.bg-dexopt=interpret-only
pm.dexopt.boot=verify
PROP

# build.prop at /system/build.prop (some code reads it there).
ln -sf /system/etc/build.prop "$ROOTFS_DIR/system/build.prop"

# ---- Top-level symlinks ----------------------------------------------------
ln -sf /system/etc "$ROOTFS_DIR/etc"
# Don't create /vendor symlink if system/vendor doesn't exist as a real dir.
[[ -d "$ROOTFS_DIR/system/vendor" ]] && ln -sf /system/vendor "$ROOTFS_DIR/vendor" || true

# ---- Validation ------------------------------------------------------------
REQUIRED=(
  "system/bin/app_process64"
  "system/bin/sh"
  "system/lib64"
  "system/framework"
  "system/etc/build.prop"
  "system/build.prop"
  "system/etc"
)
MISSING=0
for r in "${REQUIRED[@]}"; do
  if [[ ! -e "$ROOTFS_DIR/$r" ]]; then
    err "missing required path: $ROOTFS_DIR/$r"
    MISSING=1
  fi
done
if [[ $MISSING -ne 0 ]]; then
  err "Skeleton rootfs validation failed."
  exit 1
fi

ok "Skeleton rootfs built at $ROOTFS_DIR"
ok "  size: $(du -sh "$ROOTFS_DIR" | cut -f1)"
echo
log "Note: this is a dev/CI skeleton only."
log "For a real Android runtime, run: sudo bash scripts/build-rootfs.sh"
