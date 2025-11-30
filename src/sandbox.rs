use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{get_sandbox_base_dir, get_sandbox_instance_dir, UserInfo};
use crate::docker;
use crate::git;
use crate::network;

/// Metadata about a sandbox instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub name: String,
    pub repo_root: PathBuf,
    pub sandbox_dir: PathBuf,
    /// The actual clone directory (in cache)
    pub clone_dir: PathBuf,
    /// Symlink that points to clone_dir, at repo_root location for path consistency
    pub clone_symlink: PathBuf,
    pub container_name: String,
    pub created_at: String,
}

impl SandboxInfo {
    pub fn new(name: &str, repo_root: &Path) -> Result<Self> {
        let sandbox_dir = get_sandbox_instance_dir(repo_root, name)?;
        let clone_dir = sandbox_dir.join("clone");

        // The symlink lives next to the repo_root so paths are consistent
        // It will be at: /path/to/repo-sandbox-<name> -> clone_dir
        let symlink_name = format!(
            "{}-sandbox-{}",
            repo_root.file_name().unwrap().to_string_lossy(),
            name
        );
        let clone_symlink = repo_root.parent().unwrap().join(&symlink_name);

        let container_name = format!(
            "sandbox-{}-{}",
            repo_root.file_name().unwrap().to_string_lossy(),
            name
        );
        let created_at = chrono::Utc::now().to_rfc3339();

        Ok(SandboxInfo {
            name: name.to_string(),
            repo_root: repo_root.to_path_buf(),
            sandbox_dir,
            clone_dir,
            clone_symlink,
            container_name,
            created_at,
        })
    }

    /// Load sandbox info from disk.
    pub fn load(sandbox_dir: &Path) -> Result<Self> {
        let info_path = sandbox_dir.join("sandbox.json");
        let contents = std::fs::read_to_string(&info_path)
            .with_context(|| format!("Failed to read sandbox info: {}", info_path.display()))?;
        serde_json::from_str(&contents).context("Failed to parse sandbox info")
    }

    /// Save sandbox info to disk.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.sandbox_dir)?;
        let info_path = self.sandbox_dir.join("sandbox.json");
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&info_path, contents)?;
        Ok(())
    }

    /// Get the volume name for overlay storage.
    pub fn overlay_volume_name(&self, purpose: &str) -> String {
        format!(
            "sandbox-{}-{}-{}",
            self.repo_root.file_name().unwrap().to_string_lossy(),
            self.name,
            purpose
        )
    }
}

/// List all sandbox instances for a repository.
pub fn list_sandboxes(repo_root: &Path) -> Result<Vec<SandboxInfo>> {
    let base_dir = get_sandbox_base_dir(repo_root)?;

    if !base_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sandboxes = Vec::new();

    for entry in std::fs::read_dir(&base_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if let Ok(info) = SandboxInfo::load(&path) {
                sandboxes.push(info);
            }
        }
    }

    Ok(sandboxes)
}

/// Delete a sandbox and its associated resources.
pub fn delete_sandbox(info: &SandboxInfo) -> Result<()> {
    eprintln!("Deleting sandbox: {}", info.name);

    // Stop and remove container if it exists
    if docker::container_exists(&info.container_name)? {
        docker::remove_container(&info.container_name)?;
    }

    // Remove Docker volumes
    for purpose in &[
        "overlay-work",
        "overlay-upper",
        "claude-overlay",
        "home-overlay",
    ] {
        let volume_name = info.overlay_volume_name(purpose);
        if let Ok(volumes) = docker::list_volumes_with_prefix(&volume_name) {
            for vol in volumes {
                let _ = docker::remove_volume(&vol);
            }
        }
    }

    // Remove the remote from main repo
    let remote_name = format!("sandbox-{}", info.name);
    let _ = Command::new("git")
        .current_dir(&info.repo_root)
        .args(["remote", "remove", &remote_name])
        .status();

    // Remove symlink
    if info.clone_symlink.exists() || info.clone_symlink.is_symlink() {
        let _ = std::fs::remove_file(&info.clone_symlink);
    }

    // Remove sandbox directory
    if info.sandbox_dir.exists() {
        std::fs::remove_dir_all(&info.sandbox_dir)
            .with_context(|| format!("Failed to remove: {}", info.sandbox_dir.display()))?;
    }

    Ok(())
}

/// Clean up orphaned Docker volumes (volumes without corresponding sandbox directories).
pub fn cleanup_orphaned_volumes(repo_root: &Path) -> Result<()> {
    let prefix = format!(
        "sandbox-{}-",
        repo_root.file_name().unwrap().to_string_lossy()
    );

    let volumes = docker::list_volumes_with_prefix(&prefix)?;
    let sandboxes = list_sandboxes(repo_root)?;
    let sandbox_names: Vec<_> = sandboxes.iter().map(|s| &s.name).collect();

    for volume in volumes {
        // Extract sandbox name from volume name
        // Format: sandbox-<repo>-<name>-<purpose>
        let parts: Vec<_> = volume.split('-').collect();
        if parts.len() >= 3 {
            let sandbox_name = parts[2];
            if !sandbox_names.iter().any(|n| *n == sandbox_name) {
                eprintln!("Removing orphaned volume: {}", volume);
                let _ = docker::remove_volume(&volume);
            }
        }
    }

    Ok(())
}

