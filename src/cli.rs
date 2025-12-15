use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::config::{OverlayMode, Runtime, UserInfo};
use crate::docker;
use crate::git;
use crate::sandbox;
use crate::sync;

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

    /// Internal: sync daemon process (not shown in help)
    #[command(hide = true)]
    SyncDaemon {
        /// Path to the sandbox directory
        sandbox_dir: PathBuf,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::SyncDaemon { sandbox_dir } => {
            // Sync daemon doesn't need repo_root or user_info - it loads SandboxInfo
            let info = sandbox::SandboxInfo::load(&sandbox_dir)?;
            sync::run_sync_daemon(&info)?;
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
                    command,
                } => {
                    run_sandbox(
                        &repo_root,
                        &name,
                        &user_info,
                        runtime,
                        overlay_mode,
                        command,
                    )?;
                }
                Commands::List => {
                    list_sandboxes(&repo_root)?;
                }
                Commands::Delete { name } => {
                    delete_sandbox(&repo_root, &name)?;
                }
                Commands::SyncDaemon { .. } => unreachable!(),
            }
        }
    }

    Ok(())
}

fn run_sandbox(
    repo_root: &PathBuf,
    name: &str,
    user_info: &UserInfo,
    runtime: Runtime,
    overlay_mode: OverlayMode,
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

    let result = sandbox::run_sandbox(&info, &image_tag, user_info, runtime, overlay_mode, cmd);

    result
}

fn list_sandboxes(repo_root: &PathBuf) -> Result<()> {
    let sandboxes = sandbox::list_sandboxes(repo_root)?;

    if sandboxes.is_empty() {
        println!("No sandboxes found for this repository.");
        return Ok(());
    }

    println!("{:<20} {:<15} {:<30}", "NAME", "STATUS", "CREATED");
    println!("{}", "-".repeat(65));

    for info in sandboxes {
        let status = if docker::container_is_running(&info.container_name)? {
            "running"
        } else if docker::container_exists(&info.container_name)? {
            "stopped"
        } else {
            "not started"
        };

        println!("{:<20} {:<15} {:<30}", info.name, status, info.created_at);
    }

    Ok(())
}

fn delete_sandbox(repo_root: &PathBuf, name: &str) -> Result<()> {
    let sandboxes = sandbox::list_sandboxes(repo_root)?;

    let info = sandboxes
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("Sandbox '{}' not found", name))?;

    sandbox::delete_sandbox(&info)?;
    println!("Deleted sandbox: {}", name);

    Ok(())
}
