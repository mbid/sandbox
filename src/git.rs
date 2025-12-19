use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// Checkout a branch, creating it if it doesn't exist.
pub fn checkout_or_create_branch(repo: &Path, branch_name: &str) -> Result<()> {
    // Try to checkout existing branch first
    let status = Command::new("git")
        .current_dir(repo)
        .args(["checkout", branch_name])
        .stderr(Stdio::null())
        .status()
        .context("Failed to run git checkout")?;

    if status.success() {
        return Ok(());
    }

    // Branch doesn't exist, create it
    let status = Command::new("git")
        .current_dir(repo)
        .args(["checkout", "-b", branch_name])
        .status()
        .context("Failed to create branch")?;

    if !status.success() {
        bail!("Failed to create branch: {}", branch_name);
    }

    Ok(())
}

/// Fetch a specific branch from a remote into the local repo.
pub fn fetch_branch(repo: &Path, remote: &str, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["fetch", remote, branch])
        .status()
        .context("Failed to fetch branch")?;

    if !status.success() {
        bail!("Git fetch failed for {}:{}", remote, branch);
    }

    Ok(())
}

/// Ensure the meta.git bare repository exists.
/// Creates a bare clone of the host repo if it doesn't exist.
/// Returns true if a new meta.git was created, false if it already existed.
pub fn ensure_meta_git(host_repo: &Path, meta_git_dir: &Path) -> Result<bool> {
    if meta_git_dir.exists() {
        return Ok(false);
    }

    // Create parent directory if needed
    if let Some(parent) = meta_git_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    eprintln!(
        "Creating meta.git bare clone: {} -> {}",
        host_repo.display(),
        meta_git_dir.display()
    );

    let status = Command::new("git")
        .args([
            "clone",
            "--bare",
            &host_repo.to_string_lossy(),
            &meta_git_dir.to_string_lossy(),
        ])
        .status()
        .context("Failed to run git clone --bare")?;

    if !status.success() {
        bail!("Git bare clone failed");
    }

    // Sync main branch from host to ensure it's up to date
    sync_main_to_meta(host_repo, meta_git_dir)?;

    Ok(true)
}

/// Get the primary branch name (main or master) of a repository.
fn get_primary_branch(repo: &Path) -> Result<String> {
    // Try to get the default branch from HEAD
    let output = Command::new("git")
        .current_dir(repo)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .context("Failed to get HEAD branch")?;

    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            return Ok(branch);
        }
    }

    // Fallback: check if main exists, otherwise use master
    let status = Command::new("git")
        .current_dir(repo)
        .args(["show-ref", "--verify", "--quiet", "refs/heads/main"])
        .status()
        .context("Failed to check for main branch")?;

    if status.success() {
        Ok("main".to_string())
    } else {
        Ok("master".to_string())
    }
}

/// Sync the primary branch (main/master) from host repo to meta.git.
/// This is a ONE-WAY sync: host -> meta only.
pub fn sync_main_to_meta(host_repo: &Path, meta_git_dir: &Path) -> Result<()> {
    let branch = get_primary_branch(host_repo)?;

    // Fetch the branch from host into meta.git
    let status = Command::new("git")
        .current_dir(meta_git_dir)
        .args([
            "fetch",
            &host_repo.to_string_lossy(),
            &format!("{}:refs/heads/{}", branch, branch),
        ])
        .status()
        .context("Failed to fetch main branch to meta.git")?;

    if !status.success() {
        bail!("Failed to sync {} branch to meta.git", branch);
    }

    Ok(())
}

/// Sync a sandbox branch from the sandbox repo to meta.git.
pub fn sync_sandbox_to_meta(meta_git_dir: &Path, sandbox_repo: &Path, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .current_dir(meta_git_dir)
        .args([
            "fetch",
            &sandbox_repo.to_string_lossy(),
            &format!("{}:refs/heads/{}", branch, branch),
        ])
        .status()
        .context("Failed to sync sandbox branch to meta.git")?;

    if !status.success() {
        bail!("Failed to sync branch {} to meta.git", branch);
    }

    Ok(())
}

/// Sync a branch from meta.git to the host repo's remote tracking refs.
/// Updates refs/remotes/sandbox/<branch> in the host repo.
pub fn sync_meta_to_host(host_repo: &Path, meta_git_dir: &Path, branch: &str) -> Result<()> {
    // Fetch the specific branch from meta.git and update the remote tracking ref
    let status = Command::new("git")
        .current_dir(host_repo)
        .args([
            "fetch",
            &meta_git_dir.to_string_lossy(),
            &format!("refs/heads/{}:refs/remotes/sandbox/{}", branch, branch),
        ])
        .status()
        .context("Failed to sync meta.git branch to host")?;

    if !status.success() {
        bail!("Failed to sync branch {} from meta.git to host", branch);
    }

    Ok(())
}

/// Setup the "sandbox" remote in the host repo pointing to meta.git.
pub fn setup_host_sandbox_remote(host_repo: &Path, meta_git_dir: &Path) -> Result<()> {
    add_remote(host_repo, "sandbox", meta_git_dir)
}

/// Setup remotes for a sandbox repo.
/// Renames the "origin" remote (created by git clone) to "sandbox".
pub fn setup_sandbox_remotes(meta_git_dir: &Path, sandbox_repo: &Path) -> Result<()> {
    // Rename "origin" (created by git clone --shared) to "sandbox"
    let status = Command::new("git")
        .current_dir(sandbox_repo)
        .args(["remote", "rename", "origin", "sandbox"])
        .status()
        .context("Failed to rename origin remote to sandbox")?;

    if !status.success() {
        bail!("Failed to rename origin remote to sandbox");
    }

    // Update the URL to ensure it points to meta_git_dir
    let status = Command::new("git")
        .current_dir(sandbox_repo)
        .args([
            "remote",
            "set-url",
            "sandbox",
            &meta_git_dir.to_string_lossy(),
        ])
        .status()
        .context("Failed to set sandbox remote URL")?;

    if !status.success() {
        bail!("Failed to set sandbox remote URL");
    }

    // Allow fetching arbitrary SHAs (useful for syncing specific commits)
    let status = Command::new("git")
        .current_dir(sandbox_repo)
        .args(["config", "uploadpack.allowAnySHA1InWant", "true"])
        .status()
        .context("Failed to configure uploadpack.allowAnySHA1InWant")?;

    if !status.success() {
        bail!("Failed to configure sandbox repo");
    }

    Ok(())
}
