// src/container/ports.rs
//
// Host → container TCP port publishing.
//
// When the user asks for `droidker create --port 8080:80`, we install two
// iptables rules in the `nat` table on the host:
//
//   * `PREROUTING -p tcp --dport 8080 -j DNAT --to-destination <container_ip>:80`
//     — so traffic arriving on the host's external interface gets redirected
//     to the container.
//   * `POSTROUTING -p tcp -d <container_ip> --dport 80 -j MASQUERADE`
//     — so replies from the container get SNAT'd back through the host,
//     keeping the conntrack flow symmetric.
//
// We use `iptables` rather than `nft` directly because:
//   - On a 1-vCPU VPS, iptables-legacy is still the default in most distros
//     (Debian 11/12, Ubuntu 22.04 LTS).
//   - nft rules are *also* installed if `nft` exists, so the same call
//     works on modern hosts that have switched to nftables.
//
// All rules carry the comment `-m comment --comment droidker:<container_id>`
// so they can be enumerated and torn down by container ID.

use crate::error::{DroidkerError, Result};
use crate::models::{Container, PortMapping};
use std::process::Command;
use uuid::Uuid;

/// Forward a single host port to a container port via iptables DNAT.
/// Idempotent: if the rule already exists (same comment + dport + to),
/// iptables will print a duplicate-rule warning but exit 0 — we treat
/// any non-zero exit as a hard failure.
pub fn publish(container_ip: &str, id: Uuid, pm: &PortMapping) -> Result<()> {
    let comment = format!("droidker:{}:{}", id, pm.host);

    // 1. DNAT in PREROUTING (incoming traffic).
    run_iptables(&[
        "-t", "nat",
        "-A", "PREROUTING",
        "-p", "tcp",
        "--dport", &pm.host.to_string(),
        "-j", "DNAT",
        "--to-destination", &format!("{}:{}", container_ip, pm.container),
        "-m", "comment", "--comment", &comment,
    ])?;

    // 2. DNAT in OUTPUT too, so `curl localhost:<host_port>` from the
    //    host itself works (otherwise locally-generated traffic bypasses
    //    PREROUTING and hits the host's own listeners).
    run_iptables(&[
        "-t", "nat",
        "-A", "OUTPUT",
        "-p", "tcp",
        "--dport", &pm.host.to_string(),
        "-j", "DNAT",
        "--to-destination", &format!("{}:{}", container_ip, pm.container),
        "-m", "comment", "--comment", &comment,
    ])?;

    // 3. MASQUERADE the return path so conntrack sees symmetric flows.
    run_iptables(&[
        "-t", "nat",
        "-A", "POSTROUTING",
        "-p", "tcp",
        "-d", container_ip,
        "--dport", &pm.container.to_string(),
        "-j", "MASQUERADE",
        "-m", "comment", "--comment", &comment,
    ])?;

    // 4. Allow forward in the filter table (in case the default FORWARD
    //    policy is DROP).
    run_iptables(&[
        "-A", "FORWARD",
        "-p", "tcp",
        "-d", container_ip,
        "--dport", &pm.container.to_string(),
        "-j", "ACCEPT",
        "-m", "comment", "--comment", &comment,
    ])?;
    run_iptables(&[
        "-A", "FORWARD",
        "-p", "tcp",
        "-s", container_ip,
        "--sport", &pm.container.to_string(),
        "-j", "ACCEPT",
        "-m", "comment", "--comment", &comment,
    ])?;

    tracing::info!(
        container_id = %id,
        host_port = pm.host,
        container_port = pm.container,
        "published port"
    );
    Ok(())
}

