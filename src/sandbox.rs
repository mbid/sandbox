use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{get_sandbox_base_dir, get_sandbox_instance_dir, UserInfo};
use crate::docker;
use crate::git;
use crate::overlay::Overlay;

/// Metadata about a sandbox instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub name: String,
    pub repo_root: PathBuf,
    pub sandbox_dir: PathBuf,
    /// The actual clone directory (in cache)
    pub clone_dir: PathBuf,
    /// Symlink that points to repo_root, used as the source for shared clone
    /// so the clone's alternates reference this path instead of repo_root directly
    pub repo_symlink: PathBuf,
    pub container_name: String,
    pub created_at: String,
}

impl SandboxInfo {
    pub fn new(name: &str, repo_root: &Path) -> Result<Self> {
        let sandbox_dir = get_sandbox_instance_dir(repo_root, name)?;
        let clone_dir = sandbox_dir.join("clone");

        // The repo symlink lives in the sandbox cache dir and points to repo_root.
        // We create the shared clone from this symlink so the clone's alternates
        // reference the symlink path. This allows the clone to work both:
        // - Outside the container: symlink resolves to repo_root
        // - Inside the container: repo is mounted at the symlink path
        let repo_symlink = sandbox_dir.join("repo");

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
            repo_symlink,
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

    /// Get the base directory for overlay mounts.
    pub fn overlays_dir(&self) -> PathBuf {
        self.sandbox_dir.join("overlays")
    }

    /// Get the volume name prefix for this sandbox.
    pub fn volume_prefix(&self) -> String {
        format!(
            "sandbox-{}-{}",
            self.repo_root.file_name().unwrap().to_string_lossy(),
            self.name
        )
    }

    /// Create an overlay configuration for a given source directory.
    pub fn create_overlay(&self, name: &str, lower: &Path) -> Overlay {
        Overlay::new(name, lower, &self.overlays_dir(), &self.volume_prefix())
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

    // Remove overlay Docker volumes
    let volume_prefix = info.volume_prefix();
    if let Ok(volumes) = docker::list_volumes_with_prefix(&volume_prefix) {
        for vol in volumes {
            let _ = docker::remove_volume(&vol);
        }
    }

    // Remove the remote from main repo
    let remote_name = format!("sandbox-{}", info.name);
    let _ = Command::new("git")
        .current_dir(&info.repo_root)
        .args(["remote", "remove", &remote_name])
        .status();

    // Remove symlink
    if info.repo_symlink.exists() || info.repo_symlink.is_symlink() {
        let _ = std::fs::remove_file(&info.repo_symlink);
    }

    // Remove sandbox directory (includes overlay upper/work dirs)
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

    // Set up overlay mounts
    let mut overlays: Vec<Overlay> = Vec::new();

    // Overlay for target directory (Rust build artifacts, copy-on-write)
    let target_dir = info.repo_root.join("target");
    if target_dir.exists() {
        let overlay = info.create_overlay("target-dir", &target_dir);
        overlay.create_volume()?;
        overlays.push(overlay);
    }

    // Copy single files (not directories) to sandbox directory
    // These will be bind-mounted into the container
    let mut file_copies: Vec<(PathBuf, PathBuf)> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        // Copy ~/.claude.json with filtered projects (only keep the current project)
        let claude_json = home.join(".claude.json");
        if claude_json.exists() {
            let copy_path = info.sandbox_dir.join("claude.json");
            let filtered_json = filter_claude_json(&claude_json, &info.repo_root)?;
            std::fs::write(&copy_path, filtered_json)
                .with_context(|| format!("Failed to write filtered {}", copy_path.display()))?;
            file_copies.push((
                copy_path,
                PathBuf::from(format!("/home/{}/.claude.json", user_info.username)),
            ));
        }
    }

    // Build docker run arguments
    let mut args = vec![
        "run".to_string(),
        "-it".to_string(),
        "--name".to_string(),
        info.container_name.clone(),
        "--hostname".to_string(),
        info.name.clone(),
        "--label".to_string(),
        "sandbox=true".to_string(),
        // Use gVisor for sandboxing
        "--runtime".to_string(),
        "runsc".to_string(),
        // User mapping
        "--user".to_string(),
        format!("{}:{}", user_info.uid, user_info.gid),
    ];

    // Mount the original repo at the symlink path (read-only)
    // The shared clone's alternates reference the symlink path, so we mount the repo there.
    // This makes the clone work inside the container.
    args.extend([
        "--mount".to_string(),
        format!(
            "type=bind,source={},target={},readonly",
            info.repo_root.display(),
            info.repo_symlink.display()
        ),
    ]);

    // Mount the sandbox clone at the original repo path
    // This way the working directory path matches the original repo path
    args.extend([
        "--mount".to_string(),
        format!(
            "type=bind,source={},target={}",
            info.clone_dir.display(),
            info.repo_root.display()
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

    // Set working directory to the repo path (where clone is mounted)
    args.extend([
        "--workdir".to_string(),
        info.repo_root.to_string_lossy().to_string(),
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

    // Mount gitconfig if it exists (read-only)
    if let Some(home) = dirs::home_dir() {
        let gitconfig = home.join(".gitconfig");
        if gitconfig.exists() {
            args.extend([
                "--mount".to_string(),
                format!(
                    "type=bind,source={},target=/home/{}/.gitconfig,readonly",
                    gitconfig.display(),
                    user_info.username
                ),
            ]);
        }
    }

    // Mount nvim config if it exists (read-only)
    if let Some(home) = dirs::home_dir() {
        let nvim_config = home.join(".config/nvim");
        if nvim_config.exists() {
            args.extend([
                "--mount".to_string(),
                format!(
                    "type=bind,source={},target=/home/{}/.config/nvim,readonly",
                    nvim_config.display(),
                    user_info.username
                ),
            ]);
        }
    }

    // Mount target directory with overlay (copy-on-write, changes don't propagate out)
    if let Some(target_overlay) = overlays.iter().find(|o| o.name == "target-dir") {
        let target = info.repo_root.join("target");
        args.extend(target_overlay.docker_mount_args(&target));
    }

    // Mount copied files (single files are copied, not overlaid)
    for (source, target) in &file_copies {
        args.extend([
            "--mount".to_string(),
            format!(
                "type=bind,source={},target={}",
                source.display(),
                target.display()
            ),
        ]);
    }

    // Add the image
    args.push(image_tag.to_string());

    // Determine the main command
    let main_cmd = if let Some(cmd) = command {
        cmd.join(" ")
    } else {
        // Default to user's shell
        if user_info.uses_fish() {
            "fish".to_string()
        } else {
            "bash".to_string()
        }
    };

    args.push(main_cmd);

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

    // Create symlink to repo first (if it doesn't exist)
    // This symlink points to repo_root and will be used as the source for the shared clone.
    // This way, the clone's alternates reference the symlink path, making it work both:
    // - Outside the container: symlink resolves to the real repo
    // - Inside the container: repo is mounted at the symlink path
    if !info.repo_symlink.exists() && !info.repo_symlink.is_symlink() {
        symlink(&info.repo_root, &info.repo_symlink).with_context(|| {
            format!(
                "Failed to create symlink {} -> {}",
                info.repo_symlink.display(),
                info.repo_root.display()
            )
        })?;
    }

    // Create shared clone from the symlink path (not the real repo path)
    // This ensures the clone references the symlink in its alternates
    git::create_shared_clone(&info.repo_symlink, &info.clone_dir)?;

    // Setup bidirectional remotes
    git::setup_bidirectional_remotes(repo_root, &info.clone_dir, name)?;

    // Save sandbox info
    info.save()?;

    Ok(info)
}

/// Filter ~/.claude.json to only include the project matching repo_root.
/// This preserves key ordering in the JSON using serde_json's preserve_order feature.
fn filter_claude_json(claude_json_path: &Path, repo_root: &Path) -> Result<String> {
    let contents = std::fs::read_to_string(claude_json_path)
        .with_context(|| format!("Failed to read {}", claude_json_path.display()))?;

    let mut json: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse {}", claude_json_path.display()))?;

    // Filter the "projects" object to only keep the entry matching repo_root
    if let Some(projects) = json.get_mut("projects") {
        if let Some(projects_obj) = projects.as_object_mut() {
            let repo_root_str = repo_root.to_string_lossy();
            projects_obj.retain(|key, _| key == repo_root_str.as_ref());
        }
    }

    serde_json::to_string_pretty(&json).context("Failed to serialize filtered JSON")
}