/// Create and run a sandbox container.
pub fn run_sandbox(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    command: Option<&[String]>,
) -> Result<()> {
    // Check if container is already running
    if docker::container_is_running(&info.container_name)? {
        eprintln!("Attaching to existing container: {}", info.container_name);
        let shell = if user_info.uses_fish() {
            "fish"
        } else {
            "bash"
        };
        return docker::attach_container(&info.container_name, shell);
    }

    // Remove stopped container if it exists
    if docker::container_exists(&info.container_name)? {
        docker::remove_container(&info.container_name)?;
    }

    // Create overlay volumes
    for purpose in &[
        "overlay-work",
        "overlay-upper",
        "claude-overlay",
        "home-overlay",
    ] {
        let volume_name = info.overlay_volume_name(purpose);
        docker::create_volume(&volume_name)?;
    }

    // Ensure network exists for whitelist support
    network::ensure_network()?;

    // Build docker run arguments
    let mut args = vec![
        "run".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        info.container_name.clone(),
        "--label".to_string(),
        "sandbox=true".to_string(),
        // User mapping
        "--user".to_string(),
        format!("{}:{}", user_info.uid, user_info.gid),
    ];

    // Mount the original repo as read-only for shared clone to work
    // The repo is mounted at the same path inside the container
    args.extend([
        "--mount".to_string(),
        format!(
            "type=bind,source={},target={},readonly",
            info.repo_root.display(),
            info.repo_root.display()
        ),
    ]);

    // Mount the sandbox clone directory at the symlink path
    // Inside the container, the path will be the same as the symlink path externally
    // This makes paths consistent between host and container
    args.extend([
        "--mount".to_string(),
        format!(
            "type=bind,source={},target={}",
            info.clone_dir.display(),
            info.clone_symlink.display()
        ),
    ]);

    // Also mount the clone at its actual path for git operations
    args.extend([
        "--mount".to_string(),
        format!(
            "type=bind,source={},target={}",
            info.clone_dir.display(),
            info.clone_dir.display()
        ),
    ]);

    // Set working directory to the symlink path (same as external view)
    args.extend([
        "--workdir".to_string(),
        info.clone_symlink.to_string_lossy().to_string(),
    ]);

    // Mount fish config if user uses fish
    if user_info.uses_fish() {
        if let Some(home) = dirs::home_dir() {
            let fish_config = home.join(".config/fish");
            if fish_config.exists() {
                args.extend([
                    "--mount".to_string(),
                    format!(
                        "type=bind,source={},target=/home/{}/.config/fish,readonly",
                        fish_config.display(),
                        user_info.username
                    ),
                ]);
            }
        }
    }

    // Mount Claude config with overlay (copy-on-write, changes don't propagate out)
    if let Some(home) = dirs::home_dir() {
        let claude_json = home.join(".claude.json");
        let claude_dir = home.join(".claude");

        if claude_json.exists() {
            args.extend([
                "--mount".to_string(),
                format!(
                    "type=bind,source={},target=/home/{}/.claude.json,readonly",
                    claude_json.display(),
                    user_info.username
                ),
            ]);
        }

        if claude_dir.exists() {
            // Use a volume for claude overlay
            let claude_volume = info.overlay_volume_name("claude-overlay");
            args.extend([
                "--mount".to_string(),
                format!(
                    "type=volume,source={},target=/home/{}/.claude",
                    claude_volume, user_info.username
                ),
            ]);
        }
    }

    // Network: use sandbox network with whitelist
    args.extend(network::get_network_args(true));

    // Add the image
    args.push(image_tag.to_string());

    // Add command or default to shell
    if let Some(cmd) = command {
        args.extend(cmd.iter().cloned());
    } else {
        // Default to user's shell
        let shell = if user_info.uses_fish() {
            "fish"
        } else {
            "bash"
        };
        args.push(shell.to_string());
    }

    eprintln!("Starting container: {}", info.container_name);

    let status = Command::new("docker")
        .args(&args)
        .status()
        .context("Failed to run docker container")?;

    if !status.success() {
        bail!("Container exited with error");
    }

    Ok(())
}

/// Ensure a sandbox is set up and ready to use.
pub fn ensure_sandbox(repo_root: &Path, name: &str) -> Result<SandboxInfo> {
    let info = SandboxInfo::new(name, repo_root)?;

    // Create sandbox directory
    std::fs::create_dir_all(&info.sandbox_dir)?;

    // Create shared clone if it doesn't exist
    git::create_shared_clone(repo_root, &info.clone_dir)?;

    // Create symlink if it doesn't exist
    if !info.clone_symlink.exists() && !info.clone_symlink.is_symlink() {
        symlink(&info.clone_dir, &info.clone_symlink).with_context(|| {
            format!(
                "Failed to create symlink {} -> {}",
                info.clone_symlink.display(),
                info.clone_dir.display()
            )
        })?;
    }

    // Setup bidirectional remotes
    git::setup_bidirectional_remotes(repo_root, &info.clone_dir, name)?;

    // Save sandbox info
    info.save()?;

    Ok(info)
}
