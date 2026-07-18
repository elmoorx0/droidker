// src/container/manager.rs
//
// `ContainerManager` is the single source of truth for every container on the
// host. It exposes high-level operations (create/start/stop/list/...) and
// coordinates the isolation and runtime subsystems.
//
// Concurrency model:
//   - A single `RwLock<HashMap<Uuid, Container>>` holds the live state.
//   - Mutating operations (start/stop) acquire a write lock briefly, then drop
//     it before doing the heavy lifting so the API stays responsive.
//   - Per-container state files on disk act as the durable source of truth.

use crate::config::Settings;
use crate::container::isolation::{IsolationSpec, Isolator};
use crate::container::runtime::{AndroidRuntime, RuntimeSpec};
use crate::error::{DroidkerError, Result};
use crate::models::{Container, ContainerStatus, ContainerSummary, CreateContainerRequest};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use tokio::fs;
use uuid::Uuid;

pub struct ContainerManager {
    settings: Settings,
    containers: RwLock<HashMap<Uuid, Container>>,
    state_file: PathBuf,
}

impl ContainerManager {
    pub fn new(settings: Settings) -> Result<Self> {
        settings.ensure_dirs()?;
        let state_file = settings.data_dir.join("run").join("state.json");

        let mut mgr = Self {
            settings: settings.clone(),
            containers: RwLock::new(HashMap::new()),
            state_file,
        };
        mgr.load_state()?;
        tracing::info!(
            "ContainerManager initialized with {} container(s) on disk",
            mgr.containers.read().unwrap().len()
        );
        Ok(mgr)
    }

    /// Create a new container record (does not start it).
    pub async fn create(&self, req: CreateContainerRequest) -> Result<Container> {
        // --- Validate capacity ------------------------------------------------
        {
            let map = self.containers.read().unwrap();
            if map.len() >= self.settings.max_containers {
                return Err(DroidkerError::BadRequest(format!(
                    "max_containers limit ({}) reached",
                    self.settings.max_containers
                )));
            }
        }

        // --- Validate APK -----------------------------------------------------
        let apk_path = self.settings.data_dir.join("apks").join(&req.apk);
        if !apk_path.exists() {
            return Err(DroidkerError::InvalidApk(format!(
                "APK not found: {}",
                apk_path.display()
            )));
        }

        let apk_bytes = fs::read(&apk_path).await?;
        let mut hasher = Sha256::new();
        hasher.update(&apk_bytes);
        let apk_sha = hex::encode(hasher.finalize());

        // --- Generate identity -----------------------------------------------
        let id = Uuid::new_v4();
        let name = req
            .name
            .unwrap_or_else(|| format!("droid-{}", &id.to_string()[..8]));

        // Check name uniqueness
        {
            let map = self.containers.read().unwrap();
            if map.values().any(|c| c.name == name) {
                return Err(DroidkerError::AlreadyExists(format!(
                    "container name '{}' is in use",
                    name
                )));
            }
        }

        // --- Build container record -----------------------------------------
        let rootfs = self.settings.data_dir.join("overlays").join(id.to_string());
        fs::create_dir_all(&rootfs).await?;

        // Extract a basic package name from APK filename (proper parsing happens
        // in the runtime layer once `aapt` is available inside the sandbox).
        let package = req
            .apk
            .trim_end_matches(".apk")
            .replace('_', ".");

        // Validate the requested arch up-front so a typo doesn't waste a
        // rootfs overlay directory. `None` is the default (host native).
        let arch = match req.arch.as_deref() {
            None => None,
            Some(s) => Some(
                crate::container::translation::Arch::parse(s)
                    .map_err(|e| DroidkerError::BadRequest(e))?
                    .as_str()
                    .to_string(),
            ),
        };

        let now = Utc::now();
        let container = Container {
            id,
            name: name.clone(),
            package,
            apk_sha256: apk_sha,
            status: ContainerStatus::Created,
            pid: 0,
            memory_mb: req.memory_mb.unwrap_or(self.settings.container_memory_mb),
            cpu_percent: req.cpu_percent.unwrap_or(self.settings.container_cpu_percent),
            rootfs,
            ip: None,
            veth_host: None,
            ports: req.ports,
            arch: arch.clone(),
            translation: None,
            created_at: now,
            updated_at: now,
            notes: req.notes,
        };

        // Persist state
        {
            let mut map = self.containers.write().unwrap();
            map.insert(id, container.clone());
        }
        self.persist_state()?;

        tracing::info!(container_id = %id, name = %name, "Container created");
        Ok(container)
    }

