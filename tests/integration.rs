use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use rand::Rng;

/// A test fixture that creates a temporary git repository in /tmp.
/// The repository is initialized with a README.md file and an initial commit.
/// Does NOT change the current directory, allowing tests to run in parallel.
/// On drop, the temp directory is cleaned up.
pub struct TestRepo {
    pub dir: PathBuf,
    pub initial_commit: String,
}

impl TestRepo {
    /// Initialize a new test repository.
    ///
    /// Creates a temporary directory in /tmp, initializes a git repo with "master"
    /// as the initial branch, creates a README.md with "TEST" content, and makes
    /// an initial commit. Does NOT change the current directory.
    pub fn init() -> Self {
        // Random component ensures uniqueness even when parallel tests read the same timestamp
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let random: u64 = rand::rng().random();
        let dir = PathBuf::from(format!("/tmp/sandbox-test-{}-{:016x}", timestamp, random));
        fs::create_dir_all(&dir).expect("Failed to create temp directory");

        // Initialize git repo with master branch
        run_git(&dir, &["init", "--initial-branch=master"]);

        // Configure git user for commits
        run_git(&dir, &["config", "user.email", "test@example.com"]);
        run_git(&dir, &["config", "user.name", "Test User"]);

        // Create README.md
        fs::write(dir.join("README.md"), "TEST").expect("Failed to write README.md");

        // Make initial commit
        run_git(&dir, &["add", "README.md"]);
        run_git(&dir, &["commit", "-m", "Initial commit"]);

        // Get the initial commit hash
        let output = run_git(&dir, &["rev-parse", "HEAD"]);
        let initial_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

        TestRepo {
            dir,
            initial_commit,
        }
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // Clean up temp directory
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn run_git(dir: &PathBuf, args: &[&str]) -> Output {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("Failed to run git command");

    if !output.status.success() {
        panic!(
            "Git command failed: git {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

/// Run the sandbox binary with the given arguments in a specific working directory.
fn run_sandbox_in(working_dir: &PathBuf, args: &[&str]) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(working_dir)
        .args(args)
        .output()
        .expect("Failed to run sandbox command")
}

/// Run a command inside the sandbox and capture its output.
fn run_in_sandbox(repo: &TestRepo, sandbox_name: &str, command: &[&str]) -> Output {
    let mut args = vec!["enter", sandbox_name, "--runtime", "runc", "--"];
    args.extend(command);
    run_sandbox_in(&repo.dir, &args)
}

/// Run a command inside the sandbox with a specific overlay mode.
fn run_in_sandbox_with_mode(
    repo: &TestRepo,
    sandbox_name: &str,
    overlay_mode: &str,
    command: &[&str],
) -> Output {
    let mut args = vec![
        "enter",
        sandbox_name,
        "--runtime",
        "runc",
        "--overlay-mode",
        overlay_mode,
        "--",
    ];
    args.extend(command);
    run_sandbox_in(&repo.dir, &args)
}

#[test]
fn smoke_test_sandbox_enter() {
    let repo = TestRepo::init();

    // Copy the minimal Dockerfile for the sandbox
    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    // Commit the Dockerfile so the sandbox branch can be created from a clean state
    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    // Get the new commit hash (after adding Dockerfile)
    let output = run_git(&repo.dir, &["rev-parse", "HEAD"]);
    let expected_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

    let sandbox_name = "test-sandbox";

    // Test 1: Verify README.md content inside sandbox
    let output = run_in_sandbox(&repo, sandbox_name, &["cat", "README.md"]);
    assert!(
        output.status.success(),
        "Failed to read README.md in sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let readme_content = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        readme_content.trim(),
        "TEST",
        "README.md content mismatch. Got: '{}'",
        readme_content.trim()
    );

    // Test 2: Verify we're on the correct branch (sandbox name)
    let output = run_in_sandbox(&repo, sandbox_name, &["git", "branch", "--show-current"]);
    assert!(
        output.status.success(),
        "Failed to get current branch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let current_branch = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        current_branch.trim(),
        sandbox_name,
        "Branch mismatch. Expected '{}', got '{}'",
        sandbox_name,
        current_branch.trim()
    );

    // Test 3: Verify we're on the correct commit
    let output = run_in_sandbox(&repo, sandbox_name, &["git", "rev-parse", "HEAD"]);
    assert!(
        output.status.success(),
        "Failed to get current commit: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let current_commit = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        current_commit.trim(),
        expected_commit,
        "Commit mismatch. Expected '{}', got '{}'",
        expected_commit,
        current_commit.trim()
    );

    // Clean up: delete the sandbox
    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_delete_with_readonly_files_copy_mode() {
    let repo = TestRepo::init();

    // Copy the minimal Dockerfile for the sandbox
    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    // Commit the Dockerfile
    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-delete-copy";

    // Enter sandbox with copy overlay mode and create files with restrictive permissions
    let output = run_in_sandbox_with_mode(
        &repo,
        sandbox_name,
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

    // Exit the sandbox (it should stop automatically)
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Try to delete the sandbox - this should succeed even with readonly files
    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox with readonly files in copy mode: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_delete_with_readonly_files_overlayfs_mode() {
    let repo = TestRepo::init();

    // Copy the minimal Dockerfile for the sandbox
    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    // Commit the Dockerfile
    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-delete-overlayfs";

    // Enter sandbox with overlayfs mode and create files with restrictive permissions
    let output = run_in_sandbox_with_mode(
        &repo,
        sandbox_name,
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

    // Exit the sandbox (it should stop automatically)
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Try to delete the sandbox - this should succeed even with readonly files
    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox with readonly files in overlayfs mode: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_sync_with_history_rewrite() {
    let repo = TestRepo::init();

    // Copy the minimal Dockerfile for the sandbox
    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    // Commit the Dockerfile
    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-history-rewrite";

    // Configure git user inside the sandbox
    let output = run_in_sandbox(
        &repo,
        sandbox_name,
        &[
            "sh",
            "-c",
            "git config user.email 'test@example.com' && git config user.name 'Test User'",
        ],
    );
    assert!(
        output.status.success(),
        "Failed to configure git: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Make a commit inside the sandbox
    let output = run_in_sandbox(
        &repo,
        sandbox_name,
        &[
            "sh",
            "-c",
            "echo 'first version' > file.txt && git add file.txt && git commit -m 'First commit'",
        ],
    );
    assert!(
        output.status.success(),
        "Failed to create first commit: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Get the commit hash
    let output = run_in_sandbox(&repo, sandbox_name, &["git", "rev-parse", "HEAD"]);
    assert!(output.status.success());
    let first_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Wait for sync to propagate to host
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Verify the commit is synced to host's remote tracking ref
    let output = run_git(
        &repo.dir,
        &[
            "rev-parse",
            &format!("refs/remotes/sandbox/{}", sandbox_name),
        ],
    );
    let host_remote_ref = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        first_commit, host_remote_ref,
        "First commit should be synced to host"
    );

    // Now amend the commit (rewrite history)
    let output = run_in_sandbox(
        &repo,
        sandbox_name,
        &["sh", "-c", "echo 'amended version' > file.txt && git add file.txt && git commit --amend -m 'Amended commit'"],
    );
    assert!(
        output.status.success(),
        "Failed to amend commit: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Get the new commit hash
    let output = run_in_sandbox(&repo, sandbox_name, &["git", "rev-parse", "HEAD"]);
    assert!(output.status.success());
    let amended_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // The commit hash should be different after amend
    assert_ne!(
        first_commit, amended_commit,
        "Amended commit should have different hash"
    );

    // Wait for sync to propagate
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Verify the amended commit is synced to host's remote tracking ref
    let output = run_git(
        &repo.dir,
        &[
            "rev-parse",
            &format!("refs/remotes/sandbox/{}", sandbox_name),
        ],
    );
    let host_remote_ref_after = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        amended_commit, host_remote_ref_after,
        "Amended commit should be synced to host. Expected {}, got {}",
        amended_commit, host_remote_ref_after
    );

    // Clean up
    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_passthrough_env() {
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-agent-env";

    // Test: Verify error when env var is not set for agent command
    let output = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--env",
            "MISSING_API_KEY_XYZ",
        ])
        .output()
        .expect("Failed to run sandbox");

    assert!(
        !output.status.success(),
        "Agent should fail when env var is not set"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("MISSING_API_KEY_XYZ"),
        "Error should mention the missing env var. Got: '{}'",
        stderr
    );

    // Clean up
    let _ = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
}

#[test]
fn test_agent_reads_file() {
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    let secret_content = "SECRET_VALUE_12345";
    fs::write(repo.dir.join("secret.txt"), secret_content).expect("Failed to write secret.txt");

    run_git(&repo.dir, &["add", "Dockerfile", "secret.txt"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile and secret"]);

    let sandbox_name = "test-agent";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(stdin, "Run `cat secret.txt` and tell me what it contains.")
        .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(secret_content),
        "Agent output should contain the secret content.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_edits_file() {
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    let original_content = "Hello World";
    fs::write(repo.dir.join("greeting.txt"), original_content)
        .expect("Failed to write greeting.txt");

    run_git(&repo.dir, &["add", "Dockerfile", "greeting.txt"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile and greeting"]);

    let sandbox_name = "test-agent-edit";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(
        stdin,
        "Run `sed -i 's/World/Universe/' greeting.txt` then run `cat greeting.txt` and tell me the result."
    )
    .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Universe"),
        "Agent output should contain the edited content 'Universe'.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_writes_file() {
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-agent-write";
    let expected_content = "WRITTEN_BY_AGENT_12345";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(
        stdin,
        "Run `echo '{}' > newfile.txt` then run `cat newfile.txt` and tell me the result.",
        expected_content
    )
    .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(expected_content),
        "Agent output should contain the written content.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_enter_passthrough_env() {
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-env-passthrough";
    let env_value = "SECRET_ENV_VALUE_42";

    // Test 1: Verify env var is passed through when set on host
    let output = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .env("MY_TEST_VAR", env_value)
        .args([
            "enter",
            sandbox_name,
            "--runtime",
            "runc",
            "--env",
            "MY_TEST_VAR",
            "--",
            "sh",
            "-c",
            "echo $MY_TEST_VAR",
        ])
        .output()
        .expect("Failed to run sandbox");

    assert!(
        output.status.success(),
        "Failed to run command with --env flag: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        env_value,
        "Env var not passed through. Got: '{}'",
        stdout.trim()
    );

    // Test 2: Verify error when env var is not set on host
    let output = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "enter",
            sandbox_name,
            "--runtime",
            "runc",
            "--env",
            "NONEXISTENT_VAR_XYZ",
            "--",
            "echo",
            "should not reach here",
        ])
        .output()
        .expect("Failed to run sandbox");

    assert!(
        !output.status.success(),
        "Should fail when env var is not set: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("NONEXISTENT_VAR_XYZ"),
        "Error message should mention the missing env var. Got: '{}'",
        stderr
    );

    // Test 3: Verify multiple env vars can be passed
    let output = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .env("VAR_ONE", "value1")
        .env("VAR_TWO", "value2")
        .args([
            "enter",
            sandbox_name,
            "--runtime",
            "runc",
            "--env",
            "VAR_ONE",
            "--env",
            "VAR_TWO",
            "--",
            "sh",
            "-c",
            "echo $VAR_ONE-$VAR_TWO",
        ])
        .output()
        .expect("Failed to run sandbox");

    assert!(
        output.status.success(),
        "Failed to run with multiple --env flags: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "value1-value2",
        "Multiple env vars not passed correctly. Got: '{}'",
        stdout.trim()
    );

    // Clean up
    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_handles_command_with_empty_output_and_nonzero_exit() {
    // Regression test: The Anthropic API rejects tool_result blocks with empty
    // content when is_error is true. Commands like `false` or `exit 1` produce
    // no output but exit with non-zero status.
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-agent-empty-error";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    // Ask the agent to run `false` which exits with status 1 and no output
    writeln!(stdin, "Run the command `false` and tell me what happened.")
        .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The agent should NOT fail with the API error about empty content
    assert!(
        !stderr.contains("content cannot be empty"),
        "Agent failed with empty content error.\nstderr: {}",
        stderr
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_large_file_output() {
    // Regression test: reading a file that exceeds the 30000 character limit
    // should not cause the agent to deadlock.
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    let large_content = "x".repeat(35000);
    fs::write(repo.dir.join("large.txt"), &large_content).expect("Failed to write large.txt");

    run_git(&repo.dir, &["add", "Dockerfile", "large.txt"]);
    run_git(
        &repo.dir,
        &["commit", "-m", "Add Dockerfile and large file"],
    );

    let sandbox_name = "test-agent-large-file";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(
        stdin,
        "Run exactly one tool: `cat large.txt`. After that single tool call, stop immediately and tell me what you observed. Do not run any other tools."
    )
    .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");

    // Agent should complete without deadlocking and mention the output file
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/agent/bash-output-"),
        "Agent should report that output was saved to a file.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_vim_input() {
    // Test the vim-based input mode using a PTY to simulate a real terminal
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    let secret_content = "VIM_TEST_SECRET_98765";
    fs::write(repo.dir.join("secret.txt"), secret_content).expect("Failed to write secret.txt");

    run_git(&repo.dir, &["add", "Dockerfile", "secret.txt"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile and secret"]);

    // Create a directory for the mock vim script
    let mock_bin_dir = repo.dir.join("mock-bin");
    fs::create_dir_all(&mock_bin_dir).expect("Failed to create mock-bin dir");

    // Create a unique marker file path for this test
    let marker_file = repo.dir.join("vim-marker");

    // Create a mock vim script that:
    // 1. On first invocation: appends a test message and creates a marker
    // 2. On subsequent invocations: sleeps forever (will be killed by test timeout)
    let mock_vim_script = format!(
        r#"#!/bin/bash
FILE="$1"
MARKER="{}"

if [ -f "$MARKER" ]; then
    # Second invocation: sleep forever, test will kill us
    sleep 3600
    exit 0
fi

# First invocation: append the test message
echo "" >> "$FILE"
echo "Run \`cat secret.txt\` and tell me what it contains." >> "$FILE"

# Create marker for next invocation
touch "$MARKER"
"#,
        marker_file.display()
    );

    let mock_vim_path = mock_bin_dir.join("vim");
    fs::write(&mock_vim_path, mock_vim_script).expect("Failed to write mock vim");

    // Make mock vim executable
    Command::new("chmod")
        .args(["+x", mock_vim_path.to_str().unwrap()])
        .output()
        .expect("Failed to chmod mock vim");

    let sandbox_name = "test-agent-vim";

    // Get current PATH and prepend mock bin dir
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", mock_bin_dir.display(), current_path);

    // Create a PTY to simulate a real terminal
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("Failed to open PTY");

    // Build command to spawn via PTY
    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let sandbox_bin = assert_cmd::cargo::cargo_bin!("sandbox");
    let mut cmd = CommandBuilder::new(&sandbox_bin);
    cmd.cwd(&repo.dir);
    cmd.env("PATH", &new_path);
    cmd.args([
        "agent",
        sandbox_name,
        "--runtime",
        "runc",
        "--model",
        "haiku",
        "--cache",
        cache_dir.to_str().unwrap(),
    ]);

    // Spawn the agent process in the PTY
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("Failed to spawn agent in PTY");

    // Drop the slave to avoid blocking on read
    drop(pair.slave);

    // Get a reader from the master side and spawn a thread to collect output
    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("Failed to get PTY reader");
    let output_data = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let output_data_clone = output_data.clone();

    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    output_data_clone
                        .lock()
                        .unwrap()
                        .extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the agent to process the message (poll collected output for expected content)
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(120);

    loop {
        // Check if we got the expected output
        let data = output_data.lock().unwrap();
        let output_str = String::from_utf8_lossy(&data);
        if output_str.contains(secret_content) {
            break;
        }

        // Check timeout
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("Timeout waiting for agent output.\noutput: {}", output_str);
        }
        drop(data);

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Kill the agent (it's waiting for more vim input)
    let _ = child.kill();
    let _ = child.wait();
    drop(pair.master); // Close the master to unblock the reader thread
    let _ = reader_thread.join();

    let final_data = output_data.lock().unwrap();
    let output = String::from_utf8_lossy(&final_data);
    assert!(
        output.contains(secret_content),
        "Agent output should contain the secret content when using vim input.\noutput: {}",
        output
    );

    // Verify the user message was recorded in output (shows vim input worked)
    assert!(
        output.contains("> Run `cat secret.txt` and tell me what it contains."),
        "Agent output should show the user message from vim.\noutput: {}",
        output
    );
    drop(final_data);

    let delete_output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        delete_output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&delete_output.stderr)
    );
}

#[test]
fn test_agent_websearch() {
    // Test that the agent can use web search to find information beyond its knowledge cutoff.
    // The US penny production ended in November 2025, after the model's training data.
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-agent-websearch";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(
        stdin,
        "When was the last US penny minted? Answer with just the date in yyyy-mm-dd format."
    )
    .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("2025-11-12"),
        "Agent should find the last US penny minting date via web search.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_write_tool_output_format() {
    // Test that the write tool prints "[write] <filename>" on success
    // without additional success messages or content echoing.
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-agent-write-format";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(stdin, "Use the write tool to create a file called 'test.txt' with content 'hello'. Do not use bash.")
        .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should see "[write] test.txt" in output
    assert!(
        stdout.contains("[write] test.txt"),
        "Expected '[write] test.txt' in output.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Should NOT see success messages like "successful" or "Successfully"
    let stdout_lower = stdout.to_lowercase();
    assert!(
        !stdout_lower.contains("successful"),
        "Should not contain 'successful' in output.\nstdout: {}",
        stdout
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_websearch_output_format() {
    // Test that web searches print "[search] <query>" in output.
    let repo = TestRepo::init();

    fs::write(
        repo.dir.join("Dockerfile"),
        include_str!("Dockerfile-debian"),
    )
    .expect("Failed to write Dockerfile");

    run_git(&repo.dir, &["add", "Dockerfile"]);
    run_git(&repo.dir, &["commit", "-m", "Add Dockerfile"]);

    let sandbox_name = "test-agent-search-format";

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args([
            "agent",
            sandbox_name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn agent");

    let stdin = child.stdin.as_mut().expect("Failed to open stdin");
    writeln!(
        stdin,
        "When was the last US penny minted? Answer with just the date in yyyy-mm-dd format."
    )
    .expect("Failed to write to stdin");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("Failed to wait for agent");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should see "[search]" prefix for web search
    assert!(
        stdout.contains("[search]"),
        "Expected '[search]' in output for web search.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
