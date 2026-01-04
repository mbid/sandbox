use anyhow::{bail, Context, Result};
use log::{debug, info};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{hash_file, UserInfo};

/// Check if a Docker image with the given tag exists.
pub fn image_exists(tag: &str) -> Result<bool> {
    let output = Command::new("docker")
        .args(["image", "inspect", tag])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker image inspect")?;

    Ok(output.success())
}

/// Build a Docker image from a Dockerfile.
/// The image is tagged with a hash of the Dockerfile contents.
/// Returns the image tag.
pub fn build_image(dockerfile_path: &Path, context: &Path, user_info: &UserInfo) -> Result<String> {
    let dockerfile_hash = hash_file(dockerfile_path)?;
    let image_tag = format!("sandbox:{}", dockerfile_hash);

    // Check if image already exists
    if image_exists(&image_tag)? {
        debug!("Using existing image: {}", image_tag);
        return Ok(image_tag);
    }

    info!("Building Docker image: {}", image_tag);

    let status = Command::new("docker")
        .args([
            "build",
            "-f",
            &dockerfile_path.to_string_lossy(),
            "-t",
            &image_tag,
            "--build-arg",
            &format!("USER_NAME={}", user_info.username),
            "--build-arg",
            &format!("USER_ID={}", user_info.uid),
            "--build-arg",
            &format!("GROUP_ID={}", user_info.gid),
            &context.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker build")?;

    if !status.success() {
        bail!("Docker build failed");
    }

    Ok(image_tag)
}

/// Check if a container with the given name exists and is running.
pub fn container_is_running(name: &str) -> Result<bool> {
    let output = Command::new("docker")
        .args(["container", "inspect", "-f", "{{.State.Running}}", name])
        .output()
        .context("Failed to run docker container inspect")?;

    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim() == "true")
}

/// Check if a container with the given name exists (running or stopped).
pub fn container_exists(name: &str) -> Result<bool> {
    let output = Command::new("docker")
        .args(["container", "inspect", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker container inspect")?;

    Ok(output.success())
}

/// Remove a container by name.
pub fn remove_container(name: &str) -> Result<()> {
    let status = Command::new("docker")
        .args(["rm", "-f", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker rm")?;

    if !status.success() {
        bail!("Failed to remove container: {}", name);
    }

    Ok(())
}

/// List all Docker volumes with a specific prefix.
pub fn list_volumes_with_prefix(prefix: &str) -> Result<Vec<String>> {
    let output = Command::new("docker")
        .args([
            "volume",
            "ls",
            "-q",
            "--filter",
            &format!("name={}", prefix),
        ])
        .output()
        .context("Failed to list Docker volumes")?;

    if !output.status.success() {
        bail!("Failed to list Docker volumes");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(String::from).collect())
}

/// Remove a Docker volume.
pub fn remove_volume(name: &str) -> Result<()> {
    let status = Command::new("docker")
        .args(["volume", "rm", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker volume rm")?;

    if !status.success() {
        bail!("Failed to remove volume: {}", name);
    }

    Ok(())
}

/// Create a Docker volume.
pub fn create_volume(name: &str) -> Result<()> {
    let status = Command::new("docker")
        .args(["volume", "create", name])
        .stdout(Stdio::null())
        .status()
        .context("Failed to create Docker volume")?;

    if !status.success() {
        bail!("Failed to create volume: {}", name);
    }

    Ok(())
}

/// Attach to a running container and execute a command.
pub fn exec_in_container(
    name: &str,
    command: &[&str],
    env_vars: &[(String, String)],
) -> Result<()> {
    use std::io::IsTerminal;

    let mut args = vec!["exec".to_string()];

    // Only use -it flags when stdin is a TTY
    if std::io::stdin().is_terminal() {
        args.push("-it".to_string());
    }

    for (k, v) in env_vars {
        args.push("-e".to_string());
        args.push(format!("{}={}", k, v));
    }

    args.push(name.to_string());
    args.extend(command.iter().map(|s| s.to_string()));

    let status = Command::new("docker")
        .args(&args)
        .status()
        .context("Failed to exec in container")?;

    if !status.success() {
        bail!("Container exec failed");
    }

    Ok(())
}

/// Stop a running container. Silently succeeds if container is already stopped.
pub fn stop_container(name: &str) -> Result<()> {
    // Use -t 0 to skip the graceful shutdown period. Our containers run
    // `sleep infinity` which ignores SIGTERM anyway, so waiting is pointless.
    let status = Command::new("docker")
        .args(["stop", "-t", "0", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker stop")?;

    // We don't check status.success() because stopping an already-stopped
    // container is fine - we just want it stopped.
    let _ = status;
    Ok(())
}

/// Wait for a container to stop.
pub fn wait_container(name: &str) -> Result<()> {
    let status = Command::new("docker")
        .args(["wait", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to run docker wait")?;

    if !status.success() {
        anyhow::bail!("docker wait failed for container '{}'", name);
    }
    Ok(())
}

/// List all containers with a specific label.
pub fn list_containers_with_label(label: &str) -> Result<Vec<String>> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={}", label),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("Failed to list containers")?;

    if !output.status.success() {
        bail!("Failed to list containers");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(String::from).collect())
}
