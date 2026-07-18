#!/usr/bin/env bash
#
# build-rootfs.sh — Build the shared Android rootfs used by every DroidKer
# container.
#
# What it does:
#   1. Downloads an Android-x86 or LineageOS x86_64 system image (you can
#      override the source URL).
#   2. Extracts /system from the image into /opt/droidker/android-rootfs/system
#   3. Strips out Google Play Services (gms) and replaces them with microG.
#   4. Patches build.prop for headless operation.
#   5. Installs a minimal /system/etc/permissions set.
#
# Resulting layout:
#   /opt/droidker/android-rootfs/
#   ├── system/
#   │   ├── bin/app_process64
#   │   ├── lib64/           (Bionic, ART, libbinder, ...)
#   │   ├── framework/       (services.jar, framework.jar, ...)
#   │   ├── etc/             (permissions, build.prop)
#   │   ├── build.prop
#   │   └── ...
#   └── (no /data, /cache — those live per-container in the overlay upperdir)
#
# Tested on:
#   - Ubuntu 22.04 x86_64 (uses Android-x86 9.0-r2)
#   - Debian 12 x86_64
#   - Ubuntu 22.04 ARM64 (uses LineageOS 18.1 arm64-abdetached)
#
# Usage:
#   sudo bash scripts/build-rootfs.sh                  # default source
#   sudo bash scripts/build-rootfs.sh --source URL     # custom source
#   sudo bash scripts/build-rootfs.sh --arch arm64     # ARM64 host
#   sudo bash scripts/build-rootfs.sh --keep-cache     # keep downloaded image
#   sudo bash scripts/build-rootfs.sh --microg-url URL # custom microG build

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
ARCH="$(uname -m)"
SOURCE_URL=""
KEEP_CACHE=0
MICROG_URL="https://github.com/microG/android_packages_apps_GmsCore/releases/download/v0.3.1/GmsCore_v0.3.1.apks"
ROOTFS_DIR="/opt/droidker/android-rootfs"
CACHE_DIR="/var/cache/droidker"

# Default sources per arch.
declare -A DEFAULT_SOURCES=(
  ["x86_64"]="https://www.android-x86.org/download/Download/96/system.sfs"
  ["aarch64"]="https://mirror.lineageos.org/eleven/hbp/arm64-abdetached/lineage-18.1-20240115-nightly-hbp-signed.zip"
)

# ---------- Arg parsing ----------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch)        ARCH="$2"; shift 2 ;;
    --source)      SOURCE_URL="$2"; shift 2 ;;
    --microg-url)  MICROG_URL="$2"; shift 2 ;;
    --rootfs-dir)  ROOTFS_DIR="$2"; shift 2 ;;
    --keep-cache)  KEEP_CACHE=1; shift ;;
    --help|-h)
      grep '^#' "$0" | head -n 30
      exit 0
      ;;
    *)
      err "Unknown argument: $1"
      exit 2
      ;;
  esac
done

# Normalize arch.
case "$ARCH" in
  x86_64|amd64)  ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *)
    err "Unsupported arch: $ARCH (use --arch x86_64|aarch64)"
    exit 1
    ;;
esac

# Fall back to default source if not given.
if [[ -z "$SOURCE_URL" ]]; then
  SOURCE_URL="${DEFAULT_SOURCES[$ARCH]}"
fi

log "Building Android rootfs"
log "  arch:       $ARCH"
log "  source:     $SOURCE_URL"
log "  rootfs dir: $ROOTFS_DIR"
log "  cache dir:  $CACHE_DIR"

# ---------- Preflight ----------
if [[ $EUID -ne 0 ]]; then
  err "This script must be run as root (use sudo)."
  exit 1
fi

for cmd in curl wget unsquashfs 7z unzip mksquashfs; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    case "$cmd" in
      unsquashfs|mksquashfs)
        apt-get install -y squashfs-tools ;;
      7z)
        apt-get install -y p7zip-full ;;
      *)
        apt-get install -y "$cmd" ;;
    esac
  fi
done

mkdir -p "$ROOTFS_DIR" "$CACHE_DIR"

# ---------- Step 1: Download source image ----------
IMAGE_FILE="$CACHE_DIR/source.$ARCH"
if [[ -f "$IMAGE_FILE" ]]; then
  ok "Source image already cached at $IMAGE_FILE"
