use anyhow::{bail, Result};
use chrono;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

use crate::agent;
use crate::config::{Model, OverlayMode, Runtime, UserInfo};
use crate::daemon;
use crate::docker;
use crate::git;
use crate::llm_cache::LlmCache;
use crate::sandbox;
use crate::sandbox_config::SandboxConfig;
use crate::setup;

#[derive(Parser)]
#[command(name = "sandbox")]
#[command(about = "Docker-based sandbox for untrusted LLM agents")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Enter a sandbox (create if needed)
    Enter {
        /// Name for this sandbox instance
        name: String,

        /// Container runtime (overrides config file, default: runsc)
        #[arg(short, long, value_enum)]
        runtime: Option<Runtime>,

        /// Strategy for copy-on-write mounts (overrides config file, default: overlayfs)
        #[arg(short, long, value_enum)]
        overlay_mode: Option<OverlayMode>,

        /// Command to run inside the sandbox (default: interactive shell)
        #[arg(last = true)]
        command: Vec<String>,
    },

    /// List all sandboxes for the current repository
    List,

    /// Delete a sandbox
    Delete {
        /// Name of the sandbox to delete
        name: String,
    },

    /// Run an LLM agent inside a sandbox
    Agent {
        /// Name of the sandbox to use
        name: String,

        /// Container runtime (overrides config file, default: runsc)
        #[arg(short, long, value_enum)]
        runtime: Option<Runtime>,

        /// Strategy for copy-on-write mounts (overrides config file, default: overlayfs)
        #[arg(short, long, value_enum)]
        overlay_mode: Option<OverlayMode>,

        /// Claude model to use (overrides config file)
        #[arg(short, long, value_enum)]
        model: Option<Model>,

        /// LLM response cache directory for deterministic testing.
        /// See llm-cache/README.md for documentation.
        #[arg(long, hide = true)]
        cache: Option<PathBuf>,
    },

    /// Run the sandbox daemon (manages sandboxes across all projects)
    Daemon,

    /// Install the sandbox daemon as a systemd user service
    SystemInstall,

    /// Uninstall the sandbox daemon from systemd
    SystemUninstall,
}

fn init_logging(_command: &Commands) -> Result<()> {
    env_logger::init();
    Ok(())
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.command)?;

    match cli.command {
        Commands::Daemon => {
            daemon::run_daemon()?;
        }
        Commands::SystemInstall => {
            setup::system_install()?;
        }
        Commands::SystemUninstall => {
            setup::system_uninstall()?;
        }
        Commands::Enter {
            name,
            runtime,
            overlay_mode,
            command,
        } => {
            let repo_root = git::find_repo_root()?;
            let user_info = UserInfo::current()?;
            let sandbox_config = SandboxConfig::load(&repo_root)?;
            let env_vars = sandbox_config.resolve_env_vars()?;
            // CLI flags override config file values
            let runtime = runtime.or(sandbox_config.runtime).unwrap_or_default();
            let overlay_mode = overlay_mode
                .or(sandbox_config.overlay_mode)
                .unwrap_or_default();
            run_sandbox(
                &repo_root,
                &sandbox_config,
                &name,
                &user_info,
                runtime,
                overlay_mode,
                &env_vars,
                command,
            )?;
        }
        Commands::List => {
            let repo_root = git::find_repo_root()?;
            list_sandboxes(&repo_root)?;
        }
        Commands::Delete { name } => {
            let repo_root = git::find_repo_root()?;
            delete_sandbox(&repo_root, &name)?;
        }
        Commands::Agent {
            name,
            runtime,
            overlay_mode,
            model,
            cache,
        } => {
            let repo_root = git::find_repo_root()?;
            let user_info = UserInfo::current()?;
            let sandbox_config = SandboxConfig::load(&repo_root)?;
            let env_vars = sandbox_config.resolve_env_vars()?;
            let llm_cache = cache
                .map(|dir| LlmCache::new(&dir, "anthropic"))
                .transpose()?;
            // CLI flags override config file values
            let runtime = runtime.or(sandbox_config.runtime).unwrap_or_default();
            let overlay_mode = overlay_mode
                .or(sandbox_config.overlay_mode)
                .unwrap_or_default();
            let model = model.or(sandbox_config.agent.model).unwrap_or_default();
            run_agent(
                &repo_root,
                &sandbox_config,
                &name,
                &user_info,
                runtime,
                overlay_mode,
                model,
                &env_vars,
                llm_cache,
            )?;
        }
    }

    Ok(())
}

