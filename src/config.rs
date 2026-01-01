use anyhow::{Context, Result};
use clap::ValueEnum;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Container runtime to use for sandboxing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    /// gVisor runtime (default) - strong isolation via kernel syscall interception
    #[default]
    Runsc,
    /// Standard OCI runtime - no additional isolation
    Runc,
    /// Sysbox runtime - enables Docker-in-Docker with VM-like isolation
    SysboxRunc,
}

/// Strategy for copy-on-write mounts (writes inside container don't propagate to host).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlayMode {
    /// Use overlayfs (default) - efficient but may have permission issues with sysbox
    #[default]
    Overlayfs,
    /// Eagerly copy the directory - works with all runtimes but uses more disk space
    Copy,
}

/// Claude model to use for the agent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Model {
    /// Claude Opus 4.5 - most capable model
    #[default]
    #[value(name = "opus")]
    Opus,
    /// Claude Sonnet 4.5 - balanced performance and cost
    #[value(name = "sonnet")]
    Sonnet,
    /// Claude Haiku 4.5 - fast and cost-effective
    #[value(name = "haiku")]
    Haiku,
}

impl Model {
    /// Get the model identifier as used by the Anthropic API.
    pub fn api_model_id(&self) -> &'static str {
        match self {
            Model::Opus => "claude-opus-4-5-20251101",
            Model::Sonnet => "claude-sonnet-4-5-20250929",
            Model::Haiku => "claude-haiku-4-5-20251001",
        }
    }
}

impl Runtime {
    /// Get the runtime name as used by Docker's --runtime flag.
    pub fn docker_runtime_name(&self) -> &'static str {
        match self {
            Runtime::Runsc => "runsc",
            Runtime::Runc => "runc",
            Runtime::SysboxRunc => "sysbox-runc",
        }
    }
}

/// Get the cache directory for sandbox data.
/// Uses $XDG_CACHE_HOME/sandbox or ~/.cache/sandbox as fallback.
pub fn get_cache_dir() -> Result<PathBuf> {
    let cache_base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .expect("Could not determine home directory")
                .join(".cache")
        });

    Ok(cache_base.join("sandbox"))
}

/// Compute a short hash of a path for use in directory names.
pub fn hash_path(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..8]) // Use first 8 bytes (16 hex chars)
}

/// Get the sandbox directory for a specific repo.
/// Format: $XDG_CACHE_HOME/sandbox/<repo-root-dir-name>-<sha2-of-repo-root-absolute-path>
pub fn get_sandbox_base_dir(repo_root: &Path) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;
    let repo_name = repo_root
        .file_name()
        .context("Repo root has no file name")?
        .to_string_lossy();
    let path_hash = hash_path(repo_root);

    Ok(cache_dir.join(format!("{}-{}", repo_name, path_hash)))
}

/// Get the directory for a specific named sandbox instance.
pub fn get_sandbox_instance_dir(repo_root: &Path, name: &str) -> Result<PathBuf> {
    let base = get_sandbox_base_dir(repo_root)?;
    Ok(base.join(name))
}

/// Get the path to the meta.git bare repository for a repo.
/// This bare repo is shared across all sandboxes for the same repository.
pub fn get_meta_git_dir(repo_root: &Path) -> Result<PathBuf> {
    let base = get_sandbox_base_dir(repo_root)?;
    Ok(base.join("meta.git"))
}

/// Hash the contents of a file (used for Dockerfile hash-based image tagging).
pub fn hash_file(path: &Path) -> Result<String> {
    let contents =
        std::fs::read(path).with_context(|| format!("Failed to read file: {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&contents);
    let result = hasher.finalize();
    Ok(hex::encode(&result[..16])) // Use first 16 bytes (32 hex chars)
}

/// Get current user information.
pub struct UserInfo {
    pub uid: u32,
    pub gid: u32,
    pub username: String,
    pub shell: String,
}

impl UserInfo {
    pub fn current() -> Result<Self> {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();

        let username = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| format!("user{}", uid));

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

        Ok(UserInfo {
            uid,
            gid,
            username,
            shell,
        })
    }

    pub fn uses_fish(&self) -> bool {
        self.shell.ends_with("/fish") || self.shell == "fish"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_path() {
        let path = Path::new("/home/user/project");
        let hash = hash_path(path);
        assert_eq!(hash.len(), 16);
    }

    #[test]
    fn test_get_sandbox_base_dir() {
        let repo_root = Path::new("/home/user/myproject");
        let base_dir = get_sandbox_base_dir(repo_root).unwrap();
        assert!(base_dir.to_string_lossy().contains("myproject-"));
    }
}
