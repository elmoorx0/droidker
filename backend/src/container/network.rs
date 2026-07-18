// src/container/network.rs
//
// Per-container networking on the droidker0 bridge.
//
// Topology:
//
//     ┌──── host ────┐
//     │              │
//     │   droidker0  │ 10.244.0.1/16 (bridge)
//     │      ▲       │
//     │      │       │
//     │   vethXXXX   │ (host-side veth endpoint, enslaved to bridge)
//     │      │       │
//     ╞══════╪═══════╡ ── netns boundary ──
//     │      │       │
//     │   eth0       │ (container-side veth, inside netns)
//     │ 10.244.X.Y   │
//     └──────────────┘
//
// Operations performed here:
//   1. Allocate a unique IP from the 10.244.0.0/16 pool (file-backed bitmap)
//   2. Create a veth pair: vethXXXX (host) <-> eth0 (container)
//   3. Move eth0 into the container's network namespace (using its PID)
//   4. Attach vethXXXX to the droidker0 bridge
//   5. Configure eth0 inside the netns: IP + default route via 10.244.0.1
//   6. Add iptables/nft DNAT rules for any published ports (M3)
//
// All shell-outs use `ip` and `bridge` from iproute2. Doing this in pure
// libc would be ~500 lines of netlink code; the shell-out is cheaper to
// maintain and the iproute2 binary is already required by setup.sh.