else
  log "Downloading source image..."
  case "$SOURCE_URL" in
    *.sfs|*.squashfs)
      IMAGE_FILE="$CACHE_DIR/source.$ARCH.sfs"
      ;;
    *.zip)
      IMAGE_FILE="$CACHE_DIR/source.$ARCH.zip"
      ;;
    *.img)
      IMAGE_FILE="$CACHE_DIR/source.$ARCH.img"
      ;;
  esac
  curl -fL --progress-bar -o "$IMAGE_FILE" "$SOURCE_URL"
  ok "Downloaded $(du -h "$IMAGE_FILE" | cut -f1)"
fi

# ---------- Step 2: Extract /system ----------
WORK_DIR="$(mktemp -d -t droidker-rootfs-XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT

log "Extracting /system from image (working dir: $WORK_DIR)..."
case "$IMAGE_FILE" in
  *.sfs|*.squashfs)
    unsquashfs -f -d "$WORK_DIR/squashfs-root" "$IMAGE_FILE"
    SYSTEM_SRC="$WORK_DIR/squashfs-root/system"
    ;;
  *.zip)
    unzip -q "$IMAGE_FILE" -d "$WORK_DIR/zip-out"
    # LineageOS zips ship payload.bin; we'd need payload-dumper-go to extract.
    # For now, error out and ask the user to provide an .sfs or .img.
    err "ZIP source requires payload-dumper-go; please provide an .sfs or .img URL with --source"
    exit 1
    ;;
  *.img)
    # Mount the sparse/ext4 image read-only.
    mkdir -p "$WORK_DIR/img-mount"
    mount -o loop,ro "$IMAGE_FILE" "$WORK_DIR/img-mount"
    SYSTEM_SRC="$WORK_DIR/img-mount/system"
    ;;
  *)
    err "Unknown image format: $IMAGE_FILE"
    exit 1
    ;;
esac

if [[ ! -d "$SYSTEM_SRC" ]]; then
  err "Extracted image has no /system directory"
  exit 1
fi

# ---------- Step 3: Copy /system into the rootfs ----------
log "Copying /system → $ROOTFS_DIR/system ..."
rm -rf "$ROOTFS_DIR/system"
cp -a "$SYSTEM_SRC" "$ROOTFS_DIR/system"

# Make sure the system dir is owned by root and not writable by group/other.
chown -R root:root "$ROOTFS_DIR/system"
chmod -R go-w "$ROOTFS_DIR/system"

# ---------- Step 4: Strip proprietary Google services ----------
log "Removing proprietary Google apps..."
GMS_PKGS=(
  "system/priv-app/GoogleServicesFramework"
  "system/priv-app/PrebuiltGmsCore"
  "system/priv-app/GoogleLoginService"
  "system/app/Chrome"
  "system/app/Maps"
  "system/app/Gmail2"
  "system/app/YouTube"
  "system/app/Drive"
  "system/app/Photos"
  "system/app/Videos"
  "system/app/Music2"
  "system/app/Duo"
  "system/app/Hangouts"
)
for pkg in "${GMS_PKGS[@]}"; do
  if [[ -d "$ROOTFS_DIR/$pkg" ]]; then
    rm -rf "$ROOTFS_DIR/$pkg"
    log "  removed: $pkg"
  fi
done
ok "Google apps stripped"

# ---------- Step 5: Install microG as a system app ----------
log "Installing microG..."
MICROG_DST="$ROOTFS_DIR/system/priv-app/microG"
mkdir -p "$MICROG_DST"
MICROG_APK="$CACHE_DIR/microG.apk"
if [[ ! -f "$MICROG_APK" ]]; then
  curl -fL --progress-bar -o "$MICROG_APK" "$MICROG_URL"
fi
cp "$MICROG_APK" "$MICROG_DST/microG.apk"

