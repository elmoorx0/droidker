# DroidKer Architecture

This document describes the internal design of DroidKer as of Milestone 1,
and how each piece will evolve in subsequent milestones.

## 1. Design constraints

The whole system is shaped by three hard constraints:

1. **Run on a 1 GB / 1 vCPU VPS** — every component must be miserly with RAM
   and CPU. This rules out JVM-based control planes (a stock JVM daemon alone
   can easily eat 300+ MB) and pushes us toward Rust for the backend.
2. **Isolate untrusted Android apps** — apps see only their own filesystem,
   PID namespace, network, and IPC. The host kernel is shared (no KVM), so
   isolation relies entirely on Linux namespaces + cgroups + seccomp.
3. **Drive apps as if by a human finger** — touch injection must be
   indistinguishable from real input. This rules out `adb shell input tap`
   (which fires perfect-grid coordinates with zero jitter) and pushes us to
   write directly to `/dev/input/eventX` with Bezier paths and Gaussian
   delays.

## 2. Component overview

### 2.1 Backend daemon (`droidkerd`)

- **Language:** Rust (stable).
- **Framework:** Actix-Web 4 (chosen over Axum for mature multipart + CORS
  support and a stable actor model for the streaming module in M4).
- **Why Rust?** A typical Actix-Web process idles at ~10 MB resident. The
  Go equivalent (Fiber) sits around 25–40 MB. On a 1 GB VPS where we want to
  run 8 containers each capped at 128 MB, those extra 30 MB matter.
- **Worker model:** 2 actix workers (configurable). On a 1 vCPU host, more
  workers just cause context-switch overhead.

The daemon owns a single `ContainerManager` (in `container/manager.rs`)
protected by a `RwLock<HashMap<Uuid, Container>>`. Mutating operations take
the write lock briefly to update state, then drop it before doing the heavy
lifting (fork, mount, etc.) so the API stays responsive.

### 2.2 Isolation layer (`container/isolation.rs` + `cgroups.rs` + `network.rs` + `rootfs.rs`)

Each container is built from the following Linux primitives:

| Concern           | Primitive                                             |
|-------------------|-------------------------------------------------------|
| Filesystem        | `mount` namespace + `pivot_root` into an overlayfs    |
| Processes         | `PID` namespace (container sees only its own children)|
| Network           | `NET` namespace + `veth` pair bridged to `droidker0`  |
| Hostname          | `UTS` namespace                                       |
| IPC               | `IPC` namespace (System V IPC + POSIX message queues) |
| Resource limits   | cgroup v2 (`memory.max`, `cpu.max`, `pids.max`)       |
| Syscall filter    | seccomp profile (`AndroidRuntime` by default)         |
| Capabilities      | bounding set dropped to empty before exec             |

**Milestone 2 status:** Full sandbox pipeline is wired up:

1. `ContainerManager::start()` builds the `IsolationSpec` and calls
   `Isolator::prepare_sandbox()`.
2. `Isolator` creates the overlayfs layout (`rootfs.rs`), creates the
   per-container cgroup (`cgroups.rs`), allocates an IP from the
   `10.244.0.0/16` pool (`network.rs`), then forks `unshare --mount --pid
   --net --uts --ipc --user --map-root-user --fork /usr/local/bin/droidker-init`.
3. `droidker-init` (in `src/bin/init.rs`) runs as PID 1 inside the new
   namespaces. It mounts the overlayfs, bind-mounts `/dev/binder`,
   `/dev/ashmem`, `/dev/null`, `/dev/zero`, `/dev/urandom`, mounts a fresh
   `procfs` and `sysfs`, calls `pivot_root` into the merged view, drops the
   capability bounding set, and `execve`s `/system/bin/app_process64`.
4. Back in the daemon, `Isolator` moves the child PID into the cgroup,
   creates the veth pair (`vethXXXX` ↔ `eth0`), enslaves the host end to
   `droidker0`, and assigns the IP inside the child netns via `nsenter`.
5. The child PID is recorded in the container state and returned to the API.

