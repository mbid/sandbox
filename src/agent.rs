use anyhow::{bail, Context, Result};
use indoc::formatdoc;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use crate::anthropic::{
    CacheControl, Client, ContentBlock, CustomTool, Message, MessagesRequest, Role, StopReason,
    SystemBlock, SystemPrompt, Tool,
};

const MODEL: &str = "claude-haiku-4-5-20251001";
const MAX_TOKENS: u32 = 4096;

fn bash_tool() -> Tool {
    Tool::Custom(CustomTool {
        name: "bash".to_string(),
        description: formatdoc! {"
            Execute a bash command inside the sandbox and return the output.
            Output is truncated to 30000 characters, but full output can be retrieved later if needed.
        "},
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

    Ok((combined, output.status.success()))
}

pub fn run_agent(container_name: &str) -> Result<()> {
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
                model: MODEL.to_string(),
                max_tokens: MAX_TOKENS,
                // Cache system prompt (first breakpoint).
                system: Some(SystemPrompt::Blocks(vec![SystemBlock::Text {
                    text: "You are a helpful assistant running inside a sandboxed environment. You can execute bash commands to help the user.".to_string(),
                    cache_control: Some(CacheControl::default()),
                }])),
                messages: request_messages,
                tools: Some(vec![bash_tool()]),
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
                        if name == "bash" {
                            let command =
                                input.get("command").and_then(|v| v.as_str()).unwrap_or("");

                            // Print the command being executed with $ prefix
                            println!("$ {}", command);

                            let (output, success) =
                                execute_bash_in_sandbox(container_name, command)?;

                            // Print the output
                            if !output.is_empty() {
                                println!("{}", output);
                            }

                            tool_results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: output,
                                is_error: if success { None } else { Some(true) },
                                cache_control: None,
                            });
                        } else {
                            bail!("Unknown tool: {}", name);
                        }
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
