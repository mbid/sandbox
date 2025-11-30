use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::config::NETWORK_WHITELIST;

/// Docker network name for sandboxes with whitelisted access.
pub const SANDBOX_NETWORK: &str = "sandbox-net";

/// Check if the sandbox network exists.
pub fn network_exists() -> Result<bool> {
    let output = Command::new("docker")
        .args(["network", "inspect", SANDBOX_NETWORK])
        .output()
        .context("Failed to inspect network")?;

    Ok(output.status.success())
}

/// Create the sandbox network if it doesn't exist.
pub fn ensure_network() -> Result<()> {
    if network_exists()? {
        return Ok(());
    }

    eprintln!("Creating sandbox network: {}", SANDBOX_NETWORK);

    let status = Command::new("docker")
        .args(["network", "create", "--driver", "bridge", SANDBOX_NETWORK])
        .status()
        .context("Failed to create network")?;

    if !status.success() {
        bail!("Failed to create sandbox network");
    }

    Ok(())
}

/// Generate iptables rules for whitelisting specific domains.
/// Returns a script that can be run inside the container with CAP_NET_ADMIN.
pub fn generate_whitelist_script() -> String {
    let mut script = String::from("#!/bin/sh\n");
    script.push_str("# Drop all outgoing traffic by default\n");
    script.push_str("iptables -P OUTPUT DROP\n");
    script.push_str("# Allow loopback\n");
    script.push_str("iptables -A OUTPUT -o lo -j ACCEPT\n");
    script.push_str("# Allow established connections\n");
    script.push_str("iptables -A OUTPUT -m state --state ESTABLISHED,RELATED -j ACCEPT\n");
    script.push_str("# Allow DNS\n");
    script.push_str("iptables -A OUTPUT -p udp --dport 53 -j ACCEPT\n");
    script.push_str("iptables -A OUTPUT -p tcp --dport 53 -j ACCEPT\n");

    for domain in NETWORK_WHITELIST {
        script.push_str(&format!("# Allow {}\n", domain));
        // Resolve and allow the domain
        // Note: This is a simplified approach. For production, you'd want to
        // resolve DNS at runtime or use a more sophisticated firewall.
        script.push_str(&format!(
            "for ip in $(getent hosts {} | awk '{{print $1}}'); do\n",
            domain
        ));
        script.push_str("  iptables -A OUTPUT -d $ip -p tcp --dport 443 -j ACCEPT\n");
        script.push_str("  iptables -A OUTPUT -d $ip -p tcp --dport 80 -j ACCEPT\n");
        script.push_str("done\n");
    }

    script
}

/// Get network arguments for docker run.
/// If use_whitelist is true, returns args for the sandbox network.
/// Otherwise, returns args for no network.
pub fn get_network_args(use_whitelist: bool) -> Vec<String> {
    if use_whitelist {
        vec!["--network".to_string(), SANDBOX_NETWORK.to_string()]
    } else {
        vec!["--network".to_string(), "none".to_string()]
    }
}
