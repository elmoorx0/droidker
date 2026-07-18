# Milestone 2 — Real Linux Sandbox

This document tracks what M2 delivers, the design decisions behind it, and
how to smoke-test it on a fresh VPS.

## What M2 ships

### 1. Real Linux namespace isolation (`src/container/isolation.rs`)

Replaces the M1 stub with a full sandbox built from:

- **`unshare(1)`** with `--mount --pid --net --uts --ipc --user --map-root-user --fork`
  — the simplest, most debuggable way to create a fully-isolated child with
  its own PID/mount/net/UTS/IPC namespaces plus a user namespace that maps
  our uid to 0 inside.
- **Setsid + leak-the-child-handle** so the sandbox survives the daemon
  dying and gets reparented to init on daemon exit (the cgroup ensures
  cleanup happens on host shutdown).

### 2. cgroup v2 management (`src/container/cgroups.rs`)

Per-container cgroup at `/sys/fs/cgroup/droidker/container-<uuid>/` with:

- `memory.max`, `memory.high` (90% of max), `memory.swap.max` (0 — no swap)
- `cpu.max` (`$quota $period` — e.g. `50000 100000` = 50% of one core)
- `cpu.weight` (relative share)
- `pids.max` (256 by default — plenty for one Android app)
- `cgroup.freeze` / `cgroup.thaw` for pause/resume
- `memory.events` polling for OOM detection

Controllers are enabled at both the root and the `droidker` subtree, so
per-container cgroups under it can use them.

### 3. Per-container networking (`src/container/network.rs`)

Topology:

```
   host                           container netns
   ─────                          ───────────────
   droidker0 bridge ─── vethXXXX ─── eth0 (10.244.X.Y/16)
   10.244.0.1/16                    default route via 10.244.0.1
```

- IP allocation from a file-backed bitmap on the 10.244.0.0/16 pool
  (skips .0/.1/.255 of each /24)
- veth pair created via `ip link add ... type veth peer name vpeer`
- Peer moved into the container netns via `ip link set vpeer netns <pid>`
- Renamed to `eth0`, brought up, assigned IP + default route via `nsenter`
- Host-side veth enslaved to `droidker0` bridge

### 4. overlayfs rootfs (`src/container/rootfs.rs` + `src/bin/init.rs`)

Docker-style layering:

```
   /opt/droidker/android-rootfs      (lowerdir, RO, shared by all containers)
   /var/lib/droidker/overlays/<id>/upper    (upperdir, per-container RW)
   /var/lib/droidker/overlays/<id>/work     (workdir, overlay-internal)
   /var/lib/droidker/overlays/<id>/merged   (mountpoint, becomes new root)
```

`droidker-init` does:

1. `mount -t overlay overlay -o lowerdir=...,upperdir=...,workdir=... merged`
2. Bind-mount `/dev/binder`, `/dev/ashmem`, `/dev/null`, `/dev/zero`,
   `/dev/urandom`, `/dev/random`, `/dev/full`, `/dev/tty` into `merged/dev`
3. Mount a fresh `devpts` (with `newinstance,ptmxmode=0666`) → `merged/dev/pts`
4. Mount a fresh `procfs` → `merged/proc`
5. Mount a read-only `sysfs` → `merged/sys`
6. `pivot_root(merged, merged/.old_root)` then `umount2(.old_root, MNT_DETACH)`
7. `sethostname("droidker-<uuid-short>")`
8. Drop the capability bounding set (0..CAP_LAST_CAP)
9. Copy the APK into `/data/app/<package>/base.apk`
10. `execve("/system/bin/app_process64", ...)` with `BOOTCLASSPATH`,
    `CLASSPATH`, `ANDROID_DATA`, `ANDROID_ROOT`, `LD_LIBRARY_PATH` set

### 5. Skeleton rootfs builder (`scripts/make-skeleton-rootfs.sh`)

For dev/CI hosts where downloading a full Android system image isn't
practical, this script produces a minimal `/opt/droidker/android-rootfs`
that satisfies `validate_android_rootfs()` and lets the daemon spawn a
sandbox whose PID 1 is a stub `app_process64` shell script that just
sleeps. That's enough to test the namespace + cgroup + network plumbing
end-to-end without needing a real Android runtime.

For a real Android runtime, use `scripts/build-rootfs.sh` instead — it
downloads an Android-x86 (or LineageOS) system image, extracts `/system`,
strips Google proprietary apps, installs microG, and patches `build.prop`
for headless operation.

### 6. Seccomp profile (`src/seccomp.rs`)

Two profiles ship:

- **`AndroidRuntime`** (default): blocks ~30 dangerous syscalls
  (module loading, kexec, ptrace, bpf, setns, etc.) while permitting
  everything ART/Bionic needs.
- **`Strict`**: also blocks all socket/network syscalls — for apps that
  don't need network access (most test automation scenarios).
- **`Permissive`**: no filtering, for dev mode with `strace`.

The actual BPF install is stubbed for M2 — the policy + tests land now,
the BPF generator follows in M2.6 to keep the M2 PR reviewable.

## End-to-end smoke test

On a fresh Ubuntu 22.04 VPS:

```bash
# 1. Bootstrap the host (kernel modules, bridge, systemd unit)
sudo bash scripts/setup.sh

# 2. Build a skeleton rootfs (no Android image needed)
sudo bash scripts/make-skeleton-rootfs.sh

# 3. Build + install the daemon and CLI
cd backend && cargo build --release --bins
sudo cp target/release/droidkerd    /usr/local/bin/
sudo cp target/release/droidker-init /usr/local/bin/
cd ../cli && cargo build --release
sudo cp target/release/droidker /usr/local/bin/

# 4. Start the daemon
sudo systemctl enable --now droidkerd
sudo journalctl -u droidkerd -f

# In another shell:
# 5. Upload an APK (any APK — we won't actually run it, just smoke-test)
droidker upload ~/Downloads/test.apk

# 6. Create + start a container
droidker create test.apk --name smoke
droidker start smoke

# 7. Verify the sandbox is alive
droidker ps                      # should show smoke as Running
droidker inspect smoke           # should show PID + IP
sudo ls /proc/$(droidker inspect smoke -j | jq -r .pid)/root/  # should be the merged rootfs
sudo cat /sys/fs/cgroup/droidker/container-*/cgroup.procs      # should list the PID

# 8. Tear down
droidker stop smoke
droidker rm smoke
```

## Known gaps (deferred to M2.6 / M3)

1. **Seccomp BPF install** — policy + tests ship now; the actual BPF
   generator lands in M2.6.
2. **Port publishing** — `droidker run -p 8080:80` is not yet wired;
   M3 will add iptables/nft DNAT rules.
3. **APK parsing** — the package name is still extracted from the
   filename (`foo_bar.apk` → `foo.bar`). Real `aapt`-based parsing
   happens inside the sandbox once a real Android rootfs is present.
4. **ARM translation** — on x86_64 hosts, ARM APKs won't run without
   libhoudini/libndk. That's M6.
5. **Logging from inside the sandbox** — `/proc/<pid>/fd/1` captures
   stdout but the daemon doesn't yet redirect it to a per-container
   log file. M3 will add `nsenter`-based log capture.