The Android rootfs is built by `scripts/build-rootfs.sh`, which downloads
an Android-x86 (or LineageOS) system image, extracts `/system`, strips
proprietary Google apps, installs microG as a system priv-app, and patches
`build.prop` for headless operation (no boot animation, ART in interpreter
mode, fake device identifiers).

### 2.3 Seccomp filter (`seccomp.rs`)

Two blocklist profiles ship:

- **`AndroidRuntime`** (default): permits everything ART/Bionic needs,
  blocks ~25 dangerous syscalls (init_module, kexec_load, ptrace, bpf,
  setns, unshare, swap ops, time setters, etc.).
- **`Strict`**: adds network-related syscalls (socket, connect, bind, ...)
  for apps that don't need any network access.

The actual BPF program install lands in a follow-up patch (currently
the stub writes a marker file so tests can verify the call path); the
policy data structures are already in place and unit-tested.

### 2.4 Android runtime (`container/runtime.rs`)

The shared Android rootfs lives at `/opt/droidker/android-rootfs` and is
**read-only bind-mounted** into every container. It contains:

- `/system/lib(64)/` — Bionic libc, libart, libbinder, libutils, …
- `/system/framework/` — ART boot artifacts (`boot.art`, `boot.oat`)
- `/system/app/`, `/system/priv-app/` — system apps (Settings, SystemUI)
- `/system/etc/` — permissions, configs
- microG services (replaces proprietary GMS)

Per-container writable state lives in an overlay upperdir at
`/var/lib/droidker/overlays/<container-id>/`. The APK is installed into
`/data/app/<package>/` inside the overlay.

**Entry point:** `app_process` (the ART launcher) is exec'd as PID 1 inside
the sandbox. Boot args follow the standard Android `zygote` invocation, minus
the parts that require `init` to be running (we substitute a tiny init stub
written in Rust that reaps zombies and forwards signals).

### 2.5 Humanizer engine (`humanizer/input.rs`)

The math is already implemented and unit-tested in M1:

- **`BezierPath`** — quadratic Bezier (start, control, end). Control point is
  offset perpendicular to the swipe axis by a randomized `curvature` value,
  producing natural arcs instead of straight lines.
- **`human_delay_ms(base, stddev)`** — Box-Muller transform on a xorshift64
  PRNG. Returns a Gaussian-distributed delay, clamped to ≥ 1 ms.
- **`swipe_step_count(distance_px)`** — derives duration from a randomized
  human swipe speed (400–800 px/s), then samples at 60 Hz.

Why xorshift instead of `rand` crate? Two reasons:
1. Smaller binary (no need to pull in `rand` + `rand_chacha`).
2. Deterministic given a seed, which is critical for **record/replay** of
   automation sessions (landing in M5).

**M1 status:** Math is live + unit-tested. The actual `/dev/input/eventX`
writer lands in M4 once we have a virtual touchscreen per container.

### 2.6 WebRTC streaming (`streaming/mod.rs`)

Placeholder module. In M4 this will:

1. Read SurfaceFlinger output via a virtual HWC display bound to the container.
2. Encode to H.264 using:
   - Hardware VA-API if the VPS has an Intel/AMD GPU (rare but worth probing).
   - Software x264 with `preset=ultrafast tune=zerolatency` otherwise.
3. Pump frames through a `webrtc-rs` PeerConnection.
4. Expose `/api/v1/containers/{id}/stream/offer` and `/answer` for SDP
   exchange.

WebRTC was chosen over VNC because:
- VNC sends uncompressed pixels — eats ~5 Mbps per container even when idle.
- WebRTC does VP8/H.264 in-band with adaptive bitrate, ~500 kbps per stream.
- Native browser support, no plugin.

### 2.7 CLI (`droidker`)

The CLI is a thin HTTP client over the daemon's REST API. It has no direct
access to the kernel or to container state — every command round-trips
through `droidkerd`. This means:

- You can manage containers on a remote VPS from your laptop by setting
  `DROIDKER_HOST=https://my-vps.example.com:8080`.
