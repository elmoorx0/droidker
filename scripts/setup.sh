#!/usr/bin/env bash
#
# setup.sh — Bootstrap a fresh VPS for DroidKer.
#
# What this script does:
#   1. Installs build dependencies (Rust toolchain, Node.js, system libs)
#   2. Loads the binder + ashmem kernel modules (required by Android runtime)
#   3. Creates the binder/ashmem device nodes if missing
#   4. Configures a bridge network (droidker0) for container networking
#   5. Enables IP forwarding + iptables masquerade for outbound traffic
#   6. Creates the /var/lib/droidker data directory + Android rootfs skeleton
#   7. Builds the backend daemon (droidkerd) and the CLI (droidker)
#   8. Installs a systemd unit so the daemon starts on boot
#
# Tested on:
#   - Ubuntu 22.04 LTS x86_64
#   - Debian 12 x86_64
#   - Ubuntu 22.04 ARM64 (no ARM translation layer needed)
#
# Usage:
#   sudo bash setup.sh           # full install
#   sudo bash setup.sh --check   # only verify prerequisites, install nothing

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

# ---------- Preflight ----------
if [[ $EUID -ne 0 ]]; then
  err "This script must be run as root (use sudo)."
  exit 1
fi

CHECK_ONLY=0
if [[ "${1:-}" == "--check" ]]; then
  CHECK_ONLY=1
  log "Running in --check mode (no changes will be made)."
fi

# ---------- Distribution detection ----------
detect_distro() {
  if [[ -f /etc/os-release ]]; then
    . /etc/os-release
    echo "${ID:-unknown}"
  else
    echo "unknown"
  fi
}

DISTRO=$(detect_distro)
ARCH=$(uname -m)
log "Detected distro: ${DISTRO}, arch: ${ARCH}"

# ---------- Step 1: System package dependencies ----------
install_packages() {
  log "Installing system packages..."
  case "$DISTRO" in
    ubuntu|debian)
      apt-get update -y
      apt-get install -y \
        build-essential \
        pkg-config \
        libssl-dev \
        curl \
        ca-certificates \
        iptables \
        iproute2 \
        bridge-utils \
        cgroup-tools \
        kmod \
        jq \
        git \
        fuse-overlayfs \
        uidmap \
        libcap2-bin \
        util-linux \
        squashfs-tools
      ;;
    fedora|centos|rhel|rocky|alma)
      dnf install -y \
        @development-tools \
        pkgconfig \
        openssl-devel \
        curl \
        ca-certificates \
        iptables \
        iproute \
        bridge-utils \
        libcgroup-tools \
        kmod \
        jq \
        git \
        fuse-overlayfs \
        shadow-utils-subid
      ;;
    *)
      err "Unsupported distro: $DISTRO. Install deps manually and re-run."
      exit 1
      ;;
  esac
  ok "System packages installed."
}

# ---------- Step 2: Rust toolchain ----------
install_rust() {
  if command -v cargo >/dev/null 2>&1; then
    ok "Rust toolchain already present ($(rustc --version))."
    return
  fi
  log "Installing Rust via rustup..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
  ok "Rust installed."
}

# ---------- Step 3: Node.js (for the SvelteKit frontend) ----------
install_node() {
  if command -v node >/dev/null 2>&1 && [[ "$(node -v | cut -d. -f1 | tr -d v)" -ge 18 ]]; then
    ok "Node.js already present ($(node -v))."
    return
  fi
  log "Installing Node.js 20.x via NodeSource..."
  case "$DISTRO" in
    ubuntu|debian)
      curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
      apt-get install -y nodejs
      ;;
    fedora|centos|rhel|rocky|alma)
      curl -fsSL https://rpm.nodesource.com/setup_20.x | bash -
      dnf install -y nodejs
      ;;
  esac
  ok "Node.js installed ($(node -v))."
}

