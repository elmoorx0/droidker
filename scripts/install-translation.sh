#!/usr/bin/env bash
#
# install-translation.sh — Install an ARM→x86_64 binary translator for
# DroidKer containers.
#
# What it does:
#   1. Probes the host for an existing translator (libhoudini from an
#      Android-x86 install, libndk_translation from a Google Play Games
#      install, qemu-user from apt/dnf).
#   2. If nothing is found, installs one of (in order of preference):
#        a. libndk_translation (open-source, packaged as an AOSP extract)
#        b. qemu-user-static (always available via apt/dnf — slowest but
#           works on every host)
#   3. Symlinks the translator .so files into /opt/droidker/translation/
#      so the daemon's translation.rs probe finds them on next start.
#
# Why we don't install libhoudini automatically:
#   libhoudini is closed-source and licensed by Intel. We can't redistribute
#   it. If you have an Android-x86 system image with libhoudini already
#   installed, this script will detect and link it. Otherwise, you must
#   obtain libhoudini.so from a licensed source and drop it into
#   /opt/droidker/translation/libhoudini/lib64/ manually.
#
# Usage:
#   sudo bash scripts/install-translation.sh           # auto-detect + auto-install
#   sudo bash scripts/install-translation.sh --strategy libndk_translation
#   sudo bash scripts/install-translation.sh --strategy qemu-user
#   sudo bash scripts/install-translation.sh --strategy houdini --source /path/to/libhoudini.so
#   sudo bash scripts/install-translation.sh --uninstall

set -euo pipefail

# ---------- Color helpers ----------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${CYAN}[*]${NC} $*"; }
ok()   { echo -e "${GREEN}[✓]${NC} $*"; }
warn() { echo -e "${YELLOW}[!]${NC} $*"; }
err()  { echo -e "${RED}[✗]${NC} $*" >&2; }

# ---------- Defaults ----------
STRATEGY=""
SOURCE_PATH=""
UNINSTALL=0
TRANSLATION_DIR="/opt/droidker/translation"

# ---------- Arg parsing ----------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --strategy)  STRATEGY="$2"; shift 2 ;;
    --source)    SOURCE_PATH="$2"; shift 2 ;;
    --dir)       TRANSLATION_DIR="$2"; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    --help|-h)
      grep '^#' "$0" | head -n 32
      exit 0
      ;;
    *)
      err "Unknown argument: $1"
      exit 2
      ;;
  esac
done

# ---------- Preflight ----------
if [[ $EUID -ne 0 ]]; then
  err "This script must be run as root (use sudo)."
  exit 1
fi

ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" && "$ARCH" != "amd64" ]]; then
  err "Translation is only needed on x86_64 hosts (you are on $ARCH)."
  err "On $ARCH, ARM apps run natively."
  exit 1
fi

# ---------- Uninstall path ----------
if [[ $UNINSTALL -eq 1 ]]; then
  log "Removing translation layer from $TRANSLATION_DIR ..."
  rm -rf "$TRANSLATION_DIR"
  ok "Translation layer uninstalled."
  log "Restart the daemon:  sudo systemctl restart droidkerd"
  exit 0
fi

mkdir -p "$TRANSLATION_DIR"

# ---------- Detect existing translators ----------
have_houdini64=0
have_houdini32=0
have_ndk64=0
have_ndk32=0
have_qemu_aarch64=0
have_qemu_arm=0

# Probe standard Android-x86 locations (libhoudini ships with system.sfs).
for p in \
  "/opt/droidker/android-rootfs/system/lib64/libhoudini.so" \
  "/system/lib64/libhoudini.so" \
  "/usr/local/lib/droidker/libhoudini64.so"; do
  if [[ -f "$p" ]]; then
    have_houdini64=1
    HOUDINI64_PATH="$p"
    break
  fi
done
for p in \
  "/opt/droidker/android-rootfs/system/lib/libhoudini.so" \
  "/system/lib/libhoudini.so" \
  "/usr/local/lib/droidker/libhoudini.so"; do
  if [[ -f "$p" ]]; then
    have_houdini32=1
    HOUDINI32_PATH="$p"
    break
  fi
done

# Probe libndk_translation (extracted from AOSP or Google Play Games).
for p in \
  "/opt/droidker/android-rootfs/system/lib64/libndk_translation.so" \
  "/usr/local/lib/droidker/libndk_translation64.so"; do
  if [[ -f "$p" ]]; then
    have_ndk64=1
    NDK64_PATH="$p"
    break
  fi
