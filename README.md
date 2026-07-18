# DroidKer

> **Micro-container platform for Android APKs on low-resource VPS hosts.**
> Think Docker, but for Android apps — with a built-in Humanizer engine that
> drives them with realistic human-like input.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)
[![Milestone](https://img.shields.io/badge/Milestone-5-orange)](https://github.com/elmoorx0/droidker)
[![Backend: Rust](https://img.shields.io/badge/Backend-Rust-dea584)](https://www.rust-lang.org/)
[![Frontend: SvelteKit](https://img.shields.io/badge/Frontend-SvelteKit-ff3e00)](https://kit.svelte.dev/)

---

## 🎯 What is DroidKer?

DroidKer runs Android applications inside lightweight Linux sandboxes
(**micro-containers**) on a standard VPS. Each container:

- Is isolated via **Linux namespaces** (mount, PID, net, UTS, IPC) + **cgroup v2**
  resource limits.
- Shares a read-only Android rootfs (ART, Bionic libc, microG) so per-container
  disk usage is near zero.
- Talks to the host kernel's `binder` and `ashmem` devices — **no full Android
  kernel** is booted.
- Can be driven by the **Humanizer engine**, which injects touch/keyboard events
  through `/dev/input/eventX` using Bezier-curve swipes and Gaussian-delayed
  taps so automation looks human.

### Why?

- Run dozens of Android apps on a 1 GB / 1 vCPU VPS (impossible with full VMs).
- Drive Android apps for testing, scraping, or automation without paying for
  expensive cloud-Android providers.
- Keep everything isolated — one rogue app can't take down the host.

---

## 🏗️ Architecture (Milestone 1)

```
┌──────────────────────────────────────────────────────────────────┐
│                          VPS Host (1 vCPU / 1 GB)                │
│                                                                  │
│   ┌──────────────┐         ┌────────────────────────────────┐   │
│   │  SvelteKit   │  HTTP   │   droidkerd (Rust + Actix)     │   │
│   │  Dashboard   │ ──────► │   ┌──────────────────────────┐ │   │
│   │  :3000       │         │   │  ContainerManager        │ │   │
│   └──────────────┘         │   │   ├─ Isolator (ns+cgroup) │ │   │
│                            │   │   ├─ AndroidRuntime       │ │   │
│   ┌──────────────┐         │   │   └─ Humanizer Engine     │ │   │
│   │  droidker    │  HTTP   │   └──────────────────────────┘ │   │
│   │  CLI         │ ──────► │                                │   │
│   └──────────────┘         └────────────────────────────────┘   │
│                                            │                     │
│                            ┌───────────────┼──────────────┐     │
│                            ▼               ▼              ▼     │
│                       ┌────────┐     ┌────────┐     ┌────────┐  │
│                       │ cnt A  │     │ cnt B  │     │ cnt C  │  │
│                       │ns+cgrp │     │ns+cgrp │     │ns+cgrp │  │
│                       │ART+mG  │     │ART+mG  │     │ART+mG  │  │
│                       └────────┘     └────────┘     └────────┘  │
│                            │               │              │      │
│                            └───────┬───────┴──────────────┘     │
│                                    ▼                            │
│                          ┌──────────────────┐                   │
│                          │ droidker0 bridge │  ── NAT ──► eth0  │
│                          │  10.244.0.0/16   │                   │
│                          └──────────────────┘                   │
│                                                                  │
│                          Kernel: binder + ashmem loaded          │
└──────────────────────────────────────────────────────────────────┘
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the deep dive.

---

## 📁 Project Structure

```
droidker/
├── backend/                # Rust daemon (droidkerd)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs         # Entry point, HTTP server bootstrap
│       ├── api/            # Actix-Web route handlers
│       │   ├── containers.rs
│       │   ├── health.rs
│       │   └── upload.rs
│       ├── container/      # Container lifecycle
│       │   ├── manager.rs  #   in-memory registry + persistence
│       │   ├── isolation.rs#   namespace + cgroup + unshare orchestration
│       │   ├── cgroups.rs  #   cgroup v2 (memory/cpu/pids/freezer)
│       │   ├── network.rs  #   veth pair + bridge + IP allocation
│       │   ├── rootfs.rs   #   overlayfs layout + bind mounts
│       │   ├── runtime.rs  #   Android runtime invocation builder
│       │   └── bin/init.rs #   droidker-init (PID 1 inside container)
│       ├── config/         # Settings (env + TOML)
│       ├── models/         # Data types
│       ├── humanizer/      # Bezier swipes + Gaussian delays
│       ├── seccomp.rs      # Syscall blocklist profiles
│       ├── streaming/      # WebRTC scaffolding (M4)
│       └── error.rs
│
├── cli/                    # `droidker` command-line tool
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs         # clap parser
│       ├── client.rs       # HTTP client
│       ├── commands.rs     # one impl per subcommand
│       └── fmt.rs          # terminal table rendering
│
├── frontend/               # SvelteKit + TailwindCSS dashboard
│   ├── package.json
│   ├── svelte.config.js
│   ├── tailwind.config.js
│   └── src/
│       ├── app.css         # Tailwind layers + components
│       ├── routes/
│       │   ├── +layout.svelte
│       │   ├── +page.svelte        # main dashboard
│       │   ├── containers/+page.svelte
│       │   └── docs/+page.svelte
│       └── lib/
│           ├── api/api.ts          # typed REST client
│           ├── stores/containers.ts# reactive store
│           └── components/
│               ├── ContainerCard.svelte
│               ├── UploadPanel.svelte
│               └── StatusBadge.svelte
│
├── scripts/
│   ├── setup.sh            # VPS bootstrap (kernel modules, bridge, build, systemd)
│   └── build-rootfs.sh     # Build Android rootfs from an AOSP/LineageOS image
│
└── docs/
    └── ARCHITECTURE.md
```

---

## 🚀 Quickstart

### On a fresh VPS (Ubuntu 22.04 / Debian 12)

```bash
# 1. Clone the repo
git clone https://github.com/droidker/droidker.git
cd droidker

# 2. Run the bootstrap script (installs Rust, Node, kernel modules,
#    bridge, builds the binaries, installs a systemd unit)
sudo bash scripts/setup.sh

# 3. Verify
droidker info
# → ✓ DroidKer daemon
#     health:  ok
#     ready:   true
#     containers loaded: 0
```

### Launch your first container

```bash
# Upload + create + start in one step
droidker run ~/Downloads/my-app.apk --name my-app --memory 128 --cpu 50

# List
droidker ps

# Stop / start / remove
droidker stop my-app
droidker start my-app
droidker rm my-app
```

### Drive the container (M5 features)

```bash
# Raw touch — instant down+up, no humanization
droidker tap my-app 270 480

# Humanized tap — Bezier-jittered with Gaussian pressure
droidker htap my-app 270 480

# Humanized swipe — curved path with jittered timings
droidker hswipe my-app 100 800 100 200

# Humanized long-press — for context menus
droidker hlongpress my-app 270 480 800

# Take a single screenshot
droidker screenshot my-app --out shot.jpg

# Record a 30-second clip for CI artifacts
droidker record my-app --duration 30 --fps 5 --quality 70 --out ci.mjpeg
```

### Open the dashboard

The SvelteKit dashboard runs on port 3000 by default. Start it in dev mode:

```bash
cd frontend
npm install
npm run dev
# → http://your-vps-ip:3000
```

For production, build a static bundle and serve it behind nginx:

```bash
cd frontend && npm run build
# Output lands in frontend/build/ — serve with nginx, caddy, etc.
```

---

## 🔌 REST API

| Method   | Endpoint                                    | Description                          |
|----------|---------------------------------------------|--------------------------------------|
| `GET`    | `/api/v1/health`                            | Liveness probe                       |
| `GET`    | `/api/v1/ready`                             | Readiness + loaded container count   |
| `GET`    | `/api/v1/containers`                        | List all containers                  |
| `POST`   | `/api/v1/containers`                        | Create a container                   |
| `GET`    | `/api/v1/containers/{id}`                   | Inspect a container                  |
| `POST`   | `/api/v1/containers/{id}/start`             | Start a stopped container            |
| `POST`   | `/api/v1/containers/{id}/stop`              | Stop a running container             |
| `DELETE` | `/api/v1/containers/{id}`                   | Delete a stopped container           |
| `GET`    | `/api/v1/containers/{id}/logs/{kind}`       | Tail container logs (system/kernel)  |
| `GET`    | `/api/v1/containers/{id}/stats`             | Live CPU/memory/IO stats             |
| `POST`   | `/api/v1/containers/{id}/exec`              | Run a command in the sandbox (PTY)   |
| `GET`    | `/api/v1/containers/{id}/screen/ws`         | WebSocket: JPEG screen stream        |
| `POST`   | `/api/v1/containers/{id}/screen/touch`      | Inject a raw touch event             |
| `POST`   | `/api/v1/containers/{id}/screen/key`        | Inject Home/Back/Recent key          |
| `POST`   | `/api/v1/containers/{id}/screen/human/tap`       | Humanized tap (M5)              |
| `POST`   | `/api/v1/containers/{id}/screen/human/swipe`     | Humanized Bezier swipe (M5)     |
| `POST`   | `/api/v1/containers/{id}/screen/human/longpress` | Humanized long-press (M5)      |
| `GET`    | `/api/v1/containers/{id}/audio/ws`          | WebSocket: raw PCM audio stream (M5) |
| `POST`   | `/api/v1/upload/apk`                        | Upload an APK (multipart `file`)     |

---

## 🛣️ Roadmap

| Milestone | Status | Scope                                                              |
|-----------|--------|--------------------------------------------------------------------|
| **M1**    | ✅      | Project scaffold, API, CLI, dashboard, `setup.sh`                  |
| **M2**    | ✅      | Real namespace+cgroup sandbox, Android rootfs builder, `droidker-init` |
| **M2.6**  | ✅      | Seccomp BPF install in PID 1 + per-arch syscall tables             |
| **M3**    | ✅      | Per-container detail page, log streaming, `exec` into sandbox (PTY), port publishing |
| **M4**    | ✅      | MJPEG screen streaming over WebSocket + uinput virtual touchscreen  |
| **M5**    | ✅      | Humanizer wiring (Bezier+Gaussian → uinput), /dev/input bind-mount, audio WS, `droidker record` |
| **M6**    | 🔜     | Opus audio, nsenter screenrecord, pinch-zoom, ARM → x86_64 translation |

---

## ⚙️ Configuration

All settings can be overridden via env vars (`DROIDKER_<FIELD>`) or a TOML
file at `/etc/droidker/config.toml`. Defaults are tuned for 1 GB / 1 vCPU:

```toml
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
```

---

## 🧪 Development

```bash
# Backend (hot reload with cargo-watch)
cd backend
cargo install cargo-watch
cargo watch -x run

# Frontend
cd frontend
npm install
npm run dev

# Run tests
cd backend && cargo test
```

---

## ⚠️ Disclaimer

DroidKer is for **research and automation on apps you own or have permission
to automate**. The Humanizer engine exists to make test automation more
realistic; using it to bypass anti-bot protections on services you don't own
may violate their ToS. The authors take no responsibility for misuse.

---

## 📜 License

MIT — see [LICENSE](LICENSE).
