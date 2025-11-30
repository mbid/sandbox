use anyhow::{bail, Context, Result};
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
pub fn build_image(dockerfile_path: &Path, user_info: &UserInfo) -> Result<String> {
    let dockerfile_hash = hash_file(dockerfile_path)?;
    let image_tag = format!("sandbox:{}", dockerfile_hash);

    // Check if image already exists
    if image_exists(&image_tag)? {
        eprintln!("Using existing image: {}", image_tag);
        return Ok(image_tag);
    }

    eprintln!("Building Docker image: {}", image_tag);

    let dockerfile_dir = dockerfile_path
        .parent()
        .context("Dockerfile has no parent directory")?;

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
            &dockerfile_dir.to_string_lossy(),
        ])
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

/// Attach to a running container.
pub fn attach_container(name: &str, shell: &str) -> Result<()> {
    let status = Command::new("docker")
        .args(["exec", "-it", name, shell])
        .status()
        .context("Failed to attach to container")?;

    if !status.success() {
        bail!("Container exec failed");
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