    /// Start a container (fork the runtime process inside a sandbox).
    pub async fn start(&self, id: Uuid) -> Result<Container> {
        // Snapshot the container under a short lock, then release.
        let snapshot = {
            let map = self.containers.read().unwrap();
            map.get(&id)
                .cloned()
                .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?
        };

        if snapshot.status == ContainerStatus::Running {
            return Err(DroidkerError::InvalidState(
                "container is already running".into(),
            ));
        }

        // --- Locate the APK in the data dir ---------------------------------
        let apk_path = self.settings.data_dir.join("apks").join(format!(
            "{}.apk",
            snapshot.apk_sha256
        ));

        // --- Build the runtime spec (resolved binary + args) ----------------
        let runtime = AndroidRuntime::new(self.settings.clone());
        let rt_spec = RuntimeSpec {
            container_id: snapshot.id,
            package: snapshot.package.clone(),
            apk_sha256: snapshot.apk_sha256.clone(),
            rootfs_overlay_dir: self.settings.data_dir.join("overlays"),
            apk_path: apk_path.clone(),
        };
        let (runtime_bin, runtime_args) = runtime.build_invocation(&rt_spec)?;

        // --- Build isolation spec --------------------------------------------
        // M5: create the InputInjector up-front so we can pass its eventN
        // path to droidker-init, which bind-mounts it into the container's
        // /dev/input/event0 before Android's EventHub scans for devices.
        //
        // We default to 540x960 (qHD) — the same resolution the screen
        // streamer uses, so injected touches land on the right pixel
        // coordinates the user sees in the browser.
        let injector = crate::streaming::input::InputInjector::new(
            snapshot.id,
            540,
            960,
        )?;
        // Wait up to 500 ms for the kernel to allocate /dev/input/eventN.
        // On a 1-vCPU VPS under load this sometimes takes 100–200 ms.
        let input_event = injector.wait_for_event_path(500);
        // Stash the injector in the global registry so /screen/touch and
        // /screen/key API calls find it without re-creating the uinput dev.
        if let Some(path) = &input_event {
            tracing::info!(
                container_id = %snapshot.id,
                event_path = %path.display(),
                "virtual touchscreen registered"
            );
        } else {
            tracing::warn!(
                container_id = %snapshot.id,
                "InputInjector created but no /dev/input/eventN node found yet; \
                 input device will not be bind-mounted into the container"
            );
        }
        let injector_arc = std::sync::Arc::new(tokio::sync::Mutex::new(injector));
        {
            let mut injectors = crate::api::screen::INJECTORS.lock().unwrap();
            injectors.insert(snapshot.id, injector_arc);
        }

        // --- M6: resolve translation strategy --------------------------------
        // Parse the container's `arch` field (if any) and probe the host for
        // an available translator (libhoudini → libndk_translation → qemu-user
        // → none). The resulting `TranslationStrategy` is threaded through
        // `IsolationSpec` so droidker-init can bind-mount the translator's
        // `.so` files and inject the right LD_PRELOAD.
        let host_arch = crate::container::translation::Arch::detect_host();
        let target_arch = match snapshot.arch.as_deref() {
            None => None,
            Some(s) => Some(
                crate::container::translation::Arch::parse(s)
                    .map_err(|e| DroidkerError::BadRequest(e))?,
            ),
        };
        let (resolved_arch, strategy) =
            crate::container::translation::build_translation_plan(host_arch, target_arch);
        tracing::info!(
            container_id = %snapshot.id,
            host_arch = %host_arch,
            target_arch = %resolved_arch,
            strategy = strategy.as_str(),
            "translation plan resolved"
        );
        let translation_plan = crate::container::translation::TranslationPlan {
            target_arch: resolved_arch,
            strategy,
        };
        // Snapshot the strategy string before we move `translation_plan`
        // into `iso_spec` — we'll persist it into the container record
        // after the sandbox has been prepared.
        let strategy_str = translation_plan.strategy.as_str().to_string();
        let target_arch_str = translation_plan.target_arch.as_str().to_string();

        let iso_spec = IsolationSpec {
            container_id: snapshot.id,
            rootfs_overlay_dir: self.settings.data_dir.join("overlays"),
            android_rootfs: self.settings.android_rootfs.clone(),
            memory_mb: snapshot.memory_mb,
            cpu_percent: snapshot.cpu_percent,
            bridge_name: self.settings.bridge_name.clone(),
            bridge_subnet: self.settings.bridge_subnet.clone(),
            binder_device: self.settings.binder_device.clone(),
            ashmem_device: self.settings.ashmem_device.clone(),
            hostname: format!("droidker-{}", &snapshot.id.to_string()[..8]),
            runtime_bin,
            runtime_args,
            input_event: input_event.clone(),
            // M6: resolve translation strategy from the requested arch +
            // the running host's arch. The resolved strategy + extra env
            // vars are passed to droidker-init via the environment so it
            // can bind-mount the translator's `.so` files before pivot_root.
            translation: translation_plan,
        };

        // Apply namespace + cgroup isolation, returning the new PID.
        let isolator = Isolator::new(self.settings.clone());
        let (pid, veth_host, ip) = isolator.prepare_sandbox(&iso_spec)?;

        // --- Write a runtime marker for diagnostics -------------------------
        runtime.launch(&rt_spec).await?;

        // --- Update state ----------------------------------------------------
        let updated = {
            let mut map = self.containers.write().unwrap();
            let c = map
                .get_mut(&id)
                .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?;
            c.status = ContainerStatus::Running;
            c.pid = pid;
            c.ip = Some(ip);
            c.veth_host = Some(veth_host);
            // M6: persist the resolved arch + translation strategy so the
            // CLI / dashboard can surface "running on x86_64 via libhoudini"
            // info. Also overwrite `arch` if it was unset so future starts
            // remember the host-native choice.
            c.arch = Some(target_arch_str);
            c.translation = Some(strategy_str);
            c.updated_at = Utc::now();
            c.clone()
        };
        self.persist_state()?;

        // --- Publish ports (iptables DNAT rules) ----------------------------
        // Done *after* state is persisted so the container is fully "Running"
        // before any traffic can reach it. Failures here are best-effort —
        // the container is still usable inside the bridge network.
        if !updated.ports.is_empty() {
            if let Err(e) = crate::container::ports::publish_all(&updated) {
                tracing::warn!(
                    container_id = %id,
                    error = %e,
                    "port publishing failed (iptables missing or no CAP_NET_ADMIN?)"
                );
            }
        }

        tracing::info!(container_id = %id, pid, "Container started");
        Ok(updated)
    }

