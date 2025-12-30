use anyhow::{bail, Context, Result};
use chrono;
use clap::{Parser, Subcommand};
use env_logger::Builder;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use crate::agent;
use crate::config::{Model, OverlayMode, Runtime, UserInfo};
use crate::daemon;
use crate::docker;
use crate::git;
use crate::sandbox;

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

        /// Container runtime to use for sandboxing
        #[arg(short, long, value_enum, default_value_t = Runtime::Runsc)]
        runtime: Runtime,

        /// Strategy for copy-on-write mounts (directories that are writable inside the
        /// container but don't propagate changes to the host)
        #[arg(short, long, value_enum, default_value_t = OverlayMode::Overlayfs)]
        overlay_mode: OverlayMode,

        /// Pass through an environment variable from the host
        #[arg(long = "env", value_name = "VAR")]
        passthrough_env: Vec<String>,

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

        /// Container runtime to use for sandboxing
        #[arg(short, long, value_enum, default_value_t = Runtime::Runsc)]
        runtime: Runtime,

        /// Strategy for copy-on-write mounts
        #[arg(short, long, value_enum, default_value_t = OverlayMode::Overlayfs)]
        overlay_mode: OverlayMode,

        /// Claude model to use
        #[arg(short, long, value_enum, default_value_t = Model::Opus)]
        model: Model,

        /// Pass through an environment variable from the host
        #[arg(long = "env", value_name = "VAR")]
        passthrough_env: Vec<String>,
    },

    /// Internal daemon process (not shown in help)
    #[command(hide = true)]
    InternalDaemon {
        /// Path to the sandbox directory
        sandbox_dir: PathBuf,
        /// Docker image tag
        image_tag: String,
        /// Username for container
        username: String,
        /// UID for container
        uid: u32,
        /// GID for container
        gid: u32,
        /// Shell for container
        shell: String,
        /// Container runtime name
        runtime: String,
        /// Overlay mode
        overlay_mode: String,
        /// Environment variables in NAME=VALUE format
        #[arg(trailing_var_arg = true)]
        env_vars: Vec<String>,
    },
}

fn resolve_env_vars(var_names: &[String]) -> Result<Vec<(String, String)>> {
    var_names
        .iter()
        .map(|name| {
            std::env::var(name)
                .map(|value| (name.clone(), value))
                .map_err(|_| anyhow::anyhow!("environment variable '{}' is not set", name))
        })
        .collect()
}

fn init_logging(command: &Commands) -> Result<()> {
    match command {
        Commands::Enter { .. }
        | Commands::List
        | Commands::Delete { .. }
        | Commands::Agent { .. } => {
            env_logger::init();
        }
        Commands::InternalDaemon { sandbox_dir, .. } => {
            let log_path = sandbox_dir.join("daemon.log");
            let log_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .with_context(|| format!("Failed to open log file: {}", log_path.display()))?;
            Builder::from_env(env_logger::Env::default())
                .target(env_logger::Target::Pipe(Box::new(log_file)))
                .init();
        }
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_logging(&cli.command)?;

    match cli.command {
        Commands::InternalDaemon {
            sandbox_dir,
            image_tag,
            username,
            uid,
            gid,
            shell,
            runtime,
            overlay_mode,
            env_vars,
        } => {
            let info = sandbox::SandboxInfo::load(&sandbox_dir)?;
            let user_info = UserInfo {
                username,
                uid,
                gid,
                shell,
            };
            let runtime = match runtime.as_str() {
                "runsc" => Runtime::Runsc,
                "runc" => Runtime::Runc,
                "sysbox-runc" => Runtime::SysboxRunc,
                _ => bail!("Unknown runtime: {}", runtime),
            };
            let overlay_mode = match overlay_mode.as_str() {
                "overlayfs" => OverlayMode::Overlayfs,
                "copy" => OverlayMode::Copy,
                _ => bail!("Unknown overlay mode: {}", overlay_mode),
            };
            let env_vars: Vec<(String, String)> = env_vars
                .into_iter()
                .filter_map(|s| {
                    let mut parts = s.splitn(2, '=');
                    Some((parts.next()?.to_string(), parts.next()?.to_string()))
                })
                .collect();
            daemon::run_daemon_with_sync(
                &info,
                &image_tag,
                &user_info,
                runtime,
                overlay_mode,
                &env_vars,
            )?;
        }
        _ => {
            // All other commands need repo_root and user_info
            let repo_root = git::find_repo_root()?;
            let user_info = UserInfo::current()?;

            match cli.command {
                Commands::Enter {
                    name,
                    runtime,
                    overlay_mode,
                    passthrough_env,
                    command,
                } => {
                    let env_vars = resolve_env_vars(&passthrough_env)?;
                    run_sandbox(
                        &repo_root,
                        &name,
                        &user_info,
                        runtime,
                        overlay_mode,
                        &env_vars,
                        command,
                    )?;
                }
                Commands::List => {
                    list_sandboxes(&repo_root)?;
                }
                Commands::Delete { name } => {
                    delete_sandbox(&repo_root, &name)?;
                }
                Commands::Agent {
                    name,
                    runtime,
                    overlay_mode,
                    model,
                    passthrough_env,
                } => {
                    let env_vars = resolve_env_vars(&passthrough_env)?;
                    run_agent(
                        &repo_root,
                        &name,
                        &user_info,
                        runtime,
                        overlay_mode,
                        model,
                        &env_vars,
                    )?;
                }
                Commands::InternalDaemon { .. } => unreachable!(),
            }
        }
    }

    Ok(())
}

fn run_sandbox(
    repo_root: &Path,
    name: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    env_vars: &[(String, String)],
    command: Vec<String>,
) -> Result<()> {
    // Check for Dockerfile
    let dockerfile = repo_root.join("Dockerfile");
    if !dockerfile.exists() {
        bail!(
            "No Dockerfile found at {}. Please create a Dockerfile for the sandbox.",
            dockerfile.display()
        );
    }

    // Build or get existing image
    let image_tag = docker::build_image(&dockerfile, user_info)?;

    // Ensure sandbox is set up
    let info = sandbox::ensure_sandbox(repo_root, name)?;

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

    sandbox::delete_sandbox(&info)?;
    println!("Deleted sandbox: {}", name);

    Ok(())
}

fn run_agent(
    repo_root: &Path,
    name: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
    model: Model,
    env_vars: &[(String, String)],
) -> Result<()> {
    let dockerfile = repo_root.join("Dockerfile");
    if !dockerfile.exists() {
        bail!(
            "No Dockerfile found at {}. Please create a Dockerfile for the sandbox.",
            dockerfile.display()
        );
    }

    let image_tag = docker::build_image(&dockerfile, user_info)?;
    let info = sandbox::ensure_sandbox(repo_root, name)?;

    let _daemon_conn = sandbox::ensure_container_running(
        &info,
        &image_tag,
        user_info,
        runtime,
        overlay_mode,
        env_vars,
    )?;

    agent::run_agent(&info.container_name, model)
    // _daemon_conn is dropped here, signaling disconnection to daemon
}
