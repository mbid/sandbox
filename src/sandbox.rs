use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use reflink_copy::reflink_or_copy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::{
    get_meta_git_dir, get_sandbox_base_dir, get_sandbox_instance_dir, OverlayMode, Runtime,
    UserInfo,
};
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

/// Recursively copy a directory using reflink_or_copy for files.
/// This leverages copy-on-write on filesystems that support it (like btrfs).
fn copy_dir_reflink(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            copy_dir_reflink(&src_path, &dst_path)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&src_path)?;
            std::os::unix::fs::symlink(&target, &dst_path)?;
        } else {
            reflink_or_copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }

    Ok(())
}

/// Process mounts and generate docker arguments.
fn process_mounts(
    mounts: &[Mount],
    info: &SandboxInfo,
    overlay_mode: OverlayMode,
) -> Result<Vec<String>> {
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
                    match overlay_mode {
                        OverlayMode::Copy => {
                            // Eagerly copy the directory and bind mount it
                            let copy_dir = info.overlays_dir().join(&name).join("copy");
                            if !copy_dir.exists() {
                                copy_dir_reflink(&mount.host_path, &copy_dir).with_context(
                                    || {
                                        format!(
                                            "Failed to copy directory {} to {}",
                                            mount.host_path.display(),
                                            copy_dir.display()
                                        )
                                    },
                                )?;
                            }
                            docker_args.extend([
                                "--mount".to_string(),
                                format!(
                                    "type=bind,source={},target={}",
                                    copy_dir.display(),
                                    target.display()
                                ),
                            ]);
                        }
                        OverlayMode::Overlayfs => {
                            // Use overlayfs for directories
                            let overlay = info.create_overlay(&name, &mount.host_path);
                            overlay.create_volume()?;
                            docker_args.extend(overlay.docker_mount_args(&target));
                        }
                    }
                } else {
                    // Copy file to sandbox directory and bind mount it
                    let copy_path = info.sandbox_dir.join(&name);
                    reflink_or_copy(&mount.host_path, &copy_path).with_context(|| {
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

/// Fix ownership of mount parent directories inside the container.
///
/// When Docker creates parent directories for bind mounts, they're owned by root.
/// This function fixes ownership to match the host: for each mount target's parent
/// directories, if the corresponding host parent is owned by the user, we chown
/// the container parent to the user (if it was created by Docker, i.e., owned by root).
fn fix_mount_parent_ownership(
    container_name: &str,
    mounts: &[Mount],
    user_info: &UserInfo,
) -> Result<()> {
    // Collect all (host_parent, container_parent) pairs by walking up both paths in parallel
    let mut container_parents_to_fix: HashSet<PathBuf> = HashSet::new();

    for mount in mounts {
        if !mount.host_path.exists() {
            continue;
        }

        let mut host_path = mount.host_path.clone();
        let mut container_path = mount.target_path().to_path_buf();

        loop {
            let Some(host_parent) = host_path.parent() else {
                break;
            };
            let Some(container_parent) = container_path.parent() else {
                break;
            };

            // Stop at root
            if host_parent.as_os_str().is_empty() || container_parent.as_os_str().is_empty() {
                break;
            }

            // Check if host parent is owned by the user
            if let Ok(meta) = std::fs::metadata(host_parent) {
                if meta.uid() == user_info.uid {
                    container_parents_to_fix.insert(container_parent.to_path_buf());
                }
            }

            host_path = host_parent.to_path_buf();
            container_path = container_parent.to_path_buf();
        }
    }

    if container_parents_to_fix.is_empty() {
        return Ok(());
    }

    // Build a shell script that checks each directory and chowns if owned by root
    let paths: Vec<_> = container_parents_to_fix
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    // Use a heredoc-style approach to safely pass paths
    let script = format!(
        r#"for p in {}; do
            if [ -d "$p" ] && [ "$(stat -c '%u' "$p")" = "0" ]; then
                chown {}:{} "$p"
            fi
        done"#,
        paths
            .iter()
            .map(|p| format!("'{}'", p.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" "),
        user_info.uid,
        user_info.gid
    );

    let status = Command::new("docker")
        .args([
            "exec",
            "--user",
            "root",
            container_name,
            "sh",
            "-c",
            &script,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("Failed to fix mount parent ownership")?;

    if !status.success() {
        // Non-fatal: log but continue
        warn!("Failed to fix some mount parent directory ownership");
    }

    Ok(())
}

/// Metadata about a sandbox instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub name: String,
    pub repo_root: PathBuf,
    pub sandbox_dir: PathBuf,
    /// The actual clone directory (in cache)
    pub clone_dir: PathBuf,
    /// Path to the shared meta.git bare repository
    pub meta_git_dir: PathBuf,
    /// Directory for PID files tracking attached processes
    pub pids_dir: PathBuf,
    pub container_name: String,
    pub created_at: String,
}

impl SandboxInfo {
    pub fn new(name: &str, repo_root: &Path) -> Result<Self> {
        let sandbox_dir = get_sandbox_instance_dir(repo_root, name)?;
        let clone_dir = sandbox_dir.join("clone");
        let pids_dir = sandbox_dir.join("pids");
        let meta_git_dir = get_meta_git_dir(repo_root)?;

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
            meta_git_dir,
            pids_dir,
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

    /// Write a PID file for the current process to track active attachments.
    pub fn write_pid_file(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.pids_dir)?;
        let pid = std::process::id();
        let pid_file = self.pids_dir.join(format!("{}.pid", pid));
        std::fs::write(&pid_file, pid.to_string())?;
        Ok(pid_file)
    }

    /// Remove our PID file.
    pub fn remove_pid_file(&self) {
        let pid = std::process::id();
        let pid_file = self.pids_dir.join(format!("{}.pid", pid));
        let _ = std::fs::remove_file(&pid_file);
    }

    /// Check if any other processes are still attached (have live PIDs).
    pub fn has_other_live_processes(&self) -> bool {
        let our_pid = std::process::id();

        let entries = match std::fs::read_dir(&self.pids_dir) {
            Ok(e) => e,
            Err(_) => return false,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "pid") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        // Skip our own PID
                        if pid == our_pid {
                            continue;
                        }
                        // Check if process is alive
                        if process_is_alive(pid) {
                            return true;
                        } else {
                            // Clean up stale PID file
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }

        false
    }
}

/// Check if a process with the given PID is alive.
fn process_is_alive(pid: u32) -> bool {
    // On Unix, sending signal 0 checks if process exists without affecting it
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Spawn the sync daemon as a detached background process.
fn spawn_sync_daemon(info: &SandboxInfo) -> Result<()> {
    let exe = std::env::current_exe().context("Failed to get current executable path")?;

    Command::new(exe)
        .args(["sync-daemon", &info.sandbox_dir.to_string_lossy()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to spawn sync daemon")?;

    info!(
        "Sync daemon started (log: {})",
        info.sandbox_dir.join("sync.log").display()
    );

    Ok(())
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

/// Remove a directory and all its contents, fixing permissions as needed.
/// This is similar to `std::fs::remove_dir_all` but handles permission issues
/// by making directories/files writable before attempting deletion.
fn remove_dir_all_with_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if !path.exists() {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Failed to get metadata for: {}", path.display()))?;

    if metadata.is_dir() {
        // Make directory readable/writable/executable so we can list and modify it
        let mut perms = metadata.permissions();
        perms.set_mode(perms.mode() | 0o700);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("Failed to set permissions for: {}", path.display()))?;

        // Recursively delete contents
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("Failed to read directory: {}", path.display()))?
        {
            let entry = entry?;
            remove_dir_all_with_permissions(&entry.path())?;
        }

        // Remove the now-empty directory
        std::fs::remove_dir(path)
            .with_context(|| format!("Failed to remove directory: {}", path.display()))?;
    } else {
        // For files and symlinks, make writable then remove
        if !metadata.is_symlink() {
            let mut perms = metadata.permissions();
            perms.set_mode(perms.mode() | 0o600);
            std::fs::set_permissions(path, perms)
                .with_context(|| format!("Failed to set permissions for: {}", path.display()))?;
        }

        std::fs::remove_file(path)
            .with_context(|| format!("Failed to remove file: {}", path.display()))?;
    }

    Ok(())
}

/// Delete a sandbox and its associated resources.
pub fn delete_sandbox(info: &SandboxInfo) -> Result<()> {
    info!("Deleting sandbox: {}", info.name);

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

    // Remove sandbox branch from meta.git
    if info.meta_git_dir.exists() {
        let _ = Command::new("git")
            .current_dir(&info.meta_git_dir)
            .args(["branch", "-D", &info.name])
            .stderr(Stdio::null())
            .status();
    }

    // Remove remote tracking ref from host repo
    let _ = Command::new("git")
        .current_dir(&info.repo_root)
        .args([
            "update-ref",
            "-d",
            &format!("refs/remotes/sandbox/{}", info.name),
        ])
        .status();

    // Remove sandbox directory (includes overlay upper/work dirs)
    if info.sandbox_dir.exists() {
        remove_dir_all_with_permissions(&info.sandbox_dir)
            .with_context(|| format!("Failed to remove: {}", info.sandbox_dir.display()))?;
    }

    // Check if this was the last sandbox for this repo
    // If so, clean up meta.git and the sandbox remote
    let remaining_sandboxes = list_sandboxes(&info.repo_root)?;
    if remaining_sandboxes.is_empty() {
        info!("Last sandbox deleted, cleaning up meta.git...");

        // Remove meta.git
        if info.meta_git_dir.exists() {
            remove_dir_all_with_permissions(&info.meta_git_dir)
                .with_context(|| format!("Failed to remove: {}", info.meta_git_dir.display()))?;
        }

        // Remove sandbox remote from host repo
        let _ = Command::new("git")
            .current_dir(&info.repo_root)
            .args(["remote", "remove", "sandbox"])
            .status();
    }

    Ok(())
}

/// Build the list of mounts for a sandbox container.
fn build_mount_list(info: &SandboxInfo, user_info: &UserInfo) -> Vec<Mount> {
    let home = dirs::home_dir();
    let container_home = format!("/home/{}", user_info.username);

    // Core mounts for the repository setup
    let mut mounts = vec![
        // meta.git at its actual path (read-only, for git alternates and sandbox remote)
        // The sandbox clone's alternates reference this path
        Mount::new(&info.meta_git_dir, MountMode::ReadOnly),
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

/// Ensure the container is running (start it if not), then exec a command into it.
/// Uses reference counting via PID files to determine when to stop the container.
/// If we launch a new container, also spawns the sync daemon.
pub fn run_sandbox(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    command: Option<&[String]>,
) -> Result<()> {
    // Warn about overlayfs + sysbox combination
    if matches!(runtime, Runtime::SysboxRunc) && matches!(overlay_mode, OverlayMode::Overlayfs) {
        warn!(
            "Using overlayfs with sysbox-runc may cause permission issues. \
             Consider using --overlay-mode copy instead. \
             See: https://github.com/nestybox/sysbox/issues/968"
        );
    }

    // Ensure container is running
    let launched = ensure_container_running(info, image_tag, user_info, runtime, overlay_mode)?;

    // If we launched a new container, spawn the sync daemon
    if launched {
        spawn_sync_daemon(info)?;
    }

    // Write our PID file to track this attachment
    info.write_pid_file()?;

    // Determine the command to run
    let default_shell = if user_info.uses_fish() {
        "fish".to_string()
    } else {
        "bash".to_string()
    };

    let cmd: Vec<&str> = if let Some(c) = command {
        c.iter().map(|s| s.as_str()).collect()
    } else {
        vec![default_shell.as_str()]
    };

    debug!("Executing in container: {}", info.container_name);

    // Execute the command - capture result but don't return early
    let exec_result = docker::exec_in_container(&info.container_name, &cmd);

    // Cleanup: remove our PID file first
    info.remove_pid_file();

    // Check if we should stop the container
    if !info.has_other_live_processes() {
        debug!("No other processes attached, stopping container...");
        docker::stop_container(&info.container_name)?;
    }

    exec_result
}

/// Ensure the container is running, starting it if necessary.
/// Returns true if we launched a new container, false if it was already running.
pub fn ensure_container_running(
    info: &SandboxInfo,
    image_tag: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
) -> Result<bool> {
    // If already running, we're done
    if docker::container_is_running(&info.container_name)? {
        debug!("Container already running: {}", info.container_name);
        return Ok(false);
    }

    // Remove stopped container if it exists
    if docker::container_exists(&info.container_name)? {
        docker::remove_container(&info.container_name)?;
    }

    // Build docker run arguments for detached container with sleep infinity
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(), // Detached mode
        "--name".to_string(),
        info.container_name.clone(),
        "--hostname".to_string(),
        info.name.clone(),
        "--label".to_string(),
        "sandbox=true".to_string(),
        // Use configured runtime for sandboxing
        "--runtime".to_string(),
        runtime.docker_runtime_name().to_string(),
        // User mapping
        "--user".to_string(),
        format!("{}:{}", user_info.uid, user_info.gid),
        // Set working directory to the repo path (where clone is mounted)
        "--workdir".to_string(),
        info.repo_root.to_string_lossy().to_string(),
    ];

    // Build and process the mount list
    let mounts = build_mount_list(info, user_info);
    args.extend(process_mounts(&mounts, info, overlay_mode)?);

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

    // Run sleep infinity to keep container alive
    args.push("sleep".to_string());
    args.push("infinity".to_string());

    info!("Starting container: {}", info.container_name);

    let status = Command::new("docker")
        .args(&args)
        .stdout(Stdio::null())
        .status()
        .context("Failed to start docker container")?;

    if !status.success() {
        bail!("Failed to start container");
    }

    // Fix ownership of mount parent directories that Docker created as root
    fix_mount_parent_ownership(&info.container_name, &mounts, user_info)?;

    Ok(true)
}

/// Ensure a sandbox is set up and ready to use.
pub fn ensure_sandbox(repo_root: &Path, name: &str) -> Result<SandboxInfo> {
    let info = SandboxInfo::new(name, repo_root)?;

    // Create sandbox directory
    std::fs::create_dir_all(&info.sandbox_dir)?;

    // Ensure meta.git bare repository exists (shared across all sandboxes for this repo)
    git::ensure_meta_git(&info.repo_root, &info.meta_git_dir)?;

    // Setup "sandbox" remote in host repo pointing to meta.git
    git::setup_host_sandbox_remote(&info.repo_root, &info.meta_git_dir)?;

    // Sync main branch from host to meta.git
    git::sync_main_to_meta(&info.repo_root, &info.meta_git_dir)?;

    // Create shared clone from meta.git
    // The clone's alternates will reference meta_git_dir, which is mounted
    // at the same path inside the container
    git::create_shared_clone(&info.meta_git_dir, &info.clone_dir)?;

    // Checkout or create a branch named after the sandbox
    // This ensures all work in the sandbox happens on this branch
    git::checkout_or_create_branch(&info.clone_dir, name)?;

    // Setup remotes for the sandbox repo (rename "origin" to "sandbox")
    git::setup_sandbox_remotes(&info.meta_git_dir, &info.clone_dir)?;

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