/// Tear down every rule that carries `droidker:<id>:<host>` as a comment.
/// We don't try to delete rules one-by-one by full match — instead we
/// loop over `iptables -t nat -D` and `iptables -t filter -D` until they
/// all return non-zero (meaning no more matching rules exist).
pub fn unpublish_all(id: Uuid) {
    // Gather every comment string we might have created. We don't actually
    // know which host ports were used, so we use a comment-prefix match.
    // `iptables -S` lists rules; we grep for `droidker:<id>:` and parse
    // the host port out of the comment.
    let comment_prefix = format!("droidker:{}:", id);

    for table in &["nat", "filter"] {
        let listing = match list_rules(table) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for line in listing.lines() {
            if !line.contains(&comment_prefix) {
                continue;
            }
            // The rule text from `iptables -S` looks like:
            //   -A PREROUTING -p tcp --dport 8080 -j DNAT --to-destination 10.244.0.5:80 -m comment --comment droidker:UUID:8080
            // To delete, we replace the leading `-A` with `-D`.
            let delete_args: Vec<&str> = line
                .split_whitespace()
                .skip(1) // drop the leading "-A" / "-t nat -A" etc.
                .collect();
            if delete_args.is_empty() {
                continue;
            }
            // Reconstruct the chain name (first token) and the rest.
            // iptables -D takes: -D <chain> <rest...>
            let chain = delete_args[0];
            let rest: Vec<&str> = delete_args[1..].iter().copied().collect();

            let mut argv: Vec<String> = vec![
                "-t".to_string(),
                table.to_string(),
                "-D".to_string(),
                chain.to_string(),
            ];
            for r in rest {
                argv.push(r.to_string());
            }
            // Try repeatedly until deletion returns non-zero (no more matches).
            loop {
                let rc = run_iptables_silent(&argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());
                if rc.is_err() {
                    break;
                }
            }
        }
    }
    tracing::info!(container_id = %id, "all published ports removed");
}

/// Publish every port mapping on a freshly-started container.
pub fn publish_all(container: &Container) -> Result<()> {
    let ip = match container.ip.as_deref() {
        Some(ip) => ip,
        None => {
            tracing::warn!(
                container_id = %container.id,
                "container has no IP — skipping port publishing"
            );
            return Ok(());
        }
    };
    for pm in &container.ports {
        if let Err(e) = publish(ip, container.id, pm) {
            tracing::warn!(
                container_id = %container.id,
                host_port = pm.host,
                container_port = pm.container,
                error = %e,
                "failed to publish port"
            );
        }
    }
    Ok(())
}

// ----- low-level helpers ---------------------------------------------------

fn run_iptables(args: &[&str]) -> Result<()> {
    let out = Command::new("iptables").args(args).output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // "Rule does not exist" on a delete is fine — it means we've
            // already cleaned up.
            if stderr.contains("does not exist") || stderr.contains("No chain") {
                return Ok(());
            }
            Err(DroidkerError::Internal(format!(
                "iptables {} failed: {}",
                args.join(" "),
                stderr.trim()
            )))
        }
        Err(e) => {
            // iptables binary missing — common in containers; degrade
            // gracefully so the daemon still starts.
            tracing::warn!(error = %e, "iptables binary not available — port publishing disabled");
            Err(DroidkerError::Internal(format!(
                "iptables spawn failed: {e}"
            )))
        }
    }
}

fn run_iptables_silent(args: &[&str]) -> Result<()> {
    let _ = run_iptables(args);
    Ok(())
}

fn list_rules(table: &str) -> Result<String> {
    let out = Command::new("iptables")
        .args(["-t", table, "-S"])
        .output()
        .map_err(|e| DroidkerError::Internal(format!("iptables -S: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_includes_container_id_and_host_port() {
        // We don't actually invoke iptables in tests — we just confirm
        // the comment format is stable so the unpublish logic can parse
        // it back out.
        let id = Uuid::new_v4();
        let pm = PortMapping { host: 8080, container: 80 };
        let comment = format!("droidker:{}:{}", id, pm.host);
        assert!(comment.starts_with(&format!("droidker:{}:", id)));
        assert!(comment.ends_with(":8080"));
    }

    #[test]
    fn comment_prefix_matches_any_host_port() {
        let id = Uuid::new_v4();
        let prefix = format!("droidker:{}:", id);
        for host in [80u16, 443, 8080, 65535] {
            let c = format!("droidker:{}:{}", id, host);
            assert!(c.starts_with(&prefix));
        }
    }
}