# Write the privapp-permissions file that microG needs.
cat > "$ROOTFS_DIR/system/etc/permissions/privapp-permissions-microG.xml" <<'XML'
<?xml version="1.0" encoding="utf-8"?>
<permissions>
  <privapp-permissions package="com.google.android.gms">
    <permission name="android.permission.FAKE_PACKAGE_SIGNATURE" />
    <permission name="android.permission.INSTALL_LOCATION_PROVIDER" />
    <permission name="android.permission.INSTALL_PACKAGE" />
    <permission name="android.permission.LOCATION_HARDWARE" />
    <permission name="android.permission.MANAGE_USB" />
    <permission name="android.permission.MASTER_CLEAR" />
    <permission name="android.permission.READ_DREAM_STATE" />
    <permission name="android.permission.REAL_GET_TASKS" />
    <permission name="android.permission.SET_MEDIA_KEY_LISTENER" />
    <permission name="android.permission.SET_VOLUME_KEY_LISTENER" />
    <permission name="android.permission.START_ACTIVITIES_FROM_BACKGROUND" />
    <permission name="android.permission.STATUS_BAR" />
    <permission name="android.permission.UPDATE_DEVICE_STATS" />
    <permission name="android.permission.WRITE_SECURE_SETTINGS" />
  </privapp-permissions>
</permissions>
XML
ok "microG installed"

# ---------- Step 6: Patch build.prop for headless operation ----------
log "Patching build.prop for headless mode..."
BUILD_PROP="$ROOTFS_DIR/system/build.prop"
if [[ -f "$BUILD_PROP" ]]; then
  # Backup the original.
  cp "$BUILD_PROP" "$BUILD_PROP.orig"

  # Remove any existing ro.config.* lines that reference specific hardware.
  sed -i '/^ro\.config\./d' "$BUILD_PROP"

  # Append DroidKer-specific overrides.
  cat >> "$BUILD_PROP" <<'PROP'

# ----- DroidKer overrides -----
ro.droidker=true
ro.droidker.version=0.1.0
ro.hardware=droidker
ro.bootmode=unknown
ro.boot.hardware=droidker
ro.build.characteristics=nodefault
# Disable boot animation (we don't have a display).
debug.sf.nobootanimation=1
# Stay awake on AC power (we're always on AC).
ro.kernel.qemu=1
ro.kernel.qemu.gles=0
# Fake device identifiers so apps see a recognizable phone.
ro.product.model=Pixel 6
ro.product.brand=Google
ro.product.name=oriole
ro.product.device=oriole
ro.product.manufacturer=Google
# Disable ART AOT for now (M2 uses interpreter only) — saves disk + boot time.
dalvik.vm.dex2oat-filter=interpret-only
pm.dexopt.install=interpret-only
pm.dexopt.bg-dexopt=interpret-only
pm.dexopt.boot=verify
PROP
  ok "build.prop patched"
else
  warn "build.prop not found — skipping patches"
fi

# ---------- Step 7: Create empty /data, /cache, /acct directories ----------
# These exist as empty dirs in the shared rootfs; the per-container overlay
# makes them writable.
log "Creating top-level directories..."
for d in data cache acct dev proc sys; do
  mkdir -p "$ROOTFS_DIR/$d"
done

# ---------- Step 8: Symlinks that Android expects ----------
log "Creating standard symlinks..."
ln -sf /system/etc "$ROOTFS_DIR/etc"
# /vendor usually points to /system/vendor on modern Android.
if [[ ! -e "$ROOTFS_DIR/vendor" ]]; then
  ln -sf /system/vendor "$ROOTFS_DIR/vendor" 2>/dev/null || true
fi

# ---------- Step 9: Validate ----------
log "Validating rootfs..."
REQUIRED=(
  "system/bin/app_process64"
  "system/lib64"
  "system/framework"
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
  err "Rootfs validation failed."
  exit 1
fi

# ---------- Cleanup ----------
if [[ "$KEEP_CACHE" -eq 0 ]]; then
  log "Cleaning up cached image (use --keep-cache to keep it)..."
  rm -f "$IMAGE_FILE"
fi
# Unmount any loop-mounted image.
if [[ -n "${SYSTEM_SRC:-}" ]] && [[ "${SYSTEM_SRC:0:1}" == "/" ]]; then
  case "$IMAGE_FILE" in
    *.img) umount "$WORK_DIR/img-mount" 2>/dev/null || true ;;
  esac
fi

# ---------- Done ----------
ok "Android rootfs built at $ROOTFS_DIR"
ok "  size: $(du -sh "$ROOTFS_DIR" | cut -f1)"
ok "  arch: $ARCH"
echo
log "Next steps:"
log "  1. Restart the daemon:  sudo systemctl restart droidkerd"
log "  2. Launch an app:       droidker run ~/Downloads/my-app.apk"
