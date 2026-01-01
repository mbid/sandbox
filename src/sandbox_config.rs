//! Parser for the `.sandbox.toml` configuration file at the repository root.
//!
//! This file specifies sandbox settings: environment variables to pass through,
//! mount configurations, image build settings, and agent options.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config::{Model, OverlayMode, Runtime};

/// Top-level configuration structure parsed from `.sandbox.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Environment variables that must be set on the host and passed to the container.
    #[serde(default)]
    pub env: Vec<String>,

    /// Container runtime (runsc, runc, sysbox-runc).
    #[serde(default)]
    pub runtime: Option<Runtime>,

    /// Strategy for copy-on-write mounts (overlayfs or copy).
    #[serde(default, rename = "overlay-mode")]
    pub overlay_mode: Option<OverlayMode>,

    #[serde(default)]
    pub mounts: MountsConfig,

    #[serde(default)]
    pub image: Option<ImageConfig>,

    #[serde(default)]
    pub agent: AgentConfig,
}

/// Mount configuration with different mount types.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MountsConfig {
    /// Read-only mounts (container cannot modify).
    #[serde(default)]
    pub readonly: Vec<MountEntry>,

    /// Write-through mounts (changes propagate to host).
    /// Named "unsafe_write" to indicate the risk of host modification.
    #[serde(default, rename = "unsafe-write")]
    pub unsafe_write: Vec<MountEntry>,

    /// Copy-on-write / overlay mounts (isolated writes).
    #[serde(default)]
    pub overlay: Vec<MountEntry>,
}

/// A single mount entry specifying host and container paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountEntry {
    /// Path on the host filesystem.
    /// - Relative paths are relative to repo root
    /// - `~` prefix expands to user's home directory
    /// - Absolute paths are used as-is
    pub host: PathBuf,

    /// Path inside the container. Defaults to host path if omitted.
    /// Same expansion rules apply.
    pub container: Option<PathBuf>,
}

/// Docker image configuration - either a pre-built tag or build from Dockerfile.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ImageConfig {
    /// Use a pre-built image tag.
    #[serde(rename = "tag")]
    Tag(String),

    /// Build from a Dockerfile.
    #[serde(rename = "build")]
    Build {
        /// Path to Dockerfile (relative to repo root).
        dockerfile: PathBuf,
        /// Build context directory (relative to repo root). Defaults to repo root.
        context: Option<PathBuf>,
    },
}

/// Agent configuration.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Default model.
    pub model: Option<Model>,

    /// Editor for composing messages.
    pub editor: Option<String>,
}

impl SandboxConfig {
    /// Load config from the `.sandbox.toml` file in the given repo root.
    /// Returns an error if the file doesn't exist.
    pub fn load(repo_root: &Path) -> Result<Self> {
        let config_path = repo_root.join(".sandbox.toml");

        if !config_path.exists() {
            bail!(
                "No .sandbox.toml config file found at {}.\n\
                 Please create a .sandbox.toml file to configure the sandbox.\n\
                 Example minimal config:\n\n\
                 env = [\"ANTHROPIC_API_KEY\"]\n",
                config_path.display()
            );
        }

        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;

        let config: SandboxConfig = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;

        Ok(config)
    }

    /// Resolve environment variables from the host.
    /// Returns an error if any variable is not set.
    pub fn resolve_env_vars(&self) -> Result<Vec<(String, String)>> {
        self.env
            .iter()
            .map(|name| {
                std::env::var(name)
                    .map(|value| (name.clone(), value))
                    .with_context(|| format!("Required environment variable '{}' is not set", name))
            })
            .collect()
    }

