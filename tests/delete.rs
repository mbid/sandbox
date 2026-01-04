//! Integration tests for the `sandbox delete` subcommand.

mod common;

use std::fs;

use indoc::indoc;

use common::{run_git, SandboxFixture};

#[test]
fn test_delete_with_readonly_files_copy_mode() {
    let fixture = SandboxFixture::new("test-delete-copy");

    // Enter sandbox with copy overlay mode and create files with restrictive permissions
    let output = fixture.run_with_mode(
        "copy",
        &[
            "sh",
            "-c",
            "mkdir -p readonly_dir && \
             echo 'test' > readonly_dir/file.txt && \
             chmod 000 readonly_dir/file.txt && \
             chmod 000 readonly_dir",
        ],
    );
    assert!(
        output.status.success(),
        "Failed to create readonly files: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Try to delete the sandbox - this should succeed even with readonly files
    let output = fixture.run_sandbox(&["delete", &fixture.name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox with readonly files in copy mode: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_delete_with_readonly_files_overlayfs_mode() {
    let fixture = SandboxFixture::new("test-delete-overlayfs");

    // Enter sandbox with overlayfs mode and create files with restrictive permissions
    let output = fixture.run_with_mode(
        "overlayfs",
        &[
            "sh",
            "-c",
            "mkdir -p readonly_dir && \
             echo 'test' > readonly_dir/file.txt && \
             chmod 000 readonly_dir/file.txt && \
             chmod 000 readonly_dir",
        ],
    );
    assert!(
        output.status.success(),
        "Failed to create readonly files: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Try to delete the sandbox - this should succeed even with readonly files
    let output = fixture.run_sandbox(&["delete", &fixture.name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox with readonly files in overlayfs mode: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Test that deletion works when using overlay mode = copy with a repo-relative mount.
///
/// When using copy mode with a repo-relative mount (e.g., `target`), Docker creates
/// the mount target directory before applying mounts. Since the repo root is bind-mounted,
/// this can result in a root-owned directory being created in the host's clone directory.
/// The delete command should still succeed.
#[test]
fn test_delete_with_repo_relative_overlay_copy_mount() {
    let fixture = SandboxFixture::new("test-delete-repo-relative-overlay");

    // Add target to .gitignore (simulates typical Rust/build artifact directory)
    fs::write(fixture.repo.dir.join(".gitignore"), "/target\n")
        .expect("Failed to write .gitignore");

    // Write a .sandbox.toml that includes a repo-relative overlay mount
    fs::write(
        fixture.repo.dir.join(".sandbox.toml"),
        indoc! {r#"
            env = []
            overlay-mode = "copy"

            [[mounts.overlay]]
            host = "target"
        "#},
    )
    .expect("Failed to write .sandbox.toml");

    run_git(&fixture.repo.dir, &["add", ".sandbox.toml", ".gitignore"]);
    run_git(&fixture.repo.dir, &["commit", "--amend", "--no-edit"]);

    // Create target directory (after commit, so it's not in the clone)
    // This simulates a build artifact directory that exists on host but not in git
    fs::create_dir_all(fixture.repo.dir.join("target")).expect("Failed to create target directory");
    fs::write(fixture.repo.dir.join("target/test.txt"), "test content")
        .expect("Failed to write test.txt");

    // Enter sandbox - this triggers Docker to create the mount target directory
    let output = fixture.run(&["ls", "-la", "target"]);
    assert!(
        output.status.success(),
        "Failed to enter sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Try to delete the sandbox - this should succeed even if Docker created
    // a root-owned directory in the clone
    let output = fixture.run_sandbox(&["delete", &fixture.name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox with repo-relative overlay mount in copy mode: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
