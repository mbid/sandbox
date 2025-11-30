use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::UserInfo;
use crate::docker;
use crate::git;
use crate::sandbox::{self, SandboxInfo};
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
    /// Create and run a sandbox
    Run {
        /// Name for this sandbox instance
        name: String,

        /// Command to run inside the sandbox (default: interactive shell)
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },

    /// Attach to an existing running sandbox
    Attach {
        /// Name of the sandbox to attach to
        name: String,
    },

    /// List all sandboxes for the current repository
    List,

    /// Delete a sandbox
    Delete {
        /// Name of the sandbox to delete
        name: String,
    },

    /// Clean up orphaned Docker volumes
    Cleanup,

    /// Start git sync watcher for a sandbox (runs in foreground)
    Sync {
        /// Name of the sandbox to sync
        name: String,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    // Find the repository root
    let repo_root = git::find_repo_root()?;
    let user_info = UserInfo::current()?;

    match cli.command {
        Commands::Run { name, command } => {
            run_sandbox(&repo_root, &name, &user_info, command)?;
        }
        Commands::Attach { name } => {
            attach_sandbox(&repo_root, &name, &user_info)?;
        }
        Commands::List => {
            list_sandboxes(&repo_root)?;
        }
        Commands::Delete { name } => {
            delete_sandbox(&repo_root, &name)?;
        }
        Commands::Cleanup => {
            cleanup(&repo_root)?;
        }
        Commands::Sync { name } => {
            run_sync(&repo_root, &name)?;
        }
    }

    Ok(())
}

fn run_sandbox(
    repo_root: &PathBuf,
    name: &str,
    user_info: &UserInfo,
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

    // Start git sync watcher in background
    let main_repo = repo_root.clone();
    let clone_dir = info.clone_dir.clone();
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    let sync_handle = std::thread::spawn(move || {
        if let Err(e) = sync::run_sync_loop(&main_repo, &clone_dir, &running_clone) {
            eprintln!("Git sync error: {}", e);
        }
    });

    // Run the sandbox
    let cmd = if command.is_empty() {
        None
    } else {
        Some(command.as_slice())
    };

    let result = sandbox::run_sandbox(&info, &image_tag, user_info, cmd);

    // Stop sync watcher
    running.store(false, Ordering::SeqCst);
    let _ = sync_handle.join();

    result
}

fn attach_sandbox(repo_root: &PathBuf, name: &str, user_info: &UserInfo) -> Result<()> {
    let info = SandboxInfo::new(name, repo_root)?;

    if !docker::container_is_running(&info.container_name)? {
        bail!(
            "Sandbox '{}' is not running. Use 'sandbox run {}' to start it.",
            name,
            name
        );
    }

    let shell = if user_info.uses_fish() {
        "fish"
    } else {
        "bash"
    };

    docker::attach_container(&info.container_name, shell)?;

    Ok(())
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

fn cleanup(repo_root: &PathBuf) -> Result<()> {
    sandbox::cleanup_orphaned_volumes(repo_root)?;
    println!("Cleanup complete.");
    Ok(())
}

fn run_sync(repo_root: &PathBuf, name: &str) -> Result<()> {
    let info = SandboxInfo::new(name, repo_root)?;

    if !info.clone_dir.exists() {
        bail!("Sandbox '{}' not found or not set up", name);
    }

    eprintln!(
        "Starting git sync watcher between {} and {}",
        repo_root.display(),
        info.clone_dir.display()
    );
    eprintln!("Press Ctrl+C to stop");

    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    // Handle Ctrl+C
    ctrlc::set_handler(move || {
        running_clone.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    sync::run_sync_loop(repo_root, &info.clone_dir, &running)?;

    Ok(())
}
