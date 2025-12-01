use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::{get_sandbox_base_dir, get_sandbox_instance_dir, UserInfo};
use crate::docker;
use crate::git;
use crate::overlay::Overlay;

/// Specifies how a path should be mounted into the sandbox.
#[derive(Debug, Clone)]
pub enum MountMode {
    /// Read-only bind mount. Changes inside the sandbox are not allowed.
    ReadOnly,
    /// Read-write bind mount. Changes propagate back to the host.
    WriteThrough,
    /// Copy-on-write mount. Reads come from the host, writes are isolated.
    /// For directories, uses overlayfs. For files, creates a copy.
    Overlay,
}

/// Configuration for a single mount point.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Path on the host filesystem.
    pub host_path: PathBuf,
    /// Path inside the container. If None, uses host_path.
    pub container_path: Option<PathBuf>,
    /// How to mount this path.
    pub mode: MountMode,
}

impl Mount {
    /// Create a new mount configuration.
    pub fn new(host_path: impl Into<PathBuf>, mode: MountMode) -> Self {
        Mount {
            host_path: host_path.into(),
            container_path: None,
            mode,
        }
    }

    /// Set a different container path (default is to use host_path).
    pub fn with_container_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.container_path = Some(path.into());
        self
    }

    /// Get the effective container path.
    pub fn target_path(&self) -> &Path {
        self.container_path.as_ref().unwrap_or(&self.host_path)
    }

    /// Generate a unique name for this mount (used for overlay volumes and file copies).
    /// Format: <last_path_component>-<short_hash>
    fn unique_name(&self) -> String {
        let last_component = self
            .host_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mount".to_string());

        let mut hasher = Sha256::new();
        hasher.update(self.host_path.to_string_lossy().as_bytes());
        if let Some(ref cp) = self.container_path {
            hasher.update(cp.to_string_lossy().as_bytes());
        }
        let hash = hex::encode(&hasher.finalize()[..4]); // 8 hex chars

        format!("{}-{}", last_component, hash)
    }
}

/// Process mounts and generate docker arguments.
fn process_mounts(mounts: &[Mount], info: &SandboxInfo) -> Result<Vec<String>> {
    let mut docker_args = Vec::new();

    for mount in mounts {
        // Skip if host path doesn't exist
        if !mount.host_path.exists() {
            continue;
        }

        let target = mount.target_path().to_path_buf();

        match &mount.mode {
            MountMode::ReadOnly => {
                docker_args.extend([
                    "--mount".to_string(),
                    format!(
                        "type=bind,source={},target={},readonly",
                        mount.host_path.display(),
                        target.display()
                    ),
                ]);
            }
            MountMode::WriteThrough => {
                docker_args.extend([
                    "--mount".to_string(),
                    format!(
                        "type=bind,source={},target={}",
                        mount.host_path.display(),
                        target.display()
                    ),
                ]);
            }
            MountMode::Overlay => {
                let name = mount.unique_name();
                if mount.host_path.is_dir() {
                    // Use overlayfs for directories
                    let overlay = info.create_overlay(&name, &mount.host_path);
                    overlay.create_volume()?;
                    docker_args.extend(overlay.docker_mount_args(&target));
                } else {
                    // Copy file to sandbox directory and bind mount it
                    let copy_path = info.sandbox_dir.join(&name);
                    std::fs::copy(&mount.host_path, &copy_path).with_context(|| {
                        format!(
                            "Failed to copy {} to {}",
                            mount.host_path.display(),
                            copy_path.display()
                        )
                    })?;
                    docker_args.extend([
                        "--mount".to_string(),
                        format!(
                            "type=bind,source={},target={}",
                            copy_path.display(),
                            target.display()
                        ),
                    ]);
                }
            }
        }
    }

    Ok(docker_args)
}

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

/// Build the list of mounts for a sandbox container.
fn build_mount_list(info: &SandboxInfo, user_info: &UserInfo) -> Vec<Mount> {
    let home = dirs::home_dir();
    let container_home = format!("/home/{}", user_info.username);

    // Core mounts for the repository setup
    let mut mounts = vec![
        // Original repo at symlink path (read-only, for git alternates)
        Mount::new(&info.repo_root, MountMode::ReadOnly).with_container_path(&info.repo_symlink),
        // Sandbox clone at the original repo path (write-through for working directory)
        Mount::new(&info.clone_dir, MountMode::WriteThrough).with_container_path(&info.repo_root),
        // Sandbox clone at its actual path (write-through for git operations)
        Mount::new(&info.clone_dir, MountMode::WriteThrough),
        // Overlay for target/ directory (Rust build artifacts, copy-on-write)
        Mount::new(info.repo_root.join("target"), MountMode::Overlay),
    ];

    // Read-only config mounts
    if let Some(ref home) = home {
        mounts.push(
            Mount::new(home.join(".gitconfig"), MountMode::ReadOnly)
                .with_container_path(format!("{}/.gitconfig", container_home)),
        );

        if user_info.uses_fish() {
            mounts.push(
                Mount::new(home.join(".config/fish"), MountMode::ReadOnly)
                    .with_container_path(format!("{}/.config/fish", container_home)),
            );
        }

        mounts.push(
            Mount::new(home.join(".config/nvim"), MountMode::ReadOnly)
                .with_container_path(format!("{}/.config/nvim", container_home)),
        );
    }

    mounts
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
        // Set working directory to the repo path (where clone is mounted)
        "--workdir".to_string(),
        info.repo_root.to_string_lossy().to_string(),
    ];

    // Build and process the mount list
    let mounts = build_mount_list(info, user_info);
    args.extend(process_mounts(&mounts, info)?);

    // Special handling for ~/.claude.json (needs filtering, not just copying)
    if let Some(home) = dirs::home_dir() {
        let claude_json = home.join(".claude.json");
        if claude_json.exists() {
            let copy_path = info.sandbox_dir.join("claude.json");
            let filtered_json = filter_claude_json(&claude_json, &info.repo_root)?;
            std::fs::write(&copy_path, &filtered_json)
                .with_context(|| format!("Failed to write filtered {}", copy_path.display()))?;
            args.extend([
                "--mount".to_string(),
                format!(
                    "type=bind,source={},target=/home/{}/.claude.json",
                    copy_path.display(),
                    user_info.username
                ),
            ]);
        }
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