    /// Stop a container (SIGTERM → wait → SIGKILL fallback, then cleanup).
    pub async fn stop(&self, id: Uuid) -> Result<Container> {
        // Snapshot the parts we need *before* taking the write lock, so the
        // heavy work (signal, wait, net teardown, cgroup destroy) doesn't
        // block other readers.
        let (pid, ip_owned, veth_owned) = {
            let map = self.containers.read().unwrap();
            let c = map
                .get(&id)
                .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?;
            if c.status != ContainerStatus::Running {
                return Err(DroidkerError::InvalidState(format!(
                    "cannot stop container in state {:?}",
                    c.status
                )));
            }
            (
                c.pid,
                c.ip.clone(),
                c.veth_host.clone(),
            )
        };

        // --- 1. SIGTERM the container's PID 1 --------------------------------
        if pid > 0 {
            tracing::debug!(container_id = %id, pid, "sending SIGTERM");
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }

        // --- 2. Wait up to 5s for graceful exit, then SIGKILL the whole pgrp
        let exited = wait_for_exit(pid, std::time::Duration::from_secs(5));
        if !exited {
            tracing::warn!(container_id = %id, pid, "graceful exit timed out; sending SIGKILL to pgroup");
            // Kill the entire process group so children of PID 1 die too.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-(pid as i32)),
                nix::sys::signal::Signal::SIGKILL,
            );
            // Best-effort reap.
            let _ = nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(pid as i32),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            );
        }

        // --- 3. Tear down published ports (iptables rules) -------------------
        // Do this *before* releasing the IP, otherwise the iptables -D
        // commands would still reference the IP that's no longer allocated.
        crate::container::ports::unpublish_all(id);

        // --- 4. Tear down networking ----------------------------------------
        if let Some(veth) = veth_owned.as_deref() {
            let net_cfg = crate::container::network::NetworkConfigurator::new(
                &self.settings.bridge_name,
                &self.settings.bridge_subnet,
            );
            if let Err(e) = net_cfg.teardown(veth) {
                tracing::warn!(container_id = %id, error = %e, "veth teardown failed");
            }
        }
        if let Some(ip) = ip_owned.as_deref() {
            let allocator = crate::container::network::IpAllocator::new(
                &self.settings.data_dir.join("run"),
            );
            if let Err(e) = allocator.release(ip) {
                tracing::warn!(container_id = %id, error = %e, "IP release failed");
            }
        }

        // --- 5. Destroy the cgroup ------------------------------------------
        // (Cgroup destruction is best-effort; if it fails there are likely
        // still zombie processes that need reaping. systemd will clean up
        // on daemon restart.)
        let cg_path = std::path::Path::new("/sys/fs/cgroup")
            .join("droidker")
            .join(format!("container-{}", id));
        if cg_path.exists() {
            if let Err(e) = std::fs::remove_dir(&cg_path) {
                tracing::warn!(container_id = %id, error = %e, "cgroup destroy failed");
            }
        }

        // --- 6. Update state -------------------------------------------------
        let updated = {
            let mut map = self.containers.write().unwrap();
            let c = map
                .get_mut(&id)
                .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?;
            c.status = ContainerStatus::Stopped;
            c.pid = 0;
            c.ip = None;
            c.veth_host = None;
            c.updated_at = Utc::now();
            c.clone()
        };
        self.persist_state()?;
        tracing::info!(container_id = %id, "Container stopped");
        Ok(updated)
    }

    /// Delete a container (must be stopped first).
    pub async fn delete(&self, id: Uuid) -> Result<()> {
        let removed = {
            let mut map = self.containers.write().unwrap();
            let c = map
                .get(&id)
                .ok_or_else(|| DroidkerError::NotFound(id.to_string()))?;
            if c.status == ContainerStatus::Running {
                return Err(DroidkerError::InvalidState(
                    "container is running; stop it first".into(),
                ));
            }
            map.remove(&id).unwrap()
        };

        // Best-effort cleanup of the overlay directory.
        let _ = tokio::fs::remove_dir_all(&removed.rootfs).await;
        self.persist_state()?;
        tracing::info!(container_id = %id, "Container deleted");
        Ok(())
    }

    /// List all containers (lightweight summaries).
    pub fn list(&self) -> Vec<ContainerSummary> {
        let map = self.containers.read().unwrap();
        map.values().map(ContainerSummary::from).collect()
    }

    /// Fetch a single container by ID.
    pub fn get(&self, id: Uuid) -> Option<Container> {
        self.containers.read().unwrap().get(&id).cloned()
    }

    /// Find a container by name.
    pub fn get_by_name(&self, name: &str) -> Option<Container> {
        self.containers
            .read()
            .unwrap()
            .values()
            .find(|c| c.name == name)
            .cloned()
    }

    // ----- Persistence --------------------------------------------------------

    fn persist_state(&self) -> Result<()> {
        let map = self.containers.read().unwrap();
        let json = serde_json::to_string_pretty(&*map)?;
        std::fs::write(&self.state_file, json)?;
        Ok(())
    }

    fn load_state(&mut self) -> Result<()> {
        if !self.state_file.exists() {
            return Ok(());
        }
        let json = std::fs::read_to_string(&self.state_file)?;
        let map: HashMap<Uuid, Container> = serde_json::from_str(&json)?;

        // Any container that was "Running" when the daemon died is now defunct.
        let map = map
            .into_iter()
            .map(|(k, mut v)| {
                if v.status == ContainerStatus::Running {
                    v.status = ContainerStatus::Exited;
                    v.pid = 0;
                    v.ip = None;
                    v.veth_host = None;
                }
                (k, v)
            })
            .collect();

        *self.containers.write().unwrap() = map;
        Ok(())
    }
}

/// Poll /proc/<pid> every 50ms to detect process exit. Returns true if the
/// process exited within `timeout`. This is preferred over waitpid() in the
/// daemon because we are not the parent of the sandbox PID 1 (the `unshare`
/// helper is — and it has already exited by the time we get here, so the
/// child has been reparented to us or to init).
fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    let proc_path = std::path::Path::new("/proc").join(pid.to_string());
    while start.elapsed() < timeout {
        if !proc_path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !proc_path.exists()
}
