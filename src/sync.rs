use anyhow::{Context, Result};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::git;

/// Run the git sync loop, watching for changes in both repos.
/// This function blocks until `running` is set to false.
pub fn run_sync_loop(
    main_repo: &Path,
    sandbox_repo: &Path,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();

    let tx_clone = tx.clone();
    let mut main_watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx_clone.send(("main", res));
        },
        Config::default().with_poll_interval(Duration::from_secs(1)),
    )
    .context("Failed to create main repo watcher")?;

    let tx_clone = tx.clone();
    let mut sandbox_watcher = RecommendedWatcher::new(
        move |res| {
            let _ = tx_clone.send(("sandbox", res));
        },
        Config::default().with_poll_interval(Duration::from_secs(1)),
    )
    .context("Failed to create sandbox repo watcher")?;

    // Watch the .git directories
    let main_git = main_repo.join(".git");
    let sandbox_git = sandbox_repo.join(".git");

    if main_git.exists() {
        main_watcher
            .watch(&main_git, RecursiveMode::Recursive)
            .with_context(|| format!("Failed to watch: {}", main_git.display()))?;
    }

    if sandbox_git.exists() {
        sandbox_watcher
            .watch(&sandbox_git, RecursiveMode::Recursive)
            .with_context(|| format!("Failed to watch: {}", sandbox_git.display()))?;
    }

    // Track last sync times to debounce
    let mut last_main_sync = std::time::Instant::now();
    let mut last_sandbox_sync = std::time::Instant::now();
    let debounce = Duration::from_millis(500);

    while running.load(Ordering::SeqCst) {
        // Check for events with a timeout
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((source, result)) => {
                if let Ok(event) = result {
                    let now = std::time::Instant::now();

                    // Filter out certain event kinds that don't need syncing
                    let dominated_by_access = event.kind.is_access();
                    if dominated_by_access {
                        continue;
                    }

                    match source {
                        "main" => {
                            if now.duration_since(last_main_sync) > debounce {
                                // Changes in main repo - update refs in sandbox
                                let _ = git::update_server_info(main_repo);
                                last_main_sync = now;
                            }
                        }
                        "sandbox" => {
                            if now.duration_since(last_sandbox_sync) > debounce {
                                // Changes in sandbox repo - update refs in main
                                let _ = git::update_server_info(sandbox_repo);
                                last_sandbox_sync = now;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No events, continue looping
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }

    Ok(())
}
