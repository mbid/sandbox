use anyhow::{bail, Context, Result};
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

fn execute_bash_in_sandbox(container_name: &str, command: &str) -> Result<(String, bool)> {
    let output = Command::new("docker")
        .args(["exec", container_name, "bash", "-c", command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to execute command in sandbox")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{}\n{}", stdout, stderr)
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

                            let (output, success) =
                                execute_bash_in_sandbox(container_name, command)?;

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
