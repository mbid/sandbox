use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Find the root of the current git repository.
pub fn find_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git rev-parse")?;

    if !output.status.success() {
        bail!("Not in a git repository");
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

/// Create a shared clone of a git repository.
/// A shared clone uses --shared to reference the source repo's objects.
pub fn create_shared_clone(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        eprintln!("Shared clone already exists at: {}", dest.display());
        return Ok(());
    }

    // Create parent directory if needed
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    eprintln!(
        "Creating shared clone: {} -> {}",
        source.display(),
        dest.display()
    );

    let status = Command::new("git")
        .args([
            "clone",
            "--shared",
            &source.to_string_lossy(),
            &dest.to_string_lossy(),
        ])
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("Git clone failed");
    }

    Ok(())
}

/// Add a remote to a git repository.
pub fn add_remote(repo: &Path, name: &str, url: &Path) -> Result<()> {
    // Check if remote already exists
    let output = Command::new("git")
        .current_dir(repo)
        .args(["remote", "get-url", name])
        .output()
        .context("Failed to check remote")?;

    if output.status.success() {
        // Remote exists, update it
        let status = Command::new("git")
            .current_dir(repo)
            .args(["remote", "set-url", name, &url.to_string_lossy()])
            .status()
            .context("Failed to update remote")?;

        if !status.success() {
            bail!("Failed to update remote: {}", name);
        }
    } else {
        // Remote doesn't exist, add it
        let status = Command::new("git")
            .current_dir(repo)
            .args(["remote", "add", name, &url.to_string_lossy()])
            .status()
            .context("Failed to add remote")?;

        if !status.success() {
            bail!("Failed to add remote: {}", name);
        }
    }

    Ok(())
}

/// Setup bidirectional remotes between two repos.
pub fn setup_bidirectional_remotes(
    main_repo: &Path,
    sandbox_repo: &Path,
    sandbox_name: &str,
) -> Result<()> {
    // Add remote from main repo to sandbox
    let remote_name = format!("sandbox-{}", sandbox_name);
    add_remote(main_repo, &remote_name, sandbox_repo)?;

    // Add remote from sandbox to main repo (named "origin" typically, but we'll use "main")
    add_remote(sandbox_repo, "main", main_repo)?;

    Ok(())
}

/// Get the current branch name.
pub fn get_current_branch(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["branch", "--show-current"])
        .output()
        .context("Failed to get current branch")?;

    if !output.status.success() {
        bail!("Failed to get current branch");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Fetch all remotes in a repository.
pub fn fetch_all(repo: &Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["fetch", "--all"])
        .status()
        .context("Failed to fetch")?;

    if !status.success() {
        bail!("Git fetch failed");
    }

    Ok(())
}

/// Update server info for git repository (needed for some operations).
pub fn update_server_info(repo: &Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["update-server-info"])
        .status()
        .context("Failed to update server info")?;

    // This command may fail if not a bare repo, that's OK
    if !status.success() {
        // Ignore failure
    }

    Ok(())
}
