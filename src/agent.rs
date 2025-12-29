use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::str::FromStr;
use strum::{Display, EnumString};

use crate::anthropic::{
    CacheControl, Client, ContentBlock, CustomTool, Message, MessagesRequest, Role, StopReason,
    SystemBlock, SystemPrompt, Tool,
};
use crate::config::Model;

const MAX_TOKENS: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, EnumString)]
#[strum(serialize_all = "lowercase")]
enum AgentToolName {
    Bash,
    Edit,
    Write,
}

fn bash_tool() -> Tool {
    Tool::Custom(CustomTool {
        name: AgentToolName::Bash.to_string(),
        description: "Execute a bash command inside the sandbox and return the output.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                }
            },
            "required": ["command"]
        }),
        cache_control: None,
    })
}

fn edit_tool() -> Tool {
    Tool::Custom(CustomTool {
        name: AgentToolName::Edit.to_string(),
        description: "Perform a search-and-replace edit on a file.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to modify (relative to repo root)"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace (must appear exactly once in the file)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text"
                }
            },
            "required": ["file_path", "old_string", "new_string"],
            "additionalProperties": false
        }),
        cache_control: None,
    })
}

fn write_tool() -> Tool {
    Tool::Custom(CustomTool {
        name: AgentToolName::Write.to_string(),
        description: "Write content to a new file. Returns an error if the file already exists."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The path to the file to create (relative to repo root)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"],
            "additionalProperties": false
        }),
        cache_control: None,
    })
}

