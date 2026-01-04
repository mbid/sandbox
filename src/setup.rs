//! Setup and installation of systemd user service for the sandbox daemon.

use anyhow::{anyhow, Context, Result};
use indoc::formatdoc;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::daemon;

const SERVICE_NAME: &str = "sandbox";

fn systemd_user_dir() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("Could not determine config directory")?;
    Ok(config_dir.join("systemd/user"))
}

fn socket_unit_path() -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(format!("{}.socket", SERVICE_NAME)))
}

fn service_unit_path() -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(format!("{}.service", SERVICE_NAME)))
}

fn socket_unit_content() -> Result<String> {
    let socket_path = daemon::socket_path()?;
    Ok(formatdoc! {"
        [Unit]
        Description=Sandbox daemon socket

        [Socket]
        ListenStream={socket_path}
        SocketMode=0600

        [Install]
        WantedBy=sockets.target
    ", socket_path = socket_path.display()})
}

fn service_unit_content() -> Result<String> {
    let exe_path = std::env::current_exe().context("Could not determine executable path")?;
    let exe_path = exe_path
        .canonicalize()
        .with_context(|| format!("Could not resolve executable path: {}", exe_path.display()))?;

    Ok(formatdoc! {"
        [Unit]
        Description=Sandbox daemon for managing sandboxed LLM agents
        Requires={service}.socket

        [Service]
        Type=simple
        ExecStart={exe_path} daemon
        Restart=on-failure
        RestartSec=5

        [Install]
        WantedBy=default.target
    ", service = SERVICE_NAME, exe_path = exe_path.display()})
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("Failed to run systemctl")?;

    if !status.success() {
        return Err(anyhow!("systemctl --user {} failed", args.join(" ")));
    }
    Ok(())
}

fn check_systemd_available() -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .arg("--version")
        .output()
        .context("Failed to run systemctl")?;

    if !output.status.success() {
        return Err(anyhow!(
            "systemd user session is not available.\n\
             Make sure you're running in a systemd-managed session."
        ));
    }
    Ok(())
}

pub fn system_install() -> Result<()> {
    check_systemd_available()?;

    let systemd_dir = systemd_user_dir()?;
    let socket_path = socket_unit_path()?;
    let service_path = service_unit_path()?;

    fs::create_dir_all(&systemd_dir)
        .with_context(|| format!("Failed to create {}", systemd_dir.display()))?;

    let socket_content = socket_unit_content()?;
    let service_content = service_unit_content()?;

    fs::write(&socket_path, &socket_content)
        .with_context(|| format!("Failed to write {}", socket_path.display()))?;

    fs::write(&service_path, &service_content)
        .with_context(|| format!("Failed to write {}", service_path.display()))?;

    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", &format!("{}.socket", SERVICE_NAME)])?;
    systemctl(&["start", &format!("{}.socket", SERVICE_NAME)])?;

    println!("Installed sandbox daemon.");

    Ok(())
}

pub fn system_uninstall() -> Result<()> {
    check_systemd_available()?;

    let socket_path = socket_unit_path()?;
    let service_path = service_unit_path()?;

    let _ = systemctl(&["stop", &format!("{}.socket", SERVICE_NAME)]);
    let _ = systemctl(&["stop", &format!("{}.service", SERVICE_NAME)]);
    let _ = systemctl(&["disable", &format!("{}.socket", SERVICE_NAME)]);

    if socket_path.exists() {
        fs::remove_file(&socket_path)
            .with_context(|| format!("Failed to remove {}", socket_path.display()))?;
    }

    if service_path.exists() {
        fs::remove_file(&service_path)
            .with_context(|| format!("Failed to remove {}", service_path.display()))?;
    }

    systemctl(&["daemon-reload"])?;

    println!("Uninstalled sandbox daemon.");

    Ok(())
}