use crate::error::{DroidkerError, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

/// The default interface name inside every container (mirrors Docker).
pub const CONTAINER_IFNAME: &str = "eth0";

/// Allocation pool for container IPs.
pub struct IpAllocator {
    state_file: PathBuf,
    /// 10.244.0.0/16 — last two octets are variable.
    /// We skip .0 (network), .1 (gateway), .255 (broadcast) per /24 chunk.
    base: [u8; 2],
}

#[derive(Debug, Clone)]
pub struct NetHandle {
    pub host_veth: String,
    pub container_if: String,
    pub ip: String,
    pub gateway: String,
    pub prefix: u8,
}

impl IpAllocator {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_file: state_dir.join("ip-alloc.json"),
            base: [10, 244],
        }
    }

    /// Reserve the next free IP. The state file is a JSON array of "x.y"
    /// suffixes that are currently in use.
    pub fn allocate(&self, container_id: Uuid) -> Result<String> {
        let used: Vec<String> = if self.state_file.exists() {
            serde_json::from_str(&fs::read_to_string(&self.state_file)?)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Iterate 10.244.1.2 .. 10.244.255.254 skipping .0, .1, .255
        for third in 1..=255u8 {
            for fourth in 2..=254u8 {
                let suffix = format!("{third}.{fourth}");
                if used.contains(&suffix) {
                    continue;
                }
                let ip = format!("{}.{}", self.base_dot(), suffix);
                self.record(&suffix, container_id)?;
                return Ok(ip);
            }
        }
        Err(DroidkerError::Internal(
            "IP pool exhausted (10.244.0.0/16)".into(),
        ))
    }

    /// Release a previously-allocated IP.
    pub fn release(&self, ip: &str) -> Result<()> {
        if !self.state_file.exists() {
            return Ok(());
        }
        let mut used: Vec<String> =
            serde_json::from_str(&fs::read_to_string(&self.state_file)?)
                .unwrap_or_default();
        let suffix = ip
            .splitn(4, '.')
            .skip(2)
            .collect::<Vec<_>>()
            .join(".");
        used.retain(|s| s != &suffix);
        fs::write(&self.state_file, serde_json::to_string_pretty(&used)?)?;
        Ok(())
    }

    fn record(&self, suffix: &str, _container_id: Uuid) -> Result<()> {
        let mut used: Vec<String> = if self.state_file.exists() {
            serde_json::from_str(&fs::read_to_string(&self.state_file)?)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        used.push(suffix.to_string());
        fs::write(&self.state_file, serde_json::to_string_pretty(&used)?)?;
        Ok(())
    }

    fn base_dot(&self) -> String {
        format!("{}.{}", self.base[0], self.base[1])
    }
}

/// Configure networking for a container identified by its PID (which has
/// already been cloned into a new netns).
pub struct NetworkConfigurator {
    bridge: String,
    gateway: String,
    prefix: u8,
}

impl NetworkConfigurator {
    pub fn new(bridge: &str, subnet_cidr: &str) -> Self {
        // Parse "10.244.0.0/16" → gateway = 10.244.0.1, prefix = 16.
        let (net, prefix_str) = subnet_cidr.split_once('/').unwrap_or(("10.244.0.0", "16"));
        let prefix: u8 = prefix_str.parse().unwrap_or(16);
        let gateway = {
            let mut parts: Vec<String> = net.split('.').map(|s| s.to_string()).collect();
            if parts.len() == 4 {
                parts[3] = "1".to_string();
                parts.join(".")
            } else {
                "10.244.0.1".to_string()
            }
        };
        Self {
            bridge: bridge.to_string(),
            gateway,
            prefix,
        }
    }

    /// Wire up a container: create veth pair, attach to bridge, move peer
    /// into the container's netns, and assign the IP.
    pub fn setup(&self, container_pid: u32, container_ip: &str) -> Result<NetHandle> {
        let veth_host = format!("v{:x}", container_pid & 0xFFFFF);

        // 1. Create veth pair: veth_host (stays in host netns) <-> peer
        //    (will become eth0 in container netns).
        run_ip(&[
            "link",
            "add",
            &veth_host,
            "type",
            "veth",
            "peer",
            "name",
            "vpeer",
        ])?;

        // 2. Move the peer end into the container netns.
        run_ip(&[
            "link",
            "set",
            "vpeer",
            "netns",
            &container_pid.to_string(),
        ])?;

        // 3. Inside the container netns: rename to eth0, bring up, assign IP,
        //    add default route via the gateway.
        run_ip_netns(container_pid, &["link", "set", "vpeer", "name", CONTAINER_IFNAME])?;
        run_ip_netns(container_pid, &["link", "set", CONTAINER_IFNAME, "up"])?;
        run_ip_netns(
            container_pid,
            &["addr", "add", &format!("{}/{}", container_ip, self.prefix)],
        )?;
        run_ip_netns(
            container_pid,
            &["route", "add", "default", "via", &self.gateway],
        )?;
        run_ip_netns(
            container_pid,
            &["link", "set", "lo", "up"],
        )?;

        // 4. Bring up the host-side veth and enslave it to the bridge.
        run_ip(&["link", "set", &veth_host, "up"])?;
        run_ip(&["link", "set", &veth_host, "master", &self.bridge])?;

        tracing::info!(
            container_pid,
            veth_host = %veth_host,
            container_ip = %container_ip,
            gateway = %self.gateway,
            "Network configured"
        );

        Ok(NetHandle {
            host_veth: veth_host,
            container_if: CONTAINER_IFNAME.to_string(),
            ip: container_ip.to_string(),
            gateway: self.gateway.clone(),
            prefix: self.prefix,
        })
    }

    /// Tear down the host-side veth. The container-side end is destroyed
    /// automatically when the netns is torn down (i.e. when the last process
    /// in the netns exits).
    pub fn teardown(&self, host_veth: &str) -> Result<()> {
        // Best-effort: ignore errors when the veth is already gone.
        let _ = run_ip(&["link", "del", host_veth]);
        Ok(())
    }
}

// ----- Shell-out helpers --------------------------------------------------

fn run_ip(args: &[&str]) -> Result<()> {
    let out = Command::new("ip").args(args).output().map_err(|e| {
        DroidkerError::Syscall(format!("ip {:?}: {e}", args))
    })?;
    if !out.status.success() {
        return Err(DroidkerError::Syscall(format!(
            "ip {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn run_ip_netns(pid: u32, args: &[&str]) -> Result<()> {
    // `ip netns exec` requires a named namespace. We use `nsenter` instead,
    // which works directly with a PID — no symlink in /var/run/netns needed.
    let mut cmd = Command::new("nsenter");
    cmd.args([
        format!("--target={pid}"),
        "--net".to_string(),
        "--".to_string(),
        "ip".to_string(),
    ]);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().map_err(|e| {
        DroidkerError::Syscall(format!("nsenter ip {:?}: {e}", args))
    })?;
    if !out.status.success() {
        return Err(DroidkerError::Syscall(format!(
            "nsenter ip {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subnet_into_gateway_and_prefix() {
        let n = NetworkConfigurator::new("droidker0", "10.244.0.0/16");
        assert_eq!(n.gateway, "10.244.0.1");
        assert_eq!(n.prefix, 16);
    }

    #[test]
    fn handles_unusual_subnet() {
        let n = NetworkConfigurator::new("br0", "192.168.10.0/24");
        assert_eq!(n.gateway, "192.168.10.1");
        assert_eq!(n.prefix, 24);
    }
}