    /// Expand a path according to the rules:
    /// - `~` prefix -> user's home directory
    /// - Relative path -> relative to repo root
    /// - Absolute path -> as-is
    pub fn expand_host_path(path: &Path, repo_root: &Path) -> Result<PathBuf> {
        let path_str = path.to_string_lossy();
        if let Some(suffix) = path_str.strip_prefix("~/") {
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(home.join(suffix))
        } else if path_str == "~" {
            dirs::home_dir().context("Could not determine home directory")
        } else if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(repo_root.join(path))
        }
    }

    /// Expand a container path. Similar to expand_host_path but uses
    /// /home/<username> for ~ expansion inside the container.
    pub fn expand_container_path(path: &Path, username: &str) -> PathBuf {
        let path_str = path.to_string_lossy();
        if let Some(suffix) = path_str.strip_prefix("~/") {
            PathBuf::from(format!("/home/{}/{}", username, suffix))
        } else if path_str == "~" {
            PathBuf::from(format!("/home/{}", username))
        } else {
            path.to_path_buf()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OverlayMode, Runtime};
    use std::fs;
    use tempfile::TempDir;

    fn create_config(dir: &Path, content: &str) {
        fs::write(dir.join(".sandbox.toml"), content).unwrap();
    }

    #[test]
    fn test_minimal_config() {
        let dir = TempDir::new().unwrap();
        create_config(
            dir.path(),
            r#"
env = ["ANTHROPIC_API_KEY"]
"#,
        );

        let config = SandboxConfig::load(dir.path()).unwrap();
        assert_eq!(config.env, vec!["ANTHROPIC_API_KEY"]);
        assert!(config.mounts.readonly.is_empty());
        assert!(config.mounts.unsafe_write.is_empty());
        assert!(config.mounts.overlay.is_empty());
    }

    #[test]
    fn test_full_config() {
        let dir = TempDir::new().unwrap();
        create_config(
            dir.path(),
            r#"
env = ["ANTHROPIC_API_KEY", "GITHUB_TOKEN"]

runtime = "sysbox-runc"
overlay-mode = "copy"

[[mounts.readonly]]
host = "~/.gitconfig"

[[mounts.readonly]]
host = "~/.config/nvim"
container = "~/.config/nvim"

[[mounts.unsafe-write]]
host = ".env.local"

[[mounts.overlay]]
host = "target"

[[mounts.overlay]]
host = "~/.cargo/registry"
container = "~/.cargo/registry"

[image.build]
dockerfile = "Dockerfile"
context = "."

[agent]
model = "sonnet"
editor = "vim"
"#,
        );

        let config = SandboxConfig::load(dir.path()).unwrap();
        assert_eq!(config.env, vec!["ANTHROPIC_API_KEY", "GITHUB_TOKEN"]);
        assert_eq!(config.runtime, Some(Runtime::SysboxRunc));
        assert_eq!(config.overlay_mode, Some(OverlayMode::Copy));
        assert_eq!(config.mounts.readonly.len(), 2);
        assert_eq!(config.mounts.unsafe_write.len(), 1);
        assert_eq!(config.mounts.overlay.len(), 2);
        match &config.image {
            Some(ImageConfig::Build {
                dockerfile,
                context,
            }) => {
                assert_eq!(dockerfile, &PathBuf::from("Dockerfile"));
                assert_eq!(context, &Some(PathBuf::from(".")));
            }
            _ => panic!("Expected ImageConfig::Build"),
        }
        assert_eq!(config.agent.model, Some(Model::Sonnet));
        assert_eq!(config.agent.editor, Some("vim".to_string()));
    }

    #[test]
    fn test_image_tag() {
        let dir = TempDir::new().unwrap();
        create_config(
            dir.path(),
            r#"
[image]
tag = "myimage:latest"
"#,
        );

        let config = SandboxConfig::load(dir.path()).unwrap();
        match &config.image {
            Some(ImageConfig::Tag(tag)) => assert_eq!(tag, "myimage:latest"),
            _ => panic!("Expected ImageConfig::Tag"),
        }
    }

    #[test]
    fn test_missing_config_file() {
        let dir = TempDir::new().unwrap();
        let result = SandboxConfig::load(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No .sandbox.toml"));
    }

    #[test]
    fn test_expand_host_path() {
        let repo_root = Path::new("/repo");

        // Relative path
        let expanded =
            SandboxConfig::expand_host_path(&PathBuf::from("target"), repo_root).unwrap();
        assert_eq!(expanded, PathBuf::from("/repo/target"));

        // Absolute path
        let expanded =
            SandboxConfig::expand_host_path(&PathBuf::from("/etc/hosts"), repo_root).unwrap();
        assert_eq!(expanded, PathBuf::from("/etc/hosts"));

        // Home-relative path
        let expanded =
            SandboxConfig::expand_host_path(&PathBuf::from("~/.gitconfig"), repo_root).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(expanded, home.join(".gitconfig"));
    }

    #[test]
    fn test_expand_container_path() {
        // Relative path stays as-is (will be relative to workdir in container)
        let expanded = SandboxConfig::expand_container_path(&PathBuf::from("target"), "testuser");
        assert_eq!(expanded, PathBuf::from("target"));

        // Absolute path stays as-is
        let expanded =
            SandboxConfig::expand_container_path(&PathBuf::from("/etc/hosts"), "testuser");
        assert_eq!(expanded, PathBuf::from("/etc/hosts"));

        // Home-relative path expands to /home/<username>
        let expanded =
            SandboxConfig::expand_container_path(&PathBuf::from("~/.gitconfig"), "testuser");
        assert_eq!(expanded, PathBuf::from("/home/testuser/.gitconfig"));
    }

    #[test]
    fn test_unknown_field_rejected() {
        let dir = TempDir::new().unwrap();
        create_config(
            dir.path(),
            r#"
env = ["FOO"]
unknown_field = "value"
"#,
        );

        let result = SandboxConfig::load(dir.path());
        assert!(result.is_err());
    }
}