fn execute_edit_in_sandbox(
    container_name: &str,
    file_path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<(String, bool)> {
    let output = Command::new("docker")
        .args(["exec", container_name, "cat", file_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to read file in sandbox")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok((format!("Error reading file: {}", stderr), false));
    }

    let content = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(_) => return Ok(("File contains invalid UTF-8".to_string(), false)),
    };

    let count = content.matches(old_string).count();

    if count == 0 {
        return Ok((format!("old_string not found in {}", file_path), false));
    }

    if count > 1 {
        return Ok((
            format!(
                "Found {} occurrences of old_string in {}. Provide more context to make the match unique.",
                count, file_path
            ),
            false,
        ));
    }

    let new_content = content.replacen(old_string, new_string, 1);

    let write_cmd = format!("cat > '{}'", file_path.replace('\'', "'\\''"));
    let mut write_process = Command::new("docker")
        .args(["exec", "-i", container_name, "bash", "-c", &write_cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to write file in sandbox")?;

    let mut stdin = write_process
        .stdin
        .take()
        .expect("Process was launched with piped stdin");
    stdin
        .write_all(new_content.as_bytes())
        .context("Failed to write to stdin")?;
    drop(stdin);

    let output = write_process
        .wait_with_output()
        .context("Failed to wait for write process")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok((format!("Error writing file: {}", stderr), false));
    }

    Ok((format!("Successfully edited {}", file_path), true))
}

fn execute_write_in_sandbox(
    container_name: &str,
    file_path: &str,
    content: &str,
) -> Result<(String, bool)> {
    let output = Command::new("docker")
        .args(["exec", container_name, "test", "-e", file_path])
        .output()
        .context("Failed to check if file exists")?;

    if output.status.success() {
        return Ok((format!("File {} already exists", file_path), false));
    }

    if let Some(parent) = std::path::Path::new(file_path).parent() {
        if !parent.as_os_str().is_empty() {
            let mkdir_cmd = format!(
                "mkdir -p '{}'",
                parent.display().to_string().replace('\'', "'\\''")
            );
            let _ = Command::new("docker")
                .args(["exec", container_name, "bash", "-c", &mkdir_cmd])
                .output();
        }
    }

    let write_cmd = format!("cat > '{}'", file_path.replace('\'', "'\\''"));
    let mut write_process = Command::new("docker")
        .args(["exec", "-i", container_name, "bash", "-c", &write_cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to write file in sandbox")?;

    let mut stdin = write_process
        .stdin
        .take()
        .expect("Process was launched with piped stdin");
    stdin
        .write_all(content.as_bytes())
        .context("Failed to write to stdin")?;
    drop(stdin);

    let output = write_process
        .wait_with_output()
        .context("Failed to wait for write process")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Ok((format!("Error writing file: {}", stderr), false));
    }

    Ok((format!("Successfully wrote {}", file_path), true))
}

fn save_output_to_file(container_name: &str, data: &[u8]) -> Result<String> {
    // Generate a short random ID for the output file
    let id = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 0xffffff
    );

    let output_file = format!("/agent/bash-output-{}", id);

    // Create /agent directory if it doesn't exist
    Command::new("docker")
        .args(["exec", container_name, "bash", "-c", "mkdir -p /agent"])
        .output()
        .context("Failed to create /agent directory")?;

    // Write the output to file
    let write_cmd = format!("cat > {}", output_file);
    let mut write_process = Command::new("docker")
        .args(["exec", "-i", container_name, "bash", "-c", &write_cmd])
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to write output to file")?;

    let mut stdin = write_process
        .stdin
        .take()
        .expect("Process was launmched with piped stdin");
    stdin.write_all(data).context("Failed to write to stdin")?;

    write_process
        .wait()
        .context("Failed to wait for write process")?;

    Ok(output_file)
}

fn execute_bash_in_sandbox(container_name: &str, command: &str) -> Result<(String, bool)> {
    const MAX_OUTPUT_SIZE: usize = 30000;

    let output = Command::new("docker")
        .args(["exec", container_name, "bash", "-c", command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to execute command in sandbox")?;

    // Combine stdout and stderr as raw bytes
    let combined_bytes = if output.stderr.is_empty() {
        output.stdout.clone()
    } else if output.stdout.is_empty() {
        output.stderr.clone()
    } else {
        let mut combined = output.stdout.clone();
        combined.push(b'\n');
        combined.extend_from_slice(&output.stderr);
        combined
    };

    // Check if output exceeds limit - save to file if so
    if combined_bytes.len() > MAX_OUTPUT_SIZE {
        let output_file = save_output_to_file(container_name, &combined_bytes)?;
        let error_msg = format!("Full output available at {}", output_file);
        return Ok((error_msg, false));
    }

    // Validate UTF-8 - save to file if invalid
    let combined = match String::from_utf8(combined_bytes.clone()) {
        Ok(s) => s,
        Err(_) => {
            let output_file = save_output_to_file(container_name, &combined_bytes)?;
            let error_msg = format!(
                "Output is not valid UTF-8. Full output available at {}",
                output_file
            );
            return Ok((error_msg, false));
        }
    };

    let success = output.status.success();

    // If command failed with no output, report the exit status
    if !success && combined.is_empty() {
        let exit_code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        return Ok((format!("exited with status {}", exit_code), false));
    }

    Ok((combined, success))
}

pub fn run_agent(container_name: &str, model: Model) -> Result<()> {
    let client = Client::from_env()?;

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    let mut messages: Vec<Message> = Vec::new();

    print!("> ");
    stdout.flush()?;

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            print!("> ");
            stdout.flush()?;
            continue;
        }

        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: line,
                cache_control: None,
            }],
        });

        loop {
            // Cache conversation history by marking the last user message. The API
            // automatically looks back to find previously cached content.
            let mut request_messages = messages.clone();
            if let Some(last_msg) = request_messages.last_mut() {
                if last_msg.role == Role::User {
                    if let Some(last_content) = last_msg.content.last_mut() {
                        if let ContentBlock::Text { cache_control, .. } = last_content {
                            *cache_control = Some(CacheControl::default());
                        }
                    }
                }
            }

            let request = MessagesRequest {
                model: model.api_model_id().to_string(),
                max_tokens: MAX_TOKENS,
                // Cache system prompt (first breakpoint).
                system: Some(SystemPrompt::Blocks(vec![SystemBlock::Text {
                    text: "You are a helpful assistant running inside a sandboxed environment. You can execute bash commands to help the user.".to_string(),
                    cache_control: Some(CacheControl::default()),
                }])),
                messages: request_messages,
                tools: Some(vec![bash_tool(), edit_tool(), write_tool()]),
                temperature: None,
                top_p: None,
                top_k: None,
            };

            let response = client.messages(request)?;

            // Print cache usage statistics
            if response.usage.cache_read_input_tokens > 0 {
                eprintln!(
                    "[Cache hit: {} tokens read, {} tokens created]",
                    response.usage.cache_read_input_tokens,
                    response.usage.cache_creation_input_tokens
                );
            } else if response.usage.cache_creation_input_tokens > 0 {
                eprintln!(
                    "[Cache created: {} tokens]",
                    response.usage.cache_creation_input_tokens
                );
            }

            let mut has_tool_use = false;
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for block in &response.content {
                match block {
                    ContentBlock::Text { text, .. } => {
                        println!("{}", text);
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        has_tool_use = true;
                        let tool_name = AgentToolName::from_str(name)
                            .map_err(|_| anyhow::anyhow!("Unknown tool: {}", name))?;

                        let (output, success) = match tool_name {
                            AgentToolName::Bash => {
                                let command =
                                    input.get("command").and_then(|v| v.as_str()).unwrap_or("");

                                println!("$ {}", command);

                                let (output, success) =
                                    execute_bash_in_sandbox(container_name, command)?;

                                if !output.is_empty() {
                                    println!("{}", output);
                                }

                                (output, success)
                            }
                            AgentToolName::Edit => {
                                let file_path = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let old_string = input
                                    .get("old_string")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let new_string = input
                                    .get("new_string")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");

                                println!("[edit] {}", file_path);

                                let (output, success) = execute_edit_in_sandbox(
                                    container_name,
                                    file_path,
                                    old_string,
                                    new_string,
                                )?;

                                println!("{}", output);
                                (output, success)
                            }
                            AgentToolName::Write => {
                                let file_path = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let content =
                                    input.get("content").and_then(|v| v.as_str()).unwrap_or("");

                                println!("[write] {}", file_path);

                                let (output, success) =
                                    execute_write_in_sandbox(container_name, file_path, content)?;

                                println!("{}", output);
                                (output, success)
                            }
                        };

                        // Anthropic API requires non-empty content when is_error is true.
                        // Tool implementations must ensure this - panic if violated.
                        assert!(
                            success || !output.is_empty(),
                            "Tool error with empty output - tool implementation must provide error message"
                        );

                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: output,
                            is_error: if success { None } else { Some(true) },
                            cache_control: None,
                        });
                    }
                    ContentBlock::ToolResult { .. } => {}
                    ContentBlock::Image { .. } => {}
                }
            }

            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            if has_tool_use && !tool_results.is_empty() {
                messages.push(Message {
                    role: Role::User,
                    content: tool_results,
                });
            }

            if response.stop_reason != StopReason::ToolUse {
                break;
            }
        }

        print!("> ");
        stdout.flush()?;
    }

    Ok(())
}