/// Resolve the Docker image tag from config, building if necessary.
fn resolve_image_tag(
    repo_root: &Path,
    config: &SandboxConfig,
    user_info: &UserInfo,
) -> Result<String> {
    use crate::sandbox_config::ImageConfig;

    match &config.image {
        Some(ImageConfig::Tag(tag)) => Ok(tag.clone()),
        Some(ImageConfig::Build {
            dockerfile,
            context,
        }) => {
            let dockerfile_path = repo_root.join(dockerfile);
            if !dockerfile_path.exists() {
                bail!("Dockerfile not found at {}", dockerfile_path.display());
            }
            let context_path = context
                .as_ref()
                .map(|p| repo_root.join(p))
                .unwrap_or_else(|| repo_root.to_path_buf());
            docker::build_image(&dockerfile_path, &context_path, user_info)
        }
        None => {
            // Default: look for Dockerfile in repo root
            let dockerfile_path = repo_root.join("Dockerfile");
            if !dockerfile_path.exists() {
                bail!(
                    "No Dockerfile found at {}.\n\
                     Either create a Dockerfile or specify an image in .sandbox.toml:\n\n\
                     [image]\n\
                     tag = \"your-image:tag\"\n",
                    dockerfile_path.display()
                );
            }
            docker::build_image(&dockerfile_path, repo_root, user_info)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_sandbox(
    repo_root: &Path,
    config: &SandboxConfig,
    name: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
    command: Vec<String>,
) -> Result<()> {
    let image_tag = resolve_image_tag(repo_root, config, user_info)?;

    // Ensure sandbox is set up (saves mounts config for daemon to use)
    let info = sandbox::ensure_sandbox(repo_root, name, config)?;

    // Run the sandbox
    let cmd = if command.is_empty() {
        None
    } else {
        Some(command.as_slice())
    };

    sandbox::run_sandbox(
        &info,
        &image_tag,
        user_info,
        runtime,
        overlay_mode,
        env_vars,
        cmd,
    )
}

fn list_sandboxes(repo_root: &Path) -> Result<()> {
    let mut sandboxes = sandbox::list_sandboxes(repo_root)?;

    if sandboxes.is_empty() {
        println!("No sandboxes found for this repository.");
        return Ok(());
    }

    // Sort by created_at, most recent first
    sandboxes.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    println!("{:<20} {:<15} {:<20}", "NAME", "STATUS", "CREATED");
    println!("{}", "-".repeat(55));

    for info in sandboxes {
        let status = if docker::container_is_running(&info.container_name)? {
            "running"
        } else if docker::container_exists(&info.container_name)? {
            "stopped"
        } else {
            "not started"
        };

        // Format date more human-friendly
        let created = chrono::DateTime::parse_from_rfc3339(&info.created_at)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or(info.created_at.clone());

        println!("{:<20} {:<15} {:<20}", info.name, status, created);
    }

    Ok(())
}

fn delete_sandbox(repo_root: &Path, name: &str) -> Result<()> {
    let sandboxes = sandbox::list_sandboxes(repo_root)?;

    let info = sandboxes
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("Sandbox '{}' not found", name))?;

    if docker::container_is_running(&info.container_name)? {
        println!("Container is still running, waiting for it to stop...");
        docker::wait_container(&info.container_name)?;
    }

    sandbox::delete_sandbox(&info)?;
    println!("Deleted sandbox: {}", name);

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_agent(
    repo_root: &Path,
    config: &SandboxConfig,
    name: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    model: Model,
    env_vars: &[(String, String)],
    llm_cache: Option<LlmCache>,
) -> Result<()> {
    let image_tag = resolve_image_tag(repo_root, config, user_info)?;
    let info = sandbox::ensure_sandbox(repo_root, name, config)?;

    let _daemon_conn = sandbox::ensure_container_running(
        &info,
        &image_tag,
        user_info,
        runtime,
        overlay_mode,
        env_vars,
    )?;

    agent::run_agent(&info.container_name, model, llm_cache)
    // _daemon_conn is dropped here, signaling disconnection to daemon
}