- The CLI binary is tiny (~5 MB stripped) and can be scp'd anywhere.

### 2.8 Frontend dashboard

- **Framework:** SvelteKit 2 + Svelte 4 (Svelte 5's runes weren't stable at
  the time of writing).
- **Styling:** TailwindCSS 3 with a custom dark palette tuned for ops work
  on low-end screens.
- **SSR:** disabled (`+layout.ts` exports `ssr = false`). The dashboard is a
  pure SPA that talks to the daemon over REST. This keeps the deploy story
  simple — `npm run build` produces a static bundle that nginx can serve.
- **Polling:** the container store polls `GET /containers` every 3 seconds.
  In M4 we'll switch to WebSocket push to cut idle traffic.

## 3. Networking

```
   ┌──────────────────────────────────────────────┐
   │                  VPS Host                    │
   │                                              │
   │  eth0 (public IP)                            │
   │   │                                          │
   │   │ MASQUERADE                               │
   │   ▼                                          │
   │  droidker0 bridge (10.244.0.1/16)            │
   │   │                                          │
   │   ├── vethA ──► eth0 (10.244.0.2) container A│
   │   ├── vethB ──► eth0 (10.244.0.3) container B│
   │   └── vethC ──► eth0 (10.244.0.4) container C│
   └──────────────────────────────────────────────┘
```

- IP allocation: deterministic from the container UUID (see `isolation.rs`).
  In M2 we'll switch to a proper allocator that tracks assignments across
  restarts.
- Outbound traffic: MASQUERADE on the host's `eth0`.
- Inbound traffic: per-container port forwarding (M3 feature, similar to
  `docker -p`).

## 4. Threat model

| Threat                                  | Mitigation                                              |
|-----------------------------------------|---------------------------------------------------------|
| App escapes its container               | namespaces + seccomp + dropped caps (M2)                |
| App consumes all host RAM               | `memory.max` cgroup per container                       |
| App pins the CPU                        | `cpu.max` cgroup per container                          |
| App probes the host network             | Separate `NET` namespace; only bridge + NAT             |
| App exploits binder                     | Per-container binder context (binderfs)                 |
| Multiple apps share `/dev/ashmem`       | `NEWIPC` namespace + per-container ashmem fd table      |
| Malicious APK exfiltrates host files    | `pivot_root` into per-container overlay; rootfs RO bind |
| Bot detection flags automation          | Humanizer engine (Bezier + Gaussian)                    |

## 5. Performance budget (1 GB / 1 vCPU target)

| Component                       | RAM (idle) | CPU (idle) |
|---------------------------------|------------|------------|
| `droidkerd`                     | ~10 MB     | ~0%        |
| SvelteKit dashboard (nginx)     | ~20 MB     | ~0%        |
| Per-container runtime (ART)     | ~80 MB     | ~1%        |
| Per-container headroom          | ~48 MB     | varies     |
| **Total with 8 containers**     | **~900 MB**| **<30%**   |

These numbers are targets — the actual ART memory footprint depends heavily
on the app. We benchmark with real-world apps in M2.

## 6. Why not just use Anbox / Waydroid?

| Feature                  | Anbox         | Waydroid      | DroidKer                |
|--------------------------|---------------|---------------|-------------------------|
| Container model          | One big VM    | One session   | N independent sandboxes |
| Per-app isolation        | ✗             | ✗             | ✓                       |
| Headless VPS friendly    | ✗ (needs LXC) | ✗ (needs Wayland) | ✓ (no display server)|
| Memory per app           | ~300 MB shared| ~250 MB shared| ~128 MB isolated        |
| Multi-tenant             | ✗             | ✗             | ✓                       |
| Humanizer engine         | ✗             | ✗             | ✓ (M5)                  |
| WebRTC remote display    | ✗             | ✗             | ✓ (M4)                  |

DroidKer is **not** a desktop Android — it's a headless multi-tenant runtime
for automation, testing, and app-driven scraping.
