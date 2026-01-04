//! Shared test utilities and fixtures.
//!
//! This module provides reusable test infrastructure for integration tests.

// Not all test files use all helpers, but we want them available.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use indoc::indoc;
use rand::Rng;

/// Default .sandbox.toml config for tests (no required env vars).
const DEFAULT_SANDBOX_CONFIG: &str = indoc! {r#"
    env = []
"#};

/// Environment variable used to configure the daemon socket path.
const SOCKET_PATH_ENV: &str = "SANDBOX_DAEMON_SOCKET";

/// A test daemon that manages sandboxes for integration tests.
/// Each test gets its own daemon with an isolated socket to enable parallel execution.
/// On drop, the daemon process is terminated.
pub struct TestDaemon {
    pub socket_path: PathBuf,
    process: Child,
    #[allow(dead_code)]
    temp_dir: tempfile::TempDir,
}

impl TestDaemon {
    /// Start a new test daemon with an isolated socket.
    pub fn start() -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp directory");
        let socket_path = temp_dir.path().join("sandbox.sock");

        let process = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
            .env(SOCKET_PATH_ENV, &socket_path)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to spawn daemon");

        // Wait for socket to exist
        let timeout = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        while !socket_path.exists() {
            if start.elapsed() > timeout {
                panic!(
                    "Daemon socket did not appear within {:?}: {}",
                    timeout,
                    socket_path.display()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        TestDaemon {
            socket_path,
            process,
            temp_dir,
        }
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Kill the daemon process
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

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

        // Create .sandbox config file (required for sandbox to work)
        fs::write(dir.join(".sandbox.toml"), DEFAULT_SANDBOX_CONFIG)
            .expect("Failed to write .sandbox config");

        // Make initial commit
        run_git(&dir, &["add", "README.md", ".sandbox.toml"]);
        run_git(&dir, &["commit", "-m", "Initial commit"]);

        // Get the initial commit hash
        let output = run_git(&dir, &["rev-parse", "HEAD"]);
        let initial_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();

        TestRepo {
            dir,
            initial_commit,
        }
    }

    /// Add the standard Dockerfile for tests and commit it.
    pub fn add_dockerfile(&self) {
        fs::write(
            self.dir.join("Dockerfile"),
            include_str!("../Dockerfile-debian"),
        )
        .expect("Failed to write Dockerfile");

        run_git(&self.dir, &["add", "Dockerfile"]);
        run_git(&self.dir, &["commit", "-m", "Add Dockerfile"]);
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        // Clean up temp directory
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn run_git(dir: &PathBuf, args: &[&str]) -> Output {
    let output = Command::new("git")
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

/// Run the sandbox binary with the given arguments in a specific working directory,
/// using the given socket path for daemon communication.
pub fn run_sandbox_in_with_socket(
    working_dir: &PathBuf,
    socket_path: &PathBuf,
    args: &[&str],
) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(working_dir)
        .env(SOCKET_PATH_ENV, socket_path)
        .args(args)
        .output()
        .expect("Failed to run sandbox command")
}

/// Run the sandbox binary with the given arguments in a specific working directory.
/// NOTE: This requires a daemon to already be running with the default socket path.
/// For test isolation, prefer using `run_sandbox_in_with_socket` with a TestDaemon.
pub fn run_sandbox_in(working_dir: &PathBuf, args: &[&str]) -> Output {
    Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(working_dir)
        .args(args)
        .output()
        .expect("Failed to run sandbox command")
}

/// Run a command inside the sandbox and capture its output.
pub fn run_in_sandbox(repo: &TestRepo, sandbox_name: &str, command: &[&str]) -> Output {
    let mut args = vec!["enter", sandbox_name, "--runtime", "runc", "--"];
    args.extend(command);
    run_sandbox_in(&repo.dir, &args)
}

/// Run a command inside the sandbox with a specific overlay mode.
pub fn run_in_sandbox_with_mode(
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

/// Delete a sandbox, asserting success.
pub fn delete_sandbox(repo: &TestRepo, sandbox_name: &str) {
    let output = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
    assert!(
        output.status.success(),
        "Failed to delete sandbox: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Delete a sandbox, ignoring any errors.
pub fn delete_sandbox_ignore_errors(repo: &TestRepo, sandbox_name: &str) {
    let _ = run_sandbox_in(&repo.dir, &["delete", sandbox_name]);
}

/// A test fixture that wraps TestRepo and tracks a sandbox for automatic cleanup.
/// Also manages its own daemon for test isolation.
///
/// On drop, deletes the sandbox (ignoring errors) before cleaning up the repo and daemon.
pub struct SandboxFixture {
    pub repo: TestRepo,
    pub name: String,
    pub daemon: TestDaemon,
}

impl SandboxFixture {
    /// Create a new sandbox fixture with a Dockerfile already committed.
    /// Also starts a daemon for this fixture.
    pub fn new(sandbox_name: &str) -> Self {
        let repo = TestRepo::init();
        repo.add_dockerfile();
        let daemon = TestDaemon::start();
        SandboxFixture {
            repo,
            name: sandbox_name.to_string(),
            daemon,
        }
    }

    /// Run a command inside this sandbox.
    pub fn run(&self, command: &[&str]) -> Output {
        self.run_in_sandbox(command)
    }

    /// Run a command inside this sandbox with a specific overlay mode.
    pub fn run_with_mode(&self, overlay_mode: &str, command: &[&str]) -> Output {
        self.run_in_sandbox_with_mode(overlay_mode, command)
    }

    /// Run the sandbox binary with the given arguments.
    pub fn run_sandbox(&self, args: &[&str]) -> Output {
        run_sandbox_in_with_socket(&self.repo.dir, &self.daemon.socket_path, args)
    }

    fn run_in_sandbox(&self, command: &[&str]) -> Output {
        let mut args = vec!["enter", &self.name, "--runtime", "runc", "--"];
        args.extend(command);
        self.run_sandbox(&args)
    }

    fn run_in_sandbox_with_mode(&self, overlay_mode: &str, command: &[&str]) -> Output {
        let mut args = vec![
            "enter",
            &self.name,
            "--runtime",
            "runc",
            "--overlay-mode",
            overlay_mode,
            "--",
        ];
        args.extend(command);
        self.run_sandbox(&args)
    }

    /// Delete this sandbox, asserting success.
    pub fn delete(&self) {
        let output = self.run_sandbox(&["delete", &self.name]);
        assert!(
            output.status.success(),
            "Failed to delete sandbox: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for SandboxFixture {
    fn drop(&mut self) {
        // Try to delete the sandbox, ignore errors
        let _ = run_sandbox_in_with_socket(
            &self.repo.dir,
            &self.daemon.socket_path,
            &["delete", &self.name],
        );
        // daemon is dropped automatically after this, killing the process
    }
}

/// Configuration for spawning an agent process.
pub struct AgentBuilder<'a> {
    fixture: &'a SandboxFixture,
    env_vars: Vec<(&'a str, &'a str)>,
}

impl<'a> AgentBuilder<'a> {
    pub fn new(fixture: &'a SandboxFixture) -> Self {
        Self {
            fixture,
            env_vars: Vec::new(),
        }
    }

    /// Add an environment variable to the agent process.
    pub fn env(mut self, key: &'a str, value: &'a str) -> Self {
        self.env_vars.push((key, value));
        self
    }

    /// Spawn the agent process with the given prompt.
    pub fn run_with_prompt(self, prompt: &str) -> Output {
        let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"));
        cmd.current_dir(&self.fixture.repo.dir);
        cmd.env(SOCKET_PATH_ENV, &self.fixture.daemon.socket_path);
        cmd.args([
            "agent",
            &self.fixture.name,
            "--runtime",
            "runc",
            "--model",
            "haiku",
            "--cache",
            cache_dir.to_str().unwrap(),
        ]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().expect("Failed to spawn agent");

        let stdin = child.stdin.as_mut().expect("Failed to open stdin");
        writeln!(stdin, "{}", prompt).expect("Failed to write to stdin");
        drop(child.stdin.take());

        child.wait_with_output().expect("Failed to wait for agent")
    }
}

/// Poll a condition until it returns true or timeout is reached.
/// Returns true if condition was met, false if timed out.
pub fn wait_for<F>(timeout: Duration, poll_interval: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(poll_interval);
    }
    false
}