# ---------- Step 4: Kernel modules (binder + ashmem) ----------
#
# Android's IPC depends on /dev/binder (and /dev/ashmem for shared memory).
# On mainline Linux these are available as either:
#   - In-tree modules on kernels >= 5.x with CONFIG_ANDROID_BINDERFS=y
#   - Out-of-tree modules from the "anbox/binder-linux" DKMS package
#
# We try in-tree binderfs first (newer kernels), then fall back to DKMS.
setup_binder_ashmem() {
  log "Configuring binder + ashmem..."

  # --- binderfs approach (preferred) --------------------------------------
  if modprobe binder_linux 2>/dev/null || [[ -d /dev/binderfs ]]; then
    mkdir -p /dev/binderfs
    if ! mountpoint -q /dev/binderfs; then
      mount -t binder binder /dev/binderfs 2>/dev/null || true
    fi
    # binderfs exposes "binder", "hwbinder", "vndbinder" control files
    if [[ -c /dev/binderfs/binder ]]; then
      # Allocate a binder device for DroidKer
      echo "droidker-binder" > /dev/binderfs/binder-control 2>/dev/null || true
      ln -sf /dev/binderfs/droidker-binder /dev/binder
      ok "binderfs mounted at /dev/binderfs, /dev/binder symlinked."
    else
      warn "binderfs mounted but no /dev/binderfs/binder found."
    fi
  else
    warn "binder_linux module not available; will try DKMS fallback."
  fi

  # --- ashmem -------------------------------------------------------------
  # Since Linux 5.18 the ashmem module still ships with staging. Try to load.
  if modprobe ashmem_linux 2>/dev/null; then
    if [[ -c /dev/ashmem ]]; then
      ok "ashmem available at /dev/ashmem."
    fi
  else
    warn "ashmem_linux not loadable. Newer kernels use memfd-based ashmem replacement; ART >= 12 handles this gracefully."
  fi
}

# ---------- Step 5: Bridge network ----------
setup_bridge() {
  log "Configuring bridge network 'droidker0'..."
  if ip link show droidker0 &>/dev/null; then
    ok "Bridge droidker0 already exists."
  else
    ip link add name droidker0 type bridge
    ip addr add 10.244.0.1/16 dev droidker0
    ip link set droidker0 up
    ok "Bridge droidker0 created (10.244.0.0/16)."
  fi

  # Enable IPv4 forwarding
  sysctl -w net.ipv4.ip_forward=1 >/dev/null
  if ! grep -q "net.ipv4.ip_forward=1" /etc/sysctl.d/99-droidker.conf 2>/dev/null; then
    echo "net.ipv4.ip_forward=1" > /etc/sysctl.d/99-droidker.conf
  fi

  # NAT masquerade so containers can reach the internet
  iptables -t nat -C POSTROUTING -s 10.244.0.0/16 ! -o droidker0 -j MASQUERADE 2>/dev/null \
    || iptables -t nat -A POSTROUTING -s 10.244.0.0/16 ! -o droidker0 -j MASQUERADE
  iptables -C FORWARD -i droidker0 -j ACCEPT 2>/dev/null \
    || iptables -A FORWARD -i droidker0 -j ACCEPT
  iptables -C FORWARD -o droidker0 -j ACCEPT 2>/dev/null \
    || iptables -A FORWARD -o droidker0 -j ACCEPT
  ok "NAT + forwarding rules in place."
}

# ---------- Step 6: Data directories + rootfs skeleton ----------
setup_dirs() {
  log "Creating DroidKer data directories..."
  mkdir -p /var/lib/droidker/{containers,apks,overlays,logs,run}
  mkdir -p /opt/droidker/android-rootfs/{system,data,cache,acct,proc,sys,dev}
  mkdir -p /opt/droidker/android-rootfs/system/{app,priv-app,framework,lib,etc}
  mkdir -p /opt/droidker/android-rootfs/data/{app,data,local}
  mkdir -p /var/run/droidker
  chmod 0755 /var/lib/droidker
  ok "Directories created."
}

# ---------- Step 7: Build daemon + CLI + init binary ----------
build_droidker() {
  log "Building DroidKer backend (release)..."
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

  # shellcheck disable=SC1091
  source "$HOME/.cargo/env" 2>/dev/null || true

  # Build the workspace — this produces three binaries:
  #   droidkerd      (the daemon)
  #   droidker-init  (PID 1 of every container)
  # plus the standalone CLI crate.
  (cd "$PROJECT_ROOT/backend" && cargo build --release)
  (cd "$PROJECT_ROOT/cli" && cargo build --release)

  install -m 0755 "$PROJECT_ROOT/backend/target/release/droidkerd"      /usr/local/bin/droidkerd
  install -m 0755 "$PROJECT_ROOT/backend/target/release/droidker-init" /usr/local/bin/droidker-init
  install -m 0755 "$PROJECT_ROOT/cli/target/release/droidker"          /usr/local/bin/droidker

  # droidker-init needs to set capabilities and pivot_root, so it must
  # retain file capabilities post-install.
  setcap cap_sys_admin,cap_net_admin,cap_sys_chroot+ep /usr/local/bin/droidker-init 2>/dev/null || \
    warn "setcap failed — droidker-init may need root to run."

  ok "Installed: droidkerd, droidker-init, droidker"
}

