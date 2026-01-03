//! Integration tests for the `sandbox agent` subcommand.

mod common;

use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

use indoc::{formatdoc, indoc};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use common::{run_git, run_sandbox_in, AgentBuilder, SandboxFixture, TestRepo};

#[test]
fn test_agent_passthrough_env() {
    let repo = TestRepo::init();
    repo.add_dockerfile();

    // Update .sandbox to require a missing env var
    fs::write(
        repo.dir.join(".sandbox.toml"),
        indoc! {r#"
            env = ["MISSING_API_KEY_XYZ"]
        "#},
    )
    .expect("Failed to write .sandbox.toml");

    run_git(&repo.dir, &["add", ".sandbox.toml"]);
    run_git(&repo.dir, &["commit", "--amend", "--no-edit"]);

    let sandbox_name = "test-agent-env";

    // Test: Verify error when env var is not set for agent command
    let output = Command::new(assert_cmd::cargo::cargo_bin!("sandbox"))
        .current_dir(&repo.dir)
        .args(["agent", sandbox_name, "--runtime", "runc"])
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
    let fixture = SandboxFixture::new("test-agent");

    let secret_content = "SECRET_VALUE_12345";
    fs::write(fixture.repo.dir.join("secret.txt"), secret_content)
        .expect("Failed to write secret.txt");

    run_git(&fixture.repo.dir, &["add", "secret.txt"]);
    run_git(&fixture.repo.dir, &["commit", "--amend", "--no-edit"]);

    let output = AgentBuilder::new(&fixture.repo, &fixture.name)
        .run_with_prompt("Run `cat secret.txt` and tell me what it contains.");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(secret_content),
        "Agent output should contain the secret content.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_edits_file() {
    let fixture = SandboxFixture::new("test-agent-edit");

    let original_content = "Hello World";
    fs::write(fixture.repo.dir.join("greeting.txt"), original_content)
        .expect("Failed to write greeting.txt");

    run_git(&fixture.repo.dir, &["add", "greeting.txt"]);
    run_git(&fixture.repo.dir, &["commit", "--amend", "--no-edit"]);

    let output = AgentBuilder::new(&fixture.repo, &fixture.name)
        .run_with_prompt("Run `sed -i 's/World/Universe/' greeting.txt` then run `cat greeting.txt` and tell me the result.");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Universe"),
        "Agent output should contain the edited content 'Universe'.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_writes_file() {
    let fixture = SandboxFixture::new("test-agent-write");
    let expected_content = "WRITTEN_BY_AGENT_12345";

    let output = AgentBuilder::new(&fixture.repo, &fixture.name).run_with_prompt(&format!(
        "Run `echo '{}' > newfile.txt` then run `cat newfile.txt` and tell me the result.",
        expected_content
    ));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(expected_content),
        "Agent output should contain the written content.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_handles_command_with_empty_output_and_nonzero_exit() {
    // Regression test: The Anthropic API rejects tool_result blocks with empty
    // content when is_error is true. Commands like `false` or `exit 1` produce
    // no output but exit with non-zero status.
    let fixture = SandboxFixture::new("test-agent-empty-error");

    let output = AgentBuilder::new(&fixture.repo, &fixture.name)
        .run_with_prompt("Run the command `false` and tell me what happened.");

    let stderr = String::from_utf8_lossy(&output.stderr);
    // The agent should NOT fail with the API error about empty content
    assert!(
        !stderr.contains("content cannot be empty"),
        "Agent failed with empty content error.\nstderr: {}",
        stderr
    );
}

#[test]
fn test_agent_large_file_output() {
    // Regression test: reading a file that exceeds the 30000 character limit
    // should not cause the agent to deadlock.
    let fixture = SandboxFixture::new("test-agent-large-file");

    let large_content = "x".repeat(35000);
    fs::write(fixture.repo.dir.join("large.txt"), &large_content)
        .expect("Failed to write large.txt");

    run_git(&fixture.repo.dir, &["add", "large.txt"]);
    run_git(&fixture.repo.dir, &["commit", "--amend", "--no-edit"]);

    let output = AgentBuilder::new(&fixture.repo, &fixture.name)
        .run_with_prompt("Run exactly one tool: `cat large.txt`. After that single tool call, stop immediately and tell me what you observed. Do not run any other tools.");

    // Agent should complete without deadlocking and mention the output file
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/agent/bash-output-"),
        "Agent should report that output was saved to a file.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_vim_input() {
    // Test the vim-based input mode by mocking vim with a script that:
    // 1. First invocation: appends a prompt to the file
    // 2. Second invocation: exits without changes, triggering "Exit? [Y/n]" prompt
    // We then send "Y\n" via PTY to confirm exit.
    //
    // A PTY is required because the agent only uses vim input mode when stdin is a TTY.
    let fixture = SandboxFixture::new("test-agent-vim");

    let secret_content = "VIM_TEST_SECRET_98765";
    fs::write(fixture.repo.dir.join("secret.txt"), secret_content)
        .expect("Failed to write secret.txt");

    run_git(&fixture.repo.dir, &["add", "secret.txt"]);
    run_git(&fixture.repo.dir, &["commit", "--amend", "--no-edit"]);

    // Create mock vim script
    let mock_bin_dir = fixture.repo.dir.join("mock-bin");
    fs::create_dir_all(&mock_bin_dir).expect("Failed to create mock-bin dir");

    let marker_file = fixture.repo.dir.join("vim-marker");

    // Mock vim: first call appends prompt, second call exits immediately (no changes)
    let mock_vim_script = formatdoc! {r#"
        #!/bin/bash
        FILE="$1"
        MARKER="{marker}"

        if [ -f "$MARKER" ]; then
            # Second invocation: exit without modifying the file
            exit 0
        fi

        # First invocation: append the test message and create marker
        echo "" >> "$FILE"
        echo "Run \`cat secret.txt\` and tell me what it contains." >> "$FILE"
        touch "$MARKER"
    "#, marker = marker_file.display()};

    let mock_vim_path = mock_bin_dir.join("vim");
    fs::write(&mock_vim_path, mock_vim_script).expect("Failed to write mock vim");
    Command::new("chmod")
        .args(["+x", mock_vim_path.to_str().unwrap()])
        .output()
        .expect("Failed to chmod mock vim");

    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", mock_bin_dir.display(), current_path);

    // Create PTY - required for agent to use vim input mode
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("Failed to open PTY");

    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("llm-cache");
    let sandbox_bin = assert_cmd::cargo::cargo_bin!("sandbox");
    let mut cmd = CommandBuilder::new(&sandbox_bin);
    cmd.cwd(&fixture.repo.dir);
    cmd.env("PATH", &new_path);
    cmd.args([
        "agent",
        &fixture.name,
        "--runtime",
        "runc",
        "--model",
        "haiku",
        "--cache",
        cache_dir.to_str().unwrap(),
    ]);

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("Failed to spawn agent in PTY");
    drop(pair.slave);

    let mut writer = pair.master.take_writer().expect("Failed to get PTY writer");
    let mut reader = std::io::BufReader::new(
        pair.master
            .try_clone_reader()
            .expect("Failed to get PTY reader"),
    );

    // Read lines until we see the exit prompt
    let mut all_output = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("Failed to read line");
        all_output.push_str(&line);
        if line.contains("Exit? [Y/n]") {
            break;
        }
    }

    writer.write_all(b"Y\n").expect("Failed to write to PTY");
    child.wait().expect("Agent should exit successfully");

    assert!(
        all_output.contains(secret_content),
        "Agent output should contain the secret content when using vim input.\noutput: {}",
        all_output
    );

    assert!(
        all_output.contains("> Run `cat secret.txt` and tell me what it contains."),
        "Agent output should show the user message from vim.\noutput: {}",
        all_output
    );
}

#[test]
fn test_agent_websearch() {
    // Test that the agent can use web search to find information beyond its knowledge cutoff.
    // The US penny production ended in November 2025, after the model's training data.
    let fixture = SandboxFixture::new("test-agent-websearch");

    let output = AgentBuilder::new(&fixture.repo, &fixture.name).run_with_prompt(
        "When was the last US penny minted? Answer with just the date in yyyy-mm-dd format.",
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("2025-11-12"),
        "Agent should find the last US penny minting date via web search.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_agent_write_tool_output_format() {
    // Test that the write tool prints "[write] <filename>" on success
    // without additional success messages or content echoing.
    let fixture = SandboxFixture::new("test-agent-write-format");

    let output = AgentBuilder::new(&fixture.repo, &fixture.name)
        .run_with_prompt("Use the write tool to create a file called 'test.txt' with content 'hello'. Do not use bash.");

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
}

#[test]
fn test_agent_websearch_output_format() {
    // Test that web searches print "[search] <query>" in output.
    let fixture = SandboxFixture::new("test-agent-search-format");

    let output = AgentBuilder::new(&fixture.repo, &fixture.name).run_with_prompt(
        "When was the last US penny minted? Answer with just the date in yyyy-mm-dd format.",
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should see "[search]" prefix for web search
    assert!(
        stdout.contains("[search]"),
        "Expected '[search]' in output for web search.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
}
