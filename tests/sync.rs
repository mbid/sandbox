//! Integration tests for git synchronization functionality.

mod common;

use std::fs;
use std::time::Duration;

use common::{run_git, run_sandbox_in_with_socket, wait_for, SandboxFixture, TestDaemon, TestRepo};

#[test]
fn test_sync_with_history_rewrite() {
    let fixture = SandboxFixture::new("test-history-rewrite");

    // Configure git user inside the sandbox
    let output = fixture.run(&[
        "sh",
        "-c",
        "git config user.email 'test@example.com' && git config user.name 'Test User'",
    ]);
    assert!(
        output.status.success(),
        "Failed to configure git: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Make a commit inside the sandbox
    let output = fixture.run(&[
        "sh",
        "-c",
        "echo 'first version' > file.txt && git add file.txt && git commit -m 'First commit'",
    ]);
    assert!(
        output.status.success(),
        "Failed to create first commit: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Get the commit hash
    let output = fixture.run(&["git", "rev-parse", "HEAD"]);
    assert!(output.status.success());
    let first_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Wait for sync to propagate to host (poll instead of fixed sleep)
    let ref_name = format!("refs/remotes/sandbox/{}", fixture.name);
    let synced = wait_for(Duration::from_secs(5), Duration::from_millis(100), || {
        let output = run_git(&fixture.repo.dir, &["rev-parse", &ref_name]);
        String::from_utf8_lossy(&output.stdout).trim() == first_commit
    });
    assert!(
        synced,
        "First commit should be synced to host within timeout"
    );

    // Now amend the commit (rewrite history)
    let output = fixture.run(&[
        "sh",
        "-c",
        "echo 'amended version' > file.txt && git add file.txt && git commit --amend -m 'Amended commit'",
    ]);
    assert!(
        output.status.success(),
        "Failed to amend commit: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Get the new commit hash
    let output = fixture.run(&["git", "rev-parse", "HEAD"]);
    assert!(output.status.success());
    let amended_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // The commit hash should be different after amend
    assert_ne!(
        first_commit, amended_commit,
        "Amended commit should have different hash"
    );

    // Wait for amended commit to sync to host (poll instead of fixed sleep)
    let ref_name = format!("refs/remotes/sandbox/{}", fixture.name);
    let synced = wait_for(Duration::from_secs(5), Duration::from_millis(100), || {
        let output = run_git(&fixture.repo.dir, &["rev-parse", &ref_name]);
        String::from_utf8_lossy(&output.stdout).trim() == amended_commit
    });
    assert!(
        synced,
        "Amended commit should be synced to host within timeout. Expected {}",
        amended_commit
    );
}

/// Test that host-side history rewrite (amend on master) syncs correctly to meta.git.
/// This verifies that sync_main_to_meta uses force-update.
#[test]
fn test_host_history_rewrite_syncs_to_sandbox() {
    let repo = TestRepo::init();
    repo.add_dockerfile();
    let daemon = TestDaemon::start();

    let sandbox_name = "test-host-rewrite";

    // Helper to run command in sandbox
    let run_in_sandbox = |name: &str, cmd: &[&str]| {
        let mut args = vec!["enter", name, "--runtime", "runc", "--"];
        args.extend(cmd);
        run_sandbox_in_with_socket(&repo.dir, &daemon.socket_path, &args)
    };

    // Create sandbox and verify initial state
    let output = run_in_sandbox(sandbox_name, &["git", "rev-parse", "sandbox/master"]);
    assert!(
        output.status.success(),
        "Failed to get sandbox/master: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let initial_master = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Amend the commit on host (rewrite history)
    fs::write(repo.dir.join("README.md"), "AMENDED CONTENT").expect("Failed to write README.md");
    run_git(&repo.dir, &["add", "README.md"]);
    run_git(&repo.dir, &["commit", "--amend", "-m", "Amended commit"]);

    let output = run_git(&repo.dir, &["rev-parse", "HEAD"]);
    let amended_master = String::from_utf8_lossy(&output.stdout).trim().to_string();

    assert_ne!(
        initial_master, amended_master,
        "Amended commit should have different hash"
    );

    // Create a new sandbox - this triggers sync_main_to_meta
    let sandbox_name_2 = "test-host-rewrite-2";
    let output = run_in_sandbox(sandbox_name_2, &["git", "rev-parse", "sandbox/master"]);
    assert!(
        output.status.success(),
        "Failed to create second sandbox after host history rewrite: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let synced_master = String::from_utf8_lossy(&output.stdout).trim().to_string();

    assert_eq!(
        amended_master, synced_master,
        "Amended master should be synced to new sandbox"
    );

    // Clean up
    let _ = run_sandbox_in_with_socket(&repo.dir, &daemon.socket_path, &["delete", sandbox_name]);
    let _ = run_sandbox_in_with_socket(&repo.dir, &daemon.socket_path, &["delete", sandbox_name_2]);
}