done
for p in \
  "/opt/droidker/android-rootfs/system/lib/libndk_translation.so" \
  "/usr/local/lib/droidker/libndk_translation.so"; do
  if [[ -f "$p" ]]; then
    have_ndk32=1
    NDK32_PATH="$p"
    break
  fi
done

# Probe qemu-user.
for p in /usr/bin/qemu-aarch64-static /usr/bin/qemu-aarch64 /usr/local/bin/qemu-aarch64; do
  if [[ -x "$p" ]]; then
    have_qemu_aarch64=1
    QEMU_AARCH64_PATH="$p"
    break
  fi
done
for p in /usr/bin/qemu-arm-static /usr/bin/qemu-arm /usr/local/bin/qemu-arm; do
  if [[ -x "$p" ]]; then
    have_qemu_arm=1
    QEMU_ARM_PATH="$p"
    break
  fi
done

# ---------- Decide strategy ----------
if [[ -z "$STRATEGY" ]]; then
  if [[ $have_houdini64 -eq 1 ]]; then
    STRATEGY="libhoudini"
  elif [[ $have_ndk64 -eq 1 ]]; then
    STRATEGY="libndk_translation"
  else
    STRATEGY="qemu-user"
  fi
fi

log "Selected strategy: $STRATEGY"

# ---------- Install / link the chosen strategy ----------
case "$STRATEGY" in
  libhoudini)
    if [[ $have_houdini64 -eq 0 ]]; then
      if [[ -z "$SOURCE_PATH" ]]; then
        err "libhoudini.so not found on this host."
        err ""
        err "libhoudini is closed-source and licensed by Intel — we can't"
        err "auto-download it. Options:"
        err "  1. Install Android-x86 alongside DroidKer; this script will"
        err "     detect its libhoudini.so automatically."
        err "  2. Obtain libhoudini.so from a licensed source and run:"
        err "       sudo bash scripts/install-translation.sh \\"
        err "         --strategy houdini \\"
        err "         --source /path/to/libhoudini.so"
        err "  3. Fall back to qemu-user:"
        err "       sudo bash scripts/install-translation.sh --strategy qemu-user"
        exit 1
      fi
      if [[ ! -f "$SOURCE_PATH" ]]; then
        err "Source file not found: $SOURCE_PATH"
        exit 1
      fi
      HOUDINI64_PATH="$SOURCE_PATH"
    fi
    install -d "$TRANSLATION_DIR/libhoudini/lib64"
    ln -sf "$HOUDINI64_PATH" "$TRANSLATION_DIR/libhoudini/lib64/libhoudini.so"
    if [[ $have_houdini32 -eq 1 ]]; then
      install -d "$TRANSLATION_DIR/libhoudini/lib"
      ln -sf "$HOUDINI32_PATH" "$TRANSLATION_DIR/libhoudini/lib/libhoudini.so"
    else
      warn "32-bit libhoudini.so not found — 32-bit ARM APKs (armeabi-v7a)"
      warn "will fall back to qemu-user. 64-bit ARM APKs (arm64-v8a) work."
    fi
    ok "libhoudini linked into $TRANSLATION_DIR/libhoudini/"
    ;;

  libndk_translation)
    if [[ $have_ndk64 -eq 0 ]]; then
      log "Installing libndk_translation from Google's prebuilt archive..."
      install -d "$TRANSLATION_DIR/libndk_translation/lib64"
      install -d "$TRANSLATION_DIR/libndk_translation/lib"
      # Google ships libndk_translation as part of "Google Play Games on PC".
      # The prebuilt .so files can be extracted from a developer emulator
      # image; we don't redistribute them here. Try the official AOSP
      # build path:
      #   https://android.googlesource.com/platform/ndk/+/refs/heads/master/translation
      #
      # For this script we attempt to download from the
      # google-ndk-translation GitHub release (community mirror).
      NDK_URL_BASE="https://github.com/google/ndk-translation/releases/download/v1.0"
      if command -v curl >/dev/null 2>&1; then
        curl -fL --progress-bar -o /tmp/libndk_translation64.so \
          "$NDK_URL_BASE/libndk_translation64.so" || true
        curl -fL --progress-bar -o /tmp/libndk_translation.so \
          "$NDK_URL_BASE/libndk_translation.so" || true
      else
        err "curl is required to download libndk_translation"
        exit 1
      fi
      if [[ -f /tmp/libndk_translation64.so ]]; then
        cp /tmp/libndk_translation64.so \
          "$TRANSLATION_DIR/libndk_translation/lib64/libndk_translation.so"
        NDK64_PATH="$TRANSLATION_DIR/libndk_translation/lib64/libndk_translation.so"
        have_ndk64=1
      else
        err "Download failed — falling back to qemu-user."
        STRATEGY="qemu-user"
      fi
      if [[ -f /tmp/libndk_translation.so ]]; then
        cp /tmp/libndk_translation.so \
          "$TRANSLATION_DIR/libndk_translation/lib/libndk_translation.so"
        NDK32_PATH="$TRANSLATION_DIR/libndk_translation/lib/libndk_translation.so"
        have_ndk32=1
      fi
    else
      install -d "$TRANSLATION_DIR/libndk_translation/lib64"
      install -d "$TRANSLATION_DIR/libndk_translation/lib"
      ln -sf "$NDK64_PATH" "$TRANSLATION_DIR/libndk_translation/lib64/libndk_translation.so"
      if [[ $have_ndk32 -eq 1 ]]; then
        ln -sf "$NDK32_PATH" "$TRANSLATION_DIR/libndk_translation/lib/libndk_translation.so"
      fi
    fi
    if [[ "$STRATEGY" == "libndk_translation" ]]; then
      ok "libndk_translation linked into $TRANSLATION_DIR/libndk_translation/"
    fi
    ;;

  qemu-user)
    log "Installing qemu-user-static via the system package manager..."
    if command -v apt-get >/dev/null 2>&1; then
      apt-get update -qq
      apt-get install -y qemu-user-static
    elif command -v dnf >/dev/null 2>&1; then
      dnf install -y qemu-user-binfmt
    elif command -v yum >/dev/null 2>&1; then
      yum install -y qemu-user-binfmt
    elif command -v pacman >/dev/null 2>&1; then
      pacman -S --noconfirm qemu-user-static-binfmt
    else
      err "No supported package manager found (apt/dnf/yum/pacman)."
      err "Install qemu-user-static manually, then re-run this script."
      exit 1
    fi
    # Re-probe — the package should have installed the binaries.
    for p in /usr/bin/qemu-aarch64-static /usr/bin/qemu-aarch64; do
      if [[ -x "$p" ]]; then
        QEMU_AARCH64_PATH="$p"
        have_qemu_aarch64=1
        break
      fi
    done
    for p in /usr/bin/qemu-arm-static /usr/bin/qemu-arm; do
      if [[ -x "$p" ]]; then
        QEMU_ARM_PATH="$p"
        have_qemu_arm=1
        break
      fi
    done
    if [[ $have_qemu_aarch64 -eq 0 ]]; then
      err "qemu-aarch64 not found after install — qemu-user install failed?"
      exit 1
    fi
    ok "qemu-user installed: $QEMU_AARCH64_PATH"
    if [[ $have_qemu_arm -eq 0 ]]; then
      warn "qemu-arm not found — 32-bit ARM APKs won't translate."
      warn "Install qemu-user-static-binfmt for full ARM support."
    fi
    ;;

  *)
    err "Unknown strategy: $STRATEGY"
    err "Valid strategies: libhoudini | libndk_translation | qemu-user"
    exit 2
    ;;
esac

# ---------- Summary ----------
echo
log "Translation layer installation complete."
log "  strategy: $STRATEGY"
log "  location: $TRANSLATION_DIR"
if [[ "$STRATEGY" == "libhoudini" ]]; then
  log "  libhoudini64: ${HOUDINI64_PATH:-<none>}"
  log "  libhoudini32: ${HOUDINI32_PATH:-<none>}"
elif [[ "$STRATEGY" == "libndk_translation" ]]; then
  log "  libndk_translation64: ${NDK64_PATH:-<none>}"
  log "  libndk_translation32: ${NDK32_PATH:-<none>}"
elif [[ "$STRATEGY" == "qemu-user" ]]; then
  log "  qemu-aarch64: ${QEMU_AARCH64_PATH:-<none>}"
  log "  qemu-arm:     ${QEMU_ARM_PATH:-<none>}"
fi
echo
log "Next steps:"
log "  1. Restart the daemon:  sudo systemctl restart droidkerd"
log "  2. Verify detection:    droidker info"
log "  3. Run an ARM APK:      droidker run app.apk --arch arm64"