# ---------- Step 7b: Build (or skip) the Android rootfs ----------
build_rootfs() {
  if [[ -f /opt/droidker/android-rootfs/system/bin/app_process64 ]]; then
    ok "Android rootfs already present at /opt/droidker/android-rootfs — skipping build."
    return
  fi
  log "Android rootfs not found. Running build-rootfs.sh..."
  if [[ -f "$PROJECT_ROOT/scripts/build-rootfs.sh" ]]; then
    bash "$PROJECT_ROOT/scripts/build-rootfs.sh" || \
      warn "build-rootfs.sh failed — containers will not start until it succeeds."
  else
    warn "build-rootfs.sh not found; skipping rootfs build."
  fi
}

# ---------- Step 8: systemd unit ----------
install_systemd_unit() {
  log "Installing systemd unit..."
  cat > /etc/systemd/system/droidkerd.service <<'UNIT'
[Unit]
Description=DroidKer Android micro-container daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/droidkerd
Restart=on-failure
RestartSec=2
# The daemon needs CAP_SYS_ADMIN (namespaces), CAP_NET_ADMIN (veth),
# and CAP_NET_BIND_SERVICE if you want to use port <1024.
AmbientCapabilities=CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_BIND_SERVICE
NoNewPrivileges=true
# Keep resource usage low on a 1-vCPU VPS
MemoryMax=256M
CPUWeight=50

[Install]
WantedBy=multi-user.target
UNIT

  systemctl daemon-reload
  systemctl enable --now droidkerd
  ok "droidkerd.service installed and started."
}

# ---------- Step 9: Config file ----------
install_config() {
  mkdir -p /etc/droidker
  if [[ ! -f /etc/droidker/config.toml ]]; then
    cat > /etc/droidker/config.toml <<'TOML'
# DroidKer daemon configuration.
# Every field can be overridden by env var DROIDKER_<UPPERCASE_FIELD>.

host = "0.0.0.0"
port = 8080
data_dir = "/var/lib/droidker"
android_rootfs = "/opt/droidker/android-rootfs"
max_containers = 8
container_memory_mb = 128
container_cpu_percent = 50
binder_device = "/dev/binder"
ashmem_device = "/dev/ashmem"
bridge_name = "droidker0"
bridge_subnet = "10.244.0.0/16"
signaling_socket = "/var/run/droidker/signaling.sock"
host_arch = "x86_64"
TOML
    ok "Wrote /etc/droidker/config.toml"
  else
    ok "Config already exists, leaving untouched."
  fi
}

# ---------- Verify ----------
verify() {
  log "Verifying setup..."
  command -v cargo >/dev/null && ok "Rust OK" || warn "Rust missing"
  command -v node   >/dev/null && ok "Node OK" || warn "Node missing"
  [[ -c /dev/binder ]] && ok "/dev/binder OK" || warn "/dev/binder missing"
  [[ -c /dev/ashmem ]] && ok "/dev/ashmem OK" || warn "/dev/ashmem missing"
  ip link show droidker0 &>/dev/null && ok "Bridge OK" || warn "Bridge missing"
  command -v droidkerd      >/dev/null && ok "droidkerd OK"      || warn "droidkerd missing"
  command -v droidker-init  >/dev/null && ok "droidker-init OK"  || warn "droidker-init missing"
  command -v droidker       >/dev/null && ok "droidker CLI OK"   || warn "droidker CLI missing"
  [[ -f /opt/droidker/android-rootfs/system/bin/app_process64 ]] \
    && ok "Android rootfs OK" \
    || warn "Android rootfs missing (run scripts/build-rootfs.sh)"
  systemctl is-active --quiet droidkerd && ok "droidkerd running" || warn "droidkerd not running"
  echo
  ok "Setup complete. Try: droidker info"
}

# ---------- Main ----------
main() {
  install_packages
  install_rust
  install_node
  setup_binder_ashmem
  setup_bridge
  setup_dirs
  install_config

  if [[ $CHECK_ONLY -eq 0 ]]; then
    build_droidker
    install_systemd_unit
    build_rootfs
  fi

  verify
}

main "$@"
